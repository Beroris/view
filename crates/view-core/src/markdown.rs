//! Markdown, read well enough to show a README nicely: headings, paragraphs, emphasis, code,
//! quotes, lists (with tasks), tables, rules and links. It isn't a full CommonMark parser and
//! doesn't need to be: what it can't read comes through as plain text, never lost.

/// Bold, italic and the rest, as bits.
pub type Style = u8;
pub const BOLD: Style = 1;
pub const ITALIC: Style = 2;
pub const CODE: Style = 4;
pub const STRIKE: Style = 8;
pub const LINK: Style = 16;
/// An image's description, shown in place of the image.
pub const IMAGE: Style = 32;

/// A stretch of text in one style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Run>),
    Paragraph(Vec<Run>),
    /// The language named after the fence, and the code.
    Code(String, String),
    Quote(Vec<Block>),
    /// Items, each with its marker ("•", "1.", or a tick box) and what's in it.
    List(Vec<Item>),
    Rule,
    Table { align: Vec<Align>, head: Vec<Vec<Run>>, rows: Vec<Vec<Vec<Run>>> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub marker: String,
    /// `Some(done)` for a task.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

pub fn parse(text: &str) -> Vec<Block> {
    let lines: Vec<String> = text.lines().map(|l| l.replace('\t', "    ")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    blocks(&refs)
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start_matches(' ').len()
}

fn is_rule(t: &str) -> bool {
    let t: String = t.chars().filter(|c| *c != ' ').collect();
    t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

fn fence(t: &str) -> Option<&'static str> {
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn heading(t: &str) -> Option<(u8, &str)> {
    let level = t.bytes().take_while(|&b| b == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &t[level..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let rest = rest.trim().trim_end_matches('#').trim_end();
    Some((level as u8, rest))
}

/// A list item's marker: its text and the width it takes with the space after it.
fn list_marker(t: &str) -> Option<(String, usize, bool)> {
    let b = t.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'-' | b'*' | b'+') && b[1] == b' ' {
        return Some(("•".into(), 2, false));
    }
    if (b.len() == 1) && matches!(b[0], b'-' | b'*' | b'+') {
        return Some(("•".into(), 1, false));
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if (1..=9).contains(&digits) && b.len() > digits && matches!(b[digits], b'.' | b')') && (b.len() == digits + 1 || b[digits + 1] == b' ') {
        return Some((format!("{}.", &t[..digits]), digits + 2, true));
    }
    None
}

fn is_table_divider(t: &str) -> bool {
    let t = t.trim();
    t.contains('-') && t.contains(['|', ':']) && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
        || (t.contains('|') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) && t.contains('-'))
}

fn cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut code = false;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '`' => {
                code = !code;
                cell.push(c);
            }
            '|' if !code => out.push(std::mem::take(&mut cell).trim().to_string()),
            c => cell.push(c),
        }
    }
    out.push(cell.trim().to_string());
    out
}

/// Whether a line starts something other than a paragraph, and so ends one.
fn interrupts(t: &str) -> bool {
    fence(t).is_some() || heading(t).is_some() || t.starts_with('>') || is_rule(t) || list_marker(t).is_some_and(|(m, _, ordered)| !ordered || m == "1.")
}

fn blocks(lines: &[&str]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        if t.is_empty() {
            i += 1;
            continue;
        }
        let ind = indent(line);
        if let Some(f) = fence(t) {
            let lang = t[3..].trim().split_whitespace().next().unwrap_or("").to_string();
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with(f) {
                code.push(lines[i].get(ind.min(indent(lines[i]))..).unwrap_or(""));
                i += 1;
            }
            i += 1;
            out.push(Block::Code(lang, code.join("\n")));
            continue;
        }
        if ind >= 4 {
            let mut code = Vec::new();
            while i < lines.len() && (indent(lines[i]) >= 4 || lines[i].trim().is_empty()) {
                code.push(lines[i].get(4..).unwrap_or(""));
                i += 1;
            }
            while code.last().is_some_and(|l| l.trim().is_empty()) {
                code.pop();
            }
            out.push(Block::Code(String::new(), code.join("\n")));
            continue;
        }
        if let Some((level, rest)) = heading(t) {
            out.push(Block::Heading(level, inline(rest)));
            i += 1;
            continue;
        }
        if is_rule(t) {
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        if t.starts_with('>') {
            let mut inner = Vec::new();
            while i < lines.len() {
                let l = lines[i].trim_start();
                if let Some(rest) = l.strip_prefix('>') {
                    inner.push(rest.strip_prefix(' ').unwrap_or(rest));
                } else if !l.is_empty() && !interrupts(l) && !inner.last().is_none_or(|p: &&str| p.trim().is_empty()) {
                    inner.push(l); // a lazy continuation of the quoted paragraph
                } else {
                    break;
                }
                i += 1;
            }
            out.push(Block::Quote(blocks(&inner)));
            continue;
        }
        if list_marker(t).is_some() {
            let (list, next) = list(lines, i);
            out.push(list);
            i = next;
            continue;
        }
        if t.contains('|') && lines.get(i + 1).is_some_and(|n| is_table_divider(n)) {
            let head: Vec<Vec<Run>> = cells(t).iter().map(|c| inline(c)).collect();
            let align = cells(lines[i + 1])
                .iter()
                .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
                    (true, true) => Align::Center,
                    (false, true) => Align::Right,
                    _ => Align::Left,
                })
                .collect();
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
                rows.push(cells(lines[i]).iter().map(|c| inline(c)).collect());
                i += 1;
            }
            out.push(Block::Table { align, head, rows });
            continue;
        }
        if t.starts_with('<') {
            // Raw HTML: keep any words in it, drop the tags.
            let mut html = String::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                html.push_str(lines[i].trim());
                html.push(' ');
                i += 1;
            }
            let runs = inline(&html);
            if runs.iter().any(|r| !r.text.trim().is_empty()) {
                out.push(Block::Paragraph(trim_runs(runs)));
            }
            continue;
        }
        // A paragraph, perhaps turned into a heading by a line of = or - under it.
        let mut text = String::new();
        let mut level = 0;
        while i < lines.len() {
            let l = lines[i];
            let lt = l.trim();
            if lt.is_empty() || (!text.is_empty() && interrupts(lt) && !(is_rule(lt) && lt.chars().all(|c| c == '-'))) {
                break;
            }
            if !text.is_empty() && !lt.is_empty() && lt.chars().all(|c| c == '=') {
                level = 1;
                i += 1;
                break;
            }
            if !text.is_empty() && lt.len() >= 2 && lt.chars().all(|c| c == '-') {
                level = 2;
                i += 1;
                break;
            }
            if !text.is_empty() {
                let hard = text.ends_with("  ") || text.ends_with('\\');
                if text.ends_with('\\') {
                    text.pop();
                }
                text = text.trim_end().to_string();
                text.push(if hard { '\n' } else { ' ' });
            }
            text.push_str(lt);
            i += 1;
        }
        let runs = inline(&text);
        out.push(if level > 0 { Block::Heading(level, runs) } else { Block::Paragraph(runs) });
    }
    out
}

