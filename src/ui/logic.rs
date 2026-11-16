//! Pure UI logic: wording, option mappings and state folding. No egui here,
//! so everything can be unit-tested.

use crate::backend::HashVerdict;
use crate::engine::{Event, Stage};
use crate::model::{Disk, DiskIdentity, FileSystem, Partition, Scheme, WriteMode};
use crate::plan;
use crate::safety::{Listed, GIB};
use crate::util::{human_size, nominal_size};
use std::path::Path;

// --------------------------------------------------------------------------------------
// Option mappings
// --------------------------------------------------------------------------------------

/// The "largest drive offered" choices, in GiB.
pub const CAP_GIB: [u64; 7] = [32, 64, 128, 256, 512, 1024, 2048];
pub const CAP_LABELS: [&str; 7] = [
    "32 GB", "64 GB", "128 GB", "256 GB", "512 GB", "1 TB", "2 TB",
];

/// The choice that covers `max_size` (the smallest cap that is not below it).
pub fn cap_index(max_size: u64) -> usize {
    CAP_GIB
        .iter()
        .position(|g| g * GIB >= max_size)
        .unwrap_or(CAP_GIB.len() - 1)
}

pub fn cap_bytes(index: usize) -> u64 {
    CAP_GIB[index.min(CAP_GIB.len() - 1)] * GIB
}

/// Cluster size choices: default, then 4 to 32 KB.
pub const CLUSTERS: [Option<u32>; 5] = [None, Some(4096), Some(8192), Some(16384), Some(32768)];
pub const CLUSTER_LABELS: [&str; 5] = ["Default", "4 KB", "8 KB", "16 KB", "32 KB"];

pub fn cluster_index(cluster: Option<u32>) -> usize {
    CLUSTERS.iter().position(|c| *c == cluster).unwrap_or(0)
}

// --------------------------------------------------------------------------------------
// Labels
// --------------------------------------------------------------------------------------

/// What the label field keeps while the user types: at most 11 characters,
/// upper case for FAT32. Deliberately light, so the field can be emptied.
pub fn label_typing(text: &str, fs: FileSystem) -> String {
    let cut: String = text.chars().take(11).collect();
    match fs {
        FileSystem::Fat32 => cut.to_uppercase().chars().take(11).collect(),
        FileSystem::ExFat => cut,
    }
}

/// The label that is really written.
pub fn final_label(text: &str, fs: FileSystem) -> String {
    match fs {
        FileSystem::Fat32 => plan::fat_label(text),
        FileSystem::ExFat => {
            let t: String = text.trim().chars().take(11).collect();
            let t = t.trim().to_string();
            if t.is_empty() {
                "USB DRIVE".into()
            } else {
                t
            }
        }
    }
}

// --------------------------------------------------------------------------------------
// Wording
// --------------------------------------------------------------------------------------

/// File name of a path, for display (never the folder).
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// "62,008,590,336".
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "57.8 GB (62,008,590,336 bytes)".
pub fn exact_size(bytes: u64) -> String {
    format!("{} ({} bytes)", human_size(bytes), thousands(bytes))
}

/// "E: KESTREL (FAT32, 57.7 GB)".
pub fn partition_line(p: &Partition) -> String {
    let mut name = String::new();
    if let Some(l) = p.letter {
        name.push(l);
        name.push(':');
    }
    let title = if !p.label.trim().is_empty() {
        p.label.trim()
    } else if p.letter.is_none() {
        p.kind.trim()
    } else {
        ""
    };
    if !title.is_empty() {
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(title);
    }
    if name.is_empty() {
        name = format!("Partition {}", p.number);
    }
    let size = human_size(p.size);
    if p.fs.trim().is_empty() {
        format!("{name} ({size})")
    } else {
        format!("{name} ({}, {size})", p.fs.trim())
    }
}

/// All partitions of a disk on one line.
pub fn partitions_line(disk: &Disk) -> String {
    if disk.partitions.is_empty() {
        return "No partitions".into();
    }
    disk.partitions
        .iter()
        .map(partition_line)
        .collect::<Vec<_>>()
        .join("  ·  ")
}

