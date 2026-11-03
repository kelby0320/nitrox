//! The **directory-session client** — the caller's side of the `File` directory
//! operations (`docs/spec/rsproto-file-ops.md`).
//!
//! A directory path resolved through the namespace yields a session `IpcChannel` scoped to one
//! directory inode: the channel *is* the directory, and every op addresses entries **by
//! name**, never by path. [`Dir`] wraps such an endpoint and turns the codecs in
//! [`file`](crate::file) into method calls. It lives here, beside the protocol, and the `io`
//! feature keeps the codec core dependency-free. Each op is a send, a wait on the endpoint, and
//! a receive: the thread parks in the wait, never inside another syscall.

use crate::file::{
    DIR_ENTRY_PREFIX_LEN, DirEntry, NAME_REQUEST_MAX, RENAME_REQUEST_MAX, name_request, parse_read_dir_reply,
    read_dir_request, rename_request,
};
use crate::{
    OP_FILE_MKDIR, OP_FILE_READ_DIR, OP_FILE_RENAME, OP_FILE_RMDIR, OP_FILE_TOUCH, OP_FILE_UNLINK, RS_HEADER_LEN,
};
use libkern::abi::IPC_MSG_SIZE;
use libkern::handle::{RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT};
use libkern::{KError, RawHandle, Rights};
use libos::{CanLookup, Carried, Channel, Deadline, Handle, Msg, Namespace, NsReadOnly, Only, Ref, WaitSet, block_on};

/// Byte offset of the rsproto payload within an `IpcMsg` (the IPC header precedes it).
pub(crate) const PAYLOAD_OFF: usize = 24;
/// `IpcMsgHeader.payload_len` field offset.
const OFF_PAYLOAD_LEN: usize = 4;

/// The rights a directory session needs: send, receive, and `sys_wait` for the reply.
pub const DIR_SESSION_RIGHTS: u64 = RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT;

/// What can go wrong talking to a directory server. `Server` carries the reported `KError`
/// discriminant, so a caller can tell "no such entry" from "directory not empty".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DirError {
    /// The server replied with an error; the payload is its negative `KError` discriminant.
    Server(i32),
    /// A syscall failed (the payload is its negative return), or the reply never came.
    Transport(i32),
    /// The reply could not be parsed, or a name/reply did not fit the message buffer.
    Protocol,
}

/// Result of a directory operation.
pub type Result<T> = core::result::Result<T, DirError>;

/// An open directory session: an endpoint plus the one message its traffic uses.
///
/// The message is caller-provided so this crate stays `alloc`-free; the same one carries the
/// request out and the reply back. A session built by [`resolve`](Self::resolve) or
/// [`new`](Self::new) owns its endpoint and closes it when dropped; one built by the raw
/// [`open`](Self::open) or [`from_endpoint`](Self::from_endpoint) is closed only by
/// [`close`](Self::close).
pub struct Dir<'a> {
    session: Session<'a>,
}

