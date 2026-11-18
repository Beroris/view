//! Word and Excel files (the zipped XML kind, from Office 2007 on), read for the preview without
//! Office: a Word document's paragraphs, headings, lists and tables come out as [`Block`]s, the
//! same as Markdown, and a spreadsheet's first sheet as rows of cells. What's lost is the look
//! (fonts, colours, pictures, layout); the words are all there, to read, select and search.
//! PowerPoint files are [`crate::slides`]' business.

use crate::markdown::{self, Block, Item, Run, Style};
use crate::xml::{Event, attr, walk};
use crate::zip;
use std::collections::HashMap;

fn open(data: &[u8], what: &str) -> Result<Vec<zip::Entry>, String> {
    let entries = zip::entries(data).ok_or_else(|| format!("This {what} is damaged, or isn't really one."))?;
    if entries.iter().any(|e| e.name.eq_ignore_ascii_case("EncryptedPackage")) {
        return Err(format!("This {what} is locked with a password."));
    }
    Ok(entries)
}

/// A relationships file: each Id and the part it points at, as a path in the zip.
pub(crate) fn relationships(data: &[u8], entries: &[zip::Entry], rels: &str, base: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(xml) = zip::read_text(data, entries, rels) else { return out };
    walk(&xml, |e| {
        if let Event::Start("Relationship", a) = e {
            if let (Some(id), Some(target)) = (attr(a, "Id"), attr(a, "Target")) {
                out.insert(id.into_owned(), resolve(base, &target));
            }
        }
    });
    out
}

/// A relationship's target, relative to the folder of the part it's from, as a zip path.
pub(crate) fn resolve(base: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for p in target.split('/') {
        match p {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            _ => parts.push(p),
        }
    }
    parts.join("/")
}

/// An on/off property: `<w:b/>` is on, `<w:b w:val="0"/>` off.
fn on(a: &str) -> bool {
    !matches!(attr(a, "val").as_deref(), Some("0" | "false" | "off" | "none"))
}

fn push_text(runs: &mut Vec<Run>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    match runs.last_mut() {
        Some(r) if r.style == style => r.text.push_str(text),
        _ => runs.push(Run { text: text.to_string(), style }),
    }
}

fn is_blank(runs: &[Run]) -> bool {
    runs.iter().all(|r| r.text.trim().is_empty())
}

// ---- Word ----

/// How a numbered list counts at one level.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Count {
    Bullet,
    Decimal,
    Lower,
    Upper,
    LowerRoman,
    UpperRoman,
}

fn marker(count: Count, n: usize, level: usize) -> String {
    let roman = |mut n: usize| {
        let mut s = String::new();
        for (v, r) in [(1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"), (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i")] {
            while n >= v {
                s.push_str(r);
                n -= v;
            }
        }
        s
    };
    let letters = |n: usize| {
        let mut s = String::new();
        let mut k = n;
        while k > 0 {
            k -= 1;
            s.insert(0, (b'a' + (k % 26) as u8) as char);
            k /= 26;
        }
        s
    };
    match count {
        Count::Bullet => ["•", "◦", "▪"][level % 3].to_string(),
        Count::Decimal => format!("{n}."),
        Count::Lower => format!("{}.", letters(n)),
        Count::Upper => format!("{}.", letters(n).to_uppercase()),
        Count::LowerRoman => format!("{}.", roman(n)),
        Count::UpperRoman => format!("{}.", roman(n).to_uppercase()),
    }
}

/// numbering.xml: for each list and level, how it counts.
fn numbering(xml: &str) -> HashMap<(String, usize), Count> {
    let mut abstracts: HashMap<String, HashMap<usize, Count>> = HashMap::new();
    let mut nums: HashMap<String, String> = HashMap::new();
    let (mut abs, mut lvl, mut num): (Option<String>, usize, Option<String>) = (None, 0, None);
    walk(xml, |e| match e {
        Event::Start("abstractNum", a) => abs = attr(a, "abstractNumId").map(|v| v.into_owned()),
        Event::End("abstractNum") => abs = None,
        Event::Start("lvl", a) => lvl = attr(a, "ilvl").and_then(|v| v.parse().ok()).unwrap_or(0),
        Event::Start("numFmt", a) => {
            if let Some(id) = &abs {
                let count = match attr(a, "val").as_deref() {
                    Some("decimal" | "decimalZero" | "ordinal") => Count::Decimal,
                    Some("lowerLetter") => Count::Lower,
                    Some("upperLetter") => Count::Upper,
                    Some("lowerRoman") => Count::LowerRoman,
                    Some("upperRoman") => Count::UpperRoman,
                    _ => Count::Bullet,
                };
                abstracts.entry(id.clone()).or_default().insert(lvl, count);
            }
        }
        Event::Start("num", a) => num = attr(a, "numId").map(|v| v.into_owned()),
        Event::End("num") => num = None,
        Event::Start("abstractNumId", a) => {
            if let (Some(n), Some(v)) = (&num, attr(a, "val")) {
                nums.insert(n.clone(), v.into_owned());
            }
        }
        _ => {}
    });
    let mut out = HashMap::new();
    for (num, abs) in nums {
        if let Some(levels) = abstracts.get(&abs) {
            for (&l, &c) in levels {
                out.insert((num.clone(), l), c);
            }
        }
    }
    out
}

