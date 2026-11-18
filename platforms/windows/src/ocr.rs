//! Reading the words in a picture, with Windows' own on-device text recognition
//! (Windows.Media.Ocr): for photos of text and for scanned PDFs, so their words can be selected,
//! copied and found like any other text. Nothing leaves the machine. It reads in the languages
//! the user has set up in Windows.

use view_core::pdftext::PageText;
use view_core::picture::Bitmap;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

/// A line's words, each with its box as fractions of the picture.
type Line = Vec<(String, [f32; 4])>;

/// A reader for the user's languages, if Windows has one for any of them.
pub struct Reader {
    engine: OcrEngine,
    max: u32,
}

impl Reader {
    /// Call on a thread that's joined COM.
    pub fn new() -> Option<Reader> {
        let engine = OcrEngine::TryCreateFromUserProfileLanguages().ok()?;
        let max = OcrEngine::MaxImageDimension().unwrap_or(2600).max(256);
        Some(Reader { engine, max })
    }

    /// The longest side a picture should have for reading: bigger ones are made smaller first.
    pub fn max_side(&self) -> u32 {
        self.max
    }

    /// The words in a picture of premultiplied BGRA pixels, as a page of text whose boxes are
    /// fractions of the picture. `None` if it couldn't be read; an empty page if it has no words.
    pub fn read(&self, bitmap: &Bitmap) -> Option<PageText> {
        let small;
        let b = if bitmap.width.max(bitmap.height) > self.max {
            small = shrink(bitmap, self.max)?;
            &small
        } else {
            bitmap
        };
        if b.width < 8 || b.height < 8 {
            return Some(PageText::default());
        }
        let (w, h) = (b.width as f32, b.height as f32);
        let lines = (|| -> windows::core::Result<Vec<Line>> {
            let writer = DataWriter::new()?;
            writer.WriteBytes(&b.data)?;
            let buffer = writer.DetachBuffer()?;
            let soft = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, b.width as i32, b.height as i32)?;
            let result = self.engine.RecognizeAsync(&soft)?.join()?;
            let found = result.Lines()?;
            let mut lines = Vec::new();
            for i in 0..found.Size()? {
                let words = found.GetAt(i)?.Words()?;
                let mut line = Vec::new();
                for k in 0..words.Size()? {
                    let word = words.GetAt(k)?;
                    let r = word.BoundingRect()?;
                    let text = word.Text()?.to_string();
                    if !text.is_empty() {
                        line.push((text, [r.X / w, r.Y / h, (r.X + r.Width) / w, (r.Y + r.Height) / h]));
                    }
                }
                if !line.is_empty() {
                    lines.push(line);
                }
            }
            Ok(lines)
        })()
        .ok()?;
        Some(PageText::from_words(&lines))
    }
}

/// A smaller copy whose longest side is `max`, each pixel the average of those it covers.
pub fn shrink(b: &Bitmap, max: u32) -> Option<Bitmap> {
    let k = b.width.max(b.height) as f64 / max as f64;
    let (w, h) = (((b.width as f64 / k) as u32).max(1), ((b.height as f64 / k) as u32).max(1));
    let mut out = Bitmap::try_new(w, h)?;
    let span = |i: u32, limit: u32| {
        let a = ((i as f64 * k) as u32).min(limit - 1);
        (a, (((i + 1) as f64 * k) as u32).clamp(a + 1, limit))
    };
    for y in 0..h {
        let (y0, y1) = span(y, b.height);
        for x in 0..w {
            let (x0, x1) = span(x, b.width);
            let mut sum = [0u32; 4];
            let mut n = 0u32;
            for sy in y0..y1 {
                let row = (sy * b.width) as usize * 4;
                for sx in x0..x1 {
                    let p = row + sx as usize * 4;
                    for (s, &v) in sum.iter_mut().zip(&b.data[p..p + 4]) {
                        *s += v as u32;
                    }
                    n += 1;
                }
            }
            let o = ((y * w + x) * 4) as usize;
            for (d, s) in out.data[o..o + 4].iter_mut().zip(sum) {
                *d = (s / n.max(1)) as u8;
            }
        }
    }
    Some(out)
}
