//! The window: what's open, where it sits, and what keys and the mouse do.

use crate::load::{self, Done, Loaded, Pool};
use crate::platform;
use crate::render::{self, Frame, Ground};
use crate::text::Text;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use view_core::folder::{self, Folder};
use view_core::orient::Orient;
use view_core::picture;
use view_core::settings::{Backdrop, Placement, Settings, Theme, Toolbar, Transparency, Wheel};
use view_core::tween::Tween;
use view_core::viewport::{Mode, View, Viewport};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

/// How long the caption stays after the mouse moves or the picture changes, and how long it
/// takes to fade.
const CAPTION_FOR: Duration = Duration::from_millis(2500);
const FADE: Duration = Duration::from_millis(180);
const TOAST_FOR: Duration = Duration::from_millis(1800);
const ZOOM_TIME: Duration = Duration::from_millis(140);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How long a first press of Delete waits for the second.
const DELETE_WAIT: Duration = Duration::from_secs(4);

pub enum Msg {
    Done(Done),
    Picked(Option<PathBuf>),
}

/// The picture on screen. It stays there while the next one decodes.
struct Shown {
    path: PathBuf,
    /// As the file has it, and as the user has turned it.
    base: Arc<Loaded>,
    loaded: Arc<Loaded>,
    user: Orient,
    frame: usize,
    frame_at: Instant,
    loops_done: u32,
}

pub struct App {
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    proxy: Option<EventLoopProxy<Msg>>,
    settings: Settings,
    saved: Settings,
    folder: Folder,
    pool: Pool,
    cache: Vec<(PathBuf, Arc<Loaded>)>,
    failed: HashMap<PathBuf, String>,
    budget: usize,
    shown: Option<Shown>,
    vp: Viewport,
    tween: Option<Tween>,
    text: Text,
    size: (u32, u32),
    /// Screen pixels per interface unit.
    scale: f32,
    dark: bool,
    mods: ModifiersState,
    cursor: (f64, f64),
    drag: Option<(f64, f64)>,
    last_press: Option<Instant>,
    caption_until: Option<Instant>,
    drawn_caption: f32,
    toast: Option<(String, Instant)>,
    delete_asked: Option<(PathBuf, Instant)>,
    last_draw: Instant,
    dirty: bool,
    start: Option<PathBuf>,
    start_fullscreen: bool,
    clipboard: Option<arboard::Clipboard>,
}

pub fn run(file: Option<PathBuf>, fullscreen: bool) -> Result<(), String> {
    let event_loop = EventLoop::<Msg>::with_user_event().build().map_err(|e| e.to_string())?;
    let proxy = event_loop.create_proxy();
    let for_pool = proxy.clone();
    let mut app = App::new(move |done| {
        let _ = for_pool.send_event(Msg::Done(done));
    });
    app.proxy = Some(proxy);
    app.start = file;
    app.start_fullscreen = fullscreen;
    event_loop.run_app(&mut app).map_err(|e| e.to_string())
}

/// Draw one frame of `file` into a PNG, with no window.
pub fn render_to_file(file: &Path, out: &Path, width: u32, height: u32) -> Result<String, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let mut app = App::new(move |done| {
        let _ = tx.lock().unwrap().send(done);
    });
    app.settings = Settings { toolbar: Toolbar::Always, ..Settings::default() };
    app.size = (width.max(1), height.max(1));
    let started = Instant::now();
    app.open(file);
    let current = app.folder.current().map(|e| e.path.clone()).ok_or("There's no picture there.")?;
    while app.shown.as_ref().is_none_or(|s| s.path != current) && !app.failed.contains_key(&current) {
        app.on_done(rx.recv().map_err(|e| e.to_string())?);
    }
    let decoded = started.elapsed();
    if let Some(e) = app.failed.get(&current) {
        return Err(e.clone());
    }
    let (w, h) = (app.size.0 as usize, app.size.1 as usize);
    let mut px = vec![0u32; w * h];
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        app.paint(&mut Frame { px: &mut px, w, h }, Instant::now());
        best = best.min(t.elapsed());
    }
    let rgb: Vec<u8> = px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
    image::save_buffer(out, &rgb, width, height, image::ExtendedColorType::Rgb8).map_err(|e| e.to_string())?;
    Ok(format!("decoded in {:.1} ms, drawn in {:.2} ms", decoded.as_secs_f64() * 1000.0, best.as_secs_f64() * 1000.0))
}

