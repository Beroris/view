//! Decoding through the Windows Imaging Component: the codecs Windows ships (JPEG, PNG, TIFF,
//! BMP, ICO, JPEG XR, DDS) and the ones Microsoft Store extensions add (WebP, HEIF, AVIF, RAW,
//! JPEG XL).
//!
//! Each worker thread keeps its own factory. Images are decoded straight to premultiplied BGRA,
//! shrunk on the way when asked: a JPEG decoder shrinks while it decodes, which is what makes a
//! screen-sized decode of a big photo several times quicker than a full one.

use crate::util::{Wide, from_wide};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use view_core::formats::Format;
use view_core::orient::Orient;
use view_core::picture::{Bitmap, Details, Frame, Picture, fit_within};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION, GENERIC_READ, WINCODEC_ERR_BADHEADER,
    WINCODEC_ERR_BADIMAGE, WINCODEC_ERR_COMPONENTNOTFOUND, WINCODEC_ERR_STREAMNOTAVAILABLE, WINCODEC_ERR_STREAMREAD,
    WINCODEC_ERR_UNKNOWNIMAGEFORMAT,
};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IEnumUnknown};
use windows::Win32::System::Variant::{VT_UI1, VT_UI2, VT_UI4};
use windows::core::{GUID, HRESULT, IUnknown, Interface, w};

/// How to decode.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    /// Fit inside this size, 0 for full resolution.
    pub max_width: u32,
    pub max_height: u32,
    /// Never keep more pixels than this; bigger images are shrunk to fit.
    pub max_pixels: u64,
    /// How many times the target's pixels a bitmap may have before it's worth shrinking on the
    /// CPU. Above 1, a nearly screen-sized image skips the scaler and the GPU shrinks it as it
    /// draws, which is quicker and looks as good.
    pub slack: f64,
}

#[derive(Debug)]
pub enum Failure {
    /// Windows has no codec for this file. The format, if the file's bytes say what it is.
    NoCodec(Option<Format>),
    NotFound,
    Denied,
    Cancelled,
    /// Anything else, in words for people.
    Broken(String),
}

impl Failure {
    pub fn message(&self) -> String {
        match self {
            Failure::NoCodec(Some(f)) => match extension_for(*f) {
                Some(store) => format!("Windows can't open {} images yet. Install \"{store}\" from the Microsoft Store.", f.name()),
                None => format!("Windows has no codec for {} images.", f.name()),
            },
            Failure::NoCodec(None) => "This isn't an image View can open.".into(),
            Failure::NotFound => "The file isn't there any more.".into(),
            Failure::Denied => "Windows won't let View read this file.".into(),
            Failure::Cancelled => "Cancelled.".into(),
            Failure::Broken(s) => s.clone(),
        }
    }
}

/// The Microsoft Store extension that adds a format to Windows.
pub fn extension_for(f: Format) -> Option<&'static str> {
    match f {
        Format::WebP => Some("WebP Image Extensions"),
        Format::Heif => Some("HEIF Image Extensions\" and \"HEVC Video Extensions"),
        Format::Avif => Some("AV1 Video Extension"),
        Format::JpegXl => Some("JPEG XL Image Extension"),
        Format::Raw => Some("Raw Image Extension"),
        _ => None,
    }
}

/// Whether the Store extensions a format needs are installed, for formats that need one beyond
/// what Windows lists as a decoder. Windows registers decoders for HEIF, AVIF and JPEG XL that
/// open the file but can't decode its pixels until the extension is there, so a listed decoder
/// alone doesn't mean the format opens. `None`: the format needs nothing extra.
pub fn extension_installed(f: Format, packages: &[String]) -> Option<bool> {
    let has = |prefix: &str| packages.iter().any(|p| p.starts_with(prefix));
    match f {
        Format::Heif => Some(has("microsoft.heifimageextension_") && has("microsoft.hevcvideoextension")),
        Format::Avif => Some(has("microsoft.av1videoextension_")),
        Format::JpegXl => Some(packages.iter().any(|p| p.contains("jpegxl") || p.contains("jxl"))),
        _ => None,
    }
}

/// The Store packages installed for this user, lower case: "microsoft.heifimageextension_1.2…".
pub fn store_packages() -> Vec<String> {
    use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW};
    let mut out = Vec::new();
    let mut key = HKEY::default();
    let path = w!(r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages");
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path, None, KEY_READ, &mut key) }.is_err() {
        return out;
    }
    for i in 0.. {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        if unsafe { RegEnumKeyExW(key, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) }.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]).to_ascii_lowercase());
    }
    unsafe {
        let _ = RegCloseKey(key);
    }
    out
}

