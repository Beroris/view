//! Just enough of ICC profiles to name one: "Display P3", "Adobe RGB (1998)", "sRGB IEC61966-2.1".

/// The profile's description tag, if it has a readable one.
pub fn description(icc: &[u8]) -> Option<String> {
    let count = be32(icc, 128)? as usize;
    for i in 0..count.min(256) {
        let at = 132 + i * 12;
        if icc.get(at..at + 4)? == b"desc" {
            let off = be32(icc, at + 4)? as usize;
            let len = be32(icc, at + 8)? as usize;
            let tag = icc.get(off..off.checked_add(len)?)?;
            return text(tag).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        }
    }
    None
}

/// The colour model the profile is for, from its header: "RGB", "CMYK", "Greyscale".
pub fn colour_model(icc: &[u8]) -> Option<&'static str> {
    Some(match icc.get(16..20)? {
        b"RGB " => "RGB",
        b"CMYK" => "CMYK",
        b"GRAY" => "Greyscale",
        b"Lab " => "Lab",
        _ => return None,
    })
}

fn text(tag: &[u8]) -> Option<String> {
    match tag.get(0..4)? {
        // ICC v2: a count and plain ASCII.
        b"desc" => {
            let n = be32(tag, 8)? as usize;
            let bytes = tag.get(12..12 + n.min(tag.len().saturating_sub(12)))?;
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
        }
        // ICC v4: localised UTF-16 strings. Prefer English, else take the first.
        b"mluc" => {
            let records = be32(tag, 8)? as usize;
            let size = be32(tag, 12)? as usize;
            if size < 12 {
                return None;
            }
            let mut pick = None;
            for r in 0..records.min(64) {
                let at = 16 + r * size;
                let lang = tag.get(at..at + 2)?;
                let len = be32(tag, at + 4)? as usize;
                let off = be32(tag, at + 8)? as usize;
                if pick.is_none() || lang == b"en" {
                    pick = Some((off, len));
                    if lang == b"en" {
                        break;
                    }
                }
            }
            let (off, len) = pick?;
            let units: Vec<u16> = tag.get(off..off.checked_add(len)?)?.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
            Some(String::from_utf16_lossy(&units[..end]))
        }
        _ => None,
    }
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    let b = d.get(at..at + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(tag: Vec<u8>) -> Vec<u8> {
        let mut p = vec![0u8; 128];
        p[16..20].copy_from_slice(b"RGB ");
        p.extend_from_slice(&1u32.to_be_bytes());
        let off = 128 + 4 + 12;
        p.extend_from_slice(b"desc");
        p.extend_from_slice(&(off as u32).to_be_bytes());
        p.extend_from_slice(&(tag.len() as u32).to_be_bytes());
        p.extend(tag);
        p
    }

    #[test]
    fn reads_v2_description() {
        let mut tag = b"desc\0\0\0\0".to_vec();
        let s = b"Adobe RGB (1998)\0";
        tag.extend_from_slice(&(s.len() as u32).to_be_bytes());
        tag.extend_from_slice(s);
        let p = profile(tag);
        assert_eq!(description(&p).as_deref(), Some("Adobe RGB (1998)"));
        assert_eq!(colour_model(&p), Some("RGB"));
    }

    #[test]
    fn reads_v4_description() {
        let text: Vec<u8> = "Display P3".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        let mut tag = b"mluc\0\0\0\0".to_vec();
        tag.extend_from_slice(&1u32.to_be_bytes());
        tag.extend_from_slice(&12u32.to_be_bytes());
        tag.extend_from_slice(b"enUS");
        tag.extend_from_slice(&(text.len() as u32).to_be_bytes());
        tag.extend_from_slice(&28u32.to_be_bytes());
        tag.extend(text);
        assert_eq!(description(&profile(tag)).as_deref(), Some("Display P3"));
    }

    #[test]
    fn junk_is_none() {
        assert_eq!(description(&[1, 2, 3]), None);
        assert_eq!(description(&vec![0xFF; 200]), None);
    }
}
