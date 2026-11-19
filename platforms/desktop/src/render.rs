//! Drawing on the CPU: the picture, scaled and smoothed across every core, and the few
//! rounded shapes the interface is made of. Pixels are `0x00RRGGBB`, a row after a row.

use view_core::picture::Bitmap;

/// The window's pixels for one frame.
pub struct Frame<'a> {
    pub px: &'a mut [u32],
    pub w: usize,
    pub h: usize,
}

/// What shows behind and around the picture.
#[derive(Clone, Copy)]
pub struct Ground {
    /// Around the picture.
    pub backdrop: u32,
    /// Through a picture's transparent parts: two colours, in squares of `cell` pixels. The
    /// same colour twice makes it plain.
    pub under: [u32; 2],
    pub cell: usize,
}

/// One pixel of a premultiplied BGRA bitmap, as `0xAARRGGBB`.
#[inline(always)]
fn at(data: &[u8], i: usize) -> u32 {
    let b = &data[i * 4..i * 4 + 4];
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// `a` towards `b` by `t` 256ths, all four channels at once.
#[inline(always)]
fn mix(a: u32, b: u32, t: u32) -> u32 {
    let rb = (((a & 0x00FF_00FF) * (256 - t) + (b & 0x00FF_00FF) * t) >> 8) & 0x00FF_00FF;
    let ag = (((a >> 8) & 0x00FF_00FF) * (256 - t) + ((b >> 8) & 0x00FF_00FF) * t) & 0xFF00_FF00;
    rb | ag
}

/// A premultiplied pixel over an opaque one.
#[inline(always)]
fn over(p: u32, under: u32) -> u32 {
    let a = p >> 24;
    if a == 255 {
        return p & 0x00FF_FFFF;
    }
    let rest = 255 - a;
    let rest = rest + (rest >> 7);
    let rb = (((under & 0x00FF_00FF) * rest) >> 8) & 0x00FF_00FF;
    let g = (((under & 0x0000_FF00) * rest) >> 8) & 0x0000_FF00;
    ((p & 0x00FF_FFFF) + (rb | g)) & 0x00FF_FFFF
}

/// Blend a plain colour onto a pixel, `alpha` from 0 to 1.
#[inline(always)]
pub fn blend(dst: &mut u32, rgb: u32, alpha: f32) {
    let t = (alpha.clamp(0.0, 1.0) * 256.0) as u32;
    if t > 0 {
        *dst = mix(*dst & 0x00FF_FFFF, rgb & 0x00FF_FFFF, t) & 0x00FF_FFFF;
    }
}

/// How many threads share a frame of this many pixels. Small frames aren't worth the hand-off.
fn threads_for(pixels: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()).min(16);
    cores.min(pixels / 120_000).max(1)
}

