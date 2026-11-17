//! A line's inline markup: code spans, bold, emphasis, links, and the HTML a page may carry.

use alloc::string::String;
use alloc::vec::Vec;

/// How a run of text is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    bold: bool,
    code: bool,
    emph: bool,
}

impl Style {
    /// Bold.
    pub const BOLD: Style = Style { bold: true, code: false, emph: false };

    /// This and `other` together.
    pub fn with(self, other: Style) -> Style {
        Style { bold: self.bold || other.bold, code: self.code || other.code, emph: self.emph || other.emph }
    }

    /// The escape that turns this style on, or `None` for plain text. Code is the shell's cyan,
    /// emphasis an underline.
    pub fn escape(self) -> Option<&'static str> {
        match (self.bold, self.code, self.emph) {
            (false, false, false) => None,
            (true, true, _) => Some("\x1b[1;36m"),
            (true, false, true) => Some("\x1b[1;4m"),
            (true, false, false) => Some("\x1b[1m"),
            (false, true, _) => Some("\x1b[36m"),
            (false, false, true) => Some("\x1b[4m"),
        }
    }
}

/// A run of text in one style.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// The text, markup gone.
    pub text: String,
    /// How it is drawn.
    pub style: Style,
}

/// The HTML tags a page may carry, which draw nothing. Any other `<…>` is text: a placeholder
/// such as `<name>` is what a page means.
const TAGS: &[&str] = &["a", "br", "sup", "sub", "kbd", "span", "div", "details", "summary", "img"];

/// `text` as runs: code spans, `**bold**`, `*emphasis*`, a link as its text, HTML dropped.
pub fn runs(text: &str) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut style = Style::default();
    let mut cur = String::new();
    let mut i = 0;
    let flush = |out: &mut Vec<Run>, cur: &mut String, style: Style| {
        let text = printable(&core::mem::take(cur));
        if !text.is_empty() {
            out.push(Run { text, style });
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() && chars[i + 1].is_ascii_punctuation() => {
                cur.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                let ticks = chars[i..].iter().take_while(|&&c| c == '`').count();
                match find_ticks(&chars, i + ticks, ticks) {
                    Some(end) => {
                        flush(&mut out, &mut cur, style);
                        let code: String = chars[i + ticks..end].iter().collect();
                        out.push(Run { text: printable(code.trim()), style: style.with(CODE) });
                        i = end + ticks;
                    }
                    None => {
                        cur.extend(&chars[i..i + ticks]);
                        i += ticks;
                    }
                }
            }
            '*' if chars.get(i + 1) == Some(&'*') && delimits(&chars, i, 2, style.bold) => {
                flush(&mut out, &mut cur, style);
                style.bold = !style.bold;
                i += 2;
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                cur.push_str("**");
                i += 2;
            }
            '*' if delimits(&chars, i, 1, style.emph) => {
                flush(&mut out, &mut cur, style);
                style.emph = !style.emph;
                i += 1;
            }
            '!' if chars.get(i + 1) == Some(&'[') && link(&chars, i + 1).is_some() => i += 1,
            '[' => match link(&chars, i) {
                Some((label, end)) => {
                    flush(&mut out, &mut cur, style);
                    for mut r in runs(&label) {
                        r.style = r.style.with(style);
                        out.push(r);
                    }
                    i = end;
                }
                None => {
                    cur.push(c);
                    i += 1;
                }
            },
            '<' => match tag(&chars, i) {
                Some(Tag::Drop(end)) => i = end,
                Some(Tag::Url(url, end)) => {
                    cur.push_str(&url);
                    i = end;
                }
                None => {
                    cur.push(c);
                    i += 1;
                }
            },
            '&' => match entity(&chars, i) {
                Some((ch, end)) => {
                    cur.push(ch);
                    i = end;
                }
                None => {
                    cur.push(c);
                    i += 1;
                }
            },
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    flush(&mut out, &mut cur, style);
    out
}

const CODE: Style = Style { bold: false, code: true, emph: false };

/// Where the closing run of `n` backticks starts, at or after `from`.
fn find_ticks(chars: &[char], from: usize, n: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '`' {
            let run = chars[i..].iter().take_while(|&&c| c == '`').count();
            if run == n {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// Whether the `n` asterisks at `i` close the style that is `open`, or open it, rather than being
/// asterisks, as in `a * b`, `*.md and *.rs` or `2 ** 3`. One opens with a word after it, at a
/// word's start, and only if a closer follows: `n` asterisks with a word before them.
fn delimits(chars: &[char], i: usize, n: usize, open: bool) -> bool {
    let before = i.checked_sub(1).map(|j| chars[j]);
    let after = chars.get(i + n).copied();
    let closes_at = |j: usize| {
        chars[j..].iter().take(n).all(|&c| c == '*')
            && chars.get(j + n) != Some(&'*')
            && j.checked_sub(1).is_some_and(|k| !chars[k].is_whitespace() && chars[k] != '*')
    };
    if open {
        return closes_at(i);
    }
    after.is_some_and(|c| !c.is_whitespace() && c != '*')
        && before.is_none_or(|c| c.is_whitespace() || c.is_ascii_punctuation())
        && (i + n + 1..chars.len()).any(closes_at)
}

/// `s` with what a terminal would act on rather than draw taken out: a page is text, and a page
/// that carried an escape could clear or redraw the reader's terminal. A tab is a space.
pub fn printable(s: &str) -> String {
    s.chars().filter_map(|c| if c == '\t' { Some(' ') } else { (!c.is_control()).then_some(c) }).collect()
}

/// A link at `i`, `[label](address)`: its label, and where it ends. `[label][ref]` is text, as it is
/// to Markdown with no definition of `ref`, and no page has one.
fn link(chars: &[char], i: usize) -> Option<(String, usize)> {
    let mut depth = 0;
    let mut close = None;
    for (j, &c) in chars.iter().enumerate().skip(i) {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let stop = chars[close + 2..].iter().position(|&c| c == ')')? + close + 2;
    Some((chars[i + 1..close].iter().collect(), stop + 1))
}

enum Tag {
    /// A tag or comment that draws nothing, and where it ends.
    Drop(usize),
    /// `<https://…>`: its address, and where it ends.
    Url(String, usize),
}

fn tag(chars: &[char], i: usize) -> Option<Tag> {
    let rest: String = chars[i..].iter().collect();
    if rest.starts_with("<!--") {
        let end = rest.find("-->")?;
        return Some(Tag::Drop(i + rest[..end].chars().count() + 3));
    }
    let close = rest.find('>')?;
    let inner = &rest[1..close];
    let end = i + rest[..=close].chars().count();
    if inner.starts_with("http://") || inner.starts_with("https://") {
        return Some(Tag::Url(String::from(inner), end));
    }
    let name: String = inner
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let after = inner.trim_start_matches('/').get(name.len()..).unwrap_or("");
    let shaped = after.is_empty() || after.starts_with(' ') || after == "/";
    (TAGS.contains(&name.as_str()) && shaped).then_some(Tag::Drop(end))
}

fn entity(chars: &[char], i: usize) -> Option<(char, usize)> {
    let rest: String = chars[i..chars.len().min(i + 8)].iter().collect();
    [("&amp;", '&'), ("&lt;", '<'), ("&gt;", '>'), ("&quot;", '"'), ("&nbsp;", ' ')]
        .into_iter()
        .find(|(name, _)| rest.starts_with(name))
        .map(|(name, ch)| (ch, i + name.len()))
}
