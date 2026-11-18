//! Zip files: the list of what's in one, from its central directory, and the files themselves,
//! inflated with `miniz_oxide`. Enough for previews of zips and of the formats built on them
//! (Word, Excel and PowerPoint files, 3MF models), not a general archiver: no encryption, no
//! methods but stored and deflated.

/// The most one file in a zip may unpack to, so a small zip can't fill memory.
pub const MAX_PART: usize = 512 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path inside the zip, with forward slashes. Folders end with one.
    pub name: String,
    pub method: u16,
    /// Bytes stored in the zip, and bytes once unpacked.
    pub packed: u64,
    pub size: u64,
    /// When it was last changed, as the zip recorded it: year, month, day, hour, minute.
    pub modified: Option<(u16, u8, u8, u8, u8)>,
    /// Locked with a password.
    pub encrypted: bool,
    offset: u64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    b.get(i..i + 2).map(|x| u16::from_le_bytes([x[0], x[1]]))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    b.get(i..i + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

fn u64_at(b: &[u8], i: usize) -> Option<u64> {
    b.get(i..i + 8).map(|x| u64::from_le_bytes(x.try_into().unwrap()))
}

/// The time and date fields of a zip entry, which count from 1980 in two-second steps.
fn dos_time(time: u16, date: u16) -> Option<(u16, u8, u8, u8, u8)> {
    let (day, month, year) = ((date & 31) as u8, ((date >> 5) & 15) as u8, 1980 + (date >> 9));
    let (minute, hour) = (((time >> 5) & 63) as u8, (time >> 11) as u8);
    (date != 0 && (1..=12).contains(&month) && (1..=31).contains(&day) && hour < 24 && minute < 60).then_some((year, month, day, hour, minute))
}

/// Whether these bytes start like a zip.
pub fn is_zip(data: &[u8]) -> bool {
    data.starts_with(b"PK\x03\x04") || data.starts_with(b"PK\x05\x06")
}

/// The files in a zip, from its central directory. `None` if it isn't one or it's damaged.
pub fn entries(data: &[u8]) -> Option<Vec<Entry>> {
    let floor = data.len().saturating_sub(22 + 65535);
    let end = (floor..=data.len().checked_sub(22)?).rev().find(|&i| u32_at(data, i) == Some(0x0605_4b50))?;
    let mut count = u16_at(data, end + 10)? as u64;
    let mut dir = u32_at(data, end + 16)? as u64;
    // Zip64: the real numbers are in a record of their own.
    if (count == 0xFFFF || dir == 0xFFFF_FFFF) && end >= 20 && u32_at(data, end - 20) == Some(0x0706_4b50) {
        let at = u64_at(data, end - 12)? as usize;
        if u32_at(data, at) == Some(0x0606_4b50) {
            count = u64_at(data, at + 32)?;
            dir = u64_at(data, at + 48)?;
        }
    }
    let mut out = Vec::new();
    let mut i = dir as usize;
    for _ in 0..count.min(1_000_000) {
        if u32_at(data, i)? != 0x0201_4b50 {
            break;
        }
        let flags = u16_at(data, i + 8)?;
        let method = u16_at(data, i + 10)?;
        let modified = dos_time(u16_at(data, i + 12)?, u16_at(data, i + 14)?);
        let mut packed = u32_at(data, i + 20)? as u64;
        let mut size = u32_at(data, i + 24)? as u64;
        let (n, e, c) = (u16_at(data, i + 28)? as usize, u16_at(data, i + 30)? as usize, u16_at(data, i + 32)? as usize);
        let mut offset = u32_at(data, i + 42)? as u64;
        let raw = data.get(i + 46..i + 46 + n)?;
        // Bit 11 says UTF-8; older zips use the DOS code page, which for plain names is the same.
        let name = if flags & 0x800 != 0 || std::str::from_utf8(raw).is_ok() { String::from_utf8_lossy(raw).into_owned() } else { raw.iter().map(|&b| cp437(b)).collect() };
        let name = name.replace('\\', "/");
        // Sizes too big for 32 bits are in the "extra" field, in this order.
        let extra = data.get(i + 46 + n..i + 46 + n + e)?;
        let mut k = 0;
        while k + 4 <= extra.len() {
            let (id, len) = (u16_at(extra, k)?, u16_at(extra, k + 2)? as usize);
            if id == 1 {
                let mut at = k + 4;
                if size == 0xFFFF_FFFF {
                    size = u64_at(extra, at).unwrap_or(size);
                    at += 8;
                }
                if packed == 0xFFFF_FFFF {
                    packed = u64_at(extra, at).unwrap_or(packed);
                    at += 8;
                }
                if offset == 0xFFFF_FFFF {
                    offset = u64_at(extra, at).unwrap_or(offset);
                }
            }
            k += 4 + len;
        }
        out.push(Entry { name, method, packed, size, modified, encrypted: flags & 1 != 0, offset });
        i += 46 + n + e + c;
    }
    Some(out)
}

/// A byte of the DOS code page zips used before UTF-8: ASCII as is, the rest as the nearest
/// letter where there is one.
fn cp437(b: u8) -> char {
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{A0}";
    if b < 0x80 { b as char } else { HIGH.chars().nth(b as usize - 0x80).unwrap_or('?') }
}

/// One file's bytes, unpacked.
pub fn read(data: &[u8], e: &Entry) -> Result<Vec<u8>, String> {
    let bad = || "The file is damaged.".to_string();
    if e.encrypted {
        return Err("The file is locked with a password.".into());
    }
    let offset = e.offset as usize;
    if u32_at(data, offset) != Some(0x0403_4b50) {
        return Err(bad());
    }
    let (n, x) = (u16_at(data, offset + 26).ok_or_else(bad)? as usize, u16_at(data, offset + 28).ok_or_else(bad)? as usize);
    let start = offset + 30 + n + x;
    let packed = data.get(start..start.checked_add(e.packed as usize).ok_or_else(bad)?).ok_or_else(bad)?;
    match e.method {
        0 => Ok(packed.to_vec()),
        8 => miniz_oxide::inflate::decompress_to_vec_with_limit(packed, MAX_PART).map_err(|_| bad()),
        _ => Err("The file is packed in a way View can't read.".into()),
    }
}

/// The entry with this path, ignoring case and a leading slash.
pub fn find<'a>(entries: &'a [Entry], name: &str) -> Option<&'a Entry> {
    let name = name.trim_start_matches('/');
    entries.iter().find(|e| e.name.eq_ignore_ascii_case(name))
}

