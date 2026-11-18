//! Text, through DirectWrite, in Geist: the suite's interface font, built into the exe and
//! loaded from memory, so nothing is installed on the system. Letters Geist lacks come from
//! Windows' fallback fonts, and Segoe UI stands in if Geist can't be loaded at all.

use std::cell::RefCell;
use std::collections::HashMap;
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::{Interface, PCWSTR, w};

static GEIST: &[u8] = include_bytes!("../../../assets/fonts/GeistVariable.ttf");
const LOCALE: PCWSTR = w!("en-us");

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

pub struct Text {
    factory: IDWriteFactory,
    geist: Option<(IDWriteFactory6, IDWriteFontCollection2)>,
    formats: RefCell<HashMap<(u32, u32, u8, Align, bool), IDWriteTextFormat>>,
    /// System font families asked about, and whether they're installed.
    families: RefCell<HashMap<String, bool>>,
    mono: PCWSTR,
}

impl Text {
    pub fn new() -> Option<Text> {
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ok()?;
        let geist = factory.cast::<IDWriteFactory6>().ok().and_then(|six| Some((six.clone(), geist_fonts(&six)?)));
        let mono = if has_system_family(&factory, w!("Cascadia Mono")) { w!("Cascadia Mono") } else { w!("Consolas") };
        Some(Text { factory, geist, formats: RefCell::new(HashMap::new()), families: RefCell::new(HashMap::new()), mono })
    }

    /// Interface text: `px` high in physical pixels, at a weight from 100 to 900.
    pub fn format(&self, px: f32, weight: u32, align: Align, wrap: bool) -> Option<IDWriteTextFormat> {
        self.cached(px, weight, 0, align, wrap, || {
            if let Some((six, fonts)) = &self.geist {
                let axes = [DWRITE_FONT_AXIS_VALUE { axisTag: DWRITE_FONT_AXIS_TAG_WEIGHT, value: weight as f32 }];
                if let Ok(f) = unsafe { six.CreateTextFormat(w!("Geist"), fonts, &axes, px, LOCALE) } {
                    return f.cast().ok();
                }
            }
            let weight = DWRITE_FONT_WEIGHT(if weight >= 550 { 600 } else { 400 });
            unsafe { self.factory.CreateTextFormat(w!("Segoe UI"), None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, px, LOCALE) }.ok()
        })
    }

    /// Windows' symbol font: Segoe Fluent Icons on Windows 11, Segoe MDL2 Assets before it.
    pub fn icons(&self, px: f32) -> Option<IDWriteTextFormat> {
        self.cached(px, 400, 1, Align::Center, false, || {
            let family = if has_system_family(&self.factory, w!("Segoe Fluent Icons")) { w!("Segoe Fluent Icons") } else { w!("Segoe MDL2 Assets") };
            unsafe { self.factory.CreateTextFormat(family, None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, px, LOCALE) }.ok()
        })
    }

    /// Code: Cascadia Mono, which Windows 11 has, or Consolas before it.
    pub fn mono(&self, px: f32, wrap: bool) -> Option<IDWriteTextFormat> {
        self.cached(px, 400, 2, Align::Left, wrap, || unsafe { self.factory.CreateTextFormat(self.mono, None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, px, LOCALE) }.ok())
    }

    /// The monospaced family's name, and the system's fonts it comes from, for code inside prose.
    pub fn mono_family(&self) -> (PCWSTR, Option<IDWriteFontCollection>) {
        let mut fonts = None;
        let _ = unsafe { self.factory.GetSystemFontCollection(&mut fonts, false) };
        (self.mono, fonts)
    }

    /// A family from the system's fonts, or Segoe UI if it isn't installed: for text in
    /// documents, which name their own fonts.
    pub fn system_family(&self, name: &str) -> String {
        if name.is_empty() {
            return "Segoe UI".into();
        }
        if let Some(&known) = self.families.borrow().get(name) {
            return if known { name.to_string() } else { "Segoe UI".into() };
        }
        let wide = crate::util::Wide::new(name);
        let known = has_system_family(&self.factory, wide.pcwstr());
        self.families.borrow_mut().insert(name.to_string(), known);
        if known { name.to_string() } else { "Segoe UI".into() }
    }

