//! Application state, background work plumbing and the window frame
//! (header, navigation rail, About dialog, toasts, screenshots).

use super::checksums::HashState;
use super::job_view::{PendingJob, Running};
use super::logic;
use super::theme::{self, pal_ctx, regular, semibold, Icon};
use super::tools::ToolsState;
use super::widgets;
use crate::logbook::{Logbook, Stamp};
use crate::model::{Checksums, Disk, DiskIdentity, ImageInfo, WriteOptions};
use crate::plan::{self, Advice};
use crate::safety::{self, Listed};
use crate::settings::{self, Settings, ThemeChoice};
use crate::{backend, demo};
use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, Frame, Key, Layout, Margin, Rect, RichText,
    Sense, Stroke, TextureHandle,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// How the app was started.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Show invented drives and images and never touch a device.
    pub demo: bool,
    /// Render one frame of the chosen view to this PNG, then exit.
    pub screenshot: Option<PathBuf>,
    /// Which view to open first.
    pub view: Option<String>,
    /// Force the light theme (for screenshots).
    pub light: bool,
    /// Image to select at start.
    pub image: Option<PathBuf>,
    /// Disk number to preselect at start.
    pub disk: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Create,
    Checksums,
    Tools,
    Log,
    Settings,
}

/// Which image picker an inspection belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Create,
    Restore,
}

/// An inspected image with the advice derived from it.
pub struct Loaded {
    pub info: ImageInfo,
    pub advice: Advice,
}

pub enum ImageSlot {
    Empty,
    Inspecting(PathBuf),
    Failed(PathBuf, String),
    Ready(Box<Loaded>),
}

impl ImageSlot {
    pub fn path(&self) -> Option<&Path> {
        match self {
            ImageSlot::Empty => None,
            ImageSlot::Inspecting(p) | ImageSlot::Failed(p, _) => Some(p),
            ImageSlot::Ready(l) => Some(&l.info.path),
        }
    }
    pub fn loaded(&self) -> Option<&Loaded> {
        match self {
            ImageSlot::Ready(l) => Some(l),
            _ => None,
        }
    }
    pub fn inspecting(&self) -> bool {
        matches!(self, ImageSlot::Inspecting(_))
    }
    /// The files a drive must not hold while this image is used.
    pub fn needed(&self) -> Vec<PathBuf> {
        self.path()
            .map(|p| vec![p.to_path_buf()])
            .unwrap_or_default()
    }
}

pub enum Msg {
    Disks(u64, Result<Vec<Disk>, String>),
    DevicesChanged,
    Inspected(Slot, u64, Result<Box<Loaded>, String>),
    Hashed(u64, Result<Checksums, String>),
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub born: Instant,
}

struct Shot {
    path: PathBuf,
    frames: u32,
    requested: bool,
}

pub struct App {
    pub(super) demo: bool,
    pub(super) elevated: bool,
    pub(super) app_dir: PathBuf,
    pub(super) settings: Settings,
    pub(super) tab: Tab,

    pub(super) disks: Vec<Disk>,
    pub(super) disks_loading: bool,
    pub(super) disks_error: Option<String>,
    disk_gen: u64,
    last_counts: Option<(usize, usize)>,
    pending_disk: Option<u32>,
    _watch: Option<backend::WatchHandle>,

    pub(super) image: ImageSlot,
    pub(super) opts: WriteOptions,
    pub(super) create_disk: Option<DiskIdentity>,
    inspect_gen: [u64; 2],
    demo_cycle: usize,

    pub(super) tools: ToolsState,
    pub(super) hash: HashState,

    pub(super) pending: Option<PendingJob>,
    pub(super) running: Option<Running>,
    pub(super) closing: Option<Instant>,

    pub(super) logbook: Logbook,
    pub(super) about_open: bool,
    pub(super) toasts: Vec<Toast>,
    pub(super) tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pub(super) logo: Option<TextureHandle>,
    shot: Option<Shot>,
    /// Demo only: start the simulated job of the staged selection at once.
    autostart: bool,
}

