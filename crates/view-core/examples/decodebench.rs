//! Times View's own decoders on real files: `cargo run --release -p view-core --example decodebench -- <files>`.
//! Each file is decoded whole and then to fit a 2560×1440 screen, best of five.

use std::sync::atomic::AtomicBool;
use std::time::Instant;
use view_core::decode::{Limits, decode};
use view_core::formats;

fn main() {
    let cancel = AtomicBool::new(false);
    for arg in std::env::args().skip(1) {
        let Ok(data) = std::fs::read(&arg) else {
            println!("{arg}\tcan't be read");
            continue;
        };
        let Some(format) = formats::sniff(&data) else {
            println!("{arg}\tnot an image View knows");
            continue;
        };
        for (name, limits) in [("full", Limits::default()), ("screen", Limits { max_width: 2560, max_height: 1440, max_pixels: 2560 * 1440, ..Limits::default() })] {
            let mut best = f64::MAX;
            let mut size = (0, 0);
            let mut error = None;
            for _ in 0..5 {
                let t = Instant::now();
                match decode(&data, format, limits, &cancel) {
                    Ok(p) => size = (p.frames[0].bitmap.width, p.frames[0].bitmap.height),
                    Err(e) => error = Some(e),
                }
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            }
            match error {
                Some(e) => println!("{arg}\t{name}\tERROR {e}"),
                None => println!("{arg}\t{name}\t{}x{}\t{best:.1} ms", size.0, size.1),
            }
        }
    }
}
