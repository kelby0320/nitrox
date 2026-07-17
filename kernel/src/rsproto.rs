//! Kernel-side mirror of the `librsproto` wire codec — just enough of it for the
//! **kernel↔userspace-server** ABI the forwarded namespace lookup needs (slice 7).
//!
//! When a client `sys_ns_lookup`s a path bound to a **Userspace Server**, the
//! kernel speaks the resource-server protocol *on the client's behalf*: it builds
//! a [`Namespace::Resolve`](build_resolve_request) request, sends it to the
//! server's kernel-held endpoint, and later parses the server's
//! [reply](parse_reply). `librsproto` (`userspace/librsproto/`) is the userspace
//! mirror of the same format; this is a small hand-coded kernel copy because the
//! kernel may not depend on a userspace crate (and `librsproto` pulls in `alloc`).
//! Both sides are pinned by `docs/spec/rsproto-namespace-ops.md` +
//! `rsproto-wire-format.md`; the host tests here round-trip against the documented
//! offsets so the two codecs cannot drift.
//!
//! Only the **server-bound request** (encode) and the **reply** (decode) paths
//! exist — the kernel is purely the *client* of the forwarded Resolve. All
//! multi-byte integers are little-endian.

#![allow(dead_code)] // the constants document the wire format; not all are read

/// Protocol magic: ASCII `"RSMG"`. Every message begins with it.
const RS_MAGIC: u32 = 0x5253_4D47;
/// Wire size of the message envelope (`RsMsgHeader`).
const RS_HEADER_LEN: usize = 28;
/// Protocol version this codec speaks.
const RS_VERSION: u16 = 1;

/// `RsFlags::REPLY` — the message is a reply.
const RS_FLAG_REPLY: u32 = 1 << 0;
/// `RsFlags::ERROR` — an error reply (body is an `ErrorBody`).
const RS_FLAG_ERROR: u32 = 1 << 1;

/// `Namespace::Resolve` operation discriminant (`category 0x01 << 8 | 0x00`).
const OP_NS_RESOLVE: u16 = 0x0100;
/// `File::ReadRange` operation discriminant (`category 0x06 << 8 | 0x00`) — the
/// Model-B page-cache fill (`docs/spec/rsproto-file-ops.md`).
const OP_FILE_READ_RANGE: u16 = 0x0600;

/// `RESOLVE_FILE_AS_MEMOBJ` — resolve a regular file to a read-only `MemoryObject`
/// of its content, eagerly (slice 7).
const RESOLVE_FILE_AS_MEMOBJ: u32 = 1 << 0;
/// `RESOLVE_FILE_LAZY` — resolve a regular file to a lazily page-cache-filled
/// `File` resource ([`OBJECT_KIND_FILE`]); the reply carries the file size and
/// the kernel builds the page-cache object (slice 8). See `build_resolve_request`.
pub const RESOLVE_FILE_LAZY: u32 = 1 << 1;

/// Reply `object_kind`: `handles[0]` is a read-only `MemoryObject` of file content.
pub const OBJECT_KIND_MEMOBJ: u16 = 1;
/// Reply `object_kind`: a lazily-filled file — `content_len` is the total file
/// size and `handles[0]` is empty; the kernel builds the page-cache object,
/// pointed back at the server, and fills it on demand via `build_read_range_request`.
pub const OBJECT_KIND_FILE: u16 = 4;
/// Reply `object_kind`: `handles[0]` is a live `IpcChannel` endpoint — a connection to
/// the resolving server, not a file. The general "resolve a service path → get a channel
/// to it" case (the logging service's first use); the kernel installs the transferred
/// endpoint like any other resolved handle. `content_len` is unused.
pub const OBJECT_KIND_CHANNEL: u16 = 5;
/// Reply `object_kind`: a **Model A** (block-filesystem) lazy file. `content_len` is the
/// file size; `handles[0]` transfers the block **`DeviceNode`**; the reply body carries the
/// filesystem block size + the file's initial `BlockRun` map (below). The kernel builds a
/// page-cache object that fills each page zero-copy from the device via a block IRP. See
/// `docs/architecture/filesystem-data-path.md`, `docs/spec/rsproto-block-ops.md`.
pub const OBJECT_KIND_FILE_BLOCKS: u16 = 6;

