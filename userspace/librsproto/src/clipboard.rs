//! `Clipboard` — the kill ring, served at `/dev/clipboard`.
//!
//! **A store, not a broker.** The clipboard outlives the application you copied from: copy
//! from the editor, close it, paste.
//!
//! **The binding is the authority.** A read succeeds for anyone holding this endpoint: a
//! namespace without `/dev/clipboard` has no clipboard, and a `SYS_HANDLE_RESTRICT` to
//! `RIGHT_SEND` alone is a profile that can copy and not read. Focus gating is deferred
//! (`docs/rationale/deferred-decisions.md`).
//!
//! ## The ring
//!
//! The server keeps the last [`CLIP_RING`] entries, most recent first, and a
//! [`Copy`](OP_CLIP_COPY) pushes. **The ring is shared** by every holder of the endpoint;
//! **the position in it is not**: the server answers by *index* and holds no per-client
//! state, so applications cycling at once cannot fight over one cursor. An ordinary paste asks
//! for index 0; cycling is a *continuation* of a paste (Emacs's `M-y` rule).
//!
//! ## Staleness
//!
//! A pipeline can push mid-cycle, so every entry comes back with the ring's
//! [`serial`](ClipEntry::serial), and a cycle carries the serial it last saw in
//! [`ClipPaste::expect`]. If the ring has moved the server refuses with [`CLIP_ERR_STALE`] and
//! the client restarts from the newest.
//!
//! ## What the server must never log
//!
//! Counts, serials, lengths and kinds — never bytes: a clipboard eventually holds a password,
//! and the serial console is a log file.

use crate::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64};

/// `Clipboard::Copy` — push an entry onto the ring. Body: [`ClipEntry`] without its serial.
///
/// The reply is the ring's new [`serial`](ClipEntry::serial) as eight little-endian bytes.
pub const OP_CLIP_COPY: u16 = 0x0D00;

/// `Clipboard::Paste` — read one entry by index. Body: [`ClipPaste`].
///
/// Index 0 is the newest. The reply is a [`ClipEntry`]; an index past the end is
/// `KError::NotFound`, and a stale [`expect`](ClipPaste::expect) is [`CLIP_ERR_STALE`].
pub const OP_CLIP_PASTE: u16 = 0x0D01;

/// `Clipboard::List` — describe the ring without reading it. Reply: [`write_list`]'s body.
///
/// **Metadata only, as a size decision rather than a privacy one** (anyone who can send this
/// can `Paste` every index): [`CLIP_RING`] entries of [`MAX_CLIP_BYTES`] do not fit one message.
pub const OP_CLIP_LIST: u16 = 0x0D02;

/// `server_code`: the ring moved under a cycle — [`ClipPaste::expect`] is not its serial.
///
/// **A `server_code`, not a new `KError`** (`docs/reference/error-codes.md`): one server
/// produces it. The `kerror` beside it is `InvalidArgument`.
pub const CLIP_ERR_STALE: u32 = 1;

/// `server_code`: the request body did not decode. Paired with `InvalidArgument`.
pub const CLIP_ERR_MALFORMED: u32 = 2;

/// How many entries the ring keeps.
///
/// 16 × ~4 KiB is 64 KiB of server memory at worst.
pub const CLIP_RING: usize = 16;

/// `text/plain`, UTF-8. The only kind that exists today.
///
/// The tag exists so a second kind is not a second clipboard.
pub const CLIP_KIND_TEXT: u16 = 0;

/// A list of absolute paths, and what was meant to happen to them.
///
/// The bytes are UTF-8 and newline-separated: a **verb** line, then one absolute path per line.
/// The verb is `cut` or `copy`. See [`encode_paths`] and [`Paths`].
///
/// **The verb is on the wire, not in the browser**, so a cut in one window and a paste in
/// another is the same gesture.
pub const CLIP_KIND_PATH: u16 = 1;