/// A file's bytes as text, by its path in the zip.
pub fn read_text(data: &[u8], entries: &[Entry], name: &str) -> Option<String> {
    let bytes = read(data, find(entries, name)?).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A line of a zip's contents, as a tree: a folder or a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Its own name, without the folders it's in.
    pub name: String,
    pub depth: usize,
    pub folder: bool,
    /// Unpacked; for a folder, everything in it.
    pub size: u64,
    /// For a folder, the newest thing in it.
    pub modified: Option<(u16, u8, u8, u8, u8)>,
    /// For a folder: the index just past the last thing inside it.
    pub end: usize,
    /// For a folder: how many files are inside it, at any depth.
    pub files: usize,
    pub locked: bool,
}

/// What's in a zip, folders first and names in natural order, as the lines of a tree, each
/// folder followed by what's in it. With it, a line that sums it up.
pub struct Tree {
    pub nodes: Vec<Node>,
    pub summary: String,
    /// There was more than `max` lines' worth; the rest are left out.
    pub cut: bool,
}

#[derive(Default)]
struct Dir<'a> {
    dirs: Vec<(String, Dir<'a>)>,
    files: Vec<(String, &'a Entry)>,
}

impl<'a> Dir<'a> {
    fn dir(&mut self, name: &str) -> &mut Dir<'a> {
        let i = match self.dirs.iter().position(|(n, _)| n == name) {
            Some(i) => i,
            None => {
                self.dirs.push((name.to_string(), Dir::default()));
                self.dirs.len() - 1
            }
        };
        &mut self.dirs[i].1
    }

    /// Lay it out into `out`; returns its file count, size and newest date.
    fn flatten(&mut self, depth: usize, out: &mut Vec<Node>, max: usize) -> (usize, u64, Option<(u16, u8, u8, u8, u8)>) {
        self.dirs.sort_by(|a, b| crate::natural::compare(&a.0, &b.0));
        self.files.sort_by(|a, b| crate::natural::compare(&a.0, &b.0));
        let (mut files, mut size, mut newest) = (0, 0, None);
        for (name, d) in self.dirs.iter_mut() {
            let at = out.len();
            if at < max {
                out.push(Node { name: name.clone(), depth, folder: true, size: 0, modified: None, end: 0, files: 0, locked: false });
            }
            let (f, s, m) = d.flatten(depth + 1, out, max);
            let end = out.len();
            if let Some(n) = out.get_mut(at).filter(|_| at < max) {
                n.files = f;
                n.size = s;
                n.modified = m;
                n.end = end;
            }
            files += f;
            size += s;
            newest = newest.max(m);
        }
        for (name, e) in &self.files {
            if out.len() < max {
                out.push(Node { name: name.clone(), depth, folder: false, size: e.size, modified: e.modified, end: out.len() + 1, files: 0, locked: e.encrypted });
            }
            files += 1;
            size += e.size;
            newest = newest.max(e.modified);
        }
        (files, size, newest)
    }
}

