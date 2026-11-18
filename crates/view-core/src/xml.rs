//! Just enough XML for the files inside Office documents: elements with their attributes, and
//! the text between them, with entities decoded. No DTDs, no validation, and nothing is lost to
//! a mistake: a broken tag ends the walk, keeping what was read before it.

use std::borrow::Cow;

pub enum Event<'a> {
    /// An element opens: its name without namespace prefix, and its attributes as written.
    /// Empty elements (`<a/>`) open and close at once.
    Start(&'a str, &'a str),
    End(&'a str),
    Text(Cow<'a, str>),
}

/// Walk `s`, calling `each` for every element and stretch of text, in order.
pub fn walk<'a>(s: &'a str, mut each: impl FnMut(Event<'a>)) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let Some(k) = s[i..].find('<') else {
            text(&s[i..], &mut each);
            return;
        };
        if k > 0 {
            text(&s[i..i + k], &mut each);
        }
        i += k + 1;
        if s[i..].starts_with("![CDATA[") {
            let Some(e) = s[i..].find("]]>") else { return };
            each(Event::Text(Cow::Borrowed(&s[i + 8..i + e])));
            i += e + 3;
            continue;
        }
        match b.get(i) {
            Some(b'?') | Some(b'!') => {
                let close = if s[i..].starts_with("!--") { "-->" } else { ">" };
                match s[i..].find(close) {
                    Some(e) => i += e + close.len(),
                    None => return,
                }
                continue;
            }
            Some(b'/') => {
                let Some(e) = s[i..].find('>') else { return };
                each(Event::End(local(s[i + 1..i + e].trim())));
                i += e + 1;
                continue;
            }
            _ => {}
        }
        // The tag runs to the next '>' outside quotes.
        let mut j = i;
        let mut quote = 0u8;
        while j < b.len() {
            let c = b[j];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        if j >= b.len() {
            return;
        }
        let tag = &s[i..j];
        let empty = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let name_end = tag.find(|c: char| c.is_ascii_whitespace()).unwrap_or(tag.len());
        let name = local(&tag[..name_end]);
        each(Event::Start(name, &tag[name_end..]));
        if empty {
            each(Event::End(name));
        }
        i = j + 1;
    }
}

fn text<'a>(s: &'a str, each: &mut impl FnMut(Event<'a>)) {
    if !s.is_empty() {
        each(Event::Text(unescape(s)));
    }
}

/// A name without its namespace prefix.
pub fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// An attribute's value, by its name without prefix, with entities decoded.
pub fn attr<'a>(attrs: &'a str, want: &str) -> Option<Cow<'a, str>> {
    let b = attrs.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let k0 = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = &attrs[k0..i];
        while i < b.len() && (b[i] == b'=' || b[i].is_ascii_whitespace()) {
            i += 1;
        }
        let q = *b.get(i)?;
        if q != b'"' && q != b'\'' {
            return None;
        }
        let v0 = i + 1;
        let v1 = v0 + attrs[v0..].find(q as char)?;
        if local(key) == want {
            return Some(unescape(&attrs[v0..v1]));
        }
        i = v1 + 1;
    }
    None
}

/// `&amp;`, `&#233;` and the rest, as the characters they stand for.
pub fn unescape(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(k) = rest.find('&') {
        out.push_str(&rest[..k]);
        rest = &rest[k..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let c = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if name.starts_with("#x") || name.starts_with("#X") => u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32),
            _ if name.starts_with('#') => name[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_text_and_entities() {
        let mut seen = Vec::new();
        walk(r#"<?xml version="1.0"?><w:p a="1"><w:t>Fish &amp; chips &#233;</w:t><w:br/><![CDATA[<raw>]]></w:p>"#, |e| {
            seen.push(match e {
                Event::Start(n, a) => format!("<{n}{a}>"),
                Event::End(n) => format!("</{n}>"),
                Event::Text(t) => t.into_owned(),
            })
        });
        assert_eq!(seen, ["<p a=\"1\">", "<t>", "Fish & chips é", "</t>", "<br>", "</br>", "<raw>", "</p>"]);
        assert_eq!(attr(r#" w:val="a&lt;b" x='2'"#, "val").as_deref(), Some("a<b"));
        assert_eq!(attr(r#" w:val="a" x='2'"#, "x").as_deref(), Some("2"));
        assert_eq!(unescape("a & b &bogus; &#x41;"), "a & b &bogus; A");
    }
}