impl<'a> Dir<'a> {
    /// Resolve `path` in the namespace `ns` to a directory session that owns its endpoint, using
    /// `msg` for its traffic.
    ///
    /// Fails with [`DirError::Transport`] if the path does not resolve or resolves to something
    /// that is not a directory.
    pub fn resolve<'n, M: CanLookup + 'n>(
        ns: impl Into<Ref<'n, Namespace, M>>,
        path: &str,
        msg: &'a mut Msg,
    ) -> Result<Dir<'a>> {
        let endpoint = connect(ns, path).map_err(DirError::from)?;
        Ok(Dir::new(endpoint, msg))
    }

    /// Wrap an already-resolved session endpoint, which the session owns and closes.
    pub fn new(endpoint: Handle<Channel, Only>, msg: &'a mut Msg) -> Dir<'a> {
        Dir { session: Session::new(endpoint, msg) }
    }

    /// Resolve `path` in the namespace `ns` to a directory session.
    ///
    /// `buf` must be at least [`IPC_MSG_SIZE`] bytes. Fails with [`DirError::Transport`] if the
    /// path does not resolve or resolves to something that is not a directory. For callers not
    /// yet on libos; the typed form is [`resolve`](Self::resolve).
    // TODO(raw-handle-shims): goes when its last caller is on libos.
    pub fn open(ns: u64, path: &[u8], buf: &'a mut [u8]) -> Result<Dir<'a>> {
        if buf.len() < IPC_MSG_SIZE {
            return Err(DirError::Protocol);
        }
        let path = core::str::from_utf8(path).map_err(|_| DirError::Transport(KError::InvalidArgument.as_i32()))?;
        // SAFETY: the caller's own namespace handle, open for the call, as this has always asked.
        let ns = unsafe { Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(ns), Rights::empty()) };
        let endpoint = connect(ns, path).map_err(DirError::from)?;
        Ok(Dir { session: Session::raw(endpoint.into_raw().0, buf) })
    }

    /// Wrap an already-resolved session endpoint; [`close`](Self::close) closes it. For callers
    /// not yet on libos; the typed form is [`new`](Self::new).
    // TODO(raw-handle-shims): goes when its last caller is on libos.
    pub fn from_endpoint(endpoint: u64, buf: &'a mut [u8]) -> Result<Dir<'a>> {
        if buf.len() < IPC_MSG_SIZE {
            return Err(DirError::Protocol);
        }
        Ok(Dir { session: Session::raw(endpoint, buf) })
    }

    /// The underlying endpoint, borrowed (for waiting on alongside other handles).
    pub fn channel(&self) -> Ref<'_, Channel, Only> {
        self.session.channel()
    }

    /// The underlying endpoint handle (for `sys_wait` alongside other handles). For callers not
    /// yet on libos; the typed form is [`channel`](Self::channel).
    // TODO(raw-handle-shims): goes when its last caller is on libos.
    pub fn endpoint(&self) -> u64 {
        self.session.channel().raw().0
    }

    /// Close the session's endpoint handle, releasing the server-side session.
    pub fn close(self) {
        self.session.close();
    }

    /// Enumerate the directory, calling `f` once per entry — including `.` and `..`.
    /// Pagination is handled internally; return `false` from `f` to stop early (`Ok(false)`).
    /// The entry borrows the reply buffer, so it is valid only for the call.
    pub fn read_dir(&mut self, mut f: impl FnMut(&DirEntry<'_>) -> bool) -> Result<bool> {
        let mut cursor = 0u64;
        loop {
            let mut body = [0u8; 8];
            let n = read_dir_request(&mut body, cursor).ok_or(DirError::Protocol)?;
            let payload = self.session.call(OP_FILE_READ_DIR, &body[..n]).map_err(DirError::from)?;
            let msg = crate::decode(payload).map_err(|_| DirError::Protocol)?;
            if msg.is_error() {
                return Err(DirError::Server(server_error(msg.body)));
            }
            let (header, entries) = parse_read_dir_reply(msg.body).ok_or(DirError::Protocol)?;
            for entry in entries {
                if !f(&entry) {
                    return Ok(false);
                }
            }
            if header.next_cursor == 0 {
                return Ok(true);
            }
            // A server that neither advances the cursor nor terminates would spin here
            // forever; treat a non-advancing cursor as a protocol fault instead.
            if header.next_cursor <= cursor {
                return Err(DirError::Protocol);
            }
            cursor = header.next_cursor;
        }
    }

    /// Create a subdirectory named `name` in this directory.
    pub fn mkdir(&mut self, name: &[u8]) -> Result<()> {
        self.name_op(OP_FILE_MKDIR, name)
    }

    /// Remove the (non-directory) entry `name`.
    pub fn unlink(&mut self, name: &[u8]) -> Result<()> {
        self.name_op(OP_FILE_UNLINK, name)
    }

    /// Remove the empty subdirectory `name`.
    pub fn rmdir(&mut self, name: &[u8]) -> Result<()> {
        self.name_op(OP_FILE_RMDIR, name)
    }

    /// Stamp `name`'s modification time as "now", where *now* is the **server's** clock: a time
    /// the caller could choose would be forgeable metadata. Not the kernel's `File::Touch`
    /// (`rsproto-file-ops.md` § Touch): this one is name-addressed and returns a status.
    pub fn touch(&mut self, name: &[u8]) -> Result<()> {
        self.name_op(OP_FILE_TOUCH, name)
    }

    /// Rename `old` to `new`, both within this directory.
    pub fn rename(&mut self, old: &[u8], new: &[u8]) -> Result<()> {
        let (body, n) = rename_body(old, new).ok_or(DirError::Protocol)?;
        self.status_op(OP_FILE_RENAME, &body[..n])
    }

    fn name_op(&mut self, op: u16, name: &[u8]) -> Result<()> {
        let (body, n) = name_body(name).ok_or(DirError::Protocol)?;
        self.status_op(op, &body[..n])
    }

    /// A mutation: round-trip, then accept an empty-body success or surface the error.
    fn status_op(&mut self, op: u16, body: &[u8]) -> Result<()> {
        let payload = self.session.call(op, body).map_err(DirError::from)?;
        let msg = crate::decode(payload).map_err(|_| DirError::Protocol)?;
        if msg.is_error() {
            return Err(DirError::Server(server_error(msg.body)));
        }
        Ok(())
    }
}

/// What a round trip on a session channel can fail with, before any protocol on top of it.
/// Not [`DirError`]: this is the transport under *every* session client, each of which maps it
/// into its own error type; a refusal (a well-formed error reply) is not a transport fault.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// A syscall failed; the payload is its negative return.
    Transport(i32),
    /// The request did not fit the buffer, or the reply did not parse as one.
    Protocol,
}

