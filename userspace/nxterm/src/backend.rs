//! The tty side of the terminal: attaching as its backend, and hosting a shell on it.
//!
//! `nxterm` resolves `/dev/tty`, hands the tty server the far end of a channel (`AttachBackend`),
//! and gives the terminal itself to the shell it spawns; the backend it keeps carries output in and
//! keystrokes out. The terminal is handed down, not resolved, as `/dev/tty` is one binding per
//! session and would reach the session's console (`docs/spec/process-spawn-args.md`).

use alloc::boxed::Box;
use libkern::abi::SpawnArgs;
use libkern::handle::{RIGHT_RECV, RIGHT_SEND, RIGHT_TRANSFER, RIGHT_WAIT};
use libkern::{
    SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE, SYS_NS_LOOKUP, SYS_PROCESS_SPAWN, SYS_WAIT, kprint, syscall1, syscall4,
    syscall5,
};
use librsproto::{OP_TTY_ATTACH_BACKEND, OP_TTY_INPUT, OP_TTY_OUTPUT, RS_FLAG_REPLY, decode, encode};

/// Offset of the IPC payload, after the 24-byte header.
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// The most terminal input one message carries ([`librsproto::RS_MAX_BODY`]); a paste reaches it.
const MAX_BODY: usize = librsproto::RS_MAX_BODY;

static mut WAIT_HANDLES: [u64; 4] = [0; 4];
static mut WAIT_RESULTS: [u8; 24 * 4] = [0; 24 * 4];

/// The terminal half this process keeps: the backend channel.
pub struct Backend {
    /// The channel the tty server writes output to and reads input from.
    pub channel: u64,
    /// The server has closed its end; learned only from [`output`](Backend::output)'s receive.
    gone: bool,
    /// What [`output`](Backend::output) lends out, so holding it holds this `Backend`.
    msg: Box<[u8; MSG_LEN]>,
}

impl Drop for Backend {
    /// Closing the channel is how the terminal ends: the tty server sees `PeerClosed`, finishes
    /// the terminal, and the `nxsh` on it stops. A closed tab drops a `Backend`, so the close
    /// lives here rather than at the one call site that closes tabs.
    fn drop(&mut self) {
        // SAFETY: `channel` is a handle this process owns for the life of this `Backend`, and
        // nothing else holds a copy — `attach` moves the far end to the server and keeps this.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, self.channel);
        }
    }
}

/// Wait one PO and read `(status, value)` out of its `IoResult`.
fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let (status, value) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16],
                WAIT_RESULTS[17],
                WAIT_RESULTS[18],
                WAIT_RESULTS[19],
                WAIT_RESULTS[20],
                WAIT_RESULTS[21],
                WAIT_RESULTS[22],
                WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, value) }
}

/// Resolve `path` in `ns` requesting `rights`; `0` on failure.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return 0;
    }
    let (status, handle) = po_wait(po as u64);
    if status != 0 { 0 } else { handle }
}

/// A connected channel pair, or `None`.
fn make_channel() -> Option<(u64, u64)> {
    libkern::channel_create(8).ok()
}

/// Send one rsproto message on `ch`, optionally moving a handle.
fn send(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handle: Option<u64>) -> bool {
    let mut msg = [0u8; MSG_LEN];
    let Some(rs_len) = encode(&mut msg[PAYLOAD_OFF..], op, request_id, flags, body, 0) else {
        return false;
    };
    msg[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
    let count = handle.is_some() as u64;
    msg[8] = count as u8;
    let handles = [handle.unwrap_or(0)];
    // SAFETY: a whole message and `count` handles, both on this frame.
    unsafe {
        syscall5(SYS_CHANNEL_SEND, ch, msg.as_ptr() as u64, handles.as_ptr() as u64, count, libkern::SENDMODE_NOBLOCK)
            == 0
    }
}

/// Obtain a terminal and become its backend.
///
/// Returns `(terminal, backend)`: the terminal channel to hand the shell, and the backend this
/// process keeps. `None` if `/dev/tty` does not resolve or the server refuses.
///
/// # Safety
///
/// `root_ns` must be this process's live root namespace.
pub unsafe fn attach(root_ns: u64) -> Option<(u64, Backend)> {
    let tty = ns_lookup(root_ns, b"/dev/tty", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_TRANSFER);
    if tty == 0 {
        kprint(b"nxterm: /dev/tty did not resolve\n");
        return None;
    }
    let Some((near, far)) = make_channel() else {
        // SAFETY: closing our own terminal channel, which nothing else has been given.
        unsafe { syscall1(SYS_HANDLE_CLOSE, tty) };
        return None;
    };
    // `far` is moved: once the server owns it, its close is how this process learns the terminal
    // ended.
    if !send(tty, OP_TTY_ATTACH_BACKEND, 1, 0, &[], Some(far)) {
        kprint(b"nxterm: AttachBackend did not send\n");
        // SAFETY: the send failed, so both ends are still ours.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, near);
            syscall1(SYS_HANDLE_CLOSE, far);
            syscall1(SYS_HANDLE_CLOSE, tty);
        }
        return None;
    }
    // The reply is awaited: a refusal would leave the shell on a terminal whose output goes to
    // the serial console.
    // SAFETY: valid wait buffers; one waiter on the terminal channel.
    let waited = unsafe {
        WAIT_HANDLES[0] = tty;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    if waited != 1 || !recv_is_ok(tty) {
        kprint(b"nxterm: AttachBackend refused\n");
        // SAFETY: `far` is the server's now; the rest are ours.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, near);
            syscall1(SYS_HANDLE_CLOSE, tty);
        }
        return None;
    }
    Some((tty, Backend { channel: near, gone: false, msg: Box::new([0; MSG_LEN]) }))
}

