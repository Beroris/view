//! The images in the open image's folder, in the order the user picked. Only names, sizes and
//! dates are kept, never pixels, so a folder of ten thousand photos costs a few megabytes.

use crate::natural;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    /// By name, with numbers in number order, as Explorer sorts.
    #[default]
    Name,
    /// By name, letter by letter.
    Alphabetical,
    Modified,
    Created,
    Size,
}

impl SortKey {
    pub const ALL: [SortKey; 5] = [SortKey::Name, SortKey::Alphabetical, SortKey::Modified, SortKey::Created, SortKey::Size];

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Alphabetical => "Name, letter by letter",
            SortKey::Modified => "Date modified",
            SortKey::Created => "Date created",
            SortKey::Size => "Size",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
}

impl Entry {
    pub fn from_path(path: &Path) -> Entry {
        let meta = std::fs::metadata(path).ok();
        Entry {
            path: path.to_path_buf(),
            name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            size: meta.as_ref().map_or(0, |m| m.len()),
            modified: meta.as_ref().and_then(|m| m.modified().ok()),
            created: meta.as_ref().and_then(|m| m.created().ok()),
        }
    }
}

/// List the images in `dir`: files whose extension `wanted` accepts. Hidden and system files are
/// left to the platform to filter, through `keep`.
pub fn scan(dir: &Path, wanted: impl Fn(&str) -> bool, keep: impl Fn(&std::fs::Metadata) -> bool) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let path = item.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else { continue };
        if !wanted(ext) {
            continue;
        }
        // On Windows this comes from the directory listing itself, with no extra disk access.
        let Ok(meta) = item.metadata() else { continue };
        if !meta.is_file() || !keep(&meta) {
            continue;
        }
        out.push(Entry {
            name: item.file_name().to_string_lossy().into_owned(),
            path,
            size: meta.len(),
            modified: meta.modified().ok(),
            created: meta.created().ok(),
        });
    }
    Ok(out)
}

pub fn sort(entries: &mut [Entry], key: SortKey, descending: bool) {
    entries.sort_by(|a, b| {
        let o = compare(a, b, key);
        if descending { o.reverse() } else { o }
    });
}

fn compare(a: &Entry, b: &Entry, key: SortKey) -> Ordering {
    let by_name = || natural::compare(&a.name, &b.name);
    match key {
        SortKey::Name => by_name(),
        SortKey::Alphabetical => natural::compare_plain(&a.name, &b.name),
        SortKey::Modified => a.modified.cmp(&b.modified).then_with(by_name),
        SortKey::Created => a.created.cmp(&b.created).then_with(by_name),
        SortKey::Size => a.size.cmp(&b.size).then_with(by_name),
    }
}

/// Whether two paths name the same file, ignoring case where the file system does (Windows).
pub fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// The open folder and where in it we are.
#[derive(Default)]
pub struct Folder {
    pub entries: Vec<Entry>,
    pub index: usize,
}

impl Folder {
    /// A folder holding just the one file, until the real listing arrives.
    pub fn single(path: &Path) -> Folder {
        Folder { entries: vec![Entry::from_path(path)], index: 0 }
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.index)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Take a fresh listing, staying on `current`. If it isn't in the listing (it was filtered
    /// out, or the listing raced a rename), it's kept in its sorted place so it stays reachable.
    pub fn replace(&mut self, mut entries: Vec<Entry>, current: &Path, key: SortKey, descending: bool) {
        if !entries.iter().any(|e| same_path(&e.path, current)) && current.exists() {
            entries.push(Entry::from_path(current));
        }
        sort(&mut entries, key, descending);
        self.index = entries.iter().position(|e| same_path(&e.path, current)).unwrap_or(0);
        self.entries = entries;
    }

    pub fn resort(&mut self, key: SortKey, descending: bool) {
        let Some(current) = self.current().map(|e| e.path.clone()) else { return };
        sort(&mut self.entries, key, descending);
        self.index = self.entries.iter().position(|e| e.path == current).unwrap_or(0);
    }

    /// The index `delta` steps away, wrapping round at the ends if `wrap`.
    pub fn offset(&self, delta: isize, wrap: bool) -> Option<usize> {
        let n = self.entries.len() as isize;
        if n == 0 {
            return None;
        }
        let i = self.index as isize + delta;
        if wrap {
            Some(i.rem_euclid(n) as usize)
        } else if (0..n).contains(&i) {
            Some(i as usize)
        } else {
            None
        }
    }

