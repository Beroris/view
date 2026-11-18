//! The text in a PDF: which characters are where on each page, so the preview can select and
//! copy them. Windows draws the pages; this only reads them.
//!
//! Every object is found by scanning the file, so damaged cross-reference tables don't matter.
//! Each page's drawing instructions are followed just far enough to place its characters: the
//! fonts' encodings and widths, the text and graphics matrices, and forms drawn inside forms.
//! Nothing is rasterised. Characters the file gives no Unicode meaning for (some subset fonts)
//! are left out rather than guessed.

use crate::pdfcrypt::{Crypt, Params};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// Larger files aren't read for their text.
pub const MAX_BYTES: u64 = 256 << 20;
/// The most a stream may decode to, after each of its filters, and the most drawing
/// instructions a page may have in all.
const MAX_STREAM: usize = 128 << 20;
const MAX_GLYPHS: usize = 200_000;
/// The most text a page may hold, and a single character code may stand for.
const MAX_TEXT: usize = 4 << 20;
const MAX_MAPPING: usize = 64;
/// The most codes all of a document's character maps and width tables may hold, and the most
/// their streams may decode to.
const MAX_MAPPED: usize = 2_000_000;
const MAX_MAP_BYTES: usize = 256 << 20;
/// The most things (numbers, names, strings…) the file's objects may hold in all, and the
/// most one drawing instruction's operands or one block of a character map may.
const MAX_ITEMS: usize = 4_000_000;
const MAX_ITEMS_AT_ONCE: usize = 1_000_000;
/// Drawing instructions run per page; decoding a form counts too, one per 64 bytes each
/// step reads and writes, so a form drawn over and over can't keep the reader busy.
const MAX_OPS: usize = 4_000_000;

/// What goes between a character and the one before it, when the text is copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Break {
    None,
    Space,
    Line,
}

#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    /// Where its text is in [`PageText::text`], in bytes.
    pub start: u32,
    pub end: u32,
    /// Left, top, right and bottom, as fractions of the page as it's shown (turned if the page
    /// is turned).
    pub rect: [f32; 4],
    /// Which way its text runs on the page as shown, as a unit vector: (1, 0) for ordinary
    /// text, (0, 1) for text turned to run down the page.
    pub dir: [f32; 2],
    pub brk: Break,
}

/// One page's characters, in the order the file draws them, which is the reading order for
/// nearly every PDF.
#[derive(Debug, Default)]
pub struct PageText {
    pub text: String,
    pub glyphs: Vec<Glyph>,
}

impl PageText {
    pub fn str(&self, i: usize) -> &str {
        let g = &self.glyphs[i];
        &self.text[g.start as usize..g.end as usize]
    }

    /// The text of glyphs `from..to`, with spaces and line breaks where they fall.
    pub fn copy(&self, from: usize, to: usize) -> String {
        let mut out = String::new();
        for i in from..to.min(self.glyphs.len()) {
            if i > from {
                match self.glyphs[i].brk {
                    Break::Space => out.push(' '),
                    Break::Line => {
                        out.truncate(out.trim_end_matches([' ', '\t']).len());
                        out.push('\n');
                    }
                    Break::None => {}
                }
            }
            out.push_str(self.str(i));
        }
        out
    }

    /// Text read from a picture of a page (by OCR): lines of words, each with its box as
    /// fractions of the page. A word's characters share its box out equally.
    pub fn from_words(lines: &[Vec<(String, [f32; 4])>]) -> PageText {
        let mut page = PageText::default();
        for (l, line) in lines.iter().enumerate() {
            for (w, (word, r)) in line.iter().enumerate() {
                let n = word.chars().count().max(1) as f32;
                let step = (r[2] - r[0]) / n;
                for (i, c) in word.chars().enumerate() {
                    let start = page.text.len() as u32;
                    page.text.push(c);
                    let brk = match (i, w, l) {
                        (0, 0, 0) => Break::None,
                        (0, 0, _) => Break::Line,
                        (0, _, _) => Break::Space,
                        _ => Break::None,
                    };
                    let left = r[0] + step * i as f32;
                    page.glyphs.push(Glyph { start, end: page.text.len() as u32, rect: [left, r[1], left + step, r[3]], dir: [1.0, 0.0], brk });
                }
            }
        }
        page
    }

    /// The word around glyph `i`, as a range of glyphs.
    pub fn word(&self, i: usize) -> (usize, usize) {
        let wordy = |k: usize| self.str(k).chars().all(|c| c.is_alphanumeric() || c == '_');
        if i >= self.glyphs.len() || !wordy(i) {
            return (i, (i + 1).min(self.glyphs.len()));
        }
        let mut a = i;
        while a > 0 && self.glyphs[a].brk == Break::None && wordy(a - 1) {
            a -= 1;
        }
        let mut b = i + 1;
        while b < self.glyphs.len() && self.glyphs[b].brk == Break::None && wordy(b) {
            b += 1;
        }
        (a, b)
    }
}

// ---- Objects ----

