//! What the information panel shows from a file's EXIF data. Only what the file actually holds:
//! a missing value is left out, never guessed.

use exif::{Exif, Field, In, Reader, Tag, Value};
use std::io::BufReader;
use std::path::Path;

/// One line of the panel: a label and its value.
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    pub label: &'static str,
    pub value: String,
}

#[derive(Clone, Debug, Default)]
pub struct Camera {
    /// Lines for the panel, in reading order.
    pub facts: Vec<Fact>,
    /// The EXIF Orientation value, 1 to 8.
    pub orientation: Option<u32>,
    /// The colour space EXIF declares: "sRGB", "Adobe RGB", "Uncalibrated".
    pub colour_space: Option<String>,
}

/// Read a file's EXIF. Reads only the start of JPEG, PNG and WebP files; TIFF-based files
/// (including many RAW formats) are read whole, so do this off the interface thread.
pub fn read(path: &Path) -> Option<Camera> {
    let file = std::fs::File::open(path).ok()?;
    let exif = Reader::new().read_from_container(&mut BufReader::new(file)).ok()?;
    Some(summarise(&exif))
}

/// Parse EXIF from a raw block, as found in a file's APP1 segment or eXIf chunk.
pub fn from_raw(data: Vec<u8>) -> Option<Camera> {
    let data = if data.starts_with(b"Exif\0\0") { data[6..].to_vec() } else { data };
    let exif = Reader::new().read_raw(data).ok()?;
    Some(summarise(&exif))
}

fn summarise(exif: &Exif) -> Camera {
    let get = |tag: Tag| exif.get_field(tag, In::PRIMARY);
    let text = |tag: Tag| get(tag).and_then(ascii).filter(|s| !s.is_empty());
    let mut facts = Vec::new();
    let mut push = |label: &'static str, value: Option<String>| {
        if let Some(value) = value {
            facts.push(Fact { label, value });
        }
    };

    push("Camera", camera_name(text(Tag::Make), text(Tag::Model)));
    push("Lens", text(Tag::LensModel).or_else(|| text(Tag::LensMake)));
    push("Taken", text(Tag::DateTimeOriginal).or_else(|| text(Tag::DateTimeDigitized)).map(|s| exif_date(&s)));
    push("Exposure", get(Tag::ExposureTime).and_then(exposure_time));
    push("Aperture", get(Tag::FNumber).and_then(|f| rational(f)).filter(|v| *v > 0.0).map(|v| format!("f/{}", trim_float(v, 1))));
    push(
        "ISO",
        get(Tag::PhotographicSensitivity).and_then(|f| f.value.get_uint(0)).filter(|&v| v > 0).map(|v| v.to_string()),
    );
    let focal = get(Tag::FocalLength).and_then(rational).filter(|v| *v > 0.0);
    let equiv = get(Tag::FocalLengthIn35mmFilm).and_then(|f| f.value.get_uint(0)).filter(|&v| v > 0);
    push(
        "Focal length",
        focal.map(|f| match equiv {
            Some(e) if (e as f64 - f).abs() >= 1.0 => format!("{} mm ({e} mm in 35 mm terms)", trim_float(f, 1)),
            _ => format!("{} mm", trim_float(f, 1)),
        }),
    );
    push("Exposure bias", get(Tag::ExposureBiasValue).and_then(srational).filter(|v| v.abs() > 1e-6).map(|v| format!("{v:+.1} EV")));
    push("Flash", get(Tag::Flash).and_then(|f| f.value.get_uint(0)).map(|v| if v & 1 == 1 { "Fired" } else { "Did not fire" }.to_string()));
    push("White balance", get(Tag::WhiteBalance).and_then(|f| f.value.get_uint(0)).map(|v| if v == 1 { "Manual" } else { "Auto" }.to_string()));
    push("Location", gps(exif));
    push("Software", text(Tag::Software));
    push("Artist", text(Tag::Artist));
    push("Copyright", text(Tag::Copyright));

    let orientation = get(Tag::Orientation).and_then(|f| f.value.get_uint(0)).filter(|v| (1..=8).contains(v));
    let colour_space = get(Tag::ColorSpace).and_then(|f| f.value.get_uint(0)).map(|v| match v {
        1 => "sRGB".to_string(),
        2 => "Adobe RGB".to_string(),
        _ => {
            // Uncalibrated, but the interoperability index may say Adobe RGB (R03).
            let interop = exif.get_field(Tag::InteroperabilityIndex, In::PRIMARY).and_then(ascii);
            if interop.as_deref() == Some("R03") { "Adobe RGB".into() } else { "Uncalibrated".into() }
        }
    });
    Camera { facts, orientation, colour_space }
}

fn ascii(f: &Field) -> Option<String> {
    match &f.value {
        Value::Ascii(parts) => {
            let s: Vec<String> = parts.iter().map(|p| String::from_utf8_lossy(p).trim().to_string()).filter(|s| !s.is_empty()).collect();
            Some(s.join(" "))
        }
        _ => None,
    }
}

fn rational(f: &Field) -> Option<f64> {
    match &f.value {
        Value::Rational(v) => v.first().filter(|r| r.denom != 0).map(|r| r.num as f64 / r.denom as f64),
        Value::Short(_) | Value::Long(_) => f.value.get_uint(0).map(|v| v as f64),
        _ => None,
    }
}

