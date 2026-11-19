//! Drawing, on the GPU through Direct2D: a Direct3D 11 device, a flip-model swap chain on the
//! window, and a Direct2D context drawing into it. Images become GPU bitmaps once and are then
//! scaled and turned by the GPU every frame, which is what keeps zooming smooth at any size.
//!
//! If there's no usable GPU, Direct3D's software rasteriser (WARP) stands in.

use view_core::picture::Bitmap;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D};
use windows::Win32::Graphics::DirectWrite::IDWriteTextLayout;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;
use windows_numerics::{Matrix3x2, Vector2};

/// Make the Direct3D device. Loading the driver is the slow part of starting, so this runs on
/// its own thread while the window is made and the first image decodes.
pub fn create_d3d() -> Option<ID3D11Device> {
    crate::util::trace("device: starting");
    let make = |kind: D3D_DRIVER_TYPE, flags: D3D11_CREATE_DEVICE_FLAG| {
        let mut device = None;
        unsafe { D3D11CreateDevice(None, kind, HMODULE::default(), flags, None, D3D11_SDK_VERSION, Some(&mut device), None, None) }
            .ok()
            .and(device)
    };
    // Video support lets Media Foundation decode on the GPU, into textures this device can draw.
    let video = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let device = make(D3D_DRIVER_TYPE_HARDWARE, video)
        .or_else(|| make(D3D_DRIVER_TYPE_HARDWARE, D3D11_CREATE_DEVICE_BGRA_SUPPORT))
        .or_else(|| make(D3D_DRIVER_TYPE_WARP, video))
        .or_else(|| make(D3D_DRIVER_TYPE_WARP, D3D11_CREATE_DEVICE_BGRA_SUPPORT))?;
    crate::util::trace("device: made");
    // Media Foundation's decoding threads share the device with drawing.
    if let Ok(mt) = unsafe { device.GetImmediateContext() }.and_then(|c| c.cast::<ID3D11Multithread>()) {
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    Some(device)
}

pub fn color(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a }
}

pub fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

/// An image on the GPU, in tiles when it's bigger than the GPU's largest bitmap.
pub struct Texture {
    pub width: u32,
    pub height: u32,
    tiles: Vec<Tile>,
}

struct Tile {
    bitmap: ID2D1Bitmap1,
    /// The part of the image this tile shows, in image pixels.
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    /// Where that part starts inside the tile's bitmap, past the overlap.
    ox: u32,
    oy: u32,
}

/// Neighbouring tiles share this many pixels, so smoothing at their seams samples real pixels.
const OVERLAP: u32 = 2;

pub struct Gpu {
    pub dc: ID2D1DeviceContext,
    swap: IDXGISwapChain1,
    target: Option<ID2D1Bitmap1>,
    brush: ID2D1SolidColorBrush,
    checker: Option<(ID2D1BitmapBrush1, [u8; 6], u32)>,
    pub width: u32,
    pub height: u32,
    max_bitmap: u32,
    /// The image layer, drawn here first when glass panels need something to blur.
    scene: Option<ID2D1Bitmap1>,
    /// Whether this frame (or, before `begin`, the last one) was drawn into `scene`.
    scene_used: std::cell::Cell<bool>,
    /// A copy of a scene, kept to fade away over the next screen.
    kept: Option<ID2D1Bitmap1>,
    /// Scene → clamp the edges → blur, made once, and the blur's current strength.
    blur: Option<(ID2D1Effect, ID2D1Effect, f32)>,
}

