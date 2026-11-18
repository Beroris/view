//! Documents in the Space preview: text, code, Markdown, tables and PDFs, in a column that
//! scrolls. Files are read, decoded and coloured off the interface thread; laying the text out
//! with DirectWrite happens here, and again whenever the width or the theme changes. Long
//! things are cut into pieces so only what's on screen is drawn.

use crate::gfx::{Gpu, Texture, rect};
use crate::pdf::{self, Pdf};
use crate::text::{Align, Text};
use crate::theme::{Palette, Rgb};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use view_core::kind::{DocKind, Kind, Lang, TextKind};
use view_core::markdown::{self, Block, Run};
use view_core::picture::Bitmap;
use view_core::slides::{self, Deck, ItemKind, Paint};
use view_core::zip::Tree;
use view_core::pdftext::{Break, Glyph, PageText};
use view_core::textfile::{self, Span, Token};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::Interface;
use windows_numerics::Matrix3x2;

/// Posted when a document has finished loading, or a PDF page is ready.
pub const WM_DOC: u32 = WM_APP + 5;

/// The space at the top for the name over it, and round the sides, in DIPs.
const PAD_TOP: f32 = 56.0;
const PAD_X: f32 = 28.0;
const PAD_BOTTOM: f32 = 28.0;
/// Lines of code in one laid-out piece.
const CHUNK: usize = 300;
/// Rows a table shows.
const MAX_ROWS: usize = 5000;
const MAX_COLS: usize = 64;
/// A zip's lines, and the summary and column names over them, in DIPs.
const ROW_H: f32 = 30.0;
const LIST_HEAD: f32 = 52.0;
/// The longest side a slide's picture is kept at.
const PICTURE_MAX: u32 = 1600;

/// The window size a document suits.
#[derive(Clone, Copy)]
pub enum Suits {
    /// Slides, this many times as tall as they're wide.
    Slides(f32),
    /// A list this tall, in DIPs.
    List(f32),
}

pub enum Source {
    Plain(String),
    Code { text: String, spans: Vec<Span>, numbers: bool },
    Markdown(Vec<Block>),
    Table(Vec<Vec<String>>),
    /// PowerPoint slides, with their pictures decoded.
    Slides { deck: Deck, pictures: Vec<Option<Bitmap>> },
    /// What's in a zip.
    Archive(Tree),
}

struct Loaded {
    source: Result<Source, String>,
    note: Option<String>,
}

enum State {
    Loading(Receiver<Loaded>),
    Text(Source),
    PdfLoading(Pdf),
    Pdf { pdf: Pdf, pages: Vec<(f32, f32)>, textures: HashMap<usize, (u32, Texture)>, asked: HashSet<(usize, u32)>, text: PdfText },
    Failed(String),
}

/// The text of a PDF's pages, read as they come into view. A selection's `Pos` is a page and a
/// character on it.
#[derive(Default)]
struct PdfText {
    pages: HashMap<usize, PageText>,
    asked: HashSet<usize>,
    /// The text couldn't be read, so there's nothing to select.
    none: bool,
}

pub struct Doc {
    pub path: PathBuf,
    state: State,
    /// "Showing the first 20,000 lines", when the file is longer than that.
    pub note: Option<String>,
    pub scroll: f32,
    pub scroll_x: f32,
    /// Where scrolling is gliding to, down and across. `scroll` follows it.
    target: (f32, f32),
    /// When scrolling last moved, for the glide's timing.
    stepped: Instant,
    /// When a selection being dragged past an edge last scrolled the document.
    followed: Instant,
    /// Selected text, from where the drag started to where it is: piece and UTF-16 position.
    selection: Option<(Pos, Pos)>,
    /// The visible size last drawn at, for keeping scrolling in bounds.
    view: (f32, f32),
    laid: Option<Laid>,
    /// Where to scroll to once it's laid out, as shares of its height and width: where it
    /// was left the last time it was open.
    resume: Option<(f32, f32)>,
    /// Ctrl+F: what's being looked for, and where it's been found.
    finding: Option<Finding>,
    /// Slides' pictures on the GPU, by their index in the deck.
    textures: HashMap<usize, Texture>,
    /// A zip's folders that are open, and the lines of it shown, by index in its tree.
    open: HashSet<usize>,
    rows: Vec<usize>,
    /// Where the mouse is, in the window, for lighting up the line under it.
    pub hover: Option<(f32, f32)>,
    /// Where the document's box starts in the window.
    origin: (f32, f32),
}

/// The most matches kept, and the most of a PDF's characters held in memory for finding.
const MAX_HITS: usize = 5000;
const FIND_GLYPHS: usize = 1_000_000;
/// PDF pages asked for at once while looking through them, so pages being scrolled to still
/// come first.
const FIND_AHEAD: usize = 3;

/// A match: the piece of text (or PDF page) it's in, and where in it, as a selection would be.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Found {
    piece: usize,
    from: u32,
    to: u32,
}

#[derive(Default)]
struct Finding {
    query: Vec<char>,
    /// In order through the document.
    hits: Vec<Found>,
    current: Option<Found>,
    /// The matches need working out again: the words or the layout changed.
    stale: bool,
    /// Go to the first match after where the document is scrolled, once there is one.
    seek: bool,
    /// PDF pages looked through, asked for, and held on to.
    searched: HashSet<usize>,
    waiting: HashSet<usize>,
    held: HashSet<usize>,
    held_glyphs: usize,
}

