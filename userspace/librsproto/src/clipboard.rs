//! `Clipboard` — the kill ring, served at `/dev/clipboard`.
//!
//! **A store, not a broker.** Wayland's model is that the copier keeps the data and the
//! compositor arranges a transfer when somebody pastes, so nothing is ever held by a third
//! party. It was considered and rejected for one reason: the clipboard then dies with the
//! application you copied from, which is the behaviour people install clipboard managers to
//! escape. Here, copy from the editor, close it, paste. See `display-arm-plan.md` M12
//! decision 1.
//!
//! **The binding is the authority.** A read succeeds for anyone holding this endpoint, whenever
//! they like — consistent with every other resource in this system, and a real capability story
//! rather than an ambient one: a namespace without `/dev/clipboard` has no clipboard, and a
//! `SYS_HANDLE_RESTRICT` to `RIGHT_SEND` alone is a profile that can copy and not read. Focus
//! gating is what modern desktops do and is deferred; its trigger is written down in
//! `docs/rationale/deferred-decisions.md`.
//!
//! ## The ring, and the two halves of it
//!
//! The server keeps the last [`CLIP_RING`] entries, most recent first, and a [`Copy`](OP_CLIP_COPY)
//! pushes. What makes this a *kill ring* rather than a stack of slots is the division:
//!
//! - **The ring is shared.** Every holder of the endpoint sees the same entries in the same
//!   order.
//! - **The position in it is not.** "Paste the one before that" is a property of the editing
//!   somebody is doing right now, so the server answers by *index* and holds no per-client
//!   state. Two applications cycling at once cannot fight over one cursor, because there is no
//!   cursor to fight over.
//!
//! An ordinary paste asks for index 0 and gets whatever was last copied, by anyone. Cycling is
//! a *continuation* of a paste — valid only immediately after one, replacing what was just
//! inserted, and ended by any other action — which is Emacs's `M-y` rule and is what makes a
//! stale position unreachable: the position exists only inside one uninterrupted gesture.
//!
//! ## Where it can still go stale, and the `u64` that says so
//!
//! A pipeline can push while somebody is mid-cycle ([`clip`](../../coreutils/src/bin/clip.rs) is
//! exactly that), so "uninterrupted gesture" is not enough on its own. Every entry comes back
//! with the ring's [`serial`](ClipEntry::serial), and a cycle carries the serial it last saw in
//! [`ClipPaste::expect`]. If the ring has moved the server refuses — [`CLIP_ERR_STALE`] — and
//! the client starts again from the newest. One `u64`, and it turns a silent wrong paste into a
//! visible restart.
//!
//! ## What the server must never log
//!
//! Counts, serials, lengths and kinds — never bytes. A clipboard holds what a person copied,
//! which on any real machine eventually includes a password, and the serial console is a log
//! file. This is the same rule the compositor follows for keystrokes.

use crate::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64};

/// `Clipboard::Copy` — push an entry onto the ring. Body: [`ClipEntry`] without its serial.
///
/// The reply is the ring's new [`serial`](ClipEntry::serial) as eight little-endian bytes, so a
/// client that copies and then cycles has a serial without a second round trip.
pub const OP_CLIP_COPY: u16 = 0x0D00;

/// `Clipboard::Paste` — read one entry by index. Body: [`ClipPaste`].
///
/// Index 0 is the newest. The reply is a [`ClipEntry`]; an index past the end is
/// `KError::NotFound`, and a stale [`expect`](ClipPaste::expect) is [`CLIP_ERR_STALE`].
pub const OP_CLIP_PASTE: u16 = 0x0D01;

/// `Clipboard::List` — describe the ring without reading it. Reply: [`write_list`]'s body.
///
/// **Metadata only, and that is a size decision rather than a privacy one**: anybody who can
/// send this can send [`Paste`](OP_CLIP_PASTE) for every index, so withholding the bytes
/// protects nothing. What it buys is a reply that fits one message — [`CLIP_RING`] entries of
/// [`MAX_CLIP_BYTES`] each do not.
pub const OP_CLIP_LIST: u16 = 0x0D02;