/// What a [`CLIP_KIND_PATH`] entry says should happen to its paths.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathVerb {
    /// The paths are to be copied, and stay where they are.
    Copy,
    /// The paths are to be moved, and the originals go.
    Cut,
}

impl PathVerb {
    /// The word this verb is spelled with on the wire.
    pub fn word(self) -> &'static str {
        match self {
            PathVerb::Copy => "copy",
            PathVerb::Cut => "cut",
        }
    }
}

/// Write `paths` into `out` as a [`CLIP_KIND_PATH`] payload; returns how many bytes were used.
/// `None` if the payload would not fit, rather than a truncated list: half a path is a path to
/// somewhere else. **Absolute paths are the caller's rule**: a relative one would be read
/// against the *reader's* directory.
pub fn encode_paths(verb: PathVerb, paths: &[&str], out: &mut [u8]) -> Option<usize> {
    let word = verb.word().as_bytes();
    let mut n = word.len() + paths.iter().map(|p| 1 + p.len()).sum::<usize>();
    if n > out.len() || n > MAX_CLIP_BYTES {
        return None;
    }
    out[..word.len()].copy_from_slice(word);
    n = word.len();
    for p in paths {
        out[n] = b'\n';
        n += 1;
        out[n..n + p.len()].copy_from_slice(p.as_bytes());
        n += p.len();
    }
    Some(n)
}

/// A decoded [`CLIP_KIND_PATH`] payload: the verb, and the paths after it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Paths<'a> {
    /// What should happen to them.
    pub verb: PathVerb,
    /// The rest of the payload, still newline-separated — walk it with [`Paths::iter`].
    rest: &'a str,
}

impl<'a> Paths<'a> {
    /// Read a payload, or `None` if the verb is not one of the two.
    ///
    /// An unknown verb is not a copy: a future third verb would silently duplicate files.
    pub fn read(bytes: &'a [u8]) -> Option<Paths<'a>> {
        let text = core::str::from_utf8(bytes).ok()?;
        let (head, rest) = match text.split_once('\n') {
            Some((h, r)) => (h, r),
            None => (text, ""),
        };
        let verb = match head {
            "copy" => PathVerb::Copy,
            "cut" => PathVerb::Cut,
            _ => return None,
        };
        Some(Paths { verb, rest })
    }

    /// The paths, in the order they were pushed. Empty lines are skipped.
    pub fn iter(&self) -> impl Iterator<Item = &'a str> {
        self.rest.split('\n').filter(|p| !p.is_empty())
    }
}

/// Wire size of an entry's fixed head: `serial` (8), `kind` (2), `len` (4), 2 bytes padding.
pub const CLIP_ENTRY_HEAD: usize = 16;

/// Wire size of [`ClipPaste`].
pub const CLIP_PASTE_LEN: usize = 16;

/// The largest entry the ring will take, in bytes.
///
/// **Derived, not chosen**: one IPC payload less the rsproto envelope and the entry head.
///
/// **Chunking is expected**: `TODO(clipboard-chunking)`, fired by the first thing somebody
/// cannot copy.
///
/// [`MAX_EVENT_BODY`]: crate::surface::MAX_EVENT_BODY
pub const MAX_CLIP_BYTES: usize = crate::RS_MAX_PAYLOAD - crate::RS_HEADER_LEN - CLIP_ENTRY_HEAD;

/// "Whatever the ring holds now" — the [`expect`](ClipPaste::expect) of an ordinary paste.
///
/// Zero is never a real serial: the ring starts at 1 and every push increments.
pub const CLIP_ANY_SERIAL: u64 = 0;

/// One entry, on the wire.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClipEntry<'a> {
    /// The ring's serial **at the time this entry was read**, not the entry's own age.
    pub serial: u64,
    /// What kind of thing the bytes are — [`CLIP_KIND_TEXT`] today.
    pub kind: u16,
    /// The bytes. UTF-8 for [`CLIP_KIND_TEXT`], **not validated here**.
    pub bytes: &'a [u8],
}