impl From<WireError> for DirError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::Transport(c) => DirError::Transport(c),
            WireError::Protocol => DirError::Protocol,
        }
    }
}

fn transport(e: libos::Error) -> WireError {
    WireError::Transport(e.code().as_i32())
}

/// Resolve `path` in `ns` to a session endpoint: the one lookup under every session client.
pub(crate) fn connect<'n, M: CanLookup + 'n>(
    ns: impl Into<Ref<'n, Namespace, M>>,
    path: &str,
) -> core::result::Result<Handle<Channel, Only>, WireError> {
    let endpoint: Handle<Channel, Only> =
        block_on(ns.into().lookup(path, Rights::from_bits(DIR_SESSION_RIGHTS))).map_err(transport)?;
    if endpoint.raw().0 == 0 {
        // Nothing to close: a handle of 0 names nothing.
        endpoint.into_raw();
        return Err(WireError::Protocol);
    }
    Ok(endpoint)
}

/// Where a session's endpoint and message live.
enum Link<'a> {
    /// An endpoint the session owns, with the caller's message: closed on drop.
    Owned(Handle<Channel, Only>, &'a mut Msg),
    /// A raw endpoint and byte buffer of a caller not yet on libos: closed only by `close`.
    Raw(u64, &'a mut [u8]),
}

/// The endpoint and request ids shared by every session client.
pub(crate) struct Session<'a> {
    link: Link<'a>,
    /// Monotonic per-session request id — echoed by the server, useful in a trace.
    next_request_id: u64,
}

impl<'a> Session<'a> {
    pub(crate) fn new(endpoint: Handle<Channel, Only>, msg: &'a mut Msg) -> Self {
        Session { link: Link::Owned(endpoint, msg), next_request_id: 1 }
    }

    /// `buf` must be at least [`IPC_MSG_SIZE`] bytes: the caller checks.
    pub(crate) fn raw(endpoint: u64, buf: &'a mut [u8]) -> Self {
        Session { link: Link::Raw(endpoint, buf), next_request_id: 1 }
    }

    pub(crate) fn channel(&self) -> Ref<'_, Channel, Only> {
        match &self.link {
            Link::Owned(endpoint, _) => endpoint.as_ref(),
            // SAFETY: the shim's caller keeps the endpoint open until `close`, as it always has.
            Link::Raw(endpoint, _) => unsafe { Ref::from_raw(RawHandle(*endpoint), Rights::empty()) },
        }
    }

    /// Close the endpoint, whichever way the session holds it.
    pub(crate) fn close(self) {
        if let Link::Raw(endpoint, _) = self.link {
            // SAFETY: `close` is where the raw shims' caller gave the endpoint up.
            drop(unsafe { Handle::<Channel, Only>::from_raw(RawHandle(endpoint), Rights::empty()) });
        }
    }

    /// Send one request and wait for its reply; returns the reply's rsproto payload.
    pub(crate) fn call(&mut self, op: u16, body: &[u8]) -> core::result::Result<&[u8], WireError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        match &mut self.link {
            Link::Owned(endpoint, msg) => {
                let n = exchange(endpoint.as_ref(), msg, request_id, op, body)?;
                Ok(&msg.payload()[..n])
            }
            Link::Raw(endpoint, buf) => {
                let n = round_trip(*endpoint, buf, request_id, op, body)?;
                Ok(&buf[PAYLOAD_OFF..PAYLOAD_OFF + n])
            }
        }
    }
}