/// The rsproto payload of the message in `msg`, as long as its header says, within the message.
fn payload(msg: &[u8; MSG_LEN]) -> &[u8] {
    let len = u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
    &msg[PAYLOAD_OFF..PAYLOAD_OFF + len.min(MSG_LEN - PAYLOAD_OFF)]
}

/// Receive one message on `ch` and report whether it is a non-error reply.
fn recv_is_ok(ch: u64) -> bool {
    let mut msg = [0u8; MSG_LEN];
    let Ok(_sent) = libkern::channel_recv(ch, &mut msg) else {
        return false;
    };
    matches!(decode(payload(&msg)), Ok(m) if m.flags & RS_FLAG_REPLY != 0 && m.flags & librsproto::RS_FLAG_ERROR == 0)
}

impl Backend {
    /// Tell the server the user typed `bytes`, exactly what a serial keyboard would produce.
    pub fn typed(&self, bytes: &[u8]) -> bool {
        bytes.chunks(MAX_BODY).all(|part| send(self.channel, OP_TTY_INPUT, 0, 0, part, None))
    }

    /// Take one queued message of program output, if there is one.
    ///
    /// `None` when empty; never `Some(&[])`. This is the only receive on the backend:
    /// `sys_channel_recv` has no peek, so a separate probe would dequeue a message and the channel,
    /// no longer signalled, would never wake to retry. `PeerClosed` is recorded here for
    /// [`is_gone`](Self::is_gone).
    pub fn output(&mut self) -> Option<&[u8]> {
        let _sent = match libkern::channel_recv(self.channel, &mut self.msg) {
            Ok(sent) => sent,
            Err(rr) => {
                self.gone = rr == libkern::error::KError::PeerClosed.as_i32() as i64;
                return None;
            }
        };
        match decode(payload(&self.msg)) {
            Ok(m) if m.op == OP_TTY_OUTPUT && !m.body.is_empty() => Some(m.body),
            _ => None,
        }
    }

    /// Whether the server has closed its end; reads what [`output`](Self::output) last saw.
    pub fn is_gone(&self) -> bool {
        self.gone
    }
}

/// Spawn `/bin/nxsh` on `terminal`, handing the terminal down in its setup message.
///
/// The terminal handle is **moved**, on every path: keeping a second reference would stop the
/// server seeing `PeerClosed` when the shell exits. `false` if the shell did not start.
///
/// # Safety
///
/// `root_ns` must be this process's live root namespace and `terminal` a channel it owns.
/// `env` is forwarded from this terminal's own setup message so the shell has the same
/// `$env.HOME` a serial login's does.
pub unsafe fn spawn_shell(root_ns: u64, terminal: u64, env: &libstream::wire::Record) -> bool {
    let image = ns_lookup(root_ns, b"/bin/nxsh", libkern::RIGHT_MAP_READ);
    if image == 0 {
        kprint(b"nxterm: /bin/nxsh not found\n");
        // SAFETY: no shell took the terminal, so it is still ours.
        unsafe { syscall1(SYS_HANDLE_CLOSE, terminal) };
        return false;
    }
    let Some((setup_ours, setup_theirs)) = make_channel() else {
        // SAFETY: closing our own image handle, and the terminal no shell took.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, image);
            syscall1(SYS_HANDLE_CLOSE, terminal);
        }
        return false;
    };
    let mut args = SpawnArgs {
        image,
        arg0: libstream::setup::bootstrap_arg0(true),
        handle_count: 1,
        move_mask: 1,
        handles: [setup_theirs, 0, 0, 0],
        rights: [u64::MAX; 4],
        namespace: 0, // inherit a LOOKUP-only view of ours
        syscaps: 0,
    };
    // SAFETY: `args` is a valid, fully-initialised SpawnArgs.
    let child = unsafe { syscall1(SYS_PROCESS_SPAWN, (&raw mut args) as u64) };
    // SAFETY: closing our own image handle; the kernel copied the ELF during the spawn.
    unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
    if child < 0 {
        // SAFETY: the spawn failed, so both setup ends and the terminal are still ours.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, setup_ours);
            syscall1(SYS_HANDLE_CLOSE, setup_theirs);
            syscall1(SYS_HANDLE_CLOSE, terminal);
        }
        return false;
    }
    let streams = libstream::setup::Streams::default();
    let sent = libstream::setup::send_setup_full(setup_ours, streams, Some(terminal), &["nxsh"], env).is_ok();
    if sent {
        // Logged beside the send: this count is what the shell is handed, and the only link in
        // the chain a release image shows. `check-login`'s clipboard gate waits on this line.
        libkern::debug::Line::new()
            .s(b"nxterm: hosting a shell (env: ")
            .u(env.values.len() as u64)
            .s(b" fields)")
            .end();
    }
    // SAFETY: closing our end of the setup channel, and our reference to the shell: nothing here
    // waits on it, and its terminal closing is what ends it.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, setup_ours);
        syscall1(SYS_HANDLE_CLOSE, child as u64);
    }
    if !sent {
        // The terminal went with the attempt, sent or closed: it is not ours to close.
        kprint(b"nxterm: the shell's setup message did not send\n");
    }
    sent
}
