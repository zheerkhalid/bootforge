//! Invented devices and images for `--demo` and the screenshots.
//! Nothing here is read from, or refers to, a real machine.

use crate::model::{
    Arch, BootEntry, BootPlatform, Bus, Checksums, Disk, ImageInfo, ImageKind, ImagePartition,
    Partition, Scheme, WindowsInfo,
};
use std::path::PathBuf;

const MIB: u64 = 1024 * 1024;

fn part(
    number: u32,
    offset: u64,
    size: u64,
    kind: &str,
    letter: Option<char>,
    label: &str,
    fs: &str,
) -> Partition {
    Partition {
        number,
        offset,
        size,
        kind: kind.into(),
        letter,
        label: label.into(),
        fs: fs.into(),
    }
}

/// The whole (invented) machine: internal disks included, so that the demo
/// goes through the same safety filter as the real program.
pub fn disks() -> Vec<Disk> {
    let base = Disk {
        number: 0,
        vendor: String::new(),
        model: String::new(),
        serial: String::new(),
        bus: Bus::Usb,
        removable: true,
        size: 0,
        sector_size: 512,
        scheme: Some(Scheme::Mbr),
        partitions: Vec::new(),
        is_system: false,
        holds_program: false,
        read_only: false,
        offline: false,
    };
    vec![
        Disk {
            number: 0,
            vendor: "Corvid".into(),
            model: "NV2 Pro 1TB".into(),
            serial: "DEMO-0000-A1".into(),
            bus: Bus::Nvme,
            removable: false,
            size: 1_000_204_886_016,
            scheme: Some(Scheme::Gpt),
            partitions: vec![
                part(1, MIB, 260 * MIB, "EFI System", None, "", "FAT32"),
                part(
                    2,
                    261 * MIB,
                    930_000 * MIB,
                    "Basic data",
                    Some('C'),
                    "Windows",
                    "NTFS",
                ),
            ],
            is_system: true,
            holds_program: true,
            ..base.clone()
        },
        Disk {
            number: 1,
            vendor: "Halden".into(),
            model: "Barrow 4TB".into(),
            serial: "DEMO-0001-B2".into(),
            bus: Bus::Sata,
            removable: false,
            size: 4_000_787_030_016,
            scheme: Some(Scheme::Gpt),
            partitions: vec![part(
                1,
                MIB,
                3_800_000 * MIB,
                "Basic data",
                Some('D'),
                "Archive",
                "NTFS",
            )],
            ..base.clone()
        },
        Disk {
            number: 2,
            vendor: "Kestrel".into(),
            model: "Swift 3.2".into(),
            serial: "DEMO-0002-C3".into(),
            size: 62_008_590_336,
            partitions: vec![part(
                1,
                MIB,
                59_130 * MIB,
                "FAT32 (0x0C)",
                Some('E'),
                "KESTREL",
                "FAT32",
            )],
            ..base.clone()
        },
        Disk {
            number: 3,
            vendor: "Northwind".into(),
            model: "Pocket Drive".into(),
            serial: "DEMO-0003-D4".into(),
            size: 15_518_924_800,
            scheme: Some(Scheme::Gpt),
            partitions: vec![
                part(
                    1,
                    MIB,
                    4_700 * MIB,
                    "Basic data",
                    Some('F'),
                    "NIMBUS_24",
                    "FAT32",
                ),
                part(
                    2,
                    4_701 * MIB,
                    10_000 * MIB,
                    "Basic data",
                    Some('G'),
                    "Files",
                    "exFAT",
                ),
            ],
            ..base.clone()
        },
        Disk {
            number: 4,
            vendor: "Tarn".into(),
            model: "Card Reader".into(),
            serial: "DEMO-0004-E5".into(),
            bus: Bus::Sd,
            size: 127_865_454_592,
            partitions: vec![part(
                1,
                16 * MIB,
                121_900 * MIB,
                "exFAT/NTFS (0x07)",
                Some('H'),
                "CAMERA",
                "exFAT",
            )],
            read_only: true,
            ..base.clone()
        },
        Disk {
            number: 5,
            vendor: "Orbit".into(),
            model: "Portable SSD".into(),
            serial: "DEMO-0005-F6".into(),
            removable: false,
            size: 500_107_862_016,
            scheme: Some(Scheme::Gpt),
            partitions: vec![part(
                1,
                MIB,
                476_900 * MIB,
                "Basic data",
                Some('I'),
                "Projects",
                "NTFS",
            )],
            ..base
        },
    ]
}

fn entry(platform: BootPlatform, lba: u32, sectors: u32) -> BootEntry {
    BootEntry {
        platform,
        bootable: true,
        emulation: "no emulation".into(),
        lba,
        sectors,
    }
}

/// An invented Windows setup ISO with an install.wim that needs splitting.
pub fn windows_image() -> ImageInfo {
    let install = 5_420 * MIB;
    let mut i = ImageInfo::new(
        PathBuf::from("D:\\Images\\Windows11_Setup_x64.iso"),
        6_390 * MIB,
    );
    i.kind = ImageKind::Iso;
    i.label = "WIN11_X64_SETUP".into();
    i.has_iso9660 = true;
    i.udf = Some("1.02".into());
    i.boot_entries = vec![
        entry(BootPlatform::Bios, 522, 8),
        entry(BootPlatform::Uefi, 524, 2880),
    ];
    i.efi_loaders = vec!["bootx64.efi".into()];
    i.archs = vec![Arch::X64];
    i.windows = Some(WindowsInfo {
        install_path: "sources/install.wim".into(),
        install_size: install,
        is_esd: false,
        editions: vec![
            "Windows 11 Home".into(),
            "Windows 11 Pro".into(),
            "Windows 11 Education".into(),
            "Windows 11 Pro for Workstations".into(),
        ],
        version: "10.0.26100".into(),
        over_4g: true,
        is_setup: true,
        has_answer_file: false,
    });
    i.family = "Windows setup".into();
    i.file_count = 968;
    i.total_file_bytes = 6_371 * MIB;
    i.largest_file = Some(("sources/install.wim".into(), install));
    i
}

