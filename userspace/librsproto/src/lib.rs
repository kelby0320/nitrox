//! `librsproto` — the resource-server protocol wire codec.
//!
//! A pure byte codec for the protocol every userspace resource server speaks over IPC
//! (`docs/spec/rsproto-wire-format.md` + `rsproto-namespace-ops.md`). It is `#![no_std]`, **no
//! `alloc`**, with **no dependencies**: it serialises and parses messages in a
//! **caller-provided buffer** (the `IpcMsg.payload`); transferred handles ride out-of-band in
//! `IpcMsg.handles[]`, so the codec only tracks `handle_count`. Multi-byte integers are
//! little-endian, byte-serialised explicitly (no `#[repr(packed)]` field references). Under
//! `cargo test` the crate compiles as host `std`.

#![cfg_attr(not(test), no_std)]

pub mod auth;
pub mod clipboard;
pub mod desktop;
pub mod devices;
pub mod error;
pub mod file;
pub mod log;
pub mod meta;
pub mod namespace;
pub mod path;
pub mod services;
#[cfg(feature = "io")]
pub mod session;
pub mod storage;
pub mod surface;
pub mod views;

/// The protocol magic: ASCII `"RSMG"`. Every message begins with it.
pub const RS_MAGIC: u32 = 0x5253_4D47;
/// Wire size of [`RsMsgHeader`].
pub const RS_HEADER_LEN: usize = 28;
/// Protocol version this codec speaks.
pub const RS_VERSION: u16 = 1;

/// The largest rsproto message this codec will build, in bytes — envelope and body together.
///
/// Mirrors `IPC_PAYLOAD_SIZE`, which the `core`-only codec cannot read; an assertion pins the
/// two together whenever `io` is on.
pub const RS_MAX_PAYLOAD: usize = 4008;

#[cfg(feature = "io")]
const _: () = assert!(RS_MAX_PAYLOAD == libkern::abi::IPC_PAYLOAD_SIZE);

/// The largest body one message can carry: [`RS_MAX_PAYLOAD`] less the envelope. Size every
/// buffer a body is built in by this.
///
/// Not the page less its headers: the payload is the page less the IPC header and room for
/// eight handles, sent or not.
pub const RS_MAX_BODY: usize = RS_MAX_PAYLOAD - RS_HEADER_LEN;

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
/// `Meta::Unmount` — a supervisor telling a filesystem server, on its control channel, to
/// record the filesystem cleanly unmounted and exit. Empty body; the server answers with a
/// reply — empty on success, an error body if the state could not be written — then exits.
pub const OP_UNMOUNT: u16 = 0x0005;
/// `Namespace::Resolve` — resolve a path suffix to a resource handle.
pub const OP_NS_RESOLVE: u16 = 0x0100;
/// `File::ReadRange` — read a byte range of a lazily-resolved file (Model-B page-cache fill).
pub const OP_FILE_READ_RANGE: u16 = 0x0600;
/// `File::ReadDir` — read a batch of entries from an open **directory handle** (a session
/// channel scoped to one directory); the reply rides back on it (`rsproto-file-ops.md`).
pub const OP_FILE_READ_DIR: u16 = 0x0601;
/// `File::Mkdir` — create a subdirectory `name` in the open directory handle. See [`file`].
pub const OP_FILE_MKDIR: u16 = 0x0602;
/// `File::Unlink` — remove the regular file `name` from the open directory handle.
pub const OP_FILE_UNLINK: u16 = 0x0603;
/// `File::Rmdir` — remove the empty subdirectory `name` from the open directory handle.
pub const OP_FILE_RMDIR: u16 = 0x0604;
/// `File::Rename` — rename `old` to `new` **within** the open directory handle.
pub const OP_FILE_RENAME: u16 = 0x0605;
/// `File::Touch` — stamp the named file's modification time as "now".
///
/// Addressed by **file id on the server's forwarding endpoint**, sent by the *kernel* after a
/// Model A flush, with **no reply** (`rsproto-file-ops.md` § Touch). See
/// [`file::parse_touch_request`].
pub const OP_FILE_TOUCH: u16 = 0x0606;
/// `File::Forget` — a **server telling the kernel** that a file is about to be freed, sent
/// `SENDMODE_BLOCK` on a forwarding endpoint and answered by the send's `PendingOperation`
/// once no I/O of the file is in flight (`rsproto-file-ops.md` § Forget). See
/// [`file::forget_request`].
pub const OP_FILE_FORGET: u16 = 0x0607;
/// `Input::Events` — a batch of `InputEvent` records from the `input-server` to a consumer.
///
/// Server→consumer, no reply. The body is a whole number of 16-byte records; a batch never
/// splits a `SYN` group (`rsproto-input-ops.md`).
pub const OP_INPUT_EVENTS: u16 = 0x0A00;
/// `Input::Lights` — the keyboard lights a consumer wants on, from it to the `input-server`.
///
/// Consumer→server, no reply. The body is one byte of `LIGHT_*` bits from `libkern::abi`
/// (`rsproto-input-ops.md` § Lights).
pub const OP_INPUT_LIGHTS: u16 = 0x0A01;

