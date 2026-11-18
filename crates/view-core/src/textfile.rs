//! Text files for the preview: telling text from binary, decoding it, tidying minified JSON,
//! splitting CSV into cells, and colouring code. All of it works on what's already been read,
//! and none of it needs to be exact: it only decides how a preview looks.

use crate::kind::Lang;

/// The most of a file the preview reads.
pub const MAX_BYTES: usize = 1 << 20;
/// The most lines it shows.
pub const MAX_LINES: usize = 20_000;

/// Whether the start of a file looks like text: no NUL bytes (outside UTF-16), and hardly any
/// control characters.
pub fn looks_like_text(head: &[u8]) -> bool {
    if head.is_empty() {
        return true;
    }
    if head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]) {
        return true;
    }
    if head.contains(&0) {
        return false;
    }
    let controls = head.iter().filter(|&&b| b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0C | 0x1B)).count();
    controls * 100 <= head.len()
}

/// Decode a file's bytes as text: UTF-8 or UTF-16 by their byte-order marks, UTF-8 if it is,
/// and Windows-1252 otherwise. Line ends come back as `\n`. `None` if it isn't text at all.
pub fn decode(bytes: &[u8], cut: bool) -> Option<String> {
    let text = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        utf8_lossy_tail(rest)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        utf16(rest, u16::from_le_bytes)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        utf16(rest, u16::from_be_bytes)
    } else {
        if !looks_like_text(&bytes[..bytes.len().min(8192)]) {
            return None;
        }
        match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            // Cut off mid-character by the read limit: still UTF-8.
            Err(e) if cut && e.error_len().is_none() && bytes.len() - e.valid_up_to() < 4 => String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned(),
            Err(_) => bytes.iter().map(|&b| windows_1252(b)).collect(),
        }
    };
    Some(normalise_lines(&text))
}

fn utf8_lossy_tail(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(e) if e.error_len().is_none() => String::from_utf8_lossy(&b[..e.valid_up_to()]).into_owned(),
        Err(_) => String::from_utf8_lossy(b).into_owned(),
    }
}

fn utf16(b: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| unit([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

fn windows_1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

fn normalise_lines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Keep at most `max_lines` lines. Returns the text and whether any were cut.
pub fn first_lines(s: String, max_lines: usize) -> (String, bool) {
    match s.match_indices('\n').nth(max_lines.saturating_sub(1)) {
        Some((at, _)) if at + 1 < s.len() => (s[..at].to_string(), true),
        _ => (s, false),
    }
}

// ---- JSON ----

/// Lay minified JSON out over lines, indented by two spaces, keeping everything else as it
/// was. `None` if it's already laid out, or doesn't look like JSON.
pub fn pretty_json(s: &str) -> Option<String> {
    // Long lines, or everything on one line that's more than a scrap, mean it was squeezed.
    let longest = s.lines().map(str::len).max().unwrap_or(0);
    let one_line = s.trim().lines().count() == 1;
    if longest < 100 && !(one_line && longest >= 60) {
        return None;
    }
    let t = s.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    let mut out = String::with_capacity(s.len() + s.len() / 4);
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    let bytes = s.as_bytes();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    let mut i = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        i = at;
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                // An empty object or list stays on one line.
                let close = if c == '{' { b'}' } else { b']' };
                let mut j = at + 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if bytes.get(j) == Some(&close) {
                    out.push(close as char);
                    while chars.peek().is_some_and(|&(k, _)| k <= j) {
                        chars.next();
                    }
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.checked_sub(1)?;
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            '/' if bytes.get(at + 1).is_some_and(|&b| b == b'/' || b == b'*') => return None, // comments: leave alone
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    let _ = i;
    (depth == 0 && !in_string).then_some(out)
}

// ---- Tables ----

/// The likeliest separator for a CSV file: whichever of `,` `;` and tab is commonest in its
/// first line, outside quotes.
pub fn sniff_delimiter(s: &str) -> u8 {
    let first = s.lines().next().unwrap_or("");
    let mut counts = [0usize; 3];
    let mut quoted = false;
    for b in first.bytes() {
        match b {
            b'"' => quoted = !quoted,
            b',' if !quoted => counts[0] += 1,
            b';' if !quoted => counts[1] += 1,
            b'\t' if !quoted => counts[2] += 1,
            _ => {}
        }
    }
    let best = (0..3).max_by_key(|&i| (counts[i], 3 - i)).unwrap_or(0);
    [b',', b';', b'\t'][best]
}

/// Split delimited text into rows of cells, with quoted cells (`"a, b"`, `""` for a quote mark)
/// as spreadsheets write them. Returns the rows and whether any were left out.
pub fn parse_table(s: &str, delim: u8, max_rows: usize, max_cols: usize) -> (Vec<Vec<String>>, bool) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = s.chars().peekable();
    let d = delim as char;
    let mut cut = false;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if cell.is_empty() => quoted = true,
            c if c == d => {
                if row.len() < max_cols {
                    row.push(std::mem::take(&mut cell));
                } else {
                    cell.clear();
                    cut = true;
                }
            }
            '\n' => {
                if row.len() < max_cols {
                    row.push(std::mem::take(&mut cell));
                }
                cell.clear();
                rows.push(std::mem::take(&mut row));
                if rows.len() >= max_rows {
                    return (rows, chars.peek().is_some() || cut);
                }
            }
            c => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    (rows, cut)
}

// ---- Code ----

/// What a stretch of code is, for colouring it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Token {
    Comment,
    String,
    Number,
    Keyword,
    Type,
    Function,
    /// Markup tags and CSS selectors.
    Tag,
    /// Attributes, CSS properties and shell variables.
    Attr,
    /// Keys in JSON, YAML and INI files.
    Key,
    /// INI sections and diff hunk headers.
    Heading,
    Added,
    Removed,
}

/// A coloured stretch, by byte offsets into the text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub token: Token,
}

struct Rules {
    line: &'static [&'static str],
    block: &'static [(&'static str, &'static str)],
    /// Quote marks, and whether strings in them may run over several lines.
    quotes: &'static [(u8, bool)],
    triple: bool,
    lang: Lang,
    keywords: &'static [&'static str],
    /// Keywords match whatever their case.
    caseless: bool,
    /// Capitalised names are types.
    types: bool,
    /// `$name` is a variable.
    dollars: bool,
    /// `@name` is an attribute or decorator.
    ats: bool,
    /// `#word` at the start of a line is a directive.
    directives: bool,
}