/// The line under a drive list about the disks the safety rules left out.
/// `short` drops the examples (narrow columns).
pub fn hidden_text(hidden: usize, short: bool) -> String {
    let subject = if hidden == 1 {
        "1 other disk is".to_string()
    } else {
        format!("{hidden} other disks are")
    };
    if short {
        format!("{subject} hidden by the safety rules.")
    } else {
        format!("{subject} hidden by the safety rules (internal disks, the Windows disk, …).")
    }
}

pub fn scheme_text(scheme: Option<Scheme>) -> &'static str {
    match scheme {
        Some(s) => s.label(),
        None => "Not partitioned",
    }
}

/// "Kestrel Swift 3.2 (64 GB)".
pub fn disk_title(disk: &Disk) -> String {
    format!("{} ({})", disk.name(), nominal_size(disk.size))
}

/// The sentence in the action row of the Create page.
pub fn summary(
    image: Option<&str>,
    disk: Option<&Disk>,
    mode: WriteMode,
    scheme: Scheme,
) -> String {
    match (image, disk) {
        (None, None) => "Choose an image and a drive.".into(),
        (None, Some(_)) => "Choose an image.".into(),
        (Some(name), None) => format!("{name}: choose the drive to write it to."),
        (Some(name), Some(d)) => {
            let how = match mode {
                WriteMode::Image => "image mode".to_string(),
                WriteMode::Files => format!("file copy, {}", scheme.label()),
            };
            format!("{name} → {}, {how}", disk_title(d))
        }
    }
}

/// Why the Write button is disabled, or `None` when it may be pressed.
pub fn write_blocker(
    has_image: bool,
    drive_ok: bool,
    blocking_issue: bool,
) -> Option<&'static str> {
    if !has_image {
        Some("Choose an image first.")
    } else if !drive_ok {
        Some("Choose a drive first.")
    } else if blocking_issue {
        Some("Fix the problem shown under Options first.")
    } else {
        None
    }
}

/// Arguments for restarting elevated with the same selection.
pub fn relaunch_args(image: Option<&Path>, disk: Option<u32>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(p) = image {
        args.push("--image".to_string());
        args.push(p.display().to_string());
    }
    if let Some(n) = disk {
        args.push("--disk".to_string());
        args.push(n.to_string());
    }
    args
}

/// How a checksum verdict is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerdictTone {
    Good,
    Bad,
    Muted,
}

pub fn verdict_text(v: HashVerdict) -> Option<(String, VerdictTone)> {
    match v {
        HashVerdict::Empty => None,
        HashVerdict::Match(algo) => Some((format!("Matches {algo}"), VerdictTone::Good)),
        HashVerdict::Mismatch => Some(("Does not match".into(), VerdictTone::Bad)),
        HashVerdict::NotAHash => Some(("Not a checksum".into(), VerdictTone::Muted)),
    }
}

// --------------------------------------------------------------------------------------
// Drive selection across refreshes
// --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionCheck {
    /// Still the same device and still offered.
    Keep,
    /// No disk with that number any more.
    Removed,
    /// A disk with that number exists, but it is a different device.
    Changed,
    /// Same device, but the safety rules no longer offer it.
    NotOffered,
}

/// Checks a remembered selection against a fresh enumeration.
pub fn check_selection(sel: &DiskIdentity, disks: &[Disk], listed: &[Listed]) -> SelectionCheck {
    let Some(now) = disks.iter().find(|d| d.number == sel.number) else {
        // The same stick may come back under another number: that is still
        // "removed" for us, the user has to pick it again.
        return SelectionCheck::Removed;
    };
    if now.identity() != *sel {
        return SelectionCheck::Changed;
    }
    let offered = listed
        .iter()
        .any(|l| l.blocked.is_none() && l.disk.identity() == *sel);
    if offered {
        SelectionCheck::Keep
    } else {
        SelectionCheck::NotOffered
    }
}

/// Applies [`check_selection`] to a selection; returns the message for the
/// toast when the selection had to be dropped.
pub fn reconcile(
    sel: &mut Option<DiskIdentity>,
    disks: &[Disk],
    listed: &[Listed],
) -> Option<String> {
    let current = sel.as_ref()?;
    let text = match check_selection(current, disks, listed) {
        SelectionCheck::Keep => return None,
        SelectionCheck::Removed => "The selected drive was removed. Choose a drive again.",
        SelectionCheck::Changed => {
            "The selected drive is no longer the same device. Choose a drive again."
        }
        SelectionCheck::NotOffered => {
            "The selected drive can no longer be used. Choose a drive again."
        }
    };
    *sel = None;
    Some(text.to_string())
}

