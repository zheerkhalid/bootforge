//! ISO 9660 (ECMA-119) volume descriptors and directory trees, with the
//! Joliet (UCS-2 supplementary descriptor) and Rock Ridge (`NM` alternate
//! names, `CE` continuation areas, `CL`/`RE` relocated directories) extensions.

use super::{
    be32, clean_name, le32, push_extent, push_note, read_bytes, read_vec, utf16be, Budget,
    DateTime, Entry, Extent, MAX_DEPTH,
};
use crate::blockdev::ReadAt;
use crate::error::{Error, Result};

/// How many sectors after sector 16 are searched for volume descriptors.
const MAX_DESCRIPTORS: u64 = 64;
/// Longest Rock Ridge name that is assembled from `NM` pieces.
const MAX_RR_NAME: usize = 1024;
/// Largest SUSP continuation area that is followed.
const MAX_CE_BYTES: u64 = 64 * 1024;

/// What a primary or supplementary volume descriptor says.
#[derive(Clone, Debug)]
pub(crate) struct VolDesc {
    pub label: String,
    /// Logical block size (512, 1024 or 2048).
    pub block_size: u32,
    /// Volume space size in logical blocks.
    pub space_blocks: u32,
    pub root_lba: u32,
    pub root_len: u32,
    pub joliet: bool,
}

/// The volume descriptor set starting at sector 16.
#[derive(Clone, Debug, Default)]
pub(crate) struct Descriptors {
    pub primary: Option<VolDesc>,
    pub joliet: Option<VolDesc>,
    /// Sector of the El Torito boot catalog.
    pub boot_catalog: Option<u32>,
    /// A UDF "NSR" descriptor was seen in the volume recognition sequence.
    pub nsr: bool,
}

/// Reads a both-endian 32-bit field (little-endian copy first).
fn both32(b: &[u8], o: usize) -> u32 {
    let le = le32(b, o);
    if le != 0 {
        le
    } else {
        be32(b, o + 4)
    }
}

fn parse_vol_desc(s: &[u8], joliet: bool) -> Option<VolDesc> {
    let block_size = {
        let le = u16::from_le_bytes([s[128], s[129]]);
        if le != 0 {
            le
        } else {
            u16::from_be_bytes([s[130], s[131]])
        }
    } as u32;
    if !matches!(block_size, 512 | 1024 | 2048) {
        return None;
    }
    let root = &s[156..190];
    let label = if joliet {
        utf16be(&s[40..72])
    } else {
        latin_or_utf8(&s[40..72])
    };
    let label = label.trim_end_matches([' ', '\0']).trim().to_string();
    Some(VolDesc {
        label,
        block_size,
        space_blocks: both32(s, 80),
        root_lba: both32(root, 2).saturating_add(root[1] as u32),
        root_len: both32(root, 10),
        joliet,
    })
}

/// Scans the volume descriptor set. Never fails: what cannot be read is absent.
pub(crate) fn read_descriptors(src: &mut dyn ReadAt) -> Descriptors {
    let mut d = Descriptors::default();
    let mut s = [0u8; 2048];
    for i in 0..MAX_DESCRIPTORS {
        if read_bytes(src, (16 + i) * 2048, &mut s, "volume descriptor").is_err() {
            break;
        }
        match &s[1..6] {
            b"CD001" => match s[0] {
                0 => {
                    if d.boot_catalog.is_none() && &s[7..30] == b"EL TORITO SPECIFICATION" {
                        d.boot_catalog = Some(le32(&s, 71));
                    }
                }
                1 => {
                    if d.primary.is_none() {
                        d.primary = parse_vol_desc(&s, false);
                    }
                }
                2 => {
                    let joliet =
                        s[88] == 0x25 && s[89] == 0x2F && matches!(s[90], 0x40 | 0x43 | 0x45);
                    if joliet && d.joliet.is_none() {
                        d.joliet = parse_vol_desc(&s, true);
                    }
                }
                _ => {}
            },
            b"NSR02" | b"NSR03" => d.nsr = true,
            b"BEA01" | b"TEA01" | b"BOOT2" | b"CDW02" => {}
            _ => break,
        }
    }
    // Joliet without a primary descriptor is not a valid volume.
    if d.primary.is_none() {
        d.joliet = None;
    }
    d
}