const fn rules(lang: Lang, keywords: &'static [&'static str]) -> Rules {
    Rules { line: &["//"], block: &[("/*", "*/")], quotes: &[(b'"', false), (b'\'', false)], triple: false, lang, keywords, caseless: false, types: true, dollars: false, ats: false, directives: false }
}

const RUST: &[&str] = &["as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "union", "macro_rules"];
const C_LIKE: &[&str] = &[
    "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum", "extern", "float", "for", "goto", "if", "inline", "int", "long", "register", "restrict", "return", "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union", "unsigned", "void", "volatile", "while", "bool", "true", "false", "nullptr", "NULL", "class", "namespace", "template", "typename", "public", "private", "protected", "virtual", "override", "final", "new", "delete", "this", "using", "operator", "friend", "explicit", "constexpr", "noexcept", "try", "catch", "throw", "auto", "self", "nil", "YES", "NO", "fn", "pub", "var", "comptime", "defer", "errdefer", "uniform", "in", "out",
];
const CSHARP: &[&str] = &[
    "abstract", "as", "base", "bool", "break", "byte", "case", "catch", "char", "checked", "class", "const", "continue", "decimal", "default", "delegate", "do", "double", "else", "enum", "event", "explicit", "extern", "false", "finally", "fixed", "float", "for", "foreach", "goto", "if", "implicit", "in", "int", "interface", "internal", "is", "lock", "long", "namespace", "new", "null", "object", "operator", "out", "override", "params", "private", "protected", "public", "readonly", "ref", "return", "sbyte", "sealed", "short", "sizeof", "stackalloc", "static", "string", "struct", "switch", "this", "throw", "true", "try", "typeof", "uint", "ulong", "unchecked", "unsafe", "ushort", "using", "virtual", "void", "volatile", "while", "async", "await", "var", "get", "set", "init", "record", "yield", "partial", "where", "when", "nameof", "dynamic", "value", "global",
];
const JAVA: &[&str] = &[
    "abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "const", "continue", "default", "do", "double", "else", "enum", "extends", "final", "finally", "float", "for", "if", "implements", "import", "instanceof", "int", "interface", "long", "native", "new", "package", "private", "protected", "public", "return", "short", "static", "super", "switch", "synchronized", "this", "throw", "throws", "transient", "try", "void", "volatile", "while", "true", "false", "null", "var", "record", "sealed", "yield", "def", "val", "object", "trait", "match", "with", "lazy", "implicit", "override", "type", "late", "required", "async", "await", "dynamic",
];
const KOTLIN: &[&str] = &[
    "as", "break", "class", "continue", "do", "else", "false", "for", "fun", "if", "in", "interface", "is", "null", "object", "package", "return", "super", "this", "throw", "true", "try", "typealias", "val", "var", "when", "while", "by", "catch", "constructor", "finally", "get", "import", "init", "set", "where", "abstract", "annotation", "companion", "const", "data", "enum", "final", "inline", "inner", "internal", "lateinit", "open", "operator", "out", "override", "private", "protected", "public", "sealed", "suspend", "vararg",
];
const SWIFT: &[&str] = &[
    "associatedtype", "class", "deinit", "enum", "extension", "fileprivate", "func", "import", "init", "inout", "internal", "let", "open", "operator", "private", "protocol", "public", "rethrows", "static", "struct", "subscript", "typealias", "var", "break", "case", "continue", "default", "defer", "do", "else", "fallthrough", "for", "guard", "if", "in", "repeat", "return", "switch", "where", "while", "as", "catch", "false", "is", "nil", "super", "self", "Self", "throw", "throws", "true", "try", "async", "await", "some", "any", "final", "override", "mutating", "lazy", "weak", "required",
];
const GO: &[&str] = &["break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range", "return", "select", "struct", "switch", "type", "var", "true", "false", "nil", "iota"];
const SCRIPT: &[&str] = &[
    "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do", "else", "export", "extends", "false", "finally", "for", "function", "if", "import", "in", "instanceof", "new", "null", "return", "super", "switch", "this", "throw", "true", "try", "typeof", "var", "void", "while", "with", "yield", "let", "static", "async", "await", "of", "undefined", "as", "from", "get", "set", "type", "interface", "enum", "implements", "private", "protected", "public", "readonly", "declare", "namespace", "abstract", "keyof", "infer", "is", "never", "unknown", "any", "number", "string", "boolean", "symbol", "bigint", "satisfies",
];
const PYTHON: &[&str] = &["False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield", "match", "case", "self"];
const RUBY: &[&str] = &["alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else", "elsif", "end", "ensure", "false", "for", "if", "in", "module", "next", "nil", "not", "or", "redo", "rescue", "retry", "return", "self", "super", "then", "true", "undef", "unless", "until", "when", "while", "yield", "require", "attr_accessor", "attr_reader", "attr_writer", "private", "protected", "public", "lambda", "proc"];
const SHELL: &[&str] = &["if", "then", "else", "elif", "fi", "case", "esac", "for", "while", "until", "do", "done", "in", "function", "select", "return", "exit", "export", "local", "readonly", "declare", "unset", "shift", "source", "alias", "echo", "cd", "set", "trap", "eval", "exec", "true", "false"];
const POWERSHELL: &[&str] = &["begin", "break", "catch", "class", "continue", "data", "do", "dynamicparam", "else", "elseif", "end", "enum", "exit", "filter", "finally", "for", "foreach", "from", "function", "hidden", "if", "in", "param", "process", "return", "static", "switch", "throw", "trap", "try", "until", "using", "while"];
const BATCH: &[&str] = &["echo", "set", "if", "else", "goto", "call", "for", "in", "do", "exit", "not", "exist", "defined", "setlocal", "endlocal", "enabledelayedexpansion", "errorlevel", "pause", "shift", "start", "cd", "chdir", "pushd", "popd", "copy", "del", "move", "mkdir", "md", "rmdir", "rd", "type", "title", "cls", "equ", "neq", "lss", "leq", "gtr", "geq", "nul", "choice", "off", "on"];
const LUA: &[&str] = &["and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while", "self", "require"];
const SQL: &[&str] = &[
    "select", "from", "where", "insert", "into", "values", "update", "set", "delete", "create", "table", "drop", "alter", "add", "column", "index", "view", "join", "inner", "left", "right", "outer", "full", "cross", "on", "as", "and", "or", "not", "null", "is", "in", "like", "between", "exists", "distinct", "group", "by", "order", "having", "limit", "offset", "union", "all", "case", "when", "then", "else", "end", "primary", "key", "foreign", "references", "default", "unique", "check", "constraint", "begin", "commit", "rollback", "transaction", "int", "integer", "varchar", "char", "text", "date", "datetime", "timestamp", "boolean", "bool", "float", "double", "decimal", "numeric", "serial", "bigint", "smallint", "if", "asc", "desc", "with", "returning", "true", "false", "procedure", "function", "returns", "declare", "trigger", "database", "schema", "grant", "revoke", "count", "sum", "avg", "min", "max",
];
const GENERIC: &[&str] = &["if", "else", "elif", "elsif", "for", "foreach", "while", "do", "return", "function", "func", "fn", "fun", "def", "sub", "class", "struct", "import", "from", "export", "let", "var", "const", "val", "true", "false", "null", "nil", "none", "end", "module", "public", "private", "static", "void", "new", "this", "self", "use", "package", "type", "match", "case", "switch", "break", "continue", "try", "catch", "throw", "finally", "async", "await", "and", "or", "not", "in", "is", "then", "begin", "local", "my", "our"];

fn rules_for(lang: Lang) -> Rules {
    use Lang::*;
    match lang {
        Rust => Rules { quotes: &[(b'"', true)], ats: false, ..rules(lang, RUST) },
        C => Rules { directives: true, ats: true, ..rules(lang, C_LIKE) },
        CSharp => Rules { quotes: &[(b'"', false), (b'\'', false)], directives: true, ..rules(lang, CSHARP) },
        Java => Rules { ats: true, quotes: &[(b'"', false), (b'\'', false), (b'`', true)], ..rules(lang, JAVA) },
        Kotlin => Rules { ats: true, dollars: true, ..rules(lang, KOTLIN) },
        Swift => Rules { ats: true, ..rules(lang, SWIFT) },
        Go => Rules { quotes: &[(b'"', false), (b'\'', false), (b'`', true)], ..rules(lang, GO) },
        Script => Rules { quotes: &[(b'"', false), (b'\'', false), (b'`', true)], ats: true, ..rules(lang, SCRIPT) },
        Python => Rules { line: &["#"], block: &[], triple: true, ats: true, types: true, ..rules(lang, PYTHON) },
        Ruby => Rules { line: &["#"], block: &[("=begin", "=end")], ats: true, dollars: true, ..rules(lang, RUBY) },
        Shell => Rules { line: &["#"], block: &[], quotes: &[(b'"', true), (b'\'', true), (b'`', true)], dollars: true, types: false, ..rules(lang, SHELL) },
        PowerShell => Rules { line: &["#"], block: &[("<#", "#>")], quotes: &[(b'"', true), (b'\'', true)], caseless: true, dollars: true, types: false, ..rules(lang, POWERSHELL) },
        Batch => Rules { line: &["::", "rem ", "REM ", "Rem "], block: &[], quotes: &[(b'"', false)], caseless: true, types: false, ..rules(lang, BATCH) },
        Lua => Rules { line: &["--"], block: &[("--[[", "]]")], quotes: &[(b'"', false), (b'\'', false)], types: false, ..rules(lang, LUA) },
        Sql => Rules { line: &["--", "#"], quotes: &[(b'\'', true), (b'"', false), (b'`', false)], caseless: true, types: false, ..rules(lang, SQL) },
        Generic => Rules { line: &["//", "#", "--", ";"], dollars: true, types: false, ..rules(lang, GENERIC) },
        Markup | Css | Yaml | Ini | Diff => rules(lang, &[]),
    }
}

/// Colour code. Spans are in order and never overlap.
pub fn highlight(text: &str, lang: Lang) -> Vec<Span> {
    match lang {
        Lang::Markup => markup(text),
        Lang::Css => css(text),
        Lang::Yaml => yaml(text),
        Lang::Ini => ini(text),
        Lang::Diff => diff(text),
        _ => code(text, &rules_for(lang)),
    }
}

/// Colour JSON: keys apart from the strings they name.
pub fn highlight_json(text: &str) -> Vec<Span> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            let end = string_end(b, i, b'"', true);
            let mut j = end;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            out.push(Span { start: i, end, token: if b.get(j) == Some(&b':') { Token::Key } else { Token::String } });
            i = end;
        } else if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = line_end(b, i);
            out.push(Span { start: i, end, token: Token::Comment });
            i = end;
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let end = find(b, i + 2, b"*/").map_or(b.len(), |k| k + 2);
            out.push(Span { start: i, end, token: Token::Comment });
            i = end;
        } else if c == b'-' || c.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'.' | b'+' | b'-')) {
                i += 1;
            }
            out.push(Span { start, end: i, token: Token::Number });
        } else if c.is_ascii_alphabetic() {
            let start = i;
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            if matches!(&text[start..i], "true" | "false" | "null") {
                out.push(Span { start, end: i, token: Token::Keyword });
            }
        } else {
            i += 1;
        }
    }
    out
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn line_end(b: &[u8], from: usize) -> usize {
    b[from..].iter().position(|&c| c == b'\n').map_or(b.len(), |k| from + k)
}