#[derive(Clone, Debug)]
enum Obj {
    Null,
    Bool(bool),
    Num(f64),
    Str(Vec<u8>),
    Name(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
    Stream(Box<Stream>),
}

static NULL: Obj = Obj::Null;

#[derive(Clone, Debug, Default)]
struct Dict(Vec<(Vec<u8>, Obj)>);

impl Dict {
    fn get(&self, key: &[u8]) -> Option<&Obj> {
        self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

#[derive(Clone, Debug)]
struct Stream {
    dict: Dict,
    /// Its bytes in the file, still encoded.
    start: usize,
    end: usize,
    id: (u32, u16),
}

fn is_ws(c: u8) -> bool {
    matches!(c, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(c: u8) -> bool {
    matches!(c, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

enum Tok<'a> {
    Obj(Obj),
    Open,
    Close,
    DictOpen,
    DictClose,
    Word(&'a [u8]),
}

struct Lexer<'a> {
    s: &'a [u8],
    pos: usize,
    /// How many more things arrays and dictionaries may keep. A number costs a few bytes to
    /// write and many more to hold, so this is what stops a small file filling memory. Past
    /// it, the rest are read but not kept.
    room: usize,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a [u8]) -> Lexer<'a> {
        Lexer { s, pos: 0, room: MAX_ITEMS_AT_ONCE }
    }

    fn at(s: &'a [u8], pos: usize, room: usize) -> Lexer<'a> {
        Lexer { s, pos, room }
    }

    /// Whether there's room to keep one more thing, counting it if so.
    fn keep(&mut self) -> bool {
        let ok = self.room > 0;
        self.room = self.room.saturating_sub(1);
        ok
    }

    fn skip(&mut self) {
        let s = self.s;
        loop {
            while self.pos < s.len() && is_ws(s[self.pos]) {
                self.pos += 1;
            }
            if self.pos < s.len() && s[self.pos] == b'%' {
                while self.pos < s.len() && s[self.pos] != b'\n' && s[self.pos] != b'\r' {
                    self.pos += 1;
                }
            } else {
                return;
            }
        }
    }

    fn tok(&mut self) -> Option<Tok<'a>> {
        self.skip();
        let s = self.s;
        let c = *s.get(self.pos)?;
        let next = s.get(self.pos + 1).copied();
        self.pos += 1;
        Some(match c {
            b'[' => Tok::Open,
            b']' => Tok::Close,
            b'<' if next == Some(b'<') => {
                self.pos += 1;
                Tok::DictOpen
            }
            b'>' if next == Some(b'>') => {
                self.pos += 1;
                Tok::DictClose
            }
            b'<' => Tok::Obj(Obj::Str(self.hex())),
            b'(' => Tok::Obj(Obj::Str(self.literal())),
            b'/' => Tok::Obj(Obj::Name(self.name())),
            b'{' | b'}' | b')' | b'>' => Tok::Word(&s[self.pos - 1..self.pos]),
            _ => {
                let start = self.pos - 1;
                while self.pos < s.len() && !is_ws(s[self.pos]) && !is_delim(s[self.pos]) {
                    self.pos += 1;
                }
                let w = &s[start..self.pos];
                if w.iter().all(|c| matches!(c, b'0'..=b'9' | b'+' | b'-' | b'.')) {
                    Tok::Obj(Obj::Num(number(w)))
                } else {
                    match w {
                        b"true" => Tok::Obj(Obj::Bool(true)),
                        b"false" => Tok::Obj(Obj::Bool(false)),
                        b"null" => Tok::Obj(Obj::Null),
                        _ => Tok::Word(w),
                    }
                }
            }
        })
    }

    fn hex(&mut self) -> Vec<u8> {
        let s = self.s;
        let mut out = Vec::new();
        let mut half: Option<u8> = None;
        while self.pos < s.len() {
            let c = s[self.pos];
            self.pos += 1;
            let v = match c {
                b'>' => break,
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => continue,
            };
            match half.take() {
                Some(h) => out.push(h << 4 | v),
                None => half = Some(v),
            }
        }
        if let Some(h) = half {
            out.push(h << 4);
        }
        out
    }

    fn literal(&mut self) -> Vec<u8> {
        let s = self.s;
        let mut out = Vec::new();
        let mut depth = 1;
        while self.pos < s.len() {
            let c = s[self.pos];
            self.pos += 1;
            match c {
                b'(' => {
                    depth += 1;
                    out.push(c);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(c);
                }
                b'\\' => {
                    let Some(&e) = s.get(self.pos) else { break };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                match s.get(self.pos) {
                                    Some(&d @ b'0'..=b'7') => {
                                        v = v * 8 + (d - b'0') as u32;
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        b'\r' => {
                            if s.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        _ => out.push(e),
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    fn name(&mut self) -> Vec<u8> {
        let s = self.s;
        let mut out = Vec::new();
        while self.pos < s.len() && !is_ws(s[self.pos]) && !is_delim(s[self.pos]) {
            let c = s[self.pos];
            self.pos += 1;
            if c == b'#' && self.pos + 1 < s.len() {
                if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&s[self.pos..self.pos + 2]).unwrap_or("x"), 16) {
                    out.push(v);
                    self.pos += 2;
                    continue;
                }
            }
            out.push(c);
        }
        out
    }

    /// The next token inside an array or dictionary, or `None` at a keyword that can only come
    /// after one, which is left to be read again. A damaged object then stops where the next
    /// one starts instead of swallowing the rest of the file.
    fn tok_within(&mut self) -> Option<Tok<'a>> {
        let before = self.pos;
        let t = self.tok()?;
        if let Tok::Word(b"obj" | b"endobj" | b"stream" | b"endstream" | b"trailer" | b"xref" | b"startxref") = t {
            self.pos = before;
            return None;
        }
        Some(t)
    }

    /// The next whole object. `refs` reads `1 0 R` as a reference (not in content streams).
    fn obj(&mut self, refs: bool) -> Option<Obj> {
        let t = self.tok()?;
        self.finish(t, refs, 0)
    }

    fn finish(&mut self, t: Tok<'a>, refs: bool, depth: usize) -> Option<Obj> {
        if depth > 64 {
            return None;
        }
        match t {
            Tok::Obj(Obj::Num(n)) if refs && n >= 0.0 && n.fract() == 0.0 => {
                let save = self.pos;
                if let Some(Tok::Obj(Obj::Num(g))) = self.tok() {
                    if g >= 0.0 && g.fract() == 0.0 {
                        if let Some(Tok::Word(b"R")) = self.tok() {
                            return Some(Obj::Ref(n as u32));
                        }
                    }
                }
                self.pos = save;
                Some(Obj::Num(n))
            }
            Tok::Obj(o) => Some(o),
            Tok::Open => {
                let mut v = Vec::new();
                while let Some(t) = self.tok_within() {
                    if let Tok::Close = t {
                        break;
                    }
                    if let Some(o) = self.finish(t, refs, depth + 1) {
                        if self.keep() {
                            v.push(o);
                        }
                    }
                }
                Some(Obj::Array(v))
            }
            Tok::DictOpen => {
                let mut d = Vec::new();
                while let Some(t) = self.tok_within() {
                    match t {
                        Tok::DictClose => break,
                        Tok::Obj(Obj::Name(k)) => {
                            let Some(t) = self.tok_within() else { break };
                            if let Tok::DictClose = t {
                                if self.keep() {
                                    d.push((k, Obj::Null));
                                }
                                break;
                            }
                            let v = self.finish(t, refs, depth + 1).unwrap_or(Obj::Null);
                            if self.keep() {
                                d.push((k, v));
                            }
                        }
                        _ => {}
                    }
                }
                Some(Obj::Dict(Dict(d)))
            }
            _ => None,
        }
    }
}

fn number(w: &[u8]) -> f64 {
    let s = std::str::from_utf8(w).unwrap_or("0");
    if let Ok(v) = s.parse::<f64>() {
        return v;
    }
    // Writers get these wrong in small ways: "--5", "1.2.3", "-".
    let neg = s.starts_with('-');
    let digits: String = s.trim_start_matches(['-', '+']).chars().scan(false, |dot, c| {
        if c == '.' {
            if *dot {
                return None;
            }
            *dot = true;
        }
        if c == '-' || c == '+' { None } else { Some(c) }
    }).collect();
    let v = digits.parse::<f64>().unwrap_or(0.0);
    if neg { -v } else { v }
}

fn find(h: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= h.len() {
        return None;
    }
    let first = needle[0];
    let mut i = from;
    while i + needle.len() <= h.len() {
        let p = h[i..h.len() - needle.len() + 1].iter().position(|&c| c == first)?;
        i += p;
        if &h[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ---- The document ----

struct Page {
    contents: Obj,
    /// Shared with the other pages that inherit it.
    resources: Rc<Obj>,
    /// The visible part of the page, in its own units: left, bottom, right, top.
    crop: [f64; 4],
    /// Clockwise, in degrees: 0, 90, 180 or 270.
    rotate: u32,
}

pub struct Document {
    data: Vec<u8>,
    /// Each object, and where in the file it was defined (later definitions win).
    objs: HashMap<u32, (Obj, usize)>,
    crypt: Option<Crypt>,
    pages: Vec<Page>,
    fonts: RefCell<HashMap<u64, Rc<Font>>>,
    /// How many more things the objects may hold (see [`MAX_ITEMS`]).
    room: usize,
    /// Character maps and composite fonts' widths, read once each, and how many more codes
    /// and bytes the document's maps may take.
    cmaps: RefCell<HashMap<(u32, u16), Option<Rc<CMap>>>>,
    cid_widths: RefCell<HashMap<u64, Rc<HashMap<u32, f64>>>>,
    map_room: Cell<usize>,
    map_bytes: Cell<usize>,
}

/// Read a PDF's structure. The pages' text is read later, one page at a time.
pub fn open(data: Vec<u8>) -> Result<Document, String> {
    if find(&data[..data.len().min(1024)], b"%PDF", 0).is_none() {
        return Err("This isn't a PDF.".into());
    }
    let mut doc = Document { data, objs: HashMap::new(), crypt: None, pages: Vec::new(), fonts: RefCell::new(HashMap::new()), room: MAX_ITEMS, cmaps: RefCell::default(), cid_widths: RefCell::default(), map_room: Cell::new(MAX_MAPPED), map_bytes: Cell::new(MAX_MAP_BYTES) };
    let (trailers, objstms) = doc.scan();
    if let Some(t) = trailers.iter().rev().find(|t| t.get(b"Encrypt").is_some()) {
        doc.crypt = Some(doc.security(t)?);
    }
    doc.expand(objstms);
    let root = trailers.iter().rev().find_map(|t| t.get(b"Root").cloned()).or_else(|| {
        doc.objs.iter().filter(|(_, (o, _))| matches!(o, Obj::Dict(d) if d.get(b"Type").and_then(name) == Some(&b"Catalog"[..]))).map(|(n, _)| Obj::Ref(*n)).next()
    });
    let pages = root.as_ref().and_then(|r| doc.dict_of(Some(r))).and_then(|d| d.get(b"Pages")).cloned().ok_or("This PDF has no pages.")?;
    let mut out = Vec::new();
    doc.walk(&pages, &Inherit::default(), &mut out, &mut HashSet::new(), 0);
    doc.pages = out;
    Ok(doc)
}

#[derive(Clone, Default)]
struct Inherit {
    resources: Option<Rc<Obj>>,
    media: Option<[f64; 4]>,
    crop: Option<[f64; 4]>,
    rotate: Option<f64>,
}

fn name(o: &Obj) -> Option<&[u8]> {
    if let Obj::Name(n) = o { Some(n) } else { None }
}

impl Document {
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Find every object in the file. Returns the trailer dictionaries, oldest first, and the
    /// object streams to unpack.
    fn scan(&mut self) -> (Vec<Dict>, Vec<(usize, u32)>) {
        let data = &self.data;
        let n = data.len();
        let mut objs: HashMap<u32, (Obj, usize)> = HashMap::new();
        let mut trailers: Vec<(usize, Dict)> = Vec::new();
        let mut objstms = Vec::new();
        // Where each stream's data lies, in order, so its bytes aren't read as a trailer.
        let mut streams: Vec<(usize, usize)> = Vec::new();
        let mut i = 0;
        while let Some(k) = find(data, b"obj", i) {
            if self.room == 0 {
                break;
            }
            i = k + 3;
            if k + 3 < n && !is_ws(data[k + 3]) && !is_delim(data[k + 3]) {
                continue;
            }
            let Some((num, generation)) = header_before(data, k) else { continue };
            let mut lx = Lexer::at(data, k + 3, self.room);
            let parsed = lx.obj(true);
            // Each object counts as one thing, as well as what it holds.
            self.room = lx.room.saturating_sub(1);
            let Some(obj) = parsed else { continue };
            i = lx.pos;
            let obj = match obj {
                Obj::Dict(d) => {
                    let mut peek = Lexer::at(data, lx.pos, 0);
                    if let Some(Tok::Word(b"stream")) = peek.tok() {
                        let mut p = peek.pos;
                        if data.get(p) == Some(&b'\r') {
                            p += 1;
                        }
                        if data.get(p) == Some(&b'\n') {
                            p += 1;
                        }
                        let declared = match d.get(b"Length") {
                            Some(Obj::Num(l)) if *l >= 0.0 => Some(*l as usize),
                            _ => None,
                        };
                        let end = declared.filter(|&l| p.checked_add(l).is_some_and(|e| e <= n && ends_stream(data, e))).map(|l| p + l).unwrap_or_else(|| {
                            let mut e = find(data, b"endstream", p).unwrap_or(n);
                            if e > p && data[e - 1] == b'\n' {
                                e -= 1;
                            }
                            if e > p && data[e - 1] == b'\r' {
                                e -= 1;
                            }
                            e
                        });
                        i = end;
                        streams.push((p, end));
                        let kind = d.get(b"Type").and_then(name);
                        if kind == Some(&b"ObjStm"[..]) {
                            objstms.push((k, num));
                        } else if kind == Some(&b"XRef"[..]) {
                            trailers.push((k, d.clone()));
                        }
                        Obj::Stream(Box::new(Stream { dict: d, start: p, end, id: (num, generation) }))
                    } else {
                        Obj::Dict(d)
                    }
                }
                o => o,
            };
            objs.insert(num, (obj, k));
        }
        let mut i = 0;
        let mut next_stream = 0;
        while let Some(k) = find(data, b"trailer", i) {
            i = k + 7;
            while streams.get(next_stream).is_some_and(|&(_, end)| end <= k) {
                next_stream += 1;
            }
            if let Some(&(start, end)) = streams.get(next_stream) {
                if start <= k {
                    i = end;
                    continue;
                }
            }
            // A trailer is small and matters, so even once the objects have used up their
            // room, each gets a little of its own; only the latest few thousand are kept.
            let room = self.room.max(64);
            let mut lx = Lexer::at(data, k + 7, room);
            let parsed = lx.obj(true);
            self.room = self.room.saturating_sub(room - lx.room);
            if let Some(Obj::Dict(d)) = parsed {
                trailers.push((k, d));
                if trailers.len() > 4000 {
                    trailers.drain(..2000);
                }
            }
            i = i.max(lx.pos);
        }
        trailers.sort_by_key(|(k, _)| *k);
        self.objs = objs;
        (trailers.into_iter().map(|(_, d)| d).collect(), objstms)
    }

    fn security(&self, trailer: &Dict) -> Result<Crypt, String> {
        let enc = self.dict_of(trailer.get(b"Encrypt")).ok_or("This PDF's encryption is damaged.")?;
        if self.name_of(enc.get(b"Filter")).is_some_and(|f| f != b"Standard") {
            return Err("This PDF uses an encryption View can't read.".into());
        }
        let num = |k: &[u8]| self.num_of(enc.get(k));
        fn bytes<'a>(doc: &'a Document, o: Option<&'a Obj>) -> &'a [u8] {
            match doc.res(o) {
                Some(Obj::Str(s)) => s,
                _ => &[],
            }
        }
        let str_of = |k: &[u8]| bytes(self, enc.get(k));
        let id0 = match self.arr_of(trailer.get(b"ID")).and_then(|a| a.first()).and_then(|o| self.res(Some(o))) {
            Some(Obj::Str(s)) => s.as_slice(),
            _ => &[],
        };
        let filter = self.name_of(enc.get(b"StmF")).unwrap_or(b"Identity");
        let cfm = if filter == b"Identity" {
            Some(&b"None"[..])
        } else {
            self.dict_of(enc.get(b"CF")).and_then(|cf| self.dict_of(cf.get(filter))).and_then(|f| self.name_of(f.get(b"CFM")))
        };
        let v = num(b"V").unwrap_or(0.0) as u32;
        Crypt::new(&Params {
            v,
            r: num(b"R").unwrap_or(2.0) as u32,
            length_bits: num(b"Length").unwrap_or(if v >= 4 { 128.0 } else { 40.0 }) as u32,
            o: str_of(b"O"),
            u: str_of(b"U"),
            ue: str_of(b"UE"),
            p: num(b"P").unwrap_or(0.0) as i64 as i32,
            id0,
            encrypt_metadata: !matches!(self.res(enc.get(b"EncryptMetadata")), Some(Obj::Bool(false))),
            cfm: if v >= 4 { cfm } else { None },
        })
    }

    /// Unpack the objects kept inside object streams. A stream's objects lose to ones defined
    /// later in the file, which came from a later edit.
    fn expand(&mut self, mut objstms: Vec<(usize, u32)>) {
        objstms.sort();
        for (pos, num) in objstms {
            let Some((Obj::Stream(st), _)) = self.objs.get(&num) else { continue };
            // Once the objects have filled their room, the rest aren't even decoded.
            if self.room == 0 {
                break;
            }
            let Some(bytes) = self.stream_bytes(st) else { continue };
            let count = self.num_of(st.dict.get(b"N")).unwrap_or(0.0).clamp(0.0, 1e6) as usize;
            let first = self.num_of(st.dict.get(b"First")).unwrap_or(0.0).max(0.0) as usize;
            let mut head = Lexer::new(&bytes[..first.min(bytes.len())]);
            let mut found = Vec::new();
            // Each object is read from where it starts; listing one place many times over
            // doesn't get it read many times.
            let (mut read, mut work) = (HashSet::new(), 0usize);
            for _ in 0..count {
                let (Some(Tok::Obj(Obj::Num(n))), Some(Tok::Obj(Obj::Num(off)))) = (head.tok(), head.tok()) else { break };
                let at = first.saturating_add(off.max(0.0) as usize);
                if self.room == 0 {
                    break;
                }
                if !read.insert(at) {
                    continue;
                }
                let mut lx = Lexer::at(&bytes, at, self.room);
                let parsed = lx.obj(true);
                self.room = lx.room.saturating_sub(1);
                if let Some(o) = parsed {
                    found.push((n as u32, o));
                }
                // Objects that overlap could each run to the end: read the stream a few
                // times over at most.
                work += lx.pos.saturating_sub(at);
                if work > 4 * bytes.len() + (1 << 20) {
                    break;
                }
            }
            for (n, o) in found {
                if self.objs.get(&n).is_none_or(|(_, p)| *p < pos) {
                    self.objs.insert(n, (o, pos));
                }
            }
        }
    }

    fn walk(&self, node: &Obj, inherit: &Inherit, out: &mut Vec<Page>, seen: &mut HashSet<u32>, depth: usize) {
        if depth > 64 || out.len() >= 100_000 {
            return;
        }
        if let Obj::Ref(n) = node {
            if !seen.insert(*n) {
                return;
            }
        }
        let Some(d) = self.dict_of(Some(node)) else { return };
        let mut inh = inherit.clone();
        if let Some(r) = d.get(b"Resources") {
            inh.resources = Some(Rc::new(r.clone()));
        }
        if let Some(b) = self.rect_of(d.get(b"MediaBox")) {
            inh.media = Some(b);
        }
        if let Some(b) = self.rect_of(d.get(b"CropBox")) {
            inh.crop = Some(b);
        }
        if let Some(r) = self.num_of(d.get(b"Rotate")) {
            inh.rotate = Some(r);
        }
        if let Some(kids) = self.arr_of(d.get(b"Kids")) {
            for k in kids {
                self.walk(k, &inh, out, seen, depth + 1);
            }
            return;
        }
        let media = inh.media.unwrap_or([0.0, 0.0, 612.0, 792.0]);
        let crop = inh
            .crop
            .map(|c| [c[0].max(media[0]), c[1].max(media[1]), c[2].min(media[2]), c[3].min(media[3])])
            .filter(|c| c[2] - c[0] > 1.0 && c[3] - c[1] > 1.0)
            .unwrap_or(media);
        let rotate = ((inh.rotate.unwrap_or(0.0) / 90.0).round() as i64).rem_euclid(4) as u32 * 90;
        out.push(Page { contents: d.get(b"Contents").cloned().unwrap_or(Obj::Null), resources: inh.resources.unwrap_or_else(|| Rc::new(Obj::Null)), crop, rotate });
    }

    // ---- Looking things up ----

    fn get<'a>(&'a self, o: &'a Obj) -> &'a Obj {
        let mut o = o;
        for _ in 0..16 {
            match o {
                Obj::Ref(n) => o = self.objs.get(n).map_or(&NULL, |(x, _)| x),
                _ => return o,
            }
        }
        &NULL
    }

    fn res<'a>(&'a self, o: Option<&'a Obj>) -> Option<&'a Obj> {
        o.map(|o| self.get(o)).filter(|o| !matches!(o, Obj::Null))
    }

    fn dict_of<'a>(&'a self, o: Option<&'a Obj>) -> Option<&'a Dict> {
        match self.res(o)? {
            Obj::Dict(d) => Some(d),
            Obj::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }

    fn arr_of<'a>(&'a self, o: Option<&'a Obj>) -> Option<&'a [Obj]> {
        if let Obj::Array(a) = self.res(o)? { Some(a) } else { None }
    }

    fn num_of(&self, o: Option<&Obj>) -> Option<f64> {
        if let Obj::Num(n) = self.res(o)? { Some(*n) } else { None }
    }

    fn name_of<'a>(&'a self, o: Option<&'a Obj>) -> Option<&'a [u8]> {
        name(self.res(o)?)
    }

    fn rect_of(&self, o: Option<&Obj>) -> Option<[f64; 4]> {
        let a = self.arr_of(o)?;
        if a.len() < 4 {
            return None;
        }
        let v: Vec<f64> = a[..4].iter().map(|x| self.num_of(Some(x)).unwrap_or(0.0)).collect();
        Some([v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
    }

    fn matrix_of(&self, o: Option<&Obj>) -> Option<M> {
        let a = self.arr_of(o)?;
        if a.len() < 6 {
            return None;
        }
        let v: Vec<f64> = a[..6].iter().map(|x| self.num_of(Some(x)).unwrap_or(0.0)).collect();
        Some([v[0], v[1], v[2], v[3], v[4], v[5]])
    }

    /// A stream's bytes, decrypted and decoded. `None` for the image formats, which are no
    /// use here.
    fn stream_bytes(&self, st: &Stream) -> Option<Vec<u8>> {
        self.decode(st).0
    }

    /// A stream's bytes, and what getting them cost: every byte each step read and wrote,
    /// whether or not it worked, so that one that decodes to nothing is still charged for it.
    fn decode(&self, st: &Stream) -> (Option<Vec<u8>>, usize) {
        let Some(raw) = self.data.get(st.start..st.end) else { return (None, 0) };
        let mut cost = raw.len();
        let xref = st.dict.get(b"Type").and_then(name) == Some(&b"XRef"[..]);
        let filters: Vec<&[u8]> = match self.res(st.dict.get(b"Filter")) {
            Some(Obj::Name(n)) => vec![n],
            Some(Obj::Array(a)) => a.iter().filter_map(|f| self.name_of(Some(f))).collect(),
            _ => Vec::new(),
        };
        let parms: Vec<Option<&Dict>> = match self.res(st.dict.get(b"DecodeParms").or(st.dict.get(b"DP"))) {
            Some(Obj::Array(a)) => a.iter().map(|p| self.dict_of(Some(p))).collect(),
            Some(Obj::Dict(d)) => vec![Some(d)],
            _ => Vec::new(),
        };
        // Image formats, and ones View doesn't know: not worth decoding the steps before them.
        let known = |f: &[u8]| matches!(f, b"FlateDecode" | b"Fl" | b"LZWDecode" | b"LZW" | b"ASCIIHexDecode" | b"AHx" | b"ASCII85Decode" | b"A85" | b"RunLengthDecode" | b"RL" | b"Crypt");
        // Real files use one to three steps.
        if filters.len() > 8 || !filters.iter().all(|f| known(f)) {
            return (None, cost);
        }
        // A stream can name its own crypt filter, first in its list. "Identity", which is also
        // what one with no name means, leaves it as it is.
        let identity = filters.first() == Some(&&b"Crypt"[..])
            && parms.first().copied().flatten().and_then(|p| self.name_of(p.get(b"Name"))).is_none_or(|n| n == b"Identity");
        let mut data = match &self.crypt {
            Some(c) if !xref && !identity => c.decrypt(st.id.0, st.id.1, raw),
            _ => raw.to_vec(),
        };
        for (i, f) in filters.iter().enumerate() {
            let p = parms.get(i).copied().flatten();
            data = match *f {
                b"FlateDecode" | b"Fl" => match inflate(&data) {
                    Some(d) => self.predict(d, p),
                    None => return (None, cost),
                },
                b"LZWDecode" | b"LZW" => {
                    let early = p.and_then(|p| self.num_of(p.get(b"EarlyChange"))).unwrap_or(1.0) != 0.0;
                    self.predict(lzw(&data, early), p)
                }
                b"ASCIIHexDecode" | b"AHx" => Lexer::new(&data).hex(),
                b"ASCII85Decode" | b"A85" => ascii85(&data),
                b"RunLengthDecode" | b"RL" => run_length(&data),
                _ => data,
            };
            cost = cost.saturating_add(data.len());
            // A step can give back as much as it was given, so a long chain of them is stopped
            // by how much work it has been, as well as by size.
            if data.len() > MAX_STREAM || cost > 4 * MAX_STREAM {
                return (None, cost);
            }
        }
        (Some(data), cost)
    }

    /// Undo a PNG predictor, which object and cross-reference streams use.
    fn predict(&self, data: Vec<u8>, p: Option<&Dict>) -> Vec<u8> {
        let Some(p) = p else { return data };
        let get = |k: &[u8], d: f64| self.num_of(p.get(k)).unwrap_or(d).max(1.0) as usize;
        if get(b"Predictor", 1.0) < 10 {
            return data;
        }
        // Rows can't be longer than the data, whatever the file says.
        let (colors, bpc, cols) = (get(b"Colors", 1.0).min(32), get(b"BitsPerComponent", 8.0).min(16), get(b"Columns", 1.0));
        let bpp = (colors * bpc).div_ceil(8);
        let row = cols.saturating_mul(colors * bpc).div_ceil(8).min(data.len().max(1));
        let mut out = Vec::with_capacity(data.len());
        let mut prev = vec![0u8; row];
        for chunk in data.chunks(row + 1) {
            if chunk.len() < 2 {
                break;
            }
            let (kind, src) = (chunk[0], &chunk[1..]);
            let mut cur = vec![0u8; row];
            for i in 0..src.len() {
                let a = if i >= bpp { cur[i - bpp] as i32 } else { 0 };
                let b = prev[i] as i32;
                let c = if i >= bpp { prev[i - bpp] as i32 } else { 0 };
                let pred = match kind {
                    1 => a,
                    2 => b,
                    3 => (a + b) / 2,
                    4 => {
                        let pp = a + b - c;
                        let (pa, pb, pc) = ((pp - a).abs(), (pp - b).abs(), (pp - c).abs());
                        if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
                    }
                    _ => 0,
                };
                cur[i] = src[i].wrapping_add(pred as u8);
            }
            out.extend_from_slice(&cur[..src.len()]);
            prev = cur;
        }
        out
    }

    // ---- Pages ----

    /// The characters on page `index`, in the order they're drawn.
    pub fn page(&self, index: usize) -> Option<PageText> {
        let page = self.pages.get(index)?;
        let mut content = Vec::new();
        match self.get(&page.contents) {
            Obj::Stream(s) => content = self.stream_bytes(s).unwrap_or_default(),
            Obj::Array(a) => {
                // A part can be listed more than once (each time it draws where the text has got
                // to), but it's only decoded once.
                let mut decoded: HashMap<(u32, u16), Vec<u8>> = HashMap::new();
                // And decoding all the parts, even ones that come to nothing, has its limit.
                let mut work = 0usize;
                for part in a {
                    if let Obj::Stream(s) = self.get(part) {
                        if work > 4 * MAX_STREAM {
                            break;
                        }
                        let bytes = decoded.entry(s.id).or_insert_with(|| {
                            let (d, cost) = self.decode(s);
                            work += cost;
                            d.unwrap_or_default()
                        });
                        if content.len() + bytes.len() > MAX_STREAM {
                            break;
                        }
                        content.extend_from_slice(bytes);
                        content.push(b'\n');
                    }
                }
            }
            _ => {}
        }
        let mut run = Runner { doc: self, out: Vec::new(), ops: 0, forms: Vec::new(), bytes: 0 };
        let resources = self.dict_of(Some(&*page.resources));
        run.content(&content, resources, &mut GState::default(), 0);
        Some(arrange(run.out, page))
    }

    fn font(&self, resources: Option<&Dict>, key: &[u8]) -> Option<Rc<Font>> {
        let fonts = self.dict_of(resources?.get(b"Font"))?;
        let entry = fonts.get(key)?;
        let d = self.dict_of(Some(entry))?;
        // Each font is read once: by its object number, or for one written out where it's
        // used, by where its dictionary is (the document doesn't change once it's read).
        let id = match entry {
            Obj::Ref(id) => *id as u64,
            _ => d as *const Dict as u64 | 1 << 63,
        };
        if let Some(f) = self.fonts.borrow().get(&id) {
            return Some(f.clone());
        }
        let f = Rc::new(self.load_font(d));
        self.fonts.borrow_mut().insert(id, f.clone());
        Some(f)
    }

    /// A character map, read once however many fonts use it. Each map's codes count against
    /// what the whole document may map, and its bytes against what it may decode for maps.
    fn cmap(&self, s: &Stream) -> Option<Rc<CMap>> {
        if let Some(m) = self.cmaps.borrow().get(&s.id) {
            return m.clone();
        }
        // Whatever comes of it, a map is only tried once, and what decoding it cost counts.
        let left = self.map_bytes.get();
        let raw = s.end.saturating_sub(s.start);
        let (b, decoding) = if left < raw { (None, raw) } else { self.decode(s) };
        let cost = decoding.max(b.as_ref().map_or(0, Vec::len));
        self.map_bytes.set(left.saturating_sub(cost));
        let m = b.filter(|_| cost <= left).map(|b| {
            let mut room = self.map_room.get();
            let m = Rc::new(parse_cmap(&b, &mut room));
            self.map_room.set(room);
            m
        });
        self.cmaps.borrow_mut().insert(s.id, m.clone());
        m
    }

    /// A composite font's widths, by CID, from its W array.
    fn cid_widths_of(&self, cd: &Dict) -> HashMap<u32, f64> {
        let mut widths = HashMap::new();
        let Some(w) = self.arr_of(cd.get(b"W")) else { return widths };
        // Counts every width set, not just different ones, since ranges can overlap.
        let mut budget = self.map_room.get().min(400_000);
        let start = budget;
        let mut i = 0;
        while i + 1 < w.len() && budget > 0 {
            let c = self.num_of(Some(&w[i])).unwrap_or(0.0).max(0.0) as u32;
            match self.res(Some(&w[i + 1])) {
                Some(Obj::Array(ws)) => {
                    for (k, x) in ws.iter().enumerate().take(budget) {
                        widths.insert(c.saturating_add(k as u32), self.num_of(Some(x)).unwrap_or(0.0) / 1000.0);
                    }
                    budget = budget.saturating_sub(ws.len().max(1));
                    i += 2;
                }
                Some(Obj::Num(last)) => {
                    let width = w.get(i + 2).and_then(|x| self.num_of(Some(x))).unwrap_or(0.0) / 1000.0;
                    let last = (last.max(0.0) as u32).min(c.saturating_add(65535)).min(c.saturating_add(budget as u32 - 1));
                    for cid in c..=last {
                        widths.insert(cid, width);
                    }
                    budget = budget.saturating_sub((last.saturating_sub(c) as usize).max(1) + 1);
                    i += 3;
                }
                _ => break,
            }
        }
        self.map_room.set(self.map_room.get() - (start - budget));
        widths
    }

    fn load_font(&self, d: &Dict) -> Font {
        let sub = self.name_of(d.get(b"Subtype")).unwrap_or(b"");
        let mut f = Font { ascent: 0.9, descent: -0.22, default_width: 0.0, ..Font::default() };
        if let Some(Obj::Stream(s)) = self.res(d.get(b"ToUnicode")) {
            f.tounicode = self.cmap(s);
        }
        if sub == b"Type0" {
            f.composite = true;
            match self.res(d.get(b"Encoding")) {
                Some(Obj::Name(n)) => f.ucs2 = find(n, b"UCS2", 0).is_some() || find(n, b"UTF16", 0).is_some(),
                Some(Obj::Stream(s)) => {
                    if let Some(cm) = self.cmap(s) {
                        f.codespace = cm.codespace.iter().take(64).copied().collect();
                        f.encoding = Some(cm);
                    }
                }
                _ => {}
            }
            if f.codespace.is_empty() {
                f.codespace.push((2, 0, 0xFFFF));
            }
            f.codespace.sort_by_key(|c| c.0);
            f.default_width = 1.0;
            let entry = self.arr_of(d.get(b"DescendantFonts")).and_then(|a| a.first());
            if let Some(cd) = entry.and_then(|x| self.dict_of(Some(x))) {
                if let Some(dw) = self.num_of(cd.get(b"DW")) {
                    f.default_width = dw / 1000.0;
                }
                // Fonts can share their descendant, and so its widths: read them once.
                let id = match entry {
                    Some(Obj::Ref(id)) => *id as u64,
                    _ => cd as *const Dict as u64 | 1 << 63,
                };
                let cached = self.cid_widths.borrow().get(&id).cloned();
                f.widths = cached.unwrap_or_else(|| {
                    let w = Rc::new(self.cid_widths_of(cd));
                    self.cid_widths.borrow_mut().insert(id, w.clone());
                    w
                });
                self.metrics(&mut f, self.dict_of(cd.get(b"FontDescriptor")));
            }
            return f;
        }
        // A simple font: one byte a character.
        let scale = if sub == b"Type3" { self.matrix_of(d.get(b"FontMatrix")).map_or(0.001, |m| m[0].abs()) } else { 0.001 };
        let first = self.num_of(d.get(b"FirstChar")).unwrap_or(0.0).max(0.0) as u32;
        if let Some(ws) = self.arr_of(d.get(b"Widths")) {
            f.widths = Rc::new(ws.iter().enumerate().take(256).map(|(k, w)| (first.saturating_add(k as u32), self.num_of(Some(w)).unwrap_or(0.0) * scale)).collect());
        }
        let desc = self.dict_of(d.get(b"FontDescriptor"));
        f.default_width = desc.and_then(|x| self.num_of(x.get(b"MissingWidth"))).unwrap_or(0.0) * scale;
        let base = self.name_of(d.get(b"BaseFont")).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
        let base = base.split_once('+').filter(|(p, _)| p.len() == 6).map_or(base.as_str(), |(_, b)| b).to_string();
        if f.widths.is_empty() {
            f.std = std_widths(&base);
            if f.std.is_none() {
                f.default_width = 0.5;
            }
        }
        if sub != b"Type3" {
            self.metrics(&mut f, desc);
        }
        let symbolic = desc.and_then(|x| self.num_of(x.get(b"Flags"))).is_some_and(|fl| (fl as u32) & 4 != 0);
        let (base_enc, diffs) = match self.res(d.get(b"Encoding")) {
            Some(Obj::Name(n)) => (Some(n.as_slice()), None),
            Some(Obj::Dict(e)) => (self.name_of(e.get(b"BaseEncoding")), self.arr_of(e.get(b"Differences"))),
            _ => (None, None),
        };
        let table: fn(u8) -> Option<char> = match base_enc {
            Some(b"WinAnsiEncoding") => winansi,
            Some(b"MacRomanEncoding") => macroman,
            Some(b"StandardEncoding") => standard,
            _ if sub == b"TrueType" || symbolic => winansi,
            _ => standard,
        };
        let special = base.starts_with("Symbol") || base.starts_with("ZapfDingbats");
        f.table = (0..=255u8).map(|c| if special { None } else { table(c).map(String::from) }).collect();
        if let Some(diffs) = diffs {
            let mut code = 0usize;
            for x in diffs {
                match self.res(Some(x)) {
                    Some(Obj::Num(n)) => code = n.max(0.0) as usize,
                    Some(Obj::Name(nm)) => {
                        if code < 256 {
                            f.table[code] = glyph_name(nm);
                        }
                        code += 1;
                    }
                    _ => {}
                }
            }
        }
        f
    }

    fn metrics(&self, f: &mut Font, desc: Option<&Dict>) {
        let Some(desc) = desc else { return };
        if let Some(a) = self.num_of(desc.get(b"Ascent")).map(|a| a / 1000.0).filter(|a| (0.5..=1.3).contains(a)) {
            f.ascent = a;
        }
        if let Some(d) = self.num_of(desc.get(b"Descent")).map(|d| d / 1000.0).filter(|d| (-0.5..=-0.05).contains(d)) {
            f.descent = d;
        }
    }
}

/// Where "N G obj" starts, for the "obj" at `k`.
fn header_before(data: &[u8], k: usize) -> Option<(u32, u16)> {
    let mut j = k;
    let spaces = |j: &mut usize| {
        let start = *j;
        while *j > 0 && is_ws(data[*j - 1]) {
            *j -= 1;
        }
        *j < start
    };
    let digits = |j: &mut usize| -> Option<u64> {
        let end = *j;
        while *j > 0 && data[*j - 1].is_ascii_digit() && end - *j < 10 {
            *j -= 1;
        }
        if *j == end { None } else { std::str::from_utf8(&data[*j..end]).ok()?.parse().ok() }
    };
    if !spaces(&mut j) {
        return None;
    }
    let generation = digits(&mut j)?;
    if !spaces(&mut j) {
        return None;
    }
    let num = digits(&mut j)?;
    if j > 0 && !is_ws(data[j - 1]) && !is_delim(data[j - 1]) {
        return None;
    }
    Some((u32::try_from(num).ok()?, generation.min(65535) as u16))
}

fn ends_stream(data: &[u8], mut at: usize) -> bool {
    while at < data.len() && is_ws(data[at]) {
        at += 1;
    }
    data[at.min(data.len())..].starts_with(b"endstream")
}

// ---- Filters ----

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    use miniz_oxide::inflate::{decompress_to_vec_with_limit, decompress_to_vec_zlib_with_limit};
    match decompress_to_vec_zlib_with_limit(data, MAX_STREAM) {
        Ok(v) => Some(v),
        // Cut short, or a bad checksum: keep what came out.
        Err(e) if !e.output.is_empty() => Some(e.output),
        Err(_) => match decompress_to_vec_with_limit(data.get(2..)?, MAX_STREAM) {
            Ok(v) => Some(v),
            Err(e) if !e.output.is_empty() => Some(e.output),
            Err(_) => None,
        },
    }
}

/// An empty buffer for a decoder. Its size is a power of two at least as big as anything a
/// decoder writes at once, so as it doubles it lands on `MAX_STREAM` exactly, and the
/// decoders, which stop short of that, never make it double past it.
fn decoded() -> Vec<u8> {
    Vec::with_capacity(4096)
}

fn lzw(data: &[u8], early: bool) -> Vec<u8> {
    let mut out = decoded();
    let mut table: Vec<Vec<u8>> = (0..=255u8).map(|b| vec![b]).collect();
    table.push(Vec::new());
    table.push(Vec::new());
    let (mut bits, mut acc, mut width) = (0u32, 0u32, 9u32);
    let mut prev: Option<Vec<u8>> = None;
    for &b in data {
        acc = acc << 8 | b as u32;
        bits += 8;
        while bits >= width {
            let code = (acc >> (bits - width)) as usize & ((1 << width) - 1);
            bits -= width;
            match code {
                256 => {
                    table.truncate(258);
                    width = 9;
                    prev = None;
                    continue;
                }
                257 => return out,
                _ => {}
            }
            let entry = if code < table.len() {
                table[code].clone()
            } else if let Some(p) = &prev {
                let mut e = p.clone();
                e.push(p[0]);
                e
            } else {
                return out;
            };
            // Stopping short of the limit keeps the buffer from doubling past it.
            if out.len() + entry.len() > MAX_STREAM {
                return out;
            }
            out.extend_from_slice(&entry);
            if let Some(mut p) = prev.take() {
                if table.len() < 4096 {
                    p.push(entry[0]);
                    table.push(p);
                }
            }
            prev = Some(entry);
            let next = table.len() + early as usize;
            width = if next >= 2048 { 12 } else if next >= 1024 { 11 } else if next >= 512 { 10 } else { 9 };
        }
    }
    out
}

fn ascii85(data: &[u8]) -> Vec<u8> {
    let mut out = decoded();
    let mut group = Vec::with_capacity(5);
    for &c in data {
        match c {
            _ if out.len() + 4 > MAX_STREAM => break,
            b'~' => break,
            b'z' if group.is_empty() => out.extend_from_slice(&[0; 4]),
            b'!'..=b'u' => {
                group.push(c - b'!');
                if group.len() == 5 {
                    let v = group.iter().fold(0u64, |a, &d| a * 85 + d as u64) as u32;
                    out.extend_from_slice(&v.to_be_bytes());
                    group.clear();
                }
            }
            _ => {}
        }
    }
    if group.len() > 1 && out.len() + 4 <= MAX_STREAM {
        let n = group.len();
        group.resize(5, 84);
        let v = group.iter().fold(0u64, |a, &d| a * 85 + d as u64) as u32;
        out.extend_from_slice(&v.to_be_bytes()[..n - 1]);
    }
    out
}

fn run_length(data: &[u8]) -> Vec<u8> {
    let mut out = decoded();
    let mut i = 0;
    while i < data.len() && out.len() + 128 <= MAX_STREAM {
        let l = data[i] as usize;
        i += 1;
        match l {
            0..=127 => {
                let end = (i + l + 1).min(data.len());
                out.extend_from_slice(&data[i..end]);
                i = end;
            }
            128 => break,
            _ => {
                if let Some(&b) = data.get(i) {
                    out.extend(std::iter::repeat_n(b, 257 - l));
                }
                i += 1;
            }
        }
    }
    out
}

// ---- Fonts ----

#[derive(Default)]
struct Font {
    composite: bool,
    /// Code lengths in bytes and their ranges, shortest first (composite fonts).
    codespace: Vec<(usize, u32, u32)>,
    /// Its encoding, for code to CID when that isn't the identity (composite fonts).
    encoding: Option<Rc<CMap>>,
    /// Advance widths in text space units, by CID (composite) or code (simple). Shared by
    /// fonts that share their widths.
    widths: Rc<HashMap<u32, f64>>,
    default_width: f64,
    /// For the standard fonts, which needn't list their widths.
    std: Option<&'static [u16; 95]>,
    /// Its ToUnicode map, which wins over everything else, shared by the fonts that use it.
    tounicode: Option<Rc<CMap>>,
    /// Simple fonts: each code's text, from the encoding.
    table: Vec<Option<String>>,
    /// The code is the Unicode value (the predefined UCS-2 and UTF-16 encodings).
    ucs2: bool,
    ascent: f64,
    descent: f64,
}

impl Font {
    fn next_code(&self, s: &[u8], i: usize) -> (u32, usize) {
        if !self.composite {
            return (s[i] as u32, 1);
        }
        for len in 1..=4 {
            if i + len > s.len() {
                break;
            }
            let c = s[i..i + len].iter().fold(0u32, |a, &b| a << 8 | b as u32);
            if self.codespace.iter().any(|&(l, lo, hi)| l == len && (lo..=hi).contains(&c)) {
                return (c, len);
            }
        }
        let len = 2.min(s.len() - i);
        (s[i..i + len].iter().fold(0u32, |a, &b| a << 8 | b as u32), len)
    }

    fn text(&self, code: u32) -> Option<Cow<'_, str>> {
        if let Some(t) = self.tounicode.as_ref().and_then(|m| m.bf.get(&code)) {
            return Some(Cow::Borrowed(t));
        }
        if self.composite {
            return if self.ucs2 { char::from_u32(code).map(|c| Cow::Owned(c.to_string())) } else { None };
        }
        self.table.get(code as usize)?.as_deref().map(Cow::Borrowed)
    }

    fn width(&self, code: u32) -> f64 {
        if self.composite {
            let cid = self.encoding.as_ref().and_then(|m| m.cid.get(&code)).copied().unwrap_or(code);
            return self.widths.get(&cid).copied().unwrap_or(self.default_width);
        }
        if let Some(w) = self.widths.get(&code) {
            return *w;
        }
        if let Some(std) = self.std {
            let c = self.text(code).and_then(|t| t.chars().next()).unwrap_or(' ') as u32;
            return if (32..=126).contains(&c) { std[(c - 32) as usize] as f64 / 1000.0 } else { std[('n' as u32 - 32) as usize] as f64 / 1000.0 };
        }
        self.default_width
    }
}

#[derive(Default)]
struct CMap {
    codespace: Vec<(usize, u32, u32)>,
    bf: HashMap<u32, String>,
    cid: HashMap<u32, u32>,
}

/// Read a character map. `budget` is how many codes it may map, shared with the rest of the
/// document and counted down as it goes.
fn parse_cmap(data: &[u8], budget: &mut usize) -> CMap {
    let mut cm = CMap::default();
    let mut lx = Lexer::new(data);
    let mut ops: Vec<Obj> = Vec::new();
    let mut mode = 0;
    let code = |o: &Obj| match o {
        Obj::Str(s) if !s.is_empty() && s.len() <= 4 => Some((s.len(), s.iter().fold(0u32, |a, &b| a << 8 | b as u32))),
        _ => None,
    };
    while let Some(t) = lx.tok() {
        match t {
            Tok::Word(w) => {
                if w.starts_with(b"end") && mode != 0 {
                    match mode {
                        1 => {
                            for p in ops.chunks(2) {
                                if let (Some((len, lo)), Some((_, hi))) = (p.first().and_then(code), p.get(1).and_then(code)) {
                                    cm.codespace.push((len, lo, hi));
                                }
                            }
                        }
                        2 => {
                            for p in ops.chunks(2) {
                                if let (Some((_, c)), Some(dst)) = (p.first().and_then(code), p.get(1)) {
                                    let t = match dst {
                                        Obj::Str(s) if s.len() <= MAX_MAPPING => Some(utf16be(s)),
                                        Obj::Name(n) => glyph_name(n),
                                        _ => None,
                                    };
                                    if let Some(t) = t.filter(|_| *budget > 0) {
                                        *budget -= 1;
                                        cm.bf.insert(c, t);
                                    }
                                }
                            }
                        }
                        3 => {
                            for p in ops.chunks(3) {
                                let (Some((_, lo)), Some((_, hi)), Some(dst)) = (p.first().and_then(code), p.get(1).and_then(code), p.get(2)) else { continue };
                                if hi < lo || hi - lo > 65535 || *budget <= (hi - lo) as usize {
                                    continue;
                                }
                                *budget -= (hi - lo) as usize + 1;
                                match dst {
                                    Obj::Str(s) if s.len() <= MAX_MAPPING => {
                                        let units: Vec<u16> = s.chunks(2).map(|u| if u.len() == 2 { u16::from_be_bytes([u[0], u[1]]) } else { u[0] as u16 }).collect();
                                        for (k, c) in (lo..=hi).enumerate() {
                                            let mut u = units.clone();
                                            if let Some(last) = u.last_mut() {
                                                *last = last.wrapping_add(k as u16);
                                            }
                                            cm.bf.insert(c, String::from_utf16_lossy(&u));
                                        }
                                    }
                                    Obj::Array(a) => {
                                        for (c, d) in (lo..=hi).zip(a) {
                                            match d {
                                                Obj::Str(s) if s.len() <= MAX_MAPPING => {
                                                    cm.bf.insert(c, utf16be(s));
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        4 => {
                            for p in ops.chunks(2) {
                                if let (Some((_, c)), Some(Obj::Num(cid)), true) = (p.first().and_then(code), p.get(1), *budget > 0) {
                                    *budget -= 1;
                                    cm.cid.insert(c, *cid as u32);
                                }
                            }
                        }
                        5 => {
                            for p in ops.chunks(3) {
                                let (Some((_, lo)), Some((_, hi)), Some(Obj::Num(start))) = (p.first().and_then(code), p.get(1).and_then(code), p.get(2)) else { continue };
                                if hi < lo || hi - lo > 65535 || *budget <= (hi - lo) as usize {
                                    continue;
                                }
                                *budget -= (hi - lo) as usize + 1;
                                for (k, c) in (lo..=hi).enumerate() {
                                    cm.cid.insert(c, *start as u32 + k as u32);
                                }
                            }
                        }
                        _ => {}
                    }
                    mode = 0;
                } else {
                    match w {
                        b"begincodespacerange" => mode = 1,
                        b"beginbfchar" => mode = 2,
                        b"beginbfrange" => mode = 3,
                        b"begincidchar" => mode = 4,
                        b"begincidrange" => mode = 5,
                        _ => {}
                    }
                }
                ops.clear();
                lx.room = MAX_ITEMS_AT_ONCE;
            }
            t => {
                // A block may list 100 entries; far more than that is someone up to no good.
                if mode != 0 && ops.len() < 300_000 {
                    if let Some(o) = lx.finish(t, false, 0) {
                        ops.push(o);
                    }
                }
            }
        }
    }
    cm
}

fn utf16be(s: &[u8]) -> String {
    if s.len() % 2 == 1 {
        return s.iter().map(|&b| b as char).collect();
    }
    let units: Vec<u16> = s.chunks(2).map(|u| u16::from_be_bytes([u[0], u[1]])).collect();
    String::from_utf16_lossy(&units)
}

// ---- Running a page ----

type M = [f64; 6];
const ID: M = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `a` then `b`.
fn mul(a: M, b: M) -> M {
    [a[0] * b[0] + a[1] * b[2], a[0] * b[1] + a[1] * b[3], a[2] * b[0] + a[3] * b[2], a[2] * b[1] + a[3] * b[3], a[4] * b[0] + a[5] * b[2] + b[4], a[4] * b[1] + a[5] * b[3] + b[5]]
}

/// The matrix that undoes `m`, unless `m` flattens everything onto a line.
fn invert(m: &M) -> Option<M> {
    let det = m[0] * m[3] - m[1] * m[2];
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let (a, b, c, d) = (m[3] / det, -m[1] / det, -m[2] / det, m[0] / det);
    Some([a, b, c, d, -(m[4] * a + m[5] * c), -(m[4] * b + m[5] * d)])
}

fn apply(m: &M, x: f64, y: f64) -> (f64, f64) {
    (x * m[0] + y * m[2] + m[4], x * m[1] + y * m[3] + m[5])
}

#[derive(Clone)]
struct GState {
    ctm: M,
    font: Option<Rc<Font>>,
    size: f64,
    tc: f64,
    tw: f64,
    th: f64,
    tl: f64,
    rise: f64,
    /// What forms let show: each box, in its form's own space, and the way there from the page.
    clips: Vec<(M, [f64; 4])>,
}

impl Default for GState {
    fn default() -> Self {
        GState { ctm: ID, font: None, size: 0.0, tc: 0.0, tw: 0.0, th: 1.0, tl: 0.0, rise: 0.0, clips: Vec::new() }
    }
}

/// A character placed on the page, in the page's own units: its text, and the box it fills.
struct Raw {
    text: String,
    rect: [f64; 4],
    /// Two points along its baseline, for the way the text runs.
    base: [f64; 4],
}

struct Runner<'d> {
    doc: &'d Document,
    out: Vec<Raw>,
    ops: usize,
    /// The forms being drawn, innermost last, so one that draws itself stops.
    forms: Vec<u32>,
    /// How much text the page has so far.
    bytes: usize,
}

impl<'d> Runner<'d> {
    fn content(&mut self, data: &[u8], res: Option<&'d Dict>, gs: &mut GState, depth: usize) {
        let mut lx = Lexer::new(data);
        let mut stack: Vec<Obj> = Vec::new();
        let mut saved: Vec<GState> = Vec::new();
        let (mut tm, mut tlm) = (ID, ID);
        let nums = |stack: &[Obj], n: usize| -> Option<Vec<f64>> {
            if stack.len() < n {
                return None;
            }
            stack[stack.len() - n..].iter().map(|o| if let Obj::Num(v) = o { Some(*v) } else { None }).collect()
        };
        while let Some(t) = lx.tok() {
            let op = match t {
                Tok::Word(w) => w,
                t => {
                    if let Some(o) = lx.finish(t, false, 0) {
                        if stack.len() < 64 {
                            stack.push(o);
                        }
                    }
                    continue;
                }
            };
            self.ops += 1;
            if self.ops > MAX_OPS || self.out.len() > MAX_GLYPHS {
                return;
            }
            match op {
                b"q" => {
                    if saved.len() < 64 {
                        saved.push(gs.clone());
                    }
                }
                b"Q" => {
                    if let Some(g) = saved.pop() {
                        *gs = g;
                    }
                }
                b"cm" => {
                    if let Some(m) = nums(&stack, 6) {
                        gs.ctm = mul([m[0], m[1], m[2], m[3], m[4], m[5]], gs.ctm);
                    }
                }
                b"BT" => {
                    tm = ID;
                    tlm = ID;
                }
                b"Tf" => {
                    if stack.len() >= 2 {
                        if let (Obj::Name(n), Obj::Num(size)) = (&stack[stack.len() - 2], &stack[stack.len() - 1]) {
                            gs.font = self.doc.font(res, n);
                            gs.size = *size;
                        }
                    }
                }
                b"Tc" | b"Tw" | b"Tz" | b"TL" | b"Ts" => {
                    if let Some(v) = nums(&stack, 1) {
                        let v = v[0];
                        match op {
                            b"Tc" => gs.tc = v,
                            b"Tw" => gs.tw = v,
                            b"Tz" => gs.th = v / 100.0,
                            b"TL" => gs.tl = v,
                            _ => gs.rise = v,
                        }
                    }
                }
                b"Td" | b"TD" => {
                    if let Some(v) = nums(&stack, 2) {
                        if op == b"TD" {
                            gs.tl = -v[1];
                        }
                        tlm = mul([1.0, 0.0, 0.0, 1.0, v[0], v[1]], tlm);
                        tm = tlm;
                    }
                }
                b"Tm" => {
                    if let Some(m) = nums(&stack, 6) {
                        tlm = [m[0], m[1], m[2], m[3], m[4], m[5]];
                        tm = tlm;
                    }
                }
                b"T*" | b"'" | b"\"" => {
                    if op == b"\"" {
                        if let Some(v) = (stack.len() >= 3).then(|| nums(&stack[..stack.len() - 1], 2)).flatten() {
                            gs.tw = v[0];
                            gs.tc = v[1];
                        }
                    }
                    tlm = mul([1.0, 0.0, 0.0, 1.0, 0.0, -gs.tl], tlm);
                    tm = tlm;
                    if op != b"T*" {
                        if let Some(Obj::Str(s)) = stack.last() {
                            self.show(s, gs, &mut tm);
                        }
                    }
                }
                b"Tj" => {
                    if let Some(Obj::Str(s)) = stack.last() {
                        self.show(s, gs, &mut tm);
                    }
                }
                b"TJ" => {
                    if let Some(Obj::Array(a)) = stack.last() {
                        for item in a {
                            match item {
                                Obj::Str(s) => self.show(s, gs, &mut tm),
                                Obj::Num(n) => tm = mul([1.0, 0.0, 0.0, 1.0, -n / 1000.0 * gs.size * gs.th, 0.0], tm),
                                _ => {}
                            }
                        }
                    }
                }
                b"Do" => {
                    // Let go of the operands first, so forms inside forms don't each hold on
                    // to theirs.
                    if let Some(Obj::Name(n)) = stack.pop() {
                        stack.clear();
                        lx.room = MAX_ITEMS_AT_ONCE;
                        self.form(&n, res, gs, depth);
                    }
                }
                b"ID" => {
                    // An inline image's data, which could hold anything: skip to its "EI".
                    let mut p = lx.pos + 1;
                    lx.pos = data.len();
                    while let Some(e) = find(data, b"EI", p) {
                        if e > 0 && is_ws(data[e - 1]) && data.get(e + 2).is_none_or(|&c| is_ws(c) || is_delim(c)) {
                            lx.pos = e + 2;
                            break;
                        }
                        p = e + 2;
                    }
                }
                _ => {}
            }
            stack.clear();
            lx.room = MAX_ITEMS_AT_ONCE;
        }
    }

    fn form(&mut self, key: &[u8], res: Option<&'d Dict>, gs: &GState, depth: usize) {
        let doc = self.doc;
        let Some(entry) = doc.dict_of(res.and_then(|r| r.get(b"XObject"))).and_then(|x| x.get(key)) else { return };
        let Obj::Ref(id) = entry else { return };
        if depth >= 12 || self.forms.contains(id) {
            return;
        }
        let Obj::Stream(st) = doc.get(entry) else { return };
        if doc.name_of(st.dict.get(b"Subtype")) != Some(&b"Form"[..]) {
            return;
        }
        if self.ops > MAX_OPS {
            return;
        }
        // Decoding counts, even when it comes to nothing, so a form drawn over and over
        // can't keep the reader busy.
        let (data, cost) = doc.decode(st);
        self.ops += cost / 64 + 1;
        let Some(data) = data else { return };
        let mut g = gs.clone();
        g.ctm = mul(doc.matrix_of(st.dict.get(b"Matrix")).unwrap_or(ID), gs.ctm);
        // Its box hides what it draws outside it (one with no area is taken to be a mistake).
        // The box is kept in the form's own space, with the way back there from the page, so
        // a turned or slanted form clips where it really is.
        if let Some(b) = doc.rect_of(st.dict.get(b"BBox")).filter(|b| b[2] > b[0] && b[3] > b[1]) {
            if let Some(back) = invert(&g.ctm) {
                if g.clips.len() < 16 {
                    g.clips.push((back, b));
                }
            }
        }
        let inner = doc.dict_of(st.dict.get(b"Resources")).or(res);
        self.forms.push(*id);
        self.content(&data, inner, &mut g, depth + 1);
        self.forms.pop();
    }

    fn show(&mut self, s: &[u8], gs: &GState, tm: &mut M) {
        let Some(font) = gs.font.clone() else { return };
        let fs = gs.size;
        let mut i = 0;
        while i < s.len() {
            let (code, len) = font.next_code(s, i);
            i += len;
            let w0 = font.width(code);
            // Once the page is full, characters are only moved past.
            let room = self.out.len() < MAX_GLYPHS && self.bytes < MAX_TEXT;
            let found = if room { font.text(code) } else { None };
            if let Some(t) = found {
                let text = clean(&t);
                if !text.is_empty() && self.out.len() < MAX_GLYPHS && self.bytes + text.len() <= MAX_TEXT {
                    let trm = mul([fs * gs.th, 0.0, 0.0, fs, 0.0, gs.rise], mul(*tm, gs.ctm));
                    let w = w0.max(0.0);
                    let pts = [apply(&trm, 0.0, font.descent), apply(&trm, w, font.descent), apply(&trm, 0.0, font.ascent), apply(&trm, w, font.ascent)];
                    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                    for (x, y) in pts {
                        x0 = x0.min(x);
                        y0 = y0.min(y);
                        x1 = x1.max(x);
                        y1 = y1.max(y);
                    }
                    let (o, e) = (apply(&trm, 0.0, 0.0), apply(&trm, 1.0, 0.0));
                    let all = [x0, y0, x1, y1, o.0, o.1, e.0, e.1];
                    let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
                    let inside = gs.clips.iter().all(|(back, c)| {
                        let (x, y) = apply(back, mx, my);
                        x >= c[0] && x <= c[2] && y >= c[1] && y <= c[3]
                    });
                    if inside && all.iter().all(|v| v.is_finite()) {
                        self.bytes += text.len();
                        self.out.push(Raw { text, rect: [x0, y0, x1, y1], base: [o.0, o.1, e.0, e.1] });
                    }
                }
            }
            let tx = (w0 * fs + gs.tc + if len == 1 && code == 32 { gs.tw } else { 0.0 }) * gs.th;
            *tm = mul([1.0, 0.0, 0.0, 1.0, tx, 0.0], *tm);
        }
    }
}

/// Text as it should be copied: ligatures spelled out, control characters dropped.
fn clean(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    for c in t.chars() {
        match c {
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
            '\u{A0}' | '\t' => out.push(' '),
            '\u{AD}' => out.push('-'),
            '\u{FFFD}' | '\u{FEFF}' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Put a page's characters where they're shown, drop doubles and what's off the page, and
/// work out the spaces and line breaks between them.
fn arrange(raw: Vec<Raw>, page: &Page) -> PageText {
    let [x0, y0, x1, y1] = page.crop;
    let (cw, ch) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
    let (w, h) = if page.rotate % 180 == 90 { (ch, cw) } else { (cw, ch) };
    let shown = |x: f64, y: f64| -> (f64, f64) {
        let (u, v) = ((x - x0) / cw, (y1 - y) / ch);
        let (u, v) = match page.rotate {
            90 => (1.0 - v, u),
            180 => (1.0 - u, 1.0 - v),
            270 => (v, 1.0 - u),
            _ => (u, v),
        };
        (u * w, v * h)
    };
    // The extent of a box measured along a direction.
    let span = |b: &[f64; 4], (dx, dy): (f64, f64)| {
        let ps = [b[0] * dx + b[1] * dy, b[2] * dx + b[1] * dy, b[0] * dx + b[3] * dy, b[2] * dx + b[3] * dy];
        (ps.iter().copied().fold(f64::MAX, f64::min), ps.iter().copied().fold(f64::MIN, f64::max))
    };
    let mut out = PageText::default();
    let mut boxes: Vec<([f64; 4], (f64, f64))> = Vec::new();
    let mut drawn: HashMap<u64, Vec<usize>> = HashMap::new();
    for r in raw {
        let (ax, ay) = shown(r.rect[0], r.rect[1]);
        let (bx, by) = shown(r.rect[2], r.rect[3]);
        let b = [ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)];
        if b[2] < 0.0 || b[0] > w || b[3] < 0.0 || b[1] > h {
            continue;
        }
        // Which way the text runs on the page as shown: across for nearly everything, but
        // down or up for turned labels.
        let ((ox, oy), (ex, ey)) = (shown(r.base[0], r.base[1]), shown(r.base[2], r.base[3]));
        let len = (ex - ox).hypot(ey - oy);
        let dir = if len > 1e-9 { ((ex - ox) / len, (ey - oy) / len) } else { boxes.last().map_or((1.0, 0.0), |p| p.1) };
        let gh = (b[3] - b[1]).max(0.1);
        // Text drawn twice, a hair apart, to look bold, however much comes between the two.
        // Characters are filed by their text and where they start, on a grid of squares
        // `CELL` points across, and only the squares near this one are looked in: far enough
        // round to cover the tolerances, up to four squares (8 points) out, which is plenty
        // for a hair. A cell holds few characters, because one that lands on another is
        // dropped, not filed.
        const CELL: f64 = 2.0;
        let (cx, cy) = ((b[0] / CELL).floor() as i64, (b[1] / CELL).floor() as i64);
        let text_hash = {
            let mut h = DefaultHasher::new();
            r.text.hash(&mut h);
            h.finish()
        };
        let key = |x: i64, y: i64| text_hash ^ (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        let (tx, ty) = (0.3 * (b[2] - b[0]).max(0.05 * gh), 0.1 * gh);
        let reach = |t: f64| ((t / CELL).ceil() as i64).clamp(1, 4);
        let (kx, ky) = (reach(tx), reach(ty));
        let double = (cx - kx..=cx + kx).any(|x| {
            (cy - ky..=cy + ky).any(|y| {
                drawn.get(&key(x, y)).is_some_and(|ks| {
                    ks.iter().any(|&k| {
                        let p = boxes[k].0;
                        out.str(k) == r.text && (p[0] - b[0]).abs() < tx && (p[1] - b[1]).abs() < ty
                    })
                })
            })
        });
        if double {
            continue;
        }
        // Except for type too small to read, packed in by the thousand: keep a few of those.
        let cell = drawn.entry(key(cx, cy)).or_default();
        if cell.len() < 8 {
            cell.push(out.glyphs.len());
        }
        let brk = match boxes.last() {
            None => Break::None,
            Some((p, pd)) => {
                // Along the line, and across it: the same line if the two overlap across it,
                // and this one doesn't jump back.
                let across = (-dir.1, dir.0);
                let ((a0, _), (_, pa1)) = (span(&b, dir), span(p, dir));
                let ((c0, c1), (pc0, pc1)) = (span(&b, across), span(p, across));
                let (ext, pext) = ((c1 - c0).max(0.1), (pc1 - pc0).max(0.1));
                let overlap = c1.min(pc1) - c0.max(pc0);
                let avg = (ext + pext) / 2.0;
                if dir.0 * pd.0 + dir.1 * pd.1 < 0.9 || overlap < 0.5 * ext.min(pext) || a0 < pa1 - avg {
                    Break::Line
                } else {
                    let blank = |s: &str| s.chars().all(char::is_whitespace);
                    if a0 - pa1 > 0.12 * avg && !blank(&r.text) && !blank(out.str(out.glyphs.len() - 1)) { Break::Space } else { Break::None }
                }
            }
        };
        let start = out.text.len() as u32;
        out.text.push_str(&r.text);
        out.glyphs.push(Glyph { start, end: out.text.len() as u32, rect: [(b[0] / w) as f32, (b[1] / h) as f32, (b[2] / w) as f32, (b[3] / h) as f32], dir: [dir.0 as f32, dir.1 as f32], brk });
        boxes.push((b, dir));
    }
    out
}

// ---- Encodings ----

fn standard(c: u8) -> Option<char> {
    Some(match c {
        39 => '’',
        96 => '‘',
        32..=126 => c as char,
        161 => '¡',
        162 => '¢',
        163 => '£',
        164 => '⁄',
        165 => '¥',
        166 => 'ƒ',
        167 => '§',
        168 => '¤',
        169 => '\'',
        170 => '“',
        171 => '«',
        172 => '‹',
        173 => '›',
        174 => '\u{FB01}',
        175 => '\u{FB02}',
        177 => '–',
        178 => '†',
        179 => '‡',
        180 => '·',
        182 => '¶',
        183 => '•',
        184 => '‚',
        185 => '„',
        186 => '”',
        187 => '»',
        188 => '…',
        189 => '‰',
        191 => '¿',
        193 => '`',
        194 => '´',
        195 => 'ˆ',
        196 => '˜',
        197 => '¯',
        198 => '˘',
        199 => '˙',
        200 => '¨',
        202 => '˚',
        203 => '¸',
        205 => '˝',
        206 => '˛',
        207 => 'ˇ',
        208 => '—',
        225 => 'Æ',
        227 => 'ª',
        232 => 'Ł',
        233 => 'Ø',
        234 => 'Œ',
        235 => 'º',
        241 => 'æ',
        245 => 'ı',
        248 => 'ł',
        249 => 'ø',
        250 => 'œ',
        251 => 'ß',
        _ => return None,
    })
}

fn winansi(c: u8) -> Option<char> {
    const HIGH: &str = "€\0‚ƒ„…†‡ˆ‰Š‹Œ\0Ž\0\0‘’“”•–—˜™š›œ\0žŸ";
    match c {
        32..=126 => Some(c as char),
        128..=159 => HIGH.chars().nth((c - 128) as usize).filter(|&c| c != '\0'),
        160 => Some(' '),
        173 => Some('-'),
        161..=255 => Some(c as char),
        _ => None,
    }
}

fn macroman(c: u8) -> Option<char> {
    const HIGH: &str = concat!(
        "ÄÅÇÉÑÖÜáàâäãåçéè",
        "êëíìîïñóòôöõúùûü",
        "†°¢£§•¶ß®©™´¨≠ÆØ",
        "∞±≤≥¥µ∂∑∏π∫ªºΩæø",
        "¿¡¬√ƒ≈∆«»… ÀÃÕŒœ",
        "–—“”‘’÷◊ÿŸ⁄€‹›\u{FB01}\u{FB02}",
        "‡·‚„‰ÂÊÁËÈÍÎÏÌÓÔ",
        "\u{F8FF}ÒÚÛÙıˆ˜¯˘˙˚¸˝˛ˇ",
    );
    match c {
        32..=126 => Some(c as char),
        128..=255 => HIGH.chars().nth((c - 128) as usize),
        _ => None,
    }
}

/// Glyph names from 0xA1 to 0xFF, in Latin-1 order.
const LATIN1: [&str; 95] = [
    "exclamdown", "cent", "sterling", "currency", "yen", "brokenbar", "section", "dieresis", "copyright", "ordfeminine", "guillemotleft", "logicalnot", "sfthyphen", "registered", "macron", "degree", "plusminus", "twosuperior", "threesuperior", "acute", "mu", "paragraph",
    "periodcentered", "cedilla", "onesuperior", "ordmasculine", "guillemotright", "onequarter", "onehalf", "threequarters", "questiondown", "Agrave", "Aacute", "Acircumflex", "Atilde", "Adieresis", "Aring", "AE", "Ccedilla", "Egrave", "Eacute", "Ecircumflex",
    "Edieresis", "Igrave", "Iacute", "Icircumflex", "Idieresis", "Eth", "Ntilde", "Ograve", "Oacute", "Ocircumflex", "Otilde", "Odieresis", "multiply", "Oslash", "Ugrave", "Uacute", "Ucircumflex", "Udieresis", "Yacute", "Thorn", "germandbls", "agrave", "aacute",
    "acircumflex", "atilde", "adieresis", "aring", "ae", "ccedilla", "egrave", "eacute", "ecircumflex", "edieresis", "igrave", "iacute", "icircumflex", "idieresis", "eth", "ntilde", "ograve", "oacute", "ocircumflex", "otilde", "odieresis", "divide", "oslash", "ugrave",
    "uacute", "ucircumflex", "udieresis", "yacute", "thorn", "ydieresis",
];

/// Glyph names for ASCII 0x20 to 0x7E, with the letters left to the code that reads them.
const ASCII: [&str; 95] = [
    "space", "exclam", "quotedbl", "numbersign", "dollar", "percent", "ampersand", "quotesingle", "parenleft", "parenright", "asterisk", "plus", "comma", "hyphen", "period", "slash", "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    "colon", "semicolon", "less", "equal", "greater", "question", "at", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "bracketleft", "backslash", "bracketright", "asciicircum", "underscore", "grave", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "braceleft", "bar", "braceright", "asciitilde",
];

const OTHER_NAMES: &[(&str, &str)] = &[
    ("quoteright", "’"),
    ("quoteleft", "‘"),
    ("quotedblleft", "“"),
    ("quotedblright", "”"),
    ("quotesinglbase", "‚"),
    ("quotedblbase", "„"),
    ("endash", "–"),
    ("emdash", "—"),
    ("bullet", "•"),
    ("ellipsis", "…"),
    ("dagger", "†"),
    ("daggerdbl", "‡"),
    ("perthousand", "‰"),
    ("guilsinglleft", "‹"),
    ("guilsinglright", "›"),
    ("Euro", "€"),
    ("trademark", "™"),
    ("fi", "fi"),
    ("fl", "fl"),
    ("ff", "ff"),
    ("ffi", "ffi"),
    ("ffl", "ffl"),
    ("OE", "Œ"),
    ("oe", "œ"),
    ("Scaron", "Š"),
    ("scaron", "š"),
    ("Zcaron", "Ž"),
    ("zcaron", "ž"),
    ("Ydieresis", "Ÿ"),
    ("florin", "ƒ"),
    ("circumflex", "ˆ"),
    ("tilde", "˜"),
    ("dotlessi", "ı"),
    ("dotlessj", "ȷ"),
    ("minus", "−"),
    ("fraction", "⁄"),
    ("Lslash", "Ł"),
    ("lslash", "ł"),
    ("breve", "˘"),
    ("dotaccent", "˙"),
    ("ring", "˚"),
    ("ogonek", "˛"),
    ("caron", "ˇ"),
    ("hungarumlaut", "˝"),
    ("nbspace", " "),
    ("nonbreakingspace", " "),
    ("middot", "·"),
    ("arrowright", "→"),
    ("arrowleft", "←"),
    ("arrowup", "↑"),
    ("arrowdown", "↓"),
    ("checkmark", "✓"),
    ("Omega", "Ω"),
    ("pi", "π"),
    ("Delta", "∆"),
    ("summation", "∑"),
    ("product", "∏"),
    ("radical", "√"),
    ("infinity", "∞"),
    ("integral", "∫"),
    ("approxequal", "≈"),
    ("notequal", "≠"),
    ("lessequal", "≤"),
    ("greaterequal", "≥"),
    ("partialdiff", "∂"),
    ("lozenge", "◊"),
    ("Ccaron", "Č"),
    ("ccaron", "č"),
    ("Ecaron", "Ě"),
    ("ecaron", "ě"),
    ("Rcaron", "Ř"),
    ("rcaron", "ř"),
    ("Gbreve", "Ğ"),
    ("gbreve", "ğ"),
    ("Idotaccent", "İ"),
    ("Scedilla", "Ş"),
    ("scedilla", "ş"),
    ("Aogonek", "Ą"),
    ("aogonek", "ą"),
    ("Eogonek", "Ę"),
    ("eogonek", "ę"),
    ("Cacute", "Ć"),
    ("cacute", "ć"),
    ("Nacute", "Ń"),
    ("nacute", "ń"),
    ("Sacute", "Ś"),
    ("sacute", "ś"),
    ("Zacute", "Ź"),
    ("zacute", "ź"),
    ("Zdotaccent", "Ż"),
    ("zdotaccent", "ż"),
    ("Ohungarumlaut", "Ő"),
    ("ohungarumlaut", "ő"),
    ("Uhungarumlaut", "Ű"),
    ("uhungarumlaut", "ű"),
    ("Uring", "Ů"),
    ("uring", "ů"),
    ("Amacron", "Ā"),
    ("amacron", "ā"),
    ("Emacron", "Ē"),
    ("emacron", "ē"),
];

/// A glyph's name as text: "a", "eacute", "uni00E9", "f_f_i", "a.sc".
fn glyph_name(name: &[u8]) -> Option<String> {
    // Real names are short; a long one is only there to make work.
    if name.len() > 256 {
        return None;
    }
    let n = std::str::from_utf8(name).ok()?;
    let base = n.split('.').next()?;
    if base.is_empty() {
        return None;
    }
    if base.contains('_') {
        let parts: Option<Vec<String>> = base.split('_').map(|p| glyph_name(p.as_bytes())).collect();
        return parts.map(|p| p.concat());
    }
    if base.len() == 1 && base.as_bytes()[0].is_ascii_alphabetic() {
        return Some(base.to_string());
    }
    let hex = |h: &str| h.len() >= 4 && h.bytes().all(|c| c.is_ascii_hexdigit());
    if let Some(h) = base.strip_prefix("uni").filter(|h| hex(h) && h.len() % 4 == 0) {
        let units: Vec<u16> = (0..h.len()).step_by(4).filter_map(|i| u16::from_str_radix(&h[i..i + 4], 16).ok()).collect();
        return Some(String::from_utf16_lossy(&units));
    }
    if let Some(h) = base.strip_prefix('u').filter(|h| hex(h) && h.len() <= 6) {
        return u32::from_str_radix(h, 16).ok().and_then(char::from_u32).map(String::from);
    }
    if let Some(i) = ASCII.iter().position(|&a| a == base) {
        return Some(((i as u8 + 32) as char).to_string());
    }
    if let Some(i) = LATIN1.iter().position(|&a| a == base) {
        return char::from_u32(0xA1 + i as u32).map(String::from);
    }
    OTHER_NAMES.iter().find(|(k, _)| *k == base).map(|(_, v)| v.to_string())
}

// ---- The standard fonts' widths, for files that leave them out ----

fn std_widths(name: &str) -> Option<&'static [u16; 95]> {
    let n = name.to_ascii_lowercase();
    let bold = n.contains("bold") || n.contains("black") || n.contains("heavy");
    let italic = n.contains("italic") || n.contains("oblique");
    if n.contains("symbol") || n.contains("dingbat") {
        return None;
    }
    if n.contains("courier") || n.contains("mono") {
        return Some(&COURIER);
    }
    if n.contains("times") || n.contains("georgia") || (n.contains("serif") && !n.contains("sans")) {
        return Some(if bold { &TIMES_BOLD } else if italic { &TIMES_ITALIC } else { &TIMES });
    }
    Some(if bold { &HELVETICA_BOLD } else { &HELVETICA })
}

const COURIER: [u16; 95] = [600; 95];
#[rustfmt::skip]
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556,
    278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611,
    278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500,
    334, 260, 334, 584,
];
#[rustfmt::skip]
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556,
    333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611,
    333, 278, 333, 584, 556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500,
    389, 280, 389, 584,
];
#[rustfmt::skip]
const TIMES: [u16; 95] = [
    250, 333, 408, 500, 500, 833, 778, 180, 333, 333, 500, 564, 250, 333, 250, 278, 500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    278, 278, 564, 564, 564, 444, 921, 722, 667, 667, 722, 611, 556, 722, 722, 333, 389, 722, 611, 889, 722, 722, 556, 722, 667, 556, 611, 722, 722, 944, 722, 722, 611,
    333, 278, 333, 469, 500, 333, 444, 500, 444, 500, 444, 333, 500, 500, 278, 278, 500, 278, 778, 500, 500, 500, 500, 333, 389, 278, 500, 500, 722, 500, 500, 444,
    480, 200, 480, 541,
];
#[rustfmt::skip]
const TIMES_BOLD: [u16; 95] = [
    250, 333, 555, 500, 500, 1000, 833, 278, 333, 333, 500, 570, 250, 333, 250, 278, 500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    333, 333, 570, 570, 570, 500, 930, 722, 667, 722, 722, 667, 611, 778, 778, 389, 500, 778, 667, 944, 722, 778, 611, 778, 722, 556, 667, 722, 722, 1000, 722, 722, 667,
    333, 278, 333, 581, 500, 333, 500, 556, 444, 556, 444, 333, 500, 556, 278, 333, 556, 278, 833, 556, 500, 556, 556, 444, 389, 333, 556, 500, 722, 500, 500, 444,
    394, 220, 394, 520,
];
#[rustfmt::skip]
const TIMES_ITALIC: [u16; 95] = [
    250, 333, 420, 500, 500, 833, 778, 214, 333, 333, 500, 675, 250, 333, 250, 278, 500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    333, 333, 675, 675, 675, 500, 920, 611, 611, 667, 722, 611, 611, 722, 722, 333, 444, 667, 556, 833, 667, 722, 611, 722, 611, 500, 556, 722, 611, 833, 611, 556, 556,
    389, 278, 389, 422, 500, 333, 500, 500, 444, 500, 444, 278, 500, 500, 278, 278, 444, 278, 722, 500, 500, 500, 500, 389, 389, 278, 500, 444, 667, 444, 444, 389,
    400, 275, 400, 541,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-page PDF drawing `content` with Helvetica (no widths listed) as /F1, the objects
    /// numbered and laid out the way writers do.
    fn pdf(content: &str, extra: &str) -> Vec<u8> {
        format!(
            "%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 600 800] >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R {extra} >> endobj\n4 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\n5 0 obj << /Length {} >>\nstream\n{content}\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n%%EOF\n",
            content.len() + 1
        )
        .into_bytes()
    }

    fn text_of(doc: &[u8]) -> String {
        let d = open(doc.to_vec()).unwrap();
        let p = d.page(0).unwrap();
        p.copy(0, p.glyphs.len())
    }

    #[test]
    fn reads_lines_and_spaces() {
        let doc = pdf("BT /F1 20 Tf 50 700 Td (Hello) Tj ( world) Tj 0 -30 Td [(Sec) 20 (ond)] TJ ET", "");
        assert_eq!(text_of(&doc), "Hello world\nSecond");
    }

    #[test]
    fn gaps_become_spaces() {
        // Words set apart by moving, not by a space character, the way most writers do it.
        let doc = pdf("BT /F1 20 Tf 50 700 Td [(one) -600 (two)] TJ ET", "");
        assert_eq!(text_of(&doc), "one two");
    }

    #[test]
    fn places_characters_on_the_page() {
        let d = open(pdf("BT /F1 20 Tf 60 700 Td (H) Tj ET", "")).unwrap();
        let p = d.page(0).unwrap();
        let r = p.glyphs[0].rect;
        assert!((r[0] - 0.1).abs() < 1e-3, "{r:?}");
        // Helvetica's H is 0.722 em wide.
        assert!(((r[2] - r[0]) * 600.0 - 14.44).abs() < 0.1, "{r:?}");
        // The baseline is 100 from the top; the box stands mostly above it.
        assert!(r[1] * 800.0 < 100.0 && r[3] * 800.0 > 100.0);
    }

    #[test]
    fn follows_a_turned_page() {
        let d = open(pdf("BT /F1 20 Tf 60 700 Td (H) Tj ET", "/Rotate 90")).unwrap();
        let r = d.page(0).unwrap().glyphs[0].rect;
        // Turned clockwise, the top of the page is on the right: x 60 of 600 is near the top.
        assert!(r[1] < 0.15 && r[1] > 0.05, "{r:?}");
        assert!(r[0] > 0.8, "{r:?}");
    }

    #[test]
    fn reads_tounicode_and_compressed_streams() {
        let cmap = "/CIDInit /ProcSet findresource begin begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange 2 beginbfchar <0001> <0048> <0002> <0069> endbfchar 1 beginbfrange <0003> <0004> <0021> endbfrange endcmap end";
        let content = "BT /F1 12 Tf 10 10 Td <000100020003> Tj ET";
        let z = |s: &str| miniz_oxide::deflate::compress_to_vec_zlib(s.as_bytes(), 6);
        let (zc, zm) = (z(content), z(cmap));
        let mut f = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >> endobj\n4 0 obj << /Type /Font /Subtype /Type0 /BaseFont /X /Encoding /Identity-H /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /DW 500 /W [1 [700 300]] >>] /ToUnicode 6 0 R >> endobj\n".to_vec();
        f.extend(format!("5 0 obj << /Length {} /Filter /FlateDecode >>\nstream\n", zc.len()).bytes());
        f.extend(&zc);
        f.extend(format!("\nendstream\nendobj\n6 0 obj << /Length {} /Filter /FlateDecode >>\nstream\n", zm.len()).bytes());
        f.extend(&zm);
        f.extend(b"\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n");
        let d = open(f).unwrap();
        let p = d.page(0).unwrap();
        assert_eq!(p.copy(0, p.glyphs.len()), "Hi!");
        let w = |i: usize| (p.glyphs[i].rect[2] - p.glyphs[i].rect[0]) * 200.0;
        assert!((w(0) - 8.4).abs() < 0.01 && (w(1) - 3.6).abs() < 0.01 && (w(2) - 6.0).abs() < 0.01);
    }

    #[test]
    fn reads_differences_and_forms() {
        let doc = "%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /XObject << /X1 6 0 R >> >> /Contents 5 0 R >> endobj\n4 0 obj << /Type /Font /Subtype /Type1 /BaseFont /ABCDEF+Times-Roman /FirstChar 1 /Widths [500 500 500] /Encoding << /Differences [1 /eacute /fi /uni263A] >> >> endobj\n5 0 obj << /Length 20 >>\nstream\nq 1 0 0 1 10 10 cm /X1 Do Q\nendstream\nendobj\n6 0 obj << /Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /Font << /F 4 0 R >> >> /Length 30 >>\nstream\nBT /F 10 Tf (\\001\\002\\003) Tj ET\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n";
        assert_eq!(text_of(doc.as_bytes()), "éfi☺");
    }

    #[test]
    fn turned_text_stays_on_its_line() {
        // Running up the page, then a normal line.
        let doc = pdf("BT /F1 20 Tf 0 1 -1 0 300 100 Tm (Up the side) Tj ET BT /F1 20 Tf 50 700 Td (Across) Tj ET", "");
        assert_eq!(text_of(&doc), "Up the side\nAcross");
    }

    #[test]
    fn drops_text_drawn_twice_for_bold() {
        let doc = pdf("BT /F1 20 Tf 50 700 Td (Bold) Tj ET BT /F1 20 Tf 50.3 700 Td (Bold) Tj ET", "");
        assert_eq!(text_of(&doc), "Bold");
    }

    #[test]
    fn words_and_selection_text() {
        let d = open(pdf("BT /F1 20 Tf 50 700 Td (Hello there, world) Tj ET", "")).unwrap();
        let p = d.page(0).unwrap();
        assert_eq!(p.word(7), (6, 11));
        assert_eq!(p.copy(6, 11), "there");
        assert_eq!(p.word(11), (11, 12));
    }

    #[test]
    fn filters_decode() {
        assert_eq!(ascii85(b"87cURD]i,\"Ebo80~>"), b"Hello World!");
        assert_eq!(run_length(&[2, b'a', b'b', b'c', 254, b'x', 128]), b"abcxxx");
        // "-----A---B" in LZW, from the PDF reference's own example.
        assert_eq!(lzw(&[0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01], true), b"-----A---B");
    }

    #[test]
    fn tables_have_their_lengths() {
        assert_eq!(macroman(0xFF), Some('ˇ'));
        assert_eq!(macroman(0x80), Some('Ä'));
        assert_eq!(winansi(0x80), Some('€'));
        assert_eq!(winansi(0x81), None);
        assert_eq!(winansi(0x9F), Some('Ÿ'));
        assert_eq!(glyph_name(b"ydieresis").as_deref(), Some("ÿ"));
        assert_eq!(glyph_name(b"f_f_i").as_deref(), Some("ffi"));
        assert_eq!(glyph_name(b"a.sc").as_deref(), Some("a"));
        assert_eq!(glyph_name(b"zero").as_deref(), Some("0"));
        assert_eq!(glyph_name(b"g37"), None);
    }

    /// A one-page PDF whose page draws stream 5, given as its dictionary entries and data.
    fn with_stream(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut f = b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Contents 5 0 R >> endobj\n".to_vec();
        f.extend(format!("5 0 obj << /Length {} {dict} >>\nstream\n", data.len()).bytes());
        f.extend(data);
        f.extend(b"\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n");
        f
    }

    #[test]
    fn hostile_streams_stay_small() {
        // A predictor with rows a terabyte long, over two bytes of data.
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&[2, 0], 6);
        let f = with_stream("/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 1099511627776 /Colors 99999 >>", &z);
        assert!(open(f).unwrap().page(0).is_some());
        // Run-length three times over: 128 KB that would come to 32 GB.
        let data = [129u8, 129].repeat(65536);
        let f = with_stream("/Filter [/RunLengthDecode /RunLengthDecode /RunLengthDecode]", &data);
        let d = open(f).unwrap();
        assert!(d.page(0).is_some());
        assert!(run_length(&[129u8, 129].repeat(4_000_000)).len() <= MAX_STREAM);
        assert!(ascii85(&vec![b'z'; MAX_STREAM / 4 + 10]).len() <= MAX_STREAM);
        // An odd-sized start, then runs: the buffer still mustn't double past the limit.
        let mut odd = vec![126u8];
        odd.extend([0u8; 127]);
        odd.extend([129u8, 7].repeat(1_100_000));
        assert!(run_length(&odd).capacity() <= MAX_STREAM);
        assert!(ascii85(&vec![b'z'; MAX_STREAM / 4 + 10]).capacity() <= MAX_STREAM);
    }

    #[test]
    fn long_mappings_are_left_out() {
        let long = "0041".repeat(40);
        let cm = parse_cmap(&format!("2 beginbfchar <01> <{long}> <02> <0042> endbfchar 1 beginbfrange <03> <FF> <{long}> endbfrange").into_bytes(), &mut 1_000_000);
        assert_eq!(cm.bf.len(), 1);
        assert_eq!(cm.bf[&2], "B");
    }

    #[test]
    fn trailer_after_trailer_is_quick() {
        let junk = "trailer << /X ".repeat(20_000);
        let mut f = with_stream("", junk.as_bytes());
        f.extend(junk.as_bytes());
        let started = std::time::Instant::now();
        assert!(open(f).is_ok());
        assert!(started.elapsed().as_secs() < 2, "{:?}", started.elapsed());
    }

    #[test]
    fn a_damaged_object_stops_at_the_next() {
        // Object 3's dictionary is never closed.
        let doc = pdf("BT /F1 20 Tf 50 700 Td (Still here) Tj ET", "");
        let s = String::from_utf8(doc).unwrap().replace("/Count 1 /MediaBox [0 0 600 800] >> endobj", "/Count 1 /MediaBox [0 0 600 800] endobj");
        assert_eq!(text_of(s.as_bytes()), "Still here");
    }

    #[test]
    fn forms_hide_what_falls_outside_their_box() {
        let doc = "%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /XObject << /X1 6 0 R >> >> /Contents 5 0 R >> endobj\n4 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\n5 0 obj << >>\nstream\n/X1 Do\nendstream\nendobj\n6 0 obj << /Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /Font << /F 4 0 R >> >> >>\nstream\nBT /F 10 Tf 20 50 Td (inside) Tj 180 0 Td (outside) Tj ET\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n";
        assert_eq!(text_of(doc.as_bytes()), "inside");
    }

    #[test]
    fn long_bold_lines_are_copied_once() {
        let line = "The quick brown fox jumps over the lazy dog, again and again!!";
        let doc = pdf(&format!("BT /F1 10 Tf 20 700 Td ({line}) Tj ET BT /F1 10 Tf 20.01 700 Td ({line}) Tj ET"), "");
        assert_eq!(text_of(&doc), line);
    }

    #[test]
    fn turned_text_says_which_way_it_runs() {
        let d = open(pdf("BT /F1 20 Tf 0 1 -1 0 300 100 Tm (Up) Tj ET", "")).unwrap();
        let p = d.page(0).unwrap();
        let g = &p.glyphs[0];
        // Up the page as it's shown: y goes down the screen, so up is negative.
        assert!(g.dir[0].abs() < 1e-6 && (g.dir[1] + 1.0).abs() < 1e-6, "{:?}", g.dir);
    }

    #[test]
    fn overlapping_width_ranges_stay_quick() {
        let w = "0 65535 500 ".repeat(2000);
        let doc = format!("%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >> endobj\n4 0 obj << /Type /Font /Subtype /Type0 /Encoding /Identity-H /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /W [{w}] >>] >> endobj\n5 0 obj << >>\nstream\nBT /F1 12 Tf <0041> Tj ET\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n");
        let started = std::time::Instant::now();
        open(doc.into_bytes()).unwrap().page(0);
        assert!(started.elapsed().as_secs() < 2, "{:?}", started.elapsed());
    }

    #[test]
    fn many_pages_share_what_they_inherit() {
        let junk = "0 ".repeat(200_000);
        let kids: String = (0..800).map(|i| format!("{} 0 R ", 10 + i)).collect();
        let mut doc = format!("%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [{kids}] /Count 800 /Resources << /Junk [{junk}] >> >> endobj\n");
        for i in 0..800 {
            doc.push_str(&format!("{} 0 obj << /Type /Page /Parent 2 0 R >> endobj\n", 10 + i));
        }
        doc.push_str("trailer << /Root 1 0 R >>\n");
        let d = open(doc.into_bytes()).unwrap();
        assert_eq!(d.page_count(), 800);
        assert!(Rc::ptr_eq(&d.pages[0].resources, &d.pages[799].resources));
    }

    #[test]
    fn a_part_listed_twice_draws_twice() {
        let doc = "%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << /F1 4 0 R >> >> /Contents [6 0 R 5 0 R 5 0 R 7 0 R] >> endobj\n4 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\n5 0 obj << >>\nstream\n0 -30 Td (A) Tj\nendstream\nendobj\n6 0 obj << >>\nstream\nBT /F1 20 Tf 50 700 Td\nendstream\nendobj\n7 0 obj << >>\nstream\nET\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n";
        assert_eq!(text_of(doc.as_bytes()), "A\nA");
    }

    #[test]
    fn a_turned_form_clips_where_it_is() {
        // Turned 45°, the box's corner reaches out to the right; "HIDDEN" sits past its edge
        // there, inside the square around the box but not in the box.
        let doc = "%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /XObject << /X1 6 0 R >> >> /Contents 5 0 R >> endobj\n4 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\n5 0 obj << >>\nstream\n/X1 Do\nendstream\nendobj\n6 0 obj << /Type /XObject /Subtype /Form /BBox [0 0 100 100] /Matrix [0.7071 0.7071 -0.7071 0.7071 300 300] /Resources << /Font << /F 4 0 R >> >> >>\nstream\nBT /F 6 Tf 0.7071 -0.7071 0.7071 0.7071 30 60 Tm (Shown) Tj 0.7071 -0.7071 0.7071 0.7071 33 -16 Tm (HIDDEN) Tj ET\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n";
        assert_eq!(text_of(doc.as_bytes()), "Shown");
    }

    #[test]
    fn one_letter_over_and_over_stays_quick() {
        // Squeezed to a hundredth of their width, so 40,000 fit on one line.
        let doc = pdf(&format!("BT /F1 10 Tf 1 Tz 0 700 Td ({}) Tj ET", "i".repeat(40_000)), "");
        let started = std::time::Instant::now();
        let d = open(doc).unwrap();
        assert!(d.page(0).unwrap().glyphs.len() > 1000);
        assert!(started.elapsed().as_secs() < 2, "{:?}", started.elapsed());
    }

    #[test]
    fn bold_a_size_apart_is_still_bold() {
        let doc = pdf("BT /F1 10 Tf 20 100 Td (Hello) Tj ET BT /F1 10.05 Tf 20.01 100 Td (Hello) Tj ET", "");
        assert_eq!(text_of(&doc), "Hello");
    }

    #[test]
    fn objects_hold_only_so_much() {
        let zeros = "0 ".repeat(3_000_000);
        let mut f = pdf("BT /F1 20 Tf 50 700 Td (Hi) Tj ET", "");
        f.extend(format!("8 0 obj [{zeros}] endobj\n9 0 obj [{zeros}] endobj\n").bytes());
        let d = open(f).unwrap();
        let len = |n| if let Some((Obj::Array(a), _)) = d.objs.get(&n) { a.len() } else { 0 };
        assert_eq!(len(8), 3_000_000);
        assert!(len(9) < 1_000_000, "{}", len(9));
        // What came first still reads.
        let p = d.page(0).unwrap();
        assert_eq!(p.copy(0, p.glyphs.len()), "Hi");
    }

    #[test]
    fn long_glyph_names_are_left_out() {
        assert_eq!(glyph_name(b"a_b").as_deref(), Some("ab"));
        assert_eq!(glyph_name("a_".repeat(500).as_bytes()), None);
    }

    #[test]
    fn a_row_of_dots_stays_quick() {
        let doc = pdf(&format!("BT /F1 10 Tf 0.1 Tz 0 700 Td ({}) Tj ET", ".".repeat(200_000)), "");
        let started = std::time::Instant::now();
        open(doc).unwrap().page(0).unwrap();
        assert!(started.elapsed().as_secs() < 2, "{:?}", started.elapsed());
    }

    #[test]
    fn character_map_ranges_stop_at_their_budget() {
        // Four whole ranges, then one a code too long for what's left, then one that fits it
        // exactly, then one more.
        let mut s = String::from("7 begincidrange ");
        for k in 0..4 {
            s.push_str(&format!("<{:08X}> <{:08X}> 0 ", k << 16, (k << 16) + 0xFFFF));
        }
        s.push_str("<00040000> <000493E0> 0 <00050000> <000593DF> 0 <00060000> <00060000> 0 endcidrange");
        assert_eq!(parse_cmap(s.as_bytes(), &mut 300_000).cid.len(), 300_000);
    }

    #[test]
    fn identity_crypt_streams_are_not_decrypted() {
        use crate::pdfcrypt::{md5, rc4};
        // Revision 2, an empty password: the file's key, and its check value.
        const PAD: [u8; 32] = [0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08, 0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A];
        let (o, id0) = ([7u8; 32], b"id");
        let key = md5(&[&PAD[..], &o[..], &(-4i32).to_le_bytes()[..], &id0[..]].concat())[..5].to_vec();
        let u = rc4(&key, &PAD);
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02X}")).collect::<String>();
        let object_key = |n: u8| md5(&[&key[..], &[n, 0, 0, 0, 0]].concat())[..10].to_vec();
        let secret = rc4(&object_key(5), b"BT /F1 20 Tf 50 700 Td (Hidden) Tj ET");
        let plain = b"BT /F1 20 Tf 50 600 Td (Plain) Tj ET";
        let mut f = b"%PDF-1.5\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << /F1 4 0 R >> >> /Contents [5 0 R 6 0 R] >> endobj\n4 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\n".to_vec();
        f.extend(format!("5 0 obj << /Length {} >>\nstream\n", secret.len()).bytes());
        f.extend(&secret);
        f.extend(format!("\nendstream\nendobj\n6 0 obj << /Length {} /Filter /Crypt >>\nstream\n", plain.len()).bytes());
        f.extend(plain);
        f.extend(format!("\nendstream\nendobj\n7 0 obj << /Filter /Standard /V 1 /R 2 /O <{}> /U <{}> /P -4 >> endobj\ntrailer << /Root 1 0 R /Encrypt 7 0 R /ID [<6964> <6964>] >>\n", hex(&o), hex(&u)).bytes());
        assert_eq!(text_of(&f), "Hidden\nPlain");
    }

    #[test]
    fn big_bold_letters_are_still_bold() {
        let doc = pdf("BT /F1 24 Tf 21.9 700 Td (W) Tj ET BT /F1 24 Tf 24.1 700 Td (W) Tj ET", "");
        assert_eq!(text_of(&doc), "W");
    }

    #[test]
    fn object_streams_stop_when_the_room_runs_out() {
        let zeros = "0 ".repeat(4_100_000);
        let mut f = pdf("BT /F1 20 Tf 50 700 Td (Hi) Tj ET", "");
        // Ten strings packed in an object stream, which is unpacked after the objects that
        // come later in the file have used up all the room.
        let inner = "(x) ".repeat(10);
        let head: String = (0..10).map(|k| format!("{} {} ", 20 + k, k * 4)).collect();
        let packed = format!("{head}{inner}");
        f.extend(format!("9 0 obj << /Type /ObjStm /N 10 /First {} /Length {} >>\nstream\n{packed}\nendstream\nendobj\n8 0 obj [{zeros}] endobj\n", head.len(), packed.len()).bytes());
        let d = open(f).unwrap();
        assert!((20..30).all(|n| !d.objs.contains_key(&n)));
    }

    #[test]
    fn fonts_share_their_maps() {
        let cmap = "1 beginbfchar <41> <0042> endbfchar";
        let fonts: String = (0..50).map(|k| format!("/F{k} {} 0 R ", 100 + k)).collect();
        let shows: String = (0..50).map(|k| format!("/F{k} 10 Tf (A) Tj ")).collect();
        let mut doc = format!("%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << {fonts}>> >> /Contents 5 0 R >> endobj\n5 0 obj << >>\nstream\nBT 50 700 Td {shows}ET\nendstream\nendobj\n6 0 obj << >>\nstream\n{cmap}\nendstream\nendobj\n");
        for k in 0..50 {
            doc.push_str(&format!("{} 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 6 0 R >> endobj\n", 100 + k));
        }
        doc.push_str("trailer << /Root 1 0 R >>\n");
        let d = open(doc.into_bytes()).unwrap();
        let p = d.page(0).unwrap();
        assert_eq!(p.copy(0, p.glyphs.len()), "B".repeat(50));
        assert_eq!(d.cmaps.borrow().len(), 1);
        assert_eq!(d.map_room.get(), MAX_MAPPED - 1);
    }

    #[test]
    fn a_map_that_wont_decode_is_tried_once() {
        let fonts: String = (0..20).map(|k| format!("/F{k} {} 0 R ", 100 + k)).collect();
        let shows: String = (0..20).map(|k| format!("/F{k} 10 Tf (A) Tj ")).collect();
        let mut doc = format!("%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << {fonts}>> >> /Contents 5 0 R >> endobj\n5 0 obj << >>\nstream\nBT 50 700 Td {shows}ET\nendstream\nendobj\n6 0 obj << /Filter [/FlateDecode /Unknown] >>\nstream\nxxxx\nendstream\nendobj\n");
        for k in 0..20 {
            doc.push_str(&format!("{} 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 6 0 R >> endobj\n", 100 + k));
        }
        doc.push_str("trailer << /Root 1 0 R >>\n");
        let d = open(doc.into_bytes()).unwrap();
        let p = d.page(0).unwrap();
        // Helvetica's own encoding still gives the letters.
        assert_eq!(p.copy(0, p.glyphs.len()), "A".repeat(20));
        assert!(matches!(d.cmaps.borrow().get(&(6, 0)), Some(None)));
        assert_eq!(d.map_bytes.get(), MAX_MAP_BYTES - 4);
    }

    #[test]
    fn a_form_that_decodes_to_nothing_still_costs() {
        // 32 MB of spaces, packed small, that the hex step then turns into nothing.
        let bomb = miniz_oxide::deflate::compress_to_vec_zlib(&vec![b' '; 32 << 20], 9);
        let draws = "/X1 Do ".repeat(500);
        let mut f = format!("%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /XObject << /X1 6 0 R >> >> /Contents 5 0 R >> endobj\n5 0 obj << >>\nstream\n{draws}\nendstream\nendobj\n6 0 obj << /Type /XObject /Subtype /Form /BBox [0 0 100 100] /Filter [/FlateDecode /ASCIIHexDecode] /Length {} >>\nstream\n", bomb.len()).into_bytes();
        f.extend(&bomb);
        f.extend(b"\nendstream\nendobj\ntrailer << /Root 1 0 R >>\n");
        let d = open(f).unwrap();
        let started = std::time::Instant::now();
        d.page(0).unwrap();
        // Each draw costs over a million instructions now, so only a few are decoded.
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    }

    #[test]
    fn words_read_from_a_picture_copy_as_lines() {
        let lines = vec![
            vec![("Hello".to_string(), [0.1, 0.1, 0.3, 0.15]), ("world".to_string(), [0.32, 0.1, 0.5, 0.15])],
            vec![("Ünïcode".to_string(), [0.1, 0.2, 0.4, 0.25])],
        ];
        let page = PageText::from_words(&lines);
        assert_eq!(page.copy(0, page.glyphs.len()), "Hello world
Ünïcode");
        assert_eq!(page.glyphs.len(), 17);
        let e = &page.glyphs[1];
        assert!((e.rect[0] - 0.14).abs() < 1e-5 && (e.rect[2] - 0.18).abs() < 1e-5);
        assert_eq!(page.word(7), (5, 10));
    }

    #[test]
    fn a_long_chain_of_steps_is_refused() {
        // Run-length over this gives back more of the same, step after step.
        let bomb = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0x81; 1 << 20], 9);
        let chain = "/RL ".repeat(1000);
        let f = with_stream(&format!("/Filter [/FlateDecode {chain}]"), &bomb);
        let started = std::time::Instant::now();
        let d = open(f).unwrap();
        d.page(0).unwrap();
        assert!(started.elapsed().as_secs() < 2, "{:?}", started.elapsed());
        let Some((Obj::Stream(s), _)) = d.objs.get(&5) else { panic!() };
        assert!(d.decode(s).0.is_none());
    }

    #[test]
    fn single_codes_count_against_the_budget() {
        let cm = parse_cmap(b"3 beginbfchar <41> <0042> <42> <0043> <43> <0044> endbfchar", &mut 2);
        assert_eq!(cm.bf.len(), 2);
    }

    #[test]
    fn rejects_what_isnt_a_pdf() {
        assert!(open(b"hello".to_vec()).is_err());
        // Damaged: no catalog at all.
        assert!(open(b"%PDF-1.4\n1 0 obj << /Foo 1 >> endobj\n".to_vec()).is_err());
    }
}