/// styles.xml: which paragraph styles are headings, and at what level.
fn heading_styles(xml: &str) -> HashMap<String, u8> {
    let mut out = HashMap::new();
    let mut id: Option<String> = None;
    walk(xml, |e| match e {
        Event::Start("style", a) => id = attr(a, "styleId").map(|v| v.into_owned()),
        Event::End("style") => id = None,
        Event::Start("name", a) => {
            if let (Some(id), Some(name)) = (&id, attr(a, "val")) {
                let n = name.to_ascii_lowercase();
                let level = if n == "title" {
                    Some(1)
                } else if n == "subtitle" {
                    Some(3)
                } else {
                    n.strip_prefix("heading ").and_then(|l| l.trim().parse::<u8>().ok())
                };
                if let Some(l) = level {
                    out.insert(id.clone(), l.clamp(1, 6));
                }
            }
        }
        Event::Start("outlineLvl", a) => {
            if let (Some(id), Some(l)) = (&id, attr(a, "val").and_then(|v| v.parse::<u8>().ok())) {
                if l < 9 {
                    out.entry(id.clone()).or_insert((l + 1).min(6));
                }
            }
        }
        _ => {}
    });
    out
}

/// A paragraph being read.
#[derive(Default)]
struct Para {
    runs: Vec<Run>,
    style: Option<String>,
    list: Option<(String, usize)>,
    outline: Option<u8>,
}

/// A table being read: its rows, and the cell being filled.
#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Run>>>,
    row: Vec<Vec<Run>>,
    cell: Vec<Run>,
}

/// Lists being built: each level's open list, and its counters.
#[derive(Default)]
struct Lists {
    /// The blocks a list at each level is going into, innermost last.
    open: Vec<Vec<Item>>,
    counters: HashMap<(String, usize), usize>,
}

impl Lists {
    fn add(&mut self, level: usize, marker: String, runs: Vec<Run>) {
        let level = level.min(8);
        while self.open.len() > level + 1 {
            self.close_one();
        }
        while self.open.len() < level + 1 {
            self.open.push(Vec::new());
        }
        self.open[level].push(Item { marker, task: None, blocks: vec![Block::Paragraph(runs)] });
    }

    /// Close the innermost list, putting it inside the item it hangs from.
    fn close_one(&mut self) {
        let Some(items) = self.open.pop() else { return };
        if items.is_empty() {
            return;
        }
        match self.open.last_mut().and_then(|l| l.last_mut()) {
            Some(parent) => parent.blocks.push(Block::List(items)),
            None => self.open.push(items), // nothing to hang it from: keep it as the outer list
        }
    }

    fn finish(&mut self, out: &mut Vec<Block>) {
        while self.open.len() > 1 {
            self.close_one();
        }
        if let Some(items) = self.open.pop() {
            if !items.is_empty() {
                out.push(Block::List(items));
            }
        }
        self.counters.clear();
    }
}

/// A Word document's text, as blocks to lay out like Markdown.
pub fn docx(data: &[u8]) -> Result<Vec<Block>, String> {
    let entries = open(data, "Word document")?;
    let main = relationships(data, &entries, "_rels/.rels", "")
        .into_values()
        .find(|t| t.to_ascii_lowercase().ends_with("document.xml"))
        .unwrap_or_else(|| "word/document.xml".into());
    let xml = zip::read_text(data, &entries, &main).ok_or("This Word document has no text in it.")?;
    let headings = zip::read_text(data, &entries, "word/styles.xml").map(|s| heading_styles(&s)).unwrap_or_default();
    let counts = zip::read_text(data, &entries, "word/numbering.xml").map(|s| numbering(&s)).unwrap_or_default();
    Ok(word_blocks(&xml, &headings, &counts))
}

