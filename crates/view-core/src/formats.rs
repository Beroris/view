//! What a file is. The extension decides which files show up in a folder; the first bytes
//! decide how to decode one, because extensions lie (a ".jpg" that's really a PNG is common).

/// An image format View knows about. Whether it can be decoded on this machine is the
/// platform's call: some need a codec the system may not have.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Format {
    Jpeg,
    Png,
    Gif,
    WebP,
    Bmp,
    Tiff,
    Ico,
    Avif,
    Heif,
    JpegXl,
    JpegXr,
    Dds,
    Raw,
}

impl Format {
    /// The name shown to people.
    pub fn name(self) -> &'static str {
        match self {
            Format::Jpeg => "JPEG",
            Format::Png => "PNG",
            Format::Gif => "GIF",
            Format::WebP => "WebP",
            Format::Bmp => "BMP",
            Format::Tiff => "TIFF",
            Format::Ico => "ICO",
            Format::Avif => "AVIF",
            Format::Heif => "HEIF",
            Format::JpegXl => "JPEG XL",
            Format::JpegXr => "JPEG XR",
            Format::Dds => "DDS",
            Format::Raw => "Camera RAW",
        }
    }

    /// Every format, for listing what this machine can open.
    pub const ALL: [Format; 13] = [
        Format::Jpeg,
        Format::Png,
        Format::Gif,
        Format::WebP,
        Format::Bmp,
        Format::Tiff,
        Format::Ico,
        Format::Avif,
        Format::Heif,
        Format::JpegXl,
        Format::JpegXr,
        Format::Dds,
        Format::Raw,
    ];

    /// The extensions that mean this format, lower case, without the dot.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Format::Jpeg => &["jpg", "jpeg", "jpe", "jfif"],
            Format::Png => &["png", "apng"],
            Format::Gif => &["gif"],
            Format::WebP => &["webp"],
            Format::Bmp => &["bmp", "dib"],
            Format::Tiff => &["tif", "tiff"],
            Format::Ico => &["ico", "cur"],
            Format::Avif => &["avif"],
            Format::Heif => &["heic", "heif", "hif"],
            Format::JpegXl => &["jxl"],
            Format::JpegXr => &["jxr", "wdp", "hdp"],
            Format::Dds => &["dds"],
            Format::Raw => RAW_EXTENSIONS,
        }
    }

    /// The format an extension stands for.
    pub fn from_extension(ext: &str) -> Option<Format> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        Format::ALL.into_iter().find(|f| f.extensions().contains(&ext.as_str()))
    }
}

/// Camera RAW extensions that Windows' Raw Image Extension and macOS decode.
pub const RAW_EXTENSIONS: &[&str] = &[
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "pef", "srw", "x3f", "3fr", "erf",
    "kdc", "mef", "mos", "mrw", "rwl", "iiq",
];

/// What the first bytes of a file say it is. Sixteen bytes are enough for every format here.
/// RAW files mostly look like TIFF and are told apart by extension instead.
pub fn sniff(head: &[u8]) -> Option<Format> {
    let starts = |sig: &[u8]| head.len() >= sig.len() && &head[..sig.len()] == sig;
    if starts(&[0xFF, 0xD8, 0xFF]) {
        Some(Format::Jpeg)
    } else if starts(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some(Format::Gif)
    } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some(Format::WebP)
    } else if starts(b"BM") {
        Some(Format::Bmp)
    } else if starts(b"II\x2a\x00") || starts(b"MM\x00\x2a") || starts(b"II\x2b\x00") || starts(b"MM\x00\x2b") {
        Some(Format::Tiff)
    } else if starts(b"II\xbc") {
        Some(Format::JpegXr)
    } else if starts(&[0, 0, 1, 0]) || starts(&[0, 0, 2, 0]) {
        Some(Format::Ico)
    } else if starts(&[0xFF, 0x0A]) || starts(b"\x00\x00\x00\x0cJXL \x0d\x0a\x87\x0a") {
        Some(Format::JpegXl)
    } else if starts(b"DDS ") {
        Some(Format::Dds)
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        // ISO base media: the major brand says AVIF or HEIF.
        match &head[8..12] {
            b"avif" | b"avis" => Some(Format::Avif),
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1" => Some(Format::Heif),
            _ => None,
        }
    } else {
        None
    }
}

