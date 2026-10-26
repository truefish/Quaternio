//! Native text uses the same embedded font and rasterizer for preview and export.
use super::composite::ImageBuf;
use crate::project::{creative::TextStyle, Clip};
use ab_glyph::{point, Font, FontRef, ScaleFont};
use std::sync::OnceLock;
fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| {
        FontRef::try_from_slice(include_bytes!("../../ui/fonts/Inter-Regular.ttf"))
            .expect("embedded Inter")
    })
}
pub fn draw(dst: &mut ImageBuf, clip: &Clip, sequence_width: u32, weight: f32) {
    let Some(text) = &clip.creative.text else {
        return;
    };
    let factor = dst.width as f32 / sequence_width.max(1) as f32;
    let size = (text.size * factor * clip.scale).clamp(4., 1024.);
    let font = font();
    let scaled = font.as_scaled(size);
    let margin = dst.width as f32 * 0.08;
    let available = dst.width as f32 - 2. * margin;
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut width = 0.;
    for c in text.text.chars().take(4096) {
        let advance = scaled.h_advance(font.glyph_id(c));
        if c == '\n' || width + advance > available {
            lines.push(std::mem::take(&mut line));
            width = 0.;
            if c == '\n' {
                continue;
            }
        }
        line.push(c);
        width += advance;
    }
    lines.push(line);
    lines.truncate(16);
    let line_height = size * 1.25;
    let height = line_height * lines.len() as f32;
    let baseline = match text.style {
        TextStyle::Title => (dst.height as f32 - height) * 0.5,
        TextStyle::LowerThird => dst.height as f32 * 0.76,
        TextStyle::Caption => dst.height as f32 * 0.89 - height,
    };
    let color = crate::color::PreparedByteGrade::new(clip.grade).grade_u8(text.color);
    let opacity = weight.clamp(0., 1.);
    let ox = clip.pos_x * factor;
    let oy = clip.pos_y * factor;
    for (i, line) in lines.iter().enumerate() {
        let width: f32 = line
            .chars()
            .map(|c| scaled.h_advance(font.glyph_id(c)))
            .sum();
        let x = if text.style == TextStyle::LowerThird {
            margin
        } else {
            (dst.width as f32 - width) * 0.5
        } + ox;
        let y = baseline + i as f32 * line_height + oy;
        if text.background || text.style == TextStyle::Caption {
            for py in
                (y - 4.).max(0.) as u32..(y + line_height).min(dst.height as f32).max(0.) as u32
            {
                for px in
                    (x - 10.).max(0.) as u32..(x + width + 10.).min(dst.width as f32).max(0.) as u32
                {
                    blend(dst, px, py, [0; 3], opacity * 0.7);
                }
            }
        }
        let mut cursor = x;
        let mut previous = None;
        for c in line.chars() {
            let id = font.glyph_id(c);
            if let Some(prev) = previous {
                cursor += scaled.kern(prev, id);
            }
            let glyph = id.with_scale_and_position(size, point(cursor, y + scaled.ascent()));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    if px >= 0 && py >= 0 {
                        blend(dst, px as u32, py as u32, color, coverage * opacity);
                    }
                });
            }
            cursor += scaled.h_advance(id);
            previous = Some(id);
        }
    }
}
fn blend(dst: &mut ImageBuf, x: u32, y: u32, color: [u8; 3], alpha: f32) {
    if x >= dst.width || y >= dst.height {
        return;
    }
    let i = ((y * dst.width + x) * 3) as usize;
    for c in 0..3 {
        dst.rgb[i + c] = (dst.rgb[i + c] as f32 * (1. - alpha) + color[c] as f32 * alpha)
            .round()
            .clamp(0., 255.) as u8;
    }
}
