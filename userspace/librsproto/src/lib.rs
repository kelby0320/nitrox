//! `librsproto` — the resource-server protocol wire codec.
//!
//! A pure byte codec for the protocol every userspace resource server speaks over
//! IPC (`docs/spec/rsproto-wire-format.md` + `rsproto-namespace-ops.md`). It is
//! `#![no_std]`, **no `alloc`**, `core` only, and has **no dependencies**: it
//! serialises and parses messages in a **caller-provided buffer** (the
//! `IpcMsg.payload`); transferred handles ride out-of-band in `IpcMsg.handles[]`,
//! managed by the caller, so the codec only tracks `handle_count`. Multi-byte
//! integers are little-endian; bodies are byte-serialised explicitly (no
//! `#[repr(packed)]` field references) so the codec is robust regardless of
//! alignment.
//!
//! Slice 7 builds the wire codec + the **server-side** path (parse a request,
//! build a reply) that `fs-server-ext4` uses. A synchronous client (`RsClient`)
//! is deferred to its first consumer (eshell).
//!
//! Under `cargo test` the crate compiles as host `std` so the round-trip logic is
//! unit-tested.

#![cfg_attr(not(test), no_std)]

pub mod auth;
pub mod error;
pub mod file;
pub mod log;
pub mod meta;
pub mod namespace;

// --- Envelope (RsMsgHeader) -------------------------------------------------

/// The protocol magic: ASCII `"RSMG"`. Every message begins with it.
pub const RS_MAGIC: u32 = 0x5253_4D47;
/// Wire size of [`RsMsgHeader`].
pub const RS_HEADER_LEN: usize = 28;
/// Protocol version this codec speaks.
pub const RS_VERSION: u16 = 1;

/// `RsFlags` — message-envelope flags.
pub const RS_FLAG_REPLY: u32 = 1 << 0;
/// This reply is an error (body is an [`error::ErrorBody`]).
pub const RS_FLAG_ERROR: u32 = 1 << 1;
/// Last message of a streaming response.
pub const RS_FLAG_LAST_IN_STREAM: u32 = 1 << 2;

// Operation discriminants (`category << 8 | specific`). See the wire spec.
/// `Meta::Hello` — version negotiation.
pub const OP_HELLO: u16 = 0x0000;
/// `Meta::Goodbye` — clean shutdown.
pub const OP_GOODBYE: u16 = 0x0001;
/// `Meta::QueryCaps` — capability discovery.
pub const OP_QUERY_CAPS: u16 = 0x0002;
/// `Meta::Ping` — liveness.
pub const OP_PING: u16 = 0x0003;
/// `Meta::Ready` — startup signal on the control channel.
pub const OP_READY: u16 = 0x0004;
/// `Namespace::Resolve` — resolve a path suffix to a resource handle.
pub const OP_NS_RESOLVE: u16 = 0x0100;
/// `File::ReadRange` — read a byte range of a lazily-resolved file (the Model-B
/// page-cache fill). See [`file`].
pub const OP_FILE_READ_RANGE: u16 = 0x0600;
/// `File::ReadDir` — read a batch of entries from an open **directory handle** (a
/// session channel scoped to one directory; see [`file`] and
/// `docs/spec/rsproto-file-ops.md`). Client-initiated, sent on the directory channel;
/// the reply rides back on the same channel.
pub const OP_FILE_READ_DIR: u16 = 0x0601;
/// `Auth::Authenticate` — validate a `(username, password)` credential. See [`auth`].
pub const OP_AUTHENTICATE: u16 = 0x0800;

/// A decoding/validation failure.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RsError {
    /// The buffer is shorter than the header (or the declared body).
    Truncated,
    /// The magic word is not `RS_MAGIC`.
    BadMagic,
    /// A field (e.g. a length prefix) runs past the buffer.
    BadLength,
}

/// A decoded message envelope: the parsed header fields plus a borrow of the
/// `body_len` body bytes that follow the header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Message<'a> {
    pub version: u16,
    pub op: u16,
    pub request_id: u64,
    pub flags: u32,
    pub handle_count: u16,
    /// The operation body (exactly `body_len` bytes).
    pub body: &'a [u8],
}

