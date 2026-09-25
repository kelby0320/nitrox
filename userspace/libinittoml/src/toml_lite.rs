//! A minimal, line-oriented TOML parser — just enough for `init.toml`.
//!
//! Per `userspace/init/CLAUDE.md`, init parses TOML by hand (the ecosystem parser
//! pulls in things init can't use). The supported subset is exactly what
//! `docs/spec/init-toml-schema.md` needs:
//!
//! - **table arrays** `[[name]]`
//! - **one-level subtables** `[name.sub]` (the subtable of the most recent
//!   `[[name]]` element — e.g. `[mount.options]`)
//! - **scalar values**: basic strings (`"..."`, no escapes), integers (decimal,
//!   `_` separators allowed), and booleans (`true`/`false`)
//! - `#` comments (whole-line and trailing, respecting quotes) and blank lines
//!
//! Not supported (rejected or absent — upgrade the parser if `init.toml` ever
//! needs them): multi-line/literal strings, arrays, inline tables, datetimes,
//! nesting deeper than one level, dotted keys. Top-level bare `key = value` pairs
//! (before any header) are accepted into [`Document::top`] but unused by init.

use alloc::string::String;
use alloc::vec::Vec;

/// A scalar TOML value (the subset init needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    String(String),
    Integer(i64),
    Boolean(bool),
}

impl Value {
    /// The string contents, if this is a [`Value::String`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The integer, if this is a [`Value::Integer`].
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// The boolean, if this is a [`Value::Boolean`].
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }
}

/// A flat table: ordered `key → value` pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    pairs: Vec<(String, Value)>,
}

impl Table {
    fn new() -> Self {
        Table { pairs: Vec::new() }
    }

    fn insert(&mut self, key: String, value: Value) {
        self.pairs.push((key, value));
    }

    /// Look up `key`'s value (the first, if duplicated).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Convenience: the string value at `key`, if present and a string.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    /// The ordered key/value pairs (e.g. to forward an `options` table verbatim).
    pub fn pairs(&self) -> &[(String, Value)] {
        &self.pairs
    }
}

/// One element of a table array (`[[name]]`): its own table plus any one-level
/// subtables declared under it (`[name.sub]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArrayEntry {
    pub table: Table,
    subtables: Vec<(String, Table)>,
}

impl ArrayEntry {
    /// The named subtable, if present (e.g. `entry.subtable("options")`).
    pub fn subtable(&self, name: &str) -> Option<&Table> {
        self.subtables
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t)
    }
}

/// A parsed document: top-level pairs plus named table arrays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    pub top: Table,
    arrays: Vec<(String, Vec<ArrayEntry>)>,
}

impl Document {
    /// The elements of the `[[name]]` table array (empty slice if none).
    pub fn array(&self, name: &str) -> &[ArrayEntry] {
        self.arrays
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[])
    }

    fn array_index(&self, name: &str) -> Option<usize> {
        self.arrays.iter().position(|(n, _)| n == name)
    }

    fn push_array_entry(&mut self, name: &str) -> (usize, usize) {
        let arr = match self.array_index(name) {
            Some(i) => i,
            None => {
                self.arrays.push((String::from(name), Vec::new()));
                self.arrays.len() - 1
            }
        };
        self.arrays[arr].1.push(ArrayEntry::default());
        let entry = self.arrays[arr].1.len() - 1;
        (arr, entry)
    }
}

/// A parse failure, with the 1-based line number for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub kind: ErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A `[`/`[[` header was malformed (unbalanced brackets, empty name).
    BadHeader,
    /// A `[a.b]` subtable referenced an `a` that is not a current table array.
    UnknownSubtableParent,
    /// A header nested deeper than one level (`[a.b.c]`), unsupported.
    NestingTooDeep,
    /// A non-blank, non-header line had no `=`.
    MissingEquals,
    /// The key before `=` was empty.
    EmptyKey,
    /// A basic string was not terminated by a closing `"`.
    UnterminatedString,
    /// The value after `=` was not a supported scalar.
    BadValue,
}

