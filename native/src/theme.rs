//! Kinlink "Midnight" look: colors, fonts, the logo, simple drawn icons and a few widgets.
use egui::{Color32, CornerRadius, FontFamily, FontId, Painter, Pos2, Rect, Response, RichText, Sense, Stroke, Ui, Vec2, pos2, vec2};
use std::sync::Arc;

pub const BG: Color32 = Color32::from_rgb(0x0B, 0x10, 0x20);
pub const SIDEBAR: Color32 = Color32::from_rgb(0x0F, 0x16, 0x29);
pub const CARD: Color32 = Color32::from_rgb(0x12, 0x1A, 0x2E);
pub const FIELD: Color32 = Color32::from_rgb(0x14, 0x1C, 0x33);
pub const HOVER: Color32 = Color32::from_rgb(0x1B, 0x23, 0x40);
pub const LINE: Color32 = Color32::from_rgb(0x22, 0x2C, 0x47);
pub const LINE2: Color32 = Color32::from_rgb(0x2A, 0x35, 0x57);
pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xEC, 0xF8);
pub const SOFT: Color32 = Color32::from_rgb(0xB4, 0xBE, 0xD8);
pub const MUTED: Color32 = Color32::from_rgb(0x8E, 0x9A, 0xB9);
pub const ACCENT: Color32 = Color32::from_rgb(0x7C, 0x6C, 0xFF);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0xA9, 0x9F, 0xFF);
pub const ACCENT_BG: Color32 = Color32::from_rgb(0x17, 0x16, 0x3A);
pub const ACCENT_LINE: Color32 = Color32::from_rgb(0x34, 0x30, 0x7A);
pub const MINT: Color32 = Color32::from_rgb(0x3D, 0xDC, 0x97);
pub const MINT_SOFT: Color32 = Color32::from_rgb(0x9D, 0xE8, 0xC4);
pub const DANGER: Color32 = Color32::from_rgb(0xFF, 0x9D, 0xB5);
pub const DANGER_BG: Color32 = Color32::from_rgb(0x3A, 0x14, 0x26);