/// Send one request on `endpoint` and receive its reply into `msg`, returning the reply's
/// rsproto payload length. Shared by every session client, so the send/wait/recv sweep has one
/// copy. **A message with another id is skipped**, or one an earlier call left queued would
/// answer this request; this id on another op is `Protocol`.
pub fn exchange<'e>(
    endpoint: impl Into<Ref<'e, Channel, Only>>,
    msg: &mut Msg,
    request_id: u64,
    op: u16,
    body: &[u8],
) -> core::result::Result<usize, WireError> {
    let endpoint = endpoint.into();
    let len = crate::encode(msg.payload_mut(), op, request_id, 0, body, 0).ok_or(WireError::Protocol)?;
    msg.set_len(len).map_err(|_| WireError::Protocol)?;
    // The sole client of a session channel expects one reply, so the ring always has room and a
    // non-blocking send is correct.
    endpoint.send(msg, &mut Carried::new()).map_err(transport)?;

    loop {
        // The wait is where the thread blocks, never the send or the receive.
        let mut set = WaitSet::<1>::new();
        set.add(endpoint).map_err(transport)?;
        if set.wait(Deadline::NEVER).map_err(transport)?.is_empty() {
            return Err(WireError::Transport(KError::WouldBlock.as_i32()));
        }
        endpoint.recv(msg).map_err(transport)?;
        if let Some(payload_len) = reply_to(msg, request_id, op)? {
            return Ok(payload_len);
        }
    }
}

/// As [`exchange`], on a raw `endpoint` the caller keeps open, with the reply left in `buf` at
/// [`PAYLOAD_OFF`]. A `buf` shorter than [`IPC_MSG_SIZE`] is refused `Protocol`. For callers not
/// yet on libos; the typed form is [`exchange`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn round_trip(
    endpoint: u64,
    buf: &mut [u8],
    request_id: u64,
    op: u16,
    body: &[u8],
) -> core::result::Result<usize, WireError> {
    if buf.len() < IPC_MSG_SIZE {
        return Err(WireError::Protocol);
    }
    // SAFETY: the caller's own endpoint, open for the call, as this has always asked.
    let endpoint = unsafe { Ref::<Channel, Only>::from_raw(RawHandle(endpoint), Rights::empty()) };
    let mut msg = Msg::new();
    let n = exchange(endpoint, &mut msg, request_id, op, body)?;
    buf[OFF_PAYLOAD_LEN..OFF_PAYLOAD_LEN + 4].copy_from_slice(&(n as u32).to_le_bytes());
    buf[PAYLOAD_OFF..PAYLOAD_OFF + n].copy_from_slice(msg.payload());
    Ok(n)
}

/// The payload length of the message in `msg` if it answers `request_id`'s `op`, `None` if it has
/// another id, and `Protocol` for this id on another op: no call left that, and none follows it.
fn reply_to(msg: &Msg, request_id: u64, op: u16) -> core::result::Result<Option<usize>, WireError> {
    let payload = msg.payload();
    if payload.len() < RS_HEADER_LEN {
        return Err(WireError::Protocol);
    }
    let m = crate::decode(payload).map_err(|_| WireError::Protocol)?;
    if m.request_id != request_id {
        return Ok(None);
    }
    if m.op != op {
        return Err(WireError::Protocol);
    }
    Ok(Some(payload.len()))
}

/// The `KError` an error reply carries, or a generic failure if the body is malformed.
fn server_error(body: &[u8]) -> i32 {
    match crate::error::parse_error(body) {
        Some(e) if e.kerror < 0 => e.kerror,
        _ => -1,
    }
}

/// The most entries one `ReadDir` reply can carry — what a caller sizes a per-reply array by.
pub const MAX_REPLY_ENTRIES: usize = crate::RS_MAX_BODY / DIR_ENTRY_PREFIX_LEN;

/// A name request's body for `name`, or `None` for one past [`crate::file::MAX_SESSION_NAME`].
fn name_body(name: &[u8]) -> Option<([u8; NAME_REQUEST_MAX], usize)> {
    let mut body = [0u8; NAME_REQUEST_MAX];
    let n = name_request(&mut body, name)?;
    Some((body, n))
}

/// A `Rename` request's body, or `None` for a name past [`crate::file::MAX_SESSION_NAME`].
fn rename_body(old: &[u8], new: &[u8]) -> Option<([u8; RENAME_REQUEST_MAX], usize)> {
    let mut body = [0u8; RENAME_REQUEST_MAX];
    let n = rename_request(&mut body, old, new)?;
    Some((body, n))
}

