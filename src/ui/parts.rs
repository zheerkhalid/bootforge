//! BootForge-specific building blocks: numbered card titles, notes, progress
//! bars, the drop zone border and the drive list shared by all pages.

use super::logic;
use super::theme::{pal, regular, semibold, Icon, Palette};
use super::widgets;
use crate::model::{Bus, Disk, DiskIdentity};
use crate::plan::Tone;
use crate::safety::Listed;
use crate::util::{human_size, nominal_size};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, Frame, Layout, Margin, Rect, Response,
    RichText, Sense, Stroke, StrokeKind, Ui, Vec2,
};

/// The standard page header: title, subtitle and right-aligned actions.
pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str, actions: impl FnOnce(&mut Ui)) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).font(semibold(24.0)));
            ui.label(RichText::new(subtitle).color(p.muted));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), actions);
    });
    ui.add_space(6.0);
}

/// The frame every dialog uses.
pub fn modal_frame(p: &Palette) -> Frame {
    Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(22))
}

/// A right-aligned row of buttons with a fixed height (a plain right-to-left
/// layout would stretch over the whole remaining height of a dialog).
pub fn button_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 36.0),
        Layout::right_to_left(Align::Center),
        add,
    );
}

/// "(1) Image": the title of a numbered step card.
pub fn numbered_title(ui: &mut Ui, number: u32, title: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 11.0, p.accent.gamma_multiply(0.18));
        ui.painter().text(
            rect.center() - vec2(0.0, 1.0),
            Align2::CENTER_CENTER,
            number.to_string(),
            semibold(12.5),
            p.accent,
        );
        ui.label(RichText::new(title).font(semibold(15.0)));
    });
}

/// A tinted note with an icon (warnings, problems, hints).
pub fn note(ui: &mut Ui, icon: Icon, color: Color32, text: &str) {
    let p = pal(ui);
    Frame::new()
        .fill(color.gamma_multiply(if p.dark { 0.10 } else { 0.12 }))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if !icon.glyph().is_empty() {
                    ui.label(RichText::new(icon.glyph()).size(14.0).color(color));
                }
                ui.add(egui::Label::new(RichText::new(text).size(13.0).color(p.text)).wrap());
            });
        });
}

/// Small muted explanation text, wrapped.
pub fn hint(ui: &mut Ui, text: &str) {
    let p = pal(ui);
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(p.muted)).wrap());
}

/// Accent-coloured clickable text.
pub fn link(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(p.accent)).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A caption above a control.
pub fn field(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    let p = pal(ui);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(RichText::new(label).size(12.5).color(p.muted));
        add(ui);
    });
}

/// A rounded progress bar over the full width. `None` animates an
/// indeterminate bar.
pub fn progress_bar(ui: &mut Ui, fraction: Option<f32>, height: f32, color: Color32) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let radius = CornerRadius::same((height / 2.0).min(8.0) as u8);
    ui.painter().rect_filled(rect, radius, p.hover);
    match fraction {
        Some(f) => {
            let f = f.clamp(0.0, 1.0);
            if f > 0.0 {
                let w = (rect.width() * f).max(height.min(rect.width()));
                let fill = Rect::from_min_size(rect.min, vec2(w, height));
                ui.painter().rect_filled(fill, radius, color);
            }
        }
        None => {
            let t = ui.input(|i| i.time);
            let phase = ((t * 0.9) % 1.0) as f32;
            let seg = rect.width() * 0.28;
            let x = rect.left() - seg + (rect.width() + seg) * phase;
            let fill = Rect::from_min_max(
                pos2(x.max(rect.left()), rect.top()),
                pos2((x + seg).min(rect.right()), rect.bottom()),
            );
            if fill.width() > 0.0 {
                ui.painter().rect_filled(fill, radius, color);
            }
            ui.ctx().request_repaint();
        }
    }
}

