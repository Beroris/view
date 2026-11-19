//! New versions of View, from its releases on GitHub.
//!
//! View looks a few seconds after a window opens, at most once a day (Options has the switch).
//! A newer version downloads in the background and is checked against the size and SHA-256
//! GitHub lists for it. Then it installs itself when View is closed, or straight away from the
//! menu, which opens the same file again in the new version:
//!
//! - The installed copy runs the new Setup with no window. Setup closes View as `--quit` does
//!   and replaces the exe.
//! - The portable copy swaps its own exe for the new one under the same name, so shortcuts to it
//!   and its "View settings" folder keep working.
//! - Any other copy, such as a build from source, links to the release page instead.
//!
//! The check asks GitHub for the latest release and nothing else, with no cookies and no Windows
//! sign-in. Downloads come only from github.com. Network work runs on a thread of its own, which
//! posts what it found to the window. The Space preview never looks: only View's own window does.

use crate::util;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use view_core::update::{self as feed, Download, Kind, Release};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, PostMessageW, WM_APP};

/// The worker found something: `wparam` is a boxed [`Event`].
pub const WM_UPDATE: u32 = WM_APP + 11;

/// The version this copy compares against. A test build can pretend to be older.
pub const CURRENT: &str = match option_env!("VIEW_TEST_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
/// For testing the whole path without publishing: a local copy of GitHub's answer, whose files'
/// addresses start `https://github.com/local/` and are read from beside it. Never set for a
/// release.
const TEST_FEED: Option<&str> = option_env!("VIEW_TEST_FEED");

const DAY: u64 = 24 * 60 * 60;
/// After a look that failed, the next one waits this long rather than a day.
const RETRY: u64 = 60 * 60;
/// The installed copy's download waits here, in Windows' temp folder.
const DOWNLOAD_FOLDER: &str = "View update";
/// Passed to the new version once it has replaced the old one.
pub const UPDATED_ARG: &str = "--updated";

/// Where an update has got to.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Checking,
    UpToDate,
    /// A newer version is coming down.
    Downloading(String),
    /// Downloaded and checked: it installs when View closes, or now from the menu.
    Ready(String),
    /// Newer, but this copy can't install it: the release page has it.
    Available(String),
    Installing(String),
    Failed(String),
}

/// What choosing the menu's item should do.
pub enum Press {
    /// Install what's downloaded and open the file again.
    Install,
    /// Nothing more than these words.
    Say(String),
    Nothing,
}

pub struct Updater {
    kind: Kind,
    phase: Phase,
    /// The look was asked for from the menu, so its answer is said out loud.
    asked: bool,
    file: Option<PathBuf>,
    page: String,
}

/// What the worker found.
enum Event {
    Found(Release),
    UpToDate,
    Failed(String),
    Downloaded(PathBuf),
    DownloadFailed(String),
}

// --- On the window's thread -----------------------------------------------------------------

impl Updater {
    pub fn new() -> Updater {
        Updater { kind: this_copy(), phase: Phase::Idle, asked: false, file: None, page: feed::RELEASES_PAGE.to_string() }
    }

    /// The quiet look a little after a window opens. `wanted` is the switch in Options.
    pub fn start(&mut self, hwnd: HWND, wanted: bool) {
        if !wanted || self.phase != Phase::Idle {
            return;
        }
        // One that came down earlier, and couldn't be installed then, is still good.
        if let Some((version, file)) = waiting(self.kind) {
            self.phase = Phase::Ready(version);
            self.file = Some(file);
            return;
        }
        if due() {
            self.begin(hwnd, false);
        } else {
            // Not looking today, but an update may have just gone in: clear what it left.
            let kind = self.kind;
            let _ = std::thread::Builder::new().name("update".into()).spawn(move || clean_up(kind));
        }
    }

    /// The menu's item was chosen.
    pub fn press(&mut self, hwnd: HWND) -> Press {
        match &self.phase {
            Phase::Ready(_) => Press::Install,
            Phase::Available(_) => {
                self.open_page();
                Press::Nothing
            }
            Phase::Checking => Press::Say("Already looking for a new version".into()),
            Phase::Downloading(version) => Press::Say(format!("Already downloading View {version}")),
            Phase::Installing(version) => Press::Say(format!("Installing View {version}")),
            Phase::Idle | Phase::UpToDate | Phase::Failed(_) => {
                self.begin(hwnd, true);
                Press::Say("Looking for a new version\u{2026}".into())
            }
        }
    }

