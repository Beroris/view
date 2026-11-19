//! The few things that differ between systems: where settings live, the recycle bin, and
//! asking for a file.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where settings live. A copy named "portable" keeps them in a "View settings" folder beside
/// itself, as on Windows.
pub fn settings_dir() -> PathBuf {
    let exe = std::env::current_exe().ok();
    let portable = exe.as_ref().and_then(|e| e.file_stem()).is_some_and(|s| s.to_string_lossy().to_ascii_lowercase().contains("portable"));
    if let (true, Some(dir)) = (portable, exe.as_ref().and_then(|e| e.parent())) {
        return dir.join("View settings");
    }
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = var("HOME").unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/View")
    } else if cfg!(windows) {
        // Not the Windows app's own folder: this build shares its settings file's shape, not the file.
        var("APPDATA").unwrap_or(home).join("View desktop")
    } else {
        var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")).join("bero-view")
    }
}

/// Move a file to the system's bin, where it can be put back from.
pub fn bin(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| e.to_string())
}

/// Ask the user for a picture with the system's own file dialog. Waits for the answer, so
/// call it from a thread of its own. `None` if they cancelled or there's no dialog to ask with.
pub fn pick_file() -> Option<PathBuf> {
    let answer = |cmd: &mut Command| {
        let out = cmd.output().ok().filter(|o| o.status.success())?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!path.is_empty()).then(|| PathBuf::from(path))
    };
    if cfg!(target_os = "macos") {
        answer(Command::new("osascript").args(["-e", "POSIX path of (choose file of type {\"public.image\"} with prompt \"Open a picture\")"]))
    } else if cfg!(windows) {
        let script = "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.OpenFileDialog; if ($d.ShowDialog() -eq 'OK') { $d.FileName }";
        answer(Command::new("powershell").args(["-NoProfile", "-Command", script]))
    } else {
        let filter = "Pictures | *.jpg *.jpeg *.png *.gif *.webp *.bmp *.tif *.tiff *.ico";
        answer(Command::new("zenity").args(["--file-selection", "--title=Open a picture", &format!("--file-filter={filter}")]))
            .or_else(|| answer(Command::new("kdialog").args(["--getopenfilename", ".", "Pictures (*.jpg *.jpeg *.png *.gif *.webp *.bmp *.tif *.tiff *.ico)"])))
    }
}
