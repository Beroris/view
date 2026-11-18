//! The decoders built into View, all pure Rust: GIF, WebP and PNG (with APNG). Everything else
//! goes through the platform's codecs, which are faster at JPEG and know the system's extras.
//!
//! These cover what platform codecs do badly or not at all: animation (GIF, animated WebP,
//! APNG), and WebP on a system without a WebP codec.

use crate::formats::Format;
use crate::orient::Orient;
use crate::picture::{Bitmap, Details, Frame, Picture, downscale, fit_within, rgba_to_pbgra};
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, ColorType, ExtendedColorType, ImageDecoder};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};

/// Limits for one decode.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Fit a still inside this many pixels across and down, 0 for no limit.
    pub max_width: u32,
    pub max_height: u32,
    /// The most pixels a still may keep, whatever its size.
    pub max_pixels: u64,
    /// The most bytes all of an animation's frames may take before it plays by streaming.
    pub animation_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_width: 0, max_height: 0, max_pixels: 256_000_000, animation_bytes: 256 << 20 }
    }
}

/// Whether View decodes this file itself, rather than the platform's codecs. `head` is at least
/// the first few kilobytes; the whole file is better for APNG, whose marker can come late.
pub fn handles(format: Format, data: &[u8], platform_has_webp: bool) -> bool {
    match format {
        Format::Gif => true,
        Format::WebP => !platform_has_webp || crate::formats::is_animated_webp(data),
        Format::Png => crate::formats::is_apng(data),
        _ => false,
    }
}

pub type Result<T> = std::result::Result<T, String>;

/// Decode a whole file. Animations come back with every frame unless they'd take more than
/// `limits.animation_bytes`; then only the first frame is kept and `Picture::streams` is set, and
/// the caller plays it with [`stream`].
pub fn decode(data: &[u8], format: Format, limits: Limits, cancel: &AtomicBool) -> Result<Picture> {
    match format {
        Format::Gif => {
            let d = GifDecoder::new(Cursor::new(data)).map_err(err)?;
            let (w, h) = d.dimensions();
            let mut p = frames(d.into_frames(), w, h, limits, cancel)?;
            p.loops = gif_loops(data);
            p.details.format = Some(Format::Gif);
            p.details.bits_per_channel = Some(8);
            p.details.channels = Some("Indexed".into());
            Ok(p)
        }
        Format::WebP => {
            let mut d = WebPDecoder::new(Cursor::new(data)).map_err(err)?;
            let icc = d.icc_profile().ok().flatten();
            let orient = orient_of(&mut d);
            let (w, h) = d.dimensions();
            let alpha = d.color_type().has_alpha();
            let mut p = if d.has_animation() {
                let mut p = frames(d.into_frames(), w, h, limits, cancel)?;
                p.loops = webp_loops(data);
                p
            } else {
                still(d, limits, cancel)?
            };
            p.orient = orient;
            p.details.format = Some(Format::WebP);
            p.details.bits_per_channel = Some(8);
            p.details.channels = Some(if alpha { "RGBA" } else { "RGB" }.into());
            describe_icc(&mut p, icc);
            Ok(p)
        }
        Format::Png => {
            let mut d = PngDecoder::new(Cursor::new(data)).map_err(err)?;
            let icc = d.icc_profile().ok().flatten();
            let orient = orient_of(&mut d);
            let (w, h) = d.dimensions();
            let (bits, channels) = describe_colour(d.original_color_type());
            let mut p = if d.is_apng().unwrap_or(false) {
                let mut p = frames(d.apng().map_err(err)?.into_frames(), w, h, limits, cancel)?;
                p.loops = apng_loops(data);
                p
            } else {
                still(d, limits, cancel)?
            };
            p.orient = orient;
            p.details.format = Some(Format::Png);
            p.details.bits_per_channel = bits;
            p.details.channels = channels;
            describe_icc(&mut p, icc);
            Ok(p)
        }
        other => Err(format!("{} isn't decoded by View itself", other.name())),
    }
}

