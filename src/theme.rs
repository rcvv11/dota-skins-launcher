//! Visual theme: palette, fonts, egui style and the launcher's own widgets.
//!
//! Dark and quiet: near-black surfaces a few steps apart, light text and one
//! accent, the icon's neon red (its violet joins in on the launch button). The
//! colour on a page comes from the items' rarities.

use std::collections::HashSet;
use std::sync::OnceLock;

use eframe::egui::{
    self, Color32, FontFamily, FontId, Mesh, Pos2, Rect, Response, RichText, Rounding, Sense,
    Shape, Stroke, TextStyle, Ui, Vec2,
};

// ---------------------------------------------------------------- palette

/// Window background: the sidebar and the status line sit on it.
pub const BG_DEEP: Color32 = Color32::from_rgb(0x0b, 0x0c, 0x0e);
/// The content panel.
pub const BG: Color32 = Color32::from_rgb(0x11, 0x12, 0x15);
/// Cards, tiles, buttons.
pub const SURFACE: Color32 = Color32::from_rgb(0x17, 0x18, 0x1c);
pub const SURFACE_HI: Color32 = Color32::from_rgb(0x1f, 0x20, 0x25);
pub const INPUT: Color32 = Color32::from_rgb(0x14, 0x15, 0x19);
pub const BORDER: Color32 = Color32::from_rgb(0x24, 0x26, 0x2c);
pub const BORDER_HI: Color32 = Color32::from_rgb(0x34, 0x37, 0x3f);
pub const TEXT: Color32 = Color32::from_rgb(0xed, 0xee, 0xf1);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xa1, 0xa4, 0xae);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x67, 0x6b, 0x76);
pub const ACCENT: Color32 = Color32::from_rgb(0xf0, 0x46, 0x5a);
pub const ACCENT_HI: Color32 = Color32::from_rgb(0xff, 0x64, 0x76);
pub const ACCENT_LO: Color32 = Color32::from_rgb(0xb9, 0x30, 0x44);
/// The icon's violet: the launch button runs from it into the accent.
pub const VIOLET: Color32 = Color32::from_rgb(0x7b, 0x5c, 0xff);
pub const EQUIPPED: Color32 = Color32::from_rgb(0x3d, 0xd6, 0x8c);
pub const DANGER: Color32 = Color32::from_rgb(0xf2, 0x55, 0x5a);
pub const FAVORITE: Color32 = Color32::from_rgb(0xff, 0xc5, 0x3d);
pub const WARN: Color32 = Color32::from_rgb(0xe8, 0xb3, 0x41);

pub const R_SM: f32 = 6.0;
pub const R_MD: f32 = 8.0;
pub const R_LG: f32 = 12.0;

/// Height of buttons, inputs and dropdowns in a row.
pub const CONTROL_H: f32 = 34.0;

// ---------------------------------------------------------------- fonts

/// Segoe UI Semibold, else Bold: the face of headings and labels.
pub const SEMIBOLD_FILES: [&str; 2] = ["seguisb.ttf", "segoeuib.ttf"];

/// Icon fonts: Segoe Fluent Icons (Windows 11), Segoe MDL2 Assets (Windows 10).
const ICON_FONTS: [(&str, &str); 2] = [("fluent", "SegoeIcons.ttf"), ("mdl2", "segmdl2.ttf")];

/// Custom family holding the semibold face.
pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn bold_font(size: f32) -> FontId {
    FontId::new(size, bold())
}

/// egui's built-in fonts only (no Segoe), so emoji glyphs render correctly.
pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}

fn fluent() -> FontFamily {
    FontFamily::Name("fluent".into())
}

/// The icon glyphs the icon fonts on this system have.
static GLYPHS: OnceLock<HashSet<char>> = OnceLock::new();

// ---------------------------------------------------------------- setup

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    install_style(ctx);
}

