//! A web page, read for its words: headings, paragraphs, lists, quotes, tables, code and links,
//! as the same blocks Markdown gives, so it can be shown the same way. It is a reading view,
//! not a browser: no styles, no scripts, no layout of the page's own. Pictures show as their
//! descriptions. Broken markup is read as far as it makes sense, never refused.

use crate::markdown::{Align, BOLD, Block, CODE, IMAGE, ITALIC, Item, LINK, Run, STRIKE, Style};

/// The deepest that elements are followed inside one another.
const MAX_DEPTH: usize = 200;

enum Node {
    Text(String),
    Element { tag: String, attrs: Vec<(String, String)>, children: Vec<Node> },
}

pub fn to_blocks(html: &str) -> Vec<Block> {
    let tree = parse(html);
    let mut out = Vec::new();
    blocks_of(&tree, &mut out, false);
    out
}

// ---- Reading the markup into a tree ----

/// Elements that never hold anything.
fn is_void(tag: &str) -> bool {
    matches!(tag, "br" | "hr" | "img" | "input" | "meta" | "link" | "area" | "base" | "col" | "embed" | "source" | "track" | "wbr" | "param")
}

/// Elements that start a new block, and so end a paragraph left open before them.
fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "address" | "article" | "aside" | "blockquote" | "details" | "div" | "dl" | "dd" | "dt" | "fieldset" | "figcaption" | "figure" | "footer" | "form" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "header" | "hr" | "li" | "main" | "nav" | "ol" | "p" | "pre" | "section" | "table" | "ul" | "summary" | "center"
    )
}

/// Elements whose insides aren't for reading.
fn is_hidden(tag: &str) -> bool {
    matches!(tag, "script" | "style" | "head" | "template" | "noscript" | "svg" | "iframe" | "object" | "select" | "textarea" | "canvas" | "audio" | "video" | "title" | "math")
}

fn parse(html: &str) -> Vec<Node> {
    // The open elements, innermost last; the first is the page itself.
    let mut stack: Vec<(String, Vec<(String, String)>, Vec<Node>)> = vec![(String::new(), Vec::new(), Vec::new())];
    fn close(stack: &mut Vec<(String, Vec<(String, String)>, Vec<Node>)>) {
        if stack.len() > 1 {
            let (tag, attrs, children) = stack.pop().unwrap();
            stack.last_mut().unwrap().2.push(Node::Element { tag, attrs, children });
        }
    }
    /// Close elements down to and including the innermost `tag`, unless one of `stops` is met
    /// first: a list item ends the item before it, but not one in a list outside its own.
    fn close_to(stack: &mut Vec<(String, Vec<(String, String)>, Vec<Node>)>, tags: &[&str], stops: &[&str]) {
        let Some(at) = stack.iter().rposition(|(t, _, _)| tags.contains(&t.as_str()) || stops.contains(&t.as_str())) else { return };
        if at == 0 || stops.contains(&stack[at].0.as_str()) {
            return;
        }
        while stack.len() > at {
            close(stack);
        }
    }
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let end = html[i..].find('<').map_or(html.len(), |n| i + n);
            stack.last_mut().unwrap().2.push(Node::Text(entities(&html[i..end])));
            i = end;
            continue;
        }
        let rest = &html[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(html.len(), |n| i + n + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = rest.find('>').map_or(html.len(), |n| i + n + 1);
            continue;
        }
        let closing = rest.starts_with("</");
        let name_at = if closing { 2 } else { 1 };
        let name_len = rest[name_at..].bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b':').count();
        if name_len == 0 {
            // A stray "<": just a character.
            stack.last_mut().unwrap().2.push(Node::Text("<".into()));
            i += 1;
            continue;
        }
        let tag = rest[name_at..name_at + name_len].to_ascii_lowercase();
        let (attrs, after) = attributes(rest, name_at + name_len);
        i += after;
        if closing {
            if stack.iter().skip(1).any(|(t, _, _)| *t == tag) {
                while stack.len() > 1 {
                    let done = stack.last().unwrap().0 == tag;
                    close(&mut stack);
                    if done {
                        break;
                    }
                }
            }
            continue;
        }
        // Script and style hold text that isn't markup: skip to where they end.
        if matches!(tag.as_str(), "script" | "style" | "textarea" | "title") {
            let lower_end = format!("</{tag}");
            let found = html[i..].as_bytes().windows(lower_end.len()).position(|w| w.eq_ignore_ascii_case(lower_end.as_bytes()));
            i = match found {
                Some(n) => html[i + n..].find('>').map_or(html.len(), |m| i + n + m + 1),
                None => html.len(),
            };
            continue;
        }
        // What HTML lets a page leave unclosed.
        match tag.as_str() {
            "li" => close_to(&mut stack, &["li"], &["ul", "ol"]),
            "tr" => close_to(&mut stack, &["tr"], &["table"]),
            "td" | "th" => close_to(&mut stack, &["td", "th"], &["tr", "table"]),
            "thead" | "tbody" | "tfoot" => close_to(&mut stack, &["thead", "tbody", "tfoot"], &["table"]),
            "dt" | "dd" => close_to(&mut stack, &["dt", "dd"], &["dl"]),
            t if is_block(t) => close_to(&mut stack, &["p"], &["td", "th", "li", "blockquote", "div", "section", "article", "table"]),
            _ => {}
        }
        if is_void(&tag) || rest[..after].ends_with("/>") || stack.len() > MAX_DEPTH {
            stack.last_mut().unwrap().2.push(Node::Element { tag, attrs, children: Vec::new() });
        } else {
            stack.push((tag, attrs, Vec::new()));
        }
    }
    while stack.len() > 1 {
        close(&mut stack);
    }
    stack.pop().unwrap().2
}