/// Font family for headings and names (Manrope ExtraBold).
pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// Manrope for Latin text, plus a system font with Arabic as the fallback.
pub fn fonts(ctx: &egui::Context) {
    let mut f = egui::FontDefinitions::default();
    f.font_data.insert("manrope".into(), Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/manrope-500.ttf"))));
    f.font_data.insert("manrope-bold".into(), Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/manrope-800.ttf"))));
    if let Some(bytes) = crate::rtl::arabic_font() {
        f.font_data.insert("arabic".into(), Arc::new(egui::FontData::from_owned(bytes)));
    }
    let defaults = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let with = |first: &str| {
        let mut v = vec![first.to_string()];
        v.extend(defaults.iter().cloned());
        if f.font_data.contains_key("arabic") {
            v.push("arabic".into());
        }
        v
    };
    let prop = with("manrope");
    let heavy = with("manrope-bold");
    f.families.insert(FontFamily::Proportional, prop);
    f.families.insert(bold(), heavy);
    if f.font_data.contains_key("arabic") {
        f.families.entry(FontFamily::Monospace).or_default().push("arabic".into());
    }
    ctx.set_fonts(f);
}

pub fn apply(ctx: &egui::Context) {
    fonts(ctx);
    ctx.global_style_mut(|s| {
        use egui::TextStyle::*;
        s.text_styles.insert(Heading, FontId::new(24.0, bold()));
        s.text_styles.insert(Body, FontId::proportional(14.0));
        s.text_styles.insert(Button, FontId::proportional(14.0));
        s.text_styles.insert(Small, FontId::proportional(11.5));
        s.text_styles.insert(Monospace, FontId::monospace(13.0));
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(12.0, 7.0);
        s.spacing.interact_size.y = 30.0;
        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.window_stroke = Stroke::new(1.0, ACCENT_LINE);
        v.window_corner_radius = CornerRadius::same(16);
        v.extreme_bg_color = FIELD;
        v.faint_bg_color = CARD;
        v.override_text_color = Some(TEXT);
        v.hyperlink_color = ACCENT_SOFT;
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.slider_trailing_fill = true; // violet track up to the knob
        let w = &mut v.widgets;
        for (st, fill, stroke) in [(&mut w.noninteractive, CARD, LINE), (&mut w.inactive, FIELD, LINE2), (&mut w.hovered, HOVER, ACCENT_LINE), (&mut w.active, ACCENT, ACCENT), (&mut w.open, HOVER, ACCENT_LINE)] {
            st.bg_fill = fill;
            st.weak_bg_fill = fill;
            st.bg_stroke = Stroke::new(1.0, stroke);
            st.corner_radius = CornerRadius::same(10);
            st.expansion = 0.0;
        }
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.fg_stroke = Stroke::new(1.5, TEXT);
        w.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        w.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    });
}

// ---------------------------------------------------------------- logo + icons

/// The Kinlink mark: two linked rings (violet over green at the top crossing).
pub fn logo(p: &Painter, r: Rect, tile: bool) {
    let s = r.width() / 64.0;
    if tile {
        p.rect_filled(r, 16.0 * s, Color32::from_rgb(0x15, 0x1B, 0x36));
    }
    let c = |x: f32, y: f32| pos2(r.min.x + x * s, r.min.y + y * s);
    let w = 4.4 * s;
    p.circle_stroke(c(23.0, 32.0), 12.0 * s, Stroke::new(w, ACCENT));
    p.circle_stroke(c(41.0, 32.0), 12.0 * s, Stroke::new(w, MINT));
    // violet passes over green at the top crossing
    let pts: Vec<Pos2> = (0..=12).map(|i| {
        let a = (-64.0 + 44.0 * i as f32 / 12.0f32).to_radians();
        c(23.0 + 12.0 * a.cos(), 32.0 + 12.0 * a.sin())
    }).collect();
    p.add(egui::Shape::line(pts, Stroke::new(w, ACCENT)));
}

#[derive(Clone, Copy)]
pub enum Icon {
    Friends,
    Monitor,
    Files,
    Settings,
    Copy,
    Plus,
    Play,
    More,
    Mic,
    Upload,
    Update,
    Chat,
    Camera,
    Record,
    Expand,
    Shrink,
    Lock,
    Hide,
    Close,
    Menu,
    Shield,
    Keyboard,
}

/// Draws a small line icon centered in `r`.
pub fn icon(p: &Painter, r: Rect, i: Icon, color: Color32) {
    let s = r.width() / 24.0;
    let c = |x: f32, y: f32| pos2(r.min.x + x * s, r.min.y + y * s);
    let st = Stroke::new(1.8 * s.max(0.8), color);
    let line = |a: (f32, f32), b: (f32, f32)| p.line_segment([c(a.0, a.1), c(b.0, b.1)], st);
    let arc = |cx: f32, cy: f32, rad: f32, from: f32, to: f32| {
        let pts: Vec<Pos2> = (0..=16).map(|k| {
            let a = (from + (to - from) * k as f32 / 16.0).to_radians();
            c(cx + rad * a.cos(), cy + rad * a.sin())
        }).collect();
        p.add(egui::Shape::line(pts, st));
    };
    match i {
        Icon::Friends => {
            p.circle_stroke(c(9.0, 8.0), 3.6 * s, st);
            arc(9.0, 21.0, 7.0, 200.0, 340.0);
            p.circle_stroke(c(17.0, 9.0), 2.8 * s, st);
            arc(17.0, 20.0, 5.0, 230.0, 330.0);
        }
        Icon::Monitor => {
            p.rect_stroke(Rect::from_min_max(c(3.0, 4.0), c(21.0, 16.0)), 2.0 * s, st, egui::StrokeKind::Middle);
            line((12.0, 16.0), (12.0, 20.0));
            line((8.0, 20.0), (16.0, 20.0));
        }
        Icon::Files => {
            line((12.0, 3.0), (12.0, 15.0));
            line((7.0, 10.0), (12.0, 15.0));
            line((17.0, 10.0), (12.0, 15.0));
            line((4.0, 16.0), (4.0, 20.0));
            line((4.0, 20.0), (20.0, 20.0));
            line((20.0, 20.0), (20.0, 16.0));
        }
        Icon::Upload => {
            line((12.0, 15.0), (12.0, 3.0));
            line((7.0, 8.0), (12.0, 3.0));
            line((17.0, 8.0), (12.0, 3.0));
            line((4.0, 16.0), (4.0, 20.0));
            line((4.0, 20.0), (20.0, 20.0));
            line((20.0, 20.0), (20.0, 16.0));
        }
        Icon::Update => {
            line((12.0, 3.0), (12.0, 15.0));
            line((7.0, 10.0), (12.0, 15.0));
            line((17.0, 10.0), (12.0, 15.0));
            line((5.0, 20.0), (19.0, 20.0));
        }
        Icon::Settings => {
            for (y, k) in [(6.0, 15.0), (12.0, 8.0), (18.0, 13.0)] {
                line((3.0, y), (21.0, y));
                p.circle_filled(c(k, y), 2.4 * s, color);
            }
        }
        Icon::Copy => {
            p.rect_stroke(Rect::from_min_max(c(9.0, 9.0), c(21.0, 21.0)), 2.0 * s, st, egui::StrokeKind::Middle);
            line((5.0, 15.0), (3.5, 15.0));
            line((3.0, 14.0), (3.0, 4.0));
            line((4.0, 3.0), (14.0, 3.0));
            line((15.0, 3.5), (15.0, 5.0));
        }
        Icon::Plus => {
            line((12.0, 5.0), (12.0, 19.0));
            line((5.0, 12.0), (19.0, 12.0));
        }
        Icon::Play => {
            p.add(egui::Shape::convex_polygon(vec![c(7.0, 4.0), c(20.0, 12.0), c(7.0, 20.0)], color, Stroke::NONE));
        }
        Icon::More => {
            for y in [5.0, 12.0, 19.0] {
                p.circle_filled(c(12.0, y), 1.8 * s, color);
            }
        }
        Icon::Chat => {
            p.rect_stroke(Rect::from_min_max(c(3.0, 4.0), c(21.0, 16.0)), 3.0 * s, st, egui::StrokeKind::Middle);
            line((7.0, 16.0), (5.0, 21.0));
            line((5.0, 21.0), (11.0, 16.0));
        }
        Icon::Camera => {
            p.rect_stroke(Rect::from_min_max(c(2.0, 7.0), c(22.0, 20.0)), 2.5 * s, st, egui::StrokeKind::Middle);
            line((8.0, 7.0), (9.5, 4.0));
            line((9.5, 4.0), (14.5, 4.0));
            line((14.5, 4.0), (16.0, 7.0));
            p.circle_stroke(c(12.0, 13.5), 3.6 * s, st);
        }
        Icon::Record => {
            p.circle_stroke(c(12.0, 12.0), 8.5 * s, st);
            p.circle_filled(c(12.0, 12.0), 4.0 * s, color);
        }
        Icon::Expand => {
            for (a, b, d) in [((3.0, 9.0), (3.0, 3.0), (9.0, 3.0)), ((15.0, 3.0), (21.0, 3.0), (21.0, 9.0)), ((21.0, 15.0), (21.0, 21.0), (15.0, 21.0)), ((9.0, 21.0), (3.0, 21.0), (3.0, 15.0))] {
                line(a, b);
                line(b, d);
            }
        }
        Icon::Shrink => {
            for (a, b, d) in [((3.0, 9.0), (9.0, 9.0), (9.0, 3.0)), ((15.0, 3.0), (15.0, 9.0), (21.0, 9.0)), ((21.0, 15.0), (15.0, 15.0), (15.0, 21.0)), ((9.0, 21.0), (9.0, 15.0), (3.0, 15.0))] {
                line(a, b);
                line(b, d);
            }
        }
        Icon::Lock => {
            p.rect_stroke(Rect::from_min_max(c(5.0, 11.0), c(19.0, 21.0)), 2.0 * s, st, egui::StrokeKind::Middle);
            arc(12.0, 11.0, 4.5, 180.0, 360.0);
        }
        Icon::Hide => {
            arc(12.0, 20.0, 11.0, 210.0, 330.0);
            arc(12.0, 4.0, 11.0, 30.0, 150.0);
            p.circle_stroke(c(12.0, 12.0), 3.0 * s, st);
            line((4.0, 20.0), (20.0, 4.0));
        }
        Icon::Close => {
            line((6.0, 6.0), (18.0, 18.0));
            line((18.0, 6.0), (6.0, 18.0));
        }
        Icon::Menu => {
            line((4.0, 6.0), (20.0, 6.0));
            line((4.0, 12.0), (20.0, 12.0));
            line((4.0, 18.0), (20.0, 18.0));
        }
        Icon::Shield => {
            line((12.0, 2.5), (20.0, 6.0));
            line((20.0, 6.0), (19.0, 14.0));
            line((19.0, 14.0), (12.0, 21.5));
            line((12.0, 21.5), (5.0, 14.0));
            line((5.0, 14.0), (4.0, 6.0));
            line((4.0, 6.0), (12.0, 2.5));
        }
        Icon::Keyboard => {
            p.rect_stroke(Rect::from_min_max(c(2.0, 6.0), c(22.0, 18.0)), 2.0 * s, st, egui::StrokeKind::Middle);
            for x in [6.0, 10.0, 14.0, 18.0] {
                p.circle_filled(c(x, 10.0), 1.1 * s, color);
            }
            line((7.0, 14.5), (17.0, 14.5));
        }
        Icon::Mic => {
            p.rect_stroke(Rect::from_min_max(c(9.0, 2.0), c(15.0, 14.0)), 3.0 * s, st, egui::StrokeKind::Middle);
            arc(12.0, 10.0, 7.0, 0.0, 180.0);
            line((12.0, 17.0), (12.0, 22.0));
        }
    }
}

// ---------------------------------------------------------------- widgets

/// Rounded card with the Midnight border.
pub fn card() -> egui::Frame {
    egui::Frame::NONE.fill(CARD).stroke(Stroke::new(1.0, LINE)).corner_radius(14.0).inner_margin(egui::Margin::same(14))
}

pub fn section(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(MUTED).strong());
}

pub fn title(ui: &mut Ui, text: &str, sub: Option<&str>) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(text).font(FontId::new(24.0, bold())));
        if let Some(s) = sub {
            ui.label(RichText::new(s).size(13.0).color(MUTED));
        }
    });
}

