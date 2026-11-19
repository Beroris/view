//! Drawing the window's other screens: the grid of thumbnails, the filmstrip over the bottom,
//! the editor, and the preview's fonts and folders. A part of `ui.rs`, kept apart since it's a
//! lot; what they do is in `tools.rs`.

use super::*;
use crate::gfx::color;
use view_core::edit::Aspect;

// ---- The grid ----

/// Where the grid's thumbnails go, for drawing and for the keys.
pub struct GridLayout {
    pub cols: usize,
    /// A thumbnail's side, and from one to the next.
    pub cell: f32,
    pub step: f32,
    pub left: f32,
    /// Room kept at the top for the name, and at the bottom for the buttons.
    pub top: f32,
    pub below: f32,
    /// How tall all the rows are.
    pub content: f32,
}

impl GridLayout {
    pub fn max_scroll(&self, ch: f32) -> f32 {
        (self.top + self.content + self.below - ch).max(0.0)
    }
}

/// The grid for `n` files at `size` DIPs a thumbnail, in a window `cw` wide. The thumbnails
/// grow a little to fill each row exactly.
pub fn grid_layout(cw: f32, ch: f32, s: f32, size: u32, n: usize) -> GridLayout {
    let _ = ch;
    let gap = (6.0 * s).round();
    let margin = (24.0 * s).round();
    let want = (size as f32 * s).round();
    let room = (cw - margin * 2.0).max(want);
    let cols = (((room + gap) / (want + gap)).floor() as usize).max(1);
    let cell = ((room + gap) / cols as f32 - gap).floor();
    let step = cell + gap;
    let used = cols as f32 * step - gap;
    let rows = n.div_ceil(cols);
    GridLayout { cols, cell, step, left: ((cw - used) / 2.0).round(), top: ((TITLE + 12.0) * s).round(), below: (92.0 * s).round(), content: (rows as f32 * step - gap).max(0.0) }
}

/// Whether a file is a song, rather than a video, by its extension.
fn is_audio(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|e| view_core::kind::AUDIO_EXTENSIONS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()))
}

/// A small play or note badge in a thumbnail's corner, for a video or a song.
fn media_badge(cx: &Ctx, gpu: &Gpu, r: D2D_RECT_F, path: &std::path::Path, alpha: f32) {
    if !crate::app::is_media(path) {
        return;
    }
    let d = cx.px(22.0).min((r.right - r.left) * 0.4);
    let m = cx.px(6.0).min((r.right - r.left) * 0.1);
    let b = rect(r.left + m, r.bottom - m - d, d, d);
    gpu.fill_circle(b.left + d / 2.0, b.top + d / 2.0, d / 2.0, color(0, 0, 0, 0.55 * alpha));
    icon(cx, gpu, if is_audio(path) { '\u{E8D6}' } else { '\u{E768}' }, b, (d / cx.s * 0.42).round(), Rgb(255, 255, 255), alpha, false);
}

/// Every file in the folder as square thumbnails, scrolling, with the name at the top and a
/// few buttons at the bottom.
pub(super) fn draw_grid(app: &mut App, gpu: &mut Gpu) {
    let p = app.palette;
    let (cw, ch) = app.client_size();
    let s = app.scale();
    app.ui.targets.clear();
    app.ui.pick.begin();
    app.ui.solid = true;
    app.ui.panel = None;
    app.ui.edit_frame = None;
    let n = app.folder.len();
    let l = grid_layout(cw, ch, s, app.settings.grid_size, n);
    let max = l.max_scroll(ch);
    let Some(g) = app.grid.as_mut() else { return };
    g.target = g.target.clamp(0.0, max);
    let d = g.target - g.scroll;
    g.scroll = if d.abs() < 0.5 { g.target } else { g.scroll + d * 0.3 };
    let (scroll, selected) = (g.scroll, g.selected);

    let scene = gpu.begin_scene();
    gpu.clear(p.backdrop.d2d(1.0));
    let first_row = ((scroll - l.top) / l.step).floor().max(0.0) as usize;
    let last_row = ((scroll + ch - l.top) / l.step).ceil().max(0.0) as usize;
    let (a, b) = ((first_row * l.cols).min(n), ((last_row + 1) * l.cols).min(n));
    // Thumbnails on screen first, then a screenful on either side, ready for scrolling.
    let ahead = (b - a).max(l.cols);
    let order: Vec<usize> = (a..b).chain(b..(b + ahead).min(n)).chain((a.saturating_sub(ahead)..a).rev()).collect();
    app.thumbs.want(app.hwnd, order.iter().map(|&i| app.folder.entries[i].path.as_path()));

    let cx = Ctx { text: &app.text, s };
    let radius = cx.px(6.0);
    for i in a..b {
        let (row, col) = (i / l.cols, i % l.cols);
        let r = rect(l.left + col as f32 * l.step, (l.top + row as f32 * l.step - scroll).round(), l.cell, l.cell);
        let path = &app.folder.entries[i].path;
        let hit = Hit::Thumb(i as u32);
        match app.thumbs.get(path) {
            Some(t) => gpu.draw_texture_cover(t, r, radius, 1.0),
            None => {
                gpu.fill_round(r, radius, p.text.d2d(0.06));
                if app.thumbs.failed(path) {
                    icon(&cx, gpu, '\u{E7C3}', r, (l.cell / s * 0.22).round(), p.dim, 0.8, false);
                }
            }
        }
        media_badge(&cx, gpu, r, path, 1.0);
        if app.ui.pressed == Some(hit) {
            gpu.fill_round(r, radius, color(0, 0, 0, 0.18));
        } else if app.ui.hover == Some(hit) {
            gpu.fill_round(r, radius, color(255, 255, 255, 0.08));
            // The name, on a dark pill along the bottom.
            let name = &app.folder.entries[i].name;
            let h = cx.px(22.0);
            let inset = cx.px(6.0);
            let w = (measure(&cx, name, 11.5, 500) + cx.px(16.0)).min(l.cell - inset * 2.0).round();
            let pr = rect(r.left + inset, r.bottom - inset - h, w, h);
            gpu.fill_round(pr, h / 2.0, color(0, 0, 0, 0.62));
            label(&cx, gpu, name, rect(pr.left + cx.px(8.0), pr.top, w - cx.px(16.0), h), 11.5, 500, Align::Left, Rgb(255, 255, 255), 1.0);
        }
        if i == selected {
            let k = cx.px(3.0);
            gpu.stroke_round(rect(r.left - k, r.top - k, l.cell + k * 2.0, l.cell + k * 2.0), radius + k, cx.px(2.0).max(1.0), p.accent.d2d(1.0));
        }
        app.ui.targets.push((r, hit));
    }
    if n == 0 {
        let cx = Ctx { text: &app.text, s };
        message(&cx, gpu, &mut app.ui.pick, &p, cw, ch, "Nothing here", "Everything in this folder has been deleted.");
    }
    // The name and the buttons go over the thumbnails, so they're in the picture the glass blurs.
    shades(gpu, &p, cw, ch, s, 0.7, true, false);
    screen_switch(app, gpu, cw, ch);
    if scene {
        gpu.end_scene();
    }

    let mid = s * TITLE / 2.0;
    title(app, gpu, cw, mid, 1.0);
    let items = [Item::Button(Cmd::Grid, '\u{E80A}', false), Item::Gap, Item::Button(Cmd::ZoomOut, '\u{E71F}', false), Item::Button(Cmd::ZoomIn, '\u{E8A3}', false), Item::Gap, Item::Button(Cmd::Menu, '\u{E712}', false)];
    button_bar(app, gpu, cw, ch, &items, 1.0, |cmd| cmd == Cmd::Grid);
    caption_buttons(app, gpu, cw, 1.0);
    if let Some((msg, _)) = &app.toast {
        let msg = msg.clone();
        toast(app, gpu, cw, &msg);
    }
    app.ui.modal = app.dialog.is_some();
    if app.dialog.is_some() {
        dialog(app, gpu, cw, ch);
    }
}

