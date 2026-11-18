//! Space to preview, in File Explorer: `view.exe --space-preview`, a small helper that starts
//! when the user signs in (only if they turned it on). It watches the keyboard, and when Space
//! is pressed in a list of files it shows the selected file in a preview window; Space or Esc
//! again closes it. The arrow keys still move through Explorer, and the preview follows.
//!
//! The helper itself stays tiny: no graphics, no decoding. The preview is a second process,
//! `view.exe --peek`, started the first time it's needed and gone a while after it closes, so
//! the memory a picture takes doesn't stay held.

use crate::explorer;
use crate::util::{Wide, trace};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

pub const CLASS: &str = "View.SpacePreview";
/// The preview window's class, in the `--peek` process.
pub const PEEK_CLASS: &str = "View.Peek";
/// Sent to the preview window to hide it.
pub const WM_PEEK_HIDE: u32 = WM_APP + 20;
/// Sent to the preview window to copy the text selected in it.
pub const WM_PEEK_COPY: u32 = WM_APP + 21;
/// Set on the preview window while it has text selected: then Ctrl+C in Explorer copies that.
pub const SELECTION_PROP: windows::core::PCWSTR = w!("View.Selection");
/// Set on the preview window while it shows a document, so Ctrl+F and Ctrl+G go to it.
pub const DOC_PROP: windows::core::PCWSTR = w!("View.Doc");
/// Set on the preview window while its find or page box is open: then typing goes there.
pub const TYPING_PROP: windows::core::PCWSTR = w!("View.Typing");
/// A key for the preview's find or page box: `wParam` is the virtual key, `lParam` the
/// [`KEY_SHIFT`] and other flags, with the scan code above them.
pub const WM_PEEK_KEY: u32 = WM_APP + 22;
pub const KEY_SHIFT: isize = 1;
pub const KEY_CTRL: isize = 2;
pub const KEY_ALT: isize = 4;
pub const KEY_CAPS: isize = 8;
/// `COPYDATASTRUCT.dwData` for "show this file": the data is "<anchor window>\n<path>" in UTF-16.
pub const COPY_SHOW: usize = 0x5649_4557;

const WM_SHOW: u32 = WM_APP + 30;
const WM_HIDE: u32 = WM_APP + 31;
const WM_FOLLOW: u32 = WM_APP + 32;
const TIMER_FOLLOW: usize = 1;
const TIMER_POLL: usize = 2;

/// The helper's window, for the hook to post to.
static HOST: AtomicIsize = AtomicIsize::new(0);
/// Space went down in a file list and was kept from Explorer, so its release must be too.
static SWALLOWING: AtomicBool = AtomicBool::new(false);
static ESCAPING: AtomicBool = AtomicBool::new(false);
/// Ctrl+C went to the preview, so the C's release is kept from Explorer too.
static COPYING: AtomicBool = AtomicBool::new(false);
/// A key whose press went to the preview, so its release is kept from Explorer too.
static HELD_BACK: AtomicU32 = AtomicU32::new(0);
/// When a letter or digit was last typed into a file list: Explorer finds files by their first
/// letters as you type, and a space can be part of that.
static LAST_TYPED: AtomicU64 = AtomicU64::new(0);

/// Run the helper until it's told to quit. Returns the exit code.
pub fn run() -> i32 {
    // One helper is plenty.
    let _single = unsafe { windows::Win32::System::Threading::CreateMutexW(None, true, w!("Local\\View.SpacePreview")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return 0;
    }
    let _running = unsafe { windows::Win32::System::Threading::CreateMutexW(None, false, w!("Local\\View.Running")) };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
    let Some(host) = make_window() else { return 1 };
    HOST.store(host.0 as isize, Ordering::Release);

    // The hook gets a thread of its own, so a slow answer from Explorer on this one can never
    // hold up anyone's typing.
    let _ = std::thread::Builder::new().name("keyboard".into()).spawn(|| unsafe {
        let module = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).ok();
        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), module.map(|m| m.into()), 0) else { return };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    });
    trace("space preview ready");
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
    0
}

fn make_window() -> Option<HWND> {
    let class = Wide::new(CLASS);
    let instance = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }.ok()?;
    let wc = WNDCLASSEXW { cbSize: size_of::<WNDCLASSEXW>() as u32, lpfnWndProc: Some(host_proc), hInstance: instance.into(), lpszClassName: class.pcwstr(), ..Default::default() };
    unsafe { RegisterClassExW(&wc) };
    // Never shown; a top-level window rather than a message-only one so `--quit` can find it.
    unsafe { CreateWindowExW(WINDOW_EX_STYLE(0), class.pcwstr(), w!("View Space preview"), WS_POPUP, 0, 0, 0, 0, None, None, Some(instance.into()), None) }.ok()
}

fn peek_window() -> Option<HWND> {
    let class = Wide::new(PEEK_CLASS);
    unsafe { FindWindowW(class.pcwstr(), None) }.ok()
}

