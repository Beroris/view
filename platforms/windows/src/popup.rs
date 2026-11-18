//! View's own pop-up menu, drawn with Direct2D in View's colours and font. Each level (the menu
//! and any submenu open from it) is a small window of its own, so menus can reach past the edge
//! of View's window like any other. Windows rounds their corners and gives them a shadow.
//!
//! While a menu is open, `show` runs its own message loop, as Windows' menus do: it holds the
//! mouse, reads the keyboard, and returns once something is picked or the menu is dismissed.

use crate::app::App;
use crate::cmd::Cmd;
use crate::gfx::{Gpu, rect};
use crate::text::{Align, Text};
use crate::theme::{Palette, Rgb};
use crate::util::Wide;
use std::cell::Cell;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

pub enum Mark {
    None,
    /// A tick, for an option that's on.
    Tick,
    /// A dot, for the chosen one of several.
    Dot,
}

pub enum Entry {
    Item { label: String, key: &'static str, cmd: Cmd, mark: Mark, enabled: bool },
    Sub { label: String, entries: Vec<Entry> },
    Separator,
}

impl Entry {
    fn selectable(&self) -> bool {
        match self {
            Entry::Item { enabled, .. } => *enabled,
            Entry::Sub { .. } => true,
            Entry::Separator => false,
        }
    }
}

// Sizes in DIPs.
const PAD: f32 = 5.0;
const ROW: f32 = 30.0;
const SEPARATOR: f32 = 9.0;
const GUTTER: f32 = 34.0;
const RIGHT: f32 = 14.0;
const ARROW: f32 = 22.0;
const TEXT: f32 = 13.0;
const KEY: f32 = 12.0;

const CLASS: &str = "View.Menu";

thread_local! {
    /// A menu window was uncovered and needs drawing again.
    static DAMAGED: Cell<bool> = const { Cell::new(false) };
}

struct Level {
    hwnd: HWND,
    gpu: Option<Gpu>,
    /// Which submenu this is: indexes from the top menu down.
    path: Vec<usize>,
    /// Top and height of each entry, in pixels from the window's top.
    rows: Vec<(f32, f32)>,
    hover: Option<usize>,
    /// On screen.
    rect: RECT,
}

impl Drop for Level {
    fn drop(&mut self) {
        self.gpu = None;
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn entries_at<'a>(root: &'a [Entry], path: &[usize]) -> &'a [Entry] {
    let mut list = root;
    for &i in path {
        match &list[i] {
            Entry::Sub { entries, .. } => list = entries,
            _ => return &[],
        }
    }
    list
}

struct Menu<'a> {
    root: &'a [Entry],
    owner: HWND,
    d3d: &'a ID3D11Device,
    text: &'a Text,
    palette: Palette,
    s: f32,
    levels: Vec<Level>,
}

/// Show `entries` with the top-left corner at screen point `x`, `y` (or the bottom-left, if
/// `upward`), and wait for the user. Returns the command picked, if any.
pub fn show(app: &App, entries: &[Entry], x: i32, y: i32, upward: bool) -> Option<Cmd> {
    let d3d = app.d3d.as_ref()?;
    register();
    let mut menu = Menu { root: entries, owner: app.hwnd, d3d, text: &app.text, palette: app.palette, s: app.scale(), levels: Vec::new() };
    menu.open(Vec::new(), x, y, upward, None)?;
    let picked = menu.run();
    menu.levels.clear();
    unsafe {
        let _ = ReleaseCapture();
    }
    picked
}

fn register() {
    let class = Wide::new(CLASS);
    let Ok(instance) = (unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }) else { return };
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(proc),
        hInstance: instance.into(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: class.pcwstr(),
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wc) };
}

extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            DAMAGED.with(|d| d.set(true));
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

