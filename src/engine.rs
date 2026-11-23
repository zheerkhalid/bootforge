//! The job engine: runs one destructive (or reading) operation on a
//! background thread and reports stages, progress and log lines.
//!
//! The UI talks to the engine only through [`Job`], [`start`] / [`start_demo`]
//! and the [`Event`] stream of the returned [`JobHandle`].

use crate::model::{Cancel, DiskIdentity, FileSystem, ImageInfo, Scheme, WriteMode, WriteOptions};
use crate::safety::Policy;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

mod run;

pub use run::{
    backup_device, reset_device, run_on_device, write_files, write_image_raw, DismSplitter,
    FilesParams, FilesReport, WimSplitter,
};

/// Options for "restore the drive to a normal empty drive".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetOptions {
    pub scheme: Scheme,
    pub fs: FileSystem,
    pub cluster_size: Option<u32>,
    pub label: String,
    pub quick_format: bool,
    pub bad_block_check: bool,
    pub eject: bool,
}

impl Default for ResetOptions {
    fn default() -> Self {
        ResetOptions {
            scheme: Scheme::Mbr,
            fs: FileSystem::Fat32,
            cluster_size: None,
            label: "USB DRIVE".into(),
            quick_format: true,
            bad_block_check: false,
            eject: false,
        }
    }
}

/// One operation. (Jobs are created once per click; their size does not matter.)
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Job {
    /// Write an image to a drive (create bootable media, or restore a backup
    /// when `options.mode` is `Image`).
    Write {
        info: ImageInfo,
        disk: DiskIdentity,
        options: WriteOptions,
    },
    /// Read a whole drive into an image file.
    Backup { disk: DiskIdentity, out: PathBuf },
    /// Clean the drive and give it one empty partition.
    Reset {
        disk: DiskIdentity,
        options: ResetOptions,
    },
}

impl Job {
    pub fn disk(&self) -> &DiskIdentity {
        match self {
            Job::Write { disk, .. } | Job::Backup { disk, .. } | Job::Reset { disk, .. } => disk,
        }
    }

    /// Whether the job erases the drive.
    pub fn destructive(&self) -> bool {
        !matches!(self, Job::Backup { .. })
    }

    /// Short title for the progress screen.
    pub fn title(&self) -> String {
        match self {
            Job::Write { info, options, .. } => {
                let name = info
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match options.mode {
                    WriteMode::Image => format!("Writing {name}"),
                    WriteMode::Files => format!("Creating bootable drive from {name}"),
                }
            }
            Job::Backup { .. } => "Backing up the drive".into(),
            Job::Reset { .. } => "Restoring the drive to an empty drive".into(),
        }
    }

