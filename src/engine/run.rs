//! The real work behind a job.
//!
//! Everything destructive is written against `&mut dyn BlockDevice`, so the
//! same code runs on a locked physical drive and, in tests and the self-test,
//! on an image file. Only [`run_real`] touches the Windows device layer.

use super::{Job, Outcome, Reporter, ResetOptions, Stage, Summary};
use crate::blockdev::{BlockDevice, FileSource, ReadAt, SubDevice};
use crate::error::{Error, IoContext, Result};
use crate::iso::IsoImage;
use crate::model::{Cancel, Checksums, FileSystem, Scheme, TargetSystem, WriteMode, WriteOptions};
use crate::part::{self, PartSpec, PartType};
use crate::plan::{FAT32_MAX_FILE, SWM_PART_MB};
use crate::safety::{self, Policy, Refusal};
use crate::util::{human_duration, human_size, human_speed};
use crate::{device, exfat, fat32, imaging, sys};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

const MIB: u64 = 1024 * 1024;
/// Path of the Windows install image inside a setup ISO (compared case-insensitively).
const INSTALL_WIM: &str = "sources/install.wim";

// --------------------------------------------------------------------------------------
// Splitting an oversized install.wim
// --------------------------------------------------------------------------------------

/// Splits a WIM file into parts no larger than `part_mb` MiB.
pub trait WimSplitter {
    /// Returns the parts in order (`install.swm`, `install2.swm`, ...), all in `out_dir`.
    fn split(
        &self,
        wim: &Path,
        out_dir: &Path,
        part_mb: u32,
        cancel: &Cancel,
        on_percent: &mut dyn FnMut(f32),
    ) -> Result<Vec<PathBuf>>;
}

/// The DISM tool that ships with Windows.
pub struct DismSplitter(pub PathBuf);

impl WimSplitter for DismSplitter {
    fn split(
        &self,
        wim: &Path,
        out_dir: &Path,
        part_mb: u32,
        cancel: &Cancel,
        on_percent: &mut dyn FnMut(f32),
    ) -> Result<Vec<PathBuf>> {
        sys::split_wim(&self.0, wim, out_dir, part_mb, cancel, on_percent)
    }
}

/// Knobs of file-copy mode that only tests change.
pub struct FilesParams<'a> {
    /// Largest file the target file system holds (FAT32: 4 GiB - 1).
    pub max_file: u64,
    /// Size of the parts an oversized install.wim is split into, in MiB.
    pub part_mb: u32,
    /// `None`: an oversized install.wim is an error.
    pub splitter: Option<&'a dyn WimSplitter>,
    /// Folder for the extracted .wim and its parts.
    pub work_dir: PathBuf,
}

impl FilesParams<'_> {
    /// Real-world limits, no splitter.
    pub fn standard(work_dir: PathBuf) -> FilesParams<'static> {
        FilesParams {
            max_file: FAT32_MAX_FILE,
            part_mb: SWM_PART_MB,
            splitter: None,
            work_dir,
        }
    }
}

/// Deletes the listed files (and then the folder, if empty) when dropped.
struct TempFiles {
    files: Vec<PathBuf>,
    dir: Option<PathBuf>,
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
        if let Some(d) = &self.dir {
            let _ = std::fs::remove_dir(d);
        }
    }
}

// --------------------------------------------------------------------------------------
// Building blocks
// --------------------------------------------------------------------------------------

fn run_bad_block_check(
    dev: &mut dyn BlockDevice,
    cancel: &Cancel,
    rep: &mut Reporter,
) -> Result<String> {
    rep.stage(Stage::BadBlocks);
    let total = dev.size() * 2;
    let report = imaging::bad_block_check(dev, cancel, &mut |done| rep.progress(done, total))?;
    let text = report.summary();
    rep.log(text.clone());
    if !report.is_clean() {
        return Err(Error::Msg(format!(
            "{text} The drive should not be used; nothing else was written."
        )));
    }
    Ok(text)
}

/// Image mode: copies `src` to the start of the device, byte for byte.
/// Returns summary lines.
pub fn write_image_raw(
    src: &mut dyn ReadAt,
    dev: &mut dyn BlockDevice,
    options: &WriteOptions,
    cancel: &Cancel,
    rep: &mut Reporter,
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let size = src.size();
    if size == 0 {
        return Err(Error::invalid("The image file is empty."));
    }
    if size > dev.size() {
        return Err(Error::Refused(format!(
            "the image ({}) is larger than the drive ({})",
            human_size(size),
            human_size(dev.size())
        )));
    }
    if options.bad_block_check {
        lines.push(run_bad_block_check(dev, cancel, rep)?);
    }
    rep.stage(Stage::WritingImage);
    // No stale backup GPT or file-system signature may survive behind a
    // smaller image.
    imaging::erase_signatures(dev)?;
    let started = Instant::now();
    let digest = imaging::write_image(src, dev, cancel, &mut |done| rep.progress(done, size))?;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    let line = format!(
        "Wrote {} byte for byte ({} average).",
        human_size(digest.len),
        human_speed(digest.len as f64 / secs)
    );
    rep.log(format!(
        "{line} SHA-256 of the written data: {}",
        digest.sha256
    ));
    lines.push(line);
    if options.verify {
        rep.stage(Stage::Verifying);
        let total = digest.len;
        imaging::verify_image(dev, &digest, cancel, &mut |done| rep.progress(done, total))?;
        let line = "Verified: the drive reads back identical to the image.".to_string();
        rep.log(line.clone());
        lines.push(line);
    }
    Ok(lines)
}

