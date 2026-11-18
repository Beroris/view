//! Changes to a photo for saving as a copy: straighten (turn by a small angle, zoomed just
//! enough that no empty corners show), crop, and brightness, contrast, saturation and warmth.
//! The same code draws the preview while editing and makes the saved copy, so what's saved is
//! what was seen. Pixels are premultiplied BGRA, as everywhere in View.

use crate::picture::Bitmap;

/// Colour changes, each from -1 to 1, 0 for none.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Adjust {
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub warmth: f32,
}

impl Adjust {
    pub fn is_none(&self) -> bool {
        *self == Adjust::default()
    }
}

/// A crop as shares of the (straightened) picture's width and height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crop {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Crop {
    pub const FULL: Crop = Crop { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };

    pub fn is_full(&self) -> bool {
        (self.x.abs() + self.y.abs() + (self.w - 1.0).abs() + (self.h - 1.0).abs()) < 1e-4
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edit {
    /// Degrees, clockwise.
    pub angle: f32,
    pub crop: Crop,
    pub adjust: Adjust,
}

impl Default for Edit {
    fn default() -> Edit {
        Edit { angle: 0.0, crop: Crop::FULL, adjust: Adjust::default() }
    }
}

impl Edit {
    pub fn is_none(&self) -> bool {
        self.angle.abs() < 1e-3 && self.crop.is_full() && self.adjust.is_none()
    }
}

/// The most a picture can be straightened by, either way.
pub const MAX_ANGLE: f32 = 45.0;

/// How much a `w`×`h` picture turned by `degrees` has to be enlarged so it still covers its
/// own frame, with no empty corners.
pub fn cover_scale(w: f32, h: f32, degrees: f32) -> f32 {
    let a = degrees.to_radians().abs();
    let (c, s) = (a.cos(), a.sin());
    ((w * c + h * s) / w).max((w * s + h * c) / h)
}

/// Work through `rows` rows of an image `pitch` bytes wide on every core, a band each.
fn bands(out: &mut [u8], pitch: usize, rows: usize, each: impl Fn(usize, &mut [u8]) + Sync) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 16);
    let per = rows.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (i, chunk) in out.chunks_mut(per * pitch).enumerate() {
            let each = &each;
            scope.spawn(move || {
                for (k, row) in chunk.chunks_mut(pitch).enumerate() {
                    each(i * per + k, row);
                }
            });
        }
    });
}

/// The straightened picture, cut to `crop`. The result is `crop` of the source's size.
pub fn straighten_and_crop(src: &Bitmap, degrees: f32, crop: Crop) -> Option<Bitmap> {
    let (sw, sh) = (src.width as f32, src.height as f32);
    let ow = ((crop.w * sw).round() as u32).clamp(1, src.width);
    let oh = ((crop.h * sh).round() as u32).clamp(1, src.height);
    let (x0, y0) = ((crop.x * sw).round(), (crop.y * sh).round());
    let mut out = Bitmap::try_new(ow, oh)?;
    if degrees.abs() < 1e-3 {
        // A plain crop: copy rows.
        let (x0, y0) = ((x0 as u32).min(src.width - ow) as usize, (y0 as u32).min(src.height - oh) as usize);
        let (sp, op) = (src.width as usize * 4, ow as usize * 4);
        for y in 0..oh as usize {
            let from = (y0 + y) * sp + x0 * 4;
            out.data[y * op..(y + 1) * op].copy_from_slice(&src.data[from..from + op]);
        }
        return Some(out);
    }
    let k = cover_scale(sw, sh, degrees);
    let a = degrees.to_radians();
    let (c, s) = (a.cos() / k, a.sin() / k);
    let (cx, cy) = (sw / 2.0, sh / 2.0);
    let (w, h) = (src.width as i32, src.height as i32);
    let data = &src.data;
    let pitch = ow as usize * 4;
    bands(&mut out.data, pitch, oh as usize, |y, row| {
        let py = y0 + y as f32 + 0.5 - cy;
        for x in 0..ow as usize {
            let px = x0 + x as f32 + 0.5 - cx;
            // Back through the turn, to where this pixel comes from.
            let sx = px * c + py * s + cx - 0.5;
            let sy = -px * s + py * c + cy - 0.5;
            let (fx, fy) = (sx.floor(), sy.floor());
            let (tx, ty) = (sx - fx, sy - fy);
            let (ix, iy) = (fx as i32, fy as i32);
            let at = |x: i32, y: i32| {
                let (x, y) = (x.clamp(0, w - 1) as usize, y.clamp(0, h - 1) as usize);
                (y * w as usize + x) * 4
            };
            let (p00, p10, p01, p11) = (at(ix, iy), at(ix + 1, iy), at(ix, iy + 1), at(ix + 1, iy + 1));
            for ch in 0..4 {
                let top = data[p00 + ch] as f32 * (1.0 - tx) + data[p10 + ch] as f32 * tx;
                let bottom = data[p01 + ch] as f32 * (1.0 - tx) + data[p11 + ch] as f32 * tx;
                row[x * 4 + ch] = (top * (1.0 - ty) + bottom * ty + 0.5) as u8;
            }
        }
    });
    Some(out)
}

