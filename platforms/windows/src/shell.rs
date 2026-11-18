//! The Windows shell: the clipboard, the Recycle Bin, Explorer, the open dialog and the
//! delete confirmation.

use crate::util::{Wide, wide, wide_path};
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, POINT};
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;
use windows::core::{BOOL, PCWSTR, w};

// ---- Clipboard ----

fn global_from(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Some(h)
    }
}

/// Open the clipboard, retrying briefly: a clipboard manager reacting to the last change may
/// hold it for a moment.
fn open(hwnd: HWND) -> bool {
    for attempt in 0..10 {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10 + attempt * 5));
    }
    false
}

fn set(format: u32, bytes: &[u8]) -> bool {
    let Some(h) = global_from(bytes) else { return false };
    if unsafe { SetClipboardData(format, Some(HANDLE(h.0))) }.is_err() {
        // Ownership only passes to the clipboard on success.
        unsafe {
            let _ = GlobalFree(Some(h));
        }
        return false;
    }
    true
}

fn with_clipboard(hwnd: HWND, fill: impl FnOnce() -> bool) -> bool {
    if !open(hwnd) {
        return false;
    }
    let ok = unsafe {
        let _ = EmptyClipboard();
        fill()
    };
    unsafe {
        let _ = CloseClipboard();
    }
    ok
}

fn registered(name: &str) -> u32 {
    let n = wide(name);
    unsafe { RegisterClipboardFormatW(PCWSTR(n.as_ptr())) }
}

/// A packed bottom-up 32-bit DIB, with straight (not premultiplied) alpha.
pub fn dib_bytes(width: u32, height: u32, pbgra: &[u8]) -> Vec<u8> {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: height as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: width * height * 4,
        ..Default::default()
    };
    let mut out = Vec::with_capacity(size_of::<BITMAPINFOHEADER>() + pbgra.len());
    out.extend_from_slice(unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<BITMAPINFOHEADER>()) });
    let stride = width as usize * 4;
    for row in pbgra.chunks_exact(stride).rev() {
        for p in row.chunks_exact(4) {
            let a = p[3] as u32;
            if a == 0 || a == 255 {
                out.extend_from_slice(p);
            } else {
                let un = |c: u8| ((c as u32 * 255 + a / 2) / a).min(255) as u8;
                out.extend_from_slice(&[un(p[0]), un(p[1]), un(p[2]), p[3]]);
            }
        }
    }
    out
}

/// Put an image on the clipboard: a DIB, which every app reads, and a PNG, which browsers,
/// chat apps and Office prefer because it keeps transparency exactly.
pub fn copy_image(hwnd: HWND, width: u32, height: u32, pbgra: &[u8], png: Option<&[u8]>) -> bool {
    let dib = dib_bytes(width, height, pbgra);
    with_clipboard(hwnd, || {
        let ok = set(CF_DIB.0 as u32, &dib);
        if let Some(png) = png {
            let f = registered("PNG");
            if f != 0 {
                set(f, png);
            }
        }
        ok
    })
}

/// Whether a photo's turn can be saved in its EXIF Orientation tag: JPEG and TIFF can.
pub fn can_save_orientation(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif" | "tif" | "tiff")
}

/// Save how a photo is turned in its EXIF Orientation tag (1 to 8), through Windows' own
/// property handler, the way Explorer's rotate buttons do. The pixels aren't decoded or saved
/// again, so nothing of the picture is lost.
pub fn save_orientation(path: &Path, exif: u16) -> windows::core::Result<()> {
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::UI::Shell::PropertiesSystem::{GPS_READWRITE, IPropertyStore, SHGetPropertyStoreFromParsingName};
    // System.Photo.Orientation.
    const ORIENTATION: PROPERTYKEY = PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0x14b81da1_0135_4d31_96d9_6cbfc9671a99), pid: 274 };
    let path = Wide::new(&path.to_string_lossy());
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreFromParsingName(path.pcwstr(), None, GPS_READWRITE)?;
        store.SetValue(&ORIENTATION, &PROPVARIANT::from(exif))?;
        store.Commit()
    }
}

/// The text on the clipboard, if there is some. It's left there.
pub fn paste_text(hwnd: HWND) -> Option<String> {
    use windows::Win32::System::DataExchange::{GetClipboardData, IsClipboardFormatAvailable};
    use windows::Win32::System::Memory::GlobalSize;
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_err() || !open(hwnd) {
        return None;
    }
    let text = unsafe {
        GetClipboardData(CF_UNICODETEXT.0 as u32).ok().and_then(|h| {
            let h = HGLOBAL(h.0);
            let p = GlobalLock(h) as *const u16;
            if p.is_null() {
                return None;
            }
            let units = std::slice::from_raw_parts(p, GlobalSize(h) / 2);
            let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
            let s = String::from_utf16_lossy(&units[..end]);
            let _ = GlobalUnlock(h);
            Some(s)
        })
    };
    unsafe {
        let _ = CloseClipboard();
    }
    text
}