/// Wipes the device, writes a partition table with one partition and returns
/// the partition's (offset, size). The table is read back and checked.
fn partition_single(
    dev: &mut dyn BlockDevice,
    scheme: Scheme,
    kind: PartType,
    active: bool,
    rep: &mut Reporter,
) -> Result<(u64, u64)> {
    rep.stage(Stage::Partitioning);
    let sector = dev.sector_size();
    let (offset, size) = part::single_partition(dev.size(), sector, scheme)?;
    part::wipe_tables(dev)?;
    let mut spec = PartSpec::new(kind, offset, size, "BootForge");
    match scheme {
        Scheme::Mbr => {
            spec.active = active;
            part::write_mbr(dev, &[spec], part::random_u32())?;
        }
        Scheme::Gpt => part::write_gpt(dev, &[spec], part::random_guid())?,
    }
    dev.flush().ctx("Could not flush the partition table")?;
    check_table(dev, scheme, offset, size)?;
    rep.log(format!(
        "{}: one partition, {} at {}.",
        scheme.label(),
        human_size(size),
        human_size(offset)
    ));
    Ok((offset, size))
}

/// Reads the partition table back and insists on exactly the one partition.
fn check_table(dev: &mut dyn BlockDevice, scheme: Scheme, offset: u64, size: u64) -> Result<()> {
    let table = part::read_table(dev)?;
    if !table.problems.is_empty() {
        return Err(Error::Msg(format!(
            "The partition table read back from the drive is not consistent: {}",
            table.problems.join("; ")
        )));
    }
    let ok = table.scheme == Some(scheme)
        && table.parts.len() == 1
        && table.parts[0].offset == offset
        && table.parts[0].size == size;
    if !ok {
        return Err(Error::msg(
            "The partition table read back from the drive differs from what was written.",
        ));
    }
    Ok(())
}

fn dos_time(t: Option<crate::iso::DateTime>) -> Option<fat32::DosTime> {
    let t = t?;
    Some(fat32::DosTime::new(
        t.year as u32,
        t.month as u32,
        t.day as u32,
        t.hour as u32,
        t.minute as u32,
        t.second as u32,
    ))
}

/// Why a file or folder name cannot be stored on FAT32 (`None` when it can).
fn fat_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() || name == "." || name == ".." {
        return Some("it is empty or reserved");
    }
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || c == '\u{7f}' || "\"*/:<>?\\|".contains(c))
    {
        return Some("it contains a character Windows file systems do not allow");
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Some("it ends with a space or a dot");
    }
    if name.encode_utf16().count() > 255 {
        return Some("it is longer than 255 characters");
    }
    None
}

/// Checks that every path of the image can exist on a FAT32 volume: valid
/// names and no two entries that differ only in case.
fn check_fat_names(entries: &[crate::iso::Entry]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for e in entries {
        if let Some(name) = e.path.rsplit('/').next() {
            if let Some(why) = fat_name_problem(name) {
                return Err(Error::Refused(format!(
                    "\"{}\" cannot be stored on FAT32: {why}. Use image mode for this image",
                    e.path
                )));
            }
        }
        let key: String = e.path.chars().flat_map(char::to_uppercase).collect();
        if !seen.insert(key) {
            return Err(Error::Refused(format!(
                "the image contains two entries named \"{}\" that differ only in upper and lower case, which FAT32 cannot hold. Use image mode for this image",
                e.path
            )));
        }
    }
    Ok(())
}

/// What file-copy mode did.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FilesReport {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Number of .swm parts when install.wim was split, else 0.
    pub split_parts: usize,
    pub partition_offset: u64,
    pub partition_size: u64,
    pub lines: Vec<String>,
}

/// One file that goes onto the FAT32 volume.
enum Source {
    /// Index into the ISO's entries.
    Iso(usize),
    /// A temporary file (a .swm part).
    Temp(PathBuf),
}

struct Planned {
    /// Path on the target volume.
    path: String,
    size: u64,
    source: Source,
}

