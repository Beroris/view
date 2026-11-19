//! How a stored image is turned to show it: the camera's EXIF orientation, then the user's own
//! turns and flips. Kept as one value and applied when drawing, so turning an image never
//! touches its pixels.
//!
//! The transform is "mirror left to right if `flip`, then turn `turns` quarters clockwise".

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Orient {
    /// Quarter turns clockwise, 0 to 3.
    pub turns: u8,
    /// Mirrored left to right, before turning.
    pub flip: bool,
}

impl Orient {
    pub const IDENTITY: Orient = Orient { turns: 0, flip: false };

    /// From the EXIF Orientation tag, 1 to 8. Anything else is upright.
    pub fn from_exif(value: u32) -> Orient {
        let (turns, flip) = match value {
            2 => (0, true),
            3 => (2, false),
            4 => (2, true),
            5 => (3, true),
            6 => (1, false),
            7 => (1, true),
            8 => (3, false),
            _ => (0, false),
        };
        Orient { turns, flip }
    }

    /// Back to the EXIF Orientation value.
    pub fn to_exif(self) -> u32 {
        match (self.turns % 4, self.flip) {
            (0, false) => 1,
            (0, true) => 2,
            (2, false) => 3,
            (2, true) => 4,
            (3, true) => 5,
            (1, false) => 6,
            (1, true) => 7,
            (3, false) => 8,
            _ => 1,
        }
    }

    /// Whether width and height trade places.
    pub fn swaps_axes(self) -> bool {
        self.turns % 2 == 1
    }

    /// The size shown for a stored size.
    pub fn shown_size(self, w: u32, h: u32) -> (u32, u32) {
        if self.swaps_axes() { (h, w) } else { (w, h) }
    }

    pub fn rotated_cw(self) -> Orient {
        Orient { turns: (self.turns + 1) % 4, ..self }
    }

    pub fn rotated_ccw(self) -> Orient {
        Orient { turns: (self.turns + 3) % 4, ..self }
    }

    /// Mirrored left to right as it's shown. Mirroring after a turn is the same as mirroring
    /// first and turning the other way.
    pub fn flipped_horizontally(self) -> Orient {
        Orient { turns: (4 - self.turns % 4) % 4, flip: !self.flip }
    }

    /// Mirrored top to bottom as it's shown: a left-right mirror and a half turn.
    pub fn flipped_vertically(self) -> Orient {
        let o = self.flipped_horizontally();
        Orient { turns: (o.turns + 2) % 4, ..o }
    }

    /// `self`, then `then` on top.
    pub fn then(self, then: Orient) -> Orient {
        let mut o = self;
        if then.flip {
            o = o.flipped_horizontally();
        }
        for _ in 0..then.turns % 4 {
            o = o.rotated_cw();
        }
        o
    }

    /// What has to come after `self` to end up at `to`: the user's turns, kept the same on screen
    /// when the file's own orientation changes underneath them.
    pub fn to(self, to: Orient) -> Orient {
        (0..8).map(|i| Orient { turns: i % 4, flip: i >= 4 }).find(|&x| self.then(x) == to).unwrap_or(Orient::IDENTITY)
    }

    /// Where a stored pixel lands, for a stored image of `w` by `h`. Used for pixel-exact
    /// copies, which need the upright image.
    pub fn map(self, x: u32, y: u32, w: u32, h: u32) -> (u32, u32) {
        let (mut x, mut y, mut w, mut h) = (x, y, w, h);
        if self.flip {
            x = w - 1 - x;
        }
        for _ in 0..self.turns % 4 {
            // A quarter turn clockwise: (x, y) in w×h goes to (h-1-y, x) in h×w.
            (x, y) = (h - 1 - y, x);
            (w, h) = (h, w);
        }
        (x, y)
    }
}

