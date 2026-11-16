//! A very small read-only FAT12/16/32 reader, just enough to look for
//! `EFI/BOOT/BOOT*.EFI` inside an El Torito EFI system partition image.
//! Only 8.3 names are read (the names looked for are all 8.3).

use super::{le16, le32, read_bytes};
use crate::blockdev::ReadAt;
use crate::error::{Error, Result};

/// Most directory bytes read per directory.
const MAX_DIR: u64 = 1024 * 1024;

struct Fat<'a> {
    src: &'a mut dyn ReadAt,
    base: u64,
    cluster_bytes: u64,
    fat_at: u64,
    root_at: u64,
    root_bytes: u64,
    data_at: u64,
    clusters: u64,
    bits: u8,
    root_cluster: u32,
}

struct DirEntry {
    name: String,
    is_dir: bool,
    cluster: u32,
}

impl Fat<'_> {
    fn open(src: &mut dyn ReadAt, base: u64) -> Result<Fat<'_>> {
        let mut b = [0u8; 512];
        read_bytes(src, base, &mut b, "the EFI boot image")?;
        let bad = || Error::invalid("the EFI boot image is not a FAT volume");
        let bps = le16(&b, 11) as u64;
        let spc = b[13] as u64;
        let reserved = le16(&b, 14) as u64;
        let fats = b[16] as u64;
        let root_entries = le16(&b, 17) as u64;
        if !matches!(bps, 512 | 1024 | 2048 | 4096)
            || !(spc as u8).is_power_of_two()
            || reserved == 0
            || !(1..=2).contains(&fats)
        {
            return Err(bad());
        }
        let total = match le16(&b, 19) {
            0 => le32(&b, 32) as u64,
            n => n as u64,
        };
        let fat_sectors = match le16(&b, 22) {
            0 => le32(&b, 36) as u64,
            n => n as u64,
        };
        let root_sectors = (root_entries * 32).div_ceil(bps);
        let first_data = reserved + fats * fat_sectors + root_sectors;
        if fat_sectors == 0 || total <= first_data {
            return Err(bad());
        }
        let clusters = (total - first_data) / spc;
        let bits = if clusters < 4085 {
            12
        } else if clusters < 65525 {
            16
        } else {
            32
        };
        Ok(Fat {
            src,
            base,
            cluster_bytes: spc * bps,
            fat_at: reserved * bps,
            root_at: (reserved + fats * fat_sectors) * bps,
            root_bytes: root_entries * 32,
            data_at: first_data * bps,
            clusters,
            bits,
            root_cluster: le32(&b, 44),
        })
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let at = self
            .base
            .checked_add(offset)
            .ok_or_else(|| Error::invalid("bad offset in the EFI boot image"))?;
        read_bytes(self.src, at, buf, "the EFI boot image")
    }

    /// The cluster after `c`, or `None` at the end of a chain.
    fn next(&mut self, c: u32) -> Result<Option<u32>> {
        let c64 = c as u64;
        let value = match self.bits {
            12 => {
                let mut b = [0u8; 2];
                self.read(self.fat_at + c64 * 3 / 2, &mut b)?;
                let v = u16::from_le_bytes(b);
                let v = if c & 1 == 1 { v >> 4 } else { v & 0x0FFF } as u32;
                if v >= 0xFF8 {
                    return Ok(None);
                }
                v
            }
            16 => {
                let mut b = [0u8; 2];
                self.read(self.fat_at + c64 * 2, &mut b)?;
                let v = u16::from_le_bytes(b) as u32;
                if v >= 0xFFF8 {
                    return Ok(None);
                }
                v
            }
            _ => {
                let mut b = [0u8; 4];
                self.read(self.fat_at + c64 * 4, &mut b)?;
                let v = u32::from_le_bytes(b) & 0x0FFF_FFFF;
                if v >= 0x0FFF_FFF8 {
                    return Ok(None);
                }
                v
            }
        };
        if value < 2 || value as u64 >= self.clusters + 2 {
            return Ok(None);
        }
        Ok(Some(value))
    }

    /// Reads the bytes of a directory: `None` is the fixed FAT12/16 root.
    fn dir_bytes(&mut self, cluster: Option<u32>) -> Result<Vec<u8>> {
        let start = match cluster {
            None if self.bits != 32 => {
                let len = self.root_bytes.min(MAX_DIR);
                let mut v = vec![0u8; len as usize];
                self.read(self.root_at, &mut v)?;
                return Ok(v);
            }
            None => self.root_cluster,
            Some(c) => c,
        };
        let mut out = Vec::new();
        let mut c = start;
        let mut steps = 0u64;
        loop {
            if c < 2 || c as u64 >= self.clusters + 2 {
                break;
            }
            if out.len() as u64 + self.cluster_bytes > MAX_DIR || steps > self.clusters {
                break;
            }
            steps += 1;
            let at = self.data_at + (c as u64 - 2) * self.cluster_bytes;
            let old = out.len();
            out.resize(old + self.cluster_bytes as usize, 0);
            self.read(at, &mut out[old..])?;
            match self.next(c)? {
                Some(n) => c = n,
                None => break,
            }
        }
        Ok(out)
    }

    fn list(&mut self, cluster: Option<u32>) -> Result<Vec<DirEntry>> {
        let data = self.dir_bytes(cluster)?;
        let mut out = Vec::new();
        for e in data.as_chunks::<32>().0 {
            if e[0] == 0 {
                break;
            }
            let attr = e[11];
            if e[0] == 0xE5 || attr & 0x0F == 0x0F || attr & 0x08 != 0 {
                continue;
            }
            let part = |b: &[u8]| -> String {
                b.iter()
                    .map(|c| *c as char)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            };
            let stem = part(&e[0..8]);
            let ext = part(&e[8..11]);
            let name = if ext.is_empty() {
                stem
            } else {
                format!("{stem}.{ext}")
            };
            out.push(DirEntry {
                name,
                is_dir: attr & 0x10 != 0,
                cluster: (le16(e, 20) as u32) << 16 | le16(e, 26) as u32,
            });
        }
        Ok(out)
    }
}

/// Lists the `EFI/BOOT/BOOT*.EFI` files of the FAT volume starting at byte
/// `base` of the source (upper-case 8.3 names). Fails when there is no FAT
/// volume there.
pub(crate) fn efi_boot_files(src: &mut dyn ReadAt, base: u64) -> Result<Vec<String>> {
    let mut fat = Fat::open(src, base)?;
    let mut dir: Option<u32> = None;
    for want in ["EFI", "BOOT"] {
        let found = fat
            .list(dir)?
            .into_iter()
            .find(|e| e.is_dir && e.name.eq_ignore_ascii_case(want));
        match found {
            Some(e) => dir = Some(e.cluster),
            None => return Ok(Vec::new()),
        }
    }
    Ok(fat
        .list(dir)?
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.name.to_ascii_uppercase())
        .filter(|n| n.starts_with("BOOT") && n.ends_with(".EFI"))
        .collect())
}
