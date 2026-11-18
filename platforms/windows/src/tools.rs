//! The window's photo tools, beside showing the file: the grid of thumbnails and the filmstrip,
//! editing a photo and saving a copy, making it the desktop background, putting a deleted file
//! back, dragging the file out to another app, and how fast videos play. A part of `app.rs`,
//! kept apart since it's a lot; the drawing is in `ui.rs`.

use super::*;
use crate::cmd::{SPEEDS, speed_label};
use crate::editor::{Editor, Grip};
use crate::popup::{Entry as MenuEntry, Mark};
use view_core::edit::Handle;

/// How long the Undo button stays on the message after a delete.
pub const UNDO_TIME: Duration = Duration::from_secs(8);
/// The smallest and biggest thumbnails in the grid, in DIPs.
const GRID_MIN: u32 = 100;
const GRID_MAX: u32 = 320;

impl App {
    // ---- How fast it plays ----

    pub(crate) fn set_speed(&mut self, rate: f64) {
        self.speed = rate;
        if let Some(p) = self.player_mut() {
            p.set_rate(rate);
        }
        self.say(&format!("Speed {}", speed_label(rate)));
    }

    /// One speed faster or slower; with `wrap`, faster and round from the fastest to the slowest.
    pub(crate) fn speed_step(&mut self, faster: bool, wrap: bool) {
        let i = SPEEDS.iter().position(|&s| (s - self.speed).abs() < 1e-6).unwrap_or(3);
        let n = SPEEDS.len();
        let j = if wrap {
            (i + 1) % n
        } else if faster {
            (i + 1).min(n - 1)
        } else {
            i.saturating_sub(1)
        };
        self.set_speed(SPEEDS[j]);
    }

    /// The speeds, as a menu over the speed button. The preview can't hold a menu (it never
    /// has the keyboard), so there a click steps through them instead.
    pub(crate) fn speed_menu(&mut self) {
        if self.peek.is_some() {
            return self.speed_step(true, true);
        }
        let entries: Vec<MenuEntry> = SPEEDS
            .iter()
            .enumerate()
            .map(|(i, &r)| MenuEntry::Item {
                label: if r == 1.0 { "Normal".into() } else { speed_label(r) },
                key: "",
                cmd: Cmd::Speed(i as u8),
                mark: if (r - self.speed).abs() < 1e-6 { Mark::Dot } else { Mark::None },
                enabled: true,
            })
            .collect();
        let (x, y) = self.ui.anchor_of(ui::Hit::Button(Cmd::SpeedMenu)).unwrap_or(self.mouse);
        let mut pt = POINT { x, y };
        unsafe {
            let _ = ClientToScreen(self.hwnd, &mut pt);
        }
        if let Some(cmd) = crate::popup::show(self, &entries, pt.x, pt.y - (4.0 * self.scale()) as i32, true) {
            self.run(cmd);
        }
    }

    // ---- Undoing a delete ----

    pub fn can_undo(&self) -> bool {
        !self.deleted.is_empty()
    }

    /// Put the last file deleted from here back where it was, and show it.
    pub(crate) fn undo_delete(&mut self) {
        let Some(path) = self.deleted.pop() else {
            self.say("There's nothing to undo");
            return;
        };
        self.undo_until = None;
        match shell::restore(&path) {
            Ok(()) => {
                self.cache.remove(&path);
                if let Some(g) = &mut self.grid {
                    g.follow = true;
                }
                self.open(path);
                self.say("Put back");
            }
            Err(e) => self.show_dialog(ui::Dialog::notice("View couldn't put the file back", e)),
        }
    }

    // ---- The desktop background ----