fn find(b: &[u8], from: usize, what: &[u8]) -> Option<usize> {
    if from >= b.len() || what.is_empty() {
        return None;
    }
    b[from..].windows(what.len()).position(|w| w == what).map(|k| from + k)
}

/// Where a string that opens at `from` ends (just past its closing quote).
fn string_end(b: &[u8], from: usize, quote: u8, multiline: bool) -> usize {
    let mut i = from + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            b'\n' if !multiline => return i,
            _ => i += 1,
        }
    }
    b.len()
}

fn starts_with_at(b: &[u8], i: usize, s: &str) -> bool {
    b.len() >= i + s.len() && &b[i..i + s.len()] == s.as_bytes()
}

fn code(text: &str, r: &Rules) -> Vec<Span> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    'scan: while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if c == b' ' || c == b'\t' {
            i += 1;
            continue;
        }
        let at_line_start = line_start;
        line_start = false;
        for &(open, close) in r.block {
            if starts_with_at(b, i, open) && (open != "=begin" || at_line_start) {
                let end = find(b, i + open.len(), close.as_bytes()).map_or(b.len(), |k| k + close.len());
                out.push(Span { start: i, end, token: Token::Comment });
                i = end;
                continue 'scan;
            }
        }
        for &l in r.line {
            // A `#` comment in the shell has to start a word; "rem" only starts a line.
            let word_start = i == 0 || !is_ident(b[i - 1]);
            let ok = match l {
                "#" => word_start && !(r.directives && at_line_start),
                "rem " | "REM " | "Rem " | "::" => at_line_start,
                ";" => at_line_start,
                _ => true,
            };
            if ok && starts_with_at(b, i, l) {
                let end = line_end(b, i);
                out.push(Span { start: i, end, token: Token::Comment });
                i = end;
                continue 'scan;
            }
        }
        if r.directives && c == b'#' && at_line_start {
            let mut j = i + 1;
            while j < b.len() && (b[j] == b' ' || b[j].is_ascii_alphabetic()) && !(b[j] == b' ' && j > i + 1 && b[j - 1] != b' ') {
                j += 1;
            }
            out.push(Span { start: i, end: j, token: Token::Keyword });
            // `#include <stdio.h>`
            let mut k = j;
            while k < b.len() && b[k] == b' ' {
                k += 1;
            }
            if b.get(k) == Some(&b'<') {
                let end = b[k..].iter().position(|&x| x == b'>' || x == b'\n').map_or(b.len(), |p| k + p + 1).min(b.len());
                out.push(Span { start: k, end, token: Token::String });
                i = end;
            } else {
                i = j;
            }
            continue;
        }
        if r.triple && (starts_with_at(b, i, "\"\"\"") || starts_with_at(b, i, "'''")) {
            let q = &text[i..i + 3];
            let end = find(b, i + 3, q.as_bytes()).map_or(b.len(), |k| k + 3);
            out.push(Span { start: i, end, token: Token::String });
            i = end;
            continue;
        }
        if let Some(&(q, multi)) = r.quotes.iter().find(|(q, _)| *q == c) {
            // A Rust lifetime ('a) isn't a character.
            if c == b'\'' && r.lang == Lang::Rust && !(b.get(i + 2) == Some(&b'\'') || b.get(i + 1) == Some(&b'\\')) {
                i += 1;
                continue;
            }
            let end = string_end(b, i, q, multi);
            out.push(Span { start: i, end, token: Token::String });
            i = end;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            if i > 0 && is_ident(b[i - 1]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && (is_ident(b[i]) || b[i] == b'.') {
                if b[i] == b'.' && !b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    break;
                }
                i += 1;
            }
            out.push(Span { start, end: i, token: Token::Number });
            continue;
        }
        if (r.dollars && c == b'$') || (r.ats && c == b'@') || (r.lang == Lang::Batch && c == b'%') {
            let start = i;
            i += 1;
            if c == b'$' && b.get(i) == Some(&b'{') {
                i = b[i..].iter().position(|&x| x == b'}' || x == b'\n').map_or(b.len(), |p| i + p + 1).min(b.len());
            } else {
                while i < b.len() && (is_ident(b[i]) || (c == b'@' && b[i] == b'.')) {
                    i += 1;
                }
                if c == b'%' && b.get(i) == Some(&b'%') {
                    i += 1;
                }
            }
            if i > start + 1 {
                out.push(Span { start, end: i, token: if c == b'@' { Token::Type } else { Token::Attr } });
            }
            continue;
        }
        if is_ident(c) {
            let start = i;
            let hyphens = r.lang == Lang::PowerShell;
            while i < b.len() && (is_ident(b[i]) || (hyphens && b[i] == b'-' && b.get(i + 1).is_some_and(|x| x.is_ascii_alphabetic()))) {
                i += 1;
            }
            // Rust macros: `println!`
            if r.lang == Lang::Rust && b.get(i) == Some(&b'!') && b.get(i + 1) != Some(&b'=') {
                out.push(Span { start, end: i + 1, token: Token::Function });
                i += 1;
                continue;
            }
            let word = &text[start..i];
            let keyword = if r.caseless { r.keywords.iter().any(|k| k.eq_ignore_ascii_case(word)) } else { r.keywords.contains(&word) };
            let token = if keyword {
                Some(Token::Keyword)
            } else if b.get(i) == Some(&b'(') {
                Some(Token::Function)
            } else if hyphens && word.contains('-') {
                Some(Token::Function)
            } else if r.types && word.as_bytes()[0].is_ascii_uppercase() && word.len() > 1 {
                Some(Token::Type)
            } else {
                None
            };
            if let Some(token) = token {
                out.push(Span { start, end: i, token });
            }
            continue;
        }
        if r.lang == Lang::PowerShell && c == b'-' && b.get(i + 1).is_some_and(|x| x.is_ascii_alphabetic()) && (i == 0 || b[i - 1] == b' ') {
            let start = i;
            i += 1;
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            out.push(Span { start, end: i, token: Token::Attr });
            continue;
        }
        i += 1;
    }
    out
}