impl Doc {
    pub fn open(path: &Path, kind: Kind, hwnd: HWND) -> Doc {
        let state = match kind {
            Kind::Pdf => State::PdfLoading(pdf::open(path.to_path_buf(), hwnd, WM_DOC)),
            Kind::Text(_) | Kind::Document(_) => {
                let (send, recv) = channel();
                let p = path.to_path_buf();
                let hwnd = hwnd.0 as isize;
                let _ = std::thread::Builder::new().name("text".into()).spawn(move || {
                    let loaded = match kind {
                        Kind::Text(t) => load_text(&p, t),
                        Kind::Document(d) => load_document(&p, d),
                        _ => unreachable!(),
                    };
                    if send.send(loaded).is_ok() {
                        unsafe {
                            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_DOC, WPARAM(0), LPARAM(0));
                        }
                    }
                });
                State::Loading(recv)
            }
            _ => State::Failed("View can't show this file.".into()),
        };
        Doc { path: path.to_path_buf(), state, note: None, scroll: 0.0, scroll_x: 0.0, target: (0.0, 0.0), stepped: Instant::now(), followed: Instant::now(), selection: None, view: (0.0, 0.0), laid: None, resume: None, finding: None, textures: HashMap::new(), open: HashSet::new(), rows: Vec::new(), hover: None, origin: (0.0, 0.0) }
    }

    /// Take what the loading threads have finished. True if the document changed shape (it
    /// opened, or failed), so the window may want a new size.
    pub fn poll(&mut self, gpu: Option<&Gpu>) -> bool {
        match &mut self.state {
            State::Loading(rx) => match rx.try_recv() {
                Ok(l) => {
                    self.note = l.note;
                    self.state = match l.source {
                        Ok(s) => State::Text(s),
                        Err(e) => State::Failed(e),
                    };
                    // A zip that's all one folder opens with that folder open.
                    if let State::Text(Source::Archive(tree)) = &self.state {
                        let tops: Vec<usize> = tree.nodes.iter().enumerate().filter(|(_, n)| n.depth == 0).map(|(i, _)| i).collect();
                        if let [only] = tops[..] {
                            if tree.nodes[only].folder {
                                self.open.insert(only);
                            }
                        }
                        self.rows = shown_rows(tree, &self.open);
                    }
                    self.laid = None;
                    true
                }
                Err(TryRecvError::Disconnected) => {
                    self.state = State::Failed("View couldn't read this file.".into());
                    true
                }
                Err(TryRecvError::Empty) => false,
            },
            State::PdfLoading(pdf) => {
                let mut opened = None;
                while let Ok(m) = pdf.results.try_recv() {
                    match m {
                        pdf::Msg::Opened(pages) => opened = Some(Ok(pages)),
                        pdf::Msg::Failed(e) => opened = Some(Err(e)),
                        _ => {}
                    }
                }
                let Some(result) = opened else { return false };
                let State::PdfLoading(pdf) = std::mem::replace(&mut self.state, State::Failed(String::new())) else { unreachable!() };
                self.state = match result {
                    Ok(pages) => {
                        if pages.len() >= pdf::MAX_PAGES {
                            self.note = Some(format!("Showing the first {} pages", pdf::MAX_PAGES));
                        }
                        State::Pdf { pdf, pages, textures: HashMap::new(), asked: HashSet::new(), text: PdfText::default() }
                    }
                    Err(e) => State::Failed(e),
                };
                true
            }
            State::Pdf { pdf, textures, text, .. } => {
                let finding = &mut self.finding;
                while let Ok(m) = pdf.results.try_recv() {
                    match m {
                        pdf::Msg::Page { index, width, bitmap } => {
                            if let Some(t) = gpu.and_then(|g| g.upload(&bitmap)) {
                                textures.insert(index, (width, t));
                            }
                        }
                        pdf::Msg::Text { index, text: page } => {
                            if let Some(f) = finding {
                                f.waiting.remove(&index);
                                if !f.stale && f.searched.insert(index) {
                                    let hits = pdf_page_hits(&page, index, &f.query);
                                    f.merge(index, hits);
                                }
                                if !f.held.contains(&index) && f.held_glyphs + page.glyphs.len() <= FIND_GLYPHS {
                                    f.held.insert(index);
                                    f.held_glyphs += page.glyphs.len();
                                }
                            }
                            text.pages.insert(index, page);
                        }
                        pdf::Msg::NoText => text.none = true,
                        _ => {}
                    }
                }
                false
            }
            _ => false,
        }
    }

    pub fn failed(&self) -> Option<&str> {
        match &self.state {
            State::Failed(e) => Some(e),
            _ => None,
        }
    }

    pub fn is_pdf(&self) -> bool {
        matches!(self.state, State::Pdf { .. } | State::PdfLoading(_))
    }

    pub fn is_table(&self) -> bool {
        matches!(self.state, State::Text(Source::Table(_)))
    }

    /// The window size that suits it, in DIPs, for slides and zips: slides by their shape
    /// (height over width), a zip by how many lines it shows.
    pub fn suits(&self) -> Option<Suits> {
        match &self.state {
            State::Text(Source::Slides { deck, .. }) => Some(Suits::Slides(deck.height / deck.width.max(1.0))),
            State::Text(Source::Archive(tree)) => {
                // Room for what's inside, so opening a folder doesn't leave it cramped.
                let lines = tree.nodes.len().clamp(4, 14) as f32;
                Some(Suits::List((PAD_TOP + LIST_HEAD + lines * ROW_H + PAD_BOTTOM).clamp(260.0, 640.0)))
            }
            _ => None,
        }
    }

    /// A press on a zip's folder opens or closes it. True if it did.
    pub fn click(&mut self, x: f32, y: f32) -> bool {
        let Some(i) = self.row_at(x, y) else { return false };
        let State::Text(Source::Archive(tree)) = &self.state else { return false };
        if !tree.nodes[i].folder {
            return false;
        }
        if !self.open.remove(&i) {
            self.open.insert(i);
        }
        self.rows = shown_rows(tree, &self.open);
        if let Some(laid) = &mut self.laid {
            laid.height = laid.rows_top + self.rows.len() as f32 * laid.row_h + PAD_BOTTOM * laid.scale;
        }
        self.clamp_scroll();
        true
    }

    /// The zip folder under a point in the window, if there's one: for the hand cursor.
    pub fn folder_at(&self, x: f32, y: f32) -> bool {
        let State::Text(Source::Archive(tree)) = &self.state else { return false };
        self.row_at(x, y).is_some_and(|i| tree.nodes[i].folder)
    }

    /// The line of a zip under a point in the window: its index in the tree.
    fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        let laid = self.laid.as_ref().filter(|l| l.row_h > 0.0)?;
        let (dx, dy) = (x - self.origin.0, y - self.origin.1);
        if dx < 0.0 || dx > self.view.0 || dy < 0.0 || dy > self.view.1 {
            return None;
        }
        let at = dy + self.scroll - laid.rows_top;
        if at < 0.0 {
            return None;
        }
        self.rows.get((at / laid.row_h) as usize).copied()
    }

    /// A PDF's first page size, in DIPs, for sizing the window to it.
    pub fn first_page(&self) -> Option<(f32, f32)> {
        match &self.state {
            State::Pdf { pages, .. } => pages.first().copied(),
            _ => None,
        }
    }

    /// "Page 3 of 12" for a PDF, from the page in the middle of the window.
    pub fn page_label(&self) -> Option<String> {
        let laid = self.laid.as_ref()?;
        if let State::Text(Source::Slides { deck, .. }) = &self.state {
            let mid = self.scroll + self.view.1 / 2.0;
            let i = laid.slides.iter().position(|r| mid < r.bottom + 8.0 * laid.scale).unwrap_or(deck.slides.len().saturating_sub(1));
            return (deck.slides.len() > 1).then(|| format!("Slide {} of {}", i + 1, deck.slides.len()));
        }
        let State::Pdf { pages, .. } = &self.state else { return None };
        let mid = self.scroll + self.view.1 / 2.0;
        let i = laid.pages.iter().position(|r| mid < r.bottom).unwrap_or(pages.len().saturating_sub(1));
        Some(format!("Page {} of {}", i + 1, pages.len()))
    }

    /// Forget the layout, after the theme, the size of text or the GPU changed.
    pub fn reset(&mut self, device_lost: bool) {
        self.laid = None;
        if device_lost {
            self.textures.clear();
            if let State::Pdf { textures, asked, .. } = &mut self.state {
                textures.clear();
                asked.clear();
            }
        }
    }

    /// Scroll by this much, gliding there over the next few frames.
    pub fn scroll_by(&mut self, dy: f32, dx: f32) {
        let Some(laid) = &self.laid else { return };
        if !self.scrolling() {
            self.stepped = Instant::now();
        }
        let (my, mx) = ((laid.height - self.view.1).max(0.0), (laid.wide - self.view.0).max(0.0));
        self.target = ((self.target.0 + dy).clamp(0.0, my), (self.target.1 + dx).clamp(0.0, mx));
    }

    /// Scroll at once, without gliding: for following a drag.
    pub fn jump_by(&mut self, dy: f32) {
        self.scroll_by(dy, 0.0);
        self.scroll = self.target.0;
    }

    /// Called every frame, before drawing: `over` is how far past the top or bottom edge band
    /// a selection is being dragged (negative above). Scrolls towards it, faster the further
    /// out it goes, moves any glide from the wheel on, and says whether the text moved, so
    /// the end of a selection can stay under the mouse.
    pub fn follow(&mut self, over: f32, s: f32) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.followed).as_secs_f32().min(0.05);
        self.followed = now;
        let before = (self.scroll, self.scroll_x);
        self.step();
        if over != 0.0 {
            let speed = over.signum() * (over.abs() * 14.0).min(4000.0 * s);
            self.jump_by(speed * dt);
        }
        (self.scroll, self.scroll_x) != before
    }

    /// How far through it's scrolled, as shares of its height and width, for coming back to.
    pub fn place(&self) -> Option<(f32, f32)> {
        if self.resume.is_some() {
            return self.resume;
        }
        if !matches!(self.state, State::Text(_) | State::Pdf { .. }) {
            return None;
        }
        let laid = self.laid.as_ref().filter(|l| l.height > 0.0)?;
        Some((self.target.0 / laid.height, self.target.1 / laid.wide.max(1.0)))
    }

    /// Scroll to `place` (from [`Doc::place`]) once it's laid out.
    pub fn resume_at(&mut self, place: (f32, f32)) {
        self.resume = Some(place);
    }

    // ---- Finding and pages ----

    /// Words can be looked for in it: what can be selected can be searched.
    pub fn searchable(&self) -> bool {
        matches!(self.state, State::Loading(_) | State::PdfLoading(_)) || self.selectable()
    }

    /// Look for `typed`, going to the first match after where the document is scrolled.
    pub fn find(&mut self, typed: &str) {
        let query = view_core::find::query(typed);
        let f = self.finding.get_or_insert_with(Finding::default);
        if f.query == query && !f.query.is_empty() {
            return;
        }
        f.query = query;
        f.hits.clear();
        f.current = None;
        f.searched.clear();
        f.stale = true;
        f.seek = true;
    }

    /// Stop finding: the matches go, and PDF text held for it can be let go.
    pub fn end_find(&mut self) {
        self.finding = None;
    }

    /// Which match is the current one (counting from 1, if one is), how many there are, and
    /// whether a PDF is still being looked through.
    pub fn find_count(&self) -> Option<(Option<usize>, usize, bool)> {
        let f = self.finding.as_ref()?;
        let at = f.current.and_then(|c| f.hits.binary_search(&c).ok()).map(|i| i + 1);
        let searching = match &self.state {
            State::Pdf { pages, text, .. } => !f.query.is_empty() && !text.none && f.searched.len() < pages.len() && f.hits.len() < MAX_HITS,
            State::PdfLoading(_) | State::Loading(_) => !f.query.is_empty(),
            _ => false,
        };
        Some((at, f.hits.len(), searching))
    }

    /// Go to the next match, or the one before.
    pub fn find_step(&mut self, forward: bool) {
        let Some(f) = &self.finding else { return };
        if f.hits.is_empty() {
            return;
        }
        let n = f.hits.len();
        let next = match f.current.map(|c| f.hits.binary_search(&c)) {
            Some(Ok(i)) => if forward { (i + 1) % n } else { (i + n - 1) % n },
            Some(Err(i)) => if forward { i % n } else { (i + n - 1) % n },
            None => {
                let after = self.first_hit_in_view().unwrap_or(0);
                if forward { after } else { (after + n - 1) % n }
            }
        };
        let hit = f.hits[next];
        if let Some(f) = &mut self.finding {
            f.current = Some(hit);
            f.seek = false;
        }
        self.show_hit(hit);
    }

    /// The first match from the top of the window down.
    fn first_hit_in_view(&self) -> Option<usize> {
        let f = self.finding.as_ref()?;
        let top = self.scroll;
        f.hits.iter().position(|&h| self.hit_rect(h).is_some_and(|r| r.bottom > top))
    }

    /// Where a match is, in the document's own coordinates. A PDF page whose text isn't held
    /// gives the page itself.
    fn hit_rect(&self, h: Found) -> Option<D2D_RECT_F> {
        let laid = self.laid.as_ref()?;
        if let State::Pdf { text, .. } = &self.state {
            let pr = laid.pages.get(h.piece)?;
            let Some(page) = text.pages.get(&h.piece) else { return Some(D2D_RECT_F { bottom: pr.top + (pr.bottom - pr.top) * 0.25, ..*pr }) };
            return pdf_bands(pr, page, h.from as usize, h.to as usize).into_iter().reduce(union);
        }
        let Piece::Text { layout, x, y, .. } = laid.pieces.get(h.piece)? else { return None };
        range_rects(layout, h.from, h.to).into_iter().map(|m| rect(x + m.left, y + m.top, m.width, m.height)).reduce(union)
    }

    /// Scroll so a match is in the middle of the window, unless it's well in view already.
    fn show_hit(&mut self, h: Found) {
        let Some(r) = self.hit_rect(h) else { return };
        let (w, vh) = self.view;
        let margin = vh * 0.2;
        let mut to = self.target;
        if r.top < self.scroll + margin || r.bottom > self.scroll + vh - margin {
            to.0 = (r.top + r.bottom) / 2.0 - vh / 2.0;
        }
        if r.left < self.scroll_x || r.right > self.scroll_x + w {
            to.1 = (r.left + r.right) / 2.0 - w / 2.0;
        }
        self.glide_to(to);
    }

    fn glide_to(&mut self, to: (f32, f32)) {
        if !self.scrolling() {
            self.stepped = Instant::now();
        }
        let Some(laid) = &self.laid else { return };
        let (my, mx) = ((laid.height - self.view.1).max(0.0), (laid.wide - self.view.0).max(0.0));
        self.target = (to.0.clamp(0.0, my), to.1.clamp(0.0, mx));
    }

    /// Work out what finding needs before drawing: the matches, if the words or the layout
    /// changed, and the first one to go to.
    fn refresh_find(&mut self) {
        let Some(f) = &mut self.finding else { return };
        if f.stale {
            match &self.state {
                State::Text(_) => {
                    let Some(laid) = &self.laid else { return };
                    let mut hits = Vec::new();
                    if !f.query.is_empty() {
                        for (i, piece) in laid.pieces.iter().enumerate() {
                            let Piece::Text { copy: Some(copy), .. } = piece else { continue };
                            let mut h = view_core::find::Haystack::new();
                            h.push_units(copy);
                            hits.extend(h.find(&f.query, MAX_HITS - hits.len()).into_iter().map(|(from, to)| Found { piece: i, from, to }));
                            if hits.len() >= MAX_HITS {
                                break;
                            }
                        }
                    }
                    f.hits = hits;
                }
                State::Pdf { text, .. } => {
                    f.hits.clear();
                    if !f.query.is_empty() {
                        let mut pages: Vec<usize> = text.pages.keys().copied().collect();
                        pages.sort_unstable();
                        for i in pages {
                            let hits = pdf_page_hits(&text.pages[&i], i, &f.query);
                            f.merge(i, hits);
                            f.searched.insert(i);
                        }
                    }
                }
                _ => return,
            }
            f.stale = false;
            if let Some(c) = f.current {
                if f.hits.binary_search(&c).is_err() {
                    f.current = None;
                }
            }
        }
        if f.seek && !f.hits.is_empty() {
            f.seek = false;
            if let Some(i) = self.first_hit_in_view() {
                let hit = self.finding.as_ref().map(|f| f.hits[i]);
                if let (Some(f), Some(hit)) = (&mut self.finding, hit) {
                    f.current = Some(hit);
                    self.show_hit(hit);
                }
            }
        }
    }

    /// How many pages a PDF has.
    pub fn page_count(&self) -> Option<usize> {
        match &self.state {
            State::Pdf { pages, .. } => Some(pages.len()),
            _ => None,
        }
    }

    /// Scroll to the top of page `n`, counting from 1.
    pub fn go_to_page(&mut self, n: usize) {
        let Some(laid) = &self.laid else { return };
        let Some(r) = laid.pages.get(n.saturating_sub(1)) else { return };
        let top = (r.top - 12.0 * laid.scale).max(0.0);
        let x = self.target.1;
        self.glide_to((top, x));
    }

    /// There's more of the document below (or above) to scroll to.
    pub fn can_scroll(&self, down: bool) -> bool {
        let Some(laid) = &self.laid else { return false };
        if down { self.scroll < laid.height - self.view.1 - 0.5 } else { self.scroll > 0.5 }
    }

    /// Still gliding towards where scrolling is going.
    pub fn scrolling(&self) -> bool {
        (self.target.0 - self.scroll).abs() > 0.25 || (self.target.1 - self.scroll_x).abs() > 0.25
    }

    /// Move the glide on to now. Each frame covers the same share of what's left, so it starts
    /// quick and settles gently, and a new turn of the wheel just moves where it's heading.
    fn step(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.stepped).as_secs_f32().min(0.05);
        self.stepped = now;
        let k = 1.0 - (-dt * 18.0).exp();
        for (at, to) in [(&mut self.scroll, self.target.0), (&mut self.scroll_x, self.target.1)] {
            *at += (to - *at) * k;
            if (to - *at).abs() <= 0.25 {
                *at = to;
            }
        }
    }

    /// Keep scrolling inside the document, after its size changed.
    fn clamp_scroll(&mut self) {
        let Some(laid) = &self.laid else { return };
        let (my, mx) = ((laid.height - self.view.1).max(0.0), (laid.wide - self.view.0).max(0.0));
        self.target = (self.target.0.clamp(0.0, my), self.target.1.clamp(0.0, mx));
        self.scroll = self.scroll.clamp(0.0, my);
        self.scroll_x = self.scroll_x.clamp(0.0, mx);
    }

    // ---- Selecting text ----

    /// Text can be selected: everything but spreadsheets, and PDFs whose text can't be read.
    pub fn selectable(&self) -> bool {
        match &self.state {
            State::Text(Source::Plain(_) | Source::Code { .. } | Source::Markdown(_) | Source::Slides { .. }) => true,
            State::Pdf { text, .. } => !text.none,
            _ => false,
        }
    }

    /// There's text right under this point: for the text cursor, and for a double-click
    /// picking a word rather than opening the file. Text files are text everywhere; PDF pages
    /// only where they have characters.
    pub fn text_at(&self, x: f32, y: f32) -> bool {
        let State::Pdf { text, .. } = &self.state else { return self.selectable() };
        let Some(laid) = self.laid.as_ref() else { return false };
        let (x, y) = (x + self.scroll_x, y + self.scroll);
        let Some(i) = laid.pages.iter().position(|r| y >= r.top && y < r.bottom) else { return false };
        let Some(page) = text.pages.get(&i) else { return false };
        let m = 2.0 * laid.scale;
        page.glyphs.iter().any(|g| {
            let r = glyph_rect(&laid.pages[i], g);
            x >= r.left - m && x < r.right + m && y >= r.top && y < r.bottom
        })
    }

    /// The character nearest a point on a PDF: its page, its index, and whether the point is
    /// past its middle, along the way its text runs. Nearness counts across a line four times
    /// over along it, so a point beside a line finds that line.
    fn pdf_hit(&self, x: f32, y: f32) -> Option<(usize, usize, bool)> {
        let State::Pdf { text, .. } = &self.state else { return None };
        let laid = self.laid.as_ref()?;
        let (x, y) = (x + self.scroll_x, y + self.scroll);
        let away = |r: &D2D_RECT_F| if y < r.top { r.top - y } else if y > r.bottom { y - r.bottom } else { 0.0 };
        let (i, pr) = laid.pages.iter().enumerate().min_by(|a, b| away(a.1).total_cmp(&away(b.1)))?;
        let page = text.pages.get(&i)?;
        let mut best: Option<(usize, f32)> = None;
        for (k, g) in page.glyphs.iter().enumerate() {
            let r = glyph_rect(pr, g);
            let dx = if x < r.left { r.left - x } else if x > r.right { x - r.right } else { 0.0 };
            let dy = if y < r.top { r.top - y } else if y > r.bottom { y - r.bottom } else { 0.0 };
            let [ux, uy] = g.dir.map(f32::abs);
            let d = (dx * uy + dy * ux) * 4.0 + dx * ux + dy * uy;
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((k, d));
            }
        }
        let (k, _) = best?;
        let g = &page.glyphs[k];
        let r = glyph_rect(pr, g);
        let past = (x - (r.left + r.right) / 2.0) * g.dir[0] + (y - (r.top + r.bottom) / 2.0) * g.dir[1];
        Some((i, k, past > 0.0))
    }

    /// Where in the text a point in the window falls: the nearest piece of text on its line,
    /// or the start of the next one below.
    fn hit(&self, x: f32, y: f32) -> Option<Pos> {
        if let State::Pdf { .. } = self.state {
            return self.pdf_hit(x, y).map(|(page, k, after)| Pos { piece: page, at: (k + after as usize) as u32 });
        }
        let laid = self.laid.as_ref()?;
        let (x, y) = (x + self.scroll_x, y + self.scroll);
        let mut best: Option<(usize, f32)> = None;
        let mut below: Option<(usize, f32)> = None;
        let mut last = None;
        for (i, piece) in laid.pieces.iter().enumerate() {
            let Piece::Text { layout, x: px, y: py, h, copy: Some(_), .. } = piece else { continue };
            last = Some(i);
            if y >= *py && y < py + h {
                let w = Text::size(layout).0.max(1.0);
                let dx = if x < *px { px - x } else if x > px + w { x - px - w } else { 0.0 };
                if best.is_none_or(|(_, d)| dx < d) {
                    best = Some((i, dx));
                }
            } else if *py > y && below.is_none_or(|(_, by)| *py < by) {
                below = Some((i, *py));
            }
        }
        let (i, at_start) = match (best, below) {
            (Some((i, _)), _) => (i, None),
            (None, Some((i, _))) => (i, Some(true)),
            (None, None) => (last?, Some(false)),
        };
        let Piece::Text { layout, x: px, y: py, copy: Some(copy), .. } = &laid.pieces[i] else { return None };
        let len = copy.encode_utf16().count() as u32;
        let pos = match at_start {
            Some(true) => 0,
            Some(false) => len,
            None => {
                let (mut trailing, mut inside) = (windows::core::BOOL(0), windows::core::BOOL(0));
                let mut m = DWRITE_HIT_TEST_METRICS::default();
                unsafe {
                    let _ = layout.HitTestPoint(x - px, y - py, &mut trailing, &mut inside, &mut m);
                }
                (m.textPosition + if trailing.as_bool() { m.length } else { 0 }).min(len)
            }
        };
        Some(Pos { piece: i, at: pos })
    }

    /// Start selecting at a point; a double-click selects the word there.
    pub fn select_start(&mut self, x: f32, y: f32, word: bool) {
        self.selection = self.hit(x, y).map(|p| (p, p));
        if let (true, State::Pdf { text, .. }) = (word, &self.state) {
            let words = self.pdf_hit(x, y).and_then(|(page, k, _)| {
                let (a, b) = text.pages.get(&page)?.word(k);
                Some((Pos { piece: page, at: a as u32 }, Pos { piece: page, at: b as u32 }))
            });
            self.selection = words;
            return;
        }
        if word {
            if let Some((p, _)) = self.selection {
                if let Some(Piece::Text { copy: Some(copy), .. }) = self.laid.as_ref().and_then(|l| l.pieces.get(p.piece)) {
                    let (a, b) = word_at(copy, p.at);
                    self.selection = Some((Pos { piece: p.piece, at: a }, Pos { piece: p.piece, at: b }));
                }
            }
        }
    }

    pub fn select_to(&mut self, x: f32, y: f32) {
        let hit = self.hit(x, y);
        if let (Some((_, end)), Some(p)) = (&mut self.selection, hit) {
            *end = p;
        }
    }

    /// Something is selected, rather than just clicked on.
    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|(a, b)| a != b)
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// The selection in order: first, then last.
    fn ordered(&self) -> Option<(Pos, Pos)> {
        let (a, b) = self.selection?;
        Some(if (a.piece, a.at) <= (b.piece, b.at) { (a, b) } else { (b, a) })
    }

    /// The selected text, as it reads: pieces on the same line joined by tabs (table cells),
    /// the rest by line breaks.
    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.ordered().filter(|(a, b)| a != b)?;
        if let State::Pdf { text, .. } = &self.state {
            let mut out = String::new();
            for page in a.piece..=b.piece {
                let Some(t) = text.pages.get(&page) else { continue };
                let from = if page == a.piece { a.at as usize } else { 0 };
                let to = if page == b.piece { b.at as usize } else { t.glyphs.len() };
                if from < to {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&t.copy(from, to));
                }
            }
            return (!out.is_empty()).then_some(out);
        }
        let laid = self.laid.as_ref()?;
        let mut out = String::new();
        let mut prev_y: Option<f32> = None;
        for i in a.piece..=b.piece {
            let Some(Piece::Text { y, copy: Some(copy), .. }) = laid.pieces.get(i) else { continue };
            let units: Vec<u16> = copy.encode_utf16().collect();
            let from = if i == a.piece { a.at as usize } else { 0 }.min(units.len());
            let to = if i == b.piece { b.at as usize } else { units.len() }.min(units.len());
            if let Some(py) = prev_y {
                out.push(if (py - y).abs() < 1.0 { '\t' } else { '\n' });
            }
            out.push_str(&String::from_utf16_lossy(&units[from..to.max(from)]));
            prev_y = Some(*y);
        }
        Some(out)
    }

    pub fn can_scroll_sideways(&self) -> bool {
        self.laid.as_ref().is_some_and(|l| l.wide > self.view.0 + 1.0)
    }

    /// Draw into `r`, which the document fills and scrolls in.
    pub fn draw(&mut self, gpu: &mut Gpu, text: &Text, p: &Palette, r: D2D_RECT_F, s: f32, alpha: f32) {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        self.view = (w, h);
        self.origin = (r.left, r.top);
        let fresh = self.laid.as_ref().is_some_and(|l| (l.width - w).abs() < 0.5 && l.scale == s && l.dark == p.dark);
        if !fresh {
            // Laid out again at a new size: stay the same share of the way down.
            let share = self.laid.as_ref().filter(|l| l.height > 0.0).map(|l| (self.target.0 / l.height, self.target.1 / l.wide.max(1.0)));
            self.laid = self.lay_out(gpu, text, p, w, s);
            if let (Some((fy, fx)), Some(laid), None) = (share, &self.laid, self.resume) {
                self.target = (fy * laid.height, fx * laid.wide);
                self.scroll = self.target.0;
                self.scroll_x = self.target.1;
            }
            self.clamp_scroll();
            // A text's pieces are new: its matches are worked out again.
            if let (Some(f), State::Text(_)) = (&mut self.finding, &self.state) {
                f.stale = true;
            }
        }
        // Back where it was left, once there's a layout of the real thing to scroll.
        if let (Some(laid), Some((fy, fx))) = (&self.laid, self.resume) {
            if matches!(self.state, State::Text(_) | State::Pdf { .. }) && laid.height > 0.0 {
                self.target = (fy * laid.height, fx * laid.wide);
                self.scroll = self.target.0;
                self.scroll_x = self.target.1;
                self.resume = None;
                self.clamp_scroll();
            }
        }
        self.refresh_find();
        self.step();
        let selection = self.ordered().filter(|(a, b)| a != b);
        let (hits, current): (&[Found], Option<Found>) = match &self.finding {
            Some(f) => (&f.hits, f.current),
            None => (&[], None),
        };
        let found = Rgb(255, 196, 0).d2d(if p.dark { 0.42 } else { 0.5 });
        let found_now = Rgb(255, 140, 0).d2d(if p.dark { 0.75 } else { 0.7 });
        let on = |piece: usize| {
            let a = hits.partition_point(|h| h.piece < piece);
            let b = hits.partition_point(|h| h.piece <= piece);
            &hits[a..b]
        };
        let Some(laid) = &mut self.laid else { return };
        let (sy, sx) = (self.scroll, self.scroll_x);
        gpu.clip(r);
        gpu.set_transform(&Matrix3x2::translation(r.left - sx, r.top - sy));
        let (top, bottom) = (sy - h * 0.25, sy + h * 1.25);
        let highlight = p.accent.d2d(if p.dark { 0.42 } else { 0.28 });
        let pictures = match &self.state {
            State::Text(Source::Slides { pictures, .. }) => Some(pictures),
            _ => None,
        };
        let mut used = HashSet::new();
        for (i, piece) in laid.pieces.iter().enumerate() {
            match piece {
                Piece::Clip(Some(c)) => gpu.clip(*c),
                Piece::Clip(None) => gpu.unclip(),
                Piece::Shape { r: sr, geom, fill, line, flip } => {
                    if sr.bottom < top || sr.top > bottom {
                        continue;
                    }
                    draw_shape(gpu, *sr, *geom, *fill, *line, *flip);
                }
                Piece::Picture { r: pr, index, crop } => {
                    if pr.bottom < top || pr.top > bottom {
                        continue;
                    }
                    if !self.textures.contains_key(index) {
                        if let Some(t) = pictures.and_then(|ps| ps.get(*index)).and_then(|b| b.as_ref()).and_then(|b| gpu.upload(b)) {
                            self.textures.insert(*index, t);
                        }
                    }
                    if let Some(t) = self.textures.get(index) {
                        used.insert(*index);
                        let (tw, th) = (t.width as f32, t.height as f32);
                        let c = crop.map(|v| v.max(0.0));
                        let src = D2D_RECT_F { left: c[0] * tw, top: c[1] * th, right: (tw - c[2] * tw).max(c[0] * tw + 1.0), bottom: (th - c[3] * th).max(c[1] * th + 1.0) };
                        gpu.draw_texture_part(t, *pr, src, 1.0);
                    }
                }
                Piece::Fill { r: fr, radius, ink, alpha: a } => {
                    if fr.bottom < top || fr.top > bottom {
                        continue;
                    }
                    let c = ink.colour(p, *a);
                    if *radius > 0.0 { gpu.fill_round(*fr, *radius, c) } else { gpu.fill(*fr, c) }
                }
                Piece::Text { layout, x, y, h: th, ink, copy } => {
                    if y + th < top || *y > bottom {
                        continue;
                    }
                    for h in on(i) {
                        let c = if Some(*h) == current { found_now } else { found };
                        for m in range_rects(layout, h.from, h.to) {
                            gpu.fill_round(rect(x + m.left - s, y + m.top, m.width + 2.0 * s, m.height), 2.0 * s, c);
                        }
                    }
                    if let (Some((a, b)), Some(copy)) = (selection, copy) {
                        if (a.piece..=b.piece).contains(&i) {
                            let from = if i == a.piece { a.at } else { 0 };
                            let to = if i == b.piece { b.at } else { copy.encode_utf16().count() as u32 };
                            for m in range_rects(layout, from, to) {
                                gpu.fill(rect(x + m.left, y + m.top, m.width.max(3.0 * s), m.height), highlight);
                            }
                        }
                    }
                    gpu.text(layout, *x, *y, ink.colour(p, 1.0));
                }
            }
        }
        if let (Some(grid), State::Text(Source::Table(rows))) = (&mut laid.grid, &self.state) {
            grid.draw(gpu, text, p, rows, s, sy, h, sx, w);
        }
        if let State::Text(Source::Archive(tree)) = &self.state {
            let hover = self.hover.map(|(hx, hy)| (hx - r.left + sx, hy - r.top + sy));
            draw_rows(laid, tree, &self.rows, &self.open, hover, gpu, text, p, sy, h, w);
        }
        // Slides' pictures far off screen give their GPU memory back.
        if self.textures.len() > 24 {
            self.textures.retain(|i, _| used.contains(i));
        }
        if let State::Pdf { pdf, pages, textures, asked, text: pdf_text } = &mut self.state {
            let width = laid.pages.first().map_or(0, |r| (r.right - r.left).round() as u32);
            let mut visible = Vec::new();
            for (i, pr) in laid.pages.iter().enumerate() {
                if pr.bottom < top - h || pr.top > bottom + h {
                    continue;
                }
                if pr.bottom >= sy && pr.top <= sy + h {
                    visible.push(i);
                }
                gpu.fill(D2D_RECT_F { left: pr.left - 1.0, top: pr.top - 1.0, right: pr.right + 1.0, bottom: pr.bottom + 1.0 }, p.edge.d2d(p.edge_alpha * 1.5));
                gpu.fill(*pr, Rgb(255, 255, 255).d2d(1.0));
                if let Some((_, t)) = textures.get(&i) {
                    gpu.draw_texture(t, *pr, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, 1.0);
                }
                // Matches found, then the selection: a band along each line they cover.
                if let Some(page) = pdf_text.pages.get(&i) {
                    for h in on(i) {
                        let c = if Some(*h) == current { found_now } else { found };
                        for band in pdf_bands(pr, page, h.from as usize, h.to as usize) {
                            gpu.fill(band, c);
                        }
                    }
                }
                if let (Some((a, b)), Some(page)) = (selection, pdf_text.pages.get(&i)) {
                    if (a.piece..=b.piece).contains(&i) {
                        let from = if i == a.piece { a.at as usize } else { 0 };
                        let to = if i == b.piece { b.at as usize } else { page.glyphs.len() };
                        for band in pdf_bands(pr, page, from, to) {
                            gpu.fill(band, p.accent.d2d(0.32));
                        }
                    }
                }
            }
            // Ask for what's on screen first, then the pages either side: the pictures, then
            // the text.
            let near: Vec<usize> = visible.iter().copied().chain(visible.first().map(|&f| f.saturating_sub(1))).chain(visible.last().map(|&l| (l + 1).min(pages.len() - 1))).collect();
            for &i in &near {
                if textures.get(&i).is_none_or(|(tw, _)| *tw != width) && asked.insert((i, width)) {
                    pdf.want(i, width);
                }
            }
            if !pdf_text.none {
                for &i in &near {
                    if pdf_text.asked.insert(i) {
                        pdf.want_text(i);
                    }
                }
            }
            // Finding: look through the rest of the pages a few at a time, from here on.
            if let (Some(f), false) = (&mut self.finding, pdf_text.none) {
                if !f.query.is_empty() && !f.stale {
                    let n = pages.len();
                    let from = visible.first().copied().unwrap_or(0).min(n);
                    for i in (from..n).chain(0..from) {
                        if f.waiting.len() >= FIND_AHEAD || f.hits.len() >= MAX_HITS {
                            break;
                        }
                        if !f.searched.contains(&i) && !f.waiting.contains(&i) && !pdf_text.asked.contains(&i) {
                            pdf.want_text(i);
                            pdf_text.asked.insert(i);
                            f.waiting.insert(i);
                        }
                    }
                }
            }
            let finding = self.finding.as_ref();
            if let (Some(&first), Some(&last)) = (visible.first(), visible.last()) {
                textures.retain(|&i, _| i + 4 >= first && i <= last + 4);
                asked.retain(|&(i, _)| i + 4 >= first && i <= last + 4);
                // Text is small: keep more of it, all of what's selected, and while finding,
                // what's held for that and what's on its way.
                let keep = |i: usize| (i + 8 >= first && i <= last + 8) || selection.is_some_and(|(a, b)| (a.piece..=b.piece).contains(&i)) || finding.is_some_and(|f| f.held.contains(&i) || f.waiting.contains(&i));
                pdf_text.pages.retain(|&i, _| keep(i));
                pdf_text.asked.retain(|&i| keep(i));
            }
        }
        gpu.reset_transform();
        gpu.unclip();
        // A thin scroll bar, while the mouse is over the window.
        if laid.height > h + 1.0 && alpha > 0.01 {
            let track = h - (PAD_TOP + 8.0) * s;
            let thumb = (track * h / laid.height).max(28.0 * s);
            let y = r.top + PAD_TOP * s + (track - thumb) * (sy / (laid.height - h)).clamp(0.0, 1.0);
            let bar = rect(r.right - 7.0 * s, y, 3.0 * s, thumb);
            gpu.fill_round(bar, 1.5 * s, p.text.d2d(0.35 * alpha));
        }
    }

    fn lay_out(&self, gpu: &Gpu, text: &Text, p: &Palette, w: f32, s: f32) -> Option<Laid> {
        let mut l = Lay { text, gpu, s, p, pieces: Vec::new(), brushes: HashMap::new() };
        let mut laid = Laid { width: w, scale: s, dark: p.dark, height: 0.0, wide: w, pieces: Vec::new(), grid: None, pages: Vec::new(), slides: Vec::new(), rows_top: 0.0, row_h: 0.0, row_text: HashMap::new(), _brushes: Vec::new() };
        let x = (PAD_X * s).round();
        let inner = (w - x * 2.0).max(40.0 * s);
        let mut y = (PAD_TOP * s).round();
        match &self.state {
            State::Text(Source::Plain(t)) => y += l.code(t, &[], false, x, y, inner),
            State::Text(Source::Code { text: t, spans, numbers }) => y += l.code(t, spans, *numbers, x - if *numbers { 10.0 * s } else { 0.0 }, y, inner + if *numbers { 10.0 * s } else { 0.0 }),
            State::Text(Source::Markdown(blocks)) => {
                // A comfortable measure for reading, centred in wide windows.
                let measure = inner.min(760.0 * s);
                let mx = x + ((inner - measure) / 2.0).round();
                l.blocks(blocks, mx, measure, &mut y, Ink::Text, false);
            }
            State::Text(Source::Slides { deck, .. }) => {
                let m = (16.0 * s).round();
                let pw = (w - m * 2.0).max(40.0 * s).round();
                let ph = (pw * deck.height / deck.width.max(1.0)).round();
                let k = pw / deck.width.max(1.0);
                for slide in &deck.slides {
                    let page = rect(m, y, pw, ph);
                    laid.slides.push(page);
                    l.pieces.push(Piece::Fill { r: rect(m - 1.0, y - 1.0, pw + 2.0, ph + 2.0), radius: 0.0, ink: Ink::Edge, alpha: 1.5 });
                    l.slide(slide, page, k);
                    y += ph + m;
                }
                laid.height = y + PAD_BOTTOM * s - m;
                laid.pieces = l.pieces;
                laid._brushes = l.brushes.into_values().collect();
                return Some(laid);
            }
            State::Text(Source::Archive(tree)) => {
                // The sum of it, and the columns' names, over the lines.
                if let Some(layout) = l.plain(&tree.summary, 12.5, 500, inner, Align::Left) {
                    let (_, th) = Text::size(&layout);
                    l.pieces.push(Piece::Text { layout, x, y, h: th, ink: Ink::Dim, copy: Some(tree.summary.clone()) });
                }
                y += (24.0 * s).round();
                let cols = list_columns(x, inner, s);
                for (name, cx, cw, align) in [("Name", cols.name, cols.name_w, Align::Left), ("Size", cols.size, cols.size_w, Align::Right), ("Modified", cols.date, cols.date_w, Align::Right)] {
                    if let Some(layout) = l.plain(name, 11.5, 600, cw, align) {
                        let (_, th) = Text::size(&layout);
                        l.pieces.push(Piece::Text { layout, x: cx, y: y + ((LIST_HEAD - 24.0) * s - th) / 2.0 + 0.5, h: th, ink: Ink::Dim, copy: None });
                    }
                }
                y += ((LIST_HEAD - 24.0) * s).round();
                l.pieces.push(Piece::Fill { r: rect(x, y - 1.0, inner, 1.0), radius: 0.0, ink: Ink::Edge, alpha: 1.4 });
                laid.rows_top = y;
                laid.row_h = (ROW_H * s).round();
                y += laid.row_h * self.rows.len() as f32;
                if tree.cut {
                    y += 8.0 * s;
                    let note = format!("Showing the first {} lines", tree.nodes.len());
                    if let Some(layout) = l.plain(&note, 12.0, 400, inner, Align::Center) {
                        let (_, th) = Text::size(&layout);
                        l.pieces.push(Piece::Text { layout, x, y, h: th, ink: Ink::Dim, copy: Some(note) });
                        y += th;
                    }
                }
            }
            State::Text(Source::Table(rows)) => {
                let grid = Grid::measure(text, rows, x, y, s);
                y += grid.row_h * rows.len() as f32;
                laid.wide = (grid.x0 + grid.cols.iter().sum::<f32>() + x).max(w);
                laid.grid = Some(grid);
            }
            State::Pdf { pages, .. } => {
                let m = (12.0 * s).round();
                let pw = (w - m * 2.0).max(40.0 * s).round();
                y = m;
                for &(dw, dh) in pages {
                    let ph = (pw * dh / dw).round();
                    laid.pages.push(rect(m, y, pw, ph));
                    y += ph + m;
                }
                laid.height = y;
                return Some(laid);
            }
            _ => return None,
        }
        if let Some(note) = &self.note {
            y += 12.0 * s;
            if let Some(layout) = l.plain(note, 12.0, 400, inner, Align::Center) {
                let (_, th) = Text::size(&layout);
                l.pieces.push(Piece::Text { layout, x, y, h: th, ink: Ink::Dim, copy: Some(note.clone()) });
                y += th;
            }
        }
        laid.height = y + PAD_BOTTOM * s;
        laid.pieces = l.pieces;
        laid._brushes = l.brushes.into_values().collect();
        Some(laid)
    }
}

