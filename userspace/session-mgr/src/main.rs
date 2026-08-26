//! `session-mgr` — the userspace **session manager** (Phase 3, auth + session slice).
//!
//! The Tier-5 supervisor that logs a user in and hands them a sandboxed shell. It
//! holds re-delegated `BIND_NAMESPACE` (from service-mgr) and the building-block
//! endpoints — the fs-server forwarding endpoint + a channel to auth-service — from
//! which it composes each login's per-user namespace. See
//! `docs/architecture/session-and-auth.md`.
//!
//! **What it does:** receive the handed-over endpoints, prompt for a credential on a
//! terminal it opens, authenticate it against auth-service over the auth channel, construct
//! a session namespace binding the user's `/home` as a subtree of the fs-server
//! (`BIND_NAMESPACE` + subtree scoping + shared registration), spawn the user shell into it,
//! reap it, tear the session down, and prompt again.
//!
//! **One login path, in every build.** This file has no build-mode `cfg` and the crate
//! declares no features. It used to auto-log-in a hardcoded credential under `test-harness`
//! and run a fixed `-c` script, with the interactive `login()` and the whole `tty_*` layer
//! compiled out — so the gate that adjudicated the boot proved a string comparison worked.
//! Removed 2026-08-21; `cargo xtask test-interactive` types at the real prompt instead. See
//! `docs/planning/test-path-retrofit.md`, and `userspace/session-mgr/CLAUDE.md` § Forbidden.
//!
//! `#![no_std]` + `#![no_main]`, **with `alloc`**. The no-`alloc` rule was lifted on
//! 2026-07-31 because session-mgr hands each session its environment, and every step of
//! that needs a heap: a TSM1 `Record` holds `Vec`s, `send_setup` builds a `Vec<String>`,
//! and encoding returns a `Vec<u8>`. Without it the *parent* cannot give the child its
//! environment — which is what Milestone 3.5 is built on. `std` stays unported and there
//! is no runtime to hand a `main`, so those two rules are unchanged.
//! See `userspace/session-mgr/CLAUDE.md`.

#![no_std]
#![no_main]

extern crate alloc;

use libkern::debug::Line;
use libkern::*;

/// `alloc` backing: the environment record and the setup-message codec both allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;
use librsproto::{decode, encode};
use libsession::{NamespaceSpec, authenticate, ns_lookup, session_has_bin, session_has_tty, spawn_leader};

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut SEND_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_HANDLES: [u64; 8] = [0; 8];




/// Emit `msg` to the serial console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Park forever. Reached only when this supervisor cannot usefully continue — today, when
/// its endpoint handoff did not arrive, so no session can be built.
///
/// **Parks, never spins.** This was a `pause` loop, which burns a CPU for as long as the
/// machine is up. The cost is not the cycles: a run queue that is never empty means the
/// idle thread never runs, and deferred handle reclamation lives there — so a spinning
/// supervisor stops *every* exited process on the system from being reclaimed, and their
/// pipes from ever closing. That is exactly the `logging-service` bug of 2026-07-31,
/// which took an afternoon to find from a hung shell three subsystems away.
fn idle(notif: u64) -> ! {
    loop {
        wait_one(notif);
    }
}

/// Park forever without a handle to wait on — the panic path, which has no notification
/// channel in scope. Sleeps in long hops rather than spinning, for the reason [`idle`]
/// gives: a spinning process starves deferred reclamation system-wide.
fn park() -> ! {
    loop {
        sleep_ms(60_000);
    }
}

/// Sleep `ms` milliseconds on a one-shot timer. Best-effort: on any failure it returns
/// immediately (every caller is pacing, not depending on the delay).
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

/// Block on `handle`; returns `true` if it signalled (vs. a spurious wake).
fn wait_one(handle: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = handle;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    waited == 1
}

/// Receive the next control message on `ctrl` and return its transferred `handles[0]`
/// (a handoff carries exactly one moved handle, no payload). `0` on failure.
fn recv_handoff(ctrl: u64) -> u64 {
    if !wait_one(ctrl) {
        return 0;
    }
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ctrl,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    let count = unsafe { (&raw const RECV_COUNT).read() };
    if rr != 0 || count < 1 {
        return 0;
    }
    // SAFETY: the kernel installed the transferred handle at handles[0].
    unsafe { (&raw const RECV_HANDLES[0]).read() }
}

// `authenticate`, `build_namespace`, `session_has_bin`/`session_has_tty` and `ns_lookup`
// moved to `libsession` in M7 Part B — the shared session core both supervisors run. What
// stays here is the *greeter*: prompting on a terminal, which is where the two columns
// genuinely differ (`docs/architecture/graphical-session.md` §4).



