//! Decoded images kept within a memory budget. What goes first when it's full is whatever is
//! furthest from the image being looked at, not whatever was used least recently: when flicking
//! through a folder, the next image matters and the one ten back doesn't.

use crate::picture::Picture;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// A file as it was when decoded. A changed size or date means the pixels are stale.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl Stamp {
    pub fn of(path: &Path) -> Option<Stamp> {
        let m = std::fs::metadata(path).ok()?;
        Some(Stamp { size: m.len(), modified: m.modified().ok() })
    }
}

/// How finely an image was decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Quality {
    /// Shrunk to fit the screen: enough to show it fitted, quick to make and small to keep.
    Screen,
    /// Every pixel, or as many as memory allows for a huge image.
    Full,
}

struct Slot {
    path: PathBuf,
    quality: Quality,
    stamp: Option<Stamp>,
    picture: Arc<Picture>,
}

pub struct Cache {
    slots: Vec<Slot>,
    budget: usize,
}

impl Cache {
    pub fn new(budget: usize) -> Cache {
        Cache { slots: Vec::new(), budget }
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn used(&self) -> usize {
        self.slots.iter().map(|s| s.picture.bytes()).sum()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The best decode of `path` that's still current: full resolution if there is one.
    pub fn get(&self, path: &Path, stamp: Option<Stamp>) -> Option<(Quality, Arc<Picture>)> {
        self.slots
            .iter()
            .filter(|s| s.path == path && (stamp.is_none() || s.stamp == stamp))
            .max_by_key(|s| s.quality)
            .map(|s| (s.quality, s.picture.clone()))
    }

    pub fn has(&self, path: &Path, quality: Quality) -> bool {
        self.slots.iter().any(|s| s.path == path && s.quality >= quality)
    }

    /// Keep a decode. A full decode replaces the screen-sized one, which it makes redundant.
    pub fn put(&mut self, path: PathBuf, quality: Quality, stamp: Option<Stamp>, picture: Arc<Picture>) {
        if self.slots.iter().any(|s| s.path == path && s.stamp == stamp && s.quality > quality) {
            return;
        }
        self.slots.retain(|s| !(s.path == path && (s.quality <= quality || s.stamp != stamp)));
        self.slots.push(Slot { path, quality, stamp, picture });
    }

    pub fn remove(&mut self, path: &Path) {
        self.slots.retain(|s| s.path != path);
    }

    pub fn clear(&mut self) {
        self.slots.clear();
    }

    /// Drop what's out of reach, then bring the cache back within budget. `distance` says how far
    /// each file is from the current one (`None` for files no longer in the folder). Anything
    /// further than `reach` goes even when there's room, so memory falls back after a long
    /// browse. `pinned` files are never dropped. Full decodes of anything but the nearest go
    /// before screen-sized ones.
    pub fn trim(&mut self, distance: impl Fn(&Path) -> Option<usize>, pinned: &[&Path], reach: usize) {
        self.slots.retain(|s| pinned.iter().any(|p| *p == s.path) || distance(&s.path).is_some_and(|d| d <= reach));
        loop {
            if self.used() <= self.budget {
                return;
            }
            let victim = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| !pinned.iter().any(|p| *p == s.path))
                .max_by_key(|(_, s)| {
                    let d = distance(&s.path).unwrap_or(usize::MAX / 2);
                    // A full decode costs several screen-sized ones: weigh it as further away.
                    let weight = if s.quality == Quality::Full { 2 } else { 0 };
                    (d.saturating_add(weight), s.picture.bytes())
                })
                .map(|(i, _)| i);
            match victim {
                Some(i) => {
                    self.slots.remove(i);
                }
                None => return,
            }
        }
    }
}

/// A sensible budget for decoded images, from the machine's memory: a sixteenth of it, between
/// 128 MB and 512 MB. Enough for the current image at full resolution and several neighbours.
pub fn default_budget(total_memory: u64) -> usize {
    (total_memory / 16).clamp(128 << 20, 512 << 20) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orient::Orient;
    use crate::picture::{Bitmap, Details, Frame};

    fn pic(w: u32, h: u32) -> Arc<Picture> {
        Arc::new(Picture {
            width: w,
            height: h,
            frames: vec![Frame { bitmap: Bitmap::try_new(w, h).unwrap(), delay_ms: 0 }],
            loops: 0,
            orient: Orient::IDENTITY,
            has_alpha: false,
            details: Details::default(),
            icc: None,
            streams: false,
        })
    }

    #[test]
    fn full_replaces_screen() {
        let mut c = Cache::new(1 << 30);
        let p = PathBuf::from("a");
        c.put(p.clone(), Quality::Screen, None, pic(10, 10));
        c.put(p.clone(), Quality::Full, None, pic(20, 20));
        assert_eq!(c.len(), 1);
        assert_eq!(c.get(&p, None).unwrap().0, Quality::Full);
        // A later screen-sized decode doesn't push out the full one.
        c.put(p.clone(), Quality::Screen, None, pic(10, 10));
        assert_eq!(c.get(&p, None).unwrap().0, Quality::Full);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn stale_stamps_miss() {
        let mut c = Cache::new(1 << 30);
        let p = PathBuf::from("a");
        let old = Some(Stamp { size: 1, modified: None });
        c.put(p.clone(), Quality::Screen, old, pic(1, 1));
        assert!(c.get(&p, Some(Stamp { size: 2, modified: None })).is_none());
        assert!(c.get(&p, old).is_some());
    }

    #[test]
    fn trims_furthest_first_and_keeps_pinned() {
        let one = 100 * 100 * 4;
        let mut c = Cache::new(one * 2);
        for name in ["a", "b", "c"] {
            c.put(PathBuf::from(name), Quality::Screen, None, pic(100, 100));
        }
        let far = |p: &Path| match p.to_str().unwrap() {
            "a" => Some(5),
            "b" => Some(0),
            _ => Some(1),
        };
        c.trim(far, &[Path::new("b")], usize::MAX);
        assert_eq!(c.len(), 2);
        assert!(c.get(Path::new("a"), None).is_none());
        // Pinned survives even over budget.
        let mut c = Cache::new(0);
        c.put(PathBuf::from("b"), Quality::Full, None, pic(100, 100));
        c.trim(far, &[Path::new("b")], usize::MAX);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn out_of_reach_goes_even_with_room() {
        let mut c = Cache::new(1 << 30);
        for name in ["a", "b", "c", "gone"] {
            c.put(PathBuf::from(name), Quality::Screen, None, pic(10, 10));
        }
        let far = |p: &Path| match p.to_str().unwrap() {
            "a" => Some(9),
            "b" => Some(0),
            "c" => Some(3),
            _ => None,
        };
        c.trim(far, &[], 8);
        assert!(c.has(Path::new("b"), Quality::Screen) && c.has(Path::new("c"), Quality::Screen));
        assert!(!c.has(Path::new("a"), Quality::Screen) && !c.has(Path::new("gone"), Quality::Screen));
    }

    #[test]
    fn budget_scales_with_memory() {
        assert_eq!(default_budget(1 << 30), 128 << 20);
        assert_eq!(default_budget(4 << 30), 256 << 20);
        assert_eq!(default_budget(64 << 30), 512 << 20);
    }
}
