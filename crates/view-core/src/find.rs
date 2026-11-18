//! Finding words in a document, for Ctrl+F in the preview. Matching ignores case, reads curly
//! quotes as straight ones and ligatures as their letters, and lets any run of spaces or line
//! breaks match any other, so a phrase is found across the end of a line.

/// Text to search, folded for matching, with where each character came from: UTF-16 positions
/// in a piece of text, or characters on a PDF page, as the caller likes.
#[derive(Default, Debug)]
pub struct Haystack {
    chars: Vec<char>,
    /// For each character: the start and end of what it came from.
    spans: Vec<(u32, u32)>,
}

impl Haystack {
    pub fn new() -> Haystack {
        Haystack::default()
    }

    /// Add `s`, every character of it coming from `start..end`.
    pub fn push(&mut self, s: &str, start: u32, end: u32) {
        for c in s.chars() {
            fold(c, |f| self.add(f, start, end));
        }
    }

    /// Add a piece of text, its positions counted in UTF-16 units from the start.
    pub fn push_units(&mut self, s: &str) {
        let mut at = 0u32;
        for c in s.chars() {
            let next = at + c.len_utf16() as u32;
            fold(c, |f| self.add(f, at, next));
            at = next;
        }
    }

    /// A break between words or lines, at `at`.
    pub fn push_space(&mut self, at: u32) {
        self.add(' ', at, at);
    }

    fn add(&mut self, c: char, start: u32, end: u32) {
        if c == ' ' && self.chars.last() == Some(&' ') {
            return;
        }
        self.chars.push(c);
        self.spans.push((start, end));
    }

    /// Where `query` (from [`query`]) is found: the start of its first character and the end of
    /// its last, in order, not overlapping, at most `limit` of them.
    pub fn find(&self, query: &[char], limit: usize) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let n = query.len();
        if n == 0 || n > self.chars.len() {
            return out;
        }
        let mut i = 0;
        while i + n <= self.chars.len() && out.len() < limit {
            if self.chars[i] == query[0] && self.chars[i..i + n] == *query {
                let (start, _) = self.spans[i];
                let end = self.spans[i..i + n].iter().map(|s| s.1).max().unwrap_or(start);
                out.push((start, end.max(start)));
                i += n;
            } else {
                i += 1;
            }
        }
        out
    }
}

/// What the user typed, folded the same way as the text it's looked for in.
pub fn query(typed: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for c in typed.chars() {
        fold(c, |f| {
            if !(f == ' ' && out.last() == Some(&' ')) {
                out.push(f);
            }
        });
    }
    out
}

/// One character as it's compared: lower case, with lookalikes made the same.
fn fold(c: char, mut put: impl FnMut(char)) {
    match c {
        '\u{00AD}' | '\u{200B}' | '\u{FEFF}' => {}
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{02BC}' | '\u{2032}' => put('\''),
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => put('"'),
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2212}' => put('-'),
        '\u{FB00}' => "ff".chars().for_each(put),
        '\u{FB01}' => "fi".chars().for_each(put),
        '\u{FB02}' => "fl".chars().for_each(put),
        '\u{FB03}' => "ffi".chars().for_each(put),
        '\u{FB04}' => "ffl".chars().for_each(put),
        c if c.is_whitespace() => put(' '),
        c => c.to_lowercase().for_each(put),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ignoring_case_in_utf16_units() {
        let mut h = Haystack::new();
        h.push_units("😀 Hello hello HELLO");
        let found = h.find(&query("hello"), 10);
        assert_eq!(found, vec![(3, 8), (9, 14), (15, 20)]);
        assert_eq!(h.find(&query("hello"), 2).len(), 2);
    }

    #[test]
    fn spaces_and_line_breaks_match_each_other() {
        let mut h = Haystack::new();
        h.push_units("the end\n  of the line");
        assert_eq!(h.find(&query("end of"), 10), vec![(4, 12)]);
        assert_eq!(h.find(&query("END   OF"), 10), vec![(4, 12)]);
    }

    #[test]
    fn page_characters_keep_their_glyphs() {
        // Glyphs: "Of", a line break, "ﬁne" (the ligature is one glyph), "s".
        let mut h = Haystack::new();
        for (k, s) in ["O", "f"].iter().enumerate() {
            h.push(s, k as u32, k as u32 + 1);
        }
        h.push_space(2);
        h.push("\u{FB01}", 2, 3);
        h.push("n", 3, 4);
        h.push("e", 4, 5);
        assert_eq!(h.find(&query("of fine"), 10), vec![(0, 5)]);
        assert_eq!(h.find(&query("fin"), 10), vec![(2, 4)]);
    }

    #[test]
    fn curly_quotes_match_straight_ones() {
        let mut h = Haystack::new();
        h.push_units("it\u{2019}s \u{201C}quoted\u{201D}");
        assert_eq!(h.find(&query("it's \"quoted\""), 10), vec![(0, 13)]);
    }

    #[test]
    fn nothing_is_found_for_nothing() {
        let mut h = Haystack::new();
        h.push_units("abc");
        assert!(h.find(&query(""), 10).is_empty());
        assert!(h.find(&query("abcd"), 10).is_empty());
    }
}
