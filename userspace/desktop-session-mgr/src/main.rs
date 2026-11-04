//! `desktop-session-mgr` — the graphical login supervisor, `session-mgr`'s twin.
//!
//! The same sequence through the same `libsession`, except that its greeter is a window. It
//! draws first and outlives every session it starts. No `/dev/console` in a graphical session.

#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::Relaxed;
use desktop_session_mgr::{GREETER_H, GREETER_W, Greeter, KEY_ENTER, greeter_theme};
use libdraw::geom::Size;
use libdraw::text::Font;
use libkern::{Rights, Secret, SysCaps};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Channel, Deadline, Handle, Msg, Namespace, Notify, NsReadOnly, Only, Ref, WaitSet};
use librsproto::surface::{CreateWindowRequest, Role};
use libsession::{Members, authenticate_on, build_namespace, resolve, run_leader};
use libsurface::Session;
use libsurface::buffers::BufferPool;
use libsurface::ipc::ChannelTransport;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const BUFFERS: usize = 2;

const MAX_REDRAWS_LOGGED: u32 = 512;

static REDRAWS: AtomicU32 = AtomicU32::new(0);

type Endpoint = Handle<Channel, Only>;

/// The endpoints `service-mgr` hands over, each `None` where it had none.
struct Endpoints {
    fs: Option<Endpoint>,
    profile: Option<Endpoint>,
    tty: Option<Endpoint>,
    draw: Option<Endpoint>,
    clipboard: Option<Endpoint>,
    views: Option<Endpoint>,
    devices: Option<Endpoint>,
    storage: Option<Endpoint>,
    services: Option<Endpoint>,
}

/// An endpoint borrowed for a call, `None` for one that is absent.
fn borrowed(h: &Option<Endpoint>) -> Option<Ref<'_, Channel, Only>> {
    h.as_ref().map(Handle::as_ref)
}

/// A shutdown asked this supervisor to stop with no session running: exit, which `service-mgr`
/// sees as its control channel closing. A running session is ended first by `run_leader`.
fn stop() -> ! {
    log::write(b"desktop-session-mgr: asked to stop, exiting\n");
    libos::exit(0);
}

fn fail(msg: &[u8]) -> ! {
    log::write(msg);
    libos::exit(1);
}

/// Receive the next control message's one moved handle; `None` on failure. Positional: `service-mgr`
/// sends an empty message for an endpoint it does not have.
fn recv_handoff(ctrl: &Endpoint) -> Option<Endpoint> {
    let mut set = WaitSet::<1>::new();
    set.add(ctrl).ok()?;
    if set.wait(Deadline::NEVER).ok()?.len() != 1 {
        return None;
    }
    // The one handle a handoff brings; any more close as `sent` drops. A whole message's buffer:
    // the kernel copies all of one, however short its payload.
    let mut msg = Msg::new();
    ctrl.recv(&mut msg).ok()?.take(0)
}