fn failure(e: windows::core::Error) -> Failure {
    let code = e.code();
    if code == WINCODEC_ERR_COMPONENTNOTFOUND {
        return Failure::NoCodec(None);
    }
    if code == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0) || code == HRESULT::from_win32(ERROR_PATH_NOT_FOUND.0) {
        return Failure::NotFound;
    }
    if code == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) || code == HRESULT::from_win32(ERROR_SHARING_VIOLATION.0) {
        return Failure::Denied;
    }
    // HEIF without the HEVC extension, AVIF without AV1: the container opens, the pixels don't.
    if code.0 as u32 == 0xC00D5212 || code.0 as u32 == 0x88982F8B {
        return Failure::NoCodec(None);
    }
    if code == WINCODEC_ERR_BADHEADER || code == WINCODEC_ERR_BADIMAGE || code == WINCODEC_ERR_UNKNOWNIMAGEFORMAT {
        return Failure::Broken("The file is damaged or isn't really an image.".into());
    }
    if code == WINCODEC_ERR_STREAMREAD || code == WINCODEC_ERR_STREAMNOTAVAILABLE || code.0 as u32 == 0x88982F72 {
        return Failure::Broken("The file ends early: it may still be downloading or copying.".into());
    }
    if code.0 as u32 == 0x8007000E {
        return Failure::Broken("Not enough memory to open this image.".into());
    }
    Failure::Broken(format!("Windows couldn't decode this image ({:#010x}).", code.0 as u32))
}

pub struct Wic {
    factory: IWICImagingFactory,
}