/// `server_code`: the ring moved under a cycle — [`ClipPaste::expect`] is not its serial.
///
/// **A `server_code`, not a new `KError`**, and that is this system's documented rule rather
/// than a shortcut: *"a `KError` is for a condition more than one component can produce and any
/// client can act on"* (`docs/reference/error-codes.md`). One server produces this. The `kerror`
/// beside it is `InvalidArgument` — the `expect` argument is out of date — and the server code
/// is what keeps that from collapsing into "your request was malformed", which is the
/// diagnosis-of-the-operand mistake the same page exists to prevent.
pub const CLIP_ERR_STALE: u32 = 1;

/// `server_code`: the request body did not decode. Paired with `InvalidArgument`.
pub const CLIP_ERR_MALFORMED: u32 = 2;

/// How many entries the ring keeps.
///
/// Emacs's default kill ring is 60 and holds lines; this holds up to [`MAX_CLIP_BYTES`] per
/// entry, so the bound that matters is the product: 16 × ~4 KiB is 64 KiB of server memory at
/// worst, which is the same order as one small window's buffer. Deep enough that "the one before
/// that" reaches back through a working session, bounded enough that a program pushing in a loop
/// costs a fixed amount.
pub const CLIP_RING: usize = 16;

/// `text/plain`, UTF-8. The only kind that exists today.
///
/// **The tag exists so a second kind is not a second clipboard.** A file path (the browser's cut
/// and paste) or an image is a value in this space, sharing one ring and one authority; a
/// private buffer inside an application would be a clipboard nobody could inspect or attenuate.
/// No other value is defined here, because a constant nothing sends is scaffolding.
pub const CLIP_KIND_TEXT: u16 = 0;

/// Wire size of an entry's fixed head: `serial` (8), `kind` (2), `len` (4), and two bytes of
/// padding so the bytes start 8-aligned.
pub const CLIP_ENTRY_HEAD: usize = 16;

/// Wire size of [`ClipPaste`].
pub const CLIP_PASTE_LEN: usize = 16;

/// The largest entry the ring will take, in bytes.
///
/// **Derived, not chosen**, and spelled the same way at both ends the way [`MAX_EVENT_BODY`]
/// is: one IPC payload, less the rsproto envelope, less the entry's own head. About two screens
/// of terminal text.
///
/// **Chunking is expected rather than hypothetical** — M12 decision 5. The maintainer's own
/// judgement was "we can start with 1, but we may need to end up at 2", so the trigger is
/// written as one: `TODO(clipboard-chunking)`, fired by the first thing somebody cannot copy. A
/// shared memory object was the third option and is not taken, because M10 rejected handle
/// transfer for drops — a refused handle has no clean owner — and this would inherit that
/// question.
///
/// [`MAX_EVENT_BODY`]: crate::surface::MAX_EVENT_BODY
pub const MAX_CLIP_BYTES: usize = crate::RS_MAX_PAYLOAD - crate::RS_HEADER_LEN - CLIP_ENTRY_HEAD;

/// "Whatever the ring holds now" — the [`expect`](ClipPaste::expect) of an ordinary paste.
///
/// Zero is never a real serial: the ring starts at 1 and every push increments, so a client that
/// has not seen one cannot accidentally spell a serial it might match.
pub const CLIP_ANY_SERIAL: u64 = 0;

/// One entry, on the wire.
///
/// Borrowed rather than owned so the server can answer out of its ring and a client can send
/// out of its buffer, neither allocating.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClipEntry<'a> {
    /// The ring's serial **at the time this entry was read**, not the entry's own age.
    ///
    /// It answers "has anything been copied since I looked", which is the only question a
    /// cycling client has. An identity per entry would answer a different one, and the client
    /// would then have to track both.
    pub serial: u64,
    /// What kind of thing the bytes are — [`CLIP_KIND_TEXT`] today.
    pub kind: u16,
    /// The bytes. UTF-8 for [`CLIP_KIND_TEXT`], **not validated here**: this codec's job is the
    /// wire, and a server that rejected invalid UTF-8 would be diagnosing the operand.
    pub bytes: &'a [u8],
}

