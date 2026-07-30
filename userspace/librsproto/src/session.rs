//! The **directory-session client** — the caller's side of the `File` directory
//! operations (`docs/spec/rsproto-file-ops.md`).
//!
//! A directory path resolved through the namespace yields a session `IpcChannel` scoped
//! to one directory inode: the channel *is* the directory, and every op addresses entries
//! **by name**, never by path, so a holder cannot reach outside it. [`Dir`] wraps such an
//! endpoint and turns the request/reply codecs in [`file`](crate::file) into ordinary
//! method calls — the plumbing that was hand-rolled at every call site before.
//!
//! ## Why this lives here and not in `libos`
//!
//! `libos` sits *below* this crate in the userspace layering and is `no_std` **without
//! `alloc`**, so it cannot speak a protocol defined above it. The client belongs next to
//! the wire definition it speaks. Gating it behind the `io` feature keeps the codec core
//! dependency-free and host-testable — exactly the seam `libstream::channel::IpcPort`
//! uses (see the decision log, 2026-07-24).
//!
//! ## Blocking, but not "a syscall that blocks"
//!
//! Each op is a send, a `sys_wait` on the endpoint, and a recv. The thread parks in
//! `sys_wait` — never inside another syscall — which is the async-first contract, not a
//! violation of it. Sequential callers (a coreutil listing a directory) want exactly this
//! shape; a future task-based caller can drive the same codecs over `libos::Op`.

use crate::file::{
    DIR_ENTRY_PREFIX_LEN, DirEntry, name_request, parse_read_dir_reply, read_dir_request,
    rename_request,
};
use crate::{
    OP_FILE_MKDIR, OP_FILE_READ_DIR, OP_FILE_RENAME, OP_FILE_RMDIR, OP_FILE_TOUCH,
    OP_FILE_UNLINK, RS_HEADER_LEN,
};
use libkern::abi::{IPC_MSG_SIZE, SENDMODE_NOBLOCK};
use libkern::handle::{RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT};
use libkern::syscall::{
    SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE, SYS_NS_LOOKUP, SYS_WAIT, syscall1,
    syscall4, syscall5,
};

/// Byte offset of the rsproto payload within an `IpcMsg` (the IPC header precedes it).
const PAYLOAD_OFF: usize = 24;
/// `IpcMsgHeader.payload_len` field offset.
const OFF_PAYLOAD_LEN: usize = 4;
/// `IpcMsgHeader.handle_count` field offset.
const OFF_HANDLE_COUNT: usize = 8;

/// The rights a directory session needs: send requests, receive replies, and `sys_wait`
/// for the reply to arrive.
pub const DIR_SESSION_RIGHTS: u64 = RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT;

/// What can go wrong talking to a directory server. The `Server` variant carries the
/// `KError` discriminant the server reported, so a caller can distinguish "no such entry"
/// from "the directory is not empty" without decoding the reply itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DirError {
    /// The server replied with an error; the payload is its `KError` discriminant
    /// (negative, e.g. `KError::NotFound`).
    Server(i32),
    /// A syscall failed (the payload is its negative return), or the reply never came.
    Transport(i32),
    /// The reply could not be parsed, or a name/reply did not fit the message buffer.
    Protocol,
}

/// Result of a directory operation.
pub type Result<T> = core::result::Result<T, DirError>;

/// An open directory session: an endpoint handle plus the one message buffer its
/// request/reply traffic uses.
///
/// The buffer is caller-provided (`&mut [u8; IPC_MSG_SIZE]`-sized) so this crate stays
/// `alloc`-free and a caller controls where a 4 KiB buffer lives — a coreutil puts it on
/// the stack, a server in `.bss`. The same buffer carries the request out and the reply
/// back: the request is fully consumed by the time the send returns.
///
/// Closing is explicit ([`close`](Self::close)) rather than a `Drop` impl, because
/// dropping cannot report a failure and a handle close is worth not doing silently.
pub struct Dir<'a> {
    endpoint: u64,
    buf: &'a mut [u8],
    /// Monotonic per-session request id — echoed by the server, useful in a trace.
    next_request_id: u64,
}