/// A tag's attributes, from just after its name, and where the tag ends.
fn attributes(tag: &str, mut i: usize) -> (Vec<(String, String)>, usize) {
    let b = tag.as_bytes();
    let mut out = Vec::new();
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            return (out, b.len());
        }
        if b[i] == b'>' {
            return (out, i + 1);
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        if i == start {
            i += 1; // a stray "=": step over it
            continue;
        }
        let name = tag[start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let quote = b[i];
                let from = i + 1;
                i = from;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
                value = entities(&tag[from..i]);
                i = (i + 1).min(b.len());
            } else {
                let from = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                value = entities(&tag[from..i]);
            }
        }
        out.push((name, value));
    }
}

/// Text with its `&amp;` and friends turned into the characters they stand for.
fn entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map_or(rest.len(), |n| n + 1);
        let name = &rest[1..end];
        let known = if let Some(num) = name.strip_prefix('#') {
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => num.parse().ok(),
            };
            code.and_then(char::from_u32).map(String::from)
        } else {
            named(name).map(String::from)
        };
        match known {
            Some(c) if !name.is_empty() => {
                out.push_str(&c);
                rest = &rest[end..];
                rest = rest.strip_prefix(';').unwrap_or(rest);
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{A0}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "laquo" => "«",
        "raquo" => "»",
        "bull" => "•",
        "middot" => "·",
        "times" => "×",
        "divide" => "÷",
        "deg" => "°",
        "plusmn" => "±",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        "sect" => "§",
        "para" => "¶",
        "larr" => "←",
        "rarr" => "→",
        "uarr" => "↑",
        "darr" => "↓",
        "check" => "✓",
        "ensp" | "emsp" | "thinsp" => " ",
        "shy" | "zwnj" | "zwj" => "",
        _ => return None,
    })
}

// ---- Turning the tree into blocks ----

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// Runs of text gathered for the paragraph being read.
#[derive(Default)]
struct Line {
    runs: Vec<Run>,
}

impl Line {
    /// Add text as the page has it: any run of spaces and line ends is one space.
    fn text(&mut self, s: &str, style: Style) {
        let mut out = String::with_capacity(s.len());
        let mut space = self.runs.last().is_none_or(|r| r.text.ends_with([' ', '\n']));
        for c in s.chars() {
            if c.is_whitespace() && c != '\u{A0}' {
                if !space {
                    out.push(' ');
                }
                space = true;
            } else {
                out.push(c);
                space = false;
            }
        }
        self.raw(&out, style);
    }

    /// Add text exactly as given.
    fn raw(&mut self, s: &str, style: Style) {
        if s.is_empty() {
            return;
        }
        match self.runs.last_mut() {
            Some(last) if last.style == style => last.text.push_str(s),
            _ => self.runs.push(Run { text: s.to_string(), style }),
        }
    }

    /// What's been gathered, trimmed, leaving this empty. `None` if it was only space.
    fn take(&mut self) -> Option<Vec<Run>> {
        let mut runs = std::mem::take(&mut self.runs);
        while runs.last().is_some_and(|r| r.text.trim_end().is_empty()) {
            runs.pop();
        }
        if let Some(last) = runs.last_mut() {
            last.text.truncate(last.text.trim_end().len());
        }
        (!runs.is_empty()).then_some(runs)
    }
}

