//! `LogRecord` append body — the payload of a log append.
//!
//! A log append is deliberately **not** an rsproto operation: it carries no envelope
//! and no `op`. The logging service hands each emitter a dedicated log channel
//! (obtained by resolving a path under the logging service), and appending a record is
//! a raw `sys_channel_send` of the body defined here. The channel's identity is the
//! "op" — every message on it is a log record. See
//! `docs/architecture/logging.md` and `docs/spec/rsproto-wire-format.md` § Log records.
//!
//! The body carries only the emitter's **claimed** fields (`level`, `message`, optional
//! `source` sub-label, `span_id`, `trace_id`, and — deferred — structured `fields`). The
//! **trusted** fields (`principal`, `tier`, `timestamp`, `sequence`) are supplied by the
//! logging service from the channel the record arrived on; they never appear on the wire.

use crate::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64};

// --- Levels -----------------------------------------------------------------

/// Severity levels, matching `LogLevel` in the architecture doc.
pub const LEVEL_TRACE: u8 = 0;
pub const LEVEL_DEBUG: u8 = 1;
pub const LEVEL_INFO: u8 = 2;
pub const LEVEL_WARN: u8 = 3;
pub const LEVEL_ERROR: u8 = 4;
pub const LEVEL_CRITICAL: u8 = 5;

// --- Flags ------------------------------------------------------------------

/// `span_id` is present (else the wire's 0 means "absent").
pub const LOG_FLAG_HAS_SPAN: u8 = 1 << 0;
/// `trace_id` is present.
pub const LOG_FLAG_HAS_TRACE: u8 = 1 << 1;
/// A `source` sub-label follows the message.
pub const LOG_FLAG_HAS_SOURCE: u8 = 1 << 2;

// --- Body layout ------------------------------------------------------------

/// Fixed header length (before the variable message/source/fields).
pub const LOG_HEADER_LEN: usize = 24;

/// A parsed log-append body.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LogAppend<'a> {
    pub level: u8,
    /// The log message (UTF-8; not validated here).
    pub message: &'a [u8],
    /// Optional self-declared sub-label under the channel's `principal`.
    pub source: Option<&'a [u8]>,
    pub span_id: Option<u64>,
    pub trace_id: Option<u64>,
    /// Count of structured k/v `fields`. Always 0 in slice 1 (typed-I/O deferred); the
    /// field bytes themselves are not yet decoded — reserved for forward-compat.
    pub field_count: u16,
}

/// Encode a log-append body into `out`; returns its length, or `None` if `out` is too
/// small or a length exceeds its wire width. `field_count` is 0 (structured fields are
/// deferred).
pub fn encode_append(
    out: &mut [u8],
    level: u8,
    message: &[u8],
    span_id: Option<u64>,
    trace_id: Option<u64>,
    source: Option<&[u8]>,
) -> Option<usize> {
    if message.len() > u32::MAX as usize {
        return None;
    }
    let src = source.unwrap_or(&[]);
    if source.is_some() && src.len() > u16::MAX as usize {
        return None;
    }

    let mut flags = 0u8;
    if span_id.is_some() {
        flags |= LOG_FLAG_HAS_SPAN;
    }
    if trace_id.is_some() {
        flags |= LOG_FLAG_HAS_TRACE;
    }
    if source.is_some() {
        flags |= LOG_FLAG_HAS_SOURCE;
    }

    let source_bytes = if source.is_some() { 2 + src.len() } else { 0 };
    let total = LOG_HEADER_LEN + message.len() + source_bytes;
    if out.len() < total {
        return None;
    }

    out[0] = level;
    out[1] = flags;
    put_u16(out, 2, 0); // field_count
    put_u32(out, 4, message.len() as u32);
    put_u64(out, 8, span_id.unwrap_or(0));
    put_u64(out, 16, trace_id.unwrap_or(0));
    let mut off = LOG_HEADER_LEN;
    out[off..off + message.len()].copy_from_slice(message);
    off += message.len();
    if source.is_some() {
        put_u16(out, off, src.len() as u16);
        off += 2;
        out[off..off + src.len()].copy_from_slice(src);
    }
    Some(total)
}