    /// Text in a system font, `px` high, for a document's own text.
    pub fn system_format(&self, family: &str, px: f32) -> Option<IDWriteTextFormat> {
        let family = crate::util::Wide::new(&self.system_family(family));
        let f = unsafe { self.factory.CreateTextFormat(family.pcwstr(), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, px.max(0.5), LOCALE) }.ok()?;
        unsafe {
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        }
        Some(f)
    }

    fn cached(&self, px: f32, weight: u32, family: u8, align: Align, wrap: bool, make: impl FnOnce() -> Option<IDWriteTextFormat>) -> Option<IDWriteTextFormat> {
        let key = ((px * 10.0).round() as u32, weight, family, align, wrap);
        if let Some(f) = self.formats.borrow().get(&key) {
            return Some(f.clone());
        }
        let f = make()?;
        unsafe {
            let _ = f.SetWordWrapping(if wrap { DWRITE_WORD_WRAPPING_WRAP } else { DWRITE_WORD_WRAPPING_NO_WRAP });
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
            let _ = f.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            if !wrap {
                // Cut long single lines with an ellipsis rather than running past their box.
                if let Ok(sign) = self.factory.CreateEllipsisTrimmingSign(&f) {
                    let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                    let _ = f.SetTrimming(&trim, &sign);
                }
            }
        }
        self.formats.borrow_mut().insert(key, f.clone());
        Some(f)
    }

    /// Lay out `s` in a box `width` wide.
    pub fn layout(&self, s: &str, format: &IDWriteTextFormat, width: f32, height: f32) -> Option<IDWriteTextLayout> {
        let units: Vec<u16> = s.encode_utf16().collect();
        unsafe { self.factory.CreateTextLayout(&units, format, width.max(1.0), height.max(1.0)) }.ok()
    }

    /// The size a layout's text takes.
    pub fn size(layout: &IDWriteTextLayout) -> (f32, f32) {
        let mut m = DWRITE_TEXT_METRICS::default();
        if unsafe { layout.GetMetrics(&mut m) }.is_ok() { (m.widthIncludingTrailingWhitespace, m.height) } else { (0.0, 0.0) }
    }

    /// Forget cached formats, after a DPI change.
    pub fn reset(&self) {
        self.formats.borrow_mut().clear();
    }

    /// A font file's first face, loaded privately (nothing is installed): its collection and
    /// what it's called, for a preview.
    pub fn font_file(&self, path: &std::path::Path) -> Result<FontFace, String> {
        let bad = || "Windows couldn't read this font.".to_string();
        unsafe {
            let five: IDWriteFactory5 = self.factory.cast().map_err(|_| bad())?;
            let name = crate::util::Wide::path(path);
            let file = five.CreateFontFileReference(name.pcwstr(), None).map_err(|_| bad())?;
            let builder: IDWriteFontSetBuilder1 = five.CreateFontSetBuilder().map_err(|_| bad())?.cast().map_err(|_| bad())?;
            builder.AddFontFile(&file).map_err(|_| bad())?;
            let set = builder.CreateFontSet().map_err(|_| bad())?;
            let faces = set.GetFontCount();
            if faces == 0 {
                return Err(bad());
            }
            let fonts = five.CreateFontCollectionFromFontSet(&set).map_err(|_| bad())?;
            let family = fonts.GetFontFamily(0).map_err(|_| bad())?;
            let font = family.GetFont(0).map_err(|_| bad())?;
            let family_name = family.GetFamilyNames().ok().and_then(|n| local_string(&n)).unwrap_or_default();
            let face_name = font.GetFaceNames().ok().and_then(|n| local_string(&n)).unwrap_or_default();
            let info = |id: DWRITE_INFORMATIONAL_STRING_ID| {
                let mut strings = None;
                let mut exists = windows::core::BOOL(0);
                font.GetInformationalStrings(id, &mut strings, &mut exists).ok()?;
                strings.filter(|_| exists.as_bool()).and_then(|s| local_string(&s))
            };
            Ok(FontFace {
                collection: fonts.cast().map_err(|_| bad())?,
                family: family_name,
                face: face_name,
                version: info(DWRITE_INFORMATIONAL_STRING_VERSION_STRINGS),
                designer: info(DWRITE_INFORMATIONAL_STRING_DESIGNER).or_else(|| info(DWRITE_INFORMATIONAL_STRING_MANUFACTURER)),
                faces,
                weight: font.GetWeight(),
                style: font.GetStyle(),
                stretch: font.GetStretch(),
            })
        }
    }

