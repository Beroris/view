//! The window and everything it does: what's shown, how it got there, and what the user does
//! with it. Drawing the interface itself is in `ui.rs`; the menu is in `menu.rs`.

use crate::cmd::Cmd;
use crate::doc::{Doc, WM_DOC};
use crate::gfx::{self, Gpu, Texture};
use crate::media::{Player, WM_MEDIA};
use crate::model::{Model, WM_MODEL};
use crate::text::Text;
use crate::theme::{self, Palette};
use crate::ui;
use crate::util::{self, Wide, trace};
use crate::worker::{Done, Job, Output, Pool, Task, WM_JOB_DONE};
use crate::{menu, shell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use view_core::cache::{Cache, Quality};
use view_core::folder::{self, Entry, Folder};
use view_core::kind::Kind;
use view_core::metadata::Camera;
use view_core::orient::Orient;
use view_core::picture::{Frame, Picture};
use view_core::settings::{PeekMode, Settings, Toolbar, Wheel};
use view_core::tween::Tween;
use view_core::viewport::{Mode, View, Viewport};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

#[path = "tools.rs"]
mod tools;

pub const CLASS: &str = "View.Window";
pub const WM_FOLDER: u32 = WM_APP + 2;

const TIMER_IDLE: usize = 1;
const TIMER_ANIMATION: usize = 2;
const TIMER_SAVE: usize = 3;
const TIMER_LOADING: usize = 4;
const TIMER_TOAST: usize = 5;
/// Show the preview window even if its picture isn't ready yet.
const TIMER_PEEK_SHOW: usize = 6;
/// The preview process ends a while after its window was last closed.
const TIMER_PEEK_EXIT: usize = 7;
/// Posted when a preview card's thumbnail is ready.
pub const WM_CARD: u32 = WM_APP + 3;
/// Posted when a song's cover and tags have been read.
pub const WM_ART: u32 = WM_APP + 6;
/// Posted when the words in a photo have been read.
pub const WM_OCR: u32 = WM_APP + 8;
/// Read the words in a photo once it's been on screen a moment.
const TIMER_OCR: usize = 8;
/// Where the words read off a photo are left: the photo, how it was turned, and the words.
type OcrSlot = Arc<Mutex<Option<(PathBuf, Orient, Option<view_core::pdftext::PageText>)>>>;

/// How long the old image stays up while the next one decodes, before "loading" shows instead.
const KEEP_OLD: Duration = Duration::from_millis(250);
/// How many images either side of the current one stay decoded. Further ones are dropped even
/// when the cache has room, so a long browse doesn't leave memory high.
const CACHE_REACH: usize = 8;
/// How long the toolbar stays after the mouse stops.
const IDLE: Duration = Duration::from_millis(1800);
const ZOOM_TIME: Duration = Duration::from_millis(150);

/// What's on screen.
pub struct Shown {
    pub path: PathBuf,
    pub picture: Option<Arc<Picture>>,
    pub quality: Quality,
    pub texture: Option<Texture>,
    /// The frame showing, for animations.
    pub frame: usize,
    pub error: Option<String>,
    pub since: Instant,
    /// The previous image's texture, kept up for a moment while this one decodes.
    pub old: Option<(Texture, Arc<Picture>, View, Orient)>,
    /// The previous image again, fading out under this one since `fade_at`.
    pub fading: Option<(Texture, Arc<Picture>, View, Orient)>,
    pub fade_at: Instant,
    /// A video or song, playing.
    pub player: Option<Player>,
}

/// A song's cover and tags, read by the shell off this thread.
pub struct Art {
    pub path: PathBuf,
    pub texture: Option<Texture>,
    pub tags: shell::Tags,
    pub loading: bool,
}

type ArtSlot = Arc<Mutex<Option<(PathBuf, Option<view_core::picture::Bitmap>, shell::Tags)>>>;

/// Whether a file is something View plays rather than shows.
pub fn is_media(path: &Path) -> bool {
    path.extension().is_some_and(|e| view_core::kind::is_media(&e.to_string_lossy()))
}

/// How one image takes to fade into the next. Short enough that holding an arrow key
/// still flips quickly.
const CROSSFADE: Duration = Duration::from_millis(110);

struct Animation {
    due: Instant,
    plays: u32,
    /// For animations too big to keep: frames arrive from a decoding thread.
    stream: Option<(Receiver<Frame>, Arc<AtomicBool>)>,
}

impl Drop for Animation {
    fn drop(&mut self) {
        if let Some((_, stop)) = &self.stream {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

struct Scan {
    dir: PathBuf,
    entries: Vec<Entry>,
}

#[derive(Clone, Copy)]
enum Drag {
    /// Panning the image, from this point.
    Pan { x: i32, y: i32 },
    /// Pressed on an interface control.
    Press(ui::Hit),
    /// Dragging along a video's seek bar.
    Seek,
    /// Turning a 3D model, from this point.
    Orbit { x: i32, y: i32 },
    /// Dragging the volume slider.
    Volume,
    /// Selecting text in a document.
    Select,
    /// Selecting words in a photo.
    Words,
    /// Pressed on a video: plays or pauses it on release.
    Click,
    /// Selecting interface text: the information panel, a message, a dialog, a card.
    Text,
    /// Placing the caret or selecting in the find or page box.
    Field,
    /// Moving the preview, held this far from its top-left corner (in screen pixels).
    Move { dx: i32, dy: i32 },
    /// Pressed on the picture where dragging takes the file out to another app, from here.
    Out { x: i32, y: i32 },
    /// Pressed on a thumbnail in the grid: opens it, or drags it out.
    Thumb { x: i32, y: i32, index: usize },
    /// Moving the crop's frame, or one of its edges, in the editor.
    Crop,
    /// Moving one of the editor's sliders.
    Slider(u8),
}

struct Saved {
    placement: WINDOWPLACEMENT,
    style: isize,
}

pub struct App {
    pub hwnd: HWND,
    pub settings: Settings,
    pub palette: Palette,
    pub dpi: u32,
    pool: Pool,
    pub gpu: Option<Gpu>,
    pub d3d: Option<ID3D11Device>,
    pub text: Text,
    cache: Cache,
    pub folder: Folder,
    folder_dir: Option<PathBuf>,
    scan: Arc<Mutex<Option<Scan>>>,
    scanned_at: Option<Instant>,
    pub extensions: Arc<Mutex<Vec<String>>>,
    pub shown: Option<Shown>,
    pub vp: Viewport,
    /// The user's turns and flips of the current image, on top of its EXIF orientation.
    pub user: Orient,
    tween: Option<Tween>,
    forward: bool,
    screen: (u32, u32),
    drag: Option<Drag>,
    pub mouse: (i32, i32),
    pub ui: ui::State,
    fullscreen: Option<Saved>,
    pub toast: Option<(String, Instant)>,
    /// A question or notice over the image; while it's up, nothing else responds.
    pub dialog: Option<ui::Dialog>,
    pub camera: Option<(PathBuf, Option<Camera>)>,
    animation: Option<Animation>,
    tracking: bool,
    nc_tracking: bool,
    cursor_hidden: bool,
    /// Set when this is the Space preview from File Explorer rather than a View window.
    pub peek: Option<Peek>,
    /// The cover and tags of the song playing.
    pub art: Option<Art>,
    art_slot: ArtSlot,
    /// Where the next video starts, in seconds: from `--at`, or after the GPU was replaced.
    pub start_at: Option<f64>,
    /// While Z is held: the view to go back to when it's let go.
    held_z: Option<(View, Mode)>,
    /// Where each video was left this session, to start there again on coming back to it.
    video_places: HashMap<PathBuf, f64>,
    /// The zoom to open the next photo at, with "Keep zoom between photos" on: its scale, and
    /// the point of the picture at the middle of the window, as shares of its width and height.
    kept: Option<(f64, f64, f64)>,
    /// The words read off the photo on screen, for selecting and copying.
    pub words: Option<crate::words::Words>,
    ocr_slot: OcrSlot,
    /// Words are being read off a photo now; one at a time.
    ocr_running: bool,
    /// An accent key pressed in the find box (` on an Irish keyboard, say), waiting for the
    /// letter it goes on: its key and the keys held with it.
    dead_key: Option<(u16, isize)>,
    /// How fast videos and songs play, for as long as View is open.
    pub speed: f64,
    /// Files moved to the Recycle Bin from here, the last one last, for Undo.
    deleted: Vec<PathBuf>,
    /// The "Moved to the Recycle Bin" message has an Undo button until then.
    pub undo_until: Option<Instant>,
    /// Thumbnails for the filmstrip and the grid.
    pub thumbs: crate::thumbs::Thumbs,
    /// Every file in the folder as thumbnails, while that's up.
    pub grid: Option<Grid>,
    /// Where the filmstrip is, as a place in the folder: it glides to the file showing.
    pub strip_at: f32,
    /// Cropping, straightening and adjusting the photo, while that's going on.
    pub editor: Option<crate::editor::Editor>,
    /// Where a copy being saved, or a background being set, leaves its result.
    saved: Arc<Mutex<Option<Finished>>>,
}

/// The grid of thumbnails.
pub struct Grid {
    /// The thumbnail with the keyboard's attention, by its place in the folder.
    pub selected: usize,
    /// How far down it's scrolled, in pixels, and where that's gliding to.
    pub scroll: f32,
    pub target: f32,
    /// Show where the selection is: it was moved with the keys.
    pub keyed: bool,
    /// Select the file showing once the folder has been listed again: one was just put back.
    pub follow: bool,
}

/// Work done off the window's thread, finished.
enum Finished {
    /// A photo saved as a copy: where, or why not.
    Copy(Result<PathBuf, String>),
    /// The desktop background set.
    Wallpaper(Result<(), String>),
}

/// Posted when a copy has been saved or the background set.
pub const WM_SAVED: u32 = WM_APP + 10;

/// The Space preview: a window that floats over File Explorer without taking the keyboard from
/// it, so the arrow keys still move through the files, and the preview follows.
pub struct Peek {
    /// The Explorer window (or the desktop) it's previewing for.
    pub anchor: HWND,
    /// A file that isn't an image: shown as a card with the shell's own thumbnail.
    pub card: Option<Card>,
    /// An STL, 3MF or OBJ model, drawn in 3D.
    pub model: Option<Model>,
    /// Text, code, a table or a PDF, shown as a page that scrolls.
    pub doc: Option<Doc>,
    /// Waiting for the picture before showing the window, so it opens at the right size.
    pending: bool,
    /// Where the card thumbnail thread leaves its result.
    thumbnail: Arc<Mutex<Option<(PathBuf, Option<view_core::picture::Bitmap>)>>>,
    /// How far down each document was scrolled when it was last open, to go back there.
    places: HashMap<PathBuf, (f32, f32)>,
    /// The find or page box over a document, while it's open.
    pub bar: Option<Bar>,
    /// Where its middle was dragged to, on screen, while it stays open; the next file shows
    /// there too. Forgotten when it closes.
    moved: Option<(i32, i32)>,
    /// What it does when you click away, read from the settings each time it opens a file, as
    /// they're changed in the main window.
    mode: PeekMode,
    /// A font file, shown in itself, or why it can't be.
    pub font: Option<Result<crate::text::FontFace, String>>,
    /// Where a folder's count and pictures are left, read off this thread.
    folder: Arc<Mutex<Option<(PathBuf, String, Vec<view_core::picture::Bitmap>)>>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarKind {
    /// Ctrl+F: words to look for.
    Find,
    /// Ctrl+G: a page to go to.
    Page,
}

/// The box at the bottom of a document in the preview that typing goes into.
pub struct Bar {
    pub kind: BarKind,
    /// What's typed, with its caret and selection.
    pub field: view_core::field::Field,
    /// How far the text is scrolled left, in pixels, to keep the caret in the box.
    pub scroll: f32,
}

impl BarKind {
    /// The most characters each box takes.
    fn limit(self) -> usize {
        match self {
            BarKind::Find => 200,
            BarKind::Page => 6,
        }
    }
}

pub struct Card {
    pub path: PathBuf,
    pub name: String,
    /// "PDF Document · 2.4 MB" and the like.
    pub lines: Vec<String>,
    pub texture: Option<Texture>,
    /// Still asking the shell for a thumbnail.
    pub loading: bool,
    /// For a folder: some of the pictures in it.
    pub pictures: Vec<Texture>,
}

pub fn scale(dpi: u32) -> f32 {
    dpi as f32 / 96.0
}

impl App {
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen.is_some()
    }

    pub fn scale(&self) -> f32 {
        scale(self.dpi)
    }

    pub fn client_size(&self) -> (f32, f32) {
        let mut r = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut r);
        }
        ((r.right - r.left).max(1) as f32, (r.bottom - r.top).max(1) as f32)
    }

    /// The part of the window the image is shown in: all of it. The information card floats
    /// over the picture rather than pushing it aside.
    pub fn area(&self) -> (f32, f32) {
        self.client_size()
    }

    /// The view right now, partway through a zoom if one is moving.
    pub fn current_view(&self) -> View {
        match &self.tween {
            Some(t) => t.at(Instant::now()).0,
            None => self.vp.view,
        }
    }

    /// The image's orientation as shown: EXIF, then the user's.
    pub fn orient(&self) -> Orient {
        let base = self.shown.as_ref().and_then(|s| s.picture.as_ref()).map_or(Orient::IDENTITY, |p| p.orient);
        base.then(self.user)
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    pub fn is_maximized(&self) -> bool {
        unsafe { IsZoomed(self.hwnd) }.as_bool()
    }

    /// How far the current image has faded in over the last one, from 0 to 1.
    pub fn crossfade(&self) -> f32 {
        match &self.shown {
            Some(s) if s.fading.is_some() => (s.fade_at.elapsed().as_secs_f32() / CROSSFADE.as_secs_f32()).min(1.0),
            _ => 1.0,
        }
    }

    /// Whether anything is moving, so painting should carry on at the display's rate.
    pub fn animating(&self) -> bool {
        self.tween.is_some()
            || self.ui.fading()
            || self.crossfade() < 1.0
            || self.player().is_some_and(|p| p.playing() && p.has_video())
            || self.peek.as_ref().and_then(|p| p.doc.as_ref()).is_some_and(|d| d.scrolling())
            || self.selection_overshoot() != 0.0
            || self.grid.as_ref().is_some_and(|g| (g.scroll - g.target).abs() > 0.5)
            || self.strip_moving()
    }

    /// While text is being selected with the mouse past the top or bottom edge band of the
    /// preview: how far past it (negative above), else 0.
    fn selection_overshoot(&self) -> f32 {
        if !matches!(self.drag, Some(Drag::Select)) {
            return 0.0;
        }
        let edge = 40.0 * self.scale();
        let (_, h) = self.client_size();
        let y = self.mouse.1 as f32;
        let over = if y < edge {
            y - edge
        } else if y > h - edge {
            y - (h - edge)
        } else {
            0.0
        };
        // Nothing to do (or to keep drawing for) at the very top or bottom.
        let room = self.peek.as_ref().and_then(|p| p.doc.as_ref()).is_some_and(|d| d.can_scroll(over > 0.0));
        if room { over } else { 0.0 }
    }

    /// Scroll a selection along while the mouse is held past an edge, each frame, faster the
    /// further out it is, so it can run on through a long document.
    fn follow_selection(&mut self) {
        let over = self.selection_overshoot();
        let s = self.scale();
        let (x, y) = (self.mouse.0 as f32, self.mouse.1 as f32);
        let dragging = matches!(self.drag, Some(Drag::Select));
        if let Some(doc) = self.peek.as_mut().and_then(|p| p.doc.as_mut()) {
            // The wheel can glide the text along under a mouse that's holding still, too.
            if doc.follow(over, s) && dragging {
                doc.select_to(x, y);
            }
        }
    }

    /// Tell the Space helper whether there's text selected in the preview, so Ctrl+C, which
    /// goes to Explorer, can come here instead. A property on the window is all it reads.
    /// The same goes for Ctrl+F and Ctrl+G with a document showing, and for typing while its
    /// find or page box is open.
    fn selection_changed(&mut self) {
        let doc = self.peek.as_ref().and_then(|p| p.doc.as_ref());
        let selected = doc.is_some_and(|d| d.has_selection()) || self.words.as_ref().is_some_and(|w| w.has_selection()) || self.ui.pick.has_selection();
        let typing = self.peek.as_ref().is_some_and(|p| p.bar.is_some() && p.doc.is_some());
        for (prop, on) in [(crate::spacebar::SELECTION_PROP, selected), (crate::spacebar::DOC_PROP, doc.is_some()), (crate::spacebar::TYPING_PROP, typing)] {
            unsafe {
                if on {
                    let _ = SetPropW(self.hwnd, prop, Some(HANDLE(1 as *mut _)));
                } else {
                    let _ = RemovePropW(self.hwnd, prop);
                }
            }
        }
        self.invalidate();
    }

    /// Open the find box (Ctrl+F) or the page box (Ctrl+G) over the document in the preview.
    fn open_bar(&mut self, kind: BarKind) {
        let Some(peek) = &mut self.peek else { return };
        let Some(doc) = &mut peek.doc else { return };
        let problem = match kind {
            BarKind::Find if !doc.searchable() => Some("There's no text to look through in this file"),
            BarKind::Page if doc.page_count().is_none() => Some("Only PDFs have pages to go to"),
            _ => None,
        };
        if let Some(problem) = problem {
            self.say(problem);
            return;
        }
        // Pressed again while open, it selects what's typed, ready to be typed over.
        let field = match peek.bar.take() {
            Some(mut b) if b.kind == kind => {
                b.field.select_all();
                b.field
            }
            _ => view_core::field::Field::new(kind.limit()),
        };
        if kind == BarKind::Page {
            doc.end_find();
        } else if !field.is_empty() {
            doc.find(&field.text());
        }
        peek.bar = Some(Bar { kind, field, scroll: 0.0 });
        self.ui.show(true);
        self.selection_changed();
    }

    /// A key for the find or page box, passed on by the Space helper since the preview never
    /// has the keyboard.
    fn bar_key(&mut self, vk: u16, flags: isize) {
        use crate::spacebar::{KEY_ALT, KEY_CTRL, KEY_SHIFT};
        let (shift, ctrl, alt) = (flags & KEY_SHIFT != 0, flags & KEY_CTRL != 0, flags & KEY_ALT != 0);
        let key = VIRTUAL_KEY(vk);
        let Some(kind) = self.peek.as_ref().and_then(|p| p.bar.as_ref()).map(|b| b.kind) else {
            match key {
                VK_F if ctrl => self.run(Cmd::Find),
                VK_G if ctrl => self.run(Cmd::GoToPage),
                _ => {}
            }
            return;
        };
        let find = kind == BarKind::Find;
        let Some(mut field) = self.peek.as_ref().and_then(|p| p.bar.as_ref()).map(|b| b.field.clone()) else { return };
        let before = field.text();
        // Shortcuts, and keys that edit or move, drop an accent waiting for its letter.
        let modifier = matches!(key, VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_CONTROL | VK_LCONTROL | VK_RCONTROL | VK_MENU | VK_LMENU | VK_RMENU | VK_CAPITAL);
        if !modifier && ((ctrl && !alt) || edit_key(key)) {
            self.dead_key = None;
        }
        // The page box takes only digits.
        let fit = |s: &str| if find { s.to_string() } else { s.chars().filter(char::is_ascii_digit).collect() };
        match key {
            VK_ESCAPE => return self.run(Cmd::CloseBar),
            VK_RETURN | VK_F3 if find => return self.run(if shift { Cmd::FindPrev } else { Cmd::FindNext }),
            VK_DOWN if find => return self.run(Cmd::FindNext),
            VK_UP if find => return self.run(Cmd::FindPrev),
            VK_RETURN => {
                if let (Ok(n), Some(doc)) = (field.text().trim().parse::<usize>(), self.peek.as_mut().and_then(|p| p.doc.as_mut())) {
                    let count = doc.page_count().unwrap_or(0);
                    doc.go_to_page(n.clamp(1, count.max(1)));
                }
                return self.run(Cmd::CloseBar);
            }
            VK_F if ctrl => return self.run(Cmd::Find),
            VK_G if ctrl => return self.run(Cmd::GoToPage),
            // Copy what's selected in the box; with nothing selected there, what's selected in
            // the document.
            VK_C | VK_INSERT if ctrl && !alt => {
                if !field.has_selection() {
                    return self.run(Cmd::CopyText);
                }
                let ok = shell::copy_text(self.hwnd, &field.selected());
                self.say(if ok { "Copied" } else { "Couldn't copy the text" });
                return;
            }
            VK_X if ctrl && !alt => {
                if field.has_selection() && shell::copy_text(self.hwnd, &field.selected()) {
                    field.insert("");
                }
            }
            VK_A if ctrl && !alt => field.select_all(),
            VK_V if ctrl && !alt => field.insert(&fit(&shell::paste_text(self.hwnd).unwrap_or_default())),
            VK_INSERT if shift => field.insert(&fit(&shell::paste_text(self.hwnd).unwrap_or_default())),
            VK_LEFT | VK_RIGHT => field.step(key == VK_RIGHT, ctrl, shift),
            VK_HOME | VK_END => field.to_end(key == VK_END, shift),
            VK_BACK => field.delete_back(ctrl),
            VK_DELETE => field.delete_forward(ctrl),
            _ if ctrl && !alt => return,
            _ => match typed_text(vk, flags, &mut self.dead_key) {
                Some(t) => field.insert(&fit(&t)),
                None => return,
            },
        }
        let text = field.text();
        if let Some(peek) = &mut self.peek {
            if let (true, true, Some(doc)) = (find, text != before, &mut peek.doc) {
                doc.find(&text);
            }
            if let Some(bar) = &mut peek.bar {
                bar.field = field;
            }
        }
        self.invalidate();
    }

    /// A press (or with `word`, a double-click) on text in View's own interface: start selecting
    /// it, dropping any other selection. False if there's no such text there.
    fn start_text(&mut self, x: i32, y: i32, word: bool) -> bool {
        if !self.ui.pick.start(x as f32, y as f32, word) {
            return false;
        }
        if let Some(w) = &mut self.words {
            w.selection = None;
        }
        if let Some(doc) = self.peek.as_mut().and_then(|p| p.doc.as_mut()) {
            doc.clear_selection();
        }
        unsafe { SetCapture(self.hwnd) };
        self.drag = Some(Drag::Text);
        self.selection_changed();
        true
    }

    /// Put the caret in the find or page box where it was clicked; with `extend`, select from
    /// where it was to there.
    fn field_click(&mut self, x: f32, extend: bool, word: bool) {
        let Some(at) = self.ui.field_position(x) else { return };
        if let Some(bar) = self.peek.as_mut().and_then(|p| p.bar.as_mut()) {
            let at = bar.field.from_units(at);
            if word {
                bar.field.select_word(at);
            } else {
                bar.field.place(at, extend);
            }
        }
        self.invalidate();
    }

    pub fn player(&self) -> Option<&Player> {
        self.shown.as_ref().and_then(|s| s.player.as_ref())
    }

    pub fn player_mut(&mut self) -> Option<&mut Player> {
        self.shown.as_mut().and_then(|s| s.player.as_mut())
    }

    /// Note where the video on screen was left, before it's closed: for this session, and in
    /// the file of positions if the user asked for videos to be remembered.
    fn leave_video(&mut self) {
        let Some(p) = self.player() else { return };
        if !p.has_video() || p.error.is_some() {
            return;
        }
        let (path, duration) = (p.path.clone(), p.duration());
        let at = if p.ended { duration.unwrap_or(0.0) } else { p.position() };
        if view_core::resume::worth_resuming(at, duration) {
            self.video_places.insert(path.clone(), at);
        } else {
            self.video_places.remove(&path);
        }
        if self.resume_on() {
            let file = util::positions_file();
            let mut positions = view_core::resume::Positions::load(&file);
            positions.remember(&path.to_string_lossy(), at, duration);
            let _ = positions.save(&file);
        }
    }

    /// Whether video positions are kept after closing. The preview stays open in the background
    /// for a while, so it reads the setting afresh in case a View window has changed it.
    fn resume_on(&self) -> bool {
        if self.peek.is_some() { Settings::load(&util::settings_file()).resume_videos } else { self.settings.resume_videos }
    }

    /// Where to start `path` from, if it was left part-way.
    fn resume_point(&self, path: &Path) -> Option<f64> {
        if let Some(&at) = self.video_places.get(path) {
            return Some(at);
        }
        if self.resume_on() {
            return view_core::resume::Positions::load(&util::positions_file()).get(&path.to_string_lossy());
        }
        None
    }

    // ---- Text in photos ----

    /// Read the words in the photo on screen in a moment, once it's settled.
    fn want_words(&mut self) {
        self.words = None;
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_OCR, 700, None);
        }
    }

    /// Read the words in the photo on screen, off this thread, with Windows' text recognition.
    fn start_ocr(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_OCR);
        }
        let Some(s) = &self.shown else { return };
        let Some(p) = s.picture.clone() else { return };
        let orient = self.orient();
        if p.is_animated() || p.streams || self.words.as_ref().is_some_and(|w| w.path == s.path && w.orient == orient) {
            return;
        }
        if self.ocr_running {
            // One at a time: this one is looked at again when that one's done.
            return;
        }
        self.ocr_running = true;
        let path = s.path.clone();
        let slot = self.ocr_slot.clone();
        let hwnd = self.hwnd.0 as isize;
        let _ = std::thread::Builder::new().name("ocr".into()).spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            }
            let page = crate::ocr::Reader::new().and_then(|reader| {
                let f = &p.frames[0].bitmap;
                // Smaller first, then turned, so a big photo isn't copied whole.
                let small = if f.width.max(f.height) > reader.max_side() { crate::ocr::shrink(f, reader.max_side()) } else { None };
                let b = small.as_ref().unwrap_or(f);
                let (w, h, data) = view_core::orient::apply(orient, b.width, b.height, &b.data);
                reader.read(&view_core::picture::Bitmap { width: w, height: h, data })
            });
            *slot.lock().unwrap() = Some((path, orient, page));
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_OCR, WPARAM(0), LPARAM(0));
            }
        });
    }

    fn take_ocr(&mut self) {
        self.ocr_running = false;
        let Some((path, orient, page)) = self.ocr_slot.lock().unwrap().take() else { return };
        let current = self.shown.as_ref().is_some_and(|s| s.path == path) && self.orient() == orient;
        if current {
            if let Some(page) = page.filter(|p| !p.glyphs.is_empty()) {
                trace(&format!("ocr: {} characters", page.glyphs.len()));
                self.words = Some(crate::words::Words { path, orient, page, selection: None });
            }
        } else if self.shown.as_ref().is_some_and(|s| s.picture.is_some()) {
            // It's moved on since: read what's there now.
            self.want_words();
        }
    }

    /// Where the photo the words were read from is on screen, if it's still the one showing,
    /// turned the same way.
    pub fn words_placed(&self) -> Option<crate::words::Placed> {
        let w = self.words.as_ref()?;
        let s = self.shown.as_ref()?;
        let p = s.picture.as_ref()?;
        let orient = self.orient();
        (w.path == s.path && w.orient == orient).then(|| ui::image_rect(p, self.current_view(), orient))
    }

    /// A double-click on a word in a photo: select the word, and carry on selecting from it
    /// while the button's held. False if there's no word there.
    fn select_word(&mut self, x: i32, y: i32) -> bool {
        let Some(at) = self.words_placed() else { return false };
        let Some(w) = self.words.as_mut().filter(|w| w.over(x as f32, y as f32, at)) else { return false };
        w.select_start(x as f32, y as f32, at, true);
        unsafe { SetCapture(self.hwnd) };
        self.drag = Some(Drag::Words);
        self.selection_changed();
        true
    }

    fn over_words(&self, x: i32, y: i32) -> bool {
        match (self.words_placed(), &self.words) {
            (Some(at), Some(w)) => w.over(x as f32, y as f32, at),
            _ => false,
        }
    }

    /// Note how far down the document in the preview is, to go back there.
    fn leave_doc(&mut self) {
        let Some(peek) = &mut self.peek else { return };
        let Some(doc) = &peek.doc else { return };
        if let Some(place) = doc.place() {
            if place.0 > 0.001 || place.1 > 0.001 {
                peek.places.insert(doc.path.clone(), place);
            } else {
                peek.places.remove(&doc.path);
            }
        }
    }

    /// Start playing the current file, if it's a video or song.
    fn start_player(&mut self, path: &Path) {
        let Some(d3d) = self.d3d.clone() else {
            if let Some(s) = &mut self.shown {
                s.error = Some("View needs a graphics device to play video, and couldn't get one.".into());
            }
            return;
        };
        let (volume, muted) = (self.settings.volume as f64, self.settings.muted);
        match Player::open(path, &d3d, self.hwnd, true, volume, muted) {
            Ok(mut p) => {
                if self.speed != 1.0 {
                    p.set_rate(self.speed);
                }
                if let Some(s) = &mut self.shown {
                    s.player = Some(p);
                }
            }
            Err(e) => {
                trace(&format!("player: {e}"));
                if let Some(s) = &mut self.shown {
                    s.error = Some("Windows couldn't play this file.".into());
                }
            }
        }
    }

    /// News from the player.
    fn on_media(&mut self, event: u32, serial: usize) {
        let Some(p) = self.player_mut() else { return };
        if p.serial != serial {
            return;
        }
        let was_ready = p.ready;
        p.event(event);
        if p.ready && !was_ready {
            if let Some(t) = self.start_at.take() {
                let Some(p) = self.player_mut() else { return };
                p.seek(t, true);
            }
        }
        let Some(p) = self.player() else { return };
        let error = p.error.clone();
        let size = p.size();
        let ready = p.ready;
        let path = p.path.clone();
        if let Some(e) = error {
            if let Some(s) = &mut self.shown {
                s.error = Some(e);
            }
            if self.peek.as_ref().is_some_and(|p| p.pending) {
                self.peek_reveal();
            }
        } else if ready && !was_ready {
            if let Some((w, h)) = size {
                let (aw, ah) = self.area();
                self.vp = Viewport::new(w, h, aw as f64, ah as f64, true);
                self.tween = None;
            } else if self.art.as_ref().is_none_or(|a| a.path != path) {
                self.start_art(path);
            }
            if self.peek.is_some() {
                self.peek_reveal();
            }
        }
        self.invalidate();
    }

    /// Read a song's cover and tags, off this thread.
    fn start_art(&mut self, path: PathBuf) {
        self.art = Some(Art { path: path.clone(), texture: None, tags: shell::Tags::default(), loading: true });
        let slot = self.art_slot.clone();
        let hwnd = self.hwnd.0 as isize;
        let px = (480.0 * self.scale()).round() as i32;
        let _ = std::thread::Builder::new().name("art".into()).spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            }
            let tags = shell::tags(&path);
            let bitmap = shell::art(&path, px);
            *slot.lock().unwrap() = Some((path, bitmap, tags));
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_ART, WPARAM(0), LPARAM(0));
            }
        });
    }

    fn take_art(&mut self) {
        let Some((path, bitmap, tags)) = self.art_slot.lock().unwrap().take() else { return };
        let Some(art) = &mut self.art else { return };
        if art.path != path {
            return;
        }
        art.loading = false;
        art.tags = tags;
        art.texture = bitmap.as_ref().and_then(|b| self.gpu.as_ref().and_then(|g| g.upload(b)));
        self.invalidate();
    }

    /// Drag along the seek bar: `fraction` of the way through.
    fn seek_to(&mut self, fraction: f32, exact: bool) {
        let Some(p) = self.player_mut() else { return };
        if let Some(d) = p.duration() {
            p.seek(d * fraction.clamp(0.0, 1.0) as f64, exact);
        }
        self.invalidate();
    }

    fn set_volume(&mut self, volume: f32, muted: bool) {
        self.settings.volume = volume.clamp(0.0, 1.0);
        self.settings.muted = muted;
        let (v, m) = (self.settings.volume as f64, muted);
        if let Some(p) = self.player_mut() {
            p.set_volume(v, m);
        }
        self.changed();
    }

    // ---- Opening and moving between images ----

    /// Open a file from outside: the command line, a drop, the open dialog.
    pub fn open(&mut self, path: PathBuf) {
        let path = std::path::absolute(&path).unwrap_or(path);
        self.folder = Folder::single(&path);
        self.folder_dir = path.parent().map(Path::to_path_buf);
        self.cache.clear();
        self.go(0, true);
        if self.peek.is_none() {
            self.rescan();
        }
    }

    /// List the folder again, off this thread.
    fn rescan(&mut self) {
        let Some(dir) = self.folder_dir.clone() else { return };
        self.scanned_at = Some(Instant::now());
        let slot = self.scan.clone();
        let extensions = self.extensions.clone();
        let hwnd = self.hwnd.0 as isize;
        let _ = std::thread::Builder::new().name("scan".into()).spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            }
            let exts = {
                let known = extensions.lock().unwrap().clone();
                if known.is_empty() {
                    let found = supported_extensions();
                    *extensions.lock().unwrap() = found.clone();
                    found
                } else {
                    known
                }
            };
            let started = Instant::now();
            let entries = folder::scan(&dir, |e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)) || view_core::kind::is_media(e), |m| {
                use std::os::windows::fs::MetadataExt;
                const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
                m.file_attributes() & HIDDEN_OR_SYSTEM == 0
            });
            trace(&format!("scanned {} in {:.1} ms", dir.display(), started.elapsed().as_secs_f64() * 1000.0));
            if let Ok(entries) = entries {
                *slot.lock().unwrap() = Some(Scan { dir, entries });
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_FOLDER, WPARAM(0), LPARAM(0));
                }
            }
        });
    }

    fn take_scan(&mut self) {
        let Some(scan) = self.scan.lock().unwrap().take() else { return };
        if self.folder_dir.as_deref() != Some(scan.dir.as_path()) {
            return; // an old folder's listing
        }
        let Some(current) = self.folder.current().map(|e| e.path.clone()) else { return };
        self.folder.replace(scan.entries, &current, self.settings.sort, self.settings.sort_descending);
        let (index, len) = (self.folder.index, self.folder.len());
        self.strip_at = index as f32; // the filmstrip starts on the file, rather than sliding to it
        if let Some(g) = &mut self.grid {
            let follow = std::mem::take(&mut g.follow);
            g.selected = if follow { index } else { g.selected.min(len.saturating_sub(1)) };
            if follow {
                self.grid_reveal(true);
            }
        }
        self.request();
        self.set_title();
        self.invalidate();
    }

    pub fn step(&mut self, delta: isize) {
        if let Some(i) = self.folder.offset(delta, self.settings.wrap) {
            if i != self.folder.index || self.folder.len() == 1 {
                self.forward = delta > 0;
                self.go(i, false);
            }
        }
    }

    /// Show the folder's `index`th image.
    pub fn go(&mut self, index: usize, fresh: bool) {
        if index >= self.folder.len() {
            return;
        }
        self.leave_video();
        self.folder.index = index;
        let path = self.folder.entries[index].path.clone();
        let media = is_media(&path);
        self.keep_zoom(media);
        let stamp = view_core::cache::Stamp::of(&path);
        let cached = if media { None } else { self.cache.get(&path, stamp) };
        self.art = None;

        // Keep the old image up briefly, so a quick decode swaps straight over without a blank.
        let old = if fresh || media {
            None
        } else {
            let (user, view) = (self.user, self.current_view());
            self.shown.take().and_then(|s| match (s.texture, s.picture) {
                (Some(t), Some(p)) => {
                    let o = p.orient.then(user);
                    Some((t, p, view, o))
                }
                _ => s.old,
            })
        };
        self.animation = None;
        self.tween = None;
        self.user = Orient::IDENTITY;
        self.words = None;
        self.held_z = None;
        self.shown = Some(Shown { path: path.clone(), picture: None, quality: Quality::Screen, texture: None, frame: 0, error: None, since: Instant::now(), old, fading: None, fade_at: Instant::now(), player: None });
        if !media {
            self.start_at = None;
        }
        if media {
            self.vp = Viewport::new(1, 1, 1.0, 1.0, true);
            // A time asked for when View opened, else where it was left.
            let asked = self.start_at.take().filter(|_| fresh);
            self.start_at = asked.or_else(|| self.resume_point(&path));
            self.start_player(&path);
        } else if let Some((quality, picture)) = cached {
            self.present(picture, quality);
        } else {
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_LOADING, KEEP_OLD.as_millis() as u32, None);
            }
        }
        if self.settings.info_panel {
            self.camera = None;
        }
        self.request();
        self.set_title();
        self.invalidate();
    }

    /// Ask for what the current image and its neighbours need, and drop work that's stale.
    fn request(&mut self) {
        let Some(current) = self.folder.current().map(|e| e.path.clone()) else { return };
        let max_full = full_pixel_limit();
        let screen = self.screen;
        let screen_pixels = screen.0 as u64 * screen.1 as u64;
        let job = |path: &Path, task, priority| Job { path: path.to_path_buf(), task, priority, screen, max_pixels: if task == Task::Decode(Quality::Screen) { screen_pixels.max(1) } else { max_full } };

        let have_current = self.shown.as_ref().is_some_and(|s| s.picture.is_some() || s.error.is_some() || s.player.is_some());
        let media = is_media(&current);
        if !have_current && !media {
            self.pool.submit(job(&current, Task::Decode(Quality::Screen), 0));
        }
        if self.wants_full() {
            self.pool.submit(job(&current, Task::Decode(Quality::Full), 1));
        }
        if self.settings.info_panel && !media && self.camera.as_ref().is_none_or(|(p, _)| *p != current) {
            self.pool.submit(job(&current, Task::Metadata, 2));
        }
        if self.settings.info_panel && media && self.art.as_ref().is_none_or(|a| a.path != current) {
            self.start_art(current.clone());
        }
        let near = self.folder.neighbours(self.forward, 4, self.settings.wrap);
        let near_paths: Vec<PathBuf> = near.iter().map(|&i| self.folder.entries[i].path.clone()).filter(|p| !is_media(p)).collect();
        for (n, path) in near_paths.iter().enumerate() {
            if !self.cache.has(path, Quality::Screen) {
                self.pool.submit(job(path, Task::Decode(Quality::Screen), 10 + n as u32));
            }
        }
        let wants_full = self.wants_full();
        let keep_current = current.clone();
        self.pool.retain(|p, task| match task {
            Task::Decode(Quality::Screen) => *p == keep_current || near_paths.iter().any(|n| n == p),
            Task::Decode(Quality::Full) => *p == keep_current && wants_full,
            Task::Metadata => *p == keep_current,
            Task::Copy => true,
        });
        self.trim_cache();
    }

    fn trim_cache(&mut self) {
        let index = self.folder.index;
        let n = self.folder.len();
        let wrap = self.settings.wrap;
        let entries = &self.folder.entries;
        let distance = |p: &Path| {
            let i = entries.iter().position(|e| e.path == p)?;
            let d = i.abs_diff(index);
            Some(if wrap { d.min(n - d) } else { d })
        };
        let current = self.shown.as_ref().map(|s| s.path.clone());
        let pinned: Vec<&Path> = current.iter().map(|p| p.as_path()).collect();
        self.cache.trim(distance, &pinned, CACHE_REACH);
    }

    /// Whether the current image needs its full resolution: it's shown bigger than the
    /// screen-sized decode can show sharply.
    fn wants_full(&self) -> bool {
        let Some(s) = &self.shown else { return false };
        let Some(p) = &s.picture else { return false };
        if s.quality == Quality::Full || p.is_full() {
            return false;
        }
        let target = self.tween.as_ref().map_or(self.vp.view.scale, |t| t.target().scale.max(self.vp.view.scale));
        target * p.bitmap_scale() > 1.001
    }

    /// Put a decoded image on screen.
    fn present(&mut self, picture: Arc<Picture>, quality: Quality) {
        let Some(gpu) = &self.gpu else {
            // No GPU yet: keep it, and it's uploaded once there is one.
            if let Some(s) = &mut self.shown {
                s.picture = Some(picture);
                s.quality = quality;
            }
            return;
        };
        let Some(s) = &mut self.shown else { return };
        let upgrade = s.picture.as_ref().is_some_and(|p| p.width == picture.width && p.height == picture.height);
        // The file's own orientation changed (its turn was saved): keep it the same on screen.
        if let Some(old) = s.picture.as_ref().filter(|p| upgrade && p.orient != picture.orient) {
            self.user = picture.orient.to(old.orient.then(self.user));
        }
        let frame = if upgrade { s.frame.min(picture.frames.len() - 1) } else { 0 };
        let texture = gpu.upload(&picture.frames[frame].bitmap);
        s.texture = texture;
        s.frame = frame;
        s.error = None;
        s.quality = quality;
        // A new image fades in over the last one; a sharper copy of the same one just swaps in.
        // The preview changes size with each picture, so it just swaps.
        if upgrade || self.peek.is_some() {
            s.old = None;
        } else {
            s.fading = s.old.take();
            s.fade_at = Instant::now();
        }
        let animated = picture.is_animated() || picture.streams;
        let (w, h) = picture.orient.then(self.user).shown_size(picture.width, picture.height);
        s.picture = Some(picture.clone());
        if !upgrade {
            let (aw, ah) = self.area();
            self.vp = Viewport::new(w, h, aw as f64, ah as f64, self.settings.upscale_small);
            self.tween = None;
            if let Some((scale, fx, fy)) = self.kept.take() {
                let (aw, ah) = (aw as f64, ah as f64);
                let scale = scale.clamp(self.vp.min_scale(), view_core::viewport::MAX_SCALE);
                let view = View { scale, cx: aw / 2.0 - (fx - 0.5) * w as f64 * scale, cy: ah / 2.0 - (fy - 0.5) * h as f64 * scale };
                self.vp.view = self.vp.clamped(view);
                self.vp.mode = Mode::Free;
                if self.wants_full() {
                    self.request();
                }
            }
            if animated {
                self.start_animation(&picture);
            }
        }
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_LOADING);
        }
        if !upgrade {
            self.want_words();
        }
        if !upgrade && self.peek.is_some() {
            self.peek_reveal();
        }
        self.invalidate();
    }

    fn start_animation(&mut self, p: &Arc<Picture>) {
        let delay = p.frames[0].delay_ms.max(20);
        let mut a = Animation { due: Instant::now() + Duration::from_millis(delay as u64), plays: 0, stream: None };
        if p.streams {
            let (tx, rx) = sync_channel::<Frame>(2);
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = stop.clone();
            let path = self.shown.as_ref().map(|s| s.path.clone()).unwrap_or_default();
            let format = p.details.format;
            let _ = std::thread::Builder::new().name("animation".into()).spawn(move || {
                let Ok(data) = std::fs::read(&path) else { return };
                let Some(format) = format else { return };
                let mut first = true;
                let _ = view_core::decode::stream(&data, format, |f| {
                    if first {
                        first = false; // already showing
                        return !stop2.load(Ordering::Relaxed);
                    }
                    !stop2.load(Ordering::Relaxed) && tx.send(f).is_ok()
                });
            });
            a.stream = Some((rx, stop));
        }
        self.animation = Some(a);
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_ANIMATION, delay, None);
        }
    }

    fn tick_animation(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_ANIMATION);
        }
        let Some(gpu) = &self.gpu else { return };
        let Some(s) = &mut self.shown else { return };
        let Some(p) = s.picture.clone() else { return };
        let Some(a) = &mut self.animation else { return };
        let next_delay;
        if let Some((rx, _)) = &a.stream {
            match rx.try_recv() {
                Ok(frame) => {
                    s.texture = gpu.upload(&frame.bitmap);
                    next_delay = frame.delay_ms.max(20);
                }
                Err(TryRecvError::Empty) => next_delay = 10, // decoder is behind: look again soon
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            let mut next = s.frame + 1;
            if next >= p.frames.len() {
                a.plays += 1;
                if p.loops != 0 && a.plays >= p.loops {
                    return; // played its count: stay on the last frame
                }
                next = 0;
            }
            s.frame = next;
            s.texture = gpu.upload(&p.frames[next].bitmap);
            next_delay = p.frames[next].delay_ms.max(20);
        }
        // Keep time against the clock, not the timer, so slow frames don't drift.
        let now = Instant::now();
        a.due = (a.due + Duration::from_millis(next_delay as u64)).max(now);
        let wait = a.due.saturating_duration_since(now).as_millis().max(1) as u32;
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_ANIMATION, wait, None);
        }
        self.invalidate();
    }

    fn take_done(&mut self) {
        for done in self.pool.take_done() {
            self.finished(done);
        }
    }

    fn finished(&mut self, done: Done) {
        let current = self.shown.as_ref().map(|s| s.path.clone());
        let is_current = current.as_deref() == Some(done.path.as_path());
        match done.output {
            Output::Picture(p) => {
                self.cache.put(done.path.clone(), done.quality, done.stamp, p.clone());
                if is_current {
                    let better = self.shown.as_ref().is_some_and(|s| s.picture.is_none() || done.quality > s.quality);
                    if better {
                        trace(&format!("shown {}", done.path.display()));
                        self.present(p, done.quality);
                        self.request();
                    }
                }
                self.trim_cache();
            }
            Output::Failed { message, missing, .. } => {
                if missing {
                    self.cache.remove(&done.path);
                }
                if is_current && done.task != Task::Copy {
                    if let Some(s) = &mut self.shown {
                        if s.picture.is_none() && s.player.is_none() {
                            s.error = Some(message);
                            s.old = None;
                        }
                    }
                    if self.peek.as_ref().is_some_and(|p| p.pending) {
                        self.peek_reveal();
                    }
                    self.invalidate();
                } else if done.task == Task::Copy {
                    self.say(&message);
                }
            }
            Output::Camera(c) => {
                if is_current {
                    self.camera = Some((done.path, c));
                    self.invalidate();
                }
            }
            Output::Copy { width, height, pbgra, png } => {
                let ok = shell::copy_image(self.hwnd, width, height, &pbgra, png.as_deref());
                self.say(if ok { "Image copied" } else { "Couldn't copy the image" });
            }
        }
    }

    fn set_title(&self) {
        let title = match self.folder.current() {
            Some(e) => format!("{} - View", e.name),
            None => "View".into(),
        };
        let t = Wide::new(&title);
        unsafe {
            let _ = SetWindowTextW(self.hwnd, t.pcwstr());
        }
    }

    /// A short message at the top of the window.
    pub fn say(&mut self, text: &str) {
        self.say_for(text, Duration::from_millis(1600));
    }

    /// A message that stays up for `time`.
    pub fn say_for(&mut self, text: &str, time: Duration) {
        self.toast = Some((text.to_string(), Instant::now()));
        self.undo_until = None;
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_TOAST, time.as_millis() as u32, None);
        }
        self.invalidate();
    }

    // ---- Zoom and pan ----

    fn set_view(&mut self, before: View, animate: bool) {
        if animate && self.settings.smooth_zoom && before != self.vp.view {
            self.tween = Some(Tween::new(before, self.vp.view, ZOOM_TIME));
        } else {
            self.tween = None;
        }
        if self.wants_full() {
            self.request();
        }
        self.invalidate();
    }

    fn zoom_about(&mut self, factor: f64, x: f64, y: f64, animate: bool) {
        let before = self.current_view();
        // Start from where a moving zoom has got to, aiming further on from its target.
        if let Some(t) = &self.tween {
            self.vp.view = t.target();
        }
        self.vp.zoom_by(factor, x, y);
        self.set_view(before, animate);
    }

    fn zoom_step(&mut self, up: bool) {
        let before = self.current_view();
        if let Some(t) = &self.tween {
            self.vp.view = t.target();
        }
        let (x, y) = self.zoom_point();
        let target = self.vp.step(up);
        self.vp.zoom_to(target, x, y);
        self.set_view(before, true);
    }

    /// Zoom about the cursor if it's over the image area, else the area's centre.
    fn zoom_point(&self) -> (f64, f64) {
        let (aw, ah) = self.area();
        let (mx, my) = self.mouse;
        if mx >= 0 && my >= 0 && (mx as f32) < aw && (my as f32) < ah && self.ui.mouse_inside {
            (mx as f64, my as f64)
        } else {
            (aw as f64 / 2.0, ah as f64 / 2.0)
        }
    }

    fn fit(&mut self) {
        let before = self.current_view();
        self.vp.fit();
        self.set_view(before, true);
    }

    fn actual_size(&mut self) {
        let before = self.current_view();
        let (x, y) = self.zoom_point();
        self.vp.zoom_to(1.0, x, y);
        self.set_view(before, true);
    }

    /// Before going to another file: with "Keep zoom between photos" on and a photo zoomed in,
    /// note how, so the next one opens the same way.
    fn keep_zoom(&mut self, next_is_media: bool) {
        let on = self.settings.keep_zoom && self.peek.is_none() && !next_is_media;
        if !on {
            self.kept = None;
            return;
        }
        // Still waiting for the last photo: whatever it was going to get, the next one gets.
        if self.shown.as_ref().is_none_or(|s| s.picture.is_none()) {
            return;
        }
        let (view, mode) = self.held_z.unwrap_or((self.tween.as_ref().map_or(self.vp.view, |t| t.target()), self.vp.mode));
        self.kept = (mode == Mode::Free).then(|| {
            let (w, h) = (self.vp.image_w * view.scale, self.vp.image_h * view.scale);
            let fx = 0.5 + (self.vp.area_w / 2.0 - view.cx) / w;
            let fy = 0.5 + (self.vp.area_h / 2.0 - view.cy) / h;
            (view.scale, fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0))
        });
    }

    /// Z held down: 100% about the cursor. Let go: back to the view from before.
    fn hold_z(&mut self, down: bool) {
        let has_image = self.shown.as_ref().is_some_and(|s| s.picture.is_some());
        if down {
            let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
            if self.held_z.is_some() || !has_image || ctrl || self.dialog.is_some() || self.editor.is_some() || self.grid.is_some() {
                return;
            }
            let view = self.tween.as_ref().map_or(self.vp.view, |t| t.target());
            self.held_z = Some((view, self.vp.mode));
            self.actual_size();
        } else if let Some((view, mode)) = self.held_z.take() {
            if !has_image {
                return;
            }
            let before = self.current_view();
            self.vp.view = self.vp.clamped(view);
            self.vp.mode = mode;
            if mode == Mode::Fit {
                self.vp.fit();
            }
            self.set_view(before, true);
        }
    }

    /// Save the photo's turn in its EXIF Orientation tag: lossless, since the pixels aren't
    /// saved again.
    fn save_rotation(&mut self) {
        let Some((path, stored)) = self.shown.as_ref().and_then(|s| Some((s.path.clone(), s.picture.as_ref()?.orient))) else { return };
        if self.peek.is_some() {
            return;
        }
        if self.user == Orient::IDENTITY {
            self.say("It isn't turned, so there's nothing to save");
            return;
        }
        if !shell::can_save_orientation(&path) {
            self.say("Only JPEG and TIFF photos can keep a turn without losing quality");
            return;
        }
        let shown = stored.then(self.user);
        match shell::save_orientation(&path, shown.to_exif() as u16) {
            Ok(()) => {
                // What's on screen is already right; the next decode reads the new tag.
                self.cache.remove(&path);
                self.say("Rotation saved");
            }
            Err(e) => {
                trace(&format!("save orientation: {e}"));
                let denied = e.code().0 as u32 == 0x80070005;
                self.say(if denied { "Couldn't save the rotation: the file is read-only or protected" } else { "Couldn't save the rotation" });
            }
        }
    }

    fn turn(&mut self, o: Orient) {
        if self.shown.as_ref().and_then(|s| s.picture.as_ref()).is_none() {
            return;
        }
        self.held_z = None;
        self.user = o;
        let p = self.shown.as_ref().unwrap().picture.clone().unwrap();
        let (w, h) = p.orient.then(self.user).shown_size(p.width, p.height);
        self.vp.upscale_small = self.settings.upscale_small;
        self.vp.set_image(w, h);
        self.tween = None;
        // The first turn of a photo that can keep it: say how.
        let path = self.shown.as_ref().map(|s| s.path.clone()).unwrap_or_default();
        if self.peek.is_none() && o != Orient::IDENTITY && shell::can_save_orientation(&path) {
            self.say("Ctrl+S saves the rotation");
        }
        self.want_words();
        self.invalidate();
    }

    // ---- Commands ----

    pub fn run(&mut self, cmd: Cmd) {
        if self.dialog.is_some() {
            return;
        }
        if self.editor.is_some() && !cmd.while_editing() {
            return;
        }
        let has_image = self.shown.as_ref().is_some_and(|s| s.picture.is_some());
        let path = self.folder.current().map(|e| e.path.clone());
        match cmd {
            Cmd::ZoomIn | Cmd::ZoomOut if self.grid.is_some() => self.grid_resize(cmd == Cmd::ZoomIn),
            Cmd::Speed(i) => self.set_speed(crate::cmd::SPEEDS[(i as usize).min(crate::cmd::SPEEDS.len() - 1)]),
            Cmd::Faster | Cmd::Slower => self.speed_step(cmd == Cmd::Faster, false),
            Cmd::SpeedMenu => self.speed_menu(),
            Cmd::SetWallpaper => self.set_wallpaper(),
            Cmd::Undo => self.undo_delete(),
            Cmd::Grid => self.toggle_grid(),
            Cmd::Filmstrip => {
                self.settings.filmstrip = !self.settings.filmstrip;
                self.strip_at = self.folder.index as f32;
                self.changed();
            }
            Cmd::Edit => self.start_edit(),
            Cmd::EditSave | Cmd::EditCancel | Cmd::EditReset | Cmd::EditAspect(_) | Cmd::EditTurn(_) => self.edit_command(cmd),
            Cmd::Open => {
                let exts = self.extensions.lock().unwrap().clone();
                let exts = if exts.is_empty() { supported_extensions() } else { exts };
                if let Some(p) = shell::open_dialog(self.hwnd, self.folder_dir.as_deref(), &exts) {
                    self.open(p);
                }
            }
            Cmd::Next => self.step(1),
            Cmd::Prev => self.step(-1),
            Cmd::First => {
                self.forward = true;
                self.go(0, false)
            }
            Cmd::Last => {
                self.forward = false;
                if !self.folder.is_empty() {
                    self.go(self.folder.len() - 1, false)
                }
            }
            Cmd::ZoomIn => self.zoom_step(true),
            Cmd::ZoomOut => self.zoom_step(false),
            Cmd::Fit => self.fit(),
            Cmd::Actual => self.actual_size(),
            Cmd::FitOrActual => {
                if self.vp.is_fitted() && (self.vp.view.scale - 1.0).abs() > 1e-6 { self.actual_size() } else { self.fit() }
            }
            Cmd::Fullscreen => self.toggle_fullscreen(),
            Cmd::RotateCw => self.turn(self.user.rotated_cw()),
            Cmd::RotateCcw => self.turn(self.user.rotated_ccw()),
            Cmd::FlipH => self.turn(self.user.flipped_horizontally()),
            Cmd::FlipV => self.turn(self.user.flipped_vertically()),
            Cmd::SaveRotation => self.save_rotation(),
            Cmd::CopyImage => {
                if self.words.as_ref().is_some_and(|w| w.has_selection()) || self.ui.pick.has_selection() {
                    return self.run(Cmd::CopyText);
                }
                if let Some(player) = self.player().filter(|p| p.has_video()) {
                    let ok = match player.frame_pixels() {
                        Some((w, h, pixels)) => {
                            let png = crate::wic::Wic::new().and_then(|wic| wic.encode_png(w, h, &pixels));
                            shell::copy_image(self.hwnd, w, h, &pixels, png.as_deref())
                        }
                        None => false,
                    };
                    self.say(if ok { "Frame copied" } else { "Couldn't copy the frame" });
                } else if let (Some(p), true) = (&path, has_image) {
                    self.pool.submit(Job { path: p.clone(), task: Task::Copy, priority: 0, screen: self.screen, max_pixels: full_pixel_limit() });
                    self.say("Copying…");
                }
            }
            Cmd::CopyFile => {
                if let Some(p) = &path {
                    let ok = shell::copy_file(self.hwnd, p);
                    self.say(if ok { "File copied" } else { "Couldn't copy the file" });
                }
            }
            Cmd::CopyPath => {
                if let Some(p) = &path {
                    let ok = shell::copy_text(self.hwnd, &p.to_string_lossy());
                    self.say(if ok { "Path copied" } else { "Couldn't copy the path" });
                }
            }
            Cmd::CopyText => {
                let words = self.ui.pick.selected_text().or_else(|| self.words.as_ref().and_then(|w| w.selected_text()));
                if let Some(text) = words.or_else(|| self.peek.as_ref().and_then(|p| p.doc.as_ref()).and_then(|d| d.selected_text())) {
                    let ok = shell::copy_text(self.hwnd, &text);
                    self.say(if ok { "Copied" } else { "Couldn't copy the text" });
                }
            }
            Cmd::Find => self.open_bar(BarKind::Find),
            Cmd::GoToPage => self.open_bar(BarKind::Page),
            Cmd::FindNext | Cmd::FindPrev => {
                if let Some(doc) = self.peek.as_mut().and_then(|p| p.doc.as_mut()) {
                    doc.find_step(cmd == Cmd::FindNext);
                }
                self.invalidate();
            }
            Cmd::CloseBar => {
                if let Some(peek) = &mut self.peek {
                    peek.bar = None;
                    if let Some(doc) = &mut peek.doc {
                        doc.end_find();
                    }
                }
                self.selection_changed();
            }
            Cmd::Reveal => {
                if let Some(p) = &path {
                    shell::reveal(p);
                }
            }
            Cmd::OpenWith => {
                if let Some(p) = &path {
                    shell::open_with(self.hwnd, p);
                }
            }
            Cmd::Properties => {
                if let Some(p) = &path {
                    shell::properties(self.hwnd, p);
                }
            }
            Cmd::Delete => self.delete(),
            Cmd::Info => {
                self.settings.info_panel = !self.settings.info_panel;
                self.changed();
                self.relayout();
                self.request();
            }
            Cmd::Reload => {
                if let Some(p) = &path {
                    self.cache.remove(p);
                    let i = self.folder.index;
                    self.go(i, true);
                    self.rescan();
                }
            }
            Cmd::Close if self.peek.is_some() => self.peek_hide(),
            Cmd::Close => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            Cmd::OpenInView => {
                // This process had the click, so it may hand the foreground on.
                unsafe {
                    let _ = AllowSetForegroundWindow(ASFW_ANY);
                }
                let font = self.peek.as_ref().is_some_and(|p| p.font.is_some()).then(|| path.clone()).flatten();
                let other = self.peek.as_ref().and_then(|p| p.card.as_ref().map(|c| c.path.clone()).or_else(|| p.doc.as_ref().map(|d| d.path.clone()))).or(font);
                if let Some(file) = other {
                    // Not a picture or something to play: whatever Windows opens it with.
                    shell::open_default(&file);
                    self.peek_hide();
                } else if let (Some(p), Ok(exe)) = (path, std::env::current_exe()) {
                    // A video carries on from where the preview had got to.
                    let at = self.player().filter(|pl| pl.position() > 1.0).map(|pl| format!("{:.2}", pl.position()));
                    let mut command = std::process::Command::new(exe);
                    command.arg(p);
                    if let Some(at) = at {
                        command.arg("--at").arg(at);
                    }
                    let _ = command.spawn();
                    self.peek_hide();
                }
            }
            Cmd::PlayPause => {
                if let Some(p) = self.player_mut() {
                    p.toggle();
                }
                self.ui.show(true);
                self.arm_idle();
                self.invalidate();
            }
            Cmd::Mute => {
                let (v, m) = (self.settings.volume, !self.settings.muted);
                self.set_volume(if v < 0.02 { 1.0 } else { v }, m);
            }
            Cmd::Skip(secs) => {
                if let Some(p) = self.player_mut() {
                    let t = p.position() + secs as f64;
                    p.seek(t, true);
                }
                self.ui.show(true);
                self.arm_idle();
                self.invalidate();
            }
            Cmd::StepFrame(forward) => {
                if let Some(p) = self.player_mut() {
                    p.step_frame(forward);
                }
                self.ui.show(true);
                self.arm_idle();
                self.invalidate();
            }
            Cmd::SpacePreview => {
                let on = !crate::spacebar::enabled();
                if crate::spacebar::set_enabled(on) {
                    if on {
                        self.show_dialog(ui::Dialog::notice(
                            "Space preview is on",
                            "Select a file in File Explorer or on the desktop and press Space to see it. Press Space or Esc again to close it; the arrow keys move to the next file.\n\nIt starts when you sign in. Turn it off here in the same menu.",
                        ));
                    } else {
                        self.say("Space preview is off");
                    }
                } else {
                    self.say("Couldn't change Space preview");
                }
            }
            Cmd::Minimize => unsafe {
                let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
            },
            Cmd::Maximize => unsafe {
                let _ = ShowWindow(self.hwnd, if self.is_maximized() { SW_RESTORE } else { SW_MAXIMIZE });
            },
            Cmd::ToggleUi => {
                self.ui.hidden = !self.ui.hidden;
                self.invalidate();
            }
            Cmd::Menu => {
                let (x, y) = self.ui.menu_anchor.unwrap_or(self.mouse);
                let mut pt = POINT { x, y };
                unsafe {
                    let _ = ClientToScreen(self.hwnd, &mut pt);
                }
                self.context_menu(pt.x, pt.y);
            }
            Cmd::About => {
                let exts = self.extensions.lock().unwrap().clone();
                let exts = if exts.is_empty() { supported_extensions() } else { exts };
                self.show_dialog(ui::Dialog::notice(format!("View {}", env!("CARGO_PKG_VERSION")), about_text(&exts)));
            }
            Cmd::Theme(t) => {
                self.settings.theme = t;
                self.restyle();
            }
            Cmd::Backdrop(b) => {
                self.settings.backdrop = b;
                self.restyle();
            }
            Cmd::Transparency(t) => {
                self.settings.transparency = t;
                self.changed();
            }
            Cmd::Sort(k) => {
                self.settings.sort = k;
                self.resort();
            }
            Cmd::SortDescending => {
                self.settings.sort_descending = !self.settings.sort_descending;
                self.resort();
            }
            Cmd::Toolbar(t) => {
                self.settings.toolbar = t;
                self.ui.show(true);
                self.changed();
            }
            Cmd::Wheel(w) => {
                self.settings.wheel = w;
                self.changed();
            }
            Cmd::PeekMode(m) => {
                self.settings.peek_mode = m;
                self.changed();
            }
            Cmd::UpscaleSmall => {
                self.settings.upscale_small = !self.settings.upscale_small;
                self.vp.upscale_small = self.settings.upscale_small;
                if self.vp.is_fitted() {
                    self.vp.fit();
                }
                self.changed();
                self.request();
            }
            Cmd::SmoothZoom => {
                self.settings.smooth_zoom = !self.settings.smooth_zoom;
                self.changed();
            }
            Cmd::KeepZoom => {
                self.settings.keep_zoom = !self.settings.keep_zoom;
                self.changed();
            }
            Cmd::ResumeVideos => {
                self.settings.resume_videos = !self.settings.resume_videos;
                if self.settings.resume_videos {
                    // Start with what this session already knows.
                    let file = util::positions_file();
                    let mut positions = view_core::resume::Positions::load(&file);
                    for (path, &at) in &self.video_places {
                        positions.remember(&path.to_string_lossy(), at, None);
                    }
                    let _ = positions.save(&file);
                } else {
                    // Off: forget what was kept.
                    let _ = std::fs::remove_file(util::positions_file());
                }
                self.changed();
            }
            Cmd::PixelatedZoom => {
                self.settings.pixelated_zoom = !self.settings.pixelated_zoom;
                self.changed();
            }
            Cmd::Wrap => {
                self.settings.wrap = !self.settings.wrap;
                self.changed();
                self.request();
            }
            Cmd::ConfirmDelete => {
                self.settings.confirm_delete = !self.settings.confirm_delete;
                self.changed();
            }
            Cmd::EscapeCloses => {
                self.settings.escape_closes = !self.settings.escape_closes;
                self.changed();
            }
        }
    }

    fn resort(&mut self) {
        self.folder.resort(self.settings.sort, self.settings.sort_descending);
        self.changed();
        self.request();
    }

    /// Settings changed: redraw, and save shortly.
    fn changed(&mut self) {
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_SAVE, 800, None);
        }
        self.invalidate();
    }

    fn save_settings(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_SAVE);
        }
        let _ = self.settings.save(&util::settings_file());
    }

    fn restyle(&mut self) {
        self.palette = theme::palette(&self.settings);
        style_frame(self.hwnd, &self.palette);
        menu::set_dark(self.palette.dark);
        self.changed();
    }

    /// The area changed size: refit or keep the zoom, and resize the swap chain.
    fn relayout(&mut self) {
        let (w, h) = self.client_size();
        if let Some(gpu) = &mut self.gpu {
            if gpu.resize(w as u32, h as u32).is_err() {
                self.lost_device();
            }
        }
        let (aw, ah) = self.area();
        self.vp.resize(aw as f64, ah as f64);
        self.tween = None;
        if self.wants_full() {
            self.request();
        }
        self.invalidate();
    }

    fn delete(&mut self) {
        // In the grid, the one selected there.
        let picked = self.grid.as_ref().and_then(|g| self.folder.entries.get(g.selected));
        let Some(entry) = picked.or(self.folder.current()).cloned() else { return };
        if !self.settings.confirm_delete {
            self.recycle(&entry.path);
            return;
        }
        self.show_dialog(ui::Dialog {
            ask: ui::Ask::Delete(entry.path.clone()),
            title: "Delete this image?".into(),
            body: format!("\u{201C}{}\u{201D} goes to the Recycle Bin, so you can still get it back.", entry.name),
            check: Some(("Don't ask me again", false)),
            buttons: vec!["Delete", "Cancel"],
            focus: 0,
            keyed: false,
        });
    }

    /// Move `path` to the Recycle Bin and step to the next image if it was the one showing.
    fn recycle(&mut self, path: &Path) {
        let showing = self.shown.as_ref().is_some_and(|s| s.path == path);
        if showing {
            // Let go of anything that could hold the file open.
            self.animation = None;
        }
        match shell::recycle(self.hwnd, path) {
            Ok(()) => {
                self.cache.remove(path);
                self.thumbs.forget(path);
                if let Some(i) = self.folder.entries.iter().position(|e| e.path == path) {
                    self.folder.remove(i);
                }
                if self.folder.is_empty() {
                    self.shown = None;
                    self.set_title();
                    self.invalidate();
                } else if showing {
                    let i = self.folder.index.min(self.folder.len() - 1);
                    self.go(i, true);
                }
                if let Some(g) = &mut self.grid {
                    g.selected = g.selected.min(self.folder.len().saturating_sub(1));
                }
                self.deleted.push(path.to_path_buf());
                self.say_for("Moved to the Recycle Bin", tools::UNDO_TIME);
                self.undo_until = Some(Instant::now() + tools::UNDO_TIME);
            }
            Err(e) if e == "cancelled" => {}
            Err(e) => self.show_dialog(ui::Dialog::notice("View couldn't delete this file", e)),
        }
    }

    fn show_dialog(&mut self, d: ui::Dialog) {
        self.dialog = Some(d);
        self.drag = None;
        self.ui.pressed = None;
        self.show_cursor(true);
        // The controls under the mouse are behind the dialog now.
        self.ui.hover = None;
        self.invalidate();
    }

    /// Close the dialog with one of its buttons, or `None` for Esc.
    fn answer(&mut self, button: Option<usize>) {
        let Some(d) = self.dialog.take() else { return };
        self.ui.hover = None;
        self.invalidate();
        match (d.ask, button) {
            (ui::Ask::Delete(path), Some(0)) => {
                if d.check.is_some_and(|(_, ticked)| ticked) {
                    self.settings.confirm_delete = false;
                    self.changed();
                }
                self.recycle(&path);
            }
            (ui::Ask::Discard, Some(0)) => self.edit_cancel(true),
            _ => {}
        }
    }

    /// Keys while a dialog is up. Everything is swallowed, so nothing happens behind it.
    fn dialog_key(&mut self, key: VIRTUAL_KEY, shift: bool) {
        let Some(d) = &mut self.dialog else { return };
        let n = d.buttons.len();
        match key {
            VK_RETURN => {
                let f = d.focus;
                self.answer(Some(f));
            }
            VK_C | VK_INSERT if unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0 => {
                if self.ui.pick.has_selection() {
                    self.run(Cmd::CopyText);
                }
            }
            VK_ESCAPE | VK_N => self.answer(None),
            VK_DELETE if matches!(d.ask, ui::Ask::Delete(_)) => self.answer(Some(0)),
            VK_SPACE => {
                if let Some((_, ticked)) = &mut d.check {
                    *ticked = !*ticked;
                }
            }
            VK_TAB if shift => {
                d.focus = (d.focus + n - 1) % n;
                d.keyed = true;
            }
            VK_TAB | VK_LEFT | VK_RIGHT => {
                d.focus = (d.focus + 1) % n;
                d.keyed = true;
            }
            _ => {}
        }
        self.invalidate();
    }

    fn context_menu(&mut self, x: i32, y: i32) {
        if self.dialog.is_some() {
            return;
        }
        self.ui.show(true);
        if let Some(cmd) = menu::show(self, x, y) {
            self.run(cmd);
        }
    }

    // ---- Window states ----

    fn toggle_fullscreen(&mut self) {
        unsafe {
            if let Some(saved) = self.fullscreen.take() {
                SetWindowLongPtrW(self.hwnd, GWL_STYLE, saved.style);
                let _ = SetWindowPlacement(self.hwnd, &saved.placement);
                let _ = SetWindowPos(self.hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
                self.show_cursor(true);
            } else {
                let mut placement = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                let _ = GetWindowPlacement(self.hwnd, &mut placement);
                let style = GetWindowLongPtrW(self.hwnd, GWL_STYLE);
                let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
                let _ = GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
                let r = mi.rcMonitor;
                SetWindowLongPtrW(self.hwnd, GWL_STYLE, (style & !(WS_OVERLAPPEDWINDOW.0 as isize)) | WS_POPUP.0 as isize);
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
                self.fullscreen = Some(Saved { placement, style });
            }
        }
        self.ui.show(true);
        self.arm_idle();
        self.relayout();
    }

    /// The thickness of the sizing frame Windows puts round a window, in pixels at this DPI.
    fn frame_thickness(&self) -> i32 {
        use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
        unsafe { GetSystemMetricsForDpi(SM_CYFRAME, self.dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, self.dpi) }
    }

    /// What part of the window a screen point is over, for Windows: the top strip works as the
    /// title bar, the maximise button opens Windows 11's snap layouts, the rest is View's own.
    fn hit_test(&self, sx: i32, sy: i32, default: LRESULT) -> LRESULT {
        if default.0 != HTCLIENT as isize || self.is_fullscreen() {
            return default;
        }
        let mut pt = POINT { x: sx, y: sy };
        unsafe {
            let _ = ScreenToClient(self.hwnd, &mut pt);
        }
        let (cw, _) = self.client_size();
        let edge = self.frame_thickness();
        if !self.is_maximized() && pt.y < edge && self.dialog.is_none() {
            let corner = edge * 2;
            return LRESULT(if pt.x < corner {
                HTTOPLEFT
            } else if pt.x as f32 >= cw - corner as f32 {
                HTTOPRIGHT
            } else {
                HTTOP
            } as isize);
        }
        match self.ui.hit(pt.x as f32, pt.y as f32) {
            Some(ui::Hit::Maximize) => LRESULT(HTMAXBUTTON as isize),
            Some(_) => LRESULT(HTCLIENT as isize),
            None if (pt.y as f32) < (ui::TITLE * self.scale()).round() && self.dialog.is_none() => LRESULT(HTCAPTION as isize),
            None => LRESULT(HTCLIENT as isize),
        }
    }

    /// The mouse over the title strip or the maximise button, which Windows reports as outside
    /// the window's own area.
    fn on_nc_mouse_move(&mut self, code: u32, sx: i32, sy: i32) {
        let mut pt = POINT { x: sx, y: sy };
        unsafe {
            let _ = ScreenToClient(self.hwnd, &mut pt);
        }
        self.mouse = (pt.x, pt.y);
        if !self.nc_tracking {
            let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE | TME_NONCLIENT, hwndTrack: self.hwnd, dwHoverTime: 0 };
            unsafe {
                let _ = TrackMouseEvent(&mut t);
            }
            self.nc_tracking = true;
        }
        let hover = (code == HTMAXBUTTON).then_some(ui::Hit::Maximize);
        if hover != self.ui.hover {
            self.ui.hover = hover;
        }
        self.ui.over_controls = hover.is_some();
        self.ui.last_move = Instant::now();
        self.ui.show(true);
        self.arm_idle();
        self.invalidate();
    }

    fn arm_idle(&self) {
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_IDLE, IDLE.as_millis() as u32, None);
        }
    }

    fn show_cursor(&mut self, show: bool) {
        if self.cursor_hidden == !show {
            return;
        }
        self.cursor_hidden = !show;
        unsafe {
            SetCursor(if show { LoadCursorW(None, IDC_ARROW).ok() } else { None });
        }
    }

    /// Whether the toolbar should be up right now.
    fn toolbar_wanted(&self) -> bool {
        if self.ui.hidden {
            return false;
        }
        // Paused, a video keeps its controls up; playing, they rest like a picture's.
        if let Some(p) = self.player() {
            if p.error.is_some() || !p.playing() || !p.has_video() {
                return true;
            }
        } else if self.shown.as_ref().is_none_or(|s| s.picture.is_none()) {
            return true;
        }
        if self.ui.over_controls {
            return true;
        }
        let mode = if self.is_fullscreen() && self.settings.toolbar == Toolbar::Always { Toolbar::Auto } else { self.settings.toolbar };
        match mode {
            Toolbar::Always => true,
            Toolbar::Auto => self.ui.last_move.elapsed() < IDLE,
            Toolbar::Hidden => false,
        }
    }

    // ---- Painting ----

    fn paint(&mut self) {
        self.follow_selection();
        let Some(mut gpu) = self.gpu.take() else { return };
        // A 3D model is drawn by Direct3D first, for Direct2D to put on screen.
        if let (Some(m), Some(d3d)) = (self.peek.as_mut().and_then(|p| p.model.as_mut()), &self.d3d) {
            m.prepare(d3d, &gpu, gpu.width, gpu.height, self.palette.dark);
        }
        gpu.begin();
        ui::draw(self, &mut gpu);
        let result = gpu.end();
        self.gpu = Some(gpu);
        if result.is_err() {
            self.lost_device();
            return;
        }
        if self.crossfade() >= 1.0 {
            if let Some(s) = &mut self.shown {
                s.fading = None;
            }
        }
        if let Some(t) = &self.tween {
            if !t.at(Instant::now()).1 {
                self.tween = None;
                if self.wants_full() {
                    self.request();
                }
            }
        }
        if self.animating() {
            self.invalidate();
        }
    }

    /// The GPU went away (a driver update, a sleep, a removed card): start again on a new one.
    fn lost_device(&mut self) {
        trace("device lost");
        self.gpu = None;
        let (w, h) = self.client_size();
        self.d3d = gfx::create_d3d();
        if let Some(d3d) = &self.d3d {
            self.gpu = Gpu::new(self.hwnd, d3d, w as u32, h as u32).ok();
        }
        if let (Some(gpu), Some(s)) = (&self.gpu, &mut self.shown) {
            s.old = None;
            s.texture = s.picture.as_ref().and_then(|p| gpu.upload(&p.frames[s.frame.min(p.frames.len() - 1)].bitmap));
        }
        // A player is tied to the old device: start it again where it was.
        if let Some((path, at, paused)) = self.player().map(|p| (p.path.clone(), p.position(), p.paused())) {
            self.start_player(&path);
            self.start_at = Some(at);
            if paused {
                if let Some(p) = self.player_mut() {
                    p.pause();
                }
            }
        }
        if let Some(a) = &mut self.art {
            a.texture = None;
        }
        self.thumbs.reset();
        if let Some(e) = &mut self.editor {
            e.reset_gpu();
        }
        if let Some(d) = self.peek.as_mut().and_then(|p| p.doc.as_mut()) {
            d.reset(true);
        }
        if let Some(m) = self.peek.as_mut().and_then(|p| p.model.as_mut()) {
            m.reset(true);
        }
        if let Some(path) = self.art.as_ref().map(|a| a.path.clone()) {
            self.start_art(path);
        }
        self.invalidate();
    }

    // ---- Input ----

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        let moved = (x, y) != self.mouse;
        self.mouse = (x, y);
        if !self.tracking {
            let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
            unsafe {
                let _ = TrackMouseEvent(&mut t);
            }
            self.tracking = true;
        }
        self.ui.mouse_inside = true;
        match self.drag {
            Some(Drag::Out { x: x0, y: y0 }) => {
                if self.past_drag_threshold(x0, y0, x, y) {
                    if let Some(path) = self.shown.as_ref().map(|s| s.path.clone()) {
                        self.drag_file_out(&path);
                    }
                }
                return;
            }
            Some(Drag::Thumb { x: x0, y: y0, index }) => {
                if self.past_drag_threshold(x0, y0, x, y) {
                    if let Some(path) = self.folder.entries.get(index).map(|e| e.path.clone()) {
                        self.drag_file_out(&path);
                    }
                }
                return;
            }
            Some(Drag::Crop) => return self.edit_crop_to(x, y),
            Some(Drag::Slider(i)) => return self.edit_slide(i, x),
            _ => {}
        }
        if moved {
            if let Some(g) = &mut self.grid {
                g.keyed = false;
            }
            // A zip's folder lights up under the mouse.
            if let Some(doc) = self.peek.as_mut().and_then(|p| p.doc.as_mut()).filter(|d| d.suits().is_some()) {
                doc.hover = Some((x as f32, y as f32));
                self.invalidate();
            }
        }
        if let Some(Drag::Seek) = self.drag {
            let f = self.ui.seek_fraction(x as f32);
            self.seek_to(f, false);
            return;
        }
        if let Some(Drag::Volume) = self.drag {
            let v = self.ui.volume_fraction(x as f32);
            self.set_volume(v, v < 0.01);
            return;
        }
        if let Some(Drag::Orbit { x: px, y: py }) = self.drag {
            if let Some(m) = self.peek.as_mut().and_then(|p| p.model.as_mut()) {
                m.turn((x - px) as f32, (y - py) as f32);
            }
            self.drag = Some(Drag::Orbit { x, y });
            self.invalidate();
            return;
        }
        if let Some(Drag::Select) = self.drag {
            // Past the top or bottom edge, painting scrolls the page to follow.
            if let Some(doc) = self.peek.as_mut().and_then(|p| p.doc.as_mut()) {
                doc.select_to(x as f32, y as f32);
            }
            self.invalidate();
            return;
        }
        if let Some(Drag::Move { dx, dy }) = self.drag {
            // Moved by hand, never activated, so Explorer keeps the keyboard.
            let (mut pt, mut r) = (POINT { x, y }, RECT::default());
            unsafe {
                let _ = ClientToScreen(self.hwnd, &mut pt);
                let _ = SetWindowPos(self.hwnd, None, pt.x - dx, pt.y - dy, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                let _ = GetWindowRect(self.hwnd, &mut r);
            }
            if let Some(peek) = &mut self.peek {
                peek.moved = Some(((r.left + r.right) / 2, (r.top + r.bottom) / 2));
            }
            return;
        }
        if let Some(Drag::Text) = self.drag {
            self.ui.pick.drag_to(x as f32, y as f32);
            self.invalidate();
            return;
        }
        if let Some(Drag::Field) = self.drag {
            self.field_click(x as f32, true, false);
            return;
        }
        if let Some(Drag::Words) = self.drag {
            if let (Some(at), Some(w)) = (self.words_placed(), self.words.as_mut()) {
                w.select_to(x as f32, y as f32, at);
            }
            self.invalidate();
            return;
        }
        if let Some(Drag::Pan { x: px, y: py, .. }) = self.drag {
            let (dx, dy) = (x - px, y - py);
            if dx != 0 || dy != 0 {
                self.tween = None;
                self.vp.pan(dx as f64, dy as f64);
                self.drag = Some(Drag::Pan { x, y });
                self.invalidate();
            }
            return;
        }
        let hit = self.ui.hit(x as f32, y as f32);
        let over = hit.is_some();
        if hit != self.ui.hover || over != self.ui.over_controls {
            self.ui.hover = hit;
            self.ui.over_controls = over;
            self.invalidate();
        }
        if moved {
            self.ui.last_move = Instant::now();
            self.show_cursor(true);
            if self.settings.toolbar == Toolbar::Hidden && !self.is_fullscreen() {
                // The minimal mode: the toolbar comes up only near the bottom edge.
                let (_, h) = self.client_size();
                let near = (y as f32) > h - 90.0 * self.scale();
                self.ui.show(near || over);
            } else {
                self.ui.show(self.toolbar_wanted());
            }
            self.arm_idle();
            self.invalidate();
        }
    }

    fn on_idle(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_IDLE);
        }
        let want = self.toolbar_wanted() && !(self.settings.toolbar == Toolbar::Hidden && !self.ui.over_controls);
        let want = if self.settings.toolbar == Toolbar::Always && !self.is_fullscreen() { !self.ui.hidden } else { want };
        self.ui.show(want);
        if self.is_fullscreen() && !self.ui.over_controls && self.drag.is_none() {
            self.show_cursor(false);
        }
        self.invalidate();
    }

    fn on_button_down(&mut self, x: i32, y: i32) {
        unsafe {
            SetCapture(self.hwnd);
        }
        self.mouse = (x, y);
        if self.peek.as_ref().is_some_and(|p| p.mode == PeekMode::Window) {
            self.peek_raise();
        }
        if self.peek_handle(x, y) {
            let (mut pt, mut r) = (POINT { x, y }, RECT::default());
            unsafe {
                let _ = ClientToScreen(self.hwnd, &mut pt);
                let _ = GetWindowRect(self.hwnd, &mut r);
            }
            self.drag = Some(Drag::Move { dx: pt.x - r.left, dy: pt.y - r.top });
            return;
        }
        let hit = self.ui.hit(x as f32, y as f32);
        if hit == Some(ui::Hit::Field) {
            let shift = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
            self.field_click(x as f32, shift, false);
            self.drag = Some(Drag::Field);
            return;
        }
        // Text in View's own interface: a press on it starts selecting it.
        if matches!(hit, None | Some(ui::Hit::Panel | ui::Hit::Modal)) && self.start_text(x, y, false) {
            return;
        }
        // A press beside it lets it go; a button (Copy, say) leaves it be.
        if matches!(hit, None | Some(ui::Hit::Panel | ui::Hit::Modal)) && self.ui.pick.clear() {
            self.selection_changed();
        }
        if let Some(hit) = hit {
            if let ui::Hit::Slider(i) = hit {
                self.drag = Some(Drag::Slider(i));
                self.ui.pressed = Some(hit);
                self.edit_slide(i, x);
                return;
            }
            if let (ui::Hit::Thumb(i), true) = (hit, self.grid.is_some()) {
                self.drag = Some(Drag::Thumb { x, y, index: i as usize });
                self.ui.pressed = Some(hit);
                self.invalidate();
                return;
            }
            if hit == ui::Hit::Seek {
                self.drag = Some(Drag::Seek);
                self.ui.pressed = Some(hit);
                let f = self.ui.seek_fraction(x as f32);
                self.seek_to(f, false);
                return;
            }
            if hit == ui::Hit::Volume {
                self.drag = Some(Drag::Volume);
                self.ui.pressed = Some(hit);
                let v = self.ui.volume_fraction(x as f32);
                self.set_volume(v, v < 0.01);
                return;
            }
            self.drag = Some(Drag::Press(hit));
            self.ui.pressed = Some(hit);
            self.invalidate();
            return;
        }
        if self.editor.is_some() {
            return self.edit_press(x, y);
        }
        if self.grid.is_some() {
            self.drag = None;
            return;
        }
        // In the preview: a zip's folder opens, a 3D model turns, and text can be selected.
        if let Some(peek) = &mut self.peek {
            if peek.doc.as_mut().is_some_and(|d| d.click(x as f32, y as f32)) {
                self.drag = None;
                self.invalidate();
                return;
            }
            if peek.model.is_some() {
                self.drag = Some(Drag::Orbit { x, y });
                return;
            }
            if let Some(doc) = peek.doc.as_mut().filter(|d| d.selectable()) {
                doc.select_start(x as f32, y as f32, false);
                self.drag = Some(Drag::Select);
                self.selection_changed();
                return;
            }
        }
        // Words in a photo: a press on them selects text instead of panning.
        if let Some(at) = self.words_placed() {
            if let Some(w) = &mut self.words {
                if w.over(x as f32, y as f32, at) {
                    w.select_start(x as f32, y as f32, at, false);
                    self.drag = Some(Drag::Words);
                    self.selection_changed();
                    return;
                }
                if w.selection.take().is_some() {
                    self.selection_changed();
                }
            }
        }
        let empty = self.shown.as_ref().is_none_or(|s| s.picture.is_none() && s.error.is_none() && s.player.is_none());
        if empty && self.folder.is_empty() {
            self.drag = None;
            return; // a click on the empty window opens a file, on release
        }
        // Dragged, the file goes out to another app or a folder.
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        if self.drags_out(ctrl) {
            self.drag = Some(Drag::Out { x, y });
            return;
        }
        // A click on a video plays or pauses it.
        let playable = self.player().is_some_and(|p| p.error.is_none());
        if !self.vp.overflows() && !self.is_fullscreen() && !self.is_maximized() && self.peek.is_none() {
            // Nothing to pan: move the window instead, as there's no title bar to hold.
            let mut before = RECT::default();
            let mut after = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.hwnd, &mut before);
                let _ = ReleaseCapture();
                let _ = SendMessageW(self.hwnd, WM_NCLBUTTONDOWN, Some(WPARAM(HTCAPTION as usize)), Some(LPARAM(0)));
                let _ = GetWindowRect(self.hwnd, &mut after);
            }
            if playable && before == after {
                self.run(Cmd::PlayPause);
            }
            return;
        }
        if playable && !self.vp.overflows() {
            self.drag = Some(Drag::Click);
            return;
        }
        self.drag = Some(Drag::Pan { x, y });
        unsafe {
            SetCursor(LoadCursorW(None, IDC_SIZEALL).ok());
        }
    }

    fn on_button_up(&mut self, x: i32, y: i32) {
        unsafe {
            let _ = ReleaseCapture();
        }
        let drag = self.drag.take();
        self.ui.pressed = None;
        match drag {
            Some(Drag::Press(hit)) => {
                if self.ui.hit(x as f32, y as f32) == Some(hit) {
                    match hit {
                        ui::Hit::DialogButton(i) => self.answer(Some(i as usize)),
                        ui::Hit::DialogCheck => {
                            if let Some((_, ticked)) = self.dialog.as_mut().and_then(|d| d.check.as_mut()) {
                                *ticked = !*ticked;
                            }
                        }
                        ui::Hit::Thumb(i) => {
                            // A thumbnail in the filmstrip.
                            let i = i as usize;
                            if i != self.folder.index {
                                self.forward = i > self.folder.index;
                                self.go(i, false);
                            }
                        }
                        _ => {}
                    }
                    if let Some(cmd) = self.ui.command(hit) {
                        if cmd == Cmd::Menu {
                            self.ui.menu_anchor = self.ui.anchor_of(hit);
                        }
                        self.run(cmd);
                        self.ui.menu_anchor = None;
                    }
                }
            }
            Some(Drag::Pan { .. }) | Some(Drag::Orbit { .. }) | Some(Drag::Volume) | Some(Drag::Field) | Some(Drag::Move { .. }) | Some(Drag::Out { .. }) => {}
            Some(Drag::Thumb { index, .. }) => {
                if self.ui.hit(x as f32, y as f32) == Some(ui::Hit::Thumb(index as u32)) {
                    self.grid_open(index);
                }
            }
            Some(Drag::Crop) | Some(Drag::Slider(_)) => {
                if let Some(ed) = &mut self.editor {
                    ed.grip = None;
                }
            }
            Some(Drag::Text) => {
                self.ui.pick.finish();
                self.selection_changed();
            }
            Some(Drag::Select) => {
                if let Some(doc) = self.peek.as_mut().and_then(|p| p.doc.as_mut()) {
                    if !doc.has_selection() {
                        doc.clear_selection();
                    }
                }
                self.selection_changed();
            }
            Some(Drag::Words) => {
                if let Some(w) = self.words.as_mut().filter(|w| !w.has_selection()) {
                    w.selection = None;
                }
                self.selection_changed();
            }
            Some(Drag::Seek) => {
                let f = self.ui.seek_fraction(x as f32);
                self.seek_to(f, true);
            }
            Some(Drag::Click) => self.run(Cmd::PlayPause),
            None => {
                if self.folder.is_empty() && self.shown.is_none() {
                    self.run(Cmd::Open);
                }
            }
        }
        unsafe {
            SetCursor(LoadCursorW(None, IDC_ARROW).ok());
        }
        self.invalidate();
    }

    fn on_wheel(&mut self, delta: i32, x: i32, y: i32, ctrl: bool) {
        if self.dialog.is_some() || self.editor.is_some() {
            return;
        }
        if self.grid.is_some() {
            if ctrl {
                self.ui.wheel_rest += delta;
                while self.ui.wheel_rest.abs() >= 120 {
                    let up = self.ui.wheel_rest > 0;
                    self.ui.wheel_rest -= if up { 120 } else { -120 };
                    self.grid_resize(up);
                }
            } else {
                self.grid_scroll(-(delta as f32) / 120.0 * 110.0 * self.scale());
            }
            return;
        }
        // Over the filmstrip, the wheel goes through the files.
        if matches!(self.ui.hit(x as f32, y as f32), Some(ui::Hit::Thumb(_) | ui::Hit::Strip)) {
            self.ui.wheel_rest += delta;
            while self.ui.wheel_rest.abs() >= 120 {
                let back = self.ui.wheel_rest > 0;
                self.ui.wheel_rest -= if back { 120 } else { -120 };
                self.step(if back { -1 } else { 1 });
            }
            return;
        }
        if self.ui.over_panel(x as f32, y as f32) {
            self.ui.scroll_panel(-(delta as f32) / 120.0 * 48.0 * self.scale());
            self.invalidate();
            return;
        }
        // Over the sound button or its slider, the wheel sets the volume. Turning it down
        // while muted leaves it muted.
        if self.player().is_some() && matches!(self.ui.hit(x as f32, y as f32), Some(ui::Hit::Button(Cmd::Mute) | ui::Hit::Volume)) {
            if self.settings.muted && delta < 0 {
                self.say("Muted");
                return;
            }
            let v = (self.settings.volume + delta as f32 / 120.0 * 0.05).clamp(0.0, 1.0);
            self.set_volume(v, v < 0.01);
            self.say(&format!("Volume {}%", (v * 100.0).round()));
            return;
        }
        let s = self.scale();
        if let Some(m) = self.peek.as_mut().and_then(|p| p.model.as_mut()) {
            m.zoom_by(1.15f32.powf(delta as f32 / 120.0));
            self.invalidate();
            return;
        }
        if let Some(doc) = self.peek.as_mut().and_then(|p| p.doc.as_mut()) {
            let step = -(delta as f32) / 120.0 * 64.0 * s;
            let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
            if shift && doc.can_scroll_sideways() { doc.scroll_by(0.0, step) } else { doc.scroll_by(step, 0.0) }
            self.ui.show(true);
            self.arm_idle();
            self.invalidate();
            return;
        }
        if self.settings.wheel == Wheel::Navigate && !ctrl {
            self.ui.wheel_rest += delta;
            while self.ui.wheel_rest >= 120 {
                self.ui.wheel_rest -= 120;
                self.step(-1);
            }
            while self.ui.wheel_rest <= -120 {
                self.ui.wheel_rest += 120;
                self.step(1);
            }
            return;
        }
        if self.shown.as_ref().and_then(|s| s.picture.as_ref()).is_none() {
            return;
        }
        let notches = delta as f64 / 120.0;
        let factor = 1.2f64.powf(notches);
        // A whole notch from a wheel glides; a touchpad's small steps follow the fingers.
        let animate = delta.abs() >= 120;
        self.zoom_about(factor, x as f64, y as f64, animate);
    }

    fn on_key(&mut self, vk: u16, alt: bool) -> bool {
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        let key = VIRTUAL_KEY(vk);
        if self.dialog.is_some() {
            self.dialog_key(key, shift);
            return true;
        }
        if self.editor.is_some() {
            return self.editor_key(key, ctrl, shift, alt);
        }
        if self.grid.is_some() && self.grid_key(key, ctrl) {
            return true;
        }
        let pan = 64.0 * self.scale() as f64;
        let playing = self.player().is_some_and(|p| p.error.is_none());
        let cmd = match key {
            VK_OEM_PERIOD if playing && shift => Cmd::Faster,
            VK_OEM_COMMA if playing && shift => Cmd::Slower,
            // Videos and songs: Space or K plays and pauses, Shift+arrows and J/L skip, M mutes.
            VK_SPACE | VK_K if playing => Cmd::PlayPause,
            VK_RIGHT if playing && shift => Cmd::Skip(5),
            VK_LEFT if playing && shift => Cmd::Skip(-5),
            VK_L if playing && !ctrl => Cmd::Skip(10),
            VK_J if playing => Cmd::Skip(-10),
            VK_M if playing => Cmd::Mute,
            VK_OEM_PERIOD if playing => Cmd::StepFrame(true),
            VK_OEM_COMMA if playing => Cmd::StepFrame(false),
            VK_RIGHT if ctrl => {
                self.vp.pan(-pan, 0.0);
                self.invalidate();
                return true;
            }
            VK_LEFT if ctrl => {
                self.vp.pan(pan, 0.0);
                self.invalidate();
                return true;
            }
            VK_UP | VK_DOWN => {
                if self.vp.overflows() {
                    self.vp.pan(0.0, if key == VK_UP { pan } else { -pan });
                    self.invalidate();
                } else if key == VK_UP {
                    self.run(Cmd::Prev);
                } else {
                    self.run(Cmd::Next);
                }
                return true;
            }
            VK_RIGHT | VK_NEXT | VK_SPACE => Cmd::Next,
            VK_LEFT | VK_PRIOR | VK_BACK => Cmd::Prev,
            VK_HOME => Cmd::First,
            VK_END => Cmd::Last,
            VK_OEM_PLUS | VK_ADD => Cmd::ZoomIn,
            VK_OEM_MINUS | VK_SUBTRACT => Cmd::ZoomOut,
            VK_0 | VK_NUMPAD0 if ctrl => Cmd::Fit,
            VK_1 | VK_NUMPAD1 if ctrl => Cmd::Actual,
            VK_F11 => Cmd::Fullscreen,
            VK_RETURN if alt => Cmd::Fullscreen,
            VK_F => Cmd::Fullscreen,
            VK_ESCAPE => {
                if self.ui.pick.clear() {
                    self.selection_changed();
                    return true;
                }
                if let Some(w) = self.words.as_mut().filter(|w| w.has_selection()) {
                    w.selection = None;
                    self.selection_changed();
                    return true;
                }
                if self.is_fullscreen() {
                    Cmd::Fullscreen
                } else if self.settings.info_panel {
                    Cmd::Info
                } else if self.settings.escape_closes {
                    Cmd::Close
                } else {
                    return true;
                }
            }
            VK_C if ctrl && shift => Cmd::CopyPath,
            VK_C if ctrl && alt => Cmd::CopyFile,
            VK_C if ctrl => Cmd::CopyImage,
            VK_DELETE => Cmd::Delete,
            VK_R if shift => Cmd::RotateCcw,
            VK_R => Cmd::RotateCw,
            VK_L if ctrl => Cmd::RotateCcw,
            VK_H => Cmd::FlipH,
            VK_V => Cmd::FlipV,
            VK_I => Cmd::Info,
            VK_O if ctrl => Cmd::Open,
            VK_S if ctrl => Cmd::SaveRotation,
            VK_E if ctrl => Cmd::Reveal,
            VK_E => Cmd::Edit,
            VK_G if !ctrl => Cmd::Grid,
            VK_W if ctrl => Cmd::Close,
            VK_F5 => Cmd::Reload,
            VK_F1 => Cmd::About,
            VK_TAB => Cmd::ToggleUi,
            VK_APPS => Cmd::Menu,
            VK_RETURN if ctrl => Cmd::Properties,
            _ => return false,
        };
        self.run(cmd);
        true
    }

    fn on_drop(&mut self, drop: HDROP) {
        let mut buf = vec![0u16; 32768];
        let n = unsafe { DragQueryFileW(drop, 0, Some(&mut buf)) } as usize;
        unsafe { DragFinish(drop) };
        if n == 0 || self.editor.is_some() {
            return;
        }
        self.grid = None;
        let path = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        if path.is_dir() {
            // A folder: open its first image.
            let exts = supported_extensions();
            let mut list = folder::scan(&path, |e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)) || view_core::kind::is_media(e), |_| true).unwrap_or_default();
            folder::sort(&mut list, self.settings.sort, self.settings.sort_descending);
            if let Some(first) = list.first() {
                self.open(first.path.clone());
            }
        } else {
            self.open(path);
        }
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if self.peek.is_some() {
            if let Some(r) = self.handle_peek(msg, wp, lp) {
                return Some(r);
            }
        }
        let point = || ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                unsafe {
                    BeginPaint(self.hwnd, &mut ps);
                    let _ = EndPaint(self.hwnd, &ps);
                }
                self.paint();
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_SIZE => {
                if wp.0 as u32 != SIZE_MINIMIZED {
                    self.relayout();
                }
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                self.dpi = (wp.0 & 0xFFFF) as u32;
                self.text.reset();
                let r = unsafe { &*(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(self.hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                }
                self.relayout();
                Some(LRESULT(0))
            }
            WM_GETMINMAXINFO => {
                let info = unsafe { &mut *(lp.0 as *mut MINMAXINFO) };
                let s = self.scale();
                info.ptMinTrackSize = POINT { x: (360.0 * s) as i32, y: (240.0 * s) as i32 };
                Some(LRESULT(0))
            }
            WM_NCCALCSIZE if wp.0 != 0 && !self.is_fullscreen() => {
                // Windows' own frame at the sides and bottom (it's invisible, for resizing, and
                // keeps the shadow and rounded corners), but no title bar: the picture runs to
                // the top. Maximised, Windows hangs the frame past the screen's edge, so the
                // top has to come in by as much.
                let params = lp.0 as *mut NCCALCSIZE_PARAMS;
                let top = unsafe { (*params).rgrc[0].top };
                let result = unsafe { DefWindowProcW(self.hwnd, msg, wp, lp) };
                let inset = if self.is_maximized() { self.frame_thickness() } else { 0 };
                unsafe { (*params).rgrc[0].top = top + inset };
                Some(result)
            }
            WM_NCHITTEST => {
                let (sx, sy) = point();
                let default = unsafe { DefWindowProcW(self.hwnd, msg, wp, lp) };
                Some(self.hit_test(sx, sy, default))
            }
            WM_NCMOUSEMOVE => {
                let (sx, sy) = point();
                self.on_nc_mouse_move(wp.0 as u32, sx, sy);
                None
            }
            WM_NCMOUSELEAVE => {
                self.nc_tracking = false;
                if self.ui.hover == Some(ui::Hit::Maximize) {
                    self.ui.hover = None;
                    self.ui.over_controls = false;
                }
                self.invalidate();
                None
            }
            // The maximise button is View's own drawing; Windows only knows where it is, so it
            // can offer snap layouts. Clicks on it are handled here, not by Windows.
            WM_NCLBUTTONDOWN if wp.0 as u32 == HTMAXBUTTON => {
                self.ui.pressed = Some(ui::Hit::Maximize);
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_NCLBUTTONUP if wp.0 as u32 == HTMAXBUTTON => {
                let was = self.ui.pressed.take();
                if was == Some(ui::Hit::Maximize) {
                    self.run(Cmd::Maximize);
                }
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_NCLBUTTONDBLCLK if wp.0 as u32 == HTMAXBUTTON => Some(LRESULT(0)),
            WM_MOUSEMOVE => {
                let (x, y) = point();
                self.on_mouse_move(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                self.ui.mouse_inside = false;
                self.ui.hover = None;
                self.ui.over_controls = false;
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = point();
                self.on_button_down(x, y);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = point();
                self.on_button_up(x, y);
                Some(LRESULT(0))
            }
            WM_LBUTTONDBLCLK => {
                let (x, y) = point();
                let hit = self.ui.hit(x as f32, y as f32);
                if self.editor.is_some() {
                    // A double-click on a slider puts it back to the middle.
                    if let (Some(ui::Hit::Slider(i)), Some(ed)) = (hit, self.editor.as_mut()) {
                        ed.set_value(i, 0.0);
                        self.invalidate();
                    } else {
                        self.on_button_down(x, y);
                    }
                    return Some(LRESULT(0));
                }
                if self.grid.is_some() {
                    self.on_button_down(x, y);
                    return Some(LRESULT(0));
                }
                if matches!(hit, None | Some(ui::Hit::Panel | ui::Hit::Modal)) && self.start_text(x, y, true) {
                    // A double-click on interface text selects a word of it.
                } else if self.ui.hit(x as f32, y as f32).is_none() && self.select_word(x, y) {
                    // A double-click on a word in a photo selects it.
                } else if self.ui.hit(x as f32, y as f32).is_none() && self.shown.is_some() {
                    // The first click of the two played or paused a video: undo that.
                    if self.player().is_some_and(|p| p.error.is_none()) {
                        self.run(Cmd::PlayPause);
                    }
                    self.toggle_fullscreen();
                } else {
                    self.on_button_down(x, y);
                }
                Some(LRESULT(0))
            }
            WM_MBUTTONUP => {
                self.run(Cmd::FitOrActual);
                Some(LRESULT(0))
            }
            WM_XBUTTONUP => {
                let button = (wp.0 >> 16) & 0xFFFF;
                self.run(if button == 1 { Cmd::Prev } else { Cmd::Next });
                Some(LRESULT(1))
            }
            WM_CONTEXTMENU => {
                let (mut x, mut y) = point();
                if x == -1 && y == -1 {
                    let mut pt = POINT { x: self.mouse.0, y: self.mouse.1 };
                    unsafe {
                        let _ = ClientToScreen(self.hwnd, &mut pt);
                    }
                    (x, y) = (pt.x, pt.y);
                }
                self.context_menu(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
                let ctrl = (wp.0 & 0x0008) != 0;
                let (sx, sy) = point();
                let mut pt = POINT { x: sx, y: sy };
                unsafe {
                    let _ = ScreenToClient(self.hwnd, &mut pt);
                }
                self.mouse = (pt.x, pt.y);
                self.on_wheel(delta, pt.x, pt.y, ctrl);
                Some(LRESULT(0))
            }
            WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
                unsafe {
                    if self.cursor_hidden {
                        SetCursor(None);
                    } else if self.editor.is_some() {
                        SetCursor(LoadCursorW(None, self.edit_cursor()).ok());
                    } else if matches!(self.drag, Some(Drag::Pan { .. })) {
                        SetCursor(LoadCursorW(None, IDC_SIZEALL).ok());
                    } else if matches!(self.drag, Some(Drag::Words | Drag::Text)) || (self.drag.is_none() && self.ui.over_text(self.mouse.0 as f32, self.mouse.1 as f32)) || (self.drag.is_none() && self.ui.hit(self.mouse.0 as f32, self.mouse.1 as f32).is_none() && self.over_words(self.mouse.0, self.mouse.1)) {
                        SetCursor(LoadCursorW(None, IDC_IBEAM).ok());
                    } else {
                        SetCursor(LoadCursorW(None, IDC_ARROW).ok());
                    }
                }
                Some(LRESULT(1))
            }
            WM_KEYDOWN if wp.0 as u16 == VK_Z.0 => {
                // Held Z shows the image at 100% about the cursor; letting go puts it back.
                let repeat = (lp.0 >> 30) & 1 == 1;
                let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
                if ctrl && !repeat && self.dialog.is_none() {
                    // Ctrl+Z puts back the last file deleted.
                    self.run(Cmd::Undo);
                } else if !repeat {
                    self.hold_z(true);
                }
                Some(LRESULT(0))
            }
            WM_KEYUP if wp.0 as u16 == VK_Z.0 => {
                self.hold_z(false);
                Some(LRESULT(0))
            }
            WM_KILLFOCUS => {
                self.hold_z(false);
                None
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let alt = (lp.0 >> 29) & 1 == 1;
                if self.on_key(wp.0 as u16, alt) {
                    return Some(LRESULT(0));
                }
                None
            }
            WM_SYSCHAR if wp.0 == 13 => Some(LRESULT(0)), // Alt+Enter: no beep
            WM_TIMER => {
                match wp.0 {
                    TIMER_IDLE => self.on_idle(),
                    TIMER_ANIMATION => self.tick_animation(),
                    TIMER_SAVE => self.save_settings(),
                    TIMER_OCR => self.start_ocr(),
                    TIMER_LOADING => unsafe {
                        let _ = KillTimer(Some(self.hwnd), TIMER_LOADING);
                        if let Some(s) = &mut self.shown {
                            s.old = None;
                        }
                        self.invalidate();
                    },
                    TIMER_TOAST => unsafe {
                        let _ = KillTimer(Some(self.hwnd), TIMER_TOAST);
                        self.toast = None;
                        self.invalidate();
                    },
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_JOB_DONE => {
                self.take_done();
                Some(LRESULT(0))
            }
            WM_MEDIA => {
                self.on_media(wp.0 as u32, lp.0 as usize);
                Some(LRESULT(0))
            }
            WM_ART => {
                self.take_art();
                Some(LRESULT(0))
            }
            WM_OCR => {
                self.take_ocr();
                Some(LRESULT(0))
            }
            WM_FOLDER => {
                self.take_scan();
                Some(LRESULT(0))
            }
            crate::thumbs::WM_THUMB => {
                if self.thumbs.take(self.gpu.as_ref()) {
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_SAVED => {
                self.take_saved();
                Some(LRESULT(0))
            }
            WM_DROPFILES => {
                self.on_drop(HDROP(wp.0 as *mut _));
                Some(LRESULT(0))
            }
            WM_ACTIVATEAPP if wp.0 != 0 => {
                // Back from another app: files may have been added or deleted meanwhile.
                if self.scanned_at.is_some_and(|t| t.elapsed() > Duration::from_secs(2)) {
                    self.rescan();
                }
                None
            }
            WM_SETTINGCHANGE => {
                let what = if lp.0 != 0 { util::from_wide(unsafe { std::slice::from_raw_parts(lp.0 as *const u16, 64) }) } else { String::new() };
                if what.starts_with("ImmersiveColorSet") {
                    self.restyle();
                }
                None
            }
            WM_CLOSE => {
                self.remember_placement();
                self.leave_video();
                self.save_settings();
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
                Some(LRESULT(0))
            }
            WM_ENDSESSION => {
                self.remember_placement();
                self.leave_video();
                self.save_settings();
                None
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn remember_placement(&mut self) {
        let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let placement = match &self.fullscreen {
            Some(saved) => saved.placement,
            None => {
                if unsafe { GetWindowPlacement(self.hwnd, &mut wp) }.is_err() {
                    return;
                }
                wp
            }
        };
        let r = placement.rcNormalPosition;
        self.settings.placement = Some(view_core::settings::Placement {
            x: r.left,
            y: r.top,
            width: r.right - r.left,
            height: r.bottom - r.top,
            maximized: placement.showCmd == SW_SHOWMAXIMIZED.0 as u32,
        });
    }
}

// ---- The Space preview ----

impl App {
    /// Show a file from File Explorer. Images open as pictures; anything else as a card.
    pub fn peek_open(&mut self, anchor: HWND, path: PathBuf) {
        if self.peek.is_none() {
            return;
        }
        let visible = unsafe { IsWindowVisible(self.hwnd) }.as_bool();
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_PEEK_EXIT);
        }
        self.leave_video();
        self.leave_doc();
        let mode = Settings::load(&util::settings_file()).peek_mode;
        if let Some(peek) = &mut self.peek {
            peek.anchor = anchor;
            peek.mode = mode;
            peek.card = None;
            peek.doc = None;
            peek.model = None;
            peek.bar = None;
            peek.font = None;
        }
        self.dialog = None;
        self.ui.hover = None;
        self.art = None;
        if self.extensions.lock().unwrap().is_empty() {
            *self.extensions.lock().unwrap() = supported_extensions();
        }
        let kind = self.kind_of(&path);
        // Whether the window can take its size now, or waits to learn the picture's.
        let mut waits = false;
        match kind {
            Kind::Image | Kind::Video | Kind::Audio => {
                self.open(path);
                waits = self.shown.as_ref().is_some_and(|s| s.picture.is_none() && s.error.is_none());
            }
            Kind::Pdf | Kind::Text(_) | Kind::Document(_) => {
                self.animation = None;
                self.shown = None;
                self.folder = Folder::single(&path);
                let mut doc = Doc::open(&path, kind, self.hwnd);
                waits = kind == Kind::Pdf;
                if let Some(peek) = &mut self.peek {
                    if let Some(&place) = peek.places.get(&path) {
                        doc.resume_at(place);
                    }
                    peek.doc = Some(doc);
                }
            }
            Kind::Model => {
                self.animation = None;
                self.shown = None;
                self.folder = Folder::single(&path);
                let model = Model::open(&path, self.hwnd);
                if let Some(peek) = &mut self.peek {
                    peek.model = Some(model);
                }
            }
            Kind::Font => {
                self.animation = None;
                self.shown = None;
                self.folder = Folder::single(&path);
                let font = self.text.font_file(&path);
                if let Some(peek) = &mut self.peek {
                    peek.font = Some(font);
                }
            }
            Kind::Other if path.is_dir() => {
                self.animation = None;
                self.shown = None;
                self.folder = Folder::single(&path);
                self.start_folder(path);
            }
            Kind::Other => {
                self.animation = None;
                self.shown = None;
                self.folder = Folder::single(&path);
                self.start_card(path);
            }
        }
        if !visible {
            if let Some(peek) = &mut self.peek {
                peek.pending = true;
            }
            unsafe { SetTimer(Some(self.hwnd), TIMER_PEEK_SHOW, 250, None) };
        }
        if !waits {
            self.peek_reveal();
        }
        self.selection_changed();
        self.set_title();
        self.invalidate();
    }

    /// How the preview shows a file: by its extension, its name, or failing those, whether
    /// it reads as text.
    fn kind_of(&self, path: &Path) -> Kind {
        if !path.is_file() {
            return Kind::Other;
        }
        let exts = self.extensions.lock().unwrap();
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let kind = view_core::kind::from_extension(&ext, |e| exts.iter().any(|x| x == e));
        drop(exts);
        // Whether the first few kilobytes read as text.
        let text = || {
            use std::io::Read;
            let mut head = vec![0u8; 4096];
            let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
            n > 0 && view_core::textfile::looks_like_text(&head[..n]) && view_core::textfile::decode(&head[..n], true).is_some()
        };
        // Extensions with two meanings: look inside.
        if view_core::kind::VIDEO_OR_CODE.contains(&ext.as_str()) && !text() {
            return Kind::Video;
        }
        if ext == "obj" && !text() {
            return Kind::Other; // compiled code, not a model
        }
        if kind != Kind::Other {
            return kind;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(t) = view_core::kind::from_name(&name) {
            return Kind::Text(t);
        }
        // Not known by name: text if it looks like it.
        let small = std::fs::metadata(path).is_ok_and(|m| m.len() <= 64 << 20);
        if small && text() { Kind::Text(view_core::kind::TextKind::Plain) } else { Kind::Other }
    }

    fn take_doc(&mut self) {
        let gpu = self.gpu.as_ref();
        let Some(doc) = self.peek.as_mut().and_then(|p| p.doc.as_mut()) else { return };
        let changed = doc.poll(gpu);
        if changed && (doc.is_pdf() || doc.failed().is_some() || doc.suits().is_some()) {
            self.peek_reveal();
        }
        self.invalidate();
    }

    fn start_card(&mut self, path: PathBuf) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        let meta = std::fs::metadata(&path).ok();
        let kind = shell::type_name(&path);
        let mut lines = Vec::new();
        match &meta {
            Some(m) if m.is_file() => lines.push(if kind.is_empty() { view_core::human_bytes(m.len()) } else { format!("{kind}  ·  {}", view_core::human_bytes(m.len())) }),
            _ if !kind.is_empty() => lines.push(kind),
            _ => {}
        }
        if let Some(t) = meta.as_ref().and_then(|m| m.modified().ok()) {
            lines.push(format!("Modified {}", util::local_time(t)));
        }
        let Some(peek) = &mut self.peek else { return };
        peek.card = Some(Card { path: path.clone(), name, lines, texture: None, loading: true, pictures: Vec::new() });
        let slot = peek.thumbnail.clone();
        let hwnd = self.hwnd.0 as isize;
        let px = (256.0 * self.scale()).round() as i32;
        let _ = std::thread::Builder::new().name("thumbnail".into()).spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            }
            let bitmap = shell::thumbnail(&path, px);
            *slot.lock().unwrap() = Some((path, bitmap));
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CARD, WPARAM(0), LPARAM(0));
            }
        });
    }

    fn take_card(&mut self) {
        let Some(peek) = &mut self.peek else { return };
        let thumbnail = peek.thumbnail.lock().unwrap().take();
        let contents = peek.folder.lock().unwrap().take();
        let Some(card) = &mut peek.card else { return };
        let gpu = self.gpu.as_ref();
        // Either may be for a file the selection has since moved on from.
        if let Some((_, bitmap)) = thumbnail.filter(|(p, _)| *p == card.path) {
            card.loading = false;
            card.texture = bitmap.as_ref().and_then(|b| gpu.and_then(|g| g.upload(b)));
        }
        if let Some((_, summary, bitmaps)) = contents.filter(|(p, ..)| *p == card.path) {
            card.lines.insert(card.lines.len().min(1), summary);
            card.pictures = bitmaps.iter().filter_map(|b| gpu.and_then(|g| g.upload(b))).collect();
        }
        self.invalidate();
    }

    /// Size the preview to what it shows and put it in the middle of Explorer's screen.
    fn peek_reveal(&mut self) {
        let Some(peek) = &mut self.peek else { return };
        peek.pending = false;
        let anchor = peek.anchor;
        let card = peek.card.as_ref().map(|c| c.path.is_dir());
        let model = peek.model.is_some();
        let font = peek.font.is_some();
        let doc = peek.doc.as_ref().map(|d| (d.is_pdf(), d.first_page(), d.is_table(), d.failed().is_some()));
        let suits = peek.doc.as_ref().and_then(|d| d.suits());
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_PEEK_SHOW);
            let _ = KillTimer(Some(self.hwnd), TIMER_PEEK_EXIT);
        }
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let monitor = unsafe { MonitorFromWindow(anchor, MONITOR_DEFAULTTONEAREST) };
        unsafe {
            let _ = GetMonitorInfoW(monitor, &mut mi);
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        unsafe {
            let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(monitor, windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        }
        let s = dx.max(96) as f32 / 96.0;
        let work = mi.rcWork;
        let (ww, wh) = ((work.right - work.left) as f32, (work.bottom - work.top) as f32);
        let (min_w, min_h) = (360.0 * s, 260.0 * s);
        let player = self.player().map(|p| (p.size(), p.error.is_some()));
        let (w, h) = if let Some(folder) = card {
            if folder { (460.0 * s, 580.0 * s) } else { (420.0 * s, 430.0 * s) }
        } else if font {
            ((820.0 * s).min(ww * 0.7), (680.0 * s).min(wh * 0.85))
        } else if model {
            ((820.0 * s).min(ww * 0.6), (680.0 * s).min(wh * 0.75))
        } else if let Some((pdf, page, table, failed)) = doc {
            match (pdf, page, suits) {
                _ if failed => (420.0 * s, 300.0 * s),
                // Tall enough for the first page, whole.
                (true, Some((pw, ph)), _) => {
                    let m = 24.0 * s;
                    let mut h = wh * 0.86;
                    let mut w = (h - m) * pw / ph + m;
                    if w > ww * 0.7 {
                        w = ww * 0.7;
                        h = (w - m) * ph / pw + m;
                    }
                    (w.max(min_w), h.max(min_h))
                }
                (true, None, _) => (560.0 * s, 720.0 * s),
                // A list of what's in a zip: as tall as its lines, up to a point.
                (_, _, Some(crate::doc::Suits::List(lh))) => (600.0 * s, (lh * s).min(wh * 0.8)),
                // Slides: one whole slide in view, the rest below it.
                (_, _, Some(crate::doc::Suits::Slides(ratio))) => {
                    let (side, top, bottom) = (16.0 * s, 56.0 * s, 28.0 * s);
                    let mut w = (1040.0 * s).min(ww * 0.72);
                    let mut h = (w - side * 2.0) * ratio + top + bottom;
                    if h > wh * 0.86 {
                        h = wh * 0.86;
                        w = (h - top - bottom) / ratio + side * 2.0;
                    }
                    (w.max(min_w), h.max(min_h))
                }
                _ if table => ((1100.0 * s).min(ww * 0.75), (760.0 * s).min(wh * 0.8)),
                _ => ((860.0 * s).min(ww * 0.7), (900.0 * s).min(wh * 0.85)),
            }
        } else if let Some((size, failed)) = player {
            match size {
                Some((iw, ih)) if !failed => {
                    // Unlike a picture, a video looks fine scaled up, and a phone clip or an
                    // old file shown at its own few hundred pixels is a postage stamp. So
                    // every video gets the same good share of the screen, small ones scaled
                    // up to fill it.
                    let k = (ww * 0.62 / iw as f32).min(wh * 0.78 / ih as f32).min(4.0);
                    ((iw as f32 * k).max(min_w), (ih as f32 * k).max(min_h))
                }
                // A song, or something that wouldn't play: a card.
                _ => (420.0 * s, 470.0 * s),
            }
        } else if let Some(p) = self.shown.as_ref().and_then(|x| x.picture.as_ref()) {
            let (iw, ih) = p.orient.then(self.user).shown_size(p.width, p.height);
            let k = (ww * 0.62 / iw as f32).min(wh * 0.78 / ih as f32).min(1.0);
            ((iw as f32 * k).max(min_w), (ih as f32 * k).max(min_h))
        } else if unsafe { IsWindowVisible(self.hwnd) }.as_bool() {
            self.client_size()
        } else {
            (560.0 * s, 420.0 * s)
        };
        let (w, h) = (w.round() as i32, h.round() as i32);
        let (mut x, mut y) = (work.left + ((ww as i32 - w) / 2), work.top + ((wh as i32 - h) / 2));
        let (moved, mode) = self.peek.as_ref().map_or((None, PeekMode::Popup), |p| (p.moved, p.mode));
        if let Some((mx, my)) = moved {
            // Where it was dragged to, kept on that screen.
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            unsafe {
                let _ = GetMonitorInfoW(MonitorFromPoint(POINT { x: mx, y: my }, MONITOR_DEFAULTTONEAREST), &mut mi);
            }
            let r = mi.rcWork;
            x = (mx - w / 2).clamp(r.left, (r.right - w).max(r.left));
            y = (my - h / 2).clamp(r.top, (r.bottom - h).max(r.top));
        }
        // On top of everything, or, as a window, just on top of Explorer.
        let z = if mode == PeekMode::Window { HWND_NOTOPMOST } else { HWND_TOPMOST };
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(z), x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
        self.ui.show(true);
        self.arm_idle();
        self.relayout();
    }

    /// Bring the preview, as a window, up over whatever covers it, without taking the keyboard.
    fn peek_raise(&self) {
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// Whether a press here moves the preview: along its top edge, while it stays open, where
    /// there's no control or text.
    fn peek_handle(&self, x: i32, y: i32) -> bool {
        let Some(peek) = &self.peek else { return false };
        if peek.mode == PeekMode::Popup || y as f32 >= ui::TITLE * self.scale() {
            return false;
        }
        let text = peek.doc.as_ref().is_some_and(|d| d.text_at(x as f32, y as f32)) || self.ui.over_text(x as f32, y as f32);
        !text && self.ui.hit(x as f32, y as f32).is_none()
    }

    pub fn peek_hide(&mut self) {
        if self.peek.is_none() {
            return;
        }
        self.leave_video();
        self.leave_doc();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let _ = KillTimer(Some(self.hwnd), TIMER_PEEK_SHOW);
            SetTimer(Some(self.hwnd), TIMER_PEEK_EXIT, 90_000, None);
        }
        // Let go of the picture, so the next one doesn't open with this one in it.
        self.animation = None;
        self.shown = None;
        self.folder = Folder::default();
        self.tween = None;
        self.drag = None;
        self.art = None;
        if let Some(peek) = &mut self.peek {
            peek.card = None;
            peek.doc = None;
            peek.model = None;
            peek.pending = false;
            peek.bar = None;
            peek.moved = None;
            peek.font = None;
        }
        self.selection_changed();
    }

    /// Messages only the preview gets.
    fn handle_peek(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_NCCALCSIZE if wp.0 != 0 => Some(LRESULT(0)), // all of it is View's to draw in
            WM_NCHITTEST => Some(LRESULT(HTCLIENT as isize)),
            // Clicks mustn't take the keyboard from Explorer.
            WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_COPYDATA => {
                let data = unsafe { &*(lp.0 as *const windows::Win32::System::DataExchange::COPYDATASTRUCT) };
                if data.dwData != crate::spacebar::COPY_SHOW {
                    return Some(LRESULT(0));
                }
                let units = unsafe { std::slice::from_raw_parts(data.lpData as *const u16, data.cbData as usize / 2) };
                let text = String::from_utf16_lossy(units);
                if let Some((anchor, path)) = text.split_once('\n') {
                    let anchor = HWND(anchor.parse::<isize>().unwrap_or(0) as *mut _);
                    self.peek_open(anchor, PathBuf::from(path));
                }
                Some(LRESULT(1))
            }
            crate::spacebar::WM_PEEK_HIDE => {
                self.peek_hide();
                Some(LRESULT(0))
            }
            WM_DOC => {
                self.take_doc();
                Some(LRESULT(0))
            }
            WM_MODEL => {
                if let Some(m) = self.peek.as_mut().and_then(|p| p.model.as_mut()) {
                    m.poll();
                }
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_CARD => {
                self.take_card();
                Some(LRESULT(0))
            }
            WM_LBUTTONDBLCLK => {
                let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
                let hit = self.ui.hit(x as f32, y as f32);
                if hit == Some(ui::Hit::Field) {
                    self.field_click(x as f32, false, true);
                    return Some(LRESULT(0));
                }
                if hit.is_none() && self.start_text(x, y, true) {
                    return Some(LRESULT(0));
                }
                let on_ui = hit.is_some();
                if !on_ui && self.select_word(x, y) {
                    return Some(LRESULT(0));
                }
                let peek = self.peek.as_mut()?;
                // A second click on a zip's folder closes it again.
                if !on_ui && peek.doc.as_mut().is_some_and(|d| d.click(x as f32, y as f32)) {
                    self.invalidate();
                    return Some(LRESULT(0));
                }
                if let (false, Some(doc)) = (on_ui, peek.doc.as_mut().filter(|d| d.text_at(x as f32, y as f32))) {
                    // A double-click in text selects a word, and dragging on from it carries on.
                    unsafe { SetCapture(self.hwnd) };
                    doc.select_start(x as f32, y as f32, true);
                    self.drag = Some(Drag::Select);
                    self.selection_changed();
                } else if let (false, Some(m)) = (on_ui, peek.model.as_mut()) {
                    m.reset_view();
                    self.invalidate();
                } else if !on_ui {
                    self.run(Cmd::OpenInView);
                } else {
                    self.on_button_down(x, y);
                }
                Some(LRESULT(0))
            }
            WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
                // Text shows the text cursor; everything else the arrow.
                let (x, y) = (self.mouse.0 as f32, self.mouse.1 as f32);
                let text = self.ui.hit(x, y).is_none() && self.peek.as_ref().and_then(|p| p.doc.as_ref()).is_some_and(|d| d.text_at(x, y));
                let words = self.ui.hit(x, y).is_none() && self.over_words(x as i32, y as i32);
                let ui_text = self.ui.over_text(x, y) || self.ui.hit(x, y) == Some(ui::Hit::Field);
                let folder = self.ui.hit(x, y).is_none() && self.peek.as_ref().and_then(|p| p.doc.as_ref()).is_some_and(|d| d.folder_at(x, y));
                let cursor = if folder {
                    IDC_HAND
                } else if text || words || ui_text || matches!(self.drag, Some(Drag::Select | Drag::Words | Drag::Text | Drag::Field)) {
                    IDC_IBEAM
                } else {
                    IDC_ARROW
                };
                unsafe {
                    SetCursor(LoadCursorW(None, cursor).ok());
                }
                Some(LRESULT(1))
            }
            crate::spacebar::WM_PEEK_KEY => {
                self.bar_key(wp.0 as u16, lp.0);
                Some(LRESULT(0))
            }
            crate::spacebar::WM_PEEK_COPY => {
                self.run(Cmd::CopyText);
                Some(LRESULT(0))
            }
            WM_CONTEXTMENU | WM_KEYDOWN | WM_SYSKEYDOWN => Some(LRESULT(0)),
            WM_MOUSELEAVE => {
                self.tracking = false;
                self.ui.mouse_inside = false;
                self.ui.hover = None;
                self.ui.over_controls = false;
                // A song's controls are most of its preview, so they stay.
                let song = self.player().is_some_and(|p| p.ready && !p.has_video());
                self.ui.show(song);
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                // The preview sizes itself for each screen; Windows' suggested size would undo that.
                self.dpi = (wp.0 & 0xFFFF) as u32;
                self.text.reset();
                self.relayout();
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TIMER_PEEK_SHOW => {
                if self.peek.as_ref().is_some_and(|p| p.pending) {
                    self.peek_reveal();
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TIMER_PEEK_EXIT => {
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

/// The preview closes when anything but Explorer and the preview comes to the front.
extern "system" fn foreground_changed(_: windows::Win32::UI::Accessibility::HWINEVENTHOOK, _: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
    let Some(app) = app() else { return };
    let Some(peek) = &app.peek else { return };
    if hwnd == app.hwnd || !unsafe { IsWindowVisible(app.hwnd) }.as_bool() {
        return;
    }
    match peek.mode {
        PeekMode::Popup if hwnd != peek.anchor => app.peek_hide(),
        // As a window, it comes back up with the Explorer window it belongs to.
        PeekMode::Window if hwnd == peek.anchor => app.peek_raise(),
        _ => {}
    }
}

/// The most pixels a full-resolution decode may keep: about a quarter of the memory free now,
/// between 16 and 268 megapixels (that's 1 GB of pixels, 16384 × 16384).
fn full_pixel_limit() -> u64 {
    (util::available_memory() / 4 / 4).clamp(16_000_000, 268_435_456)
}

/// Extensions View opens: Windows' codecs, and View's own GIF, WebP and APNG.
pub fn supported_extensions() -> Vec<String> {
    let mut exts = crate::wic::Wic::new().map(|w| w.extensions()).unwrap_or_default();
    for own in ["gif", "webp", "png", "apng"] {
        if !exts.iter().any(|e| e == own) {
            exts.push(own.into());
        }
    }
    // Only image formats: some codecs register video-ish extensions too.
    exts.retain(|e| view_core::formats::Format::from_extension(e).is_some());
    exts.sort();
    exts
}

fn about_text(exts: &[String]) -> String {
    use view_core::formats::Format;
    let packages = crate::wic::store_packages();
    let mut open = Vec::new();
    let mut missing = Vec::new();
    for f in Format::ALL {
        let decoder = f.extensions().iter().any(|e| exts.iter().any(|x| x == e));
        let extension = crate::wic::extension_installed(f, &packages).unwrap_or(true);
        if decoder && extension {
            open.push(f.name());
        } else if let Some(store) = crate::wic::extension_for(f) {
            missing.push(format!("{} (needs \"{store}\")", f.name()));
        }
    }
    let mut s = format!("A fast image viewer for Windows.\n\nOpens {}.", open.join(", "));
    if !missing.is_empty() {
        s.push_str(&format!("\n\nNot on this PC yet: {}.", missing.join(", ")));
    }
    s
}

/// The title bar in the window's own colours, on Windows 11; a dark title bar on Windows 10.
pub fn style_frame(hwnd: HWND, p: &Palette) {
    unsafe {
        let dark: u32 = p.dark as u32;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
        let caption = p.chrome.colorref();
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as *const _, 4);
        let text = p.text.colorref();
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TEXT_COLOR, &text as *const _ as *const _, 4);
    }
}

// ---- Creating the window ----

static APP: AtomicPtrApp = AtomicPtrApp(std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()));
struct AtomicPtrApp(std::sync::atomic::AtomicPtr<App>);

fn app() -> Option<&'static mut App> {
    let p = APP.0.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &mut *p }) }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(app) = app() {
        if app.hwnd == hwnd {
            if let Some(r) = app.handle(msg, wp, lp) {
                return r;
            }
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

pub struct Start {
    pub path: Option<PathBuf>,
    pub settings: Settings,
    pub pool: Pool,
    pub screen: (u32, u32),
    pub d3d: std::thread::JoinHandle<Option<ID3D11Device>>,
    pub fullscreen: bool,
    /// Open as the Space preview for this Explorer window.
    pub peek: Option<HWND>,
    /// Where a video starts, in seconds.
    pub at: Option<f64>,
}

/// The size and place for a new window: where it was last time if that's still on a screen,
/// else 70% of the screen under the mouse, in its middle.
pub fn initial_rect(settings: &Settings) -> (RECT, bool) {
    unsafe {
        if let Some(p) = settings.placement {
            let r = RECT { left: p.x, top: p.y, right: p.x + p.width, bottom: p.y + p.height };
            if p.width > 100 && p.height > 100 && !MonitorFromRect(&r, MONITOR_DEFAULTTONULL).is_invalid() {
                return (r, p.maximized);
            }
        }
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY), &mut mi);
        let w = mi.rcWork;
        let (ww, wh) = (w.right - w.left, w.bottom - w.top);
        let (cw, ch) = ((ww as f32 * 0.7) as i32, (wh as f32 * 0.75) as i32);
        let (x, y) = (w.left + (ww - cw) / 2, w.top + (wh - ch) / 2);
        (RECT { left: x, top: y, right: x + cw, bottom: y + ch }, false)
    }
}

/// The pixel size of the monitor a rectangle is mostly on.
pub fn monitor_size(r: &RECT) -> (u32, u32) {
    let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(MonitorFromRect(r, MONITOR_DEFAULTTONEAREST), &mut mi);
    }
    let m = mi.rcMonitor;
    ((m.right - m.left).max(640) as u32, (m.bottom - m.top).max(480) as u32)
}

pub fn create(start: Start) -> Option<HWND> {
    let palette = theme::palette(&start.settings);
    menu::set_dark(palette.dark);
    let class = Wide::new(if start.peek.is_some() { crate::spacebar::PEEK_CLASS } else { CLASS });
    let instance = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }.ok()?;
    let background = unsafe { CreateSolidBrush(COLORREF(palette.backdrop.colorref())) };
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        hIcon: unsafe { LoadIconW(Some(instance.into()), PCWSTR(1 as _)) }.unwrap_or_default(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hbrBackground: background,
        lpszClassName: class.pcwstr(),
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wc) };
    let (r, maximized) = initial_rect(&start.settings);
    let title = Wide::new("View");
    // The preview floats over Explorer and never takes the keyboard from it. The sizing frame
    // is only there so Windows gives it a shadow and rounded corners; it can't be resized.
    let (ex, style) = if start.peek.is_some() { (WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, WS_POPUP | WS_THICKFRAME) } else { (WINDOW_EX_STYLE(0), WS_OVERLAPPEDWINDOW) };
    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            class.pcwstr(),
            title.pcwstr(),
            style,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .ok()?;
    trace("window created");
    style_frame(hwnd, &palette);
    unsafe { DragAcceptFiles(hwnd, true) };
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);

    let settings = start.settings;
    let cache_budget = if settings.cache_mb > 0 { (settings.cache_mb as usize) << 20 } else { view_core::cache::default_budget(util::total_memory()) };
    let boxed = Box::new(App {
        hwnd,
        palette,
        dpi,
        pool: start.pool,
        gpu: None,
        d3d: None,
        text: Text::new()?,
        cache: Cache::new(cache_budget),
        folder: Folder::default(),
        folder_dir: None,
        scan: Arc::new(Mutex::new(None)),
        scanned_at: None,
        extensions: Arc::new(Mutex::new(Vec::new())),
        shown: None,
        vp: Viewport::new(1, 1, 1.0, 1.0, settings.upscale_small),
        user: Orient::IDENTITY,
        tween: None,
        forward: true,
        screen: start.screen,
        drag: None,
        mouse: (-1, -1),
        ui: ui::State::new(),
        fullscreen: None,
        toast: None,
        dialog: None,
        camera: None,
        animation: None,
        tracking: false,
        nc_tracking: false,
        cursor_hidden: false,
        peek: start.peek.map(|anchor| Peek {
            anchor,
            card: None,
            model: None,
            doc: None,
            pending: false,
            thumbnail: Arc::new(Mutex::new(None)),
            places: HashMap::new(),
            bar: None,
            moved: None,
            mode: PeekMode::Popup,
            font: None,
            folder: Arc::new(Mutex::new(None)),
        }),
        art: None,
        art_slot: Arc::new(Mutex::new(None)),
        start_at: start.at,
        held_z: None,
        video_places: HashMap::new(),
        kept: None,
        words: None,
        ocr_slot: Arc::new(Mutex::new(None)),
        ocr_running: false,
        dead_key: None,
        speed: 1.0,
        deleted: Vec::new(),
        undo_until: None,
        thumbs: crate::thumbs::Thumbs::new(),
        grid: None,
        strip_at: 0.0,
        editor: None,
        saved: Arc::new(Mutex::new(None)),
        settings,
    });
    APP.0.store(Box::into_raw(boxed), Ordering::Release);
    let app = app()?;
    // Windows sized the frame before View was listening; ask again, now without a title bar.
    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
    }

    // The device has been loading meanwhile.
    app.d3d = start.d3d.join().ok().flatten();
    trace("device ready");
    let (w, h) = app.client_size();
    if let Some(d3d) = &app.d3d {
        app.gpu = Gpu::new(hwnd, d3d, w as u32, h as u32).ok();
    }
    trace("swap chain ready");
    app.pool.set_window(hwnd);
    if let Some(anchor) = start.peek {
        unsafe {
            let round = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as *const _, size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32);
            windows::Win32::UI::Accessibility::SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(foreground_changed), 0, 0, WINEVENT_OUTOFCONTEXT);
        }
        if let Some(path) = start.path {
            app.peek_open(anchor, path);
            let deadline = Instant::now() + Duration::from_millis(220);
            while Instant::now() < deadline && app.peek.as_ref().is_some_and(|p| p.pending) {
                app.pool.wait_done(deadline - Instant::now());
                app.take_done();
            }
        }
        trace("preview ready");
        return Some(hwnd);
    }
    if let Some(path) = start.path {
        app.open(path);
        // Give the first decode a moment, so the window appears with the image in it.
        let deadline = Instant::now() + Duration::from_millis(120);
        while Instant::now() < deadline && app.shown.as_ref().is_some_and(|s| s.picture.is_none() && s.error.is_none()) {
            app.pool.wait_done(deadline - Instant::now());
            app.take_done();
        }
    }
    let (aw, ah) = app.area();
    app.vp.resize(aw as f64, ah as f64);
    app.set_title();
    unsafe {
        let _ = ShowWindow(hwnd, if maximized { SW_SHOWMAXIMIZED } else { SW_SHOW });
        let _ = UpdateWindow(hwnd);
    }
    trace("window shown");
    if start.fullscreen {
        app.toggle_fullscreen();
    }
    app.ui.show(true);
    app.arm_idle();
    Some(hwnd)
}

