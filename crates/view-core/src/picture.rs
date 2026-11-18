//! Decoded images. Pixels are kept the way the GPU takes them, BGRA with premultiplied alpha,
//! so showing an image is a single upload with no conversion.

use crate::formats::Format;
use crate::orient::Orient;

/// Packed BGRA, premultiplied, four bytes a pixel, rows with no padding.
#[derive(Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Bitmap {
    pub fn bytes(&self) -> usize {
        self.data.len()
    }

    /// A buffer of `width` by `height` pixels, or `None` if that much memory can't be had.
    /// Huge images must fail politely instead of aborting the process.
    pub fn try_new(width: u32, height: u32) -> Option<Bitmap> {
        let len = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
        let mut data = Vec::new();
        data.try_reserve_exact(len).ok()?;
        data.resize(len, 0);
        Some(Bitmap { width, height, data })
    }

    /// Whether any pixel is less than opaque.
    pub fn has_alpha(&self) -> bool {
        self.data.chunks_exact(4).any(|p| p[3] != 255)
    }
}

/// From straight RGBA to premultiplied BGRA, in place.
pub fn rgba_to_pbgra(data: &mut [u8]) {
    for p in data.chunks_exact_mut(4) {
        let a = p[3] as u32;
        let (r, g, b) = (p[0] as u32, p[1] as u32, p[2] as u32);
        if a == 255 {
            p[0] = b as u8;
            p[2] = r as u8;
        } else {
            p[0] = mul_div_255(b, a);
            p[1] = mul_div_255(g, a);
            p[2] = mul_div_255(r, a);
        }
    }
}

/// From premultiplied BGRA back to straight RGBA, in place.
pub fn pbgra_to_rgba(data: &mut [u8]) {
    for p in data.chunks_exact_mut(4) {
        let a = p[3] as u32;
        let (b, g, r) = (p[0] as u32, p[1] as u32, p[2] as u32);
        if a == 255 || a == 0 {
            p[0] = r as u8;
            p[2] = b as u8;
        } else {
            p[0] = ((r * 255 + a / 2) / a).min(255) as u8;
            p[1] = ((g * 255 + a / 2) / a).min(255) as u8;
            p[2] = ((b * 255 + a / 2) / a).min(255) as u8;
        }
    }
}

fn mul_div_255(v: u32, a: u32) -> u8 {
    let t = v * a + 128;
    ((t + (t >> 8)) >> 8) as u8
}

/// Shrink by averaging every source pixel that falls in each target pixel. Exact for whole
/// factors and close to it otherwise; fine for the few cases the platform's scaler can't do.
pub fn downscale(src: &Bitmap, width: u32, height: u32) -> Option<Bitmap> {
    let (width, height) = (width.max(1), height.max(1));
    if width >= src.width && height >= src.height {
        return Some(Bitmap { width: src.width, height: src.height, data: src.data.clone() });
    }
    let mut out = Bitmap::try_new(width, height)?;
    let (sx, sy) = (src.width as f64 / width as f64, src.height as f64 / height as f64);
    let mut acc = vec![0u64; width as usize * 4];
    for ty in 0..height {
        let y0 = (ty as f64 * sy) as u32;
        let y1 = (((ty + 1) as f64 * sy).ceil() as u32).clamp(y0 + 1, src.height);
        acc.iter_mut().for_each(|a| *a = 0);
        let mut counts = vec![0u64; width as usize];
        for y in y0..y1 {
            let row = &src.data[(y * src.width * 4) as usize..((y + 1) * src.width * 4) as usize];
            for tx in 0..width {
                let x0 = (tx as f64 * sx) as u32;
                let x1 = (((tx + 1) as f64 * sx).ceil() as u32).clamp(x0 + 1, src.width);
                let a = &mut acc[tx as usize * 4..tx as usize * 4 + 4];
                for x in x0..x1 {
                    let p = &row[x as usize * 4..x as usize * 4 + 4];
                    a[0] += p[0] as u64;
                    a[1] += p[1] as u64;
                    a[2] += p[2] as u64;
                    a[3] += p[3] as u64;
                }
                counts[tx as usize] += (x1 - x0) as u64;
            }
        }
        let dst = &mut out.data[(ty * width * 4) as usize..((ty + 1) * width * 4) as usize];
        for tx in 0..width as usize {
            let n = counts[tx].max(1);
            for c in 0..4 {
                dst[tx * 4 + c] = ((acc[tx * 4 + c] + n / 2) / n) as u8;
            }
        }
    }
    Some(out)
}

/// One frame of an image. Stills have one; animations have one per step.
pub struct Frame {
    pub bitmap: Bitmap,
    /// How long it shows, in milliseconds. Zero for stills.
    pub delay_ms: u32,
}

