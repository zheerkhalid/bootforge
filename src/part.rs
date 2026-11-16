//! MBR and GPT partition tables: writing and reading.
//!
//! Written from the UEFI specification (chapter 5, "GUID Partition Table (GPT)
//! Disk Layout") and the classic PC MBR layout. The writer (`write_mbr`,
//! `write_gpt`) and the reader (`read_table`) share only the trivial
//! little-endian helpers, so the reader can be used to verify what the writer
//! produced.
//!
//! GUIDs in this API are 16 bytes in *canonical* order: the order in which the
//! hex digits are printed (`C12A7328-F81F-...` is `[0xC1, 0x2A, 0x73, 0x28, 0xF8, ...]`).
//! On disk GPT stores the first three groups little-endian; the conversion is
//! done here (`guid_to_disk`), callers never see the mixed-endian form.

use crate::blockdev::BlockDevice;
use crate::error::{Error, IoContext, Result};
use crate::model::Scheme;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

const MIB: u64 = 1024 * 1024;

/// Number of partition entries written in a GPT (the customary 128).
pub const GPT_ENTRY_COUNT: u32 = 128;
/// Size of one GPT partition entry in bytes.
pub const GPT_ENTRY_SIZE: u32 = 128;
const GPT_HEADER_SIZE: u32 = 92;
const GPT_REVISION: u32 = 0x0001_0000;
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const MAX_PROBLEMS: usize = 100;

/// EFI System partition, `C12A7328-F81F-11D2-BA4B-00A0C93EC93B` (canonical order).
pub const GUID_EFI_SYSTEM: [u8; 16] = [
    0xC1, 0x2A, 0x73, 0x28, 0xF8, 0x1F, 0x11, 0xD2, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];
/// Microsoft basic data, `EBD0A0A2-B9E5-4433-87C0-68B6B72699C7` (canonical order).
pub const GUID_BASIC_DATA: [u8; 16] = [
    0xEB, 0xD0, 0xA0, 0xA2, 0xB9, 0xE5, 0x44, 0x33, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];
const GUID_MS_RESERVED: [u8; 16] = [
    0xE3, 0xC9, 0xE3, 0x16, 0x0B, 0x5C, 0x4D, 0xB8, 0x81, 0x7D, 0xF9, 0x2D, 0xF0, 0x02, 0x15, 0xAE,
];
const GUID_LINUX_FS: [u8; 16] = [
    0x0F, 0xC6, 0x3D, 0xAF, 0x84, 0x83, 0x47, 0x72, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

/// What a partition is for. Maps to an MBR type byte or a GPT type GUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartType {
    /// EFI System partition (MBR 0xEF, GPT `C12A7328-...`).
    EfiSystem,
    /// Microsoft basic data (MBR 0x0C, GPT `EBD0A0A2-...`).
    BasicData,
    /// FAT32 with LBA addressing (MBR 0x0C; basic data in GPT).
    Fat32Lba,
    /// exFAT or NTFS (MBR 0x07; basic data in GPT).
    ExFatOrNtfs,
}

impl PartType {
    /// The MBR partition type byte.
    pub fn mbr_type(self) -> u8 {
        match self {
            PartType::EfiSystem => 0xEF,
            PartType::BasicData | PartType::Fat32Lba => 0x0C,
            PartType::ExFatOrNtfs => 0x07,
        }
    }

    /// The GPT partition type GUID in canonical byte order.
    pub fn gpt_type(self) -> [u8; 16] {
        match self {
            PartType::EfiSystem => GUID_EFI_SYSTEM,
            PartType::BasicData | PartType::Fat32Lba | PartType::ExFatOrNtfs => GUID_BASIC_DATA,
        }
    }
}

/// One partition to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartSpec {
    pub kind: PartType,
    /// Start in bytes from the beginning of the device (a multiple of the sector size).
    pub offset: u64,
    /// Length in bytes (a non-zero multiple of the sector size).
    pub size: u64,
    /// GPT partition name (at most 36 UTF-16 units). Ignored for MBR.
    pub name: String,
    /// MBR boot flag (0x80). Ignored for GPT. At most one partition may be active.
    pub active: bool,
    /// GPT unique partition GUID in canonical byte order (see `random_guid`).
    /// Must not be all zero and must be unique within the table. Ignored for MBR.
    pub guid: [u8; 16],
}

impl PartSpec {
    /// A spec with a fresh random GUID, not active.
    pub fn new(kind: PartType, offset: u64, size: u64, name: &str) -> PartSpec {
        PartSpec {
            kind,
            offset,
            size,
            name: name.to_string(),
            active: false,
            guid: random_guid(),
        }
    }
}

/// The identifier of a partitioned disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskId {
    None,
    /// The 32-bit MBR disk signature (offset 440).
    Mbr(u32),
    /// The GPT disk GUID in canonical byte order.
    Gpt([u8; 16]),
}

/// A partition found by `read_table`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundPart {
    /// Human description: "EFI System", "Basic data", "FAT32 LBA (0x0C)", ...
    pub kind: String,
    /// MBR type byte (`None` for GPT).
    pub type_byte: Option<u8>,
    /// GPT type GUID, canonical order (`None` for MBR).
    pub type_guid: Option<[u8; 16]>,
    /// Start in bytes.
    pub offset: u64,
    /// Length in bytes.
    pub size: u64,
    /// MBR boot flag (always false for GPT).
    pub active: bool,
    /// GPT partition name ("" for MBR).
    pub name: String,
    /// GPT unique partition GUID, canonical order (`None` for MBR).
    pub guid: Option<[u8; 16]>,
}

/// What `read_table` found on a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// `None` when the device carries neither an MBR nor a GPT.
    pub scheme: Option<Scheme>,
    pub disk_id: DiskId,
    /// Partitions in table order (empty slots skipped).
    pub parts: Vec<FoundPart>,
    /// Every inconsistency found; empty for a healthy table.
    pub problems: Vec<String>,
}

// --------------------------------------------------------------------------------------
// Small helpers
// --------------------------------------------------------------------------------------

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn le64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

fn problem(list: &mut Vec<String>, text: String) {
    if list.len() < MAX_PROBLEMS {
        list.push(text);
    } else if list.len() == MAX_PROBLEMS {
        list.push("(further problems not listed)".to_string());
    }
}

/// Converts a GUID between canonical byte order and the mixed-endian GPT
/// on-disk form (the first three groups byte-swapped). The conversion is its
/// own inverse.
pub fn guid_to_disk(guid: [u8; 16]) -> [u8; 16] {
    let g = guid;
    [
        g[3], g[2], g[1], g[0], g[5], g[4], g[7], g[6], g[8], g[9], g[10], g[11], g[12], g[13],
        g[14], g[15],
    ]
}

/// `C12A7328-F81F-11D2-BA4B-00A0C93EC93B` for a GUID in canonical byte order.
pub fn format_guid(guid: &[u8; 16]) -> String {
    let mut s = String::with_capacity(36);
    for (i, b) in guid.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}

static RANDOM_COUNTER: AtomicU64 = AtomicU64::new(0);

fn random_u64() -> u64 {
    // `RandomState::new()` carries per-process random keys (seeded by the OS)
    // that change with every instance; the time and a counter are mixed in.
    let mut h = RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.write_u128(nanos);
    h.write_u64(RANDOM_COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    h.finish()
}

/// A random non-zero 32-bit value (MBR disk signature, FAT volume id).
/// Not cryptographic: std's `RandomState` mixed with the clock.
pub fn random_u32() -> u32 {
    loop {
        let v = random_u64();
        let x = (v ^ (v >> 32)) as u32;
        if x != 0 {
            return x;
        }
    }
}

/// A random version-4 GUID in canonical byte order (version and variant bits set).
/// Not cryptographic: std's `RandomState` mixed with the clock.
pub fn random_guid() -> [u8; 16] {
    let mut g = [0u8; 16];
    g[..8].copy_from_slice(&random_u64().to_le_bytes());
    g[8..].copy_from_slice(&random_u64().to_le_bytes());
    g[6] = (g[6] & 0x0F) | 0x40;
    g[8] = (g[8] & 0x3F) | 0x80;
    g
}

fn sector_of(dev: &dyn BlockDevice) -> Result<u64> {
    let s = dev.sector_size();
    if !(512..=4096).contains(&s) || !s.is_power_of_two() {
        return Err(Error::invalid(format!(
            "Unsupported sector size ({s} bytes)."
        )));
    }
    Ok(s as u64)
}

/// Sectors taken by the GPT partition entry array.
fn gpt_entry_sectors(sector: u64) -> u64 {
    (GPT_ENTRY_COUNT as u64 * GPT_ENTRY_SIZE as u64).div_ceil(sector)
}

/// Cylinder/head/sector address with the conventional 255 heads and 63
/// sectors per track; `None` beyond cylinder 1023.
fn chs(lba: u64) -> Option<[u8; 3]> {
    const HEADS: u64 = 255;
    const SPT: u64 = 63;
    let c = lba / (HEADS * SPT);
    if c > 1023 {
        return None;
    }
    let h = (lba / SPT) % HEADS;
    let s = lba % SPT + 1;
    Some([h as u8, (s as u8) | (((c >> 8) as u8) << 6), c as u8])
}

/// The conventional "beyond CHS" address (cylinder 1023, head 254, sector 63).
const CHS_BEYOND: [u8; 3] = [0xFE, 0xFF, 0xFF];

// --------------------------------------------------------------------------------------
// Planning and writing
// --------------------------------------------------------------------------------------

/// Offset and size (bytes) of one partition covering the device: it starts at
/// 1 MiB and ends on a 1 MiB boundary, leaving room for the backup GPT when
/// `scheme` is GPT.
///
/// Fails for devices too small to hold such a partition and, for MBR, for
/// devices with more sectors than a 32-bit LBA can describe (2 TiB at 512
/// bytes per sector).
pub fn single_partition(dev_size: u64, sector: u32, scheme: Scheme) -> Result<(u64, u64)> {
    if !(512..=4096).contains(&sector) || !sector.is_power_of_two() {
        return Err(Error::invalid(format!(
            "Unsupported sector size ({sector} bytes)."
        )));
    }
    let s = sector as u64;
    let dev_size = dev_size - dev_size % s;
    let limit = match scheme {
        Scheme::Mbr => {
            if dev_size / s > u32::MAX as u64 {
                return Err(Error::invalid(
                    "The drive is too large for an MBR partition table (more than 2^32 sectors); use GPT.",
                ));
            }
            dev_size
        }
        // Backup entry array plus backup header at the very end.
        Scheme::Gpt => dev_size.saturating_sub((gpt_entry_sectors(s) + 1) * s),
    };
    let end = limit - limit % MIB;
    if end <= MIB {
        return Err(Error::invalid("The drive is too small to be partitioned."));
    }
    Ok((MIB, end - MIB))
}

/// Checks alignment, bounds (`lo..hi` in bytes) and overlap of the specs.
fn validate_specs(parts: &[PartSpec], s: u64, lo: u64, hi: u64) -> Result<()> {
    for (i, p) in parts.iter().enumerate() {
        let n = i + 1;
        if !p.offset.is_multiple_of(s) || !p.size.is_multiple_of(s) {
            return Err(Error::invalid(format!(
                "Partition {n} is not aligned to the {s}-byte sector size."
            )));
        }
        if p.size == 0 {
            return Err(Error::invalid(format!("Partition {n} is empty.")));
        }
        let end = p
            .offset
            .checked_add(p.size)
            .ok_or_else(|| Error::invalid(format!("Partition {n} is out of range.")))?;
        if p.offset < lo {
            return Err(Error::invalid(format!(
                "Partition {n} starts inside the partition table area."
            )));
        }
        if end > hi {
            return Err(Error::invalid(format!(
                "Partition {n} ends past the usable end of the drive."
            )));
        }
    }
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|&i| parts[i].offset);
    for w in order.windows(2) {
        let (a, b) = (&parts[w[0]], &parts[w[1]]);
        if a.offset + a.size > b.offset {
            return Err(Error::invalid(format!(
                "Partitions {} and {} overlap.",
                w[0] + 1,
                w[1] + 1
            )));
        }
    }
    Ok(())
}