fn install_fonts(ctx: &egui::Context) {
    use ab_glyph::Font as _;
    let mut fonts = egui::FontDefinitions::default();
    let read = |name: &str| std::fs::read(format!(r"C:\Windows\Fonts\{name}")).ok();

    let base: Vec<String> = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    fonts
        .families
        .insert(FontFamily::Name("icons".into()), base.clone());

    let mut regular = base.clone();
    if let Some(bytes) = read("segoeui.ttf") {
        fonts
            .font_data
            .insert("segoe".into(), egui::FontData::from_owned(bytes));
        regular.insert(0, "segoe".into());
    }

    let mut bold_list = base.clone();
    if let Some(bytes) = SEMIBOLD_FILES.iter().find_map(|n| read(n)) {
        fonts
            .font_data
            .insert("segoe-bold".into(), egui::FontData::from_owned(bytes));
        bold_list.insert(0, "segoe-bold".into());
    } else if regular.first().map(String::as_str) == Some("segoe") {
        bold_list.insert(0, "segoe".into());
    }

    let mut icon_list = Vec::new();
    let mut present = HashSet::new();
    for (key, file) in ICON_FONTS {
        let Some(bytes) = read(file) else { continue };
        if let Ok(face) = ab_glyph::FontRef::try_from_slice(&bytes) {
            for icon in Icon::ALL {
                let c = icon.glyph().0;
                if face.glyph_id(c).0 != 0 {
                    present.insert(c);
                }
            }
        }
        fonts
            .font_data
            .insert(key.into(), egui::FontData::from_owned(bytes));
        icon_list.push(key.to_string());
    }
    let _ = GLYPHS.set(present);
    icon_list.extend(base);

    fonts.families.insert(FontFamily::Proportional, regular);
    fonts.families.insert(bold(), bold_list);
    fonts.families.insert(fluent(), icon_list);
    ctx.set_fonts(fonts);
}

fn install_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG_DEEP;
    v.window_fill = SURFACE;
    v.extreme_bg_color = INPUT;
    v.faint_bg_color = SURFACE;
    v.code_bg_color = INPUT;
    v.window_rounding = Rounding::same(R_LG);
    v.menu_rounding = Rounding::same(R_MD + 2.0);
    v.window_stroke = Stroke::new(1.0_f32, BORDER_HI);
    v.hyperlink_color = ACCENT_HI;
    v.selection.bg_fill = Color32::from_rgb(0x2c, 0x2e, 0x37);
    v.selection.stroke = Stroke::new(1.0_f32, TEXT);
    v.text_cursor.stroke = Stroke::new(1.5_f32, TEXT);
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);
    v.window_shadow = egui::Shadow {
        offset: egui::vec2(0.0, 14.0),
        blur: 36.0,
        spread: 0.0,
        color: Color32::from_black_alpha(170),
    };
    v.popup_shadow = egui::Shadow {
        offset: egui::vec2(0.0, 8.0),
        blur: 24.0,
        spread: 0.0,
        color: Color32::from_black_alpha(150),
    };

    let rounding = Rounding::same(R_MD);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);
    w.noninteractive.rounding = rounding;

    w.inactive.bg_fill = SURFACE_HI;
    w.inactive.weak_bg_fill = SURFACE;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.rounding = rounding;

    w.hovered.bg_fill = Color32::from_rgb(0x28, 0x2a, 0x30);
    w.hovered.weak_bg_fill = SURFACE_HI;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, BORDER_HI);
    w.hovered.fg_stroke = Stroke::new(1.5_f32, TEXT);
    w.hovered.rounding = rounding;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = Color32::from_rgb(0x2d, 0x30, 0x37);
    w.active.weak_bg_fill = Color32::from_rgb(0x25, 0x27, 0x2d);
    w.active.bg_stroke = Stroke::new(1.0_f32, BORDER_HI);
    w.active.fg_stroke = Stroke::new(1.5_f32, TEXT);
    w.active.rounding = rounding;
    w.active.expansion = 0.0;

    w.open = w.hovered;

    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 7.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.combo_height = 420.0;
        s.spacing.menu_margin = egui::Margin::same(6.0);
        s.spacing.window_margin = egui::Margin::same(14.0);
        s.spacing.icon_spacing = 6.0;
        s.text_styles = [
            (TextStyle::Small, FontId::proportional(11.5)),
            (TextStyle::Body, FontId::proportional(13.5)),
            (TextStyle::Button, FontId::proportional(13.5)),
            (TextStyle::Heading, bold_font(20.0)),
            (TextStyle::Monospace, FontId::monospace(12.0)),
        ]
        .into();
    });
}

