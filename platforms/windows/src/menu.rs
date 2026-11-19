//! The right-click menu, which is also where settings live. View draws it itself, in its own
//! colours and font, with submenus that open to the side; the arrow keys, Enter and Esc work as
//! in any Windows menu. The drawing and the mouse and keyboard handling are in `popup.rs`.

use crate::app::App;
use crate::cmd::Cmd;
use crate::popup::{Entry, Mark};
use view_core::folder::SortKey;
use view_core::settings::{Backdrop, PeekMode, Theme, Toolbar, Transparency, Wheel};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCSTR, w};

/// Ask Windows to draw this process's own menus (the window menu from Alt+Space) dark or light.
/// Windows has no documented switch for this; uxtheme's SetPreferredAppMode is what Explorer
/// and Notepad use, and it's absent before Windows 10 1903, in which case they stay light.
pub fn set_dark(dark: bool) {
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        // SetPreferredAppMode is ordinal 135, FlushMenuThemes 136.
        if let Some(f) = GetProcAddress(lib, PCSTR(135 as *const u8)) {
            let set: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set(if dark { 2 } else { 3 }); // ForceDark / ForceLight
        }
        if let Some(f) = GetProcAddress(lib, PCSTR(136 as *const u8)) {
            let flush: extern "system" fn() = std::mem::transmute(f);
            flush();
        }
    }
}

fn item(label: &str, key: &'static str, cmd: Cmd, enabled: bool) -> Entry {
    Entry::Item { label: label.into(), key, cmd, mark: Mark::None, enabled }
}

fn check(label: &str, key: &'static str, cmd: Cmd, on: bool) -> Entry {
    Entry::Item { label: label.into(), key, cmd, mark: if on { Mark::Tick } else { Mark::None }, enabled: true }
}

fn radio(label: &str, cmd: Cmd, on: bool) -> Entry {
    Entry::Item { label: label.into(), key: "", cmd, mark: if on { Mark::Dot } else { Mark::None }, enabled: true }
}

/// A choice for the Space preview, greyed out while it's turned off.
fn peek_mode(label: &str, mode: PeekMode, current: PeekMode) -> Entry {
    Entry::Item { label: label.into(), key: "", cmd: Cmd::PeekMode(mode), mark: if mode == current { Mark::Dot } else { Mark::None }, enabled: crate::spacebar::enabled() }
}

fn sub(label: &str, entries: Vec<Entry>) -> Entry {
    Entry::Sub { label: label.into(), entries }
}