/// Whether a PNG is animated: an `acTL` chunk before the first `IDAT`.
pub fn is_apng(data: &[u8]) -> bool {
    let mut at = 8;
    while at + 8 <= data.len() {
        let len = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        let kind = &data[at + 4..at + 8];
        if kind == b"acTL" {
            return true;
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return false;
        }
        at = at.saturating_add(12).saturating_add(len);
    }
    false
}

/// Whether a WebP is animated: the `VP8X` header's animation flag.
pub fn is_animated_webp(data: &[u8]) -> bool {
    data.len() >= 21 && &data[12..16] == b"VP8X" && data[20] & 0x02 != 0
}

/// Whether a GIF has more than one frame, without decoding it. Walks the blocks until a second
/// image descriptor, and gives up (saying "maybe") on anything malformed.
pub fn gif_has_many_frames(data: &[u8]) -> bool {
    if data.len() < 13 {
        return false;
    }
    let mut at = 13;
    if data[10] & 0x80 != 0 {
        at += 3 * (1usize << ((data[10] & 0x07) + 1));
    }
    let mut frames = 0;
    let skip_sub_blocks = |mut at: usize| -> Option<usize> {
        loop {
            let n = *data.get(at)? as usize;
            at += 1;
            if n == 0 {
                return Some(at);
            }
            at += n;
        }
    };
    while let Some(&b) = data.get(at) {
        match b {
            0x2C => {
                frames += 1;
                if frames > 1 {
                    return true;
                }
                let Some(flags) = data.get(at + 9) else { return false };
                at += 10;
                if flags & 0x80 != 0 {
                    at += 3 * (1usize << ((flags & 0x07) + 1));
                }
                at += 1; // LZW minimum code size
                match skip_sub_blocks(at) {
                    Some(next) => at = next,
                    None => return false,
                }
            }
            0x21 => match skip_sub_blocks(at + 2) {
                Some(next) => at = next,
                None => return false,
            },
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_signatures() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some(Format::Jpeg));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"), Some(Format::Png));
        assert_eq!(sniff(b"GIF89a\x01\x00"), Some(Format::Gif));
        assert_eq!(sniff(b"RIFF\x10\x00\x00\x00WEBPVP8 "), Some(Format::WebP));
        assert_eq!(sniff(b"\x00\x00\x00\x1cftypavif\x00\x00"), Some(Format::Avif));
        assert_eq!(sniff(b"\x00\x00\x00\x18ftypheic\x00\x00"), Some(Format::Heif));
        assert_eq!(sniff(b"\x00\x00\x00\x18ftypmp42\x00\x00"), None);
        assert_eq!(sniff(b"hello world"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn extensions_map_back() {
        assert_eq!(Format::from_extension("JPG"), Some(Format::Jpeg));
        assert_eq!(Format::from_extension(".heic"), Some(Format::Heif));
        assert_eq!(Format::from_extension("nef"), Some(Format::Raw));
        assert_eq!(Format::from_extension("txt"), None);
    }

    #[test]
    fn apng_needs_actl_before_idat() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let chunk = |kind: &[u8], body: &[u8]| {
            let mut c = (body.len() as u32).to_be_bytes().to_vec();
            c.extend_from_slice(kind);
            c.extend_from_slice(body);
            c.extend_from_slice(&[0; 4]);
            c
        };
        png.extend(chunk(b"IHDR", &[0; 13]));
        let mut still = png.clone();
        still.extend(chunk(b"IDAT", &[1, 2, 3]));
        assert!(!is_apng(&still));
        png.extend(chunk(b"acTL", &[0; 8]));
        png.extend(chunk(b"IDAT", &[1, 2, 3]));
        assert!(is_apng(&png));
    }

    #[test]
    fn webp_animation_flag() {
        let mut d = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0".to_vec();
        d.extend_from_slice(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(is_animated_webp(&d));
        d[20] = 0x10;
        assert!(!is_animated_webp(&d));
    }
}
