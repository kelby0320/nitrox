//! Columns' text, paths and breadcrumbs.

use super::*;

/// A path as breadcrumb segments: the names in the body ink, the separators in the line colour.
pub fn breadcrumb<Msg>(path: &str, theme: &UiTheme) -> Vec<Element<Msg>> {
    let mut out: Vec<Element<Msg>> = Vec::new();
    let sep = |theme: &UiTheme| ink(theme.border, text("/"));
    let names: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if names.is_empty() {
        out.push(sep(theme));
        return out;
    }
    for (i, name) in names.iter().enumerate() {
        out.push(sep(theme));
        let last = i + 1 == names.len();
        out.push(if last { text(String::from(*name)) } else { ink(theme.foreground_dim, text(String::from(*name))) });
    }
    out
}

/// A byte count for the listing's `Size` column: the design's `848`, `11K`, `2.8M`.
pub fn column_size(bytes: u64) -> String {
    const K: u64 = 1024;
    if bytes < 10 * K {
        return alloc::format!("{bytes}");
    }
    let (unit, suffix) = match bytes {
        b if b >= K * K * K => (K * K * K, "G"),
        b if b >= K * K => (K * K, "M"),
        _ => (K, "K"),
    };
    let tenths = (bytes * 10 + unit / 2) / unit;
    // One decimal while the number is small enough for it to mean something: `2.8M`, then `24M`.
    if tenths < 100 {
        alloc::format!("{}.{}{suffix}", tenths / 10, tenths % 10)
    } else {
        alloc::format!("{}{suffix}", tenths / 10)
    }
}

/// What kind of thing a row is, for the `Kind` column: `dir`, or the name's extension.
pub fn column_kind(name: &str, is_dir: bool) -> String {
    if is_dir {
        return String::from("dir");
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => String::from(ext),
        _ => String::from("file"),
    }
}

/// A modification time for the `Modified` column: `HH:MM` today, `YYYY-MM-DD` before that.
pub fn column_modified(mtime: i64, now_nanos: u64) -> String {
    let Some(nanos) = u64::try_from(mtime).ok().filter(|s| *s > 0).and_then(|s| s.checked_mul(1_000_000_000)) else {
        return String::new();
    };
    let when = libtime::civil_from_unix(nanos);
    let today = libtime::civil_from_unix(now_nanos);
    let full = libtime::format_civil(&when);
    if (when.year, when.month, when.day) == (today.year, today.month, today.day) {
        // `YYYY-MM-DD HH:MM:SS`, of which the clock is five characters at offset 11.
        String::from(full.get(11..16).unwrap_or(""))
    } else {
        String::from(full.get(..10).unwrap_or(full.as_str()))
    }
}

/// A byte count, with a rounded form beside it once the exact one stops being readable.
pub fn size_text(bytes: u64) -> String {
    if bytes < 1024 {
        return alloc::format!("{bytes} bytes");
    }
    // Tenths, in integers: there is no float formatting here.
    let (unit, name) = match bytes {
        b if b >= 1024 * 1024 * 1024 => (1024u64 * 1024 * 1024, "GiB"),
        b if b >= 1024 * 1024 => (1024 * 1024, "MiB"),
        _ => (1024, "KiB"),
    };
    let tenths = (bytes * 10 + unit / 2) / unit;
    alloc::format!("{}.{} {name} ({bytes} bytes)", tenths / 10, tenths % 10)
}

/// A modification time as `YYYY-MM-DD HH:MM:SS UTC`, or `unknown`.
///
/// `0` is "unreported", not 1970: servers without a modification time send zero, as does the
/// namespace half of every listing.
pub fn modified_text(mtime: i64) -> String {
    let Some(nanos) = u64::try_from(mtime).ok().filter(|s| *s > 0).and_then(|s| s.checked_mul(1_000_000_000)) else {
        return String::from("unknown");
    };
    let mut out = libtime::format_civil(&libtime::civil_from_unix(nanos));
    out.push_str(" UTC");
    out
}

/// Join a directory path and an entry name: `libfs::join`'s rule, on `str`.
pub(super) fn join(dir: &str, name: &str) -> String {
    let mut s = String::from(dir);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(name);
    s
}

/// Everything before the final component, or `"/"` at the root: `libfs::parent`'s rule on `str`.
pub(super) fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => String::from("/"),
        Some(i) => String::from(&trimmed[..i]),
    }
}

/// The rectangle the window occupies, for the binary's layout call.
pub fn bounds(size: Size) -> Rect {
    Rect::new(0, 0, size.w, size.h)
}
