//! Visual theme: palette, fonts, egui style and small custom-painted widgets.

use eframe::egui::{
    self, Color32, FontFamily, FontId, Mesh, Pos2, Rect, Response, Rounding, Sense, Shape, Stroke,
    TextStyle, Ui, Vec2,
};

// ---------------------------------------------------------------- palette

pub const BG_DEEP: Color32 = Color32::from_rgb(0x0a, 0x0c, 0x10);
pub const BG: Color32 = Color32::from_rgb(0x0f, 0x12, 0x17);
pub const SURFACE: Color32 = Color32::from_rgb(0x16, 0x1b, 0x22);
pub const SURFACE_HI: Color32 = Color32::from_rgb(0x1d, 0x24, 0x2e);
pub const INPUT: Color32 = Color32::from_rgb(0x0c, 0x0f, 0x14);
pub const BORDER: Color32 = Color32::from_rgb(0x24, 0x2c, 0x37);
pub const BORDER_HI: Color32 = Color32::from_rgb(0x37, 0x42, 0x51);
pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xeb, 0xf0);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8a, 0x94, 0xa4);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x5a, 0x63, 0x70);
pub const ACCENT: Color32 = Color32::from_rgb(0xe0, 0x45, 0x3a);
pub const ACCENT_HI: Color32 = Color32::from_rgb(0xff, 0x5f, 0x52);
pub const ACCENT_LO: Color32 = Color32::from_rgb(0x9e, 0x24, 0x1d);
pub const EQUIPPED: Color32 = Color32::from_rgb(0x3d, 0xd6, 0x8c);
pub const DANGER: Color32 = Color32::from_rgb(0xf8, 0x51, 0x49);
pub const FAVORITE: Color32 = Color32::from_rgb(0xff, 0xc8, 0x3d);

pub const R_SM: f32 = 6.0;
pub const R_MD: f32 = 10.0;

/// Custom family holding a bold face (Segoe UI Bold when available).
pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn bold_font(size: f32) -> FontId {
    FontId::new(size, bold())
}

/// egui's built-in fonts only (no Segoe), so emoji/icon glyphs render correctly.
pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}

// ---------------------------------------------------------------- setup

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    install_style(ctx);
}

fn install_fonts(ctx: &egui::Context) {
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

    let mut bold_list = base;
    if let Some(bytes) = read("segoeuib.ttf").or_else(|| read("seguisb.ttf")) {
        fonts
            .font_data
            .insert("segoe-bold".into(), egui::FontData::from_owned(bytes));
        bold_list.insert(0, "segoe-bold".into());
    } else if regular.first().map(String::as_str) == Some("segoe") {
        bold_list.insert(0, "segoe".into());
    }

    fonts.families.insert(FontFamily::Proportional, regular);
    fonts.families.insert(bold(), bold_list);
    ctx.set_fonts(fonts);
}

fn install_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = INPUT;
    v.faint_bg_color = SURFACE;
    v.code_bg_color = INPUT;
    v.window_rounding = Rounding::same(R_MD);
    v.menu_rounding = Rounding::same(R_MD);
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.hyperlink_color = ACCENT_HI;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.55);
    v.selection.stroke = Stroke::new(1.0_f32, TEXT);
    let shadow = egui::Shadow {
        offset: egui::vec2(0.0, 10.0),
        blur: 28.0,
        spread: 0.0,
        color: Color32::from_black_alpha(140),
    };
    v.window_shadow = shadow;
    v.popup_shadow = shadow;

    let rounding = Rounding::same(R_SM);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);
    w.noninteractive.rounding = rounding;

    w.inactive.bg_fill = SURFACE_HI;
    w.inactive.weak_bg_fill = SURFACE_HI;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.rounding = rounding;

    w.hovered.bg_fill = Color32::from_rgb(0x25, 0x2e, 0x3a);
    w.hovered.weak_bg_fill = Color32::from_rgb(0x25, 0x2e, 0x3a);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, BORDER_HI);
    w.hovered.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.hovered.rounding = rounding;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = Color32::from_rgb(0x2c, 0x36, 0x44);
    w.active.weak_bg_fill = Color32::from_rgb(0x2c, 0x36, 0x44);
    w.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.active.fg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    w.active.rounding = rounding;
    w.active.expansion = 0.0;

    w.open = w.hovered;

    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.combo_height = 420.0;
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

/// Horizontal gradient over a rectangle (square corners).
pub fn hgradient(rect: Rect, left: Color32, right: Color32) -> Shape {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), left);
    mesh.colored_vertex(rect.right_top(), right);
    mesh.colored_vertex(rect.left_bottom(), left);
    mesh.colored_vertex(rect.right_bottom(), right);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    Shape::mesh(mesh)
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

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

/// Small uppercase section caption.
pub fn caption(ui: &mut Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .font(bold_font(10.5))
            .color(TEXT_FAINT)
            .extra_letter_spacing(1.2),
    );
}

// ---------------------------------------------------------------- widgets

