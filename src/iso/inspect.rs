//! Image inspection: everything the planner and the UI want to know about
//! an image file, gathered from directory structures and a few kilobytes of
//! headers (no file data is scanned).

use super::{fatmini, hybrid, read_bytes, wim, Entry, IsoImage};
use crate::blockdev::{FileSource, ReadAt};
use crate::error::{Error, Result};
use crate::model::{Arch, BootPlatform, ImageInfo, ImageKind, WindowsInfo};
use std::path::Path;

const TRUNCATED_NOTE: &str =
    "The file is smaller than the volume it describes; the download may be incomplete.";

/// Inspects the image file at `path`.
///
/// Only fails when the file cannot be opened or read at all; an unrecognised
/// or damaged image gives `ImageKind::Unknown` (or what could be recognised)
/// with notes.
pub fn inspect(path: &Path) -> Result<ImageInfo> {
    let src =
        FileSource::open(path).map_err(|e| Error::io(format!("Opening {}", path.display()), e))?;
    inspect_source(path, Box::new(src))
}

/// Same as [`inspect`] for an already opened source; `path` is only recorded
/// in the result.
pub fn inspect_source(path: &Path, mut src: Box<dyn ReadAt>) -> Result<ImageInfo> {
    let size = src.size();
    let mut info = ImageInfo::new(path.to_path_buf(), size);
    if size == 0 {
        info.notes.push("The file is empty.".to_string());
        return Ok(info);
    }

    let mut head = [0u8; 512];
    let head_len = size.min(512) as usize;
    read_bytes(
        src.as_mut(),
        0,
        &mut head[..head_len],
        "the start of the file",
    )?;
    if let Some(format) = compressed_format(&head[..head_len]) {
        info.notes.push(format!(
            "This is a compressed file ({format}); it must be decompressed before it can be written."
        ));
        return Ok(info);
    }
    if size < super::SECTOR_SIZE {
        info.notes
            .push("The file is shorter than one sector; it cannot be a disk image.".to_string());
        return Ok(info);
    }

    let tables = hybrid::detect(src.as_mut());
    info.has_mbr = tables.has_mbr;
    info.has_gpt = tables.has_gpt;
    info.partitions = tables.partitions;
    let volume_boot_sector = hybrid::boot_sector_fs(&head);
    let looks_like_iso = looks_like_iso(src.as_mut());

    match IsoImage::open(src) {
        Ok(image) => {
            info.kind = ImageKind::Iso;
            fill_from_iso(&mut info, &image);
        }
        Err(e) => {
            if looks_like_iso {
                info.kind = ImageKind::Iso;
                info.notes.push(e.to_string());
                if matches!(e, Error::Io(..)) {
                    return Err(e);
                }
            } else if info.has_mbr || info.has_gpt || volume_boot_sector.is_some() {
                info.kind = ImageKind::DiskImage;
                if let Some(fs) = volume_boot_sector {
                    info.notes.push(format!(
                        "The image is a single {fs} volume without a partition table."
                    ));
                }
                let end = info
                    .partitions
                    .iter()
                    .filter(|p| p.kind != "GPT protective")
                    .map(|p| p.offset.saturating_add(p.size))
                    .max()
                    .unwrap_or(0);
                if end > size {
                    info.notes.push(TRUNCATED_NOTE.to_string());
                }
            } else {
                info.notes.push(
                    "The file is not a recognised ISO or disk image; it can only be written as it is."
                        .to_string(),
                );
            }
        }
    }
    Ok(info)
}

/// Names the compression or archive container a file starts with.
fn compressed_format(head: &[u8]) -> Option<&'static str> {
    let is = |magic: &[u8]| head.starts_with(magic);
    if is(&[0x1F, 0x8B]) {
        Some("gzip")
    } else if is(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        Some("xz")
    } else if is(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Some("zstd")
    } else if is(b"PK\x03\x04") || is(b"PK\x05\x06") {
        Some("zip")
    } else if is(b"BZh") && head.len() > 3 && (b'1'..=b'9').contains(&head[3]) {
        Some("bzip2")
    } else if is(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
        Some("7z")
    } else {
        None
    }
}

