//! `clipboard-server` — the kill ring, served at `/dev/clipboard`.
//!
//! `service-mgr` spawns it with a control channel and binds the forwarding endpoint it answers
//! `Meta::Ready` with; it holds no `BIND_NAMESPACE`
//! (`docs/rationale/why-supervisor-registration.md`). A client that resolves `/dev/clipboard`
//! gets a session channel of its own and speaks the `Clipboard` category on it.
//!
//! A clipboard is shared mutable state between mutually untrusting programs, so "anything
//! running may read it" would be ambient authority. Here the binding is the authority: a
//! namespace without `/dev/clipboard` has no clipboard, and an endpoint attenuated to
//! `RIGHT_SEND` can copy and not read. It stores rather than brokers because the clipboard
//! would otherwise die with the application copied from.
//!
//! The ring itself is [`clipboard_server::Ring`], host-tested; this file is the plumbing.
//!
//! **It never logs an entry's bytes**: a clipboard eventually holds a password.

#![no_std]
#![no_main]

use clipboard_server::{Ring, RingError};
use libkern::control::Control;
use libkern::debug::Line;
use libkern::*;
use librsproto::clipboard::{
    CLIP_ENTRY_HEAD, CLIP_ERR_MALFORMED, CLIP_ERR_STALE, CLIP_INFO_LEN, CLIP_LIST_HEAD, CLIP_RING, ClipEntry, ClipInfo,
    ClipPaste, MAX_CLIP_BYTES, OP_CLIP_COPY, OP_CLIP_LIST, OP_CLIP_PASTE, write_list,
};
use librsproto::error::error_body;
use librsproto::namespace::{OBJECT_KIND_CHANNEL, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

/// IPC payload starts after the 24-byte `IpcMsg` header.
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

/// How many clients may hold a session: generous, as every application namespace binds this
/// endpoint. The wait set's other two are the endpoint and the control channel.
const MAX_SESSIONS: usize = libkern::abi::MAX_WAIT_HANDLES - 2;
const _: () = assert!(2 + MAX_SESSIONS <= libkern::abi::MAX_WAIT_HANDLES);

static mut RING: Ring = Ring::new();

/// Open sessions, one per client that resolved `/dev/clipboard`. `0` marks a free slot.
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] = [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] = [0; 24 * libkern::abi::MAX_WAIT_HANDLES];
static mut CH_OUT0: u64 = 0;
static mut CH_OUT1: u64 = 0;
static mut REPLY_BODY: [u8; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES] = [0; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES];

fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

fn exit(code: i64) -> ! {
    // SAFETY: SYS_PROCESS_EXIT terminates this process.
    unsafe { syscall1(SYS_PROCESS_EXIT, code as u64) };
    loop {
        core::hint::spin_loop();
    }
}

/// Create a connected channel pair (depth 4). Returns `(client_end, serve_end)`.
fn make_channel() -> Option<(u64, u64)> {
    // SAFETY: CH_OUT0/CH_OUT1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CH_OUT0) as u64, (&raw mut CH_OUT1) as u64, 4, 0) };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const CH_OUT0).read(), (&raw const CH_OUT1).read()) })
}

fn send_ready(control: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 24];
    let Some(body_len) = librsproto::meta::ready(&mut body, b"clipboard-server") else {
        return false;
    };
    // SAFETY: REPLY_MSG is a valid 4 KiB buffer; the rsproto message goes at offset 24.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], librsproto::OP_READY, 0, 0, &body[..body_len], 1) {
            Some(n) => n,
            None => return false,
        }
    };
    // SAFETY: stamp the IpcMsg header (payload_len @4, handle_count @8) + handle slot.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = client_end;
    }
    // SAFETY: valid endpoint + message + 1-handle transfer. NoBlock: the control inbox starts
    // empty.
    let sr = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            control,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    sr == 0
}

