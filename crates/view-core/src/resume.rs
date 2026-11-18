//! Where videos were left off, so they can start there again. Kept in a file of its own beside
//! the settings, read and written whole each time, so the Space preview and View windows can
//! all add to it without one undoing another's settings.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// The most videos remembered; the ones left longest ago go first.
pub const MAX_POSITIONS: usize = 200;

/// How far into a video it was left.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub path: String,
    pub seconds: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Positions {
    /// Newest last.
    pub videos: Vec<Position>,
}

impl Positions {
    pub fn load(path: &Path) -> Positions {
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Note how far into `video` it was left. Near either end there's nothing to come back to,
    /// so it's forgotten instead.
    pub fn remember(&mut self, video: &str, seconds: f64, duration: Option<f64>) {
        self.videos.retain(|v| !v.path.eq_ignore_ascii_case(video));
        if worth_resuming(seconds, duration) {
            self.videos.push(Position { path: video.to_string(), seconds });
            let extra = self.videos.len().saturating_sub(MAX_POSITIONS);
            self.videos.drain(..extra);
        }
    }

    /// Where `video` was left off, if it's remembered.
    pub fn get(&self, video: &str) -> Option<f64> {
        self.videos.iter().rev().find(|v| v.path.eq_ignore_ascii_case(video)).map(|v| v.seconds).filter(|s| s.is_finite() && *s > 0.0)
    }
}

/// Whether a video left at `seconds` is worth starting there again: not in its first few
/// seconds, and not so near the end that it's as good as finished.
pub fn worth_resuming(seconds: f64, duration: Option<f64>) -> bool {
    if !seconds.is_finite() || seconds < 5.0 {
        return false;
    }
    match duration {
        Some(d) if d.is_finite() && d > 0.0 => seconds < d - 10.0 && seconds < d * 0.95,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_kept_newest_last() {
        let mut p = Positions::default();
        p.remember("C:\\a.mp4", 60.0, Some(600.0));
        p.remember("C:\\b.mp4", 2.0, Some(600.0));
        assert_eq!(p.get("c:\\A.MP4"), Some(60.0));
        assert_eq!(p.get("C:\\b.mp4"), None, "the first seconds aren't kept");
        p.remember("C:\\a.mp4", 595.0, Some(600.0));
        assert_eq!(p.get("C:\\a.mp4"), None, "nearly finished is forgotten");
        for i in 0..MAX_POSITIONS + 5 {
            p.remember(&format!("C:\\{i}.mp4"), 30.0, None);
        }
        assert_eq!(p.videos.len(), MAX_POSITIONS);
        assert_eq!(p.get("C:\\0.mp4"), None);
        assert!(p.get(&format!("C:\\{}.mp4", MAX_POSITIONS + 4)).is_some());
    }

    #[test]
    fn positions_round_trip_through_a_file() {
        let path = std::env::temp_dir().join(format!("view-positions-{}.json", std::process::id()));
        let mut p = Positions::default();
        p.remember("C:\\film.mkv", 3725.5, Some(7200.0));
        p.save(&path).unwrap();
        assert_eq!(Positions::load(&path), p);
        std::fs::remove_file(&path).unwrap();
    }
}