impl<'a> Dir<'a> {
    /// Resolve `path` in the namespace `ns` to a directory session.
    ///
    /// `buf` must be at least [`IPC_MSG_SIZE`] bytes. Fails with
    /// [`DirError::Transport`] if the path does not resolve, or resolves to something
    /// that is not a directory (the server declines to mint a session for a file).
    pub fn open(ns: u64, path: &[u8], buf: &'a mut [u8]) -> Result<Dir<'a>> {
        if buf.len() < IPC_MSG_SIZE {
            return Err(DirError::Protocol);
        }
        // SAFETY: `path` is a valid readable slice; `ns` is a handle we hold.
        let po = unsafe {
            syscall4(
                SYS_NS_LOOKUP,
                ns,
                path.as_ptr() as u64,
                path.len() as u64,
                DIR_SESSION_RIGHTS,
            )
        };
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
        Ok(Dir {
            endpoint: resolved,
            buf,
            next_request_id: 1,
        })
    }

    /// Wrap an already-resolved session endpoint (a handle received rather than looked
    /// up). Takes ownership: [`close`](Self::close) closes it.
    pub fn from_endpoint(endpoint: u64, buf: &'a mut [u8]) -> Result<Dir<'a>> {
        if buf.len() < IPC_MSG_SIZE {
            return Err(DirError::Protocol);
        }
        Ok(Dir {
            endpoint,
            buf,
            next_request_id: 1,
        })
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

    /// Enumerate the directory, calling `f` once per entry — including `.` and `..`,
    /// which are real entries (filtering them is a display decision).
    ///
    /// Pagination is handled internally: replies are drained until the server reports no
    /// further entries, so `f` sees the whole directory. Return `false` from `f` to stop
    /// early; enumeration then returns `Ok(false)` without fetching more.
    ///
    /// The entry borrows the reply buffer, so it is only valid for the duration of the
    /// call — a caller that keeps entries copies out the fields it wants.
    pub fn read_dir(&mut self, mut f: impl FnMut(&DirEntry<'_>) -> bool) -> Result<bool> {
        let mut cursor = 0u64;
        loop {
            let mut body = [0u8; 8];
            let n = read_dir_request(&mut body, cursor).ok_or(DirError::Protocol)?;
            let reply_len = self.round_trip(OP_FILE_READ_DIR, &body[..n])?;
            // Re-borrow the reply out of the shared buffer for parsing.
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

    /// Stamp `name`'s modification time as "now", where *now* is the **server's** clock.
    ///
    /// A caller cannot supply the time: one it could choose would be forgeable metadata,
    /// so the filesystem is its own authority for it. (The `File::Touch` on the kernel
    /// control channel is a different thing wearing the same opcode — the kernel telling
    /// the server about a Model A write it could not otherwise observe, fire-and-forget
    /// and with no client behind it. This one is name-addressed inside a session and
    /// returns a status, like the other mutations here.)
    pub fn touch(&mut self, name: &[u8]) -> Result<()> {
        self.name_op(OP_FILE_TOUCH, name)
    }

    /// Rename `old` to `new`, both within this directory.
    pub fn rename(&mut self, old: &[u8], new: &[u8]) -> Result<()> {
        let mut body = [0u8; 512];
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

    /// Send one request on the session and receive its reply into `self.buf`, returning
    /// the reply's rsproto payload length.
    fn round_trip(&mut self, op: u16, body: &[u8]) -> Result<usize> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);

        let region = &mut self.buf[PAYLOAD_OFF..];
        let rs_len = crate::encode(region, op, request_id, 0, body, 0).ok_or(DirError::Protocol)?;
        self.buf[OFF_PAYLOAD_LEN..OFF_PAYLOAD_LEN + 4]
            .copy_from_slice(&(rs_len as u32).to_le_bytes());
        self.buf[OFF_HANDLE_COUNT] = 0;

        // A session reply is one message on a channel we are the sole client of, so the
        // ring always has room: a non-blocking send is correct and avoids a PO round trip.
        // SAFETY: `buf` is a valid IPC_MSG_SIZE buffer; no handles accompany the request.
        let sent = unsafe {
            syscall5(
                SYS_CHANNEL_SEND,
                self.endpoint,
                self.buf.as_ptr() as u64,
                0,
                0,
                SENDMODE_NOBLOCK,
            )
        };
        if sent != 0 {
            return Err(DirError::Transport(sent as i32));
        }

        // Park until the reply lands, then take it. `sys_wait` is where the thread
        // blocks — never inside the send or the recv.
        let handles = [self.endpoint];
        let mut results = [0u8; 24];
        // SAFETY: valid handle array + result out-buffer for one waiter.
        let waited = unsafe {
            syscall4(
                SYS_WAIT,
                handles.as_ptr() as u64,
                1,
                results.as_mut_ptr() as u64,
                u64::MAX,
            )
        };
        if waited != 1 {
            return Err(DirError::Transport(waited as i32));
        }

        let mut transferred = [0u64; 8];
        let mut count: usize = 0;
        // SAFETY: valid message/handle/count out-params.
        let got = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                self.endpoint,
                self.buf.as_mut_ptr() as u64,
                transferred.as_mut_ptr() as u64,
                (&raw mut count) as u64,
            )
        };
        if got != 0 {
            return Err(DirError::Transport(got as i32));
        }
        // Close anything the server transferred: no directory op sends handles, and
        // leaking one on a misbehaving server would exhaust the table over a long listing.
        for h in transferred.iter().take(count.min(8)) {
            // SAFETY: closing a handle just installed into our table.
            unsafe { syscall1(SYS_HANDLE_CLOSE, *h) };
        }
        let payload_len = u32::from_le_bytes([
            self.buf[OFF_PAYLOAD_LEN],
            self.buf[OFF_PAYLOAD_LEN + 1],
            self.buf[OFF_PAYLOAD_LEN + 2],
            self.buf[OFF_PAYLOAD_LEN + 3],
        ]) as usize;
        if payload_len < RS_HEADER_LEN || PAYLOAD_OFF + payload_len > self.buf.len() {
            return Err(DirError::Protocol);
        }
        Ok(payload_len)
    }
}

/// The `KError` an error reply carries, or a generic failure if the body is malformed.
fn server_error(body: &[u8]) -> i32 {
    match crate::error::parse_error(body) {
        Some(e) if e.kerror < 0 => e.kerror,
        _ => -1,
    }
}

/// Wait on a `PendingOperation`, returning its `(status, result)` and closing it.
fn po_wait(po: u64) -> (i32, u64) {
    let handles = [po];
    let mut r = [0u8; 24];
    // SAFETY: valid handle array + result out-buffer for one waiter.
    let waited = unsafe {
        syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, r.as_mut_ptr() as u64, u64::MAX)
    };
    // SAFETY: closing the PO we own (a resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 {
        return (-1, 0);
    }
    let status = i32::from_le_bytes([r[8], r[9], r[10], r[11]]);
    let result = u64::from_le_bytes([r[16], r[17], r[18], r[19], r[20], r[21], r[22], r[23]]);
    (status, result)
}

/// The most entries one `ReadDir` reply can carry — the bound a caller sizes a per-reply
/// entry array against. (Real entries are larger: every name costs at least one byte.)
pub const MAX_REPLY_ENTRIES: usize =
    (IPC_MSG_SIZE - PAYLOAD_OFF - RS_HEADER_LEN) / DIR_ENTRY_PREFIX_LEN;
