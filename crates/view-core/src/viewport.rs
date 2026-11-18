//! Where the image sits in the window: its scale and the point of the window its centre is at.
//! All in physical pixels, so 100% is one image pixel per screen pixel.

/// The shown image's size (after orientation) and the area it's shown in.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct View {
    /// Screen pixels per image pixel.
    pub scale: f64,
    /// Where the image's centre is, in the area's pixels.
    pub cx: f64,
    pub cy: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Fitted to the area, and refitted when the area changes.
    Fit,
    /// Shown at a scale the user chose.
    Free,
}

/// The zoom steps for the keyboard and the buttons. The fit scale is added between them.
const STEPS: [f64; 30] = [
    0.02, 0.03, 0.05, 0.08, 0.10, 0.15, 0.20, 0.25, 1.0 / 3.0, 0.50, 2.0 / 3.0, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0,
    10.0, 12.0, 16.0, 20.0, 24.0, 32.0, 40.0, 48.0, 64.0,
];

pub const MAX_SCALE: f64 = 64.0;

#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    /// The image as shown, at full resolution.
    pub image_w: f64,
    pub image_h: f64,
    /// The area it's shown in.
    pub area_w: f64,
    pub area_h: f64,
    pub view: View,
    pub mode: Mode,
    /// Whether fitting may enlarge an image smaller than the area.
    pub upscale_small: bool,
}

impl Viewport {
    pub fn new(image_w: u32, image_h: u32, area_w: f64, area_h: f64, upscale_small: bool) -> Viewport {
        let mut v = Viewport {
            image_w: image_w.max(1) as f64,
            image_h: image_h.max(1) as f64,
            area_w: area_w.max(1.0),
            area_h: area_h.max(1.0),
            view: View { scale: 1.0, cx: 0.0, cy: 0.0 },
            mode: Mode::Fit,
            upscale_small,
        };
        v.view = v.fitted();
        v
    }

    /// The scale that fits the whole image in the area.
    pub fn fit_scale(&self) -> f64 {
        let s = (self.area_w / self.image_w).min(self.area_h / self.image_h);
        if self.upscale_small { s } else { s.min(1.0) }
    }

    pub fn min_scale(&self) -> f64 {
        // Small enough to see the whole image as a thumbnail, but not a speck.
        let speck = 16.0 / self.image_w.max(self.image_h);
        (self.fit_scale().min(1.0) / 10.0).max(speck).min(self.fit_scale())
    }

    pub fn fitted(&self) -> View {
        View { scale: self.fit_scale(), cx: self.area_w / 2.0, cy: self.area_h / 2.0 }
    }

    pub fn is_fitted(&self) -> bool {
        self.mode == Mode::Fit
    }

    /// The area changed size (a resize, a panel opening). A fitted image refits; a zoomed one
    /// keeps the same point in the middle.
    pub fn resize(&mut self, area_w: f64, area_h: f64) {
        let (area_w, area_h) = (area_w.max(1.0), area_h.max(1.0));
        let (dx, dy) = ((area_w - self.area_w) / 2.0, (area_h - self.area_h) / 2.0);
        self.area_w = area_w;
        self.area_h = area_h;
        if self.mode == Mode::Fit {
            self.view = self.fitted();
        } else {
            self.view.cx += dx;
            self.view.cy += dy;
            self.view = self.clamped(self.view);
        }
    }

    /// The image itself changed size, for example turned on its side.
    pub fn set_image(&mut self, image_w: u32, image_h: u32) {
        self.image_w = image_w.max(1) as f64;
        self.image_h = image_h.max(1) as f64;
        self.mode = Mode::Fit;
        self.view = self.fitted();
    }

    pub fn fit(&mut self) {
        self.mode = Mode::Fit;
        self.view = self.fitted();
    }

