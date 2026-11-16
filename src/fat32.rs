//! FAT32 formatter, writer and reader.
//!
//! Written from Microsoft's "FAT32 File System Specification" (fatgen103).
//! Three independent parts:
//!
//! * `format` lays down an empty volume;
//! * `Fat32Writer` fills a freshly formatted volume with directories and
//!   files (sequential, contiguous allocation; long file names);
//! * `Fat32Reader` / `check` read a volume back without sharing any of the
//!   writer's logic, to verify what was written.
//!
//! The boot sector carries no boot loader, only a five-byte stub (`int 18h`
//! and a halt loop): these volumes are booted by UEFI firmware, which loads
//! `\EFI\BOOT\BOOT*.EFI` as a file.

use crate::blockdev::BlockDevice;
use crate::error::{Error, IoContext, Result};
use crate::model::{Cancel, ProgressFn};
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};

const MIB: u64 = 1024 * 1024;
/// I/O chunk; a multiple of every supported cluster size.
const CHUNK: usize = 1024 * 1024;
const RESERVED_SECTORS: u32 = 32;
const NUM_FATS: u32 = 2;
const FSINFO_SECTOR: u32 = 1;
const BACKUP_BOOT_SECTOR: u32 = 6;
const MEDIA: u8 = 0xF8;
/// A volume with fewer clusters is FAT16 by definition.
const MIN_CLUSTERS: u64 = 65525;
/// Largest cluster count: the count must be below 0x0FFFFFF5.
const MAX_CLUSTERS: u64 = 0x0FFF_FFF4;
const FAT_MASK: u32 = 0x0FFF_FFFF;
const EOC: u32 = 0x0FFF_FFFF;
const EOC_MIN: u32 = 0x0FFF_FFF8;
const BAD_CLUSTER: u32 = 0x0FFF_FFF7;
/// A FAT directory holds at most 65536 entries.
const MAX_DIR_BYTES: usize = 65536 * 32;
/// Largest file FAT32 can store.
pub const MAX_FILE_SIZE: u64 = 0xFFFF_FFFF;
const MAX_DEPTH: usize = 64;
const MAX_PROBLEMS: usize = 200;

const ATTR_VOLUME: u8 = 0x08;
const ATTR_DIR: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0F;
/// NT case bits in byte 12 of a short entry: base / extension shown in lower case.
const NT_LOWER_BASE: u8 = 0x08;
const NT_LOWER_EXT: u8 = 0x10;

/// A tiny original boot stub: `int 18h`, then `hlt` in a loop.
const BOOT_STUB: [u8; 5] = [0xCD, 0x18, 0xF4, 0xEB, 0xFD];

// --------------------------------------------------------------------------------------
// Public types
// --------------------------------------------------------------------------------------

/// A FAT date and time (two-second resolution, years 1980 to 2107, no time zone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DosTime {
    /// Bits 15-9 year since 1980, 8-5 month, 4-0 day.
    pub date: u16,
    /// Bits 15-11 hours, 10-5 minutes, 4-0 seconds / 2.
    pub time: u16,
}

impl DosTime {
    /// Out-of-range parts are clamped.
    pub fn new(year: u32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DosTime {
        let y = year.clamp(1980, 2107) - 1980;
        let date = (y << 9) | (month.clamp(1, 12) << 5) | day.clamp(1, 31);
        let time = (hour.min(23) << 11) | (minute.min(59) << 5) | (second.min(59) / 2);
        DosTime {
            date: date as u16,
            time: time as u16,
        }
    }

    /// From seconds since 1970-01-01, as UTC (FAT has no time zone; Windows
    /// shows these values as local time). Clamped to the FAT range.
    pub fn from_unix(seconds: i64) -> DosTime {
        let days = seconds.div_euclid(86400);
        let rem = seconds.rem_euclid(86400) as u32;
        // Days to a civil date in the proleptic Gregorian calendar, counting
        // in 400-year eras from 0000-03-01.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        if year < 1980 {
            return DosTime::new(1980, 1, 1, 0, 0, 0);
        }
        if year > 2107 {
            return DosTime::new(2107, 12, 31, 23, 59, 59);
        }
        DosTime::new(
            year as u32,
            month as u32,
            day as u32,
            rem / 3600,
            rem % 3600 / 60,
            rem % 60,
        )
    }

    /// The current time, as UTC.
    pub fn now() -> DosTime {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        DosTime::from_unix(secs)
    }

    /// (year, month, day, hour, minute, second).
    pub fn parts(self) -> (u32, u32, u32, u32, u32, u32) {
        let (d, t) = (self.date as u32, self.time as u32);
        (
            1980 + (d >> 9),
            (d >> 5) & 15,
            d & 31,
            t >> 11,
            (t >> 5) & 63,
            (t & 31) * 2,
        )
    }
}

/// What to format.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormatOptions {
    /// Volume label; passed through `sanitize_label`.
    pub label: String,
    /// Cluster size in bytes; `None` picks `default_cluster_size`.
    pub cluster_size: Option<u32>,
    /// Start of the partition on the disk in sectors, for the BPB "hidden
    /// sectors" field (stored as 32 bits; larger values are clamped).
    pub hidden_sectors: u64,
    /// Volume serial number.
    pub volume_id: u32,
    /// Full format: zero the whole volume, not only the file-system structures.
    pub zero_data: bool,
}

/// The layout of a FAT32 volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub sector_size: u32,
    pub cluster_size: u32,
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32,
    pub num_fats: u32,
    /// Sectors per FAT.
    pub fat_sectors: u32,
    pub total_sectors: u32,
    /// Number of data clusters (valid cluster numbers are 2..cluster_count + 2).
    pub cluster_count: u32,
    /// First sector of the data area (cluster 2).
    pub data_start_sector: u64,
    pub root_cluster: u32,
    pub volume_id: u32,
    /// Label from the boot sector ("" when none).
    pub label: String,
}

impl Geometry {
    /// Byte offset of a data cluster (`cluster >= 2`).
    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        (self.data_start_sector
            + (cluster as u64).saturating_sub(2) * self.sectors_per_cluster as u64)
            * self.sector_size as u64
    }

    /// Byte offset of FAT copy `index` (0-based).
    pub fn fat_offset(&self, index: u32) -> u64 {
        (self.reserved_sectors as u64 + index as u64 * self.fat_sectors as u64)
            * self.sector_size as u64
    }

    /// Bytes a quick format writes: reserved area, FATs and the root cluster.
    pub fn metadata_bytes(&self) -> u64 {
        self.cluster_offset(2) + self.cluster_size as u64
    }
}

/// A file or directory found by the reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundFile {
    /// Path from the root with forward slashes, no leading slash, long names.
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    /// 0 for an empty file.
    pub first_cluster: u32,
    /// The 8.3 name as stored, e.g. `README~1.TXT`.
    pub short_name: String,
    /// Last-modified stamp.
    pub mtime: DosTime,
}

/// What `check` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsReport {
    /// Volume label from the root directory (the boot-sector label when the
    /// root has none; "" when neither).
    pub label: String,
    pub cluster_size: u32,
    pub total_clusters: u32,
    /// Free clusters counted in the FAT.
    pub free_clusters: u32,
    /// Every file and directory, in no particular order.
    pub files: Vec<FoundFile>,
    /// Every inconsistency found; empty for a healthy volume.
    pub problems: Vec<String>,
}

// --------------------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------------------

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn problem(list: &mut Vec<String>, text: String) {
    if list.len() < MAX_PROBLEMS {
        list.push(text);
    } else if list.len() == MAX_PROBLEMS {
        list.push("(further problems not listed)".to_string());
    }
}

fn sector_of(dev: &dyn BlockDevice) -> Result<u32> {
    let s = dev.sector_size();
    if !(512..=4096).contains(&s) || !s.is_power_of_two() {
        return Err(Error::invalid(format!(
            "Unsupported sector size ({s} bytes)."
        )));
    }
    Ok(s)
}

/// Case-insensitive key of a name.
fn fold(name: &str) -> String {
    name.chars().flat_map(char::to_uppercase).collect()
}

// --------------------------------------------------------------------------------------
// Layout and formatting
// --------------------------------------------------------------------------------------

/// FAT size (sectors) and cluster count for a volume, or `Err(clusters)` with
/// the cluster count that makes the combination invalid (0: no room at all).
fn layout(total_sectors: u64, sector: u32, cluster: u32) -> std::result::Result<(u32, u32), u64> {
    let spc = (cluster / sector) as u64;
    let avail = total_sectors
        .checked_sub(RESERVED_SECTORS as u64)
        .ok_or(0u64)?;
    // Smallest F with F * sector / 4 >= (avail - 2F) / spc + 2 entries.
    let fat = (4 * avail + 8 * spc).div_ceil(sector as u64 * spc + 8);
    let data = avail.checked_sub(NUM_FATS as u64 * fat).ok_or(0u64)?;
    let clusters = data / spc;
    if !(MIN_CLUSTERS..=MAX_CLUSTERS).contains(&clusters) {
        return Err(clusters);
    }
    Ok((fat as u32, clusters as u32))
}

fn check_cluster_size(volume_size: u64, sector: u32, cluster: u32) -> Result<(u32, u32)> {
    if !cluster.is_power_of_two() || !(512..=65536).contains(&cluster) {
        return Err(Error::invalid(format!(
            "Invalid FAT32 cluster size ({cluster} bytes): it must be a power of two between 512 bytes and 64 KiB."
        )));
    }
    if cluster < sector {
        return Err(Error::invalid(format!(
            "A FAT32 cluster ({cluster} bytes) cannot be smaller than the drive's {sector}-byte sector."
        )));
    }
    let total = volume_size / sector as u64;
    if total > u32::MAX as u64 {
        return Err(Error::invalid(
            "The volume is too large for FAT32 (more than 2^32 sectors).",
        ));
    }
    layout(total, sector, cluster).map_err(|clusters| {
        if clusters < MIN_CLUSTERS {
            Error::invalid(format!(
                "With {cluster}-byte clusters this volume would have {clusters} clusters; FAT32 needs at least {MIN_CLUSTERS}. Choose a smaller cluster size."
            ))
        } else {
            Error::invalid(format!(
                "With {cluster}-byte clusters this volume would have {clusters} clusters; FAT32 allows at most {MAX_CLUSTERS}. Choose a larger cluster size."
            ))
        }
    })
}

/// The cluster sizes (bytes, ascending) with which a volume of this size can
/// be formatted as FAT32. Empty when FAT32 is impossible (volume too small or
/// too large).
pub fn valid_cluster_sizes(volume_size: u64, sector: u32) -> Vec<u32> {
    if !(512..=4096).contains(&sector) || !sector.is_power_of_two() {
        return Vec::new();
    }
    (9..=16)
        .map(|shift| 1u32 << shift)
        .filter(|c| check_cluster_size(volume_size, sector, *c).is_ok())
        .collect()
}

/// The default cluster size from Microsoft's table (up to 260 MiB: 512 bytes;
/// 8 GiB: 4 KiB; 16 GiB: 8 KiB; 32 GiB: 16 KiB; above: 32 KiB), never smaller
/// than the sector. When that size is not valid for the volume the nearest
/// valid size is used (smaller first). `None` when FAT32 is impossible.
pub fn default_cluster_size(volume_size: u64, sector: u32) -> Option<u32> {
    let table = if volume_size <= 260 * MIB {
        512
    } else if volume_size <= 8 * 1024 * MIB {
        4096
    } else if volume_size <= 16 * 1024 * MIB {
        8192
    } else if volume_size <= 32 * 1024 * MIB {
        16384
    } else {
        32768
    };
    let want = table.max(sector);
    let valid = valid_cluster_sizes(volume_size, sector);
    if valid.contains(&want) {
        return Some(want);
    }
    valid
        .iter()
        .rev()
        .find(|c| **c < want)
        .or_else(|| valid.iter().find(|c| **c > want))
        .copied()
}

/// The label as it will be stored: upper-cased, characters that are not
/// allowed in a FAT label (and anything outside ASCII) replaced by `_`,
/// surrounding spaces trimmed, cut to 11 bytes.
pub fn sanitize_label(label: &str) -> String {
    let mapped: String = label
        .trim()
        .chars()
        .map(|c| {
            let u = c.to_ascii_uppercase();
            if !u.is_ascii()
                || (u as u32) < 0x20
                || u == '\u{7f}'
                || "\"*+,./:;<=>?[\\]|".contains(u)
            {
                '_'
            } else {
                u
            }
        })
        .take(11)
        .collect();
    mapped.trim().to_string()
}

fn label_bytes(label: &str) -> [u8; 11] {
    let mut raw = [b' '; 11];
    for (slot, b) in raw.iter_mut().zip(label.bytes()) {
        *slot = b;
    }
    raw
}