impl<'a> ClipEntry<'a> {
    /// Serialise into `out`; returns the length written, or `None` if it does not fit or the
    /// payload is over [`MAX_CLIP_BYTES`].
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if self.bytes.len() > MAX_CLIP_BYTES {
            return None;
        }
        let n = CLIP_ENTRY_HEAD + self.bytes.len();
        if out.len() < n {
            return None;
        }
        put_u64(out, 0, self.serial);
        put_u16(out, 8, self.kind);
        put_u32(out, 10, self.bytes.len() as u32);
        out[14..16].fill(0);
        out[CLIP_ENTRY_HEAD..n].copy_from_slice(self.bytes);
        Some(n)
    }

    /// Parse from `b`, borrowing its bytes.
    ///
    /// **The declared length is checked against what arrived**, not trusted: a truncated
    /// message would otherwise slice past the end, and the length is the one field a peer
    /// controls entirely.
    pub fn read(b: &'a [u8]) -> Option<ClipEntry<'a>> {
        if b.len() < CLIP_ENTRY_HEAD {
            return None;
        }
        let len = get_u32(b, 10) as usize;
        if len > MAX_CLIP_BYTES || b.len() < CLIP_ENTRY_HEAD + len {
            return None;
        }
        Some(ClipEntry {
            serial: get_u64(b, 0),
            kind: get_u16(b, 8),
            bytes: &b[CLIP_ENTRY_HEAD..CLIP_ENTRY_HEAD + len],
        })
    }
}

/// A [`Paste`](OP_CLIP_PASTE) request: which entry, and what the caller last saw.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct ClipPaste {
    /// Zero-based, newest first.
    pub index: u32,
    /// The serial the caller is continuing from, or [`CLIP_ANY_SERIAL`] for an ordinary paste.
    ///
    /// A cycling client passes the serial its previous paste returned. If the ring has moved
    /// the server refuses rather than answering an index that now means something else — the
    /// whole point being that a wrong paste is silent and a refusal is not.
    pub expect: u64,
}

impl ClipPaste {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < CLIP_PASTE_LEN {
            return None;
        }
        put_u32(out, 0, self.index);
        out[4..8].fill(0);
        put_u64(out, 8, self.expect);
        Some(CLIP_PASTE_LEN)
    }

    /// Parse from `b`.
    pub fn read(b: &[u8]) -> Option<ClipPaste> {
        if b.len() < CLIP_PASTE_LEN {
            return None;
        }
        Some(ClipPaste { index: get_u32(b, 0), expect: get_u64(b, 8) })
    }
}

/// One row of a [`List`](OP_CLIP_LIST) reply: what an entry is, without what it says.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct ClipInfo {
    /// The kind tag — [`CLIP_KIND_TEXT`] today.
    pub kind: u16,
    /// How many bytes the entry holds.
    pub len: u32,
}

/// Fixed head of a `List` reply: `serial` (8), `count` (4), and four bytes of padding.
pub const CLIP_LIST_HEAD: usize = 16;
/// Wire size of one [`ClipInfo`] row: `kind` (2), padding (2), `len` (4).
pub const CLIP_INFO_LEN: usize = 8;

/// Encode a `List` reply: the ring's serial, then one row per entry, newest first.
pub fn write_list(out: &mut [u8], serial: u64, rows: &[ClipInfo]) -> Option<usize> {
    let n = CLIP_LIST_HEAD + rows.len() * CLIP_INFO_LEN;
    if out.len() < n || rows.len() > CLIP_RING {
        return None;
    }
    put_u64(out, 0, serial);
    put_u32(out, 8, rows.len() as u32);
    out[12..16].fill(0);
    for (i, r) in rows.iter().enumerate() {
        let o = CLIP_LIST_HEAD + i * CLIP_INFO_LEN;
        put_u16(out, o, r.kind);
        out[o + 2..o + 4].fill(0);
        put_u32(out, o + 4, r.len);
    }
    Some(n)
}