/// Model A resolve-reply body prefix: `ResolveReply` (8) + `block_size` (4) + `run_count`
/// (4). The `run_count` `BlockRun`s follow, 24 bytes each.
const FILE_BLOCKS_PREFIX_LEN: usize = 16;
/// Wire length of one `BlockRun` in a Model A resolve reply.
const BLOCK_RUN_WIRE_LEN: usize = 24;

/// The body (after the envelope header) of a reply message `msg`, or `None` if short.
pub fn reply_body(msg: &[u8]) -> Option<&[u8]> {
    if msg.len() < RS_HEADER_LEN {
        return None;
    }
    let body_len = get_u32(msg, 20) as usize;
    let end = RS_HEADER_LEN.checked_add(body_len)?;
    if msg.len() < end {
        return None;
    }
    Some(&msg[RS_HEADER_LEN..end])
}

/// Parse a Model A resolve reply body's header: `(block_size, run_count)`, or `None` if
/// short. `body` is the rsproto message body (after the envelope header).
pub fn file_blocks_reply_header(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < FILE_BLOCKS_PREFIX_LEN {
        return None;
    }
    Some((get_u32(body, 8), get_u32(body, 12)))
}

/// Read the `i`-th `BlockRun` from a Model A resolve reply body as
/// `(file_block, device_lba, length, flags)`, or `None` if it would run past `body`.
pub fn file_blocks_run(body: &[u8], i: usize) -> Option<(u64, u64, u32, u32)> {
    let off = FILE_BLOCKS_PREFIX_LEN + i * BLOCK_RUN_WIRE_LEN;
    if body.len() < off + BLOCK_RUN_WIRE_LEN {
        return None;
    }
    Some((get_u64(body, off), get_u64(body, off + 8), get_u32(body, off + 16), get_u32(body, off + 20)))
}

/// Fixed prefix of a `ResolveRequest` body (before the suffix bytes).
const RESOLVE_REQUEST_PREFIX_LEN: usize = 16;
/// Wire length of a success `ResolveReply` body.
const RESOLVE_REPLY_LEN: usize = 8;
/// Fixed prefix of an `ErrorBody` (before the optional message).
const ERROR_BODY_LEN: usize = 12;

/// Byte offset of the envelope's `request_id` field (a `u64`) within a message —
/// the kernel stamps it *after* building the request, once the
/// [`UserspaceServerReg`](crate::object::UserspaceServerReg) has assigned one
/// under `SCHED`.
pub const REQUEST_ID_OFFSET: usize = 8;

// --- little-endian byte helpers --------------------------------------------

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn get_u64(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(v)
}

/// Build a `Namespace::Resolve` request (envelope + body) into `out`, returning
/// the total byte length, or `None` if `out` is too small or `suffix` is longer
/// than `u16::MAX`. The `request_id` is written as `0`; the caller stamps the real
/// id with [`stamp_request_id`] once it has been assigned. `requested_rights` is
/// the lookup's requested `Rights` bits; `RESOLVE_FILE_LAZY` is always set (slice 8
/// — files resolve to a demand-filled page-cache object). A server that does not
/// honour the flag replies the eager `OBJECT_KIND_MEMOBJ` instead, which the kernel
/// still installs (the slice-7 path); a server that does honour it replies
/// `OBJECT_KIND_FILE`. `handle_count = 0` (the request carries no handles).
pub fn build_resolve_request(out: &mut [u8], requested_rights: u64, suffix: &[u8]) -> Option<usize> {
    if suffix.len() > u16::MAX as usize {
        return None;
    }
    let body_len = RESOLVE_REQUEST_PREFIX_LEN + suffix.len();
    let total = RS_HEADER_LEN + body_len;
    if out.len() < total {
        return None;
    }
    // Envelope.
    put_u32(out, 0, RS_MAGIC);
    put_u16(out, 4, RS_VERSION);
    put_u16(out, 6, OP_NS_RESOLVE);
    put_u64(out, REQUEST_ID_OFFSET, 0); // stamped later
    put_u32(out, 16, 0); // flags: a request (not a reply)
    put_u32(out, 20, body_len as u32);
    put_u16(out, 24, 0); // handle_count
    put_u16(out, 26, 0); // _reserved
    // Body: ResolveRequest.
    let b = RS_HEADER_LEN;
    put_u64(out, b, requested_rights);
    put_u32(out, b + 8, RESOLVE_FILE_LAZY);
    put_u16(out, b + 12, suffix.len() as u16);
    put_u16(out, b + 14, 0); // _reserved
    out[b + RESOLVE_REQUEST_PREFIX_LEN..total].copy_from_slice(suffix);
    Some(total)
}