fn boot_sector(geo: &Geometry, hidden: u64) -> Vec<u8> {
    let mut b = vec![0u8; geo.sector_size as usize];
    b[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    b[3..11].copy_from_slice(b"MSWIN4.1");
    put16(&mut b, 11, geo.sector_size as u16);
    b[13] = geo.sectors_per_cluster as u8;
    put16(&mut b, 14, geo.reserved_sectors as u16);
    b[16] = geo.num_fats as u8;
    // 17: root entries, 19: 16-bit sector count, 22: 16-bit FAT size: all 0 on FAT32.
    b[21] = MEDIA;
    put16(&mut b, 24, 63);
    put16(&mut b, 26, 255);
    put32(&mut b, 28, hidden.min(u32::MAX as u64) as u32);
    put32(&mut b, 32, geo.total_sectors);
    put32(&mut b, 36, geo.fat_sectors);
    // 40: flags (FATs mirrored), 42: version 0.0.
    put32(&mut b, 44, geo.root_cluster);
    put16(&mut b, 48, FSINFO_SECTOR as u16);
    put16(&mut b, 50, BACKUP_BOOT_SECTOR as u16);
    b[64] = 0x80;
    b[66] = 0x29;
    put32(&mut b, 67, geo.volume_id);
    let label = if geo.label.is_empty() {
        *b"NO NAME    "
    } else {
        label_bytes(&geo.label)
    };
    b[71..82].copy_from_slice(&label);
    b[82..90].copy_from_slice(b"FAT32   ");
    b[90..95].copy_from_slice(&BOOT_STUB);
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

fn fsinfo_sector(sector: u32, free: u32, next_free: u32) -> Vec<u8> {
    let mut b = vec![0u8; sector as usize];
    put32(&mut b, 0, 0x4161_5252);
    put32(&mut b, 484, 0x6141_7272);
    put32(&mut b, 488, free);
    put32(&mut b, 492, next_free);
    put32(&mut b, 508, 0xAA55_0000);
    b
}

/// Formats the device (a partition window) as an empty FAT32 volume.
///
/// Per the Microsoft specification: 32 reserved sectors, two FATs, FSInfo in
/// sector 1, backup boot sector in sector 6 (backup FSInfo in 7), media
/// 0xF8, the root directory in cluster 2 holding the volume-label entry.
/// Works with 512- and 4096-byte sectors. A user-chosen cluster size is
/// validated (`Error::Invalid` explains what is wrong).
///
/// `progress` receives the bytes written so far and ends at
/// `Geometry::metadata_bytes()` for a quick format or at the volume size when
/// `zero_data` is set. The boot sector is written last, so a cancelled or
/// failed format does not leave something that looks like a valid volume.
pub fn format(
    dev: &mut dyn BlockDevice,
    opts: &FormatOptions,
    cancel: &Cancel,
    progress: ProgressFn,
) -> Result<Geometry> {
    let sector = sector_of(dev)?;
    let s = sector as u64;
    let size = dev.size();
    let cluster = match opts.cluster_size {
        Some(c) => c,
        None => default_cluster_size(size, sector).ok_or_else(|| {
            Error::invalid(
                "This volume cannot be formatted as FAT32: it is too small (or too large) for any FAT32 cluster size.",
            )
        })?,
    };
    let (fat_sectors, cluster_count) = check_cluster_size(size, sector, cluster)?;
    let geo = Geometry {
        sector_size: sector,
        cluster_size: cluster,
        sectors_per_cluster: cluster / sector,
        reserved_sectors: RESERVED_SECTORS,
        num_fats: NUM_FATS,
        fat_sectors,
        total_sectors: (size / s) as u32,
        cluster_count,
        data_start_sector: RESERVED_SECTORS as u64 + NUM_FATS as u64 * fat_sectors as u64,
        root_cluster: 2,
        volume_id: opts.volume_id,
        label: sanitize_label(&opts.label),
    };

    // 1. Zeros: reserved area, both FATs and the root cluster, or everything.
    let zero_end = if opts.zero_data {
        size
    } else {
        geo.metadata_bytes()
    };
    let zeros = vec![0u8; CHUNK];
    let mut done = 0u64;
    while done < zero_end {
        cancel.check()?;
        let n = (zero_end - done).min(CHUNK as u64) as usize;
        dev.write_at(done, &zeros[..n])
            .ctx("clearing the FAT32 volume")?;
        done += n as u64;
        progress(done);
    }
    cancel.check()?;

    // 2. FAT[0] (media), FAT[1] (end of chain, clean), FAT[2] (root directory).
    let mut head = vec![0u8; sector as usize];
    put32(&mut head, 0, 0x0FFF_FF00 | MEDIA as u32);
    put32(&mut head, 4, EOC);
    put32(&mut head, 8, EOC);
    for k in 0..NUM_FATS {
        dev.write_at(geo.fat_offset(k), &head)
            .ctx("writing the FAT")?;
    }

    // 3. Root directory: the volume label.
    if !geo.label.is_empty() {
        let mut first = vec![0u8; sector as usize];
        let entry = short_entry(
            &label_bytes(&geo.label),
            ATTR_VOLUME,
            0,
            0,
            0,
            DosTime::now(),
        );
        first[..32].copy_from_slice(&entry);
        dev.write_at(geo.cluster_offset(2), &first)
            .ctx("writing the root directory")?;
    }

    // 4. FSInfo and boot sector, backups first.
    let info = fsinfo_sector(sector, cluster_count - 1, 3);
    let boot = boot_sector(&geo, opts.hidden_sectors);
    dev.write_at((BACKUP_BOOT_SECTOR + FSINFO_SECTOR) as u64 * s, &info)
        .ctx("writing the backup FSInfo sector")?;
    dev.write_at(BACKUP_BOOT_SECTOR as u64 * s, &boot)
        .ctx("writing the backup boot sector")?;
    dev.write_at(FSINFO_SECTOR as u64 * s, &info)
        .ctx("writing the FSInfo sector")?;
    dev.write_at(0, &boot).ctx("writing the boot sector")?;
    dev.flush().ctx("flushing the FAT32 volume")?;
    progress(zero_end);
    Ok(geo)
}

struct Boot {
    geo: Geometry,
    fsinfo_sector: u32,
    backup_sector: u32,
    sector0: Vec<u8>,
}

/// Reads and validates the boot sector far enough to locate everything.
fn read_boot(dev: &mut dyn BlockDevice) -> Result<Boot> {
    let sector = sector_of(dev)?;
    let bad = |why: &str| Error::invalid(format!("This is not a usable FAT32 volume: {why}."));
    if dev.size() < sector as u64 {
        return Err(bad("the device is smaller than one sector"));
    }
    let mut b = vec![0u8; sector as usize];
    dev.read_at(0, &mut b).ctx("reading the boot sector")?;
    if b[510] != 0x55 || b[511] != 0xAA {
        return Err(bad("the boot sector signature is missing"));
    }
    if le16(&b, 11) as u32 != sector {
        return Err(bad("its sector size does not match the device"));
    }
    let spc = b[13] as u32;
    if spc == 0 || !spc.is_power_of_two() {
        return Err(bad("invalid sectors per cluster"));
    }
    let reserved = le16(&b, 14) as u32;
    let num_fats = b[16] as u32;
    if reserved == 0 || !(1..=2).contains(&num_fats) {
        return Err(bad("invalid reserved area or number of FATs"));
    }
    let fat_sectors = le32(&b, 36);
    let total = le32(&b, 32);
    if le16(&b, 22) != 0 || fat_sectors == 0 || total == 0 {
        return Err(bad("the FAT32 size fields are not set (FAT12/16?)"));
    }
    if total as u64 > dev.size() / sector as u64 {
        return Err(bad("the volume is larger than the device (truncated?)"));
    }
    let data_start = reserved as u64 + num_fats as u64 * fat_sectors as u64;
    if data_start >= total as u64 {
        return Err(bad("the FATs do not fit in the volume"));
    }
    let clusters = (total as u64 - data_start) / spc as u64;
    if !(MIN_CLUSTERS..=MAX_CLUSTERS).contains(&clusters) {
        return Err(bad("the cluster count is outside the FAT32 range"));
    }
    if (fat_sectors as u64 * sector as u64) / 4 < clusters + 2 {
        return Err(bad("the FAT is too short for the cluster count"));
    }
    let root = le32(&b, 44);
    if root < 2 || root as u64 >= clusters + 2 {
        return Err(bad("the root directory cluster is out of range"));
    }
    let label = String::from_utf8_lossy(&b[71..82]).trim_end().to_string();
    let geo = Geometry {
        sector_size: sector,
        cluster_size: spc * sector,
        sectors_per_cluster: spc,
        reserved_sectors: reserved,
        num_fats,
        fat_sectors,
        total_sectors: total,
        cluster_count: clusters as u32,
        data_start_sector: data_start,
        root_cluster: root,
        volume_id: le32(&b, 67),
        label: if label == "NO NAME" {
            String::new()
        } else {
            label
        },
    };
    Ok(Boot {
        geo,
        fsinfo_sector: le16(&b, 48) as u32,
        backup_sector: le16(&b, 50) as u32,
        sector0: b,
    })
}

// --------------------------------------------------------------------------------------
// Names and directory entries
// --------------------------------------------------------------------------------------

/// Checks a long name and returns it as UTF-16.
fn validate_name(name: &str) -> Result<Vec<u16>> {
    let bad = |why: &str| {
        Error::invalid(format!(
            "The name \"{name}\" cannot be stored on FAT32: {why}."
        ))
    };
    if name.is_empty() {
        return Err(bad("it is empty"));
    }
    if name == "." || name == ".." {
        return Err(bad("it is reserved"));
    }
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || c == '\u{7f}' || "\"*/:<>?\\|".contains(c))
    {
        return Err(bad("it contains a character that is not allowed"));
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err(bad("it ends with a space or a dot"));
    }
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.len() > 255 {
        return Err(bad("it is longer than 255 characters"));
    }
    Ok(units)
}

/// Characters allowed in an 8.3 name (upper case only).
fn is_short_char(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b"$%'-_@~`!(){}^#&".contains(&b)
}

fn short_raw(base: &[u8], ext: &[u8]) -> [u8; 11] {
    let mut raw = [b' '; 11];
    raw[..base.len()].copy_from_slice(base);
    raw[8..8 + ext.len()].copy_from_slice(ext);
    raw
}

/// `BASE.EXT` for a raw 11-byte name.
fn short_display(raw: &[u8; 11]) -> String {
    let part = |bytes: &[u8]| -> String {
        let end = bytes.iter().rposition(|b| *b != b' ').map_or(0, |i| i + 1);
        bytes[..end]
            .iter()
            .map(|b| if b.is_ascii() { *b as char } else { '\u{fffd}' })
            .collect()
    };
    let (base, ext) = (part(&raw[..8]), part(&raw[8..]));
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

/// A name that is a valid 8.3 name as it stands, with base and extension each
/// in a single case: stored without long-name entries, lower case recorded in
/// the NT case bits (as Windows does). Returns the raw name and the bits.
fn exact_short(name: &str) -> Option<([u8; 11], u8)> {
    if !name.is_ascii() {
        return None;
    }
    let (base, ext) = match name.rfind('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    let mut bits = 0u8;
    for (part, bit) in [(base, NT_LOWER_BASE), (ext, NT_LOWER_EXT)] {
        if !part.bytes().all(|b| is_short_char(b.to_ascii_uppercase())) {
            return None;
        }
        let lower = part.bytes().any(|b| b.is_ascii_lowercase());
        let upper = part.bytes().any(|b| b.is_ascii_uppercase());
        if lower && upper {
            return None;
        }
        if lower {
            bits |= bit;
        }
    }
    Some((
        short_raw(
            base.to_ascii_uppercase().as_bytes(),
            ext.to_ascii_uppercase().as_bytes(),
        ),
        bits,
    ))
}

/// The basis of a generated alias: upper-cased base and extension with
/// spaces and extra dots dropped and everything not allowed in 8.3 replaced
/// by `_`. The flag says the conversion lost nothing but case and the base
/// fits (so the alias can be used without a numeric tail).
fn alias_basis(name: &str) -> (Vec<u8>, Vec<u8>, bool) {
    let trimmed = name.trim_start_matches('.');
    let mut exact = trimmed.len() == name.len();
    let (base, ext) = match trimmed.rfind('.') {
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => (trimmed, ""),
    };
    let mut convert = |text: &str| -> Vec<u8> {
        let mut out = Vec::new();
        for c in text.chars() {
            if c == ' ' || c == '.' {
                exact = false;
                continue;
            }
            let u = c.to_ascii_uppercase();
            if u.is_ascii() && is_short_char(u as u8) {
                out.push(u as u8);
            } else {
                out.push(b'_');
                exact = false;
            }
        }
        out
    };
    let mut base = convert(base);
    let mut ext = convert(ext);
    if ext.len() > 3 {
        ext.truncate(3);
        exact = false;
    }
    if base.is_empty() {
        base.push(b'_');
        exact = false;
    }
    if base.len() > 8 {
        exact = false;
    }
    (base, ext, exact)
}

/// A 16-bit hash of a long name (FNV-1a folded), for the hashed alias form.
fn name_hash(units: &[u16]) -> u16 {
    let mut h: u32 = 0x811C_9DC5;
    for u in units {
        for b in u.to_le_bytes() {
            h = (h ^ b as u32).wrapping_mul(0x0100_0193);
        }
    }
    (h ^ (h >> 16)) as u16
}

/// Generates an 8.3 alias for a long name that is unique among `taken`
/// (folded long names and aliases of the directory): the plain upper-case
/// name when nothing but case was lost, else `BASE~1` .. `BASE~4`, then
/// `BAxxxx~1` .. `BAxxxx~9` with four hex digits of a hash of the long name,
/// then `BASE~10` and up.
fn make_alias(name: &str, units: &[u16], taken: &HashSet<String>) -> Option<[u8; 11]> {
    let (base, ext, exact) = alias_basis(name);
    let attempt = |stem: &[u8]| -> Option<[u8; 11]> {
        let raw = short_raw(stem, &ext);
        (!taken.contains(&short_display(&raw))).then_some(raw)
    };
    if exact {
        if let Some(raw) = attempt(&base) {
            return Some(raw);
        }
    }
    let with_tail = |keep: usize, middle: &str, n: u32| -> Vec<u8> {
        let mut stem = base[..keep.min(base.len())].to_vec();
        stem.extend_from_slice(middle.as_bytes());
        stem.push(b'~');
        stem.extend_from_slice(n.to_string().as_bytes());
        stem
    };
    for n in 1..=4 {
        if let Some(raw) = attempt(&with_tail(6, "", n)) {
            return Some(raw);
        }
    }
    let hash = format!("{:04X}", name_hash(units));
    for n in 1..=9 {
        if let Some(raw) = attempt(&with_tail(2, &hash, n)) {
            return Some(raw);
        }
    }
    for n in 10..1_000_000u32 {
        let digits = n.to_string().len();
        if let Some(raw) = attempt(&with_tail(7 - digits, "", n)) {
            return Some(raw);
        }
    }
    None
}

/// The checksum of a short name that ties long-name entries to it.
fn lfn_checksum(raw: &[u8; 11]) -> u8 {
    raw.iter()
        .fold(0u8, |sum, b| sum.rotate_right(1).wrapping_add(*b))
}

fn short_entry(
    raw: &[u8; 11],
    attr: u8,
    nt_bits: u8,
    cluster: u32,
    size: u32,
    time: DosTime,
) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(raw);
    e[11] = attr;
    e[12] = nt_bits;
    put16(&mut e, 14, time.time);
    put16(&mut e, 16, time.date);
    put16(&mut e, 18, time.date);
    put16(&mut e, 20, (cluster >> 16) as u16);
    put16(&mut e, 22, time.time);
    put16(&mut e, 24, time.date);
    put16(&mut e, 26, cluster as u16);
    put32(&mut e, 28, size);
    e
}

/// Byte offsets of the 13 UCS-2 units inside a long-name entry.
const LFN_SLOTS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

/// The long-name entries for a name, in on-disk order (last piece first).
fn lfn_entries(units: &[u16], raw: &[u8; 11]) -> Vec<u8> {
    let count = units.len().div_ceil(13);
    let sum = lfn_checksum(raw);
    let mut out = Vec::with_capacity(count * 32);
    for seq in (1..=count).rev() {
        let piece = &units[(seq - 1) * 13..units.len().min(seq * 13)];
        let mut e = [0u8; 32];
        e[0] = seq as u8 | if seq == count { 0x40 } else { 0 };
        e[11] = ATTR_LFN;
        e[13] = sum;
        for (i, at) in LFN_SLOTS.iter().enumerate() {
            let unit = match i.cmp(&piece.len()) {
                std::cmp::Ordering::Less => piece[i],
                std::cmp::Ordering::Equal => 0x0000,
                std::cmp::Ordering::Greater => 0xFFFF,
            };
            put16(&mut e, *at, unit);
        }
        out.extend_from_slice(&e);
    }
    out
}

// --------------------------------------------------------------------------------------
// Writer
// --------------------------------------------------------------------------------------

/// A run of clusters allocated in one go. `next` is the cluster the chain
/// continues at (0: the chain ends here).
struct Extent {
    start: u32,
    len: u32,
    next: u32,
}

struct DirState {
    first_cluster: u32,
    clusters: Vec<u32>,
    /// Index of the extent holding the last cluster of this directory.
    tail_extent: usize,
    /// The directory entries written so far.
    data: Vec<u8>,
    /// Folded long names and aliases in use.
    names: HashSet<String>,
    /// Folded name -> index into `Fat32Writer::dirs`.
    subdirs: HashMap<String, usize>,
}

/// A name ready to be entered into a directory.
struct Prepared {
    raw: [u8; 11],
    nt_bits: u8,
    lfn: Option<Vec<u16>>,
    keys: Vec<String>,
    slots: usize,
}

/// Fills a freshly formatted FAT32 volume.
///
/// Files are laid out one after the other in the order they are written,
/// each in one contiguous run of clusters. File data goes to the device
/// immediately; the FATs, the directories and FSInfo are kept in memory
/// (a few bytes per file, plus the directory entries) and written by
/// `finish`. Until `finish` succeeds the volume on the device looks empty.
///
/// Paths use forward slashes (a leading slash is optional). Names keep their
/// case and are unique without regard to case. A name that is a valid 8.3
/// name in a single case per part (`BOOTX64.EFI`, `boot.wim`, `Setup.EXE` is
/// not) is stored as a plain short entry, lower case being recorded in the
/// NT case bits exactly as Windows does; every other name gets UCS-2
/// long-name entries plus a generated alias (`README~1.TXT`).
///
/// A failed `write_file` (source error, cancellation, no space) leaves the
/// writer consistent: the file is simply not there, and `finish` still
/// produces a valid volume with everything written before.
pub struct Fat32Writer<'a> {
    dev: &'a mut dyn BlockDevice,
    geo: Geometry,
    /// First cluster not yet allocated.
    next_free: u32,
    /// Every allocation, in ascending cluster order without gaps from cluster 2.
    extents: Vec<Extent>,
    /// Directories; index 0 is the root.
    dirs: Vec<DirState>,
    now: DosTime,
    buf: Vec<u8>,
}

impl<'a> Fat32Writer<'a> {
    /// Opens a volume made by `format`. Refuses a volume whose root
    /// directory holds anything but the volume label, because the writer
    /// would overwrite existing data.
    pub fn open(dev: &'a mut dyn BlockDevice) -> Result<Fat32Writer<'a>> {
        let geo = read_boot(dev)?.geo;
        let not_fresh = || {
            Error::invalid(
                "The FAT32 volume is not freshly formatted; refusing to write files to it.",
            )
        };
        if geo.root_cluster != 2 || geo.num_fats != NUM_FATS || geo.cluster_size as usize > CHUNK {
            return Err(not_fresh());
        }
        let mut head = vec![0u8; geo.sector_size as usize];
        dev.read_at(geo.fat_offset(0), &mut head)
            .ctx("reading the FAT")?;
        if le32(&head, 8) & FAT_MASK < EOC_MIN || le32(&head, 12) != 0 {
            return Err(not_fresh());
        }
        let mut root = vec![0u8; geo.cluster_size as usize];
        dev.read_at(geo.cluster_offset(2), &mut root)
            .ctx("reading the root directory")?;
        let used = root
            .as_chunks::<32>()
            .0
            .iter()
            .take_while(|e| e[0] != 0 && e[11] & 0x3F == ATTR_VOLUME)
            .count()
            * 32;
        if root[used..].iter().any(|b| *b != 0) {
            return Err(not_fresh());
        }
        root.truncate(used);
        Ok(Fat32Writer {
            dev,
            next_free: 3,
            extents: vec![Extent {
                start: 2,
                len: 1,
                next: 0,
            }],
            dirs: vec![DirState {
                first_cluster: 2,
                clusters: vec![2],
                tail_extent: 0,
                data: root,
                names: HashSet::new(),
                subdirs: HashMap::new(),
            }],
            now: DosTime::now(),
            buf: Vec::new(),
            geo,
        })
    }

    /// The layout of the volume being written.
    pub fn geometry(&self) -> &Geometry {
        &self.geo
    }

    fn free_clusters(&self) -> u64 {
        (self.geo.cluster_count as u64 + 2).saturating_sub(self.next_free as u64)
    }

    /// Bytes still available for file data (whole clusters). A new file may
    /// also need directory space.
    pub fn free_bytes(&self) -> u64 {
        self.free_clusters() * self.geo.cluster_size as u64
    }

    fn no_space(path: &str) -> Error {
        Error::msg(format!(
            "Not enough space on the FAT32 volume for \"{path}\"."
        ))
    }

    /// Splits a path into validated components.
    fn components(path: &str) -> Result<Vec<&str>> {
        let trimmed = path.strip_prefix('/').unwrap_or(path);
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let parts: Vec<&str> = trimmed.split('/').collect();
        for part in &parts {
            validate_name(part)?;
        }
        Ok(parts)
    }

    /// Chooses how `name` is stored in directory `dir`; fails when it exists.
    fn prepare(&self, dir: usize, name: &str) -> Result<Prepared> {
        let units = validate_name(name)?;
        let taken = &self.dirs[dir].names;
        let key = fold(name);
        if taken.contains(&key) {
            return Err(Error::invalid(format!(
                "\"{name}\" already exists in its directory (names are not case-sensitive on FAT32)."
            )));
        }
        if let Some((raw, nt_bits)) = exact_short(name) {
            return Ok(Prepared {
                raw,
                nt_bits,
                lfn: None,
                keys: vec![key],
                slots: 1,
            });
        }
        let raw = make_alias(name, &units, taken)
            .ok_or_else(|| Error::msg(format!("No free short name is left for \"{name}\".")))?;
        let alias_key = short_display(&raw);
        let slots = units.len().div_ceil(13) + 1;
        let mut keys = vec![key];
        if keys[0] != alias_key {
            keys.push(alias_key);
        }
        Ok(Prepared {
            raw,
            nt_bits: 0,
            lfn: Some(units),
            keys,
            slots,
        })
    }

    /// Clusters directory `dir` must grow by to take `slots` more entries.
    fn growth(&self, dir: usize, slots: usize, name: &str) -> Result<u64> {
        let d = &self.dirs[dir];
        let need = d.data.len() + slots * 32;
        if need > MAX_DIR_BYTES {
            return Err(Error::msg(format!(
                "The directory for \"{name}\" is full (a FAT32 directory holds at most 65536 entries)."
            )));
        }
        let cs = self.geo.cluster_size as usize;
        Ok((need.div_ceil(cs) as u64).saturating_sub(d.clusters.len() as u64))
    }

    /// Takes `count` clusters (the caller has checked that they are free).
    fn allocate(&mut self, count: u32) -> usize {
        self.extents.push(Extent {
            start: self.next_free,
            len: count,
            next: 0,
        });
        self.next_free += count;
        self.extents.len() - 1
    }

    fn grow(&mut self, dir: usize, clusters: u64) {
        if clusters == 0 {
            return;
        }
        let start = self.next_free;
        let index = self.allocate(clusters as u32);
        let d = &mut self.dirs[dir];
        self.extents[d.tail_extent].next = start;
        d.tail_extent = index;
        d.clusters.extend(start..start + clusters as u32);
    }

    fn commit(
        &mut self,
        dir: usize,
        p: Prepared,
        attr: u8,
        cluster: u32,
        size: u32,
        time: DosTime,
    ) {
        let d = &mut self.dirs[dir];
        if let Some(units) = &p.lfn {
            d.data.extend_from_slice(&lfn_entries(units, &p.raw));
        }
        d.data
            .extend_from_slice(&short_entry(&p.raw, attr, p.nt_bits, cluster, size, time));
        d.names.extend(p.keys);
    }

    /// Finds or creates the sub-directory `name` of `parent`.
    fn ensure_dir(&mut self, parent: usize, name: &str, path: &str) -> Result<usize> {
        let key = fold(name);
        if let Some(index) = self.dirs[parent].subdirs.get(&key) {
            return Ok(*index);
        }
        if self.dirs[parent].names.contains(&key) {
            return Err(Error::invalid(format!(
                "\"{name}\" in \"{path}\" already exists and is not a directory."
            )));
        }
        let prepared = self.prepare(parent, name)?;
        let growth = self.growth(parent, prepared.slots, name)?;
        if growth + 1 > self.free_clusters() {
            return Err(Self::no_space(path));
        }
        self.grow(parent, growth);
        let cluster = self.next_free;
        let tail_extent = self.allocate(1);
        let dotdot = if parent == 0 {
            0
        } else {
            self.dirs[parent].first_cluster
        };
        let mut data = Vec::with_capacity(64);
        data.extend_from_slice(&short_entry(
            b".          ",
            ATTR_DIR,
            0,
            cluster,
            0,
            self.now,
        ));
        data.extend_from_slice(&short_entry(
            b"..         ",
            ATTR_DIR,
            0,
            dotdot,
            0,
            self.now,
        ));
        self.dirs.push(DirState {
            first_cluster: cluster,
            clusters: vec![cluster],
            tail_extent,
            data,
            names: HashSet::new(),
            subdirs: HashMap::new(),
        });
        let index = self.dirs.len() - 1;
        self.commit(parent, prepared, ATTR_DIR, cluster, 0, self.now);
        self.dirs[parent].subdirs.insert(key, index);
        Ok(index)
    }

    /// Creates a directory and all missing parents. Existing directories are fine.
    pub fn create_dir_all(&mut self, path: &str) -> Result<()> {
        let mut dir = 0;
        for name in Self::components(path)? {
            dir = self.ensure_dir(dir, name, path)?;
        }
        Ok(())
    }

    /// Writes one file of exactly `size` bytes read from `data`, creating
    /// missing parent directories.
    ///
    /// Refuses sizes above 4 GiB - 1 (`Error::Invalid`), names that already
    /// exist, invalid names, and a source that delivers fewer or more bytes
    /// than `size`. Data is streamed in 1 MiB pieces; `progress` receives the
    /// bytes of this file written so far. `mtime` defaults to the time the
    /// writer was opened.
    pub fn write_file(
        &mut self,
        path: &str,
        size: u64,
        data: &mut dyn Read,
        mtime: Option<DosTime>,
        cancel: &Cancel,
        progress: ProgressFn,
    ) -> Result<()> {
        if size > MAX_FILE_SIZE {
            return Err(Error::invalid(format!(
                "\"{path}\" is {size} bytes; FAT32 cannot store files of 4 GiB or more."
            )));
        }
        let mut parts = Self::components(path)?;
        let Some(name) = parts.pop() else {
            return Err(Error::invalid("The file has no name."));
        };
        let cs = self.geo.cluster_size as u64;
        let clusters = size.div_ceil(cs);
        if clusters > self.free_clusters() {
            return Err(Self::no_space(path));
        }
        let mut dir = 0;
        for parent in parts {
            dir = self.ensure_dir(dir, parent, path)?;
        }
        let prepared = self.prepare(dir, name)?;
        let growth = self.growth(dir, prepared.slots, name)?;
        if clusters + growth > self.free_clusters() {
            return Err(Self::no_space(path));
        }
        self.grow(dir, growth);

        // Stream the data into the clusters right after the last allocation.
        // Nothing is recorded until all of it has arrived.
        let first = self.next_free;
        let mut offset = self.geo.cluster_offset(first);
        let mut written = 0u64;
        self.buf.resize(CHUNK, 0);
        while written < size {
            cancel.check()?;
            let want = (size - written).min(CHUNK as u64) as usize;
            let got = read_full(data, &mut self.buf[..want])
                .map_err(|e| Error::io(format!("reading the data for \"{path}\""), e))?;
            if got < want {
                return Err(Error::invalid(format!(
                    "The source of \"{path}\" ended after {} of {size} bytes.",
                    written + got as u64
                )));
            }
            let padded = want.div_ceil(cs as usize) * cs as usize;
            self.buf[want..padded].fill(0);
            self.dev
                .write_at(offset, &self.buf[..padded])
                .map_err(|e| Error::io(format!("writing \"{path}\""), e))?;
            offset += padded as u64;
            written += want as u64;
            progress(written);
        }
        let mut extra = [0u8; 1];
        let more = read_full(data, &mut extra)
            .map_err(|e| Error::io(format!("reading the data for \"{path}\""), e))?;
        if more != 0 {
            return Err(Error::invalid(format!(
                "The source of \"{path}\" holds more than the expected {size} bytes."
            )));
        }
        cancel.check()?;

        let first_cluster = if clusters == 0 {
            0
        } else {
            self.allocate(clusters as u32);
            first
        };
        self.commit(
            dir,
            prepared,
            ATTR_ARCHIVE,
            first_cluster,
            size as u32,
            mtime.unwrap_or(self.now),
        );
        progress(size);
        Ok(())
    }

    /// Writes the directories, both FATs and FSInfo (free count, next free
    /// cluster), then flushes the device.
    pub fn finish(self) -> Result<()> {
        let Fat32Writer {
            dev,
            geo,
            next_free,
            extents,
            dirs,
            ..
        } = self;
        let cs = geo.cluster_size as usize;

        // Directories, in runs of adjacent clusters.
        for mut dir in dirs {
            dir.data.resize(dir.clusters.len() * cs, 0);
            let mut i = 0;
            while i < dir.clusters.len() {
                let mut j = i + 1;
                while j < dir.clusters.len()
                    && dir.clusters[j] == dir.clusters[j - 1] + 1
                    && (j - i + 1) * cs <= CHUNK
                {
                    j += 1;
                }
                dev.write_at(
                    geo.cluster_offset(dir.clusters[i]),
                    &dir.data[i * cs..j * cs],
                )
                .ctx("writing a directory")?;
                i = j;
            }
        }

        // The FAT, generated from the extents and written to both copies.
        let fat_bytes = geo.fat_sectors as u64 * geo.sector_size as u64;
        let mut buf = vec![0u8; CHUNK.min(fat_bytes as usize)];
        let (mut extent, mut within) = (0usize, 0u32);
        let mut index = 0u64;
        let mut offset = 0u64;
        while offset < fat_bytes {
            let n = (fat_bytes - offset).min(CHUNK as u64) as usize;
            for slot in buf[..n].as_chunks_mut::<4>().0 {
                let value = match index {
                    0 => 0x0FFF_FF00 | MEDIA as u32,
                    1 => EOC,
                    _ => match extents.get(extent) {
                        None => 0,
                        Some(e) => {
                            let v = if within + 1 < e.len {
                                e.start + within + 1
                            } else if e.next != 0 {
                                e.next
                            } else {
                                EOC
                            };
                            within += 1;
                            if within == e.len {
                                extent += 1;
                                within = 0;
                            }
                            v
                        }
                    },
                };
                *slot = value.to_le_bytes();
                index += 1;
            }
            for k in 0..geo.num_fats {
                dev.write_at(geo.fat_offset(k) + offset, &buf[..n])
                    .ctx("writing the FAT")?;
            }
            offset += n as u64;
        }

        // FSInfo, main and backup.
        let end = geo.cluster_count + 2;
        let info = fsinfo_sector(
            geo.sector_size,
            end - next_free,
            if next_free < end {
                next_free
            } else {
                0xFFFF_FFFF
            },
        );
        let s = geo.sector_size as u64;
        dev.write_at(FSINFO_SECTOR as u64 * s, &info)
            .ctx("writing the FSInfo sector")?;
        dev.write_at((BACKUP_BOOT_SECTOR + FSINFO_SECTOR) as u64 * s, &info)
            .ctx("writing the backup FSInfo sector")?;
        dev.flush().ctx("flushing the FAT32 volume")?;
        Ok(())
    }
}

/// Reads until `buf` is full or the source ends; returns the bytes read.
fn read_full(source: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match source.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

// --------------------------------------------------------------------------------------
// Reader and checker
// --------------------------------------------------------------------------------------

/// One parsed directory entry (long name already attached).
struct RawEntry {
    name: String,
    short: String,
    attr: u8,
    cluster: u32,
    size: u32,
    mtime: DosTime,
}

struct LfnState {
    /// Pieces in on-disk order (highest sequence number first).
    pieces: Vec<[u16; 13]>,
    /// The sequence number expected next (0: complete).
    next: u8,
    sum: u8,
}

/// Parses the entries of a directory up to the end marker, reassembling long
/// names and validating their checksums.
fn parse_dir(data: &[u8], dir: &str, problems: &mut Vec<String>) -> Vec<RawEntry> {
    let mut out = Vec::new();
    let mut lfn: Option<LfnState> = None;
    for e in data.as_chunks::<32>().0 {
        if e[0] == 0x00 {
            break;
        }
        if e[0] == 0xE5 {
            if lfn.take().is_some() {
                problem(
                    problems,
                    format!(
                        "Directory \"{dir}\": long-name entries are followed by a deleted entry."
                    ),
                );
            }
            continue;
        }
        if e[11] & 0x3F == ATTR_LFN {
            let seq = e[0] & 0x3F;
            let mut piece = [0u16; 13];
            for (unit, at) in piece.iter_mut().zip(LFN_SLOTS) {
                *unit = le16(e, at);
            }
            if e[0] & 0x40 != 0 {
                if lfn.is_some() {
                    problem(
                        problems,
                        format!(
                            "Directory \"{dir}\": an unfinished long name is cut off by another."
                        ),
                    );
                }
                if seq == 0 || seq > 20 {
                    problem(
                        problems,
                        format!("Directory \"{dir}\": invalid long-name sequence number {seq}."),
                    );
                    lfn = None;
                } else {
                    lfn = Some(LfnState {
                        pieces: vec![piece],
                        next: seq - 1,
                        sum: e[13],
                    });
                }
            } else {
                match &mut lfn {
                    Some(state) if seq >= 1 && state.next == seq && state.sum == e[13] => {
                        state.pieces.push(piece);
                        state.next -= 1;
                    }
                    _ => {
                        problem(
                            problems,
                            format!("Directory \"{dir}\": stray or out-of-order long-name entry."),
                        );
                        lfn = None;
                    }
                }
            }
            continue;
        }

        let mut raw = [0u8; 11];
        raw.copy_from_slice(&e[..11]);
        if raw[0] == 0x05 {
            raw[0] = 0xE5;
        }
        let attr = e[11];
        let short = short_display(&raw);
        let mut name = if attr & ATTR_VOLUME != 0 {
            String::from_utf8_lossy(&raw).trim_end().to_string()
        } else {
            // Lower case recorded in the NT case bits.
            let base = short_display(&short_raw(&raw[..8], b""));
            let ext = short_display(&short_raw(&raw[8..], b""));
            let base = if e[12] & NT_LOWER_BASE != 0 {
                base.to_ascii_lowercase()
            } else {
                base
            };
            let ext = if e[12] & NT_LOWER_EXT != 0 {
                ext.to_ascii_lowercase()
            } else {
                ext
            };
            if ext.is_empty() {
                base
            } else {
                format!("{base}.{ext}")
            }
        };
        if let Some(state) = lfn.take() {
            if state.next == 0 && state.sum == lfn_checksum(&raw) {
                let units: Vec<u16> = state
                    .pieces
                    .iter()
                    .rev()
                    .flatten()
                    .copied()
                    .take_while(|u| *u != 0)
                    .collect();
                name = String::from_utf16_lossy(&units);
            } else {
                problem(
                    problems,
                    format!(
                        "Directory \"{dir}\": the long-name entries before \"{short}\" do not belong to it (incomplete or wrong checksum)."
                    ),
                );
            }
        }
        out.push(RawEntry {
            name,
            short,
            attr,
            cluster: (le16(e, 20) as u32) << 16 | le16(e, 26) as u32,
            size: le32(e, 28),
            mtime: DosTime {
                date: le16(e, 24),
                time: le16(e, 22),
            },
        });
    }
    if lfn.is_some() {
        problem(
            problems,
            format!("Directory \"{dir}\": long-name entries at the end belong to no file."),
        );
    }
    out
}

/// A cluster chain as extents `(first cluster, length)`.
struct Chain {
    extents: Vec<(u32, u32)>,
    clusters: u64,
    /// Why the walk stopped early, when it did.
    problem: Option<String>,
}

fn bit(map: &[u64], index: u32) -> bool {
    map[index as usize / 64] >> (index % 64) & 1 != 0
}

fn set_bit(map: &mut [u64], index: u32) {
    map[index as usize / 64] |= 1 << (index % 64);
}

/// An independent FAT32 reader: holds the layout and the first FAT in memory
/// (4 bytes per cluster); the device is passed to each call.
pub struct Fat32Reader {
    geo: Geometry,
    fat: Vec<u32>,
}

impl Fat32Reader {
    /// Reads the boot sector and the first FAT. Fails when the boot sector
    /// is not a usable FAT32 boot sector or does not fit the device.
    pub fn open(dev: &mut dyn BlockDevice) -> Result<Fat32Reader> {
        let geo = read_boot(dev)?.geo;
        Fat32Reader::load(dev, geo)
    }

    fn load(dev: &mut dyn BlockDevice, geo: Geometry) -> Result<Fat32Reader> {
        // `read_boot` has checked that the FAT lies inside the device and
        // covers cluster_count + 2 entries.
        let entries = geo.cluster_count as usize + 2;
        let s = geo.sector_size as usize;
        let bytes = (entries * 4).div_ceil(s) * s;
        let mut fat = Vec::with_capacity(entries);
        let mut buf = vec![0u8; CHUNK.min(bytes)];
        let mut offset = 0usize;
        while offset < bytes {
            let n = (bytes - offset).min(CHUNK);
            dev.read_at(geo.fat_offset(0) + offset as u64, &mut buf[..n])
                .ctx("reading the FAT")?;
            fat.extend(
                buf[..n]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_le_bytes(*c) & FAT_MASK)
                    .take(entries - fat.len()),
            );
            offset += n;
        }
        Ok(Fat32Reader { geo, fat })
    }

    pub fn geometry(&self) -> &Geometry {
        &self.geo
    }

    fn max_dir_clusters(&self) -> u64 {
        MAX_DIR_BYTES.div_ceil(self.geo.cluster_size as usize) as u64
    }

    /// Follows a chain for at most `limit` clusters. With `visited`, every
    /// cluster is claimed and a cluster claimed before stops the walk
    /// (cross-link or loop).
    fn chain(&self, first: u32, limit: u64, mut visited: Option<&mut [u64]>) -> Chain {
        let mut chain = Chain {
            extents: Vec::new(),
            clusters: 0,
            problem: None,
        };
        let end = self.geo.cluster_count + 2;
        let mut cur = first;
        loop {
            if cur < 2 || cur >= end {
                chain.problem = Some(format!("points outside the volume (cluster {cur})"));
                break;
            }
            if chain.clusters >= limit {
                chain.problem = Some(format!("is longer than {limit} clusters or loops"));
                break;
            }
            if let Some(map) = visited.as_deref_mut() {
                if bit(map, cur) {
                    chain.problem = Some(format!(
                        "is cross-linked with another chain or loops (cluster {cur})"
                    ));
                    break;
                }
                set_bit(map, cur);
            }
            match chain.extents.last_mut() {
                Some((start, len)) if *start as u64 + *len as u64 == cur as u64 => *len += 1,
                _ => chain.extents.push((cur, 1)),
            }
            chain.clusters += 1;
            let next = self.fat[cur as usize];
            if next >= EOC_MIN {
                break;
            }
            if next == BAD_CLUSTER {
                chain.problem = Some(format!("runs into a bad-cluster mark after cluster {cur}"));
                break;
            }
            if next == 0 {
                chain.problem = Some(format!("runs into a free cluster after cluster {cur}"));
                break;
            }
            cur = next;
        }
        chain
    }

    /// Reads the clusters of a (directory) chain; the caller bounds its length.
    fn read_chain(&self, dev: &mut dyn BlockDevice, chain: &Chain) -> Result<Vec<u8>> {
        let cs = self.geo.cluster_size as usize;
        let mut data = vec![0u8; chain.clusters as usize * cs];
        let mut at = 0usize;
        for &(first, len) in &chain.extents {
            let mut offset = self.geo.cluster_offset(first);
            let mut left = len as usize * cs;
            while left > 0 {
                let n = left.min(CHUNK.max(cs));
                dev.read_at(offset, &mut data[at..at + n])
                    .ctx("reading a directory")?;
                at += n;
                offset += n as u64;
                left -= n;
            }
        }
        Ok(data)
    }

    fn dir_entries(
        &self,
        dev: &mut dyn BlockDevice,
        first: u32,
        shown: &str,
        problems: &mut Vec<String>,
    ) -> Result<Vec<RawEntry>> {
        let chain = self.chain(first, self.max_dir_clusters(), None);
        let data = self.read_chain(dev, &chain)?;
        Ok(parse_dir(&data, shown, problems))
    }

    /// Looks a path up (forward slashes; long or short names, any case).
    pub fn stat(&self, dev: &mut dyn BlockDevice, path: &str) -> Result<FoundFile> {
        let not_found = || Error::invalid(format!("\"{path}\" was not found on the FAT32 volume."));
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() || parts.len() > MAX_DEPTH + 1 {
            return Err(not_found());
        }
        let mut dir = self.geo.root_cluster;
        let mut walked = String::new();
        let mut scratch = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let key = fold(part);
            let entry = self
                .dir_entries(dev, dir, &walked, &mut scratch)?
                .into_iter()
                .find(|e| e.attr & ATTR_VOLUME == 0 && (fold(&e.name) == key || e.short == key))
                .ok_or_else(not_found)?;
            scratch.clear();
            if !walked.is_empty() {
                walked.push('/');
            }
            walked.push_str(&entry.name);
            let is_dir = entry.attr & ATTR_DIR != 0;
            if i + 1 == parts.len() {
                return Ok(FoundFile {
                    path: walked,
                    size: if is_dir { 0 } else { entry.size as u64 },
                    is_dir,
                    first_cluster: entry.cluster,
                    short_name: entry.short,
                    mtime: entry.mtime,
                });
            }
            if !is_dir {
                return Err(not_found());
            }
            dir = entry.cluster;
        }
        Err(not_found())
    }

    /// Opens a file found by `check` or `stat` for streaming, without
    /// walking the directories again. Fails when the cluster chain does not
    /// match the size.
    pub fn open_found<'d>(
        &self,
        dev: &'d mut dyn BlockDevice,
        file: &FoundFile,
    ) -> Result<FileStream<'d>> {
        if file.is_dir {
            return Err(Error::invalid(format!("\"{}\" is a directory.", file.path)));
        }
        let cs = self.geo.cluster_size as u64;
        let need = file.size.div_ceil(cs);
        let mut extents = Vec::new();
        if need > 0 {
            let chain = self.chain(file.first_cluster, need, None);
            if chain.problem.is_some() || chain.clusters != need {
                return Err(Error::invalid(format!(
                    "\"{}\" is damaged: its cluster chain does not match its size.",
                    file.path
                )));
            }
            extents = chain
                .extents
                .iter()
                .map(|&(first, len)| (self.geo.cluster_offset(first), len as u64 * cs))
                .collect();
        }
        Ok(FileStream {
            dev,
            extents,
            extent: 0,
            within: 0,
            remaining: file.size,
            size: file.size,
            buf: Vec::new(),
            buf_pos: 0,
        })
    }

    /// Opens a file by path for streaming.
    pub fn open_file<'d>(
        &self,
        dev: &'d mut dyn BlockDevice,
        path: &str,
    ) -> Result<FileStream<'d>> {
        let found = self.stat(dev, path)?;
        self.open_found(dev, &found)
    }

    /// Reads a whole file into memory.
    pub fn read_file(&self, dev: &mut dyn BlockDevice, path: &str) -> Result<Vec<u8>> {
        let mut stream = self.open_file(dev, path)?;
        let mut data = Vec::new();
        stream
            .read_to_end(&mut data)
            .map_err(|e| Error::io(format!("reading \"{path}\""), e))?;
        Ok(data)
    }

    /// Walks every directory and chain. Returns the files, the root label
    /// and the map of clusters that belong to something.
    fn walk(
        &self,
        dev: &mut dyn BlockDevice,
        problems: &mut Vec<String>,
    ) -> Result<(Vec<FoundFile>, Option<String>, Vec<u64>)> {
        let cs = self.geo.cluster_size as u64;
        let mut visited = vec![0u64; (self.geo.cluster_count as usize + 2) / 64 + 1];
        let mut files = Vec::new();
        let mut label = None;
        // (path, first cluster, expected ".." cluster, depth)
        let mut stack = vec![(String::new(), self.geo.root_cluster, 0u32, 0usize)];
        while let Some((path, first, dotdot, depth)) = stack.pop() {
            let chain = self.chain(first, self.max_dir_clusters(), Some(&mut visited));
            if let Some(why) = &chain.problem {
                problem(
                    problems,
                    format!("Directory \"{path}\": its cluster chain {why}."),
                );
            }
            if chain.clusters == 0 {
                continue;
            }
            let data = self.read_chain(dev, &chain)?;
            let entries = parse_dir(&data, &path, problems);
            let mut names: HashSet<String> = HashSet::new();
            let mut dots = 0;
            for (i, e) in entries.into_iter().enumerate() {
                if e.attr & ATTR_VOLUME != 0 {
                    if depth == 0 && label.is_none() {
                        label = Some(e.name);
                    } else {
                        problem(
                            problems,
                            format!("Directory \"{path}\": unexpected volume-label entry."),
                        );
                    }
                    continue;
                }
                let is_dir = e.attr & ATTR_DIR != 0;
                if e.name == "." || e.name == ".." {
                    let (slot, want) = if e.name == "." {
                        (0, first)
                    } else {
                        (1, dotdot)
                    };
                    if depth == 0 || !is_dir || i != slot || e.cluster != want {
                        problem(
                            problems,
                            format!("Directory \"{path}\": bad \"{}\" entry.", e.name),
                        );
                    } else {
                        dots += 1;
                    }
                    continue;
                }
                let full = if path.is_empty() {
                    e.name.clone()
                } else {
                    format!("{path}/{}", e.name)
                };
                if !names.insert(fold(&e.name)) | !names.insert(format!("\u{0}{}", e.short)) {
                    problem(
                        problems,
                        format!(
                            "\"{full}\": its long or short name occurs twice in the directory."
                        ),
                    );
                }
                if is_dir {
                    if e.size != 0 {
                        problem(problems, format!("Directory \"{full}\": non-zero size."));
                    }
                    if depth >= MAX_DEPTH {
                        problem(
                            problems,
                            format!("Directory \"{full}\": nested too deeply; not examined."),
                        );
                    } else {
                        stack.push((
                            full.clone(),
                            e.cluster,
                            if depth == 0 { 0 } else { first },
                            depth + 1,
                        ));
                    }
                } else if e.size == 0 {
                    if e.cluster != 0 {
                        problem(
                            problems,
                            format!("\"{full}\": an empty file with a cluster chain."),
                        );
                        self.chain(e.cluster, u64::MAX, Some(&mut visited));
                    }
                } else {
                    let need = (e.size as u64).div_ceil(cs);
                    let chain = self.chain(e.cluster, u64::MAX, Some(&mut visited));
                    if let Some(why) = &chain.problem {
                        problem(problems, format!("\"{full}\": its cluster chain {why}."));
                    } else if chain.clusters != need {
                        problem(
                            problems,
                            format!(
                                "\"{full}\": has {} clusters but its size ({} bytes) needs {need}.",
                                chain.clusters, e.size
                            ),
                        );
                    }
                }
                files.push(FoundFile {
                    path: full,
                    size: if is_dir { 0 } else { e.size as u64 },
                    is_dir,
                    first_cluster: e.cluster,
                    short_name: e.short,
                    mtime: e.mtime,
                });
            }
            if depth > 0 && dots != 2 {
                problem(
                    problems,
                    format!("Directory \"{path}\": \".\" and \"..\" entries missing or wrong."),
                );
            }
        }
        Ok((files, label, visited))
    }
}