/// Writes a classic MBR with up to four primary partitions into sector 0.
///
/// The boot code area is left zero (the drives made here boot through UEFI
/// from files, or not at all). LBA fields are authoritative; CHS fields are
/// computed with 255 heads / 63 sectors for small disks and set to the
/// conventional (1023, 254, 63) beyond that. Refuses more than four
/// partitions, more than one active partition, unaligned, overlapping or
/// out-of-range specs and anything a 32-bit LBA cannot describe. Nothing is
/// written when the input is refused.
pub fn write_mbr(dev: &mut dyn BlockDevice, parts: &[PartSpec], disk_signature: u32) -> Result<()> {
    let s = sector_of(dev)?;
    let size = dev.size();
    if size < 2 * s {
        return Err(Error::invalid("The drive is too small to be partitioned."));
    }
    if parts.len() > 4 {
        return Err(Error::invalid(format!(
            "An MBR holds at most 4 primary partitions ({} requested).",
            parts.len()
        )));
    }
    if parts.iter().filter(|p| p.active).count() > 1 {
        return Err(Error::invalid("Only one MBR partition can be active."));
    }
    validate_specs(parts, s, s, size)?;
    let mut sector0 = vec![0u8; s as usize];
    put32(&mut sector0, 440, disk_signature);
    for (i, p) in parts.iter().enumerate() {
        let start = p.offset / s;
        let count = p.size / s;
        if start > u32::MAX as u64 || count > u32::MAX as u64 {
            return Err(Error::invalid(format!(
                "Partition {} cannot be described by an MBR (beyond 2^32 sectors); use GPT.",
                i + 1
            )));
        }
        let e = 446 + i * 16;
        sector0[e] = if p.active { 0x80 } else { 0x00 };
        sector0[e + 1..e + 4].copy_from_slice(&chs(start).unwrap_or(CHS_BEYOND));
        sector0[e + 4] = p.kind.mbr_type();
        sector0[e + 5..e + 8].copy_from_slice(&chs(start + count - 1).unwrap_or(CHS_BEYOND));
        put32(&mut sector0, e + 8, start as u32);
        put32(&mut sector0, e + 12, count as u32);
    }
    sector0[510] = 0x55;
    sector0[511] = 0xAA;
    dev.write_at(0, &sector0).ctx("writing the MBR")?;
    dev.flush().ctx("flushing the MBR")?;
    Ok(())
}

fn gpt_header(
    s: u64,
    my_lba: u64,
    alternate_lba: u64,
    usable: (u64, u64),
    disk_guid: [u8; 16],
    entries_lba: u64,
    entries_crc: u32,
) -> Vec<u8> {
    let mut h = vec![0u8; s as usize];
    h[0..8].copy_from_slice(GPT_SIGNATURE);
    put32(&mut h, 8, GPT_REVISION);
    put32(&mut h, 12, GPT_HEADER_SIZE);
    // 16: header CRC (below); 20: reserved.
    put64(&mut h, 24, my_lba);
    put64(&mut h, 32, alternate_lba);
    put64(&mut h, 40, usable.0);
    put64(&mut h, 48, usable.1);
    h[56..72].copy_from_slice(&guid_to_disk(disk_guid));
    put64(&mut h, 72, entries_lba);
    put32(&mut h, 80, GPT_ENTRY_COUNT);
    put32(&mut h, 84, GPT_ENTRY_SIZE);
    put32(&mut h, 88, entries_crc);
    let crc = crc32(&h[..GPT_HEADER_SIZE as usize]);
    put32(&mut h, 16, crc);
    h
}