impl App {
    fn new(done: impl Fn(Done) + Send + Sync + 'static) -> App {
        let settings = Settings::load(&platform::settings_dir().join("settings.json"));
        let budget = if settings.cache_mb > 0 { (settings.cache_mb as usize) << 20 } else { 512 << 20 };
        App {
            window: None,
            surface: None,
            proxy: None,
            saved: settings.clone(),
            settings,
            folder: Folder { entries: Vec::new(), index: 0 },
            pool: Pool::new(done),
            cache: Vec::new(),
            failed: HashMap::new(),
            budget,
            shown: None,
            vp: Viewport::new(1, 1, 1.0, 1.0, false),
            tween: None,
            text: Text::new(),
            size: (1, 1),
            scale: 1.0,
            dark: true,
            mods: ModifiersState::empty(),
            cursor: (0.0, 0.0),
            drag: None,
            last_press: None,
            caption_until: None,
            drawn_caption: 0.0,
            toast: None,
            delete_asked: None,
            last_draw: Instant::now(),
            dirty: true,
            start: None,
            start_fullscreen: false,
            clipboard: None,
        }
    }

    // ---- What's open ----

    /// Open a picture and the folder it's in, or a folder at its first picture.
    fn open(&mut self, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let (dir, file) = if path.is_dir() { (path.clone(), None) } else { (path.parent().map(Path::to_path_buf).unwrap_or_default(), Some(path)) };
        let mut entries = folder::scan(&dir, load::opens, |_| true).unwrap_or_default();
        entries.retain(|e| !e.name.starts_with('.'));
        let (key, descending) = (self.settings.sort, self.settings.sort_descending);
        match file {
            Some(f) => {
                self.folder = Folder::single(&f);
                self.folder.replace(entries, &f, key, descending);
            }
            None => {
                folder::sort(&mut entries, key, descending);
                self.folder = Folder { entries, index: 0 };
            }
        }
        self.failed.clear();
        self.cache.clear();
        self.shown = None;
        self.show_current();
    }

    fn current(&self) -> Option<PathBuf> {
        self.folder.current().map(|e| e.path.clone())
    }

    fn cached(&self, path: &Path) -> Option<Arc<Loaded>> {
        self.cache.iter().find(|(p, _)| p == path).map(|(_, l)| l.clone())
    }

    /// Put the folder's current picture on screen if it's decoded, and ask for it if not.
    fn show_current(&mut self) {
        self.delete_asked = None;
        match self.current() {
            Some(path) => {
                if let Some(loaded) = self.cached(&path) {
                    self.put_on_screen(path, loaded);
                }
            }
            None => self.shown = None,
        }
        self.request();
        self.retitle();
        self.show_caption();
        self.dirty = true;
    }

    fn put_on_screen(&mut self, path: PathBuf, loaded: Arc<Loaded>) {
        let keep = self.settings.keep_zoom && self.shown.is_some() && self.vp.mode == Mode::Free;
        let (w, h) = size_of(&loaded);
        self.shown = Some(Shown { path, base: loaded.clone(), loaded, user: Orient::IDENTITY, frame: 0, frame_at: Instant::now(), loops_done: 0 });
        self.tween = None;
        if keep {
            self.vp.set_image(w, h);
        } else {
            self.vp = Viewport::new(w, h, self.size.0 as f64, self.size.1 as f64, self.settings.upscale_small);
        }
        self.dirty = true;
    }

    /// Ask for the current picture and the ones either side of it, and let go of far ones.
    fn request(&mut self) {
        let wrap = self.settings.wrap;
        let mut want: Vec<PathBuf> = Vec::new();
        for delta in [0, 1, -1, 2] {
            let Some(e) = self.folder.offset(delta, wrap).and_then(|i| self.folder.entries.get(i)) else { continue };
            if !want.contains(&e.path) && self.cached(&e.path).is_none() && !self.failed.contains_key(&e.path) {
                want.push(e.path.clone());
            }
        }
        self.pool.want(want);
        self.trim();
    }