impl Message<'_> {
    /// `true` iff this is a reply (`RS_FLAG_REPLY`).
    pub fn is_reply(&self) -> bool {
        self.flags & RS_FLAG_REPLY != 0
    }
    /// `true` iff this is an error reply (`RS_FLAG_ERROR`).
    pub fn is_error(&self) -> bool {
        self.flags & RS_FLAG_ERROR != 0
    }
}

/// Encode a message header + `body` into `out`, returning the total byte length
/// written (header + body), or `None` if `out` is too small. The caller sets
/// `IpcMsg.header.payload_len` to the returned length and `handle_count` itself.
pub fn encode(
    out: &mut [u8],
    op: u16,
    request_id: u64,
    flags: u32,
    body: &[u8],
    handle_count: u16,
) -> Option<usize> {
    let total = RS_HEADER_LEN.checked_add(body.len())?;
    if out.len() < total {
        return None;
    }
    put_u32(out, 0, RS_MAGIC);
    put_u16(out, 4, RS_VERSION);
    put_u16(out, 6, op);
    put_u64(out, 8, request_id);
    put_u32(out, 16, flags);
    put_u32(out, 20, body.len() as u32);
    put_u16(out, 24, handle_count);
    put_u16(out, 26, 0); // _reserved
    out[RS_HEADER_LEN..total].copy_from_slice(body);
    Some(total)
}

/// Parse a received `IpcMsg` payload (`buf`) into its envelope + body. Validates
/// the magic and that the declared `body_len` fits within `buf`.
pub fn decode(buf: &[u8]) -> Result<Message<'_>, RsError> {
    if buf.len() < RS_HEADER_LEN {
        return Err(RsError::Truncated);
    }
    if get_u32(buf, 0) != RS_MAGIC {
        return Err(RsError::BadMagic);
    }
    let body_len = get_u32(buf, 20) as usize;
    let end = RS_HEADER_LEN.checked_add(body_len).ok_or(RsError::BadLength)?;
    if buf.len() < end {
        return Err(RsError::BadLength);
    }
    Ok(Message {
        version: get_u16(buf, 4),
        op: get_u16(buf, 6),
        request_id: get_u64(buf, 8),
        flags: get_u32(buf, 16),
        handle_count: get_u16(buf, 24),
        body: &buf[RS_HEADER_LEN..end],
    })
}

// --- little-endian byte helpers (shared by the body codecs) -----------------

pub(crate) fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn get_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
pub(crate) fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
pub(crate) fn get_u64(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips() {
        let mut buf = [0u8; 64];
        let body = [1u8, 2, 3, 4, 5];
        let n = encode(&mut buf, OP_NS_RESOLVE, 0xDEAD_BEEF, RS_FLAG_REPLY, &body, 1).unwrap();
        assert_eq!(n, RS_HEADER_LEN + body.len());
        let m = decode(&buf[..n]).unwrap();
        assert_eq!(m.op, OP_NS_RESOLVE);
        assert_eq!(m.request_id, 0xDEAD_BEEF);
        assert_eq!(m.flags, RS_FLAG_REPLY);
        assert!(m.is_reply() && !m.is_error());
        assert_eq!(m.handle_count, 1);
        assert_eq!(m.body, &body);
        assert_eq!(m.version, RS_VERSION);
    }

    #[test]
    fn decode_rejects_bad_magic_and_truncation() {
        assert_eq!(decode(&[0u8; 10]), Err(RsError::Truncated));
        let mut buf = [0u8; 28];
        // Valid length but zero magic.
        assert_eq!(decode(&buf), Err(RsError::BadMagic));
        // Good magic but body_len overruns the buffer.
        put_u32(&mut buf, 0, RS_MAGIC);
        put_u32(&mut buf, 20, 100);
        assert_eq!(decode(&buf), Err(RsError::BadLength));
    }

    #[test]
    fn encode_refuses_small_buffer() {
        let mut buf = [0u8; 16]; // smaller than the header
        assert!(encode(&mut buf, OP_PING, 0, 0, &[], 0).is_none());
    }
}
