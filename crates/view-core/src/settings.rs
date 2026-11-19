//! The user's choices, saved as JSON. Every field has a default, so an old or hand-edited file
//! still loads, and anything unknown is ignored.

use crate::folder::SortKey;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

/// What's behind the image.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backdrop {
    /// The theme's own background.
    #[default]
    Theme,
    Black,
    Grey,
    White,
}

/// What's behind the transparent parts of an image.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transparency {
    /// A checkerboard, dark or light to suit the backdrop.
    #[default]
    Checkerboard,
    /// The backdrop shows through.
    Backdrop,
    White,
    Black,
}

/// What the scroll wheel does without Ctrl.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wheel {
    #[default]
    Zoom,
    Navigate,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Toolbar {
    /// Shown while the mouse moves, hidden when it rests.
    #[default]
    Auto,
    Always,
    /// The minimal mode: hidden unless the mouse is right over where it would be.
    Hidden,
}

/// What the Space preview does when you click or switch away from File Explorer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeekMode {
    /// It closes, like a pop-up.
    #[default]
    Popup,
    /// It stays, above every window, until it's closed.
    OnTop,
    /// It stays, like any other window: other apps can cover it, and it comes back up with
    /// File Explorer.
    Window,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    /// The accent colour as "#rrggbb", or empty for the system's.
    pub accent: String,
    pub backdrop: Backdrop,
    pub transparency: Transparency,
    pub sort: SortKey,
    pub sort_descending: bool,
    /// Go from the last image round to the first.
    pub wrap: bool,
    pub wheel: Wheel,
    pub toolbar: Toolbar,
    /// Enlarge images smaller than the window to fill it when fitting.
    pub upscale_small: bool,
    /// Animate zooming.
    pub smooth_zoom: bool,
    /// Show pixels as squares when zoomed in a long way, instead of smoothing them.
    pub pixelated_zoom: bool,
    pub info_panel: bool,
    pub confirm_delete: bool,
    /// Esc closes View when there's nothing else for it to close.
    pub escape_closes: bool,
    pub placement: Option<Placement>,
    /// How loud videos and music play, from 0 to 1, and whether they're muted.
    pub volume: f32,
    pub muted: bool,
    /// Megabytes for decoded images, 0 for a size picked from the machine's memory.
    pub cache_mb: u32,
    /// Keep the zoom and the part of the picture shown when going to the next photo.
    pub keep_zoom: bool,
    /// Remember where each video was left off, even after View is closed.
    pub resume_videos: bool,
    pub peek_mode: PeekMode,
    /// A strip of thumbnails over the bottom, with the toolbar.
    pub filmstrip: bool,
    /// How big the grid's thumbnails are, in DIPs.
    pub grid_size: u32,
    /// Look for a new version once a day, and install it when View closes.
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: Theme::System,
            accent: String::new(),
            backdrop: Backdrop::Theme,
            transparency: Transparency::Checkerboard,
            sort: SortKey::Name,
            sort_descending: false,
            wrap: true,
            wheel: Wheel::Zoom,
            toolbar: Toolbar::Auto,
            upscale_small: false,
            smooth_zoom: true,
            pixelated_zoom: true,
            info_panel: false,
            confirm_delete: true,
            escape_closes: false,
            placement: None,
            volume: 1.0,
            muted: false,
            cache_mb: 0,
            keep_zoom: false,
            resume_videos: false,
            peek_mode: PeekMode::Popup,
            filmstrip: true,
            grid_size: 170,
            check_updates: true,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// Save by writing a new file and moving it into place, so a crash mid-write can't leave
    /// half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    /// The accent colour, if a valid one is set.
    pub fn accent_rgb(&self) -> Option<(u8, u8, u8)> {
        let hex = self.accent.trim().trim_start_matches('#');
        if hex.len() != 6 {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_files_fill_in_defaults() {
        let s: Settings = serde_json::from_str(r#"{"theme":"light","unknown":1}"#).unwrap();
        assert_eq!(s.theme, Theme::Light);
        assert!(s.wrap);
        assert_eq!(s.sort, SortKey::Name);
    }

    #[test]
    fn round_trips_through_a_file() {
        let path = std::env::temp_dir().join(format!("view-settings-{}.json", std::process::id()));
        let s = Settings { sort: SortKey::Modified, accent: "#4c8dff".into(), ..Settings::default() };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        assert_eq!(s.accent_rgb(), Some((0x4c, 0x8d, 0xff)));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn broken_files_give_defaults() {
        let path = std::env::temp_dir().join(format!("view-settings-bad-{}.json", std::process::id()));
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        std::fs::remove_file(&path).unwrap();
    }
}