    /// Drop decoded pictures furthest from the current one until the rest fit the budget.
    fn trim(&mut self) {
        let n = self.folder.len();
        let index = self.folder.index;
        let entries = &self.folder.entries;
        let distance = |path: &Path| match entries.iter().position(|e| e.path == path) {
            Some(i) => i.abs_diff(index).min(n - i.abs_diff(index)),
            None => usize::MAX,
        };
        while self.cache.len() > 1 && self.cache.iter().map(|(_, l)| l.bytes()).sum::<usize>() > self.budget {
            let Some((far, d)) = self.cache.iter().enumerate().map(|(i, (p, _))| (i, distance(p))).max_by_key(|&(_, d)| d) else { return };
            if d == 0 {
                return;
            }
            self.cache.remove(far);
        }
    }

    fn on_done(&mut self, done: Done) {
        let is_current = self.current().is_some_and(|p| p == done.path);
        match done.result {
            Ok(loaded) => {
                if self.cached(&done.path).is_none() {
                    self.cache.push((done.path.clone(), loaded.clone()));
                }
                if is_current && self.shown.as_ref().is_none_or(|s| s.path != done.path) {
                    self.put_on_screen(done.path, loaded);
                }
                self.trim();
            }
            Err(e) => {
                self.failed.insert(done.path, e);
                self.dirty |= is_current;
            }
        }
    }

    fn go(&mut self, delta: isize) {
        if let Some(i) = self.folder.offset(delta, self.settings.wrap).filter(|&i| i != self.folder.index) {
            self.folder.index = i;
            self.show_current();
        }
    }

    fn go_to(&mut self, index: usize) {
        if index < self.folder.len() && index != self.folder.index {
            self.folder.index = index;
            self.show_current();
        }
    }

    // ---- Zooming and turning ----

    /// Where the picture is drawn right now, part-way through a zoom or settled.
    fn view(&self, now: Instant) -> View {
        self.tween.map_or(self.vp.view, |t| t.at(now).0)
    }

    fn zoom(&mut self, change: impl FnOnce(&mut Viewport)) {
        if self.shown.is_none() {
            return;
        }
        let from = self.view(Instant::now());
        change(&mut self.vp);
        self.tween = (self.settings.smooth_zoom && from != self.vp.view).then(|| Tween::new(from, self.vp.view, ZOOM_TIME));
        self.show_caption();
        self.dirty = true;
    }

    fn middle(&self) -> (f64, f64) {
        (self.size.0 as f64 / 2.0, self.size.1 as f64 / 2.0)
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        self.tween = None;
        self.vp.pan(dx, dy);
        self.dirty = true;
    }

    fn turn(&mut self, change: impl Fn(Orient) -> Orient) {
        let Some(s) = &mut self.shown else { return };
        s.user = change(s.user);
        s.loaded = if s.user == Orient::IDENTITY { s.base.clone() } else { Arc::new(s.base.turned(s.user)) };
        let (w, h) = size_of(&s.loaded);
        self.tween = None;
        self.vp = Viewport::new(w, h, self.size.0 as f64, self.size.1 as f64, self.settings.upscale_small);
        self.dirty = true;
    }

    // ---- Things done to the file ----

    fn delete(&mut self) {
        let Some(path) = self.current() else { return };
        let asked = self.delete_asked.take().is_some_and(|(p, at)| p == path && at.elapsed() < DELETE_WAIT);
        if self.settings.confirm_delete && !asked {
            self.delete_asked = Some((path, Instant::now()));
            self.say("Press Delete again to move this picture to the bin");
            return;
        }
        match platform::bin(&path) {
            Ok(()) => {
                self.folder.remove(self.folder.index);
                self.cache.retain(|(p, _)| p != &path);
                self.shown = None;
                self.show_current();
                self.say("Moved to the bin");
            }
            Err(e) => self.say(&format!("That couldn't be deleted: {e}")),
        }
    }

    fn copy(&mut self) {
        let Some(s) = &self.shown else { return };
        let Some(frame) = s.loaded.picture.frames.get(s.frame) else { return };
        let mut rgba = frame.bitmap.data.clone();
        picture::pbgra_to_rgba(&mut rgba);
        let image = arboard::ImageData { width: frame.bitmap.width as usize, height: frame.bitmap.height as usize, bytes: rgba.into() };
        // The clipboard is kept for as long as View runs: on Linux a copy lives in the app that made it.
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let copied = self.clipboard.as_mut().is_some_and(|c| c.set_image(image).is_ok());
        self.say(if copied { "Copied" } else { "The picture couldn't be copied" });
    }