/// A glass bar of round buttons at the bottom middle, as the toolbar. `active` marks the ones
/// that are on.
pub(super) fn button_bar(app: &mut App, gpu: &mut Gpu, aw: f32, ah: f32, items: &[Item], alpha: f32, active: impl Fn(Cmd) -> bool) {
    let s = app.scale();
    let p = app.palette;
    let cx = Ctx { text: &app.text, s };
    let button = cx.px(36.0);
    let pad = cx.px(6.0);
    let gap = cx.px(11.0);
    let width: f32 = items.iter().map(|i| if matches!(i, Item::Gap) { gap } else { button }).sum::<f32>() + pad * 2.0;
    let height = button + pad * 2.0;
    let bar = rect(((aw - width) / 2.0).round(), (ah - height - cx.px(20.0)).round(), width, height);
    glass_box(gpu, &p, bar, height / 2.0, s, alpha);
    app.ui.targets.push((bar, Hit::Toolbar));
    let cx = Ctx { text: &app.text, s };
    let mut x = bar.left + pad;
    for item in items {
        match item {
            Item::Button(cmd, glyph, mirrored) => {
                let r = rect(x, bar.top + pad, button, button);
                let hit = Hit::Button(*cmd);
                hover_fill(gpu, &app.ui, &p, r, hit, button / 2.0, alpha, active(*cmd));
                icon(&cx, gpu, *glyph, r, 15.0, p.text, alpha, *mirrored);
                app.ui.targets.push((r, hit));
                x += button;
            }
            Item::Gap | Item::Zoom => {
                let mx = (x + gap / 2.0).round();
                gpu.fill(rect(mx, bar.top + pad + cx.px(10.0), 1.0, button - cx.px(20.0)), p.edge.d2d(p.edge_alpha * 1.6 * alpha));
                x += gap;
            }
        }
    }
}

// ---- The filmstrip ----