/// An invented hybrid Linux live ISO.
pub fn linux_image() -> ImageInfo {
    let mut i = ImageInfo::new(
        PathBuf::from("D:\\Images\\nimbus-24.10-desktop-amd64.iso"),
        3_912 * MIB,
    );
    i.kind = ImageKind::Iso;
    i.label = "NIMBUS_24_10".into();
    i.has_iso9660 = true;
    i.has_joliet = true;
    i.has_rock_ridge = true;
    i.has_mbr = true;
    i.has_gpt = true;
    i.partitions = vec![
        ImagePartition {
            kind: "ISO 9660 data".into(),
            offset: 64 * 512,
            size: 3_900 * MIB,
            bootable: true,
        },
        ImagePartition {
            kind: "EFI System".into(),
            offset: 3_901 * MIB,
            size: 5 * MIB,
            bootable: false,
        },
    ];
    i.boot_entries = vec![
        entry(BootPlatform::Bios, 260, 4),
        entry(BootPlatform::Uefi, 1_997_312, 10_144),
    ];
    i.efi_loaders = vec!["BOOTX64.EFI".into()];
    i.archs = vec![Arch::X64];
    i.family = "Linux live/installer".into();
    i.file_count = 1_204;
    i.total_file_bytes = 3_880 * MIB;
    i.largest_file = Some(("casper/filesystem.squashfs".into(), 3_310 * MIB));
    i
}

/// A raw disk image (for the "restore" demo).
pub fn disk_image() -> ImageInfo {
    let mut i = ImageInfo::new(
        PathBuf::from("D:\\Backups\\kestrel-backup.img"),
        62_008_590_336,
    );
    i.kind = ImageKind::DiskImage;
    i.has_mbr = true;
    i.partitions = vec![ImagePartition {
        kind: "FAT32 LBA".into(),
        offset: MIB,
        size: 59_130 * MIB,
        bootable: false,
    }];
    i
}

/// Checksums shown on the demo checksum page (of no real file).
/// The recently used images shown on the empty Create page.
pub fn recent_images() -> Vec<PathBuf> {
    vec![
        windows_image().path,
        linux_image().path,
        PathBuf::from("D:\\Images\\tern-rescue-12.1.iso"),
    ]
}

pub fn checksums() -> Checksums {
    Checksums {
        md5: "5f2b1c0d9e8a7b6c4d3e2f1a0b9c8d7e".into(),
        sha1: "8c1f04e2a7d95b3c6e0f1a2b3c4d5e6f7a8b9c0d".into(),
        sha256: "3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b".into(),
    }
}

/// A plausible log for the log pane.
pub fn log_lines() -> Vec<&'static str> {
    vec![
        "BootForge started (demo mode: invented devices, nothing is written).",
        "Found 2 usable drives; 3 hidden by the safety rules.",
        "Inspecting D:\\Images\\Windows11_Setup_x64.iso",
        "ISO 9660 + UDF 1.02, label WIN11_X64_SETUP, 968 files, 6.22 GB",
        "El Torito: BIOS (no emulation), UEFI (no emulation)",
        "Windows setup image: sources/install.wim 5.29 GB, 4 editions, x64, 10.0.26100",
        "install.wim exceeds 4 GB: it will be split into 2 parts with DISM",
        "== Checking the drive",
        "Kestrel Swift 3.2, 57.7 GB, USB, disk 2: identity confirmed",
        "== Locking the drive",
        "Locked and dismounted E:",
        "== Splitting install.wim",
        "DISM: 2 parts written to the temporary folder",
        "== Partitioning",
        "GPT, 1 partition (basic data), 57.7 GB at 1 MiB",
        "== Formatting",
        "FAT32, 32 KB clusters, label WIN11_X64_S",
        "== Copying files",
        "968 files, 6.22 GB copied at 41.3 MB/s",
        "== Verifying",
        "968 files re-read and compared: all identical",
        "== Finishing",
        "Volumes unlocked, partition table re-read by Windows",
        "Done in 5 min 12 s.",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::{listable, Policy};

    #[test]
    fn demo_machine_passes_through_the_real_filter() {
        let all = disks();
        let (shown, hidden) = listable(&all, &Policy::default(), &[]);
        // System NVMe, internal SATA, the USB SSD and nothing else are hidden.
        assert_eq!(hidden, 3);
        let names: Vec<String> = shown.iter().map(|l| l.disk.name()).collect();
        assert_eq!(
            names,
            [
                "Kestrel Swift 3.2",
                "Northwind Pocket Drive",
                "Tarn Card Reader"
            ]
        );
        assert!(shown[2].blocked.is_some());
        let p = Policy {
            show_usb_hdd: true,
            max_size: 512 * crate::safety::GIB,
        };
        assert_eq!(listable(&all, &p, &[]).0.len(), 4);
    }

    #[test]
    fn demo_images_get_sensible_advice() {
        use crate::model::WriteMode;
        use crate::plan::{advise, Env, WimPlan};
        let env = Env {
            dism_available: true,
        };
        let a = advise(&windows_image(), &env);
        assert_eq!(a.recommended, Some(WriteMode::Files));
        assert_eq!(a.wim, WimPlan::Split { parts: 2 });
        let a = advise(&linux_image(), &env);
        assert_eq!(a.recommended, Some(WriteMode::Image));
        let a = advise(&disk_image(), &env);
        assert_eq!(a.recommended, Some(WriteMode::Image));
    }
}