    fn pick(&mut self) {
        let Some(proxy) = self.proxy.clone() else { return };
        std::thread::spawn(move || {
            let _ = proxy.send_event(Msg::Picked(platform::pick_file()));
        });
    }

    fn say(&mut self, what: &str) {
        self.toast = Some((what.to_string(), Instant::now() + TOAST_FOR));
        self.dirty = true;
    }

    fn show_caption(&mut self) {
        self.caption_until = Some(Instant::now() + CAPTION_FOR);
    }

    // ---- Time ----

    /// How visible the caption is at `now`, from 0 to 1.
    fn caption(&self, now: Instant) -> f32 {
        match self.settings.toolbar {
            Toolbar::Always => 1.0,
            Toolbar::Hidden => 0.0,
            Toolbar::Auto => match self.caption_until {
                Some(until) if now < until => ((until - now).as_secs_f32() / FADE.as_secs_f32()).min(1.0),
                _ => 0.0,
            },
        }
    }

    /// Move on whatever runs by the clock: an animation's frames and the toast.
    fn tick(&mut self, now: Instant) {
        if let Some(s) = &mut self.shown {
            let p = &s.loaded.picture;
            let done = p.loops > 0 && s.loops_done >= p.loops;
            if p.frames.len() > 1 && !done && now >= s.frame_at + frame_time(p, s.frame) {
                s.frame += 1;
                if s.frame == p.frames.len() {
                    s.loops_done += 1;
                    s.frame = if p.loops > 0 && s.loops_done >= p.loops { p.frames.len() - 1 } else { 0 };
                }
                s.frame_at = now;
                self.dirty = true;
            }
        }
        if self.toast.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.toast = None;
            self.dirty = true;
        }
    }

    /// When something next needs drawing with nothing else happening, if ever.
    fn next_wake(&self, now: Instant, frame: Duration) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut by = |t: Instant| next = Some(next.map_or(t, |n| n.min(t)));
        // Motion is drawn a frame at a time, no faster than the display shows them.
        if self.tween.is_some() || (self.caption(now) - self.drawn_caption).abs() > 0.001 {
            by(self.last_draw + frame);
        }
        if let (Toolbar::Auto, Some(until)) = (self.settings.toolbar, self.caption_until) {
            if now + FADE < until {
                by(until - FADE);
            }
        }
        if let Some(s) = &self.shown {
            let p = &s.loaded.picture;
            if p.frames.len() > 1 && !(p.loops > 0 && s.loops_done >= p.loops) {
                by(s.frame_at + frame_time(p, s.frame));
            }
        }
        if let Some((_, until)) = &self.toast {
            by(*until);
        }
        next
    }

    // ---- Drawing ----

    fn ground(&self) -> Ground {
        let dark = match self.settings.theme {
            Theme::Dark => true,
            Theme::Light => false,
            Theme::System => self.dark,
        };
        let backdrop = match self.settings.backdrop {
            Backdrop::Theme if dark => 0x0D0D0F,
            Backdrop::Theme => 0xF0F0F2,
            Backdrop::Black => 0x000000,
            Backdrop::Grey => 0x808080,
            Backdrop::White => 0xFFFFFF,
        };
        let light = (backdrop >> 16 & 0xFF) + (backdrop >> 8 & 0xFF) + (backdrop & 0xFF) > 384;
        let under = match self.settings.transparency {
            Transparency::Checkerboard if light => [0xFFFFFF, 0xE4E4E8],
            Transparency::Checkerboard => [0x2A2A2E, 0x1E1E22],
            Transparency::Backdrop => [backdrop; 2],
            Transparency::White => [0xFFFFFF; 2],
            Transparency::Black => [0x000000; 2],
        };
        Ground { backdrop, under, cell: (8.0 * self.scale).round().max(1.0) as usize }
    }

    fn paint(&mut self, frame: &mut Frame, now: Instant) {
        let ground = self.ground();
        let light = ground.backdrop & 0xFF > 0x90;
        let (ink, soft) = if light { (0x1A1A1E, 0x6A6A72) } else { (0xF2F2F4, 0x9A9AA2) };
        let s = self.scale;
        let (fw, fh) = (frame.w as f32, frame.h as f32);
        let current = self.current();
        let error = current.as_ref().and_then(|p| self.failed.get(p));
        if let Some(error) = error {
            frame.px.fill(ground.backdrop);
            let name = current.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.centred(frame, &name, fh / 2.0 - 13.0 * s, 15.0 * s, ink);
            self.centred(frame, error, fh / 2.0 + 13.0 * s, 13.0 * s, soft);
        } else if let Some(shown) = &self.shown {
            let p = &shown.loaded.picture;
            let view = self.view(now);
            let rect = self.vp.image_rect(view);
            let full = &p.frames[shown.frame.min(p.frames.len() - 1)].bitmap;
            // Zoomed out, draw from the smallest copy that still has a pixel for every screen
            // pixel: sharper than skipping pixels of the full picture, and far less to read.
            let mut src = full;
            if shown.frame == 0 {
                for m in &shown.loaded.mips {
                    if m.width as f64 >= rect.2 && m.height as f64 >= rect.3 {
                        src = m;
                    }
                }
            }
            let smooth = !(self.settings.pixelated_zoom && view.scale >= 4.0);
            render::picture(frame, src, rect, smooth, p.has_alpha, ground);
        } else {
            frame.px.fill(ground.backdrop);
            if current.is_none() {
                self.centred(frame, "Drop a picture here", fh / 2.0 - 13.0 * s, 17.0 * s, ink);
                let open = if cfg!(target_os = "macos") { "or press ⌘O to choose one" } else { "or press Ctrl+O to choose one" };
                self.centred(frame, open, fh / 2.0 + 14.0 * s, 13.0 * s, soft);
            }
        }

        // The caption: the file's name, its place in the folder and the zoom.
        let caption = self.caption(now);
        self.drawn_caption = caption;
        if let (true, Some(path)) = (caption > 0.0, &current) {
            let size = 13.0 * s;
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let name = self.text.fitted(&name, size, fw * 0.5);
            let place = format!("{} / {}", self.folder.index + 1, self.folder.len());
            let zoom = self.shown.as_ref().map(|_| format!("{:.0}%", self.view(now).scale * 100.0)).unwrap_or_default();
            let parts = [(name, 0xFFFFFF), (place, 0xB4B4BC), (zoom, 0xB4B4BC)];
            let gap = 16.0 * s;
            let widths: Vec<f32> = parts.iter().map(|(t, _)| self.text.width(t, size)).collect();
            let inner: f32 = widths.iter().sum::<f32>() + gap * parts.iter().filter(|(t, _)| !t.is_empty()).count().saturating_sub(1) as f32;
            let (w, h) = (inner + 36.0 * s, 36.0 * s);
            let (x, y) = (((fw - w) / 2.0).round(), (fh - h - 18.0 * s).round());
            render::rounded(frame, x, y, w, h, h / 2.0, 0x121214, 0.74 * caption);
            let mut pen = x + 18.0 * s;
            for ((t, colour), width) in parts.iter().zip(&widths) {
                if !t.is_empty() {
                    self.text.draw(frame, t, pen, y + h / 2.0, size, *colour, caption);
                    pen += width + gap;
                }
            }
        }

        if self.settings.info_panel {
            self.paint_info(frame);
        }

        if let Some((what, _)) = &self.toast {
            let size = 13.0 * s;
            let what = self.text.fitted(what, size, fw - 80.0 * s);
            let (w, h) = (self.text.width(&what, size) + 36.0 * s, 36.0 * s);
            let (x, y) = (((fw - w) / 2.0).round(), (18.0 * s).round());
            render::rounded(frame, x, y, w, h, h / 2.0, 0x121214, 0.82);
            self.text.draw(frame, &what, x + 18.0 * s, y + h / 2.0, size, 0xFFFFFF, 1.0);
        }
        self.last_draw = Instant::now();
        self.dirty = false;
    }

    fn centred(&self, frame: &mut Frame, what: &str, y: f32, size: f32, rgb: u32) {
        let what = self.text.fitted(what, size, frame.w as f32 - 40.0 * self.scale);
        let x = (frame.w as f32 - self.text.width(&what, size)) / 2.0;
        self.text.draw(frame, &what, x.round(), y, size, rgb, 1.0);
    }

    /// The information panel: what the file is, and what its camera wrote into it.
    fn paint_info(&self, frame: &mut Frame) {
        let (Some(shown), Some(entry)) = (&self.shown, self.folder.current()) else { return };
        let s = self.scale;
        let p = &shown.loaded.picture;
        let mut rows: Vec<(String, String)> = vec![("Name".into(), entry.name.clone())];
        if let Some(dir) = entry.path.parent() {
            rows.push(("Folder".into(), dir.to_string_lossy().into_owned()));
        }
        rows.push(("Size".into(), view_core::human_bytes(shown.loaded.file_size)));
        rows.push(("Dimensions".into(), format!("{} × {}", p.width, p.height)));
        if let Some(f) = p.details.format {
            rows.push(("Format".into(), f.name().to_string()));
        }
        if let (Some(c), Some(b)) = (&p.details.channels, p.details.bits_per_channel) {
            rows.push(("Colour".into(), format!("{c}, {b} bits")));
        }
        if p.frames.len() > 1 {
            rows.push(("Frames".into(), p.frames.len().to_string()));
        }
        rows.extend(shown.loaded.facts.iter().map(|f| (f.label.to_string(), f.value.clone())));
        let (size, line, pad) = (12.5 * s, 23.0 * s, 16.0 * s);
        let label_w = rows.iter().map(|(l, _)| self.text.width(l, size)).fold(0.0, f32::max) + 14.0 * s;
        let w = (320.0 * s).min(frame.w as f32 - 24.0 * s);
        let max_rows = (((frame.h as f32 - 24.0 * s - pad * 2.0) / line).floor().max(0.0)) as usize;
        rows.truncate(max_rows);
        if rows.is_empty() || w < label_w + 40.0 * s {
            return;
        }
        let h = rows.len() as f32 * line + pad * 2.0;
        let (x, y) = (12.0 * s, 12.0 * s);
        render::rounded(frame, x, y, w, h, 14.0 * s, 0x121214, 0.80);
        for (i, (label, value)) in rows.iter().enumerate() {
            let mid = y + pad + line * (i as f32 + 0.5);
            self.text.draw(frame, label, x + pad, mid, size, 0x9C9CA4, 1.0);
            let value = self.text.fitted(value, size, w - pad * 2.0 - label_w);
            self.text.draw(frame, &value, x + pad + label_w, mid, size, 0xFFFFFF, 1.0);
        }
    }

    // ---- The window ----

    fn retitle(&self) {
        let Some(window) = &self.window else { return };
        match self.folder.current() {
            Some(e) => window.set_title(&format!("{} — View", e.name)),
            None => window.set_title("View"),
        }
    }

    fn resized(&mut self, size: PhysicalSize<u32>) {
        self.size = (size.width.max(1), size.height.max(1));
        self.tween = None;
        self.vp.resize(self.size.0 as f64, self.size.1 as f64);
        self.dirty = true;
    }

    fn toggle_fullscreen(&mut self) {
        let Some(window) = &self.window else { return };
        window.set_fullscreen(if window.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) });
    }

    fn redraw(&mut self) {
        let (Some(window), Some(mut surface)) = (self.window.clone(), self.surface.take()) else { return };
        let (w, h) = self.size;
        if let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) {
            if surface.resize(nw, nh).is_ok() {
                if let Ok(mut buffer) = surface.buffer_mut() {
                    self.paint(&mut Frame { px: &mut buffer, w: w as usize, h: h as usize }, Instant::now());
                    window.pre_present_notify();
                    let _ = buffer.present();
                }
            }
        }
        self.surface = Some(surface);
    }

    /// Remember where the window was and anything changed with the keys.
    fn save(&mut self) {
        if let Some(window) = &self.window {
            if window.fullscreen().is_none() {
                let size = window.inner_size();
                let at = window.outer_position().unwrap_or(PhysicalPosition::new(0, 0));
                self.settings.placement = Some(Placement { x: at.x, y: at.y, width: size.width as i32, height: size.height as i32, maximized: window.is_maximized() });
            }
        }
        if self.settings != self.saved {
            let dir = platform::settings_dir();
            let _ = std::fs::create_dir_all(&dir);
            if self.settings.save(&dir.join("settings.json")).is_ok() {
                self.saved = self.settings.clone();
            }
        }
    }

    fn key(&mut self, event_loop: &ActiveEventLoop, key: Key<&str>) {
        // The key that goes with letters for commands: ⌘ on a Mac, Ctrl elsewhere.
        let command = if cfg!(target_os = "macos") { self.mods.super_key() } else { self.mods.control_key() };
        let shift = self.mods.shift_key();
        let step = 64.0 * self.scale as f64;
        let overflows = self.vp.overflows();
        let (mx, my) = self.middle();
        match key {
            Key::Named(NamedKey::ArrowRight) if command => self.pan(-step, 0.0),
            Key::Named(NamedKey::ArrowLeft) if command => self.pan(step, 0.0),
            Key::Named(NamedKey::ArrowUp) if overflows => self.pan(0.0, step),
            Key::Named(NamedKey::ArrowDown) if overflows => self.pan(0.0, -step),
            Key::Named(NamedKey::ArrowRight | NamedKey::ArrowDown | NamedKey::PageDown | NamedKey::Space) => self.go(1),
            Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp | NamedKey::PageUp | NamedKey::Backspace) => self.go(-1),
            Key::Named(NamedKey::Home) => self.go_to(0),
            Key::Named(NamedKey::End) => self.go_to(self.folder.len().saturating_sub(1)),
            Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
            Key::Named(NamedKey::Enter) if self.mods.alt_key() => self.toggle_fullscreen(),
            Key::Named(NamedKey::Delete) => self.delete(),
            Key::Named(NamedKey::F5) => {
                if let Some(path) = self.current() {
                    self.open(&path);
                }
            }
            Key::Named(NamedKey::Escape) => {
                let fullscreen = self.window.as_ref().is_some_and(|w| w.fullscreen().is_some());
                if fullscreen {
                    self.toggle_fullscreen();
                } else if self.settings.info_panel {
                    self.settings.info_panel = false;
                    self.dirty = true;
                } else if self.settings.escape_closes {
                    self.close(event_loop);
                }
            }
            Key::Character(c) => match c.to_lowercase().as_str() {
                "+" | "=" => self.zoom(|vp| vp.zoom_to(vp.step(true), mx, my)),
                "-" | "_" => self.zoom(|vp| vp.zoom_to(vp.step(false), mx, my)),
                "0" => self.zoom(|vp| vp.fit()),
                "1" => self.zoom(|vp| vp.zoom_to(1.0, mx, my)),
                "o" => self.pick(),
                "c" if command => self.copy(),
                "w" | "q" if command => self.close(event_loop),
                "f" => self.toggle_fullscreen(),
                "i" => {
                    self.settings.info_panel = !self.settings.info_panel;
                    self.dirty = true;
                }
                "r" if shift => self.turn(Orient::rotated_ccw),
                "r" => self.turn(Orient::rotated_cw),
                "l" => self.turn(Orient::rotated_ccw),
                "h" => self.turn(Orient::flipped_horizontally),
                "v" => self.turn(Orient::flipped_vertically),
                _ => {}
            },
            _ => {}
        }
    }

    fn close(&mut self, event_loop: &ActiveEventLoop) {
        self.save();
        event_loop.exit();
    }
}