/// Play an animation that's too big to keep in memory: decode its frames one by one, again and
/// again, handing each to `sink` until it returns `false`. Each frame is the full canvas.
pub fn stream(data: &[u8], format: Format, mut sink: impl FnMut(Frame) -> bool) -> Result<()> {
    loop {
        let it = match format {
            Format::Gif => GifDecoder::new(Cursor::new(data)).map_err(err)?.into_frames(),
            Format::WebP => WebPDecoder::new(Cursor::new(data)).map_err(err)?.into_frames(),
            Format::Png => PngDecoder::new(Cursor::new(data)).map_err(err)?.apng().map_err(err)?.into_frames(),
            _ => return Err("not an animation".into()),
        };
        let mut any = false;
        for frame in it {
            let frame = frame.map_err(err)?;
            any = true;
            if !sink(to_frame(frame)) {
                return Ok(());
            }
        }
        if !any {
            return Err("the animation has no frames".into());
        }
    }
}

/// A PNG's pixels, premultiplied BGRA: for pages a platform renders to PNG, such as PDFs.
pub fn png_bitmap(data: &[u8]) -> Result<Bitmap> {
    let d = PngDecoder::new(Cursor::new(data)).map_err(err)?;
    let (w, h) = d.dimensions();
    let color = d.color_type();
    let mut buf = vec![0u8; d.total_bytes() as usize];
    d.read_image(&mut buf).map_err(err)?;
    let mut data = to_rgba8(buf, color, w, h)?;
    rgba_to_pbgra(&mut data);
    Ok(Bitmap { width: w, height: h, data })
}

fn err(e: image::ImageError) -> String {
    match e {
        image::ImageError::Limits(_) => "The image is too large to open.".into(),
        image::ImageError::Unsupported(u) => format!("This kind of image isn't supported: {u}"),
        other => format!("The file is damaged or incomplete: {other}"),
    }
}

fn orient_of(d: &mut impl ImageDecoder) -> Orient {
    d.orientation().map(|o| Orient::from_exif(o.to_exif() as u32)).unwrap_or_default()
}

fn describe_colour(c: ExtendedColorType) -> (Option<u32>, Option<String>) {
    use ExtendedColorType as E;
    let (bits, ch) = match c {
        E::L1 | E::La1 => (1, "Greyscale"),
        E::L2 | E::La2 => (2, "Greyscale"),
        E::L4 | E::La4 => (4, "Greyscale"),
        E::L8 => (8, "Greyscale"),
        E::La8 => (8, "Greyscale with alpha"),
        E::L16 => (16, "Greyscale"),
        E::La16 => (16, "Greyscale with alpha"),
        E::Rgb1 | E::Rgb2 | E::Rgb4 => (4, "Indexed"),
        E::Rgba1 | E::Rgba2 | E::Rgba4 => (4, "Indexed"),
        E::Rgb8 => (8, "RGB"),
        E::Rgba8 => (8, "RGBA"),
        E::Rgb16 => (16, "RGB"),
        E::Rgba16 => (16, "RGBA"),
        _ => return (None, None),
    };
    (Some(bits), Some(ch.to_string()))
}

fn describe_icc(p: &mut Picture, icc: Option<Vec<u8>>) {
    if let Some(icc) = icc {
        p.details.colour_space = crate::icc::description(&icc).or_else(|| Some("Embedded profile".into()));
        p.icc = Some(icc);
    }
}

/// Decode a still to RGBA, then premultiply and shrink it as the limits ask.
fn still(d: impl ImageDecoder, limits: Limits, cancel: &AtomicBool) -> Result<Picture> {
    let (w, h) = d.dimensions();
    let total = d.total_bytes();
    // These decoders can't decode smaller, so the whole image has to fit first.
    if (w as u64) * (h as u64) > limits.max_pixels.saturating_mul(4).max(1) {
        return Err(format!("The image is too large to open ({w} × {h})."));
    }
    let color = d.color_type();
    let mut buf = Vec::new();
    buf.try_reserve_exact(total as usize).map_err(|_| "Not enough memory to open this image.".to_string())?;
    buf.resize(total as usize, 0);
    d.read_image(&mut buf).map_err(err)?;
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let mut rgba = to_rgba8(buf, color, w, h)?;
    rgba_to_pbgra(&mut rgba);
    let mut bitmap = Bitmap { width: w, height: h, data: rgba };
    let has_alpha = color.has_alpha() && bitmap.has_alpha();
    let (tw, th) = fit_within(w, h, limits.max_width, limits.max_height, limits.max_pixels);
    if (tw, th) != (w, h) {
        bitmap = downscale(&bitmap, tw, th).ok_or("Not enough memory to open this image.")?;
    }
    Ok(Picture {
        width: w,
        height: h,
        frames: vec![Frame { bitmap, delay_ms: 0 }],
        loops: 0,
        orient: Orient::IDENTITY,
        has_alpha,
        details: Details { frame_count: 1, ..Details::default() },
        icc: None,
        streams: false,
    })
}