    fn begin(&mut self, hwnd: HWND, asked: bool) {
        if matches!(self.phase, Phase::Checking | Phase::Downloading(_) | Phase::Ready(_) | Phase::Installing(_)) {
            return;
        }
        self.phase = Phase::Checking;
        self.asked = asked;
        let kind = self.kind;
        let window = hwnd.0 as isize;
        let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
            let post = |event: Event| unsafe {
                let pointer = Box::into_raw(Box::new(event));
                if PostMessageW(Some(HWND(window as *mut _)), WM_UPDATE, WPARAM(pointer as usize), LPARAM(0)).is_err() {
                    // The window has gone: nobody will take it.
                    drop(Box::from_raw(pointer));
                }
            };
            clean_up(kind);
            match check(kind) {
                Err(error) => post(Event::Failed(error)),
                Ok(None) => post(Event::UpToDate),
                Ok(Some(release)) => {
                    let download = release.download.clone().filter(|_| kind.installs_itself());
                    post(Event::Found(release));
                    if let Some(download) = download {
                        post(match fetch(kind, &download) {
                            Ok(file) => Event::Downloaded(file),
                            Err(error) => Event::DownloadFailed(error),
                        });
                    }
                }
            }
        });
    }

    /// The worker's news arrived. Gives a line to show, if it's worth one.
    pub fn on_message(&mut self, pointer: usize) -> Option<String> {
        if pointer == 0 {
            return None;
        }
        let event = *unsafe { Box::from_raw(pointer as *mut Event) };
        match event {
            Event::Found(release) => {
                self.page = release.page.clone();
                let version = release.version;
                if self.kind.installs_itself() && release.download.is_some() {
                    self.phase = Phase::Downloading(version.clone());
                    self.asked.then(|| format!("Downloading View {version}\u{2026}"))
                } else {
                    self.phase = Phase::Available(version.clone());
                    stamp(now());
                    Some(format!("View {version} is out. The menu has it."))
                }
            }
            Event::UpToDate => {
                self.phase = Phase::UpToDate;
                stamp(now());
                self.asked.then(|| format!("View {CURRENT} is the newest version"))
            }
            Event::Downloaded(file) => {
                let Phase::Downloading(version) = &self.phase else { return None };
                let version = version.clone();
                self.phase = Phase::Ready(version.clone());
                self.file = Some(file);
                stamp(now());
                Some(format!("View {version} is ready. It installs when you close View."))
            }
            Event::Failed(error) | Event::DownloadFailed(error) => {
                // No connection, or GitHub being busy, will pass: look again in an hour.
                stamp(now().saturating_sub(DAY - RETRY));
                if self.asked {
                    self.phase = Phase::Failed(error.clone());
                    Some(error)
                } else {
                    self.phase = Phase::Idle;
                    None
                }
            }
        }
    }

    /// Installs the downloaded version now, and has it open `reopen` again. Setup closes this
    /// View; for the portable copy, `Ok(true)` says this one should close itself.
    pub fn install_now(&mut self, reopen: Option<&Path>) -> Result<bool, String> {
        let (Phase::Ready(version), Some(file)) = (&self.phase, &self.file) else {
            return Err("There's no update downloaded to install".into());
        };
        let (version, file) = (version.clone(), file.clone());
        let result = match self.kind {
            Kind::Installed => {
                let mut setup = std::process::Command::new(&file);
                setup.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/REOPEN=1"]);
                if let Some(path) = reopen {
                    setup.arg(format!("/REOPENFILE={}", path.display()));
                }
                setup.current_dir(std::env::temp_dir()).spawn().map(|_| false).map_err(|_| "Couldn't start the installer. Try again.".to_string())
            }
            Kind::Portable => std::env::current_exe().map_err(|_| "Couldn't find where View is".to_string()).and_then(|exe| {
                swap_in(&exe, &file)?;
                let mut new = std::process::Command::new(&exe);
                new.arg(UPDATED_ARG);
                if let Some(path) = reopen {
                    new.arg(path);
                }
                // The new version is in place either way, so opening View again starts it.
                new.spawn().map(|_| true).map_err(|_| "Updated. Open View again to start the new version.".to_string())
            }),
            Kind::Other => Err("This copy of View can't update itself".into()),
        };
        match &result {
            Ok(_) => self.phase = Phase::Installing(version),
            Err(error) => {
                self.phase = Phase::Failed(error.clone());
                self.file = None;
            }
        }
        result
    }

    /// View's window is closing: what's downloaded goes in now, with nothing shown. Not while
    /// another View window is open, since Setup would close that too.
    pub fn install_on_close(&mut self, closing: HWND) {
        let (Phase::Ready(_), Some(file)) = (&self.phase, &self.file) else { return };
        match self.kind {
            Kind::Installed => {
                if !another_window(closing) {
                    let _ = std::process::Command::new(file).args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]).current_dir(std::env::temp_dir()).spawn();
                }
            }
            Kind::Portable => {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = swap_in(&exe, file);
                }
            }
            Kind::Other => {}
        }
    }

    /// The new version's release page, with its notes.
    fn open_page(&self) {
        if self.page.starts_with(feed::DOWNLOADS_FROM) {
            crate::shell::open_default(Path::new(&self.page));
        }
    }

    /// The menu's item: what it says, and whether it can be chosen.
    pub fn menu_item(&self) -> (String, bool) {
        match &self.phase {
            Phase::Ready(version) => (format!("Install View {version} now"), true),
            Phase::Available(version) => (format!("Get View {version}\u{2026}"), true),
            Phase::Downloading(version) => (format!("Downloading View {version}\u{2026}"), false),
            Phase::Installing(version) => (format!("Installing View {version}\u{2026}"), false),
            Phase::Checking => ("Looking for updates\u{2026}".into(), false),
            Phase::Idle | Phase::UpToDate | Phase::Failed(_) => ("Check for updates".into(), true),
        }
    }
}

