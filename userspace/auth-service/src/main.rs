//! `auth-service` — the userspace **authentication service**.
//!
//! A request/reply resource server that answers the `Auth` rsproto category (`Authenticate {
//! username, password } → { AUTHENTICATED, principal, home } | DENIED`,
//! `docs/spec/rsproto-auth-ops.md`). Like the fs / profile servers it is a namespace forwarder:
//! `service-mgr` binds `/svc/auth` to reach it and it mints a session channel per caller that
//! resolves there, which is what lets `session-mgr` and `desktop-session-mgr` each hold one. The
//! credential logic is the host-tested `auth_service` library; this binary is the bare-target
//! syscall plumbing: read `/system/users`, hand the supervisor a client endpoint via
//! `Meta::Ready`, then serve.
//!
//! **And the database's only writer.** A resolve of `/svc/auth/admin` opens an *admin* session,
//! which answers `List`, `Add`, `Remove` and `SetPassword`. The view
//! broker is the one client meant to hold one, since it fronts every account operation and applies
//! the guards; anything holding the root namespace can resolve it, the same boundary `/svc/auth`
//! has always had (`TODO(svc-auth-ungated)`). A write replaces the file atomically — written to
//! `/system/users.new`, synced, then renamed over — and the copy in memory changes only once the
//! rename has held.
//!
//! `#![no_std]` + `#![no_main]`, **no `alloc`** — stack buffers, no `#[global_allocator]`
//! (the DB + messages are bounded). See `userspace/auth-service/CLAUDE.md`.

#![no_std]
#![no_main]

use auth_service::{Admin, serve_admin, serve_authenticate};
use libkern::debug::Line;
use libkern::*;
use librsproto::auth::{
    OP_AUTH_ADD, OP_AUTH_LIST, OP_AUTH_REMOVE, OP_AUTH_SET_PASSWORD, build_denied_reply, parse_account_request,
};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, RENAME_REPLACE, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply,
};
use librsproto::{OP_AUTHENTICATE, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, RS_MAX_BODY, decode, encode};

/// One page; the map granularity for reading the user DB.
const PAGE: u64 = 4096;
/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// Largest user database we hold: one page, and `libusers`' bound, so no writer of the file can
/// make one this server will not load.
const DB_MAX: usize = libusers::MAX_FILE;
const _: () = assert!(DB_MAX as u64 <= PAGE);
/// The file, and the name a new copy is written under before it is renamed over it.
const USERS: &[u8] = b"/system/users";
const USERS_NEW: &[u8] = b"/system/users.new";

/// How many clients may hold an auth session at once.
///
/// Two slots go to the forwarding endpoint and the control channel; the rest are sessions. The
/// limit is the wait set's, so it costs nothing to be generous.
const MAX_SESSIONS: usize = libkern::abi::MAX_WAIT_HANDLES - 2;
const _: () = assert!(2 + MAX_SESSIONS <= libkern::abi::MAX_WAIT_HANDLES);

/// How long a failed receive, its message still queued, pauses: its wait fires again at once.
const HELD_PARK_NS: u64 = 10_000_000;

/// **This server's state**, in `_start`'s frame rather than in `static mut`s.
struct Server {
    /// The user database, copied from `/system/users` at startup, and its length.
    db: [u8; DB_MAX],
    db_len: usize,
    /// An administrator's request writes the new database here first.
    new_db: [u8; DB_MAX],
    /// An admin reply's body — the account list is the long one.
    admin_reply: [u8; RS_MAX_BODY],
    /// Open auth sessions, one per client that resolved `/svc/auth`, each with whether it is an
    /// **admin** one, which answers only the administrative ops, or an oracle one.
    sessions: [Option<(u64, bool)>; MAX_SESSIONS],
    /// The namespace the file is read and written through: the inherited root.
    root_ns: u64,
    /// The kernel's entropy source, where a new password's salt comes from. `0` if it would not
    /// open, and then every request that needs a salt is refused.
    entropy: u64,
}

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

/// Resolve `path` in `ns` requesting `rights`; the resolved handle, or `0`. Closes the PO.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return 0;
    }
    let (status, handle) = po_wait(po as u64);
    if status != 0 { 0 } else { handle }
}