fn word_blocks(xml: &str, headings: &HashMap<String, u8>, counts: &HashMap<(String, usize), Count>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut paras: Vec<Para> = Vec::new();
    let mut tables: Vec<Table> = Vec::new();
    let mut lists = Lists::default();
    // Inside a run's properties, a paragraph's properties, text, a hyperlink, deleted text.
    let (mut in_rpr, mut in_ppr, mut in_t, mut link, mut skip) = (false, false, false, 0usize, 0usize);
    let mut style: Style = 0;
    let mut num_id: Option<String> = None;
    let mut ilvl = 0usize;
    walk(xml, |e| match e {
        Event::Start("del" | "instrText" | "delText", _) => skip += 1,
        Event::End("del" | "instrText" | "delText") => skip = skip.saturating_sub(1),
        Event::Start("p", _) => paras.push(Para::default()),
        Event::Start("pPr", _) => in_ppr = true,
        Event::End("pPr") => {
            in_ppr = false;
            if let (Some(p), Some(id)) = (paras.last_mut(), num_id.take()) {
                if id != "0" {
                    p.list = Some((id, ilvl));
                }
            }
            ilvl = 0;
        }
        Event::Start("pStyle", a) if in_ppr => {
            if let Some(p) = paras.last_mut() {
                p.style = attr(a, "val").map(|v| v.into_owned());
            }
        }
        Event::Start("outlineLvl", a) if in_ppr && !in_rpr => {
            if let (Some(p), Some(l)) = (paras.last_mut(), attr(a, "val").and_then(|v| v.parse::<u8>().ok())) {
                if l < 9 {
                    p.outline = Some((l + 1).min(6));
                }
            }
        }
        Event::Start("numId", a) if in_ppr => num_id = attr(a, "val").map(|v| v.into_owned()),
        Event::Start("ilvl", a) if in_ppr => ilvl = attr(a, "val").and_then(|v| v.parse().ok()).unwrap_or(0),
        Event::Start("r", _) => style = 0,
        Event::Start("rPr", _) => in_rpr = true,
        Event::End("rPr") => in_rpr = false,
        Event::Start("b", a) if in_rpr && !in_ppr => {
            if on(a) {
                style |= markdown::BOLD
            }
        }
        Event::Start("i", a) if in_rpr && !in_ppr => {
            if on(a) {
                style |= markdown::ITALIC
            }
        }
        Event::Start("strike" | "dstrike", a) if in_rpr && !in_ppr => {
            if on(a) {
                style |= markdown::STRIKE
            }
        }
        Event::Start("hyperlink", _) => link += 1,
        Event::End("hyperlink") => link = link.saturating_sub(1),
        Event::Start("t", _) => in_t = true,
        Event::End("t") => in_t = false,
        Event::Text(t) if in_t && skip == 0 => {
            let s = style | if link > 0 { markdown::LINK } else { 0 };
            if let Some(p) = paras.last_mut() {
                push_text(&mut p.runs, &t, s);
            }
        }
        Event::Start("tab", _) if !in_ppr => {
            if let Some(p) = paras.last_mut() {
                push_text(&mut p.runs, "\t", style);
            }
        }
        Event::Start("br" | "cr", a) if !in_ppr => {
            if attr(a, "type").as_deref() != Some("page") {
                if let Some(p) = paras.last_mut() {
                    push_text(&mut p.runs, "\n", style);
                }
            }
        }
        Event::Start("docPr", a) => {
            // A picture: its description in its place.
            let what = attr(a, "descr").filter(|d| !d.trim().is_empty()).or_else(|| attr(a, "title")).map(|d| d.into_owned()).unwrap_or_else(|| "Picture".into());
            if let Some(p) = paras.last_mut() {
                push_text(&mut p.runs, &what, markdown::IMAGE);
            }
        }
        Event::End("p") => {
            let Some(p) = paras.pop() else { return };
            // A paragraph inside a text box goes on after the one holding it.
            if let Some(t) = tables.last_mut() {
                if !t.cell.is_empty() && !is_blank(&p.runs) {
                    push_text(&mut t.cell, "\n", 0);
                }
                t.cell.extend(p.runs);
                return;
            }
            if is_blank(&p.runs) {
                return;
            }
            let level = p.outline.or_else(|| p.style.as_ref().and_then(|s| headings.get(s).copied()));
            match (&p.list, level) {
                (Some((id, l)), None) => {
                    let count = counts.get(&(id.clone(), *l)).copied().unwrap_or(Count::Bullet);
                    // Deeper levels start counting again under each new item above them.
                    lists.counters.retain(|(i, k), _| i != id || *k <= *l);
                    let n = lists.counters.entry((id.clone(), *l)).or_insert(0);
                    *n += 1;
                    let m = marker(count, *n, *l);
                    lists.add(*l, m, p.runs);
                }
                _ => {
                    lists.finish(&mut out);
                    out.push(match level {
                        Some(l) => Block::Heading(l, p.runs),
                        None => Block::Paragraph(p.runs),
                    });
                }
            }
        }
        Event::Start("tbl", _) => {
            lists.finish(&mut out);
            tables.push(Table::default());
        }
        Event::Start("tr", _) => {
            if let Some(t) = tables.last_mut() {
                t.row.clear();
            }
        }
        Event::Start("tc", _) => {
            if let Some(t) = tables.last_mut() {
                t.cell.clear();
            }
        }
        Event::End("tc") => {
            if let Some(t) = tables.last_mut() {
                let cell = std::mem::take(&mut t.cell);
                t.row.push(cell);
            }
        }
        Event::End("tr") => {
            if let Some(t) = tables.last_mut() {
                let row = std::mem::take(&mut t.row);
                t.rows.push(row);
            }
        }
        Event::End("tbl") => {
            let Some(t) = tables.pop() else { return };
            match tables.last_mut() {
                // A table in a table: its words go in the outer cell.
                Some(outer) => {
                    for row in t.rows {
                        for cell in row {
                            if !outer.cell.is_empty() {
                                push_text(&mut outer.cell, "\n", 0);
                            }
                            outer.cell.extend(cell);
                        }
                    }
                }
                None => {
                    if let Some(table) = table_block(t.rows) {
                        out.push(table);
                    }
                }
            }
        }
        _ => {}
    });
    lists.finish(&mut out);
    out
}