impl<'a> ClipEntry<'a> {
    /// Serialise into `out`; `None` if it does not fit or the payload is over [`MAX_CLIP_BYTES`].
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
    /// **The declared length is checked against what arrived**, not trusted.
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
    /// A cycling client passes the serial its previous paste returned; if the ring has moved
    /// the server refuses rather than answering an index that now means something else.
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
    /// **The declared count is checked against the bytes that arrived** (a peer-controlled
    /// length).
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

/// A connection to the clipboard server — the `/dev/clipboard` session.
///
/// Shaped like [`Desktop`](crate::desktop::Desktop): `connect`, `from_endpoint`, explicit
/// `close`, borrowed buffer.
#[cfg(feature = "io")]
pub struct Clipboard<'a> {
    endpoint: u64,
    buf: &'a mut [u8],
    next_request_id: u64,
}

/// What a `/dev/clipboard` request can fail with.
///
/// Three cases and not one, for [`DesktopError`](crate::desktop::DesktopError)'s reason.
#[cfg(feature = "io")]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClipError {
    /// A syscall failed or the reply did not decode; the payload is the negative return, if any.
    Transport(i32),
    /// The request did not fit the message buffer, or the payload is over [`MAX_CLIP_BYTES`].
    Protocol,
    /// The server refused: its `KError` (or `None` if the error reply carried no body) and its
    /// `server_code`.
    ///
    /// Two refusals share `InvalidArgument` and mean opposite things: a stale cycle is *retry*,
    /// a malformed body *a bug*.
    Refused { kerror: Option<i32>, server_code: u32 },
}

#[cfg(feature = "io")]
impl ClipError {
    /// Whether this is the ring having moved under a cycle — see [`ClipPaste::expect`].
    pub fn is_stale(&self) -> bool {
        matches!(self, ClipError::Refused { server_code, .. } if *server_code == CLIP_ERR_STALE)
    }

