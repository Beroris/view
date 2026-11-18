//! Video and music, through Windows' Media Foundation. Its Media Engine reads the file, decodes
//! (on the GPU where it can), plays the sound and keeps time; each new video frame is copied into
//! a texture on View's own device and drawn like a picture, so zoom, full screen and the glass
//! controls all work on it. Whatever Windows can play, View can: the codecs are the system's.

use crate::gfx::Gpu;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{BSTR, Interface, implement};

/// Posted to the window when the player has news: `wParam` is the Media Engine event, `lParam`
/// the player it's from, so news from one that's since been closed can be ignored.
pub const WM_MEDIA: u32 = WM_APP + 4;

static SERIAL: AtomicUsize = AtomicUsize::new(1);
static STARTUP: Once = Once::new();

#[implement(IMFMediaEngineNotify)]
struct Notify {
    hwnd: isize,
    serial: usize,
}

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, event: u32, _param1: usize, _param2: u32) -> windows::core::Result<()> {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_MEDIA, WPARAM(event as usize), LPARAM(self.serial as isize));
        }
        Ok(())
    }
}

pub struct Player {
    engine: IMFMediaEngine,
    ex: Option<IMFMediaEngineEx>,
    device: ID3D11Device,
    _manager: IMFDXGIDeviceManager,
    pub serial: usize,
    pub path: PathBuf,
    /// The texture frames are copied into, and Direct2D's view of it.
    frame: Option<(ID3D11Texture2D, ID2D1Bitmap1, u32, u32)>,
    last_pts: i64,
    has_frame: bool,
    /// The file's been opened and its length and size are known.
    pub ready: bool,
    pub ended: bool,
    pub error: Option<String>,
    /// Where the last frame step went, and when: steps quicker than the seeks count on from it.
    stepped: Option<(f64, std::time::Instant)>,
}