pub fn copy_text(hwnd: HWND, text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(units.as_ptr() as *const u8, units.len() * 2) };
    with_clipboard(hwnd, || set(CF_UNICODETEXT.0 as u32, bytes))
}

/// Put the file itself on the clipboard, as Explorer's Copy does, so it pastes as a file.
pub fn copy_file(hwnd: HWND, path: &Path) -> bool {
    let header = DROPFILES { pFiles: size_of::<DROPFILES>() as u32, pt: POINT::default(), fNC: BOOL(0), fWide: BOOL(1) };
    let mut bytes = unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<DROPFILES>()) }.to_vec();
    for u in wide_path(path).into_iter().chain(std::iter::once(0)) {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let effect = DROPEFFECT_COPY.0.to_le_bytes();
    with_clipboard(hwnd, || {
        let ok = set(CF_HDROP.0 as u32, &bytes);
        let f = registered("Preferred DropEffect");
        if f != 0 {
            set(f, &effect);
        }
        ok
    })
}

// ---- Files ----

/// Show the file selected in an Explorer window.
pub fn reveal(path: &Path) {
    let p = Wide::path(path);
    unsafe {
        let pidl = ILCreateFromPathW(p.pcwstr());
        if pidl.is_null() {
            return;
        }
        let _ = SHOpenFolderAndSelectItems(pidl, None, 0);
        ILFree(Some(pidl));
    }
}

/// Move a file to the Recycle Bin. Where there's no Recycle Bin (a network share, some USB
/// drives), Windows asks before deleting it for good.
pub fn recycle(hwnd: HWND, path: &Path) -> Result<(), String> {
    let mut from = wide_path(path);
    from.push(0); // the list ends with a second NUL
    let mut op = SHFILEOPSTRUCTW {
        hwnd,
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING | FOF_SILENT).0 as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if op.fAnyOperationsAborted.as_bool() {
        return Err("cancelled".into());
    }
    if r != 0 {
        return Err(match r {
            0x02 | 0x7C => "The file isn't there any more.".into(),
            0x05 | 0x78 => "Windows won't let View delete this file.".into(),
            0x20 => "The file is open in another app.".into(),
            _ => format!("Windows couldn't delete the file (error {r:#x})."),
        });
    }
    Ok(())
}

/// The open dialog, starting in `folder`. `extensions` are the ones View can open.
pub fn open_dialog(hwnd: HWND, folder: Option<&Path>, extensions: &[String]) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let globs = |list: &mut dyn Iterator<Item = &str>| list.map(|e| format!("*.{e}")).collect::<Vec<_>>().join(";");
        let images_list = globs(&mut extensions.iter().map(String::as_str));
        let video_list = globs(&mut view_core::kind::VIDEO_EXTENSIONS.iter().copied());
        let audio_list = globs(&mut view_core::kind::AUDIO_EXTENSIONS.iter().copied());
        let every = [images_list.as_str(), video_list.as_str(), audio_list.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(";");
        let named = [
            (Wide::new("Pictures, videos and music"), Wide::new(&every)),
            (Wide::new("Pictures"), Wide::new(if images_list.is_empty() { "*.*" } else { &images_list })),
            (Wide::new("Videos"), Wide::new(&video_list)),
            (Wide::new("Music"), Wide::new(&audio_list)),
            (Wide::new("All files"), Wide::new("*.*")),
        ];
        let filters: Vec<COMDLG_FILTERSPEC> = named.iter().map(|(n, s)| COMDLG_FILTERSPEC { pszName: n.pcwstr(), pszSpec: s.pcwstr() }).collect();
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetTitle(w!("Open"));
        if let Some(dir) = folder {
            let d = Wide::path(dir);
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(d.pcwstr(), None) {
                let _ = dialog.SetFolder(&item);
            }
        }
        dialog.Show(Some(hwnd)).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(PathBuf::from)
    }
}

/// Open a file or folder with its default app, or show Windows' "Open with" for it.
pub fn open_with(hwnd: HWND, path: &Path) {
    let p = Wide::path(path);
    let info = OPENASINFO { pcszFile: p.pcwstr(), pcszClass: PCWSTR::null(), oaifInFlags: OAIF_EXEC | OAIF_ALLOW_REGISTRATION | OAIF_HIDE_REGISTRATION };
    unsafe {
        let _ = SHOpenWithDialog(Some(hwnd), &info);
    }
}

