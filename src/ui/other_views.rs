//! The Log and Settings pages.

use super::app::App;
use super::logic;
use super::parts;
use super::theme::{self, pal, regular, Icon};
use super::widgets;
use crate::settings::ThemeChoice;
use eframe::egui::{
    self, Align, CornerRadius, FontFamily, FontId, Frame, Layout, Margin, RichText, Stroke, Ui,
};

impl App {
    pub(super) fn log_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let count = self.logbook.len();
        let subtitle = match count {
            0 => "Nothing has happened yet".to_string(),
            1 => "1 line  ·  times are UTC".to_string(),
            n => format!("{n} lines  ·  times are UTC"),
        };
        let mut save = false;
        let mut folder = false;
        let mut copy = false;
        let mut clear = false;
        parts::page_header(ui, "Log", &subtitle, |ui| {
            let some = count > 0;
            if widgets::ghost_button_enabled(ui, &Icon::DELETE.with("Clear"), some).clicked() {
                clear = true;
            }
            if widgets::ghost_button_enabled(ui, &Icon::COPY.with("Copy"), some).clicked() {
                copy = true;
            }
            if widgets::ghost_button(ui, &Icon::FOLDER_OPEN.with("Open log folder")).clicked() {
                folder = true;
            }
            if widgets::ghost_button_enabled(ui, &Icon::SAVE.with("Save as…"), some).clicked() {
                save = true;
            }
        });
        Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0_f32, p.border))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_height(ui.available_height());
                if count == 0 {
                    widgets::empty_state(
                        ui,
                        Icon::DOCUMENT,
                        p.muted,
                        "The log is empty",
                        "Every step of a write, backup or reset is recorded here, with the exact numbers.",
                    );
                    return;
                }
                let mono = FontId::new(13.0, FontFamily::Monospace);
                let row = 19.0;
                ui.spacing_mut().item_spacing.y = 0.0;
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show_rows(ui, row, count, |ui, range| {
                        let lines = self.logbook.lines();
                        for line in &lines[range.start.min(lines.len())..range.end.min(lines.len())]
                        {
                            ui.horizontal(|ui| {
                                ui.set_height(row);
                                ui.spacing_mut().item_spacing.x = 14.0;
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&line.time).font(mono.clone()).color(p.faint),
                                    )
                                    .extend(),
                                );
                                let head = line.text.starts_with("== ");
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&line.text)
                                            .font(mono.clone())
                                            .color(if head { p.accent } else { p.text }),
                                    )
                                    .selectable(true)
                                    .extend(),
                                );
                            });
                        }
                    });
            });
        if clear {
            self.logbook.clear();
        }
        if copy {
            ui.ctx().copy_text(self.logbook.text());
            self.toast("Log copied", false);
        }
        if folder {
            self.open_log_folder();
        }
        if save && !self.taking_screenshot() {
            if self.demo {
                self.toast("Demo mode: nothing is saved.", false);
            } else if let Some(path) = rfd::FileDialog::new()
                .set_title("Save the log as")
                .add_filter("Log file", &["log", "txt"])
                .set_file_name("bootforge.log")
                .save_file()
            {
                match self.logbook.save_to(&path) {
                    Ok(()) => self.toast(format!("Saved {}", logic::file_name(&path)), false),
                    Err(e) => self.toast(e, true),
                }
            }
        }
    }

    pub(super) fn settings_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        parts::page_header(
            ui,
            "Settings",
            if self.demo {
                "Demo mode: changes last until the window closes"
            } else {
                "Saved automatically"
            },
            |_| {},
        );
        let before = self.settings.clone();
        let mut about = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(820.0);
                ui.spacing_mut().item_spacing.y = 12.0;

                // `reserve` is the width kept free for the control on the right.
                let row = |ui: &mut Ui,
                           title: &str,
                           text: &str,
                           reserve: f32,
                           control: &mut dyn FnMut(&mut Ui)| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width((ui.available_width() - reserve).max(200.0));
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new(title).font(regular(14.5)));
                            ui.add(
                                egui::Label::new(RichText::new(text).size(12.5).color(p.muted))
                                    .wrap(),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| control(ui));
                    });
                };
                let s = &mut self.settings;

                widgets::card(ui, |ui| {
                    widgets::section(ui, "Appearance");
                    ui.add_space(2.0);
                    row(
                        ui,
                        "Theme",
                        "\"System\" follows the Windows light or dark setting.",
                        330.0,
                        &mut |ui| {
                            let options =
                                [ThemeChoice::Dark, ThemeChoice::Light, ThemeChoice::System];
                            let current =
                                options.iter().position(|t| *t == s.theme).unwrap_or(0);
                            if let Some(i) =
                                widgets::segmented(ui, &["Dark", "Light", "System"], current)
                            {
                                s.theme = options[i];
                            }
                        },
                    );
                });

                widgets::card(ui, |ui| {
                    widgets::section(ui, "Safety");
                    ui.add_space(2.0);
                    row(
                        ui,
                        "Show USB hard drives",
                        "Also offers USB hard drives, SSD enclosures and other USB disks with fixed media. These often hold backups and photo libraries: with this on, look twice at the drive you pick.",
                        120.0,
                        &mut |ui| {
                            if widgets::toggle(ui, s.policy.show_usb_hdd, true).clicked() {
                                s.policy.show_usb_hdd = !s.policy.show_usb_hdd;
                            }
                        },
                    );
                    if s.policy.show_usb_hdd {
                        parts::note(
                            ui,
                            Icon::WARNING,
                            p.warn,
                            "USB hard drives are now offered as targets. Everything on the drive you choose is erased.",
                        );
                    }
                    widgets::divider(ui);
                    ui.label(RichText::new("Largest drive offered").font(regular(14.5)));
                    ui.add_space(-8.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Drives larger than this are not offered at all. Raise it only when your USB drive really is that large.",
                            )
                            .size(12.5)
                            .color(p.muted),
                        )
                        .wrap(),
                    );
                    ui.horizontal(|ui| {
                        let current = logic::cap_index(s.policy.max_size);
                        if let Some(i) = widgets::segmented(ui, &logic::CAP_LABELS, current) {
                            s.policy.max_size = logic::cap_bytes(i);
                        }
                    });
                    widgets::divider(ui);
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new(Icon::SHIELD.glyph()).color(p.faint));
                        ui.add(
                            egui::Label::new(
                                RichText::new(
                                    "Internal disks, the disk Windows runs from and the disk BootForge runs from are never offered, whatever is set here.",
                                )
                                .size(12.5)
                                .color(p.faint),
                            )
                            .wrap(),
                        );
                    });
                });

                widgets::card(ui, |ui| {
                    widgets::section(ui, "Defaults");
                    ui.add_space(2.0);
                    let toggle_row =
                        |ui: &mut Ui, title: &str, text: &str, value: &mut bool| {
                            row(ui, title, text, 120.0, &mut |ui| {
                                if widgets::toggle(ui, *value, true).clicked() {
                                    *value = !*value;
                                }
                            });
                        };
                    toggle_row(
                        ui,
                        "Verify after writing",
                        "Reads everything back and compares it with the image. Takes about as long as the write itself.",
                        &mut s.verify,
                    );
                    widgets::divider(ui);
                    toggle_row(
                        ui,
                        "Eject when done",
                        "Safely removes the drive at the end, so it can be unplugged right away.",
                        &mut s.eject,
                    );
                    widgets::divider(ui);
                    toggle_row(
                        ui,
                        "Quick format",
                        "Writes only the file-system structures. Off: the whole partition is overwritten with zeros first.",
                        &mut s.quick_format,
                    );
                    widgets::divider(ui);
                    toggle_row(
                        ui,
                        "Save a log file for every operation",
                        "Kept in the log folder; useful when something goes wrong and you want to report it.",
                        &mut s.save_logs,
                    );
                });

                widgets::card(ui, |ui| {
                    row(
                        ui,
                        "About BootForge",
                        &format!("Version {}", env!("CARGO_PKG_VERSION")),
                        200.0,
                        &mut |ui| {
                            if widgets::ghost_button(ui, "About").clicked() {
                                about = true;
                            }
                        },
                    );
                });
                ui.add_space(4.0);
            });
        if about {
            self.about_open = true;
        }
        if self.settings != before {
            if self.settings.theme != before.theme {
                theme::apply_choice(ui.ctx(), self.settings.theme);
            }
            if self.settings.policy != before.policy {
                self.prune_selections();
            }
            // New selections start from the defaults; keep the current
            // choices of a picked image as they are.
            if self.image.loaded().is_none() {
                self.opts.verify = self.settings.verify;
                self.opts.eject = self.settings.eject;
                self.opts.quick_format = self.settings.quick_format;
            }
            self.save_settings();
        }
    }
}