/// Everything inside an element that reads as one line of text, in `style`.
fn inline(nodes: &[Node], style: Style, line: &mut Line) {
    for node in nodes {
        match node {
            Node::Text(t) => line.text(t, style),
            Node::Element { tag, attrs, children } => match tag.as_str() {
                "br" => line.raw("\n", style),
                "img" => {
                    if let Some(alt) = attr(attrs, "alt").map(str::trim).filter(|a| !a.is_empty()) {
                        line.text(alt, style | IMAGE);
                    }
                }
                "b" | "strong" => inline(children, style | BOLD, line),
                "i" | "em" | "cite" | "var" | "dfn" => inline(children, style | ITALIC, line),
                "code" | "kbd" | "samp" | "tt" => inline(children, style | CODE, line),
                "s" | "del" | "strike" => inline(children, style | STRIKE, line),
                "a" => inline(children, style | LINK, line),
                t if is_hidden(t) => {}
                _ => inline(children, style, line),
            },
        }
    }
}

/// An element's text exactly as written, for code.
fn plain(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Element { tag, children, .. } => {
                if tag == "br" {
                    out.push('\n');
                }
                plain(children, out);
            }
        }
    }
}

fn runs_of(nodes: &[Node], style: Style) -> Vec<Run> {
    let mut line = Line::default();
    inline(nodes, style, &mut line);
    line.take().unwrap_or_default()
}

/// Whether anything in here starts a block of its own.
fn has_blocks(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| matches!(n, Node::Element { tag, children, .. } if is_block(tag) || (!is_hidden(tag) && has_blocks(children))))
}

fn blocks_of(nodes: &[Node], out: &mut Vec<Block>, tight: bool) {
    let mut line = Line::default();
    let flush = |line: &mut Line, out: &mut Vec<Block>| {
        if let Some(runs) = line.take() {
            out.push(Block::Paragraph(runs));
        }
    };
    for node in nodes {
        let Node::Element { tag, attrs, children } = node else {
            if let Node::Text(t) = node {
                line.text(t, 0);
            }
            continue;
        };
        match tag.as_str() {
            t if is_hidden(t) => {}
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                flush(&mut line, out);
                let runs = runs_of(children, 0);
                if !runs.is_empty() {
                    out.push(Block::Heading(tag.as_bytes()[1] - b'0', runs));
                }
            }
            "hr" => {
                flush(&mut line, out);
                out.push(Block::Rule);
            }
            "pre" => {
                flush(&mut line, out);
                let mut code = String::new();
                plain(children, &mut code);
                let code = code.trim_matches('\n');
                if !code.trim().is_empty() {
                    out.push(Block::Code(String::new(), code.to_string()));
                }
            }
            "blockquote" => {
                flush(&mut line, out);
                let mut inner = Vec::new();
                blocks_of(children, &mut inner, false);
                if !inner.is_empty() {
                    out.push(Block::Quote(inner));
                }
            }
            "ul" | "ol" | "menu" => {
                flush(&mut line, out);
                let mut number = attr(attrs, "start").and_then(|s| s.parse::<i64>().ok()).unwrap_or(1);
                let mut items = Vec::new();
                for child in children {
                    let Node::Element { tag: li, children: inside, .. } = child else { continue };
                    if li != "li" {
                        continue;
                    }
                    let mut blocks = Vec::new();
                    blocks_of(inside, &mut blocks, true);
                    // A tick box at the start of an item makes it a task.
                    let task = inside.iter().find_map(|n| match n {
                        Node::Element { tag, attrs, .. } if tag == "input" && attr(attrs, "type") == Some("checkbox") => Some(attr(attrs, "checked").is_some()),
                        _ => None,
                    });
                    let marker = if tag == "ol" { format!("{number}.") } else { "•".to_string() };
                    number += 1;
                    items.push(Item { marker, task, blocks });
                }
                if !items.is_empty() {
                    out.push(Block::List(items));
                }
            }
            "table" => {
                flush(&mut line, out);
                if let Some(table) = table(children) {
                    out.push(table);
                }
            }
            "br" => line.raw("\n", 0),
            // A box of other things: its own paragraph if it's only text, else whatever it holds.
            t if is_block(t) || has_blocks(children) => {
                flush(&mut line, out);
                if has_blocks(children) {
                    blocks_of(children, out, tight);
                } else {
                    let runs = runs_of(children, 0);
                    if !runs.is_empty() {
                        out.push(Block::Paragraph(runs));
                    }
                }
            }
            _ => inline(std::slice::from_ref(node), 0, &mut line),
        }
    }
    flush(&mut line, out);
}