/// Turn a packed 4-byte-per-pixel image upright. Returns the new width and height.
pub fn apply(o: Orient, w: u32, h: u32, src: &[u8]) -> (u32, u32, Vec<u8>) {
    if o == Orient::IDENTITY || w == 0 || h == 0 {
        return (w, h, src.to_vec());
    }
    let (ow, oh) = o.shown_size(w, h);
    let mut out = vec![0u8; src.len()];
    // Each stored row lands as a straight run in the upright image: a row, or a column, forwards
    // or backwards. So one row needs only where its first pixel goes and the step to the next.
    let index = |x: u32, y: u32| {
        let (nx, ny) = o.map(x, y, w, h);
        (ny as usize * ow as usize + nx as usize) as isize
    };
    let step = if w > 1 { index(1, 0) - index(0, 0) } else { 0 };
    for (y, row) in src.chunks_exact(w as usize * 4).take(h as usize).enumerate() {
        let mut d = index(0, y as u32);
        if step == 1 {
            let at = d as usize * 4;
            out[at..at + row.len()].copy_from_slice(row);
            continue;
        }
        for px in row.chunks_exact(4) {
            let at = d as usize * 4;
            out[at..at + 4].copy_from_slice(px);
            d += step;
        }
    }
    (ow, oh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_way_from_one_orientation_to_another_is_found() {
        for a in 1..=8 {
            for b in 1..=8 {
                let (a, b) = (Orient::from_exif(a), Orient::from_exif(b));
                assert_eq!(a.then(a.to(b)), b);
            }
        }
    }

    #[test]
    fn exif_round_trips() {
        for v in 1..=8 {
            assert_eq!(Orient::from_exif(v).to_exif(), v);
        }
    }

    /// A 2×1 image [A B] under every EXIF value lands where the EXIF spec says.
    #[test]
    fn exif_values_place_pixels_correctly() {
        // Orientation 6: the stored image must be turned 90° clockwise to show it.
        // Stored row [A B] becomes a column, A on top.
        let o = Orient::from_exif(6);
        assert_eq!(o.map(0, 0, 2, 1), (0, 0));
        assert_eq!(o.map(1, 0, 2, 1), (0, 1));
        // 8: 90° anticlockwise, B on top.
        let o = Orient::from_exif(8);
        assert_eq!(o.map(0, 0, 2, 1), (0, 1));
        assert_eq!(o.map(1, 0, 2, 1), (0, 0));
        // 2: mirrored.
        assert_eq!(Orient::from_exif(2).map(0, 0, 2, 1), (1, 0));
        // 5: transpose, (x, y) -> (y, x).
        let o = Orient::from_exif(5);
        assert_eq!(o.map(1, 0, 2, 1), (0, 1));
        // 7: transverse, (x, y) -> (h-1-y, w-1-x).
        let o = Orient::from_exif(7);
        assert_eq!(o.map(0, 0, 2, 1), (0, 1));
        assert_eq!(o.map(1, 0, 2, 1), (0, 0));
    }

    #[test]
    fn flips_and_turns_compose() {
        let o = Orient::IDENTITY;
        assert_eq!(o.flipped_horizontally().flipped_horizontally(), o);
        assert_eq!(o.flipped_vertically().flipped_vertically(), o);
        assert_eq!(o.rotated_cw().rotated_ccw(), o);
        assert_eq!(o.rotated_cw().rotated_cw().rotated_cw().rotated_cw(), o);
        // A vertical flip is a horizontal flip plus a half turn.
        assert_eq!(o.flipped_vertically(), Orient { turns: 2, flip: true });
        // Turning a mirrored image, then mirroring again, is a turn the other way.
        assert_eq!(o.flipped_horizontally().rotated_cw().flipped_horizontally(), o.rotated_ccw());
    }

    #[test]
    fn flip_after_turn_matches_pixels() {
        // Turn a 3×2 image clockwise, then mirror it as shown: compare against doing it by hand.
        let o = Orient::IDENTITY.rotated_cw().flipped_horizontally();
        let (w, h) = (3, 2);
        for y in 0..h {
            for x in 0..w {
                let (tx, ty) = Orient::IDENTITY.rotated_cw().map(x, y, w, h);
                let by_hand = (h - 1 - tx, ty); // mirror within the turned 2×3
                assert_eq!(o.map(x, y, w, h), by_hand);
            }
        }
    }

    /// Every orientation puts every pixel where `map` says, for sizes with one row or column too.
    #[test]
    fn apply_agrees_with_map() {
        for (w, h) in [(5u32, 3u32), (1, 4), (4, 1), (1, 1)] {
            let src: Vec<u8> = (0..w * h).flat_map(|i| [i as u8, 0, 0, 255]).collect();
            for v in 1..=8 {
                let o = Orient::from_exif(v);
                let (ow, oh, out) = apply(o, w, h, &src);
                assert_eq!((ow, oh), o.shown_size(w, h));
                for y in 0..h {
                    for x in 0..w {
                        let (nx, ny) = o.map(x, y, w, h);
                        assert_eq!(out[((ny * ow + nx) * 4) as usize], (y * w + x) as u8, "orientation {v}, {w}x{h}, pixel {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn apply_turns_pixels() {
        // 2×1: red, blue -> turned clockwise, 1×2: red over blue.
        let src = [255, 0, 0, 255, 0, 0, 255, 255];
        let (w, h, out) = apply(Orient::from_exif(6), 2, 1, &src);
        assert_eq!((w, h), (1, 2));
        assert_eq!(out, src);
        let (_, _, out) = apply(Orient::from_exif(8), 2, 1, &src);
        assert_eq!(out, [0, 0, 255, 255, 255, 0, 0, 255]);
    }
}
