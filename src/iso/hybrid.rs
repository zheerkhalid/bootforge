//! Partition tables found inside an image file: the MBR of an isohybrid ISO
//! or of a raw disk image, and a GPT (512- or 4096-byte sectors).

use super::{le16, le32, le64, read_bytes};
use crate::blockdev::ReadAt;
use crate::model::ImagePartition;

/// Most GPT entries that are looked at.
const MAX_GPT_ENTRIES: u32 = 1024;

/// What sector 0 (and the GPT after it) says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionTables {
    /// Sector 0 ends in 0x55AA and its four partition entries are sane, at
    /// least one of them non-empty.
    pub has_mbr: bool,
    /// A GPT header ("EFI PART") follows sector 0.
    pub has_gpt: bool,
    /// Sector size the GPT was found with (512 or 4096); 512 without a GPT.
    pub sector_size: u32,
    /// The GPT's partitions when it has any, else the MBR's.
    pub partitions: Vec<ImagePartition>,
    /// The MBR's own entries (also when a GPT is present).
    pub mbr_partitions: Vec<ImagePartition>,
}

/// Text for an MBR partition type byte.
pub fn mbr_kind(kind: u8) -> String {
    let name = match kind {
        0x00 => "Hidden (isohybrid, 0x00)",
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x05 | 0x0F => "Extended",
        0x07 => "NTFS / exFAT",
        0x0B => "FAT32",
        0x0C => "FAT32 LBA",
        0x17 => "Hidden (isohybrid, 0x17)",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x8E => "Linux LVM",
        0xA5 => "FreeBSD",
        0xAF => "Apple HFS+",
        0xCD => "ISO 9660 (hybrid, 0xCD)",
        0xEE => "GPT protective",
        0xEF => "EFI System",
        other => return format!("0x{other:02X}"),
    };
    name.to_string()
}

/// Formats a GUID stored in the mixed-endian on-disk layout.
pub fn guid_text(g: &[u8]) -> String {
    if g.len() < 16 {
        return String::new();
    }
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        le32(g, 0),
        le16(g, 4),
        le16(g, 6),
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    )
}

/// Text for a GPT partition type GUID (the GUID itself when unknown).
pub fn gpt_kind(type_guid: &[u8]) -> String {
    let text = guid_text(type_guid);
    let name = match text.as_str() {
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI System",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft basic data",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "21686148-6449-6E6F-744E-656564454649" => "BIOS boot",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        _ => return text,
    };
    name.to_string()
}

/// Parses the partition table of a 512-byte sector 0.
/// `None` unless the signature is there and all four entries are sane with
/// at least one non-empty.
pub fn parse_mbr(sector0: &[u8]) -> Option<Vec<ImagePartition>> {
    if sector0.len() < 512 || sector0[510] != 0x55 || sector0[511] != 0xAA {
        return None;
    }
    let mut out = Vec::new();
    for i in 0..4 {
        let e = &sector0[446 + i * 16..446 + (i + 1) * 16];
        if e.iter().all(|b| *b == 0) {
            continue;
        }
        let sectors = le32(e, 12);
        if (e[0] != 0x00 && e[0] != 0x80) || sectors == 0 {
            return None;
        }
        out.push(ImagePartition {
            kind: mbr_kind(e[4]),
            offset: le32(e, 8) as u64 * 512,
            size: sectors as u64 * 512,
            bootable: e[0] == 0x80,
        });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Names the file system of a volume boot sector ("FAT", "NTFS", "exFAT"),
/// for raw images of a single unpartitioned volume.
pub fn boot_sector_fs(sector0: &[u8]) -> Option<&'static str> {
    if sector0.len() < 512 || sector0[510] != 0x55 || sector0[511] != 0xAA {
        return None;
    }
    if &sector0[3..11] == b"NTFS    " {
        return Some("NTFS");
    }
    if &sector0[3..11] == b"EXFAT   " {
        return Some("exFAT");
    }
    let jump = sector0[0] == 0xE9 || (sector0[0] == 0xEB && sector0[2] == 0x90);
    let bytes_per_sector = le16(sector0, 11);
    let per_cluster = sector0[13];
    let fats = sector0[16];
    if jump
        && matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
        && per_cluster.is_power_of_two()
        && le16(sector0, 14) >= 1
        && (1..=2).contains(&fats)
        && sector0[21] >= 0xF0
    {
        return Some("FAT");
    }
    None
}

fn parse_gpt(src: &mut dyn ReadAt, sector: u64) -> Option<Vec<ImagePartition>> {
    let mut h = [0u8; 92];
    read_bytes(src, sector, &mut h, "GPT header").ok()?;
    if &h[0..8] != b"EFI PART" {
        return None;
    }
    let entries_lba = le64(&h, 72);
    let count = le32(&h, 80).min(MAX_GPT_ENTRIES);
    let entry_size = le32(&h, 84) as u64;
    let mut out = Vec::new();
    if !(128..=1024).contains(&entry_size) {
        return Some(out);
    }
    let Some(base) = entries_lba.checked_mul(sector) else {
        return Some(out);
    };
    let mut e = vec![0u8; entry_size as usize];
    for i in 0..count as u64 {
        let Some(at) = base.checked_add(i * entry_size) else {
            break;
        };
        if read_bytes(src, at, &mut e, "GPT entry").is_err() {
            break;
        }
        if e[0..16].iter().all(|b| *b == 0) {
            continue;
        }
        let first = le64(&e, 32);
        let last = le64(&e, 40);
        if last < first {
            continue;
        }
        out.push(ImagePartition {
            kind: gpt_kind(&e[0..16]),
            offset: first.saturating_mul(sector),
            size: (last - first).saturating_add(1).saturating_mul(sector),
            bootable: le64(&e, 48) & 4 != 0,
        });
    }
    Some(out)
}

/// Looks for an MBR and a GPT at the start of the source. Never fails:
/// unreadable or malformed tables are simply absent.
pub fn detect(src: &mut dyn ReadAt) -> PartitionTables {
    let mut t = PartitionTables {
        sector_size: 512,
        ..Default::default()
    };
    let mut s0 = [0u8; 512];
    if read_bytes(src, 0, &mut s0, "sector 0").is_err() {
        return t;
    }
    if boot_sector_fs(&s0).is_none() {
        if let Some(parts) = parse_mbr(&s0) {
            t.has_mbr = true;
            t.mbr_partitions = parts;
        }
    }
    for sector in [512u64, 4096] {
        if let Some(parts) = parse_gpt(src, sector) {
            t.has_gpt = true;
            t.sector_size = sector as u32;
            t.partitions = parts;
            break;
        }
    }
    if t.partitions.is_empty() {
        t.partitions = t.mbr_partitions.clone();
    }
    t
}