/// Rows of cells as a table block, the first row its head. Rows are padded to the same width.
fn table_block(mut rows: Vec<Vec<Vec<Run>>>) -> Option<Block> {
    rows.retain(|r| !r.is_empty());
    let width = rows.iter().map(Vec::len).max()?;
    for r in rows.iter_mut() {
        r.resize(width, Vec::new());
    }
    let head = rows.remove(0);
    Some(Block::Table { align: vec![markdown::Align::Left; width], head, rows })
}

/// An attribute by its full name, prefix and all: `r:id`, which `attr` would confuse with `id`.
pub(crate) fn attr_ns(attrs: &str, want: &str) -> Option<String> {
    let at = attrs.find(&format!("{want}=\""))? + want.len() + 2;
    let end = attrs[at..].find('"')?;
    Some(crate::xml::unescape(&attrs[at..at + end]).into_owned())
}

// ---- Excel ----

/// A spreadsheet, as far as the preview shows it: the sheets' names, and one sheet's cells.
pub struct Workbook {
    pub sheets: Vec<String>,
    /// Which sheet `rows` is.
    pub shown: usize,
    pub rows: Vec<Vec<String>>,
    /// Rows or columns were left out past the limits.
    pub cut: bool,
}

/// How a cell's number reads, from its style's number format.
#[derive(Clone, Debug, PartialEq)]
enum Format {
    General,
    Date { time: bool, date: bool },
    Fixed { decimals: usize, group: bool, percent: bool, prefix: String, suffix: String },
}

fn builtin_format(id: u32) -> Format {
    let fixed = |decimals, group, percent| Format::Fixed { decimals, group, percent, prefix: String::new(), suffix: String::new() };
    match id {
        1 => fixed(0, false, false),
        2 => fixed(2, false, false),
        3 => fixed(0, true, false),
        4 => fixed(2, true, false),
        9 => fixed(0, false, true),
        10 => fixed(2, false, true),
        14..=17 | 27..=31 | 34..=36 | 50..=58 => Format::Date { time: false, date: true },
        18..=21 | 45..=47 => Format::Date { time: true, date: false },
        22 => Format::Date { time: true, date: true },
        _ => Format::General,
    }
}