// ---------------------------------------------------------------- painting helpers

/// Vertical gradient over a rectangle (square corners).
pub fn vgradient(rect: Rect, top: Color32, bottom: Color32) -> Shape {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    Shape::mesh(mesh)
}

/// A small texture holding a gradient (horizontal, or vertical when `vertical`),
/// made once per key. Painted as an image it keeps rounded corners smooth, which a
/// coloured mesh can't.
fn gradient_texture(ctx: &egui::Context, key: &str, stops: &[Color32], vertical: bool) -> egui::TextureHandle {
    let id = egui::Id::new(("gradient", key));
    if let Some(tex) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return tex;
    }
    const N: usize = 128;
    let pixels: Vec<Color32> = (0..N)
        .map(|i| {
            let t = i as f32 / (N - 1) as f32 * (stops.len() - 1) as f32;
            let k = (t.floor() as usize).min(stops.len().saturating_sub(2));
            lerp_color(stops[k], stops[(k + 1).min(stops.len() - 1)], t - k as f32)
        })
        .collect();
    let size = if vertical { [1, N] } else { [N, 1] };
    let image = egui::ColorImage { size, pixels };
    let tex = ctx.load_texture(format!("gradient-{key}"), image, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}

/// Paints a vertical fade, clear at the top and `color` at the bottom, with the
/// given corners rounded.
pub fn fade_up(ui: &Ui, rect: Rect, rounding: Rounding, color: Color32) {
    let tex = gradient_texture(ui.ctx(), "fade", &[Color32::TRANSPARENT, Color32::WHITE], true);
    egui::Image::new(egui::load::SizedTexture::new(tex.id(), rect.size()))
        .rounding(rounding)
        .tint(color)
        .paint_at(ui, rect);
}

/// Lays out single-line text truncated with an ellipsis to `max_w`.
pub fn galley(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w);
    ui.fonts(|f| f.layout_job(job))
}

/// Blend of two colours (premultiplied, so it works for translucent ones too).
pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

/// Section caption: small and faint.
pub fn caption(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(bold_font(12.0)).color(TEXT_FAINT));
}

/// A status dot with a soft halo.
pub fn dot(p: &egui::Painter, center: Pos2, color: Color32) {
    p.circle_filled(center, 7.0, color.gamma_multiply(0.16));
    p.circle_filled(center, 3.5, color);
}

// ---------------------------------------------------------------- icons

/// Icons: Segoe Fluent Icons glyphs, with a plain character where the system's
/// icon fonts don't have one.
#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Heroes,
    Catalog,
    Settings,
    Search,
    Play,
    Reload,
    Save,
    Folder,
    Download,
    Add,
    Delete,
    Star,
    StarFill,
    Check,
    Back,
    Close,
    ChevronDown,
    Cube,
    Warning,
    Revert,
    Palette,
}

impl Icon {
    const ALL: [Icon; 21] = [
        Icon::Heroes,
        Icon::Catalog,
        Icon::Settings,
        Icon::Search,
        Icon::Play,
        Icon::Reload,
        Icon::Save,
        Icon::Folder,
        Icon::Download,
        Icon::Add,
        Icon::Delete,
        Icon::Star,
        Icon::StarFill,
        Icon::Check,
        Icon::Back,
        Icon::Close,
        Icon::ChevronDown,
        Icon::Cube,
        Icon::Warning,
        Icon::Revert,
        Icon::Palette,
    ];