// The `Tty` category is `0x0Bxx`; `0x09xx` is `Surface`'s. A channel carries one category, so a
// clash would not show on the wire, but the registry in `rsproto-wire-format.md` keeps them apart.

/// `Tty::ReadLine` — ask the tty for one edited line. The reply body is the line's bytes
/// (no terminator). Completes only when the user finishes a line, so a client may have at
/// most one outstanding.
pub const OP_TTY_READ_LINE: u16 = 0x0B00;
/// `Tty::Read` — take whatever input is available, **unedited**.
///
/// The raw counterpart to [`OP_TTY_READ_LINE`]: completes on any byte, with no line
/// discipline. There is deliberately no `TTY_MODE_RAW` (`rsproto-tty-ops.md` § Read).
pub const OP_TTY_READ: u16 = 0x0B04;
/// `Tty::Write` — write the body's bytes to the terminal; output is a **capability**.
pub const OP_TTY_WRITE: u16 = 0x0B01;
/// `Tty::SetMode` — one body byte of flags; bit 0 is echo.
///
/// Echo belongs to the server, so reading a password does not depend on callers passing `false`.
pub const OP_TTY_SET_MODE: u16 = 0x0B02;
/// `Tty::Close` — the supervisor declaring this tty finished.
///
/// **Revocation, not release**: the server declining to serve the channel is what makes
/// teardown a guarantee (`rsproto-tty-ops.md` § Close).
pub const OP_TTY_CLOSE: u16 = 0x0B03;

/// **Interrupt** — the one message a terminal sends *unsolicited*.
///
/// An event, not a request: the terminal saw `Ctrl-C`; what it means is the client's decision
/// (`rsproto-tty-ops.md` § Interrupt).
pub const OP_TTY_INTERRUPT: u16 = 0x0B05;

/// `Tty::AttachBackend` — hand the server the other end of this terminal.
///
/// Sent by a terminal emulator, moving **one handle**: a channel whose far end it keeps. That
/// terminal's output then arrives on it as [`OP_TTY_OUTPUT`] and its input is whatever the
/// emulator sends back as [`OP_TTY_INPUT`]. Empty body; empty reply on success
/// (`rsproto-tty-ops.md` § AttachBackend).
pub const OP_TTY_ATTACH_BACKEND: u16 = 0x0B06;

/// Server → emulator on a backend channel: **render these bytes.**
///
/// Unsolicited, `request_id` 0, no reply, like [`OP_TTY_INTERRUPT`]; the bytes are already
/// through the line discipline.
pub const OP_TTY_OUTPUT: u16 = 0x0B07;

/// Emulator → server on a backend channel: **the user typed these bytes.**
///
/// Unsolicited, `request_id` 0, no reply; the server runs the line discipline over it as over
/// console input.
pub const OP_TTY_INPUT: u16 = 0x0B08;
/// `Tty::OpenSibling` — mint **another terminal on this terminal's backend**, and hand back its
///
/// A shell gives each stage a terminal on its own backend this way; resolving `/dev/tty` mints
/// one on the *default* backend (`rsproto-tty-ops.md` § OpenSibling).
pub const OP_TTY_OPEN_SIBLING: u16 = 0x0B09;
/// `Tty::Token` — a **one-time token naming this terminal's backend**.
///
/// The view broker redeems it itself ([`OP_TTY_REDEEM`]), so it need not take the caller's
/// word for which terminal it is; only a holder of a terminal on a backend can get one
/// (`rsproto-tty-ops.md` § Token).
pub const OP_TTY_TOKEN: u16 = 0x0B0A;
/// `Tty::Redeem` — **which backend a token was minted for**, once. Request body: the 16 bytes.
/// Reply: the backend's id, a little-endian `u32`; or `NotFound` for a token never minted, already
/// redeemed, or expired. Answered on any terminal: knowing the token is the proof.
pub const OP_TTY_REDEEM: u16 = 0x0B0B;
/// Bytes in a `Tty::Token` — 128 bits; one length for the minting server and the view broker.
pub const TTY_TOKEN_LEN: usize = 16;

