//! Thumbnails for the filmstrip and the grid, made off the window's thread. Windows' own
//! thumbnail cache comes first, since it's usually there already and quick; where Windows has
//! none (a format without a thumbnail handler), View decodes a small copy itself; failing both,
//! the file type's icon. Only what's asked for is made, nearest first, and the least recently
//! drawn are let go past a memory budget.

use crate::gfx::{Gpu, Texture};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use view_core::picture::Bitmap;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted when thumbnails are ready to upload.
pub const WM_THUMB: u32 = WM_APP + 9;
/// The longest side of a thumbnail, in pixels.
pub const SIZE: u32 = 320;
/// How much GPU memory thumbnails may take.
const BUDGET: usize = 160 << 20;
const THREADS: usize = 2;

enum Slot {
    Ready { texture: Texture, used: u64 },
    Failed,
}

#[derive(Default)]
struct Shared {
    /// What to make next, most wanted first.
    queue: Mutex<Vec<PathBuf>>,
    wake: Condvar,
    /// Being made now.
    busy: Mutex<HashSet<PathBuf>>,
    done: Mutex<Vec<(PathBuf, Option<Bitmap>)>>,
    hwnd: AtomicIsize,
    stop: AtomicBool,
}

pub struct Thumbs {
    shared: Arc<Shared>,
    slots: HashMap<PathBuf, Slot>,
    clock: u64,
    bytes: usize,
    started: bool,
}

impl Drop for Thumbs {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

impl Thumbs {
    pub fn new() -> Thumbs {
        Thumbs { shared: Arc::new(Shared::default()), slots: HashMap::new(), clock: 0, bytes: 0, started: false }
    }

    /// Ask for these files' thumbnails, most wanted first. Anything asked for before and not
    /// started yet is dropped in favour of this list.
    pub fn want<'a>(&mut self, hwnd: HWND, paths: impl Iterator<Item = &'a Path>) {
        self.shared.hwnd.store(hwnd.0 as isize, Ordering::Relaxed);
        let busy = self.shared.busy.lock().unwrap().clone();
        let list: Vec<PathBuf> = paths.filter(|p| !self.slots.contains_key(*p) && !busy.contains(*p)).map(Path::to_path_buf).collect();
        if list.is_empty() && self.shared.queue.lock().unwrap().is_empty() {
            return;
        }
        *self.shared.queue.lock().unwrap() = list;
        self.shared.wake.notify_all();
        if !self.started {
            self.started = true;
            for i in 0..THREADS {
                let shared = self.shared.clone();
                let _ = std::thread::Builder::new().name(format!("thumbs{i}")).spawn(move || work(shared));
            }
        }
    }

    /// A file's thumbnail, if it's been made. Marks it as just used.
    pub fn get(&mut self, path: &Path) -> Option<&Texture> {
        self.clock += 1;
        match self.slots.get_mut(path) {
            Some(Slot::Ready { texture, used }) => {
                *used = self.clock;
                Some(texture)
            }
            _ => None,
        }
    }

    /// Whether View gave up on a file's thumbnail.
    pub fn failed(&self, path: &Path) -> bool {
        matches!(self.slots.get(path), Some(Slot::Failed))
    }

    /// Put finished thumbnails on the GPU. True if any arrived.
    pub fn take(&mut self, gpu: Option<&Gpu>) -> bool {
        let done = std::mem::take(&mut *self.shared.done.lock().unwrap());
        let any = !done.is_empty();
        for (path, bitmap) in done {
            let texture = bitmap.as_ref().and_then(|b| gpu.and_then(|g| g.upload(b)));
            self.clock += 1;
            let slot = match texture {
                Some(texture) => {
                    self.bytes += texture.width as usize * texture.height as usize * 4;
                    Slot::Ready { texture, used: self.clock }
                }
                None => Slot::Failed,
            };
            if let Some(Slot::Ready { texture, .. }) = self.slots.insert(path, slot) {
                self.bytes -= texture.width as usize * texture.height as usize * 4;
            }
        }
        self.trim();
        any
    }

    /// Let go of the least recently drawn past the budget.
    fn trim(&mut self) {
        if self.bytes <= BUDGET {
            return;
        }
        let mut ready: Vec<(u64, PathBuf, usize)> = self
            .slots
            .iter()
            .filter_map(|(p, s)| match s {
                Slot::Ready { texture, used } => Some((*used, p.clone(), texture.width as usize * texture.height as usize * 4)),
                Slot::Failed => None,
            })
            .collect();
        ready.sort_by_key(|r| r.0);
        for (_, path, size) in ready {
            if self.bytes <= BUDGET * 3 / 4 {
                break;
            }
            self.slots.remove(&path);
            self.bytes -= size;
        }
    }

    /// Forget a file's thumbnail: it changed, or went.
    pub fn forget(&mut self, path: &Path) {
        if let Some(Slot::Ready { texture, .. }) = self.slots.remove(path) {
            self.bytes -= texture.width as usize * texture.height as usize * 4;
        }
    }

    /// The GPU was replaced: everything has to be made again.
    pub fn reset(&mut self) {
        self.slots.clear();
        self.bytes = 0;
    }
}

fn work(shared: Arc<Shared>) {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
    }
    let wic = crate::wic::Wic::new();
    loop {
        let path = {
            let mut queue = shared.queue.lock().unwrap();
            loop {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                if !queue.is_empty() {
                    break queue.remove(0);
                }
                queue = shared.wake.wait(queue).unwrap();
            }
        };
        if !shared.busy.lock().unwrap().insert(path.clone()) {
            continue;
        }
        let bitmap = make(wic.as_ref(), &path);
        shared.busy.lock().unwrap().remove(&path);
        shared.done.lock().unwrap().push((path, bitmap));
        let hwnd = shared.hwnd.load(Ordering::Relaxed);
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_THUMB, WPARAM(0), LPARAM(0));
        }
    }
}

/// One thumbnail: Windows' own, else a small decode, else the type's icon.
fn make(wic: Option<&crate::wic::Wic>, path: &Path) -> Option<Bitmap> {
    let shrink = |b: Bitmap| {
        let (w, h) = view_core::picture::fit_within(b.width, b.height, SIZE, SIZE, u64::MAX);
        if (w, h) == (b.width, b.height) { Some(b) } else { view_core::picture::downscale(&b, w, h) }
    };
    if let Some(b) = crate::shell::art(path, SIZE as i32) {
        return shrink(b);
    }
    if !crate::app::is_media(path) {
        let req = crate::wic::Request { max_width: SIZE, max_height: SIZE, max_pixels: (SIZE * SIZE) as u64 * 4, slack: 1.0 };
        if let Ok(p) = crate::worker::decode(wic, path, req, &AtomicBool::new(false)) {
            let f = &p.frames[0].bitmap;
            let (w, h, data) = view_core::orient::apply(p.orient, f.width, f.height, &f.data);
            return shrink(Bitmap { width: w, height: h, data });
        }
    }
    crate::shell::thumbnail(path, SIZE as i32).and_then(shrink)
}