/// What a host test needs to stand in for a session's server: a message queued from its end.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;
    use libos::mock;

    /// A namespace whose `path` answers a lookup with an endpoint, and that endpoint's peer.
    pub(crate) fn service(path: &str) -> (Ref<'static, Namespace, NsReadOnly>, Ref<'static, Channel, Only>) {
        let (ns, peer) = mock::answering(path);
        // SAFETY: the mock's handles, open for the test's life.
        unsafe { (Ref::from_raw(RawHandle(ns), Rights::empty()), Ref::from_raw(RawHandle(peer), Rights::empty())) }
    }

    /// Queue at `peer` a message with these header fields, to be read by the client's next wait.
    pub(crate) fn queue(peer: Ref<'_, Channel, Only>, request_id: u64, op: u16, flags: u32, body: &[u8]) {
        let mut msg = Msg::new();
        let n = crate::encode(msg.payload_mut(), op, request_id, flags, body, 0).unwrap();
        msg.set_len(n).unwrap();
        peer.send(&msg, &mut Carried::new()).unwrap();
    }

    /// The request the client sent, as `(request_id, op, body)`.
    pub(crate) fn request(peer: Ref<'_, Channel, Only>) -> (u64, u16, std::vec::Vec<u8>) {
        let mut msg = Msg::new();
        peer.recv(&mut msg).unwrap();
        let m = crate::decode(msg.payload()).unwrap();
        (m.request_id, m.op, m.body.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{queue, request, service};
    use super::*;
    use crate::{OP_FILE_MKDIR, RS_FLAG_ERROR, RS_FLAG_REPLY};
    use libos::mock;

    /// **A session sends a name as long as a FAT holds**, 765 bytes, alone or two to a rename, and
    /// refuses one byte more before sending anything.
    #[test]
    fn a_name_as_long_as_a_fat_holds_is_sent() {
        use crate::file::{MAX_SESSION_NAME, parse_name_request, parse_rename_request};
        let (long, past) = ([b'n'; MAX_SESSION_NAME], [b'n'; MAX_SESSION_NAME + 1]);
        let (body, n) = name_body(&long).unwrap();
        assert_eq!(parse_name_request(&body[..n]), Some(&long[..]));
        let (body, n) = rename_body(&long, &long).unwrap();
        assert_eq!(parse_rename_request(&body[..n]), Some((&long[..], &long[..])));
        assert!(name_body(&past).is_none() && rename_body(&long, &past).is_none());
    }

    /// Only the request's own reply is taken, and a message whose length does not fit is refused.
    #[test]
    fn a_reply_to_another_request_is_skipped() {
        let mut msg = Msg::new();
        let n = crate::encode(msg.payload_mut(), OP_FILE_MKDIR, 7, RS_FLAG_REPLY, b"x", 0).unwrap();
        msg.set_len(n).unwrap();
        assert_eq!(reply_to(&msg, 7, OP_FILE_MKDIR), Ok(Some(n)));
        assert_eq!(reply_to(&msg, 8, OP_FILE_MKDIR), Ok(None));
        assert_eq!(reply_to(&msg, 7, OP_FILE_RMDIR), Err(WireError::Protocol));
        msg.set_len(RS_HEADER_LEN - 1).unwrap();
        assert_eq!(reply_to(&msg, 7, OP_FILE_MKDIR), Err(WireError::Protocol));
    }

    /// Refused before anything is sent: a message one byte short reaches no syscall.
    #[test]
    fn a_round_trip_refuses_a_buffer_shorter_than_a_message() {
        let mut short = [0u8; IPC_MSG_SIZE - 1];
        assert!(matches!(round_trip(0, &mut short, 1, crate::OP_FILE_READ_DIR, &[]), Err(WireError::Protocol)));
    }

    /// A reply to an older request is passed over; the request goes out under this call's id.
    #[test]
    fn a_session_waits_for_its_own_reply_and_owns_its_endpoint() {
        mock::reset();
        let (ns, peer) = service("/dev/dir");
        let mut msg = Msg::new();
        let mut dir = Dir::resolve(ns, "/dev/dir", &mut msg).unwrap();
        let open = mock::open_handles();
        queue(peer, 99, OP_FILE_MKDIR, RS_FLAG_REPLY | RS_FLAG_ERROR, b"");
        queue(peer, 1, OP_FILE_MKDIR, RS_FLAG_REPLY, b"");
        assert_eq!(dir.mkdir(b"d"), Ok(()));
        let (id, op, body) = request(peer);
        assert_eq!((id, op), (1, OP_FILE_MKDIR));
        assert_eq!(crate::file::parse_name_request(&body), Some(&b"d"[..]));
        queue(peer, 2, OP_FILE_MKDIR, RS_FLAG_REPLY, b"");
        assert_eq!(dir.mkdir(b"e"), Ok(()));
        assert_eq!(request(peer).0, 2, "the next call has the next id");
        drop(dir);
        assert_eq!(mock::open_handles(), open - 1, "the endpoint goes with the session");
        assert_eq!(mock::stray_closes(), 0);
    }

    /// A well-formed refusal is the server's; a dead peer is the transport's, as a typed code.
    #[test]
    fn a_refusal_is_the_servers_and_a_closed_peer_the_transports() {
        mock::reset();
        let (ns, peer) = service("/dev/dir");
        let mut msg = Msg::new();
        let mut dir = Dir::resolve(ns, "/dev/dir", &mut msg).unwrap();
        let mut body = [0u8; 16];
        let n = crate::error::error_body(&mut body, KError::NotEmpty.as_i32(), 0, b"").unwrap();
        queue(peer, 1, OP_FILE_RMDIR, RS_FLAG_REPLY | RS_FLAG_ERROR, &body[..n]);
        assert_eq!(dir.rmdir(b"d"), Err(DirError::Server(KError::NotEmpty.as_i32())));
        // SAFETY: the mock's peer, closed here as a server going away would.
        drop(unsafe { Handle::<Channel, Only>::from_raw(peer.raw(), Rights::empty()) });
        assert_eq!(dir.rmdir(b"d"), Err(DirError::Transport(KError::PeerClosed.as_i32())));
    }

    #[test]
    fn a_path_that_does_not_resolve_is_a_transport_fault_and_leaks_nothing() {
        mock::reset();
        let (ns, _peer) = service("/dev/dir");
        let open = mock::open_handles();
        let mut msg = Msg::new();
        let err = Dir::resolve(ns, "/dev/gone", &mut msg).err().unwrap();
        assert_eq!(err, DirError::Transport(KError::NotFound.as_i32()));
        let mut buf = [0u8; IPC_MSG_SIZE];
        assert_eq!(
            Dir::open(0, b"/dev/\xff", &mut buf).err(),
            Some(DirError::Transport(KError::InvalidArgument.as_i32()))
        );
        assert_eq!(mock::open_handles(), open);
    }

    /// A session of the raw shims is closed by `close` and by nothing else: its caller may own
    /// the endpoint, or close it itself.
    #[test]
    fn a_session_of_the_raw_shims_is_closed_only_by_close() {
        mock::reset();
        let (ns, _peer) = service("/dev/dir");
        let mut buf = [0u8; IPC_MSG_SIZE];
        let opened = Dir::open(ns.raw().0, b"/dev/dir", &mut buf).unwrap();
        let ep = opened.endpoint();
        drop(opened);
        assert!(mock::is_open(ep), "dropped, not closed");
        let wrapped = Dir::from_endpoint(ep, &mut buf).unwrap();
        drop(wrapped);
        assert!(mock::is_open(ep), "a wrapped endpoint is its caller's until `close`");
        Dir::from_endpoint(ep, &mut buf).unwrap().close();
        assert!(!mock::is_open(ep));
        assert_eq!(mock::stray_closes(), 0);
        assert!(matches!(Dir::from_endpoint(ep, &mut buf[..IPC_MSG_SIZE - 1]), Err(DirError::Protocol)));
    }

    /// The raw round trip leaves the reply in the caller's buffer, where its callers read it.
    #[test]
    fn a_raw_round_trip_leaves_the_reply_in_the_buffer() {
        mock::reset();
        let (ns, peer) = service("/dev/dir");
        let endpoint = block_on(ns.lookup::<Channel, Only>("/dev/dir", Rights::from_bits(DIR_SESSION_RIGHTS))).unwrap();
        queue(peer, 5, OP_FILE_MKDIR, RS_FLAG_REPLY, b"mine");
        let mut buf = [0u8; IPC_MSG_SIZE];
        let n = round_trip(endpoint.raw().0, &mut buf, 5, OP_FILE_MKDIR, b"ask").unwrap();
        assert_eq!(crate::decode(&buf[PAYLOAD_OFF..PAYLOAD_OFF + n]).unwrap().body, b"mine");
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize, n);
        assert_eq!(request(peer).2, b"ask");
        assert!(mock::is_open(endpoint.raw().0), "the caller's endpoint stays open");
    }
}
