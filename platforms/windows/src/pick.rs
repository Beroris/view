//! Text in View's own interface that can be selected and copied, the way text on a web page
//! can: the information panel, messages, dialogs and the preview's file cards. Each frame notes
//! the text it draws here, in reading order, and paints the selection under it. A drag across
//! it selects, a double-click picks a word, and Ctrl+C copies.

use crate::gfx::rect;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::DirectWrite::*;

/// What goes before a piece of text, when it's copied along with the one before it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Join {
    /// A new line: the next row, or a paragraph.
    Line,
    /// A tab: the value beside a label.
    Tab,
}

struct Span {
    layout: IDWriteTextLayout,
    /// Where the layout was drawn.
    x: f32,
    y: f32,
    /// Where its text is, inside what's visible of it.
    area: D2D_RECT_F,
    text: String,
    units: u32,
    join: Join,
}

/// A place in the text: which piece, and how far into it in UTF-16 units.
type Pos = (usize, u32);

#[derive(Default)]
pub struct Picker {
    spans: Vec<Span>,
    /// What the frame before drew, to notice the text changing under a selection.
    before: Vec<String>,
    /// Where the selection started, and where it's got to.
    sel: Option<(Pos, Pos)>,
    /// The word a double-click picked, which dragging on from extends a word at a time.
    word: Option<(Pos, Pos)>,
}

impl Picker {
    /// A new frame: what's drawn now is noted afresh.
    pub fn begin(&mut self) {
        self.before = self.spans.drain(..).map(|s| s.text).collect();
        if self.sel.is_some_and(|(a, b)| a.0.max(b.0) >= self.before.len()) {
            self.sel = None; // what was selected has gone
        }
    }