/// A strip of the folder's thumbnails over the toolbar, the file showing in the middle with a
/// ring round it. It glides along as the files change, and a click on one goes there.
pub(super) fn filmstrip(app: &mut App, gpu: &mut Gpu, aw: f32, ah: f32, alpha: f32) {
    if !app.strip_shown() || alpha < 0.01 {
        return;
    }
    let s = app.scale();
    // It keeps out of the photo's way until the mouse comes down towards it, and stays while
    // the mouse is on it. The toolbar alone is what a still mouse elsewhere shows.
    let near = app.ui.mouse_inside && app.mouse.1 as f32 > ah - 190.0 * s;
    let on_it = matches!(app.ui.hover, Some(Hit::Thumb(_) | Hit::Strip)) || matches!(app.ui.pressed, Some(Hit::Thumb(_)));
    // The speeds rise out of the toolbar where the strip is: it steps aside while they're out.
    app.ui.slide_strip((near || on_it) && !app.ui.speeds_out());
    let open = app.ui.strip_open();
    if open < 0.01 {
        // Out of sight, it has nowhere to glide to.
        app.strip_at = app.folder.index as f32;
        return;
    }
    let alpha = alpha * open;
    let rise = ((1.0 - open) * 10.0 * s).round();
    let p = app.palette;
    let n = app.folder.len();
    let index = app.folder.index;
    let target = index as f32;
    let d = target - app.strip_at;
    app.strip_at = if d.abs() > 30.0 || d.abs() < 0.01 { target } else { app.strip_at + d * 0.25 };
    let at = app.strip_at;

    let cx = Ctx { text: &app.text, s };
    let cell = cx.px(46.0);
    let gap = cx.px(4.0);
    let step = cell + gap;
    let pad = cx.px(6.0);
    let bar_top = (ah - cx.px(48.0) - cx.px(20.0)).round();
    let total = n as f32 * step - gap;
    let inner = total.min((aw - cx.px(40.0)).min(cx.px(760.0)) - pad * 2.0).max(cell);
    let (w, h) = (inner + pad * 2.0, cell + pad * 2.0);
    let r = rect(((aw - w) / 2.0).round(), bar_top - cx.px(10.0) - h + rise, w, h);
    glass_box(gpu, &p, r, cx.px(14.0), s, alpha);
    let cx = Ctx { text: &app.text, s };
    if app.ui.strip_open() > 0.5 {
        app.ui.targets.push((r, Hit::Strip));
    }

    let off = if total <= inner { 0.0 } else { (at * step + cell / 2.0 - inner / 2.0).clamp(0.0, total - inner) };
    let x0 = r.left + pad - off;
    let first = (off / step).floor().max(0.0) as usize;
    let last = (((off + inner) / step).ceil() as usize).min(n - 1);
    let mut order: Vec<usize> = (first..=last).collect();
    order.sort_by_key(|&i| i.abs_diff(index));
    app.thumbs.want(app.hwnd, order.iter().map(|&i| app.folder.entries[i].path.as_path()));

    let view = rect(r.left + pad / 2.0, r.top, w - pad, h);
    gpu.clip(view);
    let radius = cx.px(5.0);
    for i in first..=last {
        let cr = rect((x0 + i as f32 * step).round(), r.top + pad, cell, cell);
        let hit = Hit::Thumb(i as u32);
        let current = i == index;
        let a = alpha * if current || app.ui.hover == Some(hit) { 1.0 } else { 0.72 };
        let path = &app.folder.entries[i].path;
        match app.thumbs.get(path) {
            Some(t) => gpu.draw_texture_cover(t, cr, radius, a),
            None => gpu.fill_round(cr, radius, p.text.d2d(0.08 * alpha)),
        }
        media_badge(&cx, gpu, cr, path, a);
        if !current {
            // A hairline, so a dark thumbnail still has an edge against a dark photo.
            gpu.stroke_round(cr, radius, 1.0, p.text.d2d(0.22 * alpha));
        }
        if current {
            // A ring just outside the picture, so it doesn't cover any of it.
            let o = cx.px(2.5);
            let ring = rect(cr.left - o, cr.top - o, cell + o * 2.0, cell + o * 2.0);
            gpu.stroke_round(ring, radius + o, cx.px(2.0).max(1.0), p.accent.d2d(alpha));
        }
        let seen = rect(cr.left.max(view.left), cr.top, (cr.right.min(view.right) - cr.left.max(view.left)).max(0.0), cell);
        if seen.right > seen.left && app.ui.strip_open() > 0.5 {
            app.ui.targets.push((seen, hit));
        }
    }
    gpu.unclip();
}

// ---- The editor ----

