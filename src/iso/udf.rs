//! UDF (ECMA-167 / OSTA UDF) reading: anchor, volume descriptor sequence,
//! file set, file entries, allocation descriptors and file identifiers.
//!
//! Supported: logical block size 2048, physical (type 1) partition maps,
//! ICB strategy 4, short / long / embedded allocation descriptors with
//! allocation extent continuation. Virtual, sparable and metadata partitions
//! (UDF 1.50 VAT media, 2.50+ Blu-ray layouts) are detected and refused with
//! an "unsupported UDF variant" error so the caller can fall back to the
//! ISO 9660 side of a bridge disc.

use super::{
    clean_name, le16, le32, le64, push_extent, push_note, read_bytes, utf16be, Budget, DateTime,
    Entry, Extent, MAX_DEPTH, MAX_DIR_BYTES,
};
use crate::blockdev::ReadAt;
use crate::error::{Error, Result};

const BLOCK: u64 = 2048;

pub(crate) const TAG_PRIMARY: u16 = 1;
pub(crate) const TAG_ANCHOR: u16 = 2;
pub(crate) const TAG_POINTER: u16 = 3;
pub(crate) const TAG_IMPL_USE: u16 = 4;
pub(crate) const TAG_PARTITION: u16 = 5;
pub(crate) const TAG_LOGICAL_VOLUME: u16 = 6;
pub(crate) const TAG_UNALLOCATED: u16 = 7;
pub(crate) const TAG_TERMINATOR: u16 = 8;
pub(crate) const TAG_INTEGRITY: u16 = 9;
pub(crate) const TAG_FILE_SET: u16 = 256;
pub(crate) const TAG_FILE_ID: u16 = 257;
pub(crate) const TAG_ALLOC_EXTENT: u16 = 258;
pub(crate) const TAG_FILE_ENTRY: u16 = 261;
pub(crate) const TAG_EXT_FILE_ENTRY: u16 = 266;

const CRC_NOTE: &str =
    "A UDF descriptor has a wrong CRC; it was used anyway because its checksum is intact.";

const fn crc_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x1021
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u16; 256] = crc_table();

/// CRC-16 as used by UDF descriptor tags (CCITT polynomial 0x1021, start 0).
pub(crate) fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for b in data {
        crc = (crc << 8) ^ CRC_TABLE[((crc >> 8) as u8 ^ *b) as usize];
    }
    crc
}

/// Checksum of a descriptor tag: the sum of its bytes except byte 4.
pub(crate) fn tag_checksum(tag: &[u8]) -> u8 {
    let mut sum = 0u8;
    for (i, b) in tag.iter().take(16).enumerate() {
        if i != 4 {
            sum = sum.wrapping_add(*b);
        }
    }
    sum
}

/// Verifies the tag at the start of `buf` and returns its identifier.
/// A wrong checksum or an unexpected identifier is an error; a wrong CRC
/// only produces a note.
fn check_tag(buf: &[u8], want: Option<u16>, notes: &mut Vec<String>) -> Result<u16> {
    if buf.len() < 16 {
        return Err(Error::invalid("a UDF descriptor is cut short"));
    }
    if tag_checksum(buf) != buf[4] {
        return Err(Error::invalid("a UDF descriptor tag has a wrong checksum"));
    }
    let id = le16(buf, 0);
    if let Some(w) = want {
        if id != w {
            return Err(Error::invalid(format!(
                "unexpected UDF descriptor (tag {id} where {w} was expected)"
            )));
        }
    }
    let crc_len = le16(buf, 10) as usize;
    let ok = match buf.get(16..16 + crc_len) {
        Some(body) => crc16(body) == le16(buf, 8),
        None => false,
    };
    if !ok {
        push_note(notes, CRC_NOTE.to_string());
    }
    Ok(id)
}

/// A long allocation descriptor / ICB address.
#[derive(Clone, Copy, Debug)]
struct LongAd {
    len: u32,
    block: u32,
    part: u16,
}

