//! Drawing: the image itself, edge to edge, and View's own interface floating over it. There's
//! no title bar: the file's name and the window buttons sit over the top of the picture, the
//! tools in a frosted-glass bar at the bottom, and arrows appear when the mouse nears a side.
//! All of it fades away when the mouse rests, so the image has the whole window.

use crate::app::App;
use crate::cmd::Cmd;
use crate::gfx::{Gpu, Texture, rect};
use crate::pick::{Join, Picker};
use crate::text::{Align, Text};
use crate::theme::{Palette, Rgb};
use std::time::{Duration, Instant};
use view_core::orient::Orient;
use view_core::picture::Picture;
use view_core::settings::Transparency;
use view_core::viewport::View;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::*;
use windows_numerics::Matrix3x2;

#[path = "screens.rs"]
mod screens;
pub use screens::{GridLayout, grid_layout};

const FADE: Duration = Duration::from_millis(160);
/// How long the volume slider takes to slide out.
const SLIDE: Duration = Duration::from_millis(180);
/// A button's fill coming up under the mouse, and going again.
const GLOW_IN: Duration = Duration::from_millis(110);
const GLOW_OUT: Duration = Duration::from_millis(200);
/// A button turning on or off.
const SWITCH: Duration = Duration::from_millis(260);
/// The photo and the grid giving way to each other.
pub const SCREEN_SWITCH: Duration = Duration::from_millis(240);

/// One button's fill and its "on" ring, each easing towards where it's going.
struct Glow {
    hit: Hit,
    /// How strong the fill is, in hover steps: from, to, since when.
    fill: (f32, f32, Instant),
    /// How far on it is, 0 to 1: from, to, since when.
    on: (f32, f32, Instant),
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

impl Glow {
    fn fill_now(&self) -> f32 {
        let (from, to, at) = self.fill;
        let d = if to > from { GLOW_IN } else { GLOW_OUT };
        from + (to - from) * ease_out(at.elapsed().as_secs_f32() / d.as_secs_f32())
    }
    fn on_now(&self) -> f32 {
        let (from, to, at) = self.on;
        // Turning on overshoots a little, so it lands with a small bounce.
        let t = (at.elapsed().as_secs_f32() / SWITCH.as_secs_f32()).clamp(0.0, 1.0);
        let e = if to > from { 1.0 + 2.2 * (t - 1.0).powi(3) + 1.2 * (t - 1.0).powi(2) } else { ease_out(t) };
        from + (to - from) * e
    }
    fn moving(&self) -> bool {
        self.fill.2.elapsed() < GLOW_OUT || self.on.2.elapsed() < SWITCH
    }
}
/// The strip at the top that works as the title bar, in DIPs.
pub const TITLE: f32 = 40.0;
/// Window buttons, in DIPs: Windows' own width.
const CAPTION_W: f32 = 46.0;
/// The height of the glass pills along the top: the name, and the window buttons.
const PILL: f32 = 30.0;
/// The information card's width, and its gap from the window's edges (the same as the window
/// buttons' capsule, so their right edges line up), in DIPs.
pub const CARD_W: f32 = 300.0;
pub const CARD_MARGIN: f32 = 5.0;
/// How far blurred glass smears what's under it, in DIPs.
const BLUR: f32 = 18.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Button(Cmd),
    Zoom,
    NavPrev,
    NavNext,
    PanelClose,
    Panel,
    Toolbar,
    /// The window buttons in the top corner.
    Minimize,
    Maximize,
    CloseWindow,
    /// A button in the open dialog, by position.
    DialogButton(u8),
    DialogCheck,
    /// The volume slider beside the sound button.
    Volume,
    /// A video's seek bar.
    Seek,
    /// Anywhere else while a dialog is open: swallowed.
    Modal,
    /// The text in the preview's find or page box.
    Field,
    /// A thumbnail in the filmstrip or the grid, by its place in the folder.
    Thumb(u32),
    /// The filmstrip, between its thumbnails.
    Strip,
    /// One of the editor's sliders.
    Slider(u8),
    /// The name at the top: a click shows or hides the information card, a drag moves the
    /// window.
    Title,
    /// The trimmed video's start (0) and end (1) handles, the bar between (2), and its splits
    /// (3 and on).
    Trim(u8),
}

pub struct State {
    /// Tab hides all of View's interface.
    pub hidden: bool,
    pub hover: Option<Hit>,
    pub pressed: Option<Hit>,
    pub over_controls: bool,
    pub mouse_inside: bool,
    pub last_move: Instant,
    pub wheel_rest: i32,
    pub menu_anchor: Option<(i32, i32)>,
    shown: bool,
    fade_from: f32,
    fade_at: Instant,
    /// Where the controls were drawn last, for clicks.
    targets: Vec<(D2D_RECT_F, Hit)>,
    panel: Option<D2D_RECT_F>,
    panel_scroll: f32,
    panel_content: f32,
    /// The volume slider's track, and how far it's slid out (0 to 1) and since when.
    volume_track: Option<D2D_RECT_F>,
    volume_out: bool,
    volume_from: f32,
    volume_at: Instant,
    /// The seek bar's track, for turning a click into a time.
    pub seek_track: Option<D2D_RECT_F>,
    /// The speeds, over the speed button: out or not, and how far out (0 to 1) since when.
    speeds_out: bool,
    speeds_from: f32,
    speeds_at: Instant,
    /// The filmstrip comes up as the mouse nears the bottom: out or not, and how far (0 to 1)
    /// since when.
    strip_out: bool,
    strip_from: f32,
    strip_at: Instant,
    /// The mark on the chosen speed slides to it: the row it left, the row it's going to, and
    /// since when.
    speed_mark: (f32, f32),
    speed_mark_at: Instant,
    /// A dialog is up: only its controls answer the mouse.
    modal: bool,
    /// Interface text that can be selected, and what is.
    pub pick: crate::pick::Picker,
    /// The find box's text as last drawn, and where, for clicks in it.
    field: Option<(windows::Win32::Graphics::DirectWrite::IDWriteTextLayout, f32)>,
    /// Everything answers the mouse, faded or not: the grid and the editor have nothing to
    /// fade away for.
    pub solid: bool,
    /// Where the photo being edited is on screen, and the editor's slider tracks.
    pub edit_frame: Option<D2D_RECT_F>,
    sliders: [Option<D2D_RECT_F>; 5],
    /// Each button's fill and ring, eased. Kept across screens, so the grid button's ring grows
    /// as the grid comes in.
    glows: std::cell::RefCell<Vec<Glow>>,
}