/// The photo being edited, with its crop's frame over it, and the tools in a card at the right.
pub(super) fn draw_editor(app: &mut App, gpu: &mut Gpu) {
    let p = app.palette;
    let (cw, ch) = app.client_size();
    let s = app.scale();
    app.ui.targets.clear();
    app.ui.pick.begin();
    app.ui.solid = true;
    app.ui.panel = None;
    if let Some(ed) = app.editor.as_mut() {
        ed.refresh(gpu);
    }
    let cx = Ctx { text: &app.text, s };
    let m = cx.px(CARD_MARGIN);
    let card_w = cx.px(CARD_W);
    let panel = rect(cw - m - card_w, cx.px(TITLE + 2.0), card_w, (ch - cx.px(TITLE + 2.0) - m).max(cx.px(200.0)));
    let area = rect(cx.px(24.0), cx.px(TITLE + 12.0), (panel.left - cx.px(48.0)).max(cx.px(60.0)), (ch - cx.px(TITLE + 36.0)).max(cx.px(60.0)));

    let scene = gpu.begin_scene();
    gpu.clear(p.backdrop.d2d(1.0));
    app.ui.edit_frame = None;
    if let Some(ed) = &app.editor {
        if let Some(t) = &ed.preview {
            let (aw, ah) = (area.right - area.left, area.bottom - area.top);
            let k = (aw / t.width as f32).min(ah / t.height as f32).min(4.0);
            let (fw, fh) = ((t.width as f32 * k).round(), (t.height as f32 * k).round());
            let frame = rect((area.left + (aw - fw) / 2.0).round(), (area.top + (ah - fh) / 2.0).round(), fw, fh);
            gpu.draw_texture(t, frame, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, 1.0);
            app.ui.edit_frame = Some(frame);
            crop_frame(&cx, gpu, frame, ed);
        }
    }
    if scene {
        gpu.end_scene();
    }

    glass_box(gpu, &p, panel, cx.px(24.0), s, 1.0);
    app.ui.targets.push((panel, Hit::Toolbar));
    let pad = cx.px(22.0);
    let x = panel.left + pad;
    let width = card_w - pad * 2.0;
    let mut y = panel.top + cx.px(20.0);
    label(&cx, gpu, "Edit", rect(x, y, width, cx.px(24.0)), 15.0, 600, Align::Left, p.text, 1.0);
    y += cx.px(40.0);

    let (aspect, saving, values) = match &app.editor {
        Some(ed) => (ed.aspect, ed.saving, (0..5).map(|i| (ed.value(i), ed.value_text(i))).collect::<Vec<_>>()),
        None => return,
    };
    // Crop: the shapes, and turning.
    section(&cx, gpu, &p, x, y, width, "Crop", false);
    let b = cx.px(30.0);
    for (k, (cmd, mirrored)) in [(Cmd::EditTurn(false), true), (Cmd::EditTurn(true), false)].into_iter().enumerate() {
        let r = rect(x + width - b * (2 - k) as f32, y - cx.px(6.0), b, b);
        let hit = Hit::Button(cmd);
        hover_fill(gpu, &app.ui, &p, r, hit, b / 2.0, 1.0, false);
        icon(&cx, gpu, '\u{E7AD}', r, 13.0, p.text, 1.0, mirrored);
        app.ui.targets.push((r, hit));
    }
    y += cx.px(28.0);
    let chip_h = cx.px(30.0);
    let chip_gap = cx.px(6.0);
    let chip_w = ((width - chip_gap * 2.0) / 3.0).floor();
    for (i, a) in Aspect::ALL.iter().enumerate() {
        let (row, col) = (i / 3, i % 3);
        let r = rect(x + col as f32 * (chip_w + chip_gap), y + row as f32 * (chip_h + chip_gap), chip_w, chip_h);
        let hit = Hit::Button(Cmd::EditAspect(i as u8));
        if *a == aspect {
            gpu.fill_round(r, chip_h / 2.0, p.accent.d2d(if app.ui.pressed == Some(hit) { 0.8 } else { 1.0 }));
            label(&cx, gpu, a.label(), r, 12.5, 600, Align::Center, p.accent.ink(), 1.0);
        } else {
            let k = if app.ui.pressed == Some(hit) { 1.8 } else if app.ui.hover == Some(hit) { 1.3 } else { 0.6 };
            gpu.fill_round(r, chip_h / 2.0, p.hover.d2d(p.hover_alpha * k));
            gpu.stroke_round(r, chip_h / 2.0, 1.0, p.edge.d2d(p.edge_alpha * 1.4));
            label(&cx, gpu, a.label(), r, 12.5, 500, Align::Center, p.text, 1.0);
        }
        app.ui.targets.push((r, hit));
    }
    y += chip_h * 2.0 + chip_gap + cx.px(20.0);
    y += slider(app, gpu, 0, &values[0], x, y, width);

    let cx = Ctx { text: &app.text, s };
    y += cx.px(6.0);
    section(&cx, gpu, &p, x, y, width, "Light and colour", true);
    y += cx.px(30.0);
    for i in 1..5u8 {
        y += slider(app, gpu, i, &values[i as usize], x, y, width);
    }

    // Reset and Cancel, and the saves under them, at the bottom of the card.
    let cx = Ctx { text: &app.text, s };
    let save_h = cx.px(36.0);
    let save = rect(x, panel.bottom - pad - save_h, width, save_h);
    let half = ((width - cx.px(8.0)) / 2.0).floor();
    let small_h = cx.px(32.0);
    let row_y = save.top - cx.px(10.0) - small_h;
    if row_y > y {
        for (k, (text, cmd)) in [("Reset", Cmd::EditReset), ("Cancel", Cmd::EditCancel)].into_iter().enumerate() {
            let r = rect(x + k as f32 * (half + cx.px(8.0)), row_y, half, small_h);
            plain_button(&cx, gpu, &app.ui, &p, r, text, Hit::Button(cmd));
            app.ui.targets.push((r, Hit::Button(cmd)));
        }
    }
    // Saving over the original is the main one, where the photo can be written back as it was;
    // a copy beside it, plainer, next to it. Otherwise a copy is all there is.
    let (main, main_text) = if app.can_save_over() {
        let copy = rect(x, save.top, half, save_h);
        plain_button(&cx, gpu, &app.ui, &p, copy, "Save a copy", Hit::Button(Cmd::EditSaveCopy));
        app.ui.targets.push((copy, Hit::Button(Cmd::EditSaveCopy)));
        (rect(x + half + cx.px(8.0), save.top, width - half - cx.px(8.0), save_h), (Cmd::EditSave, "Save"))
    } else {
        (save, (Cmd::EditSaveCopy, "Save a copy"))
    };
    let hit = Hit::Button(main_text.0);
    let a = if saving { 0.55 } else if app.ui.pressed == Some(hit) { 0.8 } else if app.ui.hover == Some(hit) { 0.9 } else { 1.0 };
    gpu.fill_round(main, save_h / 2.0, p.accent.d2d(a));
    label(&cx, gpu, if saving { "Saving…" } else { main_text.1 }, main, 13.0, 600, Align::Center, p.accent.ink(), 1.0);
    app.ui.targets.push((main, hit));

    let mid = s * TITLE / 2.0;
    title(app, gpu, panel.left, mid, 1.0);
    caption_buttons(app, gpu, cw, 1.0);
    if let Some((msg, _)) = &app.toast {
        let msg = msg.clone();
        toast(app, gpu, panel.left, &msg);
    }
    app.ui.modal = app.dialog.is_some();
    if app.dialog.is_some() {
        dialog(app, gpu, cw, ch);
    }
}

/// A heading in the editor's card, with a hairline over it unless it's the first.
fn section(cx: &Ctx, gpu: &Gpu, p: &Palette, x: f32, y: f32, width: f32, text: &str, rule: bool) {
    if rule {
        gpu.fill(rect(x, (y - cx.px(12.0)).round(), width, 1.0), p.edge.d2d(p.edge_alpha * 1.6));
    }
    label(cx, gpu, &text.to_uppercase(), rect(x, y, width, cx.px(18.0)), 11.0, 600, Align::Left, p.dim, 1.0);
}

/// A button with an edge and no fill but the hover's.
fn plain_button(cx: &Ctx, gpu: &Gpu, ui: &State, p: &Palette, r: D2D_RECT_F, text: &str, hit: Hit) {
    let h = r.bottom - r.top;
    let k = if ui.pressed == Some(hit) { 1.8 } else if ui.hover == Some(hit) { 1.3 } else { 0.7 };
    gpu.fill_round(r, h / 2.0, p.hover.d2d(p.hover_alpha * k));
    gpu.stroke_round(r, h / 2.0, 1.0, p.edge.d2d(p.edge_alpha * 1.5));
    label(cx, gpu, text, r, 13.0, 500, Align::Center, p.text, 1.0);
}

