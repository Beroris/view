//! View for Windows.
//!
//! `view.exe [file] [--fullscreen]` opens an image. `view.exe --quit` closes every View window,
//! for the installer. Each window is its own process, and nothing stays behind once it's closed.

#![cfg_attr(windows, windows_subsystem = "windows")]

// The app itself. It is kept in a file of its own so that on other systems, where none of it
// can build, this crate is just the few lines below and the workspace still builds whole.
#[cfg(windows)]
include!("windows.rs");

#[cfg(not(windows))]
fn main() {
    eprintln!("This is View for Windows. On Linux and macOS, build and run bero-view: cargo run --release -p view-desktop");
    std::process::exit(1);
}