/// The preview window, if it's showing and has `prop` set.
fn peek_with(prop: windows::core::PCWSTR) -> Option<HWND> {
    peek_window().filter(|&h| unsafe { IsWindowVisible(h).as_bool() && !GetPropW(h, prop).is_invalid() })
}

fn peek_visible() -> bool {
    peek_window().is_some_and(|h| unsafe { IsWindowVisible(h) }.as_bool())
}

fn post(msg: u32, wp: usize) {
    let host = HWND(HOST.load(Ordering::Acquire) as *mut _);
    unsafe {
        let _ = PostMessageW(Some(host), msg, WPARAM(wp), LPARAM(0));
    }
}

fn held(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

/// The keyboard hook. Windows waits on this for every key pressed anywhere, so it only looks
/// at a few window classes and posts anything slower to the helper's main thread.
extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let pass = || unsafe { CallNextHookEx(None, code, wp, lp) };
    if code < 0 {
        return pass();
    }
    // Keys other programs send count too: a remapper such as Orbit turning Caps Lock into Esc
    // sends Esc this way, and it must close the preview like the real key. (The helper never
    // sends keys itself, so it can't hear its own.)
    let k = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
    let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let vk = VIRTUAL_KEY(k.vkCode as u16);
    let swallow = LRESULT(1);
    if !down && HELD_BACK.load(Ordering::Acquire) == k.vkCode && HELD_BACK.swap(0, Ordering::AcqRel) == k.vkCode {
        return swallow;
    }
    let modifier = matches!(vk, VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_CONTROL | VK_LCONTROL | VK_RCONTROL | VK_MENU | VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN | VK_CAPITAL | VK_NUMLOCK);
    if !modifier {
        let (ctrl, alt, shift) = (held(VK_CONTROL), held(VK_MENU), held(VK_SHIFT));
        let win = held(VK_LWIN) || held(VK_RWIN);
        // While the preview's find or page box is open, typing goes there: everything but
        // shortcuts for Windows itself (Alt+Tab, the Windows key), and AltGr letters still type.
        let typing = !win && (ctrl || !alt) && vk != VK_TAB && peek_with(TYPING_PROP).is_some();
        // Ctrl+F finds and Ctrl+G goes to a page, in a document in the preview.
        let opening = down && ctrl && !alt && !win && matches!(vk, VK_F | VK_G) && peek_with(DOC_PROP).is_some();
        if (typing || opening) && explorer::explorer_in_front() {
            if down {
                let caps = unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1 != 0;
                let flags = if shift { KEY_SHIFT } else { 0 } | if ctrl { KEY_CTRL } else { 0 } | if alt { KEY_ALT } else { 0 } | if caps { KEY_CAPS } else { 0 } | ((k.scanCode as isize) << 16);
                if let Some(peek) = peek_window() {
                    unsafe {
                        let _ = PostMessageW(Some(peek), WM_PEEK_KEY, WPARAM(vk.0 as usize), LPARAM(flags));
                    }
                }
                HELD_BACK.store(k.vkCode, Ordering::Release);
            }
            return swallow;
        }
    }
    match vk {
        VK_SPACE => {
            if !down {
                return if SWALLOWING.swap(false, Ordering::AcqRel) { swallow } else { pass() };
            }
            if SWALLOWING.load(Ordering::Acquire) {
                return swallow; // held down: Windows repeats the key
            }
            let modified = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN].into_iter().any(held);
            if modified {
                return pass();
            }
            let Some(front) = explorer::file_list_in_front() else { return pass() };
            if peek_visible() {
                post(WM_HIDE, 0);
            } else {
                let typing = unsafe { GetTickCount64() }.saturating_sub(LAST_TYPED.load(Ordering::Relaxed)) < 1000;
                if typing {
                    return pass();
                }
                post(WM_SHOW, front.0 as usize);
            }
            SWALLOWING.store(true, Ordering::Release);
            swallow
        }
        VK_ESCAPE => {
            if !down {
                return if ESCAPING.swap(false, Ordering::AcqRel) { swallow } else { pass() };
            }
            if peek_visible() && explorer::explorer_in_front() {
                post(WM_HIDE, 0);
                ESCAPING.store(true, Ordering::Release);
                return swallow;
            }
            pass()
        }
        // Ctrl+C with text selected in the preview copies that text, not Explorer's files.
        VK_C if down && held(VK_CONTROL) && !held(VK_MENU) && !held(VK_SHIFT) => {
            let selected = peek_window().filter(|&h| unsafe { IsWindowVisible(h).as_bool() && !GetPropW(h, SELECTION_PROP).is_invalid() });
            match selected {
                Some(peek) if explorer::explorer_in_front() => {
                    unsafe {
                        let _ = PostMessageW(Some(peek), WM_PEEK_COPY, WPARAM(0), LPARAM(0));
                    }
                    COPYING.store(true, Ordering::Release);
                    swallow
                }
                _ => pass(),
            }
        }
        VK_C if !down && COPYING.swap(false, Ordering::AcqRel) => swallow,
        _ => {
            if down {
                let typed = matches!(vk.0, 0x30..=0x39 | 0x41..=0x5A | 0x60..=0x69 | 0xBA..=0xC0 | 0xDB..=0xDF);
                let moves = matches!(vk, VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_HOME | VK_END | VK_PRIOR | VK_NEXT);
                if typed && !held(VK_CONTROL) && !held(VK_MENU) {
                    LAST_TYPED.store(unsafe { GetTickCount64() }, Ordering::Relaxed);
                }
                if (typed || moves) && peek_visible() {
                    post(WM_FOLLOW, 0);
                }
            }
            pass()
        }
    }
}