/// One channel's curve: warmth's gain, then brightness, then contrast.
fn curve(a: &Adjust, gain: f32) -> [u8; 256] {
    let gamma = 2f32.powf(-a.brightness);
    let f = if a.contrast >= 0.0 { 1.0 + a.contrast * 1.5 } else { 1.0 + a.contrast * 0.85 };
    let mut lut = [0u8; 256];
    for (i, out) in lut.iter_mut().enumerate() {
        let mut v = (i as f32 / 255.0 * gain).min(1.0);
        v = v.powf(gamma);
        v = (v - 0.5) * f + 0.5;
        *out = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    }
    lut
}

/// Brightness, contrast, saturation and warmth, in place.
pub fn colour(b: &mut Bitmap, a: &Adjust) {
    if a.is_none() {
        return;
    }
    let (lb, lg, lr) = (curve(a, 1.0 - 0.12 * a.warmth), curve(a, 1.0 + 0.02 * a.warmth), curve(a, 1.0 + 0.12 * a.warmth));
    let sat = 1.0 + a.saturation;
    let pitch = b.width as usize * 4;
    let rows = b.height as usize;
    bands(&mut b.data, pitch, rows, |_, row| {
        for px in row.chunks_exact_mut(4) {
            let alpha = px[3];
            if alpha == 0 {
                continue;
            }
            let un = |v: u8| if alpha == 255 { v } else { ((v as u32 * 255 + alpha as u32 / 2) / alpha as u32).min(255) as u8 };
            let (mut bl, mut gr, mut rd) = (lb[un(px[0]) as usize] as f32, lg[un(px[1]) as usize] as f32, lr[un(px[2]) as usize] as f32);
            if sat != 1.0 {
                let l = 0.0722 * bl + 0.7152 * gr + 0.2126 * rd;
                bl = l + (bl - l) * sat;
                gr = l + (gr - l) * sat;
                rd = l + (rd - l) * sat;
            }
            let pre = |v: f32| (v.clamp(0.0, 255.0) * alpha as f32 / 255.0 + 0.5) as u8;
            px[0] = pre(bl);
            px[1] = pre(gr);
            px[2] = pre(rd);
        }
    });
}

/// The whole edit: straightened, cropped (if `with_crop`), and coloured.
pub fn render(src: &Bitmap, e: &Edit, with_crop: bool) -> Option<Bitmap> {
    let crop = if with_crop { e.crop } else { Crop::FULL };
    let mut out = straighten_and_crop(src, e.angle, crop)?;
    colour(&mut out, &e.adjust);
    Some(out)
}

/// Shapes a crop can be held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aspect {
    Free,
    /// The photo's own shape.
    Original,
    Square,
    /// 4:3, 3:2 and 16:9, turned to suit the photo: upright photos get 3:4 and so on.
    FourThree,
    ThreeTwo,
    SixteenNine,
}

impl Aspect {
    pub const ALL: [Aspect; 6] = [Aspect::Free, Aspect::Original, Aspect::Square, Aspect::FourThree, Aspect::ThreeTwo, Aspect::SixteenNine];

    pub fn label(self) -> &'static str {
        match self {
            Aspect::Free => "Free",
            Aspect::Original => "Original",
            Aspect::Square => "Square",
            Aspect::FourThree => "4:3",
            Aspect::ThreeTwo => "3:2",
            Aspect::SixteenNine => "16:9",
        }
    }

    /// Width over height for a photo `w`×`h`, or `None` for any shape.
    pub fn ratio(self, w: f32, h: f32) -> Option<f32> {
        let upright = h > w;
        let r = match self {
            Aspect::Free => return None,
            Aspect::Original => return Some(w / h),
            Aspect::Square => 1.0,
            Aspect::FourThree => 4.0 / 3.0,
            Aspect::ThreeTwo => 1.5,
            Aspect::SixteenNine => 16.0 / 9.0,
        };
        Some(if upright { 1.0 / r } else { r })
    }
}