/// A zip's contents as a tree, at most `max` lines of it. Mac clutter (`__MACOSX`,
/// `.DS_Store`) is left out. `None` if it isn't a zip.
pub fn tree(data: &[u8], max: usize) -> Option<Tree> {
    let list = entries(data)?;
    let mut root = Dir::default();
    for e in &list {
        let path = e.name.trim_start_matches('/');
        if path.starts_with("__MACOSX/") || path.rsplit('/').next() == Some(".DS_Store") {
            continue;
        }
        let parts: Vec<&str> = path.trim_end_matches('/').split('/').filter(|p| !p.is_empty() && *p != ".").collect();
        let Some((last, dirs)) = parts.split_last() else { continue };
        let mut d = &mut root;
        for p in dirs {
            d = d.dir(p);
        }
        if e.is_dir() {
            d.dir(last);
        } else {
            d.files.push((last.to_string(), e));
        }
    }
    let mut nodes = Vec::new();
    let (files, size, _) = root.flatten(0, &mut nodes, max);
    let folders = count_folders(&root);
    let packed: u64 = list.iter().filter(|e| !e.is_dir()).map(|e| e.packed).sum();
    let count = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let mut summary = count(files, "file", "files");
    if folders > 0 {
        summary.push_str(&format!(" in {}", count(folders, "folder", "folders")));
    }
    if files > 0 {
        summary.push_str(&format!("  ·  {}", crate::human_bytes(size)));
        if packed < size {
            summary.push_str(&format!(", {} packed", crate::human_bytes(packed)));
        }
    }
    let cut = files + folders > nodes.len();
    Some(Tree { nodes, summary, cut })
}

fn count_folders(d: &Dir) -> usize {
    d.dirs.iter().map(|(_, sub)| 1 + count_folders(sub)).sum()
}