/// A decoded `List` reply.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClipList<'a> {
    /// The ring's serial when the list was taken.
    pub serial: u64,
    rows: &'a [u8],
    count: usize,
}

impl<'a> ClipList<'a> {
    /// Parse from `b`.
    ///
    /// **The declared count is checked against the bytes that arrived**, for [`ClipEntry::read`]'s
    /// reason: it is a peer-controlled length, and an iterator built on the claim rather than the
    /// arrival would read past the end.
    pub fn read(b: &'a [u8]) -> Option<ClipList<'a>> {
        if b.len() < CLIP_LIST_HEAD {
            return None;
        }
        let count = get_u32(b, 8) as usize;
        if count > CLIP_RING || b.len() < CLIP_LIST_HEAD + count * CLIP_INFO_LEN {
            return None;
        }
        Some(ClipList { serial: get_u64(b, 0), rows: &b[CLIP_LIST_HEAD..], count })
    }

    /// How many entries the ring holds.
    pub fn count(&self) -> usize {
        self.count
    }

    /// The rows, newest first.
    pub fn entries(&self) -> impl Iterator<Item = ClipInfo> + '_ {
        (0..self.count).map(move |i| {
            let o = i * CLIP_INFO_LEN;
            ClipInfo { kind: get_u16(self.rows, o), len: get_u32(self.rows, o + 4) }
        })
    }
}

// --- The client half --------------------------------------------------------

/// A connection to the clipboard server — the `/dev/clipboard` session.
///
/// Shaped like [`Desktop`](crate::desktop::Desktop) deliberately: same `connect`, same
/// `from_endpoint`, same explicit `close`, and the same borrowed message buffer, so a reader who
/// knows one knows this.
#[cfg(feature = "io")]
pub struct Clipboard<'a> {
    endpoint: u64,
    buf: &'a mut [u8],
    next_request_id: u64,
}

/// What a `/dev/clipboard` request can fail with.
///
/// Three cases and not one, for [`DesktopError`](crate::desktop::DesktopError)'s reason: a dead
/// channel and a refusal have nothing in common except that neither produced an answer, and
/// collapsing them makes a client diagnose the operand for a fault that was never about it.
#[cfg(feature = "io")]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClipError {
    /// A syscall failed, or the reply did not decode. The payload is the syscall's negative
    /// return where there was one.
    Transport(i32),
    /// The request did not fit the message buffer, or the payload is over [`MAX_CLIP_BYTES`].
    Protocol,
    /// The server refused: its `KError` (or `None` if the error reply carried no body) and its
    /// `server_code`.
    ///
    /// **Both halves, because one of them cannot be read alone.** Two refusals here share
    /// `InvalidArgument` and mean opposite things to a client — a stale cycle is *retry from the
    /// newest*, a malformed body is *a bug in this program*. Carrying only the `kerror` would
    /// make them one case at exactly the point a client has to tell them apart.
    Refused { kerror: Option<i32>, server_code: u32 },
}

#[cfg(feature = "io")]
impl ClipError {
    /// Whether this is the ring having moved under a cycle — see [`ClipPaste::expect`].
    ///
    /// A named question rather than a comparison at each call site: every client that cycles has
    /// to ask it, and each would otherwise spell the `kerror`-plus-`server_code` pair for itself
    /// — which is how one of them ends up testing only the half that does not discriminate.
    pub fn is_stale(&self) -> bool {
        matches!(self, ClipError::Refused { server_code, .. } if *server_code == CLIP_ERR_STALE)
    }

