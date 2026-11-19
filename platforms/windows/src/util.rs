//! Small helpers: wide strings, where settings live, the machine's memory.

use std::path::{Path, PathBuf};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::core::PCWSTR;

/// A NUL-terminated UTF-16 copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Keeps a wide string alive for as long as the `PCWSTR` is used.
pub struct Wide(pub Vec<u16>);

impl Wide {
    pub fn new(s: &str) -> Wide {
        Wide(wide(s))
    }
    pub fn path(p: &Path) -> Wide {
        Wide(wide_path(p))
    }
    pub fn pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}

/// UTF-16 up to the first NUL, as a String.
pub fn from_wide(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// Whether this copy is the portable one: its file name says so, as in `View-1.0.0-Portable.exe`.
pub fn is_portable() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()))
        .is_some_and(|stem| stem.contains("portable"))
}

/// Where settings live. The portable copy keeps them in a "View settings" folder beside itself.
pub fn settings_dir() -> PathBuf {
    if is_portable() {
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("View settings"))) {
            return dir;
        }
    }
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("View")
}

pub fn settings_file() -> PathBuf {
    settings_dir().join("settings.json")
}

/// Where videos were left off, when the user asked for that to be remembered.
pub fn positions_file() -> PathBuf {
    settings_dir().join("positions.json")
}

/// Installed physical memory, in bytes.
pub fn total_memory() -> u64 {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.ullTotalPhys } else { 4 << 30 }
}

/// Memory free right now, in bytes.
pub fn available_memory() -> u64 {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.ullAvailPhys } else { 1 << 30 }
}

/// The main display's refresh rate, in frames a second. 60 if Windows won't say.
pub fn refresh_rate() -> f64 {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool();
    if ok && mode.dmDisplayFrequency > 1 { mode.dmDisplayFrequency as f64 } else { 60.0 }
}

/// Whether timings are being logged, for callers whose message costs something to put together.
pub fn tracing() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VIEW_TRACE").is_some())
}

/// Logs timings to `%TEMP%\view-trace.txt` when `VIEW_TRACE` is set. Off otherwise, and free.
pub fn trace(what: &str) {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Option<Instant>> = OnceLock::new();
    let Some(start) = START.get_or_init(|| std::env::var_os("VIEW_TRACE").map(|_| process_start().unwrap_or_else(Instant::now))) else {
        return;
    };
    let line = format!("{:>8.2} ms  {}\n", start.elapsed().as_secs_f64() * 1000.0, what);
    let path = std::env::temp_dir().join("view-trace.txt");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// When the process started, as an `Instant`, so traces count the loader's time too.
fn process_start() -> Option<std::time::Instant> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut created, mut exit, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exit, &mut kernel, &mut user) }.ok()?;
    let now = unsafe { GetSystemTimePreciseAsFileTime() };
    let as_u64 = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    let ago = as_u64(now).checked_sub(as_u64(created))?;
    std::time::Instant::now().checked_sub(std::time::Duration::from_nanos(ago * 100))
}

/// A file time in the user's own date and time format, in local time: "12/11/2026 22:40".
pub fn local_time(t: std::time::SystemTime) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::Globalization::{DATE_SHORTDATE, GetDateFormatEx, GetTimeFormatEx, TIME_NOSECONDS};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let Ok(since) = t.duration_since(std::time::UNIX_EPOCH) else { return String::new() };
    let ticks = since.as_nanos() as u64 / 100 + 116_444_736_000_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err() || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err() {
            return String::new();
        }
        let mut date = [0u16; 64];
        let mut time = [0u16; 64];
        GetDateFormatEx(PCWSTR::null(), DATE_SHORTDATE, Some(&local), PCWSTR::null(), Some(&mut date), PCWSTR::null());
        GetTimeFormatEx(PCWSTR::null(), TIME_NOSECONDS, Some(&local), PCWSTR::null(), Some(&mut time));
        format!("{} {}", from_wide(&date), from_wide(&time))
    }
}