/// A format code such as `#,##0.00`, `0%`, `"£"#,##0` or `dd/mm/yyyy`, roughly.
fn parse_format(code: &str) -> Format {
    // Only the first section (for positive numbers) matters here.
    let code = code.split(';').next().unwrap_or("");
    let mut plain = String::new();
    let mut literal = String::new();
    let mut chars = code.chars().peekable();
    let mut seen_digit = false;
    let (mut prefix, mut suffix) = (String::new(), String::new());
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                literal.clear();
                for d in chars.by_ref() {
                    if d == '"' {
                        break;
                    }
                    literal.push(d);
                }
                if seen_digit { suffix.push_str(&literal) } else { prefix.push_str(&literal) }
            }
            '[' => {
                // [$€-809] is a currency; [Red] and [h] are not text.
                let mut inner = String::new();
                for d in chars.by_ref() {
                    if d == ']' {
                        break;
                    }
                    inner.push(d);
                }
                if let Some(cur) = inner.strip_prefix('$') {
                    let sym = cur.split('-').next().unwrap_or("");
                    if seen_digit { suffix.push_str(sym) } else { prefix.push_str(sym) }
                } else if inner.chars().all(|c| matches!(c, 'h' | 'H' | 'm' | 'M' | 's' | 'S')) {
                    plain.push('h');
                }
            }
            '\\' => {
                if let Some(d) = chars.next() {
                    if seen_digit { suffix.push(d) } else { prefix.push(d) }
                }
            }
            '$' | '£' | '€' | '¥' => {
                if seen_digit { suffix.push(c) } else { prefix.push(c) }
            }
            '_' | '*' => {
                chars.next();
            }
            '0' | '#' | '?' => {
                seen_digit = true;
                plain.push(c);
            }
            _ => plain.push(c),
        }
    }
    let lower = plain.to_ascii_lowercase();
    let date = lower.contains('y') || lower.contains('d');
    let time = lower.contains('h') || lower.contains('s') || (lower.contains('m') && !date && !seen_digit);
    if (date || time) && !seen_digit {
        return Format::Date { time, date: date || (lower.contains('m') && !time) };
    }
    if !seen_digit {
        return Format::General;
    }
    let decimals = plain.find('.').map_or(0, |i| plain[i + 1..].chars().take_while(|c| matches!(c, '0' | '#' | '?')).count());
    Format::Fixed { decimals, group: plain.contains(','), percent: plain.contains('%'), prefix: prefix.trim().to_string(), suffix: suffix.trim_end().to_string() }
}

/// Digits with commas every three, before the point.
fn grouped(s: &str) -> String {
    let (int, frac) = s.split_once('.').map_or((s, None), |(a, b)| (a, Some(b)));
    let (sign, digits) = int.strip_prefix('-').map_or(("", int), |d| ("-", d));
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    match frac {
        Some(f) => format!("{sign}{out}.{f}"),
        None => format!("{sign}{out}"),
    }
}