/// True when sector 16 carries an ISO 9660 or UDF recognition descriptor.
fn looks_like_iso(src: &mut dyn ReadAt) -> bool {
    let mut id = [0u8; 5];
    if read_bytes(src, 16 * 2048 + 1, &mut id, "volume descriptor").is_err() {
        return false;
    }
    matches!(&id, b"CD001" | b"BEA01" | b"NSR02" | b"NSR03")
}

fn find_file<'a>(image: &'a IsoImage, path: &str) -> Option<&'a Entry> {
    image.find(path).filter(|e| !e.is_dir)
}

/// Architecture named by an `EFI/BOOT` loader file name (upper case).
fn arch_from_loader_name(name: &str) -> Option<Arch> {
    match name {
        "BOOTX64.EFI" => Some(Arch::X64),
        "BOOTIA32.EFI" => Some(Arch::X86),
        "BOOTAA64.EFI" => Some(Arch::Arm64),
        "BOOTARM.EFI" => Some(Arch::Arm),
        "BOOTRISCV64.EFI" => Some(Arch::RiscV64),
        "BOOTIA64.EFI" => Some(Arch::Ia64),
        _ => None,
    }
}

/// Architecture from the machine field of a PE file, read from its headers.
fn pe_arch(image: &IsoImage, entry: &Entry) -> Option<Arch> {
    let mut dos = [0u8; 64];
    image.read_at(entry, 0, &mut dos).ok()?;
    if &dos[0..2] != b"MZ" {
        return None;
    }
    let pe_at = super::le32(&dos, 0x3C) as u64;
    let mut pe = [0u8; 6];
    image.read_at(entry, pe_at, &mut pe).ok()?;
    if &pe[0..4] != b"PE\0\0" {
        return None;
    }
    match super::le16(&pe, 4) {
        0x014C => Some(Arch::X86),
        0x8664 => Some(Arch::X64),
        0x01C0 | 0x01C2 | 0x01C4 => Some(Arch::Arm),
        0xAA64 => Some(Arch::Arm64),
        0x5064 => Some(Arch::RiscV64),
        0x0200 => Some(Arch::Ia64),
        _ => None,
    }
}

fn add_arch(list: &mut Vec<Arch>, arch: Arch) {
    if !list.contains(&arch) {
        list.push(arch);
    }
}

fn read_wim(image: &IsoImage, entry: &Entry) -> Result<wim::WimInfo> {
    wim::read_wim_info(entry.size, &mut |offset, buf| {
        image.read_at(entry, offset, buf)
    })
}

/// Top-level names (lower case) that mark a Linux live or installer image.
const LINUX_MARKERS: [&str; 22] = [
    "isolinux",
    "syslinux",
    "boot/grub",
    "boot/grub2",
    "boot/isolinux",
    "boot/syslinux",
    "casper",
    "live",
    "liveos",
    "arch",
    ".disk",
    "install.amd",
    "install.386",
    "install.a64",
    "dists",
    "pool",
    "images/pxeboot",
    "loader/entries",
    "antix",
    "manjaro",
    "nixos",
    "boot/x86_64/loader",
];