fn long_ad(b: &[u8], o: usize) -> LongAd {
    LongAd {
        len: le32(b, o) & 0x3FFF_FFFF,
        block: le32(b, o + 4),
        part: le16(b, o + 8),
    }
}

/// A mounted UDF volume: enough to resolve block addresses and find the root.
#[derive(Clone, Debug)]
pub(crate) struct Mount {
    /// Per partition reference number: first sector and length in sectors.
    parts: Vec<(u64, u64)>,
    root: LongAd,
}

/// What was learned about the UDF side of an image.
pub(crate) struct UdfInfo {
    /// "1.02", "2.50", ...
    pub revision: Option<String>,
    pub label: Option<String>,
    /// Byte offset of the end of the last partition.
    pub volume_end: Option<u64>,
    /// The usable volume, or why it cannot be used.
    pub mount: Result<Mount>,
}

/// Decodes OSTA compressed unicode (compression id 8 or 16 first).
fn decode_cs0(bytes: &[u8]) -> String {
    match bytes.split_first() {
        None => String::new(),
        Some((16, rest)) => utf16be(rest),
        Some((_, rest)) => rest.iter().map(|b| *b as char).collect(),
    }
}

/// Decodes a fixed-size dstring field (the last byte holds the used length).
fn dstring(field: &[u8]) -> String {
    let Some((len, body)) = field.split_last() else {
        return String::new();
    };
    let used = (*len as usize).min(body.len());
    decode_cs0(&body[..used])
        .trim_end_matches(['\0', ' '])
        .to_string()
}

fn timestamp(b: &[u8], o: usize) -> Option<DateTime> {
    let tz = le16(b, o);
    // Low 12 bits: signed offset in minutes, -2047 meaning "not specified".
    let mut offset = (tz & 0x0FFF) as i32;
    if offset & 0x800 != 0 {
        offset -= 0x1000;
    }
    if offset == -2047 || (tz >> 12) != 1 {
        offset = 0;
    }
    let t = b.get(o..o + 12)?;
    DateTime::checked(
        le16(t, 2) as i16 as i32,
        t[4],
        t[5],
        t[6],
        t[7],
        t[8],
        offset,
    )
}

enum PartMap {
    Physical(u16),
    Unsupported(String),
}

struct LogicalVolume {
    vdsn: u32,
    label: String,
    block_size: u32,
    revision: Option<String>,
    file_set: LongAd,
    maps: Vec<PartMap>,
}

struct PartitionDesc {
    vdsn: u32,
    number: u16,
    start: u64,
    len: u64,
}

struct Sequence {
    lvd: LogicalVolume,
    partitions: Vec<PartitionDesc>,
}

fn parse_lvd(s: &[u8]) -> Result<LogicalVolume> {
    let domain_id = s.get(217..240).unwrap_or(&[]);
    let revision = if domain_id.starts_with(b"*OSTA UDF Compliant") {
        let r = le16(s, 240);
        Some(format!("{:x}.{:02x}", r >> 8, r & 0xFF))
    } else {
        None
    };
    let map_len = le32(s, 264) as usize;
    let count = le32(s, 268) as usize;
    let table = s
        .get(440..440usize.saturating_add(map_len))
        .ok_or_else(|| Error::invalid("the UDF partition map table is too large"))?;
    if count > 16 {
        return Err(Error::invalid("the UDF volume has too many partition maps"));
    }
    let mut maps = Vec::new();
    let mut p = 0usize;
    for _ in 0..count {
        let kind = *table
            .get(p)
            .ok_or_else(|| Error::invalid("the UDF partition map table is cut short"))?;
        let len = table.get(p + 1).copied().unwrap_or(0) as usize;
        if len < 2 || p + len > table.len() {
            return Err(Error::invalid("a UDF partition map has a bad length"));
        }
        let m = &table[p..p + len];
        maps.push(match (kind, len) {
            (1, 6) => PartMap::Physical(le16(m, 4)),
            (2, 64) => {
                let id = String::from_utf8_lossy(&m[5..28]).to_string();
                let what = if id.contains("Metadata") {
                    "metadata partition (UDF 2.50 or later)"
                } else if id.contains("Virtual") {
                    "virtual partition (write-once media)"
                } else if id.contains("Sparable") {
                    "sparable partition (rewritable media)"
                } else {
                    "unknown type 2 partition map"
                };
                PartMap::Unsupported(what.to_string())
            }
            _ => PartMap::Unsupported(format!("partition map type {kind}")),
        });
        p += len;
    }
    Ok(LogicalVolume {
        vdsn: le32(s, 16),
        label: dstring(&s[84..212]),
        block_size: le32(s, 212),
        revision,
        file_set: long_ad(s, 248),
        maps,
    })
}

