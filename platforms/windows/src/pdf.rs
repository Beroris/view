//! PDFs, through Windows' own renderer (Windows.Data.Pdf), the one Edge's reader is built on.
//! A thread of its own opens the document and renders pages as they're asked for, at the width
//! they'll be shown, so nothing waits on it. The same thread reads the pages' text, for
//! selecting, with View's own reader: Windows' renderer only draws.

use crate::util::trace;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use view_core::pdftext::{self, PageText};
use view_core::picture::Bitmap;
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::StorageFile;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::HSTRING;

/// The most pages the preview lays out.
pub const MAX_PAGES: usize = 2000;

pub enum Msg {
    /// The pages' sizes, in DIPs.
    Opened(Vec<(f32, f32)>),
    Failed(String),
    Page { index: usize, width: u32, bitmap: Bitmap },
    /// A page's characters and where they are.
    Text { index: usize, text: PageText },
    /// The text can't be read (an encryption View doesn't know, a damaged file), so there's
    /// nothing to select.
    NoText,
}

enum Ask {
    Render(usize, u32),
    Text(usize),
}

pub struct Pdf {
    requests: Sender<Ask>,
    pub results: Receiver<Msg>,
}

impl Pdf {
    /// Ask for page `index` at `width` pixels across.
    pub fn want(&self, index: usize, width: u32) {
        let _ = self.requests.send(Ask::Render(index, width));
    }

    /// Ask for page `index`'s text.
    pub fn want_text(&self, index: usize) {
        let _ = self.requests.send(Ask::Text(index));
    }
}

/// The text side of the thread: the file read once, on the first page asked for.
enum Reader {
    Unread,
    Ready(pdftext::Document),
    Unusable,
}

/// Open a PDF on a thread of its own, which posts `message` to `hwnd` whenever it has news. It
/// ends once the returned handle is dropped.
pub fn open(path: PathBuf, hwnd: HWND, message: u32) -> Pdf {
    let (requests, wanted) = channel::<Ask>();
    let (send, results) = channel::<Msg>();
    let hwnd = hwnd.0 as isize;
    let _ = std::thread::Builder::new().name("pdf".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let post = || unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), message, WPARAM(0), LPARAM(0));
        };
        let doc = match load(&path) {
            Ok(d) => d,
            Err(e) => {
                let _ = send.send(Msg::Failed(e));
                post();
                return;
            }
        };
        let count = doc.PageCount().unwrap_or(0) as usize;
        let sizes: Vec<(f32, f32)> = (0..count.min(MAX_PAGES))
            .map(|i| doc.GetPage(i as u32).and_then(|p| p.Size()).map_or((612.0, 792.0), |s| (s.Width.max(1.0), s.Height.max(1.0))))
            .collect();
        if sizes.is_empty() {
            let _ = send.send(Msg::Failed("This PDF has no pages.".into()));
            post();
            return;
        }
        if send.send(Msg::Opened(sizes)).is_err() {
            return;
        }
        post();
        let mut reader = Reader::Unread;
        // Windows' text recognition, made the first time a page has no text of its own.
        let mut ocr: Option<Option<crate::ocr::Reader>> = None;
        // What's been asked for and not done yet, each once, in the order asked. Pictures of
        // pages go before text, and are looked at again between pages of text, since reading a
        // scanned page's words can take a moment and scrolling shouldn't wait on it.
        let mut renders: std::collections::VecDeque<(usize, u32)> = Default::default();
        let mut texts: std::collections::VecDeque<usize> = Default::default();
        loop {
            let idle = renders.is_empty() && texts.is_empty();
            let mut take = |a: Ask| match a {
                Ask::Render(i, w) if !renders.contains(&(i, w)) => renders.push_back((i, w)),
                Ask::Text(i) if !texts.contains(&i) => texts.push_back(i),
                _ => {}
            };
            if idle {
                match wanted.recv() {
                    Ok(a) => take(a),
                    Err(_) => return,
                }
            }
            while let Ok(more) = wanted.try_recv() {
                take(more);
            }
            if let Some((index, width)) = renders.pop_front() {
                let started = std::time::Instant::now();
                if let Some(bitmap) = render(&doc, index, width) {
                    trace(&format!("pdf page {index} at {width} px in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0));
                    if send.send(Msg::Page { index, width, bitmap }).is_err() {
                        return;
                    }
                    post();
                }
                continue;
            }
            let Some(index) = texts.pop_front() else { continue };
            if let Reader::Unread = reader {
                reader = read_text(&path, count);
                // Unreadable text can still be read off the page's picture, if Windows can.
                if let (Reader::Unusable, None) = (&reader, ocr.get_or_insert_with(crate::ocr::Reader::new)) {
                    if send.send(Msg::NoText).is_err() {
                        return;
                    }
                    post();
                }
            }
            let started = std::time::Instant::now();
            let mut text = match &reader {
                // A page the reader trips over just has no text to select.
                Reader::Ready(text_doc) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| text_doc.page(index))).ok().flatten().unwrap_or_default(),
                Reader::Unusable if ocr.as_ref().is_some_and(Option::is_some) => PageText::default(),
                _ => continue,
            };
            // No text on the page: it's likely a scan, so read the words off its picture.
            if text.glyphs.is_empty() {
                if let Some(r) = ocr.get_or_insert_with(crate::ocr::Reader::new) {
                    if let Some(read) = read_picture(&doc, index, r) {
                        text = read;
                    }
                }
            }
            trace(&format!("pdf text {index}: {} characters in {:.1} ms", text.glyphs.len(), started.elapsed().as_secs_f64() * 1000.0));
            if send.send(Msg::Text { index, text }).is_err() {
                return;
            }
            post();
        }
    });
    Pdf { requests, results }
}

