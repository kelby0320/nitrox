//! `session-mgr` — the Tier-5 supervisor that logs a user in and hands them a sandboxed shell:
//! authenticate against auth-service, build a per-user namespace, spawn the shell, reap it, tear
//! the session down, prompt again. One login path in every build: no build-mode `cfg`.
//! `no_std` + `no_main` with `alloc` (the setup message and environment record allocate).

#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicU64, Ordering};
use libkern::{Rights, Secret, SysCaps, scrub};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Carried, Channel, Deadline, Handle, Msg, Namespace, Notify, NsReadOnly, Only, Ref, WaitSet, time};
use librsproto::{decode, encode};
use libsession::{Constructed, Members, authenticate_on, build_namespace, resolve, run_leader};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

type Endpoint = Handle<Channel, Only>;

/// The request id the last tty request was sent with: each its own, so its reply is told apart.
static TTY_RID: AtomicU64 = AtomicU64::new(0);

/// Park until a shutdown asks for a stop. Parks, never spins: a spinning supervisor starves the
/// idle thread, where deferred handle reclamation runs.
fn idle(notif: Ref<'_, Notify, Only>) -> ! {
    loop {
        let mut set = WaitSet::<1>::new();
        if set.add(notif).is_ok() {
            let _ = set.wait(Deadline::NEVER);
        }
        if libsession::drain_stop_requests(notif) {
            stop();
        }
    }
}

/// A shutdown asked this supervisor to stop with no session running: exit.
/// A running session is ended first, by `libsession::run_leader`.
fn stop() -> ! {
    log::write(b"session-mgr: asked to stop, exiting\n");
    libos::exit(0)
}

/// Park on the panic path, which has no handle to wait on: a wait on nothing, never a spin.
fn park() -> ! {
    loop {
        time::sleep_until(Deadline::NEVER);
    }
}

/// Block on `handle`; `true` if it signalled. The notification channel is watched beside it, so a
/// shutdown's terminate request reaches a supervisor at the prompt, which [`stop`]s.
fn wait_one(handle: &Endpoint, notif: Ref<'_, Notify, Only>) -> bool {
    loop {
        let mut set = WaitSet::<2>::new();
        let (Ok(h), Ok(n)) = (set.add(handle), set.add(notif)) else {
            return false;
        };
        let Ok(fired) = set.wait(Deadline::NEVER) else {
            return false;
        };
        if fired.is_empty() {
            return false;
        }
        if fired.contains(n) && libsession::drain_stop_requests(notif) {
            stop();
        }
        if fired.contains(h) {
            return true;
        }
    }
}

/// Receive the next control message and return its one moved handle, or `None` on failure.
fn recv_handoff(ctrl: Option<&Endpoint>, notif: Ref<'_, Notify, Only>) -> Option<Endpoint> {
    let ctrl = ctrl?;
    if !wait_one(ctrl, notif) {
        return None;
    }
    // The one handle a handoff brings; any more close as `sent` drops.
    match ctrl.recv(&mut Msg::new()) {
        Ok(mut sent) => sent.take(0),
        Err(_) => None,
    }
}

/// A borrow of an optional endpoint, `None` where there is none to bind.
fn borrowed(e: &Option<Endpoint>) -> Option<Ref<'_, Channel, Only>> {
    e.as_ref().map(Handle::as_ref)
}

/// Tell the view broker the session `views` ended.
fn close_views(sup: &Option<Endpoint>, views: Option<(u64, usize)>) {
    if let (Some(sup), Some((id, _))) = (sup, views) {
        libsession::close_views_session(sup, id);
    }
}