/// One session of `user`'s, authenticated with `home`, run to its end.
fn run_session(
    root: &Handle<Namespace, NsReadOnly>,
    notif: &Handle<Notify, Only>,
    eps: &Endpoints,
    views_sup: Option<&Endpoint>,
    user: &[u8],
    home: &[u8],
) {
    Line::new().s(b"desktop-session-mgr: login ok -> home=").s(home).end();
    // Before the namespace: the broker's base is part of what it binds.
    let mut views_base = [0u8; 24];
    let opened = views_sup.and_then(|sup| libsession::open_views_session(sup, user, &mut views_base));
    let base_len = opened.map_or(0, |v| v.1);
    let views_base = core::str::from_utf8(&views_base[..base_len]).unwrap_or("");
    let views = if opened.is_some() { borrowed(&eps.views) } else { None };

    // No `/dev/console`: a binding is authority, and the console is the serial column's recovery path.
    let built = core::str::from_utf8(home).ok().and_then(|home| {
        build_namespace(&Members {
            root_ns: Some(root.as_ref()),
            fs_endpoint: borrowed(&eps.fs),
            profile_endpoint: borrowed(&eps.profile),
            tty_endpoint: borrowed(&eps.tty),
            clipboard_endpoint: borrowed(&eps.clipboard),
            home,
            user,
            bind_fonts: true,
            bind_console: false,
            views_endpoint: views,
            views_base,
            devices_endpoint: borrowed(&eps.devices),
            storage_endpoint: borrowed(&eps.storage),
            services_endpoint: borrowed(&eps.services),
            // `/dev/draw` is bound whole below, for `desktop-shell`; `new` alone is an application's.
            bind_applications: true,
            draw_endpoint: None,
            desktop_endpoint: None,
        })
    });
    let Some(built) = built else {
        log::write(b"desktop-session-mgr: session namespace FAIL\n");
        if let (Some((id, _)), Some(sup)) = (opened, views_sup) {
            libsession::close_views_session(sup, id);
        }
        return;
    };
    let session_ns = built.ns;
    let bound = built.bound;
    // Unscoped, so the shell resolves `manage` as well as `new`; applications get `new` alone.
    if let Some(draw) = &eps.draw {
        if session_ns.bind("/dev/draw", draw).is_err() {
            log::write(b"desktop-session-mgr: /dev/draw bind FAIL (the shell cannot draw)\n");
        } else {
            log::write(b"desktop-session-mgr: /dev/draw bound into the session\n");
        }
    }
    // Reported from what was bound, not from what was asked for, so the gate's assertion can fail.
    if bound.console {
        log::write(b"desktop-session-mgr: session namespace built (WITH /dev/console)\n");
    } else {
        log::write(b"desktop-session-mgr: session namespace built (no /dev/console)\n");
    }
    if bound.clipboard {
        log::write(b"desktop-session-mgr: session has /dev/clipboard\n");
    } else {
        log::write(b"desktop-session-mgr: session has NO /dev/clipboard\n");
    }
    if bound.views {
        log::write(b"desktop-session-mgr: session has /dev/views\n");
    } else {
        log::write(b"desktop-session-mgr: session has NO /dev/views\n");
    }
    if bound.devices {
        log::write(b"desktop-session-mgr: session has /dev/devices\n");
    } else {
        log::write(b"desktop-session-mgr: session has NO /dev/devices\n");
    }
    if bound.storage {
        log::write(b"desktop-session-mgr: session has /storage\n");
    } else {
        log::write(b"desktop-session-mgr: session has NO /storage\n");
    }

    // Endpoints are passed as endpoints, since a binding never resolves back to one. The home is
    // an argument because the shell's own `/home` cannot tell it the base an application's bind
    // needs; not authority, the fs endpoint is.
    let home_str = core::str::from_utf8(home).unwrap_or("");
    let code = run_leader(
        root,
        &session_ns,
        notif,
        "desktop-shell",
        // The view base, for the home's reason; empty when the broker gave no session.
        &[home_str, views_base],
        // The graphical leader alone: it builds each application's namespace, and `nxsh` none.
        SysCaps::BIND_NAMESPACE,
        // In the order the shell reads them: compositor, fs, tty, profile, clipboard, view broker,
        // device manager, storage, services.
        &[
            borrowed(&eps.draw),
            borrowed(&eps.fs),
            borrowed(&eps.tty),
            borrowed(&eps.profile),
            borrowed(&eps.clipboard),
            views,
            borrowed(&eps.devices),
            borrowed(&eps.storage),
            borrowed(&eps.services),
        ],
    );
    if let (Some((id, _)), Some(sup)) = (opened, views_sup) {
        libsession::close_views_session(sup, id);
    }
    // The leader is reaped, so this drops the last reference to the namespace.
    drop(session_ns);
    Line::new().s(b"desktop-session-mgr: session ended (leader exit ").i(code as i64).s(b")").end();
}