impl Gpu {
    pub fn new(hwnd: HWND, d3d: &ID3D11Device, width: u32, height: u32) -> windows::core::Result<Gpu> {
        unsafe {
            let dxgi: IDXGIDevice = d3d.cast()?;
            let options = D2D1_FACTORY_OPTIONS { debugLevel: D2D1_DEBUG_LEVEL_NONE };
            // Multithreaded, because video decodes into this device from Media Foundation's threads.
            let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, Some(&options))?;
            let device = factory.CreateDevice(&dxgi)?;
            let dc = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            // Work in physical pixels: View scales its own interface for the display.
            dc.SetDpi(96.0, 96.0);
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            let adapter = dxgi.GetAdapter()?;
            let dxgi_factory: IDXGIFactory2 = adapter.GetParent()?;
            let mut desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_NONE,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: 0,
                ..Default::default()
            };
            let swap = dxgi_factory.CreateSwapChainForHwnd(d3d, hwnd, &desc, None, None).or_else(|_| {
                // Before Windows 10.
                desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
                dxgi_factory.CreateSwapChainForHwnd(d3d, hwnd, &desc, None, None)
            })?;
            // Alt+Enter is View's own business, not DXGI's.
            let _ = dxgi_factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES);
            let brush = dc.CreateSolidColorBrush(&color(255, 255, 255, 1.0), None)?;
            let max_bitmap = dc.GetMaximumBitmapSize().max(2048);
            let mut gpu = Gpu { dc, swap, target: None, brush, checker: None, width: width.max(1), height: height.max(1), max_bitmap, scene: None, scene_used: std::cell::Cell::new(false), kept: None, blur: None };
            gpu.attach()?;
            Ok(gpu)
        }
    }

    fn attach(&mut self) -> windows::core::Result<()> {
        unsafe {
            let surface: IDXGISurface = self.swap.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: std::mem::ManuallyDrop::new(None),
            };
            let target = self.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            self.dc.SetTarget(&target);
            self.target = Some(target);
        }
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) -> windows::core::Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        self.scene = None;
        self.kept = None;
        self.scene_used.set(false);
        if let Some((border, _, _)) = &self.blur {
            unsafe { border.SetInput(0, None::<&ID2D1Image>, true) };
        }
        unsafe {
            self.dc.SetTarget(None);
            self.target = None;
            self.swap.ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
        }
        self.width = width;
        self.height = height;
        self.attach()
    }

    pub fn begin(&self) {
        self.scene_used.set(false);
        unsafe {
            self.dc.BeginDraw();
            self.dc.SetTransform(&Matrix3x2::identity());
        }
    }

    /// Finish drawing and show it. An error means the device was lost and must be remade.
    /// False when nothing was shown because the window is covered or off screen: then showing
    /// doesn't wait for the display, and the caller has to pace itself.
    pub fn end(&self) -> windows::core::Result<bool> {
        const DXGI_STATUS_OCCLUDED: i32 = 0x087A0001;
        unsafe {
            self.dc.EndDraw(None, None)?;
            let shown = self.swap.Present(1, DXGI_PRESENT(0));
            shown.ok()?;
            Ok(shown.0 != DXGI_STATUS_OCCLUDED)
        }
    }

    /// Draw into the scene layer instead of the window, so glass panels can blur it later.
    /// False if the layer can't be made; drawing then goes straight to the window.
    pub fn begin_scene(&mut self) -> bool {
        if self.scene.is_none() {
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
                colorContext: std::mem::ManuallyDrop::new(None),
            };
            self.scene = unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: self.width, height: self.height }, None, 0, &props) }.ok();
            if let (Some(scene), Some((border, _, _))) = (&self.scene, &self.blur) {
                unsafe { border.SetInput(0, scene, true) };
            }
        }
        let Some(scene) = &self.scene else { return false };
        unsafe { self.dc.SetTarget(scene) };
        self.scene_used.set(true);
        true
    }

    /// Keep a copy of the last frame's scene, for `draw_kept`. False if the last frame didn't
    /// draw one.
    pub fn keep_scene(&mut self) -> bool {
        let Some(scene) = self.scene.as_ref().filter(|_| self.scene_used.get()) else {
            self.kept = None;
            return false;
        };
        if self.kept.is_none() {
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                colorContext: std::mem::ManuallyDrop::new(None),
            };
            self.kept = unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: self.width, height: self.height }, None, 0, &props) }.ok();
        }
        let Some(kept) = &self.kept else { return false };
        if unsafe { kept.CopyFromBitmap(None, scene, None) }.is_err() {
            self.kept = None;
            return false;
        }
        true
    }

    /// The kept scene, at `opacity`, grown or shrunk about the middle by `scale`. False if
    /// there's none.
    pub fn draw_kept(&self, opacity: f32, scale: f32) -> bool {
        let Some(kept) = &self.kept else { return false };
        let (w, h) = (self.width as f32, self.height as f32);
        let (dw, dh) = (w * scale, h * scale);
        let dest = rect((w - dw) / 2.0, (h - dh) / 2.0, dw, dh);
        unsafe {
            let rt: &ID2D1RenderTarget = &self.dc;
            rt.DrawBitmap(kept, Some(&dest), opacity, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
        }
        true
    }

    /// Done with the kept scene.
    pub fn drop_kept(&mut self) {
        self.kept = None;
    }

    /// Back to the window, with the scene drawn into it.
    pub fn end_scene(&mut self) {
        let (Some(scene), Some(target)) = (&self.scene, &self.target) else { return };
        unsafe {
            self.dc.SetTarget(target);
            self.dc.SetTransform(&Matrix3x2::identity());
            self.dc.DrawImage(scene, None, None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_COPY);
        }
    }

    /// Frosted glass: the scene under `r`, blurred, inside a rounded rectangle. Draw a tint over
    /// it afterwards. Does nothing if there's no scene this frame.
    pub fn glass(&mut self, r: D2D_RECT_F, radius: f32, sigma: f32, opacity: f32) {
        let Some(scene) = &self.scene else { return };
        if self.blur.is_none() {
            let made = unsafe {
                (|| -> windows::core::Result<(ID2D1Effect, ID2D1Effect)> {
                    let border = self.dc.CreateEffect(&CLSID_D2D1Border)?;
                    let clamp = (D2D1_BORDER_EDGE_MODE_CLAMP.0 as u32).to_le_bytes();
                    border.SetValue(D2D1_BORDER_PROP_EDGE_MODE_X.0 as u32, D2D1_PROPERTY_TYPE_ENUM, &clamp)?;
                    border.SetValue(D2D1_BORDER_PROP_EDGE_MODE_Y.0 as u32, D2D1_PROPERTY_TYPE_ENUM, &clamp)?;
                    border.SetInput(0, scene, true);
                    let blur = self.dc.CreateEffect(&CLSID_D2D1GaussianBlur)?;
                    let hard = (D2D1_BORDER_MODE_HARD.0 as u32).to_le_bytes();
                    blur.SetValue(D2D1_GAUSSIANBLUR_PROP_BORDER_MODE.0 as u32, D2D1_PROPERTY_TYPE_ENUM, &hard)?;
                    let speed = (D2D1_GAUSSIANBLUR_OPTIMIZATION_SPEED.0 as u32).to_le_bytes();
                    blur.SetValue(D2D1_GAUSSIANBLUR_PROP_OPTIMIZATION.0 as u32, D2D1_PROPERTY_TYPE_ENUM, &speed)?;
                    blur.SetInput(0, &border.GetOutput()?, true);
                    Ok((border, blur))
                })()
            };
            let Ok((border, blur)) = made else { return };
            self.blur = Some((border, blur, -1.0));
        }
        let Some((_, blur, current)) = &mut self.blur else { return };
        unsafe {
            if (*current - sigma).abs() > 0.01 {
                let _ = blur.SetValue(D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32, D2D1_PROPERTY_TYPE_FLOAT, &sigma.to_le_bytes());
                *current = sigma;
            }
            let Ok(out) = blur.GetOutput() else { return };
            let props = D2D1_IMAGE_BRUSH_PROPERTIES {
                sourceRectangle: r,
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR,
            };
            let Ok(brush) = self.dc.CreateImageBrush(&out, &props, None) else { return };
            brush.SetOpacity(opacity);
            brush.SetTransform(&Matrix3x2::translation(r.left, r.top));
            let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
            self.dc.FillRoundedRectangle(&rr, &brush);
        }
    }

    /// A vertical gradient filling `r`, from `top` to `bottom`.
    pub fn fill_vgradient(&self, r: D2D_RECT_F, top: D2D1_COLOR_F, bottom: D2D1_COLOR_F) {
        self.fill_gradient(r, (0.0, r.top), (0.0, r.bottom), top, bottom);
    }

    /// Fill `r` with a gradient from `a` at `from` to `b` at `to`, solid beyond either end.
    pub fn fill_gradient(&self, r: D2D_RECT_F, from: (f32, f32), to: (f32, f32), a: D2D1_COLOR_F, b: D2D1_COLOR_F) {
        unsafe {
            let stops = [D2D1_GRADIENT_STOP { position: 0.0, color: a }, D2D1_GRADIENT_STOP { position: 1.0, color: b }];
            let rt: &ID2D1RenderTarget = &self.dc;
            let Ok(collection) = rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else { return };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: Vector2 { X: from.0, Y: from.1 }, endPoint: Vector2 { X: to.0, Y: to.1 } };
            let Ok(brush) = rt.CreateLinearGradientBrush(&props, None, &collection) else { return };
            self.dc.FillRectangle(&r, &brush);
        }
    }

    pub fn clear(&self, c: D2D1_COLOR_F) {
        unsafe { self.dc.Clear(Some(&c)) }
    }

    pub fn set_transform(&self, m: &Matrix3x2) {
        unsafe { self.dc.SetTransform(m) }
    }

    pub fn reset_transform(&self) {
        unsafe { self.dc.SetTransform(&Matrix3x2::identity()) }
    }

    fn paint(&self, c: D2D1_COLOR_F) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&c) };
        &self.brush
    }

    pub fn fill(&self, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        unsafe { self.dc.FillRectangle(&r, self.paint(c)) }
    }

    pub fn fill_round(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
        unsafe { self.dc.FillRoundedRectangle(&rr, self.paint(c)) }
    }

    pub fn stroke_round(&self, r: D2D_RECT_F, radius: f32, width: f32, c: D2D1_COLOR_F) {
        let h = width / 2.0;
        let r = D2D_RECT_F { left: r.left + h, top: r.top + h, right: r.right - h, bottom: r.bottom - h };
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
        unsafe { self.dc.DrawRoundedRectangle(&rr, self.paint(c), width, None) }
    }

    pub fn fill_circle(&self, x: f32, y: f32, r: f32, c: D2D1_COLOR_F) {
        let e = D2D1_ELLIPSE { point: Vector2 { X: x, Y: y }, radiusX: r, radiusY: r };
        unsafe { self.dc.FillEllipse(&e, self.paint(c)) }
    }

    pub fn fill_ellipse(&self, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        let e = D2D1_ELLIPSE { point: Vector2 { X: (r.left + r.right) / 2.0, Y: (r.top + r.bottom) / 2.0 }, radiusX: (r.right - r.left) / 2.0, radiusY: (r.bottom - r.top) / 2.0 };
        unsafe { self.dc.FillEllipse(&e, self.paint(c)) }
    }

    pub fn stroke_ellipse(&self, r: D2D_RECT_F, width: f32, c: D2D1_COLOR_F) {
        let e = D2D1_ELLIPSE { point: Vector2 { X: (r.left + r.right) / 2.0, Y: (r.top + r.bottom) / 2.0 }, radiusX: (r.right - r.left) / 2.0, radiusY: (r.bottom - r.top) / 2.0 };
        unsafe { self.dc.DrawEllipse(&e, self.paint(c), width, None) }
    }

    pub fn line(&self, from: (f32, f32), to: (f32, f32), width: f32, c: D2D1_COLOR_F) {
        unsafe { self.dc.DrawLine(Vector2 { X: from.0, Y: from.1 }, Vector2 { X: to.0, Y: to.1 }, self.paint(c), width, None) }
    }

    /// Part of a small texture (one tile), `src` in its pixels, drawn into `dest`.
    pub fn draw_texture_part(&self, t: &Texture, dest: D2D_RECT_F, src: D2D_RECT_F, opacity: f32) {
        let Some(tile) = t.tiles.first() else { return };
        unsafe { self.dc.DrawBitmap(&tile.bitmap, Some(&dest), opacity, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, Some(&src), None) };
    }

    pub fn text(&self, layout: &IDWriteTextLayout, x: f32, y: f32, c: D2D1_COLOR_F) {
        unsafe { self.dc.DrawTextLayout(Vector2 { X: x, Y: y }, layout, self.paint(c), D2D1_DRAW_TEXT_OPTIONS_NONE) }
    }

    pub fn clip(&self, r: D2D_RECT_F) {
        unsafe { self.dc.PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_ALIASED) }
    }

    pub fn unclip(&self) {
        unsafe { self.dc.PopAxisAlignedClip() }
    }

    /// A checkerboard filling `r`, its squares `cell` pixels across, in two shades.
    pub fn checkerboard(&mut self, r: D2D_RECT_F, cell: u32, a: (u8, u8, u8), b: (u8, u8, u8), opacity: f32) {
        let key = [a.0, a.1, a.2, b.0, b.1, b.2];
        let cell = cell.max(2);
        if self.checker.as_ref().is_none_or(|(_, k, c)| *k != key || *c != cell) {
            self.checker = self.make_checker(cell, a, b).map(|brush| (brush, key, cell));
        }
        let Some((brush, _, _)) = &self.checker else { return };
        unsafe {
            brush.SetTransform(&Matrix3x2::translation(r.left.round(), r.top.round()));
            brush.SetOpacity(opacity);
            self.dc.FillRectangle(&r, brush);
        }
    }

    fn make_checker(&self, cell: u32, a: (u8, u8, u8), b: (u8, u8, u8)) -> Option<ID2D1BitmapBrush1> {
        let n = cell * 2;
        let mut data = vec![0u8; (n * n * 4) as usize];
        for y in 0..n {
            for x in 0..n {
                let (r, g, bl) = if (x < cell) == (y < cell) { a } else { b };
                let i = ((y * n + x) * 4) as usize;
                data[i..i + 4].copy_from_slice(&[bl, g, r, 255]);
            }
        }
        let bitmap = self.make_bitmap(n, n, &data, n * 4)?;
        let props = D2D1_BITMAP_BRUSH_PROPERTIES1 {
            extendModeX: D2D1_EXTEND_MODE_WRAP,
            extendModeY: D2D1_EXTEND_MODE_WRAP,
            interpolationMode: D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
        };
        unsafe { self.dc.CreateBitmapBrush(&bitmap, Some(&props), None) }.ok()
    }

    fn make_bitmap(&self, w: u32, h: u32, data: &[u8], pitch: u32) -> Option<ID2D1Bitmap1> {
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(data.as_ptr() as *const _), pitch, &props) }.ok()
    }

    /// Put a bitmap on the GPU. Bitmaps bigger than the GPU allows are cut into tiles.
    pub fn upload(&self, b: &Bitmap) -> Option<Texture> {
        let (w, h) = (b.width, b.height);
        if w == 0 || h == 0 {
            return None;
        }
        let pitch = w * 4;
        if w <= self.max_bitmap && h <= self.max_bitmap {
            let bitmap = self.make_bitmap(w, h, &b.data, pitch)?;
            return Some(Texture { width: w, height: h, tiles: vec![Tile { bitmap, x: 0, y: 0, w, h, ox: 0, oy: 0 }] });
        }
        let step = (self.max_bitmap - 2 * OVERLAP).min(8192);
        let mut tiles = Vec::new();
        let mut y = 0;
        while y < h {
            let th = step.min(h - y);
            let mut x = 0;
            while x < w {
                let tw = step.min(w - x);
                // The tile's bitmap reaches OVERLAP pixels past its part on every side it can.
                let (bx, by) = (x.saturating_sub(OVERLAP), y.saturating_sub(OVERLAP));
                let (bx1, by1) = ((x + tw + OVERLAP).min(w), (y + th + OVERLAP).min(h));
                let start = (by * pitch + bx * 4) as usize;
                let bitmap = self.make_bitmap(bx1 - bx, by1 - by, &b.data[start..], pitch)?;
                tiles.push(Tile { bitmap, x, y, w: tw, h: th, ox: x - bx, oy: y - by });
                x += tw;
            }
            y += th;
        }
        Some(Texture { width: w, height: h, tiles })
    }

    /// A Direct2D bitmap over a Direct3D texture, sharing its memory: for video frames.
    pub fn wrap_texture(&self, t: &ID3D11Texture2D) -> Option<ID2D1Bitmap1> {
        self.wrap(t, D2D1_ALPHA_MODE_IGNORE)
    }

    /// The same, for a texture with see-through parts: a 3D model on nothing.
    pub fn wrap_texture_premultiplied(&self, t: &ID3D11Texture2D) -> Option<ID2D1Bitmap1> {
        self.wrap(t, D2D1_ALPHA_MODE_PREMULTIPLIED)
    }

    fn wrap(&self, t: &ID3D11Texture2D, alpha: D2D1_ALPHA_MODE) -> Option<ID2D1Bitmap1> {
        let surface: IDXGISurface = t.cast().ok()?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: alpha },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        unsafe { self.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props)) }.ok()
    }

    pub fn draw_bitmap(&self, b: &ID2D1Bitmap1, dest: D2D_RECT_F, mode: D2D1_INTERPOLATION_MODE, opacity: f32) {
        unsafe { self.dc.DrawBitmap(b, Some(&dest), opacity, mode, None, None) };
    }

    /// A brush of its own, for colouring parts of a text layout.
    pub fn brush(&self, c: D2D1_COLOR_F) -> Option<ID2D1SolidColorBrush> {
        unsafe { self.dc.CreateSolidColorBrush(&c, None) }.ok()
    }

    /// A small texture (one tile) in a rounded rectangle, filling `r`: album art.
    pub fn draw_texture_round(&self, t: &Texture, r: D2D_RECT_F, radius: f32, opacity: f32) {
        let Some(tile) = t.tiles.first() else { return };
        let props = D2D1_BITMAP_BRUSH_PROPERTIES1 { extendModeX: D2D1_EXTEND_MODE_CLAMP, extendModeY: D2D1_EXTEND_MODE_CLAMP, interpolationMode: D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC };
        let Ok(brush) = (unsafe { self.dc.CreateBitmapBrush(&tile.bitmap, Some(&props), None) }) else { return };
        let (sx, sy) = ((r.right - r.left) / t.width as f32, (r.bottom - r.top) / t.height as f32);
        unsafe {
            brush.SetTransform(&(Matrix3x2::scale(sx, sy) * Matrix3x2::translation(r.left, r.top)));
            brush.SetOpacity(opacity);
            let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
            self.dc.FillRoundedRectangle(&rr, &brush);
        }
    }

    /// A small texture (one tile) cut to fill `r` without stretching, in a rounded rectangle:
    /// what sticks out past `r` either way is cut off. Thumbnails.
    pub fn draw_texture_cover(&self, t: &Texture, r: D2D_RECT_F, radius: f32, opacity: f32) {
        let Some(tile) = t.tiles.first() else { return };
        let props = D2D1_BITMAP_BRUSH_PROPERTIES1 { extendModeX: D2D1_EXTEND_MODE_CLAMP, extendModeY: D2D1_EXTEND_MODE_CLAMP, interpolationMode: D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC };
        let Ok(brush) = (unsafe { self.dc.CreateBitmapBrush(&tile.bitmap, Some(&props), None) }) else { return };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let k = (w / t.width.max(1) as f32).max(h / t.height.max(1) as f32);
        let (x, y) = (r.left + (w - t.width as f32 * k) / 2.0, r.top + (h - t.height as f32 * k) / 2.0);
        unsafe {
            brush.SetTransform(&(Matrix3x2::scale(k, k) * Matrix3x2::translation(x, y)));
            brush.SetOpacity(opacity);
            let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
            self.dc.FillRoundedRectangle(&rr, &brush);
        }
    }

    /// Draw a texture so that it covers `dest` (in the current transform's space).
    pub fn draw_texture(&self, t: &Texture, dest: D2D_RECT_F, mode: D2D1_INTERPOLATION_MODE, opacity: f32) {
        let (sx, sy) = ((dest.right - dest.left) / t.width as f32, (dest.bottom - dest.top) / t.height as f32);
        for tile in &t.tiles {
            let d = D2D_RECT_F {
                left: dest.left + tile.x as f32 * sx,
                top: dest.top + tile.y as f32 * sy,
                right: dest.left + (tile.x + tile.w) as f32 * sx,
                bottom: dest.top + (tile.y + tile.h) as f32 * sy,
            };
            let s = D2D_RECT_F {
                left: tile.ox as f32,
                top: tile.oy as f32,
                right: (tile.ox + tile.w) as f32,
                bottom: (tile.oy + tile.h) as f32,
            };
            unsafe { self.dc.DrawBitmap(&tile.bitmap, Some(&d), opacity, mode, Some(&s), None) };
        }
    }
}