/// Bootstrap registers: `rdi` = notification channel (reaps the user shell), `rsi` =
/// the inherited (LOOKUP-only) root namespace, `rdx` = the control channel service-mgr
/// hands the endpoints over, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"session-mgr: up\n");
    let Some(notif_handle) = start.notify.take() else {
        log::write(b"session-mgr: no notification queue\n");
        park();
    };
    let notif = notif_handle.as_ref();
    let root: Option<Handle<Namespace, NsReadOnly>> = start.root.take();
    let ctrl: Option<Endpoint> = start.take_handle0();
    // Positional: service-mgr sends an empty message for an endpoint that did not come up, so
    // none shifts. All but the first two are `service-mgr`'s routes, which outlive a restart.
    let fs_endpoint = recv_handoff(ctrl.as_ref(), notif);
    let profile_endpoint = recv_handoff(ctrl.as_ref(), notif);
    let tty_endpoint = recv_handoff(ctrl.as_ref(), notif);
    let clipboard_endpoint = recv_handoff(ctrl.as_ref(), notif);
    let views_endpoint = recv_handoff(ctrl.as_ref(), notif);
    let devices_endpoint = recv_handoff(ctrl.as_ref(), notif);
    let storage_endpoint = recv_handoff(ctrl.as_ref(), notif);
    let services_endpoint = recv_handoff(ctrl.as_ref(), notif);
    // Resolved once: the oracle lives as long as the machine. `WAIT` is required, or
    // `sys_wait` rejects the set and a login prompts, takes the password, and never answers.
    let rights = Rights::SEND | Rights::RECV | Rights::WAIT;
    let auth = root.as_ref().and_then(|r| resolve::<Channel, Only, _>(r, "/svc/auth", rights).ok());
    if auth.is_none() {
        log::write(b"session-mgr: /svc/auth resolve FAIL; no session can authenticate\n");
    }
    let (Some(root), Some(auth), true) = (root, auth, fs_endpoint.is_some()) else {
        // No verdict: say what happened and stop; `test-qemu` requires the success line below.
        log::write(b"session-mgr: endpoint handoff FAIL\n");
        idle(notif);
    };
    if profile_endpoint.is_none() {
        log::write(b"session-mgr: no profile endpoint -- sessions will have no /bin\n");
    }
    log::write(b"session-mgr: received fs + profile endpoints; auth resolved from /svc/auth\n");
    // Resolved once. A broker session opened per login and closed at logout is the broker's only
    // way to know it ended.
    let views_sup =
        views_endpoint.as_ref().and_then(|_| resolve::<Channel, Only, _>(&root, "/svc/views/session", rights).ok());
    if views_endpoint.is_some() && views_sup.is_none() {
        log::write(b"session-mgr: /svc/views/session resolve FAIL; sessions will have no `with`\n");
    }

    let mut home = [0u8; 256];
    let mut user = [0u8; 64];
    loop {
        let tty = tty_open(&root, notif);

        match login(tty.as_ref(), &auth, &mut home, &mut user) {
            Some((hl, ul)) => 'session: {
                Line::new().s(b"session-mgr: login ok -> home=").s(&home[..hl]).end();
                let mut views_base = [0u8; 24];
                let views = views_sup
                    .as_ref()
                    .and_then(|sup| libsession::open_views_session(sup, &user[..ul], &mut views_base));
                let built = core::str::from_utf8(&home[..hl]).ok().and_then(|home| {
                    build_namespace(&Members {
                        root_ns: Some(root.as_ref()),
                        fs_endpoint: borrowed(&fs_endpoint),
                        profile_endpoint: borrowed(&profile_endpoint),
                        tty_endpoint: borrowed(&tty_endpoint),
                        clipboard_endpoint: borrowed(&clipboard_endpoint),
                        home,
                        user: &user[..ul],
                        // A serial session renders no text, so it takes no fonts.
                        bind_fonts: false,
                        // The console is this column's terminal, so the serial session binds it.
                        bind_console: true,
                        views_endpoint: if views.is_some() { borrowed(&views_endpoint) } else { None },
                        views_base: core::str::from_utf8(&views_base[..views.map_or(0, |v| v.1)]).unwrap_or(""),
                        devices_endpoint: borrowed(&devices_endpoint),
                        storage_endpoint: borrowed(&storage_endpoint),
                        services_endpoint: borrowed(&services_endpoint),
                        bind_applications: true,
                        draw_endpoint: None,
                        desktop_endpoint: None,
                    })
                });
                let Some(Constructed { ns: session_ns, bound }) = built else {
                    log::write(b"session-mgr: session namespace FAIL\n");
                    close_views(&views_sup, views);
                    break 'session;
                };
                log::write(b"session-mgr: session namespace built (/home subtree + /dev/console");
                if bound.bin {
                    log::write(b" + /bin");
                }
                if bound.tty {
                    log::write(b" + /dev/tty");
                }
                if bound.views {
                    log::write(b" + /dev/views");
                }
                if bound.devices {
                    log::write(b" + /dev/devices");
                }
                if bound.storage {
                    log::write(b" + /storage");
                }
                log::write(b")\n");
                let code = run_leader(&root, &session_ns, notif, "nxsh", &[], SysCaps::empty(), &[]);
                // Tell the broker the session ended before the namespace goes.
                close_views(&views_sup, views);

                // The shell is reaped, so this drops the last reference and every binding with it.
                drop(session_ns);
                // One write: another process logging between calls would tear the line.
                Line::new().s(b"session-mgr: session ended (shell exit ").i(i64::from(code)).s(b")").end();
            }
            None => {
                log::write(b"session-mgr: login denied\n");
                // Re-prompt, never lock out (a serial console has no other way in); the pause
                // denies a free brute-force oracle.
                let _ = time::sleep(2_000_000_000);
            }
        }
        // **On every path**: each terminal kept would hold one of tty-server's 14 for good.
        if let Some(tty) = tty {
            tty.close();
        }
        if libsession::stop_asked() {
            stop();
        }
    }
}