/// One of the editor's sliders: its name and value, and a track that fills from the middle.
/// Returns how much room it took.
fn slider(app: &mut App, gpu: &mut Gpu, i: u8, (value, text): &(f32, String), x: f32, y: f32, width: f32) -> f32 {
    let s = app.scale();
    let p = app.palette;
    let cx = Ctx { text: &app.text, s };
    let name = crate::editor::SLIDERS[i as usize];
    label(&cx, gpu, name, rect(x, y, width, cx.px(20.0)), 12.5, 500, Align::Left, p.text, 1.0);
    label(&cx, gpu, text, rect(x, y, width, cx.px(20.0)), 12.5, 500, Align::Right, if *value == 0.0 { p.dim } else { p.text }, 1.0);
    let th = cx.px(4.0);
    let ty = (y + cx.px(30.0)).round();
    let track = rect(x, ty, width, th);
    gpu.fill_round(track, th / 2.0, p.text.d2d(0.18));
    let mid = x + width / 2.0;
    gpu.fill(rect(mid.round(), ty - cx.px(4.0), 1.0, th + cx.px(8.0)), p.text.d2d(0.35));
    let kx = x + (value + 1.0) / 2.0 * width;
    gpu.fill_round(rect(mid.min(kx), ty, (kx - mid).abs(), th), th / 2.0, p.accent.d2d(1.0));
    let hit = Hit::Slider(i);
    let active = app.ui.hover == Some(hit) || app.ui.pressed == Some(hit);
    gpu.fill_circle(kx, ty + th / 2.0, if active { cx.px(8.0) } else { cx.px(6.5) }, p.text.d2d(1.0));
    app.ui.sliders[i as usize] = Some(track);
    app.ui.targets.push((rect(x - cx.px(8.0), ty - cx.px(12.0), width + cx.px(16.0), th + cx.px(24.0)), hit));
    cx.px(56.0)
}

/// The crop over the photo: what's cut off darkened, a frame with handles at its corners and
/// edges, and thirds while it's moved (a finer grid while straightening).
fn crop_frame(cx: &Ctx, gpu: &Gpu, frame: D2D_RECT_F, ed: &crate::editor::Editor) {
    let (fw, fh) = (frame.right - frame.left, frame.bottom - frame.top);
    let c = ed.edit.crop;
    // Whole pixels, so the four shade bands meet without a light seam between them.
    let r = D2D_RECT_F {
        left: (frame.left + c.x * fw).round(),
        top: (frame.top + c.y * fh).round(),
        right: (frame.left + (c.x + c.w) * fw).round(),
        bottom: (frame.top + (c.y + c.h) * fh).round(),
    };
    let shade = color(0, 0, 0, 0.55);
    let fill = |x: f32, y: f32, w: f32, h: f32| {
        if w > 0.0 && h > 0.0 {
            gpu.fill(rect(x, y, w, h), shade);
        }
    };
    fill(frame.left, frame.top, fw, r.top - frame.top);
    fill(frame.left, r.bottom, fw, frame.bottom - r.bottom);
    fill(frame.left, r.top, r.left - frame.left, r.bottom - r.top);
    fill(r.right, r.top, frame.right - r.right, r.bottom - r.top);
    let white = |a: f32| color(255, 255, 255, a);
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let lines = match ed.grip.map(|g| g.0) {
        Some(crate::editor::Grip::Crop(_)) => 3,
        Some(crate::editor::Grip::Slider(0)) => 8,
        _ => 0,
    };
    for k in 1..lines {
        let f = k as f32 / lines as f32;
        gpu.fill(rect((r.left + w * f).round(), r.top, 1.0, h), white(0.4));
        gpu.fill(rect(r.left, (r.top + h * f).round(), w, 1.0), white(0.4));
    }
    gpu.stroke_round(r, 0.0, 1.0, white(0.9));
    let (len, t) = (cx.px(18.0).min(w / 3.0).min(h / 3.0), cx.px(3.0));
    for (x, y, sx, sy) in [(r.left, r.top, 1.0, 1.0), (r.right, r.top, -1.0, 1.0), (r.left, r.bottom, 1.0, -1.0), (r.right, r.bottom, -1.0, -1.0)] {
        let bx = if sx > 0.0 { x - t / 2.0 } else { x - len + t / 2.0 };
        let by = if sy > 0.0 { y - t / 2.0 } else { y - t / 2.0 };
        gpu.fill(rect(bx, by, len, t), white(1.0));
        let vx = if sx > 0.0 { x - t / 2.0 } else { x - t / 2.0 };
        let vy = if sy > 0.0 { y - t / 2.0 } else { y - len + t / 2.0 };
        gpu.fill(rect(vx, vy, t, len), white(1.0));
    }
    let bar = cx.px(22.0).min(w / 3.0).min(h / 3.0);
    let (mx, my) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
    gpu.fill(rect(mx - bar / 2.0, r.top - t / 2.0, bar, t), white(1.0));
    gpu.fill(rect(mx - bar / 2.0, r.bottom - t / 2.0, bar, t), white(1.0));
    gpu.fill(rect(r.left - t / 2.0, my - bar / 2.0, t, bar), white(1.0));
    gpu.fill(rect(r.right - t / 2.0, my - bar / 2.0, t, bar), white(1.0));
}

// ---- Fonts in the preview ----

