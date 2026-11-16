//! Types shared between the device layer, the image inspector, the engine and the UI.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// --------------------------------------------------------------------------------------
// Devices
// --------------------------------------------------------------------------------------

/// How a disk is attached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bus {
    Usb,
    /// SD / MMC card readers.
    Sd,
    Sata,
    Nvme,
    Ata,
    Scsi,
    /// RAID, SAS, iSCSI, Fibre Channel, Storage Spaces and the like.
    Raid,
    /// Virtual disks (VHD/VHDX, file-backed).
    Virtual,
    /// 1394, or anything the system reports that is not listed here.
    Other(String),
    Unknown,
}

impl Bus {
    pub fn label(&self) -> &str {
        match self {
            Bus::Usb => "USB",
            Bus::Sd => "SD card",
            Bus::Sata => "SATA",
            Bus::Nvme => "NVMe",
            Bus::Ata => "ATA",
            Bus::Scsi => "SCSI",
            Bus::Raid => "RAID",
            Bus::Virtual => "Virtual",
            Bus::Other(s) => s.as_str(),
            Bus::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scheme {
    Mbr,
    Gpt,
}

impl Scheme {
    pub fn label(self) -> &'static str {
        match self {
            Scheme::Mbr => "MBR",
            Scheme::Gpt => "GPT",
        }
    }
}

/// One partition of a disk, with the volume Windows mounted on it (if any).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Partition {
    /// 1-based partition number.
    pub number: u32,
    pub offset: u64,
    pub size: u64,
    /// Human description of the partition type ("EFI System", "Basic data", "FAT32 (0x0C)").
    pub kind: String,
    /// Drive letter, upper case, when one is assigned.
    pub letter: Option<char>,
    /// Volume label ("" when none or not mounted).
    pub label: String,
    /// File system name as reported by Windows ("FAT32", "NTFS", "exFAT", "" when unknown).
    pub fs: String,
}

/// A physical disk as enumerated (read-only) from the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    /// N in `\\.\PhysicalDriveN`.
    pub number: u32,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub bus: Bus,
    /// The device reports removable media (typical USB sticks and card readers).
    pub removable: bool,
    pub size: u64,
    pub sector_size: u32,
    /// `None` for an uninitialised / unpartitioned disk.
    pub scheme: Option<Scheme>,
    pub partitions: Vec<Partition>,
    /// Holds the Windows folder, the boot/system partition, a page file or a
    /// hibernation file. Such a disk is never written.
    pub is_system: bool,
    /// Holds the running BootForge executable.
    pub holds_program: bool,
    pub read_only: bool,
    pub offline: bool,
}

impl Disk {
    /// "Vendor Model", tidied.
    pub fn name(&self) -> String {
        let v = self.vendor.trim();
        let m = self.model.trim();
        let s = if v.is_empty() || m.to_lowercase().starts_with(&v.to_lowercase()) {
            m.to_string()
        } else {
            format!("{v} {m}")
        };
        if s.is_empty() {
            format!("Disk {}", self.number)
        } else {
            s
        }
    }

    /// Drive letters on this disk, in partition order.
    pub fn letters(&self) -> Vec<char> {
        self.partitions.iter().filter_map(|p| p.letter).collect()
    }

    pub fn identity(&self) -> DiskIdentity {
        DiskIdentity {
            number: self.number,
            serial: self.serial.clone(),
            model: self.model.clone(),
            size: self.size,
        }
    }
}

/// What must still be true about a disk at the moment of the first write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskIdentity {
    pub number: u32,
    pub serial: String,
    pub model: String,
    pub size: u64,
}

// --------------------------------------------------------------------------------------
// Images
// --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    /// An ISO 9660 / UDF optical image.
    Iso,
    /// A raw disk image with its own partition table or boot sector.
    DiskImage,
    /// Not recognised; can only be written raw.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Arch {
    X86,
    X64,
    Arm,
    Arm64,
    Ia64,
    RiscV64,
}

