//! Everything the user can ask View to do, from a key, a button or the menu.

use view_core::folder::SortKey;
use view_core::settings::{Backdrop, PeekMode, Theme, Toolbar, Transparency, Wheel};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    Open,
    Next,
    Prev,
    First,
    Last,
    ZoomIn,
    ZoomOut,
    Fit,
    Actual,
    /// Fit if zoomed, 100% if fitted: what clicking the zoom figure does.
    FitOrActual,
    Fullscreen,
    RotateCw,
    RotateCcw,
    FlipH,
    FlipV,
    /// Save the turn in the photo's EXIF orientation, without saving its pixels again.
    SaveRotation,
    CopyImage,
    CopyFile,
    CopyPath,
    /// The text selected in a document preview.
    CopyText,
    Reveal,
    OpenWith,
    Delete,
    Info,
    Properties,
    Reload,
    Close,
    Minimize,
    /// Maximise, or restore if maximised.
    Maximize,
    /// Hide or show all of View's own interface.
    ToggleUi,
    Menu,
    About,
    Theme(Theme),
    Backdrop(Backdrop),
    Transparency(Transparency),
    Sort(SortKey),
    SortDescending,
    Toolbar(Toolbar),
    Wheel(Wheel),
    /// What the Space preview does when you click away from File Explorer.
    PeekMode(PeekMode),
    UpscaleSmall,
    SmoothZoom,
    /// Open the next photo zoomed the way this one is.
    KeepZoom,
    /// Remember where videos were left off after View is closed.
    ResumeVideos,
    PixelatedZoom,
    Wrap,
    ConfirmDelete,
    EscapeCloses,
    /// Turn Space preview in File Explorer on or off.
    SpacePreview,
    /// Play or pause a video or song.
    PlayPause,
    /// Sound on or off.
    Mute,
    /// Skip forwards or back this many seconds.
    Skip(i32),
    /// Pause and move one video frame on (true) or back.
    StepFrame(bool),
    /// From the Space preview: open the file in a View window.
    OpenInView,
    /// In a document in the preview: look for words, or go to a page.
    Find,
    GoToPage,
    FindNext,
    FindPrev,
    /// Close the find or page box.
    CloseBar,
    /// Play videos and songs at `SPEEDS[n]` times normal speed, or step through the speeds.
    Speed(u8),
    Faster,
    Slower,
    /// The playing speeds, as a menu by the speed button.
    SpeedMenu,
    /// Make the photo the desktop background.
    SetWallpaper,
    /// Put the last file deleted back from the Recycle Bin.
    Undo,
    /// Every file in the folder as a grid of thumbnails, or back to the one picked.
    Grid,
    /// Show or hide the strip of thumbnails over the bottom.
    Filmstrip,
    /// Crop, straighten and adjust the photo, then save a copy.
    Edit,
    EditSave,
    EditCancel,
    EditReset,
    /// Hold the crop to `Aspect::ALL[n]`.
    EditAspect(u8),
    /// Turn the photo being edited a quarter turn, clockwise or not.
    EditTurn(bool),
}

/// The speeds videos and songs can play at.
pub const SPEEDS: [f64; 8] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// A speed as the button shows it: "1×", "1.5×", "0.25×".
pub fn speed_label(rate: f64) -> String {
    let s = format!("{rate:.2}");
    format!("{}×", s.trim_end_matches('0').trim_end_matches('.'))
}

impl Cmd {
    /// Whether it can be done while a photo is being edited: the editor's own, and the window's.
    /// Anything that would move to another file, or change this one, waits.
    pub fn while_editing(self) -> bool {
        matches!(
            self,
            Cmd::EditSave | Cmd::EditCancel | Cmd::EditReset | Cmd::EditAspect(_) | Cmd::EditTurn(_) | Cmd::Fullscreen | Cmd::Minimize | Cmd::Maximize | Cmd::Close | Cmd::Menu | Cmd::About | Cmd::CopyPath | Cmd::Reveal | Cmd::Properties
        )
    }
}