/// The listed, selectable disk a selection points to.
pub fn selected_disk<'a>(sel: &Option<DiskIdentity>, listed: &'a [Listed]) -> Option<&'a Disk> {
    let sel = sel.as_ref()?;
    listed
        .iter()
        .find(|l| l.blocked.is_none() && l.disk.identity() == *sel)
        .map(|l| &l.disk)
}

// --------------------------------------------------------------------------------------
// Job progress
// --------------------------------------------------------------------------------------

/// What the progress screen shows, folded from the engine's events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Index into the job's stage list; `None` before the first stage event.
    pub stage: Option<usize>,
    pub done: u64,
    pub total: u64,
    pub item: String,
}

impl Progress {
    /// Folds one event in. Returns true when a new stage began (the caller
    /// resets its speed meter then).
    pub fn fold(&mut self, stages: &[Stage], event: &Event) -> bool {
        match event {
            Event::Stage(s) => {
                // Stages only move forward; an unknown stage keeps the position.
                let from = self.stage.map(|i| i + 1).unwrap_or(0);
                let found = stages
                    .iter()
                    .enumerate()
                    .skip(from)
                    .find(|(_, x)| *x == s)
                    .map(|(i, _)| i)
                    .or_else(|| stages.iter().position(|x| x == s));
                if let Some(i) = found {
                    self.stage = Some(i);
                }
                self.done = 0;
                self.total = 0;
                self.item.clear();
                true
            }
            Event::Progress { done, total } => {
                self.done = *done;
                self.total = *total;
                false
            }
            Event::Item(text) => {
                self.item = text.clone();
                false
            }
            Event::Log(_) | Event::Finished(_) => false,
        }
    }