impl State {
    pub fn new() -> State {
        let long_ago = Instant::now() - Duration::from_secs(10);
        State {
            hidden: false,
            hover: None,
            pressed: None,
            over_controls: false,
            mouse_inside: false,
            last_move: Instant::now(),
            wheel_rest: 0,
            menu_anchor: None,
            shown: true,
            fade_from: 1.0,
            fade_at: long_ago,
            targets: Vec::new(),
            panel: None,
            panel_scroll: 0.0,
            panel_content: 0.0,
            seek_track: None,
            speeds_out: false,
            speeds_from: 0.0,
            speeds_at: long_ago,
            strip_out: false,
            strip_from: 0.0,
            strip_at: long_ago,
            speed_mark: (0.0, 0.0),
            speed_mark_at: long_ago,
            volume_track: None,
            volume_out: false,
            volume_from: 0.0,
            volume_at: long_ago,
            modal: false,
            pick: crate::pick::Picker::default(),
            field: None,
            solid: false,
            edit_frame: None,
            sliders: [None; 5],
            glows: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// A button's fill and on-ness this frame, easing towards `fill` and `on`.
    fn glow(&self, hit: Hit, fill: f32, on: f32) -> (f32, f32) {
        let mut glows = self.glows.borrow_mut();
        let now = Instant::now();
        let i = match glows.iter().position(|g| g.hit == hit) {
            Some(i) => i,
            None => {
                let long_ago = now - SWITCH;
                glows.push(Glow { hit, fill: (0.0, 0.0, long_ago), on: (on, on, long_ago) });
                glows.len() - 1
            }
        };
        let g = &mut glows[i];
        if g.fill.1 != fill {
            g.fill = (g.fill_now(), fill, now);
        }
        if g.on.1 != on {
            g.on = (g.on_now(), on, now);
        }
        (g.fill_now(), g.on_now())
    }

    /// Show or hide the toolbar and arrows, fading.
    pub fn show(&mut self, show: bool) {
        if show != self.shown {
            self.fade_from = self.alpha();
            self.shown = show;
            self.fade_at = Instant::now();
        }
    }

    pub fn alpha(&self) -> f32 {
        let t = (self.fade_at.elapsed().as_secs_f32() / FADE.as_secs_f32()).min(1.0);
        let to = if self.shown { 1.0 } else { 0.0 };
        self.fade_from + (to - self.fade_from) * t
    }

    pub fn fading(&self) -> bool {
        self.fade_at.elapsed() < FADE
            || self.volume_at.elapsed() < SLIDE
            || self.speeds_at.elapsed() < SLIDE
            || self.speed_mark_at.elapsed() < SLIDE
            || self.strip_at.elapsed() < SLIDE
            || self.glows.borrow().iter().any(Glow::moving)
    }

    /// Bring the filmstrip up, or let it go.
    pub fn slide_strip(&mut self, out: bool) {
        if out != self.strip_out {
            self.strip_from = self.strip_open();
            self.strip_out = out;
            self.strip_at = Instant::now();
        }
    }

    /// How far up the filmstrip is, from 0 to 1, eased.
    pub fn strip_open(&self) -> f32 {
        let t = (self.strip_at.elapsed().as_secs_f32() / SLIDE.as_secs_f32()).min(1.0);
        let t = 1.0 - (1.0 - t).powi(3);
        let to = if self.strip_out { 1.0 } else { 0.0 };
        self.strip_from + (to - self.strip_from) * t
    }

    /// Whether the speeds are out over the speed button.
    pub fn speeds_out(&self) -> bool {
        self.speeds_out
    }

    /// Bring the speeds out, with the mark on row `chosen`, or put them away.
    pub fn show_speeds(&mut self, out: bool, chosen: usize) {
        if out != self.speeds_out {
            self.speeds_from = self.speeds_open();
            self.speeds_out = out;
            self.speeds_at = Instant::now();
            if out {
                self.speed_mark = (chosen as f32, chosen as f32);
            }
        }
    }

    /// Slide the mark to row `chosen`, from wherever it is.
    pub fn mark_speed(&mut self, chosen: usize) {
        if self.speed_mark.1 != chosen as f32 {
            self.speed_mark = (self.speed_mark_row(), chosen as f32);
            self.speed_mark_at = Instant::now();
        }
    }

    /// How far out the speeds are, from 0 to 1, eased.
    fn speeds_open(&self) -> f32 {
        let t = (self.speeds_at.elapsed().as_secs_f32() / SLIDE.as_secs_f32()).min(1.0);
        let t = 1.0 - (1.0 - t).powi(3);
        let to = if self.speeds_out { 1.0 } else { 0.0 };
        self.speeds_from + (to - self.speeds_from) * t
    }

    /// The row the mark is over right now, part-way between two while it slides.
    fn speed_mark_row(&self) -> f32 {
        let t = (self.speed_mark_at.elapsed().as_secs_f32() / SLIDE.as_secs_f32()).min(1.0);
        let t = 1.0 - (1.0 - t).powi(3);
        self.speed_mark.0 + (self.speed_mark.1 - self.speed_mark.0) * t
    }

    /// Slide the volume slider out or back in.
    fn slide_volume(&mut self, out: bool) {
        if out != self.volume_out {
            self.volume_from = self.volume_open();
            self.volume_out = out;
            self.volume_at = Instant::now();
        }
    }

    /// How far out the volume slider is, from 0 to 1, eased.
    fn volume_open(&self) -> f32 {
        let t = (self.volume_at.elapsed().as_secs_f32() / SLIDE.as_secs_f32()).min(1.0);
        let t = 1.0 - (1.0 - t).powi(3);
        let to = if self.volume_out { 1.0 } else { 0.0 };
        self.volume_from + (to - self.volume_from) * t
    }

    /// The volume for a point along the slider, from 0 to 1.
    pub fn volume_fraction(&self, x: f32) -> f32 {
        self.volume_track.map_or(0.0, |r| ((x - r.left) / (r.right - r.left).max(1.0)).clamp(0.0, 1.0))
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        let inside = |r: &D2D_RECT_F| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        if self.modal {
            let in_dialog = self.targets.iter().rev().find(|(r, h)| matches!(h, Hit::DialogButton(_) | Hit::DialogCheck) && inside(r));
            return Some(in_dialog.map_or(Hit::Modal, |(_, h)| *h));
        }
        // The window buttons and the panel's close button always work; the rest only while seen.
        let visible = self.solid || (self.alpha() > 0.3 && !self.hidden);
        for (r, hit) in self.targets.iter().rev() {
            let always = matches!(hit, Hit::PanelClose | Hit::Minimize | Hit::Maximize | Hit::CloseWindow | Hit::Field | Hit::Button(Cmd::CloseBar | Cmd::FindNext | Cmd::FindPrev | Cmd::Undo));
            if (visible || always) && inside(r) {
                return Some(*hit);
            }
        }
        self.panel.filter(inside).map(|_| Hit::Panel)
    }

    pub fn command(&self, hit: Hit) -> Option<Cmd> {
        match hit {
            Hit::Button(c) => Some(c),
            Hit::Zoom => Some(Cmd::FitOrActual),
            Hit::NavPrev => Some(Cmd::Prev),
            Hit::NavNext => Some(Cmd::Next),
            Hit::PanelClose => Some(Cmd::Info),
            Hit::Minimize => Some(Cmd::Minimize),
            Hit::Maximize => Some(Cmd::Maximize),
            Hit::CloseWindow => Some(Cmd::Close),
            Hit::Panel | Hit::Toolbar | Hit::Seek | Hit::Volume | Hit::DialogButton(_) | Hit::DialogCheck | Hit::Modal | Hit::Field | Hit::Thumb(_) | Hit::Strip | Hit::Slider(_) | Hit::Title | Hit::Trim(_) => None,
        }
    }

    /// Where a menu opened from a button should hang from: its top-left corner.
    pub fn anchor_of(&self, hit: Hit) -> Option<(i32, i32)> {
        self.targets.iter().find(|(_, h)| *h == hit).map(|(r, _)| (r.left as i32, r.top as i32))
    }

    pub fn over_panel(&self, x: f32, y: f32) -> bool {
        self.panel.is_some_and(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
    }

    /// A slider's value for a point along it, from -1 to 1.
    pub fn slider_value(&self, i: u8, x: f32) -> f32 {
        self.sliders.get(i as usize).copied().flatten().map_or(0.0, |r| ((x - r.left) / (r.right - r.left).max(1.0)).clamp(0.0, 1.0) * 2.0 - 1.0)
    }

    /// How far along the seek bar a point is, from 0 to 1.
    pub fn seek_fraction(&self, x: f32) -> f32 {
        self.seek_track.map_or(0.0, |r| ((x - r.left) / (r.right - r.left).max(1.0)).clamp(0.0, 1.0))
    }

    /// Where in the find box's text a point across it is, in UTF-16 units.
    pub fn field_position(&self, x: f32) -> Option<u32> {
        let (layout, ox) = self.field.as_ref()?;
        let (mut trailing, mut inside) = (windows::core::BOOL(0), windows::core::BOOL(0));
        let mut m = windows::Win32::Graphics::DirectWrite::DWRITE_HIT_TEST_METRICS::default();
        unsafe { layout.HitTestPoint(x - ox, 1.0, &mut trailing, &mut inside, &mut m) }.ok()?;
        Some(m.textPosition + if trailing.as_bool() { m.length } else { 0 })
    }

    /// Whether interface text that can be selected is under a point, where nothing else is.
    pub fn over_text(&self, x: f32, y: f32) -> bool {
        matches!(self.hit(x, y), None | Some(Hit::Panel | Hit::Modal)) && self.pick.over(x, y)
    }

    pub fn scroll_panel(&mut self, dy: f32) {
        let view = self.panel.map_or(0.0, |r| r.bottom - r.top);
        self.panel_scroll = (self.panel_scroll + dy).clamp(0.0, (self.panel_content - view).max(0.0));
    }
}

/// The room kept under the video for the trimming controls, in DIPs.
pub const TRIM_ROOM: f32 = 186.0;

struct Ctx<'a> {
    text: &'a Text,
    s: f32,
}

impl Ctx<'_> {
    fn px(&self, dip: f32) -> f32 {
        (dip * self.s).round()
    }
}

/// Draw everything for one frame.
pub fn draw(app: &mut App, gpu: &mut Gpu) {
    if app.peek.is_some() {
        return draw_peek(app, gpu);
    }
    if app.editor.is_some() {
        return screens::draw_editor(app, gpu);
    }
    if app.grid.is_some() {
        return screens::draw_grid(app, gpu);
    }
    app.ui.solid = false;
    app.ui.edit_frame = None;
    let p = app.palette;
    let (aw, ah) = app.area();
    let (cw, ch) = app.client_size();
    let scale = app.scale();
    app.ui.targets.clear();
    app.ui.pick.begin();
    // Trimming, the controls stay up.
    let trimming = app.trim.is_some();
    app.ui.solid = trimming;
    let alpha = if trimming { 1.0 } else if app.ui.hidden { 0.0 } else { app.ui.alpha() };
    let empty = app.shown.as_ref().is_none_or(|s| s.picture.is_none());

    // Glass needs the picture drawn somewhere it can be read back and blurred.
    let glass = alpha > 0.01 || app.toast.is_some() || empty || app.settings.info_panel;
    let scene = glass && gpu.begin_scene();
    gpu.clear(p.backdrop.d2d(1.0));

    // The image, fading in over the one before; or the one before while this one decodes. It's
    // laid out in the area left of the information card, but can slide under it when zoomed.
    gpu.clip(rect(0.0, 0.0, cw, ch));
    let mut drew = false;
    let view = app.current_view();
    let orient = app.orient();
    if app.player().is_some() {
        drew = draw_media(app, gpu, aw, ah, view, false);
    } else if let Some(s) = &app.shown {
        if let (Some(t), Some(pic)) = (&s.texture, &s.picture) {
            let f = app.crossfade();
            if f < 1.0 {
                if let Some((ot, op, ov, oo)) = &s.fading {
                    draw_picture(app, gpu, ot, op, *ov, *oo, 1.0 - f * f);
                }
            }
            draw_picture(app, gpu, t, pic, view, orient, 1.0 - (1.0 - f).powi(3));
            draw_words(app, gpu);
            drew = true;
        } else if let Some((t, pic, v, o)) = &s.old {
            draw_picture(app, gpu, t, pic, *v, *o, 1.0);
            drew = true;
        }
    }
    // Soft shades behind the name at the top and the toolbar at the bottom. They go in with
    // the picture so the glass blurs them too, which keeps the bars calm on busy photos.
    if alpha > 0.01 && app.shown.is_some() && app.player().is_none_or(|p| p.has_video()) {
        shades(gpu, &p, cw, ch, scale, alpha, app.folder.current().is_some(), true);
    }
    gpu.unclip();
    screen_switch(app, gpu, cw, ch);
    if scene {
        gpu.end_scene();
    }

    let text = &app.text;
    let cx = Ctx { text, s: scale };
    if !drew {
        match &app.shown {
            None => empty_state(app, gpu, aw, ah),
            Some(s) => {
                let name = app.folder.current().map(|e| e.name.clone()).unwrap_or_default();
                match &s.error {
                    Some(e) => message(&cx, gpu, &mut app.ui.pick, &p, aw, ah, &name, e),
                    None if s.since.elapsed() >= Duration::from_millis(250) => message(&cx, gpu, &mut app.ui.pick, &p, aw, ah, &name, "Loading…"),
                    None => {}
                }
            }
        }
    }

    if app.settings.info_panel {
        info_panel(app, gpu, cw, ch);
    } else {
        app.ui.panel = None;
    }

    if trimming {
        let mid = scale * TITLE / 2.0;
        title(app, gpu, aw, mid, 1.0);
        screens::trim_bar(app, gpu, aw, ch);
    } else if alpha > 0.01 {
        let mid = scale * TITLE / 2.0;
        title(app, gpu, aw, mid, alpha);
        screens::filmstrip(app, gpu, aw, ah, alpha);
        toolbar(app, gpu, aw, ah, alpha);
        arrows(app, gpu, aw, ah, alpha);
    }
    caption_buttons(app, gpu, cw, alpha);
    if let Some((msg, _)) = &app.toast {
        let msg = msg.clone();
        toast(app, gpu, aw, &msg);
    }
    app.ui.modal = app.dialog.is_some();
    if app.dialog.is_some() {
        dialog(app, gpu, cw, ch);
    }
}

// ---- The Space preview ----

/// The preview: the picture filling the window (which is sized to it), or a card for files
/// that aren't pictures. The name and buttons come up while the mouse is over it.
fn draw_peek(app: &mut App, gpu: &mut Gpu) {
    let p = app.palette;
    let (cw, ch) = app.client_size();
    let s = app.scale();
    app.ui.targets.clear();
    app.ui.pick.begin();
    app.ui.modal = false;
    app.ui.panel = None;
    app.ui.solid = false;
    app.ui.edit_frame = None;
    let alpha = app.ui.alpha();
    let has_card = app.peek.as_ref().is_some_and(|k| k.card.is_some());
    let has_doc = app.peek.as_ref().is_some_and(|k| k.doc.is_some());
    let is_pdf = app.peek.as_ref().and_then(|k| k.doc.as_ref()).is_some_and(|d| d.is_pdf());
    let media = app.player().is_some();
    let audio = app.player().is_some_and(|pl| pl.ready && !pl.has_video());
    let has_model = app.peek.as_ref().is_some_and(|k| k.model.is_some());
    let has_font = app.peek.as_ref().is_some_and(|k| k.font.is_some());
    // Pages, players and models are drawn every frame, so they always go through the scene.
    let scene = (alpha > 0.01 || has_doc || media || has_model) && gpu.begin_scene();
    let paper = if has_card || audio || has_model || has_font || (has_doc && !is_pdf) { p.panel } else { p.backdrop };
    gpu.clear(paper.d2d(1.0));
    let mut drew = has_card;
    let mut failure: Option<String> = None;
    if has_card {
        card(app, gpu, cw, ch);
    } else if has_font {
        failure = screens::font_preview(app, gpu, cw, ch);
        drew = failure.is_none();
    } else if let Some(model) = app.peek.as_ref().and_then(|k| k.model.as_ref()) {
        // A soft floor of light under the model, darker towards the bottom.
        gpu.fill_vgradient(rect(0.0, 0.0, cw, ch), p.panel.d2d(1.0), p.backdrop.d2d(1.0));
        if let Some(e) = model.failed() {
            failure = Some(e.to_string());
        } else if let Some(b) = model.bitmap() {
            gpu.draw_bitmap(b, rect(0.0, 0.0, cw, ch), D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, 1.0);
            drew = true;
        }
    } else if has_doc {
        let text = &app.text;
        if let Some(doc) = app.peek.as_mut().and_then(|k| k.doc.as_mut()) {
            match doc.failed() {
                Some(e) => failure = Some(e.to_string()),
                None => {
                    doc.draw(gpu, text, &p, rect(0.0, 0.0, cw, ch), s, alpha);
                    drew = true;
                }
            }
        }
    } else if media {
        drew = draw_media(app, gpu, cw, ch, app.current_view(), true);
    } else if let Some(sh) = &app.shown {
        if let (Some(t), Some(pic)) = (&sh.texture, &sh.picture) {
            draw_picture(app, gpu, t, pic, app.current_view(), app.orient(), 1.0);
            draw_words(app, gpu);
            drew = true;
        }
    }
    if alpha > 0.01 && !has_card && !has_doc && !audio && !has_font {
        shades(gpu, &p, cw, ch, s, alpha, true, media);
    }
    if scene {
        gpu.end_scene();
    }
    if !drew {
        let cx = Ctx { text: &app.text, s };
        let name = app.folder.current().map(|e| e.name.clone()).unwrap_or_default();
        let error = failure.or_else(|| app.shown.as_ref().and_then(|x| x.error.clone())).or_else(|| app.player().and_then(|pl| pl.error.clone()));
        match error {
            Some(e) => message(&cx, gpu, &mut app.ui.pick, &p, cw, ch, &name, &e),
            None => message(&cx, gpu, &mut app.ui.pick, &p, cw, ch, &name, "Loading…"),
        }
    }
    if let Some((msg, _)) = &app.toast {
        let msg = msg.clone();
        toast(app, gpu, cw, &msg);
    }
    find_bar(app, gpu, cw, ch);
    if alpha < 0.01 {
        return;
    }
    // What the model is: triangles and size, under it.
    if let Some(info) = app.peek.as_ref().and_then(|k| k.model.as_ref()).and_then(|m| m.info()).map(str::to_string) {
        let cx = Ctx { text: &app.text, s };
        let w = (measure(&cx, &info, 12.0, 500) + cx.px(32.0)).round().min(cw - cx.px(24.0));
        let h = cx.px(30.0);
        let r = rect(((cw - w) / 2.0).round(), ch - h - cx.px(14.0), w, h);
        glass_box(gpu, &p, r, h / 2.0, s, alpha);
        let cx = Ctx { text: &app.text, s };
        label(&cx, gpu, &info, r, 12.0, 500, Align::Center, p.text, alpha);
    }
    // A card, a song or a font shows its own name.
    if !has_card && !audio && !has_font {
        let mid = (s * 25.0).round();
        title(app, gpu, cw, mid, alpha);
    }
    if app.player().is_some_and(|pl| pl.error.is_none()) {
        let m = (12.0 * s).round();
        let height = (48.0 * s).round();
        let bar = rect(m, ch - m - height, cw - m * 2.0, height);
        media_bar(app, gpu, bar, alpha, false);
    }
    // Close, and open for real, in the top corner.
    let cx = Ctx { text: &app.text, s };
    let d = cx.px(30.0);
    let m = cx.px(10.0);
    let close = rect(cw - m - d, m, d, d);
    glass_box(gpu, &p, close, d / 2.0, s, alpha);
    if app.ui.hover == Some(Hit::CloseWindow) {
        gpu.fill_circle(close.left + d / 2.0, close.top + d / 2.0, d / 2.0, p.hover.d2d(p.hover_alpha * 1.6 * alpha));
    }
    let cx = Ctx { text: &app.text, s };
    icon(&cx, gpu, '\u{E8BB}', close, 9.0, p.text, alpha, false);
    app.ui.targets.push((close, Hit::CloseWindow));

    let text = if has_card || has_doc || has_model || has_font { "Open" } else { "Open in View" };
    let w = (measure(&cx, text, 12.5, 600) + cx.px(28.0)).round();
    let open = rect(close.left - cx.px(8.0) - w, m, w, d);
    let hit = Hit::Button(Cmd::OpenInView);
    let a = if app.ui.pressed == Some(hit) { 0.8 } else if app.ui.hover == Some(hit) { 0.92 } else { 1.0 };
    gpu.fill_round(open, d / 2.0, p.accent.d2d(a * alpha));
    label(&cx, gpu, text, open, 12.5, 600, Align::Center, p.accent.ink(), alpha);
    app.ui.targets.push((open, hit));

    // A file with two views: laid out to read, or the text it's written in. Two halves of one
    // pill, the one showing filled in.
    let mut left_edge = open.left;
    if let Some(as_text) = app.peek.as_ref().and_then(|k| k.doc.as_ref()).and_then(|d| d.shows_text()) {
        let halves = [("Visual", false), ("Text", true)];
        let widths: Vec<f32> = halves.iter().map(|(t, _)| (measure(&cx, t, 12.5, 600) + cx.px(24.0)).round()).collect();
        let w: f32 = widths.iter().sum();
        let pill = rect(open.left - cx.px(8.0) - w, m, w, d);
        glass_box(gpu, &p, pill, d / 2.0, s, alpha);
        let cx = Ctx { text: &app.text, s };
        let mut x = pill.left;
        for ((text, is_text), hw) in halves.iter().zip(&widths) {
            let half = rect(x, m, *hw, d);
            let hit = Hit::Button(Cmd::ShowText(*is_text));
            let inset = cx.px(3.0);
            let inner = rect(half.left + inset, half.top + inset, hw - inset * 2.0, d - inset * 2.0);
            if *is_text == as_text {
                gpu.fill_round(inner, (d - inset * 2.0) / 2.0, p.text.d2d(0.16 * alpha));
            } else if app.ui.hover == Some(hit) || app.ui.pressed == Some(hit) {
                gpu.fill_round(inner, (d - inset * 2.0) / 2.0, p.hover.d2d(p.hover_alpha * 1.4 * alpha));
            }
            label(&cx, gpu, text, half, 12.5, 600, Align::Center, if *is_text == as_text { p.text } else { p.dim }, alpha);
            app.ui.targets.push((half, hit));
            x += hw;
        }
        left_edge = pill.left;
    }
    let cx = Ctx { text: &app.text, s };

    // With text selected: copy it. Ctrl+C does the same.
    if app.peek.as_ref().and_then(|k| k.doc.as_ref()).is_some_and(|d| d.has_selection()) || app.ui.pick.has_selection() {
        let text = "Copy";
        let w = (measure(&cx, text, 12.5, 600) + cx.px(28.0)).round();
        let copy = rect(left_edge - cx.px(8.0) - w, m, w, d);
        let hit = Hit::Button(Cmd::CopyText);
        glass_box(gpu, &p, copy, d / 2.0, s, alpha);
        let cx = Ctx { text: &app.text, s };
        if app.ui.hover == Some(hit) || app.ui.pressed == Some(hit) {
            gpu.fill_round(copy, d / 2.0, p.hover.d2d(p.hover_alpha * if app.ui.pressed == Some(hit) { 2.2 } else { 1.6 } * alpha));
        }
        label(&cx, gpu, text, copy, 12.5, 600, Align::Center, p.text, alpha);
        app.ui.targets.push((copy, hit));
    }
}

/// The find box (Ctrl+F) or the page box (Ctrl+G), at the bottom of a document in the preview.
/// It stays up while the mouse is away, since the keys go to it.
fn find_bar(app: &mut App, gpu: &mut Gpu, cw: f32, ch: f32) {
    use crate::app::BarKind;
    app.ui.field = None;
    let Some((kind, field, scroll)) = app.peek.as_ref().and_then(|k| k.bar.as_ref()).map(|b| (b.kind, b.field.clone(), b.scroll)) else { return };
    let typed = field.text();
    let doc = app.peek.as_ref().and_then(|k| k.doc.as_ref());
    let count = doc.and_then(|d| d.find_count());
    let pages = doc.and_then(|d| d.page_count()).unwrap_or(0);
    let s = app.scale();
    let p = app.palette;
    let cx = Ctx { text: &app.text, s };
    let h = cx.px(38.0);
    let w = cx.px(if kind == BarKind::Find { 380.0 } else { 260.0 }).min(cw - cx.px(24.0)).round();
    let r = rect(((cw - w) / 2.0).round(), (ch - h - cx.px(14.0)).round(), w, h);
    glass_box(gpu, &p, r, h / 2.0, s, 1.0);
    let b = cx.px(30.0);
    let by = r.top + (h - b) / 2.0;
    let mut right = r.right - cx.px(4.0);
    let buttons: Vec<(Cmd, char)> = match kind {
        BarKind::Find => vec![(Cmd::CloseBar, '\u{E8BB}'), (Cmd::FindNext, '\u{E70D}'), (Cmd::FindPrev, '\u{E70E}')],
        BarKind::Page => vec![(Cmd::CloseBar, '\u{E8BB}')],
    };
    for (cmd, glyph) in buttons {
        let br = rect(right - b, by, b, b);
        let hit = Hit::Button(cmd);
        hover_fill(gpu, &app.ui, &p, br, hit, b / 2.0, 1.0, false);
        let enabled = cmd == Cmd::CloseBar || count.is_some_and(|(_, n, _)| n > 0);
        icon(&cx, gpu, glyph, br, if cmd == Cmd::CloseBar { 8.5 } else { 10.0 }, p.text, if enabled { 1.0 } else { 0.35 }, false);
        app.ui.targets.push((br, hit));
        right -= b;
    }
    let status = match (kind, count) {
        (BarKind::Page, _) => format!("of {pages}"),
        (BarKind::Find, _) if typed.trim().is_empty() => String::new(),
        (BarKind::Find, Some((_, 0, true))) => "Looking…".into(),
        (BarKind::Find, Some((_, 0, false))) => "Not found".into(),
        (BarKind::Find, Some((at, n, more))) => {
            let n = if more { format!("{n}+") } else { n.to_string() };
            match at {
                Some(i) => format!("{i} of {n}"),
                None => format!("{n} found"),
            }
        }
        _ => String::new(),
    };
    let gap = cx.px(10.0);
    let sw = if status.is_empty() { 0.0 } else { measure(&cx, &status, 12.0, 500) + gap };
    let mut left = r.left + cx.px(16.0);
    let lead = if kind == BarKind::Find { None } else { Some("Go to page") };
    if kind == BarKind::Find {
        icon(&cx, gpu, '\u{E721}', rect(left, r.top, cx.px(14.0), h), 11.0, p.dim, 1.0, false);
        left += cx.px(24.0);
    }
    if let Some(lead) = lead {
        let lw = measure(&cx, lead, 13.0, 500);
        label(&cx, gpu, lead, rect(left, r.top, lw + cx.px(2.0), h), 13.0, 500, Align::Left, p.dim, 1.0);
        left += lw + gap;
    }
    let room = (right - gap - sw - left).max(cx.px(20.0));
    // The text box: the text, scrolled sideways to keep the caret in it, with the selection
    // under it. A click or a drag in it places the caret or selects.
    let area = rect(left, r.top, room, h);
    app.ui.targets.push((area, Hit::Field));
    let layout = cx.text.format(cx.px(13.0), 500, Align::Left, false).and_then(|f| cx.text.layout(&typed, &f, 100_000.0, h));
    let (full, th) = layout.as_ref().map_or((0.0, 0.0), Text::size);
    let at = |units: u32| {
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut m = windows::Win32::Graphics::DirectWrite::DWRITE_HIT_TEST_METRICS::default();
        if let Some(l) = &layout {
            unsafe {
                let _ = l.HitTestTextPosition(units, false, &mut x, &mut y, &mut m);
            }
        }
        x
    };
    let caret = at(field.units(field.caret));
    let edge = cx.px(3.0);
    let mut scroll = scroll;
    if caret - scroll > room - edge {
        scroll = caret - room + edge;
    }
    if caret - scroll < 0.0 {
        scroll = caret;
    }
    scroll = scroll.clamp(0.0, (full - room + edge).max(0.0));
    if let Some(bar) = app.peek.as_mut().and_then(|k| k.bar.as_mut()) {
        bar.scroll = scroll;
    }
    let (ox, oy) = (left - scroll, (r.top + (h - th) / 2.0).round());
    let tw = (full - scroll).clamp(0.0, room);
    gpu.clip(area);
    match &layout {
        _ if typed.is_empty() => {
            let hint = if kind == BarKind::Find { "Find in this file" } else { "" };
            label(&cx, gpu, hint, area, 13.0, 500, Align::Left, p.dim, 0.7);
        }
        Some(l) => {
            if field.has_selection() {
                let (a, b) = field.range();
                for m in crate::doc::range_rects(l, field.units(a), field.units(b)) {
                    gpu.fill(rect(ox + m.left, oy + m.top, m.width, m.height), highlight(&p));
                }
            }
            gpu.text(l, ox, oy, p.text.d2d(1.0));
        }
        None => {}
    }
    if !field.has_selection() {
        let caret_h = cx.px(18.0);
        gpu.fill(rect((ox + caret).round().min(area.right - cx.px(2.0)), r.top + (h - caret_h) / 2.0, cx.px(1.5).max(1.0), caret_h), p.accent.d2d(1.0));
    }
    gpu.unclip();
    if let Some(l) = layout {
        app.ui.field = Some((l, ox));
    }
    if !status.is_empty() {
        let x = if kind == BarKind::Page { (left + tw + gap).min(right - gap - sw + gap) } else { right - sw };
        label(&cx, gpu, &status, rect(x, r.top, sw, h), 12.0, 500, Align::Left, p.dim, 1.0);
    }
}

/// A file that isn't a picture: the shell's thumbnail or icon, its name and a few facts.
fn card(app: &mut App, gpu: &mut Gpu, cw: f32, ch: f32) {
    let Some(c) = app.peek.as_ref().and_then(|k| k.card.as_ref()) else { return };
    let s = app.scale();
    let cx = Ctx { text: &app.text, s };
    let p = app.palette;
    // A folder with pictures in it shows some, and its own icon smaller.
    let pictures = screens::card_pictures_height(&cx, c.pictures.len(), cw);
    let side = cx.px(if pictures > 0.0 { 112.0 } else { 208.0 });
    let width = cw - cx.px(56.0);
    let name_h = paragraph_height(&cx, &c.name, width, 15.0, 600);
    let line_h = cx.px(20.0);
    let below = if pictures > 0.0 { cx.px(22.0) + pictures } else { 0.0 };
    let total = side + cx.px(24.0) + name_h + cx.px(8.0) + line_h * c.lines.len() as f32 + below;
    let top = ((ch - total) / 2.0).round();
    if let Some(t) = &c.texture {
        let k = (side / t.width as f32).min(side / t.height as f32).min(2.0 * s);
        let (w, h) = ((t.width as f32 * k).round(), (t.height as f32 * k).round());
        let r = rect(((cw - w) / 2.0).round(), top + ((side - h) / 2.0).round(), w, h);
        gpu.draw_texture(t, r, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, 1.0);
    }
    let mut y = top + side + cx.px(24.0);
    y += paragraph_sel(&cx, gpu, &mut app.ui.pick, &p, &c.name, cx.px(28.0), y, width, 15.0, 600, Align::Center, p.text, None, Join::Line) + cx.px(8.0);
    for line in &c.lines {
        label_sel(&cx, gpu, &mut app.ui.pick, &p, line, rect(cx.px(28.0), y, width, line_h), 12.5, 400, Align::Center, p.dim, 1.0, Join::Line);
        y += line_h;
    }
    if below > 0.0 {
        screens::card_pictures(&cx, gpu, &c.pictures, cw, y + cx.px(22.0));
    }
}

// ---- Dialogs ----

/// What a dialog is asking about.
pub enum Ask {
    /// Move this file to the Recycle Bin?
    Delete(std::path::PathBuf),
    /// Leave the editor, losing what's been changed?
    Discard,
    /// Just telling: one button to close it.
    Notice,
}

/// A question or notice drawn in View's own style, over the image. Windows' own dialogs can't
/// follow a dark theme.
pub struct Dialog {
    pub ask: Ask,
    pub title: String,
    pub body: String,
    /// A tick box under the text, and whether it's ticked.
    pub check: Option<(&'static str, bool)>,
    /// The first is the default, drawn in the accent colour.
    pub buttons: Vec<&'static str>,
    /// The button Enter presses.
    pub focus: usize,
    /// Tab or an arrow key moved the focus, so show where it is.
    pub keyed: bool,
}

impl Dialog {
    pub fn notice(title: impl Into<String>, body: impl Into<String>) -> Dialog {
        Dialog { ask: Ask::Notice, title: title.into(), body: body.into(), check: None, buttons: vec!["OK"], focus: 0, keyed: false }
    }
}

fn dialog(app: &mut App, gpu: &mut Gpu, cw: f32, ch: f32) {
    let s = app.scale();
    let cx = Ctx { text: &app.text, s };
    let p = app.palette;
    let Some(d) = &app.dialog else { return };
    gpu.fill(rect(0.0, 0.0, cw, ch), crate::gfx::color(0, 0, 0, if p.dark { 0.5 } else { 0.25 }));

    let pad = cx.px(24.0);
    let width = cx.px(440.0).min(cw - cx.px(32.0)).max(cx.px(200.0));
    let inner = width - pad * 2.0;
    let title_h = paragraph_height(&cx, &d.title, inner, 16.0, 600);
    let body_h = if d.body.is_empty() { 0.0 } else { paragraph_height(&cx, &d.body, inner, 13.0, 400) };
    let gap = cx.px(10.0);
    let check_h = if d.check.is_some() { cx.px(20.0) + cx.px(16.0) } else { 0.0 };
    let button_h = cx.px(32.0);
    let height = pad + title_h + if body_h > 0.0 { gap + body_h } else { 0.0 } + cx.px(20.0) + check_h + button_h + pad;
    let x0 = ((cw - width) / 2.0).round();
    let y0 = ((ch - height) / 2.0).round().max(cx.px(8.0));
    let box_r = rect(x0, y0, width, height);
    // Solid rather than glass: text over a busy image must stay legible.
    shadow(gpu, box_r, cx.px(14.0), s, 1.4);
    gpu.fill_round(box_r, cx.px(14.0), p.panel.d2d(1.0));
    gpu.stroke_round(box_r, cx.px(14.0), 1.0, p.edge.d2d(p.edge_alpha));

    let mut y = y0 + pad;
    y += paragraph_sel(&cx, gpu, &mut app.ui.pick, &p, &d.title, x0 + pad, y, inner, 16.0, 600, Align::Left, p.text, None, Join::Line);
    if body_h > 0.0 {
        y += gap;
        y += paragraph_sel(&cx, gpu, &mut app.ui.pick, &p, &d.body, x0 + pad, y, inner, 13.0, 400, Align::Left, p.dim, None, Join::Line);
    }
    y += cx.px(20.0);

    let mut targets = Vec::new();
    if let Some((label_text, ticked)) = d.check {
        let size = cx.px(18.0);
        let bx = rect(x0 + pad, y + cx.px(1.0), size, size);
        let hovered = app.ui.hover == Some(Hit::DialogCheck);
        if ticked {
            gpu.fill_round(bx, cx.px(4.0), p.accent.d2d(1.0));
            icon(&cx, gpu, '\u{E73E}', bx, 12.0, p.accent.ink(), 1.0, false);
        } else {
            gpu.fill_round(bx, cx.px(4.0), p.hover.d2d(p.hover_alpha * if hovered { 1.6 } else { 0.6 }));
            gpu.stroke_round(bx, cx.px(4.0), 1.0, p.dim.d2d(0.8));
        }
        let tw = measure(&cx, label_text, 13.0, 400);
        let lr = rect(bx.right + cx.px(10.0), y, tw + cx.px(4.0), cx.px(20.0));
        label(&cx, gpu, label_text, lr, 13.0, 400, Align::Left, p.text, 1.0);
        targets.push((rect(bx.left, y - cx.px(4.0), lr.right - bx.left, cx.px(28.0)), Hit::DialogCheck));
        y += check_h;
    }

    // Buttons from the right: the default (first) furthest right, as Windows places OK.
    let mut x = x0 + width - pad;
    for (i, text) in d.buttons.iter().enumerate() {
        let w = (measure(&cx, text, 13.0, 500) + cx.px(32.0)).max(cx.px(88.0)).round();
        x -= w;
        let r = rect(x, y, w, button_h);
        let hit = Hit::DialogButton(i as u8);
        let hovered = app.ui.hover == Some(hit);
        let pressed = app.ui.pressed == Some(hit);
        if i == 0 {
            let a = if pressed { 0.8 } else if hovered { 0.9 } else { 1.0 };
            gpu.fill_round(r, button_h / 2.0, p.accent.d2d(a));
            label(&cx, gpu, text, r, 13.0, 500, Align::Center, p.accent.ink(), 1.0);
        } else {
            let a = if pressed { 1.8 } else if hovered { 1.3 } else { 0.7 };
            gpu.fill_round(r, button_h / 2.0, p.hover.d2d(p.hover_alpha * a));
            gpu.stroke_round(r, button_h / 2.0, 1.0, p.edge.d2d(p.edge_alpha * 1.5));
            label(&cx, gpu, text, r, 13.0, 500, Align::Center, p.text, 1.0);
        }
        if d.keyed && d.focus == i {
            let f = cx.px(3.0);
            gpu.stroke_round(rect(r.left - f, r.top - f, w + f * 2.0, button_h + f * 2.0), button_h / 2.0 + f, 2.0 * s.max(1.0), p.text.d2d(0.85));
        }
        targets.push((r, hit));
        x -= cx.px(8.0);
    }
    app.ui.targets.extend(targets);
}

fn paragraph_height(cx: &Ctx, s: &str, width: f32, dip: f32, weight: u32) -> f32 {
    let Some(f) = cx.text.format(cx.px(dip), weight, Align::Left, true) else { return 0.0 };
    cx.text.layout(s, &f, width, 10_000.0).map_or(0.0, |l| Text::size(&l).1)
}

fn quarter_turn(turns: u8) -> Matrix3x2 {
    let (c, s) = match turns % 4 {
        0 => (1.0, 0.0),
        1 => (0.0, 1.0),
        2 => (-1.0, 0.0),
        _ => (0.0, -1.0),
    };
    Matrix3x2 { M11: c, M12: s, M21: -s, M22: c, M31: 0.0, M32: 0.0 }
}

/// The image's rectangle on screen: left, top, width, height.
pub fn image_rect(pic: &Picture, view: View, orient: Orient) -> (f32, f32, f32, f32) {
    let (sw, sh) = orient.shown_size(pic.width, pic.height);
    let (w, h) = (sw as f64 * view.scale, sh as f64 * view.scale);
    let (mut x, mut y) = (view.cx - w / 2.0, view.cy - h / 2.0);
    if (view.scale - view.scale.round()).abs() < 1e-9 {
        x = x.round();
        y = y.round();
    }
    (x as f32, y as f32, w as f32, h as f32)
}

fn draw_picture(app: &App, gpu: &mut Gpu, t: &Texture, pic: &Picture, view: View, orient: Orient, opacity: f32) {
    if opacity <= 0.0 {
        return;
    }
    let (x, y, w, h) = image_rect(pic, view, orient);
    if pic.has_alpha {
        let r = rect(x, y, w, h);
        let p = &app.palette;
        match app.settings.transparency {
            Transparency::Checkerboard => {
                let (a, b) = p.checker;
                let cell = (8.0 * app.scale()).round() as u32;
                gpu.checkerboard(r, cell, (a.0, a.1, a.2), (b.0, b.1, b.2), opacity);
            }
            Transparency::White => gpu.fill(r, Rgb(255, 255, 255).d2d(opacity)),
            Transparency::Black => gpu.fill(r, Rgb(0, 0, 0).d2d(opacity)),
            Transparency::Backdrop => {}
        }
    }
    let (fw, fh) = (pic.width as f32, pic.height as f32);
    let flip = if orient.flip { Matrix3x2 { M11: -1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 } } else { Matrix3x2::identity() };
    let s = view.scale as f32;
    let m = flip * quarter_turn(orient.turns) * Matrix3x2::scale(s, s) * Matrix3x2::translation(x + w / 2.0, y + h / 2.0);
    // Screen pixels for each bitmap pixel decides the filter: exact at 1:1, smooth when shrinking,
    // and square pixels when zoomed far in (if the user likes that).
    let k = view.scale * pic.bitmap_scale();
    let mode = if (k - 1.0).abs() < 1e-3 && orient.turns % 2 == 0 {
        D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
    } else if k >= 3.0 && app.settings.pixelated_zoom {
        D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
    } else {
        D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC
    };
    gpu.set_transform(&m);
    gpu.draw_texture(t, rect(-fw / 2.0, -fh / 2.0, fw, fh), mode, opacity);
    gpu.reset_transform();
}

// ---- Text helpers ----

fn label(cx: &Ctx, gpu: &Gpu, s: &str, r: D2D_RECT_F, dip: f32, weight: u32, align: Align, c: Rgb, a: f32) -> f32 {
    let Some(f) = cx.text.format(cx.px(dip), weight, align, false) else { return 0.0 };
    let Some(l) = cx.text.layout(s, &f, r.right - r.left, r.bottom - r.top) else { return 0.0 };
    let (_, th) = Text::size(&l);
    gpu.text(&l, r.left, r.top + ((r.bottom - r.top) - th) / 2.0, c.d2d(a));
    th
}


/// Selected interface text: the accent, as in documents.
fn highlight(p: &Palette) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
    p.accent.d2d(if p.dark { 0.42 } else { 0.28 })
}

/// Paint selected interface text's highlight, inside `clip`.
fn mark(gpu: &Gpu, p: &Palette, clip: Option<D2D_RECT_F>, marks: Vec<D2D_RECT_F>) {
    if marks.is_empty() {
        return;
    }
    if let Some(c) = clip {
        gpu.clip(c);
    }
    for r in marks {
        gpu.fill(r, highlight(p));
    }
    if clip.is_some() {
        gpu.unclip();
    }
}

/// [`label`], as text that can be selected and copied.
fn label_sel(cx: &Ctx, gpu: &Gpu, pick: &mut Picker, p: &Palette, s: &str, r: D2D_RECT_F, dip: f32, weight: u32, align: Align, c: Rgb, a: f32, join: Join) -> f32 {
    let Some(f) = cx.text.format(cx.px(dip), weight, align, false) else { return 0.0 };
    let Some(l) = cx.text.layout(s, &f, r.right - r.left, r.bottom - r.top) else { return 0.0 };
    let (_, th) = Text::size(&l);
    let y = r.top + ((r.bottom - r.top) - th) / 2.0;
    mark(gpu, p, None, pick.add(&l, r.left, y, s, None, join));
    gpu.text(&l, r.left, y, c.d2d(a));
    th
}

/// Wrapped text from `x`, `y`, as text that can be selected and copied. Returns its height.
/// `clip` is what can be seen of it, in something that scrolls.
fn paragraph_sel(cx: &Ctx, gpu: &Gpu, pick: &mut Picker, p: &Palette, s: &str, x: f32, y: f32, width: f32, dip: f32, weight: u32, align: Align, c: Rgb, clip: Option<D2D_RECT_F>, join: Join) -> f32 {
    let Some(f) = cx.text.format(cx.px(dip), weight, align, true) else { return 0.0 };
    let Some(l) = cx.text.layout(s, &f, width, 10_000.0) else { return 0.0 };
    let (_, th) = Text::size(&l);
    mark(gpu, p, clip, pick.add(&l, x, y, s, clip, join));
    gpu.text(&l, x, y, c.d2d(1.0));
    th
}

fn measure(cx: &Ctx, s: &str, dip: f32, weight: u32) -> f32 {
    let Some(f) = cx.text.format(cx.px(dip), weight, Align::Left, false) else { return 0.0 };
    cx.text.layout(s, &f, 10_000.0, 1000.0).map_or(0.0, |l| Text::size(&l).0)
}

fn icon(cx: &Ctx, gpu: &Gpu, glyph: char, r: D2D_RECT_F, dip: f32, c: Rgb, a: f32, mirrored: bool) {
    let Some(f) = cx.text.icons(cx.px(dip)) else { return };
    let mut buf = [0u8; 4];
    let Some(l) = cx.text.layout(glyph.encode_utf8(&mut buf), &f, r.right - r.left, r.bottom - r.top) else { return };
    let (_, th) = Text::size(&l);
    let y = r.top + ((r.bottom - r.top) - th) / 2.0;
    if mirrored {
        let mid = (r.left + r.right) / 2.0;
        gpu.set_transform(&(Matrix3x2::translation(-mid, 0.0) * Matrix3x2::scale(-1.0, 1.0) * Matrix3x2::translation(mid, 0.0)));
        gpu.text(&l, r.left, y, c.d2d(a));
        gpu.reset_transform();
    } else {
        gpu.text(&l, r.left, y, c.d2d(a));
    }
}

/// A soft shadow under a floating shape.
fn shadow(gpu: &Gpu, r: D2D_RECT_F, radius: f32, s: f32, alpha: f32) {
    for i in 1..=4 {
        let spread = i as f32 * 2.5 * s;
        let sh = D2D_RECT_F { left: r.left - spread, top: r.top - spread + 3.0 * s, right: r.right + spread, bottom: r.bottom + spread + 3.0 * s };
        gpu.fill_round(sh, radius + spread, crate::gfx::color(0, 0, 0, (0.06 - i as f32 * 0.012) * alpha));
    }
}

/// Frosted glass: what's behind, blurred and tinted, with a hairline edge and a faint shadow.
fn glass_box(gpu: &mut Gpu, p: &Palette, r: D2D_RECT_F, radius: f32, s: f32, alpha: f32) {
    shadow(gpu, r, radius, s, alpha);
    gpu.glass(r, radius, BLUR * s, alpha);
    gpu.fill_round(r, radius, p.panel.d2d(p.glass * alpha));
    gpu.stroke_round(r, radius, 1.0, p.edge.d2d(p.edge_alpha * 1.2 * alpha));
}

// ---- The top: name and window buttons ----

/// A shade's solid and clear ends: black in the dark theme, white (and stronger, since white
/// hides less) in the light one. `strength` is for the dark theme.
fn shade_colours(p: &Palette, strength: f32) -> (windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F, windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F) {
    if p.dark {
        (crate::gfx::color(0, 0, 0, strength), crate::gfx::color(0, 0, 0, 0.0))
    } else {
        (crate::gfx::color(255, 255, 255, (strength * 1.5).min(1.0)), crate::gfx::color(255, 255, 255, 0.0))
    }
}

/// The shades along the top and bottom edges, darkening (or in the light theme, lightening)
/// the picture where the name and the toolbar sit.
fn shades(gpu: &Gpu, p: &Palette, w: f32, h: f32, s: f32, alpha: f32, top: bool, bottom: bool) {
    let (solid, clear) = shade_colours(p, 0.5 * alpha);
    if top {
        gpu.fill_vgradient(rect(0.0, 0.0, w, (96.0 * s).round()), solid, clear);
    }
    if bottom {
        let sh = (128.0 * s).round();
        gpu.fill_vgradient(rect(0.0, h - sh, w, sh), clear, solid);
    }
}

/// The file's name over the top of the picture, on a glass pill over a soft shade, so it reads
/// on any photo. `mid` is the pill's centre line, to line up with the buttons beside it.
fn title(app: &mut App, gpu: &mut Gpu, aw: f32, mid: f32, alpha: f32) {
    let s = app.scale();
    let cx = Ctx { text: &app.text, s };
    let p = app.palette;
    let Some(entry) = app.folder.current() else { return };
    let page = app.peek.as_ref().and_then(|k| k.doc.as_ref()).and_then(|d| d.page_label());
    let is_page = page.is_some();
    let n = app.folder.len();
    // The grid shows the folder's name, and how many there are.
    let name = match (&app.grid, entry.path.parent().and_then(|d| d.file_name())) {
        (Some(_), Some(dir)) => dir.to_string_lossy().into_owned(),
        _ => entry.name.clone(),
    };
    let counter = match page {
        Some(page) => page,
        None if app.grid.is_some() => if n == 1 { "1 file".into() } else { format!("{n} files") },
        None if app.editor.is_some() => "Editing".into(),
        None if app.trim.is_some() => "Trimming".into(),
        None if n > 1 => format!("{} of {}", app.folder.index + 1, n),
        None => String::new(),
    };
    let entry = &name;
    let gap = cx.px(10.0);
    let room = (aw - cx.px(CAPTION_W * 3.0 + 24.0) * 2.0).max(cx.px(120.0));
    let cw = if counter.is_empty() { 0.0 } else { measure(&cx, &counter, 12.0, 500) + gap };
    let nw = measure(&cx, entry, 13.0, 600).min(room - cw);
    let x = ((aw - nw - cw) / 2.0).round();
    let ph = cx.px(PILL);
    let top = (mid - ph / 2.0).round();
    let pad = cx.px(14.0);
    let pill = rect(x - pad, top, nw + cw + pad * 2.0, ph);
    glass_box(gpu, &p, pill, ph / 2.0, s, alpha);
    // In the main window it shows or hides the information card. It lights up under the mouse,
    // but looks the same whether the card is out or not.
    if app.peek.is_none() && app.grid.is_none() && app.editor.is_none() && app.trim.is_none() {
        hover_fill(gpu, &app.ui, &p, pill, Hit::Title, ph / 2.0, alpha, false);
        app.ui.targets.push((pill, Hit::Title));
    }
    label(&cx, gpu, entry, rect(x, top, nw + cx.px(2.0), ph), 13.0, 600, Align::Left, p.text, alpha);
    if !counter.is_empty() {
        let cr = rect(x + nw + gap, top, cw, ph);
        // A PDF's "Page 3 of 12" goes to another page when clicked, like Ctrl+G.
        let hit = Hit::Button(Cmd::GoToPage);
        let hovered = is_page && (app.ui.hover == Some(hit) || app.ui.pressed == Some(hit));
        label(&cx, gpu, &counter, cr, 12.0, 500, Align::Left, if hovered { p.text } else { p.soft() }, alpha);
        if is_page {
            app.ui.targets.push((rect(cr.left - gap / 2.0, top, cw, ph), hit));
        }
    }
}

/// Minimise, maximise and close, drawn by View since there's no title bar. In full screen:
/// back to a window, and close. They sit in a glass capsule so they read on any photo, but each
/// button's click area runs up to the top edge (and the close button's to the right edge), so
/// flinging the mouse into the corner still hits it.
fn caption_buttons(app: &mut App, gpu: &mut Gpu, cw: f32, alpha: f32) {
    let s = app.scale();
    let cx = Ctx { text: &app.text, s };
    let p = app.palette;
    let buttons: Vec<(Hit, char)> = if app.is_fullscreen() {
        vec![(Hit::CloseWindow, '\u{E8BB}'), (Hit::Button(Cmd::Fullscreen), '\u{E73F}')]
    } else {
        let max = if app.is_maximized() { '\u{E923}' } else { '\u{E922}' };
        vec![(Hit::CloseWindow, '\u{E8BB}'), (Hit::Maximize, max), (Hit::Minimize, '\u{E921}')]
    };
    let th = cx.px(TITLE);
    let (w, h, pad) = (cx.px(40.0), cx.px(PILL), cx.px(3.0));
    let inset = ((th - h) / 2.0).round();
    let n = buttons.len() as f32;
    let capsule = rect(cw - inset - pad * 2.0 - w * n, inset, w * n + pad * 2.0, h);
    if alpha > 0.01 {
        glass_box(gpu, &p, capsule, h / 2.0, s, alpha);
    }
    let mut x = capsule.right - pad - w;
    for (i, (hit, glyph)) in buttons.into_iter().enumerate() {
        let r = rect(x, inset, w, h);
        let hovered = app.ui.hover == Some(hit);
        let pressed = app.ui.pressed == Some(hit);
        let mut ink = p.text;
        let mut a = alpha;
        let fill = if hit == Hit::CloseWindow && (hovered || pressed) {
            ink = Rgb(255, 255, 255);
            Some(Rgb(196, 43, 28).d2d(if pressed { 0.8 } else { 1.0 }))
        } else if hovered || pressed {
            Some(p.hover.d2d(p.hover_alpha * if pressed { 1.8 } else { 1.2 }))
        } else {
            None
        };
        if let Some(c) = fill {
            a = 1.0;
            let k = cx.px(3.0);
            let hr = rect(r.left + k * 0.5, r.top + k, w - k, h - k * 2.0);
            gpu.fill_round(hr, (h - k * 2.0) / 2.0, c);
        }
        if a > 0.01 {
            icon(&cx, gpu, glyph, r, 10.0, ink, a, false);
        }
        // The click area: up to the top edge, and for the close button to the right edge too.
        let right = if i == 0 { cw } else { r.right };
        let left = if i as f32 == n - 1.0 { capsule.left } else { r.left };
        app.ui.targets.push((rect(left, 0.0, right - left, th.max(r.bottom)), hit));
        x -= w;
    }
}

// ---- The toolbar ----

enum Item {
    Button(Cmd, char, bool),
    Zoom,
    Gap,
}

// ---- Videos and music ----

/// A video's picture, or a song's cover and name. False if there's nothing to show (yet).
fn draw_media(app: &mut App, gpu: &mut Gpu, aw: f32, ah: f32, view: View, peek: bool) -> bool {
    let s = app.scale();
    let Some(player) = app.shown.as_mut().and_then(|x| x.player.as_mut()) else { return false };
    if player.error.is_some() {
        return false;
    }
    if !player.ready {
        return true; // a moment, while it opens: nothing rather than "Loading"
    }
    if let Some((w, h)) = player.size() {
        let Some(frame) = player.frame(gpu).cloned() else { return true };
        let (x, y, dw, dh) = fit_rect(w, h, view);
        let k = view.scale;
        let mode = if (k - 1.0).abs() < 1e-3 { D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR } else { D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC };
        gpu.draw_bitmap(&frame, rect(x, y, dw, dh), mode, 1.0);
        return true;
    }
    // A song: its cover, and what it is.
    let p = app.palette;
    let cx = Ctx { text: &app.text, s };
    let bar = cx.px(if peek { 76.0 } else { 96.0 });
    let side = (aw * 0.5).min(ah - bar - cx.px(150.0)).min(cx.px(340.0)).max(cx.px(96.0)).round();
    let total = side + cx.px(26.0) + cx.px(24.0) + cx.px(22.0);
    let top = (((ah - bar) - total) / 2.0).max(cx.px(if peek { 20.0 } else { 48.0 })).round();
    let art = rect(((aw - side) / 2.0).round(), top, side, side);
    let radius = cx.px(14.0);
    let name = app.folder.current().map(|e| e.name.clone()).unwrap_or_default();
    let (title, by) = match &app.art {
        Some(a) => {
            let title = a.tags.title.clone().unwrap_or_else(|| strip_extension(&name));
            let by = [a.tags.artist.clone(), a.tags.album.clone()].into_iter().flatten().collect::<Vec<_>>().join("  ·  ");
            (title, by)
        }
        None => (strip_extension(&name), String::new()),
    };
    shadow(gpu, art, radius, s, 1.6);
    match app.art.as_ref().and_then(|a| a.texture.as_ref()) {
        Some(t) => {
            // Square covers fill the square; others fit inside it.
            let k = (side / t.width as f32).min(side / t.height as f32);
            let (w, h) = ((t.width as f32 * k).round(), (t.height as f32 * k).round());
            let r = rect(((aw - w) / 2.0).round(), top + ((side - h) / 2.0).round(), w, h);
            gpu.draw_texture_round(t, r, radius, 1.0);
        }
        None => {
            gpu.fill_round(art, radius, p.panel.d2d(1.0));
            gpu.fill_round(art, radius, p.accent.d2d(0.10));
            gpu.stroke_round(art, radius, 1.0, p.edge.d2d(p.edge_alpha));
            icon(&cx, gpu, '\u{E8D6}', art, (side / s * 0.34).round(), p.accent, 0.9, false);
        }
    }
    let mut y = art.bottom + cx.px(26.0);
    let width = (aw - cx.px(48.0)).max(cx.px(100.0));
    label_sel(&cx, gpu, &mut app.ui.pick, &p, &title, rect(cx.px(24.0), y, width, cx.px(24.0)), 17.0, 600, Align::Center, p.text, 1.0, Join::Line);
    y += cx.px(28.0);
    if !by.is_empty() {
        label_sel(&cx, gpu, &mut app.ui.pick, &p, &by, rect(cx.px(24.0), y, width, cx.px(20.0)), 13.0, 400, Align::Center, p.dim, 1.0, Join::Line);
    }
    true
}

fn strip_extension(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => name[..i].to_string(),
        _ => name.to_string(),
    }
}