/// Filled violet button (optionally with an icon).
pub fn primary(ui: &mut Ui, text: &str, icon_kind: Option<Icon>, enabled: bool) -> Response {
    let font = FontId::new(13.5, bold());
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, Color32::WHITE);
    let iw = if icon_kind.is_some() { 20.0 } else { 0.0 };
    let size = vec2(galley.size().x + 32.0 + iw, 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let fill = if !enabled { LINE } else if resp.is_pointer_button_down_on() { Color32::from_rgb(0x6A, 0x5A, 0xF0) } else if resp.hovered() { Color32::from_rgb(0x8E, 0x80, 0xFF) } else { ACCENT };
    let p = ui.painter();
    p.rect_filled(rect, 10.0, fill);
    let mut x = rect.min.x + 16.0;
    let fg = if enabled { Color32::WHITE } else { MUTED };
    if let Some(i) = icon_kind {
        icon(p, Rect::from_center_size(pos2(x + 7.0, rect.center().y), vec2(14.0, 14.0)), i, fg);
        x += iw;
    }
    p.galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Square icon button with a border (copy, edit, ...).
pub fn icon_button(ui: &mut Ui, i: Icon, hint: &str, on: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::click());
    let p = ui.painter();
    p.rect(rect, 9.0, if on || resp.hovered() { HOVER } else { Color32::TRANSPARENT }, Stroke::new(1.0, LINE2), egui::StrokeKind::Inside);
    icon(p, Rect::from_center_size(rect.center(), vec2(16.0, 16.0)), i, SOFT);
    resp.on_hover_text(hint)
}