/// The biggest crop of shape `ratio` (width over height, in pixels), in the middle.
pub fn centred(ratio: f32, w: f32, h: f32) -> Crop {
    if w / h > ratio {
        let cw = h * ratio / w;
        Crop { x: (1.0 - cw) / 2.0, y: 0.0, w: cw, h: 1.0 }
    } else {
        let ch = w / ratio / h;
        Crop { x: 0.0, y: (1.0 - ch) / 2.0, w: 1.0, h: ch }
    }
}

/// What part of the crop's frame is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

/// The crop after dragging `handle` by `dx`, `dy` (shares of the picture), held to `ratio` if
/// there is one, inside the picture, and never smaller than `min_px` pixels a side. `w` and `h`
/// are the picture's size in pixels.
pub fn drag(start: Crop, handle: Handle, dx: f32, dy: f32, ratio: Option<f32>, w: f32, h: f32, min_px: f32) -> Crop {
    let (mut l, mut t, mut r, mut b) = (start.x * w, start.y * h, (start.x + start.w) * w, (start.y + start.h) * h);
    let (dx, dy) = (dx * w, dy * h);
    let min = min_px.min(w).min(h);
    let back = |l: f32, t: f32, r: f32, b: f32| Crop { x: l / w, y: t / h, w: (r - l) / w, h: (b - t) / h };
    if handle == Handle::Move {
        let (cw, ch) = (r - l, b - t);
        l = (l + dx).clamp(0.0, w - cw);
        t = (t + dy).clamp(0.0, h - ch);
        return back(l, t, l + cw, t + ch);
    }
    let west = matches!(handle, Handle::W | Handle::NW | Handle::SW);
    let east = matches!(handle, Handle::E | Handle::NE | Handle::SE);
    let north = matches!(handle, Handle::N | Handle::NE | Handle::NW);
    let south = matches!(handle, Handle::S | Handle::SE | Handle::SW);
    if west {
        l = (l + dx).clamp(0.0, r - min);
    }
    if east {
        r = (r + dx).clamp(l + min, w);
    }
    if north {
        t = (t + dy).clamp(0.0, b - min);
    }
    if south {
        b = (b + dy).clamp(t + min, h);
    }
    let Some(k) = ratio else { return back(l, t, r, b) };
    let (cw, ch) = (r - l, b - t);
    let side = (north || south) && !(east || west);
    let across = (east || west) && !(north || south);
    if side || across {
        // One edge: the other side follows, about the middle, as far as the picture allows.
        let (mut nw, mut nh) = if side { (ch * k, ch) } else { (cw, cw / k) };
        if nw > w {
            nw = w;
            nh = w / k;
        }
        if nh > h {
            nh = h;
            nw = h * k;
        }
        if side {
            let mid = (l + r) / 2.0;
            let nl = (mid - nw / 2.0).clamp(0.0, w - nw);
            let (nt, nb) = if north { (b - nh, b) } else { (t, t + nh) };
            return back(nl, nt, nl + nw, nb);
        }
        let mid = (t + b) / 2.0;
        let nt = (mid - nh / 2.0).clamp(0.0, h - nh);
        let (nl, nr) = if west { (r - nw, r) } else { (l, l + nw) };
        return back(nl, nt, nr, nt + nh);
    }
    // A corner: grow to the bigger of the two, from the opposite corner, within the picture.
    let (ax, ay) = (if west { r } else { l }, if north { b } else { t });
    let room_w = if west { ax } else { w - ax };
    let room_h = if north { ay } else { h - ay };
    let mut nw = cw.max(ch * k).min(room_w).min(room_h * k);
    nw = nw.max(min.min(room_w).min(room_h * k));
    let nh = nw / k;
    let (nl, nr) = if west { (ax - nw, ax) } else { (ax, ax + nw) };
    let (nt, nb) = if north { (ay - nh, ay) } else { (ay, ay + nh) };
    back(nl, nt, nr, nb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grey(w: u32, h: u32, v: u8) -> Bitmap {
        let mut b = Bitmap::try_new(w, h).unwrap();
        for px in b.data.chunks_exact_mut(4) {
            px.copy_from_slice(&[v, v, v, 255]);
        }
        b
    }

    #[test]
    fn nothing_changes_nothing() {
        let mut b = grey(5, 3, 100);
        b.data[0] = 7;
        let out = render(&b, &Edit::default(), true).unwrap();
        assert_eq!(out.data, b.data);
        assert!(Edit::default().is_none());
        assert_eq!(cover_scale(400.0, 300.0, 0.0), 1.0);
    }

    #[test]
    fn crops_and_turns() {
        let mut b = grey(10, 10, 0);
        // A white pixel at (6, 2).
        let i = (2 * 10 + 6) * 4;
        b.data[i..i + 3].copy_from_slice(&[255, 255, 255]);
        let c = straighten_and_crop(&b, 0.0, Crop { x: 0.5, y: 0.2, w: 0.3, h: 0.3 }).unwrap();
        assert_eq!((c.width, c.height), (3, 3));
        assert_eq!(c.data[4], 255); // (1, 0) in the crop
        // Turned a little, the picture grows to cover its frame, and keeps its size.
        let t = straighten_and_crop(&grey(40, 30, 200), 10.0, Crop::FULL).unwrap();
        assert_eq!((t.width, t.height), (40, 30));
        assert!(t.data.chunks_exact(4).all(|p| p[0] == 200 && p[3] == 255), "no empty corners");
        assert!(cover_scale(400.0, 300.0, 10.0) > 1.2);
        assert_eq!(cover_scale(400.0, 300.0, -10.0), cover_scale(400.0, 300.0, 10.0));
    }

    #[test]
    fn colours_move_the_right_way() {
        let mid = grey(2, 2, 128);
        let mut b = mid.clone();
        colour(&mut b, &Adjust { brightness: 0.5, ..Default::default() });
        assert!(b.data[0] > 150);
        let mut b = mid.clone();
        colour(&mut b, &Adjust { contrast: 1.0, ..Default::default() });
        assert!((b.data[0] as i32 - 128).abs() <= 2, "mid grey stays put");
        let mut warm = mid.clone();
        colour(&mut warm, &Adjust { warmth: 1.0, ..Default::default() });
        assert!(warm.data[2] > warm.data[0], "redder than blue");
        let mut c = Bitmap::try_new(1, 1).unwrap();
        c.data.copy_from_slice(&[20, 60, 200, 255]);
        colour(&mut c, &Adjust { saturation: -1.0, ..Default::default() });
        assert!(c.data[0] == c.data[1] && c.data[1] == c.data[2], "grey: {:?}", c.data);
        // Half see-through stays half see-through.
        let mut h = Bitmap::try_new(1, 1).unwrap();
        h.data.copy_from_slice(&[64, 64, 64, 128]);
        colour(&mut h, &Adjust { brightness: 0.3, ..Default::default() });
        assert_eq!(h.data[3], 128);
        assert!(h.data[0] <= 128);
    }

    #[test]
    fn crop_frames_drag_and_keep_their_shape() {
        let (w, h) = (400.0, 300.0);
        // Moving stops at the edges.
        let c = drag(Crop { x: 0.5, y: 0.5, w: 0.25, h: 0.25 }, Handle::Move, 0.5, -0.9, None, w, h, 10.0);
        assert!((c.x - 0.75).abs() < 1e-5 && c.y.abs() < 1e-5);
        // A free corner follows the mouse.
        let c = drag(Crop::FULL, Handle::SE, -0.25, -0.5, None, w, h, 10.0);
        assert!((c.w - 0.75).abs() < 1e-5 && (c.h - 0.5).abs() < 1e-5);
        // Held square, a corner keeps it square and inside.
        let sq = Aspect::Square.ratio(w, h).unwrap();
        let start = centred(sq, w, h);
        assert!((start.w * w - start.h * h).abs() < 0.01);
        let c = drag(start, Handle::NW, 0.1, 0.0, Some(sq), w, h, 10.0);
        assert!((c.w * w - c.h * h).abs() < 0.01, "{c:?}");
        assert!(c.x >= 0.0 && c.y >= -1e-6 && c.x + c.w <= 1.0 + 1e-6 && c.y + c.h <= 1.0 + 1e-6);
        // An edge pulls the other way about the middle.
        let c = drag(centred(1.5, w, h), Handle::E, -0.2, 0.0, Some(1.5), w, h, 10.0);
        assert!(((c.w * w) / (c.h * h) - 1.5).abs() < 0.01);
        // Never smaller than the least size.
        let c = drag(Crop::FULL, Handle::W, 2.0, 0.0, None, w, h, 10.0);
        assert!((c.w * w - 10.0).abs() < 0.01);
        assert_eq!(Aspect::FourThree.ratio(300.0, 400.0), Some(0.75));
    }
}