    /// Make the photo the desktop background. A copy is saved for Windows to use, upright and
    /// as a JPEG, so it works for any format View opens and stays put if the photo is moved.
    pub(crate) fn set_wallpaper(&mut self) {
        let Some(path) = self.shown.as_ref().filter(|s| s.picture.is_some()).map(|s| s.path.clone()) else { return };
        if self.peek.is_some() {
            return;
        }
        let user = self.user;
        let slot = self.saved.clone();
        let hwnd = self.hwnd.0 as isize;
        self.say("Setting the background…");
        let _ = std::thread::Builder::new().name("wallpaper".into()).spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            }
            let result = (|| -> Result<(), String> {
                let wic = crate::wic::Wic::new().ok_or("Windows' picture codecs aren't working.")?;
                let req = crate::wic::Request { max_width: 0, max_height: 0, max_pixels: full_pixel_limit(), slack: 1.0 };
                let p = crate::worker::decode(Some(&wic), &path, req, &AtomicBool::new(false)).map_err(|f| f.message())?;
                let f = &p.frames[0].bitmap;
                let (w, h, data) = view_core::orient::apply(p.orient.then(user), f.width, f.height, &f.data);
                let dir = util::settings_dir().join("Wallpaper");
                std::fs::create_dir_all(&dir).map_err(|e| format!("View couldn't keep a copy for the background: {e}"))?;
                // Only the newest is kept.
                if let Ok(old) = std::fs::read_dir(&dir) {
                    for e in old.flatten() {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
                let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let file = dir.join(format!("background-{stamp}.jpg"));
                wic.save(&file, w, h, &data, true, None)?;
                shell::set_wallpaper(&file)
            })();
            *slot.lock().unwrap() = Some(Finished::Wallpaper(result));
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_SAVED, WPARAM(0), LPARAM(0));
            }
        });
    }

    /// A copy saved or the background set, off this thread.
    pub(crate) fn take_saved(&mut self) {
        let Some(done) = self.saved.lock().unwrap().take() else { return };
        match done {
            Finished::Wallpaper(Ok(())) => self.say("Desktop background set"),
            Finished::Wallpaper(Err(e)) => self.show_dialog(ui::Dialog::notice("View couldn't set the background", e)),
            Finished::Copy(Ok(path)) => {
                self.editor = None;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.open(path);
                self.say(&format!("Saved as \u{201C}{name}\u{201D}"));
            }
            Finished::Copy(Err(e)) => {
                if let Some(ed) = &mut self.editor {
                    ed.saving = false;
                }
                self.show_dialog(ui::Dialog::notice("View couldn't save the copy", e));
            }
        }
    }

    // ---- Dragging the file out ----

    /// Whether a press here, with nothing else to do, takes the file out to another app when
    /// dragged: with Ctrl held, or where the picture can't be panned and the window can't be
    /// moved (maximised, full screen, or the preview).
    pub(crate) fn drags_out(&self, ctrl: bool) -> bool {
        let Some(s) = &self.shown else { return false };
        let has = s.picture.is_some() || s.player.as_ref().is_some_and(|p| p.error.is_none());
        if !has || self.editor.is_some() {
            return false;
        }
        if ctrl {
            return true;
        }
        let still = s.picture.is_some() && !self.vp.overflows();
        still && (self.is_fullscreen() || self.is_maximized() || self.peek.is_some())
    }

    /// The mouse has moved far enough from where it was pressed to be a drag, not a click.
    pub(crate) fn past_drag_threshold(&self, x0: i32, y0: i32, x: i32, y: i32) -> bool {
        let (dx, dy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
        (x - x0).abs() > dx.max(4) || (y - y0).abs() > dy.max(4)
    }

    /// Drag a file out, now. Windows runs the drag until it's dropped.
    pub(crate) fn drag_file_out(&mut self, path: &Path) {
        self.drag = None;
        self.ui.pressed = None;
        unsafe {
            let _ = ReleaseCapture();
        }
        shell::drag_out(path);
        self.invalidate();
    }

    // ---- The grid ----

    /// Every file in the folder as a grid of thumbnails; or back from it to the one showing.
    pub(crate) fn toggle_grid(&mut self) {
        if self.grid.take().is_some() {
            self.ui.show(true);
            self.arm_idle();
            self.invalidate();
            return;
        }
        if self.peek.is_some() || self.editor.is_some() || self.folder.is_empty() {
            return;
        }
        if let Some(p) = self.player_mut().filter(|p| p.playing()) {
            p.pause();
        }
        self.held_z = None;
        self.grid = Some(Grid { selected: self.folder.index, scroll: 0.0, target: 0.0, keyed: false, follow: false });
        self.grid_reveal(true);
        self.ui.show(true);
        self.invalidate();
    }

    fn grid_layout(&self) -> ui::GridLayout {
        let (cw, ch) = self.client_size();
        ui::grid_layout(cw, ch, self.scale(), self.settings.grid_size, self.folder.len())
    }

    /// Scroll the grid so the selected thumbnail is in sight; with `jump`, at once.
    pub(crate) fn grid_reveal(&mut self, jump: bool) {
        let l = self.grid_layout();
        let (_, ch) = self.client_size();
        let Some(g) = &mut self.grid else { return };
        let row = (g.selected / l.cols) as f32;
        let top = l.top + row * l.step;
        if top - g.target < l.top {
            g.target = top - l.top;
        }
        if top + l.cell - g.target > ch - l.below {
            g.target = top + l.cell - (ch - l.below);
        }
        g.target = g.target.clamp(0.0, l.max_scroll(ch));
        if jump {
            g.scroll = g.target;
        }
        self.invalidate();
    }

    /// Scroll the grid by this many pixels.
    pub(crate) fn grid_scroll(&mut self, dy: f32) {
        let l = self.grid_layout();
        let (_, ch) = self.client_size();
        if let Some(g) = &mut self.grid {
            g.target = (g.target + dy).clamp(0.0, l.max_scroll(ch));
        }
        self.invalidate();
    }

    /// Bigger or smaller thumbnails.
    pub(crate) fn grid_resize(&mut self, bigger: bool) {
        let size = self.settings.grid_size as f32 * if bigger { 1.18 } else { 1.0 / 1.18 };
        self.settings.grid_size = (size.round() as u32).clamp(GRID_MIN, GRID_MAX);
        self.changed();
        self.grid_reveal(true);
    }

    /// Show a file from the grid.
    pub(crate) fn grid_open(&mut self, index: usize) {
        self.grid = None;
        if index != self.folder.index && index < self.folder.len() {
            self.forward = index > self.folder.index;
            self.go(index, false);
        }
        self.ui.show(true);
        self.arm_idle();
        self.invalidate();
    }

    /// Keys in the grid. False for keys it leaves to the window.
    pub(crate) fn grid_key(&mut self, key: VIRTUAL_KEY, ctrl: bool) -> bool {
        let n = self.folder.len();
        let Some(i) = self.grid.as_ref().map(|g| g.selected) else { return false };
        match key {
            VK_ESCAPE | VK_G if !ctrl => {
                self.toggle_grid();
                return true;
            }
            VK_RETURN | VK_SPACE if n > 0 => {
                self.grid_open(i);
                return true;
            }
            VK_DELETE => {
                self.run(Cmd::Delete);
                return true;
            }
            VK_OEM_PLUS | VK_ADD => {
                self.grid_resize(true);
                return true;
            }
            VK_OEM_MINUS | VK_SUBTRACT => {
                self.grid_resize(false);
                return true;
            }
            _ => {}
        }
        if n == 0 {
            return matches!(key, VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_HOME | VK_END | VK_PRIOR | VK_NEXT);
        }
        let l = self.grid_layout();
        let (_, ch) = self.client_size();
        let cols = l.cols;
        let page = (((ch - l.top - l.below) / l.step).floor() as usize).max(1) * cols;
        let to = match key {
            VK_LEFT => i.saturating_sub(1),
            VK_RIGHT => (i + 1).min(n - 1),
            VK_UP => i.checked_sub(cols).unwrap_or(i),
            // From the row above the last, a short last row still takes Down: to its end.
            VK_DOWN if i / cols < (n - 1) / cols => (i + cols).min(n - 1),
            VK_DOWN => i,
            VK_HOME => 0,
            VK_END => n - 1,
            VK_PRIOR => i.saturating_sub(page),
            VK_NEXT => (i + page).min(n - 1),
            _ => return false,
        };
        if let Some(g) = &mut self.grid {
            g.selected = to;
            g.keyed = true;
        }
        self.grid_reveal(false);
        true
    }

    // ---- The filmstrip ----

    /// Whether the strip of thumbnails is over the bottom of the window.
    pub fn strip_shown(&self) -> bool {
        self.settings.filmstrip && self.peek.is_none() && self.editor.is_none() && self.grid.is_none() && self.folder.len() > 1 && self.shown.is_some()
    }

    /// Whether the filmstrip is still gliding to the file showing.
    pub(crate) fn strip_moving(&self) -> bool {
        self.strip_shown() && (self.strip_at - self.folder.index as f32).abs() > 0.01
    }

    // ---- Editing ----

    /// Start editing the photo showing.
    pub(crate) fn start_edit(&mut self) {
        if self.peek.is_some() || self.editor.is_some() {
            return;
        }
        let Some(s) = &self.shown else { return };
        let Some(p) = &s.picture else {
            if s.player.is_some() {
                self.say("Only photos can be edited");
            }
            return;
        };
        if p.is_animated() || p.streams {
            self.say("Moving pictures can't be edited");
            return;
        }
        let path = s.path.clone();
        match Editor::new(&path, &p.frames[0].bitmap, p.orient, self.user) {
            Some(e) => {
                self.grid = None;
                self.held_z = None;
                if let Some(w) = &mut self.words {
                    w.selection = None;
                }
                self.editor = Some(e);
                self.ui.show(true);
                self.invalidate();
            }
            None => self.say("Couldn't edit this photo"),
        }
    }

    /// Leave the editor. With changes made, it asks first, unless `sure`.
    pub(crate) fn edit_cancel(&mut self, sure: bool) {
        let Some(ed) = &self.editor else { return };
        if ed.saving {
            return;
        }
        if ed.has_changes() && !sure {
            self.show_dialog(ui::Dialog {
                ask: ui::Ask::Discard,
                title: "Discard your changes?".into(),
                body: "The photo stays as it was.".into(),
                check: None,
                buttons: vec!["Discard", "Keep editing"],
                focus: 0,
                keyed: false,
            });
            return;
        }
        self.editor = None;
        self.drag = None;
        self.ui.show(true);
        self.arm_idle();
        self.invalidate();
    }

    /// Save the edited photo as a copy beside it, off this thread: the full-size picture,
    /// changed as the preview was.
    pub(crate) fn edit_save(&mut self) {
        let has_alpha = self.shown.as_ref().and_then(|s| s.picture.as_ref()).is_some_and(|p| p.has_alpha);
        let Some(ed) = &mut self.editor else { return };
        if ed.saving {
            return;
        }
        if !ed.has_changes() {
            self.say("Nothing's been changed yet");
            return;
        }
        ed.saving = true;
        let (path, turns, edit) = (ed.path.clone(), ed.turns, ed.edit);
        let slot = self.saved.clone();
        let hwnd = self.hwnd.0 as isize;
        let _ = std::thread::Builder::new().name("save".into()).spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            }
            let result = (|| -> Result<PathBuf, String> {
                let wic = crate::wic::Wic::new().ok_or("Windows' picture codecs aren't working.")?;
                let req = crate::wic::Request { max_width: 0, max_height: 0, max_pixels: full_pixel_limit(), slack: 1.0 };
                let p = crate::worker::decode(Some(&wic), &path, req, &AtomicBool::new(false)).map_err(|f| f.message())?;
                let f = &p.frames[0].bitmap;
                let (w, h, data) = view_core::orient::apply(p.orient.then(turns), f.width, f.height, &f.data);
                let out = view_core::edit::render(&view_core::picture::Bitmap { width: w, height: h, data }, &edit, true).ok_or("The photo is too big to edit.")?;
                let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                let png = has_alpha || p.has_alpha || matches!(ext.as_str(), "png" | "gif" | "bmp" | "ico");
                let ext = if png { "png" } else if ext == "jpeg" { "jpeg" } else { "jpg" };
                let target = copy_name(&path, ext);
                wic.save(&target, out.width, out.height, &out.data, !png, Some(&path))?;
                Ok(target)
            })();
            *slot.lock().unwrap() = Some(Finished::Copy(result));
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_SAVED, WPARAM(0), LPARAM(0));
            }
        });
        self.invalidate();
    }

    /// Commands while editing.
    pub(crate) fn edit_command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::EditSave => self.edit_save(),
            Cmd::EditCancel => self.edit_cancel(false),
            Cmd::EditReset => {
                if let Some(ed) = &mut self.editor {
                    ed.reset();
                }
            }
            Cmd::EditAspect(i) => {
                if let (Some(ed), Some(&a)) = (&mut self.editor, view_core::edit::Aspect::ALL.get(i as usize)) {
                    ed.set_aspect(a);
                }
            }
            Cmd::EditTurn(clockwise) => {
                if let Some(ed) = &mut self.editor {
                    ed.turn(clockwise);
                }
            }
            _ => {}
        }
        self.invalidate();
    }

    /// Keys while editing. Everything else is swallowed, so nothing moves to another file.
    pub(crate) fn editor_key(&mut self, key: VIRTUAL_KEY, ctrl: bool, shift: bool, alt: bool) -> bool {
        let cmd = match key {
            VK_ESCAPE => Cmd::EditCancel,
            VK_RETURN if alt => Cmd::Fullscreen,
            VK_RETURN => Cmd::EditSave,
            VK_S if ctrl => Cmd::EditSave,
            VK_R if shift => Cmd::EditTurn(false),
            VK_R if !ctrl => Cmd::EditTurn(true),
            VK_F11 => Cmd::Fullscreen,
            VK_APPS => Cmd::Menu,
            VK_F1 => Cmd::About,
            VK_W if ctrl => Cmd::Close,
            _ => return true,
        };
        self.run(cmd);
        true
    }

    /// The part of the crop's frame under a point, if any.
    pub(crate) fn crop_handle(&self, x: f32, y: f32) -> Option<Handle> {
        let ed = self.editor.as_ref()?;
        let f = self.ui.edit_frame?;
        let (fw, fh) = (f.right - f.left, f.bottom - f.top);
        let c = ed.edit.crop;
        let (l, t, r, b) = (f.left + c.x * fw, f.top + c.y * fh, f.left + (c.x + c.w) * fw, f.top + (c.y + c.h) * fh);
        let k = 14.0 * self.scale();
        if x < l - k || x > r + k || y < t - k || y > b + k {
            return None;
        }
        let (west, east, north, south) = ((x - l).abs() < k, (x - r).abs() < k, (y - t).abs() < k, (y - b).abs() < k);
        Some(match (west, east, north, south) {
            (true, _, true, _) => Handle::NW,
            (_, true, true, _) => Handle::NE,
            (true, _, _, true) => Handle::SW,
            (_, true, _, true) => Handle::SE,
            (true, ..) => Handle::W,
            (_, true, ..) => Handle::E,
            (_, _, true, _) => Handle::N,
            (_, _, _, true) => Handle::S,
            _ if x > l && x < r && y > t && y < b => Handle::Move,
            _ => return None,
        })
    }

    /// A press in the editor, on nothing of the interface: start moving the crop.
    pub(crate) fn edit_press(&mut self, x: i32, y: i32) {
        let Some(h) = self.crop_handle(x as f32, y as f32) else {
            self.drag = None;
            return;
        };
        if let Some(ed) = &mut self.editor {
            ed.grip = Some((Grip::Crop(h), ed.edit.crop, x as f32, y as f32));
        }
        self.drag = Some(Drag::Crop);
        self.invalidate();
    }

    /// The mouse moving the crop.
    pub(crate) fn edit_crop_to(&mut self, x: i32, y: i32) {
        let Some(f) = self.ui.edit_frame else { return };
        let Some(ed) = &mut self.editor else { return };
        let Some((Grip::Crop(h), start, x0, y0)) = ed.grip else { return };
        let (w, hh) = ed.size();
        let (dx, dy) = ((x as f32 - x0) / (f.right - f.left).max(1.0), (y as f32 - y0) / (f.bottom - f.top).max(1.0));
        let ratio = ed.ratio();
        ed.edit.crop = view_core::edit::drag(start, h, dx, dy, ratio, w, hh, 32.0);
        self.invalidate();
    }

    /// The mouse moving one of the sliders.
    pub(crate) fn edit_slide(&mut self, i: u8, x: i32) {
        let v = self.ui.slider_value(i, x as f32);
        if let Some(ed) = &mut self.editor {
            ed.set_value(i, v);
            ed.grip = Some((Grip::Slider(i), ed.edit.crop, x as f32, 0.0));
        }
        self.invalidate();
    }

    /// The mouse pointer for the editor: a resizing arrow over the crop's edges.
    pub(crate) fn edit_cursor(&self) -> PCWSTR {
        let grip = self.editor.as_ref().and_then(|e| e.grip).map(|g| g.0);
        let handle = match grip {
            Some(Grip::Crop(h)) => Some(h),
            Some(Grip::Slider(_)) => None,
            None if self.ui.hit(self.mouse.0 as f32, self.mouse.1 as f32).is_none() => self.crop_handle(self.mouse.0 as f32, self.mouse.1 as f32),
            None => None,
        };
        match handle {
            Some(Handle::Move) => IDC_SIZEALL,
            Some(Handle::N | Handle::S) => IDC_SIZENS,
            Some(Handle::E | Handle::W) => IDC_SIZEWE,
            Some(Handle::NW | Handle::SE) => IDC_SIZENWSE,
            Some(Handle::NE | Handle::SW) => IDC_SIZENESW,
            None => IDC_ARROW,
        }
    }

    // ---- Folders and fonts in the preview ----

    /// A folder in the preview: a card, with what's in it and some of its pictures, counted
    /// off this thread.
    pub(crate) fn start_folder(&mut self, path: PathBuf) {
        self.start_card(path.clone());
        let Some(peek) = &self.peek else { return };
        let slot = peek.folder.clone();
        let hwnd = self.hwnd.0 as isize;
        let px = (200.0 * self.scale()).round() as i32;
        let _ = std::thread::Builder::new().name("folder".into()).spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            }
            let (summary, pictures) = folder_contents(&path);
            let bitmaps = pictures.iter().filter_map(|p| shell::thumbnail(p, px)).collect();
            *slot.lock().unwrap() = Some((path, summary, bitmaps));
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CARD, WPARAM(0), LPARAM(0));
            }
        });
    }
}