    /// Note text drawn at `x`, `y`, in reading order. `clip` is what can be seen of it, if it's in
    /// something that scrolls. Returns where its selected part is, to be painted under it.
    pub fn add(&mut self, layout: &IDWriteTextLayout, x: f32, y: f32, text: &str, clip: Option<D2D_RECT_F>, join: Join) -> Vec<D2D_RECT_F> {
        let i = self.spans.len();
        if self.sel.is_some() && self.before.get(i).is_some_and(|t| t != text) {
            self.sel = None; // the text changed: the selection no longer means anything
        }
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = layout.GetMetrics(&mut m);
        }
        let mut area = rect(x + m.left, y + m.top, m.widthIncludingTrailingWhitespace, m.height);
        if let Some(c) = clip {
            area = D2D_RECT_F { left: area.left.max(c.left), top: area.top.max(c.top), right: area.right.min(c.right), bottom: area.bottom.min(c.bottom) };
        }
        let units = text.encode_utf16().count() as u32;
        let marks = match self.range_in(i, units) {
            Some((from, to)) => crate::doc::range_rects(layout, from, to).iter().map(|r| rect(x + r.left, y + r.top, r.width.max(3.0), r.height)).collect(),
            None => Vec::new(),
        };
        self.spans.push(Span { layout: layout.clone(), x, y, area, text: text.to_string(), units, join });
        marks
    }
    /// The selected part of piece `i`, if any of it is.
    fn range_in(&self, i: usize, units: u32) -> Option<(u32, u32)> {
        let (a, b) = self.sel?;
        let (lo, hi) = (a.min(b), a.max(b));
        if i < lo.0 || i > hi.0 {
            return None;
        }
        let from = if i == lo.0 { lo.1 } else { 0 };
        let to = if i == hi.0 { hi.1 } else { units };
        (to > from).then_some((from, to))
    }

    /// The piece of text under a point, if there is one.
    fn under(&self, x: f32, y: f32) -> Option<usize> {
        let slack = 2.0;
        self.spans.iter().position(|s| s.area.right > s.area.left && x >= s.area.left - slack && x < s.area.right + slack && y >= s.area.top && y < s.area.bottom)
    }

    /// The nearest piece of text to a point: the mouse can leave the text while dragging.
    fn nearest(&self, x: f32, y: f32) -> Option<usize> {
        let away = |s: &Span| {
            let dx = (s.area.left - x).max(x - s.area.right).max(0.0);
            let dy = (s.area.top - y).max(y - s.area.bottom).max(0.0);
            dy * 4.0 + dx
        };
        (0..self.spans.len()).filter(|&i| self.spans[i].area.bottom > self.spans[i].area.top).min_by(|&a, &b| away(&self.spans[a]).total_cmp(&away(&self.spans[b])))
    }

    fn position(&self, i: usize, x: f32, y: f32) -> Pos {
        let s = &self.spans[i];
        // Clamped into its visible part, so a point beside it means the line beside it.
        let px = x.clamp(s.area.left, s.area.right.max(s.area.left));
        let py = y.clamp(s.area.top, (s.area.bottom - 1.0).max(s.area.top));
        let (mut trailing, mut inside) = (windows::core::BOOL(0), windows::core::BOOL(0));
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            let _ = s.layout.HitTestPoint(px - s.x, py - s.y, &mut trailing, &mut inside, &mut m);
        }
        let at = m.textPosition + if trailing.as_bool() { m.length } else { 0 };
        // Off the side of the text: its start or end.
        let at = if x < s.area.left && y < s.area.top + 1.0 { 0 } else if x > s.area.right && y >= s.area.bottom - 1.0 { s.units } else { at };
        (i, at.min(s.units))
    }

    pub fn over(&self, x: f32, y: f32) -> bool {
        self.under(x, y).is_some()
    }

    /// A press on text: start selecting there, or with `word`, select the word there. False if
    /// there's no text there.
    pub fn start(&mut self, x: f32, y: f32, word: bool) -> bool {
        let Some(i) = self.under(x, y) else { return false };
        let p = self.position(i, x, y);
        if word {
            let (a, b) = crate::doc::word_at(&self.spans[i].text, p.1);
            self.word = Some(((i, a), (i, b)));
            self.sel = Some(((i, a), (i, b)));
        } else {
            self.word = None;
            self.sel = Some((p, p));
        }
        true
    }

    /// The mouse moved while selecting.
    pub fn drag_to(&mut self, x: f32, y: f32) {
        let Some(i) = self.under(x, y).or_else(|| self.nearest(x, y)) else { return };
        let p = self.position(i, x, y);
        self.sel = match (self.word, self.sel) {
            (Some((a, b)), _) => {
                // A word at a time, keeping the first word selected.
                let (wa, wb) = crate::doc::word_at(&self.spans[i].text, p.1);
                if p < a { Some((b, (i, wa))) } else { Some((a, (i, wb.max(p.1)))) }
            }
            (None, Some((a, _))) => Some((a, p)),
            (None, None) => None,
        };
    }

    /// The button's up: a click that selected nothing leaves nothing selected.
    pub fn finish(&mut self) {
        if !self.has_selection() {
            self.sel = None;
        }
        self.word = None;
    }

    pub fn has_selection(&self) -> bool {
        self.sel.is_some_and(|(a, b)| a != b)
    }

    /// Drop the selection. True if there was one.
    pub fn clear(&mut self) -> bool {
        let had = self.has_selection();
        self.sel = None;
        self.word = None;
        had
    }

    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.sel.filter(|(a, b)| a != b)?;
        let (lo, hi) = (a.min(b), a.max(b));
        let mut out = String::new();
        for i in lo.0..=hi.0 {
            let s = self.spans.get(i)?;
            let Some((from, to)) = self.range_in(i, s.units) else { continue };
            let units: Vec<u16> = s.text.encode_utf16().collect();
            if !out.is_empty() {
                out.push_str(match s.join {
                    Join::Line => "\r\n",
                    Join::Tab => "\t",
                });
            }
            out.push_str(&String::from_utf16_lossy(&units[from as usize..to as usize]));
        }
        (!out.is_empty()).then_some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{Align, Text};

    /// Two rows of the information panel: a label and its value on each.
    fn panel() -> (Picker, Vec<(f32, f32)>) {
        let text = Text::new().unwrap();
        let f = text.format(16.0, 400, Align::Left, true).unwrap();
        let mut p = Picker::default();
        p.begin();
        let mut at = Vec::new();
        for (i, (s, x, join)) in [("Size", 10.0, Join::Line), ("2.1 MB", 110.0, Join::Tab), ("Folder", 10.0, Join::Line), ("C:\\Photos", 110.0, Join::Tab)].into_iter().enumerate() {
            let y = 10.0 + (i / 2) as f32 * 30.0;
            let l = text.layout(s, &f, 300.0, 1000.0).unwrap();
            assert!(p.add(&l, x, y, s, None, join).is_empty());
            at.push((x, y));
        }
        (p, at)
    }

    #[test]
    fn a_drag_across_rows_copies_them_as_lines_of_label_and_value() {
        let (mut p, at) = panel();
        assert!(!p.start(500.0, 500.0, false), "nothing there");
        assert!(p.start(at[0].0 + 1.0, at[0].1 + 8.0, false));
        assert!(!p.has_selection());
        // Past the end of the last value, and down off it.
        p.drag_to(400.0, at[3].1 + 8.0);
        assert_eq!(p.selected_text().as_deref(), Some("Size\t2.1 MB\r\nFolder\tC:\\Photos"));
        p.finish();
        assert!(p.has_selection());
        assert!(p.clear());
        assert_eq!(p.selected_text(), None);
    }

    #[test]
    fn a_double_click_picks_a_word_and_the_selection_survives_the_next_frame() {
        let (mut p, at) = panel();
        // "MB", in "2.1 MB".
        assert!(p.start(at[1].0 + 36.0, at[1].1 + 8.0, true));
        assert_eq!(p.selected_text().as_deref(), Some("MB"));
        p.finish();
        // The same text drawn again: still selected, and painted.
        let text = Text::new().unwrap();
        let f = text.format(16.0, 400, Align::Left, true).unwrap();
        p.begin();
        let mut marks = 0;
        for (s, x, y) in [("Size", 10.0, 10.0), ("2.1 MB", 110.0, 10.0), ("Folder", 10.0, 40.0), ("C:\\Photos", 110.0, 40.0)] {
            marks += p.add(&text.layout(s, &f, 300.0, 1000.0).unwrap(), x, y, s, None, Join::Line).len();
        }
        assert_eq!(marks, 1);
        assert_eq!(p.selected_text().as_deref(), Some("MB"));
        // Different text in its place: the selection goes.
        p.begin();
        p.add(&text.layout("Other", &f, 300.0, 1000.0).unwrap(), 10.0, 10.0, "Other", None, Join::Line);
        p.add(&text.layout("3 MB", &f, 300.0, 1000.0).unwrap(), 110.0, 10.0, "3 MB", None, Join::Tab);
        assert!(!p.has_selection());
    }

    #[test]
    fn text_scrolled_out_of_sight_cannot_be_picked() {
        let text = Text::new().unwrap();
        let f = text.format(16.0, 400, Align::Left, true).unwrap();
        let mut p = Picker::default();
        p.begin();
        let clip = rect(0.0, 100.0, 300.0, 100.0);
        p.add(&text.layout("hidden", &f, 300.0, 1000.0).unwrap(), 10.0, 20.0, "hidden", Some(clip), Join::Line);
        assert!(!p.over(15.0, 28.0));
        assert!(!p.start(15.0, 28.0, false));
    }
}