impl Player {
    /// Start playing a file. Events arrive at `hwnd` as [`WM_MEDIA`].
    pub fn open(path: &Path, device: &ID3D11Device, hwnd: HWND, autoplay: bool, volume: f64, muted: bool) -> windows::core::Result<Player> {
        STARTUP.call_once(|| unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        });
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        unsafe {
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(windows::core::Error::empty)?;
            manager.ResetDevice(device, token)?;

            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 3)?;
            let attrs = attrs.ok_or_else(windows::core::Error::empty)?;
            let notify: IMFMediaEngineNotify = Notify { hwnd: hwnd.0 as isize, serial }.into();
            attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager)?;
            attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
            attrs.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32)?;

            let factory: IMFMediaEngineClassFactory = CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
            let engine = factory.CreateInstance(0, &attrs)?;
            let ex = engine.cast::<IMFMediaEngineEx>().ok();
            let _ = engine.SetVolume(volume.clamp(0.0, 1.0));
            let _ = engine.SetMuted(muted);
            engine.SetAutoPlay(autoplay)?;
            engine.SetSource(&BSTR::from(path.to_string_lossy().as_ref()))?;
            Ok(Player { engine, ex, device: device.clone(), _manager: manager, serial, path: path.to_path_buf(), frame: None, last_pts: -1, has_frame: false, ready: false, ended: false, error: None, stepped: None })
        }
    }

    /// Take news from the engine.
    pub fn event(&mut self, event: u32) {
        let e = MF_MEDIA_ENGINE_EVENT(event as i32);
        if e == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA || e == MF_MEDIA_ENGINE_EVENT_CANPLAY {
            self.ready = true;
        } else if e == MF_MEDIA_ENGINE_EVENT_ENDED {
            // Media Foundation leaves a finished file "playing", so a seek back would set it
            // off again by itself. Paused is what the play button shows, so make it true.
            self.ended = true;
            unsafe {
                let _ = self.engine.Pause();
            }
        } else if e == MF_MEDIA_ENGINE_EVENT_PLAY || e == MF_MEDIA_ENGINE_EVENT_PLAYING || e == MF_MEDIA_ENGINE_EVENT_SEEKING {
            self.ended = false;
        } else if e == MF_MEDIA_ENGINE_EVENT_ERROR {
            self.error = Some(self.error_message());
        }
    }

    fn error_message(&self) -> String {
        let (code, extended) = match unsafe { self.engine.GetError() } {
            Ok(err) => unsafe { (err.GetErrorCode(), err.GetExtendedErrorCode().err().map(|e| e.code().0 as u32)) },
            Err(_) => (0, None),
        };
        const NO_CODEC: [u32; 3] = [0xC00D5212, 0xC00D36C4, 0xC00D36B4]; // topology codec not found, unsupported byte stream, invalid media type
        if code == MF_MEDIA_ENGINE_ERR_SRC_NOT_SUPPORTED.0 as u16 || extended.is_some_and(|x| NO_CODEC.contains(&x)) {
            "Windows can't play this file. It may need a codec from the Microsoft Store, such as HEVC, AV1 or VP9 Video Extensions.".into()
        } else if code == MF_MEDIA_ENGINE_ERR_ENCRYPTED.0 as u16 {
            "This file is copy-protected, so View can't play it.".into()
        } else if code == MF_MEDIA_ENGINE_ERR_DECODE.0 as u16 {
            "Windows couldn't decode this file. It may be damaged.".into()
        } else {
            "Windows couldn't play this file.".into()
        }
    }

    pub fn has_video(&self) -> bool {
        self.ready && unsafe { self.engine.HasVideo() }.as_bool()
    }

    /// The picture's size in pixels, once it's known.
    pub fn size(&self) -> Option<(u32, u32)> {
        if !self.has_video() {
            return None;
        }
        let (mut w, mut h) = (0u32, 0u32);
        unsafe { self.engine.GetNativeVideoSize(Some(&mut w), Some(&mut h)) }.ok()?;
        (w > 0 && h > 0).then_some((w, h))
    }

    /// The length in seconds, once known.
    pub fn duration(&self) -> Option<f64> {
        let d = unsafe { self.engine.GetDuration() };
        (d.is_finite() && d > 0.0).then_some(d)
    }

    pub fn position(&self) -> f64 {
        let t = unsafe { self.engine.GetCurrentTime() };
        if t.is_finite() { t.max(0.0) } else { 0.0 }
    }

    pub fn paused(&self) -> bool {
        unsafe { self.engine.IsPaused() }.as_bool()
    }

    /// Playing, so the window should keep painting.
    pub fn playing(&self) -> bool {
        self.ready && self.error.is_none() && !self.paused() && !self.ended
    }

    pub fn play(&mut self) {
        self.stepped = None;
        if self.ended {
            self.seek(0.0, true);
        }
        unsafe {
            let _ = self.engine.Play();
        }
    }

    pub fn pause(&mut self) {
        unsafe {
            let _ = self.engine.Pause();
        }
    }

    pub fn toggle(&mut self) {
        if self.paused() || self.ended { self.play() } else { self.pause() }
    }

    /// Go to `t` seconds. Rough is quicker, to the nearest key frame: for dragging.
    pub fn seek(&mut self, t: f64, exact: bool) {
        self.stepped = None;
        let t = match self.duration() {
            Some(d) => t.clamp(0.0, d),
            None => t.max(0.0),
        };
        unsafe {
            match (&self.ex, exact) {
                (Some(ex), false) => {
                    let _ = ex.SetCurrentTimeEx(t, MF_MEDIA_ENGINE_SEEK_MODE_APPROXIMATE);
                }
                _ => {
                    let _ = self.engine.SetCurrentTime(t);
                }
            }
        }
        self.ended = false;
    }

    /// Play at `rate` times normal speed: 1 is normal. Kept through seeks and replays.
    pub fn set_rate(&mut self, rate: f64) {
        unsafe {
            let _ = self.engine.SetDefaultPlaybackRate(rate);
            let _ = self.engine.SetPlaybackRate(rate);
        }
    }

    pub fn set_volume(&mut self, v: f64, muted: bool) {
        unsafe {
            let _ = self.engine.SetVolume(v.clamp(0.0, 1.0));
            let _ = self.engine.SetMuted(muted);
        }
    }

    /// The latest frame, copied over if the engine has a new one.
    pub fn frame(&mut self, gpu: &Gpu) -> Option<&ID2D1Bitmap1> {
        let (w, h) = self.size()?;
        if self.frame.as_ref().is_none_or(|f| (f.2, f.3) != (w, h)) {
            self.frame = self.make_texture(gpu, w, h);
            self.has_frame = false;
            self.last_pts = -1;
        }
        let (texture, _, _, _) = self.frame.as_ref()?;
        if let Ok(pts) = unsafe { self.engine.OnVideoStreamTick() } {
            if pts != self.last_pts || !self.has_frame {
                let dst = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
                let black = MFARGB { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbAlpha: 255 };
                if unsafe { self.engine.TransferVideoFrame(texture, None, &dst, Some(&black)) }.is_ok() {
                    self.last_pts = pts;
                    self.has_frame = true;
                }
            }
        }
        if self.has_frame { self.frame.as_ref().map(|f| &f.1) } else { None }
    }

    /// Pause and move one frame on or back.
    pub fn step_frame(&mut self, forward: bool) {
        if !self.has_video() {
            return;
        }
        self.pause();
        self.ended = false;
        // An exact seek one frame on or back. (The engine's own FrameStep only goes forward, and
        // seeks straight after it can be lost.)
        let frame = 1.0 / self.frame_rate().unwrap_or(30.0);
        let from = match self.stepped {
            Some((t, at)) if at.elapsed().as_millis() < 400 => t,
            _ => self.position(),
        };
        let t = (from + if forward { frame } else { -frame }).clamp(0.0, self.duration().unwrap_or(f64::MAX));
        self.seek(t, true);
        self.stepped = Some((t, std::time::Instant::now()));
    }

    /// Frames a second, from the video stream's type.
    fn frame_rate(&self) -> Option<f64> {
        let ex = self.ex.as_ref()?;
        let streams = unsafe { ex.GetNumberOfStreams() }.ok()?;
        for i in 0..streams.min(16) {
            let Ok(rate) = (unsafe { ex.GetStreamAttribute(i, &MF_MT_FRAME_RATE) }) else { continue };
            let Ok(packed) = u64::try_from(&rate) else { continue };
            let (num, den) = ((packed >> 32) as f64, (packed & 0xFFFF_FFFF) as f64);
            if num > 0.0 && den > 0.0 && (1.0..=1000.0).contains(&(num / den)) {
                return Some(num / den);
            }
        }
        None
    }

    /// The frame on screen as premultiplied BGRA pixels, read back from the GPU.
    pub fn frame_pixels(&self) -> Option<(u32, u32, Vec<u8>)> {
        if !self.has_frame {
            return None;
        }
        let (texture, _, w, h) = self.frame.as_ref()?;
        let (w, h) = (*w, *h);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        unsafe {
            let mut staging = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut staging)).ok()?;
            let staging = staging?;
            let context = self.device.GetImmediateContext().ok()?;
            context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
            let row = w as usize * 4;
            let mut out = Vec::with_capacity(row * h as usize);
            for y in 0..h as usize {
                let src = std::slice::from_raw_parts((mapped.pData as *const u8).add(y * mapped.RowPitch as usize), row);
                out.extend_from_slice(src);
            }
            context.Unmap(&staging, 0);
            // Video has no transparency: make sure every pixel says so.
            for px in out.chunks_exact_mut(4) {
                px[3] = 255;
            }
            Some((w, h, out))
        }
    }

    fn make_texture(&self, gpu: &Gpu, w: u32, h: u32) -> Option<(ID3D11Texture2D, ID2D1Bitmap1, u32, u32)> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.ok()?;
        let texture = texture?;
        let bitmap = gpu.wrap_texture(&texture)?;
        Some((texture, bitmap, w, h))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        unsafe {
            let _ = self.engine.Shutdown();
        }
    }
}

/// "1:05", "12:40", "1:02:03".
pub fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0).floor() as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}
