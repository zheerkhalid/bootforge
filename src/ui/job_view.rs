//! Everything around a job: the confirmation dialogs, the progress view and
//! the result view, plus the event pump that feeds them.

use super::app::{App, Tab};
use super::logic::{self, Progress};
use super::parts;
use super::theme::{pal, pal_ctx, regular, semibold, Icon};
use super::widgets;
use crate::engine::{self, Event, Job, JobHandle, Outcome, SpeedMeter, Stage, Summary};
use crate::model::{Disk, WriteMode};
use crate::util::{human_duration, human_size, human_speed, nominal_size};
use crate::{demo, logbook};
use eframe::egui::{
    self, pos2, vec2, CornerRadius, FontFamily, FontId, Frame, Margin, RichText, Sense, Stroke, Ui,
    Vec2,
};
use std::collections::VecDeque;
use std::sync::mpsc::TryRecvError;
use std::time::Instant;

/// A job waiting for the user's confirmation.
pub struct PendingJob {
    pub job: Job,
    /// The drive as it was listed when the job was requested (for the dialog).
    pub disk: Disk,
    /// The "I understand" tick box.
    pub understood: bool,
    pub origin: Tab,
}

/// A job that is running or has just ended.
pub struct Running {
    pub title: String,
    pub drive: String,
    pub destructive: bool,
    pub stages: Vec<Stage>,
    /// `None` for the staged screenshot views.
    pub handle: Option<JobHandle>,
    pub progress: Progress,
    pub meter: SpeedMeter,
    pub started: Instant,
    /// The last few log lines.
    pub tail: VecDeque<String>,
    /// The "really cancel?" dialog is open.
    pub ask_cancel: bool,
    pub cancelling: bool,
    pub outcome: Option<Outcome>,
    pub elapsed: u64,
    /// Where the log file went (or why it could not be written).
    pub log_note: Option<String>,
    pub origin: Tab,
    /// Fixed speed (bytes per second) and seconds left, for screenshots.
    pub staged: Option<(f64, u64)>,
}

impl Running {
    const TAIL: usize = 8;

    fn new(job: &Job, disk: &Disk, origin: Tab, handle: Option<JobHandle>) -> Running {
        Running {
            title: job.title(),
            drive: logic::disk_title(disk),
            destructive: job.destructive(),
            stages: job.stages(),
            handle,
            progress: Progress::default(),
            meter: SpeedMeter::new(),
            started: Instant::now(),
            tail: VecDeque::new(),
            ask_cancel: false,
            cancelling: false,
            outcome: None,
            elapsed: 0,
            log_note: None,
            origin,
            staged: None,
        }
    }

    fn push_tail(&mut self, text: &str) {
        for line in text.lines() {
            self.tail.push_back(line.to_string());
        }
        while self.tail.len() > Self::TAIL {
            self.tail.pop_front();
        }
    }

    /// Asks the job to stop; it ends with `Event::Finished` when it has.
    pub fn request_cancel(&mut self) {
        if let Some(h) = &self.handle {
            h.cancel.cancel();
        }
        self.cancelling = true;
        self.ask_cancel = false;
    }
}

fn job_intro(job: &Job) -> String {
    match job {
        Job::Write { info, options, .. } => {
            let name = logic::file_name(&info.path);
            match options.mode {
                WriteMode::Image => format!(
                    "{name} will be written to this drive byte for byte. Everything that is on the drive now is lost."
                ),
WriteMode::Files => {
                    let mut text = format!(
                        "The drive will be repartitioned and formatted, then the files of {name} are copied to it. Everything that is on the drive now is lost."
                    );
                    if options.setup.any() {
                        text.push_str(&format!(
                            " An answer file is added so that Windows Setup will {}.",
                            options.setup.describe().join(", ")
                        ));
                    }
                    text
                }
            }
        }
        Job::Reset { options, .. } => format!(
            "The drive gets a new {} partition table and one empty {} partition. Everything that is on the drive now is lost.",
            options.scheme.label(),
            options.fs.label()
        ),
        Job::Backup { out, .. } => format!(
            "Every sector of this drive is read into {}. The drive itself is not changed.",
            logic::file_name(out)
        ),
    }
}

