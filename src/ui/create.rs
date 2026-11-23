//! The Create page: image, drive, options and the action row.

use super::app::{App, ImageSlot, Loaded, Slot, Tab};
use super::logic;
use super::parts::{self, DriveAction, DriveList};
use super::theme::{pal, pal_ctx, regular, semibold, Icon};
use super::widgets;
use crate::engine::Job;
use crate::model::{Disk, FileSystem, Scheme, SetupTweaks, TargetSystem, WriteMode, WriteOptions};
use crate::plan::{self, Issue, TweakSupport};
use crate::safety::Listed;
use eframe::egui::{
    self, vec2, Align, Align2, CornerRadius, Frame, Layout, Margin, RichText, Sense, Stroke, Ui,
    Vec2,
};
use std::path::PathBuf;

/// Paints "label   value" with a fixed label column; the value wraps.
pub(super) fn fact_row(ui: &mut Ui, label: &str, value: &str) {
    let p = pal(ui);
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(112.0, 18.0), Sense::hover());
        ui.painter().text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            label,
            regular(13.0),
            p.muted,
        );
        ui.add(egui::Label::new(RichText::new(value).size(13.0)).wrap());
    });
}

/// The summary of an inspected image: name, headline, badges, facts, warnings.
pub(super) fn image_summary(ui: &mut Ui, loaded: &Loaded, with_facts: bool) {
    let p = pal(ui);
    let advice = &loaded.advice;
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
        ui.painter()
            .rect_filled(tile, CornerRadius::same(9), p.accent.gamma_multiply(0.16));
        ui.painter().text(
            tile.center(),
            Align2::CENTER_CENTER,
            Icon::DISC.glyph(),
            regular(19.0),
            p.accent,
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.add(
                egui::Label::new(
                    RichText::new(logic::file_name(&loaded.info.path)).font(semibold(15.5)),
                )
                .truncate(),
            );
            ui.add(
                egui::Label::new(RichText::new(&advice.headline).size(13.0).color(p.muted))
                    .truncate(),
            );
        });
    });
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        for b in &advice.badges {
            widgets::badge(ui, &b.text, parts::tone_color(&p, b.tone));
        }
    });
    if with_facts {
        ui.add_space(2.0);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            for (k, v) in &advice.facts {
                fact_row(ui, k, v);
            }
        });
    }
    for w in &advice.warnings {
        parts::note(ui, Icon::WARNING, p.warn, w);
    }
}

fn issue_notes(ui: &mut Ui, issues: &[Issue]) {
    let p = pal(ui);
    for issue in issues {
        if issue.blocking {
            parts::note(ui, Icon::ERROR, p.danger, &issue.text);
        } else {
            parts::note(ui, Icon::WARNING, p.warn, &issue.text);
        }
    }
}

pub(super) const BAD_BLOCK_HINT: &str =
    "Writes test patterns to the whole drive and reads them back before the real write. On large or slow drives this can take hours.";

impl App {
    /// The write job for the current selection, with the drive it targets.
    pub(super) fn build_write_job(&self) -> Option<(Job, Disk)> {
        let loaded = self.image.loaded()?;
        let (listed, _) = self.listed(&self.image.needed());
        let disk = logic::selected_disk(&self.create_disk, &listed)?.clone();
        let mut options = self.opts.clone();
        options.label = logic::final_label(&options.label, options.fs);
        options.setup =
            plan::usable_tweaks(loaded.advice.setup_tweaks, options.mode, options.setup);
        Some((
            Job::Write {
                info: loaded.info.clone(),
                disk: disk.identity(),
                options,
            },
            disk,
        ))
    }

    /// Opens the erase confirmation for the current selection.
    pub(super) fn request_write(&mut self) {
        if let Some((job, disk)) = self.build_write_job() {
            // The Windows Setup choices are offered again for the next Windows image.
            let offered = self
                .image
                .loaded()
                .map(|l| l.advice.setup_tweaks.available() && self.opts.mode == WriteMode::Files)
                .unwrap_or(false);
            if offered && self.settings.setup != self.opts.setup {
                self.settings.setup = self.opts.setup;
                self.save_settings();
            }
            self.request_job(job, disk);
        }
    }