    /// Scale to `scale`, keeping the image point under `(x, y)` where it is.
    pub fn zoom_to(&mut self, scale: f64, x: f64, y: f64) {
        let scale = scale.clamp(self.min_scale(), MAX_SCALE);
        let k = scale / self.view.scale;
        let v = View { scale, cx: x - (x - self.view.cx) * k, cy: y - (y - self.view.cy) * k };
        // At the fit scale the whole image is on screen and centred, so it is simply fitted.
        self.mode = if (scale - self.fit_scale()).abs() < 1e-9 { Mode::Fit } else { Mode::Free };
        self.view = if self.mode == Mode::Fit { self.fitted() } else { self.clamped(v) };
    }

    /// Multiply the scale by `factor` about `(x, y)`. Zooming out stops once at the fit scale,
    /// so a flick of the wheel lands on "fit" instead of flying past it.
    pub fn zoom_by(&mut self, factor: f64, x: f64, y: f64) {
        let fit = self.fit_scale();
        let mut target = self.view.scale * factor;
        let s = self.view.scale;
        if factor < 1.0 && s > fit + 1e-9 && target < fit {
            target = fit;
        } else if factor > 1.0 && s < fit - 1e-9 && target > fit {
            target = fit;
        }
        self.zoom_to(target, x, y);
    }

    /// The next zoom step up or down from the current scale, with the fit scale among them.
    pub fn step(&self, up: bool) -> f64 {
        let s = self.view.scale;
        let fit = self.fit_scale();
        let mut steps: Vec<f64> = STEPS.to_vec();
        steps.push(fit);
        steps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let next = if up {
            steps.into_iter().find(|&v| v > s * 1.001)
        } else {
            steps.into_iter().rev().find(|&v| v < s / 1.001)
        };
        next.unwrap_or(s).clamp(self.min_scale(), MAX_SCALE)
    }

    /// Move the image by `(dx, dy)` screen pixels.
    pub fn pan(&mut self, dx: f64, dy: f64) {
        if self.mode == Mode::Fit && !self.overflows() {
            return;
        }
        self.mode = Mode::Free;
        self.view.cx += dx;
        self.view.cy += dy;
        self.view = self.clamped(self.view);
    }

    /// Whether the image is bigger than the area either way, so it can be panned.
    pub fn overflows(&self) -> bool {
        self.image_w * self.view.scale > self.area_w + 0.5 || self.image_h * self.view.scale > self.area_h + 0.5
    }

    /// Keep the image covering the area where it's bigger than it, and centred where it's smaller.
    pub fn clamped(&self, v: View) -> View {
        let (w, h) = (self.image_w * v.scale, self.image_h * v.scale);
        let clamp_axis = |c: f64, size: f64, area: f64| {
            if size <= area { area / 2.0 } else { c.clamp(area - size / 2.0, size / 2.0) }
        };
        View { scale: v.scale, cx: clamp_axis(v.cx, w, self.area_w), cy: clamp_axis(v.cy, h, self.area_h) }
    }

    /// The image's rectangle in the area: left, top, width, height. At 100% the corner is put on
    /// a whole pixel, so pixels map one to one and stay sharp.
    pub fn image_rect(&self, v: View) -> (f64, f64, f64, f64) {
        let (w, h) = (self.image_w * v.scale, self.image_h * v.scale);
        let (mut x, mut y) = (v.cx - w / 2.0, v.cy - h / 2.0);
        if (v.scale - v.scale.round()).abs() < 1e-9 {
            x = x.round();
            y = y.round();
        }
        (x, y, w, h)
    }

    /// The image pixel under an area point, at full resolution, or `None` off the image.
    pub fn image_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (l, t, w, h) = self.image_rect(self.view);
        let (px, py) = ((x - l) / self.view.scale, (y - t) / self.view.scale);
        (x >= l && y >= t && x < l + w && y < t + h).then_some((px, py))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn fits_and_centres() {
        let v = Viewport::new(4000, 2000, 1000.0, 1000.0, false);
        assert!(close(v.view.scale, 0.25));
        assert!(close(v.view.cx, 500.0) && close(v.view.cy, 500.0));
        assert_eq!(v.mode, Mode::Fit);
    }

