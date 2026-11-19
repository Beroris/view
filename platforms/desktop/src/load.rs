//! Reading and decoding files, off the interface thread.

use image::{DynamicImage, ImageFormat, ImageReader};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};
use view_core::decode::{self, Limits};
use view_core::formats::{self, Format};
use view_core::metadata::{self, Fact};
use view_core::orient::{self, Orient};
use view_core::picture::{self, Bitmap, Details, Frame, Picture};

/// The formats this build opens. HEIC, AVIF, JPEG XL and camera RAW need codecs it doesn't carry.
pub const FORMATS: [Format; 7] = [Format::Jpeg, Format::Png, Format::Gif, Format::WebP, Format::Bmp, Format::Tiff, Format::Ico];

pub fn opens(extension: &str) -> bool {
    Format::from_extension(&extension.to_ascii_lowercase()).is_some_and(|f| FORMATS.contains(&f))
}

/// A decoded file, ready to draw: its pixels the right way up, with smaller copies for
/// drawing it zoomed out.
pub struct Loaded {
    pub picture: Picture,
    /// Half-size copies of the first frame, each half the one before. Stills only.
    pub mips: Vec<Bitmap>,
    pub facts: Vec<Fact>,
    pub file_size: u64,
}

impl Loaded {
    pub fn bytes(&self) -> usize {
        self.picture.bytes() + self.mips.iter().map(Bitmap::bytes).sum::<usize>()
    }

    /// The same picture turned or flipped, for the rotate and flip keys.
    pub fn turned(&self, o: Orient) -> Loaded {
        let p = &self.picture;
        let frames = p.frames.iter().map(|f| Frame { bitmap: Bitmap { width: f.bitmap.width, height: f.bitmap.height, data: f.bitmap.data.clone() }, delay_ms: f.delay_ms }).collect();
        let mut picture = Picture { width: p.width, height: p.height, frames, loops: p.loops, orient: Orient::IDENTITY, has_alpha: p.has_alpha, details: p.details.clone(), icc: None, streams: false };
        turn(&mut picture, o);
        let mips = mips_of(&picture);
        Loaded { picture, mips, facts: self.facts.clone(), file_size: self.file_size }
    }
}

pub fn load(path: &Path, cancel: &AtomicBool) -> Result<Loaded, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let by_name = || path.extension().and_then(|e| e.to_str()).and_then(|e| Format::from_extension(&e.to_ascii_lowercase()));
    let format = formats::sniff(&data).or_else(by_name).ok_or("This isn't a picture View can open.")?;
    let camera = metadata::read(path).unwrap_or_default();
    let mut picture = if decode::handles(format, &data, false) {
        decode::decode(&data, format, Limits::default(), cancel)?
    } else {
        let mut p = still(&data, format)?;
        p.orient = camera.orientation.map_or(Orient::IDENTITY, Orient::from_exif);
        p
    };
    // Turn the pixels once here, so drawing never has to think about orientation.
    let o = picture.orient;
    turn(&mut picture, o);
    let mips = mips_of(&picture);
    Ok(Loaded { picture, mips, facts: camera.facts, file_size: data.len() as u64 })
}