    pub(super) fn create_ui(&mut self, ui: &mut Ui) {
        parts::page_header(
            ui,
            "Create bootable drive",
            "Choose an image and a USB drive. Nothing is written until you confirm.",
            |_| {},
        );
        let (listed, hidden) = self.listed(&self.image.needed());
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let w = ui.available_width();
                if w >= 900.0 {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        let left = ((w - 12.0) * 0.45).floor();
                        ui.vertical(|ui| {
                            ui.set_width(left);
                            self.image_card(ui);
                        });
                        ui.vertical(|ui| {
                            ui.set_width(w - 12.0 - left);
                            ui.spacing_mut().item_spacing.y = 12.0;
                            self.drive_card(ui, &listed, hidden);
                            self.options_card(ui, &listed);
                        });
                    });
                } else {
                    ui.spacing_mut().item_spacing.y = 12.0;
                    self.image_card(ui);
                    self.drive_card(ui, &listed, hidden);
                    self.options_card(ui, &listed);
                }
                ui.add_space(4.0);
            });
    }

    fn image_card(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        let mut choose = false;
        let mut checksums = false;
        let mut reopen: Option<PathBuf> = None;
        let mut verify_now = false;
        let published = self.published_check();
        let recent: Vec<PathBuf> = if self.demo {
            crate::demo::recent_images()
        } else {
            self.settings.recent_images.clone()
        };
        widgets::card(ui, |ui| match &self.image {
            ImageSlot::Empty => {
                parts::numbered_title(ui, 1, "Image");
                ui.add_space(2.0);
                let zone = Frame::new()
                    .inner_margin(Margin::symmetric(16, 30))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.vertical_centered(|ui| {
                            parts::status_icon(ui, Icon::DISC, p.accent, 56.0);
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new("Drop an ISO or IMG file here").font(semibold(16.5)),
                            );
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        "Windows setup discs, Linux live images and raw disk images",
                                    )
                                    .size(13.0)
                                    .color(p.muted),
                                )
                                .wrap(),
                            );
                            ui.add_space(8.0);
                            if widgets::primary_button(
                                ui,
                                &Icon::OPEN_FILE.with("Choose image…"),
                                true,
                            )
                            .clicked()
                            {
                                choose = true;
                            }
                            ui.label(RichText::new("Ctrl+O").size(12.0).color(p.faint));
                        });
                    })
                    .response;
                parts::dashed_border(ui, zone.rect, p.faint.gamma_multiply(0.8));
                if !recent.is_empty() {
                    ui.add_space(4.0);
                    ui.label(RichText::new("Recent images").size(12.5).color(p.muted));
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        for path in &recent {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(Icon::DISC.glyph()).size(13.0).color(p.faint),
                                );
                                if parts::link(ui, &logic::file_name(path))
                                    .on_hover_text(path.display().to_string())
                                    .clicked()
                                {
                                    reopen = Some(path.clone());
                                }
                                let folder = path
                                    .parent()
                                    .map(|d| d.display().to_string())
                                    .unwrap_or_default();
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(folder).size(12.0).color(p.faint),
                                    )
                                    .truncate(),
                                );
                            });
                        }
                    });
                }
            }
            ImageSlot::Inspecting(path) => {
                parts::numbered_title(ui, 1, "Image");
                ui.vertical_centered(|ui| {
                    ui.add_space(34.0);
                    ui.add(egui::Spinner::new().size(30.0).color(p.accent));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Reading the image…").font(semibold(15.5)));
                    ui.add(
                        egui::Label::new(RichText::new(logic::file_name(path)).color(p.muted))
                            .truncate(),
                    );
                    ui.add_space(34.0);
                });
            }
            ImageSlot::Failed(path, error) => {
                parts::numbered_title(ui, 1, "Image");
                ui.vertical_centered(|ui| {
                    ui.add_space(14.0);
                    parts::status_icon(ui, Icon::ERROR, p.danger, 52.0);
                    ui.label(RichText::new("This file could not be read").font(semibold(15.5)));
                    ui.add(
                        egui::Label::new(RichText::new(logic::file_name(path)).color(p.muted))
                            .truncate(),
                    );
                });
                parts::note(ui, Icon::ERROR, p.danger, error);
                ui.vertical_centered(|ui| {
                    ui.add_space(4.0);
                    if widgets::ghost_button(ui, &Icon::OPEN_FILE.with("Choose another image…"))
                        .clicked()
                    {
                        choose = true;
                    }
                    ui.add_space(8.0);
                });
            }
            ImageSlot::Ready(loaded) => {
                ui.horizontal(|ui| {
                    parts::numbered_title(ui, 1, "Image");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::ghost_button(ui, "Checksums").clicked() {
                            checksums = true;
                        }
                        if widgets::ghost_button(ui, "Change…").clicked() {
                            choose = true;
                        }
                    });
                });
                image_summary(ui, loaded, true);
                match &published {
                    None => {}
                    Some(logic::PublishedCheck::Found(text)) => {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(Icon::FINGERPRINT.glyph()).color(p.info));
                            ui.add(
                                egui::Label::new(RichText::new(text).size(13.0).color(p.muted))
                                    .truncate(),
                            );
                        });
                        if parts::link(ui, "Check the image against it now").clicked() {
                            verify_now = true;
                        }
                    }
                    Some(logic::PublishedCheck::Checking(text)) => {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                            ui.label(RichText::new(text).size(13.0).color(p.muted));
                        });
                    }
                    Some(logic::PublishedCheck::Verified(text)) => {
                        parts::note(ui, Icon::DONE, p.accent, text)
                    }
                    Some(logic::PublishedCheck::Failed(text)) => {
                        parts::note(ui, Icon::ERROR, p.danger, text)
                    }
                }
            }
        });
        if choose {
            self.choose_image(&ctx, Slot::Create);
        }
        if let Some(path) = reopen {
            self.open_recent(&ctx, path);
        }
        if verify_now {
            self.start_hash(&ctx);
        }
        if checksums {
            self.tab = Tab::Checksums;
        }
    }

    /// Selects an image from the recent list; one that is gone is dropped
    /// from the list instead.
    fn open_recent(&mut self, ctx: &egui::Context, path: PathBuf) {
        if self.demo || path.is_file() {
            self.set_image(ctx, Slot::Create, path);
            return;
        }
        self.settings.forget_image(&path);
        self.save_settings();
        self.toast(
            format!(
                "{} is no longer there; it was removed from the recent images.",
                logic::file_name(&path)
            ),
            true,
        );
    }

    fn drive_card(&mut self, ui: &mut Ui, listed: &[Listed], hidden: usize) {
        let ctx = ui.ctx().clone();
        let mut action = DriveAction::None;
        widgets::card(ui, |ui| {
            parts::numbered_title(ui, 2, "Drive");
            action = parts::drive_list(
                ui,
                &DriveList {
                    loading: self.disks_loading,
                    error: self.disks_error.as_deref(),
                    listed,
                    hidden,
                },
                &mut self.create_disk,
            );
        });
        self.drive_action(&ctx, action);
    }

    pub(super) fn drive_action(&mut self, ctx: &egui::Context, action: DriveAction) {
        match action {
            DriveAction::None => {}
            DriveAction::OpenSettings => self.tab = Tab::Settings,
            DriveAction::Refresh => self.refresh(ctx),
        }
    }

    fn options_card(&mut self, ui: &mut Ui, listed: &[Listed]) {
        let p = pal(ui);
        let disk = logic::selected_disk(&self.create_disk, listed).cloned();
        widgets::card(ui, |ui| {
            let Some(loaded) = self.image.loaded() else {
                parts::numbered_title(ui, 3, "Options");
                parts::hint(
                    ui,
                    "Choose an image first: which options make sense depends on what the image contains.",
                );
                return;
            };
            let advice = &loaded.advice;
            let opts = &mut self.opts;
            ui.spacing_mut().item_spacing.y = 6.0;

            // Write mode.
            let image_ok = advice.image_mode.is_ok();
            let files_ok = advice.files_mode.is_ok();
            ui.horizontal(|ui| {
                parts::numbered_title(ui, 3, "Options");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let current = match opts.mode {
                        WriteMode::Image => 0,
                        WriteMode::Files => 1,
                    };
                    let picked = widgets::segmented_flags(
                        ui,
                        &[
                            ("Image (byte for byte)", image_ok),
                            ("File copy (FAT32)", files_ok),
                        ],
                        current,
                    );
                    if let Some(i) = picked {
                        opts.mode = if i == 0 {
                            WriteMode::Image
                        } else {
                            WriteMode::Files
                        };
                    }
                    if advice.recommended == Some(opts.mode) {
                        widgets::badge(ui, "Recommended", p.accent);
                    }
                });
            });
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                parts::hint(ui, &advice.why);
                if advice.recommended.is_some() {
                    if let Err(reason) = &advice.image_mode {
                        hint_line(ui, &format!("Image mode is not available. {reason}"));
                    }
                    if let Err(reason) = &advice.files_mode {
                        hint_line(ui, &format!("File copy is not available. {reason}"));
                    }
                }
            });

            match opts.mode {
                WriteMode::Files => files_options(ui, opts),
                WriteMode::Image => {
                    let mut on = Vec::new();
                    if advice.image_boots_uefi {
                        on.push("UEFI");
                    }
                    if advice.image_boots_bios {
                        on.push("BIOS");
                    }
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Boots on").size(12.5).color(p.muted));
                        if on.is_empty() {
                            ui.label("Not known for this file");
                        } else {
                            ui.label(RichText::new(on.join(" / ")).font(semibold(14.0)));
                            ui.label(
                                RichText::new("(as built into the image)")
                                    .size(12.5)
                                    .color(p.muted),
                            );
                        }
                    });
                    parts::hint(
                        ui,
                        "The image brings its own partition table and file system, so there is nothing to format.",
                    );
                }
            }

            if opts.mode == WriteMode::Files {
                setup_options(ui, advice.setup_tweaks, &mut opts.setup);
            }

            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(18.0, 4.0);
                widgets::checkbox(ui, &mut opts.verify, "Verify after writing")
                    .on_hover_text("Reads everything back and compares it with the image.");
                widgets::checkbox(ui, &mut opts.eject, "Eject when done");
                widgets::checkbox(ui, &mut opts.bad_block_check, "Check for bad blocks first")
                    .on_hover_text(BAD_BLOCK_HINT);
            });

            let issues = plan::check_options(&loaded.info, advice, opts, disk.as_ref());
            issue_notes(ui, &issues);
        });
    }

    /// The row under the page: what will happen, and the Write button.
    pub(super) fn create_actions(&mut self, ctx: &egui::Context) {
        let p = pal_ctx(ctx);
        let (listed, _) = self.listed(&self.image.needed());
        let disk = logic::selected_disk(&self.create_disk, &listed).cloned();
        let loaded = self.image.loaded();
        let name = loaded.map(|l| logic::file_name(&l.info.path));
        let blocking = loaded
            .map(|l| {
                plan::check_options(&l.info, &l.advice, &self.opts, disk.as_ref())
                    .iter()
                    .any(|i| i.blocking)
            })
            .unwrap_or(false);
        let summary = logic::summary(
            name.as_deref(),
            disk.as_ref(),
            self.opts.mode,
            self.opts.scheme,
        );
        let blocker = logic::write_blocker(loaded.is_some(), disk.is_some(), blocking);
        let need_admin = !self.demo && !self.elevated;
        let demo = self.demo;
        let mut write = false;
        let mut restart = false;
        egui::TopBottomPanel::bottom("create-actions")
            .exact_height(64.0)
            .frame(
                Frame::new()
                    .fill(p.bg)
                    .inner_margin(Margin::symmetric(24, 0)),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter().hline(
                    full.x_range().expand(24.0),
                    full.top(),
                    Stroke::new(1.0_f32, p.border),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if need_admin {
                        if widgets::primary_button(
                            ui,
                            &Icon::SHIELD.with("Restart as administrator to write"),
                            true,
                        )
                        .on_hover_text(
                            "Windows only lets administrators write to a drive. BootForge restarts with the same image and drive selected.",
                        )
                        .clicked()
                        {
                            restart = true;
                        }
                        ui.label(
                            RichText::new("Writing needs administrator rights")
                                .size(13.0)
                                .color(p.warn),
                        );
                    } else {
                        let button =
                            widgets::danger_button(ui, &Icon::USB.with("Write"), blocker.is_none());
                        if let Some(why) = blocker {
                            button.on_disabled_hover_text(why);
                        } else if button.clicked() {
                            write = true;
                        }
                        if demo {
                            ui.label(
                                RichText::new("Demo mode: no drive is touched")
                                    .size(13.0)
                                    .color(p.info),
                            );
                        }
                    }
                    ui.add_space(8.0);
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let color = if blocker.is_none() { p.text } else { p.muted };
                        ui.add(
                            egui::Label::new(RichText::new(summary).size(14.5).color(color))
                                .truncate(),
                        );
                    });
                });
            });
        if restart {
            self.restart_elevated(ctx);
        }
        if write {
            self.request_write();
        }
    }
}