/// What the helper's main thread remembers between keys.
struct Following {
    anchor: HWND,
    path: Option<PathBuf>,
}

static mut FOLLOWING: Following = Following { anchor: HWND(std::ptr::null_mut()), path: None };

#[allow(static_mut_refs)]
fn following() -> &'static mut Following {
    // Only ever touched on the helper's main thread.
    unsafe { &mut FOLLOWING }
}

extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SHOW => {
            let anchor = HWND(wp.0 as *mut _);
            let f = following();
            f.anchor = anchor;
            f.path = None;
            if let Some(path) = explorer::selected_file(anchor) {
                show(anchor, &path);
                f.path = Some(path);
                unsafe { SetTimer(Some(hwnd), TIMER_POLL, 300, None) };
            }
            LRESULT(0)
        }
        WM_HIDE => {
            if let Some(peek) = peek_window() {
                unsafe {
                    let _ = PostMessageW(Some(peek), WM_PEEK_HIDE, WPARAM(0), LPARAM(0));
                }
            }
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_POLL);
            }
            following().path = None;
            LRESULT(0)
        }
        WM_FOLLOW => {
            // Give Explorer a moment to move its selection first.
            unsafe { SetTimer(Some(hwnd), TIMER_FOLLOW, 40, None) };
            LRESULT(0)
        }
        WM_TIMER => {
            if wp.0 == TIMER_FOLLOW {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_FOLLOW);
                }
            }
            if !peek_visible() {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_POLL);
                }
                return LRESULT(0);
            }
            // The selection can change by keyboard or by mouse; either way, show what's selected.
            let f = following();
            if let Some(path) = explorer::selected_file(f.anchor) {
                if f.path.as_ref() != Some(&path) {
                    show(f.anchor, &path);
                    f.path = Some(path);
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Show `path` in the preview window, starting the preview process if it isn't running.
fn show(anchor: HWND, path: &std::path::Path) {
    trace(&format!("space: {}", path.display()));
    if let Some(peek) = peek_window() {
        let text: Vec<u16> = format!("{}\n{}", anchor.0 as isize, path.display()).encode_utf16().collect();
        let data = COPYDATASTRUCT { dwData: COPY_SHOW, cbData: (text.len() * 2) as u32, lpData: text.as_ptr() as *mut _ };
        unsafe {
            let host = HWND(HOST.load(Ordering::Acquire) as *mut _);
            let mut result = 0usize;
            let _ = SendMessageTimeoutW(peek, WM_COPYDATA, WPARAM(host.0 as usize), LPARAM(&data as *const _ as isize), SMTO_ABORTIFHUNG, 1000, Some(&mut result));
        }
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::process::Command::new(exe).arg("--peek").arg((anchor.0 as isize).to_string()).arg(path).spawn();
}

/// The Run-key value that starts the helper when the user signs in.
const RUN_VALUE: windows::core::PCWSTR = w!("View Space preview");
const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

/// Whether Space preview is turned on: the helper is set to start at sign-in.
pub fn enabled() -> bool {
    use windows::Win32::System::Registry::*;
    let mut size = 0u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, Some(&mut size)) }.is_ok()
}

/// Turn Space preview on or off: the sign-in entry, and the helper running now.
pub fn set_enabled(on: bool) -> bool {
    use windows::Win32::System::Registry::*;
    if on {
        let Ok(exe) = std::env::current_exe() else { return false };
        let command = Wide::new(&format!("\"{}\" --space-preview", exe.display()));
        let bytes = unsafe { std::slice::from_raw_parts(command.0.as_ptr() as *const u8, command.0.len() * 2) };
        let mut key = HKEY::default();
        unsafe {
            if RegCreateKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None).is_err() {
                return false;
            }
            let ok = RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes)).is_ok();
            let _ = RegCloseKey(key);
            if !ok {
                return false;
            }
        }
        let _ = std::process::Command::new(exe).arg("--space-preview").spawn();
        true
    } else {
        unsafe {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
        for class in [CLASS, PEEK_CLASS] {
            let c = Wide::new(class);
            if let Ok(h) = unsafe { FindWindowW(c.pcwstr(), None) } {
                unsafe {
                    let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        true
    }
}