    /// Drop an entry (deleted, or found missing), staying on the one that took its place.
    pub fn remove(&mut self, index: usize) {
        if index >= self.entries.len() {
            return;
        }
        self.entries.remove(index);
        if self.index > index || self.index >= self.entries.len() {
            self.index = self.index.saturating_sub(1);
        }
    }

    /// The images worth decoding ahead, nearest first, leaning the way the user is moving.
    pub fn neighbours(&self, forward: bool, count: usize, wrap: bool) -> Vec<usize> {
        let mut out = Vec::new();
        let (ahead, behind) = if forward { (1isize, -1isize) } else { (-1, 1) };
        // Two ahead for each one behind.
        let mut a = 1;
        let mut b = 1;
        while out.len() < count && (a + b) <= (self.entries.len() as isize) * 2 {
            for _ in 0..2 {
                if let Some(i) = self.offset(ahead * a, wrap) {
                    if i != self.index && !out.contains(&i) {
                        out.push(i);
                    }
                }
                a += 1;
            }
            if let Some(i) = self.offset(behind * b, wrap) {
                if i != self.index && !out.contains(&i) {
                    out.push(i);
                }
            }
            b += 1;
        }
        out.truncate(count);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, size: u64, secs: u64) -> Entry {
        Entry {
            path: PathBuf::from(format!("/pics/{name}")),
            name: name.into(),
            size,
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
            created: None,
        }
    }

    fn folder(names: &[&str]) -> Folder {
        let mut f = Folder::default();
        f.entries = names.iter().map(|n| entry(n, 0, 0)).collect();
        f
    }

    #[test]
    fn sorts_by_each_key() {
        let mut v = vec![entry("b10.jpg", 3, 1), entry("b2.jpg", 1, 3), entry("a.jpg", 2, 2)];
        sort(&mut v, SortKey::Name, false);
        assert_eq!(v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a.jpg", "b2.jpg", "b10.jpg"]);
        sort(&mut v, SortKey::Alphabetical, false);
        assert_eq!(v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a.jpg", "b10.jpg", "b2.jpg"]);
        sort(&mut v, SortKey::Modified, true);
        assert_eq!(v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["b2.jpg", "a.jpg", "b10.jpg"]);
        sort(&mut v, SortKey::Size, false);
        assert_eq!(v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["b2.jpg", "a.jpg", "b10.jpg"]);
    }

    #[test]
    fn offset_wraps_or_stops() {
        let mut f = folder(&["a", "b", "c"]);
        f.index = 2;
        assert_eq!(f.offset(1, false), None);
        assert_eq!(f.offset(1, true), Some(0));
        assert_eq!(f.offset(-3, true), Some(2));
        assert_eq!(f.offset(-1, false), Some(1));
    }

    #[test]
    fn neighbours_lean_forward() {
        let mut f = folder(&["a", "b", "c", "d", "e", "f"]);
        f.index = 2;
        assert_eq!(f.neighbours(true, 3, false), vec![3, 4, 1]);
        assert_eq!(f.neighbours(false, 3, false), vec![1, 0, 3]);
        f.index = 0;
        assert_eq!(f.neighbours(true, 3, false), vec![1, 2, 3]);
        assert_eq!(f.neighbours(true, 3, true), vec![1, 2, 5]);
        let one = folder(&["only"]);
        assert!(one.neighbours(true, 3, true).is_empty());
    }

    #[test]
    fn remove_keeps_position_sensible() {
        let mut f = folder(&["a", "b", "c"]);
        f.index = 1;
        f.remove(1);
        assert_eq!(f.current().unwrap().name, "c");
        f.remove(1);
        assert_eq!(f.current().unwrap().name, "a");
        f.remove(0);
        assert!(f.current().is_none());
    }

    #[test]
    fn scan_lists_only_images() {
        let dir = std::env::temp_dir().join(format!("view-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["IMG 10.jpg", "IMG 2.JPG", "notes.txt", "写真.png"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        std::fs::create_dir_all(dir.join("folder.jpg")).unwrap();
        let mut v = scan(&dir, |e| crate::formats::Format::from_extension(e).is_some(), |_| true).unwrap();
        sort(&mut v, SortKey::Name, false);
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["IMG 2.JPG", "IMG 10.jpg", "写真.png"]);
        let mut f = Folder::default();
        f.replace(v, &dir.join("IMG 10.jpg"), SortKey::Name, false);
        assert_eq!(f.index, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
