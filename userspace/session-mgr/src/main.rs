//! `session-mgr` — the Tier-5 supervisor that logs a user in and hands them a sandboxed shell:
//! authenticate against auth-service, build a per-user namespace, spawn the shell, reap it, tear
//! the session down, prompt again. One login path in every build: no build-mode `cfg`.
//! `no_std` + `no_main` with `alloc` (the setup message and environment record allocate).

#![no_std]
#![no_main]

extern crate alloc;

use libkern::debug::Line;
use libkern::*;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;
use librsproto::{decode, encode};
use libsession::{NamespaceSpec, authenticate, ns_lookup, session_has_bin, session_has_tty, spawn_leader};

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

/// Room for what a wait waits for and, beside it, the notification channel ([`wait_one`]).
static mut WAIT_HANDLES: [u64; 2] = [0; 2];
static mut WAIT_RESULTS: [u8; 48] = [0; 48];
/// This supervisor's notification channel, where a shutdown's terminate request arrives.
static mut NOTIF_CH: u64 = 0;
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_HANDLES: [u64; 8] = [0; 8];
/// The request id the last tty request was sent with: each its own, so its reply is told apart.
static mut TTY_RID: u64 = 0;

fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Park until a shutdown asks for a stop. Parks, never spins: a spinning supervisor starves the
/// idle thread, where deferred handle reclamation runs.
fn idle(notif: u64) -> ! {
    loop {
        wait_one(notif);
        if libsession::drain_for_stop(notif) {
            stop();
        }
    }
}

/// A shutdown asked this supervisor to stop with no session running: exit.
/// A running session is ended first, by `libsession::spawn_leader`.
fn stop() -> ! {
    kprint(b"session-mgr: asked to stop, exiting\n");
    // SAFETY: terminating this process; the terminal it held closes with its handles.
    unsafe { syscall4(SYS_PROCESS_EXIT, 0, 0, 0, 0) };
    park();
}

/// Park on the panic path, which has no handle to wait on: long sleeps, never a spin.
fn park() -> ! {
    loop {
        sleep_ms(60_000);
    }
}

/// Sleep `ms` on a one-shot timer; best-effort, as every caller only paces.
fn sleep_ms(ms: u64) {
    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let th = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th < 0 {
        return;
    }
    let th = th as u64;
    let mut now: u64 = 0;
    // SAFETY: `now` is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    let fire_at = now + ms * 1_000_000;
    // SAFETY: arming our own timer handle (absolute monotonic, one-shot).
    unsafe { syscall4(SYS_TIMER_SET, th, fire_at, 0, 0) };
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    unsafe {
        WAIT_HANDLES[0] = th;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            fire_at + 1_000_000_000,
        );
        syscall1(SYS_HANDLE_CLOSE, th);
    }
}

/// Block on `handle`; `true` if it signalled. The notification channel is watched beside it, so a
/// shutdown's terminate request reaches a supervisor at the prompt, which [`stop`]s.
fn wait_one(handle: u64) -> bool {
    // SAFETY: single-threaded; set once at startup.
    let notif = unsafe { NOTIF_CH };
    loop {
        let watch = notif != 0 && handle != notif;
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers for two waiters.
        let waited = unsafe {
            WAIT_HANDLES[0] = handle;
            WAIT_HANDLES[1] = notif;
            let n = if watch { 2 } else { 1 };
            let (handles, results) = ((&raw const WAIT_HANDLES) as u64, (&raw mut WAIT_RESULTS) as u64);
            syscall4(SYS_WAIT, handles, n, results, u64::MAX)
        };
        if waited < 1 {
            return false;
        }
        if !watch {
            return true;
        }
        // SAFETY: `waited` 24-byte results were written; the handle is the first word of each.
        let fired =
            |h: u64| unsafe { (0..waited as usize).any(|k| WAIT_RESULTS[24 * k..24 * k + 8] == h.to_le_bytes()) };
        if fired(notif) && libsession::drain_for_stop(notif) {
            stop();
        }
        if fired(handle) {
            return true;
        }
    }
}

/// Receive the next control message and return its one moved handle, or `0` on failure.
fn recv_handoff(ctrl: u64) -> u64 {
    if !wait_one(ctrl) {
        return 0;
    }
    // The one handle a handoff brings; any more close as `sent` drops.
    // SAFETY: single-threaded; `RECV_MSG` is lent for the call.
    match unsafe { channel_recv_raw(ctrl, &raw mut RECV_MSG) } {
        Ok(mut sent) => sent.take(0).unwrap_or(0),
        Err(_) => 0,
    }
}

