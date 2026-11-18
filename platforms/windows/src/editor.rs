//! Editing a photo: crop, straighten, and brightness, contrast, saturation and warmth, then
//! saving the result as a copy beside it. The original is never written to. While editing, a
//! screen-sized copy is changed and shown; saving does the same to the full-size picture, off
//! the window's thread. The maths is in `view_core::edit`; drawing is in `ui.rs`.

use crate::gfx::{Gpu, Texture};
use std::path::{Path, PathBuf};
use view_core::edit::{self, Aspect, Crop, Edit, Handle};
use view_core::orient::Orient;
use view_core::picture::Bitmap;

/// The longest side of the copy shown while editing.
const PREVIEW: u32 = 1800;

/// The sliders, top to bottom: their names, and the straightening angle's range.
pub const SLIDERS: [&str; 5] = ["Straighten", "Brightness", "Contrast", "Saturation", "Warmth"];

/// What a drag in the editor is moving.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Grip {
    Crop(Handle),
    Slider(u8),
}

pub struct Editor {
    pub path: PathBuf,
    /// The photo as it was on screen when editing started, shrunk, and turned by the editor's
    /// own quarter turns.
    base: Bitmap,
    /// How the decoded pixels are turned to be upright as edited: the file's own orientation,
    /// then the turns made in View, then the editor's.
    pub orient: Orient,
    /// Turns on top of the file's own orientation, for the saved copy.
    pub turns: Orient,
    pub edit: Edit,
    pub aspect: Aspect,
    pub preview: Option<Texture>,
    dirty: bool,
    /// While dragging: what, the crop when it started, and where the mouse started.
    pub grip: Option<(Grip, Crop, f32, f32)>,
    /// Being saved now.
    pub saving: bool,
    /// Quarter turns made in the editor, clockwise.
    quarters: u8,
}

impl Editor {
    /// Start editing a photo shown as `bitmap` (the decoded pixels, before turning), turned by
    /// `exif` from the file and `user` in View.
    pub fn new(path: &Path, bitmap: &Bitmap, exif: Orient, user: Orient) -> Option<Editor> {
        let (w, h) = view_core::picture::fit_within(bitmap.width, bitmap.height, PREVIEW, PREVIEW, u64::MAX);
        let small = view_core::picture::downscale(bitmap, w, h)?;
        let orient = exif.then(user);
        let (w, h, data) = view_core::orient::apply(orient, small.width, small.height, &small.data);
        Some(Editor { path: path.to_path_buf(), base: Bitmap { width: w, height: h, data }, orient, turns: user, edit: Edit::default(), aspect: Aspect::Free, preview: None, dirty: true, grip: None, saving: false, quarters: 0 })
    }

    /// The photo's size as edited, before cropping, in preview pixels.
    pub fn size(&self) -> (f32, f32) {
        (self.base.width as f32, self.base.height as f32)
    }

    /// The crop held to the shape picked, if one is.
    pub fn ratio(&self) -> Option<f32> {
        let (w, h) = self.size();
        self.aspect.ratio(w, h)
    }

    /// Make the preview again if anything changed. The crop isn't cut out of it: it's drawn
    /// over it, so the frame can still be moved.
    pub fn refresh(&mut self, gpu: &Gpu) {
        if !self.dirty && self.preview.is_some() {
            return;
        }
        self.dirty = false;
        let shown = Edit { crop: Crop::FULL, ..self.edit };
        if let Some(b) = edit::render(&self.base, &shown, false) {
            self.preview = gpu.upload(&b);
        }
    }

    /// The GPU was replaced.
    pub fn reset_gpu(&mut self) {
        self.preview = None;
        self.dirty = true;
    }

    /// A slider's value, from -1 to 1.
    pub fn value(&self, i: u8) -> f32 {
        let a = &self.edit.adjust;
        match i {
            0 => self.edit.angle / edit::MAX_ANGLE,
            1 => a.brightness,
            2 => a.contrast,
            3 => a.saturation,
            _ => a.warmth,
        }
    }

    pub fn set_value(&mut self, i: u8, v: f32) {
        let v = v.clamp(-1.0, 1.0);
        // Close to the middle sticks to it, so none is easy to get back to.
        let v = if v.abs() < 0.025 { 0.0 } else { v };
        let a = &mut self.edit.adjust;
        match i {
            0 => self.edit.angle = (v * edit::MAX_ANGLE * 10.0).round() / 10.0,
            1 => a.brightness = v,
            2 => a.contrast = v,
            3 => a.saturation = v,
            _ => a.warmth = v,
        }
        self.dirty = true;
    }

    /// A slider's value as it reads: "-12°", "+35", "0".
    pub fn value_text(&self, i: u8) -> String {
        if i == 0 {
            let a = self.edit.angle;
            return if a == 0.0 { "0°".into() } else { format!("{a:+.1}°") };
        }
        let v = (self.value(i) * 100.0).round() as i32;
        if v == 0 { "0".into() } else { format!("{v:+}") }
    }

    pub fn set_aspect(&mut self, a: Aspect) {
        self.aspect = a;
        let (w, h) = self.size();
        self.edit.crop = match a.ratio(w, h) {
            Some(r) => edit::centred(r, w, h),
            None => self.edit.crop,
        };
    }

    /// A quarter turn of the photo being edited. The crop starts again.
    pub fn turn(&mut self, clockwise: bool) {
        let o = if clockwise { Orient::IDENTITY.rotated_cw() } else { Orient::IDENTITY.rotated_ccw() };
        let (w, h, data) = view_core::orient::apply(o, self.base.width, self.base.height, &self.base.data);
        self.base = Bitmap { width: w, height: h, data };
        self.orient = self.orient.then(o);
        self.turns = self.turns.then(o);
        self.quarters = (self.quarters + if clockwise { 1 } else { 3 }) % 4;
        self.edit.crop = Crop::FULL;
        let a = self.aspect;
        self.set_aspect(a);
        self.dirty = true;
    }

    pub fn reset(&mut self) {
        self.edit = Edit::default();
        self.aspect = Aspect::Free;
        self.dirty = true;
    }

    /// Whether anything has been changed, so leaving would lose it.
    pub fn has_changes(&self) -> bool {
        !self.edit.is_none() || self.quarters % 4 != 0
    }
}