/// The playing controls: play or pause, the time, a bar to seek along, the length and the
/// sound. `trailing` adds the window's own tools after them.
fn media_bar(app: &mut App, gpu: &mut Gpu, bar: D2D_RECT_F, alpha: f32, trailing: bool) {
    let s = app.scale();
    let p = app.palette;
    let Some(player) = app.player() else { return };
    let (paused, ended, pos, duration) = (player.paused(), player.ended, player.position(), player.duration());
    let failed = player.error.is_some();
    let cx = Ctx { text: &app.text, s };
    let button = cx.px(36.0);
    let pad = ((bar.bottom - bar.top - button) / 2.0).round();
    glass_box(gpu, &p, bar, (bar.bottom - bar.top) / 2.0, s, alpha);
    app.ui.targets.push((bar, Hit::Toolbar));
    let y = bar.top + pad;
    let mut x = bar.left + pad;

    let play = rect(x, y, button, button);
    let hit = Hit::Button(Cmd::PlayPause);
    if !failed {
        hover_fill(gpu, &app.ui, &p, play, hit, button / 2.0, alpha, false);
        icon(&cx, gpu, if paused || ended { '\u{E768}' } else { '\u{E769}' }, play, 15.0, p.text, alpha, false);
        app.ui.targets.push((play, hit));
    }
    x += button + cx.px(4.0);

    // The tools at the end, measured first so the seek bar can take what's left.
    let mut tail: Vec<(Cmd, char)> = Vec::new();
    let vol_glyph = if app.settings.muted || app.settings.volume < 0.01 {
        '\u{E74F}'
    } else if app.settings.volume < 0.34 {
        '\u{E993}'
    } else if app.settings.volume < 0.67 {
        '\u{E994}'
    } else {
        '\u{E995}'
    };
    tail.push((Cmd::Mute, vol_glyph));
    let gap = cx.px(11.0);
    let trailing_items: Vec<Option<(Cmd, char)>> = if trailing {
        let fullscreen = app.is_fullscreen();
        let mut items = vec![None, Some((Cmd::Grid, '\u{E80A}')), Some((Cmd::Info, '\u{E946}')), Some((Cmd::Delete, '\u{E74D}')), None, Some((Cmd::Fullscreen, if fullscreen { '\u{E73F}' } else { '\u{E740}' })), Some((Cmd::Menu, '\u{E712}'))];
        // Videos can be trimmed: songs can't.
        if app.player().is_some_and(|pl| pl.has_video()) {
            items.insert(1, Some((Cmd::Edit, '\u{E8C6}')));
        }
        items
    } else {
        Vec::new()
    };
    // How fast it plays, as a button of its own after the sound's.
    let speed = crate::cmd::speed_label(app.speed);
    let speed_w = if failed { 0.0 } else { (measure(&cx, &speed, 12.0, 600) + cx.px(20.0)).max(button).round() };
    let tail_w = button * tail.len() as f32 + speed_w + trailing_items.iter().map(|i| if i.is_some() { button } else { gap }).sum::<f32>();
    let time_w = cx.px(if duration.is_some_and(|d| d >= 3600.0) { 62.0 } else { 46.0 });
    let tools = bar.right - pad - tail_w;
    // The volume slider slides out of the sound button's left side while the mouse is on
    // either, so the button stays put under the mouse and the seek bar makes room.
    let wanted = matches!(app.ui.hover, Some(Hit::Button(Cmd::Mute)) | Some(Hit::Volume)) || app.ui.pressed == Some(Hit::Volume);
    app.ui.slide_volume(wanted && !failed);
    let open = app.ui.volume_open();
    let slider_w = (cx.px(96.0) * open).round();
    let right = tools - slider_w;
    if slider_w > cx.px(20.0) {
        let a = alpha * open;
        let th = cx.px(4.0);
        let track = rect(right + cx.px(10.0), (y + button / 2.0 - th / 2.0).round(), slider_w - cx.px(16.0), th);
        let level = if app.settings.muted { 0.0 } else { app.settings.volume };
        gpu.fill_round(track, th / 2.0, p.text.d2d(0.22 * a));
        let done = rect(track.left, track.top, (track.right - track.left) * level, th);
        gpu.fill_round(done, th / 2.0, p.accent.d2d(a));
        let active = matches!(app.ui.hover, Some(Hit::Volume)) || app.ui.pressed == Some(Hit::Volume);
        gpu.fill_circle(done.right, track.top + th / 2.0, if active { cx.px(7.0) } else { cx.px(5.0) }, p.text.d2d(a));
        if open > 0.6 {
            app.ui.volume_track = Some(track);
            app.ui.targets.push((rect(right, y, slider_w, button), Hit::Volume));
        }
    } else if !wanted {
        app.ui.volume_track = None;
    }

    let now = media_clock(pos);
    label(&cx, gpu, &now, rect(x, y, time_w, button), 12.0, 500, Align::Center, p.text, alpha);
    x += time_w + cx.px(8.0);
    let track_right = right - time_w - cx.px(8.0);
    if track_right - x > cx.px(40.0) {
        let th = cx.px(4.0);
        let track = rect(x, (y + button / 2.0 - th / 2.0).round(), track_right - x, th);
        let f = duration.map_or(0.0, |d| (pos / d).clamp(0.0, 1.0)) as f32;
        let active = matches!(app.ui.hover, Some(Hit::Seek)) || app.ui.pressed == Some(Hit::Seek);
        gpu.fill_round(track, th / 2.0, p.text.d2d(0.22 * alpha));
        let done = rect(track.left, track.top, (track.right - track.left) * f, th);
        gpu.fill_round(done, th / 2.0, p.accent.d2d(alpha));
        let knob = if active { cx.px(7.0) } else { cx.px(5.0) };
        gpu.fill_circle(done.right, track.top + th / 2.0, knob, p.text.d2d(alpha));
        app.ui.seek_track = Some(track);
        if duration.is_some() && !failed {
            app.ui.targets.push((rect(track.left - cx.px(6.0), y, track.right - track.left + cx.px(12.0), button), Hit::Seek));
        }
        let length = duration.map_or_else(|| "–:––".to_string(), media_clock);
        label(&cx, gpu, &length, rect(track_right + cx.px(8.0), y, time_w, button), 12.0, 500, Align::Center, p.dim, alpha);
    } else {
        app.ui.seek_track = None;
    }

    let mut x = tools;
    let mut speed_button = None;
    for (cmd, glyph) in tail {
        let r = rect(x, y, button, button);
        let hit = Hit::Button(cmd);
        hover_fill(gpu, &app.ui, &p, r, hit, button / 2.0, alpha, false);
        icon(&cx, gpu, glyph, r, 15.0, p.text, alpha, false);
        app.ui.targets.push((r, hit));
        x += button;
    }
    if speed_w > 0.0 {
        let r = rect(x, y, speed_w, button);
        let hit = Hit::Button(Cmd::SpeedMenu);
        hover_fill(gpu, &app.ui, &p, r, hit, button / 2.0, alpha, app.ui.speeds_out());
        label(&cx, gpu, &speed, r, 12.0, 600, Align::Center, if app.speed == 1.0 { p.text } else { p.accent }, alpha);
        app.ui.targets.push((r, hit));
        speed_button = Some(r);
        x += speed_w;
    }
    for item in trailing_items {
        match item {
            Some((cmd, glyph)) => {
                let r = rect(x, y, button, button);
                let hit = Hit::Button(cmd);
                let active = cmd == Cmd::Info && app.settings.info_panel;
                hover_fill(gpu, &app.ui, &p, r, hit, button / 2.0, alpha, active);
                icon(&cx, gpu, glyph, r, 15.0, p.text, alpha, false);
                app.ui.targets.push((r, hit));
                x += button;
            }
            None => {
                let mx = x + gap / 2.0;
                gpu.fill(rect(mx.round(), y + cx.px(10.0), 1.0, button - cx.px(20.0)), p.edge.d2d(p.edge_alpha * 1.6 * alpha));
                x += gap;
            }
        }
    }
    if let Some(r) = speed_button {
        speeds(app, gpu, r, bar.top, alpha);
    }
}