// ---- Reading files ----

fn load_text(path: &Path, kind: TextKind) -> Loaded {
    use std::io::Read;
    let fail = |e: String| Loaded { source: Err(e), note: None };
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return fail(format!("View couldn't read this file: {e}")),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(len.min(textfile::MAX_BYTES as u64) as usize);
    if (&mut file).take(textfile::MAX_BYTES as u64).read_to_end(&mut bytes).is_err() {
        return fail("View couldn't read this file.".into());
    }
    let cut = len > bytes.len() as u64;
    let Some(text) = textfile::decode(&bytes, cut) else { return fail("This file isn't text.".into()) };
    let mut cut_lines = false;
    let mut lines = |t: String| {
        let (t, more) = textfile::first_lines(t, textfile::MAX_LINES);
        cut_lines |= more;
        t
    };
    let source = match kind {
        TextKind::Markdown => Source::Markdown(markdown::parse(&text)),
        TextKind::Json => {
            let t = lines(textfile::pretty_json(&text).unwrap_or(text));
            Source::Code { spans: textfile::highlight_json(&t), text: t, numbers: true }
        }
        TextKind::Table(d) => {
            let d = if d == b',' { textfile::sniff_delimiter(&text) } else { d };
            let (rows, more) = textfile::parse_table(&text, d, MAX_ROWS, MAX_COLS);
            cut_lines |= more;
            Source::Table(rows)
        }
        TextKind::Code(lang) => {
            let t = lines(text);
            Source::Code { spans: textfile::highlight(&t, lang), text: t, numbers: true }
        }
        TextKind::Plain => {
            let t = lines(text);
            match view_core::kind::from_shebang(t.lines().next().unwrap_or("")) {
                Some(lang) => Source::Code { spans: textfile::highlight(&t, lang), text: t, numbers: true },
                None => Source::Plain(t),
            }
        }
    };
    let note = if cut || cut_lines {
        Some(match &source {
            Source::Table(_) => format!("Showing the first {} rows", MAX_ROWS.min(textfile::MAX_LINES)),
            _ => "Showing the start of this file".to_string(),
        })
    } else {
        None
    };
    Loaded { source: Ok(source), note }
}

