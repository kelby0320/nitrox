//! Columns' text, paths and breadcrumbs.

use super::*;

/// A path as breadcrumb segments: the names in the body ink, the separators in the line colour.
///
/// **Not a styled string**: the design's path strip reads as a sequence of places with
/// punctuation between them, and the only way to say that in this toolkit is one element per
/// part. The root is a single `/`, which is a place rather than a separator.
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
        // The last segment is where you are; the ones before it are how you got here.
        let last = i + 1 == names.len();
        out.push(if last { text(String::from(*name)) } else { ink(theme.foreground_dim, text(String::from(*name))) });
    }
    out
}

/// A byte count for the listing's `Size` column: the design's `848`, `11K`, `2.8M`.
///
/// **Short, where [`size_text`] is exact.** A column is read down rather than across: what it
/// answers is "which of these is the big one", and the Properties dialog is where the exact
/// count lives. Nothing here rounds below 10 KiB, because a four-digit number is as easy to read
/// as `4K` and says more (desktop refresh, Part I).
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
///
/// **The extension, not a guessed type.** This system has no content sniffing and no type
/// database; what it knows about `theme.toml` is the four letters after the dot. A name with no
/// dot, or one whose dot starts it, is a `file`.
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
///
/// **Both arguments are times, and neither is read from the world here**: `now` comes from the
/// binary, which reads the clock when it loads a listing, so this stays a function of values and
/// the host can test the boundary between the two forms.
///
/// `0` is "unreported" rather than 1970 — [`modified_text`] carries the reasoning — and shows as
/// nothing at all, because a column of `unknown` is a column of noise.
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
///
/// **Both numbers, not one.** The exact count is the fact and the rounded one is the answer to
/// "is this big"; a file manager that showed only the round number could not tell 1.0 KiB from
/// 1.0 KiB, and one that showed only the exact one makes a person count digits.
pub fn size_text(bytes: u64) -> String {
    if bytes < 1024 {
        return alloc::format!("{bytes} bytes");
    }
    // Tenths, computed in integers: there is no float formatting here and rounding by hand is
    // one multiplication.
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
/// **`0` is "unreported", not 1970.** Servers that do not keep a modification time send zero, and
/// the whole namespace half of every listing does — so a dialog that formatted it would state
/// 1970-01-01 as a fact about a file created this morning. `fs-server-ext4` carries the same note
/// at the other end of the wire.
pub fn modified_text(mtime: i64) -> String {
    let Some(nanos) = u64::try_from(mtime).ok().filter(|s| *s > 0).and_then(|s| s.checked_mul(1_000_000_000)) else {
        return String::from("unknown");
    };
    let mut out = libtime::format_civil(&libtime::civil_from_unix(nanos));
    out.push_str(" UTC");
    out
}

/// Join a directory path and an entry name.
///
/// `libfs::join`'s rule, on `str` rather than bytes — this half of the application never sees a
/// path as bytes, and converting to call it would be a round trip through a lossy conversion
/// for a rule that is one line.
pub(super) fn join(dir: &str, name: &str) -> String {
    let mut s = String::from(dir);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(name);
    s
}

/// Everything before the final component, or `"/"` at the root.
///
/// `libfs::parent`'s rule on `str`, for the reason [`join`] is here.
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