fn erase_button(job: &Job) -> &'static str {
    match job {
        Job::Reset { .. } => "Erase and format",
        _ => "Erase and write",
    }
}

impl App {
    /// Opens the confirmation dialog for a job.
    pub(super) fn request_job(&mut self, job: Job, disk: Disk) {
        if !self.demo && !self.elevated {
            self.toast(
                "Working on a drive needs administrator rights. Use \"Restart as administrator\" first.",
                true,
            );
            return;
        }
        self.pending = Some(PendingJob {
            job,
            disk,
            understood: false,
            origin: self.tab,
        });
    }

    pub(super) fn start_job(&mut self, ctx: &egui::Context, pending: PendingJob) {
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        self.logbook.push(format!(
            "Starting: {} ({}, disk {})",
            pending.job.title(),
            logic::disk_title(&pending.disk),
            pending.disk.number
        ));
        let handle = if self.demo {
            engine::start_demo(pending.job.clone(), wake)
        } else {
            engine::start(pending.job.clone(), self.settings.policy, wake)
        };
        self.running = Some(Running::new(
            &pending.job,
            &pending.disk,
            pending.origin,
            Some(handle),
        ));
    }

    /// Builds a job state with fixed, invented numbers (demo screenshots only).
    pub(super) fn stage_demo_job(&mut self, finished: bool) {
        let Some((job, disk)) = self.build_write_job() else {
            return;
        };
        let mut run = Running::new(&job, &disk, Tab::Create, None);
        let total = match &job {
            Job::Write { info, .. } => info.total_file_bytes,
            _ => 0,
        };
        let current = run
            .stages
            .iter()
            .position(|s| *s == Stage::CopyingFiles)
            .unwrap_or(0);
        run.progress = Progress {
            stage: Some(current),
            done: total / 100 * 62,
            total,
            item: "sources/boot.wim".into(),
        };
        run.staged = Some((41.3 * 1024.0 * 1024.0, 80));
        for line in demo::log_lines().iter().skip(9).take(9) {
            run.push_tail(line);
        }
        if finished {
            run.elapsed = 312;
            run.outcome = Some(Outcome::Done(Summary {
                headline: "The drive is ready".into(),
                lines: vec![
                    format!(
                        "{} was copied to {}.",
                        job.title()
                            .trim_start_matches("Creating bootable drive from "),
                        logic::disk_title(&disk)
                    ),
                    "968 files, 6.22 GB, read back and compared: all identical.".into(),
                    "install.wim was split into 2 parts so that it fits on FAT32.".into(),
                    "The drive starts on UEFI systems.".into(),
                ],
                seconds: 312,
            }));
        }
        self.running = Some(run);
    }