impl Menu<'_> {
    fn px(&self, dip: f32) -> f32 {
        (dip * self.s).round()
    }

    fn measure(&self, s: &str, dip: f32) -> f32 {
        let Some(f) = self.text.format(self.px(dip), 400, Align::Left, false) else { return 0.0 };
        self.text.layout(s, &f, 10_000.0, 1000.0).map_or(0.0, |l| Text::size(&l).0)
    }

    /// Open a level for the entries at `path`. `beside` is the parent's row, on screen, that a
    /// submenu hangs from.
    fn open(&mut self, path: Vec<usize>, x: i32, y: i32, upward: bool, beside: Option<RECT>) -> Option<()> {
        let list = entries_at(self.root, &path);
        let mut rows = Vec::new();
        let mut top = self.px(PAD);
        let mut label_w: f32 = 0.0;
        let mut key_w: f32 = 0.0;
        for e in list {
            let h = if matches!(e, Entry::Separator) { self.px(SEPARATOR) } else { self.px(ROW) };
            rows.push((top, h));
            top += h;
            match e {
                Entry::Item { label, key, .. } => {
                    label_w = label_w.max(self.measure(label, TEXT));
                    if !key.is_empty() {
                        key_w = key_w.max(self.measure(key, KEY));
                    }
                }
                Entry::Sub { label, .. } => label_w = label_w.max(self.measure(label, TEXT)),
                Entry::Separator => {}
            }
        }
        let gap = if key_w > 0.0 { self.px(36.0) + key_w } else { 0.0 };
        let w = (self.px(GUTTER) + label_w + gap.max(self.px(ARROW)) + self.px(RIGHT)).max(self.px(200.0)).ceil() as i32;
        let h = (top + self.px(PAD)).ceil() as i32;

        // On the screen the point is on, flipped or nudged to fit.
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        unsafe {
            let _ = GetMonitorInfoW(MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST), &mut mi);
        }
        let work = mi.rcWork;
        let (mut left, mut top) = (x, if upward { y - h } else { y });
        if let Some(row) = beside {
            let overlap = self.px(4.0) as i32;
            left = row.right - overlap;
            if left + w > work.right {
                left = row.left - w + overlap;
            }
            top = row.top - self.px(PAD) as i32;
        } else if left + w > work.right {
            left = x - w;
        }
        if top + h > work.bottom {
            top = if beside.is_some() { work.bottom - h } else { y - h };
        }
        left = left.clamp(work.left, (work.right - w).max(work.left));
        top = top.clamp(work.top, (work.bottom - h).max(work.top));

        let class = Wide::new(CLASS);
        let instance = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }.ok()?;
        let hwnd = unsafe {
            CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, class.pcwstr(), w!(""), WS_POPUP, left, top, w, h, Some(self.owner), None, Some(instance.into()), None)
        }
        .ok()?;
        unsafe {
            let round = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as *const _, size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32);
            let border = border_colour(&self.palette).colorref();
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as *const _, 4);
            let dark = self.palette.dark as u32;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
        }
        let gpu = Gpu::new(hwnd, self.d3d, w as u32, h as u32).ok();
        let level = Level { hwnd, gpu, path, rows, hover: None, rect: RECT { left, top, right: left + w, bottom: top + h } };
        self.levels.push(level);
        let i = self.levels.len() - 1;
        self.paint(i);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            if i == 0 {
                SetCapture(hwnd);
            }
        }
        Some(())
    }

    fn paint(&mut self, i: usize) {
        let p = self.palette;
        let s = self.s;
        let px = |d: f32| (d * s).round();
        let text = self.text;
        let list = entries_at(self.root, &self.levels[i].path);
        let open_child = self.levels.get(i + 1).and_then(|l| l.path.last().copied());
        let level = &mut self.levels[i];
        let Some(gpu) = &mut level.gpu else { return };
        let w = gpu.width as f32;
        gpu.begin();
        gpu.clear(p.panel.d2d(1.0));
        for (n, (e, &(top, h))) in list.iter().zip(&level.rows).enumerate() {
            match e {
                Entry::Separator => {
                    gpu.fill(rect(px(PAD + 8.0), (top + h / 2.0).round(), w - px(PAD + 8.0) * 2.0, 1.0), p.edge.d2d(p.edge_alpha * 1.4));
                }
                Entry::Item { label, key, mark, enabled, .. } => {
                    let row = rect(px(PAD), top, w - px(PAD) * 2.0, h);
                    if level.hover == Some(n) && *enabled {
                        gpu.fill_round(row, px(6.0), p.hover.d2d(p.hover_alpha * 1.5));
                    }
                    let ink = if *enabled { p.text } else { p.dim };
                    let a = if *enabled { 1.0 } else { 0.55 };
                    let mark_box = rect(px(PAD), top, px(GUTTER) - px(PAD), h);
                    match mark {
                        Mark::Tick => glyph(text, gpu, '\u{E73E}', mark_box, px(12.0), ink, a),
                        Mark::Dot => gpu.fill_circle((mark_box.left + mark_box.right) / 2.0, top + h / 2.0, px(3.5), ink.d2d(a)),
                        Mark::None => {}
                    }
                    line(text, gpu, label, rect(px(GUTTER), top, w - px(GUTTER), h), px(TEXT), Align::Left, ink, a);
                    if !key.is_empty() {
                        let f = text.format(px(KEY), 400, Align::Left, false);
                        let kw = f.and_then(|f| text.layout(key, &f, 10_000.0, 1000.0)).map_or(0.0, |l| Text::size(&l).0);
                        line(text, gpu, key, rect(w - px(RIGHT) - kw - px(2.0), top, kw + px(4.0), h), px(KEY), Align::Left, p.dim, a);
                    }
                }
                Entry::Sub { label, .. } => {
                    let row = rect(px(PAD), top, w - px(PAD) * 2.0, h);
                    if level.hover == Some(n) || open_child == Some(n) {
                        gpu.fill_round(row, px(6.0), p.hover.d2d(p.hover_alpha * 1.5));
                    }
                    line(text, gpu, label, rect(px(GUTTER), top, w - px(GUTTER), h), px(TEXT), Align::Left, p.text, 1.0);
                    glyph(text, gpu, '\u{E76C}', rect(w - px(RIGHT) - px(14.0), top, px(14.0), h), px(10.0), p.dim, 1.0);
                }
            }
        }
        let _ = gpu.end();
    }

    fn repaint_all(&mut self) {
        for i in 0..self.levels.len() {
            self.paint(i);
        }
    }

    /// The level and row under a screen point, deepest level first.
    fn at(&self, pt: POINT) -> Option<(usize, Option<usize>)> {
        for (i, l) in self.levels.iter().enumerate().rev() {
            let r = l.rect;
            if pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom {
                let y = (pt.y - r.top) as f32;
                let row = l.rows.iter().position(|&(top, h)| y >= top && y < top + h);
                return Some((i, row));
            }
        }
        None
    }

    fn set_hover(&mut self, i: usize, row: Option<usize>) {
        if self.levels[i].hover != row {
            self.levels[i].hover = row;
            self.paint(i);
        }
    }

    /// Open the submenu at row `n` of level `i`, closing anything deeper first.
    fn open_sub(&mut self, i: usize, n: usize) {
        if self.levels.len() > i + 1 && self.levels[i + 1].path.last() == Some(&n) {
            return; // already open
        }
        self.levels.truncate(i + 1);
        let l = &self.levels[i];
        let (top, h) = l.rows[n];
        let row = RECT { left: l.rect.left, top: l.rect.top + top as i32, right: l.rect.right, bottom: l.rect.top + (top + h) as i32 };
        let mut path = l.path.clone();
        path.push(n);
        let _ = self.open(path, row.right, row.top, false, Some(row));
    }

    /// Move the highlight in the deepest level by one selectable row.
    fn step(&mut self, down: bool) {
        let i = self.levels.len() - 1;
        let list = entries_at(self.root, &self.levels[i].path);
        let n = list.len();
        if n == 0 {
            return;
        }
        let mut at = self.levels[i].hover.unwrap_or(if down { n - 1 } else { 0 });
        for _ in 0..n {
            at = if down { (at + 1) % n } else { (at + n - 1) % n };
            if list[at].selectable() {
                self.set_hover(i, Some(at));
                return;
            }
        }
    }

    /// Enter on the highlighted row: a command, or a submenu to step into.
    fn activate(&mut self, i: usize, n: usize) -> Option<Cmd> {
        match &entries_at(self.root, &self.levels[i].path)[n] {
            Entry::Item { cmd, enabled: true, .. } => Some(*cmd),
            Entry::Sub { .. } => {
                self.open_sub(i, n);
                if self.levels.len() > i + 1 {
                    self.step(true);
                }
                None
            }
            _ => None,
        }
    }

    fn run(&mut self) -> Option<Cmd> {
        let mut msg = MSG::default();
        loop {
            let root = self.levels.first()?.hwnd;
            if unsafe { GetCapture() } != root {
                return None; // something else took the mouse: switching apps, a dialog
            }
            if !unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                unsafe { PostQuitMessage(msg.wParam.0 as i32) };
                return None;
            }
            let screen = || {
                let mut pt = POINT { x: (msg.lParam.0 & 0xFFFF) as i16 as i32, y: ((msg.lParam.0 >> 16) & 0xFFFF) as i16 as i32 };
                unsafe {
                    let _ = ClientToScreen(msg.hwnd, &mut pt);
                }
                pt
            };
            match msg.message {
                WM_MOUSEMOVE => {
                    if let Some((i, row)) = self.at(screen()) {
                        let row = row.filter(|&n| entries_at(self.root, &self.levels[i].path)[n].selectable());
                        self.set_hover(i, row);
                        match row {
                            Some(n) if matches!(entries_at(self.root, &self.levels[i].path)[n], Entry::Sub { .. }) => self.open_sub(i, n),
                            Some(_) => {
                                if self.levels.len() > i + 1 {
                                    self.levels.truncate(i + 1);
                                }
                            }
                            None => {}
                        }
                    } else if let Some(last) = self.levels.last() {
                        // Off the menus: keep an open submenu's parent row lit, nothing else.
                        let i = self.levels.len() - 1;
                        if last.hover.is_some() {
                            self.set_hover(i, None);
                        }
                    }
                }
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                    if self.at(screen()).is_none() {
                        return None; // a click elsewhere closes the menu
                    }
                }
                WM_LBUTTONUP | WM_RBUTTONUP => {
                    if let Some((i, Some(n))) = self.at(screen()) {
                        if let Some(cmd) = self.activate(i, n) {
                            return Some(cmd);
                        }
                    }
                }
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    let deepest = self.levels.len() - 1;
                    match VIRTUAL_KEY(msg.wParam.0 as u16) {
                        VK_DOWN => self.step(true),
                        VK_UP => self.step(false),
                        VK_RIGHT => {
                            if let Some(n) = self.levels[deepest].hover {
                                if matches!(entries_at(self.root, &self.levels[deepest].path)[n], Entry::Sub { .. }) {
                                    self.activate(deepest, n);
                                }
                            }
                        }
                        VK_LEFT if deepest > 0 => self.levels.truncate(deepest),
                        VK_ESCAPE => {
                            if deepest == 0 {
                                return None;
                            }
                            self.levels.truncate(deepest);
                        }
                        VK_RETURN | VK_SPACE => {
                            if let Some(n) = self.levels[deepest].hover {
                                if let Some(cmd) = self.activate(deepest, n) {
                                    return Some(cmd);
                                }
                            }
                        }
                        VK_MENU | VK_F10 | VK_APPS | VK_TAB => return None,
                        _ => {}
                    }
                }
                WM_ACTIVATEAPP if msg.wParam.0 == 0 => unsafe {
                    DispatchMessageW(&msg);
                    return None;
                },
                WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR | WM_MOUSEWHEEL | WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK => {}
                _ => unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                },
            }
            if DAMAGED.with(|d| d.replace(false)) {
                self.repaint_all();
            }
        }
    }
}