/// Overwrite the envelope's `request_id` field in an already-built message.
pub fn stamp_request_id(buf: &mut [u8], request_id: u64) {
    put_u64(buf, REQUEST_ID_OFFSET, request_id);
}

/// A parsed Resolve reply: the correlating `request_id` and the outcome.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReplyView {
    /// Echoes the request's `request_id` (correlates the reply to its lookup).
    pub request_id: u64,
    /// The reply outcome.
    pub kind: ReplyKind,
}

/// The outcome carried by a Resolve [`ReplyView`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReplyKind {
    /// Success: the resource rides in `IpcMsg.handles[0]`. `object_kind` is one of
    /// the `OBJECT_KIND_*` values; `content_len` is the exact resource byte length.
    Success { object_kind: u16, content_len: u32 },
    /// Error: the body was an `ErrorBody`; `kerror` is a `KError` discriminant
    /// (negative).
    Error { kerror: i32 },
    /// A well-formed envelope whose body did not parse as a known Resolve reply
    /// (truncated body / unexpected shape). Treated as a protocol error.
    Malformed,
}

/// Parse a resource-server **reply** message (`buf` = the IPC payload bytes). The
/// message must be a reply (`RS_FLAG_REPLY`); a request, bad magic, or a truncated
/// envelope yields `None`. A reply whose *body* is malformed parses to
/// [`ReplyKind::Malformed`] (the envelope, hence the `request_id`, is still
/// recovered so the kernel can fail the right lookup).
pub fn parse_reply(buf: &[u8]) -> Option<ReplyView> {
    if buf.len() < RS_HEADER_LEN || get_u32(buf, 0) != RS_MAGIC {
        return None;
    }
    let flags = get_u32(buf, 16);
    if flags & RS_FLAG_REPLY == 0 {
        return None; // not a reply
    }
    let request_id = get_u64(buf, REQUEST_ID_OFFSET);
    let body_len = get_u32(buf, 20) as usize;
    let end = RS_HEADER_LEN.checked_add(body_len)?;
    if buf.len() < end {
        return None;
    }
    let body = &buf[RS_HEADER_LEN..end];

    let kind = if flags & RS_FLAG_ERROR != 0 {
        if body.len() < ERROR_BODY_LEN {
            ReplyKind::Malformed
        } else {
            ReplyKind::Error { kerror: get_u32(body, 0) as i32 }
        }
    } else if body.len() < RESOLVE_REPLY_LEN {
        ReplyKind::Malformed
    } else {
        ReplyKind::Success {
            object_kind: get_u16(body, 0),
            content_len: get_u32(body, 4),
        }
    };
    Some(ReplyView { request_id, kind })
}

// --- File::ReadRange (the Model-B page-cache fill) --------------------------

/// Fixed prefix of a `ReadRangeRequest` body (before the suffix bytes).
const READ_RANGE_REQUEST_PREFIX_LEN: usize = 16;
/// Wire length of a success `ReadRangeReply` body.
const READ_RANGE_REPLY_LEN: usize = 8;