    /// Moves the engine's events into the progress state and the log.
    pub(super) fn pump_job(&mut self, ctx: &egui::Context) {
        let Some(run) = self.running.as_mut() else {
            return;
        };
        if run.outcome.is_some() {
            return;
        }
        let Some(handle) = &run.handle else { return };
        let mut events = Vec::new();
        let mut lost = false;
        loop {
            match handle.rx.try_recv() {
                Ok(e) => events.push(e),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    lost = true;
                    break;
                }
            }
        }
        let mut finished = None;
        for event in events {
            match event {
                Event::Log(text) => {
                    run.push_tail(&text);
                    self.logbook.push(text);
                }
                Event::Finished(outcome) => finished = Some(outcome),
                other => {
                    if run.progress.fold(&run.stages, &other) {
                        run.meter.reset();
                    }
                    if let Event::Progress { done, .. } = other {
                        run.meter.add(run.started.elapsed().as_secs_f64(), done);
                    }
                }
            }
        }
        if finished.is_none() && lost {
            finished = Some(Outcome::Failed(
                "The job stopped without reporting a result.".into(),
            ));
        }
        let Some(outcome) = finished else { return };
        run.elapsed = match &outcome {
            Outcome::Done(s) => s.seconds,
            _ => run.started.elapsed().as_secs(),
        };
        self.logbook.push(match &outcome {
            Outcome::Done(s) => format!("Finished: {}", s.headline),
            Outcome::Cancelled(t) => format!("Cancelled: {t}"),
            Outcome::Failed(t) => format!("Failed: {t}"),
        });
        if !self.demo && self.settings.save_logs {
            run.log_note = Some(match self.logbook.save_auto(&self.app_dir) {
                Ok(path) => format!(
                    "The log was saved as {} in the log folder.",
                    logic::file_name(&path)
                ),
                Err(e) => e,
            });
        }
        run.outcome = Some(outcome);
        run.ask_cancel = false;
        // A long job usually ends while the user is doing something else:
        // flash the taskbar button until the window is looked at.
        if !ctx.input(|i| i.focused) {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
        // The drive has new contents: read the list again.
        self.reload_disks(ctx);
    }

    // ----------------------------------------------------------------------------------
    // Dialogs
    // ----------------------------------------------------------------------------------