/// Read `/system/users` into the server's copy, recording its length. `false` on any
/// failure: a missing DB must never authenticate.
fn load_db(srv: &mut Server, root_ns: u64) -> bool {
    let fh = ns_lookup(root_ns, b"/system/users", RIGHT_MAP_READ | RIGHT_INSPECT);
    if fh == 0 {
        return false;
    }
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
        srv.db[..size].copy_from_slice(core::slice::from_raw_parts(addr as u64 as *const u8, size));
        syscall2(SYS_MEMORY_UNMAP, addr as u64, PAGE);
        syscall1(SYS_HANDLE_CLOSE, fh);
    }
    srv.db_len = size;
    true
}

/// Create a connected channel pair (depth 4), `(client_end, serve_end)`. `None` on failure.
fn make_channel() -> Option<(u64, u64)> {
    let (mut client, mut serve) = (0u64, 0u64);
    // SAFETY: two writable out-params on this frame.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut client) as u64, (&raw mut serve) as u64, 4, 0) };
    (cr == 0).then_some((client, serve))
}

/// Send one rsproto message on `ch`, moving `handle` with it unless it is `0`. `false` on failure.
fn send(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handle: u64) -> bool {
    let mut msg = [0u8; MSG_LEN];
    let count = u16::from(handle != 0);
    let Some(rs_len) = encode(&mut msg[PAYLOAD_OFF..], op, request_id, flags, body, count) else {
        return false;
    };
    msg[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
    msg[8] = count as u8;
    let handles = [handle];
    // SAFETY: a whole message, and `count` handles, both on this frame.
    let sr = unsafe {
        syscall5(SYS_CHANNEL_SEND, ch, msg.as_ptr() as u64, handles.as_ptr() as u64, count as u64, SENDMODE_NOBLOCK)
    };
    sr == 0
}

/// The rsproto payload of the message in `msg`, as long as its header says, within the message.
fn payload(msg: &[u8; MSG_LEN]) -> &[u8] {
    let len = u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
    &msg[PAYLOAD_OFF..PAYLOAD_OFF + len.min(MSG_LEN - PAYLOAD_OFF)]
}

/// Wait out [`HELD_PARK_NS`], on no handles, which the kernel never refuses.
fn park() {
    let mut now = 0u64;
    // SAFETY: `now` is a writable u64 out-param on this frame.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    // SAFETY: no handles and no results, so the kernel reads and writes neither.
    unsafe { syscall4(SYS_WAIT, 0, 0, 0, now.saturating_add(HELD_PARK_NS)) };
}

/// Send `Meta::Ready` on the control channel, transferring `client_end`. `false` on any failure.
fn send_ready(control: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let body_len = match librsproto::meta::ready(&mut body, b"auth-service") {
        Some(n) => n,
        None => return false,
    };
    // NoBlock: the control inbox starts empty.
    send(control, librsproto::OP_READY, 0, 0, &body[..body_len], client_end)
}

/// Send a reply body for `op` on `serve_end`, echoing `request_id`; no handles are transferred.
fn send_reply(serve_end: u64, op: u16, request_id: u64, body: &[u8], error: bool) {
    let flags = if error { RS_FLAG_REPLY | RS_FLAG_ERROR } else { RS_FLAG_REPLY };
    send(serve_end, op, request_id, flags, body, 0);
}

/// The serve loop. Never returns.
///
/// **It exits on `CTRL_OP_SHUTDOWN`**, which a shutdown sends every service (`service --stop`
/// never does: `auth-service` is `essential`). Nothing needs saving: every write to the users
/// file is made whole — a new file, synced, then renamed over — before its answer is sent.
fn serve_loop(srv: &mut Server, serve_end: u64, mut control: u64) -> ! {
    kprint(b"auth-service: serving Auth::Authenticate over /svc/auth, and administration at /svc/auth/admin\n");
    loop {
        let mut handles = [0u64; libkern::abi::MAX_WAIT_HANDLES];
        let sessions = srv.sessions.iter().flatten().map(|&(ch, _)| ch);
        let mut n = 0;
        for h in [serve_end, control].into_iter().filter(|&h| h != 0).chain(sessions) {
            handles[n] = h;
            n += 1;
        }
        let mut results = [IoResult { handle: 0, status: 0, reserved: 0, result: 0 }; libkern::abi::MAX_WAIT_HANDLES];
        // SAFETY: `n` handles, and room for a result each, all on this frame; `n` is at most
        // `2 + MAX_SESSIONS`, the wait's limit by construction (asserted above).
        let waited =
            unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, n as u64, results.as_mut_ptr() as u64, u64::MAX) };
        if waited < 1 {
            continue;
        }
        for r in &results[..waited as usize] {
            let h = r.handle;
            if h == serve_end {
                serve_resolve(srv, serve_end);
            } else if h == control {
                match libkern::control::recv(control) {
                    libkern::control::Control::Op(CTRL_OP_SHUTDOWN) => {
                        kprint(b"auth-service: asked to stop, exiting\n");
                        exit(0);
                    }
                    libkern::control::Control::Closed => control = 0,
                    _ => {}
                }
            } else {
                serve_session(srv, h);
            }
        }
    }
}