/// The speeds to pick from, rising out of the speed button. The mark slides to the one chosen.
fn speeds(app: &mut App, gpu: &mut Gpu, button: D2D_RECT_F, bar_top: f32, alpha: f32) {
    use crate::cmd::{SPEEDS, speed_label};
    // Faded away with the rest of the controls, they're put away too.
    if alpha < 0.3 {
        let chosen = SPEEDS.iter().position(|&r| (r - app.speed).abs() < 1e-6).unwrap_or(3);
        app.ui.show_speeds(false, chosen);
    }
    let open = app.ui.speeds_open();
    if open < 0.01 {
        return;
    }
    let s = app.scale();
    let p = app.palette;
    let cx = Ctx { text: &app.text, s };
    let (row, pad, w) = (cx.px(30.0), cx.px(5.0), cx.px(84.0));
    let h = row * SPEEDS.len() as f32 + pad * 2.0;
    let a = alpha * open;
    // It rises the last few pixels into place as it comes in.
    let left = ((button.left + button.right) / 2.0 - w / 2.0).round();
    let top = (bar_top - cx.px(8.0) - h + (1.0 - open) * cx.px(10.0)).round();
    let panel = rect(left, top, w, h);
    glass_box(gpu, &p, panel, cx.px(14.0), s, a);
    let cx = Ctx { text: &app.text, s };
    if app.ui.speeds_out() {
        app.ui.targets.push((panel, Hit::Toolbar));
    }
    let mark = app.ui.speed_mark_row();
    let mark_box = rect(left + pad, top + pad + mark * row, w - pad * 2.0, row);
    gpu.fill_round(mark_box, cx.px(9.0), p.accent.d2d(a));
    for (i, &rate) in SPEEDS.iter().enumerate() {
        let r = rect(left + pad, top + pad + i as f32 * row, w - pad * 2.0, row);
        let hit = Hit::Button(Cmd::Speed(i as u8));
        // How much of this row the mark covers decides its ink, so the label changes colour as
        // the mark passes over it.
        let under = (1.0 - (mark - i as f32).abs()).clamp(0.0, 1.0);
        if under < 0.5 {
            hover_fill(gpu, &app.ui, &p, r, hit, cx.px(9.0), a, false);
        }
        let text = if rate == 1.0 { "Normal".to_string() } else { speed_label(rate) };
        label(&cx, gpu, &text, r, 12.5, if under >= 0.5 { 600 } else { 500 }, Align::Center, if under >= 0.5 { p.accent.ink() } else { p.text }, a);
        if app.ui.speeds_out() {
            app.ui.targets.push((r, hit));
        }
    }
}