/// A dashed rounded border (the drop zone).
pub fn dashed_border(ui: &Ui, rect: Rect, color: Color32) {
    let r = 12.0_f32;
    let mut pts = Vec::new();
    let corners = [
        (pos2(rect.right() - r, rect.top() + r), -90.0_f32),
        (pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (pos2(rect.left() + r, rect.top() + r), 180.0),
    ];
    for (c, start) in corners {
        for i in 0..=6 {
            let a = (start + 15.0 * i as f32).to_radians();
            pts.push(pos2(c.x + r * a.cos(), c.y + r * a.sin()));
        }
    }
    if let Some(first) = pts.first().copied() {
        pts.push(first);
    }
    ui.painter().extend(egui::Shape::dashed_line(
        &pts,
        Stroke::new(1.5_f32, color),
        7.0,
        5.0,
    ));
}

pub fn tone_color(p: &Palette, tone: Tone) -> Color32 {
    match tone {
        Tone::Good => p.accent,
        Tone::Info => p.info,
        Tone::Warn => p.warn,
        Tone::Neutral => p.muted,
    }
}

/// A round status icon (result screens, empty states inside cards).
pub fn status_icon(ui: &mut Ui, icon: Icon, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size / 2.0, color.gamma_multiply(0.16));
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon.glyph(),
        regular(size * 0.42),
        color,
    );
}

// --------------------------------------------------------------------------------------
// Drive list
// --------------------------------------------------------------------------------------

pub fn disk_icon(disk: &Disk) -> Icon {
    match disk.bus {
        Bus::Sd => Icon::SD_CARD,
        Bus::Usb if disk.removable => Icon::USB,
        _ => Icon::DRIVE,
    }
}

/// What the list shows.
pub struct DriveList<'a> {
    pub loading: bool,
    pub error: Option<&'a str>,
    pub listed: &'a [Listed],
    pub hidden: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DriveAction {
    None,
    OpenSettings,
    Refresh,
}

fn small_state(ui: &mut Ui, icon: Icon, color: Color32, title: &str, text: &str) {
    let p = pal(ui);
    ui.vertical_centered(|ui| {
        ui.add_space(10.0);
        status_icon(ui, icon, color, 48.0);
        ui.label(RichText::new(title).font(semibold(15.0)));
        ui.scope(|ui| {
            ui.set_max_width(380.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(egui::Label::new(RichText::new(text).size(12.5).color(p.muted)).wrap());
        });
        ui.add_space(4.0);
    });
}

/// The radio-style list of drives with its loading, error and empty states.
pub fn drive_list(
    ui: &mut Ui,
    list: &DriveList<'_>,
    selection: &mut Option<DiskIdentity>,
) -> DriveAction {
    let p = pal(ui);
    let mut action = DriveAction::None;
    if let Some(error) = list.error {
        small_state(
            ui,
            Icon::ERROR,
            p.danger,
            "The drives could not be listed",
            error,
        );
        ui.vertical_centered(|ui| {
            if widgets::ghost_button(ui, &Icon::REFRESH.with("Try again")).clicked() {
                action = DriveAction::Refresh;
            }
        });
        return action;
    }
    if list.loading && list.listed.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(18.0);
            ui.add(egui::Spinner::new().size(26.0).color(p.accent));
            ui.label(RichText::new("Looking for drives…").color(p.muted));
            ui.add_space(14.0);
        });
        return action;
    }
    if list.listed.is_empty() {
        small_state(
            ui,
            Icon::USB,
            p.muted,
            "Plug in a USB drive",
            "USB sticks and memory cards appear here as soon as Windows sees them. Press F5 to look again.",
        );
    }
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        for l in list.listed {
            let id = l.disk.identity();
            let selected = selection.as_ref() == Some(&id) && l.blocked.is_none();
            if drive_row(ui, l, selected).clicked() && l.blocked.is_none() {
                *selection = Some(id);
            }
        }
    });
    if list.hidden > 0 {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            let text = logic::hidden_text(list.hidden, ui.available_width() < 500.0);
            ui.label(RichText::new(text).size(12.5).color(p.muted));
            if link(ui, "Settings").clicked() {
                action = DriveAction::OpenSettings;
            }
        });
    }
    action
}