/// Bootstrap registers: `rdi` = notification channel (reaps the user shell), `rsi` =
/// the inherited (LOOKUP-only) root namespace, `rdx` = the control channel service-mgr
/// hands the endpoints over, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"session-mgr: up\n");
    // SAFETY: single-threaded; written once, here, before any wait reads it.
    unsafe { NOTIF_CH = notif };
    // Positional: service-mgr sends an empty message (`0`) for an endpoint that did not come up,
    // so none shifts. All but the first two are `service-mgr`'s routes, which outlive a restart.
    let fs_endpoint = recv_handoff(control);
    let profile_endpoint = recv_handoff(control);
    let tty_endpoint = recv_handoff(control);
    let clipboard_endpoint = recv_handoff(control);
    let views_endpoint = recv_handoff(control);
    let devices_endpoint = recv_handoff(control);
    let storage_endpoint = recv_handoff(control);
    let services_endpoint = recv_handoff(control);
    // Resolved once: the oracle lives as long as the machine. `RIGHT_WAIT` is required, or
    // `sys_wait` rejects the set and a login prompts, takes the password, and never answers.
    let (auth_status, auth_ch) = ns_lookup(root_ns, b"/svc/auth", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if auth_status != 0 || auth_ch == 0 {
        kprint(b"session-mgr: /svc/auth resolve FAIL; no session can authenticate\n");
    }
    if fs_endpoint == 0 || auth_ch == 0 {
        // No verdict: say what happened and stop; `test-qemu` requires the success line below.
        kprint(b"session-mgr: endpoint handoff FAIL\n");
        idle(notif);
    }
    if profile_endpoint == 0 {
        kprint(b"session-mgr: no profile endpoint -- sessions will have no /bin\n");
    }
    kprint(b"session-mgr: received fs + profile endpoints; auth resolved from /svc/auth\n");
    // Resolved once. A broker session opened per login and closed at logout is the broker's only
    // way to know it ended.
    let views_sup = if views_endpoint != 0 {
        ns_lookup(root_ns, b"/svc/views/session", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT).1
    } else {
        0
    };
    if views_endpoint != 0 && views_sup == 0 {
        kprint(b"session-mgr: /svc/views/session resolve FAIL; sessions will have no `with`\n");
    }

    let mut home = [0u8; 256];
    let mut user = [0u8; 64];
    loop {
        let tty = tty_open(root_ns);

        match login(tty, auth_ch, &mut home, &mut user) {
            Some((hl, ul)) => {
                Line::new().s(b"session-mgr: login ok -> home=").s(&home[..hl]).end();
                let mut views_base = [0u8; 24];
                let views = if views_sup != 0 {
                    libsession::views_open_session(views_sup, &user[..ul], &mut views_base)
                } else {
                    None
                };
                let session_ns = libsession::build_namespace(&NamespaceSpec {
                    root_ns,
                    fs_endpoint,
                    profile_endpoint,
                    tty_endpoint,
                    clipboard_endpoint,
                    home: &home[..hl],
                    user: &user[..ul],
                    // A serial session renders no text, so it takes no fonts.
                    bind_fonts: false,
                    // The console is this column's terminal, so the serial session binds it.
                    bind_console: true,
                    views_endpoint: if views.is_some() { views_endpoint } else { 0 },
                    views_base: &views_base[..views.map_or(0, |v| v.1)],
                    devices_endpoint,
                    storage_endpoint,
                    services_endpoint,
                    bind_applications: true,
                    draw_endpoint: 0,
                    desktop_endpoint: 0,
                });
                if session_ns == 0 {
                    kprint(b"session-mgr: session namespace FAIL\n");
                    if let Some((id, _)) = views {
                        libsession::views_close_session(views_sup, id);
                    }
                    continue;
                }
                kprint(b"session-mgr: session namespace built (/home subtree + /dev/console");
                if session_has_bin() {
                    kprint(b" + /bin");
                }
                if session_has_tty() {
                    kprint(b" + /dev/tty");
                }
                if libsession::session_has_views() {
                    kprint(b" + /dev/views");
                }
                if libsession::session_has_devices() {
                    kprint(b" + /dev/devices");
                }
                if libsession::session_has_storage() {
                    kprint(b" + /storage");
                }
                kprint(b")\n");
                let code = spawn_leader(root_ns, session_ns, notif, "nxsh", &[], 0, &[]);
                // Tell the broker the session ended before the namespace goes.
                if let Some((id, _)) = views {
                    libsession::views_close_session(views_sup, id);
                }

                // The shell is reaped, so this drops the last reference and every binding with it.
                // SAFETY: closing the namespace we created for this session.
                unsafe { syscall1(SYS_HANDLE_CLOSE, session_ns) };
                if tty != 0 {
                    tty_close(tty);
                }

                {
                    // One `kprint`: another process logging between calls would tear the line.
                    let mut line = alloc::string::String::from("session-mgr: session ended (shell exit ");
                    let mut buf = [0u8; 20];
                    if code < 0 {
                        line.push('-');
                    }
                    let digits = libkern::debug::fmt_u64(code.unsigned_abs() as u64, &mut buf);
                    line.push_str(core::str::from_utf8(digits).unwrap_or("?"));
                    line.push_str(")\n");
                    kprint(line.as_bytes());
                }
                if libsession::stop_asked() {
                    stop();
                }
            }
            None => {
                kprint(b"session-mgr: login denied\n");
                if tty != 0 {
                    tty_close(tty);
                }
                // Re-prompt, never lock out (a serial console has no other way in); the pause
                // denies a free brute-force oracle.
                sleep_ms(2000);
            }
        }
    }
}