/// A Word, Excel or PowerPoint file, or a zip, read for the preview.
fn load_document(path: &Path, kind: DocKind) -> Loaded {
    let fail = |e: String| Loaded { source: Err(e), note: None };
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 1 << 30) {
        return fail("This file is too big to preview.".into());
    }
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return fail(format!("View couldn't read this file: {e}")),
    };
    match kind {
        DocKind::Word => match view_core::office::docx(&data) {
            Ok(b) if b.is_empty() => fail("This document is empty.".into()),
            Ok(b) => Loaded { source: Ok(Source::Markdown(b)), note: None },
            Err(e) => fail(e),
        },
        DocKind::Slides => match view_core::slides::read(&data) {
            Ok(mut deck) => {
                let raw = std::mem::take(&mut deck.pictures);
                let pictures = decode_pictures(&raw);
                Loaded { source: Ok(Source::Slides { deck, pictures }), note: None }
            }
            Err(e) => fail(e),
        },
        DocKind::Sheet => match view_core::office::xlsx(&data, MAX_ROWS, MAX_COLS) {
            Ok(book) if book.rows.is_empty() => fail("This spreadsheet is empty.".into()),
            Ok(book) => {
                let mut notes = Vec::new();
                if book.sheets.len() > 1 {
                    notes.push(format!("Showing \u{201C}{}\u{201D}, sheet {} of {}", book.sheets[book.shown], book.shown + 1, book.sheets.len()));
                }
                if book.cut {
                    notes.push(format!("the first {MAX_ROWS} rows and {MAX_COLS} columns"));
                }
                let note = (!notes.is_empty()).then(|| {
                    let s = notes.join(", ");
                    if book.sheets.len() > 1 { s } else { format!("Showing {s}") }
                });
                Loaded { source: Ok(Source::Table(book.rows)), note }
            }
            Err(e) => fail(e),
        },
        DocKind::Archive => match view_core::zip::tree(&data, 50_000) {
            Some(t) if t.nodes.is_empty() => fail("This zip is empty.".into()),
            Some(t) => Loaded { source: Ok(Source::Archive(t)), note: None },
            None => fail("This zip is damaged, or isn't really a zip.".into()),
        },
    }
}