fn send_reply(ch: u64, op: u16, request_id: u64, body: &[u8], error: bool) {
    let flags = if error { RS_FLAG_REPLY | RS_FLAG_ERROR } else { RS_FLAG_REPLY };
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at PAYLOAD_OFF.
    unsafe {
        let Some(rs_len) = encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, flags, body, 0) else {
            return;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// Refuse a request with a `KError` **and** a `server_code`: a stale cycle and a malformed body
/// share `InvalidArgument` and mean opposite things (`docs/reference/error-codes.md`).
fn refuse(ch: u64, op: u16, request_id: u64, kerror: i32, server_code: u32) {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN];
    let Some(n) = error_body(&mut body, kerror, server_code, b"") else { return };
    send_reply(ch, op, request_id, &body[..n], true);
}

fn serve_resolve(serve_end: u64) {
    // SAFETY: single-threaded server; `RECV_MSG` is lent for the call.
    let Ok(_sent) = (unsafe { channel_recv_raw(serve_end, &raw mut RECV_MSG) }) else {
        return;
    };
    // SAFETY: bounded read-only slice over the payload the kernel just wrote.
    let (op, request_id, bare) = unsafe {
        let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        match decode(req) {
            // An empty suffix only: `/dev/clipboard` has no second level to resolve into.
            Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                Some(r) if r.suffix.is_empty() => (m.op, m.request_id, true),
                _ => (m.op, m.request_id, false),
            },
            Ok(m) => (m.op, m.request_id, false),
            Err(_) => return,
        }
    };
    if !bare {
        refuse(serve_end, op, request_id, KError::NotFound.as_i32(), 0);
        return;
    }
    // SAFETY: single-threaded scan of our own session table.
    let slot = unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == 0) };
    let Some(slot) = slot else {
        refuse(serve_end, op, request_id, KError::WouldBlock.as_i32(), 0);
        return;
    };
    let Some((client_end, session_end)) = make_channel() else {
        refuse(serve_end, op, request_id, KError::KernelError.as_i32(), 0);
        return;
    };
    // Bound before replying, so a fast client's first request cannot arrive before the slot is
    // live.
    // SAFETY: `slot` is free.
    unsafe { SESSION_CH[slot] = session_end };
    if !reply_session_handle(serve_end, request_id, client_end) {
        // SAFETY: the transfer failed, so both ends are still ours.
        unsafe {
            SESSION_CH[slot] = 0;
            syscall1(SYS_HANDLE_CLOSE, session_end);
            syscall1(SYS_HANDLE_CLOSE, client_end);
        }
    }
}