    /// Text in a font loaded by [`Text::font_file`].
    pub fn font_format(&self, f: &FontFace, px: f32) -> Option<IDWriteTextFormat> {
        let family = crate::util::Wide::new(&f.family);
        let format = unsafe { self.factory.CreateTextFormat(family.pcwstr(), &f.collection, f.weight, f.style, f.stretch, px, LOCALE) }.ok()?;
        unsafe {
            let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        }
        Some(format)
    }
}


/// A font from a file, for the preview.
pub struct FontFace {
    collection: IDWriteFontCollection,
    pub family: String,
    /// "Bold Italic" and the like.
    pub face: String,
    pub version: Option<String>,
    pub designer: Option<String>,
    /// How many faces the file holds.
    pub faces: u32,
    weight: DWRITE_FONT_WEIGHT,
    style: DWRITE_FONT_STYLE,
    stretch: DWRITE_FONT_STRETCH,
}

/// A name in English if there is one, else the first.
fn local_string(s: &IDWriteLocalizedStrings) -> Option<String> {
    unsafe {
        let (mut index, mut found) = (0u32, windows::core::BOOL(0));
        let _ = s.FindLocaleName(w!("en-us"), &mut index, &mut found);
        if !found.as_bool() {
            index = 0;
        }
        let n = s.GetStringLength(index).ok()? as usize;
        let mut buf = vec![0u16; n + 1];
        s.GetString(index, &mut buf).ok()?;
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

/// Geist as a font collection of its own, read from memory.
fn geist_fonts(factory: &IDWriteFactory6) -> Option<IDWriteFontCollection2> {
    unsafe {
        let loader = factory.CreateInMemoryFontFileLoader().ok()?;
        factory.RegisterFontFileLoader(&loader).ok()?;
        let file = loader.CreateInMemoryFontFileReference(factory, GEIST.as_ptr() as *const _, GEIST.len() as u32, None).ok()?;
        let builder: IDWriteFontSetBuilder1 = factory.CreateFontSetBuilder().ok()?.cast().ok()?;
        builder.AddFontFile(&file).ok()?;
        let set = builder.CreateFontSet().ok()?;
        let fonts = factory.CreateFontCollectionFromFontSet(&set, DWRITE_FONT_FAMILY_MODEL_TYPOGRAPHIC).ok()?;
        let (mut index, mut found) = (0u32, windows::core::BOOL(0));
        fonts.FindFamilyName(w!("Geist"), &mut index, &mut found).ok()?;
        found.as_bool().then_some(fonts)
    }
}

fn has_system_family(factory: &IDWriteFactory, name: PCWSTR) -> bool {
    let mut fonts = None;
    if unsafe { factory.GetSystemFontCollection(&mut fonts, false) }.is_err() {
        return false;
    }
    let Some(fonts) = fonts else { return false };
    let (mut index, mut found) = (0u32, windows::core::BOOL(0));
    unsafe { fonts.FindFamilyName(name, &mut index, &mut found).is_ok() && found.as_bool() }
}