/// A font file, shown in itself: its name, its letters and figures, and a line of text at a
/// few sizes. Returns why it can't be, if it can't.
pub(super) fn font_preview(app: &mut App, gpu: &mut Gpu, cw: f32, ch: f32) -> Option<String> {
    let s = app.scale();
    let p = app.palette;
    let cx = Ctx { text: &app.text, s };
    let face = match app.peek.as_ref().and_then(|k| k.font.as_ref())? {
        Ok(f) => f,
        Err(e) => return Some(e.clone()),
    };
    let m = cx.px(36.0);
    let width = cw - m * 2.0;
    let mut y = cx.px(58.0);
    let text = cx.text;
    let own = |dip: f32| text.font_format(face, cx.px(dip)).or_else(|| text.format(cx.px(dip), 400, Align::Left, true));
    gpu.clip(rect(0.0, 0.0, cw, ch));
    if let Some(l) = own(44.0).and_then(|f| text.layout(&face.family, &f, width, 1000.0)) {
        gpu.text(&l, m, y, p.text.d2d(1.0));
        y += Text::size(&l).1 + cx.px(8.0);
    }
    let version = face.version.as_deref().map(|v| v.split(';').next().unwrap_or(v).trim().to_string());
    let mut facts: Vec<String> = [Some(face.face.clone()).filter(|f| !f.is_empty()), version, face.designer.clone()].into_iter().flatten().collect();
    if face.faces > 1 {
        facts.push(format!("{} faces in this file", face.faces));
    }
    y += paragraph_sel(&cx, gpu, &mut app.ui.pick, &p, &facts.join("  ·  "), m, y, width, 12.5, 400, Align::Left, p.dim, None, Join::Line) + cx.px(20.0);
    gpu.fill(rect(m, y.round(), width, 1.0), p.edge.d2d(p.edge_alpha * 1.6));
    y += cx.px(20.0);
    let set = "ABCDEFGHIJKLMNOPQRSTUVWXYZ\nabcdefghijklmnopqrstuvwxyz\n1234567890  ! ? & @ # % ( ) , . ; :";
    if let Some(l) = own(26.0).and_then(|f| text.layout(set, &f, width, 2000.0)) {
        gpu.text(&l, m, y, p.text.d2d(1.0));
        y += Text::size(&l).1 + cx.px(24.0);
    }
    let sample = "The quick brown fox jumps over the lazy dog.";
    let lead = cx.px(34.0);
    for size in [12.0, 16.0, 20.0, 28.0, 36.0, 48.0, 64.0] {
        if y > ch {
            break;
        }
        let Some(l) = own(size).and_then(|f| text.layout(sample, &f, 100_000.0, 1000.0)) else { continue };
        let h = Text::size(&l).1;
        label(&cx, gpu, &format!("{size}"), rect(m, y, lead, h.min(cx.px(30.0))), 11.0, 500, Align::Left, p.dim, 1.0);
        gpu.clip(rect(m + lead, y, (width - lead).max(0.0), h));
        gpu.text(&l, m + lead, y, p.text.d2d(1.0));
        gpu.unclip();
        y += h + cx.px(10.0);
    }
    gpu.unclip();
    None
}

// ---- Folders in the preview ----

/// Some of the pictures in a folder, under its card: up to two rows of three.
pub(super) fn card_pictures(cx: &Ctx, gpu: &Gpu, pictures: &[Texture], cw: f32, top: f32) {
    let gap = cx.px(8.0);
    let cell = ((cw - cx.px(56.0) - gap * 2.0) / 3.0).min(cx.px(110.0)).floor();
    let cols = pictures.len().min(3);
    let used = cols as f32 * (cell + gap) - gap;
    let left = ((cw - used) / 2.0).round();
    for (i, t) in pictures.iter().take(6).enumerate() {
        let (row, col) = (i / 3, i % 3);
        let r = rect(left + col as f32 * (cell + gap), top + row as f32 * (cell + gap), cell, cell);
        gpu.draw_texture_cover(t, r, cx.px(8.0), 1.0);
    }
}

/// How tall [`card_pictures`] are.
pub(super) fn card_pictures_height(cx: &Ctx, n: usize, cw: f32) -> f32 {
    if n == 0 {
        return 0.0;
    }
    let gap = cx.px(8.0);
    let cell = ((cw - cx.px(56.0) - gap * 2.0) / 3.0).min(cx.px(110.0)).floor();
    let rows = n.min(6).div_ceil(3) as f32;
    rows * cell + (rows - 1.0) * gap
}

// ---- Trimming a video ----