/// Pill-shaped toggle chip. `color` tints it when active.
pub fn chip(ui: &mut Ui, label: &str, active: bool, color: Color32) -> Response {
    let font = bold_font(11.0);
    let g = galley(ui, label, font, TEXT, f32::INFINITY);
    let size = egui::vec2(g.size().x + 18.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let (fill, stroke, text) = if active {
            (color.gamma_multiply(0.22), color, lerp_color(color, Color32::WHITE, 0.35))
        } else {
            (
                lerp_color(SURFACE, SURFACE_HI, hover),
                lerp_color(BORDER, BORDER_HI, hover),
                lerp_color(TEXT_DIM, TEXT, hover),
            )
        };
        let p = ui.painter();
        p.rect(rect, Rounding::same(13.0), fill, Stroke::new(1.0_f32, stroke));
        let g = galley(ui, label, bold_font(11.0), text, f32::INFINITY);
        p.galley(rect.center() - g.size() / 2.0, g, text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Large gradient call-to-action button.
pub fn primary_button(ui: &mut Ui, label: &str, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.12);
        let press = if resp.is_pointer_button_down_on() { 1.0 } else { 0.0 };
        let p = ui.painter();
        let r = Rounding::same(R_MD);

        let glow = egui::Shadow {
            offset: egui::vec2(0.0, 6.0),
            blur: 22.0,
            spread: 0.0,
            color: ACCENT.gamma_multiply(0.25 + 0.35 * hover),
        };
        p.add(glow.as_shape(rect, r));

        let left = lerp_color(ACCENT, ACCENT_HI, hover - press * 0.5);
        let right = lerp_color(ACCENT_LO, ACCENT, hover - press * 0.5);
        p.rect_filled(rect, r, right);
        // gradient body, inset so rounded corners stay clean
        let inner = rect.shrink2(egui::vec2(R_MD * 0.6, 0.0));
        p.add(hgradient(inner, left, right));
        p.rect_filled(
            Rect::from_min_max(rect.min, Pos2::new(inner.min.x + 1.0, rect.max.y)),
            Rounding {
                nw: R_MD,
                sw: R_MD,
                ne: 0.0,
                se: 0.0,
            },
            left,
        );
        // top sheen
        let sheen = Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.center().y));
        p.rect_filled(
            sheen.shrink2(egui::vec2(2.0, 1.0)),
            Rounding {
                nw: R_MD,
                ne: R_MD,
                sw: 0.0,
                se: 0.0,
            },
            Color32::from_white_alpha(10),
        );
        p.rect_stroke(rect, r, Stroke::new(1.0_f32, ACCENT_HI.gamma_multiply(0.6)));

        let g = galley(ui, label, bold_font(15.0), Color32::WHITE, rect.width() - 16.0);
        p.galley(rect.center() - g.size() / 2.0, g, Color32::WHITE);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Outlined secondary button that fills the given size.
pub fn ghost_button(ui: &mut Ui, label: &str, size: Vec2, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let p = ui.painter();
        p.rect(
            rect,
            Rounding::same(R_SM + 2.0),
            lerp_color(SURFACE, SURFACE_HI, hover),
            Stroke::new(1.0_f32, lerp_color(BORDER_HI, color, hover)),
        );
        let text = lerp_color(TEXT, color, hover * 0.6);
        let g = galley(ui, label, bold_font(13.0), text, rect.width() - 12.0);
        p.galley(rect.center() - g.size() / 2.0, g, text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Vector icons for the top bar (font glyphs for these look inconsistent).
#[derive(Clone, Copy)]
pub enum Icon {
    Reload,
    Save,
    Folder,
}

fn paint_icon(p: &egui::Painter, c: Pos2, icon: Icon, color: Color32) {
    let st = Stroke::new(1.6_f32, color);
    let v = egui::vec2;
    match icon {
        Icon::Reload => {
            let r = 6.5;
            let pts: Vec<Pos2> = (0..=24)
                .map(|i| {
                    let a = -0.6 + (i as f32 / 24.0) * 5.0;
                    c + v(a.cos() * r, a.sin() * r)
                })
                .collect();
            let end = *pts.last().unwrap();
            p.add(Shape::line(pts, st));
            // arrow head at the end of the arc
            p.add(Shape::convex_polygon(
                vec![end + v(-3.5, -2.5), end + v(3.0, -3.0), end + v(0.5, 3.0)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Save => {
            let body = Rect::from_center_size(c, v(14.0, 14.0));
            p.rect_stroke(body, Rounding::same(2.5), st);
            let label = Rect::from_min_max(body.min + v(3.5, 0.0), Pos2::new(body.max.x - 3.5, body.min.y + 4.5));
            p.rect_stroke(label, Rounding::ZERO, st);
            p.rect_filled(
                Rect::from_min_max(Pos2::new(body.min.x + 3.5, body.max.y - 5.5), body.max - v(3.5, 2.0)),
                Rounding::same(1.0),
                color,
            );
        }
        Icon::Folder => {
            let l = c.x - 7.5;
            let r = c.x + 7.5;
            let t = c.y - 5.5;
            let b = c.y + 6.0;
            p.add(Shape::closed_line(
                vec![
                    Pos2::new(l, t),
                    Pos2::new(l + 5.0, t),
                    Pos2::new(l + 6.5, t + 2.0),
                    Pos2::new(r, t + 2.0),
                    Pos2::new(r, b),
                    Pos2::new(l, b),
                ],
                st,
            ));
            p.line_segment([Pos2::new(l, t + 4.5), Pos2::new(r, t + 4.5)], st);
        }
    }
}

/// Square icon button used in the top bar.
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let p = ui.painter();
        p.rect(
            rect,
            Rounding::same(R_SM + 2.0),
            lerp_color(SURFACE, SURFACE_HI, hover),
            Stroke::new(1.0_f32, lerp_color(BORDER, BORDER_HI, hover)),
        );
        let color = lerp_color(TEXT_DIM, TEXT, hover);
        paint_icon(p, rect.center(), icon, color);
    }
    resp.on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}