/// Answer a resolve of `/svc/auth` by minting a session channel for the caller, so one oracle
/// serves several supervisors (`docs/architecture/session-and-auth.md`).
fn serve_resolve(srv: &mut Server, serve_end: u64) {
    let mut msg = [0u8; MSG_LEN];
    let Ok(_sent) = channel_recv(serve_end, &mut msg) else {
        return park();
    };
    let (op, request_id, ok) = {
        match decode(payload(&msg)) {
            // **Two suffixes, nothing else**: empty is the oracle, `admin` the administrative one.
            Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                Some(r) if r.suffix.is_empty() => (m.op, m.request_id, Some(false)),
                Some(r) if r.suffix == b"admin" => (m.op, m.request_id, Some(true)),
                _ => (m.op, m.request_id, None),
            },
            Ok(m) => (m.op, m.request_id, None),
            Err(_) => (0, 0, None),
        }
    };
    let Some(admin) = ok else {
        reply_resolve_error(serve_end, request_id, op, KError::NotFound.as_i32());
        return;
    };
    open_auth_session(srv, serve_end, request_id, admin);
}

/// Mint an oracle or `admin` session channel and hand its client end back in the resolve reply.
fn open_auth_session(srv: &mut Server, serve_end: u64, request_id: u64, admin: bool) {
    let Some(slot) = srv.sessions.iter().position(Option::is_none) else {
        // Every slot in use — ask the caller to retry rather than denying a login outright.
        reply_resolve_error(serve_end, request_id, OP_NS_RESOLVE, KError::WouldBlock.as_i32());
        return;
    };
    let Some((client_end, session_end)) = make_channel() else {
        reply_resolve_error(serve_end, request_id, OP_NS_RESOLVE, KError::KernelError.as_i32());
        return;
    };
    // Bind the slot *before* replying, so a fast client's first request cannot arrive before
    // the slot is live.
    srv.sessions[slot] = Some((session_end, admin));
    if !reply_session_handle(serve_end, request_id, client_end) {
        srv.sessions[slot] = None;
        // SAFETY: the transfer failed; reclaim both ends.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, session_end);
            syscall1(SYS_HANDLE_CLOSE, client_end);
        }
    }
}

fn serve_session(srv: &mut Server, ch: u64) {
    // **Zeroed on every return**: an `Authenticate`, `Add` or `SetPassword` carries a password.
    let mut msg = Secret([0u8; MSG_LEN]);
    match channel_recv(ch, &mut msg) {
        Ok(_sent) => {}
        // **A closed peer frees the slot.** Without this a supervisor that exited would hold
        // one forever, and the set of clients would shrink by one per logout.
        Err(e) if e == KError::PeerClosed.as_i32() as i64 => return free_session(srv, ch),
        // **Nothing else does**: `OutOfMemory` leaves the message queued for a live client.
        Err(_) => return park(),
    }
    let serve_end = ch;
    if srv.sessions.iter().flatten().any(|&(c, admin)| c == ch && admin) {
        return serve_admin_request(srv, ch, payload(&msg));
    }

    let mut reply_body = [0u8; 512];
    let db = &srv.db[..srv.db_len];
    let (request_id, reply_len, error) = match decode(payload(&msg)) {
        Ok(m) if m.op == OP_AUTHENTICATE => match serve_authenticate(m.body, db, &mut reply_body) {
            Some(n) => (m.request_id, n, false),
            None => {
                let n = build_denied_reply(&mut reply_body).unwrap_or(0);
                (m.request_id, n, true)
            }
        },
        // A non-Auth op on this channel: deny + error (wrong protocol).
        Ok(m) => {
            let n = build_denied_reply(&mut reply_body).unwrap_or(0);
            (m.request_id, n, true)
        }
        Err(_) => (0, 0, true),
    };
    if reply_len > 0 {
        send_reply(serve_end, OP_AUTHENTICATE, request_id, &reply_body[..reply_len], error);
    }
}