/// Bootstrap registers: `rdi` the notification channel the leader is reaped on, `rsi` the
/// LOOKUP-only root namespace, `rdx` the control channel the endpoints arrive over, `rcx` unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"desktop-session-mgr: up\n");
    let (Some(notif), Some(root)) = (start.notify.take(), start.root.take()) else {
        fail(b"desktop-session-mgr: no notification queue or root namespace\n");
    };
    let control: Option<Endpoint> = start.take_handle0();

    // Endpoints in `service-mgr`'s send order; all but the first two are its routes.
    let next = || control.as_ref().and_then(recv_handoff);
    let fs = next();
    let profile = next();
    let tty = next();
    // The compositor's endpoint, which the serial column does not get: the shell needs `/dev/draw`
    // bound in its namespace, since it resolves `manage` as well as `new`.
    let draw = next();
    let clipboard = next();
    let views = next();
    // Info-only, because the shell holds it with `BIND_NAMESPACE`.
    let devices = next();
    // `None` on a boot without the storage service. Couriered rather than resolved here, because a
    // resolved endpoint dies with the service.
    let storage = next();
    let services = next();
    let eps = Endpoints { fs, profile, tty, draw, clipboard, views, devices, storage, services };
    let views_sup = if eps.views.is_some() {
        resolve::<Channel, Only, _>(&root, "/svc/views/session", Rights::SEND | Rights::RECV | Rights::WAIT).ok()
    } else {
        None
    };
    // Resolved once at startup: re-resolving per attempt would mint a session per keystroke.
    let auth = resolve::<Channel, Only, _>(&root, "/svc/auth", Rights::SEND | Rights::RECV | Rights::WAIT);
    let auth_ch = match auth {
        Ok(ch) if eps.fs.is_some() => ch,
        _ => {
            if eps.fs.is_none() {
                log::write(b"desktop-session-mgr: no fs endpoint; cannot build a session\n");
            } else {
                log::write(b"desktop-session-mgr: /svc/auth resolve FAIL\n");
            }
            fail(b"desktop-session-mgr: no graphical login\n");
        }
    };

    // The font before the window, so a missing one is reported as itself, not an empty window.
    let theme = greeter_theme();
    let (font, _) = match libdraw::text::load_ui_font(&root, &theme, b"desktop-session-mgr") {
        Ok(loaded) => loaded,
        Err(e) => {
            Line::new().s(b"desktop-session-mgr: the UI font ").s(e.why()).end();
            fail(b"desktop-session-mgr: font load FAILED\n");
        }
    };

    let transport = match ChannelTransport::open(&root) {
        Ok(t) => t,
        Err(_) => fail(b"desktop-session-mgr: connect to /dev/draw FAILED\n"),
    };
    let mut session = Session::new(transport);
    // An unreadable screen leaves the greeter at the origin and says so, rather than guess a size.
    let origin = match libsurface::screen::info(&root) {
        Ok(screen) => {
            let origin = (
                (screen.width.saturating_sub(GREETER_W) / 2) as i32,
                (screen.height.saturating_sub(GREETER_H) / 2) as i32,
            );
            Line::new()
                .s(b"desktop-session-mgr: greeter centred at ")
                .i(origin.0 as i64)
                .s(b",")
                .i(origin.1 as i64)
                .s(b" on a ")
                .u(screen.width as u64)
                .s(b"x")
                .u(screen.height as u64)
                .s(b" screen")
                .end();
            origin
        }
        Err(_) => {
            log::write(b"desktop-session-mgr: /dev/draw/screen unreadable; the greeter asks for the origin\n");
            (0, 0)
        }
    };
    // Created before every other client's window: the gates' reference windows stack above it.
    let mut greeter = Greeter::new();
    let (mut window, mut pool) = match open_greeter(&mut session, &font, &greeter, origin) {
        Some(opened) => opened,
        None => fail(b"desktop-session-mgr: greeter could not be drawn\n"),
    };
    Line::new()
        .s(b"desktop-session-mgr: greeter presented, window ")
        .u(window as u64)
        .s(b" ")
        .u(GREETER_W as u64)
        .s(b"x")
        .u(GREETER_H as u64)
        .end();

    // Blocks on the event channel, never spins: a spinning supervisor keeps the idle thread from
    // running, and deferred handle reclamation lives there.
    loop {
        {
            // The notification channel is waited on too, so a shutdown ends the supervisor.
            let mut set = WaitSet::<2>::new();
            let _ = set.add(session.waitable());
            let _ = set.add(&notif);
            let _ = set.wait(Deadline::NEVER);
        }
        if libsession::drain_stop_requests(&notif) {
            stop();
        }
        if session.pump().is_err() {
            fail(b"desktop-session-mgr: compositor connection lost\n");
        }
        let mut dirty = false;
        let mut submit = false;
        while let Some((w, event)) = session.next_event() {
            if w != window {
                continue;
            }
            if let libsurface::WindowEvent::Key(k) = event {
                // Presses only, or each character types twice; a held key's repeats are presses too.
                if k.pressed == 0 {
                    continue;
                }
                if k.keycode == KEY_ENTER {
                    submit = true;
                } else if greeter.key(k.keycode, k.modifiers, k.locks) {
                    dirty = true;
                }
            }
        }
        if submit {
            // The window is destroyed for the session, not hidden (the protocol has no "hide"):
            // left mapped, it sits under the session and covers the shell's top bar once raised.
            if let Some(w) = session.window(window) {
                let _ = w.destroy();
            }
            drop(pool);
            let mut user = [0u8; 64];
            let mut home = [0u8; 256];
            // **The password's block ends before the session starts**, which outlives any use of
            // it; `Secret` scrubs where it lies, so an earlier `drop` would scrub a copy.
            let (ul, authed) = {
                let mut pass = Secret::<128>::zeroed();
                let (ul, pl) = greeter.take_credentials(&mut user, &mut pass);
                (ul, authenticate_on(&auth_ch, &user[..ul], &pass[..pl], &mut home))
            };
            if let Some(hl) = authed {
                run_session(&root, &notif, &eps, views_sup.as_ref(), &user[..ul], &home[..hl]);
            } else {
                log::write(b"desktop-session-mgr: login denied\n");
            }
            if libsession::stop_asked() {
                stop();
            }
            // Before the window is drawn: `open_greeter` presents, and `view()` reads `denied`.
            greeter.denied = authed.is_none();
            match open_greeter(&mut session, &font, &greeter, origin) {
                Some((id, reopened)) => {
                    window = id;
                    pool = reopened;
                    // `check-logout` waits for this line after each logout.
                    Line::new().s(b"desktop-session-mgr: greeter presented again, window ").u(id as u64).end();
                }
                None => fail(b"desktop-session-mgr: could not draw the greeter again\n"),
            }
            dirty = false;
        }
        if dirty && !present(&mut session, window, &greeter, &font, &mut pool) {
            fail(b"desktop-session-mgr: greeter redraw FAILED\n");
        }
    }
}

