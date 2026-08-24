//! tiny-skia rendering for the wl-menu overlay.

use crate::config::{FontConfig, Theme};
use crate::icons::is_file_or_theme_icon;
use crate::state::{ActionConfirm, MenuSession};
use fontdue::layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle};
use fontdue::{Font, FontSettings};
use std::process::Command;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke};

/// Load menu font via fontconfig, same approach as image-picker-wl.
pub fn load_font(font_cfg: &FontConfig) -> Option<Font> {
    let pattern = font_cfg.family.clone();
    let output = Command::new("fc-match")
        .args(["-f", "%{file}", &pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    Font::from_bytes(data, FontSettings::default()).ok()
}

pub struct CardLayout {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub header_height: f32,
    pub row_height: f32,
    pub visible_rows: usize,
}

impl CardLayout {
    pub fn compute(session: &MenuSession, screen_w: u32, screen_h: u32) -> Self {
        let layout = session.config.layout;
        let row_count = session
            .confirm
            .as_ref()
            .map(|_| 2usize)
            .unwrap_or_else(|| session.rows.len().min(layout.max_rows).max(1));
        let header_height = layout.row_height + layout.padding;
        let body_height = row_count as f32 * layout.row_height;
        let height = layout.padding * 2.0 + header_height + body_height;
        let width = layout.width;
        let x = (screen_w as f32 - width) * 0.5;
        let y = screen_h as f32 * 0.18;
        Self {
            x,
            y,
            width,
            height,
            header_height,
            row_height: layout.row_height,
            visible_rows: row_count,
        }
    }
}

/// Render the full-screen menu frame.
pub fn render_frame(
    width: u32,
    height: u32,
    session: &MenuSession,
    font: Option<&Font>,
    layout: &CardLayout,
) -> Pixmap {
    let mut pixmap = Pixmap::new(width, height).expect("pixmap");
    if !session.visible {
        pixmap.fill(Color::TRANSPARENT);
        return pixmap;
    }
    pixmap.fill(session.config.theme.scrim);

    let theme = &session.config.theme;
    draw_round_rect(
        &mut pixmap,
        layout.x,
        layout.y,
        layout.width,
        layout.height,
        10.0,
        theme.background,
        Some((theme.border, 1.0)),
    );

    draw_text(
        &mut pixmap,
        font,
        &session.header_title(),
        layout.x + session.config.layout.padding,
        layout.y + session.config.layout.padding,
        session.config.font.size,
        theme.text,
    );

    // Confirm dialog uses the prompt as the title; hide the menu breadcrumb.
    let breadcrumb = session.breadcrumb();
    if session.confirm.is_none() && !breadcrumb.is_empty() && session.filter.is_empty() {
        draw_text(
            &mut pixmap,
            font,
            &breadcrumb,
            layout.x + layout.width - session.config.layout.padding - 120.0,
            layout.y + session.config.layout.padding,
            session.config.font.size * 0.75,
            with_alpha(theme.text, 0.55),
        );
    }

    let rows_top = layout.y + layout.header_height + session.config.layout.padding * 0.5;
    if let Some(confirm) = &session.confirm {
        draw_confirm(
            &mut pixmap,
            font,
            session,
            confirm,
            layout,
            rows_top,
        );
    } else {
        let scroll = session.scroll_offset(layout.visible_rows);
        draw_rows(
            &mut pixmap,
            font,
            session,
            scroll,
            layout,
            rows_top,
            theme,
            session.config.font.size,
        );
    }

    pixmap
}

/// Confirm body: only Yes / Cancel rows (prompt lives in the header).
fn draw_confirm(
    pixmap: &mut Pixmap,
    font: Option<&Font>,
    session: &MenuSession,
    confirm: &ActionConfirm,
    card: &CardLayout,
    rows_top: f32,
) {
    let theme = &session.config.theme;
    let (_, yes, no) = session.confirm_lines();
    let options = [yes.as_str(), no.as_str()];
    for (index, label) in options.iter().enumerate() {
        let y = rows_top + card.row_height * index as f32;
        let selected = confirm.selected == index;
        if selected {
            draw_round_rect(
                pixmap,
                card.x + 8.0,
                y,
                card.width - 16.0,
                card.row_height - 2.0,
                6.0,
                theme.selected_background,
                None,
            );
        }
        draw_text(
            pixmap,
            font,
            label,
            card.x + 16.0,
            y + 6.0,
            session.config.font.size,
            if selected {
                theme.selected_text
            } else {
                theme.text
            },
        );
    }
}

fn draw_rows(
    pixmap: &mut Pixmap,
    font: Option<&Font>,
    session: &MenuSession,
    scroll: usize,
    card: &CardLayout,
    rows_top: f32,
    theme: &Theme,
    font_size: f32,
) {
    let rows = &session.rows;
    let selected = session.selected;
    let end = (scroll + card.visible_rows).min(rows.len());
    for (view_i, index) in (scroll..end).enumerate() {
        let Some(row) = rows.get(index) else {
            continue;
        };
        let y = rows_top + view_i as f32 * card.row_height;
        if index == selected {
            draw_round_rect(
                pixmap,
                card.x + 8.0,
                y,
                card.width - 16.0,
                card.row_height - 2.0,
                6.0,
                theme.selected_background,
                None,
            );
        }
        let color = if index == selected {
            theme.selected_text
        } else {
            theme.text
        };
        let mut x = card.x + 16.0;
        let icon_size = (card.row_height - 10.0).clamp(16.0, 48.0);
        if !row.icon.is_empty() {
            if let Some(image) = session.icons.get(&row.icon, icon_size.round() as u32) {
                let iy = y + (card.row_height - icon_size) * 0.5;
                blit_icon(pixmap, image.as_ref(), x, iy, icon_size);
                x += icon_size + 8.0;
            } else if !is_file_or_theme_icon(&row.icon) {
                draw_text(pixmap, font, &row.icon, x, y + 6.0, font_size, color);
                x += font_size * 1.4;
            }
        }
        draw_text(pixmap, font, &row.label, x, y + 6.0, font_size, color);
        if !row.detail.is_empty() {
            draw_text(
                pixmap,
                font,
                &row.detail,
                card.x + card.width - 16.0 - row.detail.len() as f32 * font_size * 0.45,
                y + 6.0,
                font_size * 0.75,
                with_alpha(color, 0.65),
            );
        }
    }
}

/// Draw a square app icon, already scaled by the cache.
fn blit_icon(canvas: &mut Pixmap, icon: &Pixmap, x: f32, y: f32, size: f32) {
    if size <= 0.0 {
        return;
    }
    let scale = size / icon.width().max(1) as f32;
    canvas.draw_pixmap(
        0,
        0,
        icon.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, x, y),
        None,
    );
}

/// Map pointer coordinates to a row index inside the card.
pub fn hit_test_row(session: &MenuSession, layout: &CardLayout, x: f32, y: f32) -> Option<usize> {
    if !session.visible {
        return None;
    }
    if x < layout.x || x > layout.x + layout.width || y < layout.y || y > layout.y + layout.height
    {
        return None;
    }
    let rows_top = layout.y + layout.header_height + session.config.layout.padding * 0.5;
    if y < rows_top {
        return None;
    }
    let view_index = ((y - rows_top) / layout.row_height) as usize;
    if session.confirm.is_some() {
        return (view_index < 2).then_some(view_index);
    }
    let scroll = session.scroll_offset(layout.visible_rows);
    let index = scroll + view_index;
    (index < session.rows.len() && view_index < layout.visible_rows).then_some(index)
}

fn draw_round_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    _radius: f32,
    fill: Color,
    stroke: Option<(Color, f32)>,
) {
    let rect = tiny_skia::Rect::from_xywh(x, y, width, height).unwrap();
    let mut pb = PathBuilder::new();
    pb.push_rect(rect);
    let path = pb.finish().unwrap();
    let mut paint = Paint::default();
    paint.set_color(fill);
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, FillRule::Winding, tiny_skia::Transform::identity(), None);
    if let Some((color, width_px)) = stroke {
        paint.set_color(color);
        let stroke = Stroke {
            width: width_px,
            ..Default::default()
        };
        pixmap.stroke_path(
            &path,
            &paint,
            &stroke,
            tiny_skia::Transform::identity(),
            None,
        );
    }
}