/// **Serve one administrator's request** on an admin session. A write is installed before it is
/// answered, and the copy in memory changes only once the install holds.
fn serve_admin_request(srv: &mut Server, ch: u64, req: &[u8]) {
    let Ok(m) = decode(req) else {
        return;
    };
    // The name the request is about, for the log: the body of a `Remove`, the first field of an
    // `Add` or a `SetPassword`. Never the password.
    let name: &[u8] = match m.op {
        OP_AUTH_REMOVE => m.body,
        OP_AUTH_ADD | OP_AUTH_SET_PASSWORD => parse_account_request(m.body).map_or(&[], |r| r.username),
        _ => &[],
    };
    let needs_salt = matches!(m.op, OP_AUTH_ADD | OP_AUTH_SET_PASSWORD);
    let salt = if needs_salt { fresh_salt(srv.entropy) } else { Some([0u8; libusers::SALT_LEN]) };
    let Some(salt) = salt else {
        return refuse_admin(ch, m.op, m.request_id, name, KError::IoError, b"no entropy for a salt");
    };
    let outcome = serve_admin(m.op, m.body, &srv.db[..srv.db_len], &salt, &mut srv.new_db, &mut srv.admin_reply);
    match outcome {
        Admin::Reply(n) => send_reply(ch, m.op, m.request_id, &srv.admin_reply[..n], false),
        Admin::Write(n) => {
            if let Err(why) = install(srv.root_ns, &srv.new_db[..n]) {
                return refuse_admin(ch, m.op, m.request_id, name, KError::IoError, why);
            }
            srv.db[..n].copy_from_slice(&srv.new_db[..n]);
            srv.db_len = n;
            let did: &[u8] = match m.op {
                OP_AUTH_ADD => b"added ",
                OP_AUTH_REMOVE => b"removed ",
                _ => b"set a new password for ",
            };
            Line::new().s(b"auth-service: ").s(did).untrusted(name).end();
            send_reply(ch, m.op, m.request_id, &[], false);
        }
        Admin::Refused(r) => refuse_admin(ch, m.op, m.request_id, name, r.kerror(), r.why()),
    }
}

/// Refuse an administrator's request with `err` and `why`, and say so in the log.
fn refuse_admin(ch: u64, op: u16, request_id: u64, name: &[u8], err: KError, why: &[u8]) {
    let asked: &[u8] = match op {
        OP_AUTH_LIST => b"List",
        OP_AUTH_ADD => b"Add",
        OP_AUTH_REMOVE => b"Remove",
        OP_AUTH_SET_PASSWORD => b"SetPassword",
        _ => b"a request that is not administrative",
    };
    let mut l = Line::new();
    l.s(b"auth-service: refused ").s(asked);
    if !name.is_empty() {
        l.s(b" for ").untrusted(name);
    }
    l.s(b": ").s(why).end();
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN + 128];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, &why[..why.len().min(128)]).unwrap_or(0);
    send_reply(ch, op, request_id, &body[..n], true);
}

/// A new password's salt, from the kernel's entropy source; `None` if it is not open or fails.
fn fresh_salt(ent: u64) -> Option<[u8; libusers::SALT_LEN]> {
    if ent == 0 {
        return None;
    }
    let mut salt = [0u8; libusers::SALT_LEN];
    loop {
        // SAFETY: a valid out-buffer of SALT_LEN bytes, and an entropy handle with READ.
        let r = unsafe { syscall3(SYS_ENTROPY_READ, ent, salt.as_mut_ptr() as u64, salt.len() as u64) };
        if r == 0 {
            return Some(salt);
        }
        if r < 0 {
            return None;
        }
        // Not yet seeded: the answer is a PendingOperation to wait on, then read again.
        let (status, _) = po_wait(r as u64);
        if status != 0 {
            return None;
        }
    }
}

fn po_wait(po: u64) -> (i32, u64) {
    let handles = [po];
    let mut r = [0u8; 24];
    // SAFETY: a valid one-entry handle array and result buffer on this frame.
    let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, r.as_mut_ptr() as u64, u64::MAX) };
    // SAFETY: closing the PO this process owns; any resolved handle is separate.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 {
        return (KError::KernelError.as_i32(), 0);
    }
    let status = i32::from_le_bytes([r[8], r[9], r[10], r[11]]);
    let result = u64::from_le_bytes([r[16], r[17], r[18], r[19], r[20], r[21], r[22], r[23]]);
    (status, result)
}

