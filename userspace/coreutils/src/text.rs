//! **Bytes as lines** (Phase 6 Part B.7): what `log --kernel` reads the kernel's log by.

/// **The lines of `bytes`**, split at each `\n` with a `\r` before it dropped: a last line with no
/// newline is still a line, and the nothing after a final newline is not one.
pub fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let empty = bytes.is_empty();
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    body.split(|&b| b == b'\n').filter(move |_| !empty).map(|l| l.strip_suffix(b"\r").unwrap_or(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(bytes: &[u8]) -> Vec<&[u8]> {
        lines(bytes).collect()
    }

    /// **A line a newline**, the last whether or not one ends it, an empty line kept where two
    /// newlines meet, a `\r` before a newline dropped, and no line in no bytes.
    #[test]
    fn lines_end_at_newlines_and_the_last_need_not() {
        assert_eq!(all(b"a\nb\n"), [&b"a"[..], b"b"]);
        assert_eq!(all(b"a\nb"), [&b"a"[..], b"b"], "a last line with no newline");
        assert_eq!(all(b"a\n\nb\n"), [&b"a"[..], b"", b"b"], "an empty line between");
        assert_eq!(all(b"a\r\nb"), [&b"a"[..], b"b"]);
        assert_eq!(all(b"\n"), [&b""[..]], "one empty line");
        assert!(all(b"").is_empty(), "no bytes, no line");
    }
}