/// What the decoder learnt about the image itself, for the information panel.
#[derive(Clone, Debug, Default)]
pub struct Details {
    /// What the decoder found, which may differ from the extension.
    pub format: Option<Format>,
    /// Bits per channel, as stored. `None` if the decoder didn't say.
    pub bits_per_channel: Option<u32>,
    /// Channel layout, such as "RGB", "RGBA", "Greyscale", "CMYK", "Indexed".
    pub channels: Option<String>,
    /// The embedded colour profile's name, or "sRGB" for a file that says it's sRGB.
    pub colour_space: Option<String>,
    /// Whether the colours were converted from an embedded profile for showing.
    pub colour_managed: bool,
    /// Frames in an animation, pages in a TIFF.
    pub frame_count: u32,
}

/// A decoded image, ready to show.
pub struct Picture {
    /// Size of the image as stored, before orientation, at full resolution.
    pub width: u32,
    pub height: u32,
    /// The frames, each at `bitmap` size: the full size, or smaller when decoded to fit the
    /// screen or to keep a huge image within memory.
    pub frames: Vec<Frame>,
    /// Plays this many times, 0 for ever. Stills ignore it.
    pub loops: u32,
    /// How the stored pixels are turned to show them, from EXIF.
    pub orient: Orient,
    pub has_alpha: bool,
    pub details: Details,
    /// An embedded colour profile the platform still has to apply. The platform codecs apply
    /// theirs while decoding; View's own decoders hand it over here.
    pub icc: Option<Vec<u8>>,
    /// The animation was too big to keep: only its first frame is here, and the rest play by
    /// decoding again as it goes (see [`crate::decode::stream`]).
    pub streams: bool,
}

impl Picture {
    pub fn bytes(&self) -> usize {
        self.frames.iter().map(|f| f.bitmap.bytes()).sum()
    }

    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// Whether the pixels are the image's full resolution.
    pub fn is_full(&self) -> bool {
        self.frames.first().is_some_and(|f| f.bitmap.width >= self.width && f.bitmap.height >= self.height)
    }

    /// Full-resolution pixels per bitmap pixel: 1 when full, more when decoded smaller.
    pub fn bitmap_scale(&self) -> f64 {
        match self.frames.first() {
            Some(f) if f.bitmap.width > 0 => self.width as f64 / f.bitmap.width as f64,
            _ => 1.0,
        }
    }

    /// Size as shown, after orientation, at full resolution.
    pub fn shown_size(&self) -> (u32, u32) {
        self.orient.shown_size(self.width, self.height)
    }
}

/// The largest size that fits `w`×`h` inside `max_w`×`max_h` and holds no more than
/// `max_pixels`, keeping the aspect ratio. Never larger than the image.
pub fn fit_within(w: u32, h: u32, max_w: u32, max_h: u32, max_pixels: u64) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w, h);
    }
    let mut s = 1.0f64;
    if max_w > 0 && max_h > 0 {
        s = s.min(max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    }
    let pixels = w as f64 * h as f64;
    if max_pixels > 0 && pixels * s * s > max_pixels as f64 {
        s = (max_pixels as f64 / pixels).sqrt();
    }
    if s >= 1.0 {
        return (w, h);
    }
    (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiply_round_trips_for_opaque_and_clear() {
        let mut d = vec![10, 20, 30, 255, 0, 0, 0, 0];
        rgba_to_pbgra(&mut d);
        assert_eq!(d, vec![30, 20, 10, 255, 0, 0, 0, 0]);
        pbgra_to_rgba(&mut d);
        assert_eq!(d, vec![10, 20, 30, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn premultiply_halves_at_half_alpha() {
        let mut d = vec![200, 100, 0, 128];
        rgba_to_pbgra(&mut d);
        assert_eq!(d, vec![0, 50, 100, 128]);
    }

    #[test]
    fn fit_keeps_aspect_and_limits() {
        assert_eq!(fit_within(6000, 4000, 2560, 1440, 0), (2160, 1440));
        assert_eq!(fit_within(100, 50, 2560, 1440, 0), (100, 50));
        assert_eq!(fit_within(40000, 40000, 0, 0, 100_000_000), (10000, 10000));
        assert_eq!(fit_within(0, 10, 100, 100, 0), (0, 10));
    }

    #[test]
    fn downscale_averages() {
        let src = Bitmap { width: 2, height: 2, data: vec![0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255] };
        let out = downscale(&src, 1, 1).unwrap();
        assert_eq!(out.data, vec![128, 128, 128, 255]);
    }

    #[test]
    fn absurd_sizes_fail_instead_of_aborting() {
        assert!(Bitmap::try_new(u32::MAX, u32::MAX).is_none());
    }
}