fn media_clock(t: f64) -> String {
    crate::media::clock(t)
}

/// Where a picture `w`×`h` goes on screen for a view: left, top, width, height.
fn fit_rect(w: u32, h: u32, view: View) -> (f32, f32, f32, f32) {
    let (dw, dh) = (w as f64 * view.scale, h as f64 * view.scale);
    let (mut x, mut y) = (view.cx - dw / 2.0, view.cy - dh / 2.0);
    if (view.scale - view.scale.round()).abs() < 1e-9 {
        x = x.round();
        y = y.round();
    }
    (x as f32, y as f32, dw as f32, dh as f32)
}

fn toolbar(app: &mut App, gpu: &mut Gpu, aw: f32, ah: f32, alpha: f32) {
    let s = app.scale();
    let p = app.palette;
    if app.player().is_some_and(|pl| pl.error.is_none()) {
        let width = (aw - 40.0 * s).min(820.0 * s).max(300.0 * s).round();
        let height = (48.0 * s).round();
        let bar = rect(((aw - width) / 2.0).round(), (ah - height - 20.0 * s).round(), width, height);
        return media_bar(app, gpu, bar, alpha, true);
    }
    let has_image = app.shown.as_ref().is_some_and(|x| x.picture.is_some());
    let fullscreen = app.is_fullscreen();
    let mut items = Vec::new();
    if has_image {
        items.extend([
            Item::Button(Cmd::ZoomOut, '\u{E71F}', false),
            Item::Zoom,
            Item::Button(Cmd::ZoomIn, '\u{E8A3}', false),
            Item::Gap,
            Item::Button(Cmd::RotateCcw, '\u{E7AD}', true),
            Item::Button(Cmd::RotateCw, '\u{E7AD}', false),
            Item::Button(Cmd::Edit, '\u{E70F}', false),
            Item::Gap,
            Item::Button(Cmd::Grid, '\u{E80A}', false),
            Item::Button(Cmd::Info, '\u{E946}', false),
            Item::Button(Cmd::Delete, '\u{E74D}', false),
            Item::Gap,
            Item::Button(Cmd::Fullscreen, if fullscreen { '\u{E73F}' } else { '\u{E740}' }, false),
            Item::Button(Cmd::Menu, '\u{E712}', false),
        ]);
    } else if app.shown.is_some() {
        // A file that didn't open: still something to do with it.
        items.extend([Item::Button(Cmd::Open, '\u{E8E5}', false), Item::Button(Cmd::Menu, '\u{E712}', false)]);
    } else {
        return; // the empty window has its own button
    }

    let cx = Ctx { text: &app.text, s };
    let zoom = format!("{}%", (app.vp.view.scale * 100.0).round());
    let button = cx.px(36.0);
    let pad = cx.px(6.0);
    let widths: Vec<f32> = items
        .iter()
        .map(|i| match i {
            Item::Button(..) => button,
            Item::Zoom => cx.px(58.0),
            Item::Gap => cx.px(11.0),
        })
        .collect();
    let width: f32 = widths.iter().sum::<f32>() + pad * 2.0;
    let height = button + pad * 2.0;
    let x0 = ((aw - width) / 2.0).round();
    let y0 = (ah - height - cx.px(20.0)).round();
    let bar = rect(x0, y0, width, height);
    glass_box(gpu, &p, bar, height / 2.0, s, alpha);
    app.ui.targets.push((bar, Hit::Toolbar));

    let cx = Ctx { text: &app.text, s };
    let mut x = x0 + pad;
    for (item, w) in items.iter().zip(widths) {
        let r = rect(x, y0 + pad, w, button);
        match item {
            Item::Button(cmd, glyph, mirrored) => {
                let hit = Hit::Button(*cmd);
                let active = *cmd == Cmd::Info && app.settings.info_panel;
                hover_fill(gpu, &app.ui, &p, r, hit, button / 2.0, alpha, active);
                icon(&cx, gpu, *glyph, r, 15.0, p.text, alpha, *mirrored);
                app.ui.targets.push((r, hit));
            }
            Item::Zoom => {
                hover_fill(gpu, &app.ui, &p, r, Hit::Zoom, button / 2.0, alpha, false);
                label(&cx, gpu, &zoom, r, 12.5, 500, Align::Center, p.text, alpha);
                app.ui.targets.push((r, Hit::Zoom));
            }
            Item::Gap => {
                let mx = (r.left + r.right) / 2.0;
                gpu.fill(rect(mx.round(), r.top + cx.px(10.0), 1.0, button - cx.px(20.0)), p.edge.d2d(p.edge_alpha * 1.6 * alpha));
            }
        }
        x += w;
    }
}