/// Whether a View window other than `this` is open.
fn another_window(this: HWND) -> bool {
    let class = util::Wide::new(crate::app::CLASS);
    let mut after = None;
    loop {
        match unsafe { FindWindowExW(None, after, class.pcwstr(), None) } {
            Ok(found) if found != this => return true,
            Ok(found) => after = Some(found),
            Err(_) => return false,
        }
    }
}

// --- When it last looked ---------------------------------------------------------------------

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// Kept in a file of its own, so two View windows saving their settings can't undo it.
fn stamp_file() -> PathBuf {
    util::settings_dir().join("update-checked.txt")
}

fn stamp(at: u64) {
    let file = stamp_file();
    if let Some(folder) = file.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    let _ = std::fs::write(file, at.to_string());
}

/// A day since the last look, or never looked. A clock set back counts as due.
fn due() -> bool {
    let last = std::fs::read_to_string(stamp_file()).ok().and_then(|text| text.trim().parse::<u64>().ok());
    last.is_none_or(|at| now() < at || now() - at >= DAY)
}

// --- On the worker ---------------------------------------------------------------------------

fn check(kind: Kind) -> Result<Option<Release>, String> {
    let body = get(feed::LATEST_RELEASE, true, 1 << 20)?;
    feed::parse_release(&body, kind, CURRENT)
}

/// Downloads `download`, checks it, and puts it where this kind of copy installs it from.
fn fetch(kind: Kind, download: &Download) -> Result<PathBuf, String> {
    let destination = download_path(kind, download)?;
    let limit = if download.size > 0 { download.size as usize } else { 64 << 20 };
    let body = get(&download.url, false, limit).map_err(|error| {
        if error.starts_with("Couldn't reach") { "Couldn't download the update. Check your connection.".to_string() } else { error }
    })?;
    feed::check_download(&body, download)?;
    let cant_save = |folder: &Path| format!("Couldn't save the update in {}", folder.display());
    let folder = destination.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(folder).map_err(|_| cant_save(folder))?;
    let partial = sibling(&destination, ".part");
    std::fs::write(&partial, &body).map_err(|_| cant_save(folder))?;
    let _ = std::fs::remove_file(&destination);
    if std::fs::rename(&partial, &destination).is_err() {
        let _ = std::fs::remove_file(&partial);
        return Err(cant_save(folder));
    }
    Ok(destination)
}

/// Closes a WinHTTP handle when it goes.
struct Net(*mut core::ffi::c_void);