fn draw_text(
    pixmap: &mut Pixmap,
    font: Option<&Font>,
    text: &str,
    x: f32,
    y: f32,
    size: f32,
    color: Color,
) {
    let Some(font) = font else {
        return;
    };
    let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
    layout.reset(&LayoutSettings {
        x,
        y,
        ..LayoutSettings::default()
    });
    layout.append(&[font], &TextStyle::new(text, size, 0));
    for glyph in layout.glyphs() {
        let (_, bitmap) = font.rasterize_config(glyph.key);
        blit_glyph(
            pixmap,
            &bitmap,
            glyph.width,
            glyph.height,
            glyph.x,
            glyph.y,
            color,
        );
    }
}

fn blit_glyph(
    canvas: &mut Pixmap,
    bitmap: &[u8],
    width: usize,
    height: usize,
    x: f32,
    y: f32,
    color: Color,
) {
    let start_x = x.round() as i32;
    let start_y = y.round() as i32;
    let canvas_w = canvas.width() as usize;
    let canvas_h = canvas.height();
    let pixels = canvas.pixels_mut();
    for row in 0..height {
        for col in 0..width {
            let alpha = bitmap[row * width + col];
            if alpha == 0 {
                continue;
            }
            let px = start_x + col as i32;
            let py = start_y + row as i32;
            if px < 0 || py < 0 {
                continue;
            }
            let px = px as u32;
            let py = py as u32;
            if px >= canvas_w as u32 || py >= canvas_h {
                continue;
            }
            let mut paint = color;
            paint.set_alpha((f32::from(alpha) / 255.0) * color.alpha());
            let src = paint.premultiply().to_color_u8();
            let dst = &mut pixels[canvas_w * py as usize + px as usize];
            blend_pixel_u8(dst, src);
        }
    }
}

fn blend_pixel_u8(dst: &mut tiny_skia::PremultipliedColorU8, src: tiny_skia::PremultipliedColorU8) {
    let sa = f32::from(src.alpha()) / 255.0;
    if sa <= f32::EPSILON {
        return;
    }
    if sa >= 1.0 - f32::EPSILON {
        *dst = src;
        return;
    }
    let inv_sa = 1.0 - sa;
    let da = f32::from(dst.alpha()) / 255.0;
    if let Some(blended) = tiny_skia::PremultipliedColorU8::from_rgba(
        (f32::from(src.red()) + f32::from(dst.red()) * inv_sa)
            .round()
            .clamp(0.0, 255.0) as u8,
        (f32::from(src.green()) + f32::from(dst.green()) * inv_sa)
            .round()
            .clamp(0.0, 255.0) as u8,
        (f32::from(src.blue()) + f32::from(dst.blue()) * inv_sa)
            .round()
            .clamp(0.0, 255.0) as u8,
        ((sa + da * inv_sa) * 255.0).round().clamp(0.0, 255.0) as u8,
    ) {
        *dst = blended;
    }
}

fn with_alpha(color: Color, alpha: f32) -> Color {
    let mut out = color;
    out.set_alpha(alpha);
    out
}