/// A caption with a control under it, in a column of fixed width.
pub(super) fn column(ui: &mut Ui, width: f32, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.vertical(|ui| {
        ui.set_width(width);
        parts::field(ui, label, add);
    });
}

/// The label text field shared by Create and "restore to an empty drive".
pub(super) fn label_edit(ui: &mut Ui, label: &mut String, fs: FileSystem, width: f32) {
    let resp = ui.add(
        egui::TextEdit::singleline(label)
            .char_limit(11)
            .desired_width(width)
            .margin(Margin::symmetric(8, 6)),
    );
    if resp.changed() {
        *label = logic::label_typing(label, fs);
    }
    if resp.lost_focus() {
        *label = logic::final_label(label, fs);
    }
    resp.on_hover_text("At most 11 characters.");
}

pub(super) fn cluster_combo(ui: &mut Ui, id: &str, cluster: &mut Option<u32>) {
    let current = logic::cluster_index(*cluster);
    egui::ComboBox::from_id_salt(id)
        .width(104.0)
        .selected_text(logic::CLUSTER_LABELS[current])
        .show_ui(ui, |ui| {
            for (i, label) in logic::CLUSTER_LABELS.iter().enumerate() {
                if ui.selectable_label(i == current, *label).clicked() {
                    *cluster = logic::CLUSTERS[i];
                }
            }
        });
}

