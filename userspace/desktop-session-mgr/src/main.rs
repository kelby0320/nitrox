//! `desktop-session-mgr` — the graphical login supervisor, `session-mgr`'s twin.
//!
//! The same sequence through the same `libsession`, except that its greeter is a window. It
//! draws first and outlives every session it starts. No `/dev/console` in a graphical session.

#![no_std]
#![no_main]

extern crate alloc;

use desktop_session_mgr::{GREETER_H, GREETER_PITCH, GREETER_W, Greeter, KEY_ENTER, greeter_theme};
use libdraw::text::Font;
use libkern::debug::Line;
use libkern::*;
use librsproto::surface::{CreateWindowRequest, Role};
use libsession::{NamespaceSpec, authenticate, ns_lookup, spawn_leader};
use libsurface::Session;
use libsurface::ipc::ChannelTransport;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const BUFFERS: usize = 2;

const MAX_REDRAWS_LOGGED: u32 = 512;

static mut REDRAWS: u32 = 0;

fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// A shutdown asked this supervisor to stop with no session running: exit, which `service-mgr`
/// sees as its control channel closing. A running session is ended first by `spawn_leader`.
fn stop() -> ! {
    kprint(b"desktop-session-mgr: asked to stop, exiting\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 0, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

fn shared_buffer(len: usize) -> Option<(u64, *mut u8)> {
    // SAFETY: a plain anonymous object of `len` bytes.
    let h = unsafe { syscall4(SYS_MEMORY_CREATE, len as u64, 0, 0, 0) };
    if h <= 0 {
        return None;
    }
    // SAFETY: maps the object read/write at a kernel-chosen address.
    let base = unsafe { syscall4(SYS_MEMORY_MAP, h as u64, 0, len as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if base < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
        return None;
    }
    Some((h as u64, base as usize as *mut u8))
}

/// Receive the next control message's one moved handle; `0` on failure. Positional: `service-mgr`
/// sends an empty message for an endpoint it does not have.
fn recv_handoff(ctrl: u64) -> u64 {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = ctrl;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    if waited != 1 {
        return 0;
    }
    // The one handle a handoff brings; any more close as `sent` drops. A whole message's buffer:
    // the kernel copies all of one, however short its payload.
    // SAFETY: single-threaded; `RECV_MSG` is lent for the call.
    match unsafe { channel_recv_raw(ctrl, &raw mut RECV_MSG) } {
        Ok(mut sent) => sent.take(0).unwrap_or(0),
        Err(_) => 0,
    }
}

static mut RECV_MSG: [u8; IPC_MSG_SIZE] = [0; IPC_MSG_SIZE];

/// One login attempt, run to the session's end. `false` if the credentials were refused.
#[allow(clippy::too_many_arguments, reason = "the session's handles, one for each service it is given")]
fn run_session(
    root_ns: u64,
    notif: u64,
    auth_ch: u64,
    fs: u64,
    profile: u64,
    tty: u64,
    draw: u64,
    clipboard: u64,
    views: u64,
    views_sup: u64,
    devices: u64,
    storage: u64,
    services: u64,
    user: &[u8],
    password: &[u8],
) -> bool {
    let mut home = [0u8; 256];
    let Some(hl) = authenticate(auth_ch, user, password, &mut home) else {
        kprint(b"desktop-session-mgr: login denied\n");
        return false;
    };
    Line::new().s(b"desktop-session-mgr: login ok -> home=").s(&home[..hl]).end();
    // Before the namespace: the broker's base is part of what it binds.
    let mut views_base = [0u8; 24];
    let opened = if views_sup != 0 { libsession::views_open_session(views_sup, user, &mut views_base) } else { None };
    let base_len = opened.map_or(0, |v| v.1);

    // No `/dev/console`: a binding is authority, and the console is the serial column's recovery path.
    let built = libsession::build(&NamespaceSpec {
        root_ns,
        fs_endpoint: fs,
        profile_endpoint: profile,
        tty_endpoint: tty,
        clipboard_endpoint: clipboard,
        home: &home[..hl],
        user,
        bind_fonts: true,
        bind_console: false,
        views_endpoint: if opened.is_some() { views } else { 0 },
        views_base: &views_base[..base_len],
        devices_endpoint: devices,
        storage_endpoint: storage,
        services_endpoint: services,
        // `/dev/draw` is bound whole below, for `desktop-shell`; `new` alone is an application's.
        bind_applications: true,
        draw_endpoint: 0,
        desktop_endpoint: 0,
    });
    let Some(built) = built else {
        kprint(b"desktop-session-mgr: session namespace FAIL\n");
        if let Some((id, _)) = opened {
            libsession::views_close_session(views_sup, id);
        }
        return true;
    };
    let session_ns = built.ns;
    // Unscoped, so the shell resolves `manage` as well as `new`; applications get `new` alone.
    if draw != 0 {
        let path = b"/dev/draw";
        // SAFETY: valid namespace handle, path pointer and endpoint handle.
        let dr = unsafe { syscall4(SYS_NS_BIND, session_ns, path.as_ptr() as u64, path.len() as u64, draw) };
        if dr != 0 {
            kprint(b"desktop-session-mgr: /dev/draw bind FAIL (the shell cannot draw)\n");
        } else {
            kprint(b"desktop-session-mgr: /dev/draw bound into the session\n");
        }
    }
    // Reported from what was bound, not from what was asked for, so the gate's assertion can fail.
    if built.console {
        kprint(b"desktop-session-mgr: session namespace built (WITH /dev/console)\n");
    } else {
        kprint(b"desktop-session-mgr: session namespace built (no /dev/console)\n");
    }
    if built.clipboard {
        kprint(b"desktop-session-mgr: session has /dev/clipboard\n");
    } else {
        kprint(b"desktop-session-mgr: session has NO /dev/clipboard\n");
    }
    if built.views {
        kprint(b"desktop-session-mgr: session has /dev/views\n");
    } else {
        kprint(b"desktop-session-mgr: session has NO /dev/views\n");
    }
    if built.devices {
        kprint(b"desktop-session-mgr: session has /dev/devices\n");
    } else {
        kprint(b"desktop-session-mgr: session has NO /dev/devices\n");
    }
    if built.storage {
        kprint(b"desktop-session-mgr: session has /storage\n");
    } else {
        kprint(b"desktop-session-mgr: session has NO /storage\n");
    }

    // Endpoints are passed as endpoints, since a binding never resolves back to one. The home is
    // an argument because the shell's own `/home` cannot tell it the base an application's bind
    // needs; not authority, the fs endpoint is.
    let home_str = core::str::from_utf8(&home[..hl]).unwrap_or("");
    // The view base, for the home's reason; empty when the broker gave no session.
    let base_str = core::str::from_utf8(&views_base[..base_len]).unwrap_or("");
    let views_for_shell = if opened.is_some() { views } else { 0 };
    let code = spawn_leader(
        root_ns,
        session_ns,
        notif,
        "desktop-shell",
        &[home_str, base_str],
        // The graphical leader alone: it builds each application's namespace, and `nxsh` none.
        SYSCAP_BIND_NAMESPACE,
        // In the order the shell reads them: compositor, fs, tty, profile, clipboard, view broker,
        // device manager, storage, services.
        &[draw, fs, tty, profile, clipboard, views_for_shell, devices, storage, services],
    );
    if let Some((id, _)) = opened {
        libsession::views_close_session(views_sup, id);
    }
    // The leader is reaped, so this drops the last reference to the namespace.
    // SAFETY: closing the namespace we created for this session.
    unsafe { syscall1(SYS_HANDLE_CLOSE, session_ns) };
    Line::new().s(b"desktop-session-mgr: session ended (leader exit ").i(code as i64).s(b")").end();
    true
}

/// Bootstrap registers: `rdi` the notification channel the leader is reaped on, `rsi` the
/// LOOKUP-only root namespace, `rdx` the control channel the endpoints arrive over, `rcx` unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"desktop-session-mgr: up\n");

    // Endpoints in `service-mgr`'s send order; all but the first two are its routes.
    let fs_endpoint = recv_handoff(control);
    let profile_endpoint = recv_handoff(control);
    let tty_endpoint = recv_handoff(control);
    // The compositor's endpoint, which the serial column does not get: the shell needs `/dev/draw`
    // bound in its namespace, since it resolves `manage` as well as `new`.
    let draw_endpoint = recv_handoff(control);
    let clipboard_endpoint = recv_handoff(control);
    let views_endpoint = recv_handoff(control);
    // Info-only, because the shell holds it with `BIND_NAMESPACE`.
    let devices_endpoint = recv_handoff(control);
    // `0` on a boot without the storage service. Couriered rather than resolved here, because a
    // resolved endpoint dies with the service.
    let storage_endpoint = recv_handoff(control);
    let services_endpoint = recv_handoff(control);
    let views_sup = if views_endpoint != 0 {
        ns_lookup(root_ns, b"/svc/views/session", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT).1
    } else {
        0
    };
    // Resolved once at startup: re-resolving per attempt would mint a session per keystroke.
    let (auth_status, auth_ch) = ns_lookup(root_ns, b"/svc/auth", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if fs_endpoint == 0 || auth_status != 0 || auth_ch == 0 {
        if fs_endpoint == 0 {
            kprint(b"desktop-session-mgr: no fs endpoint; cannot build a session\n");
        } else {
            kprint(b"desktop-session-mgr: /svc/auth resolve FAIL\n");
        }
        fail(b"desktop-session-mgr: no graphical login\n");
    }

    // The font before the window, so a missing one is reported as itself, not an empty window.
    let theme = greeter_theme();
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run
    let (font, _) = match unsafe { libdraw::text::load_ui(root_ns, &theme, b"desktop-session-mgr") } {
        Ok(loaded) => loaded,
        Err(e) => {
            libkern::debug::Line::new().s(b"desktop-session-mgr: the UI font ").s(e.why()).end();
            fail(b"desktop-session-mgr: font load FAILED\n");
        }
    };

    // SAFETY: `root_ns` is live for this process's whole run.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => t,
        Err(_) => fail(b"desktop-session-mgr: connect to /dev/draw FAILED\n"),
    };
    let mut session = Session::new(transport);
    // An unreadable screen leaves the greeter at the origin and says so, rather than guess a size.
    // SAFETY: `root_ns` is live for this process's whole run.
    let origin = match unsafe { libsurface::screen::read(root_ns) } {
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
            kprint(b"desktop-session-mgr: /dev/draw/screen unreadable; the greeter asks for the origin\n");
            (0, 0)
        }
    };
    // Created before every other client's window: the gates' reference windows stack above it.
    let mut greeter = Greeter::new();
    let len = GREETER_PITCH * GREETER_H as usize;
    let mut addrs = [core::ptr::null_mut::<u8>(); BUFFERS];
    let mut window = match open_greeter(&mut session, &font, &greeter, &mut addrs, len, origin) {
        Some(id) => id,
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
    let ev = session.wait_handle();
    loop {
        // The notification channel is waited on too, so a shutdown ends the supervisor.
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers for two waiters.
        unsafe {
            WAIT_HANDLES[0] = ev;
            WAIT_HANDLES[1] = notif;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 2, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        if libsession::drain_for_stop(notif) {
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
            // Copied out before the fields are cleared, and cleared before the session runs:
            // the password must not still be in the greeter while a session is on screen.
            let mut user = [0u8; 64];
            let mut pass = Secret([0u8; 128]);
            let ul = greeter.user.text().len().min(user.len());
            user[..ul].copy_from_slice(&greeter.user.text().as_bytes()[..ul]);
            // One past the buffer is not cut to a prefix that may be a password: it is no password.
            let typed = greeter.password.text().as_bytes();
            let pl = pass.get_mut(..typed.len()).map_or(0, |p| {
                p.copy_from_slice(typed);
                typed.len()
            });
            greeter.reset();
            // The window is destroyed for the session, not hidden (the protocol has no "hide"):
            // left mapped, it sits under the session and covers the shell's top bar once raised.
            if let Some(w) = session.window(window) {
                let _ = w.destroy();
            }
            let ok = run_session(
                root_ns,
                notif,
                auth_ch,
                fs_endpoint,
                profile_endpoint,
                tty_endpoint,
                draw_endpoint,
                clipboard_endpoint,
                views_endpoint,
                views_sup,
                devices_endpoint,
                storage_endpoint,
                services_endpoint,
                &user[..ul],
                &pass[..pl],
            );
            drop(pass);
            if libsession::stop_asked() {
                stop();
            }
            // Before the window is drawn: `open_greeter` presents, and `view()` reads `denied`.
            greeter.denied = !ok;
            match open_greeter(&mut session, &font, &greeter, &mut addrs, len, origin) {
                Some(id) => {
                    window = id;
                    // `check-logout` waits for this line after each logout.
                    Line::new().s(b"desktop-session-mgr: greeter presented again, window ").u(id as u64).end();
                }
                None => fail(b"desktop-session-mgr: could not draw the greeter again\n"),
            }
            dirty = false;
        }
        if dirty && !present(&mut session, window, &greeter, &font, &addrs, len) {
            fail(b"desktop-session-mgr: greeter redraw FAILED\n");
        }
    }
}

/// Create the greeter window at `origin`, attach fresh buffers and present it; `None` on failure.
fn open_greeter(
    session: &mut Session<ChannelTransport>,
    font: &Font,
    greeter: &Greeter,
    addrs: &mut [*mut u8; BUFFERS],
    len: usize,
    origin: (i32, i32),
) -> Option<u32> {
    // The window asks for its origin rather than being placed: nothing manages windows yet, so a
    // `normal` window's request is honoured when the compositor stops waiting for a manager.
    let (x, y) = origin;
    let window = session.create(&CreateWindowRequest::at(GREETER_W, GREETER_H, Role::Normal, x, y), BUFFERS).ok()?;
    #[allow(clippy::needless_range_loop, reason = "the index also feeds arithmetic or a second array")]
    for i in 0..BUFFERS {
        let (handle, addr) = shared_buffer(len)?;
        addrs[i] = addr;
        let mut w = session.window(window)?;
        if w.attach(i as u32, GREETER_W, GREETER_H, GREETER_PITCH as u32, handle).is_err() {
            return None;
        }
    }
    if !present(session, window, greeter, font, addrs, len) {
        return None;
    }
    Some(window)
}

/// Render the greeter into a free buffer and commit it. `false` if the compositor refused.
fn present(
    session: &mut Session<ChannelTransport>,
    window: u32,
    greeter: &Greeter,
    font: &Font,
    addrs: &[*mut u8; BUFFERS],
    len: usize,
) -> bool {
    let picture = greeter.render(font).into_bytes();
    if picture.len() != len {
        return false;
    }
    let Some(mut w) = session.window(window) else {
        return false;
    };
    // `acquire` blocks for a buffer the compositor is not displaying; writing the committed one
    // would tear the picture.
    let Ok(slot) = w.acquire() else {
        return false;
    };
    let addr = addrs[slot as usize % BUFFERS];
    // SAFETY: `addr` maps `len` writable bytes and `picture` holds exactly `len`; the two are
    // distinct allocations, so they cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(picture.as_ptr(), addr, len) };
    let ok = w.commit(slot, (0, 0, GREETER_W, GREETER_H)).is_ok();
    // A redraw counter paces the gate between keystrokes. It is a count, never the fields:
    // reporting what was typed would put a password on the console, and its length would leak it
    // more slowly. Capped like the compositor's press diagnostics.
    // SAFETY: single-threaded supervisor; one redraw at a time.
    unsafe {
        if REDRAWS < MAX_REDRAWS_LOGGED {
            REDRAWS += 1;
            Line::new().s(b"desktop-session-mgr: greeter redraw ").u(REDRAWS as u64).end();
        }
    }
    ok
}

static mut WAIT_HANDLES: [u64; 2] = [0; 2];
/// A 24-byte `IoResult` per waiter.
static mut WAIT_RESULTS: [u8; 48] = [0; 48];

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"desktop-session-mgr: PANIC\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