impl Drop for Net {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = windows::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// GETs `url` over HTTPS, following GitHub's redirects to its file servers, with no more than
/// `limit` bytes back. `api` asks for the API's JSON.
fn get(url: &str, api: bool, limit: usize) -> Result<Vec<u8>, String> {
    use windows::Win32::Networking::WinHttp::*;
    use windows::core::{PCWSTR, w};
    if let Some(feed_file) = TEST_FEED {
        return test_get(feed_file, url);
    }
    let offline = || "Couldn't reach GitHub. Check your connection.".to_string();
    let rest = url.strip_prefix("https://").ok_or_else(offline)?;
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    unsafe {
        let agent = util::wide(&format!("View/{}", env!("CARGO_PKG_VERSION")));
        let session = Net(WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
        if session.0.is_null() {
            return Err(offline());
        }
        // Each step gives up rather than wait for ever: resolving, connecting, sending, and each
        // read.
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000);
        let host = util::wide(host);
        let connection = Net(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), 443, 0));
        if connection.0.is_null() {
            return Err(offline());
        }
        let path = util::wide(&path);
        let request = Net(WinHttpOpenRequest(connection.0, w!("GET"), PCWSTR(path.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), WINHTTP_FLAG_SECURE));
        if request.0.is_null() {
            return Err(offline());
        }
        let option = |option: u32, value: u32| {
            let _ = WinHttpSetOption(Some(request.0), option, Some(&value.to_le_bytes()));
        };
        option(WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_DISABLE_COOKIES);
        option(WINHTTP_OPTION_AUTOLOGON_POLICY, WINHTTP_AUTOLOGON_SECURITY_LEVEL_HIGH);
        let headers: Vec<u16> = if api { "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n" } else { "" }.encode_utf16().collect();
        WinHttpSendRequest(request.0, (!headers.is_empty()).then_some(&headers[..]), None, 0, 0, 0).map_err(|_| offline())?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|_| offline())?;
        let (mut status, mut size) = (0u32, 4u32);
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut core::ffi::c_void),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|_| offline())?;
        match status {
            200 => {}
            404 => return Err("Couldn't find View's releases on GitHub".into()),
            403 | 429 => return Err("GitHub is busy. Try again later.".into()),
            other => return Err(format!("GitHub answered {other}. Try again later.")),
        }
        let mut body = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr() as *mut core::ffi::c_void, chunk.len() as u32, &mut read)
                .map_err(|_| "The download stopped. Check your connection and try again.".to_string())?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > limit {
                return Err("GitHub sent more than the release lists. Try again later.".into());
            }
        }
        Ok(body)
    }
}

/// [`get`] for a test build: GitHub's answer and the files come from a folder.
fn test_get(feed_file: &str, url: &str) -> Result<Vec<u8>, String> {
    std::thread::sleep(Duration::from_millis(300));
    let path = if url == feed::LATEST_RELEASE {
        PathBuf::from(feed_file)
    } else {
        let name = url.strip_prefix("https://github.com/local/").filter(|name| feed::is_plain_file_name(name)).ok_or("Not a test file.")?;
        Path::new(feed_file).with_file_name(name)
    };
    std::fs::read(path).map_err(|_| "Couldn't reach GitHub. Check your connection.".to_string())
}

// --- Installing -------------------------------------------------------------------------------

/// Where this kind of copy keeps the new version until it's installed.
fn download_path(kind: Kind, download: &Download) -> Result<PathBuf, String> {
    match kind {
        Kind::Installed => Ok(std::env::temp_dir().join(DOWNLOAD_FOLDER).join(&download.name)),
        // Beside the exe, so moving it into place can't fail for being on another drive. Its
        // name carries the version, so a later View knows what's waiting.
        Kind::Portable => {
            let exe = std::env::current_exe().map_err(|_| "Couldn't find where View is".to_string())?;
            let version = feed::parse_release_name(&download.name).ok_or("The release's file has an odd name")?;
            Ok(sibling(&exe, &format!(".{version}.new")))
        }
        Kind::Other => Err("This copy of View can't update itself".into()),
    }
}