/// Reads one volume descriptor sequence (main or reserve).
fn read_sequence(
    src: &mut dyn ReadAt,
    mut loc: u64,
    mut len: u64,
    notes: &mut Vec<String>,
) -> Result<Sequence> {
    let mut lvd: Option<LogicalVolume> = None;
    let mut partitions: Vec<PartitionDesc> = Vec::new();
    let mut s = [0u8; 2048];
    let mut hops = 0;
    let mut i = 0u64;
    let mut seen = 0;
    while i < len / BLOCK && seen < 256 {
        seen += 1;
        let offset = (loc + i)
            .checked_mul(BLOCK)
            .ok_or_else(|| Error::invalid("bad UDF volume descriptor location"))?;
        read_bytes(src, offset, &mut s, "a UDF volume descriptor")?;
        i += 1;
        if s.iter().all(|b| *b == 0) {
            break; // unrecorded sector ends the sequence
        }
        match check_tag(&s, None, notes)? {
            TAG_PARTITION => {
                let pd = PartitionDesc {
                    vdsn: le32(&s, 16),
                    number: le16(&s, 22),
                    start: le32(&s, 188) as u64,
                    len: le32(&s, 192) as u64,
                };
                match partitions.iter_mut().find(|p| p.number == pd.number) {
                    Some(old) => {
                        if pd.vdsn >= old.vdsn {
                            *old = pd;
                        }
                    }
                    None => {
                        if partitions.len() < 16 {
                            partitions.push(pd);
                        }
                    }
                }
            }
            TAG_LOGICAL_VOLUME => {
                let new = parse_lvd(&s)?;
                if lvd.as_ref().map(|old| new.vdsn >= old.vdsn).unwrap_or(true) {
                    lvd = Some(new);
                }
            }
            TAG_POINTER => {
                let next_len = le32(&s, 20) as u64 & 0x3FFF_FFFF;
                let next_loc = le32(&s, 24) as u64;
                hops += 1;
                if next_len == 0 || hops > 4 {
                    break;
                }
                loc = next_loc;
                len = next_len;
                i = 0;
            }
            TAG_TERMINATOR => break,
            _ => {}
        }
    }
    match lvd {
        Some(lvd) if !partitions.is_empty() => Ok(Sequence { lvd, partitions }),
        _ => Err(Error::invalid(
            "the UDF volume descriptor sequence is incomplete",
        )),
    }
}

fn mount(src: &mut dyn ReadAt, seq: &Sequence, notes: &mut Vec<String>) -> Result<Mount> {
    if seq.lvd.block_size as u64 != BLOCK {
        return Err(Error::invalid(format!(
            "unsupported UDF variant: logical block size {}",
            seq.lvd.block_size
        )));
    }
    let mut parts = Vec::new();
    for m in &seq.lvd.maps {
        match m {
            PartMap::Physical(number) => {
                let pd = seq
                    .partitions
                    .iter()
                    .find(|p| p.number == *number)
                    .ok_or_else(|| {
                        Error::invalid("a UDF partition map names a missing partition")
                    })?;
                parts.push((pd.start, pd.len));
            }
            PartMap::Unsupported(what) => {
                return Err(Error::invalid(format!("unsupported UDF variant: {what}")));
            }
        }
    }
    if parts.is_empty() {
        return Err(Error::invalid("the UDF volume has no partition map"));
    }
    let mut m = Mount {
        parts,
        root: LongAd {
            len: 0,
            block: 0,
            part: 0,
        },
    };
    let fsd_at = m.block_offset(seq.lvd.file_set.part, seq.lvd.file_set.block)?;
    let mut s = [0u8; 512];
    read_bytes(src, fsd_at, &mut s, "the UDF file set descriptor")?;
    check_tag(&s, Some(TAG_FILE_SET), notes)?;
    m.root = long_ad(&s, 400);
    if m.root.len == 0 {
        return Err(Error::invalid("the UDF file set has no root directory"));
    }
    Ok(m)
}

