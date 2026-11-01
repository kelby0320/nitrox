//! `session-mgr` — the Tier-5 supervisor that logs a user in and hands them a sandboxed shell:
//! authenticate against auth-service, build a per-user namespace, spawn the shell, reap it, tear
//! the session down, prompt again. One login path in every build: no build-mode `cfg`.
//! `no_std` + `no_main` with `alloc` (the setup message and environment record allocate).

#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicU64, Ordering};
use libkern::debug::Line;
use libkern::*;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;
use librsproto::{decode, encode};
use libsession::{NamespaceSpec, authenticate, ns_lookup, spawn_leader};

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

/// This supervisor's notification channel, where a shutdown's terminate request arrives.
static NOTIF_CH: AtomicU64 = AtomicU64::new(0);
/// The request id the last tty request was sent with: each its own, so its reply is told apart.
static TTY_RID: AtomicU64 = AtomicU64::new(0);

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

/// Sleep `ms`, in a wait on no handles, which the kernel never refuses.
fn sleep_ms(ms: u64) {
    let mut now: u64 = 0;
    // SAFETY: `now` is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    // SAFETY: no handles and no results, so the kernel reads and writes neither.
    unsafe { syscall4(SYS_WAIT, 0, 0, 0, now.saturating_add(ms * 1_000_000)) };
}

/// Block on `handle`; `true` if it signalled. The notification channel is watched beside it, so a
/// shutdown's terminate request reaches a supervisor at the prompt, which [`stop`]s.
fn wait_one(handle: u64) -> bool {
    let notif = NOTIF_CH.load(Ordering::Relaxed);
    loop {
        let watch = notif != 0 && handle != notif;
        let handles = [handle, notif];
        let mut results = [IoResult { handle: 0, status: 0, reserved: 0, result: 0 }; 2];
        let n = if watch { 2 } else { 1 };
        // SAFETY: `n` handles, and room for a result each, all on this frame.
        let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, n, results.as_mut_ptr() as u64, u64::MAX) };
        if waited < 1 {
            return false;
        }
        if !watch {
            return true;
        }
        let fired = |h: u64| results[..waited as usize].iter().any(|r| r.handle == h);
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
    match channel_recv(ctrl, &mut [0u8; MSG_LEN]) {
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
    NOTIF_CH.store(notif, Ordering::Relaxed);
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
            Some((hl, ul)) => 'session: {
                Line::new().s(b"session-mgr: login ok -> home=").s(&home[..hl]).end();
                let mut views_base = [0u8; 24];
                let views = if views_sup != 0 {
                    libsession::views_open_session(views_sup, &user[..ul], &mut views_base)
                } else {
                    None
                };
                let built = libsession::build(&NamespaceSpec {
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
                let Some(built) = built else {
                    kprint(b"session-mgr: session namespace FAIL\n");
                    if let Some((id, _)) = views {
                        libsession::views_close_session(views_sup, id);
                    }
                    break 'session;
                };
                let session_ns = built.ns;
                kprint(b"session-mgr: session namespace built (/home subtree + /dev/console");
                if built.bin {
                    kprint(b" + /bin");
                }
                if built.tty {
                    kprint(b" + /dev/tty");
                }
                if built.views {
                    kprint(b" + /dev/views");
                }
                if built.devices {
                    kprint(b" + /dev/devices");
                }
                if built.storage {
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
            }
            None => {
                kprint(b"session-mgr: login denied\n");
                // Re-prompt, never lock out (a serial console has no other way in); the pause
                // denies a free brute-force oracle.
                sleep_ms(2000);
            }
        }
        // **On every path**: each terminal kept would hold one of tty-server's 14 for good.
        if tty != 0 {
            tty_close(tty);
        }
        if libsession::stop_asked() {
            stop();
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
        let mut pass = Secret::<128>::zeroed();
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
/// `out` (`0` for the acknowledgement-only ops), or `None` on any failure, a reply past `out`
/// included: a line is never cut short, so a password past it is no prefix of one.
fn tty_request(ch: u64, op: u16, body: &[u8], out: &mut [u8]) -> Option<usize> {
    let rid = TTY_RID.fetch_add(1, Ordering::Relaxed) + 1;
    let mut msg = [0u8; MSG_LEN];
    let rs_len = encode(&mut msg[PAYLOAD_OFF..], op, rid, 0, body, 0)?;
    msg[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
    // SAFETY: a whole message on this frame, and no handles.
    if unsafe { syscall5(SYS_CHANNEL_SEND, ch, msg.as_ptr() as u64, 0, 0, SENDMODE_NOBLOCK) } != 0 {
        return None;
    }
    // Scrubbed on every return: a line read under the password prompt is the password.
    let mut reply = Secret::<MSG_LEN>::zeroed();
    loop {
        if !wait_one(ch) {
            return None;
        }
        let _sent = channel_recv(ch, reply.array_mut()).ok()?;
        let payload_len = u32::from_le_bytes([reply[4], reply[5], reply[6], reply[7]]) as usize;
        let m = decode(reply.get(PAYLOAD_OFF..PAYLOAD_OFF + payload_len)?).ok()?;
        // **Not this request's**: `tty-server`'s unsolicited `INTERRUPT` after a Ctrl-C, request id
        // 0. Taken as the reply, it left every later request reading the one before's.
        if m.request_id != rid {
            continue;
        }
        if m.op != op || m.is_error() {
            return None;
        }
        out.get_mut(..m.body.len())?.copy_from_slice(m.body);
        return Some(m.body.len());
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