fn files_options(ui: &mut Ui, opts: &mut WriteOptions) {
    let p = pal(ui);
    let w = ui.available_width();
    let gap = 14.0;
    let c1 = 136.0_f32;
    let c2 = 190.0_f32;
    let c3 = (w - c1 - c2 - 2.0 * gap).max(110.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        column(ui, c1, "Partition scheme", |ui| {
            let current = if opts.scheme == Scheme::Gpt { 0 } else { 1 };
            if let Some(i) = widgets::segmented(ui, &["GPT", "MBR"], current) {
                opts.scheme = if i == 0 { Scheme::Gpt } else { Scheme::Mbr };
            }
        });
        column(ui, c2, "Target system", |ui| {
            let current = if opts.target == TargetSystem::Uefi {
                0
            } else {
                1
            };
            if let Some(i) = widgets::segmented(ui, &["UEFI", "BIOS / CSM"], current) {
                opts.target = if i == 0 {
                    TargetSystem::Uefi
                } else {
                    TargetSystem::Bios
                };
            }
        });
        column(ui, c3, "File system", |ui| {
            ui.horizontal(|ui| {
                ui.set_height(30.0);
                ui.label(RichText::new(opts.fs.label()).font(semibold(14.0)));
                ui.label(RichText::new("required by UEFI").size(12.5).color(p.muted));
            })
            .response
            .on_hover_text(
                "File copy always formats FAT32: it is the one file system every UEFI firmware can start from.",
            );
        });
    });
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        column(ui, c1, "Volume label", |ui| {
            label_edit(ui, &mut opts.label, opts.fs, c1 - 18.0);
        });
        column(ui, c2, "Cluster size", |ui| {
            cluster_combo(ui, "create-cluster", &mut opts.cluster_size);
        });
        column(ui, c3, "Quick format", |ui| {
            ui.horizontal(|ui| {
                ui.set_height(30.0);
                if widgets::toggle(ui, opts.quick_format, true).clicked() {
                    opts.quick_format = !opts.quick_format;
                }
                ui.label(
                    RichText::new(if opts.quick_format { "On" } else { "Off: zero first" })
                        .size(12.5)
                        .color(p.muted),
                );
            })
            .response
            .on_hover_text(
                "Off: the whole partition is overwritten with zeros before formatting, which takes much longer.",
            );
        });
    });
}

