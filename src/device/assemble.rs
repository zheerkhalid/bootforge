//! The pure half of disk enumeration: plain facts in, [`Disk`] values out.
//!
//! Nothing in this file calls Windows. `collect.rs` gathers the raw facts
//! (IOCTL output buffers, volume lists, registry values) and the functions
//! here decode and combine them, which lets every decision that matters for
//! safety (what is the system disk, which letters live on which disk) be
//! unit-tested with invented data.

use crate::model::{Bus, Disk, Partition, Scheme};
use std::collections::BTreeSet;

// --------------------------------------------------------------------------------------
// Facts
// --------------------------------------------------------------------------------------

/// How a partition's type is recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PartitionType {
    /// MBR type byte.
    Mbr(u8),
    /// GPT type GUID, lower-case `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
    Gpt(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PartitionFacts {
    pub number: u32,
    pub offset: u64,
    pub size: u64,
    pub kind: PartitionType,
}

/// What `IOCTL_STORAGE_QUERY_PROPERTY` says about a device (strings raw).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct DeviceStrings {
    pub vendor: String,
    pub product: String,
    pub serial: String,
    /// `STORAGE_BUS_TYPE` value.
    pub bus_type: u32,
    pub removable: bool,
}

/// Everything collected about one physical disk.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct DiskFacts {
    pub number: u32,
    pub device: DeviceStrings,
    /// 0 when there is no medium.
    pub size: u64,
    /// 0 when unknown.
    pub sector_size: u32,
    pub scheme: Option<Scheme>,
    pub partitions: Vec<PartitionFacts>,
    pub offline: bool,
    pub read_only: bool,
}

/// One piece of a volume on a disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Extent {
    pub disk: u32,
    pub offset: u64,
    pub length: u64,
}

/// A mounted (or mountable) volume.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct VolumeFacts {
    /// `\\?\Volume{guid}\`.
    pub guid_path: String,
    /// NT device name, `\Device\HarddiskVolume3`.
    pub device: String,
    pub letter: Option<char>,
    pub label: String,
    pub fs: String,
    pub extents: Vec<Extent>,
    /// (disk number, partition number) from `IOCTL_STORAGE_GET_DEVICE_NUMBER`,
    /// available even when the extents are not (no medium).
    pub device_number: Option<(u32, u32)>,
}

impl VolumeFacts {
    /// Every disk this volume touches.
    pub(crate) fn disks(&self) -> BTreeSet<u32> {
        let mut set: BTreeSet<u32> = self.extents.iter().map(|e| e.disk).collect();
        if let Some((disk, _)) = self.device_number {
            set.insert(disk);
        }
        set
    }
}

/// Where Windows and this program live.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct SystemFacts {
    /// Volume GUID path of the volume holding the Windows directory.
    pub windows_volume: Option<String>,
    /// NT device name of the partition Windows booted from
    /// (`HKLM\SYSTEM\Setup\SystemPartition`), e.g. `\Device\HarddiskVolume1`.
    pub system_partition: Option<String>,
    /// `ExistingPageFiles` entries, e.g. `\??\C:\pagefile.sys`.
    pub page_files: Vec<String>,
    /// Volume GUID path of the volume holding the running executable
    /// (`None` on a network share, or when it could not be resolved).
    pub program_volume: Option<String>,
    /// Drive letter of the running executable, used when the volume could
    /// not be resolved.
    pub program_letter: Option<char>,
}

// --------------------------------------------------------------------------------------
// Decoding IOCTL output
// --------------------------------------------------------------------------------------

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A non-negative `LARGE_INTEGER`.
fn size_at(b: &[u8], at: usize) -> Option<u64> {
    u64_at(b, at).filter(|v| *v <= i64::MAX as u64)
}

/// The NUL-terminated ANSI string at `offset` (0 means "not present").
fn ansi_at(b: &[u8], offset: u32) -> String {
    let offset = offset as usize;
    if offset == 0 || offset >= b.len() {
        return String::new();
    }
    let rest = &b[offset..];
    let end = rest.iter().position(|c| *c == 0).unwrap_or(rest.len());
    rest[..end].iter().map(|&c| c as char).collect()
}

/// Decodes a `STORAGE_DEVICE_DESCRIPTOR` (the output of
/// `IOCTL_STORAGE_QUERY_PROPERTY` for `StorageDeviceProperty`).
///
/// Layout: Version u32 @0, Size u32 @4, DeviceType u8 @8, DeviceTypeModifier
/// u8 @9, RemovableMedia u8 @10, CommandQueueing u8 @11, VendorIdOffset u32
/// @12, ProductIdOffset u32 @16, ProductRevisionOffset u32 @20,
/// SerialNumberOffset u32 @24, BusType u32 @28; the strings follow.
pub(crate) fn parse_device_descriptor(b: &[u8]) -> Option<DeviceStrings> {
    if b.len() < 32 {
        return None;
    }
    Some(DeviceStrings {
        vendor: ansi_at(b, u32_at(b, 12)?),
        product: ansi_at(b, u32_at(b, 16)?),
        serial: ansi_at(b, u32_at(b, 24)?),
        bus_type: u32_at(b, 28)?,
        removable: b[10] != 0,
    })
}

/// Decodes a `DISK_GEOMETRY_EX` into (disk size, bytes per sector).
///
/// Layout: DISK_GEOMETRY { Cylinders i64 @0, MediaType u32 @8,
/// TracksPerCylinder u32 @12, SectorsPerTrack u32 @16, BytesPerSector u32
/// @20 }, DiskSize i64 @24.
pub(crate) fn parse_geometry(b: &[u8]) -> Option<(u64, u32)> {
    Some((size_at(b, 24)?, u32_at(b, 20)?))
}

/// Formats the 16 bytes of a Windows `GUID` in the usual lower-case form.
pub(crate) fn format_guid(b: &[u8]) -> Option<String> {
    let b = b.get(..16)?;
    Some(format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    ))
}

