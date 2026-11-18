//! Short, restrained motion between two views, for zooming with the keyboard and the wheel.

use crate::viewport::View;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Tween {
    from: View,
    to: View,
    start: Instant,
    length: Duration,
}

impl Tween {
    pub fn new(from: View, to: View, length: Duration) -> Tween {
        Tween { from, to, start: Instant::now(), length }
    }

    pub fn target(&self) -> View {
        self.to
    }

    /// Where it is at `now`, and whether it's still moving.
    pub fn at(&self, now: Instant) -> (View, bool) {
        let t = if self.length.is_zero() { 1.0 } else { (now - self.start).as_secs_f64() / self.length.as_secs_f64() };
        if t >= 1.0 {
            return (self.to, false);
        }
        let e = ease_out(t.max(0.0));
        // Scale moves geometrically, so a zoom from 10% to 1000% feels even all the way.
        let scale = self.from.scale * (self.to.scale / self.from.scale).powf(e);
        // The centre moves so the zoom stays about the same point throughout: interpolating the
        // centre in step with the scale's progress does that for a zoom about a fixed point.
        let k = if (self.to.scale - self.from.scale).abs() > 1e-12 {
            (scale - self.from.scale) / (self.to.scale - self.from.scale)
        } else {
            e
        };
        let cx = self.from.cx + (self.to.cx - self.from.cx) * k;
        let cy = self.from.cy + (self.to.cy - self.from.cy) * k;
        (View { scale, cx, cy }, true)
    }
}

/// Cubic ease-out: quick to respond, gentle to settle.
fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ends_on_target() {
        let a = View { scale: 1.0, cx: 0.0, cy: 0.0 };
        let b = View { scale: 4.0, cx: 100.0, cy: 50.0 };
        let t = Tween::new(a, b, Duration::from_millis(100));
        let (v, moving) = t.at(Instant::now() + Duration::from_millis(200));
        assert!(!moving);
        assert_eq!(v, b);
    }

    #[test]
    fn zoom_about_a_point_stays_on_it() {
        // Zooming about (10, 0) from scale 1 at centre 0: the target centre is 10 - 10*4 = -30.
        let a = View { scale: 1.0, cx: 0.0, cy: 0.0 };
        let b = View { scale: 4.0, cx: -30.0, cy: 0.0 };
        let t = Tween::new(a, b, Duration::from_millis(100));
        let (v, _) = t.at(t.start + Duration::from_millis(40));
        // The image point under x = 10 is (10 - cx) / scale; it should still be 10.
        assert!(((10.0 - v.cx) / v.scale - 10.0).abs() < 1e-9);
    }
}
