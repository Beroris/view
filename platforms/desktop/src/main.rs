//! View for Linux and macOS.
//!
//! `bero-view [file or folder] [--fullscreen]` opens a picture. It is a lean relative of the
//! Windows app: the same engine for folders, zooming, caching and metadata, with every pixel
//! decoded and drawn by View itself, so it needs no GPU, toolkit or system codecs.
//!
//! `bero-view --render <picture> <out.png> [width height]` draws one frame to a file without
//! opening a window, for checking a build on a machine with no display.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod load;
mod platform;
mod render;
mod text;

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("View {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("View {}\n\nbero-view [file or folder] [--fullscreen]\nbero-view --render <picture> <out.png> [width height]", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--render") {
        let size = |n: usize, default: u32| args.get(i + n).and_then(|s| s.parse().ok()).unwrap_or(default);
        let (Some(file), Some(out)) = (args.get(i + 1), args.get(i + 2)) else {
            eprintln!("bero-view --render <picture> <out.png> [width height]");
            std::process::exit(2);
        };
        match app::render_to_file(&PathBuf::from(file), &PathBuf::from(out), size(3, 1280), size(4, 800)) {
            Ok(note) => println!("{note}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let fullscreen = args.iter().any(|a| a == "--fullscreen");
    let file = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from);
    if let Err(e) = app::run(file, fullscreen) {
        eprintln!("View couldn't start: {e}");
        std::process::exit(1);
    }
}