/// Offset of the first `PARTITION_INFORMATION_EX` in a
/// `DRIVE_LAYOUT_INFORMATION_EX` and the size of each entry.
pub(crate) const LAYOUT_HEADER: usize = 48;
pub(crate) const LAYOUT_ENTRY: usize = 144;

const ZERO_GUID: &str = "00000000-0000-0000-0000-000000000000";

/// Decodes a `DRIVE_LAYOUT_INFORMATION_EX` into the partition scheme and the
/// partitions that are in use. Empty table slots, zero-length entries and
/// extended-partition containers are skipped.
///
/// Layout: PartitionStyle u32 @0 (0 MBR, 1 GPT, 2 RAW), PartitionCount u32
/// @4, a 40-byte union @8, then the entries @48, 144 bytes each:
/// PartitionStyle u32 @0, StartingOffset i64 @8, PartitionLength i64 @16,
/// PartitionNumber u32 @24, then @32 either the MBR part (PartitionType u8)
/// or the GPT part (PartitionType GUID).
pub(crate) fn parse_layout(b: &[u8]) -> Option<(Option<Scheme>, Vec<PartitionFacts>)> {
    let style = u32_at(b, 0)?;
    let count = u32_at(b, 4)? as usize;
    let scheme = match style {
        0 => Some(Scheme::Mbr),
        1 => Some(Scheme::Gpt),
        _ => return Some((None, Vec::new())),
    };
    let mut partitions = Vec::new();
    for i in 0..count {
        let at = LAYOUT_HEADER + i * LAYOUT_ENTRY;
        let Some(e) = b.get(at..at + LAYOUT_ENTRY) else {
            break;
        };
        let (Some(offset), Some(size), Some(number)) =
            (size_at(e, 8), size_at(e, 16), u32_at(e, 24))
        else {
            continue;
        };
        if size == 0 {
            continue;
        }
        let kind = if style == 0 {
            let t = e[32];
            // 0 = unused slot; 0x05 / 0x0F / 0x85 = extended container, whose
            // logical drives are listed as entries of their own.
            if matches!(t, 0x00 | 0x05 | 0x0F | 0x85) {
                continue;
            }
            PartitionType::Mbr(t)
        } else {
            let guid = format_guid(&e[32..48])?;
            if guid == ZERO_GUID {
                continue;
            }
            PartitionType::Gpt(guid)
        };
        partitions.push(PartitionFacts {
            number,
            offset,
            size,
            kind,
        });
    }
    partitions.sort_by_key(|p| p.offset);
    // An MBR "disk" without any partition is how Windows reports blank media.
    let scheme = if partitions.is_empty() && style == 0 && u32_at(b, 8) == Some(0) {
        None
    } else {
        scheme
    };
    Some((scheme, partitions))
}

/// Decodes a `VOLUME_DISK_EXTENTS`.
///
/// Layout: NumberOfDiskExtents u32 @0, then @8 entries of 24 bytes:
/// DiskNumber u32 @0, StartingOffset i64 @8, ExtentLength i64 @16.
pub(crate) fn parse_extents(b: &[u8]) -> Vec<Extent> {
    let count = u32_at(b, 0).unwrap_or(0) as usize;
    let mut out = Vec::new();
    for i in 0..count {
        let at = 8 + i * 24;
        let (Some(disk), Some(offset), Some(length)) =
            (u32_at(b, at), size_at(b, at + 8), size_at(b, at + 16))
        else {
            break;
        };
        out.push(Extent {
            disk,
            offset,
            length,
        });
    }
    out
}

/// Decodes a `STORAGE_DEVICE_NUMBER` into (device number, partition number).
/// Layout: DeviceType u32 @0, DeviceNumber u32 @4, PartitionNumber u32 @8.
pub(crate) fn parse_device_number(b: &[u8]) -> Option<(u32, u32)> {
    let number = u32_at(b, 4)?;
    let partition = u32_at(b, 8)?;
    // 0xFFFFFFFF is what multi-disk (dynamic, spanned) volumes report.
    (number != u32::MAX).then_some((number, partition))
}

/// Decodes a `GET_DISK_ATTRIBUTES` into (offline, read-only).
/// Layout: Version u32 @0, Reserved u32 @4, Attributes u64 @8.
pub(crate) fn parse_disk_attributes(b: &[u8]) -> Option<(bool, bool)> {
    let attributes = u64_at(b, 8)?;
    Some((attributes & 1 != 0, attributes & 2 != 0))
}

// --------------------------------------------------------------------------------------
// Names and tidying
// --------------------------------------------------------------------------------------

/// Maps a `STORAGE_BUS_TYPE` value to [`Bus`].
pub(crate) fn bus_from_type(bus_type: u32) -> Bus {
    match bus_type {
        0 => Bus::Unknown,
        1 => Bus::Scsi,
        2 | 3 => Bus::Ata,
        4 => Bus::Other("IEEE 1394".into()),
        5 => Bus::Other("SSA".into()),
        6 | 8 | 9 | 10 | 16 => Bus::Raid,
        7 => Bus::Usb,
        11 => Bus::Sata,
        12 | 13 => Bus::Sd,
        14 | 15 => Bus::Virtual,
        17 => Bus::Nvme,
        18 => Bus::Other("Storage class memory".into()),
        19 => Bus::Other("UFS".into()),
        n => Bus::Other(format!("bus type {n}")),
    }
}

/// Human description of an MBR partition type byte, e.g. `FAT32 (0x0C)`.
pub(crate) fn mbr_type_name(t: u8) -> String {
    let name = match t {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x07 => "NTFS/exFAT",
        0x0B | 0x0C => "FAT32",
        0x05 | 0x0F | 0x85 => "Extended",
        0x11 | 0x14 | 0x16 | 0x1E => "Hidden FAT16",
        0x17 => "Hidden NTFS",
        0x1B | 0x1C => "Hidden FAT32",
        0x27 => "Windows Recovery",
        0x42 => "Dynamic disk",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x8E => "Linux LVM",
        0xA5 => "FreeBSD",
        0xA6 => "OpenBSD",
        0xA8 | 0xAB => "macOS",
        0xAF => "HFS+",
        0xEE => "GPT protective",
        0xEF => "EFI System",
        0xFD => "Linux RAID",
        _ => "Unknown",
    };
    format!("{name} (0x{t:02X})")
}