/// Tracks which table subsequent `key = value` lines write into.
enum Cursor {
    Top,
    ArrayMain { arr: usize, entry: usize },
    ArraySub { arr: usize, entry: usize, sub: usize },
}

/// Parse `input` into a [`Document`], or the first [`ParseError`].
pub fn parse(input: &str) -> Result<Document, ParseError> {
    let mut doc = Document::default();
    let mut cursor = Cursor::Top;

    for (idx, raw) in input.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let lineno = idx + 1;

        if let Some(rest) = line.strip_prefix("[[") {
            // Table-array header: `[[name]]`.
            let name = rest
                .strip_suffix("]]")
                .ok_or(err(lineno, ErrorKind::BadHeader))?
                .trim();
            if name.is_empty() || !is_bare_key(name) {
                return Err(err(lineno, ErrorKind::BadHeader));
            }
            let (arr, entry) = doc.push_array_entry(name);
            cursor = Cursor::ArrayMain { arr, entry };
        } else if let Some(rest) = line.strip_prefix('[') {
            // Table header: `[name.sub]` (one level) — the only `[...]` form init
            // uses. `[name]` (top-level table) is not needed and is rejected.
            let inner = rest
                .strip_suffix(']')
                .ok_or(err(lineno, ErrorKind::BadHeader))?
                .trim();
            let mut segs = inner.split('.');
            let parent = segs.next().unwrap_or("").trim();
            let sub = segs.next().map(str::trim);
            if segs.next().is_some() {
                return Err(err(lineno, ErrorKind::NestingTooDeep));
            }
            let Some(sub) = sub else {
                // Bare `[name]`: a top-level table — unsupported subset.
                return Err(err(lineno, ErrorKind::BadHeader));
            };
            if parent.is_empty() || !is_bare_key(parent) || sub.is_empty() || !is_bare_key(sub) {
                return Err(err(lineno, ErrorKind::BadHeader));
            }
            // The subtable attaches to the most recent `[[parent]]` element.
            let arr = doc
                .array_index(parent)
                .filter(|&a| !doc.arrays[a].1.is_empty())
                .ok_or(err(lineno, ErrorKind::UnknownSubtableParent))?;
            let entry = doc.arrays[arr].1.len() - 1;
            doc.arrays[arr].1[entry]
                .subtables
                .push((String::from(sub), Table::new()));
            let sub_idx = doc.arrays[arr].1[entry].subtables.len() - 1;
            cursor = Cursor::ArraySub { arr, entry, sub: sub_idx };
        } else {
            // `key = value`.
            let eq = line.find('=').ok_or(err(lineno, ErrorKind::MissingEquals))?;
            let key = line[..eq].trim();
            if key.is_empty() || !is_bare_key(key) {
                return Err(err(lineno, ErrorKind::EmptyKey));
            }
            let value = parse_value(line[eq + 1..].trim(), lineno)?;
            let table = match cursor {
                Cursor::Top => &mut doc.top,
                Cursor::ArrayMain { arr, entry } => &mut doc.arrays[arr].1[entry].table,
                Cursor::ArraySub { arr, entry, sub } => {
                    &mut doc.arrays[arr].1[entry].subtables[sub].1
                }
            };
            table.insert(String::from(key), value);
        }
    }

    Ok(doc)
}

const fn err(line: usize, kind: ErrorKind) -> ParseError {
    ParseError { line, kind }
}