/// A deck's pictures, decoded with Windows' codecs at no more than [`PICTURE_MAX`] across, and
/// smaller when there are many of them, so they all fit in about 384 MB.
fn decode_pictures(raw: &[Vec<u8>]) -> Vec<Option<Bitmap>> {
    if raw.is_empty() {
        return Vec::new();
    }
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let Some(wic) = crate::wic::Wic::new() else { return raw.iter().map(|_| None).collect() };
    let side = ((384u64 << 20) as f32 / raw.len() as f32 / 4.0).sqrt() as u32;
    let side = side.clamp(320, PICTURE_MAX);
    raw.iter().map(|b| wic.decode_bytes(b, side)).collect()
}

/// The lines of a zip's tree that show: everything not inside a closed folder.
fn shown_rows(tree: &Tree, open: &HashSet<usize>) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tree.nodes.len() {
        out.push(i);
        let n = &tree.nodes[i];
        i = if n.folder && !open.contains(&i) { n.end.max(i + 1) } else { i + 1 };
    }
    out
}

/// Where a zip's columns go, across a list `width` wide starting at `x`.
struct Columns {
    name: f32,
    name_w: f32,
    size: f32,
    size_w: f32,
    date: f32,
    date_w: f32,
}

fn list_columns(x: f32, width: f32, s: f32) -> Columns {
    let date_w = (96.0 * s).round();
    let size_w = (78.0 * s).round();
    let gap = (14.0 * s).round();
    let date = x + width - date_w;
    let size = date - gap - size_w;
    Columns { name: x, name_w: (size - gap - x).max(40.0 * s), size, size_w, date, date_w }
}

/// The glyph for a file in a zip, from Windows' icon font, by its extension.
fn file_glyph(name: &str) -> char {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "avif" | "svg" | "ico" | "raw" | "dng" | "cr2" | "nef" | "arw" => '\u{EB9F}',
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "wmv" | "m4v" => '\u{E714}',
        "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "wma" => '\u{E8D6}',
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "bz2" => '\u{F012}',
        "exe" | "msi" | "dll" | "bat" | "cmd" | "ps1" => '\u{E756}',
        "rs" | "js" | "ts" | "py" | "c" | "h" | "cpp" | "cs" | "java" | "go" | "html" | "css" | "json" | "xml" | "yml" | "yaml" | "toml" | "sh" => '\u{E943}',
        _ => '\u{E7C3}',
    }
}

/// A zip's lines on screen: an arrow and an icon, the name indented by depth, the size and
/// the date. A folder's line lights up under the mouse; a press opens or closes it.
#[allow(clippy::too_many_arguments)]
fn draw_rows(laid: &mut Laid, tree: &Tree, rows: &[usize], open: &HashSet<usize>, hover: Option<(f32, f32)>, gpu: &Gpu, text: &Text, p: &Palette, sy: f32, h: f32, w: f32) {
    let s = laid.scale;
    let rh = laid.row_h;
    if rh <= 0.0 || rows.is_empty() {
        return;
    }
    let x = (PAD_X * s).round();
    let inner = (w - x * 2.0).max(40.0 * s);
    let cols = list_columns(x, inner, s);
    let first = ((sy - laid.rows_top) / rh).floor().max(0.0) as usize;
    let last = ((((sy + h - laid.rows_top) / rh).ceil().max(0.0)) as usize + 1).min(rows.len());
    let (Some(body), Some(small), Some(icons)) = (text.format((13.0 * s).round(), 450, Align::Left, false), text.format((12.0 * s).round(), 400, Align::Right, false), text.icons((14.0 * s).round())) else { return };
    let (Some(arrow_f), Some(folder_bold)) = (text.icons((9.0 * s).round()), text.format((13.0 * s).round(), 560, Align::Left, false)) else { return };
    let indent = (18.0 * s).round();
    for (k, &ni) in rows.iter().enumerate().take(last).skip(first) {
        let n = &tree.nodes[ni];
        let y = laid.rows_top + k as f32 * rh;
        let line = rect(x - 8.0 * s, y + 1.0, inner + 16.0 * s, rh - 2.0);
        let over = hover.is_some_and(|(hx, hy)| hy >= y && hy < y + rh && hx >= line.left && hx <= line.right);
        if over && n.folder {
            gpu.fill_round(line, 6.0 * s, p.hover.d2d(p.hover_alpha * 1.2));
        } else if k % 2 == 1 {
            gpu.fill_round(line, 6.0 * s, p.hover.d2d(p.hover_alpha * 0.35));
        }
        let lx = x + n.depth as f32 * indent;
        let mid = y + rh / 2.0;
        if n.folder {
            let glyph = if open.contains(&ni) { "\u{E70D}" } else { "\u{E76C}" };
            if let Some(l) = text.layout(glyph, &arrow_f, 12.0 * s, rh) {
                let (_, th) = Text::size(&l);
                gpu.text(&l, lx, mid - th / 2.0, p.dim.d2d(1.0));
            }
        }
        let ix = lx + 15.0 * s;
        let (glyph, colour) = if n.folder { ('\u{E8D5}', Rgb(0xE8, 0xB3, 0x3A).d2d(1.0)) } else { (file_glyph(&n.name), p.dim.d2d(1.0)) };
        if let Some(l) = text.layout(&glyph.to_string(), &icons, 18.0 * s, rh) {
            let (_, th) = Text::size(&l);
            gpu.text(&l, ix, mid - th / 2.0, colour);
        }
        let nx = ix + 26.0 * s;
        let cached = laid.row_text.entry(ni).or_insert_with(|| {
            let name = if n.locked { format!("{} (locked)", n.name) } else { n.name.clone() };
            let size = if n.folder && n.files == 0 { String::new() } else { view_core::human_bytes(n.size) };
            let date = n.modified.map(view_core::zip::date_text).unwrap_or_default();
            [
                text.layout(&name, if n.folder { &folder_bold } else { &body }, (cols.size - 10.0 * s - nx).max(20.0 * s), rh),
                text.layout(&size, &small, cols.size_w, rh),
                text.layout(&date, &small, cols.date_w, rh),
            ]
        });
        for (i, (l, at)) in cached.iter().zip([nx, cols.size, cols.date]).enumerate() {
            if let Some(l) = l {
                let (_, th) = Text::size(l);
                gpu.text(l, at, (mid - th / 2.0).round(), if i == 0 { p.text.d2d(1.0) } else { p.dim.d2d(1.0) });
            }
        }
    }
    if laid.row_text.len() > 2000 {
        laid.row_text.clear();
    }
}

/// A slide's shape: filled, outlined, or a line.
fn draw_shape(gpu: &Gpu, r: D2D_RECT_F, geom: slides::Geom, fill: Option<D2D1_COLOR_F>, line: Option<(D2D1_COLOR_F, f32)>, flip: (bool, bool)) {
    use slides::Geom;
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    match geom {
        Geom::Line => {
            if let Some((c, lw)) = line {
                let (x0, x1) = if flip.0 { (r.right, r.left) } else { (r.left, r.right) };
                let (y0, y1) = if flip.1 { (r.bottom, r.top) } else { (r.top, r.bottom) };
                gpu.line((x0, y0), (x1, y1), lw, c);
            }
        }
        Geom::Ellipse => {
            if let Some(c) = fill {
                gpu.fill_ellipse(r, c);
            }
            if let Some((c, lw)) = line {
                gpu.stroke_ellipse(r, lw, c);
            }
        }
        Geom::Rect | Geom::Round(_) => {
            let radius = if let Geom::Round(f) = geom { w.min(h) * f.clamp(0.0, 0.5) } else { 0.0 };
            if let Some(c) = fill {
                if radius > 0.0 { gpu.fill_round(r, radius, c) } else { gpu.fill(r, c) }
            }
            if let Some((c, lw)) = line {
                gpu.stroke_round(D2D_RECT_F { left: r.left - lw / 2.0, top: r.top - lw / 2.0, right: r.right + lw / 2.0, bottom: r.bottom + lw / 2.0 }, radius, lw, c);
            }
        }
    }
}

fn rgba(c: slides::Rgba) -> D2D1_COLOR_F {
    Rgb(c.r, c.g, c.b).d2d(c.a)
}

/// The colouring for a fenced code block's language.
fn fence_lang(name: &str) -> Option<Result<Lang, ()>> {
    let n = name.to_ascii_lowercase();
    Some(Ok(match n.as_str() {
        "json" | "jsonc" | "json5" => return Some(Err(())),
        "rust" | "rs" => Lang::Rust,
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" | "node" => Lang::Script,
        "py" | "python" | "python3" => Lang::Python,
        "sh" | "bash" | "shell" | "zsh" | "console" | "shellsession" | "fish" => Lang::Shell,
        "ps" | "ps1" | "powershell" | "pwsh" => Lang::PowerShell,
        "bat" | "batch" | "cmd" => Lang::Batch,
        "c" | "cpp" | "c++" | "h" | "objc" | "zig" => Lang::C,
        "cs" | "csharp" | "c#" => Lang::CSharp,
        "html" | "xml" | "svg" | "xaml" | "vue" => Lang::Markup,
        "css" | "scss" | "less" => Lang::Css,
        "yaml" | "yml" => Lang::Yaml,
        "toml" | "ini" | "cfg" | "conf" => Lang::Ini,
        "diff" | "patch" => Lang::Diff,
        "sql" => Lang::Sql,
        _ => match view_core::kind::from_extension(&n, |_| false) {
            Kind::Text(TextKind::Code(l)) => l,
            Kind::Text(TextKind::Json) => return Some(Err(())),
            _ => return None,
        },
    }))
}

// ---- Laying out ----

#[derive(Clone, Copy)]
enum Ink {
    Text,
    Dim,
    Accent,
    Edge,
    Tint,
    /// The page itself, for cutting holes in fills.
    Paper,
    /// Text on an accent fill.
    OnAccent,
    /// A colour of the document's own.
    Fixed(Rgb),
}

impl Ink {
    fn colour(self, p: &Palette, a: f32) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
        match self {
            Ink::Text => p.text.d2d(a),
            Ink::Dim => p.dim.d2d(a),
            Ink::Accent => p.accent.d2d(a),
            Ink::Edge => p.edge.d2d(p.edge_alpha * a),
            Ink::Tint => p.hover.d2d(p.hover_alpha * a),
            Ink::Paper => p.panel.d2d(a),
            Ink::OnAccent => p.accent.ink().d2d(a),
            Ink::Fixed(c) => c.d2d(a),
        }
    }
}