/// Build a `File::ReadRange` request (envelope + body) into `out`, returning the
/// total byte length, or `None` if `out` is too small or `suffix` exceeds
/// `u16::MAX`. The kernel sends this to fill one page of a lazily-resolved file:
/// `offset` is the (page-aligned) file offset, `len` the byte count (at most one
/// page), `suffix` the path naming the file (the fill is stateless). The
/// `request_id` is written `0`; the caller stamps it with [`stamp_request_id`].
/// The filled bytes come back in the reply's `handles[0]` (a `MemoryObject`).
pub fn build_read_range_request(
    out: &mut [u8],
    offset: u64,
    len: u32,
    suffix: &[u8],
) -> Option<usize> {
    if suffix.len() > u16::MAX as usize {
        return None;
    }
    let body_len = READ_RANGE_REQUEST_PREFIX_LEN + suffix.len();
    let total = RS_HEADER_LEN + body_len;
    if out.len() < total {
        return None;
    }
    // Envelope.
    put_u32(out, 0, RS_MAGIC);
    put_u16(out, 4, RS_VERSION);
    put_u16(out, 6, OP_FILE_READ_RANGE);
    put_u64(out, REQUEST_ID_OFFSET, 0); // stamped later
    put_u32(out, 16, 0); // flags: a request
    put_u32(out, 20, body_len as u32);
    put_u16(out, 24, 0); // handle_count
    put_u16(out, 26, 0); // _reserved
    // Body: ReadRangeRequest.
    let b = RS_HEADER_LEN;
    put_u64(out, b, offset);
    put_u32(out, b + 8, len);
    put_u16(out, b + 12, suffix.len() as u16);
    put_u16(out, b + 14, 0); // _reserved
    out[b + READ_RANGE_REQUEST_PREFIX_LEN..total].copy_from_slice(suffix);
    Some(total)
}

/// A parsed `File::ReadRange` reply: the correlating `request_id` and outcome.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RangeReplyView {
    /// Echoes the request's `request_id`.
    pub request_id: u64,
    /// The reply outcome.
    pub kind: RangeReplyKind,
}

/// The outcome carried by a [`RangeReplyView`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RangeReplyKind {
    /// Success: the filled bytes ride in `IpcMsg.handles[0]`; `content_len` is the
    /// valid byte count (a short tail at end-of-file is the caller's to zero-pad).
    Success { content_len: u32 },
    /// Error: the body was an `ErrorBody`; `kerror` is a `KError` discriminant.
    Error { kerror: i32 },
    /// A well-formed reply envelope whose body did not parse.
    Malformed,
}

/// Parse a `File::ReadRange` **reply** (`buf` = the IPC payload). Like
/// [`parse_reply`] but for the `ReadRange` body shape; the envelope's `request_id`
/// is recovered even from a malformed body.
pub fn parse_read_range_reply(buf: &[u8]) -> Option<RangeReplyView> {
    if buf.len() < RS_HEADER_LEN || get_u32(buf, 0) != RS_MAGIC {
        return None;
    }
    let flags = get_u32(buf, 16);
    if flags & RS_FLAG_REPLY == 0 {
        return None;
    }
    let request_id = get_u64(buf, REQUEST_ID_OFFSET);
    let body_len = get_u32(buf, 20) as usize;
    let end = RS_HEADER_LEN.checked_add(body_len)?;
    if buf.len() < end {
        return None;
    }
    let body = &buf[RS_HEADER_LEN..end];

    let kind = if flags & RS_FLAG_ERROR != 0 {
        if body.len() < ERROR_BODY_LEN {
            RangeReplyKind::Malformed
        } else {
            RangeReplyKind::Error { kerror: get_u32(body, 0) as i32 }
        }
    } else if body.len() < READ_RANGE_REPLY_LEN {
        RangeReplyKind::Malformed
    } else {
        RangeReplyKind::Success { content_len: get_u32(body, 0) }
    };
    Some(RangeReplyView { request_id, kind })
}

/// The operation discriminant of a well-formed reply envelope, or `None` if `buf`
/// is too short / not `RS_MAGIC`. The forwarding completion path uses it to route
/// a reply to the right parser ([`parse_reply`] for `Resolve`,
/// [`parse_read_range_reply`] for `ReadRange`).
pub fn reply_op(buf: &[u8]) -> Option<u16> {
    if buf.len() < RS_HEADER_LEN || get_u32(buf, 0) != RS_MAGIC {
        return None;
    }
    Some(get_u16(buf, 6))
}

