//! `LogRecord` append body — the payload of a log append — and, since administration Part E.6,
//! **`Read`**, the op that reads the logging service's ring back.
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

/// A level's name, as the serial sink prints it and `log` shows it; `?` for one this does not
/// know.
pub fn level_name(level: u8) -> &'static str {
    match level {
        LEVEL_TRACE => "TRACE",
        LEVEL_DEBUG => "DEBUG",
        LEVEL_INFO => "INFO",
        LEVEL_WARN => "WARN",
        LEVEL_ERROR => "ERROR",
        LEVEL_CRITICAL => "CRIT",
        _ => "?",
    }
}

// --- Tiers ------------------------------------------------------------------
//
// Here rather than in `logging-service` since administration Part E.6, when `log` came to read
// them back too.

/// Kernel tier — the kernel `klog`/audit rings. Never resolved; defined for the record's `tier`.
pub const TIER_KERNEL: u8 = 0;
/// System tier — supervised services (`service-mgr` / `init` mint these).
pub const TIER_SYSTEM: u8 = 1;
/// Application tier — user apps (session-mgr, later).
pub const TIER_APP: u8 = 2;

/// A tier's name, as a log path spells it; `?` for one this does not know.
pub fn tier_name(tier: u8) -> &'static str {
    match tier {
        TIER_KERNEL => "kernel",
        TIER_SYSTEM => "system",
        TIER_APP => "app",
        _ => "?",
    }
}

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

// --- Reading the ring back (administration Part E.6) --------------------------

/// **`Log::Read`** — the records kept after a sequence number, on a **read session**: a channel
/// opened by resolving anything on a read endpoint, which the `logs` grant binds at `/dev/logs`
/// (`docs/spec/rsproto-log-ops.md`). The first reply-bearing op in the `Log` category, which was
/// reserved for it.
pub const OP_LOG_READ: u16 = 0x0700;

/// A `Read` request body: `after: u64` — only records whose sequence is greater — then
/// `max: u32` — at most this many, `0` for as many as one reply holds.
pub const READ_REQUEST_LEN: usize = 12;

/// Encode a `Read` request.
pub fn read_request(after: u64, max: u32) -> [u8; READ_REQUEST_LEN] {
    let mut b = [0u8; READ_REQUEST_LEN];
    put_u64(&mut b, 0, after);
    put_u32(&mut b, 8, max);
    b
}

/// Parse a `Read` request: `(after, max)`, or `None` for any body not exactly
/// [`READ_REQUEST_LEN`] bytes.
pub fn parse_read_request(body: &[u8]) -> Option<(u64, u32)> {
    (body.len() == READ_REQUEST_LEN).then(|| (get_u64(body, 0), get_u32(body, 8)))
}

/// A `Read` reply's header: `count: u32`, `flags: u32` (reserved, `0`), and `oldest: u64` — the
/// sequence of the oldest record the ring still holds, `0` when it holds none — so a reader asking
/// after an older one learns that what came between was dropped.
pub const READ_REPLY_HEADER_LEN: usize = 16;

/// Each record's fixed part: `sequence: u64`, `time: u64` (the wall clock at ingest, in
/// nanoseconds since the epoch; `0` when the clock was not set), `timestamp: u64` (the monotonic
/// clock at ingest), `tier: u8`, `level: u8`, then `principal_len`, `source_len` and `message_len`,
/// each a `u16` — followed by those three strings, in that order. `source_len` `0` is no source.
pub const RECORD_HEADER_LEN: usize = 32;

/// One record of a `Read` reply, as the logging service kept it: the trusted fields it stamped,
/// then the emitter's claims.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReadRecord<'a> {
    pub sequence: u64,
    /// The wall clock at ingest, or `None` when it was not set.
    pub time: Option<u64>,
    /// The monotonic clock at ingest.
    pub timestamp: u64,
    pub tier: u8,
    pub level: u8,
    pub principal: &'a [u8],
    pub source: Option<&'a [u8]>,
    pub message: &'a [u8],
}

impl ReadRecord<'_> {
    /// The bytes this record takes in a reply.
    pub fn encoded_len(&self) -> usize {
        RECORD_HEADER_LEN + self.principal.len() + self.source.map_or(0, <[u8]>::len) + self.message.len()
    }
}

/// **Builds a `Read` reply** in `out`, a record at a time, until the next one does not fit.
pub struct ReadReplyWriter<'a> {
    out: &'a mut [u8],
    len: usize,
    count: u32,
}

impl<'a> ReadReplyWriter<'a> {
    /// Start a reply saying the ring's oldest record is `oldest`; `None` if `out` cannot hold even
    /// the header.
    pub fn new(out: &'a mut [u8], oldest: u64) -> Option<Self> {
        if out.len() < READ_REPLY_HEADER_LEN {
            return None;
        }
        put_u32(out, 0, 0);
        put_u32(out, 4, 0);
        put_u64(out, 8, oldest);
        Some(ReadReplyWriter { out, len: READ_REPLY_HEADER_LEN, count: 0 })
    }