fn markup(text: &str) -> Vec<Span> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if starts_with_at(b, i, "<!--") {
            let end = find(b, i + 4, b"-->").map_or(b.len(), |k| k + 3);
            out.push(Span { start: i, end, token: Token::Comment });
            i = end;
        } else if starts_with_at(b, i, "<![CDATA[") {
            let end = find(b, i, b"]]>").map_or(b.len(), |k| k + 3);
            out.push(Span { start: i, end, token: Token::String });
            i = end;
        } else if b[i] == b'<' && b.get(i + 1).is_some_and(|&x| x.is_ascii_alphabetic() || matches!(x, b'/' | b'!' | b'?')) {
            let start = i;
            i += 1;
            while i < b.len() && (is_ident(b[i]) || matches!(b[i], b'/' | b'!' | b'?' | b':' | b'-' | b'.')) {
                i += 1;
            }
            out.push(Span { start, end: i, token: Token::Tag });
            // Attributes until the tag closes.
            while i < b.len() && b[i] != b'>' {
                let c = b[i];
                if c == b'"' || c == b'\'' {
                    let end = string_end(b, i, c, true);
                    out.push(Span { start: i, end, token: Token::String });
                    i = end;
                } else if is_ident(c) {
                    let s = i;
                    while i < b.len() && (is_ident(b[i]) || matches!(b[i], b':' | b'-' | b'.' | b'@')) {
                        i += 1;
                    }
                    out.push(Span { start: s, end: i, token: Token::Attr });
                } else if c == b'/' || c == b'?' {
                    let s = i;
                    i += 1;
                    if b.get(i) == Some(&b'>') {
                        out.push(Span { start: s, end: i + 1, token: Token::Tag });
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if b.get(i) == Some(&b'>') {
                out.push(Span { start: i, end: i + 1, token: Token::Tag });
                i += 1;
            }
        } else if b[i] == b'&' {
            let s = i;
            let end = b[i..].iter().take(12).position(|&x| x == b';').map(|p| i + p + 1);
            match end {
                Some(e) if b[s + 1..e - 1].iter().all(|&x| x.is_ascii_alphanumeric() || x == b'#') => {
                    out.push(Span { start: s, end: e, token: Token::Number });
                    i = e;
                }
                _ => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out
}

fn css(text: &str) -> Vec<Span> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if starts_with_at(b, i, "/*") {
            let end = find(b, i + 2, b"*/").map_or(b.len(), |k| k + 2);
            out.push(Span { start: i, end, token: Token::Comment });
            i = end;
        } else if starts_with_at(b, i, "//") && (i == 0 || b[i - 1] != b':') {
            let end = line_end(b, i);
            out.push(Span { start: i, end, token: Token::Comment });
            i = end;
        } else if c == b'"' || c == b'\'' {
            let end = string_end(b, i, c, false);
            out.push(Span { start: i, end, token: Token::String });
            i = end;
        } else if c == b'{' {
            depth += 1;
            i += 1;
        } else if c == b'}' {
            depth = depth.saturating_sub(1);
            i += 1;
        } else if c == b'@' || (c == b'$' && depth == 0) {
            let s = i;
            i += 1;
            while i < b.len() && (is_ident(b[i]) || b[i] == b'-') {
                i += 1;
            }
            out.push(Span { start: s, end: i, token: Token::Keyword });
        } else if c == b'#' && depth > 0 {
            let s = i;
            i += 1;
            while i < b.len() && b[i].is_ascii_hexdigit() {
                i += 1;
            }
            out.push(Span { start: s, end: i, token: Token::Number });
        } else if c.is_ascii_digit() || (c == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit) && depth > 0) {
            let s = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'%') {
                i += 1;
            }
            if depth > 0 {
                out.push(Span { start: s, end: i, token: Token::Number });
            }
        } else if is_ident(c) || c == b'-' || ((c == b'.' || c == b'#' || c == b':') && depth == 0) {
            let s = i;
            i += 1;
            while i < b.len() && (is_ident(b[i]) || b[i] == b'-') {
                i += 1;
            }
            let mut j = i;
            while j < b.len() && b[j] == b' ' {
                j += 1;
            }
            // Inside a rule, `name:` is a property; outside, words make up the selector.
            if depth > 0 && b.get(j) == Some(&b':') && !b.get(j + 1).is_some_and(|x| x.is_ascii_alphabetic() && depth > 1) {
                out.push(Span { start: s, end: i, token: Token::Attr });
            } else if depth == 0 || b.get(j) == Some(&b'{') {
                out.push(Span { start: s, end: i, token: Token::Tag });
            } else if b.get(i) == Some(&b'(') {
                out.push(Span { start: s, end: i, token: Token::Function });
            }
        } else {
            i += 1;
        }
    }
    out
}