/// Create the greeter window at `origin`, attach fresh buffers and present it; `None` on failure.
fn open_greeter(
    session: &mut Session<ChannelTransport>,
    font: &Font,
    greeter: &Greeter,
    origin: (i32, i32),
) -> Option<(u32, BufferPool)> {
    // The window asks for its origin rather than being placed: nothing manages windows yet, so a
    // `normal` window's request is honoured when the compositor stops waiting for a manager.
    let (x, y) = origin;
    let window = session.create(&CreateWindowRequest::at(GREETER_W, GREETER_H, Role::Normal, x, y), BUFFERS).ok()?;
    let mut pool = BufferPool::new(&mut session.window(window)?, Size::new(GREETER_W, GREETER_H), BUFFERS)?;
    if !present(session, window, greeter, font, &mut pool) {
        return None;
    }
    Some((window, pool))
}

/// Render the greeter into a free buffer and commit it. `false` if the compositor refused.
fn present(
    session: &mut Session<ChannelTransport>,
    window: u32,
    greeter: &Greeter,
    font: &Font,
    pool: &mut BufferPool,
) -> bool {
    let picture = greeter.render(font).into_bytes();
    let Some(mut w) = session.window(window) else {
        return false;
    };
    // `acquire` blocks for a buffer the compositor is not displaying; writing the committed one
    // would tear the picture.
    let Ok(slot) = w.acquire() else {
        return false;
    };
    let ok = pool.write(slot, &picture) && w.commit(slot, (0, 0, GREETER_W, GREETER_H)).is_ok();
    // A redraw counter paces the gate between keystrokes. It is a count, never the fields:
    // reporting what was typed would put a password on the console, and its length would leak it
    // more slowly. Capped like the compositor's press diagnostics.
    let redraws = REDRAWS.load(Relaxed);
    if redraws < MAX_REDRAWS_LOGGED {
        REDRAWS.store(redraws + 1, Relaxed);
        Line::new().s(b"desktop-session-mgr: greeter redraw ").u(redraws as u64 + 1).end();
    }
    ok
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"desktop-session-mgr: PANIC\n");
    libos::exit(1);
}