/// `File::ReadRange` op discriminant, exported for the completion router.
pub const READ_RANGE_OP: u16 = OP_FILE_READ_RANGE;
/// `Namespace::Resolve` op discriminant, exported for the completion router.
pub const RESOLVE_OP: u16 = OP_NS_RESOLVE;

#[cfg(test)]
mod tests {
    use super::*;

    // The kernel encoder must agree byte-for-byte with `librsproto`'s decoder
    // (and vice-versa). These tests pin the documented offsets; the userspace
    // crate's own round-trip tests pin the same layout from the other side.

    #[test]
    fn resolve_request_has_documented_layout() {
        let mut buf = [0u8; 128];
        let n = build_resolve_request(&mut buf, 0x8000, b"system/current-generation").unwrap();
        assert_eq!(n, RS_HEADER_LEN + RESOLVE_REQUEST_PREFIX_LEN + 25);
        // Envelope.
        assert_eq!(get_u32(&buf, 0), RS_MAGIC);
        assert_eq!(get_u16(&buf, 4), RS_VERSION);
        assert_eq!(get_u16(&buf, 6), OP_NS_RESOLVE);
        assert_eq!(get_u64(&buf, REQUEST_ID_OFFSET), 0);
        assert_eq!(get_u32(&buf, 16), 0); // a request, no flags
        assert_eq!(get_u32(&buf, 20) as usize, RESOLVE_REQUEST_PREFIX_LEN + 25);
        assert_eq!(get_u16(&buf, 24), 0); // handle_count
        // Body.
        let b = RS_HEADER_LEN;
        assert_eq!(get_u64(&buf, b), 0x8000); // requested_rights
        assert_eq!(get_u32(&buf, b + 8), RESOLVE_FILE_LAZY);
        assert_eq!(get_u16(&buf, b + 12), 25); // suffix_len
        assert_eq!(&buf[b + 16..n], b"system/current-generation");
    }

    #[test]
    fn stamp_request_id_overwrites_the_field() {
        let mut buf = [0u8; 64];
        build_resolve_request(&mut buf, 0, b"x").unwrap();
        stamp_request_id(&mut buf, 0xABCD_1234_5678);
        assert_eq!(get_u64(&buf, REQUEST_ID_OFFSET), 0xABCD_1234_5678);
    }

    #[test]
    fn build_refuses_small_buffer() {
        let mut buf = [0u8; 8];
        assert!(build_resolve_request(&mut buf, 0, b"abc").is_none());
    }

    /// Build a reply the way a server would, to exercise the decoder.
    fn make_reply(request_id: u64, flags: u32, body: &[u8]) -> [u8; 128] {
        let mut buf = [0u8; 128];
        put_u32(&mut buf, 0, RS_MAGIC);
        put_u16(&mut buf, 4, RS_VERSION);
        put_u16(&mut buf, 6, OP_NS_RESOLVE);
        put_u64(&mut buf, REQUEST_ID_OFFSET, request_id);
        put_u32(&mut buf, 16, flags);
        put_u32(&mut buf, 20, body.len() as u32);
        buf[RS_HEADER_LEN..RS_HEADER_LEN + body.len()].copy_from_slice(body);
        buf
    }

    #[test]
    fn parse_success_reply() {
        let mut body = [0u8; RESOLVE_REPLY_LEN];
        put_u16(&mut body, 0, OBJECT_KIND_MEMOBJ);
        put_u32(&mut body, 4, 4096);
        let buf = make_reply(42, RS_FLAG_REPLY, &body);
        let r = parse_reply(&buf[..RS_HEADER_LEN + RESOLVE_REPLY_LEN]).unwrap();
        assert_eq!(r.request_id, 42);
        assert_eq!(
            r.kind,
            ReplyKind::Success { object_kind: OBJECT_KIND_MEMOBJ, content_len: 4096 }
        );
    }

    #[test]
    fn parse_error_reply() {
        let mut body = [0u8; ERROR_BODY_LEN];
        put_u32(&mut body, 0, (-10i32) as u32); // NotFound
        let buf = make_reply(7, RS_FLAG_REPLY | RS_FLAG_ERROR, &body);
        let r = parse_reply(&buf[..RS_HEADER_LEN + ERROR_BODY_LEN]).unwrap();
        assert_eq!(r.request_id, 7);
        assert_eq!(r.kind, ReplyKind::Error { kerror: -10 });
    }