    /// The icon font's glyph, and the stand-in from egui's own fonts.
    fn glyph(self) -> (char, &'static str) {
        match self {
            Icon::Heroes => ('\u{E77B}', "👤"),
            Icon::Catalog => ('\u{E8A9}', "▦"),
            Icon::Settings => ('\u{E713}', "⚙"),
            Icon::Search => ('\u{E721}', "🔍"),
            Icon::Play => ('\u{E768}', "▶"),
            Icon::Reload => ('\u{E72C}', "⟳"),
            Icon::Save => ('\u{E74E}', "💾"),
            Icon::Folder => ('\u{E8B7}', "📁"),
            Icon::Download => ('\u{E896}', "⬇"),
            Icon::Add => ('\u{E710}', "+"),
            Icon::Delete => ('\u{E74D}', "🗑"),
            Icon::Star => ('\u{E734}', "☆"),
            Icon::StarFill => ('\u{E735}', "★"),
            Icon::Check => ('\u{E73E}', "✔"),
            Icon::Back => ('\u{E72B}', "←"),
            Icon::Close => ('\u{E711}', "✕"),
            Icon::ChevronDown => ('\u{E70D}', "⏷"),
            Icon::Cube => ('\u{F158}', "◈"),
            Icon::Warning => ('\u{E7BA}', "⚠"),
            Icon::Revert => ('\u{E7A7}', "↺"),
            Icon::Palette => ('\u{E790}', "◆"),
        }
    }
}

/// Paints an icon centred on `center`, `size` points tall.
pub fn paint_icon(p: &egui::Painter, center: Pos2, icon: Icon, size: f32, color: Color32) {
    let (glyph, stand_in) = icon.glyph();
    if GLYPHS.get().is_some_and(|g| g.contains(&glyph)) {
        p.text(center, egui::Align2::CENTER_CENTER, glyph, FontId::new(size, fluent()), color);
    } else {
        p.text(center, egui::Align2::CENTER_CENTER, stand_in, icon_font(size), color);
    }
}

// ---------------------------------------------------------------- buttons

/// Look of a [`button`].
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// Filled with the accent.
    Primary,
    /// A surface with an outline.
    Secondary,
    /// Text only until hovered.
    Ghost,
    /// Red outline: takes something away.
    Danger,
}