/// A newer version already downloaded and checked, by an earlier View.
fn waiting(kind: Kind) -> Option<(String, PathBuf)> {
    let current = feed::parse_version(CURRENT)?;
    let mut best: Option<((u64, u64, u64), PathBuf)> = None;
    let mut offer = |version: Option<(u64, u64, u64)>, file: PathBuf| {
        if let Some(version) = version.filter(|v| *v > current) {
            if best.as_ref().is_none_or(|(b, _)| version > *b) {
                best = Some((version, file));
            }
        }
    };
    match kind {
        Kind::Installed => {
            for entry in std::fs::read_dir(std::env::temp_dir().join(DOWNLOAD_FOLDER)).ok()?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with("-Setup.exe") {
                    offer(feed::parse_release_name(&name).and_then(|v| feed::parse_version(&v)), entry.path());
                }
            }
        }
        Kind::Portable => {
            let exe = std::env::current_exe().ok()?;
            let stem = format!("{}.", exe.file_name()?.to_string_lossy());
            for entry in std::fs::read_dir(exe.parent()?).ok()?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(version) = name.strip_prefix(&stem).and_then(|rest| rest.strip_suffix(".new")) {
                    offer(feed::parse_version(version), entry.path());
                }
            }
        }
        Kind::Other => {}
    }
    best.map(|((a, b, c), file)| (format!("{a}.{b}.{c}"), file))
}

/// `path` with `suffix` added to its whole file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Puts `new` where `exe` is, keeping the old one as "….old" until it has quit. Windows lets a
/// running exe be renamed, though not deleted.
fn swap_in(exe: &Path, new: &Path) -> Result<(), String> {
    let old = sibling(exe, ".old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|_| "Couldn't make room for the new version".to_string())?;
    if std::fs::rename(new, exe).is_err() {
        let _ = std::fs::rename(&old, exe);
        return Err("Couldn't put the new version in place".into());
    }
    Ok(())
}

/// Clears what an update left: the old exe, a download that never finished, and the Setup that
/// ran. A newer version still waiting to go in is kept.
fn clean_up(kind: Kind) {
    let keep = waiting(kind).map(|(_, file)| file);
    match kind {
        Kind::Portable => {
            let Ok(exe) = std::env::current_exe() else { return };
            let (Some(folder), Some(name)) = (exe.parent(), exe.file_name()) else { return };
            let stem = format!("{}.", name.to_string_lossy());
            for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let ours = name.strip_prefix(&stem).is_some_and(|rest| rest == "old" || rest.ends_with(".new") || rest.ends_with(".new.part"));
                if ours && Some(entry.path()) != keep {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        _ => {
            for entry in std::fs::read_dir(std::env::temp_dir().join(DOWNLOAD_FOLDER)).into_iter().flatten().flatten() {
                if Some(entry.path()) != keep {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// How this copy updates: by the portable name, or by being the copy Setup installed, which has
/// Setup's uninstaller beside it.
fn this_copy() -> Kind {
    if util::is_portable() {
        return Kind::Portable;
    }
    let installed = std::env::current_exe().ok().is_some_and(|exe| exe.parent().is_some_and(|folder| folder.join("unins000.exe").is_file()));
    if installed { Kind::Installed } else { Kind::Other }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_portable_swap_keeps_the_name() {
        let dir = std::env::temp_dir().join(format!("view-swap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("View-1.1.0-Portable.exe");
        let new = sibling(&exe, ".1.2.0.new");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        swap_in(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(sibling(&exe, ".old")).unwrap(), b"old");
        assert!(!new.exists());
        // Nothing to swap in: the old one stays where it was.
        assert!(swap_in(&exe, &new).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_menu_says_where_it_has_got_to() {
        let mut updater = Updater { kind: Kind::Installed, phase: Phase::Idle, asked: false, file: None, page: String::new() };
        assert_eq!(updater.menu_item(), ("Check for updates".to_string(), true));
        updater.phase = Phase::Downloading("9.0.0".into());
        assert!(!updater.menu_item().1);
        updater.phase = Phase::Ready("9.0.0".into());
        assert_eq!(updater.menu_item(), ("Install View 9.0.0 now".to_string(), true));
        // Ready, but with no file there's nothing to run.
        assert!(updater.install_now(None).is_err());
    }

    /// Asks GitHub for real, and downloads and checks the latest release's portable exe. Run by
    /// hand: `cargo test -p view-windows -- --ignored the_real_release`.
    #[test]
    #[ignore]
    fn the_real_release_downloads_and_checks_out() {
        let body = get(feed::LATEST_RELEASE, true, 1 << 20).expect("GitHub answers");
        let release = feed::parse_release(&body, Kind::Portable, "0.0.1").expect("it makes sense").expect("newer than 0.0.1");
        let download = release.download.expect("a portable exe");
        assert!(download.sha256.is_some(), "GitHub lists a digest for {}", download.name);
        let file = get(&download.url, false, download.size as usize).expect("it downloads, following GitHub's redirect");
        feed::check_download(&file, &download).expect("it's the file the release lists");
    }
}