/// Writes a GPT: protective MBR, primary header at LBA 1 followed by 128
/// entries, and the backup entries and header at the end of the device.
///
/// Works for 512- and 4096-byte sectors. Partitions must be sector-aligned,
/// lie between the first and last usable LBA, not overlap, and carry distinct
/// non-zero GUIDs and names of at most 36 UTF-16 units; anything else is
/// refused before a single byte is written. Old data between the structures
/// is not touched: call `wipe_tables` first.
pub fn write_gpt(dev: &mut dyn BlockDevice, parts: &[PartSpec], disk_guid: [u8; 16]) -> Result<()> {
    let s = sector_of(dev)?;
    let total = dev.size() / s;
    let es = gpt_entry_sectors(s);
    // Protective MBR, two headers, two entry arrays and at least one usable sector.
    if total < 2 * es + 4 {
        return Err(Error::invalid("The drive is too small for a GPT."));
    }
    if parts.len() > GPT_ENTRY_COUNT as usize {
        return Err(Error::invalid(format!(
            "A GPT made here holds at most {GPT_ENTRY_COUNT} partitions."
        )));
    }
    if disk_guid == [0u8; 16] {
        return Err(Error::invalid("The GPT disk GUID must not be zero."));
    }
    let first_usable = 2 + es;
    let last_usable = total - es - 2;
    let backup_entries_lba = total - es - 1;
    let backup_header_lba = total - 1;
    validate_specs(parts, s, first_usable * s, (last_usable + 1) * s)?;

    let mut entries = vec![0u8; (es * s) as usize];
    for (i, p) in parts.iter().enumerate() {
        if p.guid == [0u8; 16] {
            return Err(Error::invalid(format!(
                "Partition {} has no unique GUID.",
                i + 1
            )));
        }
        if parts[..i].iter().any(|q| q.guid == p.guid) {
            return Err(Error::invalid(format!(
                "Partition {} repeats the GUID of another partition.",
                i + 1
            )));
        }
        let name: Vec<u16> = p.name.encode_utf16().collect();
        if name.len() > 36 || name.contains(&0) {
            return Err(Error::invalid(format!(
                "The name of partition {} is not valid for a GPT (36 characters at most).",
                i + 1
            )));
        }
        let e = &mut entries[i * GPT_ENTRY_SIZE as usize..(i + 1) * GPT_ENTRY_SIZE as usize];
        e[0..16].copy_from_slice(&guid_to_disk(p.kind.gpt_type()));
        e[16..32].copy_from_slice(&guid_to_disk(p.guid));
        put64(e, 32, p.offset / s);
        put64(e, 40, (p.offset + p.size) / s - 1);
        // 48: attributes, all zero.
        for (k, u) in name.iter().enumerate() {
            e[56 + k * 2..58 + k * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    let entries_crc = crc32(&entries[..(GPT_ENTRY_COUNT * GPT_ENTRY_SIZE) as usize]);

    // Protective MBR: one 0xEE entry covering the disk (clamped to 32 bits).
    let mut pmbr = vec![0u8; s as usize];
    pmbr[446] = 0x00;
    pmbr[447..450].copy_from_slice(&[0x00, 0x02, 0x00]);
    pmbr[450] = 0xEE;
    pmbr[451..454].copy_from_slice(&chs(total - 1).unwrap_or([0xFF, 0xFF, 0xFF]));
    put32(&mut pmbr, 454, 1);
    put32(&mut pmbr, 458, (total - 1).min(u32::MAX as u64) as u32);
    pmbr[510] = 0x55;
    pmbr[511] = 0xAA;

    let primary = gpt_header(
        s,
        1,
        backup_header_lba,
        (first_usable, last_usable),
        disk_guid,
        2,
        entries_crc,
    );
    let backup = gpt_header(
        s,
        backup_header_lba,
        1,
        (first_usable, last_usable),
        disk_guid,
        backup_entries_lba,
        entries_crc,
    );

    dev.write_at(backup_entries_lba * s, &entries)
        .ctx("writing the backup GPT entries")?;
    dev.write_at(backup_header_lba * s, &backup)
        .ctx("writing the backup GPT header")?;
    dev.write_at(2 * s, &entries)
        .ctx("writing the GPT entries")?;
    dev.write_at(s, &primary).ctx("writing the GPT header")?;
    dev.write_at(0, &pmbr).ctx("writing the protective MBR")?;
    dev.flush().ctx("flushing the GPT")?;
    Ok(())
}

/// Zeroes the first and the last 1 MiB of the device (clamped to its size),
/// which removes any MBR, GPT, backup GPT and the file-system signatures at
/// the usual places.
pub fn wipe_tables(dev: &mut dyn BlockDevice) -> Result<()> {
    sector_of(dev)?;
    let size = dev.size();
    let len = MIB.min(size);
    if len == 0 {
        return Ok(());
    }
    let zeros = vec![0u8; len as usize];
    dev.write_at(0, &zeros)
        .ctx("wiping the start of the drive")?;
    if size > len {
        // Both are sector multiples, so the tail start is aligned too.
        dev.write_at(size - len, &zeros)
            .ctx("wiping the end of the drive")?;
    }
    dev.flush().ctx("flushing the wipe")?;
    Ok(())
}

// --------------------------------------------------------------------------------------
// Reading
// --------------------------------------------------------------------------------------

struct MbrEntry {
    boot: u8,
    kind: u8,
    start: u32,
    count: u32,
}

fn mbr_entries(sector0: &[u8]) -> Vec<MbrEntry> {
    (0..4)
        .map(|i| {
            let e = 446 + i * 16;
            MbrEntry {
                boot: sector0[e],
                kind: sector0[e + 4],
                start: le32(sector0, e + 8),
                count: le32(sector0, e + 12),
            }
        })
        .collect()
}

fn mbr_kind(kind: u8) -> String {
    let name = match kind {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x05 | 0x0F => "Extended",
        0x07 => "NTFS/exFAT",
        0x0B => "FAT32",
        0x0C => "FAT32 LBA",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0xEE => "GPT protective",
        0xEF => "EFI System",
        _ => "Unknown",
    };
    format!("{name} (0x{kind:02X})")
}

fn gpt_kind(type_guid: &[u8; 16]) -> String {
    if *type_guid == GUID_EFI_SYSTEM {
        "EFI System".to_string()
    } else if *type_guid == GUID_BASIC_DATA {
        "Basic data".to_string()
    } else if *type_guid == GUID_MS_RESERVED {
        "Microsoft reserved".to_string()
    } else if *type_guid == GUID_LINUX_FS {
        "Linux filesystem".to_string()
    } else {
        format!("Unknown ({})", format_guid(type_guid))
    }
}

/// A FAT, exFAT or NTFS boot sector (a "superfloppy"), which also ends in
/// 0x55AA but holds no partition table.
fn looks_like_boot_sector(s0: &[u8]) -> bool {
    &s0[3..11] == b"NTFS    "
        || &s0[3..11] == b"EXFAT   "
        || &s0[82..87] == b"FAT32"
        || &s0[54..57] == b"FAT"
}

fn read_lba(dev: &mut dyn BlockDevice, s: u64, lba: u64, count: u64) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; (count * s) as usize];
    dev.read_at(lba * s, &mut buf)
        .ctx("reading the partition table")?;
    Ok(buf)
}

struct GptHeader {
    alternate: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: [u8; 16],
    entries_lba: u64,
    count: u32,
    entry_size: u32,
    entries_crc: u32,
}

/// Validates one header sector. Returns `None` when the header cannot be trusted.
fn parse_gpt_header(
    sec: &[u8],
    at_lba: u64,
    which: &str,
    problems: &mut Vec<String>,
) -> Option<GptHeader> {
    if &sec[0..8] != GPT_SIGNATURE {
        problem(
            problems,
            format!("{which} GPT header: signature \"EFI PART\" missing at LBA {at_lba}."),
        );
        return None;
    }
    let revision = le32(sec, 8);
    if revision != GPT_REVISION {
        problem(
            problems,
            format!("{which} GPT header: unexpected revision 0x{revision:08X}."),
        );
    }
    let hsize = le32(sec, 12) as usize;
    if hsize < GPT_HEADER_SIZE as usize || hsize > sec.len() {
        problem(
            problems,
            format!("{which} GPT header: invalid header size {hsize}."),
        );
        return None;
    }
    let mut copy = sec[..hsize].to_vec();
    copy[16..20].fill(0);
    let (stored, computed) = (le32(sec, 16), crc32(&copy));
    if stored != computed {
        problem(
            problems,
            format!(
                "{which} GPT header: CRC mismatch (stored 0x{stored:08X}, computed 0x{computed:08X})."
            ),
        );
        return None;
    }
    if le32(sec, 20) != 0 || sec[hsize..].iter().any(|b| *b != 0) {
        problem(
            problems,
            format!("{which} GPT header: reserved bytes are not zero."),
        );
    }
    let my_lba = le64(sec, 24);
    if my_lba != at_lba {
        problem(
            problems,
            format!(
                "{which} GPT header: says it is at LBA {my_lba} but was found at LBA {at_lba}."
            ),
        );
    }
    let mut disk_guid = [0u8; 16];
    disk_guid.copy_from_slice(&sec[56..72]);
    Some(GptHeader {
        alternate: le64(sec, 32),
        first_usable: le64(sec, 40),
        last_usable: le64(sec, 48),
        disk_guid: guid_to_disk(disk_guid),
        entries_lba: le64(sec, 72),
        count: le32(sec, 80),
        entry_size: le32(sec, 84),
        entries_crc: le32(sec, 88),
    })
}

/// Reads and CRC-checks the entry array a header points to.
fn read_gpt_entries(
    dev: &mut dyn BlockDevice,
    s: u64,
    total: u64,
    h: &GptHeader,
    which: &str,
    problems: &mut Vec<String>,
) -> Result<Option<Vec<u8>>> {
    if h.entry_size < 128 || h.entry_size > 4096 || !h.entry_size.is_power_of_two() {
        problem(
            problems,
            format!("{which} GPT header: invalid entry size {}.", h.entry_size),
        );
        return Ok(None);
    }
    if h.count == 0 || h.count > 4096 {
        problem(
            problems,
            format!(
                "{which} GPT header: implausible number of entries ({}).",
                h.count
            ),
        );
        return Ok(None);
    }
    // At most 4096 * 4096 bytes, and inside the device.
    let bytes = h.count as u64 * h.entry_size as u64;
    let sectors = bytes.div_ceil(s);
    let inside = h.entries_lba >= 2
        && h.entries_lba
            .checked_add(sectors)
            .is_some_and(|end| end <= total);
    if !inside {
        problem(
            problems,
            format!(
                "{which} GPT header: the entry array (LBA {}, {sectors} sectors) lies outside the drive.",
                h.entries_lba
            ),
        );
        return Ok(None);
    }
    let mut data = read_lba(dev, s, h.entries_lba, sectors)?;
    data.truncate(bytes as usize);
    let computed = crc32(&data);
    if computed != h.entries_crc {
        problem(
            problems,
            format!(
                "{which} GPT entries: CRC mismatch (header says 0x{:08X}, computed 0x{computed:08X}).",
                h.entries_crc
            ),
        );
        return Ok(None);
    }
    Ok(Some(data))
}

fn check_overlaps(parts: &[FoundPart], problems: &mut Vec<String>) {
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|&i| parts[i].offset);
    for w in order.windows(2) {
        let (a, b) = (&parts[w[0]], &parts[w[1]]);
        if a.offset.saturating_add(a.size) > b.offset {
            problem(
                problems,
                format!("Partitions {} and {} overlap.", w[0] + 1, w[1] + 1),
            );
        }
    }
}

fn read_gpt(
    dev: &mut dyn BlockDevice,
    s: u64,
    total: u64,
    sector0: &[u8],
    table: &mut Table,
) -> Result<()> {
    table.scheme = Some(Scheme::Gpt);
    let problems = &mut table.problems;

    // Protective MBR.
    if sector0[510] != 0x55 || sector0[511] != 0xAA {
        problem(
            problems,
            "Protective MBR: boot signature 0x55AA missing.".to_string(),
        );
    } else {
        let entries = mbr_entries(sector0);
        let protective: Vec<&MbrEntry> = entries.iter().filter(|e| e.kind == 0xEE).collect();
        if protective.len() != 1 {
            problem(
                problems,
                format!(
                    "Protective MBR: expected exactly one entry of type 0xEE, found {}.",
                    protective.len()
                ),
            );
        }
        if entries.iter().any(|e| e.kind != 0xEE && e.kind != 0) {
            problem(
                problems,
                "Protective MBR: contains other partitions (hybrid MBR).".to_string(),
            );
        }
        if let Some(e) = protective.first() {
            let expected = (total - 1).min(u32::MAX as u64) as u32;
            if e.start != 1 {
                problem(
                    problems,
                    format!("Protective MBR: starts at LBA {} instead of 1.", e.start),
                );
            }
            if e.count != expected {
                problem(
                    problems,
                    format!(
                        "Protective MBR: covers {} sectors, expected {expected}.",
                        e.count
                    ),
                );
            }
        }
    }

    if total < 3 {
        problem(problems, "The drive is too small for a GPT.".to_string());
        return Ok(());
    }
    let last = total - 1;
    let primary_sec = read_lba(dev, s, 1, 1)?;
    let backup_sec = read_lba(dev, s, last, 1)?;
    let primary = parse_gpt_header(&primary_sec, 1, "Primary", problems);
    let backup = parse_gpt_header(&backup_sec, last, "Backup", problems);

    let primary_entries = match &primary {
        Some(h) => read_gpt_entries(dev, s, total, h, "Primary", problems)?,
        None => None,
    };
    let backup_entries = match &backup {
        Some(h) => read_gpt_entries(dev, s, total, h, "Backup", problems)?,
        None => None,
    };

    if let Some(h) = &primary {
        if h.alternate != last {
            problem(
                problems,
                format!(
                    "Primary GPT header: backup header expected at LBA {} but the last LBA of the drive is {last}.",
                    h.alternate
                ),
            );
        }
        let sectors = (h.count as u64 * h.entry_size as u64).div_ceil(s);
        if h.entries_lba.saturating_add(sectors) > h.first_usable {
            problem(
                problems,
                "Primary GPT header: the entry array overlaps the usable area.".to_string(),
            );
        }
    }
    if let Some(h) = &backup {
        if h.alternate != 1 {
            problem(
                problems,
                format!(
                    "Backup GPT header: primary header expected at LBA {} instead of 1.",
                    h.alternate
                ),
            );
        }
        let sectors = (h.count as u64 * h.entry_size as u64).div_ceil(s);
        if h.entries_lba <= h.last_usable || h.entries_lba.saturating_add(sectors) > last {
            problem(
                problems,
                "Backup GPT header: the entry array is not between the usable area and the backup header."
                    .to_string(),
            );
        }
    }
    for (which, h) in [("Primary", &primary), ("Backup", &backup)] {
        if let Some(h) = h {
            if h.first_usable > h.last_usable || h.last_usable >= last || h.first_usable < 2 {
                problem(
                    problems,
                    format!(
                        "{which} GPT header: implausible usable range (LBA {} to {}).",
                        h.first_usable, h.last_usable
                    ),
                );
            }
            if h.disk_guid == [0u8; 16] {
                problem(
                    problems,
                    format!("{which} GPT header: the disk GUID is zero."),
                );
            }
        }
    }
    if let (Some(p), Some(b)) = (&primary, &backup) {
        if p.disk_guid != b.disk_guid {
            problem(
                problems,
                "Primary and backup GPT headers carry different disk GUIDs.".to_string(),
            );
        }
        if p.first_usable != b.first_usable || p.last_usable != b.last_usable {
            problem(
                problems,
                "Primary and backup GPT headers disagree about the usable range.".to_string(),
            );
        }
        if p.count != b.count || p.entry_size != b.entry_size || p.entries_crc != b.entries_crc {
            problem(
                problems,
                "Primary and backup GPT headers describe different entry arrays.".to_string(),
            );
        }
    }
    if let (Some(p), Some(b)) = (&primary_entries, &backup_entries) {
        if p != b {
            problem(
                problems,
                "Primary and backup GPT entry arrays differ.".to_string(),
            );
        }
    }

    // Take the partitions from the primary copy when it is intact, else the backup.
    let (header, entries) = match (&primary, &primary_entries, &backup, &backup_entries) {
        (Some(h), Some(e), _, _) => (h, e),
        (_, _, Some(h), Some(e)) => (h, e),
        _ => {
            if let Some(h) = primary.as_ref().or(backup.as_ref()) {
                table.disk_id = DiskId::Gpt(h.disk_guid);
            }
            return Ok(());
        }
    };
    table.disk_id = DiskId::Gpt(header.disk_guid);
    let mut seen_guids: Vec<[u8; 16]> = Vec::new();
    for (i, e) in entries.chunks_exact(header.entry_size as usize).enumerate() {
        if e[0..16].iter().all(|b| *b == 0) {
            continue;
        }
        let n = i + 1;
        let mut type_guid = [0u8; 16];
        type_guid.copy_from_slice(&e[0..16]);
        let type_guid = guid_to_disk(type_guid);
        let mut guid = [0u8; 16];
        guid.copy_from_slice(&e[16..32]);
        let guid = guid_to_disk(guid);
        let (first, last_lba) = (le64(e, 32), le64(e, 40));
        if last_lba < first {
            problem(
                problems,
                format!("GPT entry {n}: ends (LBA {last_lba}) before it starts (LBA {first})."),
            );
            continue;
        }
        if first < header.first_usable || last_lba > header.last_usable {
            problem(
                problems,
                format!("GPT entry {n}: lies outside the usable area (LBA {first} to {last_lba})."),
            );
        }
        let offset = first.checked_mul(s);
        let size = (last_lba - first)
            .checked_add(1)
            .and_then(|c| c.checked_mul(s));
        let (Some(offset), Some(size)) = (offset, size) else {
            problem(problems, format!("GPT entry {n}: out of range."));
            continue;
        };
        if guid == [0u8; 16] {
            problem(problems, format!("GPT entry {n}: the unique GUID is zero."));
        } else if seen_guids.contains(&guid) {
            problem(
                problems,
                format!("GPT entry {n}: repeats the unique GUID of another entry."),
            );
        }
        seen_guids.push(guid);
        let units: Vec<u16> = e[56..128]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|u| *u != 0)
            .collect();
        table.parts.push(FoundPart {
            kind: gpt_kind(&type_guid),
            type_byte: None,
            type_guid: Some(type_guid),
            offset,
            size,
            active: false,
            name: String::from_utf16_lossy(&units),
            guid: Some(guid),
        });
    }
    check_overlaps(&table.parts, problems);
    Ok(())
}

