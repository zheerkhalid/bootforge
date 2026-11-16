//! Turns the facts about an image into advice: which write modes are
//! possible, which one is recommended and why, and what would stop a write.

use crate::model::{
    Arch, Disk, FileSystem, ImageInfo, ImageKind, Scheme, TargetSystem, WriteMode, WriteOptions,
};
use crate::settings::Settings;
use crate::util::human_size;

/// Largest file FAT32 can hold.
pub const FAT32_MAX_FILE: u64 = 0xFFFF_FFFF;
/// Size of the .swm parts an oversized install.wim is split into.
pub const SWM_PART_MB: u32 = 3800;
const MIB: u64 = 1024 * 1024;

/// Facts about this PC that influence the advice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Env {
    /// `Dism.exe` was found in the Windows folder.
    pub dism_available: bool,
}

/// What happens to `sources\install.wim` in file-copy mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WimPlan {
    /// Not a Windows image, or nothing special to do.
    NotNeeded,
    /// Larger than FAT32 allows: split into this many .swm parts with DISM.
    Split { parts: u32 },
    /// install.esd over 4 GiB: DISM cannot split it.
    EsdTooBig,
    /// Would need splitting, but DISM is not available.
    NoDism,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Good,
    Info,
    Warn,
    Neutral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Badge {
    pub text: String,
    pub tone: Tone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advice {
    /// One line saying what the image is.
    pub headline: String,
    pub badges: Vec<Badge>,
    /// Label/value rows for the details panel.
    pub facts: Vec<(String, String)>,
    /// `Err(reason)` when the image cannot be written raw.
    pub image_mode: Result<(), String>,
    /// `Err(reason)` when file-copy mode is not possible.
    pub files_mode: Result<(), String>,
    /// `None` when neither mode is possible.
    pub recommended: Option<WriteMode>,
    /// Why that mode is recommended (or why nothing is possible).
    pub why: String,
    pub wim: WimPlan,
    /// In image mode: the image carries what UEFI firmware needs.
    pub image_boots_uefi: bool,
    /// In image mode: the image carries what a BIOS needs.
    pub image_boots_bios: bool,
    /// Suggested FAT32 volume label for file-copy mode.
    pub default_label: String,
    pub warnings: Vec<String>,
}

impl Advice {
    pub fn mode_allowed(&self, mode: WriteMode) -> Result<(), String> {
        match mode {
            WriteMode::Image => self.image_mode.clone(),
            WriteMode::Files => self.files_mode.clone(),
        }
    }
}

/// A FAT32-safe label derived from the image's volume label: upper case,
/// at most 11 characters, only characters FAT allows.
pub fn fat_label(label: &str) -> String {
    let mut out = String::new();
    for c in label.trim().chars() {
        let c = c.to_ascii_uppercase();
        let ok = c.is_ascii_alphanumeric() || " _-$%'@~`!(){}^#&".contains(c);
        out.push(if ok { c } else { '_' });
        if out.len() == 11 {
            break;
        }
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        "BOOTFORGE".into()
    } else {
        out
    }
}

fn arch_text(archs: &[Arch]) -> String {
    archs
        .iter()
        .map(|a| a.label())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn advise(info: &ImageInfo, env: &Env) -> Advice {
    let mut badges = Vec::new();
    let mut facts: Vec<(String, String)> = Vec::new();
    let mut warnings = Vec::new();
    let badge = |badges: &mut Vec<Badge>, text: &str, tone: Tone| {
        badges.push(Badge {
            text: text.to_string(),
            tone,
        })
    };

    let has_efi_partition = info
        .partitions
        .iter()
        .any(|p| p.kind.to_lowercase().contains("efi"));
    let hybrid = info.is_hybrid();

    // ---- what it is -------------------------------------------------------------
    let headline = match info.kind {
        ImageKind::Iso => {
            let family = if info.family.is_empty() {
                "ISO image".to_string()
            } else {
                format!("{} ISO", info.family)
            };
            if info.archs.is_empty() {
                family
            } else {
                format!("{family} · {}", arch_text(&info.archs))
            }
        }
        ImageKind::DiskImage => "Raw disk image".to_string(),
        ImageKind::Unknown => "Unrecognised file".to_string(),
    };
    match info.kind {
        ImageKind::Iso => {
            if hybrid {
                badge(&mut badges, "Hybrid ISO", Tone::Good);
            } else {
                badge(&mut badges, "ISO", Tone::Neutral);
            }
        }
        ImageKind::DiskImage => badge(&mut badges, "Disk image", Tone::Good),
        ImageKind::Unknown => badge(&mut badges, "Unknown format", Tone::Warn),
    }
    let uefi = info.uefi_el_torito() || info.uefi_file_bootable() || has_efi_partition;
    if uefi {
        badge(&mut badges, "UEFI", Tone::Info);
    }
    if info.bios_el_torito() {
        badge(&mut badges, "BIOS", Tone::Info);
    }
    if let Some(w) = &info.windows {
        badge(
            &mut badges,
            if w.is_setup {
                "Windows setup"
            } else {
                "Windows image"
            },
            Tone::Info,
        );
    }
    for a in &info.archs {
        badge(&mut badges, a.label(), Tone::Neutral);
    }

    facts.push(("Size".into(), human_size(info.size)));
    if !info.label.is_empty() {
        facts.push(("Volume label".into(), info.label.clone()));
    }
    if info.kind == ImageKind::Iso {
        let mut fs = Vec::new();
        if info.has_iso9660 {
            fs.push("ISO 9660".to_string());
        }
        if info.has_joliet {
            fs.push("Joliet".to_string());
        }
        if info.has_rock_ridge {
            fs.push("Rock Ridge".to_string());
        }
        if let Some(v) = &info.udf {
            fs.push(format!("UDF {v}"));
        }
        facts.push(("File systems".into(), fs.join(", ")));
        let table = match (info.has_mbr, info.has_gpt) {
            (true, true) => "MBR and GPT (hybrid)",
            (true, false) => "MBR (hybrid)",
            (false, true) => "GPT (hybrid)",
            (false, false) => "None (optical layout only)",
        };
        facts.push(("Partition table".into(), table.into()));
        let mut boot = Vec::new();
        if info.bios_el_torito() {
            boot.push("BIOS (El Torito)".to_string());
        }
        if info.uefi_el_torito() {
            boot.push("UEFI (El Torito)".to_string());
        }
        if !info.efi_loaders.is_empty() {
            boot.push(format!("UEFI files: {}", info.efi_loaders.join(", ")));
        }
        facts.push((
            "Boot entries".into(),
            if boot.is_empty() {
                "None found".into()
            } else {
                boot.join(" · ")
            },
        ));
        if info.file_count > 0 {
            facts.push((
                "Contents".into(),
                format!(
                    "{} files, {}",
                    info.file_count,
                    human_size(info.total_file_bytes)
                ),
            ));
        }
    } else if info.kind == ImageKind::DiskImage {
        let table = match (info.has_mbr, info.has_gpt) {
            (_, true) => "GPT",
            (true, false) => "MBR",
            _ => "None (single file system)",
        };
        facts.push(("Partition table".into(), table.into()));
        if !info.partitions.is_empty() {
            facts.push((
                "Partitions".into(),
                info.partitions
                    .iter()
                    .map(|p| format!("{} ({})", p.kind, human_size(p.size)))
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
    }
    if let Some(w) = &info.windows {
        facts.push((
            "Install image".into(),
            format!("{} · {}", w.install_path, human_size(w.install_size)),
        ));
        if !w.version.is_empty() {
            facts.push(("Windows version".into(), w.version.clone()));
        }
        if !w.editions.is_empty() {
            facts.push(("Editions".into(), w.editions.join(", ")));
        }
    }
    if !info.archs.is_empty() {
        facts.push(("Architecture".into(), arch_text(&info.archs)));
    }

    // ---- image mode -------------------------------------------------------------
    let image_mode = match info.kind {
        ImageKind::DiskImage => Ok(()),
        ImageKind::Unknown => {
            warnings.push(
                "BootForge does not recognise this file. It can still be written to the drive byte for byte, but only do that if you know it is a disk image."
                    .to_string(),
            );
            Ok(())
        }
        ImageKind::Iso if hybrid => Ok(()),
        ImageKind::Iso => Err(
            "This ISO has no partition table (it is not a hybrid image), so a byte-for-byte copy would not boot from a USB drive."
                .to_string(),
        ),
    };
    let image_boots_uefi =
        info.kind == ImageKind::Iso && hybrid && (has_efi_partition || info.uefi_el_torito())
            || info.kind == ImageKind::DiskImage && has_efi_partition;
    let image_boots_bios = info.kind == ImageKind::Iso && info.has_mbr && info.bios_el_torito();

    // ---- file-copy mode ---------------------------------------------------------
    let mut wim = WimPlan::NotNeeded;
    let files_mode = (|| {
        if info.kind != ImageKind::Iso {
            return Err("File copy needs an ISO image; this file is written as it is.".to_string());
        }
        if !info.uefi_file_bootable() {
            return Err(if info.uefi_el_torito() {
                "The UEFI boot files of this ISO exist only inside an embedded boot image, not as files in EFI\\BOOT, so copying the files would not give a bootable drive.".to_string()
            } else {
                "This ISO has no UEFI boot loader (EFI\\BOOT\\BOOT*.EFI). File copy makes UEFI-bootable drives only; BIOS booting from copied files would need a boot loader that BootForge does not ship.".to_string()
            });
        }
        let install = info.windows.as_ref().map(|w| w.install_path.to_lowercase());
        if let Some((path, size)) = &info.largest_file {
            let is_install = install.as_deref() == Some(path.to_lowercase().as_str());
            if *size > FAT32_MAX_FILE && !is_install {
                return Err(format!(
                    "\"{path}\" is {}, more than a FAT32 volume can hold in one file (4 GB).",
                    human_size(*size)
                ));
            }
        }
        if let Some(w) = &info.windows {
            if w.install_size > FAT32_MAX_FILE {
                if w.is_esd {
                    wim = WimPlan::EsdTooBig;
                    return Err(format!(
                        "{} is {}; FAT32 holds at most 4 GB per file and an .esd image cannot be split.",
                        w.install_path,
                        human_size(w.install_size)
                    ));
                }
                if !env.dism_available {
                    wim = WimPlan::NoDism;
                    return Err(format!(
                        "{} is {}; it has to be split for FAT32, and the Windows DISM tool needed for that was not found on this PC.",
                        w.install_path,
                        human_size(w.install_size)
                    ));
                }
                let part = SWM_PART_MB as u64 * MIB;
                wim = WimPlan::Split {
                    parts: w.install_size.div_ceil(part) as u32,
                };
            }
        }
        Ok(())
    })();
    if let WimPlan::Split { parts } = wim {
        if let Some(w) = &info.windows {
            warnings.push(format!(
                "{} is {}, too large for FAT32. In file-copy mode it is split into about {parts} parts (install.swm, install2.swm, …) with the DISM tool of this PC; Windows Setup reads them natively. This needs about {} of free temporary space.",
                w.install_path,
                human_size(w.install_size),
                human_size(w.install_size * 2)
            ));
        }
    }

    // ---- recommendation ---------------------------------------------------------
    let (recommended, why) = match (&image_mode, &files_mode) {
        (_, Ok(())) if info.windows.is_some() => (
            Some(WriteMode::Files),
            "Windows images are not hybrid: the files are copied to a FAT32 partition that UEFI firmware can boot.".to_string(),
        ),
        (Ok(()), _) if info.kind == ImageKind::Iso => (
            Some(WriteMode::Image),
            "This is a hybrid ISO: a byte-for-byte copy gives exactly the drive its makers intended.".to_string(),
        ),
        (Ok(()), _) => (
            Some(WriteMode::Image),
            "Disk images are written to the drive byte for byte.".to_string(),
        ),
        (Err(_), Ok(())) => (
            Some(WriteMode::Files),
            "The ISO is not hybrid but contains UEFI boot files, so its files are copied to a FAT32 partition.".to_string(),
        ),
        (Err(a), Err(b)) => (None, format!("{a} {b}")),
    };

    let default_label = fat_label(&info.label);
    if info.kind == ImageKind::Iso
        && info.windows.is_none()
        && files_mode.is_ok()
        && info.label.trim().len() > 11
    {
        warnings.push(format!(
            "The volume label \"{}\" is longer than the 11 characters FAT32 allows. Some Linux images find their files by label and will not start from a file copy; prefer image mode for them.",
            info.label.trim()
        ));
    }
    warnings.extend(info.notes.iter().cloned());

    Advice {
        headline,
        badges,
        facts,
        image_mode,
        files_mode,
        recommended,
        why,
        wim,
        image_boots_uefi,
        image_boots_bios,
        default_label,
        warnings,
    }
}

/// The options a fresh selection starts with.
pub fn default_options(advice: &Advice, settings: &Settings) -> WriteOptions {
    let mode = advice.recommended.unwrap_or(WriteMode::Image);
    WriteOptions {
        mode,
        scheme: Scheme::Gpt,
        target: TargetSystem::Uefi,
        fs: FileSystem::Fat32,
        cluster_size: None,
        label: advice.default_label.clone(),
        quick_format: settings.quick_format,
        bad_block_check: false,
        verify: settings.verify,
        eject: settings.eject,
    }
}

/// Bytes the drive must have for this image and these options.
pub fn required_bytes(info: &ImageInfo, opts: &WriteOptions) -> u64 {
    match opts.mode {
        WriteMode::Image => info.size,
        WriteMode::Files => {
            // Files, slack of one 32 KiB cluster per file and directory entry
            // overhead, the two FATs, reserved areas and partition alignment.
            let data = info.total_file_bytes + info.file_count * 32 * 1024;
            data + data / 2048 + 48 * MIB
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    /// Blocking issues disable the Write button.
    pub blocking: bool,
    pub text: String,
}

/// Everything wrong with the current combination of image, options and drive.
pub fn check_options(
    info: &ImageInfo,
    advice: &Advice,
    opts: &WriteOptions,
    disk: Option<&Disk>,
) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut block = |text: String| {
        issues.push(Issue {
            blocking: true,
            text,
        })
    };
    if let Err(reason) = advice.mode_allowed(opts.mode) {
        block(reason);
    }
    if opts.mode == WriteMode::Files {
        if opts.target == TargetSystem::Bios {
            block("File copy makes UEFI-bootable drives only. For BIOS/CSM booting use image mode with a hybrid ISO.".to_string());
        }
        if opts.fs != FileSystem::Fat32 {
            block("UEFI firmware boots from FAT32 only; file copy always uses FAT32.".to_string());
        }
    }
    if let Some(d) = disk {
        let need = required_bytes(info, opts);
        if d.size > 0 && need > d.size {
            block(format!(
                "The drive is too small: about {} is needed, the drive has {}.",
                human_size(need),
                human_size(d.size)
            ));
        }
        if opts.mode == WriteMode::Files
            && opts.scheme == Scheme::Mbr
            && d.size > (u32::MAX as u64) * d.sector_size.max(512) as u64
        {
            block("The drive is too large for an MBR partition table; choose GPT.".to_string());
        }
    }
    let mut warn = |text: String| {
        issues.push(Issue {
            blocking: false,
            text,
        })
    };
    if opts.mode == WriteMode::Image {
        match opts.target {
            TargetSystem::Bios if !advice.image_boots_bios && info.kind == ImageKind::Iso => warn(
                "This image has no BIOS boot entry; it will probably start only on UEFI systems.".to_string(),
            ),
            TargetSystem::Uefi if !advice.image_boots_uefi && info.kind == ImageKind::Iso => warn(
                "This image has no UEFI boot entry; it will probably start only on BIOS/CSM systems.".to_string(),
            ),
            _ => {}
        }
    }
    if opts.bad_block_check {
        warn("The bad-block check writes and reads the whole drive once before the real write; on large or slow drives this can take hours.".to_string());
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootEntry, BootPlatform, ImagePartition, WindowsInfo};
    use std::path::PathBuf;

    fn entry(platform: BootPlatform) -> BootEntry {
        BootEntry {
            platform,
            bootable: true,
            emulation: "no emulation".into(),
            lba: 100,
            sectors: 4,
        }
    }

    fn linux_hybrid() -> ImageInfo {
        let mut i = ImageInfo::new(PathBuf::from("X:\\images\\nimbus.iso"), 3 * 1024 * MIB);
        i.kind = ImageKind::Iso;
        i.label = "NIMBUS_24".into();
        i.has_iso9660 = true;
        i.has_joliet = true;
        i.has_mbr = true;
        i.partitions = vec![ImagePartition {
            kind: "EFI System".into(),
            offset: 0,
            size: 4 * MIB,
            bootable: false,
        }];
        i.boot_entries = vec![entry(BootPlatform::Bios), entry(BootPlatform::Uefi)];
        i.efi_loaders = vec!["BOOTX64.EFI".into()];
        i.archs = vec![Arch::X64];
        i.family = "Linux live/installer".into();
        i.file_count = 900;
        i.total_file_bytes = 2900 * MIB;
        i.largest_file = Some(("casper/filesystem.squashfs".into(), 2500 * MIB));
        i
    }

    fn windows(install_size: u64, esd: bool) -> ImageInfo {
        let mut i = ImageInfo::new(
            PathBuf::from("X:\\images\\win.iso"),
            install_size + 900 * MIB,
        );
        i.kind = ImageKind::Iso;
        i.label = "CCCOMA_X64FRE_EN-US_DV9".into();
        i.has_iso9660 = true;
        i.udf = Some("1.02".into());
        i.boot_entries = vec![entry(BootPlatform::Bios), entry(BootPlatform::Uefi)];
        i.efi_loaders = vec!["bootx64.efi".into()];
        i.archs = vec![Arch::X64];
        i.family = "Windows setup".into();
        let path = if esd {
            "sources/install.esd"
        } else {
            "sources/install.wim"
        };
        i.windows = Some(WindowsInfo {
            install_path: path.into(),
            install_size,
            is_esd: esd,
            editions: vec!["Windows 11 Home".into(), "Windows 11 Pro".into()],
            version: "10.0.26100".into(),
            over_4g: install_size > FAT32_MAX_FILE,
            is_setup: true,
        });
        i.file_count = 950;
        i.total_file_bytes = install_size + 800 * MIB;
        i.largest_file = Some((path.into(), install_size));
        i
    }

    const DISM: Env = Env {
        dism_available: true,
    };

    #[test]
    fn hybrid_linux_prefers_image_mode_and_allows_both() {
        let a = advise(&linux_hybrid(), &DISM);
        assert_eq!(a.recommended, Some(WriteMode::Image));
        assert!(a.image_mode.is_ok() && a.files_mode.is_ok());
        assert!(a.image_boots_bios && a.image_boots_uefi);
        assert_eq!(a.default_label, "NIMBUS_24");
        assert!(a.badges.iter().any(|b| b.text == "Hybrid ISO"));
    }

    #[test]
    fn windows_setup_uses_file_copy_and_refuses_raw() {
        let a = advise(&windows(3000 * MIB, false), &DISM);
        assert_eq!(a.recommended, Some(WriteMode::Files));
        assert!(a.image_mode.is_err());
        assert_eq!(a.wim, WimPlan::NotNeeded);
        assert_eq!(a.default_label, "CCCOMA_X64F");
    }

    #[test]
    fn big_wim_is_split_when_dism_exists() {
        let info = windows(5500 * MIB, false);
        let a = advise(&info, &DISM);
        assert_eq!(a.wim, WimPlan::Split { parts: 2 });
        assert!(a.files_mode.is_ok());
        assert!(a.warnings.iter().any(|w| w.contains("split")));
        let a = advise(&info, &Env::default());
        assert_eq!(a.wim, WimPlan::NoDism);
        assert!(a.files_mode.is_err());
        assert_eq!(a.recommended, None);
    }

    #[test]
    fn big_esd_cannot_be_written() {
        let a = advise(&windows(4200 * MIB, true), &DISM);
        assert_eq!(a.wim, WimPlan::EsdTooBig);
        assert_eq!(a.recommended, None);
        assert!(a.why.contains("cannot be split"));
        // Exactly at the limit still fits.
        let a = advise(&windows(FAT32_MAX_FILE, true), &DISM);
        assert!(a.files_mode.is_ok());
    }

    #[test]
    fn other_oversized_files_block_file_copy() {
        let mut i = linux_hybrid();
        i.largest_file = Some(("live/filesystem.squashfs".into(), 5000 * MIB));
        let a = advise(&i, &DISM);
        assert!(a.files_mode.is_err());
        assert_eq!(a.recommended, Some(WriteMode::Image));
    }

    #[test]
    fn bios_only_optical_iso_has_no_mode() {
        let mut i = linux_hybrid();
        i.has_mbr = false;
        i.partitions.clear();
        i.efi_loaders.clear();
        i.boot_entries = vec![entry(BootPlatform::Bios)];
        let a = advise(&i, &DISM);
        assert_eq!(a.recommended, None);
        assert!(a.image_mode.is_err() && a.files_mode.is_err());
    }

    #[test]
    fn disk_images_and_unknown_files_are_written_raw() {
        let mut i = ImageInfo::new(PathBuf::from("X:\\a.img"), 512 * MIB);
        i.kind = ImageKind::DiskImage;
        i.has_mbr = true;
        let a = advise(&i, &DISM);
        assert_eq!(a.recommended, Some(WriteMode::Image));
        assert!(a.files_mode.is_err());
        i.kind = ImageKind::Unknown;
        let a = advise(&i, &DISM);
        assert_eq!(a.recommended, Some(WriteMode::Image));
        assert!(!a.warnings.is_empty());
    }

    #[test]
    fn option_checks() {
        let info = linux_hybrid();
        let a = advise(&info, &DISM);
        let mut o = default_options(&a, &Settings::default());
        assert_eq!(o.mode, WriteMode::Image);
        assert!(check_options(&info, &a, &o, None).is_empty());
        o.mode = WriteMode::Files;
        o.target = TargetSystem::Bios;
        let issues = check_options(&info, &a, &o, None);
        assert!(issues.iter().any(|i| i.blocking));
        o.target = TargetSystem::Uefi;
        let small = Disk {
            number: 5,
            vendor: String::new(),
            model: "Stick".into(),
            serial: String::new(),
            bus: crate::model::Bus::Usb,
            removable: true,
            size: 2_000_000_000,
            sector_size: 512,
            scheme: None,
            partitions: vec![],
            is_system: false,
            holds_program: false,
            read_only: false,
            offline: false,
        };
        let issues = check_options(&info, &a, &o, Some(&small));
        assert!(issues
            .iter()
            .any(|i| i.blocking && i.text.contains("too small")));
        let win = windows(3000 * MIB, false);
        let a = advise(&win, &DISM);
        let mut o = default_options(&a, &Settings::default());
        o.mode = WriteMode::Image;
        assert!(check_options(&win, &a, &o, None).iter().any(|i| i.blocking));
    }

    #[test]
    fn labels() {
        assert_eq!(fat_label("Ubuntu 24.04 LTS amd64"), "UBUNTU 24_0");
        assert_eq!(fat_label("  "), "BOOTFORGE");
        assert_eq!(fat_label("ärch*"), "_RCH_");
    }
}