fn drive_row(ui: &mut Ui, l: &Listed, selected: bool) -> Response {
    let p = pal(ui);
    let d = &l.disk;
    let blocked = l.blocked.is_some();
    let (rect, resp) = ui.allocate_exact_size(
        vec2(ui.available_width(), 50.0),
        if blocked {
            Sense::hover()
        } else {
            Sense::click()
        },
    );
    let radius = CornerRadius::same(10);
    let (fill, stroke) = if selected {
        (
            p.selected,
            Stroke::new(1.0_f32, p.accent.gamma_multiply(0.75)),
        )
    } else if resp.hovered() && !blocked {
        (p.hover, Stroke::new(1.0_f32, p.border))
    } else {
        (Color32::TRANSPARENT, Stroke::new(1.0_f32, p.border))
    };
    ui.painter().rect_filled(rect, radius, fill);
    ui.painter()
        .rect_stroke(rect, radius, stroke, StrokeKind::Inside);

    // Radio mark.
    let rc = pos2(rect.left() + 20.0, rect.center().y);
    if selected {
        ui.painter().circle_filled(rc, 8.0, p.accent);
        ui.painter().circle_filled(rc, 3.2, p.on_accent);
    } else {
        ui.painter().circle_stroke(
            rc,
            7.2,
            Stroke::new(1.5_f32, if blocked { p.border } else { p.faint }),
        );
    }

    // Icon tile.
    let tile = Rect::from_center_size(pos2(rect.left() + 54.0, rect.center().y), Vec2::splat(32.0));
    ui.painter().rect_filled(
        tile,
        CornerRadius::same(8),
        if selected {
            p.accent.gamma_multiply(0.18)
        } else {
            p.hover
        },
    );
    let icon_color = if blocked {
        p.faint
    } else if selected {
        p.accent
    } else {
        p.muted
    };
    ui.painter().text(
        tile.center(),
        Align2::CENTER_CENTER,
        disk_icon(d).glyph(),
        regular(16.0),
        icon_color,
    );

    // Badges on the right.
    let mut right = rect.right() - 12.0;
    let scheme = logic::scheme_text(d.scheme);
    for (text, color) in [
        (scheme, p.muted),
        (d.bus.label(), if blocked { p.muted } else { p.info }),
    ] {
        let w = ui
            .painter()
            .layout_no_wrap(text.to_string(), semibold(11.5), color)
            .size()
            .x
            + 14.0;
        let r = Rect::from_min_size(pos2(right - w, rect.top() + 8.0), vec2(w, 20.0));
        widgets::paint_badge(ui, r, text, color);
        right -= w + 6.0;
    }

    // Texts.
    let left = rect.left() + 80.0;
    let name_color = if blocked { p.faint } else { p.text };
    let line1 = Rect::from_min_max(
        pos2(left, rect.top() + 7.0),
        pos2(right - 6.0, rect.top() + 29.0),
    );
    let nominal = nominal_size(d.size);
    let nominal_w = ui
        .painter()
        .layout_no_wrap(nominal.clone(), regular(13.5), p.muted)
        .size()
        .x;
    let name_rect = Rect::from_min_max(
        line1.min,
        pos2(line1.right() - nominal_w - 10.0, line1.bottom()),
    );
    let name = widgets::elided(ui, &d.name(), semibold(14.5), name_color, name_rect.width());
    let name_w = name.size().x;
    ui.painter().galley(
        pos2(left, line1.center().y - name.size().y / 2.0),
        name,
        name_color,
    );
    if d.size > 0 {
        ui.painter().text(
            pos2(left + name_w + 10.0, line1.center().y),
            Align2::LEFT_CENTER,
            nominal,
            regular(13.5),
            if blocked { p.faint } else { p.muted },
        );
    }
    let line2 = Rect::from_min_max(
        pos2(left, rect.top() + 29.0),
        pos2(rect.right() - 12.0, rect.bottom() - 7.0),
    );
    match &l.blocked {
        Some(reason) => {
            let text = format!("Cannot be used: {reason}.");
            if widgets::text_in(ui, line2, &text, regular(12.5), p.warn) {
                resp.clone().on_hover_text(text);
            }
        }
        None => {
            let mut text = String::new();
            if d.size > 0 {
                text.push_str(&human_size(d.size));
                text.push_str("  ·  ");
            }
            text.push_str(&format!("Disk {}  ·  ", d.number));
            text.push_str(&logic::partitions_line(d));
            if widgets::text_in(ui, line2, &text, regular(12.5), p.muted) {
                resp.clone().on_hover_text(text);
            }
        }
    }
    resp
}