fn scalar(text: &str, start: usize, end: usize, out: &mut Vec<Span>) {
    let v = text[start..end].trim();
    if v.is_empty() {
        return;
    }
    let lead = start + text[start..end].find(v).unwrap_or(0);
    let token = if v.starts_with('"') || v.starts_with('\'') {
        Token::String
    } else if v.parse::<f64>().is_ok() || v.starts_with("0x") {
        Token::Number
    } else if matches!(v.to_ascii_lowercase().as_str(), "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~") {
        Token::Keyword
    } else {
        return;
    };
    out.push(Span { start: lead, end: lead + v.len(), token });
}

/// A `#` comment that starts a line or follows a space: where it starts in `line`.
fn hash_comment(line: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '#') if i == 0 || line[..i].ends_with([' ', '\t']) => return Some(i),
            _ => {}
        }
    }
    None
}

fn yaml(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in text.split('\n') {
        let comment = hash_comment(line);
        let body = &line[..comment.unwrap_or(line.len())];
        let trimmed = body.trim_start();
        let indent = body.len() - trimmed.len();
        let mut content = trimmed;
        let mut offset = indent;
        if let Some(rest) = content.strip_prefix("- ") {
            offset += 2;
            content = rest;
        }
        if trimmed == "---" || trimmed == "..." {
            out.push(Span { start: at + indent, end: at + indent + 3, token: Token::Comment });
        } else if let Some(colon) = key_colon(content) {
            out.push(Span { start: at + offset, end: at + offset + colon, token: Token::Key });
            scalar(text, at + offset + colon + 1, at + body.len(), &mut out);
        } else {
            scalar(text, at + offset, at + body.len(), &mut out);
        }
        if let Some(c) = comment {
            out.push(Span { start: at + c, end: at + line.len(), token: Token::Comment });
        }
        at += line.len() + 1;
    }
    out
}