/// Compares two streams of `size` bytes.
fn streams_equal(
    a: &mut dyn Read,
    b: &mut dyn Read,
    size: u64,
    cancel: &Cancel,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<bool> {
    let mut ba = vec![0u8; MIB as usize];
    let mut bb = vec![0u8; MIB as usize];
    let mut left = size;
    while left > 0 {
        cancel.check()?;
        let n = left.min(MIB) as usize;
        a.read_exact(&mut ba[..n])
            .ctx("Could not re-read the source")?;
        if b.read_exact(&mut bb[..n]).is_err() {
            return Ok(false);
        }
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
        left -= n as u64;
        on_bytes(n as u64);
    }
    Ok(true)
}

/// File-copy mode: partition, format FAT32, copy the ISO's files (splitting
/// an oversized install.wim), verify.
pub fn write_files(
    iso: &IsoImage,
    dev: &mut dyn BlockDevice,
    options: &WriteOptions,
    params: &FilesParams,
    cancel: &Cancel,
    rep: &mut Reporter,
) -> Result<FilesReport> {
    if options.fs != FileSystem::Fat32 {
        return Err(Error::Refused(
            "file copy always uses FAT32 (UEFI firmware boots from FAT32 only)".into(),
        ));
    }
    if options.target != TargetSystem::Uefi {
        return Err(Error::Refused(
            "file copy makes UEFI-bootable drives only".into(),
        ));
    }
    let entries = iso.entries();
    let has_loader = entries.iter().any(|e| {
        let p = e.path.to_ascii_lowercase();
        !e.is_dir && p.starts_with("efi/boot/boot") && p.ends_with(".efi")
    });
    if !has_loader {
        return Err(Error::Refused(
            "the image has no UEFI boot loader (EFI\\BOOT\\BOOT*.EFI), so a file copy would not boot".into(),
        ));
    }

    // ---- plan: what goes where, before anything is touched ----------------------
    check_fat_names(entries)?;
    let mut planned: Vec<Planned> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    let mut to_split: Option<usize> = None;
    for (i, e) in entries.iter().enumerate() {
        if e.is_dir {
            dirs.push(e.path.clone());
            continue;
        }
        if e.size > params.max_file {
            if e.path.eq_ignore_ascii_case(INSTALL_WIM) && params.splitter.is_some() {
                to_split = Some(i);
                continue;
            }
            return Err(Error::Refused(
                if e.path.eq_ignore_ascii_case(INSTALL_WIM) {
                    format!(
                    "{} is {} and cannot be stored on FAT32 without splitting; the DISM tool needed for that is not available",
                    e.path,
                    human_size(e.size)
                )
                } else {
                    format!(
                        "\"{}\" is {}, more than FAT32 can hold in one file",
                        e.path,
                        human_size(e.size)
                    )
                },
            ));
        }
        planned.push(Planned {
            path: e.path.clone(),
            size: e.size,
            source: Source::Iso(i),
        });
    }

    // Rough size check before anything is touched (the exact one follows
    // once the volume exists). Split parts add up to the .wim's size.
    let rough: u64 = planned.iter().map(|p| p.size).sum::<u64>()
        + to_split.map(|i| entries[i].size).unwrap_or(0);
    if rough + 4 * MIB > dev.size() {
        return Err(Error::Refused(format!(
            "the drive ({}) is too small for the {} of files in this image",
            human_size(dev.size()),
            human_size(rough)
        )));
    }

    let mut report = FilesReport::default();
    if options.bad_block_check {
        report.lines.push(run_bad_block_check(dev, cancel, rep)?);
    }

    // ---- split install.wim (the drive's old contents are still intact) ----------
    let mut temp = TempFiles {
        files: Vec::new(),
        dir: None,
    };
    if let Some(i) = to_split {
        let e = &entries[i];
        let splitter = params.splitter.expect("checked above");
        rep.stage(Stage::SplittingWim);
        std::fs::create_dir_all(&params.work_dir).ctx("Could not create the temporary folder")?;
        temp.dir = Some(params.work_dir.clone());
        if let Some(free) = sys::free_space(&params.work_dir) {
            let need = e.size.saturating_mul(2) + 64 * MIB;
            if free < need {
                return Err(Error::Msg(format!(
                    "Splitting {} needs about {} of free space in {}, but only {} is free.",
                    e.path,
                    human_size(need),
                    params.work_dir.display(),
                    human_size(free)
                )));
            }
        }
        let wim = params.work_dir.join("install.wim");
        temp.files.push(wim.clone());
        rep.item(format!("Extracting {} from the image", e.path));
        rep.log(format!(
            "Extracting {} ({}) to the temporary folder.",
            e.path,
            human_size(e.size)
        ));
        {
            let mut out = std::fs::File::create(&wim).ctx("Could not create the temporary .wim")?;
            let mut reader = iso.open_file(e);
            let mut buf = vec![0u8; MIB as usize];
            let mut done = 0u64;
            // Extraction is the first half of this stage's bar, DISM the second.
            while done < e.size {
                cancel.check()?;
                let n = (e.size - done).min(MIB) as usize;
                reader
                    .read_exact(&mut buf[..n])
                    .ctx("Could not read install.wim from the image")?;
                std::io::Write::write_all(&mut out, &buf[..n])
                    .ctx("Could not write the temporary .wim")?;
                done += n as u64;
                rep.progress(done / 2, e.size);
            }
            out.sync_all().ctx("Could not write the temporary .wim")?;
        }
        rep.item("Splitting with DISM");
        let size = e.size;
        let parts = splitter.split(&wim, &params.work_dir, params.part_mb, cancel, &mut |pct| {
            let pct = pct.clamp(0.0, 100.0) as f64;
            rep.progress(size / 2 + (size as f64 / 2.0 * pct / 100.0) as u64, size)
        })?;
        temp.files.extend(parts.iter().cloned());
        let _ = std::fs::remove_file(&wim);
        if parts.is_empty() {
            return Err(Error::msg("Splitting install.wim produced no parts."));
        }
        let dir = e.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        for p in &parts {
            let size = std::fs::metadata(p)
                .ctx("Could not read a split part")?
                .len();
            if size > params.max_file {
                return Err(Error::msg(
                    "A part of the split install.wim is still too large for FAT32.",
                ));
            }
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            planned.push(Planned {
                path: if dir.is_empty() {
                    name
                } else {
                    format!("{dir}/{name}")
                },
                size,
                source: Source::Temp(p.clone()),
            });
        }
        report.split_parts = parts.len();
        let line = format!(
            "{} ({}) was split into {} parts.",
            e.path,
            human_size(e.size),
            parts.len()
        );
        rep.log(line.clone());
        report.lines.push(line);
    }
    let total: u64 = planned.iter().map(|p| p.size).sum();

    // ---- partition and format ------------------------------------------------------
    let kind = match options.scheme {
        // "Basic data" so that Windows gives the volume a drive letter; UEFI
        // firmware boots removable FAT volumes whatever the type GUID.
        Scheme::Gpt => PartType::BasicData,
        Scheme::Mbr => PartType::Fat32Lba,
    };
    let (offset, size) = partition_single(dev, options.scheme, kind, true, rep)?;
    report.partition_offset = offset;
    report.partition_size = size;
    let sector = dev.sector_size() as u64;

    rep.stage(Stage::Formatting);
    let label = fat32::sanitize_label(&options.label);
    let geometry = {
        let mut vol = SubDevice::new(dev, offset, size).ctx("Could not open the new partition")?;
        let opts = fat32::FormatOptions {
            label: label.clone(),
            cluster_size: options.cluster_size,
            hidden_sectors: offset / sector,
            volume_id: part::random_u32(),
            zero_data: !options.quick_format,
        };
        let full = !options.quick_format;
        fat32::format(&mut vol, &opts, cancel, &mut |done| {
            rep.progress(done, if full { size } else { 0 })
        })?
    };
    let line = format!(
        "{}, FAT32 with {} clusters, label \"{}\".",
        options.scheme.label(),
        human_size(geometry.cluster_size as u64),
        label
    );
    rep.log(line.clone());
    report.lines.push(line);

    // ---- copy ------------------------------------------------------------------------
    rep.stage(Stage::CopyingFiles);
    let started = Instant::now();
    {
        let mut vol = SubDevice::new(dev, offset, size).ctx("Could not open the new partition")?;
        let mut writer = fat32::Fat32Writer::open(&mut vol)?;
        let free = writer.free_bytes();
        // One cluster of slack per file and directory, plus directory entries.
        let need =
            total + (planned.len() as u64 + dirs.len() as u64 + 1) * geometry.cluster_size as u64;
        if need > free {
            return Err(Error::Msg(format!(
                "The drive is too small: the files need about {}, the new volume has {}.",
                human_size(need),
                human_size(free)
            )));
        }
        for d in &dirs {
            cancel.check()?;
            writer.create_dir_all(d)?;
        }
        let mut base = 0u64;
        for p in &planned {
            cancel.check()?;
            rep.item(p.path.clone());
            let mut progress = |done: u64| rep.progress(base + done, total);
            match &p.source {
                Source::Iso(i) => {
                    let e = &entries[*i];
                    let mut reader = iso.open_file(e);
                    writer.write_file(
                        &p.path,
                        p.size,
                        &mut reader,
                        dos_time(e.mtime),
                        cancel,
                        &mut progress,
                    )?;
                }
                Source::Temp(path) => {
                    let mut f = std::fs::File::open(path).ctx("Could not open a split part")?;
                    writer.write_file(&p.path, p.size, &mut f, None, cancel, &mut progress)?;
                }
            }
            base += p.size;
        }
        rep.item("Writing the file allocation tables");
        writer.finish()?;
    }
    dev.flush().ctx("Could not flush the drive")?;
    report.files = planned.len() as u64;
    report.dirs = dirs.len() as u64;
    report.bytes = total;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    let line = format!(
        "{} files in {} folders copied, {} ({} average).",
        planned.len(),
        dirs.len(),
        human_size(total),
        human_speed(total as f64 / secs)
    );
    rep.log(line.clone());
    report.lines.push(line);

    // ---- verify ------------------------------------------------------------------------
    if options.verify {
        rep.stage(Stage::Verifying);
        check_table(dev, options.scheme, offset, size)?;
        let mut vol = SubDevice::new(dev, offset, size).ctx("Could not open the new partition")?;
        rep.item("Checking the file system");
        let fs = fat32::check(&mut vol)?;
        if !fs.problems.is_empty() {
            return Err(Error::Msg(format!(
                "The file system read back from the drive is not consistent: {}",
                fs.problems
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
        let reader = fat32::Fat32Reader::open(&mut vol)?;
        let mut done = 0u64;
        for p in &planned {
            cancel.check()?;
            rep.item(p.path.clone());
            let found = fs
                .files
                .iter()
                .find(|f| !f.is_dir && f.path.eq_ignore_ascii_case(&p.path));
            let Some(found) = found else {
                return Err(Error::Msg(format!(
                    "Verification failed: \"{}\" is missing on the drive.",
                    p.path
                )));
            };
            if found.size != p.size {
                return Err(Error::Msg(format!(
                    "Verification failed: \"{}\" has the wrong size on the drive.",
                    p.path
                )));
            }
            let mut on_drive = reader.open_found(&mut vol, found)?;
            let mut count = |n: u64| {
                done += n;
                rep.progress(done, total)
            };
            let same = match &p.source {
                Source::Iso(i) => {
                    let mut src = iso.open_file(&entries[*i]);
                    streams_equal(&mut src, &mut on_drive, p.size, cancel, &mut count)?
                }
                Source::Temp(path) => {
                    let mut src = std::fs::File::open(path).ctx("Could not open a split part")?;
                    streams_equal(&mut src, &mut on_drive, p.size, cancel, &mut count)?
                }
            };
            if !same {
                return Err(Error::Msg(format!(
                    "Verification failed: \"{}\" reads back differently from the drive.",
                    p.path
                )));
            }
        }
        let line = format!(
            "Verified: partition table and file system are consistent and all {} files read back identical.",
            planned.len()
        );
        rep.log(line.clone());
        report.lines.push(line);
    }
    drop(temp);
    Ok(report)
}

/// "Restore to a normal empty drive": wipe, one partition, format.
pub fn reset_device(
    dev: &mut dyn BlockDevice,
    options: &ResetOptions,
    cancel: &Cancel,
    rep: &mut Reporter,
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    if options.bad_block_check {
        lines.push(run_bad_block_check(dev, cancel, rep)?);
    }
    let kind = match options.fs {
        FileSystem::Fat32 => PartType::Fat32Lba,
        FileSystem::ExFat => PartType::ExFatOrNtfs,
    };
    // A data drive needs no active partition.
    let (offset, size) = partition_single(dev, options.scheme, kind, false, rep)?;
    let sector = dev.sector_size() as u64;
    rep.stage(Stage::Formatting);
    let full = !options.quick_format;
    let (label, cluster) = {
        let mut vol = SubDevice::new(dev, offset, size).ctx("Could not open the new partition")?;
        match options.fs {
            FileSystem::Fat32 => {
                let label = fat32::sanitize_label(&options.label);
                let opts = fat32::FormatOptions {
                    label: label.clone(),
                    cluster_size: options.cluster_size,
                    hidden_sectors: offset / sector,
                    volume_id: part::random_u32(),
                    zero_data: full,
                };
                let g = fat32::format(&mut vol, &opts, cancel, &mut |done| {
                    rep.progress(done, if full { size } else { 0 })
                })?;
                let fs = fat32::check(&mut vol)?;
                if !fs.problems.is_empty() {
                    return Err(Error::Msg(format!(
                        "The new file system does not read back consistently: {}",
                        fs.problems.join("; ")
                    )));
                }
                (label, g.cluster_size)
            }
            FileSystem::ExFat => {
                if full {
                    imaging::zero_fill(&mut vol, 0, size, cancel, &mut |done| {
                        rep.progress(done, size)
                    })?;
                }
                let opts = exfat::ExFatOptions {
                    label: options.label.clone(),
                    cluster_size: options.cluster_size,
                    partition_offset_sectors: offset / sector,
                    volume_serial: part::random_u32(),
                };
                let g = exfat::format(&mut vol, &opts, cancel, &mut |_| {})?;
                let fs = exfat::check(&mut vol)?;
                if !fs.problems.is_empty() {
                    return Err(Error::Msg(format!(
                        "The new file system does not read back consistently: {}",
                        fs.problems.join("; ")
                    )));
                }
                (g.label.clone(), g.cluster_size)
            }
        }
    };
    dev.flush().ctx("Could not flush the drive")?;
    let line = format!(
        "{} with one {} partition of {} ({} clusters), label \"{}\".",
        options.scheme.label(),
        options.fs.label(),
        human_size(size),
        human_size(cluster as u64),
        label
    );
    rep.log(line.clone());
    lines.push(line);
    lines.push("The new partition table and file system were read back and are consistent.".into());
    Ok(lines)
}

/// Reads the whole device into `out`.
pub fn backup_device(
    dev: &mut dyn BlockDevice,
    out: &Path,
    cancel: &Cancel,
    rep: &mut Reporter,
) -> Result<Checksums> {
    rep.stage(Stage::ReadingDrive);
    let total = dev.size();
    let sums = imaging::backup_to_file(dev, out, cancel, &mut |done| rep.progress(done, total))?;
    rep.log(format!("Saved {} to {}", human_size(total), out.display()));
    rep.log(format!("SHA-256 {}", sums.sha256));
    Ok(sums)
}

/// Runs a destructive job on an already opened device. Returns summary lines.
pub fn run_on_device(
    job: &Job,
    dev: &mut dyn BlockDevice,
    params: &FilesParams,
    cancel: &Cancel,
    rep: &mut Reporter,
) -> Result<Vec<String>> {
    match job {
        Job::Write { info, options, .. } => match options.mode {
            WriteMode::Image => {
                let mut src = FileSource::open(&info.path).ctx("Could not open the image")?;
                write_image_raw(&mut src, dev, options, cancel, rep)
            }
            WriteMode::Files => {
                let src = FileSource::open(&info.path).ctx("Could not open the image")?;
                let iso = IsoImage::open(Box::new(src))?;
                Ok(write_files(&iso, dev, options, params, cancel, rep)?.lines)
            }
        },
        Job::Reset { options, .. } => reset_device(dev, options, cancel, rep),
        Job::Backup { .. } => Err(Error::msg("A backup does not write to the drive.")),
    }
}

// --------------------------------------------------------------------------------------
// The real machine
// --------------------------------------------------------------------------------------

/// What the drive looks like after a job that did not finish.
fn drive_state(job: &Job, opened: bool) -> &'static str {
    if !job.destructive() {
        "The drive was only read; it is unchanged."
    } else if opened {
        "The drive may be partly written and unusable as it is. Write it again, or use \"Restore to an empty drive\" in Drive tools."
    } else {
        "Nothing was written to the drive."
    }
}

pub(super) fn run_real(job: &Job, policy: &Policy, cancel: &Cancel, rep: &mut Reporter) -> Outcome {
    let started = Instant::now();
    let mut opened = false;
    match real_inner(job, policy, cancel, rep, &mut opened) {
        Ok(mut summary) => {
            summary.seconds = started.elapsed().as_secs();
            rep.log(format!("Done in {}.", human_duration(summary.seconds)));
            Outcome::Done(summary)
        }
        Err(Error::Cancelled) => {
            rep.log("Cancelled.");
            Outcome::Cancelled(drive_state(job, opened).to_string())
        }
        Err(e) => {
            rep.log(format!("Failed: {e}"));
            Outcome::Failed(format!("{e}\n\n{}", drive_state(job, opened)))
        }
    }
}

fn real_inner(
    job: &Job,
    policy: &Policy,
    cancel: &Cancel,
    rep: &mut Reporter,
    opened: &mut bool,
) -> Result<Summary> {
    rep.log(job.title());
    if !sys::is_elevated() {
        return Err(Error::msg(
            "Administrator rights are needed to access a drive directly. Restart BootForge as administrator.",
        ));
    }
    rep.stage(Stage::Checking);
    let disks = device::list_disks()?;
    let expected = job.disk();

    if let Job::Backup { out, .. } = job {
        let now = disks
            .iter()
            .find(|d| d.number == expected.number)
            .ok_or_else(|| Refusal::Changed("it is no longer connected".into()))?;
        safety::same_device(expected, now)?;
        // Reading is harmless, but only drives that could be listed are read,
        // and the backup must not land on the drive it is a copy of.
        match safety::check(now, policy, std::slice::from_ref(out)) {
            Ok(()) | Err(Refusal::ReadOnly) => {}
            Err(r) => return Err(r.into()),
        }
        if out.parent().and_then(device::disk_of_path) == Some(now.number) {
            return Err(Refusal::HoldsFile(out.clone()).into());
        }
        if let Some(free) = out.parent().and_then(sys::free_space) {
            if free < now.size {
                return Err(Error::Msg(format!(
                    "The backup needs {}, but only {} is free in the chosen folder.",
                    human_size(now.size),
                    human_size(free)
                )));
            }
        }
        rep.log(format!(
            "{}, {}, {}, disk {}: identity confirmed.",
            now.name(),
            human_size(now.size),
            now.bus.label(),
            now.number
        ));
        let mut raw = device::open_for_read(now.number)?;
        let sums = backup_device(&mut raw, out, cancel, rep)?;
        rep.stage(Stage::Finishing);
        drop(raw);
        return Ok(Summary {
            headline: "Backup finished".into(),
            lines: vec![
                format!("{} saved to {}", human_size(now.size), out.display()),
                format!("SHA-256: {}", sums.sha256),
            ],
            seconds: 0,
        });
    }

    let (needed, eject): (Vec<PathBuf>, bool) = match job {
        Job::Write { info, options, .. } => (vec![info.path.clone()], options.eject),
        Job::Reset { options, .. } => (Vec::new(), options.eject),
        Job::Backup { .. } => unreachable!("handled above"),
    };
    let clearance = safety::clear(expected, &disks, policy, &needed)?;
    // The letter-based rule misses files reached through mount points or
    // substituted drives; ask Windows which disk really holds them.
    for f in &needed {
        if device::disk_of_path(f) == Some(expected.number) {
            return Err(Refusal::HoldsFile(f.clone()).into());
        }
    }
    if let Some(now) = disks.iter().find(|d| d.number == expected.number) {
        rep.log(format!(
            "{}, {}, {}, disk {}: identity confirmed, all safety rules passed.",
            now.name(),
            human_size(now.size),
            now.bus.label(),
            now.number
        ));
    }
    // Fail early on an unreadable image, before the drive is locked.
    if let Job::Write { info, .. } = job {
        std::fs::File::open(&info.path).ctx("Could not open the image")?;
    }
    cancel.check()?;

    rep.stage(Stage::Locking);
    let mut raw = device::open_for_write(&clearance)?;
    *opened = true;
    let letters: String = clearance
        .letters()
        .iter()
        .map(|l| format!("{l}: "))
        .collect();
    rep.log(if letters.is_empty() {
        "Drive opened; it has no mounted volumes.".to_string()
    } else {
        format!("Locked and dismounted {}", letters.trim_end())
    });

    let dism = sys::find_dism().map(DismSplitter);
    let params = FilesParams {
        max_file: FAT32_MAX_FILE,
        part_mb: SWM_PART_MB,
        splitter: dism.as_ref().map(|d| d as &dyn WimSplitter),
        work_dir: sys::work_dir().join(format!("split-{}", std::process::id())),
    };
    let result = run_on_device(job, &mut raw, &params, cancel, rep);

    // Whatever happened: flush, unlock, let Windows re-read the drive.
    rep.stage(Stage::Finishing);
    let finish = raw.finish(eject && result.is_ok());
    let mut lines = result?;
    match finish? {
        None => {
            rep.log("Volumes unlocked; Windows re-read the drive.");
            if eject {
                lines.push("The drive was ejected and can be unplugged.".into());
            }
        }
        Some(note) => {
            rep.log(format!("Finishing: {note}"));
            lines.push(format!("Note: {note}"));
        }
    }
    let headline = match job {
        Job::Write { options, .. } if options.mode == WriteMode::Files => {
            "The bootable drive is ready"
        }
        Job::Write { .. } => "The image was written",
        _ => "The drive is empty and ready to use",
    };
    Ok(Summary {
        headline: headline.into(),
        lines,
        seconds: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockdev::{MemDevice, MemSource};
    use crate::engine::Event;
    use crate::iso::build;

    fn files_options() -> WriteOptions {
        WriteOptions {
            mode: WriteMode::Files,
            label: "TEST".into(),
            ..WriteOptions::default()
        }
    }

    fn params() -> FilesParams<'static> {
        FilesParams::standard(std::env::temp_dir().join("bootforge-engine-test-unused"))
    }

    #[test]
    fn file_copy_refuses_what_it_cannot_do_before_touching_the_device() {
        let iso = IsoImage::open(Box::new(MemSource(build::sample_linux_hybrid()))).unwrap();
        let cancel = Cancel::new();
        let (mut rep, _rx) = Reporter::collecting();
        let mut dev = MemDevice::new(64 * MIB, 512);

        let mut o = files_options();
        o.target = TargetSystem::Bios;
        let r = write_files(&iso, &mut dev, &o, &params(), &cancel, &mut rep);
        assert!(matches!(r, Err(Error::Refused(_))));

        let mut o = files_options();
        o.fs = FileSystem::ExFat;
        let r = write_files(&iso, &mut dev, &o, &params(), &cancel, &mut rep);
        assert!(matches!(r, Err(Error::Refused(_))));

        // A drive far too small is refused before it is wiped.
        let mut tiny = MemDevice::new(MIB, 512);
        let r = write_files(
            &iso,
            &mut tiny,
            &files_options(),
            &params(),
            &cancel,
            &mut rep,
        );
        assert!(matches!(r, Err(Error::Refused(_))), "{r:?}");
        assert_eq!(dev.writes + tiny.writes, 0);
    }

    #[test]
    fn file_copy_refuses_an_iso_without_uefi_loader() {
        let image = build::IsoBuilder::new("NOEFI")
            .file("boot/vmlinuz", build::pattern(5000, 3, 1))
            .build();
        let iso = IsoImage::open(Box::new(MemSource(image))).unwrap();
        let (mut rep, _rx) = Reporter::collecting();
        let mut dev = MemDevice::new(64 * MIB, 512);
        let r = write_files(
            &iso,
            &mut dev,
            &files_options(),
            &params(),
            &Cancel::new(),
            &mut rep,
        );
        assert!(matches!(r, Err(Error::Refused(_))));
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn image_mode_refuses_oversized_and_empty_images_untouched() {
        let (mut rep, _rx) = Reporter::collecting();
        let mut dev = MemDevice::new(MIB, 512);
        let mut big = MemSource(vec![1u8; 2 * MIB as usize]);
        let o = WriteOptions::default();
        let r = write_image_raw(&mut big, &mut dev, &o, &Cancel::new(), &mut rep);
        assert!(matches!(r, Err(Error::Refused(_))));
        let mut empty = MemSource(Vec::new());
        let r = write_image_raw(&mut empty, &mut dev, &o, &Cancel::new(), &mut rep);
        assert!(matches!(r, Err(Error::Invalid(_))));
        assert_eq!(dev.writes, 0);
    }

    #[test]
    fn image_mode_reports_stages_and_final_progress() {
        let (mut rep, rx) = Reporter::collecting();
        let mut dev = MemDevice::new(8 * MIB, 512);
        let data = build::pattern(3 * MIB as usize + 100, 7, 3);
        let mut src = MemSource(data.clone());
        let lines = write_image_raw(
            &mut src,
            &mut dev,
            &WriteOptions::default(),
            &Cancel::new(),
            &mut rep,
        )
        .unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(dev.data[..data.len()], data[..]);
        // The partial last sector is zero padded.
        assert!(dev.data[data.len()..data.len() + 412]
            .iter()
            .all(|b| *b == 0));
        let events: Vec<Event> = rx.try_iter().collect();
        let stages: Vec<Stage> = events
            .iter()
            .filter_map(|e| match e {
                Event::Stage(s) => Some(*s),
                _ => None,
            })
            .collect();
        assert_eq!(stages, [Stage::WritingImage, Stage::Verifying]);
        assert!(events.contains(&Event::Progress {
            done: data.len() as u64,
            total: data.len() as u64
        }));
    }

    #[test]
    fn file_copy_works_on_4k_sector_devices_with_full_format() {
        let image = build::sample_linux_hybrid();
        let iso = IsoImage::open(Box::new(MemSource(image))).unwrap();
        let (mut rep, _rx) = Reporter::collecting();
        let mut dev = MemDevice::new(320 * MIB, 4096);
        let mut o = files_options();
        o.quick_format = false;
        o.scheme = Scheme::Mbr;
        let report = write_files(&iso, &mut dev, &o, &params(), &Cancel::new(), &mut rep).unwrap();
        assert!(report.files > 0 && report.split_parts == 0);
        let table = part::read_table(&mut dev).unwrap();
        assert!(table.problems.is_empty(), "{:?}", table.problems);
        let mut vol =
            SubDevice::new(&mut dev, report.partition_offset, report.partition_size).unwrap();
        let fs = fat32::check(&mut vol).unwrap();
        assert!(fs.problems.is_empty(), "{:?}", fs.problems);
        assert_eq!(fs.label, "TEST");
    }

    #[test]
    fn reset_refuses_a_drive_with_bad_blocks_and_formats_a_good_one() {
        let (mut rep, _rx) = Reporter::collecting();
        let mut dev = MemDevice::new(64 * MIB, 512);
        dev.bad_ranges.push(10 * MIB..10 * MIB + 512);
        let o = ResetOptions {
            bad_block_check: true,
            ..ResetOptions::default()
        };
        let r = reset_device(&mut dev, &o, &Cancel::new(), &mut rep);
        assert!(matches!(r, Err(Error::Msg(_))));
        let mut dev = MemDevice::new(64 * MIB, 512);
        let lines = reset_device(&mut dev, &o, &Cancel::new(), &mut rep).unwrap();
        assert!(lines.iter().any(|l| l.contains("No bad blocks")));
        assert!(part::read_table(&mut dev).unwrap().problems.is_empty());
    }

    #[test]
    fn names_fat32_cannot_hold_are_refused_before_touching_the_device() {
        assert_eq!(fat_name_problem("vmlinuz-6.8"), None);
        assert_eq!(fat_name_problem("Ünïcode name.txt"), None);
        assert!(fat_name_problem("a:b").is_some());
        assert!(fat_name_problem("what?").is_some());
        assert!(fat_name_problem("trailing.").is_some());
        assert!(fat_name_problem(&"x".repeat(256)).is_some());
        let (mut rep, _rx) = Reporter::collecting();
        for image in [
            // Two names that differ only in case (legal on Linux media).
            build::IsoBuilder::new("CASE")
                .rock_ridge(true)
                .file(
                    "EFI/BOOT/BOOTX64.EFI",
                    build::fake_efi_binary(crate::model::Arch::X64),
                )
                .file("docs/README", vec![1, 2, 3])
                .file("docs/readme", vec![4, 5, 6])
                .build(),
            // A character FAT32 does not allow.
            build::IsoBuilder::new("CHARS")
                .rock_ridge(true)
                .file(
                    "EFI/BOOT/BOOTX64.EFI",
                    build::fake_efi_binary(crate::model::Arch::X64),
                )
                .file("pool/a:b.deb", vec![1, 2, 3])
                .build(),
        ] {
            let iso = IsoImage::open(Box::new(MemSource(image))).unwrap();
            let mut dev = MemDevice::new(64 * MIB, 512);
            let r = write_files(
                &iso,
                &mut dev,
                &files_options(),
                &params(),
                &Cancel::new(),
                &mut rep,
            );
            assert!(matches!(r, Err(Error::Refused(_))), "{r:?}");
            assert_eq!(dev.writes, 0);
        }
    }

    #[test]
    fn states_after_failure_are_described_truthfully() {
        let disk = crate::model::DiskIdentity {
            number: 7,
            serial: "S".into(),
            model: "M".into(),
            size: 1,
        };
        let backup = Job::Backup {
            disk: disk.clone(),
            out: PathBuf::from("x.img"),
        };
        assert!(drive_state(&backup, true).contains("unchanged"));
        let reset = Job::Reset {
            disk,
            options: ResetOptions::default(),
        };
        assert!(drive_state(&reset, false).contains("Nothing was written"));
        assert!(drive_state(&reset, true).contains("partly written"));
    }

    #[test]
    fn real_jobs_fail_cleanly_without_elevation_or_device() {
        // Disk number 4000 does not exist; whether or not the test process
        // is elevated, the job must end with a failure and touch nothing.
        let job = Job::Reset {
            disk: crate::model::DiskIdentity {
                number: 4000,
                serial: "NONE".into(),
                model: "NONE".into(),
                size: 1 << 30,
            },
            options: ResetOptions::default(),
        };
        let (mut rep, _rx) = Reporter::collecting();
        let out = run_real(&job, &Policy::default(), &Cancel::new(), &mut rep);
        match out {
            Outcome::Failed(text) => assert!(text.contains("Nothing was written"), "{text}"),
            other => panic!("{other:?}"),
        }
    }
}