/// Parse a log-append body. Rejects a truncated header, message, or source. The
/// structured `fields` region (when `field_count > 0`, a future addition) is not
/// decoded; only its count is surfaced.
pub fn parse_append(body: &[u8]) -> Option<LogAppend<'_>> {
    if body.len() < LOG_HEADER_LEN {
        return None;
    }
    let level = body[0];
    let flags = body[1];
    let field_count = get_u16(body, 2);
    let message_len = get_u32(body, 4) as usize;
    let span = get_u64(body, 8);
    let trace = get_u64(body, 16);

    let msg_end = LOG_HEADER_LEN.checked_add(message_len)?;
    if body.len() < msg_end {
        return None;
    }
    let message = &body[LOG_HEADER_LEN..msg_end];

    let source = if flags & LOG_FLAG_HAS_SOURCE != 0 {
        if body.len() < msg_end + 2 {
            return None;
        }
        let source_len = get_u16(body, msg_end) as usize;
        let src_start = msg_end + 2;
        let src_end = src_start.checked_add(source_len)?;
        if body.len() < src_end {
            return None;
        }
        Some(&body[src_start..src_end])
    } else {
        None
    };

    Some(LogAppend {
        level,
        message,
        source,
        span_id: (flags & LOG_FLAG_HAS_SPAN != 0).then_some(span),
        trace_id: (flags & LOG_FLAG_HAS_TRACE != 0).then_some(trace),
        field_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_record_round_trips() {
        let mut buf = [0u8; 128];
        let n = encode_append(&mut buf, LEVEL_INFO, b"hello", None, None, None).unwrap();
        assert_eq!(n, LOG_HEADER_LEN + 5);
        let r = parse_append(&buf[..n]).unwrap();
        assert_eq!(
            r,
            LogAppend {
                level: LEVEL_INFO,
                message: b"hello",
                source: None,
                span_id: None,
                trace_id: None,
                field_count: 0,
            }
        );
    }

    #[test]
    fn full_record_round_trips() {
        let mut buf = [0u8; 256];
        let n = encode_append(
            &mut buf,
            LEVEL_ERROR,
            b"disk full",
            Some(0xABCD),
            Some(0x1234_5678),
            Some(b"foo.worker"),
        )
        .unwrap();
        let r = parse_append(&buf[..n]).unwrap();
        assert_eq!(r.level, LEVEL_ERROR);
        assert_eq!(r.message, b"disk full");
        assert_eq!(r.source, Some(&b"foo.worker"[..]));
        assert_eq!(r.span_id, Some(0xABCD));
        assert_eq!(r.trace_id, Some(0x1234_5678));
    }

    #[test]
    fn absent_optionals_stay_none_even_if_wire_bytes_nonzero() {
        // Encode with no span/trace; the wire span/trace fields are 0 and the flags
        // clear, so parse must report None regardless.
        let mut buf = [0u8; 64];
        let n = encode_append(&mut buf, LEVEL_DEBUG, b"x", None, None, None).unwrap();
        let r = parse_append(&buf[..n]).unwrap();
        assert_eq!(r.span_id, None);
        assert_eq!(r.trace_id, None);
    }

    #[test]
    fn parse_rejects_truncation() {
        // Short header.
        assert!(parse_append(&[0u8; 8]).is_none());
        // message_len overruns the body.
        let mut buf = [0u8; 32];
        buf[1] = 0; // no flags
        super::put_u32(&mut buf, 4, 100); // claims 100 message bytes
        assert!(parse_append(&buf).is_none());
        // has_source set but no room for the source_len.
        let mut buf2 = [0u8; 64];
        let n = encode_append(&mut buf2, LEVEL_INFO, b"hi", None, None, None).unwrap();
        buf2[1] |= LOG_FLAG_HAS_SOURCE; // lie: claim a source that isn't there
        assert!(parse_append(&buf2[..n]).is_none());
    }
}