    /// 0..=1 within the current stage; `None` when the amount is unknown.
    pub fn fraction(&self) -> Option<f32> {
        if self.total == 0 {
            None
        } else {
            Some((self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32)
        }
    }

    /// 0..=1 over the whole job, by stage index.
    pub fn overall(&self, stage_count: usize) -> f32 {
        if stage_count == 0 {
            return 0.0;
        }
        let Some(i) = self.stage else { return 0.0 };
        ((i as f32 + self.fraction().unwrap_or(0.0)) / stage_count as f32).clamp(0.0, 1.0)
    }

    /// "3.86 GB of 6.22 GB".
    pub fn amount(&self) -> String {
        if self.total == 0 {
            String::new()
        } else {
            format!("{} of {}", human_size(self.done), human_size(self.total))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;
    use crate::engine::Outcome;
    use crate::safety::{listable, Policy};

    #[test]
    fn size_cap_mapping() {
        assert_eq!(cap_index(256 * GIB), 3);
        assert_eq!(cap_index(crate::safety::DEFAULT_MAX_SIZE), 3);
        assert_eq!(cap_index(1), 0);
        assert_eq!(cap_index(100 * GIB), 2);
        assert_eq!(cap_index(u64::MAX), 6);
        for i in 0..CAP_GIB.len() {
            assert_eq!(cap_index(cap_bytes(i)), i);
        }
        assert_eq!(cap_bytes(99), 2048 * GIB);
        assert_eq!(cap_bytes(6), crate::safety::HARD_MAX_SIZE);
        assert_eq!(CAP_GIB.len(), CAP_LABELS.len());
    }

    #[test]
    fn cluster_mapping() {
        assert_eq!(cluster_index(None), 0);
        assert_eq!(cluster_index(Some(32768)), 4);
        assert_eq!(cluster_index(Some(12345)), 0);
        assert_eq!(CLUSTERS.len(), CLUSTER_LABELS.len());
    }

    #[test]
    fn labels() {
        assert_eq!(
            label_typing("my usb stick 123", FileSystem::Fat32),
            "MY USB STIC"
        );
        assert_eq!(label_typing("", FileSystem::Fat32), "");
        assert_eq!(label_typing("Data ", FileSystem::ExFat), "Data ");
        assert_eq!(final_label("win 11?", FileSystem::Fat32), "WIN 11_");
        assert_eq!(final_label("  ", FileSystem::Fat32), "BOOTFORGE");
        assert_eq!(
            final_label(" Photos 2026 backup ", FileSystem::ExFat),
            "Photos 2026"
        );
        assert_eq!(final_label("", FileSystem::ExFat), "USB DRIVE");
    }

    #[test]
    fn wording() {
        assert_eq!(
            hidden_text(3, false),
            "3 other disks are hidden by the safety rules (internal disks, the Windows disk, …)."
        );
        assert_eq!(
            hidden_text(1, true),
            "1 other disk is hidden by the safety rules."
        );
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(62_008_590_336), "62,008,590,336");
        assert_eq!(exact_size(62_008_590_336), "57.8 GB (62,008,590,336 bytes)");
        assert_eq!(
            file_name(Path::new("D:\\Images\\Windows11_Setup_x64.iso")),
            "Windows11_Setup_x64.iso"
        );
        let p = Partition {
            number: 1,
            size: 62_007_541_760,
            kind: "FAT32 LBA".into(),
            letter: Some('E'),
            label: "KESTREL".into(),
            fs: "FAT32".into(),
            ..Default::default()
        };
        assert_eq!(partition_line(&p), "E: KESTREL (FAT32, 57.7 GB)");
        let efi = Partition {
            number: 1,
            size: 260 * 1024 * 1024,
            kind: "EFI System".into(),
            fs: "FAT32".into(),
            ..Default::default()
        };
        assert_eq!(partition_line(&efi), "EFI System (FAT32, 260 MB)");
        let bare = Partition {
            number: 3,
            size: 1024,
            ..Default::default()
        };
        assert_eq!(partition_line(&bare), "Partition 3 (1.00 KB)");
    }

    #[test]
    fn summary_sentence() {
        let disks = demo::disks();
        let kestrel = disks.iter().find(|d| d.vendor == "Kestrel").unwrap();
        assert_eq!(
            summary(
                Some("Windows11_Setup_x64.iso"),
                Some(kestrel),
                WriteMode::Files,
                Scheme::Gpt
            ),
            "Windows11_Setup_x64.iso → Kestrel Swift 3.2 (64 GB), file copy, GPT"
        );
        assert_eq!(
            summary(Some("a.iso"), Some(kestrel), WriteMode::Image, Scheme::Mbr),
            "a.iso → Kestrel Swift 3.2 (64 GB), image mode"
        );
        assert_eq!(
            summary(None, None, WriteMode::Image, Scheme::Gpt),
            "Choose an image and a drive."
        );
        assert!(summary(Some("a.iso"), None, WriteMode::Image, Scheme::Gpt).contains("a.iso"));
        assert_eq!(write_blocker(true, true, false), None);
        assert!(write_blocker(false, true, false).is_some());
        assert!(write_blocker(true, false, false).is_some());
        assert!(write_blocker(true, true, true).is_some());
    }

    #[test]
    fn relaunch_arguments() {
        assert!(relaunch_args(None, None).is_empty());
        assert_eq!(
            relaunch_args(Some(Path::new("D:\\a b.iso")), Some(2)),
            ["--image", "D:\\a b.iso", "--disk", "2"]
        );
    }

    #[test]
    fn verdicts() {
        assert_eq!(verdict_text(HashVerdict::Empty), None);
        assert_eq!(
            verdict_text(HashVerdict::Match("SHA-256")),
            Some(("Matches SHA-256".to_string(), VerdictTone::Good))
        );
        assert_eq!(
            verdict_text(HashVerdict::Mismatch).unwrap().1,
            VerdictTone::Bad
        );
        assert_eq!(
            verdict_text(HashVerdict::NotAHash).unwrap().1,
            VerdictTone::Muted
        );
    }

    #[test]
    fn selection_survives_only_the_same_offered_device() {
        let policy = Policy::default();
        let disks = demo::disks();
        let (listed, _) = listable(&disks, &policy, &[]);
        let kestrel = disks
            .iter()
            .find(|d| d.vendor == "Kestrel")
            .unwrap()
            .clone();

        let mut sel = Some(kestrel.identity());
        assert_eq!(reconcile(&mut sel, &disks, &listed), None);
        assert!(sel.is_some());
        assert_eq!(selected_disk(&sel, &listed).unwrap().number, kestrel.number);

        // Unplugged.
        let without: Vec<Disk> = disks
            .iter()
            .filter(|d| d.number != kestrel.number)
            .cloned()
            .collect();
        let (l2, _) = listable(&without, &policy, &[]);
        let mut sel = Some(kestrel.identity());
        assert!(reconcile(&mut sel, &without, &l2)
            .unwrap()
            .contains("removed"));
        assert!(sel.is_none());

        // Another stick took the same disk number.
        let mut swapped = disks.clone();
        for d in &mut swapped {
            if d.number == kestrel.number {
                d.serial = "DEMO-OTHER".into();
            }
        }
        let (l3, _) = listable(&swapped, &policy, &[]);
        assert_eq!(
            check_selection(&kestrel.identity(), &swapped, &l3),
            SelectionCheck::Changed
        );
        // Same model and serial but a different size is a different device too.
        let mut resized = disks.clone();
        for d in &mut resized {
            if d.number == kestrel.number {
                d.size -= 512;
            }
        }
        let (l4, _) = listable(&resized, &policy, &[]);
        let mut sel = Some(kestrel.identity());
        assert!(reconcile(&mut sel, &resized, &l4).is_some());
        assert!(sel.is_none());

        // Became write-protected: listed but blocked.
        let mut locked = disks.clone();
        for d in &mut locked {
            if d.number == kestrel.number {
                d.read_only = true;
            }
        }
        let (l5, _) = listable(&locked, &policy, &[]);
        assert_eq!(
            check_selection(&kestrel.identity(), &locked, &l5),
            SelectionCheck::NotOffered
        );
        assert!(selected_disk(&Some(kestrel.identity()), &l5).is_none());

        // Nothing selected: nothing to say.
        let mut none = None;
        assert_eq!(reconcile(&mut none, &disks, &listed), None);
    }

    #[test]
    fn events_fold_into_progress() {
        let stages = [
            Stage::Checking,
            Stage::Locking,
            Stage::CopyingFiles,
            Stage::Verifying,
            Stage::Finishing,
        ];
        let mut p = Progress::default();
        assert_eq!(p.overall(stages.len()), 0.0);
        assert!(p.fold(&stages, &Event::Stage(Stage::Checking)));
        assert_eq!(p.stage, Some(0));
        assert_eq!(p.fraction(), None);
        assert!(p.fold(&stages, &Event::Stage(Stage::Locking)));
        assert!(p.fold(&stages, &Event::Stage(Stage::CopyingFiles)));
        assert!(!p.fold(&stages, &Event::Item("sources/install.wim".into())));
        assert!(!p.fold(
            &stages,
            &Event::Progress {
                done: 50,
                total: 200
            }
        ));
        assert_eq!(p.stage, Some(2));
        assert_eq!(p.fraction(), Some(0.25));
        assert_eq!(p.item, "sources/install.wim");
        assert!((p.overall(stages.len()) - 2.25 / 5.0).abs() < 1e-6);
        assert_eq!(p.amount(), "50 bytes of 200 bytes");
        assert!(!p.fold(&stages, &Event::Log("x".into())));
        assert!(!p.fold(&stages, &Event::Finished(Outcome::Cancelled(String::new()))));
        assert_eq!(p.stage, Some(2));

        // A new stage restarts the numbers and clears the item.
        p.fold(&stages, &Event::Stage(Stage::Verifying));
        assert_eq!((p.stage, p.done, p.total), (Some(3), 0, 0));
        assert!(p.item.is_empty());
        assert_eq!(p.amount(), "");
        // A stage the list does not know keeps the position.
        p.fold(&stages, &Event::Stage(Stage::BadBlocks));
        assert_eq!(p.stage, Some(3));
        // Over-reporting is clamped.
        p.fold(
            &stages,
            &Event::Progress {
                done: 300,
                total: 200,
            },
        );
        assert_eq!(p.fraction(), Some(1.0));
        assert_eq!(Progress::default().overall(0), 0.0);
    }

    #[test]
    fn repeated_stages_move_forward() {
        // Formatting can appear once per partition in principle: the second
        // event must not jump back to the first entry.
        let stages = [Stage::Formatting, Stage::CopyingFiles, Stage::Formatting];
        let mut p = Progress::default();
        p.fold(&stages, &Event::Stage(Stage::Formatting));
        assert_eq!(p.stage, Some(0));
        p.fold(&stages, &Event::Stage(Stage::CopyingFiles));
        p.fold(&stages, &Event::Stage(Stage::Formatting));
        assert_eq!(p.stage, Some(2));
    }
}