/// Show the menu at a screen point and return what was picked.
pub fn show(app: &App, x: i32, y: i32) -> Option<Cmd> {
    let s = &app.settings;
    if app.editor.is_some() {
        let saving = app.editor.as_ref().is_some_and(|e| e.saving);
        let over = app.can_save_over();
        let entries = vec![
            item("Save", if over { "Enter" } else { "" }, Cmd::EditSave, !saving && over),
            item("Save a copy", if over { "Ctrl+Shift+S" } else { "Enter" }, Cmd::EditSaveCopy, !saving),
            item("Reset", "", Cmd::EditReset, !saving),
            item("Cancel", "Esc", Cmd::EditCancel, !saving),
            Entry::Separator,
            item("Rotate right", "R", Cmd::EditTurn(true), !saving),
            item("Rotate left", "Shift+R", Cmd::EditTurn(false), !saving),
            Entry::Separator,
            check("Full screen", "F11", Cmd::Fullscreen, app.is_fullscreen()),
            item("About View", "F1", Cmd::About, true),
        ];
        return crate::popup::show(app, &entries, x, y, app.ui.menu_anchor.is_some());
    }
    if let Some(t) = &app.trim {
        let saving = t.saving.is_some();
        let over = app.can_save_over();
        let paused = app.player().is_none_or(|p| p.paused() || p.ended);
        let picked = t.selected.and_then(|i| t.parts.get(i));
        let entries = vec![
            item("Save", if over { "Enter" } else { "" }, Cmd::EditSave, !saving && over),
            item("Save a copy", if over { "Ctrl+Shift+S" } else { "Enter" }, Cmd::EditSaveCopy, !saving),
            item("Undo", "Ctrl+Z", Cmd::Undo, !saving && t.can_undo()),
            item("Redo", "Ctrl+Y", Cmd::Redo, !saving && t.can_redo()),
            item("Reset", "", Cmd::EditReset, !saving),
            item("Cancel", "Esc", Cmd::EditCancel, !saving),
            Entry::Separator,
            item(if paused { "Play" } else { "Pause" }, "Space", Cmd::PlayPause, !saving),
            item("Start here", "I", Cmd::TrimMark(true), !saving),
            item("End here", "O", Cmd::TrimMark(false), !saving),
            Entry::Separator,
            item("Split here", "S", Cmd::TrimSplit, !saving),
            item("Delete the part", "Del", Cmd::TrimDelete, !saving && picked.is_some()),
            item(if picked.is_some_and(|p| p.silent) { "Give the part its sound back" } else { "Remove the part's sound" }, "", Cmd::TrimPartSound, !saving && picked.is_some() && t.sound),
            check("Remove the sound", "", Cmd::TrimSound, t.mute && t.sound),
            Entry::Separator,
            check("Full screen", "F11", Cmd::Fullscreen, app.is_fullscreen()),
            item("About View", "F1", Cmd::About, true),
        ];
        return crate::popup::show(app, &entries, x, y, app.ui.menu_anchor.is_some());
    }
    let has = app.shown.as_ref().is_some_and(|x| x.picture.is_some());
    let file = app.folder.current().is_some();
    let fitted = app.vp.is_fitted();
    let editable = app.shown.as_ref().and_then(|x| x.picture.as_ref()).is_some_and(|p| !p.is_animated() && !p.streams) && app.peek.is_none();

    let mut sort: Vec<Entry> = SortKey::ALL.iter().map(|&k| radio(k.label(), Cmd::Sort(k), s.sort == k)).collect();
    sort.push(Entry::Separator);
    sort.push(check("Reverse order", "", Cmd::SortDescending, s.sort_descending));

    let look = vec![
        radio("Theme: System", Cmd::Theme(Theme::System), s.theme == Theme::System),
        radio("Theme: Dark", Cmd::Theme(Theme::Dark), s.theme == Theme::Dark),
        radio("Theme: Light", Cmd::Theme(Theme::Light), s.theme == Theme::Light),
        Entry::Separator,
        radio("Background: Theme", Cmd::Backdrop(Backdrop::Theme), s.backdrop == Backdrop::Theme),
        radio("Background: Black", Cmd::Backdrop(Backdrop::Black), s.backdrop == Backdrop::Black),
        radio("Background: Grey", Cmd::Backdrop(Backdrop::Grey), s.backdrop == Backdrop::Grey),
        radio("Background: White", Cmd::Backdrop(Backdrop::White), s.backdrop == Backdrop::White),
        Entry::Separator,
        radio("Transparency: Checkerboard", Cmd::Transparency(Transparency::Checkerboard), s.transparency == Transparency::Checkerboard),
        radio("Transparency: Background", Cmd::Transparency(Transparency::Backdrop), s.transparency == Transparency::Backdrop),
        radio("Transparency: White", Cmd::Transparency(Transparency::White), s.transparency == Transparency::White),
        radio("Transparency: Black", Cmd::Transparency(Transparency::Black), s.transparency == Transparency::Black),
        Entry::Separator,
        radio("Toolbar: Hide when idle", Cmd::Toolbar(Toolbar::Auto), s.toolbar == Toolbar::Auto),
        radio("Toolbar: Always show", Cmd::Toolbar(Toolbar::Always), s.toolbar == Toolbar::Always),
        radio("Toolbar: Only near the bottom edge", Cmd::Toolbar(Toolbar::Hidden), s.toolbar == Toolbar::Hidden),
        Entry::Separator,
        check("Show the filmstrip", "", Cmd::Filmstrip, s.filmstrip),
        check("Hide the interface", "Tab", Cmd::ToggleUi, app.ui.hidden),
    ];

    let options = vec![
        check("Preview files with Space in File Explorer", "", Cmd::SpacePreview, crate::spacebar::enabled()),
        peek_mode("Space preview: Close when you click away", PeekMode::Popup, s.peek_mode),
        peek_mode("Space preview: Stay open, on top", PeekMode::OnTop, s.peek_mode),
        peek_mode("Space preview: Stay open, like a window", PeekMode::Window, s.peek_mode),
        Entry::Separator,
        radio("Mouse wheel zooms", Cmd::Wheel(Wheel::Zoom), s.wheel == Wheel::Zoom),
        radio("Mouse wheel goes to the next image", Cmd::Wheel(Wheel::Navigate), s.wheel == Wheel::Navigate),
        Entry::Separator,
        check("Enlarge small images to fit", "", Cmd::UpscaleSmall, s.upscale_small),
        check("Smooth zooming", "", Cmd::SmoothZoom, s.smooth_zoom),
        check("Keep zoom between photos", "", Cmd::KeepZoom, s.keep_zoom),
        check("Show pixels when zoomed in", "", Cmd::PixelatedZoom, s.pixelated_zoom),
        check("Go round from the last image to the first", "", Cmd::Wrap, s.wrap),
        check("Remember video positions after closing", "", Cmd::ResumeVideos, s.resume_videos),
        check("Ask before deleting", "", Cmd::ConfirmDelete, s.confirm_delete),
        check("Esc closes View", "", Cmd::EscapeCloses, s.escape_closes),
        check("Update View automatically", "", Cmd::AutoUpdate, s.check_updates),
    ];
    // An update waiting to be had goes in the menu itself; otherwise checking for one is an
    // option, beside updating automatically.
    let (update, can_update) = app.update.menu_item();
    let waiting = can_update && update != "Check for updates";
    let mut options = options;
    if !waiting && app.peek.is_none() {
        options.push(Entry::Separator);
        options.push(item(&update, "", Cmd::Update, can_update));
    }

    // Only what applies to what's showing: no greyed-out lines. Everything left out still has
    // its key, and the rarer things sit in submenus.
    let video = app.player().is_some_and(|p| p.has_video() && p.error.is_none()) && app.peek.is_none();
    let mut entries = vec![item("Open…", "Ctrl+O", Cmd::Open, true)];
    if file {
        entries.push(item("Open with…", "", Cmd::OpenWith, true));
        entries.push(item("Show in folder", "Ctrl+E", Cmd::Reveal, true));
    }
    entries.push(Entry::Separator);
    let mut copy = Vec::new();
    if has {
        copy.push(item("Copy image", "Ctrl+C", Cmd::CopyImage, true));
    }
    if file {
        copy.push(item("Copy file", "Ctrl+Alt+C", Cmd::CopyFile, true));
        copy.push(item("Copy path", "Ctrl+Shift+C", Cmd::CopyPath, true));
    }
    if !copy.is_empty() {
        entries.push(sub("Copy", copy));
    }
    if has {
        entries.push(sub("Rotate and flip", vec![
            item("Rotate right", "R", Cmd::RotateCw, true),
            item("Rotate left", "Shift+R", Cmd::RotateCcw, true),
            item("Flip horizontally", "H", Cmd::FlipH, true),
            item("Flip vertically", "V", Cmd::FlipV, true),
            Entry::Separator,
            item("Save rotation", "Ctrl+S", Cmd::SaveRotation, app.user != view_core::orient::Orient::IDENTITY),
        ]));
    }
    if video {
        entries.push(item("Trim…", "E", Cmd::Edit, true));
    } else if editable {
        entries.push(item("Edit…", "E", Cmd::Edit, true));
    }
    if has && app.peek.is_none() {
        entries.push(item("Set as desktop background", "", Cmd::SetWallpaper, true));
    }
    entries.push(Entry::Separator);
    // One line for zoom: whichever of the two it isn't at.
    if has {
        entries.push(match fitted {
            true => item("Actual size", "Ctrl+1", Cmd::Actual, true),
            false => item("Fit to window", "Ctrl+0", Cmd::Fit, true),
        });
    }
    entries.push(check("Full screen", "F11", Cmd::Fullscreen, app.is_fullscreen()));
    if file {
        entries.push(check("All files as a grid", "G", Cmd::Grid, app.grid.is_some()));
        entries.push(check("Information", "I", Cmd::Info, s.info_panel));
    }
    entries.push(Entry::Separator);
    entries.push(sub("Sort by", sort));
    entries.push(sub("Appearance", look));
    entries.push(sub("Options", options));
    entries.push(Entry::Separator);
    if app.can_undo() {
        entries.push(item(app.undo_label(), "Ctrl+Z", Cmd::Undo, true));
    }
    if file {
        entries.push(item("Delete", "Del", Cmd::Delete, true));
        entries.push(item("Properties", "Ctrl+Enter", Cmd::Properties, true));
    }
    entries.push(Entry::Separator);
    if waiting && app.peek.is_none() {
        entries.push(item(&update, "", Cmd::Update, true));
    }
    entries.push(item("About View", "F1", Cmd::About, true));
    // No separator first, last, or twice over where a group came out empty.
    let mut tidy: Vec<Entry> = Vec::with_capacity(entries.len());
    for e in entries {
        if matches!(e, Entry::Separator) && tidy.last().is_none_or(|l| matches!(l, Entry::Separator)) {
            continue;
        }
        tidy.push(e);
    }
    if matches!(tidy.last(), Some(Entry::Separator)) {
        tidy.pop();
    }
    // From the toolbar it opens upwards, since the toolbar is at the bottom.
    crate::popup::show(app, &tidy, x, y, app.ui.menu_anchor.is_some())
}