/// Read the file for its text. Unusable if it's too big, damaged past reading, or its pages
/// don't line up with the ones Windows shows.
fn read_text(path: &std::path::Path, pages: usize) -> Reader {
    let started = std::time::Instant::now();
    if std::fs::metadata(path).map_or(true, |m| m.len() > pdftext::MAX_BYTES) {
        return Reader::Unusable;
    }
    let Ok(data) = std::fs::read(path) else { return Reader::Unusable };
    let opened = std::panic::catch_unwind(move || pdftext::open(data)).ok().and_then(Result::ok);
    trace(&format!("pdf text opened in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0));
    match opened {
        Some(d) if d.page_count() == pages => Reader::Ready(d),
        _ => Reader::Unusable,
    }
}
fn load(path: &std::path::Path) -> Result<PdfDocument, String> {
    let open = || -> windows::core::Result<PdfDocument> {
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?.join()?;
        PdfDocument::LoadFromFileAsync(&file)?.join()
    };
    open().map_err(|e| {
        const WRONG_PASSWORD: i32 = 0x8007052Bu32 as i32;
        if e.code().0 == WRONG_PASSWORD { "This PDF is locked with a password.".to_string() } else { "Windows couldn't open this PDF. It may be damaged.".to_string() }
    })
}

/// The words on page `index`, read off a picture of it big enough for small print.
fn read_picture(doc: &PdfDocument, index: usize, reader: &crate::ocr::Reader) -> Option<PageText> {
    let size = doc.GetPage(index as u32).ok()?.Size().ok()?;
    let long = reader.max_side().min(3000) as f32;
    let width = (long * size.Width / size.Width.max(size.Height).max(1.0)).round().max(64.0) as u32;
    let bitmap = render(doc, index, width)?;
    reader.read(&bitmap)
}

fn render(doc: &PdfDocument, index: usize, width: u32) -> Option<Bitmap> {
    let page = doc.GetPage(index as u32).ok()?;
    let size = page.Size().ok()?;
    let height = ((width as f32 * size.Height / size.Width.max(1.0)).round() as u32).clamp(1, 16384);
    let stream = InMemoryRandomAccessStream::new().ok()?;
    let options = PdfPageRenderOptions::new().ok()?;
    options.SetDestinationWidth(width).ok()?;
    options.SetDestinationHeight(height).ok()?;
    page.RenderWithOptionsToStreamAsync(&stream, &options).ok()?.join().ok()?;
    let len = stream.Size().ok()? as u32;
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).ok()?).ok()?;
    reader.LoadAsync(len).ok()?.join().ok()?;
    let mut png = vec![0u8; len as usize];
    reader.ReadBytes(&mut png).ok()?;
    view_core::decode::png_bitmap(&png).ok()
}