enum Piece {
    /// `copy` is the text it shows, for pieces that can be selected.
    Text { layout: IDWriteTextLayout, x: f32, y: f32, h: f32, ink: Ink, copy: Option<String> },
    Fill { r: D2D_RECT_F, radius: f32, ink: Ink, alpha: f32 },
    /// A slide's shape, in its own colours.
    Shape { r: D2D_RECT_F, geom: slides::Geom, fill: Option<D2D1_COLOR_F>, line: Option<(D2D1_COLOR_F, f32)>, flip: (bool, bool) },
    /// A slide's picture, and the share cut off each side.
    Picture { r: D2D_RECT_F, index: usize, crop: [f32; 4] },
    /// What follows is cut to this box (a slide), until a Clip(None).
    Clip(Option<D2D_RECT_F>),
}

struct Laid {
    width: f32,
    scale: f32,
    dark: bool,
    height: f32,
    /// How wide the content is: more than the window for a wide table.
    wide: f32,
    pieces: Vec<Piece>,
    grid: Option<Grid>,
    pages: Vec<D2D_RECT_F>,
    /// Where each slide is.
    slides: Vec<D2D_RECT_F>,
    /// A zip's lines: where they start, how tall each is, and their text, made as they show.
    rows_top: f32,
    row_h: f32,
    row_text: HashMap<usize, [Option<IDWriteTextLayout>; 3]>,
    _brushes: Vec<ID2D1SolidColorBrush>,
}

/// Colours for code, after GitHub's.
fn syntax(token: Token, dark: bool) -> Rgb {
    use Token::*;
    if dark {
        match token {
            Comment => Rgb(0x8B, 0x94, 0x9E),
            String => Rgb(0xA5, 0xD6, 0xFF),
            Number | Key => Rgb(0x79, 0xC0, 0xFF),
            Keyword => Rgb(0xFF, 0x7B, 0x72),
            Type => Rgb(0xFF, 0xA6, 0x57),
            Function | Attr => Rgb(0xD2, 0xA8, 0xFF),
            Tag | Added => Rgb(0x7E, 0xE7, 0x87),
            Heading => Rgb(0x79, 0xC0, 0xFF),
            Removed => Rgb(0xFF, 0xA1, 0x98),
        }
    } else {
        match token {
            Comment => Rgb(0x6E, 0x77, 0x81),
            String => Rgb(0x0A, 0x30, 0x69),
            Number | Key => Rgb(0x05, 0x50, 0xAE),
            Keyword => Rgb(0xCF, 0x22, 0x2E),
            Type => Rgb(0x95, 0x38, 0x00),
            Function | Attr => Rgb(0x82, 0x50, 0xDF),
            Tag | Added => Rgb(0x11, 0x63, 0x29),
            Heading => Rgb(0x05, 0x50, 0xAE),
            Removed => Rgb(0x82, 0x07, 0x1E),
        }
    }
}

fn range(start: u32, end: u32) -> DWRITE_TEXT_RANGE {
    DWRITE_TEXT_RANGE { startPosition: start, length: end.saturating_sub(start) }
}

/// A place in the text: which piece, and how far into it in UTF-16 units.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pos {
    piece: usize,
    at: u32,
}

/// The word around UTF-16 position `at`: letters and digits together, spaces together, and any
/// other character on its own.
pub(crate) fn word_at(s: &str, at: u32) -> (u32, u32) {
    let mut chars = Vec::new();
    let mut u = 0u32;
    for c in s.chars() {
        chars.push((u, c));
        u += c.len_utf16() as u32;
    }
    let class = |c: char| if c.is_alphanumeric() || c == '_' { 0 } else if c.is_whitespace() { 1 } else { 2 };
    let Some(mut k) = chars.iter().rposition(|&(p, _)| p <= at) else { return (at, at) };
    // A click on the right half of a word's last letter lands just after the word: it meant
    // the word.
    if k > 0 && chars[k].0 == at && class(chars[k].1) != 0 && class(chars[k - 1].1) == 0 {
        k -= 1;
    }
    let c = class(chars[k].1);
    if c == 2 {
        return (chars[k].0, chars[k].0 + chars[k].1.len_utf16() as u32);
    }
    let mut a = k;
    while a > 0 && class(chars[a - 1].1) == c {
        a -= 1;
    }
    let mut b = k;
    while b + 1 < chars.len() && class(chars[b + 1].1) == c {
        b += 1;
    }
    (chars[a].0, chars[b].0 + chars[b].1.len_utf16() as u32)
}

/// The boxes a run of text takes up, one per line it's on, inside its layout.
pub(crate) fn range_rects(layout: &IDWriteTextLayout, from: u32, to: u32) -> Vec<DWRITE_HIT_TEST_METRICS> {
    if to <= from {
        return Vec::new();
    }
    let mut count = 0u32;
    unsafe {
        let _ = layout.HitTestTextRange(from, to - from, 0.0, 0.0, None, &mut count);
    }
    let mut out = vec![DWRITE_HIT_TEST_METRICS::default(); count as usize];
    if count > 0 && unsafe { layout.HitTestTextRange(from, to - from, 0.0, 0.0, Some(&mut out), &mut count) }.is_ok() {
        out.truncate(count as usize);
        out
    } else {
        Vec::new()
    }
}

/// UTF-16 positions for byte positions, which is what DirectWrite counts in.
fn utf16_map(s: &str) -> Vec<u32> {
    let mut map = vec![0u32; s.len() + 1];
    let mut u = 0u32;
    for (i, c) in s.char_indices() {
        map[i] = u;
        u += c.len_utf16() as u32;
    }
    map[s.len()] = u;
    map
}

struct Lay<'a> {
    text: &'a Text,
    gpu: &'a Gpu,
    s: f32,
    p: &'a Palette,
    pieces: Vec<Piece>,
    brushes: HashMap<(u8, u8, u8), ID2D1SolidColorBrush>,
}