/// Authenticate a user in up to three attempts: `(home_len, user_len)` with the home path in
/// `home_out` and the name (published at `/session/user`) in `user_out`, or `None` if denied.
fn login(tty: Option<&Tty<'_>>, auth: &Endpoint, home_out: &mut [u8], user_out: &mut [u8]) -> Option<(usize, usize)> {
    let Some(tty) = tty else {
        log::write(b"session-mgr: no terminal for login\n");
        return None;
    };
    for _ in 0..3 {
        tty.write(b"\r\nnitrox login: ");
        let mut user = [0u8; 64];
        tty.set_echo(true);
        let ulen = tty.read_line(&mut user);

        tty.write(b"password: ");
        let mut pass = Secret::<128>::zeroed();
        // The server stops echoing and erasing, so a backspace reveals nothing of the length.
        tty.set_echo(false);
        let plen = tty.read_line(&mut pass);
        tty.set_echo(true);
        // The password's newline was not echoed either: supply it.
        tty.write(b"\r\n");

        if let Some(hl) = authenticate_on(auth, &user[..ulen], &pass[..plen], home_out) {
            let ul = ulen.min(user_out.len());
            user_out[..ul].copy_from_slice(&user[..ul]);
            return Some((hl, ul));
        }
        tty.write(b"login incorrect\r\n");
    }
    None
}

/// A reply scrubbed where it lies when it goes out of scope, on every way out: a line read under
/// the password prompt is the password, and a move of the message would leave a copy.
struct Scrubbed(Msg);

impl Drop for Scrubbed {
    fn drop(&mut self) {
        scrub(self.0.payload_mut());
    }
}

/// A terminal: a per-caller channel `/dev/tty` yielded, which belongs to the session.
struct Tty<'n> {
    ch: Endpoint,
    notif: Ref<'n, Notify, Only>,
}

/// Open a terminal, `None` if `/dev/tty` did not resolve.
fn tty_open<'n>(root: &Handle<Namespace, NsReadOnly>, notif: Ref<'n, Notify, Only>) -> Option<Tty<'n>> {
    let rights = Rights::SEND | Rights::RECV | Rights::WAIT | Rights::TRANSFER;
    let ch = resolve::<Channel, Only, _>(root, "/dev/tty", rights).ok()?;
    Some(Tty { ch, notif })
}

impl Tty<'_> {
    /// Send one tty request and wait for its reply. Returns the reply body length written into
    /// `out` (`0` for the acknowledgement-only ops), or `None` on any failure, a reply past `out`
    /// included: a line is never cut short, so a password past it is no prefix of one.
    fn request(&self, op: u16, body: &[u8], out: &mut [u8]) -> Option<usize> {
        let rid = TTY_RID.fetch_add(1, Ordering::Relaxed) + 1;
        let mut msg = Msg::new();
        let n = encode(msg.payload_mut(), op, rid, 0, body, 0)?;
        msg.set_len(n).ok()?;
        self.ch.send(&msg, &mut Carried::new()).ok()?;
        let mut reply = Scrubbed(Msg::new());
        loop {
            if !wait_one(&self.ch, self.notif) {
                return None;
            }
            let _sent = self.ch.recv(&mut reply.0).ok()?;
            let m = decode(reply.0.payload()).ok()?;
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

    fn write(&self, text: &[u8]) {
        let _ = self.request(librsproto::OP_TTY_WRITE, text, &mut [0u8; 1]);
    }

    /// Turn echo on or off; server-side state, so a caller cannot forget it for a password.
    fn set_echo(&self, on: bool) {
        let flags = [if on { librsproto::TTY_MODE_ECHO } else { 0 }];
        let _ = self.request(librsproto::OP_TTY_SET_MODE, &flags, &mut [0u8; 1]);
    }

    fn read_line(&self, out: &mut [u8]) -> usize {
        self.request(librsproto::OP_TTY_READ_LINE, &[], out).unwrap_or(0)
    }

    /// Tell the server this terminal is finished, then drop the handle. Revocation, not release: a
    /// surviving process keeps its handle, so only the server declining to serve ends it.
    fn close(self) {
        let _ = self.request(librsproto::OP_TTY_CLOSE, &[], &mut [0u8; 1]);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"session-mgr: PANIC\n");
    park();
}