/// Fill the frame with the backdrop and draw `src` over the rectangle `(x, y, w, h)`, which
/// may be partly or wholly outside the frame. `smooth` blends neighbouring pixels; otherwise
/// each shows as a square. `alpha` says the bitmap has transparent parts worth blending.
pub fn picture(frame: &mut Frame, src: &Bitmap, rect: (f64, f64, f64, f64), smooth: bool, alpha: bool, ground: Ground) {
    let (fw, fh) = (frame.w, frame.h);
    let (x, y, w, h) = rect;
    if fw == 0 || fh == 0 {
        return;
    }
    if src.width == 0 || src.height == 0 || w < 0.5 || h < 0.5 || src.data.len() < src.width as usize * src.height as usize * 4 {
        frame.px.fill(ground.backdrop);
        return;
    }
    let (sw, sh) = (src.width as usize, src.height as usize);
    let x0 = (x.round().max(0.0) as usize).min(fw);
    let x1 = ((x + w).round().max(0.0) as usize).clamp(x0, fw);
    let y0 = (y.round().max(0.0) as usize).min(fh);
    let y1 = ((y + h).round().max(0.0) as usize).clamp(y0, fh);
    // For each column of the frame: the two source columns it lies between, and how far along.
    let step_x = sw as f64 / w;
    let step_y = sh as f64 / h;
    let column = |dx: usize| -> (u32, u32, u32) {
        let s = (dx as f64 + 0.5 - x) * step_x;
        if smooth {
            let s = (s - 0.5).clamp(0.0, (sw - 1) as f64);
            let i = s as usize;
            (i as u32, (i + 1).min(sw - 1) as u32, ((s - i as f64) * 256.0) as u32)
        } else {
            let i = (s.max(0.0) as usize).min(sw - 1) as u32;
            (i, i, 0)
        }
    };
    let columns: Vec<(u32, u32, u32)> = (x0..x1).map(column).collect();
    let data = &src.data[..];
    let draw_rows = |rows: &mut [u32], first: usize| {
        for (n, row) in rows.chunks_exact_mut(fw).enumerate() {
            let dy = first + n;
            if dy < y0 || dy >= y1 {
                row.fill(ground.backdrop);
                continue;
            }
            row[..x0].fill(ground.backdrop);
            row[x1..].fill(ground.backdrop);
            let s = (dy as f64 + 0.5 - y) * step_y;
            let (r0, r1, ty) = if smooth {
                let s = (s - 0.5).clamp(0.0, (sh - 1) as f64);
                let i = s as usize;
                (i, (i + 1).min(sh - 1), ((s - i as f64) * 256.0) as u32)
            } else {
                let i = (s.max(0.0) as usize).min(sh - 1);
                (i, i, 0)
            };
            let top = &data[r0 * sw * 4..(r0 + 1) * sw * 4];
            let bottom = &data[r1 * sw * 4..(r1 + 1) * sw * 4];
            let out = &mut row[x0..x1];
            let cell_row = dy / ground.cell.max(1);
            for (i, (o, &(c0, c1, tx))) in out.iter_mut().zip(&columns).enumerate() {
                let p = if smooth {
                    let a = mix(at(top, c0 as usize), at(top, c1 as usize), tx);
                    let b = mix(at(bottom, c0 as usize), at(bottom, c1 as usize), tx);
                    mix(a, b, ty)
                } else {
                    at(top, c0 as usize)
                };
                *o = if alpha { over(p, ground.under[(cell_row + (x0 + i) / ground.cell.max(1)) & 1]) } else { p & 0x00FF_FFFF };
            }
        }
    };
    let threads = threads_for(fw * fh);
    if threads == 1 {
        draw_rows(frame.px, 0);
        return;
    }
    let rows_each = fh.div_ceil(threads);
    std::thread::scope(|scope| {
        for (i, part) in frame.px.chunks_mut(rows_each * fw).enumerate() {
            let draw_rows = &draw_rows;
            scope.spawn(move || draw_rows(part, i * rows_each));
        }
    });
}

