//! Text in photos: the words Windows reads off the picture on screen (see [`crate::ocr`]), so
//! they can be selected and copied like the text in a document. Dragging across words selects
//! them; dragging anywhere else still pans.

use std::path::PathBuf;
use view_core::orient::Orient;
use view_core::pdftext::{Break, PageText};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

pub struct Words {
    /// The photo, and the way it was turned, when the words were read.
    pub path: PathBuf,
    pub orient: Orient,
    /// The characters, with boxes as fractions of the picture as it's shown.
    pub page: PageText,
    /// From where a drag started to where it is, as character positions.
    pub selection: Option<(usize, usize)>,
}

/// Where the picture is on screen: left, top, width and height.
pub type Placed = (f32, f32, f32, f32);

impl Words {
    fn glyph_rect(&self, k: usize, at: Placed) -> D2D_RECT_F {
        let (x, y, w, h) = at;
        let r = self.page.glyphs[k].rect;
        D2D_RECT_F { left: x + r[0] * w, top: y + r[1] * h, right: x + r[2] * w, bottom: y + r[3] * h }
    }

    /// Whether there's a word right under a point: there, a press selects rather than pans.
    pub fn over(&self, px: f32, py: f32, at: Placed) -> bool {
        (0..self.page.glyphs.len()).any(|k| {
            let r = self.glyph_rect(k, at);
            let m = ((r.bottom - r.top) * 0.15).max(2.0);
            px >= r.left - m && px < r.right + m && py >= r.top - m && py < r.bottom + m
        })
    }

    /// The character position nearest a point: before or after the character, by which half
    /// of it the point is in. Nearness counts across a line four times over along it, so a
    /// point beside a line finds that line.
    pub fn position(&self, px: f32, py: f32, at: Placed) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for k in 0..self.page.glyphs.len() {
            let r = self.glyph_rect(k, at);
            let dx = if px < r.left { r.left - px } else if px > r.right { px - r.right } else { 0.0 };
            let dy = if py < r.top { r.top - py } else if py > r.bottom { py - r.bottom } else { 0.0 };
            let d = dy * 4.0 + dx;
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((k, d));
            }
        }
        let (k, _) = best?;
        let r = self.glyph_rect(k, at);
        Some(k + (px > (r.left + r.right) / 2.0) as usize)
    }

    /// Start selecting at a point; `word` selects the whole word there (a double-click).
    pub fn select_start(&mut self, px: f32, py: f32, at: Placed, word: bool) {
        let Some(p) = self.position(px, py, at) else { return };
        self.selection = Some((p, p));
        if word {
            let k = p.min(self.page.glyphs.len().saturating_sub(1));
            // The character the point is on, not the one after it.
            let k = if p > 0 && !self.inside(k, px, py, at) { p - 1 } else { k };
            let (a, b) = self.page.word(k);
            self.selection = Some((a, b));
        }
    }

    fn inside(&self, k: usize, px: f32, py: f32, at: Placed) -> bool {
        let r = self.glyph_rect(k, at);
        px >= r.left && px < r.right && py >= r.top && py < r.bottom
    }

    pub fn select_to(&mut self, px: f32, py: f32, at: Placed) {
        let p = self.position(px, py, at);
        if let (Some((_, end)), Some(p)) = (&mut self.selection, p) {
            *end = p;
        }
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|(a, b)| a != b)
    }

    fn ordered(&self) -> Option<(usize, usize)> {
        let (a, b) = self.selection.filter(|(a, b)| a != b)?;
        Some((a.min(b), a.max(b).min(self.page.glyphs.len())))
    }

    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.ordered()?;
        Some(self.page.copy(a, b))
    }

    /// The selection on screen: a band along each line it covers.
    pub fn bands(&self, at: Placed) -> Vec<D2D_RECT_F> {
        let Some((a, b)) = self.ordered() else { return Vec::new() };
        let mut out = Vec::new();
        let mut line: Option<D2D_RECT_F> = None;
        for k in a..b {
            if self.page.glyphs[k].brk == Break::Line {
                out.extend(line.take());
            }
            let r = self.glyph_rect(k, at);
            line = Some(match line {
                Some(l) => D2D_RECT_F { left: l.left.min(r.left), top: l.top.min(r.top), right: l.right.max(r.right), bottom: l.bottom.max(r.bottom) },
                None => r,
            });
        }
        out.extend(line);
        out
    }
}