    /// Whether this is "there is no entry at that index" — an empty ring, or a cycle that has
    /// walked off the end of it.
    pub fn is_empty(&self) -> bool {
        matches!(
            self,
            ClipError::Refused { kerror: Some(k), .. }
                if *k == libkern::error::KError::NotFound.as_i32()
        )
    }
}

#[cfg(feature = "io")]
impl<'a> Clipboard<'a> {
    /// Resolve `/dev/clipboard` in `ns` and wrap the session it answers with.
    ///
    /// `buf` must be at least [`IPC_MSG_SIZE`](libkern::abi::IPC_MSG_SIZE) bytes.
    pub fn connect(ns: u64, buf: &'a mut [u8]) -> Result<Clipboard<'a>, ClipError> {
        use libkern::abi::IPC_MSG_SIZE;
        use libkern::syscall::{SYS_NS_LOOKUP, syscall4};
        if buf.len() < IPC_MSG_SIZE {
            return Err(ClipError::Protocol);
        }
        let path = b"/dev/clipboard";
        // SAFETY: a valid path slice and a namespace handle this process holds.
        let po = unsafe {
            syscall4(
                SYS_NS_LOOKUP,
                ns,
                path.as_ptr() as u64,
                path.len() as u64,
                crate::session::DIR_SESSION_RIGHTS,
            )
        };
        if po < 0 {
            return Err(ClipError::Transport(po as i32));
        }
        let (status, endpoint) = crate::session::po_wait(po as u64);
        if status < 0 {
            return Err(ClipError::Transport(status));
        }
        if endpoint == 0 {
            return Err(ClipError::Protocol);
        }
        Ok(Clipboard { endpoint, buf, next_request_id: 1 })
    }

    /// Wrap an endpoint resolved elsewhere. Takes ownership: [`close`](Self::close) closes it.
    pub fn from_endpoint(endpoint: u64, buf: &'a mut [u8]) -> Result<Clipboard<'a>, ClipError> {
        if buf.len() < libkern::abi::IPC_MSG_SIZE {
            return Err(ClipError::Protocol);
        }
        Ok(Clipboard { endpoint, buf, next_request_id: 1 })
    }

    /// Close the session's endpoint. Explicit rather than a `Drop`, for `Dir`'s reason.
    pub fn close(self) {
        // SAFETY: closing an endpoint this session owns.
        unsafe { libkern::syscall::syscall1(libkern::syscall::SYS_HANDLE_CLOSE, self.endpoint) };
    }

    /// Send one request and copy its reply body into `out`, returning the length written.
    fn request(&mut self, op: u16, body: &[u8], out: &mut [u8]) -> Result<usize, ClipError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let len = crate::session::round_trip(self.endpoint, self.buf, request_id, op, body)
            .map_err(|e| match e {
                crate::session::WireError::Transport(c) => ClipError::Transport(c),
                crate::session::WireError::Protocol => ClipError::Protocol,
            })?;
        let off = crate::session::PAYLOAD_OFF;
        let payload = &self.buf[off..off + len];
        let msg = crate::decode(payload).map_err(|_| ClipError::Transport(0))?;
        if msg.flags & crate::RS_FLAG_ERROR != 0 {
            // **Parsed as an `ErrorBody`, not read as four bytes.** `Desktop` takes the shortcut
            // because it has nothing to do with the finer code; here the finer code *is* the
            // answer, so the body goes through the codec that knows its layout.
            let e = crate::error::parse_error(msg.body);
            return Err(ClipError::Refused {
                kerror: e.map(|e| e.kerror),
                server_code: e.map_or(0, |e| e.server_code),
            });
        }
        let n = msg.body.len().min(out.len());
        out[..n].copy_from_slice(&msg.body[..n]);
        Ok(n)
    }

    /// Push `bytes` onto the ring; returns the ring's new serial.
    pub fn copy(&mut self, kind: u16, bytes: &[u8]) -> Result<u64, ClipError> {
        if bytes.len() > MAX_CLIP_BYTES {
            return Err(ClipError::Protocol);
        }
        let mut body = [0u8; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES];
        // The serial field is the server's to fill; a copy has nothing to say about it.
        let n = ClipEntry { serial: 0, kind, bytes }
            .write(&mut body)
            .ok_or(ClipError::Protocol)?;
        let mut out = [0u8; 8];
        let got = self.request(OP_CLIP_COPY, &body[..n], &mut out)?;
        if got < 8 {
            return Err(ClipError::Protocol);
        }
        Ok(u64::from_le_bytes(out))
    }

    /// Read entry `index`, copying its bytes into `out`.
    ///
    /// Returns `(serial, kind, len)`. `len` is the entry's **whole** length even when `out` was
    /// too small to hold it — a caller that silently pasted a truncated entry would be the
    /// quietest possible way to corrupt somebody's text, so the number to compare against `out`
    /// comes back rather than being inferred.
    pub fn paste(
        &mut self,
        index: u32,
        expect: u64,
        out: &mut [u8],
    ) -> Result<(u64, u16, usize), ClipError> {
        let mut body = [0u8; CLIP_PASTE_LEN];
        let n = ClipPaste { index, expect }.write(&mut body).ok_or(ClipError::Protocol)?;
        let mut reply = [0u8; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES];
        let got = self.request(OP_CLIP_PASTE, &body[..n], &mut reply)?;
        let entry = ClipEntry::read(&reply[..got]).ok_or(ClipError::Protocol)?;
        let take = entry.bytes.len().min(out.len());
        out[..take].copy_from_slice(&entry.bytes[..take]);
        Ok((entry.serial, entry.kind, entry.bytes.len()))
    }

    /// Describe the ring: its serial, and one `(kind, len)` per entry, newest first.
    ///
    /// `rows` is filled from the start; the returned count is how many were written, which is
    /// `min(ring, rows.len())`.
    pub fn list(&mut self, rows: &mut [ClipInfo]) -> Result<(u64, usize), ClipError> {
        let mut reply = [0u8; CLIP_LIST_HEAD + CLIP_RING * CLIP_INFO_LEN];
        let got = self.request(OP_CLIP_LIST, &[], &mut reply)?;
        let list = ClipList::read(&reply[..got]).ok_or(ClipError::Protocol)?;
        let mut n = 0;
        for (i, e) in list.entries().enumerate() {
            if i >= rows.len() {
                break;
            }
            rows[i] = e;
            n += 1;
        }
        Ok((list.serial, n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_round_trips() {
        let mut buf = [0u8; 256];
        let n = ClipEntry { serial: 7, kind: CLIP_KIND_TEXT, bytes: b"hello" }
            .write(&mut buf)
            .unwrap();
        assert_eq!(n, CLIP_ENTRY_HEAD + 5);
        let e = ClipEntry::read(&buf[..n]).unwrap();
        assert_eq!(e.serial, 7);
        assert_eq!(e.kind, CLIP_KIND_TEXT);
        assert_eq!(e.bytes, b"hello");
    }

    #[test]
    fn an_empty_entry_is_a_legal_entry() {
        // Copying an empty selection is a thing a person can do, and a codec that refused it
        // would make the application decide what to do about a case the ring can hold fine.
        let mut buf = [0u8; 32];
        let n = ClipEntry { serial: 1, kind: CLIP_KIND_TEXT, bytes: b"" }.write(&mut buf).unwrap();
        assert_eq!(n, CLIP_ENTRY_HEAD);
        assert_eq!(ClipEntry::read(&buf[..n]).unwrap().bytes, b"");
    }

    #[test]
    fn a_declared_length_past_the_message_is_refused() {
        // **The peer controls this field.** A reader that trusted it would slice past the end
        // of what arrived — the whole class the `read` bound exists for. Handing the reader
        // bytes a correct writer would never produce is the only way to test it: a round-trip
        // test cannot, because the writer and the reader would agree about the lie.
        //
        // **The length has to be *inside* the cap**, or this tests the wrong half. `read`
        // refuses on two conditions — over `MAX_CLIP_BYTES`, and past what arrived — and a
        // stamped 4000 trips the first, so the arrival bound this test is named for was never
        // reached and could be deleted with the suite still green (PR #271 review, blocking 2).
        // 100 is a legal entry length and twenty bytes is not a hundred.
        let mut buf = [0u8; CLIP_ENTRY_HEAD + 4];
        ClipEntry { serial: 1, kind: CLIP_KIND_TEXT, bytes: b"abcd" }.write(&mut buf).unwrap();
        put_u32(&mut buf, 10, 100);
        assert!(100 < MAX_CLIP_BYTES, "the cap must not be what refuses this");
        assert_eq!(ClipEntry::read(&buf), None);
    }

    #[test]
    fn a_declared_length_over_the_cap_is_refused_too() {
        // The other half, kept separate so neither can stand in for the other — **and the
        // buffer really has to be long enough to hold what it claims**, or the arrival bound
        // refuses it first and this tests nothing. The first version of this test used a
        // twenty-byte buffer and passed with the cap check deleted; the control caught it,
        // which is the same lesson the review had just delivered one test along.
        let mut buf = [0u8; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES + 1];
        ClipEntry { serial: 1, kind: CLIP_KIND_TEXT, bytes: b"abcd" }.write(&mut buf).unwrap();
        put_u32(&mut buf, 10, (MAX_CLIP_BYTES + 1) as u32);
        assert!(
            buf.len() >= CLIP_ENTRY_HEAD + MAX_CLIP_BYTES + 1,
            "the arrival bound must not be what refuses this"
        );
        assert_eq!(ClipEntry::read(&buf), None);
    }

    #[test]
    fn an_entry_over_the_cap_does_not_encode() {
        let big = [b'x'; MAX_CLIP_BYTES + 1];
        let mut buf = [0u8; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES + 8];
        assert_eq!(ClipEntry { serial: 1, kind: CLIP_KIND_TEXT, bytes: &big }.write(&mut buf), None);
    }

    #[test]
    fn the_cap_leaves_an_entry_inside_one_payload() {
        // The derivation is the promise: an entry at the cap, wrapped in an envelope, fits the
        // IPC payload with nothing left over to be wrong about.
        assert_eq!(CLIP_ENTRY_HEAD + MAX_CLIP_BYTES + crate::RS_HEADER_LEN, crate::RS_MAX_PAYLOAD);
    }

    #[test]
    fn a_paste_request_round_trips() {
        let mut buf = [0u8; CLIP_PASTE_LEN];
        ClipPaste { index: 3, expect: 99 }.write(&mut buf).unwrap();
        let p = ClipPaste::read(&buf).unwrap();
        assert_eq!(p.index, 3);
        assert_eq!(p.expect, 99);
    }

    #[test]
    fn a_list_round_trips_and_a_declared_count_past_the_body_is_refused() {
        let rows = [
            ClipInfo { kind: CLIP_KIND_TEXT, len: 12 },
            ClipInfo { kind: CLIP_KIND_TEXT, len: 0 },
        ];
        let mut buf = [0u8; 64];
        let n = write_list(&mut buf, 5, &rows).unwrap();
        let l = ClipList::read(&buf[..n]).unwrap();
        assert_eq!(l.serial, 5);
        assert_eq!(l.count(), 2);
        let mut got = [ClipInfo::default(); 2];
        for (i, e) in l.entries().enumerate() {
            got[i] = e;
        }
        assert_eq!(got, rows);

        // The same peer-controlled-length class as the entry's.
        put_u32(&mut buf, 8, 9);
        assert_eq!(ClipList::read(&buf[..n]), None);
    }

    #[test]
    fn the_any_serial_sentinel_is_not_a_serial_the_ring_can_reach() {
        // The ring starts at 1 and only increments, so this is a fact about the server rather
        // than a coincidence — asserted here because the *protocol* is what promises it.
        assert_eq!(CLIP_ANY_SERIAL, 0);
    }
}