impl Arch {
    pub fn label(self) -> &'static str {
        match self {
            Arch::X86 => "x86 (32-bit)",
            Arch::X64 => "x64",
            Arch::Arm => "ARM (32-bit)",
            Arch::Arm64 => "ARM64",
            Arch::Ia64 => "Itanium",
            Arch::RiscV64 => "RISC-V 64",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootPlatform {
    /// El Torito platform 0x00 (x86 BIOS).
    Bios,
    /// El Torito platform 0xEF.
    Uefi,
    /// PowerPC, Mac or an unknown platform id.
    Other(u8),
}

/// One El Torito boot catalog entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootEntry {
    pub platform: BootPlatform,
    pub bootable: bool,
    /// "no emulation", "1.44 MB floppy", "hard disk", ...
    pub emulation: String,
    /// LBA of the boot image in 2048-byte sectors.
    pub lba: u32,
    /// Size of the boot image in 512-byte sectors, as recorded in the catalog.
    pub sectors: u32,
}

/// An entry of an MBR or GPT found inside the image itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePartition {
    pub kind: String,
    pub offset: u64,
    pub size: u64,
    pub bootable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WindowsInfo {
    /// Path of the install image inside the ISO, e.g. `sources/install.wim`.
    pub install_path: String,
    pub install_size: u64,
    /// install.esd (solid compression; cannot be split by DISM).
    pub is_esd: bool,
    /// Edition names from the WIM XML ("Windows 11 Pro", ...). Empty when not read.
    pub editions: Vec<String>,
    /// Version text from the WIM XML when available ("10.0.26100").
    pub version: String,
    /// True when install.wim/esd is 4 GiB or larger and so cannot be stored on FAT32.
    pub over_4g: bool,
    /// Has `sources/boot.wim` and `bootmgr`-style setup files (a setup disc
    /// rather than e.g. a recovery or PE disc).
    pub is_setup: bool,
}

/// What the inspector found out about an image file. Facts only: what to do
/// with them is decided in `plan`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub path: PathBuf,
    pub size: u64,
    pub kind: ImageKind,
    /// Volume label (Joliet/UDF label preferred over the ISO 9660 one).
    pub label: String,
    pub has_iso9660: bool,
    pub has_joliet: bool,
    pub has_rock_ridge: bool,
    /// UDF revision as text ("1.02", "2.50"), `None` when there is no UDF.
    pub udf: Option<String>,
    /// A valid MBR (0x55AA with at least one partition entry) sits in sector 0.
    pub has_mbr: bool,
    /// A GPT header ("EFI PART") sits in sector 1 (512-byte sectors).
    pub has_gpt: bool,
    /// Partitions of that MBR/GPT.
    pub partitions: Vec<ImagePartition>,
    /// El Torito entries, in catalog order.
    pub boot_entries: Vec<BootEntry>,
    /// UEFI boot loaders found in `/EFI/BOOT` (file names such as `BOOTX64.EFI`),
    /// or inside the El Torito EFI image when that could be read.
    pub efi_loaders: Vec<String>,
    pub archs: Vec<Arch>,
    pub windows: Option<WindowsInfo>,
    /// Short family guess for display ("Windows setup", "Linux live/installer", "").
    pub family: String,
    pub file_count: u64,
    /// Sum of all file sizes in the image's file system.
    pub total_file_bytes: u64,
    /// Largest file: path and size.
    pub largest_file: Option<(String, u64)>,
    /// Things worth telling the user (truncated image, odd structures, ...).
    pub notes: Vec<String>,
}

impl ImageInfo {
    /// An empty report for `path`.
    pub fn new(path: PathBuf, size: u64) -> ImageInfo {
        ImageInfo {
            path,
            size,
            kind: ImageKind::Unknown,
            label: String::new(),
            has_iso9660: false,
            has_joliet: false,
            has_rock_ridge: false,
            udf: None,
            has_mbr: false,
            has_gpt: false,
            partitions: Vec::new(),
            boot_entries: Vec::new(),
            efi_loaders: Vec::new(),
            archs: Vec::new(),
            windows: None,
            family: String::new(),
            file_count: 0,
            total_file_bytes: 0,
            largest_file: None,
            notes: Vec::new(),
        }
    }