/// Windows' Properties sheet for a file.
pub fn properties(hwnd: HWND, path: &Path) {
    let p = Wide::path(path);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        hwnd,
        lpVerb: w!("properties"),
        lpFile: p.pcwstr(),
        nShow: 1,
        ..Default::default()
    };
    unsafe {
        let _ = ShellExecuteExW(&mut info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dib_is_bottom_up_and_straight_alpha() {
        let px = [10, 10, 10, 255, 64, 32, 0, 128];
        let d = dib_bytes(1, 2, &px);
        let hs = size_of::<BITMAPINFOHEADER>();
        assert_eq!(d.len(), hs + 8);
        assert_eq!(&d[hs..hs + 4], &[128, 64, 0, 128]);
        assert_eq!(&d[hs + 4..], &[10, 10, 10, 255]);
    }
}

// ---- Previews of any file ----

/// The shell's own picture of a file, about `px` square: a thumbnail for photos, videos and
/// documents that have one, its icon otherwise. Call on a thread of its own: some thumbnails
/// take a while.
pub fn thumbnail(path: &Path, px: i32) -> Option<view_core::picture::Bitmap> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::DeleteObject;
    let p = Wide::path(path);
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(p.pcwstr(), None).ok()?;
        let hbm = factory.GetImage(SIZE { cx: px, cy: px }, SIIGBF_BIGGERSIZEOK).ok()?;
        let pixels = hbitmap_pixels(hbm);
        let _ = DeleteObject(hbm.into());
        pixels
    }
}

/// A bitmap handle's pixels, as premultiplied BGRA. The shell's bitmaps carry alpha when they
/// have any; ones that don't leave it at zero, which here means opaque.
fn hbitmap_pixels(hbm: windows::Win32::Graphics::Gdi::HBITMAP) -> Option<view_core::picture::Bitmap> {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(hbm.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 {
            return None;
        }
        let mut out = view_core::picture::Bitmap::try_new(w as u32, h as u32)?;
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() },
            ..Default::default()
        };
        let dc = GetDC(None);
        let lines = GetDIBits(dc, hbm, 0, h as u32, Some(out.data.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        if lines == 0 {
            return None;
        }
        if out.data.chunks_exact(4).all(|px| px[3] == 0) {
            out.data.chunks_exact_mut(4).for_each(|px| px[3] = 255);
        }
        Some(out)
    }
}

/// A song's cover, or a video's frame: only a real thumbnail, never the file type's icon.
pub fn art(path: &Path, px: i32) -> Option<view_core::picture::Bitmap> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::DeleteObject;
    let p = Wide::path(path);
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(p.pcwstr(), None).ok()?;
        let hbm = factory.GetImage(SIZE { cx: px, cy: px }, SIIGBF_BIGGERSIZEOK | SIIGBF_THUMBNAILONLY).ok()?;
        let pixels = hbitmap_pixels(hbm);
        let _ = DeleteObject(hbm.into());
        pixels
    }
}

/// What a song's tags say, as Windows reads them.
#[derive(Clone, Debug, Default)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<String>,
}

pub fn tags(path: &Path) -> Tags {
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::core::GUID;
    const SUMMARY: GUID = GUID::from_u128(0xF29F85E0_4FF9_1068_AB91_08002B27B3D9);
    const MUSIC: GUID = GUID::from_u128(0x56A3372E_CE9C_11D2_9F0E_006097C686F6);
    let p = Wide::path(path);
    let Ok(item) = (unsafe { SHCreateItemFromParsingName::<_, _, IShellItem2>(p.pcwstr(), None) }) else { return Tags::default() };
    let get = |fmtid: GUID, pid: u32| -> Option<String> {
        let key = PROPERTYKEY { fmtid, pid };
        let s = unsafe { item.GetString(&key) }.ok()?;
        let out = unsafe { s.to_string() }.ok();
        unsafe { CoTaskMemFree(Some(s.0 as *const _)) };
        out.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
    };
    Tags { title: get(SUMMARY, 2), artist: get(MUSIC, 2).or_else(|| get(MUSIC, 13)), album: get(MUSIC, 4), year: get(MUSIC, 5) }
}

/// What Windows calls this kind of file: "PDF Document", "MP4 Video", "File folder".
pub fn type_name(path: &Path) -> String {
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    let p = Wide::path(path);
    let mut info = SHFILEINFOW::default();
    let ok = unsafe { SHGetFileInfoW(p.pcwstr(), FILE_FLAGS_AND_ATTRIBUTES(0), Some(&mut info), size_of::<SHFILEINFOW>() as u32, SHGFI_TYPENAME) };
    if ok == 0 { String::new() } else { crate::util::from_wide(&info.szTypeName) }
}

