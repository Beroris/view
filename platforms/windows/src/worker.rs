//! Decoding off the interface thread. A few worker threads take jobs nearest-first; the window
//! hears about each result through a posted message and picks it up from the done list.
//!
//! Jobs that stop mattering (the user moved on) are dropped from the queue, and the ones
//! already running are told to stop through their cancel flag, which the decoders check
//! between bands of rows.

use crate::util::trace;
use crate::wic::{Failure, Request, Wic};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use view_core::cache::{Quality, Stamp};
use view_core::formats::{self, Format};
use view_core::picture::Picture;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted to the window when a job finishes.
pub const WM_JOB_DONE: u32 = WM_APP + 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Task {
    /// Decode for showing, at `Quality`.
    Decode(Quality),
    /// Read EXIF for the information panel.
    Metadata,
    /// Decode at full size, upright, and encode a PNG, for the clipboard.
    Copy,
}

pub struct Job {
    pub path: PathBuf,
    pub task: Task,
    /// Lower goes first.
    pub priority: u32,
    /// The screen size, for `Quality::Screen`.
    pub screen: (u32, u32),
    /// The most pixels a full decode may keep.
    pub max_pixels: u64,
}

pub enum Output {
    Picture(Arc<Picture>),
    Camera(Option<view_core::metadata::Camera>),
    Copy { width: u32, height: u32, pbgra: Vec<u8>, png: Option<Vec<u8>> },
    Failed { message: String, missing: bool },
}

pub struct Done {
    pub path: PathBuf,
    pub task: Task,
    /// What the decode turned out to be: a screen-sized request for a small image is full.
    pub quality: Quality,
    pub stamp: Option<Stamp>,
    pub output: Output,
}

struct Running {
    path: PathBuf,
    task: Task,
    cancel: Arc<AtomicBool>,
}

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    wake: Condvar,
    running: Mutex<Vec<Running>>,
    done: Mutex<Vec<Done>>,
    finished: Condvar,
    hwnd: AtomicIsize,
    quit: AtomicBool,
}

pub struct Pool {
    shared: Arc<Shared>,
}

impl Pool {
    pub fn new(threads: usize) -> Pool {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            running: Mutex::new(Vec::new()),
            done: Mutex::new(Vec::new()),
            finished: Condvar::new(),
            hwnd: AtomicIsize::new(0),
            quit: AtomicBool::new(false),
        });
        for i in 0..threads.max(1) {
            let shared = shared.clone();
            let _ = std::thread::Builder::new().name(format!("decode-{i}")).stack_size(1 << 20).spawn(move || work(shared));
        }
        Pool { shared }
    }

    /// Where to post results. Results finished before this are posted for at once.
    pub fn set_window(&self, hwnd: HWND) {
        self.shared.hwnd.store(hwnd.0 as isize, Ordering::Release);
        if !self.shared.done.lock().unwrap().is_empty() {
            notify(&self.shared);
        }
    }

    pub fn submit(&self, job: Job) {
        let mut q = self.shared.queue.lock().unwrap();
        if q.iter().any(|j| j.path == job.path && j.task == job.task) {
            // Already waiting: just move it up if it's more urgent now.
            for j in q.iter_mut().filter(|j| j.path == job.path && j.task == job.task) {
                j.priority = j.priority.min(job.priority);
            }
            return;
        }
        if self.shared.running.lock().unwrap().iter().any(|r| r.path == job.path && r.task == job.task && !r.cancel.load(Ordering::Relaxed)) {
            return;
        }
        // Finished and waiting to be picked up: the first photo is decoded while the window is
        // still being made, and asking again then would decode it a second time.
        if self.shared.done.lock().unwrap().iter().any(|d| d.path == job.path && d.task == job.task) {
            return;
        }
        q.push_back(job);
        drop(q);
        self.shared.wake.notify_one();
    }

    /// Keep only the work `wanted` still says yes to: drop the rest from the queue and tell the
    /// running ones to stop. Copies are never dropped; the user asked for them.
    pub fn retain(&self, wanted: impl Fn(&Path, Task) -> bool) {
        self.shared.queue.lock().unwrap().retain(|j| j.task == Task::Copy || wanted(&j.path, j.task));
        for r in self.shared.running.lock().unwrap().iter() {
            if r.task != Task::Copy && !wanted(&r.path, r.task) {
                r.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Wait up to `timeout` for a result.
    pub fn wait_done(&self, timeout: std::time::Duration) {
        let done = self.shared.done.lock().unwrap();
        if done.is_empty() {
            let _ = self.shared.finished.wait_timeout(done, timeout);
        }
    }

    pub fn take_done(&self) -> Vec<Done> {
        std::mem::take(&mut *self.shared.done.lock().unwrap())
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        for r in self.shared.running.lock().unwrap().iter() {
            r.cancel.store(true, Ordering::Relaxed);
        }
        self.shared.wake.notify_all();
    }
}

fn notify(shared: &Shared) {
    let hwnd = shared.hwnd.load(Ordering::Acquire);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_JOB_DONE, WPARAM(0), LPARAM(0));
        }
    }
}