fn list(lines: &[&str], mut i: usize) -> (Block, usize) {
    let base = indent(lines[i]);
    let ordered = list_marker(lines[i].trim()).is_some_and(|(_, _, o)| o);
    let mut items = Vec::new();
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        let Some((marker, width, o)) = list_marker(t) else { break };
        if indent(line) != base && !(indent(line) < base + 2 && indent(line) + 2 > base) || o != ordered {
            break;
        }
        let content = base + width;
        let mut body = vec![t.get(width..).unwrap_or("")];
        i += 1;
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() {
                // A blank line stays in the item only if the item carries on after it.
                match lines[i + 1..].iter().find(|n| !n.trim().is_empty()) {
                    Some(n) if indent(n) >= content => {
                        body.push("");
                        i += 1;
                        continue;
                    }
                    _ => break,
                }
            }
            let li = indent(l);
            if li > base {
                body.push(&l[li.min(content)..]);
            } else if list_marker(l.trim()).is_none() && !interrupts(l.trim()) && !body.last().is_some_and(|b| b.is_empty()) {
                body.push(l.trim());
            } else {
                break;
            }
            i += 1;
        }
        let mut task = None;
        if let Some(first) = body.first_mut() {
            for (box_, done) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
                if let Some(rest) = first.strip_prefix(box_) {
                    task = Some(done);
                    *first = rest;
                }
            }
        }
        items.push(Item { marker, task, blocks: blocks(&body) });
        // Skip the blank lines between items.
        let mut j = i;
        while j < lines.len() && lines[j].trim().is_empty() {
            j += 1;
        }
        if j < lines.len() && indent(lines[j]) == base && list_marker(lines[j].trim()).is_some_and(|(_, _, o)| o == ordered) {
            i = j;
        } else {
            break;
        }
    }
    (Block::List(items), i)
}

fn trim_runs(mut runs: Vec<Run>) -> Vec<Run> {
    if let Some(r) = runs.first_mut() {
        r.text = r.text.trim_start().to_string();
    }
    if let Some(r) = runs.last_mut() {
        r.text = r.text.trim_end().to_string();
    }
    runs.retain(|r| !r.text.is_empty());
    runs
}

