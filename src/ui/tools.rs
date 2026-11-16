//! The Drive tools page: back up a drive, restore an image, and return a
//! drive to a normal empty drive.

use super::app::{demo_env, App, ImageSlot, Slot};
use super::create::{cluster_combo, column, image_summary, label_edit, BAD_BLOCK_HINT};
use super::logic;
use super::parts::{self, DriveAction, DriveList};
use super::theme::{pal, regular, semibold, Icon};
use super::widgets;
use crate::engine::{Job, ResetOptions};
use crate::model::{DiskIdentity, FileSystem, Scheme, WriteMode, WriteOptions};
use crate::plan;
use crate::safety::GIB;
use crate::settings::Settings;
use crate::util::human_size;
use crate::{backend, demo};
use eframe::egui::{self, vec2, Align2, CornerRadius, RichText, Sense, Ui, Vec2};
use std::path::PathBuf;

pub struct ToolsState {
    pub backup_disk: Option<DiskIdentity>,
    pub backup_out: Option<PathBuf>,
    /// Free bytes on the volume the backup goes to, when known.
    pub backup_free: Option<u64>,
    pub restore_image: ImageSlot,
    pub restore_disk: Option<DiskIdentity>,
    pub restore_verify: bool,
    pub reset_disk: Option<DiskIdentity>,
    pub reset: ResetOptions,
}

impl ToolsState {
    pub fn new(settings: &Settings) -> ToolsState {
        ToolsState {
            backup_disk: None,
            backup_out: None,
            backup_free: None,
            restore_image: ImageSlot::Empty,
            restore_disk: None,
            restore_verify: settings.verify,
            reset_disk: None,
            reset: ResetOptions {
                quick_format: settings.quick_format,
                ..ResetOptions::default()
            },
        }
    }
}

fn tool_title(ui: &mut Ui, icon: Icon, title: &str, text: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        ui.painter()
            .rect_filled(tile, CornerRadius::same(9), p.accent.gamma_multiply(0.16));
        ui.painter().text(
            tile.center(),
            Align2::CENTER_CENTER,
            icon.glyph(),
            regular(17.0),
            p.accent,
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new(title).font(semibold(15.5)));
            ui.add(egui::Label::new(RichText::new(text).size(13.0).color(p.muted)).wrap());
        });
    });
    ui.add_space(4.0);
}

/// Widths of the two columns inside a tool card.
fn split(ui: &Ui) -> (f32, f32) {
    let w = ui.available_width();
    let left = ((w - 16.0) * 0.5).floor();
    (left, w - 16.0 - left)
}

impl App {
    /// Preselects a little of everything for the demo screenshot.
    pub(super) fn stage_demo_tools(&mut self) {
        let (listed, _) = self.listed(&[]);
        self.tools.backup_disk = listed
            .iter()
            .find(|l| l.blocked.is_none())
            .map(|l| l.disk.identity());
        self.tools.backup_out = Some(PathBuf::from("D:\\Backups\\kestrel-swift.img"));
        self.tools.backup_free = Some(212 * GIB);
        self.set_loaded(Slot::Restore, demo::disk_image(), demo_env());
    }

    fn can_work(&self) -> bool {
        self.demo || self.elevated
    }