    /// Append `rec`, or return `false` and leave the reply as it was: it does not fit, or a string
    /// is longer than a `u16`, or it is empty where it must not be — a principal, or a source that
    /// is present.
    pub fn push(&mut self, rec: &ReadRecord<'_>) -> bool {
        let source = rec.source.unwrap_or(&[]);
        let too_long = |s: &[u8]| s.len() > u16::MAX as usize;
        if rec.principal.is_empty() || rec.source.is_some_and(<[u8]>::is_empty) {
            return false;
        }
        if too_long(rec.principal) || too_long(source) || too_long(rec.message) {
            return false;
        }
        let end = self.len + rec.encoded_len();
        if end > self.out.len() || self.count == u32::MAX {
            return false;
        }
        let o = &mut *self.out;
        let at = self.len;
        put_u64(o, at, rec.sequence);
        put_u64(o, at + 8, rec.time.unwrap_or(0));
        put_u64(o, at + 16, rec.timestamp);
        o[at + 24] = rec.tier;
        o[at + 25] = rec.level;
        put_u16(o, at + 26, rec.principal.len() as u16);
        put_u16(o, at + 28, source.len() as u16);
        put_u16(o, at + 30, rec.message.len() as u16);
        let mut off = at + RECORD_HEADER_LEN;
        for s in [rec.principal, source, rec.message] {
            o[off..off + s.len()].copy_from_slice(s);
            off += s.len();
        }
        self.len = end;
        self.count += 1;
        put_u32(o, 0, self.count);
        true
    }

    /// How many records it holds.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The reply's length.
    pub fn finish(self) -> usize {
        self.len
    }
}

/// A `Read` reply, **checked whole before any record is handed out**: every record lies inside the
/// body, the count is the number there, nothing follows the last, and the reserved flags are `0`.
#[derive(Copy, Clone, Debug)]
pub struct ReadReply<'a> {
    /// The oldest sequence the ring still holds, `0` for none.
    pub oldest: u64,
    /// How many records follow.
    pub count: u32,
    records: &'a [u8],
}

/// Parse a `Read` reply, or `None` for one that is not exactly what [`ReadReplyWriter`] makes.
/// **A reader is fed bytes no writer makes** — a count past the records, a length past the body,
/// bytes after the last record, a sequence that does not rise — and must refuse each rather than
/// read past a string or return half a reply.
pub fn parse_read_reply(body: &[u8]) -> Option<ReadReply<'_>> {
    if body.len() < READ_REPLY_HEADER_LEN || get_u32(body, 4) != 0 {
        return None;
    }
    let count = get_u32(body, 0);
    let oldest = get_u64(body, 8);
    let records = &body[READ_REPLY_HEADER_LEN..];
    let mut off = 0usize;
    let mut last: Option<u64> = None;
    for _ in 0..count {
        let rec = record_at(records, off)?;
        if last.is_some_and(|l| rec.sequence <= l) || rec.sequence == 0 {
            return None;
        }
        last = Some(rec.sequence);
        off += rec.encoded_len();
    }
    (off == records.len()).then_some(ReadReply { oldest, count, records })
}

impl<'a> ReadReply<'a> {
    /// The records, oldest first.
    pub fn records(&self) -> ReadRecords<'a> {
        ReadRecords { records: self.records, off: 0, left: self.count }
    }
}

/// The records of a checked [`ReadReply`].
pub struct ReadRecords<'a> {
    records: &'a [u8],
    off: usize,
    left: u32,
}

impl<'a> Iterator for ReadRecords<'a> {
    type Item = ReadRecord<'a>;

    fn next(&mut self) -> Option<ReadRecord<'a>> {
        if self.left == 0 {
            return None;
        }
        // Checked whole by `parse_read_reply`, so this cannot miss; `?` all the same.
        let rec = record_at(self.records, self.off)?;
        self.off += rec.encoded_len();
        self.left -= 1;
        Some(rec)
    }
}