impl Wic {
    /// A factory for this thread. COM must already be initialised on it.
    pub fn new() -> Option<Wic> {
        let factory = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }.ok()?;
        Some(Wic { factory })
    }

    /// The file extensions Windows has decoders for, lower case, without dots.
    pub fn extensions(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(list): windows::core::Result<IEnumUnknown> =
            (unsafe { self.factory.CreateComponentEnumerator(WICDecoder.0 as u32, WICComponentEnumerateDefault.0 as u32) })
        else {
            return out;
        };
        loop {
            let mut item = [None];
            let mut got = 0u32;
            if unsafe { list.Next(&mut item, Some(&mut got)) }.is_err() || got == 0 {
                break;
            }
            let Some(unknown): Option<IUnknown> = item[0].take() else { break };
            let Ok(info) = unknown.cast::<IWICBitmapDecoderInfo>() else { continue };
            let mut len = 0u32;
            let _ = unsafe { info.GetFileExtensions(&mut [], &mut len) };
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize];
            if unsafe { info.GetFileExtensions(&mut buf, &mut len) }.is_err() {
                continue;
            }
            for ext in from_wide(&buf).split(',') {
                let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
                if !ext.is_empty() && !out.contains(&ext) {
                    out.push(ext);
                }
            }
        }
        out
    }

    /// Decode `path`. `sniffed` is what its first bytes say it is, for messages.
    pub fn decode(&self, path: &Path, sniffed: Option<Format>, req: Request, cancel: &AtomicBool) -> Result<Picture, Failure> {
        let name = Wide::path(path);
        let decoder = unsafe { self.factory.CreateDecoderFromFilename(name.pcwstr(), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand) }
            .map_err(|e| match failure(e) {
                Failure::NoCodec(_) => Failure::NoCodec(sniffed),
                f => f,
            })?;
        let container = unsafe { decoder.GetContainerFormat() }.unwrap_or_default();
        let format = container_format(&container, sniffed);
        let count = unsafe { decoder.GetFrameCount() }.unwrap_or(1).max(1);
        let index = if container == GUID_ContainerFormatIco { largest_frame(&decoder, count) } else { 0 };
        let frame = unsafe { decoder.GetFrame(index) }.map_err(failure)?;
        let (mut w, mut h) = (0u32, 0u32);
        unsafe { frame.GetSize(&mut w, &mut h) }.map_err(failure)?;
        if w == 0 || h == 0 {
            return Err(Failure::Broken("The image has no pixels.".into()));
        }
        let pixel_format = unsafe { frame.GetPixelFormat() }.unwrap_or_default();
        let (bits, channels, transparent) = self.describe_pixels(&pixel_format);
        let orient = if container == GUID_ContainerFormatIco { Orient::IDENTITY } else { orientation(&frame) };

        let mut details = Details {
            format: Some(format),
            bits_per_channel: bits,
            channels,
            frame_count: if container == GUID_ContainerFormatIco { 1 } else { count },
            ..Details::default()
        };

        // The shown size decides the limit, but the scaler works on the stored image.
        let (sw, sh) = orient.shown_size(w, h);
        let (tw, th) = fit_within(sw, sh, req.max_width, req.max_height, req.max_pixels);
        let (tw, th) = orient.shown_size(tw, th); // back to stored axes
        let scaling = (tw, th) != (w, h);

        let colour = self.colour_contexts(&frame);
        details.colour_space = colour.name.clone();
        let source: IWICBitmapSource = frame.cast().map_err(failure)?;
        let allowed = (tw as f64 * th as f64 * req.slack.max(1.0)) as u64;
        let worth_scaling = |w: u32, h: u32| scaling && w as u64 * h as u64 > allowed;
        let source = if transparent {
            // Scale premultiplied pixels, or edges of transparent areas pick up dark fringes.
            let managed = self.manage(&source, &colour, &mut details);
            let pbgra = self.convert(&managed, &GUID_WICPixelFormat32bppPBGRA)?;
            if worth_scaling(w, h) { self.scale(&pbgra, tw, th)? } else { pbgra }
        } else {
            // A JPEG decoder can shrink by 2, 4 or 8 while decoding, several times quicker than
            // decoding everything and scaling after.
            let (source, cw, ch) = match self.shrink_while_decoding(&frame, w, h, tw, th) {
                Some((b, cw, ch)) => (b, cw, ch),
                None => (source, w, h),
            };
            let scaled = if worth_scaling(cw, ch) {
                self.scale(&source, tw, th).or_else(|_| self.scale(&self.convert(&source, &GUID_WICPixelFormat32bppBGRA)?, tw, th))?
            } else {
                source
            };
            let managed = self.manage(&scaled, &colour, &mut details);
            self.convert(&managed, &GUID_WICPixelFormat32bppPBGRA)?
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(Failure::Cancelled);
        }
        let bitmap = copy_pixels(&source, cancel)?;
        Ok(Picture {
            width: w,
            height: h,
            frames: vec![Frame { bitmap, delay_ms: 0 }],
            loops: 0,
            orient,
            has_alpha: transparent,
            details,
            icc: None,
            streams: false,
        })
    }

    /// A picture held in memory (one inside a PowerPoint file), no more than `max_side`
    /// pixels across either way. `None` if Windows can't decode it.
    pub fn decode_bytes(&self, bytes: &[u8], max_side: u32) -> Option<Bitmap> {
        let stream = unsafe { self.factory.CreateStream() }.ok()?;
        unsafe { stream.InitializeFromMemory(bytes) }.ok()?;
        let decoder = unsafe { self.factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand) }.ok()?;
        let frame = unsafe { decoder.GetFrame(0) }.ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        unsafe { frame.GetSize(&mut w, &mut h) }.ok()?;
        if w == 0 || h == 0 {
            return None;
        }
        let source: IWICBitmapSource = frame.cast().ok()?;
        let pbgra = self.convert(&source, &GUID_WICPixelFormat32bppPBGRA).ok()?;
        let k = (max_side as f32 / w.max(h) as f32).min(1.0);
        let source = if k < 1.0 { self.scale(&pbgra, ((w as f32 * k).round() as u32).max(1), ((h as f32 * k).round() as u32).max(1)).ok()? } else { pbgra };
        copy_pixels(&source, &AtomicBool::new(false)).ok()
    }

    fn describe_pixels(&self, guid: &GUID) -> (Option<u32>, Option<String>, bool) {
        let info = unsafe { self.factory.CreateComponentInfo(guid) }.ok().and_then(|i| i.cast::<IWICPixelFormatInfo2>().ok());
        let Some(info) = info else { return (None, None, false) };
        let bpp = unsafe { info.GetBitsPerPixel() }.unwrap_or(0);
        let ch = unsafe { info.GetChannelCount() }.unwrap_or(0);
        let transparent = unsafe { info.SupportsTransparency() }.map(|b| b.as_bool()).unwrap_or(false);
        let indexed = [GUID_WICPixelFormat1bppIndexed, GUID_WICPixelFormat2bppIndexed, GUID_WICPixelFormat4bppIndexed, GUID_WICPixelFormat8bppIndexed];
        let grey = [
            GUID_WICPixelFormatBlackWhite,
            GUID_WICPixelFormat2bppGray,
            GUID_WICPixelFormat4bppGray,
            GUID_WICPixelFormat8bppGray,
            GUID_WICPixelFormat16bppGray,
            GUID_WICPixelFormat16bppGrayFixedPoint,
            GUID_WICPixelFormat16bppGrayHalf,
            GUID_WICPixelFormat32bppGrayFloat,
            GUID_WICPixelFormat32bppGrayFixedPoint,
        ];
        let cmyk = [GUID_WICPixelFormat32bppCMYK, GUID_WICPixelFormat64bppCMYK, GUID_WICPixelFormat40bppCMYKAlpha, GUID_WICPixelFormat80bppCMYKAlpha];
        let (name, bits) = if indexed.contains(guid) {
            ("Indexed", bpp)
        } else if grey.contains(guid) {
            ("Greyscale", bpp)
        } else if cmyk.contains(guid) {
            ("CMYK", if ch > 0 { bpp / ch } else { 8 })
        } else if transparent {
            ("RGBA", if ch > 0 { bpp / ch } else { 8 })
        } else {
            ("RGB", if ch > 0 { bpp / ch } else { 8 })
        };
        // Packed and padded layouts (such as 32bpp BGR, 8 bits a channel with a spare byte) don't
        // divide evenly; report the common depths only.
        let bits = match bits {
            1 | 2 | 4 | 8 | 10 | 12 | 16 | 32 => Some(bits),
            _ => Some(8),
        };
        (bits, Some(name.to_string()), transparent)
    }

    fn colour_contexts(&self, frame: &IWICBitmapFrameDecode) -> Colour {
        let mut count = 0u32;
        if unsafe { frame.GetColorContexts(&mut [], &mut count) }.is_err() || count == 0 {
            return Colour::default();
        }
        let mut contexts: Vec<Option<IWICColorContext>> = (0..count).map(|_| unsafe { self.factory.CreateColorContext() }.ok()).collect();
        if contexts.iter().any(|c| c.is_none()) || unsafe { frame.GetColorContexts(&mut contexts, &mut count) }.is_err() {
            return Colour::default();
        }
        for ctx in contexts.into_iter().flatten() {
            match unsafe { ctx.GetType() } {
                Ok(t) if t == WICColorContextProfile => {
                    let mut len = 0u32;
                    let _ = unsafe { ctx.GetProfileBytes(&mut [], &mut len) };
                    let mut bytes = vec![0u8; len as usize];
                    if len == 0 || unsafe { ctx.GetProfileBytes(&mut bytes, &mut len) }.is_err() {
                        continue;
                    }
                    let name = view_core::icc::description(&bytes).unwrap_or_else(|| "Embedded profile".into());
                    let is_srgb = name.to_ascii_lowercase().contains("srgb");
                    return Colour { name: Some(name), context: (!is_srgb).then_some(ctx) };
                }
                Ok(t) if t == WICColorContextExifColorSpace => {
                    let space = unsafe { ctx.GetExifColorSpace() }.unwrap_or(1);
                    match space {
                        1 => return Colour { name: Some("sRGB".into()), context: None },
                        2 => return Colour { name: Some("Adobe RGB".into()), context: Some(ctx) },
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Colour::default()
    }

    /// Convert from the image's colour profile to sRGB, if it has one that isn't sRGB. On any
    /// failure the pixels pass through unchanged: wrong-ish colours beat no image.
    fn manage(&self, source: &IWICBitmapSource, colour: &Colour, details: &mut Details) -> IWICBitmapSource {
        let Some(src_ctx) = &colour.context else { return source.clone() };
        let attempt = |input: &IWICBitmapSource| -> windows::core::Result<IWICBitmapSource> {
            let dst = unsafe { self.factory.CreateColorContext() }?;
            unsafe { dst.InitializeFromExifColorSpace(1) }?;
            let t = unsafe { self.factory.CreateColorTransformer() }?;
            unsafe { t.Initialize(input, src_ctx, &dst, &GUID_WICPixelFormat32bppBGRA) }?;
            t.cast()
        };
        let managed = attempt(source).or_else(|first| {
            crate::util::trace(&format!("colour transform: {first:?}"));
            let bgra = self.convert(source, &GUID_WICPixelFormat32bppBGRA).map_err(|_| windows::core::Error::empty())?;
            attempt(&bgra)
        });
        match managed {
            Ok(m) => {
                details.colour_managed = true;
                m
            }
            Err(e) => {
                crate::util::trace(&format!("colour transform failed: {e:?}"));
                source.clone()
            }
        }
    }

    fn convert(&self, source: &IWICBitmapSource, to: &GUID) -> Result<IWICBitmapSource, Failure> {
        if unsafe { source.GetPixelFormat() }.is_ok_and(|f| f == *to) {
            return Ok(source.clone());
        }
        let c = unsafe { self.factory.CreateFormatConverter() }.map_err(failure)?;
        unsafe { c.Initialize(source, to, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeMedianCut) }.map_err(failure)?;
        c.cast().map_err(failure)
    }

    /// Decode at the smallest of 1/2, 1/4 or 1/8 size that still covers `tw` × `th`, if the
    /// codec can. Returns the pixels in a WIC bitmap and their size.
    fn shrink_while_decoding(&self, frame: &IWICBitmapFrameDecode, w: u32, h: u32, tw: u32, th: u32) -> Option<(IWICBitmapSource, u32, u32)> {
        if tw >= w / 2 || th >= h / 2 {
            return None;
        }
        let t: IWICBitmapSourceTransform = frame.cast().ok()?;
        let (cw, ch) = [8u32, 4, 2].into_iter().find_map(|d| {
            let (mut cw, mut ch) = (w.div_ceil(d), h.div_ceil(d));
            if cw < tw || ch < th {
                return None;
            }
            unsafe { t.GetClosestSize(&mut cw, &mut ch) }.ok()?;
            (cw >= tw && ch >= th && cw < w).then_some((cw, ch))
        })?;
        let mut format = unsafe { frame.GetPixelFormat() }.ok()?;
        unsafe { t.GetClosestPixelFormat(&mut format) }.ok()?;
        let bitmap = unsafe { self.factory.CreateBitmap(cw, ch, &format, WICBitmapCacheOnLoad) }.ok()?;
        {
            let rect = WICRect { X: 0, Y: 0, Width: cw as i32, Height: ch as i32 };
            let lock = unsafe { bitmap.Lock(&rect, WICBitmapLockWrite.0 as u32) }.ok()?;
            let stride = unsafe { lock.GetStride() }.ok()?;
            let mut size = 0u32;
            let mut ptr = std::ptr::null_mut();
            unsafe { lock.GetDataPointer(&mut size, &mut ptr) }.ok()?;
            let buf = unsafe { std::slice::from_raw_parts_mut(ptr, size as usize) };
            unsafe { t.CopyPixels(std::ptr::null(), cw, ch, &format, WICBitmapTransformRotate0, stride, buf) }.ok()?;
        }
        Some((bitmap.cast().ok()?, cw, ch))
    }

    fn scale(&self, source: &IWICBitmapSource, w: u32, h: u32) -> Result<IWICBitmapSource, Failure> {
        let s = unsafe { self.factory.CreateBitmapScaler() }.map_err(failure)?;
        // Only ever used to shrink, where Fant (an area average) looks as good as cubic and is
        // several times quicker.
        unsafe { s.Initialize(source, w, h, WICBitmapInterpolationModeFant) }
            .or_else(|_| unsafe { s.Initialize(source, w, h, WICBitmapInterpolationModeHighQualityCubic) })
            .map_err(failure)?;
        s.cast().map_err(failure)
    }

    /// Apply an embedded ICC profile to pixels View decoded itself.
    pub fn manage_bitmap(&self, bitmap: &mut Bitmap, icc: &[u8]) -> bool {
        let name = view_core::icc::description(icc).unwrap_or_default().to_ascii_lowercase();
        if name.contains("srgb") {
            return false;
        }
        let mut run = || -> windows::core::Result<()> {
            let ctx = unsafe { self.factory.CreateColorContext() }?;
            unsafe { ctx.InitializeFromMemory(icc) }?;
            let colour = Colour { name: None, context: Some(ctx) };
            let stride = bitmap.width * 4;
            let src = unsafe { self.factory.CreateBitmapFromMemory(bitmap.width, bitmap.height, &GUID_WICPixelFormat32bppPBGRA, stride, &bitmap.data) }?;
            let src: IWICBitmapSource = src.cast()?;
            let straight = self.convert(&src, &GUID_WICPixelFormat32bppBGRA).map_err(|_| windows::core::Error::empty())?;
            let mut d = Details::default();
            let managed = self.manage(&straight, &colour, &mut d);
            if !d.colour_managed {
                return Err(windows::core::Error::empty());
            }
            let out = self.convert(&managed, &GUID_WICPixelFormat32bppPBGRA).map_err(|_| windows::core::Error::empty())?;
            unsafe { out.CopyPixels(std::ptr::null(), stride, &mut bitmap.data) }
        };
        run().is_ok()
    }

    /// Encode premultiplied BGRA pixels as a PNG, for the clipboard.
    pub fn encode_png(&self, width: u32, height: u32, pbgra: &[u8]) -> Option<Vec<u8>> {
        use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
        use windows::Win32::System::Com::{STREAM_SEEK_SET, STATSTG, STATFLAG_NONAME};
        unsafe {
            let stream = CreateStreamOnHGlobal(Default::default(), true).ok()?;
            let enc = self.factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()).ok()?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache).ok()?;
            let mut frame = None;
            enc.CreateNewFrame(&mut frame, std::ptr::null_mut()).ok()?;
            let frame = frame?;
            frame.Initialize(None).ok()?;
            frame.SetSize(width, height).ok()?;
            let src = self.factory.CreateBitmapFromMemory(width, height, &GUID_WICPixelFormat32bppPBGRA, width * 4, pbgra).ok()?;
            let src: IWICBitmapSource = src.cast().ok()?;
            let straight = self.convert(&src, &GUID_WICPixelFormat32bppBGRA).ok()?;
            let mut fmt = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut fmt).ok()?;
            frame.WriteSource(&straight, std::ptr::null()).ok()?;
            frame.Commit().ok()?;
            enc.Commit().ok()?;
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
            stream.Seek(0, STREAM_SEEK_SET, None).ok()?;
            let mut out = vec![0u8; stat.cbSize as usize];
            let mut read = 0u32;
            stream.Read(out.as_mut_ptr() as *mut _, out.len() as u32, Some(&mut read)).ok().ok()?;
            out.truncate(read as usize);
            Some(out)
        }
    }

    /// Save premultiplied BGRA pixels as a JPEG (at high quality) or a PNG, to a file of its
    /// own first and then into place, so a failure never leaves half a file. For a JPEG made
    /// from a JPEG, the camera's details (EXIF and XMP) come across, set upright, since the
    /// pixels are; the old thumbnail inside is dropped, as it shows the picture before.
    pub fn save(&self, path: &Path, width: u32, height: u32, pbgra: &[u8], jpeg: bool, details_from: Option<&Path>) -> Result<(), String> {
        use windows::Win32::System::Com::StructuredStorage::{IPropertyBag2, PROPBAG2};
        use windows::Win32::System::Variant::VARIANT;
        let fail = |e: windows::core::Error| format!("Windows couldn't save the picture: {}", e.message());
        let tmp = path.with_extension("view-saving");
        let result = (|| -> Result<(), String> {
            unsafe {
                let stream = self.factory.CreateStream().map_err(fail)?;
                stream.InitializeFromFilename(Wide::path(&tmp).pcwstr(), windows::Win32::Foundation::GENERIC_WRITE.0).map_err(fail)?;
                let container = if jpeg { GUID_ContainerFormatJpeg } else { GUID_ContainerFormatPng };
                let enc = self.factory.CreateEncoder(&container, std::ptr::null()).map_err(fail)?;
                enc.Initialize(&stream, WICBitmapEncoderNoCache).map_err(fail)?;
                let mut frame = None;
                let mut bag: Option<IPropertyBag2> = None;
                enc.CreateNewFrame(&mut frame, &mut bag).map_err(fail)?;
                let frame = frame.ok_or("Windows couldn't save the picture.")?;
                if let (true, Some(bag)) = (jpeg, &bag) {
                    let name = Wide::new("ImageQuality");
                    let prop = PROPBAG2 { pstrName: windows::core::PWSTR(name.0.as_ptr() as *mut _), ..Default::default() };
                    let _ = bag.Write(1, &prop, &VARIANT::from(0.93f32));
                }
                frame.Initialize(bag.as_ref()).map_err(fail)?;
                frame.SetSize(width, height).map_err(fail)?;
                let target = if jpeg { GUID_WICPixelFormat24bppBGR } else { GUID_WICPixelFormat32bppBGRA };
                let mut fmt = target;
                frame.SetPixelFormat(&mut fmt).map_err(fail)?;
                if let (true, Some(from)) = (jpeg, details_from) {
                    self.copy_details(from, &frame, width, height);
                }
                let src: IWICBitmapSource = self.factory.CreateBitmapFromMemory(width, height, &GUID_WICPixelFormat32bppPBGRA, width * 4, pbgra).map_err(fail)?.cast().map_err(fail)?;
                let src = self.convert(&src, &fmt).map_err(|f| f.message())?;
                frame.WriteSource(&src, std::ptr::null()).map_err(fail)?;
                frame.Commit().map_err(fail)?;
                enc.Commit().map_err(fail)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => std::fs::rename(&tmp, path).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("Windows couldn't save the picture: {e}")
            }),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Copy a JPEG's EXIF and XMP into a JPEG being written, upright and with the new size.
    /// Best effort: a photo with odd metadata is still saved, just without it.
    fn copy_details(&self, from: &Path, frame: &IWICBitmapFrameEncode, width: u32, height: u32) {
        let name = Wide::path(from);
        unsafe {
            let Ok(decoder) = self.factory.CreateDecoderFromFilename(name.pcwstr(), None, GENERIC_READ, WICDecodeMetadataCacheOnLoad) else { return };
            if decoder.GetContainerFormat().ok() != Some(GUID_ContainerFormatJpeg) {
                return;
            }
            let Ok(source) = decoder.GetFrame(0).and_then(|f| f.GetMetadataQueryReader()) else { return };
            let Ok(writer) = frame.GetMetadataQueryWriter() else { return };
            for block in [w!("/app1"), w!("/xmp")] {
                let mut v = PROPVARIANT::default();
                if source.GetMetadataByName(block, &mut v).is_err() {
                    continue;
                }
                let copied = IUnknown::try_from(&v).ok().and_then(|u| u.cast::<IWICMetadataQueryReader>().ok()).and_then(|r| self.factory.CreateQueryWriterFromReader(&r, std::ptr::null()).ok());
                let _ = PropVariantClear(&mut v);
                if let Some(w) = copied {
                    let _ = writer.SetMetadataByName(block, &PROPVARIANT::from(IUnknown::from(w)));
                }
            }
            let _ = writer.SetMetadataByName(w!("/app1/ifd/{ushort=274}"), &PROPVARIANT::from(1u16));
            let _ = writer.SetMetadataByName(w!("/app1/ifd/exif/{ushort=40962}"), &PROPVARIANT::from(width));
            let _ = writer.SetMetadataByName(w!("/app1/ifd/exif/{ushort=40963}"), &PROPVARIANT::from(height));
            let _ = writer.RemoveMetadataByName(w!("/app1/thumb"));
            let _ = writer.RemoveMetadataByName(w!("/xmp/tiff:Orientation"));
        }
    }
}

#[derive(Default)]
struct Colour {
    name: Option<String>,
    /// The profile to convert from, when it isn't sRGB.
    context: Option<IWICColorContext>,
}

fn container_format(guid: &GUID, sniffed: Option<Format>) -> Format {
    let known = [
        (GUID_ContainerFormatJpeg, Format::Jpeg),
        (GUID_ContainerFormatPng, Format::Png),
        (GUID_ContainerFormatGif, Format::Gif),
        (GUID_ContainerFormatBmp, Format::Bmp),
        (GUID_ContainerFormatTiff, Format::Tiff),
        (GUID_ContainerFormatIco, Format::Ico),
        (GUID_ContainerFormatWmp, Format::JpegXr),
        (GUID_ContainerFormatDds, Format::Dds),
        (GUID_ContainerFormatWebp, Format::WebP),
        (GUID_ContainerFormatAdng, Format::Raw),
        (GUID_ContainerFormatRaw, Format::Raw),
    ];
    if *guid == GUID_ContainerFormatHeif {
        // The HEIF codec opens AVIF too; the file's brand tells them apart.
        return if sniffed == Some(Format::Avif) { Format::Avif } else { Format::Heif };
    }
    known.iter().find(|(g, _)| g == guid).map(|(_, f)| *f).or(sniffed).unwrap_or(Format::Raw)
}

/// An icon file holds several sizes; show the biggest.
fn largest_frame(decoder: &IWICBitmapDecoder, count: u32) -> u32 {
    let mut best = (0u64, 0u32);
    for i in 0..count.min(64) {
        let Ok(f) = (unsafe { decoder.GetFrame(i) }) else { continue };
        let (mut w, mut h) = (0, 0);
        if unsafe { f.GetSize(&mut w, &mut h) }.is_ok() {
            let area = w as u64 * h as u64;
            if area > best.0 {
                best = (area, i);
            }
        }
    }
    best.1
}

/// The EXIF orientation, through WIC's own metadata policy, which knows where each format keeps it.
fn orientation(frame: &IWICBitmapFrameDecode) -> Orient {
    let Ok(reader) = (unsafe { frame.GetMetadataQueryReader() }) else { return Orient::IDENTITY };
    let mut v = PROPVARIANT::default();
    if unsafe { reader.GetMetadataByName(w!("System.Photo.Orientation"), &mut v) }.is_err() {
        return Orient::IDENTITY;
    }
    let value = unsafe {
        let inner = &v.Anonymous.Anonymous;
        match inner.vt {
            VT_UI2 => inner.Anonymous.uiVal as u32,
            VT_UI4 => inner.Anonymous.ulVal,
            VT_UI1 => inner.Anonymous.bVal as u32,
            _ => 1,
        }
    };
    unsafe {
        let _ = PropVariantClear(&mut v);
    }
    Orient::from_exif(value)
}

/// Pull the pixels out in bands, so a cancelled decode stops early and a huge one never needs
/// a second buffer.
fn copy_pixels(source: &IWICBitmapSource, cancel: &AtomicBool) -> Result<Bitmap, Failure> {
    let (mut w, mut h) = (0u32, 0u32);
    unsafe { source.GetSize(&mut w, &mut h) }.map_err(failure)?;
    let mut bitmap = Bitmap::try_new(w, h).ok_or_else(|| Failure::Broken("Not enough memory to open this image.".into()))?;
    let stride = w * 4;
    // About 4 MB a band.
    let band = ((4 << 20) / stride.max(1)).clamp(16, 1024);
    let mut y = 0;
    while y < h {
        if cancel.load(Ordering::Relaxed) {
            return Err(Failure::Cancelled);
        }
        let rows = band.min(h - y);
        let rect = WICRect { X: 0, Y: y as i32, Width: w as i32, Height: rows as i32 };
        let start = (y * stride) as usize;
        let end = start + (rows * stride) as usize;
        unsafe { source.CopyPixels(&rect, stride, &mut bitmap.data[start..end]) }.map_err(failure)?;
        y += rows;
    }
    Ok(bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn saved in a JPEG's EXIF tag reads back as that turn, with every pixel as it was.
    #[test]
    fn a_saved_turn_keeps_every_pixel() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let wic = Wic::new().unwrap();
        let path = std::env::temp_dir().join(format!("view-turn-{}.jpg", std::process::id()));
        let (w, h) = (64u32, 32u32);
        let px: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, (i * 7 % 253) as u8, (i * 13 % 255) as u8, 255]).collect();
        unsafe {
            let stream = wic.factory.CreateStream().unwrap();
            stream.InitializeFromFilename(Wide::path(&path).pcwstr(), windows::Win32::Foundation::GENERIC_WRITE.0).unwrap();
            let enc = wic.factory.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null()).unwrap();
            enc.Initialize(&stream, WICBitmapEncoderNoCache).unwrap();
            let mut frame = None;
            enc.CreateNewFrame(&mut frame, std::ptr::null_mut()).unwrap();
            let frame = frame.unwrap();
            frame.Initialize(None).unwrap();
            frame.SetSize(w, h).unwrap();
            let mut fmt = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut fmt).unwrap();
            let src: IWICBitmapSource = wic.factory.CreateBitmapFromMemory(w, h, &GUID_WICPixelFormat32bppBGRA, w * 4, &px).unwrap().cast().unwrap();
            let src = wic.convert(&src, &GUID_WICPixelFormat24bppBGR).ok().unwrap();
            frame.WriteSource(&src, std::ptr::null()).unwrap();
            frame.Commit().unwrap();
            enc.Commit().unwrap();
        }
        let req = Request { max_width: 0, max_height: 0, max_pixels: u64::MAX, slack: 1.0 };
        let before = wic.decode(&path, None, req, &AtomicBool::new(false)).ok().unwrap();
        crate::shell::save_orientation(&path, 6).unwrap();
        let after = wic.decode(&path, None, req, &AtomicBool::new(false)).ok().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(before.orient, Orient::IDENTITY);
        assert_eq!(after.orient, Orient::from_exif(6));
        assert!(before.frames[0].bitmap.data == after.frames[0].bitmap.data, "the pixels must be untouched");
    }
}
