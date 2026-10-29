//! The **directory-session client** — the caller's side of the `File` directory
//! operations (`docs/spec/rsproto-file-ops.md`).
//!
//! A directory path resolved through the namespace yields a session `IpcChannel` scoped to one
//! directory inode: the channel *is* the directory, and every op addresses entries **by
//! name**, never by path. [`Dir`] wraps such an endpoint and turns the codecs in
//! [`file`](crate::file) into method calls. It lives here, not in `libos`, which sits *below*
//! this crate; the `io` feature keeps the codec core dependency-free. Each op is a send, a
//! `sys_wait` on the endpoint, and a recv: the thread parks in `sys_wait`, never inside
//! another syscall.

use crate::file::{
    DIR_ENTRY_PREFIX_LEN, DirEntry, RENAME_REQUEST_MAX, name_request, parse_read_dir_reply, read_dir_request,
    rename_request,
};
use crate::{
    OP_FILE_MKDIR, OP_FILE_READ_DIR, OP_FILE_RENAME, OP_FILE_RMDIR, OP_FILE_TOUCH, OP_FILE_UNLINK, RS_HEADER_LEN,
};
use libkern::abi::{IPC_MSG_SIZE, SENDMODE_NOBLOCK};
use libkern::handle::{RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT};
use libkern::syscall::{SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE, SYS_NS_LOOKUP, SYS_WAIT, syscall1, syscall4, syscall5};

/// Byte offset of the rsproto payload within an `IpcMsg` (the IPC header precedes it).
pub(crate) const PAYLOAD_OFF: usize = 24;
/// `IpcMsgHeader.payload_len` field offset.
const OFF_PAYLOAD_LEN: usize = 4;
/// `IpcMsgHeader.handle_count` field offset.
const OFF_HANDLE_COUNT: usize = 8;

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

/// An open directory session: an endpoint handle plus the one message buffer its traffic uses.
///
/// The buffer is caller-provided so this crate stays `alloc`-free; the same buffer carries the
/// request out and the reply back. Closing is explicit ([`close`](Self::close)), not a `Drop`,
/// because dropping cannot report a failure.
pub struct Dir<'a> {
    endpoint: u64,
    buf: &'a mut [u8],
    /// Monotonic per-session request id — echoed by the server, useful in a trace.
    next_request_id: u64,
}

