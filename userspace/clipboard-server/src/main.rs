//! `clipboard-server` — the kill ring, served at `/dev/clipboard`.
//!
//! A resource server in the ordinary shape: `init` spawns it with a control channel, it answers
//! `Meta::Ready` with a forwarding endpoint, and `init` binds that endpoint. A client that
//! resolves `/dev/clipboard` gets a session channel of its own and speaks the `Clipboard`
//! category on it. It binds nothing itself and holds no `BIND_NAMESPACE` — the rule every
//! resource server here follows (`docs/rationale/why-supervisor-registration.md`).
//!
//! **Why a server at all, rather than a slot in the compositor.** M12 decision 1: a clipboard is
//! shared mutable state between mutually untrusting programs, and "anything running may read
//! what you last copied" is ambient authority — the mechanism by which a password manager's
//! clipboard gets scraped on real systems. Here the binding *is* the authority: a namespace
//! without `/dev/clipboard` has no clipboard, and an endpoint attenuated to `RIGHT_SEND` is a
//! profile that can copy and not read.
//!
//! **And why it stores rather than brokers.** Wayland's model keeps the data in the copier and
//! has the compositor arrange a transfer on paste, so nothing is held by a third party. It was
//! rejected because the clipboard then dies with the application you copied from, which is what
//! people install clipboard managers to escape.
//!
//! The ring itself — which entry index 0 names, what a wrap does, when a cycle is refused — is
//! [`clipboard_server::Ring`], host-tested. This file is the plumbing.
//!
//! `#![no_std]` + `#![no_main]`, **no `alloc`**: the ring is a bounded `.bss` object, so there
//! is nothing here to allocate.
//!
//! **It never logs an entry's bytes.** Counts, serials, lengths and kinds only. A clipboard
//! eventually holds a password, and the serial console is a log file.

#![no_std]
#![no_main]

use clipboard_server::{Ring, RingError};
use libkern::debug::Line;
use libkern::*;
use librsproto::clipboard::{
    CLIP_ENTRY_HEAD, CLIP_ERR_MALFORMED, CLIP_ERR_STALE, CLIP_INFO_LEN, CLIP_LIST_HEAD, CLIP_RING,
    ClipEntry, ClipInfo, ClipPaste, MAX_CLIP_BYTES, OP_CLIP_COPY, OP_CLIP_LIST, OP_CLIP_PASTE,
    write_list,
};
use librsproto::error::error_body;
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply,
};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

/// How many clients may hold a clipboard session at once.
///
/// **Generous on purpose, and the bound is the wait set's rather than a guess.** Unlike
/// `/svc/auth`, whose two callers are both supervisors, this endpoint is bound into *every*
/// application namespace the shell constructs and into both session namespaces — so the editor,
/// the browser, the terminal and every `clip` a pipeline runs each want one.
const MAX_SESSIONS: usize = libkern::abi::MAX_WAIT_HANDLES - 1;
const _: () = assert!(1 + MAX_SESSIONS <= libkern::abi::MAX_WAIT_HANDLES);

/// The ring, in `.bss`. See [`Ring`] for why it is fixed-size.
static mut RING: Ring = Ring::new();

/// Open sessions, one per client that resolved `/dev/clipboard`. `0` marks a free slot.
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
/// Sized from the **ABI**, not from what this server expects.
///
/// `sys_channel_recv` passes no receiver-side capacity: the kernel copies out `n * 8` bytes
/// where `n` is the *sender's* stamped count, bounded only by `IPC_HANDLE_MAX`. This endpoint
/// is bound into every application namespace, so a client sending a request with eight handles
/// attached would smash whatever follows a shorter array — no bug in the client required
/// (PR #245 review, blocking 2, against the identical array in `desktop-shell`).
static mut RECV_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] = [0; libkern::abi::IPC_HANDLE_MAX];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] =
    [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] =
    [0; 24 * libkern::abi::MAX_WAIT_HANDLES];
static mut CH_OUT0: u64 = 0;
static mut CH_OUT1: u64 = 0;
/// Where a reply body is built. Big enough for the largest of the three: an entry at the cap.
static mut REPLY_BODY: [u8; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES] =
    [0; CLIP_ENTRY_HEAD + MAX_CLIP_BYTES];