/// Human description of a GPT partition type GUID.
pub(crate) fn gpt_type_name(guid: &str) -> String {
    let name = match guid.to_ascii_lowercase().as_str() {
        "c12a7328-f81f-11d2-ba4b-00a0c93ec93b" => "EFI System",
        "e3c9e316-0b5c-4db8-817d-f92df00215ae" => "Microsoft Reserved",
        "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7" => "Basic data",
        "de94bba4-06d1-4d40-a16a-bfd50179d6ac" => "Windows Recovery",
        "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3" => "Dynamic disk metadata",
        "af9b60a0-1431-4f62-bc68-3311714a69ad" => "Dynamic disk data",
        "e75caf8f-f680-4cee-afa3-b001e56efc2d" => "Storage Spaces",
        "21686148-6449-6e6f-744e-656564454649" => "BIOS boot",
        "0fc63daf-8483-4772-8e79-3d69d8477de4" => "Linux",
        "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f" => "Linux swap",
        "e6d6d379-f507-44c2-a23c-238f2a3df928" => "Linux LVM",
        "a19d880f-05fc-4d3b-a006-743f0f84911e" => "Linux RAID",
        "48465300-0000-11aa-aa11-00306543ecac" => "HFS+",
        "7c3457ef-0000-11aa-aa11-00306543ecac" => "APFS",
        other => return format!("Unknown ({other})"),
    };
    name.to_string()
}

fn kind_name(kind: &PartitionType) -> String {
    match kind {
        PartitionType::Mbr(t) => mbr_type_name(*t),
        PartitionType::Gpt(g) => gpt_type_name(g),
    }
}

/// Trims a device string: control characters become spaces, runs of white
/// space collapse to one space, both ends are trimmed.
pub(crate) fn tidy_text(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Tidies a serial number as reported by the storage stack.
///
/// Besides trimming padding, this undoes the hex encoding some USB bridges
/// and older drivers apply (the serial arrives as the hex digits of its
/// ASCII characters, often padded with `20`): when the text is an even
/// number of at least 8 hex digits and every decoded byte is printable
/// ASCII, the decoded, trimmed text is used. The result is deterministic, so
/// the same device always yields the same serial, which is what the identity
/// check before writing relies on.
pub(crate) fn tidy_serial(raw: &str) -> String {
    let text = tidy_text(raw);
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() >= 8
        && compact.len().is_multiple_of(2)
        && compact.bytes().all(|b| b.is_ascii_hexdigit())
    {
        let decoded: Vec<u8> = compact
            .as_bytes()
            .chunks(2)
            .filter_map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok())
            .collect();
        let printable = decoded.iter().all(|b| (0x20..=0x7E).contains(b));
        let meaningful = decoded.iter().any(|b| b.is_ascii_alphanumeric());
        if printable && meaningful {
            let s: String = decoded.iter().map(|&b| b as char).collect();
            return tidy_text(&s);
        }
    }
    text
}

/// The drive letter in a page-file entry such as `\??\C:\pagefile.sys`.
pub(crate) fn page_file_letter(entry: &str) -> Option<char> {
    let entry = entry.trim();
    let entry = entry.strip_prefix("\\??\\").unwrap_or(entry);
    let b = entry.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\')
        .then(|| b[0].to_ascii_uppercase() as char)
}

// --------------------------------------------------------------------------------------
// Assembly
// --------------------------------------------------------------------------------------

fn has_letter(volume: &VolumeFacts, letter: char) -> bool {
    volume
        .letter
        .is_some_and(|l| l.eq_ignore_ascii_case(&letter))
}

/// The disks Windows itself depends on, plus whether anything could not be
/// resolved (`doubt`), in which case the caller must fail closed.
pub(crate) fn system_disks(volumes: &[VolumeFacts], facts: &SystemFacts) -> (BTreeSet<u32>, bool) {
    let mut disks = BTreeSet::new();
    let mut doubt = false;
    let mut claim = |found: Option<&VolumeFacts>| match found.map(|v| v.disks()) {
        Some(d) if !d.is_empty() => disks.extend(d),
        _ => doubt = true,
    };

    // The volume holding the Windows directory (also home of hiberfil.sys).
    match &facts.windows_volume {
        Some(guid) => claim(
            volumes
                .iter()
                .find(|v| v.guid_path.eq_ignore_ascii_case(guid)),
        ),
        None => claim(None),
    }
    // The partition the firmware booted Windows from (ESP or active partition).
    match &facts.system_partition {
        Some(device) => claim(
            volumes
                .iter()
                .find(|v| !v.device.is_empty() && v.device.eq_ignore_ascii_case(device.trim())),
        ),
        None => claim(None),
    }
    // Every page file.
    for entry in &facts.page_files {
        match page_file_letter(entry) {
            Some(letter) => claim(volumes.iter().find(|v| has_letter(v, letter))),
            None => claim(None),
        }
    }
    (disks, doubt)
}

/// The disks holding the running executable.
pub(crate) fn program_disks(volumes: &[VolumeFacts], facts: &SystemFacts) -> BTreeSet<u32> {
    let by_guid = facts.program_volume.as_ref().and_then(|guid| {
        volumes
            .iter()
            .find(|v| v.guid_path.eq_ignore_ascii_case(guid))
    });
    let by_letter = || {
        facts
            .program_letter
            .and_then(|l| volumes.iter().find(|v| has_letter(v, l)))
    };
    by_guid
        .or_else(by_letter)
        .map(|v| v.disks())
        .unwrap_or_default()
}