/// Bit 0 of `Tty::SetMode`'s flags byte: echo typed characters back.
pub const TTY_MODE_ECHO: u8 = 1 << 0;

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

/// A decoded message envelope: the parsed header fields plus a borrow of the body bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Message<'a> {
    /// Protocol version (`RS_VERSION`).
    pub version: u16,
    /// Operation discriminant (`category << 8 | specific`).
    pub op: u16,
    /// Correlates a reply with its request; 0 for unsolicited messages.
    pub request_id: u64,
    /// `RS_FLAG_*` bits.
    pub flags: u32,
    /// Number of handles transferred out-of-band with the message.
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
/// written (header + body), or `None` if `out` is too small — **or the message too long to
/// send**, a body over [`RS_MAX_BODY`], which the kernel would refuse with nothing to tell the
/// receiver. The caller sets `IpcMsg.header.payload_len` to the returned length and
/// `handle_count` itself.
pub fn encode(out: &mut [u8], op: u16, request_id: u64, flags: u32, body: &[u8], handle_count: u16) -> Option<usize> {
    let total = RS_HEADER_LEN.checked_add(body.len())?;
    if out.len() < total || total > RS_MAX_PAYLOAD {
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

/// Parse a received `IpcMsg` payload into its envelope + body, checking magic and `body_len`.
pub fn decode(buf: &[u8]) -> Result<Message<'_>, RsError> {
    let mut r = Reader::new(buf);
    let header = (|| Some((r.u32()?, r.u16()?, r.u16()?, r.u64()?, r.u32()?, r.u32()?, r.u16()?, r.skip(2)?)))();
    let (magic, version, op, request_id, flags, body_len, handle_count, ()) = header.ok_or(RsError::Truncated)?;
    if magic != RS_MAGIC {
        return Err(RsError::BadMagic);
    }
    let body = r.bytes(body_len as usize).ok_or(RsError::BadLength)?;
    Ok(Message { version, op, request_id, flags, handle_count, body })
}

pub(crate) fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
/// A bounds-checked cursor over a received body: every read answers `None` past the end, so a
/// parser cannot index beyond the bytes it was handed whatever its length guard says.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// A reader at the start of `b`.
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Reader { b, at: 0 }
    }

    /// The next `n` bytes, or `None` (nothing consumed) if fewer remain.
    pub(crate) fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.b.get(self.at..end)?;
        self.at = end;
        Some(s)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.bytes(N)?.try_into().ok()
    }

    /// One byte.
    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.array::<1>()?[0])
    }
    /// A little-endian `u16`.
    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_le_bytes)
    }
    /// A little-endian `u32`.
    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }
    /// A little-endian `u64`.
    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }
    /// A little-endian `i32`.
    pub(crate) fn i32(&mut self) -> Option<i32> {
        self.array().map(i32::from_le_bytes)
    }
    /// A `u8` length, then that many bytes.
    pub(crate) fn lenpfx_u8(&mut self) -> Option<&'a [u8]> {
        let n = self.u8()?;
        self.bytes(n as usize)
    }
    /// A `u16` length, then that many bytes.
    pub(crate) fn lenpfx_u16(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()?;
        self.bytes(n as usize)
    }
    /// Step over `n` bytes.
    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        self.bytes(n).map(|_| ())
    }
    /// Everything not yet read; consumes it.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.at..];
        self.at = self.b.len();
        s
    }
    /// Bytes read so far.
    pub(crate) fn at(&self) -> usize {
        self.at
    }
    /// `Some(())` only if every byte has been read.
    pub(crate) fn finish(&self) -> Option<()> {
        (self.at == self.b.len()).then_some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each read succeeds at exactly its width and answers `None`, consuming nothing, one byte short.
    #[test]
    fn reader_reads_at_the_exact_length_and_refuses_one_short() {
        let b = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(Reader::new(&b[..1]).u8(), Some(1));
        assert_eq!(Reader::new(&b[..2]).u16(), Some(0x0201));
        assert_eq!(Reader::new(&b[..4]).u32(), Some(0x0403_0201));
        assert_eq!(Reader::new(&b).u64(), Some(0x0807_0605_0403_0201));
        assert_eq!(Reader::new(&[0xFE, 0xFF, 0xFF, 0xFF]).i32(), Some(-2));
        assert_eq!(Reader::new(&b[..3]).bytes(3), Some(&b[..3]));
        assert_eq!(Reader::new(&b[..3]).skip(3), Some(()));
        assert_eq!(Reader::new(&[]).u8(), None);
        assert_eq!(Reader::new(&b[..1]).u16(), None);
        assert_eq!(Reader::new(&b[..3]).u32(), None);
        assert_eq!(Reader::new(&b[..7]).u64(), None);
        assert_eq!(Reader::new(&b[..3]).i32(), None);
        assert_eq!(Reader::new(&b[..2]).bytes(3), None);
        assert_eq!(Reader::new(&b[..2]).skip(3), None);
        let mut r = Reader::new(&b[..3]);
        assert_eq!(r.u32(), None);
        assert_eq!(r.at(), 0, "a refused read consumes nothing");
    }

    #[test]
    fn reader_length_prefix_past_the_end_is_refused() {
        assert_eq!(Reader::new(&[2, 0, 7, 8]).lenpfx_u16(), Some(&[7u8, 8][..]));
        assert_eq!(Reader::new(&[2, 0, 7]).lenpfx_u16(), None);
        assert_eq!(Reader::new(&[0xFF, 0xFF, 7]).lenpfx_u16(), None);
        assert_eq!(Reader::new(&[2]).lenpfx_u16(), None);
        assert_eq!(Reader::new(&[2, 7, 8]).lenpfx_u8(), Some(&[7u8, 8][..]));
        assert_eq!(Reader::new(&[3, 7, 8]).lenpfx_u8(), None);
        assert_eq!(Reader::new(&[0, 0]).lenpfx_u16(), Some(&[][..]));
        assert_eq!(Reader::new(&[]).bytes(usize::MAX), None);
    }

    #[test]
    fn reader_tracks_position_rest_and_finish() {
        let mut r = Reader::new(&[1, 2, 3, 4]);
        assert_eq!(r.finish(), None);
        assert_eq!(r.u16(), Some(0x0201));
        assert_eq!(r.at(), 2);
        assert_eq!(r.rest(), &[3, 4]);
        assert_eq!(r.at(), 4);
        assert_eq!(r.finish(), Some(()));
        assert_eq!(r.rest(), &[] as &[u8]);
    }

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
        assert_eq!(decode(&buf), Err(RsError::BadMagic));
        put_u32(&mut buf, 0, RS_MAGIC);
        put_u32(&mut buf, 20, 100);
        assert_eq!(decode(&buf), Err(RsError::BadLength));
    }

    #[test]
    fn encode_refuses_small_buffer() {
        let mut buf = [0u8; 16]; // smaller than the header
        assert!(encode(&mut buf, OP_PING, 0, 0, &[], 0).is_none());
    }

    /// A body one byte over what a message carries is refused, in a buffer with room for it.
    #[test]
    fn encode_refuses_a_body_longer_than_a_message_carries() {
        let mut page = [0u8; 4096 - 24];
        let body = [7u8; RS_MAX_BODY + 1];
        assert_eq!(RS_MAX_BODY, 3980);
        assert!(encode(&mut page, OP_PING, 0, 0, &body[..RS_MAX_BODY + 1], 0).is_none());
        assert_eq!(encode(&mut page, OP_PING, 0, 0, &body[..RS_MAX_BODY], 0), Some(RS_MAX_PAYLOAD));
    }
}

