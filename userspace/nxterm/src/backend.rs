//! The tty side of the terminal: obtaining a terminal, becoming its backend, and hosting a
//! shell on it.
//!
//! **This is a pty, assembled from the pieces this system has.** `nxterm` resolves `/dev/tty`
//! like any program, hands the tty server the far end of a channel it made — `AttachBackend` —
//! and then gives the terminal itself away to the shell it spawns. What it keeps is the
//! backend: the server writes the shell's output down it and reads the user's keystrokes back.
//! Unix would call the kept half the master and the given half the slave; here the line
//! discipline is the tty server rather than the kernel, so those are the only two pieces.
//!
//! **Why the terminal is handed down rather than resolved.** `/dev/tty` is a namespace binding,
//! one per session, so a program cannot resolve its way to a *particular* window — the shell
//! would find the session's console. The handle travels in the setup message, which is where
//! `docs/spec/process-spawn-args.md` says everything beyond the four bootstrap registers
//! belongs, and it is the same thing Unix does by inheriting fd 0/1/2 instead of looking them
//! up. `docs/architecture/graphical-session.md` §6.1 owns making the resolved path work here too.

use libkern::abi::SpawnArgs;
use libkern::handle::{RIGHT_RECV, RIGHT_SEND, RIGHT_TRANSFER, RIGHT_WAIT};
use libkern::{
    SYS_CHANNEL_CREATE, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE, SYS_NS_LOOKUP,
    SYS_PROCESS_SPAWN, SYS_WAIT, kprint, syscall1, syscall4, syscall5,
};
use librsproto::{
    OP_TTY_ATTACH_BACKEND, OP_TTY_INPUT, OP_TTY_OUTPUT, RS_FLAG_REPLY, decode, encode,
};

/// IPC payload starts after the 24-byte header.
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// The most terminal input one message carries. Keystrokes are small; this bounds the copy.
const MAX_BODY: usize = MSG_LEN - PAYLOAD_OFF - 64;

static mut MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut WAIT_HANDLES: [u64; 4] = [0; 4];
static mut WAIT_RESULTS: [u8; 24 * 4] = [0; 24 * 4];
static mut CH0: u64 = 0;
static mut CH1: u64 = 0;

/// The terminal half this process keeps: the backend channel.
pub struct Backend {
    /// The channel the tty server writes output to and reads input from.
    pub channel: u64,
    /// The server has closed its end — learned from the receive that found it, never from a
    /// receive of its own. See [`output`](Backend::output).
    gone: bool,
}

/// Wait one PO and read `(status, value)` out of its `IoResult`.
fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    let (status, value) = unsafe {
        (
            i32::from_le_bytes([
                WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11],
            ]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
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
    let po =
        unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return 0;
    }
    let (status, handle) = po_wait(po as u64);
    if status != 0 { 0 } else { handle }
}

/// A connected channel pair, or `None`.
fn make_channel() -> Option<(u64, u64)> {
    // SAFETY: CH0/CH1 are valid writable out-params.
    let r = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CH0) as u64, (&raw mut CH1) as u64, 8, 0)
    };
    if r != 0 {
        return None;
    }
    // SAFETY: the kernel wrote both out-params.
    Some(unsafe { (CH0, CH1) })
}

/// Send one rsproto message on `ch`, optionally moving a handle.
fn send(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handle: Option<u64>) -> bool {
    // SAFETY: MSG/HANDLES are valid buffers owned by this module.
    unsafe {
        let Some(rs_len) = encode(&mut MSG[PAYLOAD_OFF..], op, request_id, flags, body, 0) else {
            return false;
        };
        MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        let n = match handle {
            Some(h) => {
                MSG[8] = 1;
                HANDLES[0] = h;
                1
            }
            None => {
                MSG[8] = 0;
                0
            }
        };
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const MSG) as u64,
            (&raw const HANDLES) as u64,
            n,
            libkern::SENDMODE_NOBLOCK,
        ) == 0
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
    let (near, far) = make_channel()?;
    // **`far` is moved.** After the send it belongs to the tty server, which is what makes the
    // server able to close it — and that close is how this process learns its terminal ended.
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
    // The reply is awaited rather than assumed: a refusal means the shell would be talking to a
    // terminal whose output goes to the serial console, which is silent and confusing.
    // SAFETY: valid wait buffers; one waiter on the terminal channel.
    let waited = unsafe {
        WAIT_HANDLES[0] = tty;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
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
    Some((tty, Backend { channel: near, gone: false }))
}