fn to_rgba8(buf: Vec<u8>, color: ColorType, w: u32, h: u32) -> Result<Vec<u8>> {
    let n = w as usize * h as usize;
    let out = match color {
        ColorType::Rgba8 => buf,
        ColorType::Rgb8 => {
            let mut out = Vec::with_capacity(n * 4);
            for p in buf.chunks_exact(3) {
                out.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
            out
        }
        ColorType::L8 => buf.iter().flat_map(|&l| [l, l, l, 255]).collect(),
        ColorType::La8 => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        ColorType::Rgba16 | ColorType::Rgb16 | ColorType::L16 | ColorType::La16 => {
            // 16 bits a channel, native endian. Keep the top byte, rounded.
            let ch = color.channel_count() as usize;
            let v: Vec<u8> = buf
                .chunks_exact(2)
                .map(|b| {
                    let x = u16::from_ne_bytes([b[0], b[1]]) as u32;
                    ((x * 255 + 32767) / 65535) as u8
                })
                .collect();
            match ch {
                4 => v,
                3 => v.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
                2 => v.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
                _ => v.iter().flat_map(|&l| [l, l, l, 255]).collect(),
            }
        }
        other => return Err(format!("Unusual pixel layout {other:?} isn't supported.")),
    };
    Ok(out)
}

fn to_frame(f: image::Frame) -> Frame {
    let (num, den) = f.delay().numer_denom_ms();
    let delay = if den == 0 { 0 } else { num / den };
    let buf = f.into_buffer();
    let (w, h) = buf.dimensions();
    let mut data = buf.into_raw();
    rgba_to_pbgra(&mut data);
    Frame { bitmap: Bitmap { width: w, height: h, data }, delay_ms: animation_delay(delay) }
}

/// Browsers show frames that ask for under 20 ms at 100 ms, because such files were made for
/// old viewers that ignored small delays. Doing the same plays them at the intended speed.
pub fn animation_delay(ms: u32) -> u32 {
    if ms < 20 { 100 } else { ms }
}

fn frames(it: image::Frames<'_>, w: u32, h: u32, limits: Limits, cancel: &AtomicBool) -> Result<Picture> {
    let mut frames: Vec<Frame> = Vec::new();
    let mut bytes = 0usize;
    let mut streams = false;
    let mut count = 0u32;
    for frame in it {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let frame = match frame {
            Ok(f) => f,
            // A broken frame late in a file: show what came before it, like browsers do.
            Err(e) if !frames.is_empty() => {
                let _ = e;
                break;
            }
            Err(e) => return Err(err(e)),
        };
        count += 1;
        if streams {
            continue; // only counting
        }
        let frame = to_frame(frame);
        bytes += frame.bitmap.bytes();
        if bytes > limits.animation_bytes && !frames.is_empty() {
            frames.truncate(1);
            streams = true;
            continue;
        }
        frames.push(frame);
    }
    if frames.is_empty() {
        return Err("The image has no frames.".into());
    }
    let has_alpha = frames.iter().any(|f| f.bitmap.has_alpha());
    Ok(Picture {
        width: w,
        height: h,
        frames,
        loops: 0,
        orient: Orient::IDENTITY,
        has_alpha,
        details: Details { frame_count: count, ..Details::default() },
        icc: None,
        streams,
    })
}

/// A GIF's loop count, from its NETSCAPE2.0 block: 0 is for ever, and no block means once.
pub fn gif_loops(data: &[u8]) -> u32 {
    let sig = b"NETSCAPE2.0";
    match data.windows(sig.len()).position(|w| w == sig) {
        Some(at) if data.len() >= at + 15 && data[at + 11] == 3 && data[at + 12] == 1 => {
            u16::from_le_bytes([data[at + 13], data[at + 14]]) as u32
        }
        Some(_) => 0,
        None => 1,
    }
}

/// An animated WebP's loop count, from its ANIM chunk: 0 is for ever.
pub fn webp_loops(data: &[u8]) -> u32 {
    let mut at = 12;
    while at + 8 <= data.len() {
        let kind = &data[at..at + 4];
        let len = u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]) as usize;
        if kind == b"ANIM" && at + 14 <= data.len() {
            return u16::from_le_bytes([data[at + 12], data[at + 13]]) as u32;
        }
        at = at.saturating_add(8).saturating_add(len + (len & 1));
    }
    0
}