/// Truncation and corruption drivers shared by the modules' parser tests.
#[cfg(test)]
pub(crate) mod fuzz {
    /// Feed `accepts` every prefix of `valid` and require `false` for each strict prefix: for a
    /// body that is only valid whole.
    pub(crate) fn refuses_every_prefix(valid: &[u8], accepts: impl Fn(&[u8]) -> bool) {
        assert!(accepts(valid), "the whole body must parse");
        for n in 0..valid.len() {
            assert!(!accepts(&valid[..n]), "prefix of {n} of {} bytes was accepted", valid.len());
        }
        survives_corruption(valid, accepts);
    }

    /// Feed `parse` every prefix of `valid`, and of `valid` with each byte in turn flipped to
    /// `0xFF`, `0x80` and `0x00`: it must answer, never panic.
    pub(crate) fn survives_corruption(valid: &[u8], parse: impl Fn(&[u8]) -> bool) {
        let mut buf = valid.to_vec();
        for i in 0..valid.len() {
            for flip in [0xFFu8, 0x80, 0x00] {
                buf.copy_from_slice(valid);
                buf[i] = flip;
                for n in 0..=buf.len() {
                    parse(&buf[..n]);
                }
            }
        }
        for n in 0..=valid.len() {
            parse(&valid[..n]);
        }
    }
}