/// Lists a disk's partitions with the volumes mounted on them.
fn partitions_of(disk: &DiskFacts, volumes: &[VolumeFacts]) -> Vec<Partition> {
    let mut parts: Vec<Partition> = disk
        .partitions
        .iter()
        .map(|p| Partition {
            number: p.number,
            offset: p.offset,
            size: p.size,
            kind: kind_name(&p.kind),
            ..Partition::default()
        })
        .collect();

    for v in volumes {
        let extent = v.extents.iter().find(|e| e.disk == disk.number);
        let by_number = v
            .device_number
            .filter(|(d, _)| *d == disk.number)
            .map(|(_, p)| p);
        if extent.is_none() && by_number.is_none() {
            continue;
        }
        // Prefer the partition the volume's extent starts in; fall back to
        // the partition number the volume reports.
        let index = extent
            .and_then(|e| {
                parts.iter().position(|p| p.offset == e.offset).or_else(|| {
                    parts
                        .iter()
                        .position(|p| e.offset >= p.offset && e.offset - p.offset < p.size)
                })
            })
            .or_else(|| {
                by_number
                    .filter(|n| *n != 0)
                    .and_then(|n| parts.iter().position(|p| p.number == n))
            });
        let target = match index {
            Some(i) if parts[i].letter.is_none() && parts[i].fs.is_empty() => &mut parts[i],
            // A second, letterless volume on the same partition adds nothing.
            Some(_) if v.letter.is_none() => continue,
            // No matching entry (or it is taken): never lose a mounted
            // volume, the safety rules need its letter.
            _ => {
                parts.push(Partition {
                    number: by_number.unwrap_or(0),
                    offset: extent.map(|e| e.offset).unwrap_or(0),
                    size: extent.map(|e| e.length).unwrap_or(0),
                    kind: "Volume".to_string(),
                    ..Partition::default()
                });
                parts.last_mut().expect("just pushed")
            }
        };
        target.letter = v.letter.map(|l| l.to_ascii_uppercase());
        target.label = v.label.clone();
        target.fs = v.fs.clone();
    }
    parts
}

