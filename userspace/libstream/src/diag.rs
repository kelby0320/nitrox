//! **A diagnostic's level** (the laptop polish's Part E): error, warning or notice.
//!
//! A diagnostic is one message on a pipeline's shared `stderr`, its payload the text
//! ([`pipeline-stdio.md`](../../../docs/spec/pipeline-stdio.md)). The shell painted every one as an
//! error, so a program's progress and its usage read as failures. Now a message may say what it is
//! with **one leading byte**: [`NOTICE`] or [`WARNING`]. **A message whose first byte is neither is
//! an error, whole** — which is every message a program not yet changed sends, so nothing that
//! was an error stops being one. No text starts with either control.

use alloc::vec::Vec;

/// The first byte of a notice: progress, a result said in words, an answer to a question.
pub const NOTICE: u8 = 0x01;
/// The first byte of a warning: something the person may have wanted, and is not getting.
pub const WARNING: u8 = 0x02;

/// What a diagnostic is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Something went wrong — the default, and every message without a level byte.
    Error,
    /// Something the person may have wanted, and is not getting.
    Warning,
    /// Neither: progress, a result, an answer.
    Notice,
}

/// The message for `text` at `level`: the text, behind its level's byte unless it is an error.
pub fn frame(level: Level, text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 1);
    match level {
        Level::Error => {}
        Level::Warning => out.push(WARNING),
        Level::Notice => out.push(NOTICE),
    }
    out.extend_from_slice(text);
    out
}

/// A message's level and its text, the level byte removed.
pub fn parse(msg: &[u8]) -> (Level, &[u8]) {
    match msg.split_first() {
        Some((&NOTICE, text)) => (Level::Notice, text),
        Some((&WARNING, text)) => (Level::Warning, text),
        _ => (Level::Error, msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_level_reads_back_as_itself() {
        for level in [Level::Error, Level::Warning, Level::Notice] {
            let msg = frame(level, b"disk: 3 device(s)");
            assert_eq!(parse(&msg), (level, &b"disk: 3 device(s)"[..]), "{level:?}");
        }
        assert_eq!(frame(Level::Error, b"x"), b"x", "an error carries no byte: it is today's message");
    }

    /// **The reader, with messages no framed writer made**: every program that sends plain text
    /// is still an error, and so is an empty message; only the two level bytes are taken as one.
    #[test]
    fn a_message_without_a_level_byte_is_an_error_whole() {
        assert_eq!(parse(b"remove: /x: no such file"), (Level::Error, &b"remove: /x: no such file"[..]));
        assert_eq!(parse(b""), (Level::Error, &b""[..]));
        assert_eq!(parse(&[0x03, b'x']), (Level::Error, &[0x03, b'x'][..]), "another control is text");
        assert_eq!(parse(&[NOTICE]), (Level::Notice, &b""[..]), "a level byte alone is an empty notice");
    }
}