/// Open a file the way double-clicking it in Explorer would.
pub fn open_default(path: &Path) {
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let p = Wide::path(path);
    unsafe {
        ShellExecuteW(None, w!("open"), p.pcwstr(), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

// ---- The desktop background ----

/// Make an image file the desktop background on every screen, keeping the way it's fitted.
pub fn set_wallpaper(path: &Path) -> Result<(), String> {
    use windows::Win32::System::Com::CLSCTX_LOCAL_SERVER;
    use windows::Win32::UI::WindowsAndMessaging::{SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SystemParametersInfoW};
    let p = Wide::path(path);
    unsafe {
        if let Ok(wp) = CoCreateInstance::<_, IDesktopWallpaper>(&DesktopWallpaper, None, CLSCTX_LOCAL_SERVER) {
            if wp.SetWallpaper(PCWSTR::null(), p.pcwstr()).is_ok() {
                return Ok(());
            }
        }
        SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, Some(p.0.as_ptr() as *mut _), SPIF_UPDATEINIFILE | SPIF_SENDCHANGE).map_err(|e| format!("Windows wouldn't set the background: {}", e.message()))
    }
}

// ---- Undoing a delete ----

/// What a Recycle Bin `$I` file says: where the file came from, and when it was deleted.
fn recycled_from(info: &[u8]) -> Option<(PathBuf, u64)> {
    let version = u64::from_le_bytes(info.get(0..8)?.try_into().ok()?);
    let when = u64::from_le_bytes(info.get(16..24)?.try_into().ok()?);
    let units: Vec<u16> = match version {
        1 => info.get(24..24 + 520)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect(),
        2 => {
            let n = u32::from_le_bytes(info.get(24..28)?.try_into().ok()?) as usize;
            info.get(28..28 + n * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
        }
        _ => return None,
    };
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    Some((PathBuf::from(String::from_utf16_lossy(&units[..end])), when))
}

/// Put a file deleted to the Recycle Bin back where it was: the newest one deleted from there.
pub fn restore(original: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let root = original.ancestors().last().ok_or("That file's drive isn't there any more.")?;
    let bin = root.join("$Recycle.Bin");
    let wanted: Vec<u16> = original.as_os_str().encode_wide().map(|u| if (b'a' as u16..=b'z' as u16).contains(&u) { u - 32 } else { u }).collect();
    let mut best: Option<(u64, PathBuf, PathBuf)> = None;
    // Each user's own folder in the bin; others' can't be read, and are skipped.
    for user in std::fs::read_dir(&bin).map_err(|_| "View couldn't find the Recycle Bin on that drive.".to_string())?.flatten() {
        let Ok(files) = std::fs::read_dir(user.path()) else { continue };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix("$I") else { continue };
            let Ok(info) = std::fs::read(f.path()) else { continue };
            let Some((from, when)) = recycled_from(&info) else { continue };
            let from_upper: Vec<u16> = from.as_os_str().encode_wide().map(|u| if (b'a' as u16..=b'z' as u16).contains(&u) { u - 32 } else { u }).collect();
            if from_upper != wanted {
                continue;
            }
            let data = user.path().join(format!("$R{rest}"));
            if data.exists() && best.as_ref().is_none_or(|(t, _, _)| when > *t) {
                best = Some((when, data, f.path()));
            }
        }
    }
    let (_, data, info) = best.ok_or("It isn't in the Recycle Bin any more.")?;
    if original.exists() {
        return Err("There's a file with that name in its folder again.".into());
    }
    std::fs::rename(&data, original).map_err(|e| format!("Windows wouldn't move it back: {e}"))?;
    let _ = std::fs::remove_file(&info);
    let p = Wide::path(original);
    let b = Wide::path(&data);
    unsafe {
        SHChangeNotify(SHCNE_CREATE, SHCNF_PATHW, Some(p.0.as_ptr() as *const _), None);
        SHChangeNotify(SHCNE_DELETE, SHCNF_PATHW, Some(b.0.as_ptr() as *const _), None);
    }
    Ok(())
}

// ---- Dragging out ----

/// Drag a file out of View into another app or a folder, as Explorer would: it's copied (or
/// linked), never moved, so it's still here afterwards. Returns once it's dropped or let go.
pub fn drag_out(path: &Path) -> bool {
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::System::Ole::DROPEFFECT_LINK;
    let p = Wide::path(path);
    unsafe {
        let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(p.pcwstr(), None) else { return false };
        let Ok(data) = item.BindToHandler::<_, IDataObject>(None, &BHID_DataObject) else { return false };
        SHDoDragDrop(None, &data, None, DROPEFFECT_COPY | DROPEFFECT_LINK).is_ok()
    }
}