fn read_mbr(s: u64, total: u64, sector0: &[u8], table: &mut Table) {
    table.scheme = Some(Scheme::Mbr);
    table.disk_id = DiskId::Mbr(le32(sector0, 440));
    let entries = mbr_entries(sector0);
    for (i, e) in entries.iter().enumerate() {
        if e.kind == 0 {
            if e.start != 0 || e.count != 0 || e.boot != 0 {
                problem(
                    &mut table.problems,
                    format!("MBR entry {}: type 0 but not empty.", i + 1),
                );
            }
            continue;
        }
        let n = i + 1;
        if e.start == 0 || e.count == 0 {
            problem(
                &mut table.problems,
                format!("MBR entry {n}: invalid start or length."),
            );
        }
        if e.start as u64 + e.count as u64 > total {
            problem(
                &mut table.problems,
                format!("MBR entry {n}: ends past the end of the drive."),
            );
        }
        table.parts.push(FoundPart {
            kind: mbr_kind(e.kind),
            type_byte: Some(e.kind),
            type_guid: None,
            offset: e.start as u64 * s,
            size: e.count as u64 * s,
            active: e.boot == 0x80,
            name: String::new(),
            guid: None,
        });
    }
    if entries.iter().filter(|e| e.boot == 0x80).count() > 1 {
        problem(
            &mut table.problems,
            "MBR: more than one active partition.".to_string(),
        );
    }
    check_overlaps(&table.parts, &mut table.problems);
}

/// Reads the partition table of a device, independently of the writers above.
///
/// For GPT it checks the protective MBR, both headers (signature, revision,
/// size, CRC32, own and alternate LBA, usable range), both entry arrays (CRC32,
/// location) and that the two copies agree; partitions are taken from the
/// primary copy, or from the backup when the primary is damaged. For MBR it
/// checks bounds, overlap and the boot flags (logical partitions inside an
/// extended partition are not walked). Everything inconsistent is described in
/// `problems`; a blank device gives `scheme: None` and no problems. A FAT,
/// exFAT or NTFS boot sector in sector 0 (no partition table) also gives
/// `scheme: None`.
///
/// Never panics on garbage; `Err` is returned only for I/O errors and devices
/// smaller than one sector.
pub fn read_table(dev: &mut dyn BlockDevice) -> Result<Table> {
    let s = sector_of(dev)?;
    let total = dev.size() / s;
    if total == 0 {
        return Err(Error::invalid("The device is smaller than one sector."));
    }
    let mut table = Table {
        scheme: None,
        disk_id: DiskId::None,
        parts: Vec::new(),
        problems: Vec::new(),
    };
    let sector0 = read_lba(dev, s, 0, 1)?;
    let entries = mbr_entries(&sector0);
    let mbr_valid = sector0[510] == 0x55
        && sector0[511] == 0xAA
        && entries.iter().all(|e| e.boot == 0 || e.boot == 0x80)
        && !looks_like_boot_sector(&sector0);
    let protective = mbr_valid && entries.iter().any(|e| e.kind == 0xEE);
    let plain_mbr = mbr_valid && !protective && entries.iter().any(|e| e.kind != 0);
    let primary_sig = total > 1 && &read_lba(dev, s, 1, 1)?[0..8] == GPT_SIGNATURE;
    let backup_sig = total > 2 && &read_lba(dev, s, total - 1, 1)?[0..8] == GPT_SIGNATURE;

    if plain_mbr {
        read_mbr(s, total, &sector0, &mut table);
        if primary_sig {
            problem(
                &mut table.problems,
                "A stale GPT header is still present at LBA 1.".to_string(),
            );
        }
        if backup_sig {
            problem(
                &mut table.problems,
                "A stale backup GPT header is still present at the end of the drive.".to_string(),
            );
        }
    } else if primary_sig || backup_sig || protective {
        read_gpt(dev, s, total, &sector0, &mut table)?;
    } else if mbr_valid {
        read_mbr(s, total, &sector0, &mut table);
    }
    Ok(table)
}