/// Authenticate a user in up to three attempts: `(home_len, user_len)` with the home path in
/// `home_out` and the name (published at `/session/user`) in `user_out`, or `None` if denied.
fn login(tty: u64, auth_ch: u64, home_out: &mut [u8], user_out: &mut [u8]) -> Option<(usize, usize)> {
    if tty == 0 {
        kprint(b"session-mgr: no terminal for login\n");
        return None;
    }
    for _ in 0..3 {
        tty_write(tty, b"\r\nnitrox login: ");
        let mut user = [0u8; 64];
        tty_set_echo(tty, true);
        let ulen = tty_read_line(tty, &mut user);

        tty_write(tty, b"password: ");
        let mut pass = [0u8; 128];
        // The server stops echoing and erasing, so a backspace reveals nothing of the length.
        tty_set_echo(tty, false);
        let plen = tty_read_line(tty, &mut pass);
        tty_set_echo(tty, true);
        // The password's newline was not echoed either: supply it.
        tty_write(tty, b"\r\n");

        if let Some(hl) = authenticate(auth_ch, &user[..ulen], &pass[..plen], home_out) {
            let ul = ulen.min(user_out.len());
            user_out[..ul].copy_from_slice(&user[..ul]);
            return Some((hl, ul));
        }
        tty_write(tty, b"login incorrect\r\n");
    }
    None
}

/// Open a terminal: `/dev/tty` yields a fresh per-caller channel, which belongs to the session.
fn tty_open(root_ns: u64) -> u64 {
    let (st, ch) = ns_lookup(root_ns, b"/dev/tty", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_TRANSFER);
    if st != 0 { 0 } else { ch }
}

/// Send one tty request and wait for its reply. Returns the reply body length written into
/// `out` (`0` for the acknowledgement-only ops), or `None` on any failure.
fn tty_request(ch: u64, op: u16, body: &[u8], out: &mut [u8]) -> Option<usize> {
    // SAFETY: single-threaded; each request takes the next id.
    let rid = unsafe {
        TTY_RID += 1;
        TTY_RID
    };
    // SAFETY: SEND_MSG is a valid buffer; the rsproto message goes at offset 24.
    let sent = unsafe {
        let rs_len = encode(&mut SEND_MSG[PAYLOAD_OFF..], op, rid, 0, body, 0)?;
        SEND_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        SEND_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    };
    if !sent {
        return None;
    }
    loop {
        if !wait_one(ch) {
            return None;
        }
        // SAFETY: single-threaded; `RECV_MSG` is lent for the call.
        let _sent = unsafe { channel_recv_raw(ch, &raw mut RECV_MSG) }.ok()?;
        // SAFETY: bounded read-only slice over the reply.
        let rep = unsafe {
            let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            )
        };
        let m = decode(rep).ok()?;
        // **Not this request's**: `tty-server`'s unsolicited `INTERRUPT` after a Ctrl-C, request id
        // 0. Taken as the reply, it left every later request reading the one before's.
        if m.request_id != rid {
            continue;
        }
        if m.op != op || m.is_error() {
            return None;
        }
        let n = m.body.len().min(out.len());
        out[..n].copy_from_slice(&m.body[..n]);
        return Some(n);
    }
}

fn tty_write(ch: u64, text: &[u8]) {
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_WRITE, text, &mut scratch);
}

/// Turn echo on or off; server-side state, so a caller cannot forget it for a password.
fn tty_set_echo(ch: u64, on: bool) {
    let flags = [if on { librsproto::TTY_MODE_ECHO } else { 0 }];
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_SET_MODE, &flags, &mut scratch);
}

fn tty_read_line(ch: u64, out: &mut [u8]) -> usize {
    tty_request(ch, librsproto::OP_TTY_READ_LINE, &[], out).unwrap_or(0)
}

/// Tell the server this terminal is finished, then drop the handle. Revocation, not release: a
/// surviving process keeps its handle, so only the server declining to serve ends it.
fn tty_close(ch: u64) {
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_CLOSE, &[], &mut scratch);
    // SAFETY: closing our own channel handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ch) };
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"session-mgr: PANIC\n");
    park();
}