/// A button with an optional icon. `size.x` 0: as wide as its text;
/// `size.y` 0: [`CONTROL_H`].
pub fn button(ui: &mut Ui, icon: Option<Icon>, label: &str, size: Vec2, kind: Kind) -> Response {
    let font = bold_font(13.0);
    let measured = galley(ui, label, font.clone(), TEXT, f32::INFINITY);
    let icon_w = match (icon, label.is_empty()) {
        (None, _) => 0.0,
        (Some(_), true) => 16.0,
        (Some(_), false) => 22.0,
    };
    let h = if size.y > 0.0 { size.y } else { CONTROL_H };
    let w = if size.x > 0.0 { size.x } else { measured.size().x + icon_w + 28.0 };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let press = resp.is_pointer_button_down_on();
        let (fill, stroke, text) = match kind {
            Kind::Primary => {
                let fill = if press { ACCENT_LO } else { lerp_color(ACCENT, ACCENT_HI, hover) };
                (fill, Color32::TRANSPARENT, Color32::WHITE)
            }
            Kind::Secondary => (
                lerp_color(SURFACE, SURFACE_HI, hover),
                lerp_color(BORDER, BORDER_HI, hover),
                TEXT,
            ),
            Kind::Ghost => (
                SURFACE_HI.gamma_multiply(hover),
                Color32::TRANSPARENT,
                lerp_color(TEXT_DIM, TEXT, hover),
            ),
            Kind::Danger => (
                DANGER.gamma_multiply(0.05 + 0.08 * hover),
                DANGER.gamma_multiply(0.35 + 0.3 * hover),
                lerp_color(DANGER, ACCENT_HI, hover),
            ),
        };
        let p = ui.painter();
        p.rect(rect, Rounding::same(R_MD), fill, Stroke::new(1.0_f32, stroke));
        let g = galley(ui, label, font, text, (rect.width() - icon_w - 16.0).max(0.0));
        let total = icon_w + g.size().x;
        let x0 = rect.center().x - total / 2.0;
        if let Some(icon) = icon {
            paint_icon(p, Pos2::new(x0 + 8.0, rect.center().y), icon, 14.0, text);
        }
        p.galley(Pos2::new(x0 + icon_w, rect.center().y - g.size().y / 2.0), g, text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The launch button: the icon's violet running into the accent.
pub fn cta_button(ui: &mut Ui, icon: Option<Icon>, label: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.12);
        let press = resp.is_pointer_button_down_on();
        let r = Rounding::same(R_MD + 2.0);
        let p = ui.painter();
        p.add(
            egui::Shadow {
                offset: egui::vec2(0.0, 8.0),
                blur: 26.0,
                spread: -6.0,
                color: ACCENT.gamma_multiply(0.20 + 0.25 * hover),
            }
            .as_shape(rect, r),
        );
        let tex = gradient_texture(ui.ctx(), "cta", &[VIOLET, ACCENT], false);
        let tint = if press {
            Color32::from_gray(200)
        } else {
            lerp_color(Color32::from_gray(228), Color32::WHITE, hover)
        };
        egui::Image::new(egui::load::SizedTexture::new(tex.id(), rect.size()))
            .rounding(r)
            .tint(tint)
            .paint_at(ui, rect);
        p.rect_stroke(rect, r, Stroke::new(1.0_f32, Color32::from_white_alpha(22)));

        let g = galley(ui, label, bold_font(14.5), Color32::WHITE, rect.width() - 48.0);
        let icon_w = if icon.is_some() { 24.0 } else { 0.0 };
        let x0 = rect.center().x - (icon_w + g.size().x) / 2.0;
        if let Some(icon) = icon {
            paint_icon(p, Pos2::new(x0 + 8.0, rect.center().y), icon, 14.0, Color32::WHITE);
        }
        p.galley(Pos2::new(x0 + icon_w, rect.center().y - g.size().y / 2.0), g, Color32::WHITE);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Square, frameless icon button.
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    icon_button_ex(ui, icon, 32.0, TEXT_DIM, tooltip)
}

/// Icon button of a given side and resting colour.
pub fn icon_button_ex(ui: &mut Ui, icon: Icon, side: f32, color: Color32, tooltip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(side, side), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let p = ui.painter();
        p.rect_filled(rect, Rounding::same(R_MD), SURFACE_HI.gamma_multiply(hover));
        let size = (side * 0.45).round().clamp(11.0, 16.0);
        paint_icon(p, rect.center(), icon, size, lerp_color(color, TEXT, hover));
    }
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if tooltip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tooltip)
    }
}

// ---------------------------------------------------------------- pills