pub fn run(options: Options, icon: egui::IconData) -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_title(if options.demo {
            "BootForge (demo)"
        } else {
            "BootForge"
        })
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([980.0, 620.0])
        .with_drag_and_drop(true)
        .with_icon(icon.clone());
    let native = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "BootForge",
        native,
        Box::new(move |cc| Ok(Box::new(App::new(cc, options, icon)))),
    )
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, options: Options, icon: egui::IconData) -> Self {
        let ctx = &cc.egui_ctx;
        theme::install(ctx);
        // Demo mode never loads or saves anything; the folder is not used then.
        let app_dir = if options.demo {
            PathBuf::new()
        } else {
            settings::app_dir()
        };
        let mut settings = if options.demo {
            Settings::default()
        } else {
            Settings::load(&app_dir)
        };
        if options.light {
            settings.theme = ThemeChoice::Light;
        }
        theme::apply_choice(ctx, settings.theme);

        let logo = (icon.width > 0).then(|| {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            );
            ctx.load_texture("logo", image, egui::TextureOptions::LINEAR)
        });
        let (tx, rx) = channel();
        let mut logbook = Logbook::new();
        if options.demo {
            // Fixed times keep the screenshots stable.
            for (i, line) in demo::log_lines().into_iter().enumerate() {
                logbook.push_at(Stamp::from_unix(1_763_287_200 + i as u64 * 13), line);
            }
        } else {
            logbook.push(format!("BootForge {} started.", env!("CARGO_PKG_VERSION")));
        }
        let elevated = options.demo || backend::is_elevated();
        let watch = (!options.demo && options.screenshot.is_none()).then(|| {
            let tx = tx.clone();
            let ctx = ctx.clone();
            backend::watch_devices(move || {
                let _ = tx.send(Msg::DevicesChanged);
                ctx.request_repaint();
            })
        });
        let mut app = App {
            demo: options.demo,
            elevated,
            app_dir,
            tools: ToolsState::new(&settings),
            settings,
            tab: Tab::Create,
            disks: Vec::new(),
            disks_loading: false,
            disks_error: None,
            disk_gen: 0,
            last_counts: None,
            pending_disk: options.disk,
            _watch: watch,
            image: ImageSlot::Empty,
            opts: WriteOptions::default(),
            create_disk: None,
            inspect_gen: [0; 2],
            demo_cycle: 0,
            hash: HashState::default(),
            pending: None,
            running: None,
            closing: None,
            logbook,
            about_open: false,
            toasts: Vec::new(),
            tx,
            rx,
            logo,
            autostart: false,
            shot: options.screenshot.clone().map(|path| Shot {
                path,
                frames: 0,
                requested: false,
            }),
        };
        app.opts.verify = app.settings.verify;
        app.opts.eject = app.settings.eject;
        app.opts.quick_format = app.settings.quick_format;
        app.reload_disks(ctx);
        if let Some(path) = &options.image {
            app.set_image(ctx, Slot::Create, path.clone());
        }
        app.apply_view(options.view.as_deref().unwrap_or("create"));
        app
    }

    /// Opens the view named on the command line. The staged ones (a chosen
    /// image, an open dialog, a job) exist only in demo mode.
    fn apply_view(&mut self, view: &str) {
        let demo = self.demo;
        let stage_create = |app: &mut App, linux: bool| {
            let info = if linux {
                demo::linux_image()
            } else {
                demo::windows_image()
            };
            app.set_loaded(Slot::Create, info, demo_env());
            let (listed, _) = app.listed(&app.image.needed());
            app.create_disk = listed
                .iter()
                .find(|l| l.blocked.is_none())
                .map(|l| l.disk.identity());
        };
        match view {
            "create" if demo => stage_create(self, false),
            "create-linux" if demo => stage_create(self, true),
            "confirm" if demo => {
                stage_create(self, false);
                self.request_write();
            }
            "progress" if demo => {
                stage_create(self, false);
                self.stage_demo_job(false);
            }
            "demo-run" if demo => {
                stage_create(self, false);
                self.autostart = true;
            }
            "done" if demo => {
                stage_create(self, false);
                self.stage_demo_job(true);
            }
            "checksums" => {
                self.tab = Tab::Checksums;
                if demo {
                    self.stage_demo_checksums();
                }
            }
            "tools" | "drive-tools" => {
                self.tab = Tab::Tools;
                if demo {
                    self.stage_demo_tools();
                }
            }
            "log" => self.tab = Tab::Log,
            "settings" => self.tab = Tab::Settings,
            "about" => {
                if demo {
                    stage_create(self, false);
                }
                self.about_open = true;
            }
            _ => {}
        }
    }

    // ----------------------------------------------------------------------------------
    // Drives
    // ----------------------------------------------------------------------------------

    /// The drives the safety rules allow to be shown, and how many are hidden.
    pub(super) fn listed(&self, needed: &[PathBuf]) -> (Vec<Listed>, usize) {
        safety::listable(&self.disks, &self.settings.policy, needed)
    }

    pub(super) fn reload_disks(&mut self, ctx: &egui::Context) {
        if self.demo {
            self.disks = demo::disks();
            self.disks_error = None;
            self.disks_loading = false;
            self.after_disks();
            return;
        }
        self.disk_gen += 1;
        self.disks_loading = true;
        let gen = self.disk_gen;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = backend::list_disks().map_err(|e| e.to_string());
            let _ = tx.send(Msg::Disks(gen, result));
            ctx.request_repaint();
        });
    }

    /// Runs after every enumeration: preselects the disk given on the command
    /// line (once) and drops selections that no longer point to the same,
    /// usable device.
    fn after_disks(&mut self) {
        let (listed, hidden) = self.listed(&[]);
        if let Some(n) = self.pending_disk.take() {
            match listed
                .iter()
                .find(|l| l.disk.number == n && l.blocked.is_none())
            {
                Some(l) => self.create_disk = Some(l.disk.identity()),
                None => self.toast(
                    format!("Disk {n} cannot be used as a target; choose a drive from the list."),
                    true,
                ),
            }
        }
        let usable = listed.iter().filter(|l| l.blocked.is_none()).count();
        if !self.demo && self.last_counts != Some((usable, hidden)) {
            self.last_counts = Some((usable, hidden));
            self.logbook.push(format!(
                "Found {usable} usable drive{}; {hidden} hidden by the safety rules.",
                if usable == 1 { "" } else { "s" }
            ));
        }
        self.prune_selections();
    }

    /// Drops every drive selection that is not the same offered device any more.
    pub(super) fn prune_selections(&mut self) {
        let (listed, _) = self.listed(&[]);
        let mut message = None;
        for sel in [
            &mut self.create_disk,
            &mut self.tools.backup_disk,
            &mut self.tools.restore_disk,
            &mut self.tools.reset_disk,
        ] {
            if let Some(m) = logic::reconcile(sel, &self.disks, &listed) {
                message.get_or_insert(m);
            }
        }
        if let Some(m) = message {
            self.toast(m, true);
        }
    }

    pub(super) fn usable_count(&self) -> usize {
        self.listed(&[])
            .0
            .iter()
            .filter(|l| l.blocked.is_none())
            .count()
    }

    // ----------------------------------------------------------------------------------
    // Images
    // ----------------------------------------------------------------------------------

    fn slot_mut(&mut self, slot: Slot) -> &mut ImageSlot {
        match slot {
            Slot::Create => &mut self.image,
            Slot::Restore => &mut self.tools.restore_image,
        }
    }

    /// Stores an inspected image and resets what depends on it.
    pub(super) fn set_loaded(&mut self, slot: Slot, info: ImageInfo, env: plan::Env) {
        let advice = plan::advise(&info, &env);
        self.finish_inspection(slot, Ok(Box::new(Loaded { info, advice })));
    }

    fn finish_inspection(&mut self, slot: Slot, result: Result<Box<Loaded>, String>) {
        let path = self.slot_mut(slot).path().map(Path::to_path_buf);
        match result {
            Ok(loaded) => {
                if !self.demo {
                    self.logbook.push(format!(
                        "Image: {} ({})",
                        logic::file_name(&loaded.info.path),
                        loaded.advice.headline
                    ));
                }
                if slot == Slot::Create {
                    self.opts = plan::default_options(&loaded.advice, &self.settings);
                    self.hash
                        .set_file(loaded.info.path.clone(), Some(loaded.info.size));
                }
                *self.slot_mut(slot) = ImageSlot::Ready(loaded);
            }
            Err(e) => {
                if !self.demo {
                    self.logbook
                        .push(format!("The image could not be read: {e}"));
                }
                *self.slot_mut(slot) = ImageSlot::Failed(path.unwrap_or_default(), e);
            }
        }
    }

    /// Selects an image file. In demo mode the path is ignored and an
    /// invented image is used: nothing is read.
    pub(super) fn set_image(&mut self, ctx: &egui::Context, slot: Slot, path: PathBuf) {
        if self.demo {
            let info = match slot {
                Slot::Restore => demo::disk_image(),
                Slot::Create => {
                    self.demo_cycle += 1;
                    if self.demo_cycle % 2 == 1 {
                        demo::windows_image()
                    } else {
                        demo::linux_image()
                    }
                }
            };
            self.set_loaded(slot, info, demo_env());
            return;
        }
        let index = slot as usize;
        self.inspect_gen[index] += 1;
        let gen = self.inspect_gen[index];
        *self.slot_mut(slot) = ImageSlot::Inspecting(path.clone());
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = backend::inspect(&path)
                .map(|info| {
                    let advice = plan::advise(&info, &backend::plan_env());
                    Box::new(Loaded { info, advice })
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Inspected(slot, gen, result));
            ctx.request_repaint();
        });
    }

    /// Opens the system file dialog for an image (demo: picks an invented one).
    pub(super) fn choose_image(&mut self, ctx: &egui::Context, slot: Slot) {
        if self.shot.is_some() {
            return;
        }
        if self.demo {
            self.set_image(ctx, slot, PathBuf::new());
            return;
        }
        let mut dialog = rfd::FileDialog::new()
            .set_title("Choose an image")
            .add_filter("Disk images (ISO, IMG)", &["iso", "img"])
            .add_filter("ISO images", &["iso"])
            .add_filter("IMG disk images", &["img"])
            .add_filter("All files", &["*"]);
        if let Some(dir) = &self.settings.last_image_dir {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            if let Some(dir) = path.parent() {
                self.settings.last_image_dir = Some(dir.to_path_buf());
                self.save_settings();
            }
            self.set_image(ctx, slot, path);
        }
    }

    // ----------------------------------------------------------------------------------
    // Small helpers
    // ----------------------------------------------------------------------------------

    pub(super) fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast {
            text: text.into(),
            error,
            born: Instant::now(),
        });
    }

    pub(super) fn taking_screenshot(&self) -> bool {
        self.shot.is_some()
    }

    /// Saves the settings (never in demo mode).
    pub(super) fn save_settings(&mut self) {
        if self.demo {
            return;
        }
        if let Err(e) = self.settings.save(&self.app_dir) {
            self.toast(e, true);
        }
    }

    /// Restarts BootForge elevated, keeping the chosen image and drive.
    pub(super) fn restart_elevated(&mut self, ctx: &egui::Context) {
        if self.demo {
            return;
        }
        let args = logic::relaunch_args(
            self.image.path(),
            self.create_disk.as_ref().map(|d| d.number),
        );
        match backend::relaunch_elevated(&args) {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(e) => self.toast(
                format!("BootForge could not be restarted as administrator: {e}"),
                true,
            ),
        }
    }

    pub(super) fn job_active(&self) -> bool {
        self.running
            .as_ref()
            .map(|r| r.outcome.is_none())
            .unwrap_or(false)
    }

    pub(super) fn refresh(&mut self, ctx: &egui::Context) {
        if self.job_active() {
            return;
        }
        self.reload_disks(ctx);
        if self.demo {
            self.toast("Drive list refreshed", false);
        }
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Disks(gen, result) => {
                    if gen != self.disk_gen {
                        continue;
                    }
                    self.disks_loading = false;
                    match result {
                        Ok(disks) => {
                            self.disks = disks;
                            self.disks_error = None;
                            self.after_disks();
                        }
                        Err(e) => {
                            if self.disks_error.as_deref() != Some(e.as_str()) {
                                self.logbook
                                    .push(format!("The drives could not be listed: {e}"));
                            }
                            self.disks.clear();
                            self.disks_error = Some(e);
                            self.pending_disk = None;
                            self.create_disk = None;
                            self.tools.backup_disk = None;
                            self.tools.restore_disk = None;
                            self.tools.reset_disk = None;
                        }
                    }
                }
                Msg::DevicesChanged => {
                    // While a job runs the drive is expected to come and go;
                    // the list is read again when the job has ended.
                    if !self.job_active() {
                        self.reload_disks(ctx);
                    }
                }
                Msg::Inspected(slot, gen, result) => {
                    if gen == self.inspect_gen[slot as usize] {
                        self.finish_inspection(slot, result);
                    }
                }
                Msg::Hashed(gen, result) => self.hash_finished(gen, result),
            }
        }
    }

    fn modal_open(&self) -> bool {
        self.pending.is_some()
            || self.about_open
            || self.running.as_ref().map(|r| r.ask_cancel).unwrap_or(false)
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.shot.is_some() {
            return;
        }
        let (f5, open) = ctx.input(|i| {
            (
                i.key_pressed(Key::F5),
                i.modifiers.command && i.key_pressed(Key::O),
            )
        });
        if f5 {
            self.refresh(ctx);
        }
        if open && self.running.is_none() && !self.modal_open() {
            match self.tab {
                Tab::Checksums => self.choose_hash_file(),
                Tab::Tools => self.choose_image(ctx, Slot::Restore),
                _ => {
                    self.tab = Tab::Create;
                    self.choose_image(ctx, Slot::Create);
                }
            }
        }
    }

    /// Files dropped onto the window select the image (or, on the Checksums
    /// page, the file to hash).
    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        let Some(path) = dropped.into_iter().next() else {
            return;
        };
        if self.running.is_some() || self.modal_open() {
            return;
        }
        match self.tab {
            Tab::Checksums => {
                if self.demo {
                    self.hash
                        .set_file(demo::windows_image().path, Some(demo::windows_image().size));
                } else {
                    let size = std::fs::metadata(&path).ok().map(|m| m.len());
                    self.hash.set_file(path, size);
                }
            }
            Tab::Tools => self.set_image(ctx, Slot::Restore, path),
            _ => {
                self.tab = Tab::Create;
                self.set_image(ctx, Slot::Create, path);
            }
        }
    }

    fn drop_overlay(&self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !hovering || self.running.is_some() || self.modal_open() {
            return;
        }
        let p = pal_ctx(ctx);
        let screen = ctx.screen_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop-overlay"),
        ));
        painter.rect_filled(screen, 0, p.bg.gamma_multiply(0.86));
        let text = if self.tab == Tab::Checksums {
            "Drop the file to compute its checksums"
        } else {
            "Drop the ISO or IMG file to select it"
        };
        painter.text(
            screen.center(),
            Align2::CENTER_CENTER,
            text,
            semibold(22.0),
            p.accent,
        );
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let busy = self.disks_loading || self.image.inspecting() || self.job_active();
        let Some(shot) = &mut self.shot else { return };
        ctx.request_repaint();
        shot.frames += 1;
        if shot.frames == 1 {
            ctx.set_pixels_per_point(1.0);
        }
        if shot.frames == 3 {
            // Now that one point is one pixel, this gives an exact 1280 x 800 image.
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(1280.0, 800.0)));
        }
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let [w, h] = image.size;
            if let Some(parent) = shot.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let result = image::save_buffer(
                &shot.path,
                image.as_raw(),
                w as u32,
                h as u32,
                image::ColorType::Rgba8,
            );
            if let Err(e) = result {
                eprintln!("screenshot failed: {e}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            self.shot = None;
            return;
        }
        if !shot.requested && shot.frames > 40 && (!busy || shot.frames > 1500) {
            shot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
    }

    /// Closing the window while a job runs cancels the job and waits for it
    /// to stop cleanly (for a short while) instead of killing a write.
    fn handle_close(&mut self, ctx: &egui::Context) {
        const PATIENCE: Duration = Duration::from_secs(20);
        let live = self
            .running
            .as_ref()
            .map(|r| r.outcome.is_none() && r.handle.is_some())
            .unwrap_or(false);
        if ctx.input(|i| i.viewport().close_requested()) && live {
            let started = *self.closing.get_or_insert_with(Instant::now);
            if let Some(r) = &mut self.running {
                r.request_cancel();
            }
            if started.elapsed() < PATIENCE {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
        if let Some(started) = self.closing {
            if !live || started.elapsed() >= PATIENCE {
                self.closing = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    // ----------------------------------------------------------------------------------
    // Frame: header and navigation
    // ----------------------------------------------------------------------------------

    fn header(&mut self, ctx: &egui::Context) {
        let p = pal_ctx(ctx);
        egui::TopBottomPanel::top("header")
            .exact_height(64.0)
            .frame(
                Frame::new()
                    .fill(p.rail)
                    .inner_margin(Margin::symmetric(18, 0)),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter().hline(
                    full.x_range().expand(18.0),
                    full.bottom(),
                    Stroke::new(1.0_f32, p.border),
                );
                ui.horizontal_centered(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                    if let Some(logo) = &self.logo {
                        ui.painter().image(
                            logo.id(),
                            rect,
                            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    ui.add_space(2.0);
                    ui.label(RichText::new("BootForge").font(semibold(19.0)));
                    if self.demo {
                        widgets::badge(ui, "DEMO", p.info)
                            .on_hover_text("Invented drives and images. No drive is touched.");
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button(ui, Icon::INFO, "About BootForge").clicked() {
                            self.about_open = true;
                        }
                        if widgets::icon_button(ui, Icon::REFRESH, "Refresh the drive list (F5)")
                            .clicked()
                        {
                            self.refresh(ctx);
                        }
                        ui.add_space(6.0);
                        if self.elevated {
                            widgets::badge(ui, &Icon::SHIELD.with("Administrator"), p.accent)
                                .on_hover_text(
                                    "BootForge runs with administrator rights, which Windows requires for writing to a drive.",
                                );
                        } else {
                            let idle = self.running.is_none();
                            if widgets::ghost_button_enabled(ui, "Restart as administrator", idle)
                                .clicked()
                            {
                                self.restart_elevated(ctx);
                            }
                            widgets::badge(ui, &Icon::WARNING.with("Standard rights"), p.warn)
                                .on_hover_text(
                                    "BootForge runs without administrator rights. You can inspect images and compute checksums; writing to a drive needs administrator rights.",
                                );
                        }
                    });
                });
            });
    }

    fn rail(&mut self, ctx: &egui::Context) {
        let p = pal_ctx(ctx);
        let locked = self.job_active();
        let usable = self.usable_count();
        egui::SidePanel::left("rail")
            .exact_width(208.0)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(p.rail)
                    .inner_margin(Margin::symmetric(10, 14)),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter().vline(
                    full.right() + 10.0,
                    full.y_range().expand(14.0),
                    Stroke::new(1.0_f32, p.border),
                );
                ui.spacing_mut().item_spacing.y = 4.0;
                let items: [(Tab, Icon, &str, Option<usize>); 5] = [
                    (
                        Tab::Create,
                        Icon::USB,
                        "Create",
                        (self.disks_error.is_none() && !self.disks_loading).then_some(usable),
                    ),
                    (Tab::Checksums, Icon::FINGERPRINT, "Checksums", None),
                    (Tab::Tools, Icon::TOOLS, "Drive tools", None),
                    (Tab::Log, Icon::DOCUMENT, "Log", None),
                    (Tab::Settings, Icon::SETTINGS, "Settings", None),
                ];
                for (tab, icon, label, count) in items {
                    if tab == Tab::Settings {
                        ui.add_space(8.0);
                        widgets::divider(ui);
                        ui.add_space(8.0);
                    }
                    let active = self.tab == tab;
                    let (rect, resp) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
                    if active {
                        ui.painter()
                            .rect_filled(rect, CornerRadius::same(9), p.selected);
                        let bar = Rect::from_min_size(
                            rect.left_center() - vec2(0.0, 9.0),
                            vec2(3.0, 18.0),
                        );
                        ui.painter()
                            .rect_filled(bar, CornerRadius::same(2), p.accent);
                    } else if resp.hovered() && !locked {
                        ui.painter()
                            .rect_filled(rect, CornerRadius::same(9), p.hover);
                    }
                    let fg = if locked && !active {
                        p.faint
                    } else if active {
                        p.text
                    } else {
                        p.muted
                    };
                    ui.painter().text(
                        rect.left_center() + vec2(24.0, 0.0),
                        Align2::CENTER_CENTER,
                        icon.glyph(),
                        regular(16.0),
                        if active { p.accent } else { fg },
                    );
                    ui.painter().text(
                        rect.left_center() + vec2(46.0, -1.0),
                        Align2::LEFT_CENTER,
                        label,
                        if active {
                            semibold(14.5)
                        } else {
                            regular(14.5)
                        },
                        fg,
                    );
                    if let Some(n) = count {
                        ui.painter().text(
                            rect.right_center() - vec2(12.0, 1.0),
                            Align2::RIGHT_CENTER,
                            n.to_string(),
                            regular(12.5),
                            p.faint,
                        );
                    }
                    if locked {
                        resp.on_hover_text("Available again when the running job has ended.");
                    } else if resp.clicked() {
                        // Leaving a finished job's result closes it.
                        self.running = None;
                        self.tab = tab;
                    }
                }
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                                .size(12.0)
                                .color(p.faint),
                        );
                    });
                });
            });
    }

    // ----------------------------------------------------------------------------------
    // About and toasts
    // ----------------------------------------------------------------------------------

    fn about_dialog(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let p = pal_ctx(ctx);
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("about"))
            .frame(
                Frame::new()
                    .fill(p.card)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::same(28)),
            )
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.vertical_centered(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(84.0, 84.0), Sense::hover());
                    if let Some(logo) = &self.logo {
                        ui.painter().image(
                            logo.id(),
                            rect,
                            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("BootForge").font(semibold(26.0)));
                    ui.label(
                        RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                            .color(p.muted),
                    );
                    ui.add_space(10.0);
                    ui.label("Turns ISO and disk images into bootable USB drives, carefully.");
                    ui.add_space(10.0);
                    ui.label(RichText::new("Developed by Zheer Khalid").font(semibold(15.0)));
                    ui.add_space(2.0);
                    ui.hyperlink_to(
                        RichText::new("zheer.dev").color(p.accent),
                        "https://zheer.dev",
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Free and open source under the MIT License.")
                            .size(12.0)
                            .color(p.faint),
                    );
                    ui.add_space(14.0);
                    if widgets::primary_button(ui, "Close", true).clicked() {
                        close = true;
                    }
                });
            });
        if close || modal.should_close() {
            self.about_open = false;
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts
            .retain(|t| t.born.elapsed().as_secs_f32() < if t.error { 8.0 } else { 3.5 });
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let p = pal_ctx(ctx);
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, vec2(-18.0, -84.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
                    for t in self.toasts.iter().rev().take(4) {
                        let color = if t.error { p.danger } else { p.accent };
                        Frame::new()
                            .fill(p.card)
                            .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.7)))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(14, 10))
                            .show(ui, |ui| {
                                ui.set_max_width(420.0);
                                ui.horizontal(|ui| {
                                    let icon = if t.error { Icon::ERROR } else { Icon::DONE };
                                    ui.label(RichText::new(icon.glyph()).color(color));
                                    ui.add(egui::Label::new(t.text.as_str()).wrap());
                                });
                            });
                    }
                });
            });
    }
}