fn hover_fill(gpu: &Gpu, ui: &State, p: &Palette, r: D2D_RECT_F, hit: Hit, radius: f32, alpha: f32, active: bool) {
    let pressed = ui.pressed == Some(hit);
    let hovered = ui.hover == Some(hit);
    // A button that's on stays filled in, clearly more than a hover, with a hairline round it.
    let want = if pressed { 2.6 } else if active && hovered { 2.4 } else if active { 2.0 } else if hovered { 1.4 } else { 0.0 };
    let (fill, on) = ui.glow(hit, want, if active { 1.0 } else { 0.0 });
    // The fill swells out from the middle as it comes up, and pulls in a touch while pressed.
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let grow = (fill / 1.4).min(1.0);
    let squeeze = if pressed { 0.06 } else { 0.0 };
    let inset = |k: f32| rect(r.left + w * k, r.top + h * k, w * (1.0 - 2.0 * k), h * (1.0 - 2.0 * k));
    if fill > 0.005 {
        let k = (1.0 - grow) * 0.14 + squeeze;
        gpu.fill_round(inset(k), (radius * (1.0 - 2.0 * k)).max(0.0), p.hover.d2d(p.hover_alpha * fill * alpha));
    }
    if on > 0.005 {
        // The ring grows into place as it turns on, and shrinks away as it turns off.
        let k = (1.0 - on.min(1.0)) * 0.2 - (on - 1.0).max(0.0) * 0.12;
        gpu.stroke_round(inset(k), (radius * (1.0 - 2.0 * k)).max(0.0), 1.0, p.hover.d2d(p.hover_alpha * 2.2 * on.min(1.0) * alpha));
    }
}