fn still(data: &[u8], format: Format) -> Result<Picture, String> {
    let kind = match format {
        Format::Jpeg => ImageFormat::Jpeg,
        Format::Png => ImageFormat::Png,
        Format::Gif => ImageFormat::Gif,
        Format::WebP => ImageFormat::WebP,
        Format::Bmp => ImageFormat::Bmp,
        Format::Tiff => ImageFormat::Tiff,
        Format::Ico => ImageFormat::Ico,
        other => return Err(format!("{} files aren't supported on this system yet.", other.name())),
    };
    let mut reader = ImageReader::with_format(Cursor::new(data), kind);
    reader.no_limits();
    let img = reader.decode().map_err(|e| e.to_string())?;
    let (width, height) = (img.width(), img.height());
    let colour = img.color();
    let has_alpha = colour.has_alpha();
    let channels = match (colour.has_color(), has_alpha) {
        (true, false) => "RGB",
        (true, true) => "RGBA",
        (false, false) => "Greyscale",
        (false, true) => "Greyscale with alpha",
    };
    let bits = colour.bits_per_pixel() as u32 / colour.channel_count().max(1) as u32;
    let data = match img {
        DynamicImage::ImageRgb8(b) => picture::rgb_to_bgra(b.as_raw()),
        other => {
            let mut v = other.into_rgba8().into_raw();
            picture::rgba_to_pbgra(&mut v);
            v
        }
    };
    Ok(Picture {
        width,
        height,
        frames: vec![Frame { bitmap: Bitmap { width, height, data }, delay_ms: 0 }],
        loops: 0,
        orient: Orient::IDENTITY,
        has_alpha,
        details: Details { format: Some(format), bits_per_channel: Some(bits), channels: Some(channels.into()), frame_count: 1, ..Details::default() },
        icc: None,
        streams: false,
    })
}

/// Turn every frame's pixels by `o`, so the picture needs no turning to show.
fn turn(p: &mut Picture, o: Orient) {
    p.orient = Orient::IDENTITY;
    if o == Orient::IDENTITY {
        return;
    }
    for f in &mut p.frames {
        let (width, height, data) = orient::apply(o, f.bitmap.width, f.bitmap.height, &f.bitmap.data);
        f.bitmap = Bitmap { width, height, data };
    }
    (p.width, p.height) = o.shown_size(p.width, p.height);
}

fn mips_of(p: &Picture) -> Vec<Bitmap> {
    let mut out: Vec<Bitmap> = Vec::new();
    let Some(first) = p.frames.first().filter(|_| p.frames.len() == 1) else { return out };
    let (mut w, mut h) = (first.bitmap.width, first.bitmap.height);
    loop {
        (w, h) = (w / 2, h / 2);
        if w < 32 || h < 32 {
            return out;
        }
        match picture::downscale(out.last().unwrap_or(&first.bitmap), w, h) {
            Some(m) => out.push(m),
            None => return out,
        }
    }
}

pub struct Done {
    pub path: PathBuf,
    pub result: Result<Arc<Loaded>, String>,
}

#[derive(Default)]
struct Queue {
    wanted: Vec<PathBuf>,
    running: Vec<PathBuf>,
}

/// A few threads that decode whatever is wanted most.
pub struct Pool {
    shared: Arc<(Mutex<Queue>, Condvar)>,
}

impl Pool {
    pub fn new(done: impl Fn(Done) + Send + Sync + 'static) -> Pool {
        let shared: Arc<(Mutex<Queue>, Condvar)> = Arc::default();
        let done = Arc::new(done);
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1)).clamp(1, 4);
        for i in 0..threads {
            let (shared, done) = (shared.clone(), done.clone());
            let work = move || {
                static NEVER: AtomicBool = AtomicBool::new(false);
                loop {
                    let path = {
                        let mut q = shared.0.lock().unwrap();
                        while q.wanted.is_empty() {
                            q = shared.1.wait(q).unwrap();
                        }
                        let path = q.wanted.remove(0);
                        q.running.push(path.clone());
                        path
                    };
                    // A decoder that panics on a broken file must not take the thread with it.
                    let result = std::panic::catch_unwind(|| load(&path, &NEVER)).unwrap_or_else(|_| Err("This file couldn't be decoded.".into()));
                    shared.0.lock().unwrap().running.retain(|p| p != &path);
                    done(Done { path, result: result.map(Arc::new) });
                }
            };
            let _ = std::thread::Builder::new().name(format!("decode {i}")).spawn(work);
        }
        Pool { shared }
    }

    /// Say what should be decoded, most urgent first. Replaces what was wanted before; a decode
    /// already under way carries on.
    pub fn want(&self, paths: Vec<PathBuf>) {
        let mut q = self.shared.0.lock().unwrap();
        q.wanted = paths.into_iter().filter(|p| !q.running.contains(p)).collect();
        self.shared.1.notify_all();
    }
}
