//! El Torito boot catalog: the boot record volume descriptor points at a
//! catalog of 32-byte entries (validation entry, initial/default entry, then
//! section headers each followed by section entries).

use super::{le16, le32, read_bytes};
use crate::blockdev::ReadAt;
use crate::error::{Error, Result};
use crate::model::{BootEntry, BootPlatform};

/// Most catalog sectors that are read (64 entries each).
const MAX_CATALOG_SECTORS: u64 = 4;

/// Maps an El Torito platform id.
pub fn platform(id: u8) -> BootPlatform {
    match id {
        0x00 => BootPlatform::Bios,
        0xEF => BootPlatform::Uefi,
        other => BootPlatform::Other(other),
    }
}

/// Text for the boot media type of an entry (low four bits of byte 1).
pub fn emulation_text(media: u8) -> String {
    match media & 0x0F {
        0 => "no emulation".to_string(),
        1 => "1.2 MB floppy".to_string(),
        2 => "1.44 MB floppy".to_string(),
        3 => "2.88 MB floppy".to_string(),
        4 => "hard disk".to_string(),
        n => format!("unknown ({n})"),
    }
}

fn entry(e: &[u8], platform: BootPlatform) -> BootEntry {
    BootEntry {
        platform,
        bootable: e[0] == 0x88,
        emulation: emulation_text(e[1]),
        lba: le32(e, 8),
        sectors: le16(e, 6) as u32,
    }
}

/// Parses the bytes of a boot catalog.
///
/// Fails when the validation entry is missing or its checksum is wrong, or
/// when the initial entry is not an entry at all.
pub fn parse_catalog(data: &[u8]) -> Result<Vec<BootEntry>> {
    if data.len() < 64 {
        return Err(Error::invalid("the boot catalog is cut short"));
    }
    let v = &data[0..32];
    if v[0] != 1 || v[30] != 0x55 || v[31] != 0xAA {
        return Err(Error::invalid("the boot catalog has no validation entry"));
    }
    let mut sum = 0u16;
    for i in 0..16 {
        sum = sum.wrapping_add(le16(v, i * 2));
    }
    if sum != 0 {
        return Err(Error::invalid("the boot catalog checksum is wrong"));
    }
    let mut out = Vec::new();
    let initial = &data[32..64];
    if initial[0] != 0x88 && initial[0] != 0x00 {
        return Err(Error::invalid("the boot catalog has a bad initial entry"));
    }
    out.push(entry(initial, platform(v[1])));

    let mut p = 64;
    while p + 32 <= data.len() {
        let h = &data[p..p + 32];
        p += 32;
        match h[0] {
            0x90 | 0x91 => {
                let plat = platform(h[1]);
                let mut left = le16(h, 2);
                while left > 0 && p + 32 <= data.len() {
                    let e = &data[p..p + 32];
                    p += 32;
                    if e[0] == 0x44 {
                        continue; // extension of the previous entry
                    }
                    if (e[0] != 0x88 && e[0] != 0x00) || e.iter().all(|b| *b == 0) {
                        return Ok(out); // unused space or not an entry: stop quietly
                    }
                    out.push(entry(e, plat));
                    left -= 1;
                }
                if h[0] == 0x91 {
                    break;
                }
            }
            0x44 => {}
            _ => break,
        }
    }
    Ok(out)
}

/// Reads the catalog starting at the given 2048-byte sector.
pub(crate) fn read_catalog(src: &mut dyn ReadAt, lba: u32) -> Result<Vec<BootEntry>> {
    let start = lba as u64 * 2048;
    if lba < 16 || start + 2048 > src.size() {
        return Err(Error::invalid(
            "the boot catalog lies beyond the end of the image",
        ));
    }
    let sectors = ((src.size() - start) / 2048).min(MAX_CATALOG_SECTORS);
    let mut data = vec![0u8; (sectors * 2048) as usize];
    read_bytes(src, start, &mut data, "the boot catalog")?;
    parse_catalog(&data)
}

/// Finds the El Torito boot record among the volume descriptors and reads
/// its catalog. `Ok(None)` when the image has no boot record.
pub fn read_boot_entries(src: &mut dyn ReadAt) -> Result<Option<Vec<BootEntry>>> {
    match super::iso9660::read_descriptors(src).boot_catalog {
        Some(lba) => read_catalog(src, lba).map(Some),
        None => Ok(None),
    }
}