/// Rounded-square avatar with the first letter and an online dot.
pub fn avatar(ui: &mut Ui, name: &str, online: Option<bool>, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let hues = [(0x2B, 0x24, 0x70, 0xC9, 0xC2, 0xFF), (0x17, 0x3A, 0x33, 0x9D, 0xE8, 0xC4), (0x3A, 0x26, 0x14, 0xFF, 0xC9, 0x8A), (0x14, 0x2E, 0x3A, 0x8A, 0xD8, 0xFF)];
    let h = name.bytes().fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize)) % hues.len();
    let (br, bg, bb, fr, fg, fb) = hues[h];
    let online_known = online.is_some();
    let (bgc, fgc) = if online == Some(false) { (Color32::from_rgb(0x1C, 0x24, 0x40), MUTED) } else { (Color32::from_rgb(br, bg, bb), Color32::from_rgb(fr, fg, fb)) };
    let p = ui.painter();
    p.rect_filled(rect, size * 0.28, bgc);
    let letter = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
    p.text(rect.center(), egui::Align2::CENTER_CENTER, letter, FontId::new(size * 0.42, bold()), fgc);
    if online_known && online == Some(true) {
        let c = pos2(rect.max.x - 2.0, rect.max.y - 2.0);
        p.circle_filled(c, 6.5, CARD);
        p.circle_filled(c, 4.5, MINT);
    }
}