/// The controls for trimming, in a card along the bottom. On top, play and the bar: the parts
/// left end to end (a deleted part closes up), the picture along it and the sound in a thinner
/// lane under, what's kept between two handles, and the splits. Under it, the tools and the
/// times; at the bottom, the buttons.
pub fn trim_bar(app: &mut App, gpu: &mut Gpu, aw: f32, ch: f32) {
    let s = app.scale();
    let p = app.palette;
    let Some(t) = app.trim.as_ref() else { return };
    let (start, end, mute, sound, progress, selected) = (t.start, t.end, t.mute, t.sound, t.progress(), t.selected);
    let has_splits = !t.splits.is_empty();
    // The parts left: which, where they are in the video, and what's become of their sound.
    let parts: Vec<(usize, f64, f64, crate::trim::Part)> = (0..t.parts.len()).filter(|&i| !t.parts[i].deleted).map(|i| (i, t.bounds(i).0, t.bounds(i).1, t.parts[i])).collect();
    // The splits that can be moved, between two parts left; and where parts were deleted.
    let movable: Vec<(usize, f64)> = (0..t.splits.len()).filter(|&i| t.split_shown(i)).map(|i| (i, t.splits[i])).collect();
    let line_length = t.length();
    let shown = t.shown();
    let line = move |x: f64| {
        let mut along = 0.0;
        for &(a, b) in &shown {
            if x < a {
                return along;
            }
            if x <= b {
                return along + (x - a);
            }
            along += b - a;
        }
        along
    };
    let kept = t.kept();
    let saving = progress.is_some();
    let (pos, paused) = app.player().map_or((0.0, true), |pl| (pl.position(), pl.paused() || pl.ended));
    let cx = Ctx { text: &app.text, s };
    let width = (aw - cx.px(32.0)).min(cx.px(920.0)).max(cx.px(300.0));
    let pad = cx.px(14.0);
    let bar_h = cx.px(46.0);
    let bh = cx.px(32.0);
    let height = pad * 2.0 + bar_h + cx.px(12.0) + bh + cx.px(8.0) + bh;
    let card = rect(((aw - width) / 2.0).round(), ch - cx.px(16.0) - height, width, height);
    glass_box(gpu, &p, card, cx.px(22.0), s, 1.0);
    app.ui.targets.push((card, Hit::Toolbar));
    let cx = Ctx { text: &app.text, s };
    let live = |targets: &mut Vec<(D2D_RECT_F, Hit)>, r: D2D_RECT_F, hit: Hit| {
        if !saving {
            targets.push((r, hit));
        }
    };

    // Play, and the bar.
    let button = cx.px(36.0);
    let row = card.top + pad;
    let play = rect(card.left + pad, row + (bar_h - button) / 2.0, button, button);
    let hit = Hit::Button(Cmd::PlayPause);
    hover_fill(gpu, &app.ui, &p, play, hit, button / 2.0, 1.0, false);
    icon(&cx, gpu, if paused { '\u{E768}' } else { '\u{E769}' }, play, 15.0, p.text, 1.0, false);
    live(&mut app.ui.targets, play, hit);
    let tab = cx.px(12.0);
    let strip = rect(play.right + cx.px(10.0), row, card.right - pad - play.right - cx.px(10.0), bar_h);
    // The handles sit outside what's kept, so the track runs between their inner edges.
    let track = rect(strip.left + tab, strip.top, strip.right - strip.left - tab * 2.0, bar_h);
    let tw = track.right - track.left;
    let at = |t: f64| (track.left + tw * (line(t) / line_length).clamp(0.0, 1.0) as f32).round();
    let lane_gap = cx.px(4.0);
    let sound_h = cx.px(13.0);
    let picture = rect(track.left, track.top, tw, bar_h - sound_h - lane_gap);
    let sound_lane = rect(track.left, track.bottom - sound_h, tw, sound_h);
    gpu.fill_round(strip, cx.px(8.0), p.text.d2d(0.07));
    let edge = cx.px(2.0);
    // Each part left: lit where it's kept, its sound lit unless silenced.
    for &(i, a, b, part) in &parts {
        let (xa, xb) = (at(a.max(start)), at(b.min(end)));
        if xb <= xa {
            continue;
        }
        let w = xb - xa;
        gpu.fill(rect(xa, picture.top, w, picture.bottom - picture.top), p.accent.d2d(if selected == Some(i) { 0.30 } else { 0.16 }));
        gpu.fill(rect(xa, track.top, w, edge), p.accent.d2d(1.0));
        gpu.fill(rect(xa, track.bottom - edge, w, edge), p.accent.d2d(1.0));
        if sound && !mute && !part.silent {
            gpu.fill(rect(xa, sound_lane.top, w, sound_h - edge), p.accent.d2d(0.42));
        } else if sound && !mute && w > cx.px(18.0) {
            icon(&cx, gpu, '\u{E74F}', rect(xa, sound_lane.top, w, sound_h - edge), 9.0, p.dim, 1.0, false);
        }
    }
    // Where parts were deleted, the two either side meet at a notch.
    for pair in parts.windows(2) {
        if (pair[0].2 - pair[1].1).abs() > 1e-9 {
            let x = at(pair[1].1);
            gpu.fill(rect(x - cx.px(1.0), strip.top, cx.px(2.0), bar_h), p.panel.d2d(1.0));
            let k = cx.px(4.0);
            gpu.fill_circle(x, strip.top - k / 2.0, k, p.panel.d2d(1.0));
            gpu.fill_circle(x, strip.bottom + k / 2.0, k, p.panel.d2d(1.0));
        }
    }
    // The part picked, outlined.
    if let Some(&(_, a, b, _)) = parts.iter().find(|x| Some(x.0) == selected) {
        let (xa, xb) = (at(a), at(b));
        gpu.stroke_round(rect(xa + 1.0, track.top + 1.0, (xb - xa - 2.0).max(1.0), bar_h - 2.0), cx.px(3.0), cx.px(2.0), p.text.d2d(0.95));
    }
    app.ui.seek_track = Some(track);
    live(&mut app.ui.targets, strip, Hit::Trim(2));

    // The splits: a line each, to drag.
    let reach = cx.px(5.0);
    for &(i, t) in &movable {
        let x = at(t);
        let mark = rect(x - cx.px(1.5), strip.top - cx.px(3.0), cx.px(3.0), bar_h + cx.px(6.0));
        let hit = Hit::Trim(3 + i as u8);
        let lit = app.ui.hover == Some(hit) || app.ui.pressed == Some(hit);
        gpu.fill_round(mark, cx.px(1.5), p.text.d2d(if lit { 1.0 } else { 0.85 }));
        gpu.fill_circle(x, strip.bottom + cx.px(3.0), cx.px(if lit { 4.5 } else { 3.5 }), p.text.d2d(1.0));
        live(&mut app.ui.targets, rect(x - reach, strip.top - reach, reach * 2.0, bar_h + reach * 2.0), hit);
    }
    for (i, x) in [(0u8, at(start) - tab), (1u8, at(end))] {
        let handle = rect(x, track.top, tab, bar_h);
        let hit = Hit::Trim(i);
        let a = if app.ui.pressed == Some(hit) { 0.8 } else if app.ui.hover == Some(hit) { 0.9 } else { 1.0 };
        gpu.fill_round(handle, cx.px(5.0), p.accent.d2d(a));
        // Square on the inside, where it meets what's kept.
        let inner = if i == 0 { rect(x + tab / 2.0, track.top, tab / 2.0, bar_h) } else { rect(x, track.top, tab / 2.0, bar_h) };
        gpu.fill(inner, p.accent.d2d(a));
        let grip = rect((x + tab / 2.0 - 1.0).round(), track.top + bar_h * 0.32, 2.0, bar_h * 0.36);
        gpu.fill_round(grip, 1.0, p.accent.ink().d2d(0.8));
        live(&mut app.ui.targets, rect(x - reach, strip.top - reach, tab + reach * 2.0, bar_h + reach * 2.0), hit);
    }
    // Where it's playing.
    let xp = at(pos);
    gpu.fill(rect(xp - 1.0, strip.top - cx.px(4.0), 2.0, bar_h + cx.px(8.0)), p.text.d2d(0.95));
    gpu.fill_circle(xp, strip.top - cx.px(4.0), cx.px(4.0), p.text.d2d(0.95));

    let bw = |t: &str| (measure(&cx, t, 13.0, 600) + cx.px(32.0)).round();
    let gap = cx.px(8.0);

    // The tools: split; with a part picked, delete it or silence it; else take all the sound
    // out. The times on the right.
    let row2 = row + bar_h + cx.px(12.0);
    // Where it's playing in the video as it'll be saved, and how long that is.
    let times = format!("{} / {}", crate::trim::clock((line(pos) - line(start)).clamp(0.0, kept)), crate::trim::clock(kept));
    let length = String::new();
    let (tw1, tw2) = (measure(&cx, &times, 13.0, 600), measure(&cx, &length, 12.0, 500));
    let right_edge = card.right - pad - cx.px(4.0);
    label(&cx, gpu, &length, rect(right_edge - tw2 - cx.px(2.0), row2, tw2 + cx.px(2.0), bh), 12.0, 500, Align::Left, p.dim, 1.0);
    let times_left = right_edge - tw2 - cx.px(10.0) - tw1;
    label(&cx, gpu, &times, rect(times_left - cx.px(2.0), row2, tw1 + cx.px(4.0), bh), 13.0, 600, Align::Left, p.text, 1.0);
    let picked = parts.iter().find(|x| Some(x.0) == selected).map(|x| x.3);
    let mut tools: Vec<(&str, Cmd, bool)> = vec![("Split", Cmd::TrimSplit, false)];
    match picked {
        Some(part) => {
            tools.push(("Delete", Cmd::TrimDelete, false));
            if sound && !mute {
                tools.push((if part.silent { "Sound back" } else { "Remove its sound" }, Cmd::TrimPartSound, part.silent));
            }
        }
        None if sound => tools.push((if mute { "Sound removed" } else { "Remove all sound" }, Cmd::TrimSound, mute)),
        None => {}
    }
    let mut x = card.left + pad;
    for (text, cmd, on) in tools {
        let w = bw(text);
        if x + w > times_left - gap {
            break;
        }
        let r = rect(x, row2, w, bh);
        plain_button(&cx, gpu, &app.ui, &p, r, text, Hit::Button(cmd));
        if on {
            gpu.stroke_round(r, bh / 2.0, 1.5, p.accent.d2d(1.0));
        }
        live(&mut app.ui.targets, r, Hit::Button(cmd));
        x = r.right + gap;
    }

    // The buttons: Reset on the left; Save, Save a copy and Cancel on the right.
    let row3 = row2 + bh + cx.px(8.0);
    let over = app.can_save_over();
    let main = if over { (Cmd::EditSave, "Save") } else { (Cmd::EditSaveCopy, "Save a copy") };
    let main_text = match progress {
        Some(f) => format!("Saving\u{2026} {}%", (f * 100.0).floor() as u32),
        None => main.1.to_string(),
    };
    let w = bw(&main_text).max(bw("Saving\u{2026} 100%"));
    let mr = rect(card.right - pad - w, row3, w, bh);
    let hit = Hit::Button(main.0);
    let a = if saving { 0.55 } else if app.ui.pressed == Some(hit) { 0.8 } else if app.ui.hover == Some(hit) { 0.9 } else { 1.0 };
    gpu.fill_round(mr, bh / 2.0, p.accent.d2d(a));
    label(&cx, gpu, &main_text, mr, 13.0, 600, Align::Center, p.accent.ink(), 1.0);
    live(&mut app.ui.targets, mr, hit);
    let mut x = mr.left - gap;
    let mut plain = Vec::new();
    if over {
        plain.push(("Save a copy", Cmd::EditSaveCopy));
    }
    plain.push(("Cancel", Cmd::EditCancel));
    for (text, cmd) in plain {
        let w = bw(text);
        let r = rect(x - w, row3, w, bh);
        plain_button(&cx, gpu, &app.ui, &p, r, text, Hit::Button(cmd));
        live(&mut app.ui.targets, r, Hit::Button(cmd));
        x = r.left - gap;
    }
    let reset = rect(card.left + pad, row3, bw("Reset"), bh);
    if reset.right < x {
        plain_button(&cx, gpu, &app.ui, &p, reset, "Reset", Hit::Button(Cmd::EditReset));
        live(&mut app.ui.targets, reset, Hit::Button(Cmd::EditReset));
        // What the keys do, where there's room.
        let hint = if has_splits { "S splits \u{00B7} click a part to pick it \u{00B7} Del deletes it \u{00B7} Ctrl+Z undoes" } else { "S splits where it's playing \u{00B7} Ctrl+Z undoes" };
        let hw = measure(&cx, hint, 12.0, 400);
        if reset.right + cx.px(14.0) + hw < x {
            label(&cx, gpu, hint, rect(reset.right + cx.px(14.0), row3, hw + cx.px(2.0), bh), 12.0, 400, Align::Left, p.dim, 1.0);
        }
    }
}