/// A name for an edited copy beside the photo: "Beach (edited).jpg", then "Beach (edited 2).jpg".
fn copy_name(path: &Path, ext: &str) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Photo".into());
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    (1..)
        .map(|n| dir.join(if n == 1 { format!("{stem} (edited).{ext}") } else { format!("{stem} (edited {n}).{ext}") }))
        .find(|p| !p.exists())
        .unwrap()
}

/// What's in a folder, all the way down, as a line for its card: "120 files in 4 folders ·
/// 2.4 GB". Gives up counting after a while, for a huge folder. Also the first few pictures at
/// its top.
fn folder_contents(dir: &Path) -> (String, Vec<PathBuf>) {
    let started = Instant::now();
    let (mut files, mut folders, mut bytes, mut seen) = (0u64, 0u64, 0u64, 0u64);
    let mut cut = false;
    let mut pictures = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    'walk: while let Some(d) = stack.pop() {
        let Ok(list) = std::fs::read_dir(&d) else { continue };
        let top = d == dir;
        for e in list.flatten() {
            seen += 1;
            if seen > 200_000 || started.elapsed() > Duration::from_millis(1500) {
                cut = true;
                break 'walk;
            }
            let Ok(t) = e.file_type() else { continue };
            if t.is_symlink() {
                continue;
            }
            if t.is_dir() {
                folders += 1;
                stack.push(e.path());
            } else {
                files += 1;
                bytes += e.metadata().map_or(0, |m| m.len());
                let picture = e.path().extension().is_some_and(|x| view_core::formats::Format::from_extension(&x.to_string_lossy().to_ascii_lowercase()).is_some());
                if top && picture && pictures.len() < 6 {
                    pictures.push(e.path());
                }
            }
        }
    }
    let count = |n: u64, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{} {many}", group(n)) };
    let summary = if files == 0 && folders == 0 {
        "Empty".to_string()
    } else {
        let at_least = if cut { "At least " } else { "" };
        let what = match (files, folders) {
            (0, d) => count(d, "folder", "folders"),
            (f, 0) => count(f, "file", "files"),
            (f, d) => format!("{} in {}", count(f, "file", "files"), count(d, "folder", "folders")),
        };
        if files > 0 { format!("{at_least}{what}  ·  {}", view_core::human_bytes(bytes)) } else { format!("{at_least}{what}") }
    };
    (summary, pictures)
}

/// 12345 as "12,345".
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
