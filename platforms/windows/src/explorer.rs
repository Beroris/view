//! File Explorer, from outside: whether the keyboard is in a file list (Explorer's or the
//! desktop's), and which file is selected there. The first is cheap enough for a keyboard hook;
//! the second asks Explorer over COM, so it runs on the helper's own thread instead.

use crate::util::from_wide;
use std::path::PathBuf;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, IServiceProvider};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, w};

pub fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

fn is_explorer_window(class: &str) -> bool {
    matches!(class, "CabinetWClass" | "ExploreWClass")
}

fn is_desktop_window(class: &str) -> bool {
    matches!(class, "Progman" | "WorkerW")
}

/// Whether an Explorer window or the desktop is in front, wherever its keyboard focus is.
pub fn explorer_in_front() -> bool {
    let front = unsafe { GetForegroundWindow() };
    let class = class_of(front);
    !front.is_invalid() && (is_explorer_window(&class) || is_desktop_window(&class))
}

/// The Explorer window (or the desktop) in front, if the keyboard is in its list of files: not
/// renaming, not in the address bar or search box, not in the folder tree.
pub fn file_list_in_front() -> Option<HWND> {
    unsafe {
        let front = GetForegroundWindow();
        if front.is_invalid() {
            return None;
        }
        let class = class_of(front);
        if !is_explorer_window(&class) && !is_desktop_window(&class) {
            return None;
        }
        let thread = GetWindowThreadProcessId(front, None);
        let mut gui = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        GetGUIThreadInfo(thread, &mut gui).ok()?;
        let focus = gui.hwndFocus;
        if focus.is_invalid() || !matches!(class_of(focus).as_str(), "DirectUIHWND" | "SysListView32") {
            return None;
        }
        let parent = GetParent(focus).ok()?;
        (class_of(parent) == "SHELLDLL_DefView").then_some(front)
    }
}

/// The file selected in that window: the focused one if it's among the selection, else the
/// first selected. Files only: not virtual items like Control Panel's.
pub fn selected_file(window: HWND) -> Option<PathBuf> {
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        let browser = if is_desktop_window(&class_of(window)) { desktop_browser(&windows)? } else { folder_browser(&windows, window)? };
        let view: IFolderView2 = browser.QueryActiveShellView().ok()?.cast().ok()?;
        let selection = view.GetSelection(false).ok()?;
        let count = selection.GetCount().ok()?;
        if count == 0 {
            return None;
        }
        let mut item = selection.GetItemAt(0).ok()?;
        if let Ok(focused) = view.GetFocusedItem() {
            if focused >= 0 {
                if let Ok(f) = view.GetItem::<IShellItem>(focused) {
                    for i in 0..count {
                        if let Ok(s) = selection.GetItemAt(i) {
                            if s.Compare(&f, SICHINT_CANONICAL.0 as u32).is_ok_and(|order| order == 0) {
                                item = f;
                                break;
                            }
                        }
                    }
                }
            }
        }
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = from_wide(name.as_wide());
        CoTaskMemFree(Some(name.0 as *const _));
        (!path.is_empty()).then(|| PathBuf::from(path))
    }
}

fn desktop_browser(windows: &IShellWindows) -> Option<IShellBrowser> {
    unsafe {
        let location = VARIANT::from(0i32); // CSIDL_DESKTOP
        let root = VARIANT::default();
        let mut hwnd = 0i32;
        let found = windows.FindWindowSW(&location, &root, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH).ok()?;
        found.cast::<IServiceProvider>().ok()?.QueryService(&SID_STopLevelBrowser).ok()
    }
}

/// The file view the keyboard is in, inside `window`.
fn focused_view(window: HWND) -> Option<HWND> {
    unsafe {
        let thread = GetWindowThreadProcessId(window, None);
        let mut gui = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        GetGUIThreadInfo(thread, &mut gui).ok()?;
        let view = GetParent(gui.hwndFocus).ok()?;
        (class_of(view) == "SHELLDLL_DefView" && IsChild(window, view).as_bool()).then_some(view)
    }
}

/// The browser behind an Explorer window. With tabs, every tab is listed with the same window,
/// so the right one is picked by its file view: the one with the keyboard in it, else the tab in
/// front. Hidden tabs can still report themselves visible, so visibility comes last.
fn folder_browser(windows: &IShellWindows, window: HWND) -> Option<IShellBrowser> {
    unsafe {
        let focused = focused_view(window);
        // The tab in front is the first tab window in Z order.
        let front_tab = FindWindowExW(Some(window), None, w!("ShellTabWindowClass"), None).ok();
        let n = windows.Count().ok()?;
        let (mut in_front, mut visible, mut first) = (None, None, None);
        for i in 0..n {
            let Ok(item) = windows.Item(&VARIANT::from(i)) else { continue };
            let Ok(app) = item.cast::<IWebBrowserApp>() else { continue };
            if app.HWND().map(|h| h.0 as isize) != Ok(window.0 as isize) {
                continue;
            }
            let Ok(provider) = item.cast::<IServiceProvider>() else { continue };
            let Ok(browser) = provider.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else { continue };
            let view = browser.QueryActiveShellView().and_then(|v| v.GetWindow()).ok();
            if focused.is_some() && view == focused {
                return Some(browser);
            }
            let tab = browser.GetWindow().ok();
            if in_front.is_none() && front_tab.is_some() && tab == front_tab {
                in_front = Some(browser);
            } else if visible.is_none() && tab.is_some_and(|t| IsWindowVisible(t).as_bool()) && view.is_some_and(|v| IsWindowVisible(v).as_bool()) {
                visible = Some(browser);
            } else {
                first.get_or_insert(browser);
            }
        }
        in_front.or(visible).or(first)
    }
}