/// A number the way Excel's General format shows it: up to 15 significant digits, no
/// trailing zeros.
fn general(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    if v.abs() >= 1e15 || v.abs() < 1e-9 {
        return format!("{v:e}");
    }
    let digits = (15 - (v.abs().log10().floor() as i32 + 1)).clamp(0, 15) as usize;
    let s = format!("{v:.digits$}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// A serial day number as a date and time: days since the end of 1899 (or 1904).
fn date_text(serial: f64, base1904: bool, f: &Format) -> String {
    let Format::Date { time, date } = f else { return general(serial) };
    let mut days = serial.floor() as i64;
    let secs = ((serial - serial.floor()) * 86400.0).round() as i64;
    if !base1904 && days >= 60 {
        days -= 1; // Excel counts a 29 February 1900 that never was
    }
    // Days from 1 January 1970.
    let unix = days + if base1904 { 24107 } else { -25568 };
    let (y, m, d) = civil(unix);
    let (h, min, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (date, time) {
        (true, true) => format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}"),
        (false, true) if s != 0 => format!("{h:02}:{min:02}:{s:02}"),
        (false, true) => format!("{h:02}:{min:02}"),
        _ => format!("{y:04}-{m:02}-{d:02}"),
    }
}

/// Days since 1970 as a year, month and day (Howard Hinnant's method).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn number_text(raw: &str, f: &Format, base1904: bool) -> String {
    let Ok(v) = raw.trim().parse::<f64>() else { return raw.to_string() };
    match f {
        Format::General => general(v),
        Format::Date { .. } => date_text(v, base1904, f),
        Format::Fixed { decimals, group, percent, prefix, suffix } => {
            let v = if *percent { v * 100.0 } else { v };
            let s = format!("{v:.decimals$}");
            let s = if *group { grouped(&s) } else { s };
            format!("{prefix}{s}{}{suffix}", if *percent { "%" } else { "" })
        }
    }
}

/// The column of a cell reference: "C7" is 2.
fn column(r: &str) -> Option<usize> {
    let mut n = 0usize;
    let mut any = false;
    for c in r.chars() {
        if c.is_ascii_alphabetic() {
            n = n * 26 + (c.to_ascii_uppercase() as u8 - b'A') as usize + 1;
            any = true;
        } else {
            break;
        }
    }
    any.then(|| n - 1)
}

/// An Excel workbook: its sheets' names, and the first sheet with anything in it.
pub fn xlsx(data: &[u8], max_rows: usize, max_cols: usize) -> Result<Workbook, String> {
    let entries = open(data, "spreadsheet")?;
    let book = zip::read_text(data, &entries, "xl/workbook.xml").ok_or("This spreadsheet is damaged.")?;
    let rels = relationships(data, &entries, "xl/_rels/workbook.xml.rels", "xl");
    let mut sheets: Vec<(String, String)> = Vec::new();
    let mut base1904 = false;
    walk(&book, |e| match e {
        Event::Start("sheet", a) => {
            let hidden = matches!(attr(a, "state").as_deref(), Some("hidden" | "veryHidden"));
            if let (false, Some(name), Some(part)) = (hidden, attr(a, "name"), attr_ns(a, "r:id").and_then(|id| rels.get(&id).cloned())) {
                sheets.push((name.into_owned(), part));
            }
        }
        Event::Start("workbookPr", a) => base1904 = matches!(attr(a, "date1904").as_deref(), Some("1" | "true")),
        _ => {}
    });
    if sheets.is_empty() {
        return Err("This spreadsheet has no sheets.".into());
    }
    let strings = zip::read_text(data, &entries, "xl/sharedStrings.xml").map(|s| shared_strings(&s)).unwrap_or_default();
    let formats = zip::read_text(data, &entries, "xl/styles.xml").map(|s| cell_formats(&s)).unwrap_or_default();
    let names: Vec<String> = sheets.iter().map(|(n, _)| n.clone()).collect();
    let mut first: Option<(usize, Vec<Vec<String>>, bool)> = None;
    for (i, (_, part)) in sheets.iter().enumerate() {
        let Some(xml) = zip::read_text(data, &entries, part) else { continue };
        let (rows, cut) = sheet_rows(&xml, &strings, &formats, base1904, max_rows, max_cols);
        if !rows.is_empty() {
            first = Some((i, rows, cut));
            break;
        }
        first.get_or_insert((i, rows, cut));
    }
    let (shown, rows, cut) = first.unwrap_or((0, Vec::new(), false));
    Ok(Workbook { sheets: names, shown, rows, cut })
}

fn shared_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut in_t, mut phonetic) = (String::new(), false, false);
    walk(xml, |e| match e {
        Event::Start("si", _) => cur.clear(),
        Event::End("si") => out.push(std::mem::take(&mut cur)),
        Event::Start("rPh", _) => phonetic = true,
        Event::End("rPh") => phonetic = false,
        Event::Start("t", _) => in_t = true,
        Event::End("t") => in_t = false,
        Event::Text(t) if in_t && !phonetic => cur.push_str(&t),
        _ => {}
    });
    out
}

/// Each cell style's number format, by the style's index.
fn cell_formats(xml: &str) -> Vec<Format> {
    let mut custom: HashMap<u32, Format> = HashMap::new();
    let mut out = Vec::new();
    let mut in_xfs = false;
    walk(xml, |e| match e {
        Event::Start("numFmt", a) => {
            if let (Some(id), Some(code)) = (attr(a, "numFmtId").and_then(|v| v.parse().ok()), attr(a, "formatCode")) {
                custom.insert(id, parse_format(&code));
            }
        }
        Event::Start("cellXfs", _) => in_xfs = true,
        Event::End("cellXfs") => in_xfs = false,
        Event::Start("xf", a) if in_xfs => {
            let id: u32 = attr(a, "numFmtId").and_then(|v| v.parse().ok()).unwrap_or(0);
            out.push(custom.get(&id).cloned().unwrap_or_else(|| builtin_format(id)));
        }
        _ => {}
    });
    out
}