impl Lay<'_> {
    fn px(&self, dip: f32) -> f32 {
        (dip * self.s).round()
    }

    fn brush(&mut self, c: Rgb) -> Option<ID2D1SolidColorBrush> {
        if let Some(b) = self.brushes.get(&(c.0, c.1, c.2)) {
            return Some(b.clone());
        }
        let b = self.gpu.brush(c.d2d(1.0))?;
        self.brushes.insert((c.0, c.1, c.2), b.clone());
        Some(b)
    }

    fn plain(&self, s: &str, dip: f32, weight: u32, width: f32, align: Align) -> Option<IDWriteTextLayout> {
        let f = self.text.format(self.px(dip), weight, align, true)?;
        self.text.layout(s, &f, width, 1e7)
    }

    /// Monospaced text, in pieces of [`CHUNK`] lines, with line numbers if asked. Returns its
    /// height.
    fn code(&mut self, text: &str, spans: &[Span], numbers: bool, x: f32, y0: f32, width: f32) -> f32 {
        let size = self.px(12.5);
        let Some(format) = self.text.mono(size, true) else { return 0.0 };
        let line_h = (size * 1.55).round();
        let base = (line_h * 0.78).round();
        let char_w = self.text.layout("0000000000", &format, 1e5, 1e5).map_or(size * 0.6, |l| Text::size(&l).0 / 10.0);
        let total = text.lines().count().max(1);
        let gutter = if numbers { (total.to_string().len() as f32 * char_w + self.px(22.0)).round() } else { 0.0 };
        let map = utf16_map(text);
        let colours: Vec<(Token, Option<ID2D1SolidColorBrush>)> =
            [Token::Comment, Token::String, Token::Number, Token::Keyword, Token::Type, Token::Function, Token::Tag, Token::Attr, Token::Key, Token::Heading, Token::Added, Token::Removed].into_iter().map(|t| (t, self.brush(syntax(t, self.p.dark)))).collect();
        let mut y = y0;
        let mut line_no = 1usize;
        let mut start = 0usize;
        let mut next_span = 0usize;
        while start <= text.len() {
            // The chunk's end: after CHUNK lines, or the end of the text.
            let mut end = start;
            let mut lines = 0;
            while end < text.len() && lines < CHUNK {
                match text[end..].find('\n') {
                    Some(k) => {
                        end += k + 1;
                        lines += 1;
                    }
                    None => {
                        end = text.len();
                        lines += 1;
                    }
                }
            }
            let piece = text[start..end].strip_suffix('\n').unwrap_or(&text[start..end]);
            let Some(layout) = self.text.layout(if piece.is_empty() { " " } else { piece }, &format, width - gutter, 1e7) else { break };
            unsafe {
                let _ = layout.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, line_h, base);
                let _ = layout.SetIncrementalTabStop(char_w * 4.0);
            }
            let u0 = map[start];
            while next_span < spans.len() && spans[next_span].start < start + piece.len() {
                let sp = spans[next_span];
                if sp.end > start {
                    let (a, b) = (sp.start.max(start), sp.end.min(start + piece.len()));
                    if let Some((_, Some(brush))) = colours.iter().find(|(t, _)| *t == sp.token) {
                        unsafe {
                            let _ = layout.SetDrawingEffect(brush, range(map[a] - u0, map[b] - u0));
                        }
                    }
                }
                if sp.end > start + piece.len() {
                    break; // carries on into the next piece
                }
                next_span += 1;
            }
            let mut count = 0u32;
            unsafe {
                let _ = layout.GetLineMetrics(None, &mut count);
            }
            let mut metrics = vec![DWRITE_LINE_METRICS::default(); count as usize];
            unsafe {
                let _ = layout.GetLineMetrics(Some(&mut metrics), &mut count);
            }
            let h = line_h * count.max(1) as f32;
            if numbers {
                // A number on each line's first row, and nothing beside the rows it wraps onto.
                let mut gutter_text = String::new();
                let mut starts = true;
                for m in &metrics {
                    if starts {
                        gutter_text.push_str(&line_no.to_string());
                        line_no += 1;
                    }
                    gutter_text.push('\n');
                    starts = m.newlineLength > 0;
                }
                if let Some(g) = self.text.layout(&gutter_text, &format, gutter - self.px(14.0), 1e7) {
                    unsafe {
                        let _ = g.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, line_h, base);
                        let _ = g.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
                    }
                    self.pieces.push(Piece::Text { layout: g, x, y, h, ink: Ink::Dim, copy: None });
                }
            }
            self.pieces.push(Piece::Text { layout, x: x + gutter, y, h, ink: Ink::Text, copy: Some(piece.to_string()) });
            y += h;
            if end >= text.len() {
                break;
            }
            start = end;
        }
        if numbers {
            self.pieces.push(Piece::Fill { r: rect(x + gutter - self.px(9.0), y0, 1.0, y - y0), radius: 0.0, ink: Ink::Edge, alpha: 1.0 });
        }
        y - y0
    }

    /// Styled text: emphasis, code and links.
    fn runs(&mut self, runs: &[Run], dip: f32, weight: u32, width: f32, spacing: f32) -> Option<(IDWriteTextLayout, f32, String)> {
        let mut s = String::new();
        for r in runs {
            s.push_str(&r.text);
        }
        if s.is_empty() {
            s.push(' ');
        }
        let format = self.text.format(self.px(dip), weight, Align::Left, true)?;
        let layout = self.text.layout(&s, &format, width, 1e7)?;
        let size = self.px(dip);
        unsafe {
            let _ = layout.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, (size * spacing).round(), (size * (spacing - 0.32)).round());
        }
        let four: Option<IDWriteTextLayout4> = layout.cast().ok();
        let (mono, system) = self.text.mono_family();
        let accent = self.brush(self.p.accent);
        let code = self.brush(syntax(Token::Removed, self.p.dark));
        let dim = self.brush(self.p.dim);
        let mut at = 0u32;
        for r in runs {
            let n = r.text.encode_utf16().count() as u32;
            let rg = range(at, at + n);
            at += n;
            unsafe {
                if r.style & markdown::BOLD != 0 {
                    let bold = [DWRITE_FONT_AXIS_VALUE { axisTag: DWRITE_FONT_AXIS_TAG_WEIGHT, value: (weight + 250).min(800) as f32 }];
                    match &four {
                        Some(f) => {
                            let _ = f.SetFontAxisValues(&bold, rg);
                        }
                        None => {
                            let _ = layout.SetFontWeight(DWRITE_FONT_WEIGHT_BOLD, rg);
                        }
                    }
                }
                if r.style & (markdown::ITALIC | markdown::IMAGE) != 0 {
                    let _ = layout.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, rg);
                }
                if r.style & markdown::STRIKE != 0 {
                    let _ = layout.SetStrikethrough(true, rg);
                }
                if r.style & markdown::CODE != 0 {
                    if let Some(sys) = &system {
                        let _ = layout.SetFontCollection(sys, rg);
                    }
                    let _ = layout.SetFontFamilyName(mono, rg);
                    let _ = layout.SetFontSize((size * 0.88).round(), rg);
                    if let Some(b) = &code {
                        let _ = layout.SetDrawingEffect(b, rg);
                    }
                } else if r.style & markdown::LINK != 0 {
                    if let Some(b) = &accent {
                        let _ = layout.SetDrawingEffect(b, rg);
                    }
                } else if r.style & markdown::IMAGE != 0 {
                    if let Some(b) = &dim {
                        let _ = layout.SetDrawingEffect(b, rg);
                    }
                }
            }
        }
        let (_, h) = Text::size(&layout);
        Some((layout, h, s))
    }

    fn blocks(&mut self, blocks: &[Block], x: f32, width: f32, y: &mut f32, ink: Ink, tight: bool) {
        let first_y = *y;
        let gap = if tight { self.px(4.0) } else { self.px(12.0) };
        for block in blocks {
            match block {
                Block::Heading(level, runs) => {
                    let i = (*level as usize).clamp(1, 6) - 1;
                    let size = [26.0, 21.0, 17.0, 15.0, 14.0, 13.0][i];
                    let weight = [700, 650, 620, 600, 600, 600][i];
                    if *y > first_y {
                        *y += self.px([14.0, 12.0, 8.0, 6.0, 4.0, 4.0][i]);
                    }
                    if let Some((layout, h, copy)) = self.runs(runs, size, weight, width, 1.3) {
                        self.pieces.push(Piece::Text { layout, x, y: *y, h, ink, copy: Some(copy) });
                        *y += h;
                    }
                    if i < 2 {
                        *y += self.px(7.0);
                        self.pieces.push(Piece::Fill { r: rect(x, *y, width, 1.0), radius: 0.0, ink: Ink::Edge, alpha: 1.2 });
                        *y += self.px(1.0);
                    }
                    *y += self.px(10.0);
                }
                Block::Paragraph(runs) => {
                    if let Some((layout, h, copy)) = self.runs(runs, 14.0, 400, width, 1.55) {
                        self.pieces.push(Piece::Text { layout, x, y: *y, h, ink, copy: Some(copy) });
                        *y += h + gap;
                    }
                }
                Block::Code(lang, code) => {
                    let pad = self.px(14.0);
                    let top = *y;
                    let spans = match fence_lang(lang) {
                        Some(Ok(l)) => textfile::highlight(code, l),
                        Some(Err(())) => textfile::highlight_json(code),
                        None => Vec::new(),
                    };
                    let at = self.pieces.len();
                    let h = self.code(code, &spans, false, x + pad, top + pad, width - pad * 2.0);
                    self.pieces.insert(at, Piece::Fill { r: rect(x, top, width, h + pad * 2.0), radius: self.px(8.0), ink: Ink::Tint, alpha: 1.0 });
                    *y = top + h + pad * 2.0 + gap;
                }
                Block::Quote(inner) => {
                    let top = *y;
                    let inset = self.px(18.0);
                    self.blocks(inner, x + inset, width - inset, y, Ink::Dim, tight);
                    let bottom = *y - gap;
                    self.pieces.push(Piece::Fill { r: rect(x, top, self.px(3.0), (bottom - top).max(self.px(14.0))), radius: self.px(1.5), ink: Ink::Edge, alpha: 3.0 });
                }
                Block::List(items) => {
                    let indent = self.px(24.0);
                    for item in items {
                        let top = *y;
                        match item.task {
                            Some(done) => {
                                let d = self.px(14.0);
                                let bx = rect(x + self.px(2.0), top + self.px(4.0), d, d);
                                if done {
                                    self.pieces.push(Piece::Fill { r: bx, radius: self.px(3.0), ink: Ink::Accent, alpha: 1.0 });
                                    if let Some(f) = self.text.icons(self.px(10.0)) {
                                        if let Some(layout) = self.text.layout("\u{E73E}", &f, d, d) {
                                            let (_, th) = Text::size(&layout);
                                            self.pieces.push(Piece::Text { layout, x: bx.left, y: bx.top + (d - th) / 2.0, h: th, ink: Ink::OnAccent, copy: None });
                                        }
                                    }
                                } else {
                                    self.pieces.push(Piece::Fill { r: bx, radius: self.px(3.0), ink: Ink::Edge, alpha: 3.0 });
                                    let inner = rect(bx.left + 1.5 * self.s, bx.top + 1.5 * self.s, d - 3.0 * self.s, d - 3.0 * self.s);
                                    self.pieces.push(Piece::Fill { r: inner, radius: self.px(2.0), ink: Ink::Paper, alpha: 1.0 });
                                }
                            }
                            None => {
                                let marker = if item.marker == "•" { "•" } else { item.marker.as_str() };
                                if let Some(layout) = self.plain(marker, 14.0, 500, indent - self.px(6.0), Align::Left) {
                                    unsafe {
                                        let size = self.px(14.0);
                                        let _ = layout.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, (size * 1.55).round(), (size * 1.23).round());
                                    }
                                    let (_, th) = Text::size(&layout);
                                    self.pieces.push(Piece::Text { layout, x: x + self.px(4.0), y: top, h: th, ink: Ink::Dim, copy: None });
                                }
                            }
                        }
                        self.blocks(&item.blocks, x + indent, width - indent, y, ink, true);
                        if *y == top {
                            *y += self.px(22.0);
                        }
                    }
                    if !tight {
                        *y += self.px(8.0);
                    }
                }
                Block::Rule => {
                    *y += self.px(8.0);
                    self.pieces.push(Piece::Fill { r: rect(x, *y, width, 1.0), radius: 0.0, ink: Ink::Edge, alpha: 1.6 });
                    *y += self.px(20.0);
                }
                Block::Table { align, head, rows } => self.table(align, head, rows, x, width, y, gap),
            }
        }
    }

    /// A slide in `page`, `k` pixels to the EMU: its background, then its shapes, pictures
    /// and text in order, cut to the page.
    fn slide(&mut self, slide: &slides::Slide, page: D2D_RECT_F, k: f32) {
        self.pieces.push(Piece::Clip(Some(page)));
        match slide.background {
            Paint::Colour(c) => self.pieces.push(Piece::Shape { r: page, geom: slides::Geom::Rect, fill: Some(rgba(c)), line: None, flip: (false, false) }),
            Paint::Picture(index) => {
                self.pieces.push(Piece::Shape { r: page, geom: slides::Geom::Rect, fill: Some(Rgb(255, 255, 255).d2d(1.0)), line: None, flip: (false, false) });
                self.pieces.push(Piece::Picture { r: page, index, crop: [0.0; 4] });
            }
        }
        for item in &slide.items {
            let [ix, iy, iw, ih] = item.rect;
            let r = rect(page.left + ix * k, page.top + iy * k, iw * k, ih * k);
            match &item.kind {
                ItemKind::Shape { geom, fill, line } => {
                    let line = line.map(|(c, w)| (rgba(c), (w * k).max(0.75)));
                    match fill {
                        Some(Paint::Picture(index)) => {
                            self.pieces.push(Piece::Picture { r, index: *index, crop: [0.0; 4] });
                            if line.is_some() {
                                self.pieces.push(Piece::Shape { r, geom: *geom, fill: None, line, flip: item.flip });
                            }
                        }
                        Some(Paint::Colour(c)) => self.pieces.push(Piece::Shape { r, geom: *geom, fill: Some(rgba(*c)), line, flip: item.flip }),
                        None => self.pieces.push(Piece::Shape { r, geom: *geom, fill: None, line, flip: item.flip }),
                    }
                }
                ItemKind::Picture { index, crop } => self.pieces.push(Piece::Picture { r, index: *index, crop: *crop }),
                ItemKind::Missing(what) => {
                    let grey = Rgb(128, 128, 128);
                    self.pieces.push(Piece::Shape { r, geom: slides::Geom::Rect, fill: Some(grey.d2d(0.12)), line: Some((grey.d2d(0.35), 1.0)), flip: (false, false) });
                    let size = (r.bottom - r.top).min(r.right - r.left).mul_add(0.08, 0.0).clamp(9.0 * self.s, 16.0 * self.s);
                    if let Some(f) = self.text.format(size.round(), 500, Align::Center, false) {
                        if let Some(layout) = self.text.layout(what, &f, r.right - r.left, r.bottom - r.top) {
                            let (_, th) = Text::size(&layout);
                            let y = r.top + ((r.bottom - r.top) - th) / 2.0;
                            self.pieces.push(Piece::Text { layout, x: r.left, y, h: th, ink: Ink::Fixed(grey), copy: None });
                        }
                    }
                }
                ItemKind::Text(tb) => self.slide_text(tb, r, k),
            }
        }
        self.pieces.push(Piece::Clip(None));
    }

    /// A slide's text box in `r`. Text set to shrink on overflow is made smaller until it fits.
    fn slide_text(&mut self, tb: &slides::TextBox, r: D2D_RECT_F, k: f32) {
        let inner = D2D_RECT_F { left: r.left + tb.inset[0] * k, top: r.top + tb.inset[1] * k, right: r.right - tb.inset[2] * k, bottom: r.bottom - tb.inset[3] * k };
        let (iw, ih) = ((inner.right - inner.left).max(1.0), (inner.bottom - inner.top).max(1.0));
        let mut scale = tb.scale;
        let mut lines;
        loop {
            lines = self.slide_paras(tb, inner.left, iw, k * scale);
            let total = lines.last().map_or(0.0, |(_, _, y, h, _, _)| y + h);
            if !tb.shrink || total <= ih + 0.5 || scale <= 0.3 {
                break;
            }
            scale *= 0.9;
        }
        let total = lines.last().map_or(0.0, |(_, _, y, h, _, _)| y + h);
        let top = match tb.anchor {
            slides::Anchor::Top => inner.top,
            slides::Anchor::Middle => inner.top + (ih - total) / 2.0,
            slides::Anchor::Bottom => inner.bottom - total,
        };
        for (layout, x, y, h, ink, copy) in lines {
            self.pieces.push(Piece::Text { layout, x, y: (top + y).round(), h, ink, copy });
        }
    }

    /// A text box's paragraphs, laid out from the top: each one's text, x, y, height, colour
    /// and words (bullets have none). `k` is pixels to the EMU, shrinking included.
    #[allow(clippy::type_complexity)]
    fn slide_paras(&mut self, tb: &slides::TextBox, x: f32, width: f32, k: f32) -> Vec<(IDWriteTextLayout, f32, f32, f32, Ink, Option<String>)> {
        let px = |pt: f32| (pt * slides::EMU_PER_PT * k).max(0.5);
        let mut out = Vec::new();
        let mut y = 0.0;
        for (pi, para) in tb.paras.iter().enumerate() {
            if pi > 0 {
                y += px(para.before);
            }
            let mut s: String = para.runs.iter().map(|r| r.text.as_str()).collect();
            let empty = s.is_empty();
            if empty {
                s.push(' ');
            }
            let first = para.runs.first();
            let size = px(first.map_or(para.size, |r| r.size));
            let family = first.map_or("Calibri", |r| r.font.as_str());
            let Some(format) = self.text.system_format(family, size) else { continue };
            let mut tx = x + para.margin * k;
            let mut bullet = None;
            if let Some(b) = &para.bullet {
                let bx = x + (para.margin + para.indent) * k;
                let bsize = px(b.size);
                if let Some(bf) = self.text.system_format(family, bsize) {
                    if let Some(bl) = self.text.layout(&b.text, &bf, 1e4, 1e4) {
                        let (bw, bh) = Text::size(&bl);
                        // No room hung out for it: the text moves over to make some.
                        if para.indent >= -1.0 {
                            tx = tx.max(bx + bw + size * 0.35);
                        }
                        bullet = Some((bl, bx, bh, b.colour));
                    }
                }
            }
            let tw = (x + width - tx).max(size);
            let Some(layout) = self.text.layout(&s, &format, tw, 1e6) else { continue };
            let (_, system) = self.text.mono_family();
            unsafe {
                if !tb.wrap {
                    let _ = layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                }
                let _ = layout.SetTextAlignment(match para.align {
                    slides::Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                    slides::Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                    slides::Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
                    slides::Align::Justify => DWRITE_TEXT_ALIGNMENT_JUSTIFIED,
                });
                let mut at = 0u32;
                for run in &para.runs {
                    let n = run.text.encode_utf16().count() as u32;
                    let rg = range(at, at + n);
                    at += n;
                    let fam = crate::util::Wide::new(&self.text.system_family(&run.font));
                    if let Some(sys) = &system {
                        let _ = layout.SetFontCollection(sys, rg);
                    }
                    let _ = layout.SetFontFamilyName(fam.pcwstr(), rg);
                    let _ = layout.SetFontSize(px(run.size), rg);
                    if run.bold {
                        let _ = layout.SetFontWeight(DWRITE_FONT_WEIGHT_BOLD, rg);
                    }
                    if run.italic {
                        let _ = layout.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, rg);
                    }
                    if run.underline {
                        let _ = layout.SetUnderline(true, rg);
                    }
                    if run.strike {
                        let _ = layout.SetStrikethrough(true, rg);
                    }
                    if let Some(b) = self.brush(Rgb(run.colour.r, run.colour.g, run.colour.b)) {
                        let _ = layout.SetDrawingEffect(&b, rg);
                    }
                }
                if (para.line - 1.0).abs() > 0.01 {
                    if let Ok(three) = layout.cast::<IDWriteTextLayout3>() {
                        let spacing = DWRITE_LINE_SPACING { method: DWRITE_LINE_SPACING_METHOD_PROPORTIONAL, height: para.line, baseline: para.line, leadingBefore: 0.0, fontLineGapUsage: DWRITE_FONT_LINE_GAP_USAGE_DEFAULT };
                        let _ = three.SetLineSpacing(&spacing);
                    }
                }
            }
            let (_, h) = Text::size(&layout);
            if let Some((bl, bx, bh, colour)) = bullet {
                // The bullet sits on the first line's middle.
                let mut first_h = h;
                let mut count = 0u32;
                unsafe {
                    let _ = layout.GetLineMetrics(None, &mut count);
                    let mut m = vec![DWRITE_LINE_METRICS::default(); count as usize];
                    if count > 0 && layout.GetLineMetrics(Some(&mut m), &mut count).is_ok() {
                        first_h = m[0].height;
                    }
                }
                out.push((bl, bx, y + (first_h - bh) / 2.0, bh, Ink::Fixed(Rgb(colour.r, colour.g, colour.b)), None));
            }
            let ink = Ink::Fixed(first.map_or(Rgb(0, 0, 0), |r| Rgb(r.colour.r, r.colour.g, r.colour.b)));
            out.push((layout, tx, y, h, ink, (!empty).then(|| s.replace('\u{2028}', "\n"))));
            y += h;
        }
        out
    }

    fn table(&mut self, align: &[markdown::Align], head: &[Vec<Run>], rows: &[Vec<Vec<Run>>], x: f32, width: f32, y: &mut f32, gap: f32) {
        let cols = head.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
        if cols == 0 {
            return;
        }
        let pad = self.px(10.0);
        let empty = Vec::new();
        let mut cells: Vec<Vec<(IDWriteTextLayout, f32, String)>> = Vec::new();
        let mut natural = vec![self.px(24.0); cols];
        for (ri, row) in std::iter::once(head).chain(rows.iter().map(|r| r.as_slice())).enumerate() {
            let mut laid = Vec::new();
            for c in 0..cols {
                let runs = row.get(c).unwrap_or(&empty);
                let Some((layout, _, copy)) = self.runs(runs, 13.0, if ri == 0 { 600 } else { 400 }, 1e5, 1.45) else { return };
                let mut m = DWRITE_TEXT_METRICS::default();
                unsafe {
                    let _ = layout.GetMetrics(&mut m);
                }
                natural[c] = natural[c].max(m.widthIncludingTrailingWhitespace.ceil() + pad * 2.0);
                laid.push((layout, 0.0, copy));
            }
            cells.push(laid);
        }
        // Shrink the widest columns until the table fits.
        let mut widths = natural.clone();
        let total: f32 = widths.iter().sum();
        if total > width {
            let fair = width / cols as f32;
            let small: f32 = widths.iter().filter(|w| **w <= fair).sum();
            let big = widths.iter().filter(|w| **w > fair).count() as f32;
            let share = ((width - small) / big.max(1.0)).max(self.px(48.0));
            for w in widths.iter_mut() {
                if *w > fair {
                    *w = share;
                }
            }
        }
        let table_w: f32 = widths.iter().sum::<f32>().min(width.max(widths.iter().sum()));
        let top = *y;
        for (ri, row) in cells.iter_mut().enumerate() {
            let mut h: f32 = 0.0;
            for (c, (layout, ch, _)) in row.iter_mut().enumerate() {
                unsafe {
                    let _ = layout.SetMaxWidth(widths[c] - pad * 2.0);
                    let _ = layout.SetTextAlignment(match align.get(c) {
                        Some(markdown::Align::Center) => DWRITE_TEXT_ALIGNMENT_CENTER,
                        Some(markdown::Align::Right) => DWRITE_TEXT_ALIGNMENT_TRAILING,
                        _ => DWRITE_TEXT_ALIGNMENT_LEADING,
                    });
                }
                *ch = Text::size(layout).1;
                h = h.max(*ch);
            }
            let row_h = h + pad * 1.4;
            if ri == 0 {
                self.pieces.push(Piece::Fill { r: rect(x, *y, table_w, row_h), radius: 0.0, ink: Ink::Tint, alpha: 1.0 });
            }
            let mut cx = x;
            for (c, (layout, ch, copy)) in row.drain(..).enumerate() {
                self.pieces.push(Piece::Text { layout, x: cx + pad, y: *y + pad * 0.7, h: ch, ink: Ink::Text, copy: Some(copy) });
                cx += widths[c];
            }
            *y += row_h;
            self.pieces.push(Piece::Fill { r: rect(x, *y, table_w, 1.0), radius: 0.0, ink: Ink::Edge, alpha: 1.2 });
        }
        self.pieces.push(Piece::Fill { r: rect(x, top, table_w, 1.0), radius: 0.0, ink: Ink::Edge, alpha: 1.2 });
        *y += gap;
    }
}

