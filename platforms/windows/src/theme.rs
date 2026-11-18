//! Colours. The same dark-first palette as Orbit and Prism, a light one to match Windows'
//! light mode, and the user's accent colour (Windows' own unless they pick one).

use view_core::settings::{Backdrop, Settings, Theme as Choice};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::w;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    /// Behind the image.
    pub backdrop: Rgb,
    /// The window's own background, for the title bar to match.
    pub chrome: Rgb,
    /// Floating panels: the toolbar, the information panel, messages.
    pub panel: Rgb,
    /// How strongly the panel colour tints frosted glass.
    pub glass: f32,
    pub edge: Rgb,
    pub edge_alpha: f32,
    pub text: Rgb,
    pub dim: Rgb,
    pub hover: Rgb,
    pub hover_alpha: f32,
    pub accent: Rgb,
    /// The two checkerboard shades behind transparent images.
    pub checker: (Rgb, Rgb),
}

const FALLBACK_ACCENT: Rgb = Rgb(0x4C, 0x8D, 0xFF);

pub fn palette(s: &Settings) -> Palette {
    let dark = match s.theme {
        Choice::Dark => true,
        Choice::Light => false,
        Choice::System => system_is_dark(),
    };
    let mut p = if dark {
        Palette {
            dark,
            backdrop: Rgb(13, 13, 15),
            chrome: Rgb(13, 13, 15),
            panel: Rgb(0x1C, 0x1C, 0x21),
            glass: 0.58,
            edge: Rgb(255, 255, 255),
            edge_alpha: 0.10,
            text: Rgb(0xEE, 0xEE, 0xF2),
            dim: Rgb(0xA1, 0xA1, 0xAA),
            hover: Rgb(255, 255, 255),
            hover_alpha: 0.09,
            accent: FALLBACK_ACCENT,
            checker: (Rgb(0x2A, 0x2A, 0x2F), Rgb(0x36, 0x36, 0x3C)),
        }
    } else {
        Palette {
            dark,
            backdrop: Rgb(240, 240, 242),
            chrome: Rgb(240, 240, 242),
            panel: Rgb(252, 252, 253),
            glass: 0.66,
            edge: Rgb(0, 0, 0),
            edge_alpha: 0.10,
            text: Rgb(24, 24, 27),
            dim: Rgb(96, 96, 104),
            hover: Rgb(0, 0, 0),
            hover_alpha: 0.06,
            accent: Rgb(0, 95, 184),
            checker: (Rgb(0xFF, 0xFF, 0xFF), Rgb(0xE4, 0xE4, 0xE8)),
        }
    };
    p.accent = s.accent_rgb().map(|(r, g, b)| Rgb(r, g, b)).or_else(|| system_accent(dark)).unwrap_or(p.accent);
    match s.backdrop {
        Backdrop::Theme => {}
        Backdrop::Black => p.backdrop = Rgb(0, 0, 0),
        Backdrop::Grey => p.backdrop = Rgb(0x80, 0x80, 0x80),
        Backdrop::White => p.backdrop = Rgb(255, 255, 255),
    }
    // A checkerboard that suits the backdrop rather than the theme.
    let lum = p.backdrop.0 as u32 + p.backdrop.1 as u32 + p.backdrop.2 as u32;
    p.checker = if lum > 3 * 150 { (Rgb(0xFF, 0xFF, 0xFF), Rgb(0xD8, 0xD8, 0xDC)) } else { (Rgb(0x2A, 0x2A, 0x2F), Rgb(0x3A, 0x3A, 0x40)) };
    p
}

pub fn system_is_dark() -> bool {
    let mut value = 1u32;
    let mut size = 4u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut _ as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok() && value == 0
}

/// Windows' accent in the shade it uses itself on this background: lighter on dark, deeper on light.
fn system_accent(dark: bool) -> Option<Rgb> {
    let mut bytes = [0u8; 32];
    let mut size = bytes.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent"),
            w!("AccentPalette"),
            RRF_RT_REG_BINARY,
            None,
            Some(bytes.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if status.is_err() || size != 32 {
        return None;
    }
    let i = if dark { 1 } else { 4 };
    Some(Rgb(bytes[i * 4], bytes[i * 4 + 1], bytes[i * 4 + 2]))
}

impl Rgb {
    pub fn d2d(self, a: f32) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
        crate::gfx::color(self.0, self.1, self.2, a)
    }

    /// As a COLORREF, for DWM and GDI.
    pub fn colorref(self) -> u32 {
        self.0 as u32 | (self.1 as u32) << 8 | (self.2 as u32) << 16
    }

    /// Text that reads on this colour: near-black on light colours, white on dark ones.
    pub fn ink(self) -> Rgb {
        let lin = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        let l = 0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2);
        // Where black and white text have equal contrast (WCAG): about 0.18.
        if l > 0.18 { Rgb(18, 18, 20) } else { Rgb(255, 255, 255) }
    }
}