fn size_of(loaded: &Loaded) -> (u32, u32) {
    loaded.picture.frames.first().map_or((1, 1), |f| (f.bitmap.width.max(1), f.bitmap.height.max(1)))
}

fn frame_time(p: &picture::Picture, frame: usize) -> Duration {
    Duration::from_millis(p.frames.get(frame).map_or(100, |f| f.delay_ms.max(10)) as u64)
}

fn icon() -> Option<winit::window::Icon> {
    let img = image::load_from_memory_with_format(include_bytes!("../../../assets/icon.png"), image::ImageFormat::Png).ok()?;
    let small = img.resize(128, 128, image::imageops::FilterType::Triangle).into_rgba8();
    let (w, h) = small.dimensions();
    winit::window::Icon::from_rgba(small.into_raw(), w, h).ok()
}

impl ApplicationHandler<Msg> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attributes = Window::default_attributes().with_title("View").with_window_icon(icon()).with_min_inner_size(LogicalSize::new(320.0, 240.0));
        attributes = match self.settings.placement {
            Some(p) if p.width > 100 && p.height > 100 => attributes.with_inner_size(PhysicalSize::new(p.width as u32, p.height as u32)).with_maximized(p.maximized),
            _ => attributes.with_inner_size(LogicalSize::new(1100.0, 760.0)),
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            // The name a desktop file's StartupWMClass and the Wayland app ID match on.
            use winit::platform::wayland::WindowAttributesExtWayland;
            attributes = attributes.with_name("bero-view", "bero-view");
        }
        let window = match event_loop.create_window(attributes) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("View couldn't open a window: {e}");
                event_loop.exit();
                return;
            }
        };
        let surface = softbuffer::Context::new(window.clone()).and_then(|context| softbuffer::Surface::new(&context, window.clone()));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("View couldn't draw to its window: {e}");
                event_loop.exit();
                return;
            }
        }
        self.scale = window.scale_factor() as f32;
        self.dark = window.theme() != Some(winit::window::Theme::Light);
        if self.start_fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let size = window.inner_size();
        self.window = Some(window);
        self.resized(size);
        if let Some(file) = self.start.take() {
            self.open(&file);
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, msg: Msg) {
        match msg {
            Msg::Done(done) => self.on_done(done),
            Msg::Picked(Some(path)) => self.open(&path),
            Msg::Picked(None) => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.close(event_loop),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(size) => self.resized(size),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor as f32;
                self.dirty = true;
            }
            WindowEvent::ThemeChanged(theme) => {
                self.dark = theme != winit::window::Theme::Light;
                self.dirty = true;
            }
            WindowEvent::ModifiersChanged(mods) => self.mods = mods.state(),
            WindowEvent::DroppedFile(path) => self.open(&path),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => self.key(event_loop, event.logical_key.as_ref()),
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x, position.y);
                if let Some((lx, ly)) = self.drag {
                    if (x, y) != (lx, ly) {
                        self.pan(x - lx, y - ly);
                        self.drag = Some((x, y));
                        self.last_press = None;
                    }
                }
                self.cursor = (x, y);
                self.show_caption();
            }
            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => {
                    if self.last_press.take().is_some_and(|at| at.elapsed() < DOUBLE_CLICK) {
                        self.toggle_fullscreen();
                    } else {
                        self.last_press = Some(Instant::now());
                        self.drag = Some(self.cursor);
                    }
                }
                (MouseButton::Left, ElementState::Released) => self.drag = None,
                (MouseButton::Middle, ElementState::Pressed) => {
                    let (x, y) = self.cursor;
                    let fitted = self.vp.is_fitted();
                    self.zoom(|vp| if fitted { vp.zoom_to(1.0, x, y) } else { vp.fit() });
                }
                (MouseButton::Back, ElementState::Pressed) => self.go(-1),
                (MouseButton::Forward, ElementState::Pressed) => self.go(1),
                _ => {}
            },
            WindowEvent::MouseWheel { delta, .. } => match delta {
                MouseScrollDelta::LineDelta(_, lines) if lines != 0.0 => {
                    let command = if cfg!(target_os = "macos") { self.mods.super_key() } else { self.mods.control_key() };
                    if (self.settings.wheel == Wheel::Zoom) != command {
                        let (x, y) = self.cursor;
                        self.zoom(|vp| vp.zoom_by(1.2f64.powf(lines as f64), x, y));
                    } else {
                        self.go(if lines > 0.0 { -1 } else { 1 });
                    }
                }
                // A trackpad: two fingers move the picture, as everywhere else on a Mac.
                MouseScrollDelta::PixelDelta(by) => self.pan(by.x, by.y),
                _ => {}
            },
            WindowEvent::PinchGesture { delta, .. } => {
                let (x, y) = self.cursor;
                self.tween = None;
                self.vp.zoom_by(1.0 + delta, x, y);
                self.show_caption();
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else { return };
        let now = Instant::now();
        self.tick(now);
        if self.tween.is_some_and(|t| !t.at(now).1) {
            // The zoom has arrived: one last frame, exactly where it was going.
            self.tween = None;
            self.dirty = true;
        }
        let hertz = window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).map_or(60.0, |m| m as f64 / 1000.0).clamp(24.0, 480.0);
        let frame = Duration::from_secs_f64(1.0 / hertz);
        if self.dirty {
            window.request_redraw();
        }
        match self.next_wake(now, frame) {
            Some(at) if at <= now => {
                window.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}