    #[test]
    fn parse_rejects_non_reply_and_bad_magic() {
        // A request (no REPLY flag) is not a reply.
        let mut req = [0u8; 64];
        build_resolve_request(&mut req, 0, b"x").unwrap();
        assert!(parse_reply(&req).is_none());
        // Bad magic.
        let buf = [0u8; 32];
        assert!(parse_reply(&buf).is_none());
    }

    #[test]
    fn parse_recovers_request_id_on_malformed_body() {
        // A success reply whose body is too short still yields the request_id.
        let buf = make_reply(99, RS_FLAG_REPLY, &[0u8; 2]);
        let r = parse_reply(&buf[..RS_HEADER_LEN + 2]).unwrap();
        assert_eq!(r.request_id, 99);
        assert_eq!(r.kind, ReplyKind::Malformed);
    }

    #[test]
    fn read_range_request_has_documented_layout() {
        let mut buf = [0u8; 128];
        let n = build_read_range_request(&mut buf, 0x2000, 4096, b"system/big-file").unwrap();
        assert_eq!(n, RS_HEADER_LEN + READ_RANGE_REQUEST_PREFIX_LEN + 15);
        // Envelope.
        assert_eq!(get_u32(&buf, 0), RS_MAGIC);
        assert_eq!(get_u16(&buf, 6), OP_FILE_READ_RANGE);
        assert_eq!(get_u64(&buf, REQUEST_ID_OFFSET), 0);
        assert_eq!(get_u32(&buf, 16), 0); // a request
        assert_eq!(get_u32(&buf, 20) as usize, READ_RANGE_REQUEST_PREFIX_LEN + 15);
        // Body.
        let b = RS_HEADER_LEN;
        assert_eq!(get_u64(&buf, b), 0x2000); // offset
        assert_eq!(get_u32(&buf, b + 8), 4096); // len
        assert_eq!(get_u16(&buf, b + 12), 15); // suffix_len
        assert_eq!(&buf[b + 16..n], b"system/big-file");
        // The request_id stamps the same field as Resolve.
        stamp_request_id(&mut buf, 0x55);
        assert_eq!(get_u64(&buf, REQUEST_ID_OFFSET), 0x55);
    }

    #[test]
    fn parse_read_range_success_and_error() {
        let mut body = [0u8; READ_RANGE_REPLY_LEN];
        put_u32(&mut body, 0, 100);
        let mut buf = make_reply(3, RS_FLAG_REPLY, &body);
        // make_reply stamps OP_NS_RESOLVE in the op field; rewrite to ReadRange.
        put_u16(&mut buf, 6, OP_FILE_READ_RANGE);
        let r = parse_read_range_reply(&buf[..RS_HEADER_LEN + READ_RANGE_REPLY_LEN]).unwrap();
        assert_eq!(r, RangeReplyView { request_id: 3, kind: RangeReplyKind::Success { content_len: 100 } });

        let mut ebody = [0u8; ERROR_BODY_LEN];
        put_u32(&mut ebody, 0, (-10i32) as u32);
        let mut ebuf = make_reply(4, RS_FLAG_REPLY | RS_FLAG_ERROR, &ebody);
        put_u16(&mut ebuf, 6, OP_FILE_READ_RANGE);
        let e = parse_read_range_reply(&ebuf[..RS_HEADER_LEN + ERROR_BODY_LEN]).unwrap();
        assert_eq!(e.kind, RangeReplyKind::Error { kerror: -10 });
    }

    #[test]
    fn reply_op_routes_resolve_vs_read_range() {
        let mut req = [0u8; 64];
        build_resolve_request(&mut req, 0, b"x").unwrap();
        assert_eq!(reply_op(&req), Some(RESOLVE_OP));
        build_read_range_request(&mut req, 0, 0, b"x").unwrap();
        assert_eq!(reply_op(&req), Some(READ_RANGE_OP));
        assert_eq!(reply_op(&[0u8; 4]), None); // too short
    }
}
