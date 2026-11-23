//! `--selftest` and `--inspect`.
//!
//! The self-test builds small synthetic ISO images, writes them to image
//! files in the temp folder through the same engine code that writes real
//! drives, and verifies the results by parsing them back with the independent
//! readers (partition table, FAT32, exFAT, ISO). It never opens a physical
//! drive.

use crate::blockdev::{BlockDevice, FileDevice, FileSource, MemDevice, MemSource, SubDevice};
use crate::engine::{
    backup_device, reset_device, write_files, write_image_raw, FilesParams, Reporter, ResetOptions,
    WimSplitter,
};
use crate::error::{Error, Result};
use crate::iso::{self, IsoImage};
use crate::model::{Cancel, FileSystem, ImageKind, Scheme, TargetSystem, WriteMode, WriteOptions};
use crate::plan::{self, Env};
use crate::safety::{self, Policy};
use crate::util::human_size;
use crate::{demo, exfat, fat32, hash, imaging, part};
use std::path::{Path, PathBuf};

const MIB: u64 = 1024 * 1024;

/// A stand-in for DISM: cuts the file into `part_mb` MiB pieces named
/// `install.swm`, `install2.swm`, ... (not real split WIMs; the self-test only
/// needs to see that the parts replace the original on the drive).
pub struct ChunkSplitter;

impl WimSplitter for ChunkSplitter {
    fn split(
        &self,
        wim: &Path,
        out_dir: &Path,
        part_mb: u32,
        cancel: &Cancel,
        on_percent: &mut dyn FnMut(f32),
    ) -> Result<Vec<PathBuf>> {
        let data = std::fs::read(wim).map_err(|e| Error::io("reading the .wim", e))?;
        let mut parts = Vec::new();
        let chunk = part_mb as usize * MIB as usize;
        let count = data.len().div_ceil(chunk).max(1);
        for (i, piece) in data.chunks(chunk).enumerate() {
            cancel.check()?;
            let name = if i == 0 {
                "install.swm".to_string()
            } else {
                format!("install{}.swm", i + 1)
            };
            let path = out_dir.join(name);
            std::fs::write(&path, piece).map_err(|e| Error::io("writing a part", e))?;
            parts.push(path);
            on_percent((i + 1) as f32 * 100.0 / count as f32);
        }
        Ok(parts)
    }
}

/// Removes the self-test folder when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ensure(cond: bool, what: &str) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(Error::Msg(format!("check failed: {what}")))
    }
}

fn file_options(scheme: Scheme, label: &str) -> WriteOptions {
    WriteOptions {
        mode: WriteMode::Files,
        scheme,
        target: TargetSystem::Uefi,
        fs: FileSystem::Fat32,
        cluster_size: None,
        label: label.into(),
        quick_format: true,
        bad_block_check: false,
        verify: true,
        eject: false,
        setup: Default::default(),
    }
}

/// Compares every file of the ISO with what the FAT32 volume on `dev` holds,
/// skipping `skip` (lower-case path). Returns the number of files compared.
fn compare_tree(
    iso: &IsoImage,
    dev: &mut dyn BlockDevice,
    offset: u64,
    size: u64,
    skip: Option<&str>,
) -> Result<usize> {
    let mut vol = SubDevice::new(dev, offset, size).map_err(|e| Error::io("partition", e))?;
    let fs = fat32::check(&mut vol)?;
    ensure(
        fs.problems.is_empty(),
        &format!("FAT32 consistent ({:?})", fs.problems),
    )?;
    let mut n = 0;
    for e in iso.entries() {
        if Some(e.path.to_ascii_lowercase().as_str()) == skip {
            continue;
        }
        let found = fs
            .files
            .iter()
            .find(|f| f.path.eq_ignore_ascii_case(&e.path))
            .ok_or_else(|| Error::Msg(format!("missing on the volume: {}", e.path)))?;
        ensure(found.is_dir == e.is_dir, &format!("kind of {}", e.path))?;
        if e.is_dir {
            continue;
        }
        let mut expected = vec![0u8; e.size as usize];
        iso.read_at(e, 0, &mut expected)?;
        let got = fat32::read_file(&mut vol, &found.path)?;
        ensure(got == expected, &format!("contents of {}", e.path))?;
        n += 1;
    }
    Ok(n)
}