/// Receive one message on `ch` and report whether it is a non-error reply.
fn recv_is_ok(ch: u64) -> bool {
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut MSG) as u64,
            (&raw mut HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr != 0 {
        return false;
    }
    // SAFETY: bounded read-only slice over the just-received message.
    unsafe {
        let payload_len = u32::from_le_bytes([MSG[4], MSG[5], MSG[6], MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        matches!(decode(req), Ok(m) if m.flags & RS_FLAG_REPLY != 0 && m.flags & librsproto::RS_FLAG_ERROR == 0)
    }
}

impl Backend {
    /// Tell the server the user typed `bytes`.
    ///
    /// The one call that replaced the loopback. What goes down here is exactly what a serial
    /// keyboard would have produced, which is why the discipline above it needs no idea a
    /// window is involved.
    pub fn typed(&self, bytes: &[u8]) -> bool {
        bytes.chunks(MAX_BODY).all(|part| send(self.channel, OP_TTY_INPUT, 0, 0, part, None))
    }

    /// Take one queued message of program output, if there is one.
    ///
    /// Returns `None` when the ring is empty, and `Some(&[])` never — an empty body would be
    /// indistinguishable from "nothing to do".
    ///
    /// **This is the only receive on the backend**, and that is a correctness requirement
    /// rather than tidiness. `sys_channel_recv` has no peek: it dequeues. A first version had
    /// [`is_gone`](Self::is_gone) call it separately "to probe the ring", which meant every
    /// turn of the event loop ate a message — and back-to-back messages are the norm, since
    /// `sink_write` emits one per chunk and one per `Act::Write`. The bytes vanished, and
    /// because the message *was* dequeued the channel stopped being signalled, so nothing woke
    /// to retry: silent output loss with a hole in the line, the same failure this part fixed
    /// one file over by making the server's send block (PR #194 review, finding 1).
    ///
    /// `PeerClosed` is recorded here for `is_gone` to report, because the receive that
    /// discovers it is this one.
    pub fn output(&mut self) -> Option<&'static [u8]> {
        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                self.channel,
                (&raw mut MSG) as u64,
                (&raw mut HANDLES) as u64,
                (&raw mut RECV_COUNT) as u64,
            )
        };
        if rr != 0 {
            self.gone = rr == libkern::error::KError::PeerClosed.as_i32() as i64;
            return None;
        }
        // SAFETY: bounded read-only slice over the just-received message. `'static` is honest
        // here: `MSG` is a module static and the caller consumes the bytes before the next
        // receive, which is the same contract every server in this tree uses for its buffer.
        unsafe {
            let payload_len = u32::from_le_bytes([MSG[4], MSG[5], MSG[6], MSG[7]]) as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match decode(req) {
                Ok(m) if m.op == OP_TTY_OUTPUT && !m.body.is_empty() => Some(m.body),
                _ => None,
            }
        }
    }

    /// Whether the server has closed its end — the terminal is over.
    ///
    /// A plain read of what [`output`](Self::output) last saw. It performs no receive of its
    /// own: there is no way to ask a channel a question without consuming from it.
    pub fn is_gone(&self) -> bool {
        self.gone
    }
}

/// Spawn `/bin/nxsh` on `terminal`, handing the terminal down in its setup message.
///
/// The terminal handle is **moved**: this process has no further use for it, and keeping a
/// second reference would stop the server seeing `PeerClosed` when the shell exits.
///
/// # Safety
///
/// `root_ns` must be this process's live root namespace and `terminal` a channel it owns.
/// `env` is forwarded from this terminal's **own** setup message, not invented here.
///
/// It was `Record::default()` until M7 Part F, which was invisible while `nxterm` was only
/// ever spawned by `init` with no environment of its own — the shell it hosts simply had none
/// either. Launched from `desktop-shell` into a constructed namespace, that becomes a terminal
/// whose shell has no `$env.HOME` while every serial login's does, and the asymmetry is the
/// bug: a graphical terminal should be the same shell in a window.
pub unsafe fn spawn_shell(root_ns: u64, terminal: u64, env: &libstream::wire::Record) -> i64 {
    let image = ns_lookup(root_ns, b"/bin/nxsh", libkern::RIGHT_MAP_READ);
    if image == 0 {
        kprint(b"nxterm: /bin/nxsh not found\n");
        return -1;
    }
    let Some((setup_ours, setup_theirs)) = make_channel() else {
        // SAFETY: closing our own image handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
        return -1;
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
        return child;
    }
    let streams = libstream::setup::Streams::default();
    let sent = libstream::setup::send_setup_full(
        setup_ours,
        &streams,
        Some(terminal),
        &["nxsh"],
        env,
    )
    .is_ok();
    if sent {
        // **Reported here, beside the send, rather than where the environment arrived.** This
        // number is what a launched terminal hands its shell, and it is the only part of the
        // chain a *release* image shows: the shell's own banner goes to the tty, which for a
        // windowed terminal means the grid, which renders only under `test-harness`. Reading
        // it at the far end of the function from `_start` would let the two drift — a send
        // changed back to `Record::default()` would still log a healthy count.
        libkern::debug::Line::new()
            .s(b"nxterm: hosting a shell (env: ")
            .u(env.values.len() as u64)
            .s(b" fields)")
            .end();
    }
    // SAFETY: closing our end of the setup channel.
    unsafe { syscall1(SYS_HANDLE_CLOSE, setup_ours) };
    if !sent {
        // **The failure branch is the one that has to close it.** A successful send commits the
        // move by closing the sender's handles itself, so closing here would be a no-op that
        // errors; on failure the handle is still ours and nothing else will take it. The first
        // version had these the wrong way round, which left the tty-server holding a terminal —
        // and one of its fifteen wait slots — for the life of the boot, while the orphaned
        // shell fell back to resolving `/dev/tty` and landed a second REPL on the serial
        // console (PR #194 review, finding 5).
        // SAFETY: the send failed, so the terminal handle is still ours.
        unsafe { syscall1(SYS_HANDLE_CLOSE, terminal) };
        kprint(b"nxterm: the shell's setup message did not send\n");
        return -1;
    }
    child
}