/// Bytes that are ASCII in practice; decode as UTF-8 when valid, else Latin-1.
fn latin_or_utf8(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => b.iter().map(|c| *c as char).collect(),
    }
}

/// The system use area of a directory record (after the padded name).
fn system_use(rec: &[u8]) -> &[u8] {
    let nlen = rec.get(32).copied().unwrap_or(0) as usize;
    let start = 33 + nlen + if nlen.is_multiple_of(2) { 1 } else { 0 };
    rec.get(start..).unwrap_or(&[])
}

/// True when a system use area holds well-formed Rock Ridge / SUSP fields.
fn susp_has_rock_ridge(su: &[u8]) -> bool {
    let mut p = 0;
    while p + 4 <= su.len() {
        let len = su[p + 2] as usize;
        if len < 4 || p + len > su.len() {
            return false;
        }
        match &su[p..p + 2] {
            b"SP" => {
                if len >= 7 && su[p + 4] == 0xBE && su[p + 5] == 0xEF {
                    return true;
                }
            }
            b"RR" | b"PX" | b"NM" | b"TF" | b"ER" => return true,
            _ => {}
        }
        p += len;
    }
    false
}

/// Looks at the first block of the root directory for Rock Ridge records.
pub(crate) fn detect_rock_ridge(src: &mut dyn ReadAt, vol: &VolDesc) -> bool {
    let bs = vol.block_size as u64;
    let len = (vol.root_len as u64).min(bs);
    let data = match read_vec(src, vol.root_lba as u64 * bs, len, bs, "root directory") {
        Ok(d) => d,
        Err(_) => return false,
    };
    let mut pos = 0;
    while pos < data.len() {
        let l = data[pos] as usize;
        if l < 34 || pos + l > data.len() {
            break;
        }
        let su = system_use(&data[pos..pos + l]);
        if susp_has_rock_ridge(su) {
            return true;
        }
        pos += l;
    }
    false
}

/// What the Rock Ridge fields of one record say.
#[derive(Default)]
struct RockRidge {
    name: Vec<u8>,
    has_name: bool,
    /// `CL`: the record stands for a directory relocated to this block.
    child_link: Option<u32>,
    /// `RE`: the record is the relocated copy and must be hidden.
    relocated: bool,
}

/// One child of a directory, with multi-extent records already combined.
struct Child {
    name: String,
    is_dir: bool,
    /// First data block and byte length of a directory.
    dir_lba: u64,
    dir_len: u64,
    size: u64,
    extents: Vec<Extent>,
    mtime: Option<DateTime>,
}

struct Walker<'a> {
    src: &'a mut dyn ReadAt,
    vol: &'a VolDesc,
    use_rr: bool,
    lenient: bool,
    /// Bytes to skip at the start of every system use area (from `SP`).
    rr_skip: usize,
    saw_nm: bool,
    budget: Budget,
    out: Vec<Entry>,
    notes: &'a mut Vec<String>,
}

/// Reads the whole tree of one volume descriptor.
///
/// `use_rr` applies Rock Ridge names when present; the returned flag tells
/// whether any was found. In lenient mode unreadable sub-directories are
/// skipped with a note; the root directory must be readable in either mode.
pub(crate) fn read_tree(
    src: &mut dyn ReadAt,
    vol: &VolDesc,
    use_rr: bool,
    lenient: bool,
    notes: &mut Vec<String>,
) -> Result<(Vec<Entry>, bool)> {
    let mut w = Walker {
        src,
        vol,
        use_rr: use_rr && !vol.joliet,
        lenient,
        rr_skip: 0,
        saw_nm: false,
        budget: Budget::new(),
        out: Vec::new(),
        notes,
    };
    w.walk(vol.root_lba as u64, vol.root_len as u64, "", 0, true)?;
    Ok((w.out, w.saw_nm))
}