    /// An ISO that also carries a partition table, so it boots when written raw.
    pub fn is_hybrid(&self) -> bool {
        self.kind == ImageKind::Iso && (self.has_mbr || self.has_gpt)
    }
    pub fn bios_el_torito(&self) -> bool {
        self.boot_entries
            .iter()
            .any(|e| e.bootable && e.platform == BootPlatform::Bios)
    }
    pub fn uefi_el_torito(&self) -> bool {
        self.boot_entries
            .iter()
            .any(|e| e.bootable && e.platform == BootPlatform::Uefi)
    }
    /// Has `/EFI/BOOT/BOOT*.EFI` as plain files, so copying the files to a
    /// FAT32 partition gives a UEFI-bootable drive.
    pub fn uefi_file_bootable(&self) -> bool {
        !self.efi_loaders.is_empty()
    }
}

// --------------------------------------------------------------------------------------
// Options
// --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteMode {
    /// Copy the image to the drive byte for byte.
    Image,
    /// Partition, format FAT32 and copy the files out of the ISO.
    Files,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetSystem {
    Uefi,
    /// Legacy BIOS or UEFI with CSM.
    Bios,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileSystem {
    Fat32,
    ExFat,
}

impl FileSystem {
    pub fn label(self) -> &'static str {
        match self {
            FileSystem::Fat32 => "FAT32",
            FileSystem::ExFat => "exFAT",
        }
    }
}

/// Everything the user chose for one write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteOptions {
    pub mode: WriteMode,
    pub scheme: Scheme,
    pub target: TargetSystem,
    pub fs: FileSystem,
    /// Cluster size in bytes; `None` picks the default for the partition size.
    pub cluster_size: Option<u32>,
    pub label: String,
    /// Quick format: only the file-system structures are written.
    /// Otherwise the whole partition is zeroed first.
    pub quick_format: bool,
    /// Destructive read/write surface test before writing.
    pub bad_block_check: bool,
    /// Read everything back and compare.
    pub verify: bool,
    /// Safely remove the drive when done.
    pub eject: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            mode: WriteMode::Image,
            scheme: Scheme::Gpt,
            target: TargetSystem::Uefi,
            fs: FileSystem::Fat32,
            cluster_size: None,
            label: String::new(),
            quick_format: true,
            bad_block_check: false,
            verify: true,
            eject: false,
        }
    }
}

// --------------------------------------------------------------------------------------
// Progress and cancellation
// --------------------------------------------------------------------------------------

/// A shared "please stop" flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    /// `Err(Error::Cancelled)` once cancellation was requested.
    pub fn check(&self) -> crate::error::Result<()> {
        if self.is_cancelled() {
            Err(crate::error::Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Byte-progress callback used by the low-level operations: called with the
/// number of bytes completed so far within the current operation (monotonic,
/// ending at the operation's total). Called often; keep it cheap.
pub type ProgressFn<'a> = &'a mut dyn FnMut(u64);

/// The three checksums shown for an image, lower-case hex.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Checksums {
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_name_avoids_repeating_the_vendor() {
        let mut d = Disk {
            number: 3,
            vendor: "Kestrel".into(),
            model: "Kestrel Flash 64".into(),
            serial: String::new(),
            bus: Bus::Usb,
            removable: true,
            size: 0,
            sector_size: 512,
            scheme: None,
            partitions: vec![],
            is_system: false,
            holds_program: false,
            read_only: false,
            offline: false,
        };
        assert_eq!(d.name(), "Kestrel Flash 64");
        d.model = "Flash 64".into();
        assert_eq!(d.name(), "Kestrel Flash 64");
        d.vendor.clear();
        d.model.clear();
        assert_eq!(d.name(), "Disk 3");
    }

    #[test]
    fn cancel_flag() {
        let c = Cancel::new();
        assert!(c.check().is_ok());
        c.clone().cancel();
        assert!(c.check().unwrap_err().is_cancelled());
    }
}