/// Looks for a UDF volume. `None` when no anchor is found (not UDF).
pub(crate) fn probe(src: &mut dyn ReadAt, notes: &mut Vec<String>) -> Option<UdfInfo> {
    let n = src.size() / BLOCK;
    let mut candidates = vec![256u64];
    if n > 1 {
        candidates.push(n - 1);
    }
    if n > 257 {
        candidates.push(n - 257);
    }
    let mut s = [0u8; 512];
    let mut anchor: Option<[(u64, u64); 2]> = None;
    let mut scratch = Vec::new();
    for lba in candidates {
        if read_bytes(src, lba * BLOCK, &mut s, "the UDF anchor").is_err() {
            continue;
        }
        if le16(&s, 0) == TAG_ANCHOR && check_tag(&s, Some(TAG_ANCHOR), &mut scratch).is_ok() {
            anchor = Some([
                (le32(&s, 20) as u64, le32(&s, 16) as u64),
                (le32(&s, 28) as u64, le32(&s, 24) as u64),
            ]);
            break;
        }
    }
    let anchor = anchor?;

    let mut info = UdfInfo {
        revision: None,
        label: None,
        volume_end: None,
        mount: Err(Error::invalid(
            "the UDF volume descriptor sequence is missing",
        )),
    };
    for (i, (loc, len)) in anchor.into_iter().enumerate() {
        if len == 0 {
            continue;
        }
        match read_sequence(src, loc, len, notes) {
            Ok(seq) => {
                if i == 1 {
                    push_note(
                        notes,
                        "The main UDF volume descriptor sequence is damaged; the reserve copy was used."
                            .to_string(),
                    );
                }
                info.revision = seq.lvd.revision.clone();
                info.label = Some(seq.lvd.label.clone());
                info.volume_end = seq
                    .partitions
                    .iter()
                    .map(|p| (p.start + p.len).saturating_mul(BLOCK))
                    .max();
                info.mount = mount(src, &seq, notes);
                break;
            }
            Err(e) => info.mount = Err(e),
        }
    }
    Some(info)
}

impl Mount {
    /// Byte offset of a logical block, checked against its partition.
    fn block_offset(&self, part: u16, block: u32) -> Result<u64> {
        let (start, len) = *self
            .parts
            .get(part as usize)
            .ok_or_else(|| Error::invalid("a UDF address names a missing partition"))?;
        if block as u64 >= len {
            return Err(Error::invalid("a UDF address lies outside its partition"));
        }
        (start + block as u64)
            .checked_mul(BLOCK)
            .ok_or_else(|| Error::invalid("a UDF address is out of range"))
    }
}

/// A parsed (extended) file entry.
struct Icb {
    /// Byte offset of the entry itself (identifies a directory).
    at: u64,
    file_type: u8,
    size: u64,
    mtime: Option<DateTime>,
    extents: Vec<Extent>,
}

struct Walker<'a> {
    src: &'a mut dyn ReadAt,
    mount: &'a Mount,
    lenient: bool,
    budget: Budget,
    out: Vec<Entry>,
    notes: &'a mut Vec<String>,
}

/// Reads the whole UDF tree. In lenient mode unreadable entries below the
/// root are skipped with a note.
pub(crate) fn read_tree(
    src: &mut dyn ReadAt,
    mount: &Mount,
    lenient: bool,
    notes: &mut Vec<String>,
) -> Result<Vec<Entry>> {
    let mut w = Walker {
        src,
        mount,
        lenient,
        budget: Budget::new(),
        out: Vec::new(),
        notes,
    };
    let root = w.read_icb(mount.root)?;
    if root.file_type != 4 {
        return Err(Error::invalid("the UDF root is not a directory"));
    }
    w.budget.visit(root.at)?;
    w.walk(&root, "", 0)?;
    Ok(w.out)
}