    /// Whether this is "no entry at that index": an empty ring, or a cycle that walked off it.
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
            syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, crate::session::DIR_SESSION_RIGHTS)
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
        let len = crate::session::round_trip(self.endpoint, self.buf, request_id, op, body).map_err(|e| match e {
            crate::session::WireError::Transport(c) => ClipError::Transport(c),
            crate::session::WireError::Protocol => ClipError::Protocol,
        })?;
        let off = crate::session::PAYLOAD_OFF;
        let payload = &self.buf[off..off + len];
        let msg = crate::decode(payload).map_err(|_| ClipError::Transport(0))?;
        if msg.flags & crate::RS_FLAG_ERROR != 0 {
            // Parsed as an `ErrorBody`: here the finer code is the answer.
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
        let n = ClipEntry { serial: 0, kind, bytes }.write(&mut body).ok_or(ClipError::Protocol)?;
        let mut out = [0u8; 8];
        let got = self.request(OP_CLIP_COPY, &body[..n], &mut out)?;
        if got < 8 {
            return Err(ClipError::Protocol);
        }
        Ok(u64::from_le_bytes(out))
    }

    /// Read entry `index`, copying its bytes into `out`.
    ///
    /// `len` is the entry's **whole** length even when `out` was too small to hold it, so a
    /// caller cannot silently paste a truncated entry.
    pub fn paste(&mut self, index: u32, expect: u64, out: &mut [u8]) -> Result<(u64, u16, usize), ClipError> {
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

    #[test]
    fn paths_round_trip() {
        let mut buf = [0u8; 256];
        let n = encode_paths(PathVerb::Cut, &["/home/a.txt", "/home/b"], &mut buf).expect("it fits");
        let got = Paths::read(&buf[..n]).expect("it reads");
        assert_eq!(got.verb, PathVerb::Cut);
        let mut seen = got.iter();
        assert_eq!(seen.next(), Some("/home/a.txt"));
        assert_eq!(seen.next(), Some("/home/b"));
        assert_eq!(seen.next(), None, "and nothing after them");
    }

    /// Bytes a correct writer would never produce: the half a round trip cannot reach.
    #[test]
    fn a_reader_refuses_what_it_should() {
        assert!(Paths::read(b"link\n/home/a").is_none());
        assert!(Paths::read(b"\n/home/a").is_none(), "an empty verb is not one either");
        assert!(Paths::read(b"copied\n/home/a").is_none(), "nor a prefix of one");
        let only = Paths::read(b"copy").expect("a bare verb reads");
        assert_eq!(only.iter().count(), 0);
        let padded = Paths::read(b"copy\n/home/a\n\n/home/b\n").expect("reads");
        assert_eq!(padded.iter().count(), 2);
        assert!(Paths::read(&[b'c', b'o', b'p', b'y', b'\n', 0xff, 0xfe]).is_none());
    }

    #[test]
    fn a_payload_that_does_not_fit_is_refused() {
        let mut small = [0u8; 8];
        assert!(encode_paths(PathVerb::Copy, &["/home/a-long-name"], &mut small).is_none());
        let mut buf = [0u8; 64];
        assert!(encode_paths(PathVerb::Copy, &["/a"], &mut buf).is_some(), "and fits when it does");
    }

    use super::*;

    #[test]
    fn an_entry_round_trips() {
        let mut buf = [0u8; 256];
        let n = ClipEntry { serial: 7, kind: CLIP_KIND_TEXT, bytes: b"hello" }.write(&mut buf).unwrap();
        assert_eq!(n, CLIP_ENTRY_HEAD + 5);
        let e = ClipEntry::read(&buf[..n]).unwrap();
        assert_eq!(e.serial, 7);
        assert_eq!(e.kind, CLIP_KIND_TEXT);
        assert_eq!(e.bytes, b"hello");
    }

    #[test]
    fn an_empty_entry_is_a_legal_entry() {
        let mut buf = [0u8; 32];
        let n = ClipEntry { serial: 1, kind: CLIP_KIND_TEXT, bytes: b"" }.write(&mut buf).unwrap();
        assert_eq!(n, CLIP_ENTRY_HEAD);
        assert_eq!(ClipEntry::read(&buf[..n]).unwrap().bytes, b"");
    }

    #[test]
    fn a_declared_length_past_the_message_is_refused() {
        // The length stays inside the cap (100), so the arrival bound is the one tested.
        let mut buf = [0u8; CLIP_ENTRY_HEAD + 4];
        ClipEntry { serial: 1, kind: CLIP_KIND_TEXT, bytes: b"abcd" }.write(&mut buf).unwrap();
        put_u32(&mut buf, 10, 100);
        const { assert!(100 < MAX_CLIP_BYTES, "the cap must not be what refuses this") };
        assert_eq!(ClipEntry::read(&buf), None);
    }

    #[test]
    fn a_declared_length_over_the_cap_is_refused_too() {
        let mut buf = [0u8; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES + 1];
        ClipEntry { serial: 1, kind: CLIP_KIND_TEXT, bytes: b"abcd" }.write(&mut buf).unwrap();
        put_u32(&mut buf, 10, (MAX_CLIP_BYTES + 1) as u32);
        assert!(buf.len() > CLIP_ENTRY_HEAD + MAX_CLIP_BYTES, "the arrival bound must not be what refuses this");
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
        let rows = [ClipInfo { kind: CLIP_KIND_TEXT, len: 12 }, ClipInfo { kind: CLIP_KIND_TEXT, len: 0 }];
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

        put_u32(&mut buf, 8, 9);
        assert_eq!(ClipList::read(&buf[..n]), None);
    }

    #[test]
    fn the_any_serial_sentinel_is_not_a_serial_the_ring_can_reach() {
        assert_eq!(CLIP_ANY_SERIAL, 0);
    }
}