fn table(nodes: &[Node]) -> Option<Block> {
    // Rows, wherever they are under the table, each with whether it's a heading row.
    fn rows(nodes: &[Node], in_head: bool, out: &mut Vec<(bool, Vec<Vec<Run>>)>) {
        for node in nodes {
            let Node::Element { tag, children, .. } = node else { continue };
            match tag.as_str() {
                "thead" => rows(children, true, out),
                "tbody" | "tfoot" => rows(children, false, out),
                "tr" => {
                    let mut head = in_head;
                    let mut cells = Vec::new();
                    for cell in children {
                        let Node::Element { tag, children, .. } = cell else { continue };
                        if tag == "th" || tag == "td" {
                            head |= tag == "th" && cells.is_empty();
                            cells.push(runs_of(children, 0));
                        }
                    }
                    if !cells.is_empty() {
                        out.push((head, cells));
                    }
                }
                _ => {}
            }
        }
    }
    let mut all = Vec::new();
    rows(nodes, false, &mut all);
    if all.is_empty() {
        return None;
    }
    let width = all.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
    for (_, cells) in &mut all {
        cells.resize(width, Vec::new());
    }
    // The first row heads the table whether or not the page says so: a table needs a head.
    let head = all.remove(0).1;
    Some(Block::Table { align: vec![Align::Left; width], head, rows: all.into_iter().map(|(_, c)| c).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(runs: &[Run]) -> String {
        runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn headings_and_paragraphs() {
        let b = to_blocks("<html><head><title>T</title><style>p { x: y }</style></head><body><h1>Hello</h1><p>One\n  two <b>three</b>.</p><p>Next</p></body></html>");
        assert_eq!(b.len(), 3);
        assert!(matches!(&b[0], Block::Heading(1, r) if text(r) == "Hello"));
        let Block::Paragraph(r) = &b[1] else { panic!() };
        assert_eq!(text(r), "One two three.");
        assert_eq!(r[1], Run { text: "three".into(), style: BOLD });
        assert!(matches!(&b[2], Block::Paragraph(r) if text(r) == "Next"));
    }

    #[test]
    fn scripts_comments_and_entities() {
        let b = to_blocks("<p>a &amp; b &lt;c&gt; &#65;&#x42; &copy; &unknown; x<!-- gone --></p><script>if (a < b) { document.write('<p>no</p>') }</script><p>after</p>");
        assert_eq!(b.len(), 2);
        assert!(matches!(&b[0], Block::Paragraph(r) if text(r) == "a & b <c> AB © &unknown; x"));
        assert!(matches!(&b[1], Block::Paragraph(r) if text(r) == "after"));
    }

    #[test]
    fn lists_with_unclosed_items_and_nesting() {
        let b = to_blocks("<ul><li>One<li>Two<ul><li>Inner</ul><li>Three</ul><ol start=3><li>c</li><li>d</li></ol>");
        let Block::List(items) = &b[0] else { panic!("{b:?}") };
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].marker, "•");
        assert!(matches!(&items[1].blocks[..], [Block::Paragraph(r), Block::List(inner)] if text(r) == "Two" && inner.len() == 1));
        let Block::List(numbered) = &b[1] else { panic!() };
        assert_eq!((numbered[0].marker.as_str(), numbered[1].marker.as_str()), ("3.", "4."));
    }

    #[test]
    fn tables_code_quotes_and_rules() {
        let b = to_blocks("<table><thead><tr><th>A<th>B</thead><tr><td>1</td><td>2</td></tr><tr><td>3</td></tr></table><pre>fn  x() {\n  1\n}</pre><blockquote><p>Said</p></blockquote><hr>");
        let Block::Table { head, rows, align } = &b[0] else { panic!("{b:?}") };
        assert_eq!((head.len(), rows.len(), align.len()), (2, 2, 2));
        assert_eq!(text(&rows[0][1]), "2");
        assert!(rows[1][1].is_empty());
        assert!(matches!(&b[1], Block::Code(_, c) if c == "fn  x() {\n  1\n}"));
        assert!(matches!(&b[2], Block::Quote(q) if q.len() == 1));
        assert_eq!(b[3], Block::Rule);
    }

    #[test]
    fn links_images_and_line_breaks() {
        let b = to_blocks("<div>See <a href='x'>this <em>page</em></a><br>and <img src=a.png alt='a cat'> here</div>");
        let Block::Paragraph(r) = &b[0] else { panic!("{b:?}") };
        assert_eq!(text(r), "See this page\nand a cat here");
        assert!(r.iter().any(|x| x.text == "page" && x.style == LINK | ITALIC));
        assert!(r.iter().any(|x| x.text == "a cat" && x.style == IMAGE));
    }

    #[test]
    fn nonsense_is_read_not_refused() {
        for s in ["", "<", "<<<>>>", "</p></div>", "<p", "<a href=", "<p>open <b>never closed", "a < b and c > d", &"<div>".repeat(5000), "<table><td>lone</table>", "&#99999999999;&#xZZ;&"] {
            let _ = to_blocks(s);
        }
        assert!(matches!(&to_blocks("<p>open <b>never closed")[0], Block::Paragraph(r) if text(r) == "open never closed"));
        assert!(matches!(&to_blocks("a < b and c > d")[0], Block::Paragraph(r) if text(r) == "a < b and c > d"));
    }
}
