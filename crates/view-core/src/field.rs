//! What's in a one-line text box: the text, where the caret is, and what's selected. The
//! drawing and the keys are the platform's; this is the editing, the same as Windows' own boxes:
//! typing replaces the selection, Ctrl with the arrows and Backspace goes a word at a time, and
//! Shift with any move selects.

/// Positions are in characters from the start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Field {
    chars: Vec<char>,
    /// Where the caret is. The selection runs between it and `anchor`.
    pub caret: usize,
    pub anchor: usize,
    /// The most characters it takes.
    limit: usize,
}

/// Letters and digits make words; runs of spaces, and other characters, sit between them.
fn class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
    }
}

impl Field {
    pub fn new(limit: usize) -> Field {
        Field { chars: Vec::new(), caret: 0, anchor: 0, limit }
    }

    /// Start with `s` in the box, all of it selected, so typing replaces it and the arrows keep it.
    pub fn with_text(s: &str, limit: usize) -> Field {
        let mut f = Field::new(limit);
        f.insert(s);
        f.select_all();
        f
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// The selection, start first.
    pub fn range(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    pub fn selected(&self) -> String {
        let (a, b) = self.range();
        self.chars[a..b].iter().collect()
    }

    /// Type or paste `s` over the selection. Only its first line counts, and only what fits.
    pub fn insert(&mut self, s: &str) {
        let (a, b) = self.range();
        self.chars.drain(a..b);
        let room = self.limit.saturating_sub(self.chars.len());
        let line = s.split(['\r', '\n']).next().unwrap_or("");
        let new: Vec<char> = line.chars().filter(|c| !c.is_control()).take(room).collect();
        let n = new.len();
        self.chars.splice(a..a, new);
        self.caret = a + n;
        self.anchor = self.caret;
    }

    /// Backspace: the selection if there is one, otherwise the character (or with Ctrl, the
    /// word) before the caret.
    pub fn delete_back(&mut self, word: bool) {
        if !self.has_selection() {
            self.anchor = if word { self.word_before(self.caret) } else { self.caret.saturating_sub(1) };
        }
        self.insert("");
    }

    /// Delete: the selection, or the character (or word) after the caret.
    pub fn delete_forward(&mut self, word: bool) {
        if !self.has_selection() {
            self.anchor = if word { self.word_after(self.caret) } else { (self.caret + 1).min(self.len()) };
        }
        self.insert("");
    }

    /// Left or Right. Without Shift, a selection collapses to the side it's going.
    pub fn step(&mut self, forward: bool, word: bool, extend: bool) {
        let to = if !extend && !word && self.has_selection() {
            let (a, b) = self.range();
            if forward { b } else { a }
        } else if word {
            if forward { self.word_after(self.caret) } else { self.word_before(self.caret) }
        } else if forward {
            (self.caret + 1).min(self.len())
        } else {
            self.caret.saturating_sub(1)
        };
        self.place(to, extend);
    }

    /// Home and End.
    pub fn to_end(&mut self, end: bool, extend: bool) {
        self.place(if end { self.len() } else { 0 }, extend);
    }

    /// Put the caret at `at`, carrying the selection along with Shift.
    pub fn place(&mut self, at: usize, extend: bool) {
        self.caret = at.min(self.len());
        if !extend {
            self.anchor = self.caret;
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.len();
    }

    /// A double-click: the word (or run of spaces, or the single other character) at `at`.
    pub fn select_word(&mut self, at: usize) {
        if self.is_empty() {
            return;
        }
        let mut k = at.min(self.len() - 1);
        // Just past a word's end still means the word.
        if k == at && k > 0 && class(self.chars[k]) != 0 && class(self.chars[k - 1]) == 0 {
            k -= 1;
        }
        let c = class(self.chars[k]);
        let (mut a, mut b) = (k, k + 1);
        if c != 2 {
            while a > 0 && class(self.chars[a - 1]) == c {
                a -= 1;
            }
            while b < self.len() && class(self.chars[b]) == c {
                b += 1;
            }
        }
        self.anchor = a;
        self.caret = b;
    }

    /// Ctrl+Left's stop: the start of the word before `at`.
    fn word_before(&self, at: usize) -> usize {
        let mut i = at;
        while i > 0 && class(self.chars[i - 1]) == 1 {
            i -= 1;
        }
        if i > 0 {
            let c = class(self.chars[i - 1]);
            while i > 0 && class(self.chars[i - 1]) == c {
                i -= 1;
            }
        }
        i
    }

    /// Ctrl+Right's stop: past the word at `at` and the spaces after it, as Windows does.
    fn word_after(&self, at: usize) -> usize {
        let mut i = at;
        if i < self.len() {
            let c = class(self.chars[i]);
            if c != 1 {
                while i < self.len() && class(self.chars[i]) == c {
                    i += 1;
                }
            }
        }
        while i < self.len() && class(self.chars[i]) == 1 {
            i += 1;
        }
        i
    }

    /// UTF-16 units before character `at`, which is how DirectWrite counts.
    pub fn units(&self, at: usize) -> u32 {
        self.chars[..at.min(self.len())].iter().map(|c| c.len_utf16() as u32).sum()
    }

    /// The character position for UTF-16 position `units`.
    pub fn from_units(&self, units: u32) -> usize {
        let mut u = 0u32;
        for (i, c) in self.chars.iter().enumerate() {
            if u >= units {
                return i;
            }
            u += c.len_utf16() as u32;
        }
        self.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(s: &str) -> Field {
        let mut f = Field::new(200);
        f.insert(s);
        f
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut f = field("hello world");
        f.place(6, false);
        f.place(11, true);
        assert_eq!(f.selected(), "world");
        f.insert("there");
        assert_eq!(f.text(), "hello there");
        assert_eq!((f.caret, f.anchor), (11, 11));
        let mut f = Field::with_text("old", 200);
        f.insert("n");
        assert_eq!(f.text(), "n");
    }

    #[test]
    fn arrows_move_and_select() {
        let mut f = field("one two  three");
        f.to_end(false, false);
        f.step(true, true, false);
        assert_eq!(f.caret, 4);
        f.step(true, true, true);
        assert_eq!(f.selected(), "two  ");
        // Left without Shift drops the selection at its start.
        f.step(false, false, false);
        assert_eq!((f.caret, f.has_selection()), (4, false));
        f.to_end(true, false);
        f.step(false, true, false);
        assert_eq!(f.caret, 9);
        f.step(false, true, true);
        assert_eq!(f.selected(), "two  ");
    }

    #[test]
    fn backspace_and_delete_take_letters_words_or_the_selection() {
        let mut f = field("find this text");
        f.delete_back(true);
        assert_eq!(f.text(), "find this ");
        f.delete_back(false);
        assert_eq!(f.text(), "find this");
        f.place(0, false);
        f.delete_forward(true);
        assert_eq!(f.text(), "this");
        f.select_all();
        f.delete_back(false);
        assert!(f.is_empty());
        f.delete_back(true);
        f.delete_forward(false);
        assert!(f.is_empty());
    }

    #[test]
    fn a_double_click_picks_the_word() {
        let mut f = field("say \"hello\" now");
        f.select_word(6);
        assert_eq!(f.selected(), "hello");
        f.select_word(10);
        assert_eq!(f.selected(), "hello");
        f.select_word(4);
        assert_eq!(f.selected(), "\"");
        f.select_word(99);
        assert_eq!(f.selected(), "now");
    }

    #[test]
    fn pastes_keep_one_line_and_the_limit() {
        let mut f = Field::new(5);
        f.insert("ab\tc\r\nnext line");
        assert_eq!(f.text(), "abc");
        f.insert("defgh");
        assert_eq!(f.text(), "abcde");
    }

    #[test]
    fn positions_convert_to_utf16_and_back() {
        let f = field("a😀b");
        assert_eq!(f.units(2), 3);
        assert_eq!(f.from_units(3), 2);
        assert_eq!(f.from_units(1), 1);
        assert_eq!(f.from_units(9), 3);
    }
}