/// What demo mode assumes about the (invented) PC.
pub(super) fn demo_env() -> plan::Env {
    plan::Env {
        dism_available: true,
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_messages(ctx);
        self.pump_job(ctx);
        if std::mem::take(&mut self.autostart) && self.demo {
            // Demo only: play a whole simulated job without any clicks.
            if let Some((job, disk)) = self.build_write_job() {
                self.start_job(
                    ctx,
                    PendingJob {
                        job,
                        disk,
                        understood: true,
                        origin: Tab::Create,
                    },
                );
            }
        }
        self.handle_close(ctx);
        self.handle_keys(ctx);
        self.handle_drops(ctx);
        self.handle_screenshot(ctx);

        self.header(ctx);
        self.rail(ctx);

        let p = pal_ctx(ctx);
        let central = Frame::new()
            .fill(p.bg)
            .inner_margin(Margin::symmetric(24, 18));
        if self.running.is_some() {
            egui::CentralPanel::default()
                .frame(central)
                .show(ctx, |ui| self.job_ui(ui));
        } else {
            if self.tab == Tab::Create {
                self.create_actions(ctx);
            }
            egui::CentralPanel::default()
                .frame(if self.tab == Tab::Create {
                    central.inner_margin(Margin {
                        left: 24,
                        right: 24,
                        top: 18,
                        bottom: 0,
                    })
                } else {
                    central
                })
                .show(ctx, |ui| match self.tab {
                    Tab::Create => self.create_ui(ui),
                    Tab::Checksums => self.checksums_ui(ui),
                    Tab::Tools => self.tools_ui(ui),
                    Tab::Log => self.log_ui(ui),
                    Tab::Settings => self.settings_ui(ui),
                });
        }

        self.confirm_dialog(ctx);
        self.cancel_dialog(ctx);
        self.about_dialog(ctx);
        self.drop_overlay(ctx);
        self.toasts_ui(ctx);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // Screenshots must not depend on where the mouse happens to be:
        // no hover highlights, no tooltips.
        if self.shot.is_some() {
            raw_input.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::PointerMoved(_)
                        | egui::Event::MouseMoved(_)
                        | egui::Event::PointerButton { .. }
                        | egui::Event::MouseWheel { .. }
                )
            });
            raw_input.events.push(egui::Event::PointerGone);
            raw_input.dropped_files.clear();
            raw_input.hovered_files.clear();
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.hash.cancel();
        if let Some(r) = &mut self.running {
            if r.outcome.is_none() {
                r.request_cancel();
            }
        }
    }
}