fn fill_from_iso(info: &mut ImageInfo, image: &IsoImage) {
    info.label = image.label().to_string();
    info.has_iso9660 = image.has_iso9660();
    info.has_joliet = image.has_joliet();
    info.has_rock_ridge = image.has_rock_ridge();
    info.udf = image.udf_revision().map(|s| s.to_string());
    info.boot_entries = image.boot_entries().to_vec();
    info.notes.extend(image.notes().iter().cloned());

    for e in image.entries() {
        if e.is_dir {
            continue;
        }
        info.file_count += 1;
        info.total_file_bytes = info.total_file_bytes.saturating_add(e.size);
        if info
            .largest_file
            .as_ref()
            .map(|(_, s)| e.size > *s)
            .unwrap_or(true)
        {
            info.largest_file = Some((e.path.clone(), e.size));
        }
    }

    if image
        .volume_size()
        .map(|v| v > image.image_size())
        .unwrap_or(false)
    {
        info.notes.push(TRUNCATED_NOTE.to_string());
    }

    // UEFI loaders stored as plain files.
    let mut archs: Vec<Arch> = Vec::new();
    for e in image.entries() {
        if e.is_dir {
            continue;
        }
        let lower = e.path.to_lowercase();
        let Some(name) = lower.strip_prefix("efi/boot/") else {
            continue;
        };
        if name.contains('/') || !name.starts_with("boot") || !name.ends_with(".efi") {
            continue;
        }
        let name = name.to_uppercase();
        let by_name = arch_from_loader_name(&name);
        let by_header = pe_arch(image, e);
        match (by_name, by_header) {
            (Some(n), Some(h)) if n != h => {
                info.notes.push(format!(
                    "EFI/BOOT/{name} is built for {} although its name says {}.",
                    h.label(),
                    n.label()
                ));
                add_arch(&mut archs, h);
            }
            (_, Some(h)) => add_arch(&mut archs, h),
            (Some(n), None) => add_arch(&mut archs, n),
            (None, None) => {}
        }
        if !info.efi_loaders.contains(&name) {
            info.efi_loaders.push(name);
        }
    }

    // UEFI boot only through the El Torito EFI image.
    if info.efi_loaders.is_empty() {
        let efi_entry = info
            .boot_entries
            .iter()
            .find(|b| b.platform == BootPlatform::Uefi && b.lba != 0);
        if let Some(b) = efi_entry {
            let base = b.lba as u64 * 2048;
            match image.with_source(|s| fatmini::efi_boot_files(s, base)) {
                Ok(files) if !files.is_empty() => {
                    for f in &files {
                        if let Some(a) = arch_from_loader_name(f) {
                            add_arch(&mut archs, a);
                        }
                    }
                    info.notes.push(format!(
                        "UEFI boot files exist only inside the embedded EFI image ({}); copying the files alone will not give a UEFI-bootable drive.",
                        files.join(", ")
                    ));
                }
                Ok(_) => info.notes.push(
                    "The image has a UEFI boot entry but no EFI/BOOT loader was found in it."
                        .to_string(),
                ),
                Err(_) => info.notes.push(
                    "The image has a UEFI boot entry whose EFI image could not be read as a FAT volume."
                        .to_string(),
                ),
            }
        }
    }

    // Windows install image.
    let install = [
        "sources/install.wim",
        "sources/install.esd",
        "sources/install.swm",
    ]
    .into_iter()
    .find_map(|p| find_file(image, p));
    let boot_wim = find_file(image, "sources/boot.wim");
    if let Some(e) = install {
        let lower = e.path.to_lowercase();
        let mut w = WindowsInfo {
            install_path: e.path.clone(),
            install_size: e.size,
            is_esd: lower.ends_with(".esd"),
            editions: Vec::new(),
            version: String::new(),
            over_4g: e.size > 0xFFFF_FFFF,
            is_setup: boot_wim.is_some()
                && ["bootmgr", "bootmgr.efi", "setup.exe"]
                    .into_iter()
                    .any(|p| find_file(image, p).is_some()),
        };
        match read_wim(image, e) {
            Ok(meta) => {
                for img in &meta.images {
                    if !img.name.is_empty() && !w.editions.contains(&img.name) {
                        w.editions.push(img.name.clone());
                    }
                }
                w.version = meta
                    .images
                    .iter()
                    .map(|i| i.version.clone())
                    .find(|v| !v.is_empty())
                    .unwrap_or_default();
                for a in meta.archs() {
                    add_arch(&mut archs, a);
                }
            }
            Err(err) => info.notes.push(format!(
                "The editions in {} could not be listed ({}).",
                e.path,
                err.to_string().trim_end_matches('.')
            )),
        }
        info.windows = Some(w);
        info.family = "Windows setup".to_string();
    } else if let Some(e) = boot_wim {
        info.family = "Windows PE / recovery".to_string();
        if let Ok(meta) = read_wim(image, e) {
            for a in meta.archs() {
                add_arch(&mut archs, a);
            }
        }
    } else if LINUX_MARKERS
        .iter()
        .any(|m| image.find(m).map(|e| e.is_dir).unwrap_or(false))
    {
        info.family = "Linux live/installer".to_string();
    }

    archs.sort();
    info.archs = archs;
}