/// A file being read back; implements `std::io::Read`.
pub struct FileStream<'d> {
    dev: &'d mut dyn BlockDevice,
    /// (byte offset on the device, length in bytes) of each run of clusters.
    extents: Vec<(u64, u64)>,
    extent: usize,
    within: u64,
    /// File bytes not yet loaded into `buf`.
    remaining: u64,
    size: u64,
    buf: Vec<u8>,
    buf_pos: usize,
}

impl FileStream<'_> {
    /// The size of the file in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }
}

impl Read for FileStream<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf_pos == self.buf.len() {
            if self.remaining == 0 || out.is_empty() {
                return Ok(0);
            }
            let Some(&(offset, len)) = self.extents.get(self.extent) else {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "cluster chain shorter than the file",
                ));
            };
            // Whole clusters, so always a multiple of the sector size.
            let n = (len - self.within).min(CHUNK as u64) as usize;
            self.buf.resize(n, 0);
            self.dev.read_at(offset + self.within, &mut self.buf)?;
            self.within += n as u64;
            if self.within == len {
                self.extent += 1;
                self.within = 0;
            }
            self.buf.truncate((n as u64).min(self.remaining) as usize);
            self.remaining -= self.buf.len() as u64;
            self.buf_pos = 0;
        }
        let n = out.len().min(self.buf.len() - self.buf_pos);
        out[..n].copy_from_slice(&self.buf[self.buf_pos..self.buf_pos + n]);
        self.buf_pos += n;
        Ok(n)
    }
}