impl Walker<'_> {
    fn read_dir(&mut self, lba: u64, len: u64) -> Result<Vec<u8>> {
        self.budget.take_dir(len)?;
        let offset = lba
            .checked_mul(self.vol.block_size as u64)
            .ok_or_else(|| Error::invalid("bad directory location"))?;
        read_vec(self.src, offset, len, super::MAX_DIR_BYTES, "a directory")
    }

    /// Collects the Rock Ridge fields of one system use area, following
    /// continuation areas. Malformed fields end the scan quietly.
    fn rock_ridge(&mut self, su: &[u8]) -> RockRidge {
        let mut info = RockRidge::default();
        let mut area: Vec<u8> = su.get(self.rr_skip..).unwrap_or(&[]).to_vec();
        for _hop in 0..8 {
            let mut next: Option<(u32, u32, u32)> = None;
            let mut p = 0;
            while p + 4 <= area.len() {
                let len = area[p + 2] as usize;
                if len < 4 || p + len > area.len() {
                    break;
                }
                let body = &area[p + 4..p + len];
                match &area[p..p + 2] {
                    b"NM" => {
                        if let Some((flags, text)) = body.split_first() {
                            // Bits 1 and 2 mean "." and "..": no name bytes.
                            if flags & 0x06 == 0 && info.name.len() + text.len() <= MAX_RR_NAME {
                                info.name.extend_from_slice(text);
                                info.has_name = true;
                            }
                        }
                    }
                    b"CE" => {
                        if body.len() >= 24 {
                            next = Some((le32(body, 0), le32(body, 8), le32(body, 16)));
                        }
                    }
                    b"CL" => {
                        if body.len() >= 4 {
                            info.child_link = Some(le32(body, 0));
                        }
                    }
                    b"RE" => info.relocated = true,
                    b"ST" => {
                        next = None;
                        break;
                    }
                    _ => {}
                }
                p += len;
            }
            let Some((block, offset, len)) = next else {
                break;
            };
            let at = (block as u64)
                .saturating_mul(self.vol.block_size as u64)
                .saturating_add(offset as u64);
            match read_vec(
                self.src,
                at,
                len as u64,
                MAX_CE_BYTES,
                "a continuation area",
            ) {
                Ok(v) => area = v,
                Err(_) => break,
            }
        }
        info
    }

    fn parse_records(&mut self, data: &[u8], is_root: bool) -> Result<Vec<Child>> {
        let bs = self.vol.block_size as usize;
        let mut children: Vec<Child> = Vec::new();
        let mut pending: Option<Child> = None;
        let mut pos = 0usize;
        let mut first = true;
        while pos < data.len() {
            let l = data[pos] as usize;
            if l == 0 {
                // Records do not cross blocks: the rest of this one is padding.
                pos = (pos / bs + 1) * bs;
                continue;
            }
            if l < 34 || pos + l > data.len() {
                return Err(Error::invalid("a directory record has a bad length"));
            }
            let rec = &data[pos..pos + l];
            pos += l;
            let nlen = rec[32] as usize;
            if nlen == 0 || 33 + nlen > l {
                return Err(Error::invalid("a directory record has a bad name length"));
            }
            let raw_name = &rec[33..33 + nlen];
            let flags = rec[25];
            let su = system_use(rec);

            if first && is_root && self.use_rr {
                // The root's "." record may carry SP with the skip length.
                if su.len() >= 7 && &su[0..2] == b"SP" && su[4] == 0xBE && su[5] == 0xEF {
                    self.rr_skip = su[6] as usize;
                }
            }
            first = false;

            if nlen == 1 && raw_name[0] <= 1 {
                continue; // "." and ".."
            }
            if flags & 0x04 != 0 {
                continue; // associated file (resource fork)
            }

            let mut lba = both32(rec, 2) as u64 + rec[1] as u64;
            let mut len = both32(rec, 10) as u64;
            let mut is_dir = flags & 0x02 != 0;
            let multi = flags & 0x80 != 0;

            // A continuation of a multi-extent file: only the location matters.
            if let Some(p) = pending.as_mut() {
                let offset = lba
                    .checked_mul(bs as u64)
                    .ok_or_else(|| Error::invalid("bad file location"))?;
                push_extent(
                    &mut p.extents,
                    Extent {
                        offset,
                        len,
                        sparse: false,
                    },
                )?;
                p.size = p.size.saturating_add(len);
                if !multi {
                    if let Some(done) = pending.take() {
                        children.push(done);
                    }
                }
                continue;
            }

            let mut name: Option<String> = None;
            if self.use_rr {
                let info = self.rock_ridge(su);
                if info.relocated {
                    continue;
                }
                if info.has_name && !info.name.is_empty() {
                    name = Some(String::from_utf8_lossy(&info.name).into_owned());
                    self.saw_nm = true;
                }
                if let Some(block) = info.child_link {
                    // A placeholder for a relocated directory: its real size
                    // is in the "." record at the target.
                    let at = (block as u64)
                        .checked_mul(bs as u64)
                        .ok_or_else(|| Error::invalid("bad directory location"))?;
                    let mut head = [0u8; 34];
                    read_bytes(self.src, at, &mut head, "a relocated directory")?;
                    is_dir = true;
                    lba = le32(&head, 2) as u64 + head[1] as u64;
                    len = both32(&head, 10) as u64;
                }
            }
            let name = match name {
                Some(n) => n,
                None if self.vol.joliet => {
                    let n = utf16be(raw_name);
                    if is_dir {
                        n
                    } else {
                        strip_version(&n).to_string()
                    }
                }
                None => {
                    let n = latin_or_utf8(raw_name);
                    if is_dir {
                        n
                    } else {
                        strip_version(&n).trim_end_matches('.').to_string()
                    }
                }
            };

            let date = &rec[18..25];
            let mtime = DateTime::checked(
                1900 + date[0] as i32,
                date[1],
                date[2],
                date[3],
                date[4],
                date[5],
                (date[6] as i8) as i32 * 15,
            );

            let mut child = Child {
                name: clean_name(&name),
                is_dir,
                dir_lba: lba,
                dir_len: len,
                size: 0,
                extents: Vec::new(),
                mtime,
            };
            if !is_dir {
                let offset = lba
                    .checked_mul(bs as u64)
                    .ok_or_else(|| Error::invalid("bad file location"))?;
                push_extent(
                    &mut child.extents,
                    Extent {
                        offset,
                        len,
                        sparse: false,
                    },
                )?;
                child.size = len;
                if multi {
                    pending = Some(child);
                    continue;
                }
            }
            children.push(child);
        }
        if let Some(p) = pending.take() {
            children.push(p);
        }
        Ok(children)
    }

    fn walk(
        &mut self,
        lba: u64,
        len: u64,
        prefix: &str,
        depth: usize,
        is_root: bool,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::invalid("the directory tree is too deep"));
        }
        self.budget.visit(lba)?;
        let data = self.read_dir(lba, len)?;
        let children = self.parse_records(&data, is_root)?;
        drop(data);
        for c in children {
            self.budget.take_entry()?;
            let path = if prefix.is_empty() {
                c.name
            } else {
                format!("{prefix}/{}", c.name)
            };
            if c.is_dir {
                let mark = self.out.len();
                self.out.push(Entry {
                    path: path.clone(),
                    is_dir: true,
                    size: 0,
                    extents: Vec::new(),
                    mtime: c.mtime,
                });
                if let Err(e) = self.walk(c.dir_lba, c.dir_len, &path, depth + 1, false) {
                    if !self.lenient {
                        return Err(e);
                    }
                    self.out.truncate(mark + 1);
                    push_note(
                        self.notes,
                        format!(
                            "The folder \"{path}\" could not be read ({e}); it is shown empty."
                        ),
                    );
                }
            } else {
                self.out.push(Entry {
                    path,
                    is_dir: false,
                    size: c.size,
                    extents: c.extents,
                    mtime: c.mtime,
                });
            }
        }
        Ok(())
    }
}

/// Removes a `;N` file version suffix.
fn strip_version(name: &str) -> &str {
    match name.rfind(';') {
        Some(i) if i + 1 < name.len() && name[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
            &name[..i]
        }
        _ => name,
    }
}