/// A spreadsheet's grid. Cells are laid out only as they come into view.
struct Grid {
    x0: f32,
    y0: f32,
    row_h: f32,
    cols: Vec<f32>,
    cells: HashMap<(usize, usize), IDWriteTextLayout>,
}

impl Grid {
    fn measure(text: &Text, rows: &[Vec<String>], x0: f32, y0: f32, s: f32) -> Grid {
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![48.0 * s; cols];
        if let Some(f) = text.format((12.5 * s).round(), 400, Align::Left, false) {
            for (ri, row) in rows.iter().take(200).enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    if cell.is_empty() {
                        continue;
                    }
                    let cell: String = cell.chars().take(80).collect();
                    let w = text.layout(&cell, &f, 1e5, 1e4).map_or(0.0, |l| Text::size(&l).0) * if ri == 0 { 1.06 } else { 1.0 };
                    widths[c] = widths[c].max((w + 26.0 * s).min(300.0 * s).ceil());
                }
            }
        }
        Grid { x0, y0, row_h: (30.0 * s).round(), cols: widths, cells: HashMap::new() }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(&mut self, gpu: &Gpu, text: &Text, p: &Palette, rows: &[Vec<String>], s: f32, sy: f32, h: f32, sx: f32, w: f32) {
        if rows.is_empty() {
            return;
        }
        let total_w: f32 = self.cols.iter().sum();
        let first = ((sy - self.y0) / self.row_h).floor().max(0.0) as usize;
        let last = (((sy + h - self.y0) / self.row_h).ceil().max(0.0) as usize + 1).min(rows.len());
        let (Some(body), Some(head)) = (text.format((12.5 * s).round(), 400, Align::Left, false), text.format((12.5 * s).round(), 600, Align::Left, false)) else { return };
        let pad = (12.0 * s).round();
        // Columns on screen.
        let mut col_x = Vec::with_capacity(self.cols.len());
        let mut x = self.x0;
        for &cw in &self.cols {
            col_x.push(x);
            x += cw;
        }
        let visible = |c: usize| col_x[c] + self.cols[c] >= sx && col_x[c] <= sx + w;
        let draw_row = |gpu: &Gpu, cells: &mut HashMap<(usize, usize), IDWriteTextLayout>, ri: usize, y: f32| {
            let row = &rows[ri];
            for c in 0..self.cols.len() {
                if !visible(c) {
                    continue;
                }
                let Some(value) = row.get(c).filter(|v| !v.is_empty()) else { continue };
                let key = (ri, c);
                if !cells.contains_key(&key) {
                    let shown: String = value.chars().take(400).map(|ch| if ch == '\n' { ' ' } else { ch }).collect();
                    if let Some(l) = text.layout(&shown, if ri == 0 { &head } else { &body }, self.cols[c] - pad * 2.0, self.row_h) {
                        // Numbers line up on the right, as in a spreadsheet.
                        if ri > 0 && value.trim().parse::<f64>().is_ok() {
                            unsafe {
                                let _ = l.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
                            }
                        }
                        cells.insert(key, l);
                    }
                }
                if let Some(l) = cells.get(&key) {
                    let (_, th) = Text::size(l);
                    gpu.text(l, col_x[c] + pad, y + ((self.row_h - th) / 2.0).round(), if ri == 0 { p.text.d2d(1.0) } else { p.text.d2d(0.92) });
                }
            }
        };
        for ri in first.max(1)..last {
            let y = self.y0 + ri as f32 * self.row_h;
            if ri % 2 == 0 {
                gpu.fill(rect(self.x0, y, total_w, self.row_h), p.hover.d2d(p.hover_alpha * 0.55));
            }
            draw_row(gpu, &mut self.cells, ri, y);
        }
        // The heading row stays at the top as the rows scroll under it.
        let hy = (self.y0).max(sy + 48.0 * s);
        gpu.fill(rect(self.x0, hy, total_w, self.row_h), p.panel.d2d(1.0));
        gpu.fill(rect(self.x0, hy, total_w, self.row_h), p.hover.d2d(p.hover_alpha * 1.4));
        gpu.fill(rect(self.x0, hy + self.row_h - 1.0, total_w, 1.0), p.edge.d2d(p.edge_alpha * 1.8));
        draw_row(gpu, &mut self.cells, 0, hy);
        for c in 1..self.cols.len() {
            if visible(c) {
                gpu.fill(rect(col_x[c], self.y0.min(hy), 1.0, self.row_h * (last as f32) + (hy - self.y0).max(0.0)), p.edge.d2d(p.edge_alpha * 0.9));
            }
        }
        // Forget cells far off screen.
        if self.cells.len() > 4000 {
            self.cells.retain(|&(r, _), _| r == 0 || (r + 60 >= first && r <= last + 60));
        }
    }
}

/// A PDF character's box in the document, on the page laid out at `page`.
fn glyph_rect(page: &D2D_RECT_F, g: &Glyph) -> D2D_RECT_F {
    let (w, h) = (page.right - page.left, page.bottom - page.top);
    D2D_RECT_F { left: page.left + g.rect[0] * w, top: page.top + g.rect[1] * h, right: page.left + g.rect[2] * w, bottom: page.top + g.rect[3] * h }
}

impl Finding {
    /// Put in a page's matches, in place of any it had.
    fn merge(&mut self, piece: usize, hits: Vec<Found>) {
        self.hits.retain(|h| h.piece != piece);
        let at = self.hits.partition_point(|h| h.piece < piece);
        self.hits.splice(at..at, hits);
        self.hits.truncate(MAX_HITS);
    }
}

/// Where the words are found on a PDF page, as ranges of its characters.
fn pdf_page_hits(page: &PageText, index: usize, query: &[char]) -> Vec<Found> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut h = view_core::find::Haystack::new();
    for (k, g) in page.glyphs.iter().enumerate() {
        if k > 0 && g.brk != Break::None {
            h.push_space(k as u32);
        }
        h.push(page.str(k), k as u32, k as u32 + 1);
    }
    h.find(query, MAX_HITS).into_iter().map(|(from, to)| Found { piece: index, from, to }).collect()
}

/// The bands that mark characters `from..to` on a PDF page: one along each line they're on.
/// Spaces only widen a band between characters; a line of nothing but spaces (how some
/// writers keep empty lines) gets none.
fn pdf_bands(pr: &D2D_RECT_F, page: &PageText, from: usize, to: usize) -> Vec<D2D_RECT_F> {
    let to = to.min(page.glyphs.len());
    let mut out = Vec::new();
    let mut line: Option<D2D_RECT_F> = None;
    for k in from.min(to)..to {
        let g = &page.glyphs[k];
        if g.brk == Break::Line {
            out.extend(line.take());
        }
        if page.str(k).chars().all(char::is_whitespace) {
            continue;
        }
        let gr = glyph_rect(pr, g);
        line = Some(match line {
            Some(l) => union(l, gr),
            None => gr,
        });
    }
    out.extend(line);
    out
}

fn union(a: D2D_RECT_F, b: D2D_RECT_F) -> D2D_RECT_F {
    D2D_RECT_F { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) }
}