/// The record at `off` in `records`, or `None` if its fixed part or its strings run past the end,
/// or its principal is empty.
fn record_at(records: &[u8], off: usize) -> Option<ReadRecord<'_>> {
    let fixed = records.get(off..off.checked_add(RECORD_HEADER_LEN)?)?;
    let (plen, slen, mlen) =
        (get_u16(fixed, 26) as usize, get_u16(fixed, 28) as usize, get_u16(fixed, 30) as usize);
    let start = off + RECORD_HEADER_LEN;
    let strings = records.get(start..start + plen + slen + mlen)?;
    if plen == 0 {
        return None;
    }
    let time = get_u64(fixed, 8);
    Some(ReadRecord {
        sequence: get_u64(fixed, 0),
        time: (time != 0).then_some(time),
        timestamp: get_u64(fixed, 16),
        tier: fixed[24],
        level: fixed[25],
        principal: &strings[..plen],
        source: (slen != 0).then(|| &strings[plen..plen + slen]),
        message: &strings[plen + slen..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two records, as the service keeps them: one with a source and the clock set, one without
    /// either.
    fn two() -> [ReadRecord<'static>; 2] {
        [
            ReadRecord {
                sequence: 7,
                time: Some(1_790_605_800_000_000_000),
                timestamp: 5_000_000_000,
                tier: 1,
                level: LEVEL_INFO,
                principal: b"view-broker",
                source: Some(b"audit"),
                message: b"view: alice admin date \xe2\x80\x94 started",
            },
            ReadRecord {
                sequence: 9,
                time: None,
                timestamp: 6_000_000_000,
                tier: 2,
                level: LEVEL_WARN,
                principal: b"heartbeat",
                source: None,
                message: b"",
            },
        ]
    }

    /// Write `records` into a reply, checking each was taken.
    fn reply(oldest: u64, records: &[ReadRecord<'_>]) -> Vec<u8> {
        let mut out = vec![0u8; 512];
        let mut w = ReadReplyWriter::new(&mut out, oldest).unwrap();
        for r in records {
            assert!(w.push(r), "{r:?}");
        }
        let n = w.finish();
        out.truncate(n);
        out
    }

    /// **`Read` round-trips**, the request exactly and the reply record for record, including no
    /// source and no wall clock.
    #[test]
    fn read_request_and_reply_round_trip() {
        assert_eq!(parse_read_request(&read_request(41, 100)), Some((41, 100)));
        let body = reply(3, &two());
        let r = parse_read_reply(&body).unwrap();
        assert_eq!((r.oldest, r.count), (3, 2));
        let got: Vec<_> = r.records().collect();
        assert_eq!(got, two());
        // An empty reply is the end of the ring, and says so the same way.
        let empty = reply(0, &[]);
        let r = parse_read_reply(&empty).unwrap();
        assert_eq!((r.oldest, r.count, r.records().count()), (0, 0, 0));
    }

    /// **A record that does not fit is not written**, and the reply stays whole: the writer never
    /// leaves half a record for a reader to trip on.
    #[test]
    fn a_record_that_does_not_fit_is_left_out() {
        let [a, b] = two();
        let mut out = vec![0u8; READ_REPLY_HEADER_LEN + a.encoded_len() + b.encoded_len() - 1];
        let mut w = ReadReplyWriter::new(&mut out, 1).unwrap();
        assert!(w.push(&a));
        assert!(!w.push(&b));
        assert_eq!(w.count(), 1);
        let n = w.finish();
        let r = parse_read_reply(&out[..n]).unwrap();
        assert_eq!(r.records().collect::<Vec<_>>(), [a]);
        // An empty principal, or a source present and empty, is refused rather than written as
        // something a reader would read differently.
        let mut out = [0u8; 256];
        let mut w = ReadReplyWriter::new(&mut out, 0).unwrap();
        assert!(!w.push(&ReadRecord { principal: b"", ..a }));
        assert!(!w.push(&ReadRecord { source: Some(b""), ..a }));
        assert_eq!(w.count(), 0);
    }

    /// **The request is exactly twelve bytes.**
    #[test]
    fn a_read_request_of_any_other_length_is_refused() {
        let good = read_request(1, 2);
        assert_eq!(parse_read_request(&good[..11]), None);
        let mut long = [0u8; 13];
        long[..12].copy_from_slice(&good);
        assert_eq!(parse_read_request(&long), None);
        assert_eq!(parse_read_request(&[]), None);
    }

    /// **Bytes no writer makes**, each refused whole: the reader is what stands between a
    /// misbehaving server and `log` reading past a string.
    #[test]
    fn a_read_reply_no_writer_makes_is_refused() {
        let good = reply(3, &two());
        assert!(parse_read_reply(&good).is_some());
        let refused = |mutate: &dyn Fn(&mut Vec<u8>), why: &str| {
            let mut b = good.clone();
            mutate(&mut b);
            assert!(parse_read_reply(&b).is_none(), "{why}");
        };
        let first = READ_REPLY_HEADER_LEN;
        refused(&|b| b.truncate(READ_REPLY_HEADER_LEN - 1), "a short header");
        refused(&|b| super::put_u32(b, 0, 3), "a count past the records");
        refused(&|b| super::put_u32(b, 0, 1), "a count short of the records: bytes after the last");
        refused(&|b| b.push(0), "a byte after the last record");
        refused(&|b| b.truncate(b.len() - 1), "the last string cut short");
        refused(&|b| super::put_u32(b, 4, 1), "a reserved flag set");
        refused(&|b| super::put_u16(b, first + 26, 0xFFFF), "a principal length past the body");
        refused(&|b| super::put_u16(b, first + 30, 0xFFFF), "a message length past the body");
        refused(&|b| super::put_u16(b, first + 26, 0), "an empty principal");
        refused(&|b| super::put_u64(b, first, 9), "a sequence that does not rise");
        refused(&|b| super::put_u64(b, first, 0), "a sequence of 0");
        // A count of u32::MAX over a short body must stop at the body, not loop four billion
        // times — it fails on the first record past the end.
        refused(&|b| super::put_u32(b, 0, u32::MAX), "a count of u32::MAX");
    }

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