/// Combines the collected facts into the final disk list.
///
/// Fail-closed rule: when the Windows volume, the boot partition or a page
/// file cannot be mapped to a disk, every disk that is not attached by USB
/// is marked `is_system`.
pub(crate) fn assemble(
    disks: &[DiskFacts],
    volumes: &[VolumeFacts],
    facts: &SystemFacts,
) -> Vec<Disk> {
    let (system, doubt) = system_disks(volumes, facts);
    let program = program_disks(volumes, facts);
    let mut out: Vec<Disk> = disks
        .iter()
        .map(|d| {
            let bus = bus_from_type(d.device.bus_type);
            let is_system = system.contains(&d.number) || (doubt && bus != Bus::Usb);
            Disk {
                number: d.number,
                vendor: tidy_text(&d.device.vendor),
                model: tidy_text(&d.device.product),
                serial: tidy_serial(&d.device.serial),
                bus,
                removable: d.device.removable,
                size: d.size,
                // Never 0, so that nothing downstream divides by zero; an
                // empty reader is recognised by `size == 0`.
                sector_size: if d.sector_size == 0 {
                    512
                } else {
                    d.sector_size
                },
                scheme: d.scheme,
                partitions: partitions_of(d, volumes),
                is_system,
                holds_program: program.contains(&d.number),
                read_only: d.read_only,
                offline: d.offline,
            }
        })
        .collect();
    out.sort_by_key(|d| d.number);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESP: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
    const MSR: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";
    const DATA: &str = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    fn guid_bytes(text: &str) -> [u8; 16] {
        let hex: String = text.chars().filter(|c| *c != '-').collect();
        let raw: Vec<u8> = (0..16)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let mut b = [0u8; 16];
        // The first three fields are stored little-endian.
        b[..4].copy_from_slice(&[raw[3], raw[2], raw[1], raw[0]]);
        b[4..6].copy_from_slice(&[raw[5], raw[4]]);
        b[6..8].copy_from_slice(&[raw[7], raw[6]]);
        b[8..].copy_from_slice(&raw[8..]);
        b
    }

    /// Builds a STORAGE_DEVICE_DESCRIPTOR with the given strings.
    fn descriptor(vendor: &str, product: &str, serial: &str, bus: u32, removable: bool) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        b[10] = removable as u8;
        b[28..32].copy_from_slice(&bus.to_le_bytes());
        for (field, text) in [(12usize, vendor), (16, product), (24, serial)] {
            if text.is_empty() {
                continue; // offset 0 = absent
            }
            let at = b.len() as u32;
            b[field..field + 4].copy_from_slice(&at.to_le_bytes());
            b.extend_from_slice(text.as_bytes());
            b.push(0);
        }
        let len = b.len() as u32;
        b[4..8].copy_from_slice(&len.to_le_bytes());
        b
    }

    enum Entry<'a> {
        Mbr(u8),
        Gpt(&'a str),
    }

    fn layout(style: u32, signature: u32, entries: &[(u32, u64, u64, Entry)]) -> Vec<u8> {
        let mut b = vec![0u8; LAYOUT_HEADER + entries.len() * LAYOUT_ENTRY];
        b[0..4].copy_from_slice(&style.to_le_bytes());
        b[4..8].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        b[8..12].copy_from_slice(&signature.to_le_bytes());
        for (i, (number, offset, size, kind)) in entries.iter().enumerate() {
            let e = &mut b[LAYOUT_HEADER + i * LAYOUT_ENTRY..][..LAYOUT_ENTRY];
            e[0..4].copy_from_slice(&style.to_le_bytes());
            e[8..16].copy_from_slice(&offset.to_le_bytes());
            e[16..24].copy_from_slice(&size.to_le_bytes());
            e[24..28].copy_from_slice(&number.to_le_bytes());
            match kind {
                Entry::Mbr(t) => e[32] = *t,
                Entry::Gpt(g) => e[32..48].copy_from_slice(&guid_bytes(g)),
            }
        }
        b
    }

    fn part(number: u32, offset: u64, size: u64, kind: PartitionType) -> PartitionFacts {
        PartitionFacts {
            number,
            offset,
            size,
            kind,
        }
    }

    fn volume(
        n: u32,
        letter: Option<char>,
        fs: &str,
        label: &str,
        extents: &[(u32, u64, u64)],
    ) -> VolumeFacts {
        VolumeFacts {
            guid_path: format!("\\\\?\\Volume{{00000000-0000-0000-0000-{n:012}}}\\"),
            device: format!("\\Device\\HarddiskVolume{n}"),
            letter,
            label: label.into(),
            fs: fs.into(),
            extents: extents
                .iter()
                .map(|&(disk, offset, length)| Extent {
                    disk,
                    offset,
                    length,
                })
                .collect(),
            device_number: None,
        }
    }

    /// An invented PC: disk 0 NVMe with Windows, disk 1 SATA data disk,
    /// disk 2 a USB stick, disk 3 an empty card reader.
    fn machine() -> (Vec<DiskFacts>, Vec<VolumeFacts>, SystemFacts) {
        let disks = vec![
            DiskFacts {
                number: 0,
                device: DeviceStrings {
                    vendor: "".into(),
                    product: "Aurora NV900 1TB ".into(),
                    serial: "0011_2233_4455_6677.".into(),
                    bus_type: 17,
                    removable: false,
                },
                size: 1000 * GIB,
                sector_size: 512,
                scheme: Some(Scheme::Gpt),
                partitions: vec![
                    part(1, MIB, 100 * MIB, PartitionType::Gpt(ESP.into())),
                    part(2, 101 * MIB, 16 * MIB, PartitionType::Gpt(MSR.into())),
                    part(3, 117 * MIB, 900 * GIB, PartitionType::Gpt(DATA.into())),
                ],
                ..DiskFacts::default()
            },
            DiskFacts {
                number: 1,
                device: DeviceStrings {
                    vendor: "".into(),
                    product: "Granite HD4000".into(),
                    serial: "   GR-4000-XYZ".into(),
                    bus_type: 11,
                    removable: false,
                },
                size: 4000 * GIB,
                sector_size: 4096,
                scheme: Some(Scheme::Gpt),
                partitions: vec![part(
                    1,
                    16 * MIB,
                    3999 * GIB,
                    PartitionType::Gpt(DATA.into()),
                )],
                ..DiskFacts::default()
            },
            DiskFacts {
                number: 2,
                device: DeviceStrings {
                    vendor: "Kestrel ".into(),
                    product: "Flash  3.1\0".into(),
                    serial: "4B455354524C3031".into(), // "KESTRL01"
                    bus_type: 7,
                    removable: true,
                },
                size: 32 * GIB,
                sector_size: 512,
                scheme: Some(Scheme::Mbr),
                partitions: vec![part(1, MIB, 31 * GIB, PartitionType::Mbr(0x0C))],
                ..DiskFacts::default()
            },
            DiskFacts {
                number: 3,
                device: DeviceStrings {
                    vendor: "Generic".into(),
                    product: "SD Reader".into(),
                    serial: "".into(),
                    bus_type: 7,
                    removable: true,
                },
                ..DiskFacts::default()
            },
        ];
        let mut reader = volume(6, Some('G'), "", "", &[]);
        reader.device_number = Some((3, 1));
        let volumes = vec![
            volume(1, None, "FAT32", "", &[(0, MIB, 100 * MIB)]),
            volume(
                3,
                Some('C'),
                "NTFS",
                "Windows",
                &[(0, 117 * MIB, 900 * GIB)],
            ),
            volume(4, Some('D'), "NTFS", "Data", &[(1, 16 * MIB, 3999 * GIB)]),
            volume(5, Some('e'), "FAT32", "STICK", &[(2, MIB, 31 * GIB)]),
            reader,
        ];
        let facts = SystemFacts {
            windows_volume: Some(volumes[1].guid_path.to_uppercase()),
            system_partition: Some("\\Device\\HarddiskVolume1".into()),
            page_files: vec!["\\??\\C:\\pagefile.sys".into()],
            program_volume: Some(volumes[2].guid_path.clone()),
            program_letter: Some('D'),
        };
        (disks, volumes, facts)
    }

    #[test]
    fn device_descriptor_is_decoded() {
        let b = descriptor("Kestrel ", "Flash 3.1", "AB12", 7, true);
        let d = parse_device_descriptor(&b).unwrap();
        assert_eq!(
            d,
            DeviceStrings {
                vendor: "Kestrel ".into(),
                product: "Flash 3.1".into(),
                serial: "AB12".into(),
                bus_type: 7,
                removable: true,
            }
        );
        // Absent strings, fixed media.
        let d = parse_device_descriptor(&descriptor("", "Disk", "", 17, false)).unwrap();
        assert_eq!(
            (d.vendor.as_str(), d.serial.as_str(), d.removable),
            ("", "", false)
        );
        // Truncated header, and offsets pointing outside the buffer.
        assert!(parse_device_descriptor(&b[..20]).is_none());
        let mut wild = descriptor("", "", "", 1, false);
        wild[24..28].copy_from_slice(&5000u32.to_le_bytes());
        assert_eq!(parse_device_descriptor(&wild).unwrap().serial, "");
        // A string that runs to the end of the buffer without a NUL.
        let mut open = descriptor("", "", "XYZ", 1, false);
        open.pop();
        assert_eq!(parse_device_descriptor(&open).unwrap().serial, "XYZ");
    }

    #[test]
    fn geometry_extents_numbers_and_attributes() {
        let mut g = vec![0u8; 40];
        g[20..24].copy_from_slice(&4096u32.to_le_bytes());
        g[24..32].copy_from_slice(&(64 * GIB).to_le_bytes());
        assert_eq!(parse_geometry(&g), Some((64 * GIB, 4096)));
        assert_eq!(parse_geometry(&g[..30]), None);
        g[24..32].copy_from_slice(&(-1i64).to_le_bytes());
        assert_eq!(parse_geometry(&g), None);

        let mut e = vec![0u8; 8 + 2 * 24];
        e[0..4].copy_from_slice(&2u32.to_le_bytes());
        e[8..12].copy_from_slice(&4u32.to_le_bytes());
        e[16..24].copy_from_slice(&MIB.to_le_bytes());
        e[24..32].copy_from_slice(&GIB.to_le_bytes());
        e[32..36].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(
            parse_extents(&e),
            vec![
                Extent {
                    disk: 4,
                    offset: MIB,
                    length: GIB
                },
                Extent {
                    disk: 5,
                    offset: 0,
                    length: 0
                }
            ]
        );
        // A count larger than the buffer does not read past it.
        e[0..4].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(parse_extents(&e).len(), 2);
        assert!(parse_extents(&[]).is_empty());

        let mut n = vec![0u8; 12];
        n[4..8].copy_from_slice(&3u32.to_le_bytes());
        n[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(parse_device_number(&n), Some((3, 2)));
        n[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_device_number(&n), None);
        assert_eq!(parse_device_number(&n[..8]), None);

        let mut a = vec![0u8; 16];
        assert_eq!(parse_disk_attributes(&a), Some((false, false)));
        a[8] = 3;
        assert_eq!(parse_disk_attributes(&a), Some((true, true)));
        a[8] = 2;
        assert_eq!(parse_disk_attributes(&a), Some((false, true)));
        assert_eq!(parse_disk_attributes(&a[..8]), None);
    }

    #[test]
    fn layout_structs_have_the_documented_shape() {
        use std::mem::{offset_of, size_of};
        use windows::Win32::System::Ioctl::{
            DISK_EXTENT, DISK_GEOMETRY_EX, DRIVE_LAYOUT_INFORMATION_EX, GET_DISK_ATTRIBUTES,
            PARTITION_INFORMATION_EX, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER,
            VOLUME_DISK_EXTENTS,
        };
        // The byte offsets used by the parsers, checked against the system headers.
        assert_eq!(size_of::<PARTITION_INFORMATION_EX>(), LAYOUT_ENTRY);
        assert_eq!(
            offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry),
            LAYOUT_HEADER
        );
        assert_eq!(offset_of!(PARTITION_INFORMATION_EX, StartingOffset), 8);
        assert_eq!(offset_of!(PARTITION_INFORMATION_EX, PartitionLength), 16);
        assert_eq!(offset_of!(PARTITION_INFORMATION_EX, PartitionNumber), 24);
        assert_eq!(offset_of!(PARTITION_INFORMATION_EX, Anonymous), 32);
        assert_eq!(offset_of!(DISK_GEOMETRY_EX, DiskSize), 24);
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia), 10);
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset), 12);
        assert_eq!(
            offset_of!(STORAGE_DEVICE_DESCRIPTOR, SerialNumberOffset),
            24
        );
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType), 28);
        assert_eq!(offset_of!(VOLUME_DISK_EXTENTS, Extents), 8);
        assert_eq!(size_of::<DISK_EXTENT>(), 24);
        assert_eq!(offset_of!(STORAGE_DEVICE_NUMBER, DeviceNumber), 4);
        assert_eq!(offset_of!(GET_DISK_ATTRIBUTES, Attributes), 8);
    }

    #[test]
    fn gpt_layout_skips_unused_entries() {
        let b = layout(
            1,
            0,
            &[
                (3, 117 * MIB, 900 * GIB, Entry::Gpt(DATA)),
                (1, MIB, 100 * MIB, Entry::Gpt(ESP)),
                (0, 0, 0, Entry::Gpt(ZERO_GUID)),
                (4, 5 * GIB, 0, Entry::Gpt(DATA)),
                (5, 950 * GIB, GIB, Entry::Gpt(ZERO_GUID)),
                (2, 101 * MIB, 16 * MIB, Entry::Gpt(MSR)),
            ],
        );
        let (scheme, parts) = parse_layout(&b).unwrap();
        assert_eq!(scheme, Some(Scheme::Gpt));
        assert_eq!(
            parts.iter().map(|p| p.number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(parts[0].kind, PartitionType::Gpt(ESP.into()));
        assert_eq!((parts[2].offset, parts[2].size), (117 * MIB, 900 * GIB));
        // A buffer cut short yields the entries that are complete.
        let (_, parts) = parse_layout(&b[..LAYOUT_HEADER + LAYOUT_ENTRY + 10]).unwrap();
        assert_eq!(parts.len(), 1);
        assert!(parse_layout(&b[..6]).is_none());
    }

    #[test]
    fn mbr_layout_skips_empty_slots_and_containers() {
        let b = layout(
            0,
            0x1234_5678,
            &[
                (1, MIB, 8 * GIB, Entry::Mbr(0x0C)),
                (0, 9 * GIB, 20 * GIB, Entry::Mbr(0x0F)),
                (0, 0, 0, Entry::Mbr(0)),
                (0, 0, 0, Entry::Mbr(0)),
                (2, 9 * GIB + MIB, 10 * GIB, Entry::Mbr(0x07)),
            ],
        );
        let (scheme, parts) = parse_layout(&b).unwrap();
        assert_eq!(scheme, Some(Scheme::Mbr));
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].kind, PartitionType::Mbr(0x07));
        // Blank media: "MBR" with signature 0 and nothing in it.
        let blank = layout(
            0,
            0,
            &[
                (0, 0, 0, Entry::Mbr(0)),
                (0, 0, 0, Entry::Mbr(0)),
                (0, 0, 0, Entry::Mbr(0)),
                (0, 0, 0, Entry::Mbr(0)),
            ],
        );
        assert_eq!(parse_layout(&blank).unwrap(), (None, vec![]));
        // An initialised but empty MBR disk keeps its scheme.
        let empty = layout(0, 0xCAFE, &[]);
        assert_eq!(parse_layout(&empty).unwrap(), (Some(Scheme::Mbr), vec![]));
        // RAW.
        assert_eq!(parse_layout(&layout(2, 0, &[])).unwrap(), (None, vec![]));
    }

    #[test]
    fn guid_formatting() {
        assert_eq!(format_guid(&guid_bytes(ESP)).unwrap(), ESP);
        assert_eq!(format_guid(&[0u8; 16]).unwrap(), ZERO_GUID);
        assert!(format_guid(&[0u8; 15]).is_none());
    }

    #[test]
    fn bus_types_map_to_buses() {
        let expect = [
            (0, Bus::Unknown),
            (1, Bus::Scsi),
            (2, Bus::Ata),
            (3, Bus::Ata),
            (6, Bus::Raid),
            (7, Bus::Usb),
            (8, Bus::Raid),
            (9, Bus::Raid),
            (10, Bus::Raid),
            (11, Bus::Sata),
            (12, Bus::Sd),
            (13, Bus::Sd),
            (14, Bus::Virtual),
            (15, Bus::Virtual),
            (16, Bus::Raid),
            (17, Bus::Nvme),
        ];
        for (n, bus) in expect {
            assert_eq!(bus_from_type(n), bus, "bus type {n}");
        }
        assert_eq!(bus_from_type(4), Bus::Other("IEEE 1394".into()));
        assert_eq!(bus_from_type(19), Bus::Other("UFS".into()));
        assert_eq!(bus_from_type(99), Bus::Other("bus type 99".into()));
        // Only type 7 is ever USB.
        assert_eq!(
            (0..200).filter(|n| bus_from_type(*n) == Bus::Usb).count(),
            1
        );
    }

    #[test]
    fn partition_type_names() {
        assert_eq!(mbr_type_name(0x0C), "FAT32 (0x0C)");
        assert_eq!(mbr_type_name(0x07), "NTFS/exFAT (0x07)");
        assert_eq!(mbr_type_name(0xEF), "EFI System (0xEF)");
        assert_eq!(mbr_type_name(0x99), "Unknown (0x99)");
        assert_eq!(gpt_type_name(ESP), "EFI System");
        assert_eq!(gpt_type_name(&DATA.to_uppercase()), "Basic data");
        assert_eq!(gpt_type_name(MSR), "Microsoft Reserved");
        assert_eq!(
            gpt_type_name("11111111-2222-3333-4444-555555555555"),
            "Unknown (11111111-2222-3333-4444-555555555555)"
        );
    }

    #[test]
    fn strings_are_tidied() {
        assert_eq!(
            tidy_text("  Kestrel   Flash\t3.1 \0\0"),
            "Kestrel Flash 3.1"
        );
        assert_eq!(tidy_text("\u{1}\u{2}"), "");
        // Padded.
        assert_eq!(tidy_serial("      GR-4000-XYZ   "), "GR-4000-XYZ");
        // Hex-encoded ASCII, with encoded padding.
        assert_eq!(tidy_serial("4B455354524C3031"), "KESTRL01");
        assert_eq!(tidy_serial("202020204b455354524c3031"), "KESTRL01");
        assert_eq!(tidy_serial(" 2020 4B45 5354 "), "KEST");
        // Hex digits that do not decode to printable text stay as they are.
        assert_eq!(tidy_serial("00E04C680001"), "00E04C680001");
        assert_eq!(tidy_serial("AA11BB22CC33DD44"), "AA11BB22CC33DD44");
        // Too short, odd length or not hex: untouched.
        assert_eq!(tidy_serial("414243"), "414243");
        assert_eq!(tidy_serial("4B455354524C303"), "4B455354524C303");
        assert_eq!(tidy_serial("0011_2233_4455_6677."), "0011_2233_4455_6677.");
        // All-space hex decodes to nothing meaningful and is kept.
        assert_eq!(tidy_serial("2020202020202020"), "2020202020202020");
        assert_eq!(tidy_serial(""), "");
        // Deterministic.
        assert_eq!(
            tidy_serial(&tidy_serial("  ABC 123 ")),
            tidy_serial("  ABC 123 ")
        );
    }

    #[test]
    fn page_file_entries() {
        assert_eq!(page_file_letter("\\??\\C:\\pagefile.sys"), Some('C'));
        assert_eq!(page_file_letter("d:\\swapfile.sys"), Some('D'));
        assert_eq!(page_file_letter("\\??\\Volume{x}\\pagefile.sys"), None);
        assert_eq!(page_file_letter(""), None);
    }

    #[test]
    fn full_machine_is_assembled() {
        let (disks, volumes, facts) = machine();
        let out = assemble(&disks, &volumes, &facts);
        assert_eq!(out.len(), 4);

        let nvme = &out[0];
        assert_eq!((nvme.bus.clone(), nvme.removable), (Bus::Nvme, false));
        assert_eq!(nvme.model, "Aurora NV900 1TB");
        assert_eq!(nvme.serial, "0011_2233_4455_6677.");
        assert!(nvme.is_system && !nvme.holds_program);
        assert_eq!(nvme.scheme, Some(Scheme::Gpt));
        assert_eq!(nvme.partitions.len(), 3);
        assert_eq!(nvme.partitions[0].kind, "EFI System");
        assert_eq!(nvme.partitions[0].fs, "FAT32");
        assert_eq!(nvme.partitions[0].letter, None);
        assert_eq!(nvme.partitions[1].kind, "Microsoft Reserved");
        assert_eq!(nvme.partitions[2].letter, Some('C'));
        assert_eq!(nvme.partitions[2].label, "Windows");
        assert_eq!(nvme.letters(), vec!['C']);

        let sata = &out[1];
        assert_eq!(sata.bus, Bus::Sata);
        assert_eq!(sata.serial, "GR-4000-XYZ");
        assert_eq!(sata.sector_size, 4096);
        assert!(!sata.is_system && sata.holds_program);

        let stick = &out[2];
        assert_eq!((stick.bus.clone(), stick.removable), (Bus::Usb, true));
        assert_eq!(stick.name(), "Kestrel Flash 3.1");
        assert_eq!(stick.serial, "KESTRL01");
        assert!(!stick.is_system && !stick.holds_program);
        assert_eq!(stick.partitions[0].kind, "FAT32 (0x0C)");
        // Letters are upper-cased.
        assert_eq!(stick.letters(), vec!['E']);
        assert_eq!(stick.partitions[0].label, "STICK");

        // The empty reader is still listed, with its letter, size 0.
        let reader = &out[3];
        assert_eq!(
            (reader.size, reader.sector_size, reader.scheme),
            (0, 512, None)
        );
        assert_eq!(reader.letters(), vec!['G']);
        assert!(!reader.is_system);
    }

    #[test]
    fn boot_partition_and_page_file_disks_are_system_disks() {
        let (disks, mut volumes, mut facts) = machine();
        // Windows boots from an ESP on the SATA disk and pages to the stick.
        volumes.push(volume(9, None, "FAT32", "", &[(1, MIB, 15 * MIB)]));
        facts.system_partition = Some("\\device\\harddiskvolume9".into());
        facts.page_files.push("\\??\\E:\\pagefile.sys".into());
        let out = assemble(&disks, &volumes, &facts);
        assert_eq!(
            out.iter().map(|d| d.is_system).collect::<Vec<_>>(),
            vec![true, true, true, false]
        );
    }

    #[test]
    fn a_spanned_windows_volume_marks_every_disk_it_touches() {
        let (disks, mut volumes, facts) = machine();
        volumes[1].extents.push(Extent {
            disk: 1,
            offset: 3000 * GIB,
            length: 100 * GIB,
        });
        let out = assemble(&disks, &volumes, &facts);
        assert!(out[0].is_system && out[1].is_system && !out[2].is_system);
    }

    #[test]
    fn unresolvable_system_facts_fail_closed() {
        let (disks, volumes, good) = machine();
        type Change = Box<dyn Fn(&mut SystemFacts)>;
        let cases: Vec<Change> = vec![
            Box::new(|f| f.windows_volume = None),
            Box::new(|f| f.windows_volume = Some("\\\\?\\Volume{unknown}\\".into())),
            Box::new(|f| f.system_partition = None),
            Box::new(|f| f.system_partition = Some("\\Device\\HarddiskVolume77".into())),
            Box::new(|f| f.page_files.push("\\??\\Q:\\pagefile.sys".into())),
            Box::new(|f| f.page_files.push("nonsense".into())),
        ];
        for (i, change) in cases.iter().enumerate() {
            let mut facts = good.clone();
            change(&mut facts);
            let out = assemble(&disks, &volumes, &facts);
            // Every non-USB disk is now a system disk; USB disks stay usable.
            assert!(out[0].is_system && out[1].is_system, "case {i}");
            assert!(!out[2].is_system && !out[3].is_system, "case {i}");
        }
        // A Windows volume that exists but reports no extents is also doubt.
        let mut blind = volumes.clone();
        blind[1].extents.clear();
        let out = assemble(&disks, &blind, &good);
        assert!(out[1].is_system);
        // No page file at all is not doubt.
        let mut none = good.clone();
        none.page_files.clear();
        let out = assemble(&disks, &volumes, &none);
        assert!(out[0].is_system && !out[1].is_system);
    }

    #[test]
    fn windows_on_a_usb_disk_is_still_a_system_disk() {
        let (disks, mut volumes, mut facts) = machine();
        // Windows To Go: everything lives on the stick.
        facts.windows_volume = Some(volumes[3].guid_path.clone());
        facts.system_partition = Some(volumes[3].device.clone());
        facts.page_files = vec!["\\??\\E:\\pagefile.sys".into()];
        volumes[3].letter = Some('E');
        let out = assemble(&disks, &volumes, &facts);
        assert!(out[2].is_system);
        assert!(!out[0].is_system);
    }

    #[test]
    fn program_disk_falls_back_to_the_letter() {
        let (disks, volumes, mut facts) = machine();
        facts.program_volume = None;
        facts.program_letter = Some('E');
        let out = assemble(&disks, &volumes, &facts);
        assert!(out[2].holds_program && !out[1].holds_program);
        // On a network share: no disk holds the program.
        facts.program_letter = None;
        let out = assemble(&disks, &volumes, &facts);
        assert!(out.iter().all(|d| !d.holds_program));
        // An unknown GUID falls back to the letter as well.
        facts.program_volume = Some("\\\\?\\Volume{gone}\\".into());
        facts.program_letter = Some('C');
        let out = assemble(&disks, &volumes, &facts);
        assert!(out[0].holds_program);
    }

    #[test]
    fn volumes_match_partitions_by_offset_number_or_get_their_own_entry() {
        let disk = DiskFacts {
            number: 5,
            size: 64 * GIB,
            sector_size: 512,
            scheme: Some(Scheme::Mbr),
            partitions: vec![
                part(1, MIB, 10 * GIB, PartitionType::Mbr(0x07)),
                part(2, 11 * GIB, 10 * GIB, PartitionType::Mbr(0x0B)),
            ],
            ..DiskFacts::default()
        };
        // Extent inside (not at the start of) partition 2.
        let inside = volume(1, Some('K'), "FAT32", "IN", &[(5, 11 * GIB + 4096, GIB)]);
        // Known only by device number.
        let mut numbered = volume(2, Some('L'), "NTFS", "NUM", &[]);
        numbered.device_number = Some((5, 1));
        // On the disk but outside every partition.
        let stray = volume(3, Some('M'), "exFAT", "STRAY", &[(5, 40 * GIB, GIB)]);
        // On another disk entirely.
        let other = volume(4, Some('N'), "NTFS", "OTHER", &[(6, MIB, GIB)]);
        let parts = partitions_of(&disk, &[inside, numbered, stray, other]);
        assert_eq!(parts.len(), 3);
        assert_eq!(
            (parts[0].letter, parts[0].label.as_str()),
            (Some('L'), "NUM")
        );
        assert_eq!(
            (parts[1].letter, parts[1].fs.as_str()),
            (Some('K'), "FAT32")
        );
        assert_eq!(parts[2].letter, Some('M'));
        assert_eq!(
            (parts[2].offset, parts[2].size, parts[2].kind.as_str()),
            (40 * GIB, GIB, "Volume")
        );

        // Two lettered volumes claiming one partition: neither letter is lost.
        let a = volume(1, Some('P'), "NTFS", "", &[(5, MIB, GIB)]);
        let b = volume(2, Some('Q'), "NTFS", "", &[(5, MIB, GIB)]);
        let parts = partitions_of(&disk, &[a, b]);
        let letters: Vec<char> = parts.iter().filter_map(|p| p.letter).collect();
        assert_eq!(letters, vec!['P', 'Q']);
    }
}