    pub(super) fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        let p = pal_ctx(ctx);
        let destructive = pending.job.destructive();
        let mut decision: Option<bool> = None;
        let id = if destructive {
            "confirm-erase"
        } else {
            "confirm-backup"
        };
        let modal = egui::Modal::new(egui::Id::new(id))
            .frame(parts::modal_frame(&p))
            .show(ctx, |ui| {
                ui.set_width(540.0);
                let d = &pending.disk;
                ui.horizontal(|ui| {
                    if destructive {
                        parts::status_icon(ui, Icon::WARNING, p.danger, 34.0);
                    } else {
                        parts::status_icon(ui, Icon::SAVE, p.accent, 34.0);
                    }
                    ui.label(
                        RichText::new(if destructive {
                            "Erase this drive?"
                        } else {
                            "Back up this drive?"
                        })
                        .font(semibold(19.0)),
                    );
                });
                ui.add(
                    egui::Label::new(RichText::new(job_intro(&pending.job)).color(p.muted)).wrap(),
                );
                ui.add_space(4.0);
                Frame::new()
                    .fill(p.bg)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(parts::disk_icon(d).glyph())
                                    .size(18.0)
                                    .color(p.muted),
                            );
                            ui.label(RichText::new(d.name()).font(semibold(16.0)));
                            ui.label(
                                RichText::new(nominal_size(d.size))
                                    .size(14.5)
                                    .color(p.muted),
                            );
                        });
                        ui.add_space(2.0);
                        let rows = [
                            ("Size", logic::exact_size(d.size)),
                            ("Connection", d.bus.label().to_string()),
                            (
                                "Disk number",
                                format!("{} ({})", d.number, logic::scheme_text(d.scheme)),
                            ),
                        ];
                        egui::Grid::new("confirm-facts")
                            .num_columns(2)
                            .spacing(vec2(18.0, 2.0))
                            .min_row_height(20.0)
                            .show(ui, |ui| {
                                for (k, v) in rows {
                                    ui.label(RichText::new(k).size(13.0).color(p.muted));
                                    ui.label(RichText::new(v).size(13.5));
                                    ui.end_row();
                                }
                            });
                        ui.add_space(6.0);
                        widgets::divider(ui);
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(if destructive {
                                "Partitions that will be erased"
                            } else {
                                "Partitions on this drive"
                            })
                            .size(12.5)
                            .color(p.muted),
                        );
                        egui::ScrollArea::vertical()
                            .max_height(150.0)
                            .show(ui, |ui| {
                                if d.partitions.is_empty() {
                                    ui.label(
                                        RichText::new("The drive has no partitions.")
                                            .color(p.faint),
                                    );
                                }
                                for part in &d.partitions {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new("•").color(if destructive {
                                            p.danger
                                        } else {
                                            p.muted
                                        }));
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(logic::partition_line(part))
                                                    .font(semibold(14.0)),
                                            )
                                            .truncate(),
                                        );
                                    });
                                }
                            });
                    });
                if let Job::Backup { .. } = &pending.job {
                    ui.add_space(4.0);
                    parts::note(
                        ui,
                        Icon::INFO,
                        p.info,
                        &format!(
                            "The image file will be {} large, the full size of the drive.",
                            human_size(d.size)
                        ),
                    );
                }
                ui.add_space(8.0);
                if destructive {
                    widgets::checkbox(
                        ui,
                        &mut pending.understood,
                        "I understand that everything on this drive will be permanently erased",
                    );
                    ui.add_space(6.0);
                }
                parts::button_row(ui, |ui| {
                    if destructive {
                        if widgets::danger_button(
                            ui,
                            erase_button(&pending.job),
                            pending.understood,
                        )
                        .clicked()
                        {
                            decision = Some(true);
                        }
                    } else if widgets::primary_button(ui, "Start backup", true).clicked() {
                        decision = Some(true);
                    }
                    if widgets::ghost_button(ui, "Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                if let Some(pending) = self.pending.take() {
                    self.start_job(ctx, pending);
                }
            }
            Some(false) => self.pending = None,
            None => {}
        }
    }

    pub(super) fn cancel_dialog(&mut self, ctx: &egui::Context) {
        let Some(run) = &mut self.running else { return };
        if !run.ask_cancel {
            return;
        }
        let p = pal_ctx(ctx);
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(egui::Id::new("cancel-job"))
            .frame(parts::modal_frame(&p))
            .show(ctx, |ui| {
                ui.set_width(440.0);
                ui.label(RichText::new("Cancel?").font(semibold(19.0)));
                let text = if run.destructive {
                    "The drive will be left unusable until it is written or reset again."
                } else {
                    "The unfinished image file cannot be used. The drive itself is not changed."
                };
                ui.add(egui::Label::new(RichText::new(text).color(p.muted)).wrap());
                ui.add_space(10.0);
                parts::button_row(ui, |ui| {
                    if widgets::danger_button(ui, "Cancel the job", true).clicked() {
                        decision = Some(true);
                    }
                    if widgets::ghost_button(ui, "Keep going").clicked() {
                        decision = Some(false);
                    }
                });
            });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        match decision {
            Some(true) => run.request_cancel(),
            Some(false) => run.ask_cancel = false,
            None => {}
        }
    }

    // ----------------------------------------------------------------------------------
    // Progress and result
    // ----------------------------------------------------------------------------------

    pub(super) fn job_ui(&mut self, ui: &mut Ui) {
        let finished = self
            .running
            .as_ref()
            .map(|r| r.outcome.is_some())
            .unwrap_or(false);
        if finished {
            self.result_ui(ui);
        } else {
            self.progress_ui(ui);
        }
    }

    fn progress_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let closing = self.closing.is_some();
        let Some(run) = &mut self.running else { return };
        let mut ask = false;
        let subtitle = if run.destructive {
            format!(
                "{}  ·  keep the drive plugged in until this has finished",
                run.drive
            )
        } else {
            format!("{}  ·  the drive is only read", run.drive)
        };
        parts::page_header(ui, &run.title, &subtitle, |ui| {
            if run.cancelling {
                ui.label(
                    RichText::new(if closing {
                        "Cancelling, then closing…"
                    } else {
                        "Cancelling…"
                    })
                    .color(p.warn),
                );
                ui.add(egui::Spinner::new().size(18.0).color(p.warn));
            } else if widgets::ghost_button(ui, &Icon::CLOSE.with("Cancel")).clicked() {
                ask = true;
            }
        });

        let current = run.progress.stage;
        let fraction = run.progress.fraction();
        let (speed, eta) = match run.staged {
            Some((s, e)) => (Some(s), Some(e)),
            None => (run.meter.speed(), run.meter.eta(run.progress.total)),
        };
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    let total = ui.available_width();
                    let left = 270.0_f32.min(total * 0.36);
                    ui.vertical(|ui| {
                        ui.set_width(left);
                        widgets::card(ui, |ui| {
                            widgets::section(ui, "Steps");
                            ui.add_space(2.0);
                            for (i, stage) in run.stages.iter().enumerate() {
                                stage_row(ui, stage.label(), i, current);
                            }
                        });
                    });
                    ui.vertical(|ui| {
                        ui.set_width(total - left - 12.0);
                        ui.spacing_mut().item_spacing.y = 12.0;
                        widgets::card(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 8.0;
                            let label = current
                                .and_then(|i| run.stages.get(i))
                                .map(|s| s.label())
                                .unwrap_or("Starting");
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(label).font(semibold(18.0)));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if let Some(f) = fraction {
                                            ui.label(
                                                RichText::new(format!("{:.0} %", f * 100.0))
                                                    .font(semibold(18.0))
                                                    .color(p.accent),
                                            );
                                        }
                                    },
                                );
                            });
                            parts::progress_bar(ui, fraction, 14.0, p.accent);
                            ui.horizontal(|ui| {
                                let amount = run.progress.amount();
                                ui.label(
                                    RichText::new(if amount.is_empty() {
                                        "Working…".to_string()
                                    } else {
                                        amount
                                    })
                                    .size(13.5),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if fraction.is_some() {
                                            let left = match eta {
                                                Some(s) => format!("{} left", human_duration(s)),
                                                None => "estimating the time left".to_string(),
                                            };
                                            ui.label(RichText::new(left).size(13.5).color(p.muted));
                                            if let Some(s) = speed {
                                                ui.label(RichText::new("·").color(p.faint));
                                                ui.label(RichText::new(human_speed(s)).size(13.5));
                                            }
                                        }
                                    },
                                );
                            });
                            if !run.progress.item.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&run.progress.item).size(13.0).color(p.muted),
                                    )
                                    .truncate(),
                                );
                            }
                            ui.add_space(2.0);
                            widgets::divider(ui);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Overall").size(12.5).color(p.muted));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let step = current.map(|i| i + 1).unwrap_or(0);
                                        ui.label(
                                            RichText::new(format!(
                                                "Step {step} of {}",
                                                run.stages.len()
                                            ))
                                            .size(12.5)
                                            .color(p.muted),
                                        );
                                    },
                                );
                            });
                            parts::progress_bar(
                                ui,
                                Some(run.progress.overall(run.stages.len())),
                                6.0,
                                p.accent.gamma_multiply(0.7),
                            );
                        });
                        widgets::card(ui, |ui| {
                            widgets::section(ui, "Activity");
                            ui.spacing_mut().item_spacing.y = 3.0;
                            if run.tail.is_empty() {
                                ui.label(RichText::new("Nothing yet.").color(p.faint));
                            }
                            for line in &run.tail {
                                let head = line.starts_with("== ");
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(line.trim_start_matches("== "))
                                            .font(FontId::new(12.5, FontFamily::Monospace))
                                            .color(if head { p.text } else { p.muted }),
                                    )
                                    .truncate(),
                                );
                            }
                        });
                    });
                });
            });
        if ask {
            run.ask_cancel = true;
        }
    }

    fn result_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let Some(run) = &self.running else { return };
        let Some(outcome) = &run.outcome else { return };
        let (icon, color, headline, lines, footer): (Icon, _, String, Vec<String>, String) =
            match outcome {
                Outcome::Done(s) => (
                    Icon::CHECK,
                    p.accent,
                    s.headline.clone(),
                    s.lines.clone(),
                    format!("Took {}.", human_duration(run.elapsed)),
                ),
                Outcome::Cancelled(text) => (
                    Icon::WARNING,
                    p.warn,
                    "Cancelled".into(),
                    vec![text.clone()],
                    format!("Stopped after {}.", human_duration(run.elapsed)),
                ),
                Outcome::Failed(text) => (
                    Icon::ERROR,
                    p.danger,
                    "The job did not finish".into(),
                    vec![text.clone()],
                    "The log has the details of every step up to the error.".into(),
                ),
            };
        let done = matches!(outcome, Outcome::Done(_));
        let mut close = false;
        let mut open_log = false;
        parts::page_header(ui, &run.title, &run.drive, |_| {});
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(760.0);
                widgets::card(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(14.0);
                        parts::status_icon(ui, icon, color, 72.0);
                        ui.add_space(4.0);
                        ui.label(RichText::new(&headline).font(semibold(22.0)));
                        ui.label(RichText::new(&footer).color(p.muted));
                        ui.add_space(6.0);
                    });
                    Frame::new()
                        .fill(p.bg)
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::same(14))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for line in &lines {
                                ui.horizontal_top(|ui| {
                                    ui.label(
                                        RichText::new(if done {
                                            Icon::CHECK.glyph()
                                        } else {
                                            "•"
                                        })
                                        .size(13.0)
                                        .color(color),
                                    );
                                    ui.add(egui::Label::new(line.as_str()).wrap());
                                });
                            }
                        });
                    if let Some(note) = &run.log_note {
                        ui.add(
                            egui::Label::new(RichText::new(note).size(12.5).color(p.faint)).wrap(),
                        );
                    }
                    ui.add_space(4.0);
                    parts::button_row(ui, |ui| {
                        if widgets::primary_button(ui, "Done", true).clicked() {
                            close = true;
                        }
                        if widgets::ghost_button(ui, &Icon::DOCUMENT.with("Open log")).clicked() {
                            open_log = true;
                        }
                    });
                });
            });
        if open_log {
            self.running = None;
            self.tab = Tab::Log;
        } else if close {
            let origin = run.origin;
            self.running = None;
            self.tab = origin;
        }
    }

    /// Opens the folder the log files are saved in.
    pub(super) fn open_log_folder(&mut self) {
        if self.demo {
            self.toast("Demo mode: no log files are written.", false);
            return;
        }
        let dir = logbook::logs_dir(&self.app_dir);
        let _ = std::fs::create_dir_all(&dir);
        crate::backend::open_path(&dir);
    }
}

fn stage_row(ui: &mut Ui, label: &str, index: usize, current: Option<usize>) {
    let p = pal(ui);
    let done = current.map(|c| index < c).unwrap_or(false);
    let active = current == Some(index);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
        let c = rect.center();
        if done {
            ui.painter()
                .circle_filled(c, 10.0, p.accent.gamma_multiply(0.18));
            let stroke = Stroke::new(1.8_f32, p.accent);
            let pts = [
                pos2(c.x - 4.2, c.y + 0.2),
                pos2(c.x - 1.4, c.y + 3.0),
                pos2(c.x + 4.4, c.y - 3.2),
            ];
            ui.painter().line_segment([pts[0], pts[1]], stroke);
            ui.painter().line_segment([pts[1], pts[2]], stroke);
        } else if active {
            egui::Spinner::new()
                .color(p.accent)
                .paint_at(ui, rect.shrink(2.0));
        } else {
            ui.painter()
                .circle_stroke(c, 5.0, Stroke::new(1.5_f32, p.faint));
        }
        let text = RichText::new(label);
        ui.label(if active {
            text.font(semibold(14.5))
        } else if done {
            text.font(regular(14.5)).color(p.text)
        } else {
            text.font(regular(14.5)).color(p.faint)
        });
    });
}