/// Just after going between the photo and the grid: what was there gives way to what's come,
/// growing and fading as it goes. Drawn into the scene, so the glass bars blur it too.
fn screen_switch(app: &App, gpu: &Gpu, cw: f32, ch: f32) {
    let Some((at, into_grid)) = app.screen_switch else { return };
    let t = at.elapsed().as_secs_f32() / SCREEN_SWITCH.as_secs_f32();
    if t >= 1.0 {
        return;
    }
    let e = ease_out(t);
    // Into the grid the photo shrinks back into it; out of it, the grid swells past you.
    let scale = if into_grid { 1.0 - 0.06 * e } else { 1.0 + 0.08 * e };
    if !gpu.draw_kept(1.0 - e, scale) {
        // Nothing kept from before: what's come fades up from the background instead.
        gpu.fill(rect(0.0, 0.0, cw, ch), app.palette.backdrop.d2d(1.0 - e));
    }
}

/// Previous and next, at the sides. Each comes up as the mouse nears its side, so a still
/// picture has nothing over it.
fn arrows(app: &mut App, gpu: &mut Gpu, aw: f32, ah: f32, alpha: f32) {
    if app.folder.len() < 2 || aw < 400.0 * app.scale() {
        return;
    }
    let s = app.scale();
    let p = app.palette;
    let d = (44.0 * s).round();
    let y = ((ah - d) / 2.0).round();
    let (mx, inside) = (app.mouse.0 as f32, app.ui.mouse_inside);
    let (zone, ramp) = (aw * 0.22, 60.0 * s);
    // With the information card open, the next arrow sits just left of it.
    let right = app.ui.panel.map_or(aw, |r| r.left);
    for (hit, x, glyph) in [(Hit::NavPrev, (18.0 * s).round(), '\u{E76B}'), (Hit::NavNext, right - (18.0 * s).round() - d, '\u{E76C}')] {
        let dist = if hit == Hit::NavPrev { mx } else { right - mx };
        let my = app.mouse.1 as f32;
        let on_other = app.ui.hover.is_some_and(|h| !matches!(h, Hit::NavPrev | Hit::NavNext));
        let in_bars = my < TITLE * s || my > ah - 150.0 * s;
        let near = if inside && !on_other && !in_bars { (1.0 - (dist - zone) / ramp).clamp(0.0, 1.0) } else { 0.0 };
        let hovered = app.ui.hover == Some(hit);
        let a = alpha * if hovered { 1.0 } else { near };
        if a < 0.02 {
            continue;
        }
        let r = rect(x, y, d, d);
        glass_box(gpu, &p, r, d / 2.0, s, a);
        if hovered {
            gpu.fill_circle(x + d / 2.0, y + d / 2.0, d / 2.0, p.hover.d2d(p.hover_alpha * a));
        }
        let cx = Ctx { text: &app.text, s };
        icon(&cx, gpu, glyph, r, 15.0, p.text, a, false);
        if a > 0.3 {
            app.ui.targets.push((r, hit));
        }
    }
}