/// Reads a whole file by path (see `Fat32Reader` to read many files).
pub fn read_file(dev: &mut dyn BlockDevice, path: &str) -> Result<Vec<u8>> {
    Fat32Reader::open(dev)?.read_file(dev, path)
}

/// Opens a file by path for streaming (see `Fat32Reader` to read many files).
pub fn open_file<'d>(dev: &'d mut dyn BlockDevice, path: &str) -> Result<FileStream<'d>> {
    Fat32Reader::open(dev)?.open_file(dev, path)
}

/// Checks a FAT32 volume by reading it back, independently of the writer.
///
/// Validates the boot sector and that its backup is identical, the FSInfo
/// signatures (main and backup), that all FAT copies are equal, the reserved
/// FAT entries; walks every directory (long names reassembled, checksums
/// validated, `.`/`..` entries, duplicate names); follows every cluster chain
/// (range, loops, cross-links, bad or free clusters inside a chain) and
/// compares its length with the file size; counts lost clusters; recounts
/// the free clusters and compares with FSInfo.
///
/// `Err` is returned for I/O errors and when the boot sector is not a usable
/// FAT32 boot sector (including a volume larger than the device); everything
/// else is described in `problems`. Never panics on garbage. Memory: 4 bytes
/// per cluster for the FAT plus one bit per cluster.
pub fn check(dev: &mut dyn BlockDevice) -> Result<FsReport> {
    let boot = read_boot(dev)?;
    let geo = boot.geo.clone();
    let b = &boot.sector0;
    let s = geo.sector_size as u64;
    let mut problems: Vec<String> = Vec::new();
    let p = &mut problems;

    // Boot sector details that do not stop the reader.
    if !(b[0] == 0xEB && b[2] == 0x90) && b[0] != 0xE9 {
        problem(p, "Boot sector: unexpected jump instruction.".to_string());
    }
    if b[21] != 0xF0 && b[21] < 0xF8 {
        problem(
            p,
            format!("Boot sector: invalid media byte 0x{:02X}.", b[21]),
        );
    }
    if le16(b, 17) != 0 || le16(b, 19) != 0 {
        problem(
            p,
            "Boot sector: FAT12/16 fields (root entries, 16-bit sector count) are not zero."
                .to_string(),
        );
    }
    if le16(b, 40) & 0x0080 != 0 {
        problem(p, "Boot sector: FAT mirroring is switched off.".to_string());
    }
    if le16(b, 42) != 0 {
        problem(p, "Boot sector: unknown FAT32 version.".to_string());
    }
    if b[66] != 0x29 || &b[82..90] != b"FAT32   " {
        problem(
            p,
            "Boot sector: extended boot signature or \"FAT32\" type string missing.".to_string(),
        );
    }

    // Backup boot sector.
    if boot.backup_sector == 0 || boot.backup_sector >= geo.reserved_sectors {
        problem(p, "Boot sector: no backup boot sector.".to_string());
    } else {
        let mut copy = vec![0u8; s as usize];
        dev.read_at(boot.backup_sector as u64 * s, &mut copy)
            .ctx("reading the backup boot sector")?;
        if copy != *b {
            problem(
                p,
                "The backup boot sector differs from the boot sector.".to_string(),
            );
        }
    }

    // FSInfo, main and backup.
    let mut info_free = 0xFFFF_FFFFu32;
    let mut info_next = 0xFFFF_FFFFu32;
    if boot.fsinfo_sector == 0 || boot.fsinfo_sector >= geo.reserved_sectors {
        problem(p, "Boot sector: no FSInfo sector.".to_string());
    } else {
        let mut sectors = vec![(boot.fsinfo_sector, "FSInfo sector")];
        let backup = boot.backup_sector + boot.fsinfo_sector;
        if boot.backup_sector != 0 && backup < geo.reserved_sectors {
            sectors.push((backup, "Backup FSInfo sector"));
        }
        for (index, (sector, what)) in sectors.into_iter().enumerate() {
            let mut info = vec![0u8; s as usize];
            dev.read_at(sector as u64 * s, &mut info)
                .ctx("reading the FSInfo sector")?;
            if le32(&info, 0) != 0x4161_5252
                || le32(&info, 484) != 0x6141_7272
                || le32(&info, 508) != 0xAA55_0000
            {
                problem(p, format!("{what}: signatures missing."));
            } else if index == 0 {
                info_free = le32(&info, 488);
                info_next = le32(&info, 492);
            }
        }
    }

    // FAT copies must be identical.
    let fat_bytes = geo.fat_sectors as u64 * s;
    let n_max = CHUNK.min(fat_bytes as usize);
    let (mut a, mut c) = (vec![0u8; n_max], vec![0u8; n_max]);
    'copies: for k in 1..geo.num_fats {
        let mut offset = 0u64;
        while offset < fat_bytes {
            let n = (fat_bytes - offset).min(CHUNK as u64) as usize;
            dev.read_at(geo.fat_offset(0) + offset, &mut a[..n])
                .ctx("reading the FAT")?;
            dev.read_at(geo.fat_offset(k) + offset, &mut c[..n])
                .ctx("reading the second FAT")?;
            if let Some(i) = a[..n].iter().zip(&c[..n]).position(|(x, y)| x != y) {
                problem(
                    p,
                    format!(
                        "The FAT copies differ (first difference at entry {}).",
                        (offset + i as u64) / 4
                    ),
                );
                break 'copies;
            }
            offset += n as u64;
        }
    }

    let reader = Fat32Reader::load(dev, geo.clone())?;
    if reader.fat[0] != 0x0FFF_FF00 | b[21] as u32 {
        problem(
            p,
            format!(
                "FAT entry 0 is 0x{:08X}; it should carry the media byte.",
                reader.fat[0]
            ),
        );
    }
    if reader.fat[1] | 0x0C00_0000 != EOC {
        problem(p, format!("FAT entry 1 is 0x{:08X}.", reader.fat[1]));
    } else if reader.fat[1] != EOC {
        problem(
            p,
            "The volume is marked dirty or as having had I/O errors.".to_string(),
        );
    }

    let (files, root_label, visited) = reader.walk(dev, p)?;

    // Lost clusters and the free count.
    let (mut free, mut lost) = (0u32, 0u32);
    for cluster in 2..geo.cluster_count + 2 {
        let value = reader.fat[cluster as usize];
        if value == 0 {
            free += 1;
        } else if value != BAD_CLUSTER && !bit(&visited, cluster) {
            lost += 1;
        }
    }
    if lost > 0 {
        problem(
            p,
            format!("{lost} cluster(s) are marked in use but belong to no file."),
        );
    }
    if info_free != 0xFFFF_FFFF && info_free != free {
        problem(
            p,
            format!("FSInfo says {info_free} clusters are free; the FAT has {free}."),
        );
    }
    if info_next != 0xFFFF_FFFF && (info_next < 2 || info_next >= geo.cluster_count + 2) {
        problem(
            p,
            format!("FSInfo: the next-free hint ({info_next}) is out of range."),
        );
    }

    Ok(FsReport {
        label: root_label.unwrap_or_else(|| geo.label.clone()),
        cluster_size: geo.cluster_size,
        total_clusters: geo.cluster_count,
        free_clusters: free,
        files,
        problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::{FileDevice, MemDevice, SubDevice};
    use crate::part::testdev::{SparseDevice, XorShift};

    const GIB: u64 = 1024 * MIB;
    const NONE: Vec<String> = Vec::new();
    /// The smallest comfortable FAT32 volume: 68528 clusters of 512 bytes.
    const SMALL: u64 = 34 * MIB;

    fn opts(label: &str, cluster: Option<u32>) -> FormatOptions {
        FormatOptions {
            label: label.to_string(),
            cluster_size: cluster,
            hidden_sectors: 2048,
            volume_id: 0xCAFE_F00D,
            zero_data: false,
        }
    }

    fn fmt(dev: &mut dyn BlockDevice, label: &str, cluster: Option<u32>) -> Geometry {
        format(dev, &opts(label, cluster), &Cancel::new(), &mut |_| {}).unwrap()
    }

    fn small() -> MemDevice {
        let mut d = MemDevice::new(SMALL, 512);
        fmt(&mut d, "TEST", None);
        d
    }

    fn bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1).fill(&mut v);
        v
    }

    fn put(w: &mut Fat32Writer, path: &str, data: &[u8]) -> Result<()> {
        w.write_file(
            path,
            data.len() as u64,
            &mut &data[..],
            None,
            &Cancel::new(),
            &mut |_| {},
        )
    }

    fn has(problems: &[String], needle: &str) -> bool {
        problems.iter().any(|p| p.contains(needle))
    }

    fn clone_dev(d: &MemDevice) -> MemDevice {
        let mut c = MemDevice::new(d.data.len() as u64, d.sector_size());
        c.data.copy_from_slice(&d.data);
        c
    }

    #[test]
    fn dos_time() {
        let t = DosTime::new(2024, 2, 29, 13, 37, 59);
        assert_eq!(t.date, (44 << 9) | (2 << 5) | 29);
        assert_eq!(t.time, (13 << 11) | (37 << 5) | 29);
        assert_eq!(t.parts(), (2024, 2, 29, 13, 37, 58));
        // 1700000000 = 2023-11-14 22:13:20 UTC.
        assert_eq!(
            DosTime::from_unix(1_700_000_000).parts(),
            (2023, 11, 14, 22, 13, 20)
        );
        // 951782400 = 2000-02-29 00:00:00; 946684799 = 1999-12-31 23:59:59.
        assert_eq!(
            DosTime::from_unix(951_782_400).parts(),
            (2000, 2, 29, 0, 0, 0)
        );
        assert_eq!(
            DosTime::from_unix(946_684_799).parts(),
            (1999, 12, 31, 23, 59, 58)
        );
        assert_eq!(
            DosTime::from_unix(315_532_800).parts(),
            (1980, 1, 1, 0, 0, 0)
        );
        // Clamped at both ends.
        assert_eq!(DosTime::from_unix(0).parts(), (1980, 1, 1, 0, 0, 0));
        assert_eq!(DosTime::from_unix(-5).parts(), (1980, 1, 1, 0, 0, 0));
        assert_eq!(
            DosTime::from_unix(i64::MAX / 2).parts(),
            (2107, 12, 31, 23, 59, 58)
        );
        assert_eq!(
            DosTime::new(1900, 0, 0, 99, 99, 99).parts(),
            (1980, 1, 1, 23, 59, 58)
        );
        assert!(DosTime::now().parts().0 >= 2024);
    }

    #[test]
    fn labels() {
        assert_eq!(sanitize_label("  win 11 setup  "), "WIN 11 SETU");
        assert_eq!(sanitize_label("a.b*c?d"), "A_B_C_D");
        assert_eq!(sanitize_label("CCCOMA_X64FRE_EN-US_DV9"), "CCCOMA_X64F");
        assert_eq!(sanitize_label("\u{00e9}t\u{00e9} 2024"), "_T_ 2024");
        assert_eq!(sanitize_label("0123456789 X"), "0123456789");
        assert_eq!(sanitize_label(""), "");
        assert_eq!(sanitize_label("   "), "");
        assert_eq!(sanitize_label("tab\there"), "TAB_HERE");
        assert!(sanitize_label("\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}").len() <= 11);
    }

    #[test]
    fn cluster_size_tables() {
        // Microsoft's defaults.
        assert_eq!(default_cluster_size(64 * MIB, 512), Some(512));
        assert_eq!(default_cluster_size(260 * MIB, 512), Some(512));
        assert_eq!(default_cluster_size(260 * MIB + 512, 512), Some(4096));
        assert_eq!(default_cluster_size(8 * GIB, 512), Some(4096));
        assert_eq!(default_cluster_size(8 * GIB + 512, 512), Some(8192));
        assert_eq!(default_cluster_size(16 * GIB, 512), Some(8192));
        assert_eq!(default_cluster_size(32 * GIB, 512), Some(16384));
        assert_eq!(default_cluster_size(64 * GIB, 512), Some(32768));
        assert_eq!(default_cluster_size(2000 * GIB, 512), Some(32768));
        // Never below the sector; falls back to a valid size.
        assert_eq!(default_cluster_size(512 * MIB, 4096), Some(4096));
        assert_eq!(default_cluster_size(200 * MIB, 4096), None);
        // Impossible volumes.
        assert_eq!(default_cluster_size(16 * MIB, 512), None);
        assert_eq!(default_cluster_size(0, 512), None);
        assert_eq!(default_cluster_size(3000 * GIB, 512), None);
        assert_eq!(default_cluster_size(3000 * GIB, 4096), Some(32768));
        assert!(valid_cluster_sizes(16 * MIB, 512).is_empty());
        assert!(valid_cluster_sizes(GIB, 513).is_empty());

        // 64 MiB: 512-byte clusters give 129022 clusters; 1 KiB would give
        // 65012, below the FAT32 minimum of 65525.
        assert_eq!(valid_cluster_sizes(64 * MIB, 512), vec![512]);
        // 2 GiB: 32 KiB clusters would give 65519 clusters: just too few.
        assert_eq!(
            valid_cluster_sizes(2 * GIB, 512),
            vec![512, 1024, 2048, 4096, 8192, 16384]
        );
        assert_eq!(
            valid_cluster_sizes(32 * GIB, 512),
            vec![512, 1024, 2048, 4096, 8192, 16384, 32768, 65536]
        );
        // 200 GiB with 512-byte clusters would exceed 0x0FFFFFF4 clusters.
        assert_eq!(
            valid_cluster_sizes(200 * GIB, 512),
            vec![1024, 2048, 4096, 8192, 16384, 32768, 65536]
        );
        assert_eq!(valid_cluster_sizes(GIB, 4096), vec![4096, 8192]);
        // Every size reported valid really formats and checks.
        for cluster in valid_cluster_sizes(300 * MIB, 512) {
            let mut d = SparseDevice::new(300 * MIB, 512);
            let geo = fmt(&mut d, "X", Some(cluster));
            assert_eq!(geo.cluster_size, cluster);
            assert_eq!(check(&mut d).unwrap().problems, NONE, "cluster {cluster}");
        }
    }

    #[test]
    fn format_64_mib_byte_level() {
        let mut d = MemDevice::new(64 * MIB, 512);
        let geo = fmt(&mut d, "BootForge", None);
        // By hand: 131072 sectors, 32 reserved, 1 sector per cluster.
        // FAT sectors F: smallest F with 128 F >= (131040 - 2F) + 2, i.e.
        // F >= 131042 / 130 = 1008.02 -> 1009. Clusters: 131040 - 2018 = 129022.
        assert_eq!(geo.fat_sectors, 1009);
        assert_eq!(geo.cluster_count, 129022);
        assert_eq!(geo.data_start_sector, 2050);
        assert_eq!(geo.cluster_size, 512);
        assert_eq!(geo.label, "BOOTFORGE");
        assert_eq!(geo.metadata_bytes(), 2051 * 512);

        let v = &d.data;
        assert_eq!(&v[0..3], &[0xEB, 0x58, 0x90]);
        assert_eq!(&v[3..11], b"MSWIN4.1");
        assert_eq!(&v[11..13], &[0x00, 0x02], "bytes per sector");
        assert_eq!(v[13], 1, "sectors per cluster");
        assert_eq!(&v[14..16], &[32, 0], "reserved sectors");
        assert_eq!(v[16], 2, "FATs");
        assert_eq!(&v[17..21], &[0, 0, 0, 0], "root entries, 16-bit total");
        assert_eq!(v[21], 0xF8, "media");
        assert_eq!(&v[22..24], &[0, 0], "16-bit FAT size");
        assert_eq!(&v[24..26], &[63, 0], "sectors per track");
        assert_eq!(&v[26..28], &[255, 0], "heads");
        assert_eq!(&v[28..32], &2048u32.to_le_bytes(), "hidden sectors");
        assert_eq!(&v[32..36], &131072u32.to_le_bytes(), "total sectors");
        assert_eq!(&v[36..40], &1009u32.to_le_bytes(), "FAT size");
        assert_eq!(&v[40..44], &[0, 0, 0, 0], "flags, version");
        assert_eq!(&v[44..48], &2u32.to_le_bytes(), "root cluster");
        assert_eq!(&v[48..50], &[1, 0], "FSInfo sector");
        assert_eq!(&v[50..52], &[6, 0], "backup boot sector");
        assert!(v[52..64].iter().all(|b| *b == 0));
        assert_eq!(v[64], 0x80, "drive number");
        assert_eq!(v[65], 0);
        assert_eq!(v[66], 0x29, "extended boot signature");
        assert_eq!(&v[67..71], &[0x0D, 0xF0, 0xFE, 0xCA], "volume id");
        assert_eq!(&v[71..82], b"BOOTFORGE  ");
        assert_eq!(&v[82..90], b"FAT32   ");
        assert_eq!(&v[90..95], &[0xCD, 0x18, 0xF4, 0xEB, 0xFD]);
        assert!(
            v[95..510].iter().all(|b| *b == 0),
            "no boot code beyond the stub"
        );
        assert_eq!(&v[510..512], &[0x55, 0xAA]);

        // FSInfo in sector 1.
        let info = &v[512..1024];
        assert_eq!(&info[0..4], b"RRaA");
        assert!(info[4..484].iter().all(|b| *b == 0));
        assert_eq!(&info[484..488], b"rrAa");
        assert_eq!(&info[488..492], &129021u32.to_le_bytes(), "free clusters");
        assert_eq!(&info[492..496], &3u32.to_le_bytes(), "next free");
        assert!(info[496..508].iter().all(|b| *b == 0));
        assert_eq!(&info[508..512], &[0x00, 0x00, 0x55, 0xAA]);
        // Sectors 2..6 empty; backups in 6 and 7; the rest of the reserved area empty.
        assert!(v[1024..6 * 512].iter().all(|b| *b == 0));
        assert_eq!(&v[6 * 512..7 * 512], &v[0..512]);
        assert_eq!(&v[7 * 512..8 * 512], &v[512..1024]);
        assert!(v[8 * 512..32 * 512].iter().all(|b| *b == 0));

        // Both FATs.
        for start in [32 * 512, (32 + 1009) * 512] {
            let fat = &v[start..start + 1009 * 512];
            assert_eq!(&fat[0..4], &[0xF8, 0xFF, 0xFF, 0x0F]);
            assert_eq!(&fat[4..8], &[0xFF, 0xFF, 0xFF, 0x0F]);
            assert_eq!(&fat[8..12], &[0xFF, 0xFF, 0xFF, 0x0F]);
            assert!(fat[12..].iter().all(|b| *b == 0));
        }

        // Root directory: one volume-label entry.
        let root = &v[2050 * 512..2051 * 512];
        assert_eq!(&root[0..11], b"BOOTFORGE  ");
        assert_eq!(root[11], 0x08);
        assert_eq!(&root[26..32], &[0; 6], "no cluster, no size");
        assert!(root[32..].iter().all(|b| *b == 0));

        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.label, "BOOTFORGE");
        assert_eq!(report.cluster_size, 512);
        assert_eq!(report.total_clusters, 129022);
        assert_eq!(report.free_clusters, 129021);
        assert!(report.files.is_empty());
    }

    #[test]
    fn format_other_sizes() {
        // 300 MiB: 4 KiB clusters. 614400 sectors; F = ceil((4 * 614368 + 64)
        // / (4096 + 8)) = ceil(598.8) = 599; (614368 - 1198) / 8 = 76646 clusters.
        let mut d = SparseDevice::new(300 * MIB, 512);
        let geo = fmt(&mut d, "", None);
        assert_eq!(
            (geo.cluster_size, geo.fat_sectors, geo.cluster_count),
            (4096, 599, 76646)
        );
        assert_eq!(d.bytes(13, 1), [8]);
        assert_eq!(d.bytes(71, 11), b"NO NAME    ");
        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.label, "");
        // No label entry in the root.
        assert!(d.bytes(geo.cluster_offset(2), 4096).iter().all(|b| *b == 0));

        // 2 GiB: 4194304 sectors; (4 * 4194272 + 64) / 4104 = 4088 exactly;
        // (4194272 - 8176) / 8 = 523262 clusters, and 4088 * 128 = 523264
        // entries hold exactly clusters + 2.
        let mut d = SparseDevice::new(2 * GIB, 512);
        let geo = fmt(&mut d, "TWO GIG", None);
        assert_eq!(
            (geo.cluster_size, geo.fat_sectors, geo.cluster_count),
            (4096, 4088, 523262)
        );
        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.free_clusters, 523261);

        // Odd size (not a multiple of the cluster) with a chosen cluster size.
        let mut d = SparseDevice::new(700 * MIB + 512 * 13, 512);
        let geo = fmt(&mut d, "ODD", Some(8192));
        assert_eq!(geo.total_sectors as u64, d.size() / 512);
        assert_eq!(check(&mut d).unwrap().problems, NONE);

        // 32 GiB with the default 16 KiB clusters and 64 KiB clusters.
        for cluster in [None, Some(65536)] {
            let mut d = SparseDevice::new(32 * GIB, 512);
            let geo = fmt(&mut d, "BIG", cluster);
            assert_eq!(geo.cluster_size, cluster.unwrap_or(16384));
            assert_eq!(d.bytes(13, 1), [(geo.cluster_size / 512) as u8]);
            assert_eq!(check(&mut d).unwrap().problems, NONE);
        }
    }

    #[test]
    fn format_4096_byte_sectors() {
        // 512 MiB of 4 KiB sectors: 131072 sectors, one sector per cluster.
        // F = ceil((4 * 131040 + 8) / (4096 + 8)) = ceil(127.7) = 128;
        // clusters = 131040 - 256 = 130784.
        let mut d = SparseDevice::new(512 * MIB, 4096);
        let geo = fmt(&mut d, "FOURK", None);
        assert_eq!(
            (geo.cluster_size, geo.fat_sectors, geo.cluster_count),
            (4096, 128, 130784)
        );
        assert_eq!(geo.data_start_sector, 32 + 256);
        let b = d.bytes(0, 4096);
        assert_eq!(&b[11..13], &[0x00, 0x10]);
        assert_eq!(b[13], 1);
        assert_eq!(&b[14..16], &[32, 0]);
        assert_eq!(&b[32..36], &131072u32.to_le_bytes());
        assert_eq!(&b[36..40], &128u32.to_le_bytes());
        assert_eq!(&b[510..512], &[0x55, 0xAA], "signature stays at offset 510");
        assert!(b[512..].iter().all(|x| *x == 0));
        // FSInfo in sector 1, backups in sectors 6 and 7 (4 KiB each).
        assert_eq!(d.bytes(4096, 4), b"RRaA");
        assert_eq!(d.bytes(4096 + 484, 4), b"rrAa");
        assert_eq!(d.bytes(4096 + 508, 4), [0x00, 0x00, 0x55, 0xAA]);
        assert_eq!(d.bytes(6 * 4096, 4096), b);
        assert_eq!(d.bytes(7 * 4096, 4), b"RRaA");
        assert_eq!(
            d.bytes(32 * 4096, 12),
            [0xF8, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F]
        );
        assert_eq!(d.bytes((32 + 128) * 4096, 4), [0xF8, 0xFF, 0xFF, 0x0F]);
        assert_eq!(d.bytes(288 * 4096, 11), b"FOURK      ");

        // Files on it.
        let big = bytes(1, 3 * 4096 + 5);
        let mut w = Fat32Writer::open(&mut d).unwrap();
        put(&mut w, "EFI/BOOT/BOOTX64.EFI", &big).unwrap();
        put(&mut w, "A long name here.txt", b"hello").unwrap();
        w.finish().unwrap();
        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.free_clusters, 130784 - 1 - 2 - 4 - 1);
        assert_eq!(read_file(&mut d, "efi/boot/bootx64.efi").unwrap(), big);
        assert_eq!(read_file(&mut d, "A long name here.txt").unwrap(), b"hello");

        // A 512-byte cluster is refused on this device, and 300 MiB is too
        // small for FAT32 with 4 KiB clusters... but 8 KiB is never offered either.
        let mut d = SparseDevice::new(512 * MIB, 4096);
        let e = format(&mut d, &opts("", Some(512)), &Cancel::new(), &mut |_| {});
        assert!(matches!(e, Err(Error::Invalid(_))));
        let mut d = SparseDevice::new(200 * MIB, 4096);
        let e = format(&mut d, &opts("", None), &Cancel::new(), &mut |_| {});
        assert!(matches!(e, Err(Error::Invalid(_))));
        assert_eq!(d.writes, 0);
    }

    #[test]
    fn format_refusals_progress_and_cancel() {
        let mut d = MemDevice::new(64 * MIB, 512);
        for (cluster, needle) in [
            (3000u32, "power of two"),
            (256, "power of two"),
            (131072, "power of two"),
            (0, "power of two"),
            (1024, "at least 65525"),
            (32768, "at least 65525"),
        ] {
            match format(
                &mut d,
                &opts("", Some(cluster)),
                &Cancel::new(),
                &mut |_| {},
            ) {
                Err(Error::Invalid(text)) => assert!(text.contains(needle), "{text}"),
                other => panic!("cluster {cluster}: {:?}", other.map(|_| ())),
            }
        }
        let mut huge = SparseDevice::new(200 * GIB, 512);
        match format(&mut huge, &opts("", Some(512)), &Cancel::new(), &mut |_| {}) {
            Err(Error::Invalid(text)) => assert!(text.contains("at most"), "{text}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
        let mut tiny = MemDevice::new(16 * MIB, 512);
        assert!(matches!(
            format(&mut tiny, &opts("", None), &Cancel::new(), &mut |_| {}),
            Err(Error::Invalid(_))
        ));
        let mut over = SparseDevice::new(3000 * GIB, 512);
        assert!(matches!(
            format(
                &mut over,
                &opts("", Some(65536)),
                &Cancel::new(),
                &mut |_| {}
            ),
            Err(Error::Invalid(_))
        ));
        assert_eq!(d.writes + tiny.writes + huge.writes + over.writes, 0);

        // Quick format: old data beyond the root cluster stays; progress ends at the metadata size.
        d.data.fill(0xA5);
        let mut seen = Vec::new();
        let geo = format(&mut d, &opts("Q", None), &Cancel::new(), &mut |n| {
            seen.push(n)
        })
        .unwrap();
        assert!(seen.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(*seen.last().unwrap(), geo.metadata_bytes());
        let meta = geo.metadata_bytes() as usize;
        assert!(d.data[meta..].iter().all(|b| *b == 0xA5));
        assert_eq!(check(&mut d).unwrap().problems, NONE);

        // Full format: everything else is zero; progress ends at the volume size.
        let mut full = opts("F", None);
        full.zero_data = true;
        let mut last = 0;
        format(&mut d, &full, &Cancel::new(), &mut |n| last = n).unwrap();
        assert_eq!(last, 64 * MIB);
        assert!(d.data[meta..].iter().all(|b| *b == 0));
        assert_eq!(check(&mut d).unwrap().problems, NONE);

        // Cancel during the zeroing: no boot sector is left behind.
        d.data.fill(0xA5);
        let cancel = Cancel::new();
        let c2 = cancel.clone();
        let e = format(&mut d, &full, &cancel, &mut |n| {
            if n >= 3 * MIB {
                c2.cancel();
            }
        });
        assert!(matches!(e, Err(Error::Cancelled)));
        assert!(d.data[..512].iter().all(|b| *b == 0));
        assert!(d.data[4 * MIB as usize..].iter().all(|b| *b == 0xA5));
        assert!(check(&mut d).is_err());
    }

    #[test]
    fn short_and_long_name_rules() {
        // Plain 8.3 names, with the NT case bits.
        assert_eq!(exact_short("BOOTX64.EFI"), Some((*b"BOOTX64 EFI", 0)));
        assert_eq!(exact_short("boot.wim"), Some((*b"BOOT    WIM", 0x18)));
        assert_eq!(exact_short("README.txt"), Some((*b"README  TXT", 0x10)));
        assert_eq!(exact_short("setup.EXE"), Some((*b"SETUP   EXE", 0x08)));
        assert_eq!(exact_short("EFI"), Some((*b"EFI        ", 0)));
        assert_eq!(exact_short("sources"), Some((*b"SOURCES    ", 0x08)));
        assert_eq!(exact_short("a1_$~!#.x-y"), Some((*b"A1_$~!# X-Y", 0x18)));
        assert_eq!(exact_short("12345678.123"), Some((*b"12345678123", 0)));
        // Not plain 8.3: mixed case, too long, spaces, two dots, other characters.
        for name in [
            "Setup.exe",
            "readme.Txt",
            "123456789",
            "a.html",
            "my file",
            "a.b.c",
            ".hidden",
            "a+b",
            "caf\u{00e9}",
            "a,b",
            "x[1]",
        ] {
            assert_eq!(exact_short(name), None, "{name}");
        }

        // Alias bases.
        let basis = |n: &str| {
            let (b, e, x) = alias_basis(n);
            (
                String::from_utf8(b).unwrap(),
                String::from_utf8(e).unwrap(),
                x,
            )
        };
        assert_eq!(basis("Mixed.Txt"), ("MIXED".into(), "TXT".into(), true));
        assert_eq!(
            basis("Read Me First.txt"),
            ("READMEFIRST".into(), "TXT".into(), false)
        );
        assert_eq!(basis("a.b.c.d"), ("ABC".into(), "D".into(), false));
        assert_eq!(basis(".hidden"), ("HIDDEN".into(), "".into(), false));
        assert_eq!(basis("index.html"), ("INDEX".into(), "HTM".into(), false));
        assert_eq!(basis("a+b=c.txt"), ("A_B_C".into(), "TXT".into(), false));
        assert_eq!(
            basis("\u{65e5}\u{672c}.dat"),
            ("__".into(), "DAT".into(), false)
        );
        assert_eq!(basis(" .x"), ("_".into(), "X".into(), false));

        // Alias numbering: ~1..~4, then the hashed form, then ~10 and up.
        let units: Vec<u16> = "Read Me First.txt".encode_utf16().collect();
        let mut taken: HashSet<String> = HashSet::new();
        let next = |taken: &mut HashSet<String>| {
            let raw = make_alias("Read Me First.txt", &units, taken).unwrap();
            let shown = short_display(&raw);
            assert!(taken.insert(shown.clone()), "{shown} handed out twice");
            shown
        };
        for n in 1..=4 {
            assert_eq!(next(&mut taken), format!("README~{n}.TXT"));
        }
        let hash = format!("{:04X}", name_hash(&units));
        for n in 1..=9 {
            assert_eq!(next(&mut taken), format!("RE{hash}~{n}.TXT"));
        }
        assert_eq!(next(&mut taken), "READM~10.TXT");
        assert_eq!(next(&mut taken), "READM~11.TXT");
        for _ in 12..100 {
            next(&mut taken);
        }
        assert_eq!(next(&mut taken), "READ~100.TXT");
        // A name that only differs in case from its alias takes it without a tail,
        // unless it is taken.
        let u: Vec<u16> = "Mixed.Txt".encode_utf16().collect();
        let mut taken: HashSet<String> = HashSet::new();
        assert_eq!(
            short_display(&make_alias("Mixed.Txt", &u, &taken).unwrap()),
            "MIXED.TXT"
        );
        taken.insert("MIXED.TXT".into());
        assert_eq!(
            short_display(&make_alias("Mixed.Txt", &u, &taken).unwrap()),
            "MIXED~1.TXT"
        );

        // The long-name checksum, against the specification's formula.
        let spec_sum = |raw: &[u8; 11]| {
            let mut sum: u8 = 0;
            for b in raw {
                sum = (if sum & 1 != 0 { 0x80u8 } else { 0 })
                    .wrapping_add(sum >> 1)
                    .wrapping_add(*b);
            }
            sum
        };
        for raw in [
            b"README~1TXT",
            b"BOOTX64 EFI",
            b"A          ",
            b"\xE5\xFF\x80~1 \xFE    ",
        ] {
            assert_eq!(lfn_checksum(raw), spec_sum(raw));
        }
        // "A" + ten spaces by hand: 0x41, then ten times rotate-right and add 0x20.
        assert_eq!(lfn_checksum(b"A          "), {
            let mut s = 0x41u8;
            for _ in 0..10 {
                s = s.rotate_right(1).wrapping_add(0x20);
            }
            s
        });

        // Invalid long names.
        for name in [
            "", ".", "..", "a:b", "a*b", "what?", "a\\b", "a|b", "a<b", "a>b", "a\"b", "tab\t",
            "end.", "end ", "nul\u{0}",
        ] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
        assert!(validate_name(&"x".repeat(255)).is_ok());
        assert!(validate_name(&"x".repeat(256)).is_err());
        // 128 characters outside the BMP are 256 UTF-16 units.
        assert!(validate_name(&"\u{1F600}".repeat(127)).is_ok());
        assert!(validate_name(&"\u{1F600}".repeat(128)).is_err());
        assert!(validate_name(" leading space and dots...x").is_ok());
    }

    #[test]
    fn long_name_entries_byte_level() {
        let mut d = small();
        let mut w = Fat32Writer::open(&mut d).unwrap();
        let stamp = DosTime::new(2025, 6, 7, 8, 9, 10);
        w.write_file(
            "Read Me First.txt",
            3,
            &mut &b"abc"[..],
            Some(stamp),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        w.write_file(
            "boot.wim",
            0,
            &mut io::empty(),
            Some(stamp),
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        let geo = w.geometry().clone();
        w.finish().unwrap();
        let root = geo.cluster_offset(2) as usize;
        let e = &d.data[root..root + 512];
        // Entry 0: the label. Entries 1 and 2: the long name, last piece first.
        assert_eq!(&e[0..11], b"TEST       ");
        let sum = {
            let mut s: u8 = 0;
            for b in b"README~1TXT" {
                s = (if s & 1 != 0 { 0x80u8 } else { 0 })
                    .wrapping_add(s >> 1)
                    .wrapping_add(*b);
            }
            s
        };
        let l2 = &e[32..64];
        assert_eq!(l2[0], 0x42, "second and last piece");
        assert_eq!(l2[11], 0x0F);
        assert_eq!(l2[12], 0);
        assert_eq!(l2[13], sum);
        assert_eq!(&l2[26..28], &[0, 0]);
        // ".txt", a terminating zero, then 0xFFFF padding.
        assert_eq!(&l2[1..11], &[b'.', 0, b't', 0, b'x', 0, b't', 0, 0, 0]);
        assert!(l2[14..26].iter().all(|b| *b == 0xFF));
        assert!(l2[28..32].iter().all(|b| *b == 0xFF));
        let l1 = &e[64..96];
        assert_eq!(l1[0], 0x01);
        assert_eq!(l1[11], 0x0F);
        assert_eq!(l1[13], sum);
        let units: Vec<u8> = "Read Me First"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(&l1[1..11], &units[0..10]);
        assert_eq!(&l1[14..26], &units[10..22]);
        assert_eq!(&l1[28..32], &units[22..26]);
        // The short entry.
        let s = &e[96..128];
        assert_eq!(&s[0..11], b"README~1TXT");
        assert_eq!(s[11], 0x20);
        assert_eq!(s[12], 0);
        assert_eq!(&s[22..24], &stamp.time.to_le_bytes());
        assert_eq!(&s[24..26], &stamp.date.to_le_bytes());
        assert_eq!(&s[20..22], &[0, 0]);
        assert_eq!(&s[26..28], &[3, 0], "first cluster 3");
        assert_eq!(&s[28..32], &3u32.to_le_bytes());
        // A lower-case 8.3 name: one entry, NT case bits, no clusters for an empty file.
        let s = &e[128..160];
        assert_eq!(&s[0..11], b"BOOT    WIM");
        assert_eq!(s[12], 0x18);
        assert_eq!(&s[26..32], &[0; 6]);
        assert_eq!(e[160], 0, "end of directory");
        // FAT: cluster 3 is a one-cluster chain; FSInfo updated.
        let fat = geo.fat_offset(0) as usize;
        assert_eq!(&d.data[fat + 12..fat + 16], &[0xFF, 0xFF, 0xFF, 0x0F]);
        assert_eq!(&d.data[fat + 16..fat + 20], &[0; 4]);
        assert_eq!(le32(&d.data, 512 + 488), geo.cluster_count - 2);
        assert_eq!(le32(&d.data, 512 + 492), 4);
        assert_eq!(&d.data[7 * 512..8 * 512], &d.data[512..1024]);
        assert_eq!(
            &d.data[root + 3 * 512 - 1024..root + 3 * 512 - 1021],
            b"abc"
        );

        let reader = Fat32Reader::open(&mut d).unwrap();
        let f = reader.stat(&mut d, "read me first.TXT").unwrap();
        assert_eq!(
            (f.path.as_str(), f.short_name.as_str()),
            ("Read Me First.txt", "README~1.TXT")
        );
        assert_eq!(
            (f.size, f.first_cluster, f.is_dir, f.mtime),
            (3, 3, false, stamp)
        );
        // Found through its alias too.
        assert_eq!(
            reader.stat(&mut d, "README~1.TXT").unwrap().path,
            "Read Me First.txt"
        );
        let f = reader.stat(&mut d, "BOOT.WIM").unwrap();
        assert_eq!(
            (f.path.as_str(), f.short_name.as_str(), f.first_cluster),
            ("boot.wim", "BOOT.WIM", 0)
        );
        assert!(reader.stat(&mut d, "nothing").is_err());
        assert!(reader.stat(&mut d, "boot.wim/x").is_err());
        assert!(reader.stat(&mut d, "").is_err());
    }

    #[test]
    fn mixed_tree_round_trip() {
        let mut d = small();
        let unicode = "\u{00dc}n\u{00ef}c\u{00f6}d\u{00e9} \u{2014} \u{0444}\u{0430}\u{0439}\u{043b} \u{65e5}\u{672c}\u{8a9e} \u{1F600}.dat";
        let longest = format!("{}.bin", "L".repeat(251));
        let files: Vec<(String, Vec<u8>)> = vec![
            ("EFI/BOOT/BOOTX64.EFI".into(), bytes(1, 300_000)),
            ("EFI/BOOT/bootia32.efi".into(), bytes(2, 70_001)),
            ("sources/boot.wim".into(), bytes(3, MIB as usize + 17)),
            ("empty.txt".into(), Vec::new()),
            ("one.bin".into(), vec![0x5A]),
            ("exact.bin".into(), bytes(4, 512)),
            ("two.bin".into(), bytes(5, 1024)),
            ("Read Me First.txt".into(), bytes(6, 513)),
            (unicode.into(), bytes(7, 4000)),
            ("a.b.c.d".into(), bytes(8, 10)),
            (longest.clone(), bytes(9, 2000)),
            ("Mixed.Txt".into(), bytes(10, 100)),
            (".hidden".into(), bytes(11, 77)),
            ("a/b/c/d/e/f/g/h/file.txt".into(), bytes(12, 1500)),
            ("a/b/Other File.TXT".into(), bytes(13, 3)),
            ("sources/$OEM$/Setup Complete.cmd".into(), bytes(14, 900)),
            ("/leading/slash.txt".into(), bytes(15, 42)),
        ];
        let stamp = DosTime::new(2024, 12, 24, 18, 30, 0);
        let mut w = Fat32Writer::open(&mut d).unwrap();
        let free_before = w.free_bytes();
        assert_eq!(free_before, 68527 * 512);
        w.create_dir_all("empty dir/inner").unwrap();
        w.create_dir_all("EFI").unwrap();
        w.create_dir_all("").unwrap();
        w.create_dir_all("/").unwrap();
        for (path, data) in &files {
            let mut last = None;
            w.write_file(
                path,
                data.len() as u64,
                &mut &data[..],
                Some(stamp),
                &Cancel::new(),
                &mut |n| {
                    assert!(last.is_none_or(|l| l <= n));
                    last = Some(n);
                },
            )
            .unwrap();
            assert_eq!(last, Some(data.len() as u64), "{path}");
        }
        // Directory lookups are case-insensitive; existing names are refused.
        put(&mut w, "efi/boot/extra.efi", b"x").unwrap();
        for dup in [
            "SOURCES/BOOT.WIM",
            "Empty.TXT",
            "read me first.TXT",
            "efi",
            "README~1.TXT",
            "EFI/BOOT/BOOTX64.EFI",
        ] {
            assert!(
                matches!(put(&mut w, dup, b"dup"), Err(Error::Invalid(_))),
                "{dup}"
            );
        }
        // A file where a directory is needed, and the reverse.
        assert!(put(&mut w, "one.bin/child", b"x").is_err());
        assert!(w.create_dir_all("one.bin").is_err());
        assert!(w.create_dir_all("a/b/c").is_ok());
        // Invalid names change nothing.
        for bad in ["a//b", "bad:name", "dir/", "x/../y", "trailing./f", "q?"] {
            assert!(
                matches!(put(&mut w, bad, b"x"), Err(Error::Invalid(_))),
                "{bad}"
            );
        }
        assert!(w.free_bytes() < free_before);
        w.finish().unwrap();

        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.label, "TEST");
        let reader = Fat32Reader::open(&mut d).unwrap();
        let mut expected_clusters = 0u64;
        for (path, data) in &files {
            let path = path.trim_start_matches('/');
            let found = report
                .files
                .iter()
                .find(|f| f.path == path)
                .unwrap_or_else(|| panic!("{path} missing"));
            assert_eq!(found.size, data.len() as u64, "{path}");
            assert!(!found.is_dir);
            assert_eq!(found.mtime, stamp);
            assert_eq!(found.first_cluster == 0, data.is_empty());
            assert_eq!(&reader.read_file(&mut d, path).unwrap(), data, "{path}");
            // Streaming in odd-sized reads gives the same bytes.
            let mut stream = reader.open_found(&mut d, found).unwrap();
            assert_eq!(stream.size(), data.len() as u64);
            let mut got = Vec::new();
            let mut piece = [0u8; 777];
            loop {
                let n = stream.read(&mut piece).unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&piece[..n]);
            }
            assert_eq!(&got, data, "{path}");
            expected_clusters += (data.len() as u64).div_ceil(512);
        }
        assert_eq!(read_file(&mut d, "EFI/BOOT/EXTRA.EFI").unwrap(), b"x");
        let mut s = open_file(&mut d, "one.bin").unwrap();
        let mut one = Vec::new();
        s.read_to_end(&mut one).unwrap();
        assert_eq!(one, [0x5A]);

        // Short names as stored.
        let short = |path: &str| {
            report
                .files
                .iter()
                .find(|f| f.path == path)
                .unwrap()
                .short_name
                .clone()
        };
        assert_eq!(short("EFI"), "EFI");
        assert_eq!(short("sources"), "SOURCES");
        assert_eq!(short("sources/boot.wim"), "BOOT.WIM");
        assert_eq!(short("EFI/BOOT/bootia32.efi"), "BOOTIA32.EFI");
        assert_eq!(short("Read Me First.txt"), "README~1.TXT");
        assert_eq!(short(unicode), "_N_C_D~1.DAT");
        assert_eq!(short("a.b.c.d"), "ABC~1.D");
        assert_eq!(short(&longest), "LLLLLL~1.BIN");
        assert_eq!(short("Mixed.Txt"), "MIXED.TXT");
        assert_eq!(short(".hidden"), "HIDDEN~1");
        assert_eq!(short("empty dir"), "EMPTYD~1");
        assert_eq!(short("sources/$OEM$"), "$OEM$");
        assert_eq!(short("sources/$OEM$/Setup Complete.cmd"), "SETUPC~1.CMD");
        assert_eq!(short("a/b/Other File.TXT"), "OTHERF~1.TXT");

        // Directories: listed, with correct dot entries (checked by `check`), and counted.
        let dirs: Vec<&FoundFile> = report.files.iter().filter(|f| f.is_dir).collect();
        for name in [
            "EFI",
            "EFI/BOOT",
            "sources",
            "sources/$OEM$",
            "a/b/c/d/e/f/g/h",
            "empty dir/inner",
            "leading",
        ] {
            assert!(dirs.iter().any(|f| f.path == name), "{name}");
        }
        assert_eq!(dirs.len(), 15);
        assert_eq!(report.files.len(), 15 + files.len() + 1);
        // ".." of a first-level directory is cluster 0; deeper it is the parent's cluster.
        let efi = reader.stat(&mut d, "EFI").unwrap().first_cluster;
        let boot = reader.stat(&mut d, "EFI/BOOT").unwrap().first_cluster;
        let geo = reader.geometry().clone();
        let at = geo.cluster_offset(efi) as usize;
        assert_eq!(&d.data[at..at + 11], b".          ");
        assert_eq!(le16(&d.data, at + 26) as u32, efi);
        assert_eq!(&d.data[at + 32..at + 43], b"..         ");
        assert_eq!(
            (le16(&d.data, at + 32 + 20), le16(&d.data, at + 32 + 26)),
            (0, 0)
        );
        let at = geo.cluster_offset(boot) as usize;
        assert_eq!(le16(&d.data, at + 26) as u32, boot);
        assert_eq!(le16(&d.data, at + 32 + 26) as u32, efi);

        // Space accounting: every directory here fits one cluster, except the
        // root, which needs more than 512 bytes of entries.
        let root_clusters = {
            let chain = reader.chain(2, u64::MAX, None);
            assert!(chain.problem.is_none());
            chain.clusters
        };
        assert!(root_clusters >= 2, "the root directory grew");
        let used = 68528 - report.free_clusters as u64;
        assert_eq!(used, expected_clusters + 1 + 15 + root_clusters);
        // Files are contiguous and in writing order.
        let first = |p: &str| {
            report
                .files
                .iter()
                .find(|f| f.path == p)
                .unwrap()
                .first_cluster
        };
        assert!(first("EFI/BOOT/BOOTX64.EFI") < first("EFI/BOOT/bootia32.efi"));
        assert!(first("EFI/BOOT/bootia32.efi") < first("sources/boot.wim"));
        let chain = reader.chain(first("sources/boot.wim"), u64::MAX, None);
        assert_eq!(chain.extents.len(), 1);
        assert_eq!(chain.clusters, 2049);
    }

    #[test]
    fn many_files_grow_the_directory_and_collide() {
        let mut d = small();
        let mut w = Fat32Writer::open(&mut d).unwrap();
        // Each name needs three long-name entries and a short one: 128 bytes.
        // 300 of them (plus dot entries) need 76 clusters of 512 bytes.
        for i in 0..300 {
            let name = format!("many/long file name number {i}.txt");
            put(&mut w, &name, format!("content {i}").as_bytes()).unwrap();
        }
        // Colliding plain 8.3 names in the root, which grows too.
        for i in 0..40 {
            put(&mut w, &format!("FILE{i:04}.DAT"), &[i as u8]).unwrap();
            put(&mut w, &format!("File{i:04}.Dat extra"), &[i as u8, 1]).unwrap();
        }
        w.finish().unwrap();
        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.files.len(), 1 + 300 + 80);
        let reader = Fat32Reader::open(&mut d).unwrap();
        let many = reader.stat(&mut d, "many").unwrap();
        let chain = reader.chain(many.first_cluster, u64::MAX, None);
        assert_eq!(chain.clusters, 76);
        assert!(chain.extents.len() > 50, "interleaved with the files");
        let mut shorts = HashSet::new();
        for i in 0..300 {
            let path = format!("many/long file name number {i}.txt");
            let f = report.files.iter().find(|f| f.path == path).unwrap();
            assert!(
                shorts.insert(f.short_name.clone()),
                "{} twice",
                f.short_name
            );
            assert!(f.short_name.len() <= 12 && f.short_name.ends_with(".TXT"));
            assert_eq!(
                reader.read_file(&mut d, &path).unwrap(),
                format!("content {i}").as_bytes()
            );
            // Each file is reachable through its alias as well.
            assert_eq!(
                reader
                    .stat(&mut d, &format!("many/{}", f.short_name))
                    .unwrap()
                    .path,
                path
            );
        }
        assert!(shorts.contains("LONGFI~1.TXT") && shorts.contains("LONGFI~4.TXT"));
        assert!(!shorts.contains("LONGFI~5.TXT"));
        // The other 296 use the hashed form (a tail above 1 only when two hashes agree).
        let hashed =
            |s: &&String| s.len() == 12 && s.as_bytes()[6] == b'~' && !s.starts_with("LONGFI");
        assert_eq!(shorts.iter().filter(hashed).count(), 296);
        for i in 0..40 {
            assert_eq!(
                read_file(&mut d, &format!("file{i:04}.dat")).unwrap(),
                [i as u8]
            );
            assert_eq!(
                read_file(&mut d, &format!("File{i:04}.Dat extra")).unwrap(),
                [i as u8, 1]
            );
        }
    }

    #[test]
    fn directory_entry_limit() {
        // 65536 entries at most: the label, then 65535 plain 8.3 files; the
        // next one is refused and everything still checks.
        let mut d = SparseDevice::new(SMALL, 512);
        fmt(&mut d, "L", None);
        let mut w = Fat32Writer::open(&mut d).unwrap();
        for i in 0..65535u32 {
            put(&mut w, &format!("F{i:07}"), &[]).unwrap();
        }
        assert!(matches!(put(&mut w, "ONEMORE", &[]), Err(Error::Msg(_))));
        assert!(w.create_dir_all("DIR").is_err());
        w.finish().unwrap();
        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.files.len(), 65535);
        assert_eq!(report.free_clusters, 68528 - 4096);
    }

    #[test]
    fn volume_full_is_a_clean_error() {
        let mut d = small();
        let mut w = Fat32Writer::open(&mut d).unwrap();
        let block = bytes(21, 8 * MIB as usize);
        let mut written = 0;
        loop {
            let free = w.free_bytes();
            match put(&mut w, &format!("big{written}.bin"), &block) {
                Ok(()) => written += 1,
                Err(Error::Msg(text)) => {
                    assert!(text.contains("Not enough space"), "{text}");
                    assert_eq!(w.free_bytes(), free, "a refused file takes nothing");
                    break;
                }
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(written, 4);
        // A deep path that does not fit creates nothing either.
        let dirs_before = w.dirs.len();
        assert!(put(&mut w, "x/y/z/too big.bin", &block).is_err());
        assert_eq!(w.dirs.len(), dirs_before);
        // Fill the rest exactly: the last free cluster goes to the file.
        let rest = w.free_bytes();
        assert!(rest > 0 && rest < 8 * MIB);
        let tail = bytes(22, rest as usize);
        put(&mut w, "TAIL.BIN", &tail).unwrap();
        assert_eq!(w.free_bytes(), 0);
        assert!(matches!(put(&mut w, "ONE.BIN", b"x"), Err(Error::Msg(_))));
        assert!(matches!(w.create_dir_all("NEWDIR"), Err(Error::Msg(_))));
        // An empty file needs no cluster and its entry still fits the root cluster.
        put(&mut w, "EMPTY", b"").unwrap();
        w.finish().unwrap();

        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert_eq!(report.free_clusters, 0);
        assert_eq!(report.files.len(), 6);
        assert_eq!(le32(&d.data, 512 + 488), 0, "FSInfo free count");
        assert_eq!(le32(&d.data, 512 + 492), 0xFFFF_FFFF, "FSInfo next free");
        for i in 0..4 {
            assert_eq!(read_file(&mut d, &format!("big{i}.bin")).unwrap(), block);
        }
        assert_eq!(read_file(&mut d, "tail.bin").unwrap(), tail);
        assert!(read_file(&mut d, "ONE.BIN").is_err());
        // The writer refuses a volume that is no longer fresh.
        assert!(matches!(Fat32Writer::open(&mut d), Err(Error::Invalid(_))));
    }

    #[test]
    fn four_gib_limit() {
        let mut d = small();
        let writes = d.writes;
        let mut w = Fat32Writer::open(&mut d).unwrap();
        let e = w.write_file(
            "dir/install.wim",
            0x1_0000_0000,
            &mut io::empty(),
            None,
            &Cancel::new(),
            &mut |_| {},
        );
        match e {
            Err(Error::Invalid(text)) => assert!(
                text.contains("4 GiB") && text.contains("install.wim"),
                "{text}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(w
            .write_file(
                "huge",
                u64::MAX,
                &mut io::empty(),
                None,
                &Cancel::new(),
                &mut |_| {}
            )
            .is_err());
        // One byte less is a valid FAT32 size; here it merely does not fit.
        let e = w.write_file(
            "max",
            0xFFFF_FFFF,
            &mut io::empty(),
            None,
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(matches!(e, Err(Error::Msg(_))));
        assert_eq!(
            w.dirs.len(),
            1,
            "no directory was created for a refused file"
        );
        w.finish().unwrap();
        assert!(d.writes > writes);
        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        assert!(report.files.is_empty());
    }

    /// A reader that cancels (or fails, or lies about its length) part-way.
    struct Tricky<'a> {
        data: &'a [u8],
        pos: usize,
        cancel_at: Option<(usize, Cancel)>,
        fail_at: Option<usize>,
    }

    impl Read for Tricky<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if let Some((at, cancel)) = &self.cancel_at {
                if self.pos >= *at {
                    cancel.cancel();
                }
            }
            if self.fail_at.is_some_and(|at| self.pos >= at) {
                return Err(io::Error::other("source failed"));
            }
            // Short, irregular reads.
            let n = out.len().min(self.data.len() - self.pos).min(100_003);
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn cancel_and_source_errors_mid_file() {
        let mut d = small();
        let data = bytes(31, 5 * MIB as usize);
        let before = bytes(32, 3000);
        let after = bytes(33, 70_000);
        let mut w = Fat32Writer::open(&mut d).unwrap();
        put(&mut w, "before.bin", &before).unwrap();
        let free = w.free_bytes();

        // Cancelled after about 2 MiB.
        let cancel = Cancel::new();
        let mut source = Tricky {
            data: &data,
            pos: 0,
            cancel_at: Some((2 * MIB as usize, cancel.clone())),
            fail_at: None,
        };
        let mut progressed = 0;
        let e = w.write_file(
            "sub/cancelled.bin",
            data.len() as u64,
            &mut source,
            None,
            &cancel,
            &mut |n| progressed = n,
        );
        assert!(matches!(e, Err(Error::Cancelled)));
        assert!((2 * MIB..5 * MIB).contains(&progressed), "{progressed}");
        // The source fails.
        let mut source = Tricky {
            data: &data,
            pos: 0,
            cancel_at: None,
            fail_at: Some(MIB as usize),
        };
        let e = w.write_file(
            "sub/failed.bin",
            data.len() as u64,
            &mut source,
            None,
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(matches!(e, Err(Error::Io(_, _))));
        // The source is shorter or longer than announced.
        let e = w.write_file(
            "sub/short.bin",
            data.len() as u64 + 1,
            &mut &data[..],
            None,
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(
            matches!(&e, Err(Error::Invalid(t)) if t.contains("ended after")),
            "{e:?}"
        );
        let e = w.write_file(
            "sub/long.bin",
            data.len() as u64 - 1,
            &mut &data[..],
            None,
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(
            matches!(&e, Err(Error::Invalid(t)) if t.contains("more than the expected")),
            "{e:?}"
        );
        let e = w.write_file(
            "sub/long0.bin",
            0,
            &mut &data[..],
            None,
            &Cancel::new(),
            &mut |_| {},
        );
        assert!(e.is_err());
        // Only the directory "sub" was created (one cluster).
        assert_eq!(w.free_bytes(), free - 512);

        // The writer goes on working, over the clusters the failed files touched.
        let mut source = Tricky {
            data: &after,
            pos: 0,
            cancel_at: None,
            fail_at: None,
        };
        w.write_file(
            "sub/after.bin",
            after.len() as u64,
            &mut source,
            None,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap();
        w.finish().unwrap();
        let report = check(&mut d).unwrap();
        assert_eq!(report.problems, NONE);
        let mut paths: Vec<&str> = report.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["before.bin", "sub", "sub/after.bin"]);
        assert_eq!(read_file(&mut d, "before.bin").unwrap(), before);
        assert_eq!(read_file(&mut d, "sub/after.bin").unwrap(), after);
    }

    #[test]
    fn writer_refuses_unformatted_or_used_volumes() {
        assert!(Fat32Writer::open(&mut MemDevice::new(SMALL, 512)).is_err());
        assert!(Fat32Writer::open(&mut MemDevice::new(0, 512)).is_err());
        let mut d = small();
        {
            let mut w = Fat32Writer::open(&mut d).unwrap();
            put(&mut w, "A.TXT", b"a").unwrap();
            w.finish().unwrap();
        }
        assert!(matches!(Fat32Writer::open(&mut d), Err(Error::Invalid(_))));
        // Leftovers after the label in the root cluster are refused too.
        let mut d = small();
        let root = 34 * MIB as usize - 68528 * 512;
        d.data[root + 300] = 1;
        assert!(Fat32Writer::open(&mut d).is_err());
        // Without a label the root is empty and the writer starts at offset 0.
        let mut d = MemDevice::new(SMALL, 512);
        fmt(&mut d, "", None);
        let mut w = Fat32Writer::open(&mut d).unwrap();
        put(&mut w, "A.TXT", b"a").unwrap();
        w.finish().unwrap();
        assert_eq!(&d.data[root..root + 11], b"A       TXT");
        let report = check(&mut d).unwrap();
        assert_eq!((report.problems, report.label.as_str()), (NONE, ""));
    }

    #[test]
    fn inside_a_gpt_partition_and_on_an_image_file() {
        use crate::model::Scheme;
        use crate::part;
        let path = std::env::temp_dir().join(format!("bootforge-fat32-{}.img", std::process::id()));
        let payload = bytes(41, 200_000);
        let result = (|| -> Result<(part::Table, FsReport, Vec<u8>)> {
            let mut disk = FileDevice::create(&path, 40 * MIB, 512)?;
            let (offset, size) = part::single_partition(disk.size(), 512, Scheme::Gpt)?;
            part::wipe_tables(&mut disk)?;
            let spec = part::PartSpec::new(part::PartType::BasicData, offset, size, "BOOTFORGE");
            part::write_gpt(&mut disk, &[spec], part::random_guid())?;
            {
                let mut vol = SubDevice::new(&mut disk, offset, size)?;
                let mut o = opts("Stick", None);
                o.hidden_sectors = offset / 512;
                format(&mut vol, &o, &Cancel::new(), &mut |_| {})?;
                let mut w = Fat32Writer::open(&mut vol)?;
                put(&mut w, "EFI/BOOT/BOOTX64.EFI", &payload)?;
                w.finish()?;
            }
            drop(disk);
            let mut disk = FileDevice::open(&path, 512)?;
            let table = part::read_table(&mut disk)?;
            let mut vol = SubDevice::new(&mut disk, table.parts[0].offset, table.parts[0].size)?;
            let report = check(&mut vol)?;
            let data = read_file(&mut vol, "EFI/BOOT/BOOTX64.EFI")?;
            Ok((table, report, data))
        })();
        std::fs::remove_file(&path).unwrap();
        let (table, report, data) = result.unwrap();
        assert_eq!(table.problems, NONE);
        assert_eq!(table.scheme, Some(Scheme::Gpt));
        assert_eq!(report.problems, NONE);
        assert_eq!(report.label, "STICK");
        assert_eq!(data, payload);
    }

    /// A small tree used by the corruption tests, and where its pieces are.
    struct Sample {
        dev: MemDevice,
        geo: Geometry,
        /// First clusters of "big.bin" (20 clusters) and "other.bin" (4 clusters).
        big: u32,
        other: u32,
    }

    fn sample() -> Sample {
        let mut dev = small();
        let mut w = Fat32Writer::open(&mut dev).unwrap();
        put(&mut w, "big.bin", &bytes(51, 20 * 512)).unwrap();
        put(&mut w, "other.bin", &bytes(52, 4 * 512 - 7)).unwrap();
        put(&mut w, "dir/A long file name.txt", &bytes(53, 100)).unwrap();
        put(&mut w, "dir/sub/x.txt", &bytes(54, 600)).unwrap();
        w.finish().unwrap();
        let report = check(&mut dev).unwrap();
        assert_eq!(report.problems, NONE);
        let first = |p: &str| {
            report
                .files
                .iter()
                .find(|f| f.path == p)
                .unwrap()
                .first_cluster
        };
        let reader = Fat32Reader::open(&mut dev).unwrap();
        Sample {
            geo: reader.geometry().clone(),
            big: first("big.bin"),
            other: first("other.bin"),
            dev,
        }
    }

    impl Sample {
        fn set_fat(&mut self, cluster: u32, value: u32, copies: std::ops::Range<u32>) {
            for k in copies {
                let at = self.geo.fat_offset(k) as usize + cluster as usize * 4;
                self.dev.data[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
    }

    fn problems_after(edit: impl FnOnce(&mut Sample)) -> Vec<String> {
        let mut s = sample();
        edit(&mut s);
        check(&mut s.dev).unwrap().problems
    }

    #[test]
    fn checker_reports_corruption() {
        // One byte in the second FAT only.
        let p = problems_after(|s| {
            let at = s.geo.fat_offset(1) as usize + 4 * 1000;
            s.dev.data[at] ^= 1;
        });
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            has(&p, "FAT copies differ (first difference at entry 1000)"),
            "{p:?}"
        );

        // A chain that loops back on itself.
        let p = problems_after(|s| s.set_fat(s.big + 5, s.big + 2, 0..2));
        assert!(
            has(
                &p,
                "\"big.bin\": its cluster chain is cross-linked with another chain or loops"
            ),
            "{p:?}"
        );
        assert!(
            has(&p, "14 cluster(s) are marked in use but belong to no file"),
            "{p:?}"
        );
        // A chain that points at itself.
        let p = problems_after(|s| s.set_fat(s.big, s.big, 0..2));
        assert!(has(&p, "loops"), "{p:?}");

        // Cross-link: one file runs into the other.
        let p = problems_after(|s| s.set_fat(s.big + 19, s.other, 0..2));
        assert!(
            has(&p, "cross-linked") || has(&p, "has 24 clusters"),
            "{p:?}"
        );
        assert!(!p.is_empty());

        // Chain cut short, running into a free cluster, a bad mark, out of the volume.
        let p = problems_after(|s| s.set_fat(s.big + 9, EOC, 0..2));
        assert!(
            has(
                &p,
                "\"big.bin\": has 10 clusters but its size (10240 bytes) needs 20"
            ),
            "{p:?}"
        );
        assert!(has(&p, "10 cluster(s) are marked in use"), "{p:?}");
        let p = problems_after(|s| s.set_fat(s.big + 3, 0, 0..2));
        assert!(has(&p, "runs into a free cluster after cluster"), "{p:?}");
        assert!(has(&p, "FSInfo says"), "{p:?}");
        let p = problems_after(|s| s.set_fat(s.big + 3, BAD_CLUSTER, 0..2));
        assert!(has(&p, "bad-cluster mark"), "{p:?}");
        let p = problems_after(|s| s.set_fat(s.big + 3, 0x0FFF_FFF0, 0..2));
        assert!(
            has(&p, "points outside the volume (cluster 268435440)"),
            "{p:?}"
        );
        let p = problems_after(|s| s.set_fat(s.big + 3, 1, 0..2));
        assert!(has(&p, "points outside the volume (cluster 1)"), "{p:?}");
        // A chain longer than the size needs.
        let p = problems_after(|s| {
            let free = s.other + 500;
            s.set_fat(s.other + 3, free, 0..2);
            s.set_fat(free, EOC, 0..2);
        });
        assert!(
            has(
                &p,
                "\"other.bin\": has 5 clusters but its size (2041 bytes) needs 4"
            ),
            "{p:?}"
        );

        // Reserved FAT entries.
        let p = problems_after(|s| s.set_fat(0, 0x0FFF_FFF0, 0..2));
        assert!(has(&p, "FAT entry 0"), "{p:?}");
        let p = problems_after(|s| s.set_fat(1, 0x07FF_FFFF, 0..2));
        assert!(has(&p, "marked dirty"), "{p:?}");
        // The root directory chain destroyed.
        let p = problems_after(|s| s.set_fat(2, 0, 0..2));
        assert!(
            has(
                &p,
                "Directory \"\": its cluster chain runs into a free cluster"
            ),
            "{p:?}"
        );

        // Signatures and the backup boot sector.
        let p = problems_after(|s| s.dev.data[512] = 0);
        assert!(has(&p, "FSInfo sector: signatures missing"), "{p:?}");
        let p = problems_after(|s| s.dev.data[7 * 512 + 510] = 0);
        assert!(has(&p, "Backup FSInfo sector: signatures missing"), "{p:?}");
        let p = problems_after(|s| s.dev.data[6 * 512 + 100] ^= 4);
        assert_eq!(p, ["The backup boot sector differs from the boot sector."]);
        let p = problems_after(|s| s.dev.data[72] = b'x');
        assert!(has(&p, "backup boot sector differs"), "{p:?}");
        let p = problems_after(|s| s.dev.data[82] = b'X');
        assert!(has(&p, "type string missing"), "{p:?}");
        let p = problems_after(|s| s.dev.data[512 + 488] ^= 1);
        assert!(has(&p, "FSInfo says"), "{p:?}");
        let p = problems_after(|s| {
            s.dev.data[512 + 492..512 + 496].copy_from_slice(&0x7000_0000u32.to_le_bytes())
        });
        assert!(has(&p, "next-free hint"), "{p:?}");

        // Directory damage. Root: label, big.bin, other.bin, dir.
        let root = sample().geo.cluster_offset(2) as usize;
        // File size changed.
        let p = problems_after(|s| {
            s.dev.data[root + 32 + 28..root + 32 + 32].copy_from_slice(&5u32.to_le_bytes())
        });
        assert!(
            has(
                &p,
                "\"big.bin\": has 20 clusters but its size (5 bytes) needs 1"
            ),
            "{p:?}"
        );
        // Size zero with a chain.
        let p = problems_after(|s| s.dev.data[root + 32 + 28..root + 32 + 32].fill(0));
        assert!(
            has(&p, "\"big.bin\": an empty file with a cluster chain"),
            "{p:?}"
        );
        // First cluster zeroed.
        let p = problems_after(|s| {
            s.dev.data[root + 32 + 26..root + 32 + 28].fill(0);
        });
        assert!(
            has(
                &p,
                "\"big.bin\": its cluster chain points outside the volume (cluster 0)"
            ),
            "{p:?}"
        );
        // Two entries with the same name.
        let p = problems_after(|s| s.dev.data.copy_within(root + 32..root + 43, root + 64));
        assert!(has(&p, "occurs twice"), "{p:?}");
        // A second label.
        let p = problems_after(|s| s.dev.data[root + 64 + 11] = 0x08);
        assert!(has(&p, "unexpected volume-label entry"), "{p:?}");
        // The directory "dir": dot entries, then two long-name entries and the short one.
        let dir_cluster = le16(&sample().dev.data, root + 96 + 26) as usize;
        let dir = root + (dir_cluster - 2) * 512;
        let p = problems_after(|s| s.dev.data[dir + 26] ^= 1);
        assert!(has(&p, "Directory \"dir\": bad \".\" entry"), "{p:?}");
        let p = problems_after(|s| s.dev.data[dir + 32 + 26] = 9);
        assert!(has(&p, "Directory \"dir\": bad \"..\" entry"), "{p:?}");
        let p = problems_after(|s| s.dev.data[dir] = b'X');
        assert!(
            has(&p, "\".\" and \"..\" entries missing or wrong"),
            "{p:?}"
        );
        // Long-name checksum of one piece changed; the short name is used instead.
        let p = problems_after(|s| s.dev.data[dir + 64 + 13] ^= 0x55);
        assert!(has(&p, "stray or out-of-order long-name entry"), "{p:?}");
        // Checksum of all pieces changed consistently: they no longer match the short name.
        let p = problems_after(|s| {
            s.dev.data[dir + 64 + 13] ^= 0x55;
            s.dev.data[dir + 96 + 13] ^= 0x55;
        });
        assert!(
            has(
                &p,
                "the long-name entries before \"ALONGF~1.TXT\" do not belong to it"
            ),
            "{p:?}"
        );
        // The short name changed under the long name.
        let p = problems_after(|s| s.dev.data[dir + 128] = b'B');
        assert!(has(&p, "do not belong to it"), "{p:?}");
        // Sequence numbers.
        let p = problems_after(|s| s.dev.data[dir + 64] = 0x40);
        assert!(has(&p, "invalid long-name sequence number 0"), "{p:?}");
        let p = problems_after(|s| s.dev.data[dir + 96] = 0x03);
        assert!(has(&p, "stray or out-of-order"), "{p:?}");
        // The short entry deleted, leaving the long name orphaned; its cluster is lost.
        let p = problems_after(|s| s.dev.data[dir + 128] = 0xE5);
        assert!(has(&p, "followed by a deleted entry"), "{p:?}");
        assert!(has(&p, "1 cluster(s) are marked in use"), "{p:?}");
        // A directory pointing at its own parent: a loop in the tree.
        let p = problems_after(|s| {
            let sub = dir + 160;
            assert_eq!(&s.dev.data[sub..sub + 11], b"SUB        ");
            s.dev.data[sub + 26..sub + 28].copy_from_slice(&(dir_cluster as u16).to_le_bytes());
        });
        assert!(
            has(
                &p,
                "Directory \"dir/sub\": its cluster chain is cross-linked"
            ),
            "{p:?}"
        );
    }

    #[test]
    fn reader_refuses_damaged_files() {
        let mut s = sample();
        let (big, other) = (s.big, s.other);
        s.set_fat(big + 9, EOC, 0..2);
        s.set_fat(other + 1, other, 0..2);
        let reader = Fat32Reader::open(&mut s.dev).unwrap();
        assert!(matches!(
            reader.read_file(&mut s.dev, "big.bin"),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            reader.read_file(&mut s.dev, "other.bin"),
            Err(Error::Invalid(_))
        ));
        assert!(reader.read_file(&mut s.dev, "dir").is_err());
        assert_eq!(
            reader.read_file(&mut s.dev, "dir/sub/x.txt").unwrap(),
            bytes(54, 600)
        );
    }

    #[test]
    fn checker_rejects_what_it_cannot_parse() {
        let edits: [(usize, &[u8]); 12] = [
            (510, &[0]),   // signature
            (11, &[0, 4]), // 1024-byte sectors
            (13, &[3]),    // sectors per cluster
            (13, &[0]),
            (14, &[0, 0]),                   // reserved sectors
            (16, &[0]),                      // FATs
            (22, &[9, 0]),                   // FAT16 size field
            (36, &[0, 0, 0, 0]),             // FAT32 size
            (36, &[1, 0, 0, 0]),             // FAT too short
            (32, &[0xFF, 0xFF, 0xFF, 0x7F]), // more sectors than the device
            (32, &[0, 0x10, 0, 0]),          // too few clusters
            (44, &[0xFF, 0xFF, 0xFF, 0x0F]), // root cluster
        ];
        let base = sample();
        for (at, value) in edits {
            let mut d = clone_dev(&base.dev);
            d.data[at..at + value.len()].copy_from_slice(value);
            assert!(
                matches!(check(&mut d), Err(Error::Invalid(_))),
                "edit at {at}"
            );
            assert!(Fat32Reader::open(&mut d).is_err());
            assert!(Fat32Writer::open(&mut d).is_err());
        }
        // Truncated copies never panic: the boot sector says the volume is larger.
        for sectors in [0usize, 1, 2, 8, 33, 2000, 69631] {
            let mut d = MemDevice::new(sectors as u64 * 512, 512);
            d.data.copy_from_slice(&base.dev.data[..sectors * 512]);
            assert!(check(&mut d).is_err(), "{sectors} sectors");
            assert!(read_file(&mut d, "big.bin").is_err());
        }
        // Blank, garbage, wrong sector size.
        assert!(check(&mut MemDevice::new(SMALL, 512)).is_err());
        let mut d = MemDevice::new(MIB, 512);
        XorShift(7).fill(&mut d.data);
        d.data[510] = 0x55;
        d.data[511] = 0xAA;
        assert!(check(&mut d).is_err());
        let mut d = MemDevice::new(SMALL, 4096);
        d.data.copy_from_slice(&base.dev.data);
        assert!(check(&mut d).is_err());
    }

    #[test]
    fn checker_survives_random_damage() {
        let base = sample();
        let geo = base.geo.clone();
        let fat = geo.fat_offset(0) as usize;
        let root = geo.cluster_offset(2) as usize;
        let mut rng = XorShift(0xFACE_B00C_1357_9BDF);
        for round in 0..150u32 {
            let mut d = clone_dev(&base.dev);
            for _ in 0..1 + round % 8 {
                // The boot sector, the start of the FAT and the directories.
                let at = match rng.next() % 5 {
                    0 => 11 + (rng.next() % 80) as usize,
                    1 => 512 + 484 + (rng.next() % 28) as usize,
                    2 | 3 => fat + (rng.next() % 200) as usize,
                    _ => root + (rng.next() % (40 * 512)) as usize,
                };
                d.data[at] = rng.next() as u8;
            }
            // Either a clean error or a report; reading every file found must
            // not panic or hang either.
            if let Ok(report) = check(&mut d) {
                if let Ok(reader) = Fat32Reader::open(&mut d) {
                    for f in report.files.iter().filter(|f| !f.is_dir).take(20) {
                        if let Ok(mut stream) = reader.open_found(&mut d, f) {
                            let _ = io::copy(&mut stream, &mut io::sink());
                        }
                        let _ = reader.stat(&mut d, &f.path);
                    }
                }
            }
        }
    }

    #[test]
    fn deep_and_looping_directories_are_bounded() {
        // 70 levels: deeper than the reader descends; reported, not followed.
        let mut d = small();
        let mut w = Fat32Writer::open(&mut d).unwrap();
        let deep = vec!["d"; 70].join("/");
        w.create_dir_all(&deep).unwrap();
        w.finish().unwrap();
        let report = check(&mut d).unwrap();
        assert!(
            has(&report.problems, "nested too deeply"),
            "{:?}",
            report.problems
        );
        assert_eq!(report.files.len(), 65);
        let reader = Fat32Reader::open(&mut d).unwrap();
        assert!(
            reader
                .stat(&mut d, &vec!["d"; 60].join("/"))
                .unwrap()
                .is_dir
        );
        assert!(reader.stat(&mut d, &deep).is_err());
    }
}