/// iOS-style switch with a label; returns true when flipped.
pub fn switch(ui: &mut Ui, on: &mut bool, label: &str) -> bool {
    let font = FontId::proportional(13.5);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, TEXT);
    let (rect, resp) = ui.allocate_exact_size(vec2(44.0 + galley.size().x, 24.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let track = Rect::from_min_size(pos2(rect.min.x, rect.center().y - 10.0), vec2(34.0, 20.0));
    let p = ui.painter();
    p.rect_filled(track, 10.0, if *on { ACCENT } else { LINE2 });
    p.circle_filled(pos2(track.min.x + 10.0 + t * 14.0, track.center().y), 8.0, Color32::WHITE);
    p.galley(pos2(track.max.x + 10.0, rect.center().y - galley.size().y / 2.0), galley, TEXT);
    resp.clicked()
}

/// Sidebar navigation item.
pub fn nav(ui: &mut Ui, i: Icon, text: &str, active: bool, badge: Option<usize>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    let p = ui.painter();
    if active || resp.hovered() {
        p.rect_filled(rect, 10.0, if active { HOVER } else { Color32::from_rgb(0x15, 0x1D, 0x35) });
    }
    let fg = if active { Color32::WHITE } else { SOFT };
    icon(p, Rect::from_center_size(pos2(rect.min.x + 21.0, rect.center().y), vec2(18.0, 18.0)), i, if active { ACCENT_SOFT } else { fg });
    p.text(pos2(rect.min.x + 42.0, rect.center().y), egui::Align2::LEFT_CENTER, text, FontId::new(14.0, if active { bold() } else { FontFamily::Proportional }), fg);
    if let Some(n) = badge.filter(|n| *n > 0) {
        let c = pos2(rect.max.x - 18.0, rect.center().y);
        p.rect_filled(Rect::from_center_size(c, vec2(22.0, 18.0)), 9.0, ACCENT);
        p.text(c, egui::Align2::CENTER_CENTER, n.to_string(), FontId::new(11.0, bold()), Color32::WHITE);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Big square action tile (session menu): icon over a label; `on` = highlight color.
pub fn tile(ui: &mut Ui, i: Icon, label: &str, on: Option<Color32>, w: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 58.0), Sense::click());
    let p = ui.painter();
    let (fill, stroke, fg) = match on {
        Some(c) => (c.gamma_multiply(0.18), c.gamma_multiply(0.55), c),
        None => (if resp.hovered() { HOVER } else { FIELD }, LINE2, TEXT),
    };
    p.rect(rect, 11.0, fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    icon(p, Rect::from_center_size(pos2(rect.center().x, rect.min.y + 21.0), vec2(18.0, 18.0)), i, fg);
    p.text(pos2(rect.center().x, rect.max.y - 13.0), egui::Align2::CENTER_CENTER, label, FontId::new(11.0, bold()), fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Full-width red action (Disconnect).
pub fn danger_wide(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    p.rect_filled(rect, 10.0, if resp.hovered() && enabled { Color32::from_rgb(0x55, 0x18, 0x34) } else { DANGER_BG });
    p.text(rect.center(), egui::Align2::CENTER_CENTER, text, FontId::new(13.5, bold()), if enabled { DANGER } else { DANGER.gamma_multiply(0.5) });
    resp
}