/// An APNG's play count, from its acTL chunk: 0 is for ever.
pub fn apng_loops(data: &[u8]) -> u32 {
    let mut at = 8;
    while at + 8 <= data.len() {
        let len = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        if &data[at + 4..at + 8] == b"acTL" && at + 16 <= data.len() {
            return u32::from_be_bytes([data[at + 12], data[at + 13], data[at + 14], data[at + 15]]);
        }
        at = at.saturating_add(12).saturating_add(len);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Rgba, RgbaImage};

    fn gif(frames: usize, repeat: Option<Repeat>) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            if let Some(r) = repeat {
                enc.set_repeat(r).unwrap();
            }
            for i in 0..frames {
                let img = RgbaImage::from_pixel(4, 3, Rgba([i as u8 * 40, 0, 0, 255]));
                enc.encode_frame(image::Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(50, 1))).unwrap();
            }
        }
        out
    }

    #[test]
    fn decodes_animated_gif_with_delays_and_loops() {
        let data = gif(3, Some(Repeat::Infinite));
        assert!(crate::formats::gif_has_many_frames(&data));
        let p = decode(&data, Format::Gif, Limits::default(), &AtomicBool::new(false)).unwrap();
        assert_eq!((p.width, p.height), (4, 3));
        assert_eq!(p.frames.len(), 3);
        assert_eq!(p.frames[1].delay_ms, 50);
        assert_eq!(p.loops, 0);
        assert!(!p.has_alpha);
        // BGRA: red ends up in the third byte.
        assert_eq!(&p.frames[1].bitmap.data[0..4], &[0, 0, 40, 255]);
    }

    #[test]
    fn single_frame_gif_is_a_still() {
        let data = gif(1, None);
        assert!(!crate::formats::gif_has_many_frames(&data));
        let p = decode(&data, Format::Gif, Limits::default(), &AtomicBool::new(false)).unwrap();
        assert_eq!(p.frames.len(), 1);
        assert_eq!(gif_loops(&data), 1);
    }

    #[test]
    fn big_animation_streams() {
        let data = gif(4, Some(Repeat::Infinite));
        let limits = Limits { animation_bytes: 4 * 3 * 4 + 1, ..Limits::default() };
        let p = decode(&data, Format::Gif, limits, &AtomicBool::new(false)).unwrap();
        assert!(p.streams);
        assert_eq!(p.frames.len(), 1);
        assert_eq!(p.details.frame_count, 4);
        let mut seen = 0;
        stream(&data, Format::Gif, |_| {
            seen += 1;
            seen < 10
        })
        .unwrap();
        assert_eq!(seen, 10, "streaming loops round");
    }

    #[test]
    fn png_still_shrinks_to_limits() {
        let img = RgbaImage::from_pixel(64, 32, Rgba([10, 20, 30, 128]));
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let limits = Limits { max_width: 16, max_height: 16, ..Limits::default() };
        let p = decode(&png, Format::Png, limits, &AtomicBool::new(false)).unwrap();
        assert_eq!((p.width, p.height), (64, 32));
        assert_eq!((p.frames[0].bitmap.width, p.frames[0].bitmap.height), (16, 8));
        assert!(p.has_alpha);
        assert!(!p.is_full());
        assert_eq!(p.bitmap_scale(), 4.0);
    }

    #[test]
    fn garbage_fails_politely() {
        let r = decode(b"GIF89a garbage", Format::Gif, Limits::default(), &AtomicBool::new(false));
        assert!(r.is_err());
        let r = decode(b"\x89PNG\r\n\x1a\nxx", Format::Png, Limits::default(), &AtomicBool::new(false));
        assert!(r.is_err());
    }

    #[test]
    fn tiny_delays_play_at_browser_speed() {
        assert_eq!(animation_delay(0), 100);
        assert_eq!(animation_delay(10), 100);
        assert_eq!(animation_delay(20), 20);
    }
}