/// Runs the whole self-test. `Ok` carries the log lines.
pub fn run() -> std::result::Result<Vec<String>, String> {
    run_keeping(None)
}

/// Like [`run`]; with `keep` the image files are written into that folder
/// and left there for inspection with other tools.
pub fn run_keeping(keep: Option<&Path>) -> std::result::Result<Vec<String>, String> {
    let mut log = Vec::new();
    match run_inner(&mut log, keep) {
        Ok(()) => Ok(log),
        Err(e) => {
            log.push(format!("FAILED: {e}"));
            Err(log.join("\n"))
        }
    }
}

fn run_inner(log: &mut Vec<String>, keep: Option<&Path>) -> Result<()> {
    let dir = match keep {
        Some(d) => d.to_path_buf(),
        None => std::env::temp_dir().join(format!("bootforge-selftest-{}", std::process::id())),
    };
    std::fs::create_dir_all(&dir).map_err(|e| Error::io("creating the self-test folder", e))?;
    let _scratch = keep.is_none().then(|| Scratch(dir.clone()));
    let cancel = Cancel::new();
    let (mut rep, events) = Reporter::collecting();
    let mut ok = |text: String| log.push(format!("[ok] {text}"));
    let io = |what: &str| {
        let what = what.to_string();
        move |e: std::io::Error| Error::io(what.clone(), e)
    };
    let env = Env {
        dism_available: true,
    };

    // ---- 1. safety rules ---------------------------------------------------------------
    let disks = demo::disks();
    let (shown, hidden) = safety::listable(&disks, &Policy::default(), &[]);
    ensure(
        shown.iter().all(|l| !l.disk.is_system && l.disk.removable),
        "only removable, non-system disks are listed",
    )?;
    ensure(
        hidden == 3,
        "internal, system and fixed USB disks are hidden",
    )?;
    let lax = Policy {
        show_usb_hdd: true,
        max_size: u64::MAX,
    };
    for d in &disks {
        if d.is_system || !matches!(d.bus, crate::model::Bus::Usb | crate::model::Bus::Sd) {
            ensure(
                safety::clear(&d.identity(), &disks, &lax, &[]).is_err(),
                "internal and system disks are refused by the engine check",
            )?;
        }
    }
    let stick = disks[2].clone();
    let mut swapped = disks.clone();
    swapped[2].serial = "SOMETHING-ELSE".into();
    ensure(
        safety::clear(&stick.identity(), &swapped, &Policy::default(), &[]).is_err(),
        "a swapped drive is refused",
    )?;
    ok("safety rules: internal/system disks refused, swapped drive refused".into());

    // ---- 2. inspect a synthetic hybrid Linux ISO ----------------------------------
    let linux = iso::build::sample_linux_hybrid();
    let linux_path = dir.join("linux-hybrid.iso");
    std::fs::write(&linux_path, &linux).map_err(io("writing the test ISO"))?;
    let info = iso::inspect(&linux_path)?;
    ensure(
        info.kind == ImageKind::Iso && info.is_hybrid(),
        "hybrid ISO recognised",
    )?;
    ensure(
        info.bios_el_torito() && info.uefi_el_torito(),
        "El Torito BIOS + UEFI",
    )?;
    ensure(info.uefi_file_bootable(), "EFI loader found")?;
    let advice = plan::advise(&info, &env);
    ensure(
        advice.recommended == Some(WriteMode::Image),
        "image mode recommended",
    )?;
    ensure(advice.files_mode.is_ok(), "file copy also possible")?;
    ok(format!(
        "inspect: {} ({}), label {}, {} files",
        advice.headline,
        human_size(info.size),
        info.label,
        info.file_count
    ));

    // ---- 3. image mode ------------------------------------------------------------------
    let dev_path = dir.join("device-image-mode.img");
    {
        let mut dev = FileDevice::create(&dev_path, 48 * MIB, 512).map_err(io("device file"))?;
        // Old junk that must not survive: a fake backup GPT at the end.
        let end = dev.size() - 512;
        dev.write_at(end, &[0xEEu8; 512])
            .map_err(io("device file"))?;
        let mut src = FileSource::open(&linux_path).map_err(io("test ISO"))?;
        let opts = WriteOptions {
            mode: WriteMode::Image,
            verify: true,
            ..WriteOptions::default()
        };
        write_image_raw(&mut src, &mut dev, &opts, &cancel, &mut rep)?;
        let mut back = vec![0u8; linux.len().div_ceil(512) * 512];
        dev.read_at(0, &mut back).map_err(io("device file"))?;
        ensure(
            back[..linux.len()] == linux[..],
            "device starts with the image",
        )?;
        let mut tail = [0u8; 512];
        dev.read_at(end, &mut tail).map_err(io("device file"))?;
        ensure(
            tail.iter().all(|b| *b == 0),
            "old data at the end was wiped",
        )?;
        // Verification must notice a single flipped byte.
        let digest = imaging::write_image(
            &mut MemSource(linux.clone()),
            &mut dev,
            &cancel,
            &mut |_| {},
        )?;
        let mut sector = [0u8; 512];
        dev.read_at(65536, &mut sector).map_err(io("device file"))?;
        sector[7] ^= 0x10;
        dev.write_at(65536, &sector).map_err(io("device file"))?;
        ensure(
            imaging::verify_image(&mut dev, &digest, &cancel, &mut |_| {}).is_err(),
            "verify detects a flipped bit",
        )?;
        sector[7] ^= 0x10;
        dev.write_at(65536, &sector).map_err(io("device file"))?;
        imaging::verify_image(&mut dev, &digest, &cancel, &mut |_| {})?;
    }
    let from_device = iso::inspect(&dev_path)?;
    ensure(
        from_device.label == info.label && from_device.file_count == info.file_count,
        "the written device parses as the same ISO",
    )?;
    ok("image mode: written, verified, parsed back from the device image".into());

    // ---- 4. file-copy mode, GPT -----------------------------------------------------
    let iso_linux = IsoImage::open(Box::new(MemSource(linux.clone())))?;
    let params = FilesParams::standard(dir.join("work"));
    {
        let path = dir.join("device-files-gpt.img");
        let mut dev = FileDevice::create(&path, 96 * MIB, 512).map_err(io("device file"))?;
        let opts = file_options(Scheme::Gpt, &advice.default_label);
        let report = write_files(&iso_linux, &mut dev, &opts, &params, &cancel, &mut rep)?;
        let table = part::read_table(&mut dev)?;
        ensure(table.problems.is_empty(), "GPT consistent")?;
        ensure(
            table.scheme == Some(Scheme::Gpt) && table.parts.len() == 1,
            "GPT with one partition",
        )?;
        ensure(table.parts[0].offset == MIB, "partition starts at 1 MiB")?;
        let n = compare_tree(
            &iso_linux,
            &mut dev,
            report.partition_offset,
            report.partition_size,
            None,
        )?;
        let mut vol = SubDevice::new(&mut dev, report.partition_offset, report.partition_size)
            .map_err(io("partition"))?;
        let fs = fat32::check(&mut vol)?;
        ensure(
            fs.files
                .iter()
                .any(|f| f.path.eq_ignore_ascii_case("EFI/BOOT/BOOTX64.EFI")),
            "EFI/BOOT/BOOTX64.EFI is on the volume",
        )?;
        ensure(fs.label == advice.default_label, "volume label")?;
        ok(format!(
            "file copy (GPT + FAT32): {n} files identical, label {}, {} clusters",
            fs.label,
            human_size(fs.cluster_size as u64)
        ));
    }

    // ---- 5. Windows-style UDF ISO, MBR ----------------------------------------------
    let wim_size = 3 * MIB as usize + 12_345;
    let win = iso::build::sample_windows_setup(wim_size);
    let win_path = dir.join("windows-setup.iso");
    std::fs::write(&win_path, &win).map_err(io("writing the test ISO"))?;
    let win_info = iso::inspect(&win_path)?;
    let win_advice = plan::advise(&win_info, &env);
    let w = win_info
        .windows
        .as_ref()
        .ok_or_else(|| Error::msg("check failed: Windows setup image recognised"))?;
    ensure(
        w.install_size == wim_size as u64 && w.editions.len() == 2,
        "install.wim facts",
    )?;
    ensure(win_info.udf.is_some(), "UDF recognised")?;
    ensure(
        win_advice.recommended == Some(WriteMode::Files),
        "file copy recommended",
    )?;
    ensure(
        win_advice.image_mode.is_err(),
        "raw mode refused for a non-hybrid ISO",
    )?;
    let iso_win = IsoImage::open(Box::new(MemSource(win.clone())))?;
    {
        let path = dir.join("device-files-mbr.img");
        let mut dev = FileDevice::create(&path, 80 * MIB, 512).map_err(io("device file"))?;
        // With every Windows Setup option on: the answer file must arrive too.
        ensure(
            win_advice.setup_tweaks.available(),
            "Windows Setup options offered for a setup image",
        )?;
        let mut opts = file_options(Scheme::Mbr, &win_advice.default_label);
        opts.setup = crate::model::SetupTweaks {
            skip_requirements: true,
            local_account: true,
            skip_privacy: true,
        };
        let report = write_files(&iso_win, &mut dev, &opts, &params, &cancel, &mut rep)?;
        let table = part::read_table(&mut dev)?;
        ensure(table.problems.is_empty(), "MBR consistent")?;
        ensure(
            table.scheme == Some(Scheme::Mbr)
                && table.parts.len() == 1
                && table.parts[0].active
                && table.parts[0].type_byte == Some(0x0C),
            "MBR with one active FAT32 partition",
        )?;
        let n = compare_tree(
            &iso_win,
            &mut dev,
            report.partition_offset,
            report.partition_size,
            None,
        )?;
        ok(format!(
            "Windows-style UDF ISO, file copy (MBR + FAT32): {n} files identical, {} editions read from the WIM",
            w.editions.len()
        ));
        let expected = crate::unattend::build(&opts.setup, &win_info.archs)
            .ok_or_else(|| Error::msg("no answer file was built"))?;
        let mut vol = SubDevice::new(&mut dev, report.partition_offset, report.partition_size)
            .map_err(|e| Error::io("partition", e))?;
        let got = fat32::read_file(&mut vol, crate::unattend::FILE_NAME)?;
        ensure(
            report.answer_file && got == expected.as_bytes(),
            "answer file on the volume is the one that was built",
        )?;
        ok(format!(
            "Windows Setup options: {} ({} bytes) added and read back identical",
            crate::unattend::FILE_NAME,
            got.len()
        ));
    }

    // ---- 6. oversized install.wim: split -----------------------------------------------
    {
        let small_limit = FilesParams {
            max_file: MIB,
            part_mb: 1,
            splitter: Some(&ChunkSplitter),
            work_dir: dir.join("work-split"),
        };
        let path = dir.join("device-files-split.img");
        let mut dev = FileDevice::create(&path, 80 * MIB, 512).map_err(io("device file"))?;
        let opts = file_options(Scheme::Gpt, "SPLIT");
        let report = write_files(&iso_win, &mut dev, &opts, &small_limit, &cancel, &mut rep)?;
        ensure(report.split_parts == 4, "install.wim split into 4 parts")?;
        compare_tree(
            &iso_win,
            &mut dev,
            report.partition_offset,
            report.partition_size,
            Some("sources/install.wim"),
        )?;
        let mut vol = SubDevice::new(&mut dev, report.partition_offset, report.partition_size)
            .map_err(io("partition"))?;
        let fs = fat32::check(&mut vol)?;
        ensure(
            !fs.files
                .iter()
                .any(|f| f.path.eq_ignore_ascii_case("sources/install.wim")),
            "install.wim itself is not on the volume",
        )?;
        let mut joined = Vec::new();
        for name in [
            "install.swm",
            "install2.swm",
            "install3.swm",
            "install4.swm",
        ] {
            joined.extend(fat32::read_file(&mut vol, &format!("sources/{name}"))?);
        }
        let entry = iso_win
            .find("sources/install.wim")
            .ok_or_else(|| Error::msg("check failed: install.wim in the ISO"))?;
        let mut original = vec![0u8; entry.size as usize];
        iso_win.read_at(entry, 0, &mut original)?;
        ensure(
            joined == original,
            "the parts add up to the original install.wim",
        )?;
        ensure(
            !dir.join("work-split").exists(),
            "temporary split files were removed",
        )?;
        // Without a splitter the job is refused before the device is touched.
        let no_splitter = FilesParams {
            max_file: MIB,
            part_mb: 1,
            splitter: None,
            work_dir: dir.join("work-none"),
        };
        let mut mem = MemDevice::new(64 * MIB, 512);
        let r = write_files(&iso_win, &mut mem, &opts, &no_splitter, &cancel, &mut rep);
        ensure(
            matches!(r, Err(Error::Refused(_))),
            "oversized file refused without DISM",
        )?;
        ensure(mem.writes == 0, "refusal happened before any write")?;
        ok("oversized install.wim: split into parts, parts verified, refused cleanly without a splitter".into());
    }

    // ---- 6b. a real over-4-GiB install.wim (sparse, so the image stays tiny) --------
    {
        let big = iso::build::sample_windows_setup_sparse(2 * MIB as usize, 5 * 1024 * MIB);
        let big_info = iso::inspect_source(
            Path::new("big-windows.iso"),
            Box::new(MemSource(big.clone())),
        )?;
        let bw = big_info
            .windows
            .as_ref()
            .ok_or_else(|| Error::msg("check failed: Windows image recognised"))?;
        ensure(
            bw.over_4g && bw.install_size > plan::FAT32_MAX_FILE,
            "install.wim over 4 GiB",
        )?;
        let with_dism = plan::advise(&big_info, &env);
        ensure(
            matches!(with_dism.wim, plan::WimPlan::Split { parts: 2 })
                && with_dism.files_mode.is_ok(),
            "advice: split into 2 parts with DISM",
        )?;
        let without = plan::advise(&big_info, &Env::default());
        ensure(
            without.recommended.is_none(),
            "advice without DISM: no mode offered",
        )?;
        let iso_big = IsoImage::open(Box::new(MemSource(big)))?;
        let mut mem = MemDevice::new(64 * MIB, 512);
        let opts = file_options(Scheme::Gpt, "BIG");
        let r = write_files(&iso_big, &mut mem, &opts, &params, &cancel, &mut rep);
        ensure(
            matches!(r, Err(Error::Refused(_))) && mem.writes == 0,
            "a 5 GiB install.wim is refused untouched when no splitter is available",
        )?;
        ok(format!(
            "install.wim of {}: split planned with DISM, refused without it",
            human_size(bw.install_size)
        ));
    }

    // ---- 7. restore to an empty drive -------------------------------------------------
    {
        let mut dev = MemDevice::new(64 * MIB, 512);
        let opts = ResetOptions {
            label: "empty one".into(),
            ..ResetOptions::default()
        };
        reset_device(&mut dev, &opts, &cancel, &mut rep)?;
        let table = part::read_table(&mut dev)?;
        ensure(
            table.scheme == Some(Scheme::Mbr) && table.parts.len() == 1 && !table.parts[0].active,
            "MBR, one inactive partition",
        )?;
        let (o, s) = (table.parts[0].offset, table.parts[0].size);
        let mut vol = SubDevice::new(&mut dev, o, s).map_err(io("partition"))?;
        let fs = fat32::check(&mut vol)?;
        ensure(
            fs.problems.is_empty() && fs.label == "EMPTY ONE",
            "empty FAT32 volume",
        )?;

        let mut dev = MemDevice::new(96 * MIB, 512);
        let opts = ResetOptions {
            scheme: Scheme::Gpt,
            fs: FileSystem::ExFat,
            label: "Data".into(),
            quick_format: false,
            ..ResetOptions::default()
        };
        reset_device(&mut dev, &opts, &cancel, &mut rep)?;
        let table = part::read_table(&mut dev)?;
        ensure(
            table.scheme == Some(Scheme::Gpt) && table.problems.is_empty(),
            "GPT consistent",
        )?;
        let (o, s) = (table.parts[0].offset, table.parts[0].size);
        let mut vol = SubDevice::new(&mut dev, o, s).map_err(io("partition"))?;
        let fs = exfat::check(&mut vol)?;
        ensure(
            fs.problems.is_empty() && fs.label == "Data",
            "empty exFAT volume",
        )?;
        ok("restore to empty drive: MBR + FAT32 and GPT + exFAT parse back clean".into());
    }

    // ---- 8. backup ---------------------------------------------------------------------
    {
        let mut dev = FileDevice::open(&dev_path, 512).map_err(io("device file"))?;
        let out = dir.join("backup.img");
        let sums = backup_device(&mut dev, &out, &cancel, &mut rep)?;
        let again = hash::hash_file(&out, &cancel, &mut |_| {})?;
        let original = hash::hash_file(&dev_path, &cancel, &mut |_| {})?;
        ensure(
            sums == again && again == original,
            "backup is identical to the device",
        )?;
        ok(format!(
            "backup: {} read back, SHA-256 matches",
            human_size(dev.size())
        ));
    }

    // ---- 9. bad-block check ------------------------------------------------------------
    {
        let mut dev = MemDevice::new(8 * MIB, 512);
        let clean = imaging::bad_block_check(&mut dev, &cancel, &mut |_| {})?;
        ensure(clean.is_clean(), "healthy device passes")?;
        let mut dev = MemDevice::new(8 * MIB, 512);
        dev.bad_ranges.push(3 * MIB..3 * MIB + 4096);
        let opts = ResetOptions {
            bad_block_check: true,
            ..ResetOptions::default()
        };
        let r = reset_device(&mut dev, &opts, &cancel, &mut rep);
        ensure(r.is_err(), "a device with bad blocks is rejected")?;
        ok("bad-block check: clean device passes, damaged device is rejected".into());
    }

    // ---- 10. cancel --------------------------------------------------------------------
    {
        let stop = Cancel::new();
        stop.cancel();
        let mut dev = MemDevice::new(64 * MIB, 512);
        let opts = file_options(Scheme::Gpt, "X");
        let r = write_files(&iso_linux, &mut dev, &opts, &params, &stop, &mut rep);
        ensure(matches!(r, Err(Error::Cancelled)), "cancel stops file copy")?;
        let mut src = MemSource(linux.clone());
        let r = write_image_raw(
            &mut src,
            &mut dev,
            &WriteOptions::default(),
            &stop,
            &mut rep,
        );
        ensure(
            matches!(r, Err(Error::Cancelled)),
            "cancel stops image mode",
        )?;
        ok("cancel: both modes stop with a clean \"cancelled\" result".into());
    }

    let lines = events.try_iter().count();
    ok(format!("{lines} engine events were produced along the way"));
    log.push(
        "No physical drive was opened: every write went to image files in the temp folder.".into(),
    );
    Ok(())
}