/// Filter pill. A `dot` colour (a rarity) goes before the label.
pub fn chip(ui: &mut Ui, label: &str, active: bool, dot: Option<Color32>) -> Response {
    let font = FontId::proportional(12.5);
    let measured = galley(ui, label, font.clone(), TEXT, f32::INFINITY);
    let lead = if dot.is_some() { 13.0 } else { 0.0 };
    let size = egui::vec2(measured.size().x + lead + 24.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let act = ui.ctx().animate_bool_with_time(resp.id.with("a"), active, 0.12);
        let fill = lerp_color(SURFACE.gamma_multiply(hover), SURFACE_HI, act);
        let stroke = lerp_color(lerp_color(BORDER, BORDER_HI, hover), BORDER_HI, act);
        let text = lerp_color(lerp_color(TEXT_DIM, TEXT, hover), TEXT, act);
        let p = ui.painter();
        p.rect(rect, Rounding::same(rect.height() / 2.0), fill, Stroke::new(1.0_f32, stroke));
        let mut x = rect.min.x + 12.0;
        if let Some(c) = dot {
            p.circle_filled(Pos2::new(x + 3.0, rect.center().y), 3.5, c);
            x += lead;
        }
        let g = galley(ui, label, font, text, f32::INFINITY);
        p.galley(Pos2::new(x, rect.center().y - g.size().y / 2.0), g, text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// On/off pill with an icon and a count ("Favourites 3").
pub fn toggle_chip(ui: &mut Ui, icon: Icon, label: &str, count: usize, active: bool, color: Color32) -> Response {
    let font = FontId::proportional(12.5);
    let count = count.to_string();
    let lw = galley(ui, label, font.clone(), TEXT, f32::INFINITY).size().x;
    let cw = galley(ui, &count, bold_font(11.5), TEXT, f32::INFINITY).size().x;
    let size = egui::vec2(12.0 + 14.0 + 7.0 + lw + 8.0 + cw + 12.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let act = ui.ctx().animate_bool_with_time(resp.id.with("a"), active, 0.12);
        let fill = lerp_color(SURFACE.gamma_multiply(hover), color.gamma_multiply(0.13), act);
        let stroke = lerp_color(lerp_color(BORDER, BORDER_HI, hover), color.gamma_multiply(0.55), act);
        let text = lerp_color(lerp_color(TEXT_DIM, TEXT, hover), TEXT, act);
        let p = ui.painter();
        p.rect(rect, Rounding::same(rect.height() / 2.0), fill, Stroke::new(1.0_f32, stroke));
        let icon_c = lerp_color(lerp_color(TEXT_FAINT, TEXT_DIM, hover), color, act);
        paint_icon(p, Pos2::new(rect.min.x + 19.0, rect.center().y), icon, 12.5, icon_c);
        let g = galley(ui, label, font, text, f32::INFINITY);
        let x = rect.min.x + 33.0;
        p.galley(Pos2::new(x, rect.center().y - g.size().y / 2.0), g, text);
        let cc = lerp_color(TEXT_FAINT, TEXT_DIM, act.max(hover));
        let g = galley(ui, &count, bold_font(11.5), cc, f32::INFINITY);
        p.galley(Pos2::new(x + lw + 8.0, rect.center().y - g.size().y / 2.0), g, cc);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Options side by side, one chosen; the highlight slides between them.
/// Returns the option clicked.
pub fn segmented(ui: &mut Ui, id_salt: &str, options: &[String], selected: usize) -> Option<usize> {
    if options.is_empty() {
        return None;
    }
    let selected = selected.min(options.len() - 1);
    let font = FontId::proportional(13.0);
    let pad = 3.0;
    let h = CONTROL_H;
    let widths: Vec<f32> = options
        .iter()
        .map(|o| galley(ui, o, font.clone(), TEXT, 360.0).size().x + 30.0)
        .collect();
    let total = widths.iter().sum::<f32>() + pad * 2.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, h), Sense::hover());
    let id = ui.id().with(id_salt);
    let mut segs = Vec::with_capacity(widths.len());
    let mut x = rect.min.x + pad;
    for w in &widths {
        segs.push(Rect::from_min_size(Pos2::new(x, rect.min.y + pad), egui::vec2(*w, h - pad * 2.0)));
        x += w;
    }
    let target = segs[selected];
    let ax = ui.ctx().animate_value_with_time(id.with("x"), target.min.x, 0.16);
    let aw = ui.ctx().animate_value_with_time(id.with("w"), target.width(), 0.16);
    let p = ui.painter();
    p.rect(rect, Rounding::same(R_MD + 1.0), SURFACE, Stroke::new(1.0_f32, BORDER));
    let hi = Rect::from_min_size(Pos2::new(ax, target.min.y), egui::vec2(aw, target.height()));
    p.rect(hi, Rounding::same(R_MD - 1.0), SURFACE_HI, Stroke::new(1.0_f32, BORDER_HI));
    let mut clicked = None;
    for (i, seg) in segs.iter().enumerate() {
        let resp = ui
            .interact(*seg, id.with(i), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let color = if i == selected || resp.hovered() { TEXT } else { TEXT_DIM };
        let g = galley(ui, &options[i], font.clone(), color, seg.width() - 16.0);
        ui.painter().galley(seg.center() - g.size() / 2.0, g, color);
        if resp.clicked() && i != selected {
            clicked = Some(i);
        }
    }
    clicked
}

/// Sidebar navigation entry.
pub fn nav_item(ui: &mut Ui, icon: Icon, label: &str, active: bool) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 36.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let act = ui.ctx().animate_bool_with_time(resp.id.with("a"), active, 0.15);
        let p = ui.painter();
        p.rect_filled(rect, Rounding::same(R_MD), lerp_color(SURFACE.gamma_multiply(hover), SURFACE_HI, act));
        let icon_c = lerp_color(lerp_color(TEXT_FAINT, TEXT_DIM, hover), ACCENT_HI, act);
        paint_icon(p, Pos2::new(rect.min.x + 20.0, rect.center().y), icon, 15.0, icon_c);
        let text = lerp_color(TEXT_DIM, TEXT, act.max(hover));
        let font = if active { bold_font(13.5) } else { FontId::proportional(13.5) };
        let g = galley(ui, label, font, text, rect.width() - 50.0);
        p.galley(Pos2::new(rect.min.x + 40.0, rect.center().y - g.size().y / 2.0), g, text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---------------------------------------------------------------- inputs

/// Single-line text input with an optional leading icon; a search input (`clear`)
/// gets a button that empties it.
pub fn input(ui: &mut Ui, text: &mut String, hint: &str, width: f32, icon: Option<Icon>, clear: bool) -> Response {
    let (rect, frame) = ui.allocate_exact_size(egui::vec2(width, CONTROL_H), Sense::click());
    let background = ui.painter().add(Shape::Noop);
    let left = if icon.is_some() { 34.0 } else { 11.0 };
    let right = if clear { 30.0 } else { 10.0 };
    let text_rect = Rect::from_min_max(
        Pos2::new(rect.min.x + left, rect.min.y),
        Pos2::new((rect.max.x - right).max(rect.min.x + left + 10.0), rect.max.y),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(text_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let mut resp = child.add_sized(
        text_rect.size(),
        egui::TextEdit::singleline(text)
            .hint_text(RichText::new(hint).color(TEXT_FAINT))
            .frame(false)
            .margin(egui::Margin::symmetric(0.0, 0.0))
            .vertical_align(egui::Align::Center),
    );
    if frame.clicked() {
        resp.request_focus();
    }
    if clear && !text.is_empty() {
        let r = Rect::from_center_size(Pos2::new(rect.max.x - 17.0, rect.center().y), egui::vec2(22.0, 22.0));
        let x = ui
            .interact(r, resp.id.with("clear"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let p = ui.painter();
        p.rect_filled(r, Rounding::same(R_SM), SURFACE_HI.gamma_multiply(if x.hovered() { 1.0 } else { 0.0 }));
        paint_icon(p, r.center(), Icon::Close, 10.0, if x.hovered() { TEXT } else { TEXT_FAINT });
        if x.clicked() {
            text.clear();
            resp.mark_changed();
        }
    }
    let focus = resp.has_focus();
    let hover = resp.hovered() || frame.hovered();
    let stroke = if focus {
        ACCENT.gamma_multiply(0.75)
    } else if hover {
        BORDER_HI
    } else {
        BORDER
    };
    ui.painter().set(
        background,
        egui::epaint::RectShape::new(rect, Rounding::same(R_MD), INPUT, Stroke::new(1.0_f32, stroke)),
    );
    if focus {
        ui.painter().rect_stroke(rect.expand(2.0), Rounding::same(R_MD + 2.0), Stroke::new(2.0_f32, ACCENT.gamma_multiply(0.16)));
    }
    if let Some(icon) = icon {
        let c = if focus { TEXT_DIM } else { TEXT_FAINT };
        paint_icon(ui.painter(), Pos2::new(rect.min.x + 18.0, rect.center().y), icon, 13.0, c);
    }
    resp
}

/// On/off switch with a label to its right. Flips `on` when clicked.
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let font = FontId::proportional(13.5);
    let measured = galley(ui, label, font.clone(), TEXT, f32::INFINITY);
    let size = egui::vec2(36.0 + 12.0 + measured.size().x, 24.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_with_time(resp.id.with("on"), *on, 0.15);
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let track = Rect::from_min_size(Pos2::new(rect.min.x, rect.center().y - 10.0), egui::vec2(36.0, 20.0));
        let off = lerp_color(BORDER_HI, Color32::from_rgb(0x42, 0x45, 0x4e), hover);
        let p = ui.painter();
        p.rect_filled(track, Rounding::same(10.0), lerp_color(off, ACCENT, t));
        let x = egui::lerp(track.min.x + 10.0..=track.max.x - 10.0, t);
        p.circle_filled(Pos2::new(x, track.center().y), 7.0, Color32::WHITE);
        let text = lerp_color(TEXT_DIM, TEXT, t.max(hover));
        let g = galley(ui, label, font, text, f32::INFINITY);
        p.galley(Pos2::new(track.max.x + 12.0, rect.center().y - g.size().y / 2.0), g, text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Dropdown with the launcher's height and a chevron; `add` fills the list.
pub fn combo<R>(
    ui: &mut Ui,
    id_salt: &str,
    selected: impl Into<egui::WidgetText>,
    width: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = CONTROL_H;
        ui.spacing_mut().button_padding = egui::vec2(11.0, 7.0);
        egui::ComboBox::from_id_salt(id_salt)
            .selected_text(selected)
            .width(width)
            .icon(|ui, rect, visuals, _open, _above| {
                paint_icon(ui.painter(), rect.center(), Icon::ChevronDown, 10.0, visuals.fg_stroke.color);
            })
            .show_ui(ui, |ui| {
                // the list is denser than the button
                ui.spacing_mut().interact_size.y = 28.0;
                ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                add(ui)
            })
            .inner
    })
    .inner
}

// ---------------------------------------------------------------- containers

/// A titled block of settings on a surface.
pub fn section(ui: &mut Ui, title: &str, subtitle: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::none()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(Rounding::same(R_LG))
        .inner_margin(egui::Margin::same(18.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.label(RichText::new(title).font(bold_font(14.5)).color(TEXT));
            if !subtitle.is_empty() {
                ui.label(RichText::new(subtitle).size(12.0).color(TEXT_FAINT));
            }
            ui.add_space(8.0);
            add(ui);
        });
}

/// A dialog over the dimmed window (the rest can't be clicked meanwhile).
pub fn modal<R>(ctx: &egui::Context, id: &str, width: f32, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new(id).with("backdrop"))
        .order(egui::Order::PanelResizeLine)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha(150));
            ui.allocate_rect(screen, Sense::click_and_drag());
        });
    egui::Window::new(id)
        .id(egui::Id::new(id))
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .frame(
            egui::Frame::window(&ctx.style())
                .inner_margin(egui::Margin::same(22.0))
                .rounding(Rounding::same(14.0)),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            add(ui)
        })
        .and_then(|r| r.inner)
}