/// A switch with a short state text next to it, as tall as the other fields.
fn switch_field(ui: &mut Ui, value: &mut bool, enabled: bool, state: &str, hover: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.set_height(30.0);
        if widgets::toggle(ui, *value && enabled, enabled).clicked() {
            *value = !*value;
        }
        ui.label(RichText::new(state).size(12.5).color(p.muted));
    })
    .response
    .on_hover_text(hover);
}

/// The Windows Setup choices of file-copy mode (shown for Windows setup images only).
fn setup_options(ui: &mut Ui, support: TweakSupport, tweaks: &mut SetupTweaks) {
    match support {
        TweakSupport::NotWindows => {}
        TweakSupport::OwnAnswerFile => hint_line(
            ui,
            "Windows Setup options are not offered: this image brings its own answer file (autounattend.xml), which is copied as it is.",
        ),
        TweakSupport::Available { windows11 } => {
            let w = ui.available_width();
            let gap = 14.0;
            let c1 = 136.0_f32;
            let c2 = 190.0_f32;
            let c3 = (w - c1 - c2 - 2.0 * gap).max(110.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                column(ui, c1, "Windows 11 checks", |ui| {
                    let state = match (windows11, tweaks.skip_requirements) {
                        (false, _) => "Not in this image",
                        (true, true) => "Skipped",
                        (true, false) => "Kept",
                    };
                    switch_field(
                        ui,
                        &mut tweaks.skip_requirements,
                        windows11,
                        state,
                        "Skip the TPM 2.0, Secure Boot and 4 GB memory checks, so that Windows 11 Setup continues on a PC that does not meet them.",
                    );
                });
                column(ui, c2, "Local account", |ui| {
                    let state = if tweaks.local_account {
                        "No Microsoft sign-in"
                    } else {
                        "As Setup asks"
                    };
                    switch_field(
                        ui,
                        &mut tweaks.local_account,
                        true,
                        state,
                        "The first start asks for a local user name instead of a Microsoft account, and works without an internet connection.",
                    );
                });
                column(ui, c3, "Privacy questions", |ui| {
                    let state = if tweaks.skip_privacy {
                        "Skipped, all off"
                    } else {
                        "Asked"
                    };
                    switch_field(
                        ui,
                        &mut tweaks.skip_privacy,
                        true,
                        state,
                        "The questions about location, diagnostics and advertising are not shown; all of them are answered with \"no\".",
                    );
                });
            });
            if tweaks.any() {
                hint_line(
                    ui,
                    "Windows Setup: adds a small answer file (autounattend.xml) to the drive. The files of the image are not changed; delete that file to get the plain Setup back.",
                );
            }
        }
    }
}

/// One line of muted text, cut with an ellipsis; the whole text shows on hover.
fn hint_line(ui: &mut Ui, text: &str) {
    let p = pal(ui);
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(p.muted)).truncate())
        .on_hover_text(text);
}