fn srational(f: &Field) -> Option<f64> {
    match &f.value {
        Value::SRational(v) => v.first().filter(|r| r.denom != 0).map(|r| r.num as f64 / r.denom as f64),
        _ => rational(f),
    }
}

/// "Canon" + "Canon EOS R5" reads as "Canon EOS R5", not "Canon Canon EOS R5".
fn camera_name(make: Option<String>, model: Option<String>) -> Option<String> {
    match (make, model) {
        (Some(make), Some(model)) => {
            let first = make.split_whitespace().next().unwrap_or("").to_lowercase();
            if model.to_lowercase().starts_with(&first) { Some(model) } else { Some(format!("{make} {model}")) }
        }
        (make, model) => model.or(make),
    }
}

fn exposure_time(f: &Field) -> Option<String> {
    let Value::Rational(v) = &f.value else { return None };
    let r = v.first()?;
    if r.num == 0 || r.denom == 0 {
        return None;
    }
    let secs = r.num as f64 / r.denom as f64;
    Some(if secs >= 1.0 {
        format!("{} s", trim_float(secs, 1))
    } else {
        format!("1/{} s", (1.0 / secs).round() as u64)
    })
}

/// EXIF writes dates as "2024:06:01 14:22:05".
fn exif_date(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 19 && b[4] == b':' && b[7] == b':' {
        format!("{}-{}-{} {}", &s[0..4], &s[5..7], &s[8..10], &s[11..19])
    } else {
        s.to_string()
    }
}

fn gps(exif: &Exif) -> Option<String> {
    let coord = |tag: Tag, reference: Tag, neg: &[u8]| -> Option<f64> {
        let f = exif.get_field(tag, In::PRIMARY)?;
        let Value::Rational(v) = &f.value else { return None };
        if v.len() < 3 || v.iter().any(|r| r.denom == 0) {
            return None;
        }
        let deg = v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0;
        let r = exif.get_field(reference, In::PRIMARY).and_then(ascii).unwrap_or_default();
        Some(if neg.contains(&r.bytes().next().unwrap_or(b'N')) { -deg } else { deg })
    };
    let lat = coord(Tag::GPSLatitude, Tag::GPSLatitudeRef, b"S")?;
    let lon = coord(Tag::GPSLongitude, Tag::GPSLongitudeRef, b"W")?;
    if lat.abs() > 90.0 || lon.abs() > 180.0 {
        return None;
    }
    Some(format!("{lat:.5}, {lon:.5}"))
}

fn trim_float(v: f64, places: usize) -> String {
    let s = format!("{v:.places$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_names_do_not_repeat_the_make() {
        assert_eq!(camera_name(Some("Canon".into()), Some("Canon EOS R5".into())).as_deref(), Some("Canon EOS R5"));
        assert_eq!(camera_name(Some("NIKON CORPORATION".into()), Some("NIKON Z 6".into())).as_deref(), Some("NIKON Z 6"));
        assert_eq!(camera_name(Some("Apple".into()), Some("iPhone 15 Pro".into())).as_deref(), Some("Apple iPhone 15 Pro"));
        assert_eq!(camera_name(None, Some("X".into())).as_deref(), Some("X"));
        assert_eq!(camera_name(None, None), None);
    }

    #[test]
    fn dates_and_numbers_read_naturally() {
        assert_eq!(exif_date("2024:06:01 14:22:05"), "2024-06-01 14:22:05");
        assert_eq!(exif_date("garbage"), "garbage");
        assert_eq!(trim_float(2.80, 1), "2.8");
        assert_eq!(trim_float(4.0, 1), "4");
    }

    /// A minimal little-endian TIFF block with Make, Model and Orientation.
    fn tiny_exif() -> Vec<u8> {
        let mut d = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
        let entries: u16 = 3;
        d.extend_from_slice(&entries.to_le_bytes());
        let data_at = 8 + 2 + 12 * entries as u32 + 4;
        let make = b"Acme\0";
        let model = b"Acme Snapper 3\0";
        // Make (0x010F), ASCII
        d.extend_from_slice(&0x010Fu16.to_le_bytes());
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&(make.len() as u32).to_le_bytes());
        d.extend_from_slice(&data_at.to_le_bytes());
        // Model (0x0110)
        d.extend_from_slice(&0x0110u16.to_le_bytes());
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&(model.len() as u32).to_le_bytes());
        d.extend_from_slice(&(data_at + make.len() as u32).to_le_bytes());
        // Orientation (0x0112), SHORT 6
        d.extend_from_slice(&0x0112u16.to_le_bytes());
        d.extend_from_slice(&3u16.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&6u32.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(make);
        d.extend_from_slice(model);
        d
    }

    #[test]
    fn reads_a_raw_exif_block() {
        let mut raw = b"Exif\0\0".to_vec();
        raw.extend(tiny_exif());
        let cam = from_raw(raw).expect("parses");
        assert_eq!(cam.orientation, Some(6));
        assert_eq!(cam.facts[0], Fact { label: "Camera", value: "Acme Snapper 3".into() });
    }
}