/// A bare key / header name: non-empty, ASCII alphanumeric plus `_` and `-`.
fn is_bare_key(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Drop a trailing `# comment`, but not a `#` inside a double-quoted string.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (i, b) in line.bytes().enumerate() {
        match b {
            b'"' => in_string = !in_string,
            b'#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Parse a scalar value (already trimmed).
fn parse_value(s: &str, line: usize) -> Result<Value, ParseError> {
    if let Some(rest) = s.strip_prefix('"') {
        // Basic string; no escape processing (init.toml values don't need it).
        let body = rest.strip_suffix('"').ok_or(err(line, ErrorKind::UnterminatedString))?;
        if body.contains('"') {
            return Err(err(line, ErrorKind::UnterminatedString));
        }
        return Ok(Value::String(String::from(body)));
    }
    if s == "true" {
        return Ok(Value::Boolean(true));
    }
    if s == "false" {
        return Ok(Value::Boolean(false));
    }
    // Integer: decimal, optional leading sign, `_` separators allowed.
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'_' || b == b'-' || b == b'+')
    {
        let mut digits = String::new();
        for b in s.bytes() {
            if b != b'_' {
                digits.push(b as char);
            }
        }
        if let Ok(i) = digits.parse::<i64>() {
            return Ok(Value::Integer(i));
        }
    }
    Err(err(line, ErrorKind::BadValue))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_table_array_and_subtable() {
        let src = "\
[[mount]]
fs_server = \"fs-server-ext4\"
mount_point = \"/\"

[mount.options]
data_journal = true
block_size = 4096
";
        let doc = parse(src).unwrap();
        let mounts = doc.array("mount");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].table.get_str("fs_server"), Some("fs-server-ext4"));
        assert_eq!(mounts[0].table.get_str("mount_point"), Some("/"));
        let opts = mounts[0].subtable("options").unwrap();
        assert_eq!(opts.get("data_journal"), Some(&Value::Boolean(true)));
        assert_eq!(opts.get("block_size"), Some(&Value::Integer(4096)));
    }

    #[test]
    fn multiple_array_entries_keep_order() {
        let src = "\
[[mount]]
mount_point = \"/\"
[[mount]]
mount_point = \"/store\"
";
        let doc = parse(src).unwrap();
        let m = doc.array("mount");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].table.get_str("mount_point"), Some("/"));
        assert_eq!(m[1].table.get_str("mount_point"), Some("/store"));
        // Each entry's options are independent.
        assert!(m[0].subtable("options").is_none());
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let src = "\
# a comment
[[mount]]

device = \"gpt-partlabel:root\"  # trailing comment
";
        let doc = parse(src).unwrap();
        assert_eq!(
            doc.array("mount")[0].table.get_str("device"),
            Some("gpt-partlabel:root")
        );
    }

    #[test]
    fn hash_inside_string_is_not_a_comment() {
        let doc = parse("[[mount]]\nlabel = \"a#b\"\n").unwrap();
        assert_eq!(doc.array("mount")[0].table.get_str("label"), Some("a#b"));
    }

    #[test]
    fn integer_with_underscores_and_sign() {
        let doc = parse("[[mount]]\na = 1_000\nb = -7\n").unwrap();
        let t = &doc.array("mount")[0].table;
        assert_eq!(t.get("a"), Some(&Value::Integer(1000)));
        assert_eq!(t.get("b"), Some(&Value::Integer(-7)));
    }

    #[test]
    fn errors_carry_line_numbers() {
        assert_eq!(
            parse("[[mount]]\nbroken\n"),
            Err(ParseError { line: 2, kind: ErrorKind::MissingEquals })
        );
        assert_eq!(
            parse("[[mount]]\nx = \"oops\n"),
            Err(ParseError { line: 2, kind: ErrorKind::UnterminatedString })
        );
        assert_eq!(
            parse("[mount.options]\nx = 1\n"),
            Err(ParseError { line: 1, kind: ErrorKind::UnknownSubtableParent })
        );
        assert_eq!(
            parse("[[m]]\n[a.b.c]\n"),
            Err(ParseError { line: 2, kind: ErrorKind::NestingTooDeep })
        );
        assert_eq!(
            parse("[plain]\n"),
            Err(ParseError { line: 1, kind: ErrorKind::BadHeader })
        );
    }
}