fn reply_session_handle(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the reply goes at PAYLOAD_OFF and the transferred
    // handle in REPLY_HANDLES[0].
    unsafe {
        let Some(rs_len) = encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1)
        else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = client_end;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

fn free_session(ch: u64) {
    // SAFETY: single-threaded table; closing a handle this process owns.
    unsafe {
        #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
        for i in 0..MAX_SESSIONS {
            if SESSION_CH[i] == ch {
                SESSION_CH[i] = 0;
            }
        }
        syscall1(SYS_HANDLE_CLOSE, ch);
    }
}

fn serve_session(ch: u64) {
    // SAFETY: single-threaded server; `RECV_MSG` is lent for the call.
    let Ok(_sent) = (unsafe { channel_recv_raw(ch, &raw mut RECV_MSG) }) else {
        // A closed peer frees the slot; otherwise every exited application holds one forever.
        free_session(ch);
        return;
    };
    // SAFETY: bounded read-only slice over the payload the kernel just wrote. The borrow ends
    // before anything writes to `RECV_MSG` again — nothing below receives.
    let msg = unsafe {
        let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        decode(req)
    };
    let Ok(msg) = msg else { return };
    match msg.op {
        OP_CLIP_COPY => serve_copy(ch, msg.request_id, msg.body),
        OP_CLIP_PASTE => serve_paste(ch, msg.request_id, msg.body),
        OP_CLIP_LIST => serve_list(ch, msg.request_id),
        // `Unsupported`, not `InvalidArgument`: well-formed, just not for this resource.
        op => refuse(ch, op, msg.request_id, KError::Unsupported.as_i32(), 0),
    }
}

fn serve_copy(ch: u64, request_id: u64, body: &[u8]) {
    let Some(entry) = ClipEntry::read(body) else {
        refuse(ch, OP_CLIP_COPY, request_id, KError::InvalidArgument.as_i32(), CLIP_ERR_MALFORMED);
        return;
    };
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: single-threaded server; the ring is touched from nowhere else.
    let pushed = unsafe { (*(&raw mut RING)).push(entry.kind, entry.bytes) };
    match pushed {
        Ok(serial) => {
            let mut out = [0u8; 8];
            out.copy_from_slice(&serial.to_le_bytes());
            send_reply(ch, OP_CLIP_COPY, request_id, &out, false);
            Line::new()
                .s(b"clipboard-server: copied ")
                .i(entry.bytes.len() as i64)
                .s(b" bytes, serial ")
                .i(serial as i64)
                .end();
        }
        Err(RingError::TooLarge) => refuse(ch, OP_CLIP_COPY, request_id, KError::TooLarge.as_i32(), 0),
        // An arm rather than `unreachable!()`: a panic takes the clipboard down for every client.
        Err(_) => refuse(ch, OP_CLIP_COPY, request_id, KError::KernelError.as_i32(), 0),
    }
}

fn serve_paste(ch: u64, request_id: u64, body: &[u8]) {
    let Some(req) = ClipPaste::read(body) else {
        refuse(ch, OP_CLIP_PASTE, request_id, KError::InvalidArgument.as_i32(), CLIP_ERR_MALFORMED);
        return;
    };
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: single-threaded server; the ring is read here and written only in `serve_copy`.
    let ring = unsafe { &*(&raw const RING) };
    match ring.get(req.index as usize, req.expect) {
        Ok((kind, bytes)) => {
            let entry = ClipEntry { serial: ring.serial(), kind, bytes };
            #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
            // SAFETY: REPLY_BODY is sized for an entry at the cap, which is the largest this
            // can be, and nothing else is borrowing it.
            let n = unsafe { entry.write(&mut *(&raw mut REPLY_BODY)) };
            let Some(n) = n else {
                refuse(ch, OP_CLIP_PASTE, request_id, KError::KernelError.as_i32(), 0);
                return;
            };
            #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
            // SAFETY: `n` bytes were just written; the slice is read-only and not aliased.
            // The reference is spelled out rather than left to autoref — a raw pointer's
            // target has to be valid *and* unaliased for one to exist, and `deny`ing the
            // implicit form is how that stays a decision rather than an accident.
            let out = unsafe { &(&*(&raw const REPLY_BODY))[..n] };
            send_reply(ch, OP_CLIP_PASTE, request_id, out, false);
        }
        Err(RingError::Stale) => {
            refuse(ch, OP_CLIP_PASTE, request_id, KError::InvalidArgument.as_i32(), CLIP_ERR_STALE)
        }
        Err(_) => refuse(ch, OP_CLIP_PASTE, request_id, KError::NotFound.as_i32(), 0),
    }
}

fn serve_list(ch: u64, request_id: u64) {
    let mut rows = [ClipInfo::default(); CLIP_RING];
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: single-threaded server.
    let ring = unsafe { &*(&raw const RING) };
    let n = ring.list(&mut rows);
    let mut body = [0u8; CLIP_LIST_HEAD + CLIP_RING * CLIP_INFO_LEN];
    let Some(len) = write_list(&mut body, ring.serial(), &rows[..n]) else {
        refuse(ch, OP_CLIP_LIST, request_id, KError::KernelError.as_i32(), 0);
        return;
    };
    send_reply(ch, OP_CLIP_LIST, request_id, &body[..len], false);
}

fn serve_loop(serve_end: u64, mut control: u64) -> ! {
    kprint(b"clipboard-server: serving Clipboard ops over /dev/clipboard\n");
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots and `n` is bounded by
        // `2 + MAX_SESSIONS`, which is that limit by construction (asserted above).
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
            // The control channel, for `service --stop`; dropped once its supervisor has gone,
            // when it would stay signalled for good.
            if control != 0 {
                WAIT_HANDLES[n] = control;
                n += 1;
            }
            #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
            for i in 0..MAX_SESSIONS {
                if SESSION_CH[i] != 0 {
                    WAIT_HANDLES[n] = SESSION_CH[i];
                    n += 1;
                }
            }
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, n as u64, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        if waited < 1 {
            continue;
        }
        for j in 0..(waited as usize) {
            let off = j * 24;
            // SAFETY: `waited` records were written; `off + 8` stays inside WAIT_RESULTS.
            let h = unsafe {
                u64::from_le_bytes([
                    WAIT_RESULTS[off],
                    WAIT_RESULTS[off + 1],
                    WAIT_RESULTS[off + 2],
                    WAIT_RESULTS[off + 3],
                    WAIT_RESULTS[off + 4],
                    WAIT_RESULTS[off + 5],
                    WAIT_RESULTS[off + 6],
                    WAIT_RESULTS[off + 7],
                ])
            };
            if h == serve_end {
                serve_resolve(serve_end);
            } else if h == control {
                match libkern::control::recv(control) {
                    Control::Op(CTRL_OP_SHUTDOWN) => {
                        kprint(b"clipboard-server: asked to stop, exiting\n");
                        exit(0);
                    }
                    Control::Closed => control = 0,
                    _ => {}
                }
            } else {
                serve_session(h);
            }
        }
    }
}

/// Bootstrap registers: `rdx` is the control channel its supervisor installed; the rest are unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"clipboard-server: up\n");
    let Some((client_end, serve_end)) = make_channel() else {
        kprint(b"clipboard-server: channel create FAIL\n");
        exit(1);
    };
    if !send_ready(control, client_end) {
        kprint(b"clipboard-server: Ready send FAIL\n");
        exit(1);
    }
    serve_loop(serve_end, control);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"clipboard-server: PANIC\n");
    exit(1);
}