fn toast(app: &mut App, gpu: &mut Gpu, aw: f32, msg: &str) {
    let s = app.scale();
    let p = app.palette;
    let cx = Ctx { text: &app.text, s };
    // Just after a delete, an Undo button in it.
    let undo = app.undo_until.is_some_and(|t| Instant::now() < t) && app.can_undo();
    let tw = measure(&cx, msg, 13.0, 500);
    let bw = if undo { (measure(&cx, "Undo", 13.0, 600) + cx.px(28.0)).round() } else { 0.0 };
    let w = tw + cx.px(36.0) + if undo { bw - cx.px(8.0) } else { 0.0 };
    let h = cx.px(36.0);
    let r = rect(((aw - w) / 2.0).round(), cx.px(TITLE + 8.0), w.round(), h);
    glass_box(gpu, &p, r, h / 2.0, s, 1.0);
    let cx = Ctx { text: &app.text, s };
    if !undo {
        label(&cx, gpu, msg, r, 13.0, 500, Align::Center, p.text, 1.0);
        return;
    }
    label(&cx, gpu, msg, rect(r.left + cx.px(18.0), r.top, tw + cx.px(2.0), h), 13.0, 500, Align::Left, p.text, 1.0);
    let k = cx.px(4.0);
    let b = rect(r.right - k - bw, r.top + k, bw, h - k * 2.0);
    let hit = Hit::Button(Cmd::Undo);
    let a = if app.ui.pressed == Some(hit) { 2.2 } else if app.ui.hover == Some(hit) { 1.6 } else { 0.9 };
    gpu.fill_round(b, (h - k * 2.0) / 2.0, p.hover.d2d(p.hover_alpha * a));
    label(&cx, gpu, "Undo", b, 13.0, 600, Align::Center, p.accent, 1.0);
    app.ui.targets.push((b, hit));
}

fn empty_state(app: &mut App, gpu: &mut Gpu, aw: f32, ah: f32) {
    let s = app.scale();
    let p = app.palette;
    let cx = Ctx { text: &app.text, s };
    let mid = (ah / 2.0).round();
    let d = cx.px(76.0);
    let circle = rect(((aw - d) / 2.0).round(), mid - cx.px(118.0), d, d);
    gpu.fill_circle((circle.left + circle.right) / 2.0, (circle.top + circle.bottom) / 2.0, d / 2.0, p.accent.d2d(0.14));
    icon(&cx, gpu, '\u{EB9F}', circle, 30.0, p.accent, 1.0, false);
    label(&cx, gpu, "Open an image", rect(0.0, mid - cx.px(28.0), aw, cx.px(30.0)), 20.0, 600, Align::Center, p.text, 1.0);
    label(&cx, gpu, "Drop a picture, video or song here, or press Ctrl+O", rect(0.0, mid + cx.px(6.0), aw, cx.px(22.0)), 13.0, 400, Align::Center, p.dim, 1.0);

    let text = "Choose an image";
    let bw = (measure(&cx, text, 13.0, 600) + cx.px(40.0)).round();
    let bh = cx.px(38.0);
    let r = rect(((aw - bw) / 2.0).round(), mid + cx.px(48.0), bw, bh);
    let hit = Hit::Button(Cmd::Open);
    let a = if app.ui.pressed == Some(hit) { 0.8 } else if app.ui.hover == Some(hit) { 0.9 } else { 1.0 };
    gpu.fill_round(r, bh / 2.0, p.accent.d2d(a));
    label(&cx, gpu, text, r, 13.0, 600, Align::Center, p.accent.ink(), 1.0);
    app.ui.targets.push((r, hit));
}

fn message(cx: &Ctx, gpu: &Gpu, pick: &mut Picker, p: &Palette, aw: f32, ah: f32, title: &str, body: &str) {
    let width = cx.px(440.0).min(aw - cx.px(32.0));
    let x = ((aw - width) / 2.0).round();
    let mid = ah / 2.0 - cx.px(30.0);
    label_sel(cx, gpu, pick, p, title, rect(x, mid, width, cx.px(26.0)), 15.0, 600, Align::Center, p.text, 1.0, Join::Line);
    paragraph_sel(cx, gpu, pick, p, body, x, mid + cx.px(32.0), width, 13.0, 400, Align::Center, p.dim, None, Join::Line);
}

// ---- The information panel ----

enum Row {
    Title(String),
    Section(&'static str),
    Fact(String, String),
}

fn rows(app: &App) -> Vec<Row> {
    let mut out = Vec::new();
    let Some(entry) = app.folder.current() else { return out };
    out.push(Row::Title(entry.name.clone()));
    let pic = app.shown.as_ref().and_then(|s| s.picture.clone());
    if let Some(pic) = &pic {
        out.push(Row::Section("Image"));
        let (w, h) = pic.orient.shown_size(pic.width, pic.height);
        let mp = w as f64 * h as f64 / 1e6;
        let dims = if mp >= 0.1 { format!("{w} × {h}  ({mp:.1} MP)") } else { format!("{w} × {h}") };
        out.push(Row::Fact("Dimensions".into(), dims));
        let d = &pic.details;
        if let Some(f) = d.format {
            out.push(Row::Fact("Format".into(), f.name().into()));
        }
        let exif_space = app.camera.as_ref().and_then(|(_, c)| c.as_ref()).and_then(|c| c.colour_space.clone());
        if let Some(space) = d.colour_space.clone().or(exif_space) {
            let note = if d.colour_managed { ", converted to sRGB" } else { "" };
            out.push(Row::Fact("Colour".into(), format!("{space}{note}")));
        }
        if let Some(ch) = &d.channels {
            let depth = d.bits_per_channel.map(|b| if ch == "Indexed" { format!(", {b}-bit palette") } else { format!(", {b} bits per channel") }).unwrap_or_default();
            out.push(Row::Fact("Pixels".into(), format!("{ch}{depth}")));
        }
        if pic.is_animated() || pic.streams {
            out.push(Row::Fact("Frames".into(), d.frame_count.to_string()));
        } else if d.frame_count > 1 {
            out.push(Row::Fact("Pages".into(), format!("{} (showing the first)", d.frame_count)));
        }
        if pic.orient != Orient::IDENTITY {
            out.push(Row::Fact("Orientation".into(), orientation_words(pic.orient)));
        }
    }
    if let Some(player) = app.player() {
        let tags = app.art.as_ref().filter(|a| a.path == entry.path).map(|a| &a.tags);
        match player.size() {
            Some((w, h)) => {
                out.push(Row::Section("Video"));
                out.push(Row::Fact("Dimensions".into(), format!("{w} × {h}")));
            }
            None => out.push(Row::Section("Music")),
        }
        if let Some(d) = player.duration() {
            out.push(Row::Fact("Length".into(), media_clock(d)));
        }
        if let Some(t) = tags {
            for (label, value) in [("Title", &t.title), ("Artist", &t.artist), ("Album", &t.album), ("Year", &t.year)] {
                if let Some(v) = value {
                    out.push(Row::Fact(label.into(), v.clone()));
                }
            }
        }
    }
    out.push(Row::Section("File"));
    let size = std::fs::metadata(&entry.path).map(|m| m.len()).unwrap_or(entry.size);
    out.push(Row::Fact("Size".into(), format!("{}  ({} bytes)", view_core::human_bytes(size), group(size))));
    if let Some(t) = entry.modified {
        out.push(Row::Fact("Modified".into(), crate::util::local_time(t)));
    }
    if let Some(t) = entry.created {
        out.push(Row::Fact("Created".into(), crate::util::local_time(t)));
    }
    if let Some(dir) = entry.path.parent() {
        out.push(Row::Fact("Folder".into(), dir.display().to_string()));
    }
    if let Some((_, Some(cam))) = &app.camera {
        if !cam.facts.is_empty() {
            out.push(Row::Section("Camera"));
            for f in &cam.facts {
                out.push(Row::Fact(f.label.into(), f.value.clone()));
            }
        }
    }
    out
}

fn orientation_words(o: Orient) -> String {
    match o.to_exif() {
        2 => "Mirrored",
        3 => "Turned upside down",
        4 => "Mirrored and upside down",
        5 => "Mirrored and turned 90° anticlockwise",
        6 => "Turned 90° clockwise",
        7 => "Mirrored and turned 90° clockwise",
        8 => "Turned 90° anticlockwise",
        _ => "Upright",
    }
    .to_string()
}

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

/// The information card: frosted glass floating over the picture at the right, centred top to
/// bottom. It's as tall as what's in it; if that's more than 90% of the window (or would reach
/// the window buttons), it stops there and what's in it scrolls.
fn info_panel(app: &mut App, gpu: &mut Gpu, cw: f32, ch: f32) {
    let s = app.scale();
    let p = app.palette;
    let rows = rows(app);
    if rows.is_empty() {
        app.ui.panel = None;
        return;
    }
    let cx = Ctx { text: &app.text, s };
    let card_w = cx.px(CARD_W);
    let m = cx.px(CARD_MARGIN);
    let pad = cx.px(22.0);
    let width = card_w - pad * 2.0;
    let label_w = cx.px(92.0);
    let (pad_top, pad_bottom) = (cx.px(20.0), cx.px(20.0));

    // Measure first, so the card can be sized to fit.
    let title_w = width - cx.px(30.0);
    let mut content = pad_top + pad_bottom;
    for row in &rows {
        content += match row {
            Row::Title(t) => paragraph_height(&cx, t, title_w, 15.0, 600) + cx.px(14.0),
            Row::Section(_) => cx.px(44.0),
            Row::Fact(l, v) => {
                let lh = paragraph_height(&cx, l, label_w - cx.px(8.0), 12.5, 400);
                let vh = paragraph_height(&cx, v, width - label_w, 12.5, 400);
                lh.max(vh) + cx.px(8.0)
            }
        };
    }
    let h = content.min(ch * 0.9).min(ch - cx.px(TITLE) * 2.0).max(cx.px(80.0)).round();
    let r = rect(cw - m - card_w, ((ch - h) / 2.0).round(), card_w, h);
    // The same glass, and as round, as the toolbar.
    let radius = cx.px(24.0);
    glass_box(gpu, &p, r, radius, s, 1.0);
    app.ui.panel = Some(r);
    app.ui.panel_content = content;
    app.ui.panel_scroll = app.ui.panel_scroll.clamp(0.0, (content - h).max(0.0));

    let view = rect(r.left, r.top + cx.px(2.0), card_w, h - cx.px(4.0));
    gpu.clip(view);
    let top = r.top + pad_top - app.ui.panel_scroll;
    let mut y = top;
    for row in &rows {
        match row {
            Row::Title(t) => {
                let th = paragraph_sel(&cx, gpu, &mut app.ui.pick, &p, t, r.left + pad, y, title_w, 15.0, 600, Align::Left, p.text, Some(view), Join::Line);
                y += th + cx.px(14.0);
            }
            Row::Section(t) => {
                // A hairline like the toolbar's dividers, then the heading.
                y += cx.px(8.0);
                gpu.fill(rect(r.left + pad, y.round(), width, 1.0), p.edge.d2d(p.edge_alpha * 1.6));
                y += cx.px(14.0);
                label(&cx, gpu, &t.to_uppercase(), rect(r.left + pad, y, width, cx.px(18.0)), 11.0, 600, Align::Left, p.soft(), 1.0);
                y += cx.px(22.0);
            }
            Row::Fact(l, v) => {
                let lh = paragraph_sel(&cx, gpu, &mut app.ui.pick, &p, l, r.left + pad, y, label_w - cx.px(8.0), 12.5, 400, Align::Left, p.soft(), Some(view), Join::Line);
                let vh = paragraph_sel(&cx, gpu, &mut app.ui.pick, &p, v, r.left + pad + label_w, y, width - label_w, 12.5, 400, Align::Left, p.text, Some(view), Join::Tab);
                y += lh.max(vh) + cx.px(8.0);
            }
        }
    }
    gpu.unclip();
    // Close, in the card's top corner: the same button as the Space preview's.
    let d = cx.px(30.0);
    let close = rect(r.right - cx.px(12.0) - d, r.top + cx.px(12.0), d, d);
    hover_fill(gpu, &app.ui, &p, close, Hit::PanelClose, d / 2.0, 1.0, false);
    icon(&cx, gpu, '\u{E8BB}', close, 9.0, p.text, 1.0, false);
    app.ui.targets.push((close, Hit::PanelClose));
}

/// Words selected in a photo: a band along each line, in the selection blue Windows uses,
/// which shows on light and dark pictures alike.
fn draw_words(app: &App, gpu: &mut Gpu) {
    let (Some(at), Some(words)) = (app.words_placed(), app.words.as_ref()) else { return };
    let band = Rgb(0, 120, 215).d2d(0.4);
    for r in words.bands(at) {
        gpu.fill(r, band);
    }
}