/// `--inspect`: a readable report about an image.
pub fn inspect_report(path: &Path) -> std::result::Result<Vec<String>, String> {
    let info = iso::inspect(path).map_err(|e| e.to_string())?;
    let env = Env {
        dism_available: crate::sys::find_dism().is_some(),
    };
    let advice = plan::advise(&info, &env);
    let mut out = vec![advice.headline.clone()];
    out.push(format!(
        "  [{}]",
        advice
            .badges
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("] [")
    ));
    for (k, v) in &advice.facts {
        out.push(format!("  {k:<16} {v}"));
    }
    for e in &info.boot_entries {
        out.push(format!(
            "  El Torito        {:?}, {}, {}, LBA {}, {} sectors",
            e.platform,
            if e.bootable {
                "bootable"
            } else {
                "not bootable"
            },
            e.emulation,
            e.lba,
            e.sectors
        ));
    }
    for p in &info.partitions {
        out.push(format!(
            "  Partition        {} at {}, {}{}",
            p.kind,
            p.offset,
            human_size(p.size),
            if p.bootable { ", active" } else { "" }
        ));
    }
    if let Some((path, size)) = &info.largest_file {
        out.push(format!("  Largest file     {path} ({})", human_size(*size)));
    }
    out.push(format!(
        "  Image mode       {}",
        match &advice.image_mode {
            Ok(()) => "possible".to_string(),
            Err(e) => format!("no: {e}"),
        }
    ));
    out.push(format!(
        "  File-copy mode   {}",
        match &advice.files_mode {
            Ok(()) => "possible".to_string(),
            Err(e) => format!("no: {e}"),
        }
    ));
    out.push(format!(
        "  Recommended      {}",
        match advice.recommended {
            Some(WriteMode::Image) => "image mode",
            Some(WriteMode::Files) => "file copy",
            None => "nothing: this image cannot be made bootable by BootForge",
        }
    ));
    out.push(format!("  Why              {}", advice.why));
    for w in &advice.warnings {
        out.push(format!("  Note             {w}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn selftest_passes() {
        match super::run() {
            Ok(log) => assert!(log.len() >= 10, "{log:?}"),
            Err(e) => panic!("{e}"),
        }
    }
}