/// Where a `key:` ends, if `s` starts with one.
fn key_colon(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.first().is_some_and(|&c| c == b'"' || c == b'\'') {
        let end = string_end(b, 0, b[0], false);
        return (b.get(end) == Some(&b':')).then_some(end);
    }
    let colon = s.find(':')?;
    let after = b.get(colon + 1);
    (colon > 0 && (after.is_none() || after == Some(&b' ') || after == Some(&b'\t')) && !s[..colon].contains([' ', '{', '[', ','])).then_some(colon)
}

fn ini(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        let lead = at + line.len() - trimmed.len();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            out.push(Span { start: lead, end: at + line.len(), token: Token::Comment });
        } else if trimmed.starts_with('[') {
            let end = trimmed.rfind(']').map_or(trimmed.len(), |k| k + 1);
            out.push(Span { start: lead, end: lead + end, token: Token::Heading });
        } else if let Some(eq) = trimmed.find(['=', ':']).filter(|&k| k > 0) {
            let key = trimmed[..eq].trim_end();
            out.push(Span { start: lead, end: lead + key.len(), token: Token::Key });
            let value_start = lead + eq + 1;
            let value = &text[value_start..at + line.len()];
            match hash_comment(value) {
                Some(c) if c > 0 => {
                    scalar(text, value_start, value_start + c, &mut out);
                    out.push(Span { start: value_start + c, end: at + line.len(), token: Token::Comment });
                }
                _ => scalar(text, value_start, at + line.len(), &mut out),
            }
        }
        at += line.len() + 1;
    }
    out
}