/// **Replace `/system/users` with `bytes`, atomically.** A leftover `users.new`, from a write that
/// died, is cut to nothing; the new copy is created at its size, written through a mapping,
/// unmapped, synced, and then renamed over the file. A crash before the rename leaves the old file
/// as it was, and the rename is the filesystem's one operation. `Err` names the step that failed.
fn install(ns: u64, bytes: &[u8]) -> Result<(), &'static [u8]> {
    let path = |p: &[u8]| (p.as_ptr() as u64, p.len() as u64);
    let (np, nl) = path(USERS_NEW);
    // A leftover, cut to nothing. Absent is the usual case, and not an error.
    // SAFETY: a valid path and namespace handle.
    let po = unsafe { syscall5(SYS_FILE_TRUNCATE, ns, np, nl, RIGHT_MAP_READ | RIGHT_MAP_WRITE, 0) };
    if po > 0 {
        let (_, h) = po_wait(po as u64);
        if h != 0 {
            // SAFETY: closing the handle the truncate's resolve installed.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        }
    }
    // SAFETY: a valid path and namespace handle.
    let po = unsafe { syscall5(SYS_FILE_CREATE, ns, np, nl, RIGHT_MAP_READ | RIGHT_MAP_WRITE, bytes.len() as u64) };
    if po < 0 {
        return Err(b"users.new could not be created");
    }
    let (status, file) = po_wait(po as u64);
    if status != 0 || file == 0 {
        return Err(b"users.new could not be created");
    }
    if !bytes.is_empty() {
        // SAFETY: a file handle this call created with MAP_READ | MAP_WRITE; the file is at most a
        // page (DB_MAX <= PAGE).
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, file, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
        if addr < 0 {
            // SAFETY: closing this call's own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, file) };
            return Err(b"users.new could not be mapped");
        }
        // SAFETY: `bytes.len()` bytes are mapped writable at `addr`, and `bytes` is the server's
        // own buffer, which this mapping cannot alias. Unmapped before the sync, as `libfs` does: a
        // write-back that begins with a writable mapping cannot call the file clean.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len());
            syscall2(SYS_MEMORY_UNMAP, addr as u64, PAGE);
        }
    }
    // SAFETY: a file handle this call holds.
    let synced = unsafe { syscall1(SYS_FILE_SYNC, file) };
    // SAFETY: closing this call's own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, file) };
    if synced != 0 {
        return Err(b"users.new could not be written back");
    }
    let (up, ul) = path(USERS);
    // SAFETY: two valid paths and a namespace handle.
    let po = unsafe { syscall6(SYS_FILE_RENAME, ns, np, nl, up, ul, RENAME_REPLACE as u64) };
    if po < 0 {
        return Err(b"users.new could not be renamed over users");
    }
    let (status, _) = po_wait(po as u64);
    if status != 0 {
        return Err(b"users.new could not be renamed over users");
    }
    Ok(())
}

fn free_session(srv: &mut Server, ch: u64) {
    for s in srv.sessions.iter_mut().filter(|s| matches!(s, Some((c, _)) if *c == ch)) {
        *s = None;
    }
    // SAFETY: closing a handle this process owns.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ch) };
}

/// Reply to a resolve with the minted session channel in `handles[0]`: an auth handle *is* a live
/// channel to this server, as a directory handle is to `profile-server`.
fn reply_session_handle(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    send(serve_end, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, client_end)
}

/// Reply to a resolve with an error: **the whole twelve-byte `ErrorBody`**, as every server sends
/// one. The kernel reads a shorter body on a forwarded resolve as malformed, so a caller refused
/// `NotFound` or `WouldBlock` would be handed `KernelError` instead.
fn reply_resolve_error(serve_end: u64, request_id: u64, op: u16, code: i32) {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN];
    let _ = librsproto::error::error_body(&mut body, code, 0, b"");
    send(serve_end, op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &body, 0);
}

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the inherited
/// root namespace (resolves `/system/users`), `rdx` = the control-channel endpoint the
/// supervisor installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"auth-service: up\n");
    // SAFETY: a register-only syscall: a handle or a negative error.
    let ent = unsafe { syscall0(SYS_ENTROPY_CREATE) };
    let mut srv = Server {
        db: [0; DB_MAX],
        db_len: 0,
        new_db: [0; DB_MAX],
        admin_reply: [0; RS_MAX_BODY],
        sessions: [None; MAX_SESSIONS],
        root_ns,
        entropy: if ent > 0 { ent as u64 } else { 0 },
    };
    if !load_db(&mut srv, root_ns) {
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
    serve_loop(&mut srv, serve_end, control);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"auth-service: PANIC\n");
    exit(1);
}