// --------------------------------------------------------------------------------------
// Test support shared with the file-system modules
// --------------------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod testdev {
    use crate::blockdev::BlockDevice;
    use std::collections::HashMap;
    use std::io;

    const BLOCK: u64 = 4096;

    /// A device of any size held sparsely in memory: only blocks that were
    /// written with something other than zeros take space.
    pub(crate) struct SparseDevice {
        size: u64,
        sector: u32,
        blocks: HashMap<u64, Box<[u8]>>,
        pub(crate) writes: u64,
    }

    impl SparseDevice {
        pub(crate) fn new(size: u64, sector: u32) -> SparseDevice {
            SparseDevice {
                size: size - size % sector as u64,
                sector,
                blocks: HashMap::new(),
                writes: 0,
            }
        }

        fn check(&self, offset: u64, len: usize) -> io::Result<()> {
            let s = self.sector as u64;
            let ok = offset.is_multiple_of(s)
                && (len as u64).is_multiple_of(s)
                && offset
                    .checked_add(len as u64)
                    .is_some_and(|e| e <= self.size);
            if ok {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "bad sparse device access",
                ))
            }
        }

        /// Unaligned read for tests.
        pub(crate) fn bytes(&mut self, offset: u64, len: usize) -> Vec<u8> {
            let s = self.sector as u64;
            let first = offset - offset % s;
            let last = (offset + len as u64).div_ceil(s) * s;
            let mut tmp = vec![0u8; (last - first) as usize];
            self.read_at(first, &mut tmp).unwrap();
            tmp[(offset - first) as usize..(offset - first) as usize + len].to_vec()
        }
    }

    impl BlockDevice for SparseDevice {
        fn sector_size(&self) -> u32 {
            self.sector
        }
        fn size(&self) -> u64 {
            self.size
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            self.check(offset, buf.len())?;
            let (mut pos, mut done) = (offset, 0usize);
            while done < buf.len() {
                let (bi, bo) = (pos / BLOCK, (pos % BLOCK) as usize);
                let n = (BLOCK as usize - bo).min(buf.len() - done);
                match self.blocks.get(&bi) {
                    Some(b) => buf[done..done + n].copy_from_slice(&b[bo..bo + n]),
                    None => buf[done..done + n].fill(0),
                }
                pos += n as u64;
                done += n;
            }
            Ok(())
        }
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
            self.check(offset, buf.len())?;
            self.writes += 1;
            let (mut pos, mut done) = (offset, 0usize);
            while done < buf.len() {
                let (bi, bo) = (pos / BLOCK, (pos % BLOCK) as usize);
                let n = (BLOCK as usize - bo).min(buf.len() - done);
                let src = &buf[done..done + n];
                let zero = src.iter().all(|b| *b == 0);
                if zero && n == BLOCK as usize {
                    self.blocks.remove(&bi);
                } else if !zero || self.blocks.contains_key(&bi) {
                    let block = self
                        .blocks
                        .entry(bi)
                        .or_insert_with(|| vec![0u8; BLOCK as usize].into_boxed_slice());
                    block[bo..bo + n].copy_from_slice(src);
                }
                pos += n as u64;
                done += n;
            }
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A small deterministic generator for test data.
    pub(crate) struct XorShift(pub(crate) u64);

    impl XorShift {
        pub(crate) fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        pub(crate) fn fill(&mut self, buf: &mut [u8]) {
            for chunk in buf.chunks_mut(8) {
                let v = self.next().to_le_bytes();
                chunk.copy_from_slice(&v[..chunk.len()]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testdev::{SparseDevice, XorShift};
    use super::*;
    use crate::blockdev::{FileDevice, MemDevice};

    const GIB: u64 = 1024 * MIB;

    /// Bit-by-bit CRC-32 (reflected polynomial 0xEDB88320), independent of crc32fast.
    fn crc32_bitwise(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ 0xEDB8_8320
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    fn guid(n: u8) -> [u8; 16] {
        let mut g = [n; 16];
        g[6] = 0x40 | (n & 0x0F);
        g[8] = 0x80 | (n & 0x3F);
        g
    }

    fn spec(kind: PartType, offset: u64, size: u64, name: &str, n: u8) -> PartSpec {
        PartSpec {
            kind,
            offset,
            size,
            name: name.to_string(),
            active: false,
            guid: guid(n),
        }
    }

    #[test]
    fn crc32_known_answers() {
        // The standard check value of CRC-32/ISO-HDLC, and the one-byte case
        // worked by hand: crc("a") = 0xE8B7BE43.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_bitwise(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
        assert_eq!(crc32_bitwise(b"a"), 0xE8B7_BE43);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(&[0u8; 4]), 0x2144_DF1C);
        assert_eq!(crc32_bitwise(&[0u8; 4]), 0x2144_DF1C);
    }

    #[test]
    fn guid_encoding_and_text() {
        assert_eq!(
            guid_to_disk(GUID_EFI_SYSTEM),
            [
                0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
                0xC9, 0x3B
            ]
        );
        assert_eq!(guid_to_disk(guid_to_disk(GUID_BASIC_DATA)), GUID_BASIC_DATA);
        assert_eq!(
            format_guid(&GUID_EFI_SYSTEM),
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        );
        assert_eq!(
            format_guid(&GUID_BASIC_DATA),
            "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        );
        assert_eq!(PartType::EfiSystem.mbr_type(), 0xEF);
        assert_eq!(PartType::BasicData.mbr_type(), 0x0C);
        assert_eq!(PartType::Fat32Lba.mbr_type(), 0x0C);
        assert_eq!(PartType::ExFatOrNtfs.mbr_type(), 0x07);
        assert_eq!(PartType::Fat32Lba.gpt_type(), GUID_BASIC_DATA);
        assert_eq!(PartType::ExFatOrNtfs.gpt_type(), GUID_BASIC_DATA);
        assert_eq!(PartType::EfiSystem.gpt_type(), GUID_EFI_SYSTEM);
    }

    #[test]
    fn random_values() {
        let a = random_guid();
        let b = random_guid();
        assert_ne!(a, b);
        for g in [a, b] {
            assert_eq!(g[6] >> 4, 4, "version 4");
            assert_eq!(g[8] >> 6, 0b10, "RFC 4122 variant");
        }
        // In text form: xxxxxxxx-xxxx-4xxx-[89AB]xxx-...
        let text = format_guid(&a);
        assert_eq!(text.as_bytes()[14], b'4');
        assert!(b"89AB".contains(&text.as_bytes()[19]));
        let values: Vec<u32> = (0..16).map(|_| random_u32()).collect();
        assert!(values.iter().all(|v| *v != 0));
        assert!(values.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn chs_values() {
        assert_eq!(chs(0), Some([0, 1, 0]));
        assert_eq!(chs(1), Some([0, 2, 0]));
        assert_eq!(chs(2048), Some([32, 33, 0]));
        // LBA 16065 = cylinder 1, head 0, sector 1.
        assert_eq!(chs(16065), Some([0, 1, 1]));
        // Last addressable: cylinder 1023, head 254, sector 63.
        assert_eq!(chs(1024 * 255 * 63 - 1), Some([0xFE, 0xFF, 0xFF]));
        assert_eq!(chs(1024 * 255 * 63), None);
    }

    #[test]
    fn single_partition_layouts() {
        // 8 GiB, 512: MBR uses everything from 1 MiB; GPT loses the last MiB to the backup.
        assert_eq!(
            single_partition(8 * GIB, 512, Scheme::Mbr).unwrap(),
            (MIB, 8 * GIB - MIB)
        );
        assert_eq!(
            single_partition(8 * GIB, 512, Scheme::Gpt).unwrap(),
            (MIB, 8 * GIB - 2 * MIB)
        );
        // Odd size: 7.5 GiB plus 300 sectors.
        let odd = 7 * GIB + 512 * MIB + 300 * 512;
        assert_eq!(
            single_partition(odd, 512, Scheme::Mbr).unwrap(),
            (MIB, 7 * GIB + 511 * MIB)
        );
        // The 300 spare sectors hold the 33 backup sectors: the full MiB is usable.
        assert_eq!(
            single_partition(odd, 512, Scheme::Gpt).unwrap(),
            (MIB, 7 * GIB + 511 * MIB)
        );
        // 4096-byte sectors: backup GPT needs 5 sectors.
        assert_eq!(
            single_partition(GIB + 5 * 4096, 4096, Scheme::Gpt).unwrap(),
            (MIB, GIB - MIB)
        );
        assert_eq!(
            single_partition(GIB + 4 * 4096, 4096, Scheme::Gpt).unwrap(),
            (MIB, GIB - 2 * MIB)
        );
        // Too small.
        assert!(single_partition(MIB, 512, Scheme::Mbr).is_err());
        assert!(single_partition(2 * MIB - 512, 512, Scheme::Mbr).is_err());
        assert!(single_partition(2 * MIB, 512, Scheme::Mbr).is_ok());
        assert!(single_partition(2 * MIB, 512, Scheme::Gpt).is_err());
        assert!(single_partition(0, 512, Scheme::Gpt).is_err());
        assert!(single_partition(GIB, 1000, Scheme::Gpt).is_err());
        // MBR limit: 2^32 sectors.
        let two_tib = 512u64 << 32;
        assert!(single_partition(two_tib - 512, 512, Scheme::Mbr).is_ok());
        assert!(single_partition(two_tib, 512, Scheme::Mbr).is_err());
        assert!(single_partition(two_tib, 4096, Scheme::Mbr).is_ok());
        assert!(single_partition(two_tib, 512, Scheme::Gpt).is_ok());
        // The result is accepted by the writers.
        for scheme in [Scheme::Mbr, Scheme::Gpt] {
            let mut d = MemDevice::new(9 * MIB + 512 * 77, 512);
            let (offset, size) = single_partition(d.size(), 512, scheme).unwrap();
            let p = [spec(PartType::Fat32Lba, offset, size, "Data", 1)];
            match scheme {
                Scheme::Mbr => write_mbr(&mut d, &p, 1).unwrap(),
                Scheme::Gpt => write_gpt(&mut d, &p, guid(9)).unwrap(),
            }
            let t = read_table(&mut d).unwrap();
            assert_eq!(t.problems, Vec::<String>::new());
            assert_eq!((t.parts[0].offset, t.parts[0].size), (offset, size));
        }
    }

    #[test]
    fn mbr_round_trip_512() {
        let mut d = MemDevice::new(64 * MIB + 512 * 3, 512);
        let mut parts = vec![
            spec(PartType::EfiSystem, MIB, 32 * MIB, "", 1),
            spec(PartType::ExFatOrNtfs, 33 * MIB, 31 * MIB, "", 2),
        ];
        parts[0].active = true;
        write_mbr(&mut d, &parts, 0xDEAD_BEEF).unwrap();
        assert_eq!(d.writes, 1);

        // Byte-level layout.
        assert!(
            d.data[..440].iter().all(|b| *b == 0),
            "boot code area is zero"
        );
        assert_eq!(&d.data[440..444], &[0xEF, 0xBE, 0xAD, 0xDE]);
        assert_eq!(&d.data[444..446], &[0, 0]);
        let e = &d.data[446..462];
        assert_eq!(e[0], 0x80);
        assert_eq!(&e[1..4], &[32, 33, 0], "CHS of LBA 2048");
        assert_eq!(e[4], 0xEF);
        // Last LBA 67583 = cylinder 4, head 52, sector 48.
        assert_eq!(&e[5..8], &[52, 48, 4]);
        assert_eq!(&e[8..12], &2048u32.to_le_bytes());
        assert_eq!(&e[12..16], &65536u32.to_le_bytes());
        let e = &d.data[462..478];
        assert_eq!(e[0], 0x00);
        assert_eq!(e[4], 0x07);
        assert_eq!(&e[8..12], &67584u32.to_le_bytes());
        assert_eq!(&e[12..16], &63488u32.to_le_bytes());
        assert!(d.data[478..510].iter().all(|b| *b == 0));
        assert_eq!(&d.data[510..512], &[0x55, 0xAA]);

        let t = read_table(&mut d).unwrap();
        assert_eq!(t.scheme, Some(Scheme::Mbr));
        assert_eq!(t.disk_id, DiskId::Mbr(0xDEAD_BEEF));
        assert_eq!(t.problems, Vec::<String>::new());
        assert_eq!(t.parts.len(), 2);
        assert_eq!(t.parts[0].kind, "EFI System (0xEF)");
        assert_eq!(t.parts[0].type_byte, Some(0xEF));
        assert_eq!(t.parts[0].type_guid, None);
        assert_eq!((t.parts[0].offset, t.parts[0].size), (MIB, 32 * MIB));
        assert!(t.parts[0].active);
        assert_eq!(t.parts[1].kind, "NTFS/exFAT (0x07)");
        assert_eq!((t.parts[1].offset, t.parts[1].size), (33 * MIB, 31 * MIB));
        assert!(!t.parts[1].active);
    }

    #[test]
    fn mbr_round_trip_4096_and_large() {
        // 4096-byte sectors: LBAs count 4 KiB sectors, the sector tail stays zero.
        let mut d = MemDevice::new(16 * MIB, 4096);
        let parts = [spec(PartType::Fat32Lba, MIB, 15 * MIB, "", 1)];
        write_mbr(&mut d, &parts, 7).unwrap();
        assert_eq!(&d.data[454..458], &256u32.to_le_bytes());
        assert_eq!(&d.data[458..462], &3840u32.to_le_bytes());
        assert_eq!(d.data[450], 0x0C);
        assert_eq!(&d.data[510..512], &[0x55, 0xAA]);
        assert!(d.data[512..4096].iter().all(|b| *b == 0));
        let t = read_table(&mut d).unwrap();
        assert_eq!(t.problems, Vec::<String>::new());
        assert_eq!((t.parts[0].offset, t.parts[0].size), (MIB, 15 * MIB));
        assert_eq!(t.parts[0].kind, "FAT32 LBA (0x0C)");

        // A 100 GiB disk: the end is beyond CHS, so the conventional value is stored.
        let mut d = SparseDevice::new(100 * GIB + 512 * 1001, 512);
        let (offset, size) = single_partition(d.size(), 512, Scheme::Mbr).unwrap();
        write_mbr(&mut d, &[spec(PartType::Fat32Lba, offset, size, "", 1)], 9).unwrap();
        let s0 = d.bytes(0, 512);
        assert_eq!(&s0[447..450], &[32, 33, 0]);
        assert_eq!(&s0[451..454], &[0xFE, 0xFF, 0xFF]);
        let t = read_table(&mut d).unwrap();
        assert_eq!(t.problems, Vec::<String>::new());
        assert_eq!((t.parts[0].offset, t.parts[0].size), (MIB, 100 * GIB - MIB));
    }

    /// Checks a written GPT byte by byte against values computed here.
    fn assert_gpt_bytes(d: &mut SparseDevice, parts: &[PartSpec], disk_guid: [u8; 16]) {
        let s = d.sector_size() as u64;
        let total = d.size() / s;
        let es: u64 = if s == 512 { 32 } else { 4 };
        let last = total - 1;

        // Protective MBR.
        let m = d.bytes(0, s as usize);
        assert!(m[..446].iter().all(|b| *b == 0));
        assert_eq!(m[446], 0x00);
        assert_eq!(&m[447..450], &[0x00, 0x02, 0x00]);
        assert_eq!(m[450], 0xEE);
        assert_eq!(&m[454..458], &1u32.to_le_bytes());
        let expect_size = (total - 1).min(0xFFFF_FFFF) as u32;
        assert_eq!(&m[458..462], &expect_size.to_le_bytes());
        assert!(m[462..510].iter().all(|b| *b == 0));
        assert_eq!(&m[510..512], &[0x55, 0xAA]);

        // Primary header.
        let h = d.bytes(s, s as usize);
        assert_eq!(&h[0..8], b"EFI PART");
        assert_eq!(&h[8..12], &[0x00, 0x00, 0x01, 0x00]);
        assert_eq!(&h[12..16], &92u32.to_le_bytes());
        assert_eq!(&h[20..24], &[0; 4]);
        assert_eq!(&h[24..32], &1u64.to_le_bytes());
        assert_eq!(&h[32..40], &last.to_le_bytes());
        assert_eq!(&h[40..48], &(2 + es).to_le_bytes());
        assert_eq!(&h[48..56], &(total - es - 2).to_le_bytes());
        assert_eq!(&h[56..72], &guid_to_disk(disk_guid));
        assert_eq!(&h[72..80], &2u64.to_le_bytes());
        assert_eq!(&h[80..84], &128u32.to_le_bytes());
        assert_eq!(&h[84..88], &128u32.to_le_bytes());
        assert!(h[92..].iter().all(|b| *b == 0));
        let mut copy = h[..92].to_vec();
        copy[16..20].fill(0);
        assert_eq!(le32(&h, 16), crc32_bitwise(&copy), "primary header CRC");

        // Entries.
        let entries = d.bytes(2 * s, 128 * 128);
        assert_eq!(le32(&h, 88), crc32_bitwise(&entries), "entry array CRC");
        for (i, p) in parts.iter().enumerate() {
            let e = &entries[i * 128..(i + 1) * 128];
            assert_eq!(&e[0..16], &guid_to_disk(p.kind.gpt_type()));
            assert_eq!(&e[16..32], &guid_to_disk(p.guid));
            assert_eq!(&e[32..40], &(p.offset / s).to_le_bytes());
            assert_eq!(&e[40..48], &((p.offset + p.size) / s - 1).to_le_bytes());
            assert_eq!(&e[48..56], &[0; 8]);
            let name: Vec<u8> = p
                .name
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect();
            assert_eq!(&e[56..56 + name.len()], &name[..]);
            assert!(e[56 + name.len()..].iter().all(|b| *b == 0));
        }
        assert!(entries[parts.len() * 128..].iter().all(|b| *b == 0));

        // Backup header at the last LBA, backup entries right before it.
        let b = d.bytes(last * s, s as usize);
        assert_eq!(&b[0..8], b"EFI PART");
        assert_eq!(&b[24..32], &last.to_le_bytes());
        assert_eq!(&b[32..40], &1u64.to_le_bytes());
        assert_eq!(&b[72..80], &(last - es).to_le_bytes());
        assert_eq!(&b[40..56], &h[40..56]);
        assert_eq!(&b[56..72], &h[56..72]);
        assert_eq!(&b[80..92], &h[80..92]);
        let mut copy = b[..92].to_vec();
        copy[16..20].fill(0);
        assert_eq!(le32(&b, 16), crc32_bitwise(&copy), "backup header CRC");
        assert_eq!(d.bytes((last - es) * s, 128 * 128), entries);
    }

    #[test]
    fn gpt_round_trip_many_sizes() {
        let cases: [(u64, u32); 6] = [
            (64 * MIB, 512),
            (100 * MIB + 7 * 512, 512),
            (3 * MIB + 512, 512),
            (GIB + 3 * 4096, 4096),
            (16 * MIB, 4096),
            (30 * GIB + 123 * 512, 512),
        ];
        for (size, sector) in cases {
            let mut d = SparseDevice::new(size, sector);
            let (offset, len) = single_partition(size, sector, Scheme::Gpt).unwrap();
            let s = sector as u64;
            // Split in two when there is room: an ESP and a data partition.
            let parts = if len >= 4 * MIB {
                vec![
                    spec(PartType::EfiSystem, offset, MIB, "EFI system partition", 1),
                    spec(
                        PartType::BasicData,
                        offset + MIB,
                        len - MIB,
                        "Daten \u{00e4}\u{1F600}",
                        2,
                    ),
                ]
            } else {
                vec![spec(PartType::Fat32Lba, offset, len, "BOOTFORGE", 1)]
            };
            let disk_guid = guid(0x5A);
            write_gpt(&mut d, &parts, disk_guid).unwrap();
            assert_gpt_bytes(&mut d, &parts, disk_guid);

            let t = read_table(&mut d).unwrap();
            assert_eq!(
                t.problems,
                Vec::<String>::new(),
                "size {size} sector {sector}"
            );
            assert_eq!(t.scheme, Some(Scheme::Gpt));
            assert_eq!(t.disk_id, DiskId::Gpt(disk_guid));
            assert_eq!(t.parts.len(), parts.len());
            for (f, p) in t.parts.iter().zip(&parts) {
                assert_eq!((f.offset, f.size), (p.offset, p.size));
                assert_eq!(f.name, p.name);
                assert_eq!(f.guid, Some(p.guid));
                assert_eq!(f.type_guid, Some(p.kind.gpt_type()));
                assert_eq!(f.type_byte, None);
                assert!(!f.active);
                assert!(f.offset.is_multiple_of(s));
            }
            assert_eq!(
                t.parts[0].kind,
                if parts.len() == 2 {
                    "EFI System"
                } else {
                    "Basic data"
                }
            );
        }
    }

    #[test]
    fn gpt_raw_bytes_of_the_efi_type_guid() {
        let mut d = MemDevice::new(8 * MIB, 512);
        let parts = [spec(PartType::EfiSystem, MIB, 6 * MIB, "ESP", 3)];
        write_gpt(&mut d, &parts, guid(1)).unwrap();
        // First entry at LBA 2.
        assert_eq!(
            &d.data[1024..1040],
            &[
                0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
                0xC9, 0x3B
            ]
        );
        assert_eq!(&d.data[512..520], b"EFI PART");
        assert_eq!(&d.data[520..524], &[0x00, 0x00, 0x01, 0x00]);
        assert_eq!(&d.data[524..528], &[92, 0, 0, 0]);
        let last = 8 * MIB as usize - 512;
        assert_eq!(&d.data[last..last + 8], b"EFI PART");
        // First usable LBA 34, last usable = 16384 - 34 = 16350.
        assert_eq!(&d.data[552..560], &34u64.to_le_bytes());
        assert_eq!(&d.data[560..568], &16350u64.to_le_bytes());
        // 4096-byte sectors: first usable LBA 6.
        let mut d = MemDevice::new(8 * MIB, 4096);
        write_gpt(&mut d, &parts, guid(1)).unwrap();
        assert_eq!(&d.data[4096..4104], b"EFI PART");
        assert_eq!(&d.data[4096 + 40..4096 + 48], &6u64.to_le_bytes());
        assert_eq!(&d.data[4096 + 48..4096 + 56], &(2048u64 - 6).to_le_bytes());
        assert_eq!(&d.data[8192..8196], &[0x28, 0x73, 0x2A, 0xC1]);
        assert_eq!(read_table(&mut d).unwrap().problems, Vec::<String>::new());
    }

    #[test]
    fn gpt_protective_mbr_is_clamped_on_huge_disks() {
        // 3 TiB at 512 bytes per sector: more than 2^32 sectors.
        let size = 3 * 1024 * GIB;
        let mut d = SparseDevice::new(size, 512);
        let (offset, len) = single_partition(size, 512, Scheme::Gpt).unwrap();
        assert_eq!((offset, len), (MIB, size - 2 * MIB));
        let parts = [spec(PartType::BasicData, offset, len, "Big", 1)];
        write_gpt(&mut d, &parts, guid(2)).unwrap();
        assert_gpt_bytes(&mut d, &parts, guid(2));
        let m = d.bytes(0, 512);
        assert_eq!(m[450], 0xEE);
        assert_eq!(&m[451..454], &[0xFF, 0xFF, 0xFF]);
        assert_eq!(&m[458..462], &[0xFF, 0xFF, 0xFF, 0xFF]);
        let t = read_table(&mut d).unwrap();
        assert_eq!(t.problems, Vec::<String>::new());
        assert_eq!(t.parts[0].size, len);
        // MBR refuses a partition that needs more than 32 bits.
        let mut d = SparseDevice::new(size, 512);
        let e = write_mbr(
            &mut d,
            &[spec(PartType::BasicData, MIB, size - MIB, "", 1)],
            1,
        );
        assert!(matches!(e, Err(Error::Invalid(_))));
        assert_eq!(d.writes, 0);
    }

    #[test]
    fn gpt_on_an_image_file() {
        let path =
            std::env::temp_dir().join(format!("bootforge-part-{}-gpt.img", std::process::id()));
        {
            let mut d = FileDevice::create(&path, 12 * MIB + 512 * 5, 512).unwrap();
            let (offset, len) = single_partition(d.size(), 512, Scheme::Gpt).unwrap();
            let parts = [spec(PartType::BasicData, offset, len, "Image", 4)];
            write_gpt(&mut d, &parts, guid(8)).unwrap();
        }
        let mut d = FileDevice::open(&path, 512).unwrap();
        let t = read_table(&mut d).unwrap();
        drop(d);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(t.problems, Vec::<String>::new());
        assert_eq!(t.scheme, Some(Scheme::Gpt));
        assert_eq!(t.parts.len(), 1);
        assert_eq!((t.parts[0].offset, t.parts[0].size), (MIB, 10 * MIB));
        assert_eq!(t.parts[0].name, "Image");
    }

    fn sample_gpt() -> MemDevice {
        let mut d = MemDevice::new(16 * MIB, 512);
        let parts = [
            spec(PartType::EfiSystem, MIB, 4 * MIB, "ESP", 1),
            spec(PartType::BasicData, 5 * MIB, 10 * MIB, "Data", 2),
        ];
        write_gpt(&mut d, &parts, guid(7)).unwrap();
        assert_eq!(read_table(&mut d).unwrap().problems, Vec::<String>::new());
        d
    }

    fn has(t: &Table, needle: &str) -> bool {
        t.problems.iter().any(|p| p.contains(needle))
    }

    #[test]
    fn reader_reports_gpt_corruption() {
        let last = 16 * MIB as usize - 512;

        // A flipped byte in the primary header: CRC problem, partitions come from the backup.
        let mut d = sample_gpt();
        d.data[512 + 41] ^= 0x01;
        let t = read_table(&mut d).unwrap();
        assert!(
            has(&t, "Primary GPT header: CRC mismatch"),
            "{:?}",
            t.problems
        );
        assert_eq!(t.scheme, Some(Scheme::Gpt));
        assert_eq!(t.parts.len(), 2);
        assert_eq!(t.disk_id, DiskId::Gpt(guid(7)));

        // A flipped byte in the primary entries.
        let mut d = sample_gpt();
        d.data[1024 + 33] ^= 0x10;
        let t = read_table(&mut d).unwrap();
        assert!(
            has(&t, "Primary GPT entries: CRC mismatch"),
            "{:?}",
            t.problems
        );
        assert_eq!(t.parts.len(), 2);
        assert_eq!(t.parts[0].offset, MIB, "taken from the backup copy");

        // A flipped byte in the backup header.
        let mut d = sample_gpt();
        d.data[last + 60] ^= 0x80;
        let t = read_table(&mut d).unwrap();
        assert!(
            has(&t, "Backup GPT header: CRC mismatch"),
            "{:?}",
            t.problems
        );
        assert_eq!(t.parts.len(), 2);

        // A flipped byte in the backup entries.
        let mut d = sample_gpt();
        d.data[last - 32 * 512 + 130] ^= 0x01;
        let t = read_table(&mut d).unwrap();
        assert!(
            has(&t, "Backup GPT entries: CRC mismatch"),
            "{:?}",
            t.problems
        );

        // Backup wiped entirely.
        let mut d = sample_gpt();
        d.data[last..].fill(0);
        let t = read_table(&mut d).unwrap();
        assert!(has(&t, "Backup GPT header: signature"), "{:?}", t.problems);
        assert_eq!(t.parts.len(), 2);

        // Both headers gone but the protective MBR remains.
        let mut d = sample_gpt();
        d.data[last..].fill(0);
        d.data[512..1024].fill(0);
        let t = read_table(&mut d).unwrap();
        assert_eq!(t.scheme, Some(Scheme::Gpt));
        assert!(t.parts.is_empty());
        assert!(has(&t, "Primary GPT header: signature"));
        assert!(has(&t, "Backup GPT header: signature"));

        // Protective MBR damaged.
        let mut d = sample_gpt();
        d.data[510] = 0;
        let t = read_table(&mut d).unwrap();
        assert!(
            has(&t, "Protective MBR: boot signature"),
            "{:?}",
            t.problems
        );
        assert_eq!(t.parts.len(), 2);
        let mut d = sample_gpt();
        d.data[458] ^= 0x01;
        let t = read_table(&mut d).unwrap();
        assert!(has(&t, "Protective MBR: covers"), "{:?}", t.problems);
        let mut d = sample_gpt();
        d.data[450] = 0x0C;
        let t = read_table(&mut d).unwrap();
        // Now it reads as a plain MBR with stale GPT headers.
        assert_eq!(t.scheme, Some(Scheme::Mbr));
        assert!(has(&t, "stale GPT header"), "{:?}", t.problems);
        assert!(has(&t, "stale backup GPT header"), "{:?}", t.problems);
    }

    /// Recomputes the header CRC after a deliberate edit, so the edit itself is examined.
    fn reseal(d: &mut MemDevice, header_at: usize) {
        d.data[header_at + 16..header_at + 20].fill(0);
        let crc = crc32_bitwise(&d.data[header_at..header_at + 92]);
        d.data[header_at + 16..header_at + 20].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn reader_reports_gpt_inconsistencies_with_valid_crcs() {
        let last = 16 * MIB as usize - 512;

        // The image was copied to a larger device: the backup is no longer at the end.
        let small = sample_gpt();
        let mut d = MemDevice::new(20 * MIB, 512);
        d.data[..16 * MIB as usize].copy_from_slice(&small.data);
        let t = read_table(&mut d).unwrap();
        assert!(
            has(&t, "backup header expected at LBA 32767"),
            "{:?}",
            t.problems
        );
        assert!(has(&t, "Backup GPT header: signature"));
        assert!(has(&t, "Protective MBR: covers"));
        assert_eq!(t.parts.len(), 2);

        // Wrong MyLBA with a valid CRC.
        let mut d = sample_gpt();
        d.data[512 + 24] = 5;
        reseal(&mut d, 512);
        let t = read_table(&mut d).unwrap();
        assert!(has(&t, "says it is at LBA 5"), "{:?}", t.problems);

        // Backup pointing at the wrong primary.
        let mut d = sample_gpt();
        d.data[last + 32] = 9;
        reseal(&mut d, last);
        let t = read_table(&mut d).unwrap();
        assert!(
            has(&t, "primary header expected at LBA 9"),
            "{:?}",
            t.problems
        );

        // Different disk GUIDs.
        let mut d = sample_gpt();
        d.data[last + 56] ^= 0xFF;
        reseal(&mut d, last);
        let t = read_table(&mut d).unwrap();
        assert!(has(&t, "different disk GUIDs"), "{:?}", t.problems);

        // Wrong revision.
        let mut d = sample_gpt();
        d.data[512 + 10] = 2;
        reseal(&mut d, 512);
        let t = read_table(&mut d).unwrap();
        assert!(has(&t, "unexpected revision"), "{:?}", t.problems);

        // A huge entry count with a valid header CRC must not allocate or read wildly.
        let mut d = sample_gpt();
        d.data[512 + 80..512 + 84].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        reseal(&mut d, 512);
        let t = read_table(&mut d).unwrap();
        assert!(has(&t, "implausible number of entries"), "{:?}", t.problems);
        assert_eq!(t.parts.len(), 2, "backup still usable");

        // Entry array pointing outside the device.
        let mut d = sample_gpt();
        d.data[512 + 72..512 + 80].copy_from_slice(&u64::MAX.to_le_bytes());
        reseal(&mut d, 512);
        let t = read_table(&mut d).unwrap();
        assert!(has(&t, "lies outside the drive"), "{:?}", t.problems);

        // Overlapping entries and an entry outside the usable area, both copies consistent.
        let mut d = sample_gpt();
        let backup_entries = last - 32 * 512;
        for base in [1024usize, backup_entries] {
            // Second entry starts inside the first.
            d.data[base + 128 + 32..base + 128 + 40].copy_from_slice(&4096u64.to_le_bytes());
            // Third entry: beyond the last usable LBA.
            d.data[base + 256..base + 272].copy_from_slice(&guid_to_disk(GUID_BASIC_DATA));
            d.data[base + 272..base + 288].copy_from_slice(&guid(3));
            d.data[base + 288..base + 296].copy_from_slice(&32760u64.to_le_bytes());
            d.data[base + 296..base + 304].copy_from_slice(&40000u64.to_le_bytes());
        }
        let crc = crc32_bitwise(&d.data[1024..1024 + 16384]);
        for h in [512usize, last] {
            d.data[h + 88..h + 92].copy_from_slice(&crc.to_le_bytes());
            reseal(&mut d, h);
        }
        let t = read_table(&mut d).unwrap();
        assert!(has(&t, "overlap"), "{:?}", t.problems);
        assert!(
            has(&t, "GPT entry 3: lies outside the usable area"),
            "{:?}",
            t.problems
        );
        assert_eq!(t.problems.len(), 2, "{:?}", t.problems);
    }

    #[test]
    fn reader_reports_mbr_problems() {
        let mut d = MemDevice::new(8 * MIB, 512);
        let parts = [
            spec(PartType::Fat32Lba, MIB, 3 * MIB, "", 1),
            spec(PartType::ExFatOrNtfs, 4 * MIB, 4 * MIB, "", 2),
        ];
        write_mbr(&mut d, &parts, 5).unwrap();
        assert_eq!(read_table(&mut d).unwrap().problems, Vec::<String>::new());

        // Entry running past the end of the device.
        let mut bad = MemDevice::new(8 * MIB, 512);
        bad.data.copy_from_slice(&d.data);
        bad.data[462 + 12..462 + 16].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
        let t = read_table(&mut bad).unwrap();
        assert!(
            has(&t, "MBR entry 2: ends past the end"),
            "{:?}",
            t.problems
        );

        // Overlap.
        let mut bad = MemDevice::new(8 * MIB, 512);
        bad.data.copy_from_slice(&d.data);
        bad.data[462 + 8..462 + 12].copy_from_slice(&4096u32.to_le_bytes());
        let t = read_table(&mut bad).unwrap();
        assert!(has(&t, "overlap"), "{:?}", t.problems);

        // Two active partitions.
        let mut bad = MemDevice::new(8 * MIB, 512);
        bad.data.copy_from_slice(&d.data);
        bad.data[446] = 0x80;
        bad.data[462] = 0x80;
        let t = read_table(&mut bad).unwrap();
        assert!(has(&t, "more than one active"), "{:?}", t.problems);

        // No signature: not a table at all.
        let mut bad = MemDevice::new(8 * MIB, 512);
        bad.data.copy_from_slice(&d.data);
        bad.data[511] = 0;
        let t = read_table(&mut bad).unwrap();
        assert_eq!(t.scheme, None);
        assert!(t.parts.is_empty());

        // A FAT32 boot sector is not an MBR.
        let mut fs = MemDevice::new(8 * MIB, 512);
        fs.data[82..90].copy_from_slice(b"FAT32   ");
        fs.data[510] = 0x55;
        fs.data[511] = 0xAA;
        fs.data[450] = 0x0C;
        let t = read_table(&mut fs).unwrap();
        assert_eq!(t.scheme, None);
        assert!(t.problems.is_empty());
    }

    #[test]
    fn blank_garbage_and_truncated_devices_never_panic() {
        // Blank devices of awkward sizes.
        for size in [512u64, 1024, 1536, 4096, 33 * 512, 67 * 512, MIB] {
            let mut d = MemDevice::new(size, 512);
            let t = read_table(&mut d).unwrap();
            assert_eq!(t.scheme, None);
            assert!(t.problems.is_empty());
        }
        assert!(read_table(&mut MemDevice::new(0, 512)).is_err());
        assert!(read_table(&mut MemDevice::new(100, 512)).is_err());

        // Random garbage, with and without the signatures that lead deeper into the parser.
        let mut rng = XorShift(0x1234_5678_9ABC_DEF1);
        for round in 0..200u32 {
            let sector = if round % 2 == 0 { 512 } else { 4096 };
            let sectors = 1 + rng.next() % 80;
            let mut d = MemDevice::new(sectors * sector as u64, sector);
            rng.fill(&mut d.data);
            let s = sector as usize;
            if round % 3 != 0 {
                d.data[510] = 0x55;
                d.data[511] = 0xAA;
                for i in 0..4 {
                    d.data[446 + i * 16] &= 0x80;
                }
                if round % 5 == 0 {
                    d.data[450] = 0xEE;
                }
            }
            if round % 4 != 0 && d.data.len() >= 3 * s {
                d.data[s..s + 8].copy_from_slice(b"EFI PART");
                let end = d.data.len() - s;
                d.data[end..end + 8].copy_from_slice(b"EFI PART");
                if round % 8 < 4 {
                    // A valid CRC over a garbage header, to get past the CRC gate.
                    d.data[s + 12..s + 16].copy_from_slice(&92u32.to_le_bytes());
                    reseal(&mut d, s);
                    d.data[end + 12..end + 16].copy_from_slice(&92u32.to_le_bytes());
                    reseal(&mut d, end);
                }
            }
            let _ = read_table(&mut d).unwrap();
        }

        // A valid GPT cut short at every interesting length.
        let full = sample_gpt();
        for sectors in [1usize, 2, 3, 33, 34, 35, 100, 2048, 32767] {
            let mut d = MemDevice::new(sectors as u64 * 512, 512);
            d.data.copy_from_slice(&full.data[..sectors * 512]);
            let t = read_table(&mut d).unwrap();
            assert_eq!(t.scheme, Some(Scheme::Gpt));
            assert!(!t.problems.is_empty(), "{sectors} sectors");
        }
    }

    #[test]
    fn writers_refuse_bad_input_without_writing() {
        let fresh = || MemDevice::new(16 * MIB, 512);
        let ok = |d: &MemDevice| assert_eq!(d.writes, 0);

        // Overlapping partitions.
        let overlapping = [
            spec(PartType::BasicData, MIB, 4 * MIB, "a", 1),
            spec(PartType::BasicData, 4 * MIB, 4 * MIB, "b", 2),
        ];
        let mut d = fresh();
        assert!(matches!(
            write_gpt(&mut d, &overlapping, guid(9)),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            write_mbr(&mut d, &overlapping, 1),
            Err(Error::Invalid(_))
        ));
        ok(&d);
        // Given in reverse order they still overlap.
        let reversed = [overlapping[1].clone(), overlapping[0].clone()];
        assert!(write_gpt(&mut d, &reversed, guid(9)).is_err());
        ok(&d);

        // Past the end (MBR), into the backup GPT (GPT).
        let past = [spec(PartType::BasicData, MIB, 16 * MIB, "a", 1)];
        assert!(write_mbr(&mut d, &past, 1).is_err());
        assert!(write_gpt(&mut d, &past, guid(9)).is_err());
        let into_backup = [spec(PartType::BasicData, MIB, 15 * MIB, "a", 1)];
        assert!(write_mbr(&mut d, &into_backup, 1).is_ok());
        let mut d = fresh();
        assert!(write_gpt(&mut d, &into_backup, guid(9)).is_err());
        // One sector less than the backup area is still too much; exactly up to it is fine.
        let exact = [spec(PartType::BasicData, MIB, 15 * MIB - 33 * 512, "a", 1)];
        let over = [spec(PartType::BasicData, MIB, 15 * MIB - 32 * 512, "a", 1)];
        assert!(write_gpt(&mut d, &over, guid(9)).is_err());
        ok(&d);
        assert!(write_gpt(&mut d, &exact, guid(9)).is_ok());
        assert_eq!(read_table(&mut d).unwrap().problems, Vec::<String>::new());

        // Inside the table area.
        let mut d = fresh();
        let early = [spec(PartType::BasicData, 33 * 512, MIB, "a", 1)];
        assert!(write_gpt(&mut d, &early, guid(9)).is_err());
        assert!(write_mbr(&mut d, &[spec(PartType::BasicData, 0, MIB, "", 1)], 1).is_err());
        // LBA 34 is the first usable sector.
        assert!(write_gpt(
            &mut d,
            &[spec(PartType::BasicData, 34 * 512, MIB, "a", 1)],
            guid(9)
        )
        .is_ok());

        // Unaligned, empty, overflowing.
        let mut d = fresh();
        for bad in [
            spec(PartType::BasicData, MIB + 100, MIB, "a", 1),
            spec(PartType::BasicData, MIB, MIB + 1, "a", 1),
            spec(PartType::BasicData, MIB, 0, "a", 1),
            spec(PartType::BasicData, u64::MAX - 511, 1024, "a", 1),
        ] {
            assert!(write_gpt(&mut d, std::slice::from_ref(&bad), guid(9)).is_err());
            assert!(write_mbr(&mut d, std::slice::from_ref(&bad), 1).is_err());
        }
        // 4096-byte sectors refuse 512-aligned partitions.
        let mut d4 = MemDevice::new(16 * MIB, 4096);
        assert!(write_gpt(
            &mut d4,
            &[spec(PartType::BasicData, MIB + 512, MIB, "a", 1)],
            guid(9)
        )
        .is_err());
        assert_eq!(d4.writes, 0);

        // Too many MBR partitions, two active ones.
        let five: Vec<PartSpec> = (0..5)
            .map(|i| spec(PartType::BasicData, (1 + i) * MIB, MIB, "", i as u8 + 1))
            .collect();
        assert!(matches!(
            write_mbr(&mut d, &five, 1),
            Err(Error::Invalid(_))
        ));
        assert!(write_gpt(&mut d, &five, guid(9)).is_ok());
        let mut d = fresh();
        let mut two = five[..2].to_vec();
        two[0].active = true;
        two[1].active = true;
        assert!(write_mbr(&mut d, &two, 1).is_err());
        assert!(write_mbr(&mut d, &five[..4], 1).is_ok());
        assert_eq!(read_table(&mut d).unwrap().parts.len(), 4);

        // GPT specifics: zero or repeated GUIDs, over-long names, zero disk GUID.
        let mut d = fresh();
        let mut zero = spec(PartType::BasicData, MIB, MIB, "a", 1);
        zero.guid = [0; 16];
        assert!(write_gpt(&mut d, &[zero], guid(9)).is_err());
        let twins = [
            spec(PartType::BasicData, MIB, MIB, "a", 1),
            spec(PartType::BasicData, 2 * MIB, MIB, "b", 1),
        ];
        assert!(write_gpt(&mut d, &twins, guid(9)).is_err());
        let long = spec(PartType::BasicData, MIB, MIB, &"x".repeat(37), 1);
        assert!(write_gpt(&mut d, &[long], guid(9)).is_err());
        let fits = spec(PartType::BasicData, MIB, MIB, &"x".repeat(36), 1);
        assert!(write_gpt(
            &mut d,
            &[spec(PartType::BasicData, MIB, MIB, "a", 1)],
            [0; 16]
        )
        .is_err());
        ok(&d);
        assert!(write_gpt(&mut d, &[fits], guid(9)).is_ok());
        assert_eq!(read_table(&mut d).unwrap().parts[0].name, "x".repeat(36));

        // Tiny devices.
        for sectors in [0u64, 1, 2, 40, 67] {
            let mut tiny = MemDevice::new(sectors * 512, 512);
            assert!(write_gpt(&mut tiny, &[], guid(9)).is_err(), "{sectors}");
            assert_eq!(tiny.writes, 0);
        }
        let mut tiny = MemDevice::new(68 * 512, 512);
        assert!(write_gpt(&mut tiny, &[], guid(9)).is_ok());
        assert_eq!(
            read_table(&mut tiny).unwrap().problems,
            Vec::<String>::new()
        );
        let mut tiny = MemDevice::new(512, 512);
        assert!(write_mbr(&mut tiny, &[], 1).is_err());
        assert_eq!(tiny.writes, 0);
    }

    #[test]
    fn wipe_removes_both_tables() {
        let mut d = sample_gpt();
        // Something in the middle must survive.
        d.data[8 * MIB as usize] = 0x77;
        wipe_tables(&mut d).unwrap();
        assert!(d.data[..MIB as usize].iter().all(|b| *b == 0));
        assert!(d.data[15 * MIB as usize..].iter().all(|b| *b == 0));
        assert_eq!(d.data[8 * MIB as usize], 0x77);
        let t = read_table(&mut d).unwrap();
        assert_eq!(t.scheme, None);
        assert!(t.parts.is_empty() && t.problems.is_empty());

        // Clamped on small and odd devices.
        for size in [
            512u64,
            100 * 512,
            MIB,
            MIB + 512,
            2 * MIB - 512,
            3 * MIB + 512,
        ] {
            let mut d = MemDevice::new(size, 512);
            d.data.fill(0xAB);
            wipe_tables(&mut d).unwrap();
            let kept = d.data.iter().filter(|b| **b != 0).count() as u64;
            assert_eq!(kept, size.saturating_sub(2 * MIB), "size {size}");
        }
        wipe_tables(&mut MemDevice::new(0, 512)).unwrap();
        let mut d = MemDevice::new(4 * MIB, 4096);
        d.data.fill(1);
        wipe_tables(&mut d).unwrap();
        assert_eq!(d.data.iter().filter(|b| **b != 0).count(), 2 * MIB as usize);
    }
}