fn diff(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in text.split('\n') {
        let token = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") || line.starts_with("index ") {
            Some(Token::Keyword)
        } else if line.starts_with("@@") {
            Some(Token::Heading)
        } else if line.starts_with('+') {
            Some(Token::Added)
        } else if line.starts_with('-') {
            Some(Token::Removed)
        } else {
            None
        };
        if let Some(token) = token {
            out.push(Span { start: at, end: at + line.len(), token });
        }
        at += line.len() + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str, lang: Lang) -> Vec<(&str, Token)> {
        highlight(text, lang).into_iter().map(|s| (&text[s.start..s.end], s.token)).collect()
    }

    #[test]
    fn text_or_binary() {
        assert!(looks_like_text(b"hello\nworld\t!"));
        assert!(!looks_like_text(b"PK\x03\x04\x00\x00binary"));
        assert_eq!(decode(b"a\r\nb\rc", false).as_deref(), Some("a\nb\nc"));
        assert_eq!(decode(b"\xEF\xBB\xBFhi", false).as_deref(), Some("hi"));
        assert_eq!(decode(b"\xFF\xFEh\0i\0", false).as_deref(), Some("hi"));
        assert_eq!(decode(b"caf\xE9", false).as_deref(), Some("café"));
        // Cut through the middle of "é" by the read limit.
        assert_eq!(decode(b"caf\xC3", true).as_deref(), Some("caf"));
        assert_eq!(decode(b"\x00\x01\x02", false), None);
    }

    #[test]
    fn first_lines_cuts() {
        assert_eq!(first_lines("a\nb\nc".into(), 2), ("a\nb".into(), true));
        assert_eq!(first_lines("a\nb".into(), 2), ("a\nb".into(), false));
        assert_eq!(first_lines("a\nb\n".into(), 2), ("a\nb\n".into(), false));
    }

    #[test]
    fn minified_json_is_laid_out() {
        let long = format!("{{\"a\":[1,2,{{}}],\"b\":\"x, y: {{z}}\",\"pad\":\"{}\"}}", "p".repeat(160));
        let pretty = pretty_json(&long).unwrap();
        assert!(pretty.starts_with("{\n  \"a\": [\n    1,\n    2,\n    {}\n  ],\n  \"b\": \"x, y: {z}\","));
        assert_eq!(pretty_json("{\n  \"a\": 1\n}"), None);
    }

    #[test]
    fn csv_cells_and_quotes() {
        let (rows, cut) = parse_table("a,b,c\n1,\"two, three\",\"say \"\"hi\"\"\"\n", b',', 100, 10);
        assert_eq!(rows, vec![vec!["a", "b", "c"], vec!["1", "two, three", "say \"hi\""]]);
        assert!(!cut);
        assert_eq!(sniff_delimiter("a;b;c\n1;2;3"), b';');
        assert_eq!(sniff_delimiter("a\tb"), b'\t');
        let (rows, cut) = parse_table("1\n2\n3\n", b',', 2, 10);
        assert_eq!(rows.len(), 2);
        assert!(cut);
    }

    #[test]
    fn rust_colours() {
        let t = tokens("fn main() { let s = \"hi\"; // done\n println!(\"{}\", 42); 'a: loop {} }", Lang::Rust);
        assert!(t.contains(&("fn", Token::Keyword)));
        assert!(t.contains(&("main", Token::Function)));
        assert!(t.contains(&("\"hi\"", Token::String)));
        assert!(t.contains(&("// done", Token::Comment)));
        assert!(t.contains(&("println!", Token::Function)));
        assert!(t.contains(&("42", Token::Number)));
        assert!(!t.iter().any(|(s, _)| s.starts_with("'a")));
    }

    #[test]
    fn python_and_shell_comments() {
        let t = tokens("def f(x):\n    \"\"\"doc\nstring\"\"\"\n    return x # half", Lang::Python);
        assert!(t.contains(&("\"\"\"doc\nstring\"\"\"", Token::String)));
        assert!(t.contains(&("# half", Token::Comment)));
        let t = tokens("echo $HOME a#b # c", Lang::Shell);
        assert!(t.contains(&("$HOME", Token::Attr)));
        assert!(t.contains(&("# c", Token::Comment)));
        assert!(!t.contains(&("#b # c", Token::Comment)));
    }

    #[test]
    fn markup_and_data_files() {
        let t = tokens("<a href=\"x\">hi &amp; bye</a><!-- c -->", Lang::Markup);
        assert!(t.contains(&("<a", Token::Tag)));
        assert!(t.contains(&("href", Token::Attr)));
        assert!(t.contains(&("\"x\"", Token::String)));
        assert!(t.contains(&("&amp;", Token::Number)));
        assert!(t.contains(&("<!-- c -->", Token::Comment)));
        let t = tokens("name: View # app\nlist:\n  - 3\n", Lang::Yaml);
        assert!(t.contains(&("name", Token::Key)));
        assert!(t.contains(&("# app", Token::Comment)));
        assert!(t.contains(&("3", Token::Number)));
        let t = tokens("[package]\nname = \"view\"\n; note", Lang::Ini);
        assert!(t.contains(&("[package]", Token::Heading)));
        assert!(t.contains(&("name", Token::Key)));
        assert!(t.contains(&("\"view\"", Token::String)));
        assert!(t.contains(&("; note", Token::Comment)));
        let j = "{\"k\": \"v\", \"n\": -1.5e3, \"t\": true}";
        let t: Vec<_> = highlight_json(j).into_iter().map(|s| (&j[s.start..s.end], s.token)).collect();
        assert_eq!(t, vec![("\"k\"", Token::Key), ("\"v\"", Token::String), ("\"n\"", Token::Key), ("-1.5e3", Token::Number), ("\"t\"", Token::Key), ("true", Token::Keyword)]);
    }
}