    pub(super) fn tools_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        parts::page_header(
            ui,
            "Drive tools",
            "Keep a copy of a drive, put it back, or make a drive ordinary again.",
            |_| {},
        );
        let mut restart = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                if !self.can_work() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(Icon::WARNING.glyph()).color(p.warn));
                        ui.label(
                            RichText::new(
                                "Reading or writing a whole drive needs administrator rights.",
                            )
                            .color(p.warn),
                        );
                        if parts::link(ui, "Restart as administrator").clicked() {
                            restart = true;
                        }
                    });
                }
                self.backup_card(ui);
                self.restore_card(ui);
                self.reset_card(ui);
                ui.add_space(4.0);
            });
        if restart {
            self.restart_elevated(&ctx);
        }
    }

    fn backup_card(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        let needed: Vec<PathBuf> = self.tools.backup_out.iter().cloned().collect();
        let (listed, hidden) = self.listed(&needed);
        let disk = logic::selected_disk(&self.tools.backup_disk, &listed).cloned();
        let can_work = self.can_work();
        let mut action = DriveAction::None;
        let mut save_as = false;
        let mut start = false;
        widgets::card(ui, |ui| {
            tool_title(
                ui,
                Icon::DOWNLOAD,
                "Back up a drive to an image file",
                "Reads every sector into an .img file, so the drive can be put back exactly as it is now.",
            );
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                let (left, right) = split(ui);
                ui.vertical(|ui| {
                    ui.set_width(left);
                    action = parts::drive_list(
                        ui,
                        &DriveList {
                            loading: self.disks_loading,
                            error: self.disks_error.as_deref(),
                            listed: &listed,
                            hidden,
                        },
                        &mut self.tools.backup_disk,
                    );
                });
                ui.vertical(|ui| {
                    ui.set_width(right);
                    parts::field(ui, "Image file", |ui| {
                        ui.horizontal(|ui| {
                            if widgets::ghost_button(ui, &Icon::SAVE.with("Save as…")).clicked() {
                                save_as = true;
                            }
                            match &self.tools.backup_out {
                                Some(path) => {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(path.display().to_string()).size(13.5),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(path.display().to_string());
                                }
                                None => {
                                    ui.label(RichText::new("No file chosen yet").color(p.faint));
                                }
                            }
                        });
                    });
                    let mut enough = true;
                    if let Some(d) = &disk {
                        match self.tools.backup_free {
                            Some(free) if self.tools.backup_out.is_some() => {
                                enough = free >= d.size;
                                if enough {
                                    parts::hint(
                                        ui,
                                        &format!(
                                            "The image will be {}; {} is free at that location.",
                                            human_size(d.size),
                                            human_size(free)
                                        ),
                                    );
                                } else {
                                    parts::note(
                                        ui,
                                        Icon::ERROR,
                                        p.danger,
                                        &format!(
                                            "Not enough space: the image will be {}, but only {} is free at that location.",
                                            human_size(d.size),
                                            human_size(free)
                                        ),
                                    );
                                }
                            }
                            _ => parts::hint(
                                ui,
                                &format!(
                                    "The image will be {}, the full size of the drive.",
                                    human_size(d.size)
                                ),
                            ),
                        }
                    } else {
                        parts::hint(ui, "The image file is as large as the whole drive.");
                    }
                    let ready =
                        disk.is_some() && self.tools.backup_out.is_some() && enough && can_work;
                    parts::button_row(ui, |ui| {
                        if widgets::primary_button(ui, &Icon::DOWNLOAD.with("Back up…"), ready)
                            .clicked()
                        {
                            start = true;
                        }
                    });
                });
            });
        });
        self.drive_action(&ctx, action);
        if save_as && !self.taking_screenshot() {
            self.choose_backup_file(disk.as_ref().map(|d| d.name()));
        }
        if start {
            if let (Some(d), Some(out)) = (disk, self.tools.backup_out.clone()) {
                self.request_job(
                    Job::Backup {
                        disk: d.identity(),
                        out,
                    },
                    d,
                );
            }
        }
    }

    fn choose_backup_file(&mut self, disk_name: Option<String>) {
        if self.demo {
            self.tools.backup_out = Some(PathBuf::from("D:\\Backups\\kestrel-swift.img"));
            self.tools.backup_free = Some(212 * GIB);
            return;
        }
        let suggested: String = disk_name
            .unwrap_or_else(|| "drive".into())
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let picked = rfd::FileDialog::new()
            .set_title("Save the drive image as")
            .add_filter("Disk image", &["img"])
            .set_file_name(format!("{}-backup.img", suggested.trim_matches('-')))
            .save_file();
        if let Some(mut path) = picked {
            if path.extension().is_none() {
                path.set_extension("img");
            }
            self.tools.backup_free = path.parent().and_then(backend::free_space);
            self.tools.backup_out = Some(path);
            // A drive that holds the chosen file can no longer be the source.
            self.prune_selections();
        }
    }

    fn restore_card(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        let (listed, hidden) = self.listed(&self.tools.restore_image.needed());
        let disk = logic::selected_disk(&self.tools.restore_disk, &listed).cloned();
        let options = WriteOptions {
            mode: WriteMode::Image,
            verify: self.tools.restore_verify,
            eject: self.settings.eject,
            ..WriteOptions::default()
        };
        let can_work = self.can_work();
        let mut action = DriveAction::None;
        let mut choose = false;
        let mut start = false;
        widgets::card(ui, |ui| {
            tool_title(
                ui,
                Icon::UPLOAD,
                "Restore an image to a drive",
                "Writes an .img backup (or any disk image) back to a drive, byte for byte.",
            );
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                let (left, right) = split(ui);
                ui.vertical(|ui| {
                    ui.set_width(left);
                    action = parts::drive_list(
                        ui,
                        &DriveList {
                            loading: self.disks_loading,
                            error: self.disks_error.as_deref(),
                            listed: &listed,
                            hidden,
                        },
                        &mut self.tools.restore_disk,
                    );
                });
                ui.vertical(|ui| {
                    ui.set_width(right);
                    let mut blocked = true;
                    match &self.tools.restore_image {
                        ImageSlot::Empty => {
                            parts::field(ui, "Image file", |ui| {
                                ui.horizontal(|ui| {
                                    if widgets::ghost_button(
                                        ui,
                                        &Icon::OPEN_FILE.with("Choose image…"),
                                    )
                                    .clicked()
                                    {
                                        choose = true;
                                    }
                                    ui.label(
                                        RichText::new("or drop a file on this page").color(p.faint),
                                    );
                                });
                            });
                        }
                        ImageSlot::Inspecting(path) => {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().size(18.0).color(p.accent));
                                ui.add(
                                    egui::Label::new(format!(
                                        "Reading {}…",
                                        logic::file_name(path)
                                    ))
                                    .truncate(),
                                );
                            });
                        }
                        ImageSlot::Failed(path, error) => {
                            parts::note(
                                ui,
                                Icon::ERROR,
                                p.danger,
                                &format!("{} could not be read: {error}", logic::file_name(path)),
                            );
                            if widgets::ghost_button(
                                ui,
                                &Icon::OPEN_FILE.with("Choose another image…"),
                            )
                            .clicked()
                            {
                                choose = true;
                            }
                        }
                        ImageSlot::Ready(loaded) => {
                            image_summary(ui, loaded, false);
                            parts::hint(
                                ui,
                                &format!("Image size: {}", logic::exact_size(loaded.info.size)),
                            );
                            let issues = plan::check_options(
                                &loaded.info,
                                &loaded.advice,
                                &options,
                                disk.as_ref(),
                            );
                            blocked = issues.iter().any(|i| i.blocking);
                            for issue in issues.iter().filter(|i| i.blocking) {
                                parts::note(ui, Icon::ERROR, p.danger, &issue.text);
                            }
                        }
                    }
                    let has_image = self.tools.restore_image.loaded().is_some();
                    let ready = has_image && disk.is_some() && !blocked && can_work;
                    ui.horizontal(|ui| {
                        widgets::checkbox(
                            ui,
                            &mut self.tools.restore_verify,
                            "Verify after writing",
                        );
                    });
                    parts::button_row(ui, |ui| {
                        if widgets::danger_button(ui, &Icon::UPLOAD.with("Restore…"), ready)
                            .clicked()
                        {
                            start = true;
                        }
                        if has_image && widgets::ghost_button(ui, "Change…").clicked() {
                            choose = true;
                        }
                    });
                });
            });
        });
        self.drive_action(&ctx, action);
        if choose {
            self.choose_image(&ctx, Slot::Restore);
        }
        if start {
            if let (Some(d), Some(loaded)) = (disk, self.tools.restore_image.loaded()) {
                let job = Job::Write {
                    info: loaded.info.clone(),
                    disk: d.identity(),
                    options,
                };
                self.request_job(job, d);
            }
        }
    }

    fn reset_card(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        let (listed, hidden) = self.listed(&[]);
        let disk = logic::selected_disk(&self.tools.reset_disk, &listed).cloned();
        let can_work = self.can_work();
        let mut action = DriveAction::None;
        let mut start = false;
        widgets::card(ui, |ui| {
            tool_title(
                ui,
                Icon::BROOM,
                "Restore a drive to a normal empty drive",
                "Removes the boot layout and gives the drive one empty partition, ready for everyday files.",
            );
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                let (left, right) = split(ui);
                ui.vertical(|ui| {
                    ui.set_width(left);
                    action = parts::drive_list(
                        ui,
                        &DriveList {
                            loading: self.disks_loading,
                            error: self.disks_error.as_deref(),
                            listed: &listed,
                            hidden,
                        },
                        &mut self.tools.reset_disk,
                    );
                });
                ui.vertical(|ui| {
                    ui.set_width(right);
                    let o = &mut self.tools.reset;
                    let gap = 14.0;
                    let c1 = 136.0_f32;
                    let c2 = 170.0_f32;
                    let c3 = (right - c1 - c2 - 2.0 * gap).max(110.0);
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        column(ui, c1, "Partition scheme", |ui| {
                            let current = if o.scheme == Scheme::Mbr { 0 } else { 1 };
                            if let Some(i) = widgets::segmented(ui, &["MBR", "GPT"], current) {
                                o.scheme = if i == 0 { Scheme::Mbr } else { Scheme::Gpt };
                            }
                        });
                        column(ui, c2, "File system", |ui| {
                            let current = if o.fs == FileSystem::Fat32 { 0 } else { 1 };
                            if let Some(i) = widgets::segmented(ui, &["FAT32", "exFAT"], current) {
                                o.fs = if i == 0 {
                                    FileSystem::Fat32
                                } else {
                                    FileSystem::ExFat
                                };
                                o.label = logic::final_label(&o.label, o.fs);
                            }
                        });
                        column(ui, c3, "Cluster size", |ui| {
                            cluster_combo(ui, "reset-cluster", &mut o.cluster_size);
                        });
                    });
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        column(ui, c1, "Volume label", |ui| {
                            label_edit(ui, &mut o.label, o.fs, c1 - 18.0);
                        });
                        column(ui, c2 + gap + c3, "Before formatting", |ui| {
                            ui.horizontal(|ui| {
                                ui.set_height(30.0);
                                ui.spacing_mut().item_spacing = vec2(16.0, 4.0);
                                widgets::checkbox(ui, &mut o.quick_format, "Quick format")
                                    .on_hover_text(
                                        "Off: the whole partition is overwritten with zeros first, which takes much longer.",
                                    );
                                widgets::checkbox(ui, &mut o.bad_block_check, "Check for bad blocks")
                                    .on_hover_text(BAD_BLOCK_HINT);
                            });
                        });
                    });
                    if o.fs == FileSystem::ExFat {
                        parts::note(
                            ui,
                            Icon::WARNING,
                            p.warn,
                            "The built-in exFAT formatter has not been tested on real hardware yet. FAT32 is the safe choice; if an exFAT drive misbehaves, format it again in Windows.",
                        );
                    } else {
                        parts::hint(
                            ui,
                            "FAT32 works everywhere but holds files up to 4 GB. exFAT has no such limit; its built-in formatter has not been tested on real hardware yet.",
                        );
                    }
                    if o.bad_block_check {
                        parts::note(ui, Icon::WARNING, p.warn, BAD_BLOCK_HINT);
                    }
                    let ready = disk.is_some() && can_work;
                    parts::button_row(ui, |ui| {
                        if widgets::danger_button(ui, &Icon::BROOM.with("Erase and reset…"), ready)
                            .clicked()
                        {
                            start = true;
                        }
                    });
                });
            });
        });
        self.drive_action(&ctx, action);
        if start {
            if let Some(d) = disk {
                let mut options = self.tools.reset.clone();
                options.label = logic::final_label(&options.label, options.fs);
                self.request_job(
                    Job::Reset {
                        disk: d.identity(),
                        options,
                    },
                    d,
                );
            }
        }
    }
}