    /// The stages this job will go through, in order.
    pub fn stages(&self) -> Vec<Stage> {
        let mut s = vec![Stage::Checking];
        match self {
            Job::Write { info, options, .. } => {
                s.push(Stage::Locking);
                if options.bad_block_check {
                    s.push(Stage::BadBlocks);
                }
                match options.mode {
                    WriteMode::Image => {
                        s.push(Stage::WritingImage);
                        if options.verify {
                            s.push(Stage::Verifying);
                        }
                    }
                    WriteMode::Files => {
                        let split = info
                            .windows
                            .as_ref()
                            .map(|w| w.install_size > crate::plan::FAT32_MAX_FILE)
                            .unwrap_or(false);
                        if split {
                            s.push(Stage::SplittingWim);
                        }
                        s.push(Stage::Partitioning);
                        s.push(Stage::Formatting);
                        s.push(Stage::CopyingFiles);
                        if options.verify {
                            s.push(Stage::Verifying);
                        }
                    }
                }
                s.push(Stage::Finishing);
            }
            Job::Backup { .. } => {
                s.push(Stage::ReadingDrive);
                s.push(Stage::Finishing);
            }
            Job::Reset { options, .. } => {
                s.push(Stage::Locking);
                if options.bad_block_check {
                    s.push(Stage::BadBlocks);
                }
                s.push(Stage::Partitioning);
                s.push(Stage::Formatting);
                s.push(Stage::Finishing);
            }
        }
        s
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Re-enumerating and re-checking the device against the safety rules.
    Checking,
    /// Locking and dismounting the volumes on the drive.
    Locking,
    BadBlocks,
    SplittingWim,
    Partitioning,
    Formatting,
    WritingImage,
    CopyingFiles,
    Verifying,
    ReadingDrive,
    /// Flushing, unlocking, letting Windows re-read the drive, ejecting.
    Finishing,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Checking => "Checking the drive",
            Stage::Locking => "Locking the drive",
            Stage::BadBlocks => "Checking for bad blocks",
            Stage::SplittingWim => "Splitting install.wim",
            Stage::Partitioning => "Partitioning",
            Stage::Formatting => "Formatting",
            Stage::WritingImage => "Writing the image",
            Stage::CopyingFiles => "Copying files",
            Stage::Verifying => "Verifying",
            Stage::ReadingDrive => "Reading the drive",
            Stage::Finishing => "Finishing",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub headline: String,
    pub lines: Vec<String>,
    pub seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done(Summary),
    /// Cancelled by the user; the text says what state the drive is in.
    Cancelled(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A new stage began (progress restarts at zero).
    Stage(Stage),
    /// Bytes done / total within the current stage. `total == 0` means the
    /// amount is unknown (show an indeterminate bar).
    Progress { done: u64, total: u64 },
    /// What is being worked on right now (a file name, a pass description).
    Item(String),
    /// A line for the log.
    Log(String),
    /// Always the last event.
    Finished(Outcome),
}

/// The receiving end of a running job.
pub struct JobHandle {
    pub rx: Receiver<Event>,
    pub cancel: Cancel,
}

/// Where a job sends its events.
pub struct Reporter {
    tx: Sender<Event>,
    wake: Box<dyn Fn() + Send>,
    last_progress: Option<Instant>,
}

impl Reporter {
    pub fn new(tx: Sender<Event>, wake: Box<dyn Fn() + Send>) -> Reporter {
        Reporter {
            tx,
            wake,
            last_progress: None,
        }
    }

    /// A reporter that collects into a channel without waking anything (tests, self-test).
    pub fn collecting() -> (Reporter, Receiver<Event>) {
        let (tx, rx) = channel();
        (Reporter::new(tx, Box::new(|| {})), rx)
    }

    pub fn send(&mut self, e: Event) {
        let _ = self.tx.send(e);
        (self.wake)();
    }
    pub fn stage(&mut self, s: Stage) {
        self.last_progress = None;
        self.send(Event::Log(format!("== {}", s.label())));
        self.send(Event::Stage(s));
    }
    pub fn log(&mut self, text: impl Into<String>) {
        self.send(Event::Log(text.into()));
    }
    pub fn item(&mut self, text: impl Into<String>) {
        self.send(Event::Item(text.into()));
    }
    /// Rate-limited to about 20 events a second; the final value always goes out.
    pub fn progress(&mut self, done: u64, total: u64) {
        let now = Instant::now();
        let due = self
            .last_progress
            .map(|t| now.duration_since(t) >= Duration::from_millis(50))
            .unwrap_or(true);
        // With an unknown total (0) there is no "final value": rate-limit only.
        if due || (total > 0 && done >= total) {
            self.last_progress = Some(now);
            self.send(Event::Progress { done, total });
        }
    }
}

/// Smoothed speed and time-remaining estimate from progress samples.
#[derive(Debug, Default)]
pub struct SpeedMeter {
    samples: VecDeque<(f64, u64)>,
}

impl SpeedMeter {
    /// Seconds of history the speed is averaged over.
    const WINDOW: f64 = 6.0;

    pub fn new() -> SpeedMeter {
        SpeedMeter::default()
    }
    pub fn reset(&mut self) {
        self.samples.clear();
    }
    /// Adds a sample: `seconds` since any fixed origin, `done` bytes so far.
    pub fn add(&mut self, seconds: f64, done: u64) {
        if let Some(&(_, last)) = self.samples.back() {
            if done < last {
                self.samples.clear();
            }
        }
        self.samples.push_back((seconds, done));
        while self.samples.len() > 2
            && seconds - self.samples.front().map(|s| s.0).unwrap_or(seconds) > Self::WINDOW
        {
            self.samples.pop_front();
        }
    }
    /// Bytes per second over the window; `None` until there is enough data.
    pub fn speed(&self) -> Option<f64> {
        let (t0, d0) = *self.samples.front()?;
        let (t1, d1) = *self.samples.back()?;
        if t1 - t0 < 0.4 || d1 <= d0 {
            return None;
        }
        Some((d1 - d0) as f64 / (t1 - t0))
    }
    /// Seconds left for `total` bytes.
    pub fn eta(&self, total: u64) -> Option<u64> {
        let speed = self.speed()?;
        let done = self.samples.back()?.1;
        if total <= done {
            return Some(0);
        }
        Some(((total - done) as f64 / speed).ceil() as u64)
    }
}

/// Starts a job on a background thread. `wake` is called after every event
/// (the UI passes a repaint request).
///
/// The device is re-enumerated and re-checked against `policy` and the
/// confirmed identity inside the thread, immediately before it is opened.
pub fn start(job: Job, policy: Policy, wake: impl Fn() + Send + 'static) -> JobHandle {
    let (tx, rx) = channel();
    let cancel = Cancel::new();
    let c = cancel.clone();
    std::thread::spawn(move || {
        // A PC that goes to sleep in the middle of a write leaves a broken drive.
        let _awake = crate::sys::KeepAwake::new();
        let mut rep = Reporter::new(tx, Box::new(wake));
        let outcome = run::run_real(&job, &policy, &c, &mut rep);
        rep.send(Event::Finished(outcome));
    });
    JobHandle { rx, cancel }
}

/// Plays a job's stages with invented progress. Nothing is opened, read or
/// written. Used by `--demo`.
pub fn start_demo(job: Job, wake: impl Fn() + Send + 'static) -> JobHandle {
    let (tx, rx) = channel();
    let cancel = Cancel::new();
    let c = cancel.clone();
    std::thread::spawn(move || {
        let mut rep = Reporter::new(tx, Box::new(wake));
        let started = Instant::now();
        let total: u64 = match &job {
            Job::Write { info, .. } => info.size.max(1),
            _ => job.disk().size.max(1),
        };
        rep.log(format!(
            "Demo mode: simulating \"{}\". No drive is touched.",
            job.title()
        ));
        for stage in job.stages() {
            rep.stage(stage);
            let (steps, bytes) = match stage {
                Stage::WritingImage | Stage::CopyingFiles | Stage::ReadingDrive => (160, total),
                Stage::Verifying | Stage::BadBlocks => (100, total),
                Stage::SplittingWim => (60, total),
                Stage::Formatting => (12, 0),
                _ => (6, 0),
            };
            const ITEMS: [&str; 6] = [
                "boot/bcd",
                "efi/boot/bootx64.efi",
                "sources/boot.wim",
                "sources/install.swm",
                "sources/install2.swm",
                "support/readme.txt",
            ];
            for i in 1..=steps {
                if matches!(stage, Stage::CopyingFiles | Stage::Verifying) && i % 20 == 1 {
                    rep.item(ITEMS[(i as usize / 20) % ITEMS.len()]);
                }
                if c.is_cancelled() {
                    rep.log("Cancelled.");
                    rep.send(Event::Finished(Outcome::Cancelled(
                        "Demo mode: nothing was written.".into(),
                    )));
                    return;
                }
                std::thread::sleep(Duration::from_millis(45));
                if bytes > 0 {
                    rep.progress(bytes / steps * i, bytes);
                }
            }
        }
        rep.log("Done.");
        rep.send(Event::Finished(Outcome::Done(Summary {
            headline: "Finished (demo)".into(),
            lines: vec!["Demo mode: no drive was touched.".into()],
            seconds: started.elapsed().as_secs(),
        })));
    });
    JobHandle { rx, cancel }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_meter_estimates() {
        let mut m = SpeedMeter::new();
        assert_eq!(m.speed(), None);
        m.add(0.0, 0);
        m.add(0.2, 2_000_000);
        assert_eq!(m.speed(), None); // not enough time yet
        m.add(1.0, 10_000_000);
        m.add(2.0, 20_000_000);
        let s = m.speed().unwrap();
        assert!((s - 10_000_000.0).abs() < 1.0);
        assert_eq!(m.eta(50_000_000), Some(3));
        assert_eq!(m.eta(20_000_000), Some(0));
        // Old samples leave the window: a change of pace shows up.
        for i in 3..20 {
            m.add(i as f64, 20_000_000 + (i - 2) * 1_000_000);
        }
        let s = m.speed().unwrap();
        assert!((s - 1_000_000.0).abs() < 1.0, "{s}");
        // Going backwards (a new stage) starts over.
        m.add(21.0, 5);
        assert_eq!(m.speed(), None);
    }

    #[test]
    fn progress_is_rate_limited_but_the_final_value_always_arrives() {
        let (mut rep, rx) = Reporter::collecting();
        for i in 0..=1000u64 {
            rep.progress(i, 1000);
        }
        let known: Vec<Event> = rx.try_iter().collect();
        assert!(known.len() < 100, "{}", known.len());
        assert_eq!(
            known.last(),
            Some(&Event::Progress {
                done: 1000,
                total: 1000
            })
        );
        // An unknown total (an indeterminate bar) must not flood the window.
        rep.stage(Stage::Formatting);
        let _ = rx.try_iter().count();
        for i in 0..1000u64 {
            rep.progress(i, 0);
        }
        assert!(rx.try_iter().count() < 100);
    }

    #[test]
    fn stage_plans() {
        let disk = DiskIdentity {
            number: 9,
            serial: "X".into(),
            model: "M".into(),
            size: 1 << 30,
        };
        let info = ImageInfo::new(PathBuf::from("a.iso"), 1 << 20);
        let mut options = WriteOptions::default();
        let job = Job::Write {
            info: info.clone(),
            disk: disk.clone(),
            options: options.clone(),
        };
        assert_eq!(
            job.stages(),
            vec![
                Stage::Checking,
                Stage::Locking,
                Stage::WritingImage,
                Stage::Verifying,
                Stage::Finishing
            ]
        );
        assert!(job.destructive());
        options.mode = WriteMode::Files;
        options.verify = false;
        options.bad_block_check = true;
        let job = Job::Write {
            info,
            disk: disk.clone(),
            options,
        };
        assert_eq!(
            job.stages(),
            vec![
                Stage::Checking,
                Stage::Locking,
                Stage::BadBlocks,
                Stage::Partitioning,
                Stage::Formatting,
                Stage::CopyingFiles,
                Stage::Finishing
            ]
        );
        let job = Job::Backup {
            disk,
            out: PathBuf::from("b.img"),
        };
        assert!(!job.destructive());
        assert_eq!(job.stages().len(), 3);
    }

    #[test]
    fn demo_job_finishes_and_can_be_cancelled() {
        let disk = DiskIdentity {
            number: 9,
            serial: "X".into(),
            model: "M".into(),
            size: 1 << 30,
        };
        let h = start_demo(
            Job::Reset {
                disk,
                options: ResetOptions::default(),
            },
            || {},
        );
        h.cancel.cancel();
        let mut last = None;
        while let Ok(e) = h.rx.recv_timeout(Duration::from_secs(20)) {
            last = Some(e);
        }
        assert!(matches!(last, Some(Event::Finished(Outcome::Cancelled(_)))));
    }
}