impl<'a> Dir<'a> {
    /// Resolve `path` in the namespace `ns` to a directory session.
    ///
    /// `buf` must be at least [`IPC_MSG_SIZE`] bytes. Fails with [`DirError::Transport`] if the
    /// path does not resolve or resolves to something that is not a directory.
    pub fn open(ns: u64, path: &[u8], buf: &'a mut [u8]) -> Result<Dir<'a>> {
        if buf.len() < IPC_MSG_SIZE {
            return Err(DirError::Protocol);
        }
        // SAFETY: `path` is a valid readable slice; `ns` is a handle we hold.
        let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, DIR_SESSION_RIGHTS) };
        if po < 0 {
            return Err(DirError::Transport(po as i32));
        }
        let (status, resolved) = po_wait(po as u64);
        if status < 0 {
            return Err(DirError::Transport(status));
        }
        if resolved == 0 {
            return Err(DirError::Protocol);
        }
        Ok(Dir { endpoint: resolved, buf, next_request_id: 1 })
    }

    /// Wrap an already-resolved session endpoint; [`close`](Self::close) closes it.
    pub fn from_endpoint(endpoint: u64, buf: &'a mut [u8]) -> Result<Dir<'a>> {
        if buf.len() < IPC_MSG_SIZE {
            return Err(DirError::Protocol);
        }
        Ok(Dir { endpoint, buf, next_request_id: 1 })
    }

    /// The underlying endpoint handle (for `sys_wait` alongside other handles).
    pub fn endpoint(&self) -> u64 {
        self.endpoint
    }

    /// Close the session's endpoint handle, releasing the server-side session.
    pub fn close(self) {
        // SAFETY: closing a handle this session owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, self.endpoint) };
    }

    /// Enumerate the directory, calling `f` once per entry — including `.` and `..`.
    /// Pagination is handled internally; return `false` from `f` to stop early (`Ok(false)`).
    /// The entry borrows the reply buffer, so it is valid only for the call.
    pub fn read_dir(&mut self, mut f: impl FnMut(&DirEntry<'_>) -> bool) -> Result<bool> {
        let mut cursor = 0u64;
        loop {
            let mut body = [0u8; 8];
            let n = read_dir_request(&mut body, cursor).ok_or(DirError::Protocol)?;
            let reply_len = self.round_trip(OP_FILE_READ_DIR, &body[..n])?;
            let payload = &self.buf[PAYLOAD_OFF..PAYLOAD_OFF + reply_len];
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
        let mut body = [0u8; RENAME_REQUEST_MAX];
        let n = rename_request(&mut body, old, new).ok_or(DirError::Protocol)?;
        self.status_op(OP_FILE_RENAME, &body[..n])
    }

    fn name_op(&mut self, op: u16, name: &[u8]) -> Result<()> {
        let mut body = [0u8; 258];
        let n = name_request(&mut body, name).ok_or(DirError::Protocol)?;
        self.status_op(op, &body[..n])
    }

    /// A mutation: round-trip, then accept an empty-body success or surface the error.
    fn status_op(&mut self, op: u16, body: &[u8]) -> Result<()> {
        let reply_len = self.round_trip(op, body)?;
        let payload = &self.buf[PAYLOAD_OFF..PAYLOAD_OFF + reply_len];
        let msg = crate::decode(payload).map_err(|_| DirError::Protocol)?;
        if msg.is_error() {
            return Err(DirError::Server(server_error(msg.body)));
        }
        Ok(())
    }

    /// Send one request and receive its reply into `self.buf`; returns the reply's payload length.
    fn round_trip(&mut self, op: u16, body: &[u8]) -> Result<usize> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        round_trip(self.endpoint, self.buf, request_id, op, body).map_err(DirError::from)
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

/// Send one request on `endpoint` and receive its reply into `buf`, returning the reply's
/// rsproto payload length. Shared by [`Dir`] and [`crate::desktop::Desktop`], so the
/// send/wait/recv/handle sweep has one copy. A `buf` shorter than [`IPC_MSG_SIZE`] is refused
/// `Protocol`: the kernel copies a whole message from it.
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
    let region = &mut buf[PAYLOAD_OFF..];
    let rs_len = crate::encode(region, op, request_id, 0, body, 0).ok_or(WireError::Protocol)?;
    buf[OFF_PAYLOAD_LEN..OFF_PAYLOAD_LEN + 4].copy_from_slice(&(rs_len as u32).to_le_bytes());
    buf[OFF_HANDLE_COUNT] = 0;

    // The sole client of a session channel expects one reply, so the ring always has room and a
    // non-blocking send is correct.
    // SAFETY: `buf` is a valid IPC_MSG_SIZE buffer; no handles accompany the request.
    let sent = unsafe { syscall5(SYS_CHANNEL_SEND, endpoint, buf.as_ptr() as u64, 0, 0, SENDMODE_NOBLOCK) };
    if sent != 0 {
        return Err(WireError::Transport(sent as i32));
    }

    // `sys_wait` is where the thread blocks, never the send or the recv.
    let handles = [endpoint];
    let mut results = [0u8; 24];
    // SAFETY: valid handle array + result out-buffer for one waiter.
    let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
    if waited != 1 {
        return Err(WireError::Transport(waited as i32));
    }

    let msg: &mut [u8; IPC_MSG_SIZE] = (&mut buf[..IPC_MSG_SIZE]).try_into().map_err(|_| WireError::Protocol)?;
    let _sent = libkern::channel_recv(endpoint, msg).map_err(|got| WireError::Transport(got as i32))?;
    let payload_len = u32::from_le_bytes([
        buf[OFF_PAYLOAD_LEN],
        buf[OFF_PAYLOAD_LEN + 1],
        buf[OFF_PAYLOAD_LEN + 2],
        buf[OFF_PAYLOAD_LEN + 3],
    ]) as usize;
    if payload_len < RS_HEADER_LEN || PAYLOAD_OFF + payload_len > buf.len() {
        return Err(WireError::Protocol);
    }
    Ok(payload_len)
}

/// The `KError` an error reply carries, or a generic failure if the body is malformed.
fn server_error(body: &[u8]) -> i32 {
    match crate::error::parse_error(body) {
        Some(e) if e.kerror < 0 => e.kerror,
        _ => -1,
    }
}

/// Wait on a `PendingOperation`, returning its `(status, result)` and closing it.
///
/// `pub(crate)`: [`crate::desktop::Desktop`] resolves its endpoint the same way.
pub(crate) fn po_wait(po: u64) -> (i32, u64) {
    let handles = [po];
    let mut r = [0u8; 24];
    // SAFETY: valid handle array + result out-buffer for one waiter.
    let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, r.as_mut_ptr() as u64, u64::MAX) };
    // SAFETY: closing the PO we own (a resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 {
        return (-1, 0);
    }
    let status = i32::from_le_bytes([r[8], r[9], r[10], r[11]]);
    let result = u64::from_le_bytes([r[16], r[17], r[18], r[19], r[20], r[21], r[22], r[23]]);
    (status, result)
}

/// The most entries one `ReadDir` reply can carry — what a caller sizes a per-reply array by.
pub const MAX_REPLY_ENTRIES: usize = crate::RS_MAX_BODY / DIR_ENTRY_PREFIX_LEN;

#[cfg(test)]
mod tests {
    use super::*;

    /// Refused before anything is sent: a message one byte short reaches no syscall.
    #[test]
    fn a_round_trip_refuses_a_buffer_shorter_than_a_message() {
        let mut short = [0u8; IPC_MSG_SIZE - 1];
        assert!(matches!(round_trip(0, &mut short, 1, crate::OP_FILE_READ_DIR, &[]), Err(WireError::Protocol)));
    }
}
