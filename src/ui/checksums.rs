//! The Checksums page: MD5, SHA-1 and SHA-256 of a file, and a comparison
//! with a pasted value.

use super::app::{App, Msg};
use super::logic::{self, VerdictTone};
use super::parts;
use super::theme::{pal, regular, semibold, Icon};
use super::widgets;
use crate::backend::{self, HashVerdict};
use crate::demo;
use crate::model::{Cancel, Checksums};
use crate::util::human_size;
use eframe::egui::{
    self, vec2, Align, Align2, FontFamily, FontId, Frame, Layout, Margin, RichText, Sense, Ui,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct HashRun {
    cancel: Cancel,
    done: Arc<AtomicU64>,
}

#[derive(Default)]
pub struct HashState {
    pub file: Option<PathBuf>,
    pub size: Option<u64>,
    pub run: Option<HashRun>,
    pub sums: Option<Checksums>,
    pub error: Option<String>,
    pub pasted: String,
    /// Look for a checksum file next to a chosen file (off in demo mode,
    /// which reads nothing).
    pub probe: bool,
    /// The checksum file found next to the file, and the checksum it lists.
    pub published: Option<(String, String)>,
    gen: u64,
    /// A verdict fixed for the screenshot, valid while the pasted text is this.
    staged: Option<(String, HashVerdict)>,
}

impl HashState {
    /// `probe`: look for a checksum file next to a chosen file.
    pub fn new(probe: bool) -> HashState {
        HashState {
            probe,
            ..HashState::default()
        }
    }

    /// Selects a file; results of another file are dropped.
    pub fn set_file(&mut self, path: PathBuf, size: Option<u64>) {
        if self.file.as_ref() == Some(&path) {
            self.size = size.or(self.size);
            return;
        }
        self.cancel();
        // A checksum that was filled in for the previous file does not
        // belong to this one; what the user pasted stays.
        if let Some((_, old)) = self.published.take() {
            if self.pasted == old {
                self.pasted.clear();
            }
        }
        if self.probe {
            self.published = backend::published_checksum(&path);
            if let Some((_, digest)) = &self.published {
                if self.pasted.trim().is_empty() {
                    self.pasted = digest.clone();
                }
            }
        }
        self.file = Some(path);
        self.size = size;
        self.sums = None;
        self.error = None;
    }

    /// Stops a running computation and ignores whatever it still reports.
    pub fn cancel(&mut self) {
        if let Some(run) = self.run.take() {
            run.cancel.cancel();
        }
        self.gen += 1;
    }
}

impl App {
    pub(super) fn stage_demo_checksums(&mut self) {
        let image = demo::windows_image();
        self.hash.set_file(image.path, Some(image.size));
        let sums = demo::checksums();
        self.hash.pasted = sums.sha256.clone();
        self.hash.staged = Some((sums.sha256.clone(), HashVerdict::Match("SHA-256")));
        self.hash.published = Some(("SHA256SUMS".into(), sums.sha256.clone()));
        self.hash.sums = Some(sums);
    }

    pub(super) fn choose_hash_file(&mut self) {
        if self.taking_screenshot() {
            return;
        }
        if self.demo {
            let image = demo::windows_image();
            self.hash.set_file(image.path, Some(image.size));
            return;
        }
        let mut dialog = rfd::FileDialog::new().set_title("Choose a file");
        if let Some(dir) = &self.settings.last_image_dir {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            let size = std::fs::metadata(&path).ok().map(|m| m.len());
            self.hash.set_file(path, size);
        }
    }

    pub(super) fn start_hash(&mut self, ctx: &egui::Context) {
        let Some(path) = self.hash.file.clone() else {
            return;
        };
        self.hash.cancel();
        self.hash.sums = None;
        self.hash.error = None;
        let cancel = Cancel::new();
        let done = Arc::new(AtomicU64::new(0));
        self.hash.run = Some(HashRun {
            cancel: cancel.clone(),
            done: done.clone(),
        });
        let gen = self.hash.gen;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let demo = self.demo;
        let total = self.hash.size.unwrap_or(0);
        std::thread::spawn(move || {
            let result = if demo {
                // Nothing is read in demo mode: the bar is played.
                let mut out = Ok(demo::checksums());
                for i in 1..=40u64 {
                    if cancel.is_cancelled() {
                        out = Err("Cancelled.".to_string());
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(35));
                    done.store(total / 40 * i, Ordering::Relaxed);
                    ctx.request_repaint();
                }
                out
            } else {
                backend::hash_file(&path, &cancel, &mut |n| done.store(n, Ordering::Relaxed))
                    .map_err(|e| e.to_string())
            };
            let _ = tx.send(Msg::Hashed(gen, result));
            ctx.request_repaint();
        });
    }

    pub(super) fn hash_finished(&mut self, gen: u64, result: Result<Checksums, String>) {
        if gen != self.hash.gen {
            return;
        }
        self.hash.run = None;
        match result {
            Ok(sums) => {
                if !self.demo {
                    if let Some(f) = &self.hash.file {
                        self.logbook.push(format!(
                            "Checksums of {}: SHA-256 {}",
                            logic::file_name(f),
                            sums.sha256
                        ));
                    }
                }
                self.hash.sums = Some(sums);
            }
            Err(e) => self.hash.error = Some(e),
        }
    }

    /// Where the image on the Create page stands against a checksum that was
    /// published next to it (`None`: no such checksum, or another file is
    /// on the Checksums page).
    pub(super) fn published_check(&self) -> Option<logic::PublishedCheck> {
        let (file, digest) = self.hash.published.as_ref()?;
        if self.hash.file.as_deref() != self.image.path() {
            return None;
        }
        let verdict = self
            .hash
            .sums
            .as_ref()
            .map(|sums| backend::compare_hash(digest, sums));
        Some(logic::published_check(
            file,
            self.hash.run.is_some(),
            verdict,
        ))
    }

    fn verdict(&self) -> HashVerdict {
        let Some(sums) = &self.hash.sums else {
            return HashVerdict::Empty;
        };
        if let Some((text, verdict)) = &self.hash.staged {
            if *text == self.hash.pasted {
                return *verdict;
            }
        }
        backend::compare_hash(&self.hash.pasted, sums)
    }

    pub(super) fn checksums_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        parts::page_header(
            ui,
            "Checksums",
            "Make sure a download arrived intact before you write it.",
            |_| {},
        );
        let mut choose = false;
        let mut compute = false;
        let mut cancel = false;
        let mut copied: Option<&'static str> = None;
        let verdict = self.verdict();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(860.0);
                ui.spacing_mut().item_spacing.y = 12.0;

                widgets::card(ui, |ui| {
                    widgets::section(ui, "File");
                    match &self.hash.file {
                        None => {
                            let zone = Frame::new()
                                .inner_margin(Margin::symmetric(16, 22))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.vertical_centered(|ui| {
                                        ui.label(
                                            RichText::new("Drop a file here").font(semibold(16.0)),
                                        );
                                        ui.label(
                                            RichText::new("or pick one; any file works, not only images")
                                                .size(13.0)
                                                .color(p.muted),
                                        );
                                        ui.add_space(6.0);
                                        if widgets::primary_button(
                                            ui,
                                            &Icon::OPEN_FILE.with("Choose file…"),
                                            true,
                                        )
                                        .clicked()
                                        {
                                            choose = true;
                                        }
                                    });
                                })
                                .response;
                            parts::dashed_border(ui, zone.rect, p.faint.gamma_multiply(0.8));
                        }
                        Some(path) => {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(Icon::DOCUMENT.glyph())
                                        .size(18.0)
                                        .color(p.accent),
                                );
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    ui.label(
                                        RichText::new(logic::file_name(path)).font(semibold(15.0)),
                                    );
                                    let folder = path
                                        .parent()
                                        .map(|d| d.display().to_string())
                                        .unwrap_or_default();
                                    let detail = match self.hash.size {
                                        Some(s) => format!("{}  ·  {folder}", human_size(s)),
                                        None => folder,
                                    };
                                    let w = (ui.available_width() - 250.0).max(120.0);
                                    let (rect, _) =
                                        ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
                                    widgets::text_in(ui, rect, &detail, regular(13.0), p.muted);
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if self.hash.run.is_some() {
                                        if widgets::ghost_button(ui, "Cancel").clicked() {
                                            cancel = true;
                                        }
                                    } else {
                                        if widgets::primary_button(
                                            ui,
                                            if self.hash.sums.is_some() {
                                                "Compute again"
                                            } else {
                                                "Compute"
                                            },
                                            true,
                                        )
                                        .clicked()
                                        {
                                            compute = true;
                                        }
                                        if widgets::ghost_button(ui, "Change…").clicked() {
                                            choose = true;
                                        }
                                    }
                                });
                            });
                            if let Some(run) = &self.hash.run {
                                let done = run.done.load(Ordering::Relaxed);
                                let fraction = match self.hash.size {
                                    Some(total) if total > 0 => {
                                        Some((done as f64 / total as f64).min(1.0) as f32)
                                    }
                                    _ => None,
                                };
                                parts::progress_bar(ui, fraction, 10.0, p.accent);
                                let text = match (fraction, self.hash.size) {
                                    (Some(f), Some(total)) => format!(
                                        "{:.0} %  ·  {} of {}",
                                        f * 100.0,
                                        human_size(done),
                                        human_size(total)
                                    ),
                                    _ => format!("{} read", human_size(done)),
                                };
                                ui.label(RichText::new(text).size(13.0).color(p.muted));
                                ui.ctx().request_repaint_after(Duration::from_millis(100));
                            }
                            if let Some(e) = &self.hash.error {
                                parts::note(
                                    ui,
                                    Icon::ERROR,
                                    p.danger,
                                    &format!("The checksums could not be computed: {e}"),
                                );
                            }
                        }
                    }
                });

                widgets::card(ui, |ui| {
                    widgets::section(ui, "Checksums");
                    let rows: [(&'static str, Option<&str>); 3] = [
                        ("MD5", self.hash.sums.as_ref().map(|s| s.md5.as_str())),
                        ("SHA-1", self.hash.sums.as_ref().map(|s| s.sha1.as_str())),
                        ("SHA-256", self.hash.sums.as_ref().map(|s| s.sha256.as_str())),
                    ];
                    let matched = match verdict {
                        HashVerdict::Match(a) => Some(a),
                        _ => None,
                    };
                    for (i, (name, value)) in rows.into_iter().enumerate() {
                        if i > 0 {
                            widgets::divider(ui);
                        }
                        ui.horizontal(|ui| {
                            ui.set_height(32.0);
                            let (rect, _) = ui.allocate_exact_size(vec2(84.0, 20.0), Sense::hover());
                            ui.painter().text(
                                rect.left_center(),
                                Align2::LEFT_CENTER,
                                name,
                                semibold(13.5),
                                p.muted,
                            );
                            match value {
                                Some(v) => {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(v)
                                                .font(FontId::new(13.5, FontFamily::Monospace))
                                                .color(if matched == Some(name) {
                                                    p.accent
                                                } else {
                                                    p.text
                                                }),
                                        )
                                        .selectable(true)
                                        .truncate(),
                                    );
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if widgets::icon_button(ui, Icon::COPY, "Copy").clicked() {
                                            ui.ctx().copy_text(v.to_string());
                                            copied = Some(name);
                                        }
                                        if matched == Some(name) {
                                            widgets::badge(ui, "Match", p.accent);
                                        }
                                    });
                                }
                                None => {
                                    ui.label(
                                        RichText::new(if self.hash.run.is_some() {
                                            "computing…"
                                        } else {
                                            "not computed yet"
                                        })
                                        .color(p.faint),
                                    );
                                }
                            }
                        });
                    }
                });

                widgets::card(ui, |ui| {
                    widgets::section(ui, "Compare");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.hash.pasted)
                            .hint_text(
                                RichText::new("Paste a checksum to compare").color(p.faint),
                            )
                            .font(FontId::new(13.5, FontFamily::Monospace))
                            .margin(Margin::symmetric(10, 8))
                            .desired_width(f32::INFINITY),
                    );
                    if let Some((file, digest)) = &self.hash.published {
                        if self.hash.pasted == *digest {
                            ui.label(
                                RichText::new(format!(
                                    "Filled in from {file}, found next to the file."
                                ))
                                .size(12.5)
                                .color(p.muted),
                            );
                        }
                    }
                    ui.horizontal(|ui| {
                        ui.set_height(22.0);
                        if self.hash.sums.is_none() {
                            ui.label(
                                RichText::new(
                                    "Compute the checksums first; the pasted value is then checked against all three.",
                                )
                                .size(13.0)
                                .color(p.faint),
                            );
                        } else {
                            match logic::verdict_text(verdict) {
                                None => {
                                    ui.label(
                                        RichText::new(
                                            "The publisher of an image usually lists its SHA-256 next to the download.",
                                        )
                                        .size(13.0)
                                        .color(p.faint),
                                    );
                                }
                                Some((text, tone)) => {
                                    let (icon, color) = match tone {
                                        VerdictTone::Good => (Icon::DONE, p.accent),
                                        VerdictTone::Bad => (Icon::ERROR, p.danger),
                                        VerdictTone::Muted => (Icon::INFO, p.muted),
                                    };
                                    ui.label(RichText::new(icon.glyph()).color(color));
                                    ui.label(RichText::new(text).font(semibold(14.5)).color(color));
                                    if tone == VerdictTone::Bad {
                                        ui.label(
                                            RichText::new(
                                                "The file is not the one that checksum belongs to. Download it again.",
                                            )
                                            .size(13.0)
                                            .color(p.muted),
                                        );
                                    }
                                }
                            }
                        }
                    });
                });
            });
        if choose {
            self.choose_hash_file();
        }
        if cancel {
            self.hash.cancel();
        }
        if compute {
            self.start_hash(&ctx);
        }
        if let Some(name) = copied {
            self.toast(format!("{name} copied"), false);
        }
    }
}