/// The hairline round a menu: the panel colour a step towards the text colour.
fn border_colour(p: &Palette) -> Rgb {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * if p.dark { 0.16 } else { 0.14 }).round() as u8;
    Rgb(mix(p.panel.0, p.text.0), mix(p.panel.1, p.text.1), mix(p.panel.2, p.text.2))
}

fn line(text: &Text, gpu: &Gpu, s: &str, r: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F, px: f32, align: Align, c: Rgb, a: f32) {
    let Some(f) = text.format(px, 400, align, false) else { return };
    let Some(l) = text.layout(s, &f, r.right - r.left, r.bottom - r.top) else { return };
    let (_, th) = Text::size(&l);
    gpu.text(&l, r.left, (r.top + ((r.bottom - r.top) - th) / 2.0).round(), c.d2d(a));
}

fn glyph(text: &Text, gpu: &Gpu, g: char, r: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F, px: f32, c: Rgb, a: f32) {
    let Some(f) = text.icons(px) else { return };
    let mut buf = [0u8; 4];
    let Some(l) = text.layout(g.encode_utf8(&mut buf), &f, r.right - r.left, r.bottom - r.top) else { return };
    let (_, th) = Text::size(&l);
    gpu.text(&l, r.left, r.top + ((r.bottom - r.top) - th) / 2.0, c.d2d(a));
}