fn work(shared: Arc<Shared>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let wic = Wic::new();
    loop {
        let job = {
            let mut q = shared.queue.lock().unwrap();
            loop {
                if shared.quit.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(best) = q.iter().enumerate().min_by_key(|(_, j)| j.priority).map(|(i, _)| i) {
                    break q.remove(best).unwrap();
                }
                q = shared.wake.wait(q).unwrap();
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        shared.running.lock().unwrap().push(Running { path: job.path.clone(), task: job.task, cancel: cancel.clone() });
        let started = std::time::Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(wic.as_ref(), &job, &cancel)));
        shared.running.lock().unwrap().retain(|r| !Arc::ptr_eq(&r.cancel, &cancel));
        let done = match result {
            Ok(Some(done)) => done,
            Ok(None) => continue, // cancelled
            Err(_) => Done {
                path: job.path.clone(),
                task: job.task,
                quality: Quality::Full,
                stamp: None,
                output: Output::Failed { message: "View hit a problem decoding this image.".into(), missing: false },
            },
        };
        trace(&format!("{:?} {} in {:.1} ms", job.task, job.path.display(), started.elapsed().as_secs_f64() * 1000.0));
        if cancel.load(Ordering::Relaxed) && job.task != Task::Copy {
            continue;
        }
        shared.done.lock().unwrap().push(done);
        shared.finished.notify_all();
        notify(&shared);
    }
}

fn run(wic: Option<&Wic>, job: &Job, cancel: &AtomicBool) -> Option<Done> {
    let stamp = Stamp::of(&job.path);
    let finish = |quality, output| Some(Done { path: job.path.clone(), task: job.task, quality, stamp, output });
    match job.task {
        Task::Metadata => finish(Quality::Full, Output::Camera(view_core::metadata::read(&job.path))),
        Task::Decode(quality) => {
            let req = match quality {
                Quality::Screen => Request { max_width: job.screen.0, max_height: job.screen.1, max_pixels: job.max_pixels, slack: 2.25 },
                Quality::Full => Request { max_width: 0, max_height: 0, max_pixels: job.max_pixels, slack: 1.0 },
            };
            match decode(wic, &job.path, req, cancel) {
                Ok(p) => {
                    let q = if p.is_full() { Quality::Full } else { quality };
                    finish(q, Output::Picture(Arc::new(p)))
                }
                Err(Failure::Cancelled) => None,
                Err(f) => finish(
                    quality,
                    Output::Failed { message: f.message(), missing: matches!(f, Failure::NotFound) },
                ),
            }
        }
        Task::Copy => {
            let req = Request { max_width: 0, max_height: 0, max_pixels: job.max_pixels, slack: 1.0 };
            match decode(wic, &job.path, req, cancel) {
                Ok(p) => {
                    let f = &p.frames[0].bitmap;
                    let (w, h, pixels) = view_core::orient::apply(p.orient, f.width, f.height, &f.data);
                    let png = wic.and_then(|w2| w2.encode_png(w, h, &pixels));
                    finish(Quality::Full, Output::Copy { width: w, height: h, pbgra: pixels, png })
                }
                Err(f) => finish(Quality::Full, Output::Failed { message: f.message(), missing: matches!(f, Failure::NotFound) }),
            }
        }
    }
}

/// Decode a file with whichever decoder suits it.
pub fn decode(wic: Option<&Wic>, path: &Path, req: Request, cancel: &AtomicBool) -> Result<Picture, Failure> {
    let io = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound => Failure::NotFound,
        std::io::ErrorKind::PermissionDenied => Failure::Denied,
        _ => Failure::Broken(format!("View couldn't read the file: {e}")),
    };
    const HEAD: usize = 4096;
    let mut head = read_head(path, HEAD).map_err(io)?;
    if head.is_empty() {
        return Err(Failure::Broken("The file is empty.".into()));
    }
    let sniffed = formats::sniff(&head);
    // An APNG's marker comes before the first image data, but big profiles and EXIF blocks can
    // push it past the first few kilobytes.
    if sniffed == Some(Format::Png) && head.len() == HEAD && !formats::is_apng(&head) {
        head = read_head(path, 256 << 10).map_err(io)?;
    }
    let own = sniffed.filter(|&f| view_core::decode::handles(f, &head, true));
    if let Some(format) = own {
        return decode_own(wic, path, format, req, cancel);
    }
    let Some(wic) = wic else { return Err(Failure::Broken("Windows' imaging component isn't available.".into())) };
    match wic.decode(path, sniffed, req, cancel) {
        // No Windows codec (or it failed) for a format View can decode itself: do that instead.
        Err(Failure::NoCodec(_) | Failure::Broken(_)) if matches!(sniffed, Some(Format::WebP | Format::Png | Format::Gif)) => {
            decode_own(Some(wic), path, sniffed.unwrap(), req, cancel)
        }
        other => other,
    }
}

fn decode_own(wic: Option<&Wic>, path: &Path, format: Format, req: Request, cancel: &AtomicBool) -> Result<Picture, Failure> {
    let data = std::fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Failure::NotFound,
        _ => Failure::Broken(format!("View couldn't read the file: {e}")),
    })?;
    let limits = view_core::decode::Limits {
        max_width: req.max_width,
        max_height: req.max_height,
        max_pixels: req.max_pixels,
        ..Default::default()
    };
    let mut p = view_core::decode::decode(&data, format, limits, cancel).map_err(|e| if e == "cancelled" { Failure::Cancelled } else { Failure::Broken(e) })?;
    if let (Some(icc), Some(wic)) = (p.icc.take(), wic) {
        let mut managed = false;
        for f in p.frames.iter_mut() {
            managed |= wic.manage_bitmap(&mut f.bitmap, &icc);
        }
        p.details.colour_managed = managed;
    }
    Ok(p)
}

fn read_head(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max];
    let mut n = 0;
    while n < max {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    buf.truncate(n);
    Ok(buf)
}