    #[test]
    fn small_images_stay_actual_size_unless_asked() {
        let v = Viewport::new(100, 100, 1000.0, 800.0, false);
        assert!(close(v.fit_scale(), 1.0));
        let v = Viewport::new(100, 100, 1000.0, 800.0, true);
        assert!(close(v.fit_scale(), 8.0));
    }

    #[test]
    fn zoom_keeps_point_under_cursor() {
        let mut v = Viewport::new(2000, 1000, 1000.0, 500.0, false);
        let before = v.image_point(250.0, 100.0).unwrap();
        v.zoom_to(2.0, 250.0, 100.0);
        let after = v.image_point(250.0, 100.0).unwrap();
        assert!(close(before.0, after.0) && close(before.1, after.1), "{before:?} vs {after:?}");
        assert_eq!(v.mode, Mode::Free);
    }

    #[test]
    fn zooming_out_lands_on_fit_once() {
        let mut v = Viewport::new(2000, 1000, 1000.0, 500.0, false);
        v.zoom_to(1.0, 500.0, 250.0);
        v.zoom_by(0.1, 500.0, 250.0);
        assert!(close(v.view.scale, 0.5));
        assert_eq!(v.mode, Mode::Fit);
        v.zoom_by(0.8, 500.0, 250.0);
        assert!(v.view.scale < 0.5);
        // Even below fit the image stays centred.
        assert!(close(v.view.cx, 500.0));
    }

    #[test]
    fn steps_include_fit() {
        let mut v = Viewport::new(3000, 1000, 1000.0, 1000.0, false);
        let fit = v.fit_scale();
        assert!(close(v.step(true), 0.5));
        v.zoom_to(0.5, 0.0, 0.0);
        assert!(close(v.step(false), fit));
        v.zoom_to(1.0, 0.0, 0.0);
        assert!(close(v.step(true), 1.25));
        assert!(close(v.step(false), 0.75));
    }

    #[test]
    fn pan_stops_at_edges() {
        let mut v = Viewport::new(2000, 1000, 1000.0, 500.0, false);
        v.zoom_to(1.0, 500.0, 250.0);
        v.pan(100_000.0, 0.0);
        let (l, _, _, _) = v.image_rect(v.view);
        assert!(close(l, 0.0), "left edge stops at the window's: {l}");
        v.pan(-100_000.0, 0.0);
        let (l, _, w, _) = v.image_rect(v.view);
        assert!(close(l + w, 1000.0));
    }

    #[test]
    fn fitted_image_does_not_pan() {
        let mut v = Viewport::new(200, 100, 1000.0, 500.0, false);
        let before = v.view;
        v.pan(50.0, 50.0);
        assert_eq!(v.view, before);
    }

    #[test]
    fn resize_refits_or_keeps_centre() {
        let mut v = Viewport::new(2000, 1000, 1000.0, 500.0, false);
        v.resize(500.0, 500.0);
        assert!(close(v.view.scale, 0.25));
        v.zoom_to(1.0, 250.0, 250.0);
        let p = v.image_point(250.0, 250.0).unwrap();
        v.resize(700.0, 700.0);
        let q = v.image_point(350.0, 350.0).unwrap();
        assert!(close(p.0, q.0) && close(p.1, q.1));
    }

    #[test]
    fn actual_size_snaps_to_whole_pixels() {
        let mut v = Viewport::new(101, 101, 1000.0, 1000.0, false);
        v.zoom_to(1.0, 500.0, 500.0);
        let (l, t, _, _) = v.image_rect(v.view);
        assert_eq!(l, l.round());
        assert_eq!(t, t.round());
    }

    #[test]
    fn scale_limits_hold() {
        let mut v = Viewport::new(100, 100, 1000.0, 1000.0, false);
        v.zoom_to(1e9, 0.0, 0.0);
        assert!(close(v.view.scale, MAX_SCALE));
        v.zoom_to(1e-9, 0.0, 0.0);
        assert!(v.view.scale >= v.min_scale() - 1e-12);
    }
}