fn sheet_rows(xml: &str, strings: &[String], formats: &[Format], base1904: bool, max_rows: usize, max_cols: usize) -> (Vec<Vec<String>>, bool) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cut = false;
    let (mut row_at, mut col_at) = (0usize, 0usize);
    let (mut kind, mut style) = (String::new(), 0usize);
    let (mut value, mut in_v, mut in_is_t) = (String::new(), false, false);
    let mut next_row = 0usize;
    walk(xml, |e| match e {
        Event::Start("row", a) => {
            row_at = attr(a, "r").and_then(|v| v.parse::<usize>().ok()).map_or(next_row, |r| r.saturating_sub(1));
            next_row = row_at + 1;
            col_at = 0;
        }
        Event::Start("c", a) => {
            if let Some(c) = attr(a, "r").and_then(|r| column(&r)) {
                col_at = c;
            }
            kind = attr(a, "t").map(|v| v.into_owned()).unwrap_or_default();
            style = attr(a, "s").and_then(|v| v.parse().ok()).unwrap_or(0);
            value.clear();
        }
        Event::Start("v", _) => in_v = true,
        Event::End("v") => in_v = false,
        Event::Start("t", _) => in_is_t = true,
        Event::End("t") => in_is_t = false,
        Event::Text(t) if in_v || in_is_t => value.push_str(&t),
        Event::End("c") => {
            let col = col_at;
            col_at += 1;
            if value.is_empty() {
                return;
            }
            if row_at >= max_rows || col >= max_cols {
                cut = true;
                return;
            }
            let text = match kind.as_str() {
                "s" => value.trim().parse::<usize>().ok().and_then(|i| strings.get(i)).cloned().unwrap_or_default(),
                "b" => if value.trim() == "1" { "TRUE" } else { "FALSE" }.to_string(),
                "str" | "inlineStr" | "e" => value.clone(),
                _ => number_text(&value, formats.get(style).unwrap_or(&Format::General), base1904),
            };
            if text.is_empty() {
                return;
            }
            if rows.len() <= row_at {
                rows.resize(row_at + 1, Vec::new());
            }
            let r = &mut rows[row_at];
            if r.len() <= col {
                r.resize(col + 1, String::new());
            }
            r[col] = text;
        }
        _ => {}
    });
    // Nothing above or left of the data: drop empty rows at the top, and empty columns.
    while rows.first().is_some_and(|r| r.iter().all(String::is_empty)) {
        rows.remove(0);
    }
    while rows.last().is_some_and(|r| r.iter().all(String::is_empty)) {
        rows.pop();
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let left = (0..width).take_while(|&c| rows.iter().all(|r| r.get(c).is_none_or(String::is_empty))).count();
    for r in rows.iter_mut() {
        r.resize(width, String::new());
        r.drain(..left);
    }
    (rows, cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::tests::zip;

    fn text(runs: &[Run]) -> String {
        runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn word_headings_lists_tables_and_styles() {
        let styles = r#"<w:styles><w:style w:styleId="Heading1"><w:name w:val="heading 1"/></w:style><w:style w:styleId="Titel"><w:name w:val="Title"/></w:style></w:styles>"#;
        let numbering = r#"<w:numbering><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="lowerLetter"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
        let doc = r#"<w:document><w:body>
            <w:p><w:pPr><w:pStyle w:val="Titel"/></w:pPr><w:r><w:t>Report</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="Heading1"/><w:rPr><w:b/></w:rPr></w:pPr><w:r><w:t>Intro</w:t></w:r></w:p>
            <w:p><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">Bold </w:t></w:r><w:r><w:rPr><w:b w:val="0"/></w:rPr><w:t>plain</w:t></w:r><w:del><w:r><w:delText>gone</w:delText></w:r></w:del></w:p>
            <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>One</w:t></w:r></w:p>
            <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Sub</w:t></w:r></w:p>
            <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Two</w:t></w:r></w:p>
            <w:p/>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr>
            <w:tr><w:tc><w:p><w:r><w:t>1</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
            </w:body></w:document>"#;
        let data = zip(&[("[Content_Types].xml", b"<Types/>"), ("word/document.xml", doc.as_bytes()), ("word/styles.xml", styles.as_bytes()), ("word/numbering.xml", numbering.as_bytes())]);
        let blocks = docx(&data).unwrap();
        assert!(matches!(&blocks[0], Block::Heading(1, r) if text(r) == "Report"));
        assert!(matches!(&blocks[1], Block::Heading(1, r) if text(r) == "Intro"));
        match &blocks[2] {
            Block::Paragraph(r) => {
                assert_eq!(text(r), "Bold plain");
                assert_eq!(r[0].style, markdown::BOLD);
                assert_eq!(r[1].style, 0);
            }
            b => panic!("{b:?}"),
        }
        match &blocks[3] {
            Block::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].marker, "1.");
                assert_eq!(items[1].marker, "2.");
                assert!(matches!(&items[0].blocks[1], Block::List(sub) if sub[0].marker == "a."));
            }
            b => panic!("{b:?}"),
        }
        match &blocks[4] {
            Block::Table { head, rows, .. } => {
                assert_eq!(text(&head[1]), "B");
                assert_eq!(rows[0].len(), 2);
            }
            b => panic!("{b:?}"),
        }
        assert_eq!(blocks.len(), 5);
    }

    #[test]
    fn spreadsheet_cells_strings_numbers_and_dates() {
        let book = r#"<workbook xmlns:r="r"><sheets><sheet name="Empty" sheetId="1" r:id="rId1"/><sheet name="Hidden" state="hidden" sheetId="3" r:id="rId3"/><sheet name="Data" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
        let rels = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="/xl/worksheets/sheet2.xml"/><Relationship Id="rId3" Target="worksheets/sheet3.xml"/></Relationships>"#;
        let strings = r#"<sst><si><t>Name</t></si><si><r><t>Pr</t></r><r><t>ice</t></r></si><si><t>Tea &amp; cake</t></si></sst>"#;
        let styles = r#"<styleSheet><numFmts><numFmt numFmtId="164" formatCode="&quot;£&quot;#,##0.00"/></numFmts><cellStyleXfs><xf numFmtId="9"/></cellStyleXfs><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="14"/><xf numFmtId="9"/></cellXfs></styleSheet>"#;
        let s1 = r#"<worksheet><sheetData/></worksheet>"#;
        let s2 = r#"<worksheet><sheetData>
            <row r="2"><c r="B2" t="s"><v>0</v></c><c r="C2" t="s"><v>1</v></c><c r="D2"><v>When</v></c></row>
            <row r="3"><c r="B3" t="s"><v>2</v></c><c r="C3" s="1"><v>1234.5</v></c><c r="D3" s="2"><v>45000</v></c><c r="E3" s="3"><v>0.25</v></c></row>
            <row r="4"><c r="B4" t="inlineStr"><is><t>Inline</t></is></c><c r="C4"><v>0.1</v></c><c r="D4" t="b"><v>1</v></c></row>
            </sheetData></worksheet>"#;
        let data = zip(&[
            ("xl/workbook.xml", book.as_bytes()),
            ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
            ("xl/sharedStrings.xml", strings.as_bytes()),
            ("xl/styles.xml", styles.as_bytes()),
            ("xl/worksheets/sheet1.xml", s1.as_bytes()),
            ("xl/worksheets/sheet2.xml", s2.as_bytes()),
        ]);
        let wb = xlsx(&data, 100, 100).unwrap();
        assert_eq!(wb.sheets, ["Empty", "Data"]);
        assert_eq!(wb.shown, 1);
        assert_eq!(wb.rows[0], ["Name", "Price", "When", ""]);
        assert_eq!(wb.rows[1], ["Tea & cake", "£1,234.50", "2023-03-15", "25%"]);
        assert_eq!(wb.rows[2], ["Inline", "0.1", "TRUE", ""]);
    }

    #[test]
    fn numbers_read_like_excel() {
        assert_eq!(general(0.1 + 0.2), "0.3");
        assert_eq!(general(42.0), "42");
        assert_eq!(general(-1.5), "-1.5");
        assert_eq!(grouped("-1234567.891"), "-1,234,567.891");
        assert_eq!(parse_format("0.0%"), Format::Fixed { decimals: 1, group: false, percent: true, prefix: String::new(), suffix: String::new() });
        assert_eq!(parse_format("[$€-809]#,##0"), Format::Fixed { decimals: 0, group: true, percent: false, prefix: "€".into(), suffix: String::new() });
        assert_eq!(parse_format("dd/mm/yyyy h:mm"), Format::Date { time: true, date: true });
        assert_eq!(parse_format("mmm-yy"), Format::Date { time: false, date: true });
        assert_eq!(date_text(1.5, false, &Format::Date { time: true, date: true }), "1900-01-01 12:00");
        assert_eq!(date_text(61.0, false, &Format::Date { time: false, date: true }), "1900-03-01");
        assert_eq!(resolve("ppt", "../media/a.png"), "media/a.png");
        assert_eq!(resolve("xl", "worksheets/s.xml"), "xl/worksheets/s.xml");
    }
}
