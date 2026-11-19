//! View's engine, shared by every platform. Nothing here calls the operating system beyond the
//! standard library: the platform hosts bring the window, the native codecs and the shell.
//!
//! - [`folder`]: the images beside the one that's open, in the order the user picked.
//! - [`natural`]: "IMG 2" before "IMG 10", the way file managers sort.
//! - [`formats`]: what a file is, from its first bytes and its extension.
//! - [`viewport`]: fit, zoom about a point, pan, and the zoom steps.
//! - [`orient`]: EXIF orientation and the user's turns and flips, as one transform.
//! - [`picture`]: decoded pixels, ready for the GPU.
//! - [`decode`]: the decoders built into View (GIF, WebP and APNG), for animation and as a fallback.
//! - [`cache`]: a memory budget for decoded images, kept nearest the current one.
//! - [`metadata`]: EXIF and ICC details for the information panel.
//! - [`settings`]: the user's choices, as JSON.
//! - [`resume`]: where videos were left off.
//! - [`kind`]: whether a file is a picture, a video, a song, a PDF or text.
//! - [`textfile`] and [`markdown`]: text files read for the preview, coloured and laid out.
//! - [`mesh`]: 3D models (STL, 3MF and OBJ) read into triangles for the preview.
//! - [`pdftext`]: where the characters are on a PDF's pages, for selecting and copying them.
//! - [`find`]: looking for words in a document.
//! - [`field`]: a one-line text box's text, caret and selection.
//! - [`zip`] and [`office`]: what's in a zip, and the words in Word, Excel and PowerPoint files.
//! - [`slides`]: PowerPoint slides laid out as they look, for drawing.
//! - [`edit`]: crop, straighten and colour changes to a photo, for saving as a copy.
//! - [`update`]: reading GitHub's answer about the latest release, and checking a download.

pub mod cache;
pub mod decode;
pub mod folder;
pub mod field;
pub mod find;
pub mod formats;
pub mod html;
pub mod icc;
pub mod kind;
pub mod markdown;
pub mod mesh;
pub mod metadata;
pub mod natural;
pub mod orient;
pub mod pdftext;
mod pdfcrypt;
pub mod picture;
pub mod resume;
pub mod settings;
pub mod textfile;
pub mod tween;
pub mod update;
pub mod viewport;
mod xml;
pub mod zip;
pub mod office;
pub mod slides;
pub mod edit;

/// Format a byte count the way Explorer does: "1.4 MB", "820 KB", "12 bytes".
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return if n == 1 { "1 byte".into() } else { format!("{n} bytes") };
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if v < 10.0 { format!("{:.1} {}", v, UNITS[unit]) } else { format!("{:.0} {}", v, UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn bytes_read_like_explorer() {
        assert_eq!(human_bytes(0), "0 bytes");
        assert_eq!(human_bytes(1), "1 byte");
        assert_eq!(human_bytes(1023), "1023 bytes");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(820 * 1024), "820 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024 + 400 * 1024), "3.4 MB");
    }
}