/// Bootstrap registers: `rdi` = notification channel (reaps the user shell), `rsi` =
/// the inherited (LOOKUP-only) root namespace, `rdx` = the control channel service-mgr
/// hands the endpoints over, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"session-mgr: up\n");
    // Receive the handed-over endpoints, in order: (1) fs-server endpoint, (2) profile
    // server endpoint, (3) tty server endpoint. Positional — service-mgr sends an empty message
    // for an endpoint it does not have, so a missing one shortens no one's count.
    let fs_endpoint = recv_handoff(control);
    let profile_endpoint = recv_handoff(control);
    let tty_endpoint = recv_handoff(control);
    // **The auth channel is resolved, not couriered** (M7 Part C). **`init`** binds
    // `auth-service` at `/svc/auth` — not `service-mgr`, which spawned it and cannot bind,
    // because a declared service holds an inherited LOOKUP-only root — and every supervisor
    // asks the namespace for a session of
    // its own — which is what lets `desktop-session-mgr` have one too. Before this it arrived
    // here positionally as a fourth handle, and there was only ever one to hand out.
    //
    // Resolved once, at startup rather than per login: the oracle's lifetime is the machine's,
    // and re-resolving on each attempt would mint a session per login.
    // `RIGHT_WAIT` is not optional: `libsession::authenticate` blocks on this channel for
    // the reply, and a handle without it makes `sys_wait` reject the whole set — which
    // presents as a login that prompts, takes the password, and then never answers.
    let (auth_status, auth_ch) =
        ns_lookup(root_ns, b"/svc/auth", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if auth_status != 0 || auth_ch == 0 {
        kprint(b"session-mgr: /svc/auth resolve FAIL; no session can authenticate\n");
    }
    if fs_endpoint == 0 || auth_ch == 0 {
        // No verdict here any more. A supervisor that cannot build sessions is a boot
        // failure, but adjudicating one is not a session supervisor's job — it says what
        // happened and stops. `test-qemu` requires the success line below in the
        // transcript, so this path fails the run without this process knowing a run exists
        // (`docs/planning/test-path-retrofit.md` Part B).
        kprint(b"session-mgr: endpoint handoff FAIL\n");
        idle(notif);
    }
    // A session without `/bin` is the pre-Part-F shell: the language works, nothing
    // spawns. Degraded, not broken — worth reporting and worth not failing the login over.
    if profile_endpoint == 0 {
        kprint(b"session-mgr: no profile endpoint -- sessions will have no /bin\n");
    }
    kprint(b"session-mgr: received fs + profile endpoints; auth resolved from /svc/auth\n");

    // The session loop: authenticate a user, construct their per-user namespace, spawn
    // the shell into it, and reap it — the same way in every build.
    let mut home = [0u8; 256];
    let mut user = [0u8; 64];
    // **The session loop.** It was a single `match` — log in once, run a shell, then park
    // forever — so typing `exit` left a machine with no prompt and no way back short of a
    // reboot. A login that cannot be repeated is not a login.
    loop {
    // One terminal per session: it serves the login prompt, is bound into the session's
    // namespace for the shell, and is closed when the session ends.
    let tty = tty_open(root_ns);

    match login(tty, auth_ch, &mut home, &mut user) {
        Some((hl, ul)) => {
            Line::new().s(b"session-mgr: login ok -> home=").s(&home[..hl]).end();
            let session_ns = libsession::build_namespace(&NamespaceSpec {
                root_ns,
                fs_endpoint,
                profile_endpoint,
                tty_endpoint,
                home: &home[..hl],
                user: &user[..ul],
                // A serial session renders no text, so it takes no fonts.
                bind_fonts: false,
                // The console *is* this column's terminal, so the serial session binds it.
                // The graphical column will pass `false` — governing decision 3.
                bind_console: true,
            });
            if session_ns == 0 {
                kprint(b"session-mgr: session namespace FAIL\n");
                // One bad session: go back to the prompt rather than bricking the console,
                // since a permanent park is a worse answer than letting someone try again.
                continue;
            }
            kprint(b"session-mgr: session namespace built (/home subtree + /dev/console");
            // Named separately because it is the one member that can be absent, and a
            // run with the bind disabled proved the unconditional message would announce
            // a `/bin` that was not there — the log has to be able to say "no".
            if session_has_bin() {
                kprint(b" + /bin");
            }
            if session_has_tty() {
                kprint(b" + /dev/tty");
            }
            kprint(b")\n");
            // The payoff: an unprivileged shell in the per-user namespace writes to home.
            // `nxsh` here; the graphical column will pass `desktop-shell`. Everything
            // else about the spawn -- the empty syscaps, the session namespace, the Tier-1
            // setup channel carrying argv and the environment -- is identical, which is what
            // made it worth sharing.
            let code = spawn_leader(root_ns, session_ns, notif, "nxsh", &[], 0, &[]);

            // Tear the session down. The shell has been reaped, so this drops the last
            // reference to the namespace and with it every binding in it — the `/home`
            // and `/bin` registrations and the `/session/user` snapshot. Without it each
            // logout would leak a namespace, which is only harmless while there is
            // exactly one login per boot.
            // SAFETY: closing the namespace we created for this session.
            unsafe { syscall1(SYS_HANDLE_CLOSE, session_ns) };
            // And the terminal. `Close` is the revocation — a process that outlived the
            // session cannot have its handle taken back, so the server declining to serve
            // is what ends the terminal.
            if tty != 0 {
                tty_close(tty);
            }

            // The shell exited because the user asked it to. Say what happened and nothing
            // more — this used to sit beside a harness branch claiming "verified its
            // environment + wrote to home", a check an interactive session never ran.
            {
                // **One `kprint`, not four.** The console is shared, so a line assembled
                // from several calls can be torn by any other process that logs between
                // them — this one came back as `session ended (shell exit tty-server:
                // terminal closed\n3)`, because the tty server logs the close while the
                // status is being written. A log line that another writer can split down
                // the middle is not a log line; assemble it, then emit it once.
                let mut line = alloc::string::String::from(
                    "session-mgr: session ended (shell exit ",
                );
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
            if tty != 0 {
                tty_close(tty);
            }
            // Re-prompt rather than locking out. A serial console has no second way in,
            // so a lockout bricks the machine; the pause is what keeps repeated failure
            // from being a free brute-force oracle.
            sleep_ms(2000);
        }
    }
    }
}

/// Authenticate a user, returning `(home_len, user_len)` with the home path copied into
/// `home_out` and the authenticated name into `user_out`, or `None` if denied.
///
/// The name comes back because the session namespace publishes it at `/session/user`:
/// we are the component that authenticated, so we are the one that knows who this is.
///
/// Prompts for a username and a password on the terminal, up to a few attempts.
///
/// **There used to be two of these**, and the other one is why
/// `docs/planning/test-path-retrofit.md` exists: under `test-harness` this function was
/// replaced by an auto-login of a hardcoded demo credential, so the gate that adjudicated
/// the whole boot ran a build in which the prompt below, the echo discipline, and the whole
/// `tty_*` layer were compiled out. What it proved about logging in was that a string
/// comparison worked. The real path is now driven from the host by
/// `cargo xtask test-interactive`, against the release image.
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
        // The server stops echoing; this cannot be forgotten the way a `bool` argument
        // could, and the discipline also stops *erasing* on screen, so a backspace in a
        // password reveals nothing about its length.
        tty_set_echo(tty, false);
        let plen = tty_read_line(tty, &mut pass);
        tty_set_echo(tty, true);
        // Nothing was echoed for the password, so the newline the user typed was not shown
        // either: supply it here so what follows starts on its own line.
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

/// Open a terminal: resolve `/dev/tty`, which yields a fresh per-caller channel.
///
/// One tty per session. It serves the login prompt first, is then bound into the session's
/// namespace for the shell, and is closed when the session ends — the terminal belongs to
/// the session, not to session-mgr.
fn tty_open(root_ns: u64) -> u64 {
    let (st, ch) = ns_lookup(root_ns, b"/dev/tty", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_TRANSFER);
    if st != 0 { 0 } else { ch }
}

/// Send one tty request and wait for its reply. Returns the reply body length written into
/// `out` (`0` for the acknowledgement-only ops), or `None` on any failure.
fn tty_request(ch: u64, op: u16, body: &[u8], out: &mut [u8]) -> Option<usize> {
    // SAFETY: SEND_MSG is a valid buffer; the rsproto message goes at offset 24.
    let sent = unsafe {
        let rs_len = encode(&mut SEND_MSG[PAYLOAD_OFF..], op, 1, 0, body, 0)?;
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
    if !sent || !wait_one(ch) {
        return None;
    }
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
        return None;
    }
    // SAFETY: bounded read-only slice over the reply.
    unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let rep = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        let m = decode(rep).ok()?;
        if m.op != op || m.is_error() {
            return None;
        }
        let n = m.body.len().min(out.len());
        out[..n].copy_from_slice(&m.body[..n]);
        Some(n)
    }
}

/// Write `text` to the terminal. Output through a handle the process *holds* — not the
/// ambient debug syscall every program used to print with.
fn tty_write(ch: u64, text: &[u8]) {
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_WRITE, text, &mut scratch);
}

/// Turn echo on or off.
///
/// **This is why the tty server exists.** It was a `bool` every caller of `read_line` had
/// to remember to pass, so reading a password safely depended on each of them getting it
/// right. Now it is the server's state and a client cannot forget it.
fn tty_set_echo(ch: u64, on: bool) {
    let flags = [if on { librsproto::TTY_MODE_ECHO } else { 0 }];
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_SET_MODE, &flags, &mut scratch);
}

/// Read one edited line. The line discipline — backspace, kill, echo — is the server's,
/// so this is a request rather than a byte loop.
fn tty_read_line(ch: u64, out: &mut [u8]) -> usize {
    tty_request(ch, librsproto::OP_TTY_READ_LINE, &[], out).unwrap_or(0)
}

/// Tell the server this terminal is finished, then drop the handle.
///
/// **Revocation, not release.** Handles are refcounted and this kernel has none, so closing
/// cannot take a tty back from a process that outlived the session. The server declining to
/// serve the channel is what makes teardown a guarantee.
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