/// Keys in the find box that edit or move rather than type.
fn edit_key(key: VIRTUAL_KEY) -> bool {
    matches!(key, VK_ESCAPE | VK_RETURN | VK_F3 | VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_HOME | VK_END | VK_BACK | VK_DELETE | VK_INSERT)
}

/// What a key types, in the keyboard layout of the window in front (File Explorer's, since
/// that's where the keys were pressed). Nothing for keys that don't type.
///
/// Accent keys (dead keys) type nothing themselves but go on the next letter: ` then a is à.
/// Windows keeps that waiting accent itself, out of sight, and it can be left stuck there,
/// putting a stray accent on every letter after. So View keeps it instead, in `dead`, and
/// leaves Windows' own state empty before and after each key.
fn typed_text(vk: u16, flags: isize, dead: &mut Option<(u16, isize)>) -> Option<String> {
    let layout = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), None)) };
    let convert = |vk: u16, flags: isize, out: &mut [u16]| -> i32 {
        use crate::spacebar::{KEY_ALT, KEY_CAPS, KEY_CTRL, KEY_SHIFT};
        let mut state = [0u8; 256];
        if flags & KEY_SHIFT != 0 {
            state[VK_SHIFT.0 as usize] = 0x80;
            state[VK_LSHIFT.0 as usize] = 0x80;
        }
        // Ctrl and Alt together are AltGr, which types the third character on a key.
        if flags & KEY_CTRL != 0 && flags & KEY_ALT != 0 {
            for k in [VK_CONTROL, VK_LCONTROL, VK_MENU, VK_RMENU] {
                state[k.0 as usize] = 0x80;
            }
        }
        if flags & KEY_CAPS != 0 {
            state[VK_CAPITAL.0 as usize] = 0x01;
        }
        let scan = ((flags >> 16) & 0xFF) as u32;
        unsafe { ToUnicodeEx(vk as u32, scan, &state, out, 0, Some(layout)) }
    };
    // A space takes up any accent left waiting, so the slate is clean.
    let flush = || {
        let mut junk = [0u16; 8];
        let space = unsafe { MapVirtualKeyExW(VK_SPACE.0 as u32, MAPVK_VK_TO_VSC_EX, Some(layout)) } as isize;
        while convert(VK_SPACE.0, space << 16, &mut junk) < 0 {}
    };
    flush();
    let mut buf = [0u16; 8];
    if let Some((dvk, dflags)) = dead.take() {
        convert(dvk, dflags, &mut buf); // sets the accent waiting again, for this key to take
    }
    let n = convert(vk, flags, &mut buf);
    if n < 0 {
        // An accent key: it waits for the next letter.
        *dead = Some((vk, flags));
    }
    flush();
    if n <= 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..n as usize]);
    (!s.chars().any(char::is_control)).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Irish layout, where ` is an accent key, as on the machine this went wrong on.
    fn irish() -> Option<HKL> {
        let hkl = unsafe { LoadKeyboardLayoutW(windows::core::w!("00001809"), KLF_NOTELLSHELL) }.ok()?;
        (hkl.0 as usize & 0xFFFF == 0x1809).then_some(hkl)
    }

    #[test]
    fn accent_keys_go_on_the_next_letter_and_never_get_stuck() {
        if irish().is_none() {
            return; // the layout isn't on this PC
        }
        // Typed while the Irish layout is the one in front.
        unsafe {
            let _ = ActivateKeyboardLayout(irish().unwrap(), KLF_SETFORPROCESS);
        }
        let layout = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), None)) };
        if layout.0 as usize & 0xFFFF != 0x1809 {
            return; // the window in front uses another layout: nothing to check against
        }
        let scan = |vk: u16| (unsafe { MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC, Some(layout)) } as isize) << 16;
        // An accent left waiting in Windows' own state, as happened: it mustn't land on anything.
        let mut junk = [0u16; 8];
        assert_eq!(unsafe { ToUnicodeEx(0xDF, (scan(0xDF) >> 16) as u32, &[0u8; 256], &mut junk, 0, Some(layout)) }, -1);
        let mut dead = None;
        let mut typed = String::new();
        // ` a t ` ` s
        for vk in [0xDF, b'A' as u16, b'T' as u16, 0xDF, 0xDF, b'S' as u16] {
            typed.push_str(&typed_text(vk, scan(vk), &mut dead).unwrap_or_default());
        }
        assert_eq!(typed, "àt``s");
        assert!(dead.is_none());
    }
}