/// A rounded rectangle with soft edges, blended over what's there.
pub fn rounded(frame: &mut Frame, x: f32, y: f32, w: f32, h: f32, radius: f32, rgb: u32, alpha: f32) {
    let radius = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (hw, hh) = (w / 2.0 - radius, h / 2.0 - radius);
    let x0 = x.floor().max(0.0) as usize;
    let y0 = y.floor().max(0.0) as usize;
    let x1 = ((x + w).ceil().max(0.0) as usize).min(frame.w);
    let y1 = ((y + h).ceil().max(0.0) as usize).min(frame.h);
    for py in y0..y1 {
        let dy = ((py as f32 + 0.5 - cy).abs() - hh).max(0.0);
        let row = &mut frame.px[py * frame.w..(py + 1) * frame.w];
        for (px, dst) in row.iter_mut().enumerate().take(x1).skip(x0) {
            let dx = ((px as f32 + 0.5 - cx).abs() - hw).max(0.0);
            let cover = if dx == 0.0 || dy == 0.0 { (radius - dx.max(dy) + 0.5).clamp(0.0, 1.0) } else { (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0) };
            blend(dst, rgb, cover * alpha);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(w: u32, h: u32, pixel: [u8; 4]) -> Bitmap {
        Bitmap { width: w, height: h, data: pixel.repeat((w * h) as usize) }
    }

    const PLAIN: Ground = Ground { backdrop: 0x101010, under: [0xFFFFFF, 0xFFFFFF], cell: 8 };

    #[test]
    fn an_opaque_picture_fills_its_rectangle_and_nothing_else() {
        let src = bitmap(4, 4, [10, 20, 30, 255]);
        for smooth in [true, false] {
            let mut px = vec![0u32; 20 * 10];
            picture(&mut Frame { px: &mut px, w: 20, h: 10 }, &src, (5.0, 2.0, 8.0, 6.0), smooth, false, PLAIN);
            for y in 0..10 {
                for x in 0..20 {
                    let inside = (5..13).contains(&x) && (2..8).contains(&y);
                    assert_eq!(px[y * 20 + x], if inside { 0x1E140A } else { 0x101010 }, "at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn a_picture_off_the_edges_is_clipped() {
        let src = bitmap(3, 3, [0, 0, 255, 255]);
        let mut px = vec![0u32; 6 * 6];
        picture(&mut Frame { px: &mut px, w: 6, h: 6 }, &src, (-10.0, -10.0, 40.0, 40.0), true, false, PLAIN);
        assert!(px.iter().all(|&p| p == 0xFF0000));
        picture(&mut Frame { px: &mut px, w: 6, h: 6 }, &src, (50.0, 50.0, 4.0, 4.0), true, false, PLAIN);
        assert!(px.iter().all(|&p| p == 0x101010));
    }

    #[test]
    fn transparent_pixels_show_what_is_under_them() {
        let clear = bitmap(2, 2, [0, 0, 0, 0]);
        let mut px = vec![0u32; 4 * 4];
        picture(&mut Frame { px: &mut px, w: 4, h: 4 }, &clear, (0.0, 0.0, 4.0, 4.0), true, true, PLAIN);
        assert!(px.iter().all(|&p| p == 0xFFFFFF));
        // Half-transparent black over white is mid grey.
        let half = bitmap(2, 2, [0, 0, 0, 128]);
        picture(&mut Frame { px: &mut px, w: 4, h: 4 }, &half, (0.0, 0.0, 4.0, 4.0), true, true, PLAIN);
        assert!(px.iter().all(|&p| (p & 0xFF).abs_diff(0x7F) <= 2 && p >> 24 == 0), "{:x}", px[0]);
    }

    #[test]
    fn big_frames_drawn_on_many_threads_match_one_thread() {
        let mut src = bitmap(64, 64, [0, 0, 0, 255]);
        for (i, b) in src.data.iter_mut().enumerate() {
            if i % 4 != 3 {
                *b = (i * 7 % 251) as u8;
            }
        }
        let (w, h) = (900, 700);
        let mut many = vec![0u32; w * h];
        picture(&mut Frame { px: &mut many, w, h }, &src, (-20.5, 13.25, 1000.0, 600.0), true, false, PLAIN);
        // One row at a time is always one thread.
        let mut one = vec![0u32; w * h];
        for y in 0..h {
            let mut row = vec![0u32; w];
            picture(&mut Frame { px: &mut row, w, h: 1 }, &src, (-20.5, 13.25 - y as f64, 1000.0, 600.0), true, false, PLAIN);
            one[y * w..(y + 1) * w].copy_from_slice(&row);
        }
        assert!(many == one);
    }

    #[test]
    fn a_rounded_box_covers_its_middle_and_spares_its_corners() {
        let mut px = vec![0u32; 40 * 40];
        rounded(&mut Frame { px: &mut px, w: 40, h: 40 }, 4.0, 4.0, 32.0, 32.0, 10.0, 0xFFFFFF, 1.0);
        assert_eq!(px[20 * 40 + 20], 0xFFFFFF);
        assert_eq!(px[4 * 40 + 4], 0);
        assert_eq!(px[20 * 40 + 2], 0);
    }
}
