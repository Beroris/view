// The Windows app. Included by main.rs, which is all there is of this crate on other systems.

mod app;
mod cmd;
mod doc;
mod editor;
mod explorer;
mod gfx;
mod media;
mod menu;
mod model;
mod ocr;
mod pdf;
mod pick;
mod popup;
mod shell;
mod spacebar;
mod text;
mod thumbs;
mod trim;
mod theme;
mod ui;
mod update;
mod util;
mod wic;
mod words;
mod worker;

use std::path::PathBuf;
use util::{Wide, trace};
use view_core::cache::Quality;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::*;

fn main() {
    trace("start");
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--quit") {
        quit_all();
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--decode-bench") {
        std::process::exit(bench::decode(&args[i + 1..]));
    }
    if let Some(i) = args.iter().position(|a| a == "--selection-test") {
        // Development aid: what Space preview would show for an Explorer window.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let hwnd = windows::Win32::Foundation::HWND(args.get(i + 1).and_then(|h| h.parse::<isize>().ok()).unwrap_or(0) as *mut _);
        let t = std::time::Instant::now();
        let found = explorer::selected_file(hwnd);
        let _ = std::fs::write(std::env::temp_dir().join("view-bench.txt"), format!("{found:?} in {:.1} ms
", t.elapsed().as_secs_f64() * 1000.0));
        return;
    }
    if args.iter().any(|a| a == "--space-preview") {
        std::process::exit(spacebar::run());
    }
    // `--peek <explorer window> <file>`: the Space preview, started by the helper.
    let peek = args.iter().position(|a| a == "--peek").and_then(|i| args.get(i + 1)).and_then(|h| h.parse::<isize>().ok()).map(|h| windows::Win32::Foundation::HWND(h as *mut _));
    // Held for as long as any View runs (several can), so the installer can tell when the last one
    // has gone after asking them to quit. Deliberately never closed.
    let _running = unsafe { windows::Win32::System::Threading::CreateMutexW(None, false, windows::core::w!("Local\\View.Running")) };
    let fullscreen = args.iter().any(|a| a == "--fullscreen");
    let path = match peek {
        Some(_) => args.iter().skip_while(|a| *a != "--peek").nth(2).map(PathBuf::from),
        None => args.iter().enumerate().find(|(i, a)| !a.starts_with("--") && (*i == 0 || args[i - 1] != "--at")).map(|(_, a)| PathBuf::from(a)),
    };

    let settings = view_core::settings::Settings::load(&util::settings_file());

    // Start the slow parts at once, side by side: decoding the image and loading the GPU driver.
    let (rect, _) = app::initial_rect(&settings);
    let screen = app::monitor_size(&rect);
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 4) - 1;
    let pool = worker::Pool::new(threads.max(2));
    if let Some(p) = &path {
        let p = std::path::absolute(p).unwrap_or(p.clone());
        let pixels = screen.0 as u64 * screen.1 as u64;
        if !app::is_media(&p) {
            pool.submit(worker::Job { path: p, task: worker::Task::Decode(Quality::Screen), priority: 0, screen, max_pixels: pixels });
        }
    }
    let d3d = std::thread::Builder::new().name("gpu".into()).spawn(gfx::create_d3d).expect("thread");

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        // OLE too, for dragging files out of the window.
        let _ = windows::Win32::System::Ole::OleInitialize(None);
    }
    let at = args.iter().position(|a| a == "--at").and_then(|i| args.get(i + 1)).and_then(|t| t.parse::<f64>().ok());
    let updated = args.iter().any(|a| a == update::UPDATED_ARG);
    let start = app::Start { path, settings, pool, screen, d3d, fullscreen, peek, at, updated };
    if app::create(start).is_none() {
        return;
    }
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Close every View window, the Space preview and its helper, and wait (up to a few seconds)
/// for them to go.
fn quit_all() {
    let classes = [app::CLASS, spacebar::PEEK_CLASS, spacebar::CLASS].map(Wide::new);
    for _ in 0..50 {
        let found: Vec<_> = classes.iter().filter_map(|c| unsafe { FindWindowW(c.pcwstr(), None) }.ok()).collect();
        if found.is_empty() {
            return;
        }
        for hwnd in found {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Timing decodes from the command line, for measuring: `view.exe --decode-bench <file>...`.
mod bench {
    use crate::wic::{Request, Wic};
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    pub fn decode(files: &[String]) -> i32 {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let Some(wic) = Wic::new() else { return 2 };
        let mut out = String::new();
        let cancel = AtomicBool::new(false);
        for f in files {
            let path = std::path::Path::new(f);
            for (name, req) in [
                ("screen", Request { max_width: 2560, max_height: 1440, max_pixels: 2560 * 1440, slack: 2.25 }),
                ("full", Request { max_width: 0, max_height: 0, max_pixels: 268_435_456, slack: 1.0 }),
            ] {
                let mut best = f64::MAX;
                let mut size = (0, 0);
                let mut err = None;
                for _ in 0..5 {
                    let t = Instant::now();
                    match crate::worker::decode(Some(&wic), path, req, &cancel) {
                        Ok(p) => size = (p.frames[0].bitmap.width, p.frames[0].bitmap.height),
                        Err(e) => err = Some(e.message()),
                    }
                    best = best.min(t.elapsed().as_secs_f64() * 1000.0);
                }
                match err {
                    Some(e) => out.push_str(&format!("{f}\t{name}\tERROR {e}\n")),
                    None => out.push_str(&format!("{f}\t{name}\t{}x{}\t{best:.1} ms\n", size.0, size.1)),
                }
            }
        }
        let _ = std::fs::write(std::env::temp_dir().join("view-bench.txt"), &out);
        0
    }
}
