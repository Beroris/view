//! Text, in the suite's own font, drawn straight into the frame.

use crate::render::{Frame, blend};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};

pub struct Text {
    font: FontRef<'static>,
}

impl Text {
    pub fn new() -> Text {
        let font = FontRef::try_from_slice(include_bytes!("../../../assets/fonts/GeistVariable.ttf")).expect("the built-in font is a font");
        Text { font }
    }

    /// ab_glyph sizes by line height; this turns a size in pixels per em into that.
    fn scale(&self, size: f32) -> PxScale {
        let em = self.font.units_per_em().unwrap_or(1000.0);
        PxScale::from(size * self.font.height_unscaled() / em)
    }

    pub fn width(&self, s: &str, size: f32) -> f32 {
        let f = self.font.as_scaled(self.scale(size));
        let mut x = 0.0;
        let mut last = None;
        for c in s.chars() {
            let id = f.glyph_id(c);
            if let Some(l) = last {
                x += f.kern(l, id);
            }
            x += f.h_advance(id);
            last = Some(id);
        }
        x
    }

    /// `s`, cut short with an ellipsis if it's wider than `max`.
    pub fn fitted(&self, s: &str, size: f32, max: f32) -> String {
        if self.width(s, size) <= max {
            return s.to_string();
        }
        let mut chars: Vec<char> = s.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let t: String = chars.iter().collect::<String>() + "…";
            if self.width(&t, size) <= max {
                return t;
            }
        }
        String::new()
    }

    /// Draw `s` with its left edge at `x` and the middle of its line at `y`.
    pub fn draw(&self, frame: &mut Frame, s: &str, x: f32, y: f32, size: f32, rgb: u32, alpha: f32) {
        let scale = self.scale(size);
        let f = self.font.as_scaled(scale);
        let baseline = y + (f.ascent() + f.descent()) / 2.0;
        let mut pen = x;
        let mut last = None;
        for c in s.chars() {
            let id = f.glyph_id(c);
            if let Some(l) = last {
                pen += f.kern(l, id);
            }
            let glyph = id.with_scale_and_position(scale, point(pen, baseline));
            pen += f.h_advance(id);
            last = Some(id);
            let Some(outline) = self.font.outline_glyph(glyph) else { continue };
            let bounds = outline.px_bounds();
            let (ox, oy) = (bounds.min.x as i32, bounds.min.y as i32);
            outline.draw(|gx, gy, cover| {
                let (px, py) = (ox + gx as i32, oy + gy as i32);
                if px >= 0 && py >= 0 && (px as usize) < frame.w && (py as usize) < frame.h {
                    blend(&mut frame.px[py as usize * frame.w + px as usize], rgb, cover * alpha);
                }
            });
        }
    }
}
