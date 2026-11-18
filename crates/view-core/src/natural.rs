//! Natural ordering of file names: runs of digits compare by value, so "IMG 2" comes before
//! "IMG 10", and letters compare without regard to case. This is the order Explorer shows.

use std::cmp::Ordering;

/// Compare two file names naturally.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    // Ties broken later, so the order is total: fewer leading zeros first, then case.
    let mut tie = Ordering::Equal;
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return tie.then_with(|| a.cmp(b)),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let (m, n) = (digits(&mut x), digits(&mut y));
                let (mt, nt) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                let by_value = mt.len().cmp(&nt.len()).then_with(|| mt.cmp(nt));
                if by_value != Ordering::Equal {
                    return by_value;
                }
                if tie == Ordering::Equal {
                    tie = n.len().cmp(&m.len()).reverse();
                }
            }
            (Some(c), Some(d)) => {
                x.next();
                y.next();
                if c == d {
                    continue;
                }
                let (lc, ld) = (fold(c), fold(d));
                if lc != ld {
                    return lc.cmp(&ld);
                }
                if tie == Ordering::Equal {
                    tie = c.cmp(&d);
                }
            }
        }
    }
}

fn digits(it: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while let Some(&c) = it.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        s.push(c);
        it.next();
    }
    s
}

/// A character for comparing without case. Punctuation sorts before letters and digits, as in
/// Explorer, so "a_b" comes before "ab".
fn fold(c: char) -> (u8, char) {
    let lower = c.to_lowercase().next().unwrap_or(c);
    let class = if lower.is_alphanumeric() { 1 } else { 0 };
    (class, lower)
}

/// Plain alphabetical order, ignoring case but not treating digits specially.
pub fn compare_plain(a: &str, b: &str) -> Ordering {
    let fa = a.chars().map(|c| c.to_lowercase().next().unwrap_or(c));
    let fb = b.chars().map(|c| c.to_lowercase().next().unwrap_or(c));
    fa.cmp(fb).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_by(|a, b| compare(a, b));
        v
    }

    #[test]
    fn numbers_go_by_value() {
        assert_eq!(sorted(vec!["IMG 10.jpg", "IMG 2.jpg", "IMG 1.jpg"]), vec!["IMG 1.jpg", "IMG 2.jpg", "IMG 10.jpg"]);
        assert_eq!(sorted(vec!["a100", "a20", "a3"]), vec!["a3", "a20", "a100"]);
    }

    #[test]
    fn case_is_ignored() {
        assert_eq!(sorted(vec!["beta.png", "Alpha.png", "alpha2.png"]), vec!["Alpha.png", "alpha2.png", "beta.png"]);
    }

    #[test]
    fn leading_zeros_tie_break_but_keep_value_order() {
        assert_eq!(sorted(vec!["x010", "x9", "x10"]), vec!["x9", "x10", "x010"]);
        assert_eq!(compare("x01", "x1"), Ordering::Greater);
    }

    #[test]
    fn order_is_total() {
        assert_ne!(compare("A.jpg", "a.jpg"), Ordering::Equal);
        assert_eq!(compare("a.jpg", "a.jpg"), Ordering::Equal);
    }

    #[test]
    fn unicode_names() {
        assert_eq!(sorted(vec!["Ö2", "ö10", "ö1"]), vec!["ö1", "Ö2", "ö10"]);
        assert_eq!(sorted(vec!["写真 2", "写真 1"]), vec!["写真 1", "写真 2"]);
    }

    #[test]
    fn huge_numbers_do_not_overflow() {
        assert_eq!(compare("f99999999999999999999999", "f100000000000000000000000"), Ordering::Less);
    }

    #[test]
    fn plain_is_alphabetical() {
        let mut v = vec!["IMG 10", "IMG 2"];
        v.sort_by(|a, b| compare_plain(a, b));
        assert_eq!(v, vec!["IMG 10", "IMG 2"]);
    }
}