/// A zip's date as Explorer shows it in short: "2 Mar 2024".
pub fn date_text(m: (u16, u8, u8, u8, u8)) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} {}", m.2, MONTHS[(m.1 as usize).clamp(1, 12) - 1], m.0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A zip holding `files`, the first stored and the rest deflated.
    pub fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut dir = Vec::new();
        for (i, (name, body)) in files.iter().enumerate() {
            let (method, packed) = if i == 0 { (0u16, body.to_vec()) } else { (8u16, miniz_oxide::deflate::compress_to_vec(body, 6)) };
            let offset = out.len() as u32;
            // 14:30 on 2 March 2024.
            let (time, date) = ((14u16 << 11) | (30 << 5), ((2024u16 - 1980) << 9) | (3 << 5) | 2);
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend([20, 0, 0, 0]);
            out.extend(method.to_le_bytes());
            out.extend(time.to_le_bytes());
            out.extend(date.to_le_bytes());
            out.extend([0u8; 4]); // crc
            out.extend((packed.len() as u32).to_le_bytes());
            out.extend((body.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(&packed);
            dir.extend(0x0201_4b50u32.to_le_bytes());
            dir.extend([20, 0, 20, 0, 0, 0]);
            dir.extend(method.to_le_bytes());
            dir.extend(time.to_le_bytes());
            dir.extend(date.to_le_bytes());
            dir.extend([0u8; 4]);
            dir.extend((packed.len() as u32).to_le_bytes());
            dir.extend((body.len() as u32).to_le_bytes());
            dir.extend((name.len() as u16).to_le_bytes());
            dir.extend([0u8; 12]); // extra, comment, disk, attributes
            dir.extend(offset.to_le_bytes());
            dir.extend(name.as_bytes());
        }
        let at = out.len() as u32;
        out.extend(&dir);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((dir.len() as u32).to_le_bytes());
        out.extend(at.to_le_bytes());
        out.extend([0u8; 2]);
        out
    }

    #[test]
    fn lists_and_reads_stored_and_deflated_files() {
        let data = zip(&[("a.txt", b"hello"), ("dir/", b""), ("dir/b.txt", &b"world ".repeat(100))]);
        assert!(is_zip(&data));
        let list = entries(&data).unwrap();
        assert_eq!(list.len(), 3);
        assert!(list[1].is_dir());
        assert_eq!(list[2].size, 600);
        assert!(list[2].packed < 600);
        assert_eq!(list[0].modified, Some((2024, 3, 2, 14, 30)));
        assert_eq!(read(&data, &list[0]).unwrap(), b"hello");
        assert_eq!(read_text(&data, &list, "/DIR/B.TXT").unwrap().len(), 600);
        assert!(entries(b"not a zip at all, just some words").is_none());
    }

    #[test]
    fn a_tree_of_folders_and_files() {
        let data = zip(&[
            ("b.txt", b"hello"),
            ("photos/2024/z.jpg", b"123"),
            ("photos/a10.jpg", b"1234"),
            ("photos/a9.jpg", b"12"),
            ("empty/", b""),
            ("__MACOSX/photos/._a9.jpg", b"junk"),
            ("photos/.DS_Store", b"junk"),
        ]);
        let t = tree(&data, 100).unwrap();
        let lines: Vec<String> = t.nodes.iter().map(|n| format!("{}{}{}", "  ".repeat(n.depth), n.name, if n.folder { "/" } else { "" })).collect();
        assert_eq!(lines, ["empty/", "photos/", "  2024/", "    z.jpg", "  a9.jpg", "  a10.jpg", "b.txt"]);
        let photos = &t.nodes[1];
        assert_eq!((photos.files, photos.size, photos.end), (3, 9, 6));
        assert_eq!(t.nodes[2].end, 4);
        assert_eq!(t.nodes[0].end, 1);
        assert!(t.summary.starts_with("4 files in 3 folders  ·  14 bytes"), "{}", t.summary);
        assert!(!t.cut);
        let small = tree(&data, 3).unwrap();
        assert_eq!(small.nodes.len(), 3);
        assert!(small.cut);
        assert_eq!(date_text((2024, 3, 2, 14, 30)), "2 Mar 2024");
    }
}
