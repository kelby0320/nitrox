//! `File` category (`op = 0x06xx`) bodies. Slice 8 defines `ReadRange` — the
//! Model-B page-cache fill: the kernel, demand-faulting a file-backed mapping,
//! asks the resource server for a byte range of a file and the server replies with
//! the bytes in a transferred `MemoryObject`. See `docs/spec/rsproto-file-ops.md`.
//!
//! `File` is the **file-content access** category — positioned, stateless reads of
//! a file resolved lazily (`Namespace::Resolve` with `RESOLVE_FILE_LAZY`, reply
//! `OBJECT_KIND_FILE`). It is deliberately distinct from `Stream` (`0x02`,
//! cursor-based streaming) and `Block` (`0x03`, extent/block-level — Model A's
//! future home).

use crate::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64};

// --- ReadRange request ------------------------------------------------------

/// Fixed prefix of a `ReadRangeRequest` (before the suffix bytes).
pub const READ_RANGE_REQUEST_PREFIX_LEN: usize = 16;

/// A parsed `ReadRangeRequest`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReadRangeRequest<'a> {
    /// File byte offset of the range (page-aligned in the page-cache caller).
    pub offset: u64,
    /// Number of bytes requested (the page-cache caller asks at most one page).
    pub len: u32,
    /// The path suffix naming the file (UTF-8, no leading `/`) — the fill is
    /// stateless, so each `ReadRange` re-identifies its file by path.
    pub suffix: &'a [u8],
}

/// Write a `ReadRangeRequest` body; returns its length.
pub fn read_range_request(out: &mut [u8], offset: u64, len: u32, suffix: &[u8]) -> Option<usize> {
    if suffix.len() > u16::MAX as usize {
        return None;
    }
    let total = READ_RANGE_REQUEST_PREFIX_LEN + suffix.len();
    if out.len() < total {
        return None;
    }
    put_u64(out, 0, offset);
    put_u32(out, 8, len);
    put_u16(out, 12, suffix.len() as u16);
    put_u16(out, 14, 0);
    out[READ_RANGE_REQUEST_PREFIX_LEN..total].copy_from_slice(suffix);
    Some(total)
}

/// Parse a `ReadRangeRequest` body.
pub fn parse_read_range_request(body: &[u8]) -> Option<ReadRangeRequest<'_>> {
    if body.len() < READ_RANGE_REQUEST_PREFIX_LEN {
        return None;
    }
    let suffix_len = get_u16(body, 12) as usize;
    let end = READ_RANGE_REQUEST_PREFIX_LEN.checked_add(suffix_len)?;
    if body.len() < end {
        return None;
    }
    Some(ReadRangeRequest {
        offset: get_u64(body, 0),
        len: get_u32(body, 8),
        suffix: &body[READ_RANGE_REQUEST_PREFIX_LEN..end],
    })
}

// --- ReadRange reply (success) ----------------------------------------------

/// `ReadRangeReply` wire length (the filled bytes ride in `IpcMsg.handles[0]` as a
/// read-only `MemoryObject` of at most one page).
pub const READ_RANGE_REPLY_LEN: usize = 8;

/// A parsed success `ReadRangeReply`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReadRangeReply {
    /// Valid bytes in `handles[0]` (≤ the requested `len`); the rest of the page,
    /// if any, is zero — a short tail at end-of-file.
    pub content_len: u32,
}

/// Write a success `ReadRangeReply` body; returns its length.
pub fn read_range_reply(out: &mut [u8], content_len: u32) -> Option<usize> {
    if out.len() < READ_RANGE_REPLY_LEN {
        return None;
    }
    put_u32(out, 0, content_len);
    put_u32(out, 4, 0);
    Some(READ_RANGE_REPLY_LEN)
}

/// Parse a success `ReadRangeReply` body.
pub fn parse_read_range_reply(body: &[u8]) -> Option<ReadRangeReply> {
    if body.len() < READ_RANGE_REPLY_LEN {
        return None;
    }
    Some(ReadRangeReply { content_len: get_u32(body, 0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_range_request_round_trips() {
        let mut buf = [0u8; 128];
        let n = read_range_request(&mut buf, 0x2000, 4096, b"system/big-file").unwrap();
        assert_eq!(n, READ_RANGE_REQUEST_PREFIX_LEN + 15);
        let r = parse_read_range_request(&buf[..n]).unwrap();
        assert_eq!(r.offset, 0x2000);
        assert_eq!(r.len, 4096);
        assert_eq!(r.suffix, b"system/big-file");
    }

    #[test]
    fn read_range_reply_round_trips() {
        let mut buf = [0u8; 16];
        let n = read_range_reply(&mut buf, 4096).unwrap();
        assert_eq!(n, READ_RANGE_REPLY_LEN);
        assert_eq!(parse_read_range_reply(&buf[..n]).unwrap(), ReadRangeReply { content_len: 4096 });
        // A short end-of-file tail.
        read_range_reply(&mut buf, 100).unwrap();
        assert_eq!(parse_read_range_reply(&buf).unwrap().content_len, 100);
    }

    #[test]
    fn parse_rejects_truncated() {
        assert!(parse_read_range_request(&[0u8; 8]).is_none());
        // suffix_len claims 50 bytes but the body is short.
        let mut buf = [0u8; 32];
        super::put_u16(&mut buf, 12, 50);
        assert!(parse_read_range_request(&buf).is_none());
        assert!(parse_read_range_reply(&[0u8; 4]).is_none());
    }
}