const ENTITIES: &[(&str, &str)] = &[("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&#39;", "'"), ("&apos;", "'"), ("&nbsp;", "\u{a0}"), ("&copy;", "©"), ("&reg;", "®"), ("&trade;", "™"), ("&mdash;", "—"), ("&ndash;", "–"), ("&hellip;", "…"), ("&rarr;", "→"), ("&larr;", "←"), ("&times;", "×")];

/// Text with its emphasis, code, links and images, as runs.
pub fn inline(s: &str) -> Vec<Run> {
    let mut runs = Vec::new();
    spans(s, 0, &mut runs);
    // Join neighbours in the same style.
    let mut out: Vec<Run> = Vec::new();
    for r in runs {
        if r.text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.style == r.style => last.text.push_str(&r.text),
            _ => out.push(r),
        }
    }
    out
}

fn spans(s: &str, base: Style, out: &mut Vec<Run>) {
    let b = s.as_bytes();
    let mut style = base;
    let mut cur = String::new();
    let flush = |cur: &mut String, style: Style, out: &mut Vec<Run>| {
        if !cur.is_empty() {
            out.push(Run { text: std::mem::take(cur), style });
        }
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\\' if b.get(i + 1).is_some_and(|x| x.is_ascii_punctuation()) => {
                cur.push(b[i + 1] as char);
                i += 2;
            }
            b'`' => {
                let n = b[i..].iter().take_while(|&&x| x == b'`').count();
                let fence = "`".repeat(n);
                match s[i + n..].find(&fence) {
                    Some(k) => {
                        flush(&mut cur, style, out);
                        let code = &s[i + n..i + n + k];
                        let code = if code.len() >= 2 && code.starts_with(' ') && code.ends_with(' ') { &code[1..code.len() - 1] } else { code };
                        out.push(Run { text: code.to_string(), style: style | CODE });
                        i += n + k + n;
                    }
                    None => {
                        cur.push_str(&fence);
                        i += n;
                    }
                }
            }
            b'*' | b'_' | b'~' => {
                let n = b[i..].iter().take_while(|&&x| x == c).count();
                let prev = if i == 0 { b' ' } else { b[i - 1] };
                let next = b.get(i + n).copied().unwrap_or(b' ');
                let flag = match (c, n) {
                    (b'~', 2) => STRIKE,
                    (b'~', _) => 0,
                    (_, 1) => ITALIC,
                    (_, 2) => BOLD,
                    (_, 3) => BOLD | ITALIC,
                    _ => 0,
                };
                // Underscores inside a word (snake_case) are just underscores.
                let intraword = c == b'_' && prev.is_ascii_alphanumeric() && next.is_ascii_alphanumeric();
                let delim = &s[i..i + n];
                if flag == 0 || intraword {
                    cur.push_str(delim);
                } else if style & flag == flag && !prev.is_ascii_whitespace() {
                    flush(&mut cur, style, out);
                    style &= !flag;
                } else if !next.is_ascii_whitespace() && s[i + n..].contains(delim) {
                    flush(&mut cur, style, out);
                    style |= flag;
                } else {
                    cur.push_str(delim);
                }
                i += n;
            }
            b'!' if b.get(i + 1) == Some(&b'[') => match link(s, i + 1) {
                Some((text, end)) => {
                    flush(&mut cur, style, out);
                    let alt = text.trim();
                    if !alt.is_empty() {
                        out.push(Run { text: alt.to_string(), style: style | IMAGE });
                    }
                    i = end;
                }
                None => {
                    cur.push('!');
                    i += 1;
                }
            },
            b'[' => match link(s, i) {
                Some((text, end)) => {
                    flush(&mut cur, style, out);
                    spans(text, style | LINK, out);
                    i = end;
                }
                None => {
                    cur.push('[');
                    i += 1;
                }
            },
            b'<' => {
                let close = s[i..].find('>').map(|k| i + k);
                match close {
                    Some(e) if s[i + 1..e].starts_with("http") && !s[i + 1..e].contains(' ') => {
                        flush(&mut cur, style, out);
                        out.push(Run { text: s[i + 1..e].to_string(), style: style | LINK });
                        i = e + 1;
                    }
                    Some(e) if b.get(i + 1).is_some_and(|x| x.is_ascii_alphabetic() || *x == b'/' || *x == b'!') => {
                        let tag = s[i + 1..e].trim_start_matches('/').split([' ', '/']).next().unwrap_or("").to_ascii_lowercase();
                        if tag == "br" {
                            cur.push('\n');
                        } else if tag == "img" {
                            if let Some(alt) = attribute(&s[i..e], "alt").filter(|a| !a.is_empty()) {
                                flush(&mut cur, style, out);
                                out.push(Run { text: alt, style: style | IMAGE });
                            }
                        }
                        i = e + 1;
                    }
                    _ => {
                        cur.push('<');
                        i += 1;
                    }
                }
            }
            b'&' => match ENTITIES.iter().find(|(e, _)| s[i..].starts_with(e)) {
                Some((e, r)) => {
                    cur.push_str(r);
                    i += e.len();
                }
                None => {
                    cur.push('&');
                    i += 1;
                }
            },
            _ => {
                let ch = s[i..].chars().next().unwrap();
                cur.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    flush(&mut cur, style, out);
}

/// `[text](target)` or `[text][ref]` starting at `at`: the text, and where the link ends.
fn link(s: &str, at: usize) -> Option<(&str, usize)> {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut j = at;
    while j < b.len() {
        match b[j] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            b'\\' => j += 1,
            _ => {}
        }
        j += 1;
    }
    if j >= b.len() {
        return None;
    }
    let text = &s[at + 1..j];
    match b.get(j + 1) {
        Some(b'(') => {
            let mut depth = 0;
            let mut k = j + 1;
            while k < b.len() {
                match b[k] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some((text, k + 1));
                        }
                    }
                    _ => {}
                }
                k += 1;
            }
            None
        }
        Some(b'[') => s[j + 2..].find(']').map(|k| (text, j + 2 + k + 1)),
        _ => None,
    }
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let q = rest.chars().next()?;
    if q == '"' || q == '\'' {
        rest[1..].find(q).map(|e| rest[1..1 + e].to_string())
    } else {
        Some(rest.split([' ', '>']).next().unwrap_or("").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, style: Style) -> Run {
        Run { text: text.into(), style }
    }

    #[test]
    fn emphasis_code_and_links() {
        assert_eq!(inline("a **b** *c* `d` ~~e~~"), vec![run("a ", 0), run("b", BOLD), run(" ", 0), run("c", ITALIC), run(" ", 0), run("d", CODE), run(" ", 0), run("e", STRIKE)]);
        assert_eq!(inline("see [the **docs**](https://x.y) now"), vec![run("see ", 0), run("the ", LINK), run("docs", LINK | BOLD), run(" now", 0)]);
        assert_eq!(inline("snake_case_name and 2 * 3"), vec![run("snake_case_name and 2 * 3", 0)]);
        assert_eq!(inline("![logo](a.png) &amp; <br>x"), vec![run("logo", IMAGE), run(" & \nx", 0)]);
        assert_eq!(inline("a \\*not\\* b"), vec![run("a *not* b", 0)]);
    }

    #[test]
    fn blocks_read_a_readme() {
        let md = "# View\n\nA **fast** viewer.\nSecond line.\n\n- one\n- two\n  - nested\n- [x] done\n\n1. first\n2. second\n\n> quoted\n> text\n\n```rust\nfn main() {}\n```\n\n---\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\nSub\n---\n";
        let b = parse(md);
        assert_eq!(b[0], Block::Heading(1, vec![run("View", 0)]));
        assert_eq!(b[1], Block::Paragraph(vec![run("A ", 0), run("fast", BOLD), run(" viewer. Second line.", 0)]));
        let Block::List(items) = &b[2] else { panic!("{:?}", b[2]) };
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[1].blocks[1], Block::List(inner) if inner.len() == 1));
        assert_eq!(items[2].task, Some(true));
        let Block::List(items) = &b[3] else { panic!() };
        assert_eq!(items[1].marker, "2.");
        assert_eq!(b[4], Block::Quote(vec![Block::Paragraph(vec![run("quoted text", 0)])]));
        assert_eq!(b[5], Block::Code("rust".into(), "fn main() {}".into()));
        assert_eq!(b[6], Block::Rule);
        let Block::Table { align, head, rows } = &b[7] else { panic!() };
        assert_eq!(align, &vec![Align::Left, Align::Right]);
        assert_eq!(head.len(), 2);
        assert_eq!(rows[0][1], vec![run("2", 0)]);
        assert_eq!(b[8], Block::Heading(2, vec![run("Sub", 0)]));
    }

    #[test]
    fn html_keeps_its_words() {
        let b = parse("<p align=\"center\">\n  <img src=\"x.png\" alt=\"Logo\">\n</p>\n\nText");
        assert_eq!(b[0], Block::Paragraph(vec![run("Logo", IMAGE)]));
        assert_eq!(b[1], Block::Paragraph(vec![run("Text", 0)]));
        assert!(parse("<div>\n</div>").is_empty());
    }
}
