//! `auth-service` — the userspace **authentication service** (Phase 3).
//!
//! A request/reply resource server that validates credentials: it answers the `Auth`
//! rsproto category (`Authenticate { username, password } → { AUTHENTICATED,
//! principal, home } | DENIED`, `docs/spec/rsproto-auth-ops.md`) on a plain IPC
//! channel. Unlike the fs / profile servers it is **not** a namespace forwarder — a
//! client (session-mgr) holds a direct channel and sends `Authenticate` requests; no
//! path is resolved. The credential logic (user-DB parse + PBKDF2 verify) is the
//! host-tested `auth_service` library; this binary is the bare-target syscall
//! plumbing: read `/system/users`, hand the supervisor a client endpoint via
//! `Meta::Ready`, then serve.
//!
//! `#![no_std]` + `#![no_main]`, **no `alloc`** — fixed `.bss` buffers, no
//! `#[global_allocator]` (the DB + messages are bounded). `libkern` + `libcrypto`
//! (via the lib) + `librsproto`. See `userspace/auth-service/CLAUDE.md`.

#![no_std]
#![no_main]

use auth_service::serve_authenticate;
use libkern::*;
use librsproto::auth::build_denied_reply;
use librsproto::{OP_AUTHENTICATE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

/// One page; the map granularity for reading the user DB.
const PAGE: u64 = 4096;
/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// Largest user database we hold (one page — the demo DB is a handful of lines).
const DB_MAX: usize = 4096;

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut CH_OUT0: u64 = 0;
static mut CH_OUT1: u64 = 0;
/// The user database, copied from `/system/users` at startup.
static mut USER_DB: [u8; DB_MAX] = [0; DB_MAX];
static mut USER_DB_LEN: usize = 0;

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

/// Resolve `path` in `ns` requesting `rights`; return the resolved handle, or `0`.
/// Waits + closes the `PendingOperation`.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return 0;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // IoResult: status @8..12, resolved handle @16..24.
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 || status != 0 { 0 } else { handle }
}

/// Read `/system/users` into `USER_DB`, recording its length in `USER_DB_LEN`.
/// Returns `false` on any failure (the server then denies every credential — a
/// missing DB must never authenticate).
fn load_db(root_ns: u64) -> bool {
    let fh = ns_lookup(root_ns, b"/system/users", RIGHT_MAP_READ | RIGHT_INSPECT);
    if fh == 0 {
        return false;
    }
    // Stat for the exact byte size (the file is a small text DB).
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `&mut info` is a valid 24-byte HandleInfo out-param.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, fh, (&mut info as *mut HandleInfo) as u64) };
    if sr < 0 || info.size == 0 || info.size as usize > DB_MAX {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return false;
    }
    let size = info.size as usize;
    // SAFETY: `fh` is a mappable file handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return false;
    }
    // SAFETY: `addr..addr+size` maps `size` valid bytes of the DB (size ≤ DB_MAX ≤ PAGE);
    // copy them into our owned buffer, then unmap + close.
    unsafe {
        let src = core::slice::from_raw_parts(addr as u64 as *const u8, size);
        let dst = core::slice::from_raw_parts_mut((&raw mut USER_DB) as *mut u8, size);
        dst.copy_from_slice(src);
        USER_DB_LEN = size;
        syscall2(SYS_MEMORY_UNMAP, addr as u64, PAGE);
        syscall1(SYS_HANDLE_CLOSE, fh);
    }
    true
}

/// Create a connected channel pair (depth 4). Returns `(client_end, serve_end)`: the
/// supervisor routes `client_end` to session-mgr (which sends `Authenticate` on it);
/// the server serves on `serve_end`. `None` on failure.
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

/// Send `Meta::Ready` on the control channel, transferring `client_end` (the endpoint
/// a client sends `Authenticate` on). `false` on any failure.
fn send_ready(control: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let body_len = match librsproto::meta::ready(&mut body, b"auth-service") {
        Some(n) => n,
        None => return false,
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
    // SAFETY: valid endpoint + message + 1-handle transfer. NoBlock: the control inbox
    // starts empty.
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

/// Send a reply body on `serve_end`, echoing `request_id`, with `error` set iff this
/// is a malformed-request error reply. No handles are transferred.
fn send_reply(serve_end: u64, request_id: u64, body: &[u8], error: bool) {
    let flags = if error { RS_FLAG_REPLY | RS_FLAG_ERROR } else { RS_FLAG_REPLY };
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset 24.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_AUTHENTICATE, request_id, flags, body, 0) {
            Some(n) => n,
            None => return,
        }
    };
    // SAFETY: stamp the header (payload_len @4, handle_count @8 = 0) and send.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// The serve loop: block for an `Authenticate` request, validate it against the DB,
/// and reply. Never returns.
fn serve_loop(serve_end: u64) -> ! {
    kprint(b"auth-service: serving Auth::Authenticate\n");
    loop {
        // SAFETY: one waiter on the serving endpoint.
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        if waited != 1 {
            continue;
        }
        // SAFETY: valid recv out-params (an Authenticate carries no transferred handles).
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
            continue;
        }

        // Decode the rsproto request from the IpcMsg payload (offset 24, `payload_len`),
        // then build the reply into a local body buffer over non-aliasing statics.
        let mut reply_body = [0u8; 512];
        // SAFETY: read the header length + form a bounded read-only slice over RECV_MSG;
        // `USER_DB`/`USER_DB_LEN` are read-only here (written once at startup).
        let (request_id, reply_len, error) = unsafe {
            let payload_len =
                u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match decode(req) {
                Ok(m) if m.op == OP_AUTHENTICATE => {
                    let db = core::slice::from_raw_parts((&raw const USER_DB) as *const u8, USER_DB_LEN);
                    match serve_authenticate(m.body, db, &mut reply_body) {
                        Some(n) => (m.request_id, n, false),
                        // Malformed request → an error reply.
                        None => {
                            let n = build_denied_reply(&mut reply_body).unwrap_or(0);
                            (m.request_id, n, true)
                        }
                    }
                }
                // A non-Auth op on this channel: deny + error (wrong protocol).
                Ok(m) => {
                    let n = build_denied_reply(&mut reply_body).unwrap_or(0);
                    (m.request_id, n, true)
                }
                Err(_) => (0, 0, true),
            }
        };
        if reply_len > 0 {
            send_reply(serve_end, request_id, &reply_body[..reply_len], error);
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the inherited
/// root namespace (resolves `/system/users`), `rdx` = the control-channel endpoint the
/// supervisor installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"auth-service: up\n");
    if !load_db(root_ns) {
        // No DB → the server would deny everything; that is a misconfiguration, so
        // fail loudly rather than run a useless authenticator.
        kprint(b"auth-service: /system/users load FAIL\n");
        exit(1);
    }
    kprint(b"auth-service: user database loaded\n");

    let (client_end, serve_end) = match make_channel() {
        Some(pair) => pair,
        None => {
            kprint(b"auth-service: channel create FAIL\n");
            exit(1);
        }
    };
    if !send_ready(control, client_end) {
        kprint(b"auth-service: Ready send FAIL\n");
        exit(1);
    }
    serve_loop(serve_end);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"auth-service: PANIC\n");
    exit(1);
}