/// Emit `msg` to the serial console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Exit the process (does not return).
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
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CH_OUT0) as u64, (&raw mut CH_OUT1) as u64, 4, 0)
    };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const CH_OUT0).read(), (&raw const CH_OUT1).read()) })
}

/// Send `Meta::Ready` on the control channel, transferring the forwarding endpoint.
fn send_ready(control: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 24];
    let Some(body_len) = librsproto::meta::ready(&mut body, b"clipboard-server") else {
        return false;
    };
    // SAFETY: REPLY_MSG is a valid 4 KiB buffer; the rsproto message goes at offset 24.
    let rs_len = unsafe {
        match encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            librsproto::OP_READY,
            0,
            0,
            &body[..body_len],
            1,
        ) {
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

/// Send one reply on `ch`, echoing `op` and `request_id`.
fn send_reply(ch: u64, op: u16, request_id: u64, body: &[u8], error: bool) {
    let flags = if error { RS_FLAG_REPLY | RS_FLAG_ERROR } else { RS_FLAG_REPLY };
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at PAYLOAD_OFF.
    unsafe {
        let Some(rs_len) = encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, flags, body, 0)
        else {
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

/// Refuse a request: a `KError` and the finer `server_code` beside it.
///
/// **Both, always.** Two of this server's refusals share `InvalidArgument` and mean opposite
/// things to a client — a stale cycle is *start again from the newest*, a malformed body is *a
/// bug in this program*. `docs/reference/error-codes.md` is explicit that a condition only one
/// component can produce belongs in `server_code` rather than in a new discriminant, and this is
/// the other half of honouring that: the code has to actually be sent.
fn refuse(ch: u64, op: u16, request_id: u64, kerror: i32, server_code: u32) {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN];
    let Some(n) = error_body(&mut body, kerror, server_code, b"") else { return };
    send_reply(ch, op, request_id, &body[..n], true);
}

/// Answer a resolve of `/dev/clipboard` by minting a session channel for the caller.
fn serve_resolve(serve_end: u64) {
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            serve_end,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr != 0 {
        return;
    }
    // SAFETY: bounded read-only slice over the payload the kernel just wrote.
    let (op, request_id, bare) = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        match decode(req) {
            // **An empty suffix only.** `/dev/clipboard` is the resource itself. A suffix would
            // name something beneath it — a per-entry path, say — and this server has no second
            // level, so answering one would invent a namespace it does not have.
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
    // live — the ordering `auth-service` states and `desktop-shell` copies.
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

/// Reply to a resolve with the minted session channel in `handles[0]`.
fn reply_session_handle(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the reply goes at PAYLOAD_OFF and the transferred
    // handle in REPLY_HANDLES[0].
    unsafe {
        let Some(rs_len) = encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            OP_NS_RESOLVE,
            request_id,
            RS_FLAG_REPLY,
            &body,
            1,
        ) else {
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

/// Close a session channel and free its slot.
fn free_session(ch: u64) {
    // SAFETY: single-threaded table; closing a handle this process owns.
    unsafe {
        for i in 0..MAX_SESSIONS {
            if SESSION_CH[i] == ch {
                SESSION_CH[i] = 0;
            }
        }
        syscall1(SYS_HANDLE_CLOSE, ch);
    }
}

/// Serve one request on an open session. Frees the slot when the peer closes.
fn serve_session(ch: u64) {
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr != 0 {
        // **A closed peer frees the slot.** Without this every application that exits holds one
        // forever, and this endpoint has many more clients than `/svc/auth` does.
        free_session(ch);
        return;
    }
    // SAFETY: bounded read-only slice over the payload the kernel just wrote. The borrow ends
    // before anything writes to `RECV_MSG` again — nothing below receives.
    let msg = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
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
        // A category this server does not speak. `Unsupported` rather than `InvalidArgument`:
        // the request was well-formed and simply is not for this resource.
        op => refuse(ch, op, msg.request_id, KError::Unsupported.as_i32(), 0),
    }
}

/// `Clipboard::Copy` — push, and answer with the ring's new serial.
fn serve_copy(ch: u64, request_id: u64, body: &[u8]) {
    let Some(entry) = ClipEntry::read(body) else {
        refuse(ch, OP_CLIP_COPY, request_id, KError::InvalidArgument.as_i32(), CLIP_ERR_MALFORMED);
        return;
    };
    // SAFETY: single-threaded server; the ring is touched from nowhere else.
    let pushed = unsafe { (*(&raw mut RING)).push(entry.kind, entry.bytes) };
    match pushed {
        Ok(serial) => {
            let mut out = [0u8; 8];
            out.copy_from_slice(&serial.to_le_bytes());
            send_reply(ch, OP_CLIP_COPY, request_id, &out, false);
            // **A count and a length, never the bytes** — see this module's header.
            Line::new()
                .s(b"clipboard-server: copied ")
                .i(entry.bytes.len() as i64)
                .s(b" bytes, serial ")
                .i(serial as i64)
                .end();
        }
        Err(RingError::TooLarge) => {
            refuse(ch, OP_CLIP_COPY, request_id, KError::TooLarge.as_i32(), 0)
        }
        // `push` returns nothing else; an arm rather than an `unreachable!()` because this is a
        // server and a panic here takes the clipboard down for every client.
        Err(_) => refuse(ch, OP_CLIP_COPY, request_id, KError::KernelError.as_i32(), 0),
    }
}

/// `Clipboard::Paste` — read one entry by index, refusing a cycle the ring has moved under.
fn serve_paste(ch: u64, request_id: u64, body: &[u8]) {
    let Some(req) = ClipPaste::read(body) else {
        refuse(ch, OP_CLIP_PASTE, request_id, KError::InvalidArgument.as_i32(), CLIP_ERR_MALFORMED);
        return;
    };
    // SAFETY: single-threaded server; the ring is read here and written only in `serve_copy`.
    let ring = unsafe { &*(&raw const RING) };
    match ring.get(req.index as usize, req.expect) {
        Ok((kind, bytes)) => {
            let entry = ClipEntry { serial: ring.serial(), kind, bytes };
            // SAFETY: REPLY_BODY is sized for an entry at the cap, which is the largest this
            // can be, and nothing else is borrowing it.
            let n = unsafe { entry.write(&mut *(&raw mut REPLY_BODY)) };
            let Some(n) = n else {
                refuse(ch, OP_CLIP_PASTE, request_id, KError::KernelError.as_i32(), 0);
                return;
            };
            // SAFETY: `n` bytes were just written; the slice is read-only and not aliased.
            // The reference is spelled out rather than left to autoref — a raw pointer's
            // target has to be valid *and* unaliased for one to exist, and `deny`ing the
            // implicit form is how that stays a decision rather than an accident.
            let out = unsafe { &(&*(&raw const REPLY_BODY))[..n] };
            send_reply(ch, OP_CLIP_PASTE, request_id, out, false);
        }
        Err(RingError::Stale) => refuse(
            ch,
            OP_CLIP_PASTE,
            request_id,
            KError::InvalidArgument.as_i32(),
            CLIP_ERR_STALE,
        ),
        Err(_) => refuse(ch, OP_CLIP_PASTE, request_id, KError::NotFound.as_i32(), 0),
    }
}

/// `Clipboard::List` — the ring's shape, without its contents.
fn serve_list(ch: u64, request_id: u64) {
    let mut rows = [ClipInfo::default(); CLIP_RING];
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

/// The serve loop: the forwarding endpoint plus every open session. Never returns.
fn serve_loop(serve_end: u64) -> ! {
    kprint(b"clipboard-server: serving Clipboard ops over /dev/clipboard\n");
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots and `n` is bounded by
        // `1 + MAX_SESSIONS`, which is that limit by construction (asserted above).
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
            for i in 0..MAX_SESSIONS {
                if SESSION_CH[i] != 0 {
                    WAIT_HANDLES[n] = SESSION_CH[i];
                    n += 1;
                }
            }
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                n as u64,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
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
            } else {
                serve_session(h);
            }
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the inherited root
/// namespace (unused — this server reads nothing), `rdx` = the control channel `init` installed,
/// `rcx` = `arg0` (unused).
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
    serve_loop(serve_end);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"clipboard-server: PANIC\n");
    exit(1);
}