impl Walker<'_> {
    fn read_icb(&mut self, ad: LongAd) -> Result<Icb> {
        let at = self.mount.block_offset(ad.part, ad.block)?;
        let mut b = [0u8; 2048];
        read_bytes(self.src, at, &mut b, "a UDF file entry")?;
        let (mtime_at, lengths_at, base) = match check_tag(&b, None, self.notes)? {
            TAG_FILE_ENTRY => (84, 168, 176usize),
            TAG_EXT_FILE_ENTRY => (92, 208, 216usize),
            other => {
                return Err(Error::invalid(format!(
                    "unexpected UDF descriptor (tag {other} where a file entry was expected)"
                )))
            }
        };
        let strategy = le16(&b, 20);
        if strategy != 4 {
            return Err(Error::invalid(format!(
                "unsupported UDF variant: ICB strategy {strategy}"
            )));
        }
        let file_type = b[27];
        let ad_type = le16(&b, 34) & 7;
        let ea_len = le32(&b, lengths_at) as usize;
        let ad_len = le32(&b, lengths_at + 4) as usize;
        let ad_start = base.saturating_add(ea_len);
        let ad_end = ad_start.saturating_add(ad_len);
        if ad_end > b.len() {
            return Err(Error::invalid("a UDF file entry is larger than one block"));
        }
        let size = le64(&b, 56);
        let extents = match ad_type {
            3 => {
                if size > ad_len as u64 {
                    return Err(Error::invalid(
                        "a UDF file embedded in its entry is larger than the entry",
                    ));
                }
                let mut v = Vec::new();
                push_extent(
                    &mut v,
                    Extent {
                        offset: at + ad_start as u64,
                        len: size,
                        sparse: false,
                    },
                )?;
                v
            }
            0 | 1 => self.collect(&b[ad_start..ad_end], ad_type == 1, ad.part, size)?,
            _ => {
                return Err(Error::invalid(
                    "unsupported UDF variant: extended allocation descriptors",
                ))
            }
        };
        Ok(Icb {
            at,
            file_type,
            size,
            mtime: timestamp(&b, mtime_at),
            extents,
        })
    }

    /// Turns allocation descriptors into extents covering exactly `size` bytes.
    fn collect(&mut self, first: &[u8], long: bool, part: u16, size: u64) -> Result<Vec<Extent>> {
        let step = if long { 16 } else { 8 };
        let mut area: Vec<u8> = first.to_vec();
        let mut out: Vec<Extent> = Vec::new();
        let mut remaining = size;
        let mut hops = 0;
        'chain: loop {
            let mut next: Option<(u16, u32)> = None;
            let mut p = 0;
            while p + step <= area.len() {
                let raw = le32(&area, p);
                let len = (raw & 0x3FFF_FFFF) as u64;
                let kind = raw >> 30;
                let block = le32(&area, p + 4);
                let prt = if long { le16(&area, p + 8) } else { part };
                p += step;
                if len == 0 {
                    break 'chain;
                }
                if kind == 3 {
                    next = Some((prt, block));
                    break;
                }
                if remaining == 0 {
                    break 'chain; // allocated space past the end of the file
                }
                let take = len.min(remaining);
                let extent = if kind == 0 {
                    Extent {
                        offset: self.mount.block_offset(prt, block)?,
                        len: take,
                        sparse: false,
                    }
                } else {
                    Extent {
                        offset: 0,
                        len: take,
                        sparse: true,
                    }
                };
                push_extent(&mut out, extent)?;
                remaining -= take;
            }
            let Some((prt, block)) = next else {
                break;
            };
            hops += 1;
            if hops > 4096 {
                return Err(Error::invalid("a UDF file has too many allocation extents"));
            }
            let at = self.mount.block_offset(prt, block)?;
            let mut b = [0u8; 2048];
            read_bytes(self.src, at, &mut b, "a UDF allocation extent")?;
            check_tag(&b, Some(TAG_ALLOC_EXTENT), self.notes)?;
            let len = le32(&b, 20) as usize;
            area = b
                .get(24..24usize.saturating_add(len))
                .ok_or_else(|| Error::invalid("a UDF allocation extent is larger than one block"))?
                .to_vec();
        }
        if remaining > 0 {
            // The tail of the file is not recorded: it reads as zeros.
            push_extent(
                &mut out,
                Extent {
                    offset: 0,
                    len: remaining,
                    sparse: true,
                },
            )?;
        }
        Ok(out)
    }

    /// Reads the data of a directory into memory.
    fn dir_data(&mut self, icb: &Icb) -> Result<Vec<u8>> {
        self.budget.take_dir(icb.size)?;
        let size = self.src.size();
        let mut recorded = 0u64;
        for x in &icb.extents {
            if x.sparse {
                return Err(Error::invalid("a UDF directory is not fully recorded"));
            }
            match x.offset.checked_add(x.len) {
                Some(end) if end <= size => recorded += x.len,
                _ => {
                    return Err(Error::invalid(
                        "a directory lies beyond the end of the image",
                    ))
                }
            }
        }
        if recorded != icb.size || recorded > MAX_DIR_BYTES {
            return Err(Error::invalid("a UDF directory has a bad size"));
        }
        let mut data = vec![0u8; recorded as usize];
        let mut pos = 0usize;
        for x in &icb.extents {
            let n = x.len as usize;
            read_bytes(self.src, x.offset, &mut data[pos..pos + n], "a directory")?;
            pos += n;
        }
        Ok(data)
    }

    /// Parses the file identifier descriptors of a directory.
    fn children(&mut self, data: &[u8]) -> Result<Vec<(String, LongAd)>> {
        let mut out = Vec::new();
        let mut p = 0usize;
        while p + 38 <= data.len() {
            if le16(data, p) == 0 {
                break; // padding
            }
            check_tag(&data[p..], Some(TAG_FILE_ID), self.notes)?;
            let characteristics = data[p + 18];
            let name_len = data[p + 19] as usize;
            let icb = long_ad(data, p + 20);
            let impl_len = le16(data, p + 36) as usize;
            let name_at = p + 38 + impl_len;
            let end = name_at + name_len;
            if end > data.len() {
                return Err(Error::invalid(
                    "a UDF file identifier runs past the end of its directory",
                ));
            }
            let name = decode_cs0(&data[name_at..end]);
            p += (38 + impl_len + name_len + 3) & !3;
            if characteristics & 0x0C != 0 {
                continue; // deleted entry or the parent link
            }
            out.push((clean_name(&name), icb));
        }
        Ok(out)
    }

    fn walk(&mut self, dir: &Icb, prefix: &str, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::invalid("the directory tree is too deep"));
        }
        let data = self.dir_data(dir)?;
        let children = self.children(&data)?;
        drop(data);
        for (name, ad) in children {
            self.budget.take_entry()?;
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let icb = match self.read_icb(ad) {
                Ok(i) => i,
                Err(e) if self.lenient => {
                    push_note(
                        self.notes,
                        format!("\"{path}\" could not be read ({e}); it was left out."),
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };
            match icb.file_type {
                4 => {
                    let mark = self.out.len();
                    self.out.push(Entry {
                        path: path.clone(),
                        is_dir: true,
                        size: 0,
                        extents: Vec::new(),
                        mtime: icb.mtime,
                    });
                    let result = match self.budget.visit(icb.at) {
                        Ok(()) => self.walk(&icb, &path, depth + 1),
                        Err(e) => Err(e),
                    };
                    if let Err(e) = result {
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
                }
                // Unspecified, regular and real-time files.
                0 | 5 | 249 => self.out.push(Entry {
                    path,
                    is_dir: false,
                    size: icb.size,
                    extents: icb.extents,
                    mtime: icb.mtime,
                }),
                // Symbolic links, devices, stream directories: nothing to copy.
                _ => {}
            }
        }
        Ok(())
    }
}
