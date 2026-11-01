//! `desktop-shell`: the graphical session's leader, and the compositor's first real manager.
//!
//! A top bar and a bottom bar (one entry per `normal` window), both reserving space with a
//! `panel` strut. It does not bind its own endpoint (`graphical-session.md` §3): it holds
//! `BIND_NAMESPACE` to construct application namespaces continuously, and serves `/dev/desktop`
//! by binding it into those, not into the session namespace. A process cannot verify a binding of
//! itself by using it, so a `desktop` command in `/bin` is the consumer.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;
use libkern::debug::Line;
use libkern::*;

const KEY_ESC: u16 = 1;
const KEY_ENTER: u16 = 28;
/// `EV_KEY` code for `h`; named here, as `libkern::abi` lists only the keys the ABI needs.
const KEY_H: u16 = 35;

const HOTKEY_MINIMIZE: u32 = 1;
const KEY_R: u16 = 19;
const HOTKEY_RENAME: u32 = 2;
const KEY_A: u16 = 30;
const HOTKEY_APPS: u32 = 3;
/// How many desktops the number chords reach: four, as each costs two of `MAX_HOTKEYS`' sixteen.
const CHORD_DESKTOPS: u32 = 4;
/// Ids `HOTKEY_SWITCH_BASE + n` switch to desktop `n`; `HOTKEY_MOVE_BASE + n` move to it.
const HOTKEY_SWITCH_BASE: u32 = 10;
const HOTKEY_MOVE_BASE: u32 = 20;
const KEY_1: u16 = 2;
use desktop_shell::ending::{Asked, Closing, Ending, Next, POWER_PROGRAM, POWER_VIEW, Refusal, failed};
use desktop_shell::panel::{self, BottomMsg, EndingMsg, MenuMsg, RefusalMsg, TopMsg};
use desktop_shell::snap::RestoreList;
use desktop_shell::{
    Application, BAR_H, CARD_BORDER, CARD_CAPTION_GAP, CARD_CAPTION_H, CASCADE_STEP, Screen, ShownDesktop, parse_entry,
};
use librsproto::surface::{
    CreateWindowRequest, Edge, MgrLayout, OP_MGR_QUERY_LAYOUT, OP_MGR_SET_WINDOW_DESKTOP, Role, STICKY_DESKTOP,
};
use libsurface::ipc::ChannelTransport;
use libsurface::{Session, Transport};
use libui::element::{Element, Insets, custom, fill, offset, padding, row, sized, stack, text};
use libui::layout::layout;
use libui::menu::{Item, KeyOutcome, MenuState};
use libui::paint::{FontMetrics, Theme, paint_over};
use libui::widget::TextFieldState;
use libui::window::Child;

mod apps;
mod closing;
mod desktop_sessions;
mod menus;
mod overview;
mod placement;
mod power;
mod wallpaper;
mod windows;

use apps::*;
use closing::*;
use desktop_sessions::*;
use menus::*;
use overview::*;
use placement::*;
use power::*;
use wallpaper::*;
use windows::*;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The wall clock as the top bar shows it, `YYYY-MM-DD HH:MM` with no month names (no timezone
/// database or locale), or empty when unset: the kernel reports `Unsupported`, not 1970.
fn clock_text() -> alloc::string::String {
    let mut nanos = 0u64;
    // SAFETY: `&mut nanos` is a valid writable u64 out-param for the clock read.
    let r =
        unsafe { libkern::syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_REALTIME, (&raw mut nanos) as u64) };
    if r < 0 {
        return alloc::string::String::new();
    }
    let c = libtime::civil_from_unix(nanos);
    let full = libtime::format_civil(&c);
    alloc::string::String::from(full.get(..16).unwrap_or(full.as_str()))
}

/// When the displayed minute next changes, as a monotonic deadline aligned to the minute.
fn next_minute(now: u64) -> u64 {
    let mut nanos = 0u64;
    // SAFETY: as above.
    let r =
        unsafe { libkern::syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_REALTIME, (&raw mut nanos) as u64) };
    const MINUTE: u64 = 60 * 1_000_000_000;
    if r < 0 {
        return now.saturating_add(MINUTE);
    }
    now.saturating_add(MINUTE - nanos % MINUTE)
}

const BUFFERS: usize = 2;

fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// A shutdown asked this shell to stop: windows are asked to close first (`Asked::Stop`), then
/// this exit.
fn stop() -> ! {
    kprint(b"desktop-shell: asked to stop, exiting\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 0, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// Read every notification queued on `notif`, and say whether a terminate request was among them.
fn stop_asked(notif: u64) -> bool {
    let mut asked = false;
    loop {
        let mut n = Notification::zeroed();
        // SAFETY: `n` is a valid 64-byte writable out-param on this frame.
        if unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut n) as u64, 0, 0) } != 0 {
            return asked;
        }
        asked |= n.kind == libkern::abi::KIND_TERMINATE_REQUESTED;
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

/// `theme`, with a bar's ground in place of a window's.
fn panel(theme: &Theme) -> Theme {
    Theme { background: theme.panel, ..*theme }
}

fn env_str<'a>(env: &'a libstream::wire::Record, name: &str) -> Option<&'a str> {
    env.schema.fields.iter().position(|f| f.name == name).and_then(|i| env.values.get(i)).and_then(|v| v.as_str())
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

/// Receive one message and return its first transferred handle, or `0`; any more close as the
/// rest of the [`Received`] drops.
fn recv_handle(ch: u64) -> u64 {
    if !wait_one(ch) {
        return 0;
    }
    // SAFETY: single-threaded; `RECV_MSG` is lent for the call.
    match unsafe { channel_recv_raw(ch, &raw mut RECV_MSG) } {
        Ok(mut sent) => sent.take(0).unwrap_or(0),
        Err(_) => 0,
    }
}

fn wait_one(h: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers, and `WAIT_RESULTS` holds one
    // `IoResult` per handle in the set — see its declaration for why that is the requirement.
    let waited = unsafe {
        WAIT_HANDLES[0] = h;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    waited == 1
}

static mut RECV_MSG: [u8; 4096] = [0; 4096];

/// Wait set: the compositor's event channel, the manager channel, `/dev/desktop` and its
/// sessions, the notification channel (a shutdown's terminate request), and the view broker's
/// while a power action runs.
static mut WAIT_HANDLES: [u64; 5 + MAX_DESKTOP_SESSIONS] = [0; 5 + MAX_DESKTOP_SESSIONS];
static mut SHELL_NOTIF: u64 = 0;
/// One 24-byte `IoResult` per handle in the set: `sys_wait` takes no length and writes one record
/// per signalled handle (`kernel/src/syscall/table.rs`), so a smaller buffer overruns a static.
static mut WAIT_RESULTS: [u8; 24 * (5 + MAX_DESKTOP_SESSIONS)] = [0; 24 * (5 + MAX_DESKTOP_SESSIONS)];

/// Bootstrap registers, as `libsession::spawn_leader` fills them: `rdi` = notification
/// channel, `rsi` = the **session** namespace, `rdx` = the Tier-1 setup channel carrying
/// `argv` and the environment, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, session_ns: u64, setup: u64, arg0: u64) -> ! {
    kprint(b"desktop-shell: up (graphical session leader)\n");
    // SAFETY: single-threaded; written once, here, before the loop waits on it.
    unsafe { SHELL_NOTIF = notif };

    let (argv, env) = match libstream::setup::bootstrap(notif, session_ns, setup, arg0).setup() {
        Some(Ok(s)) => (s.argv, s.env),
        _ => (alloc::vec::Vec::new(), libstream::wire::Record::default()),
    };
    let theme = read_theme(session_ns);
    let env = env.with_str_field("THEME", &theme.to_config());
    // `argv[1]` is the user's real home; empty leaves an application's `/home` unbound rather
    // than wider than the session's own.
    let home: &str = argv.get(1).map(|s| s.as_str()).unwrap_or("");
    if home.is_empty() {
        kprint(b"desktop-shell: no home in argv; applications get no /home\n");
    }
    // Every server's is `service-mgr`'s route to it, reaching that server alone whichever
    // instance runs (`fs` and `profile` are `init`'s servers' own): what is bound into an
    // application reaches a restarted server, and `BIND_NAMESPACE` reaches nothing a route does
    // not lead to.
    let draw_endpoint = recv_handle(setup);
    let fs_endpoint = recv_handle(setup);
    let tty_endpoint = recv_handle(setup);
    let profile_endpoint = recv_handle(setup);
    let clipboard_endpoint = recv_handle(setup);
    // The view broker's, and `argv[2]`, the session's base: both bound into every application
    // namespace so `with` works in a terminal launched here. Either absent means no `/dev/views`.
    let views_endpoint = recv_handle(setup);
    let views_base: &str = argv.get(2).map(|s| s.as_str()).unwrap_or("");
    // An info-only endpoint of the device manager's, bound at `/dev/devices` with base `/info`:
    // nothing reached through it can subscribe to a device.
    let devices_endpoint = recv_handle(setup);
    // The storage service's session endpoint, bound at `/storage` and `/dev/storage`: it answers
    // the filesystems and table only, so this shell cannot mount with it.
    let storage_endpoint = recv_handle(setup);
    let services_endpoint = recv_handle(setup);
    if draw_endpoint == 0 {
        kprint(b"desktop-shell: no compositor endpoint; cannot launch applications\n");
    }
    if fs_endpoint == 0 || tty_endpoint == 0 {
        kprint(b"desktop-shell: applications will have no fonts or no terminal\n");
    }

    // Resolved from the session namespace, where `/dev/draw` was bound unscoped; an application's
    // gets a narrower bind, which is what gates the manager channel.
    // SAFETY: `session_ns` is this process's namespace, live for its whole run.
    let (font, _) = match unsafe { libdraw::text::load_ui(session_ns, &theme, b"desktop-shell") } {
        Ok(loaded) => loaded,
        Err(e) => {
            libkern::debug::Line::new().s(b"desktop-shell: the UI font ").s(e.why()).end();
            fail(b"desktop-shell: font load FAILED (is /system readable in the session?)\n");
        }
    };

    // SAFETY: `session_ns` is live for this process's whole run.
    let transport = match unsafe { ChannelTransport::connect(session_ns) } {
        Ok(t) => t,
        Err(_) => fail(b"desktop-shell: connect to /dev/draw FAILED\n"),
    };
    let mut session = Session::new(transport);

    // The screen's size, asked: every bar, the wallpaper, the overview and the cascade lay out on
    // it. Fatal if unreadable: a guessed size would be drawn where nobody can see it.
    // SAFETY: `session_ns` is live for this process's whole run.
    let screen = match unsafe { libsurface::screen::read(session_ns) } {
        Ok(s) => Screen { width: s.width, height: s.height },
        Err(_) => fail(b"desktop-shell: /dev/draw/screen unreadable; cannot lay out the desktop\n"),
    };
    Line::new().s(b"desktop-shell: screen ").u(screen.width as u64).s(b"x").u(screen.height as u64).end();

    let wallpaper = open_wallpaper(session_ns, &mut session, &theme, screen);
    let wallpaper_window = wallpaper.as_ref().map_or(0, |w| w.window);

    // `panel`, not `normal`: the role reserves the strut. A `Child`, so its words are routed.
    let panel_theme = panel(&theme);
    let mut shown_clock = clock_text();
    let mut top = match Child::open_sized(
        &mut session,
        Role::Panel { dock: Edge::Top, reserve: BAR_H },
        (0, 0),
        libdraw::geom::Size::new(screen.width, BAR_H),
        &panel::top_bar(&shown_clock, None, None, &theme),
        &font,
        &panel_theme,
        BUFFERS,
    ) {
        Some(c) => c,
        None => fail(b"desktop-shell: top bar CreateWindow FAILED\n"),
    };
    let window = top.id();
    // Said once: an absent clock (RTC unreadable at boot) otherwise looks like a broken bar.
    {
        let mut l = Line::new();
        l.s(b"desktop-shell: clock ");
        if shown_clock.is_empty() {
            l.s(b"unset");
        } else {
            l.s(shown_clock.as_bytes());
        }
        l.end();
    }
    // Build one application namespace and check it before anything is launched, and gate on the
    // result: a shell that knew its namespaces were wrong must not launch into them. It runs
    // before the manager channel is taken, the only moment a `manage` resolve gets an honest
    // answer from a namespace that does not bind it; the desktop endpoint is created first, as it
    // is bound.
    let desktop_endpoint = match make_channel() {
        Some((client_end, serve_end)) => {
            // SAFETY: storing our own serve end.
            unsafe { DESKTOP_SERVE = serve_end };
            kprint(b"desktop-shell: serving /dev/desktop\n");
            client_end
        }
        None => {
            kprint(b"desktop-shell: could not create the /dev/desktop endpoint\n");
            0
        }
    };

    let user = session_user(session_ns);
    let mut launcher = Launcher {
        session_ns,
        draw: draw_endpoint,
        fs: fs_endpoint,
        tty: tty_endpoint,
        profile: profile_endpoint,
        desktop: desktop_endpoint,
        clipboard: clipboard_endpoint,
        views: views_endpoint,
        views_base,
        devices: devices_endpoint,
        storage: storage_endpoint,
        services: services_endpoint,
        home,
        user: &user,
        env: &env,
        enabled: false,
    };
    let mut may_launch = false;
    if draw_endpoint != 0
        && let Some(built) = build_app_namespace(&launcher)
    {
        may_launch = verify_app_namespace(
            built.ns,
            !home.is_empty(),
            desktop_endpoint != 0,
            devices_endpoint != 0,
            storage_endpoint != 0,
            !user.is_empty(),
        );
        // SAFETY: closing the namespace; nothing has been launched into it yet.
        unsafe { syscall1(SYS_HANDLE_CLOSE, built.ns) };
    }
    if !may_launch {
        kprint(b"desktop-shell: application namespaces are not gated; launching is disabled\n");
    }
    launcher.enabled = may_launch;

    Line::new()
        .s(b"desktop-shell: top bar presented, window ")
        .u(window as u64)
        .s(b" ")
        .u(screen.width as u64)
        .s(b"x")
        .u(BAR_H as u64)
        .end();

    // The bottom bar is created here and placed much later: a `panel` is held for the manager
    // like a `normal` window and `create` blocks until the first `Configure`, so creating it
    // after the manager is attached parks the shell, unable to drain the channel and send the
    // releasing `Place`. `Place` works on an existing window, so only the placement waits.
    let mut entries: alloc::vec::Vec<WinEntry> = alloc::vec::Vec::new();
    let mut list_dirty = true;
    let mut desktops: alloc::vec::Vec<Desktop> = alloc::vec![Desktop { id: 1, name: alloc::string::String::new() }];
    let mut next_desktop_id: u32 = 2;
    let mut current_desktop: u32 = 1;
    let mut told_desktop: u32 = 1;
    // A `Child`, created undrawn: the compositor docks a panel at the origin until it is placed,
    // so a frame now would sit over the top bar.
    let mut bottom_bar = Child::create_sized(
        &mut session,
        Role::Panel { dock: Edge::Bottom, reserve: BAR_H },
        libdraw::geom::Size::new(screen.width, BAR_H),
        BUFFERS,
    );
    let bottom = bottom_bar.as_ref().map(|c| c.id());
    match bottom {
        Some(id) => Line::new().s(b"desktop-shell: bottom bar presented, window ").u(id as u64).end(),
        None => kprint(b"desktop-shell: bottom bar CreateWindow FAILED; no window list\n"),
    }
    let mut shown: Option<ShownDesktop> = None;

    // The manager channel, which makes this the compositor's first real manager, from the session
    // namespace that binds the `/dev/draw` subtree unscoped (an application's binds
    // `/dev/draw/new` alone). A `normal` window's first `Configure` is held until the manager
    // acts, so from here nothing reaches the screen unless this process places it.
    // SAFETY: `session_ns` is live for this process's whole run.
    let mut manager = match unsafe { ChannelTransport::manage(session_ns) } {
        Ok(m) => {
            kprint(b"desktop-shell: manager channel held\n");
            Some(m)
        }
        Err(_) => {
            // Not fatal: a shell that cannot manage still draws a bar and launches things; say
            // so.
            kprint(b"desktop-shell: /dev/draw/manage unavailable; windows will not be placed\n");
            None
        }
    };
    // The scheme, to the one process that cannot read it: `init` started the compositor before
    // anyone read a theme. Sent before any application is launched.
    if let Some(m) = manager.as_mut() {
        tell_scheme(m, theme.scheme);
    }
    let mut layout = manager.as_mut().and_then(query_layout).unwrap_or(default_layout(screen));
    Line::new()
        .s(b"desktop-shell: work area ")
        .i(layout.work_x as i64)
        .s(b",")
        .i(layout.work_y as i64)
        .s(b" ")
        .u(layout.work_w as u64)
        .s(b"x")
        .u(layout.work_h as u64)
        .s(b" of ")
        .u(layout.screen_w as u64)
        .s(b"x")
        .u(layout.screen_h as u64)
        .end();
    if let Some(m) = manager.as_mut() {
        register_snap_zones(m, &layout);
    }

    // Where a maximised window came from. The shell's, not the compositor's: a `maximized` flag
    // there would be a second source of truth.
    let mut restore = RestoreList::new();
    // Windows already asked to close, whose next middle-click destroys them.
    //
    // Asked first, insisted on afterwards: a taskbar that destroyed a window would take the
    // decision from the only participant that knows whether it matters. Insisting waits for the
    // person, not a clock: a timer loses the editor's buffer, and a shell cannot tell wedged from
    // asking, so the second click says "I meant it".
    //
    // The arming expires, because the shell cannot see an answer (`CloseRequested` has no refusal
    // by design): otherwise a person who chose keep editing leaves the window armed for its life,
    // and a later click goes straight to `Manage::Close`.
    let mut asked_to_close: alloc::vec::Vec<(u32, u64)> = alloc::vec::Vec::new();
    let mut next_origin = layout.work_y + CASCADE_STEP;

    let mut hotkey_done = manager.is_none();

    if let Some(m) = manager.as_mut() {
        // Both bars and the wallpaper are made sticky, or they vanish on the first switch: the
        // compositor stamps new windows with its current desktop and `visible_on` governs
        // compositing, focus and hit-testing, so after `Super+2` no chrome would be on screen.
        // `STICKY_DESKTOP` makes chrome the screen's, not one desktop's; `check-login`'s switch
        // step catches the wallpaper's absence.
        for surface in [Some(window), bottom, Some(wallpaper_window).filter(|&w| w != 0)].into_iter().flatten() {
            if window_value(m, OP_MGR_SET_WINDOW_DESKTOP, surface, STICKY_DESKTOP) {
                Line::new().s(b"desktop-shell: surface ").u(surface as u64).s(b" is sticky").end();
            } else {
                Line::new()
                    .s(b"desktop-shell: surface ")
                    .u(surface as u64)
                    .s(b" could not be made sticky; it will vanish on a desktop switch")
                    .end();
            }
        }
    }
    if let Some(id) = bottom
        && let Some(m) = manager.as_mut()
    {
        place_window(m, id, 0, screen.window_list_y());
        Line::new().s(b"desktop-shell: bottom bar placed at 0,").i(screen.window_list_y() as i64).end();
    }
    present_bottom(
        &mut session,
        bottom_bar.as_mut(),
        &entries,
        &desktops,
        current_desktop,
        &shown,
        &theme,
        &panel_theme,
        &font,
        screen,
    );

    let programs = read_applications(session_ns);
    Line::new().s(b"desktop-shell: /applications lists ").u(programs.len() as u64).s(b" application(s)").end();
    let app_home = env_str(&env, "HOME").unwrap_or("/home");
    let places = libfs::places(app_home);
    let mut bar = BarMenus {
        state: MenuState::new(3),
        win: None,
        query: TextFieldState::new(),
        programs: &programs,
        places: &places,
        home: app_home,
        anchors: menu_anchors(&shown_clock, &theme, &font, screen),
    };
    let mut prompt: Option<Child> = None;
    let mut closing: Option<Closing> = None;
    let mut ending_win: Option<Child> = None;
    let mut last_focus: u32 = 0;
    let mut powering: Option<(u64, Ending)> = None;
    let mut refused: Option<(Child, Ending, Refusal)> = None;
    let ending_anchor = ending_anchor(&shown_clock, &theme, &font, screen);
    let mut name = TextFieldState::new();
    let mut overview: Option<u32> = None;
    let mut over_addrs = [core::ptr::null_mut::<u8>(); BUFFERS];
    let mut shots: alloc::vec::Vec<(u32, u32, u32, alloc::vec::Vec<u8>)> = alloc::vec::Vec::new();
    let mut dragging: Option<u32> = None;
    let mut overview_dirty = false;

    // Blocks on the compositor's event channel, never spins: a spinning leader keeps a run queue
    // non-empty, so the idle thread never runs and reclamation stops machine-wide.
    let ev = session.wait_handle();
    let mut sent_request = false;
    let mut next_tick = now_ns().map(next_minute).unwrap_or(u64::MAX);
    loop {
        // Both channels in one wait: the session's events and the manager's. Polling one while
        // blocked on the other would make a held window wait for a keystroke. A zero deadline
        // when a request has just been sent, or the session holds events: `sys_wait` sees only
        // kernel queues, and events parked inside the transport while a reply was awaited wake
        // nothing, so a press would sit until the minute turned. `pump` moves them to the queues
        // the drain reads.
        if session.pump().is_err() {
            fail(b"desktop-shell: compositor connection lost\n");
        }
        let mut ending_deadline = u64::MAX;
        let mut ended = None;
        if let Some(c) = closing.as_mut() {
            if let Some(m) = manager.as_mut() {
                let windows: alloc::vec::Vec<(u32, alloc::string::String)> =
                    entries.iter().map(|e| (e.id, entry_title(e))).collect();
                for id in c.sync(&windows) {
                    request_close(m, id);
                    sent_request = true;
                }
            }
            match c.next(now_ns().unwrap_or(u64::MAX)) {
                Next::Wait { until } => ending_deadline = until,
                Next::Dialog => {
                    let opening = ending_win.is_none();
                    show_ending_dialog(&mut session, &mut ending_win, c, window, ending_anchor, &theme, &font);
                    // The keyboard goes back to the question it was taken from: the new dialog
                    // took focus from the editor's "discard?", which answers only Escape as
                    // Cancel, so answering it cancelled the logout. Only an application's
                    // transient window gets it back.
                    let ours_now = [window, bottom.unwrap_or(0), bar.id().unwrap_or(0), wallpaper_window];
                    let transient = last_focus != 0
                        && !ours_now.contains(&last_focus)
                        && prompt.as_ref().map(|p| p.id()) != Some(last_focus)
                        && refused.as_ref().map(|(r, _, _)| r.id()) != Some(last_focus)
                        && !entries.iter().any(|e| e.id == last_focus);
                    if opening
                        && ending_win.is_some()
                        && transient
                        && let Some(m) = manager.as_mut()
                        && raise_id(m, last_focus)
                    {
                        Line::new()
                            .s(b"desktop-shell: the keyboard back to window ")
                            .u(last_focus as u64)
                            .s(b", the question it was taken from")
                            .end();
                        sent_request = true;
                    }
                }
                Next::End(asked) => {
                    if let Some(d) = ending_win.take() {
                        d.close(&mut session);
                    }
                    ended = Some(asked);
                }
            }
        }
        // Every window is gone. Log out and a stop end the shell here; Restart and Shut down ask
        // the broker for `shutdown`, and the shell goes on until asked to stop, or says why not.
        if let Some(asked) = ended {
            closing = None;
            match asked {
                Asked::Person(e) if e.power_args().is_some() => match ask_to_power(&launcher, e) {
                    Ok(ch) => powering = Some((ch, e)),
                    Err(r) => show_refusal(&mut session, &mut refused, e, r, window, ending_anchor, &theme, &font),
                },
                _ => end_session(asked),
            }
        }
        let deadline = if sent_request || session.events_pending() > 0 { 0 } else { next_tick.min(ending_deadline) };
        sent_request = false;
        let mgr_h = manager.as_ref().map(|m| m.wait_handle()).unwrap_or(0);
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers sized for the whole set.
        unsafe {
            WAIT_HANDLES[0] = ev;
            let mut n = 1u64;
            if mgr_h != 0 {
                WAIT_HANDLES[n as usize] = mgr_h;
                n += 1;
            }
            if DESKTOP_SERVE != 0 {
                WAIT_HANDLES[n as usize] = DESKTOP_SERVE;
                n += 1;
            }
            #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
            for i in 0..MAX_DESKTOP_SESSIONS {
                if DESKTOP_SESSIONS[i] != 0 {
                    WAIT_HANDLES[n as usize] = DESKTOP_SESSIONS[i];
                    n += 1;
                }
            }
            if SHELL_NOTIF != 0 {
                WAIT_HANDLES[n as usize] = SHELL_NOTIF;
                n += 1;
            }
            if let Some((ch, _)) = powering {
                WAIT_HANDLES[n as usize] = ch;
                n += 1;
            }
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, n, (&raw mut WAIT_RESULTS) as u64, deadline)
        };
        // SAFETY: single-threaded; set once at startup.
        let notif = unsafe { SHELL_NOTIF };
        // A stop asks the windows too, with no dialog, and goes once they have or after
        // `STOP_WAIT_NS`, inside the leader's bound. It overrides a person's end under way. With
        // nothing to ask, or no clock, it goes at once.
        if notif != 0 && stop_asked(notif) {
            let Some(now) = now_ns().filter(|_| manager.is_some()) else { stop() };
            if !matches!(closing.as_ref().map(Closing::asked), Some(Asked::Stop)) {
                if let Some(d) = ending_win.take() {
                    d.close(&mut session);
                }
                if let Some((d, _, _)) = refused.take() {
                    d.close(&mut session);
                }
                kprint(b"desktop-shell: asked to stop; asking every window to close first\n");
                closing = Some(Closing::begin(Asked::Stop, now));
            }
        }
        // What became of a Restart or a Shut down asked for, unless the stop it was for has come:
        // `shutdown` exits once `service-mgr` has begun, so any other exit did not happen.
        if let Some((ch, e)) = powering
            && !matches!(closing.as_ref().map(Closing::asked), Some(Asked::Stop))
        {
            let heard = match libprompt::ipc::recv(ch) {
                Ok(Some((librsproto::views::OP_VIEWS_EXITED, _, _, body))) => {
                    let (code, crashed) = librsproto::views::parse_exited(&body).unwrap_or((1, true));
                    Some((code == 0 && !crashed).then_some(()).ok_or_else(|| Refusal::exited(code, crashed)))
                }
                Ok(_) => None,
                Err(()) => Some(Err(Refusal::Failed(alloc::string::String::from(failed::BROKER_GONE)))),
            };
            match heard {
                None => {}
                Some(Ok(())) => {
                    libprompt::ipc::close(ch);
                    powering = None;
                    Line::new()
                        .s(b"desktop-shell: ")
                        .s(e.title().as_bytes())
                        .s(b" has begun; waiting to be asked to stop")
                        .end();
                }
                Some(Err(r)) => {
                    libprompt::ipc::close(ch);
                    powering = None;
                    show_refusal(&mut session, &mut refused, e, r, window, ending_anchor, &theme, &font);
                }
            }
        }
        // SAFETY: reading our own endpoint and session table.
        unsafe {
            if DESKTOP_SERVE != 0 {
                serve_desktop_endpoint();
            }
            #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
            for i in 0..MAX_DESKTOP_SESSIONS {
                let ch = DESKTOP_SESSIONS[i];
                if ch != 0 {
                    list_dirty |= serve_desktop_session(
                        ch,
                        manager.as_mut(),
                        &mut desktops,
                        &entries,
                        &mut current_desktop,
                        &mut told_desktop,
                        &mut next_desktop_id,
                        &launcher,
                    );
                }
            }
        }
        if let Some(m) = manager.as_mut() {
            // The compositor told of any move the endpoint just made, before a window is drained
            // and recorded on the current desktop.
            sync_current(m, &desktops, current_desktop, &mut told_desktop);
            let ours = [
                window,
                bottom.unwrap_or(0),
                bar.id().unwrap_or(0),
                prompt.as_ref().map_or(0, |c| c.id()),
                wallpaper_window,
                ending_win.as_ref().map_or(0, |c| c.id()),
                refused.as_ref().map_or(0, |(c, _, _)| c.id()),
            ];
            let mut fired = alloc::vec::Vec::new();
            let mut states: alloc::vec::Vec<librsproto::surface::WindowState> = alloc::vec::Vec::new();
            let mut dropped: alloc::vec::Vec<librsproto::surface::MgrDragEnded> = alloc::vec::Vec::new();
            list_dirty |= place_new_windows(
                m,
                &mut next_origin,
                &mut entries,
                &mut last_focus,
                &ours,
                &mut fired,
                &mut layout,
                &mut states,
                &mut dropped,
                &mut restore,
                current_desktop,
            );

            let work = (layout.work_x, layout.work_y, layout.work_w, layout.work_h);
            for d in dropped {
                let a = desktop_shell::snap::drag_ended(&d, work, &mut restore);
                sent_request |= answer(m, d.asked.window, a, b"drop");
            }
            let (sent, dirty) = answer_states(m, states, &mut entries, &mut restore, work);
            sent_request |= sent;
            list_dirty |= dirty;

            list_dirty |= normalize_desktops(&mut desktops, &entries, &mut current_desktop, &mut next_desktop_id);
            for id in fired {
                // `Super+N` names the Nth desktop, not id N: ids stop matching the switcher.
                if (HOTKEY_SWITCH_BASE + 1..=HOTKEY_SWITCH_BASE + CHORD_DESKTOPS).contains(&id) {
                    let n = (id - HOTKEY_SWITCH_BASE) as usize;
                    if let Some(d) = desktops.get(n - 1) {
                        let to = d.id;
                        if switch_desktop(m, &desktops, &mut current_desktop, &mut told_desktop, to) {
                            sent_request = true;
                            list_dirty = true;
                            normalize_desktops(&mut desktops, &entries, &mut current_desktop, &mut next_desktop_id);
                            overview_dirty = overview.is_some();
                        }
                    }
                    continue;
                }
                if (HOTKEY_MOVE_BASE + 1..=HOTKEY_MOVE_BASE + CHORD_DESKTOPS).contains(&id) {
                    let n = (id - HOTKEY_MOVE_BASE) as usize;
                    let Some(to) = desktops.get(n - 1).map(|d| d.id) else { continue };
                    if let Some(e) = entries.iter_mut().find(|e| e.focused) {
                        let wid = e.id;
                        sent_request = true;
                        if window_value(m, OP_MGR_SET_WINDOW_DESKTOP, wid, to) {
                            e.desktop = to;
                            Line::new()
                                .s(b"desktop-shell: moved window ")
                                .u(wid as u64)
                                .s(b" to ")
                                .s(desktop_label(&desktops, to).as_bytes())
                                .end();
                            normalize_desktops(&mut desktops, &entries, &mut current_desktop, &mut next_desktop_id);
                            list_dirty = true;
                        } else {
                            kprint(b"desktop-shell: SetWindowDesktop was refused\n");
                        }
                    }
                    continue;
                }
                if id == HOTKEY_RENAME {
                    // The prompt is a popup, as the menus are: a `panel` takes no keyboard focus.
                    // Only when nothing else is up, or two popups would each believe the next
                    // keystroke is theirs.
                    if prompt.is_none() && bar.win.is_none() {
                        name.clear();
                        prompt = open_prompt(&mut session, window, bottom, &name, &theme, &font, screen);
                        if let Some(c) = prompt.as_ref() {
                            stick(m, c.id(), b"the name prompt");
                            kprint(b"desktop-shell: naming this desktop\n");
                        }
                    }
                    continue;
                }
                if id == HOTKEY_APPS {
                    // The keyboard's way in, and the only one without a pointer. A second press
                    // puts away whatever popup is up, and each says which it closed, because a
                    // dismissal read as the wrong one sends a later gate looking for the other
                    // name.
                    if prompt.is_some() {
                        close_prompt(&mut session, &mut prompt, &mut name);
                    } else if bar.win.is_some() {
                        bar.close(&mut session);
                    } else {
                        bar.state.toggle(panel::APPS);
                        if let Some(id) = bar.sync(&mut session, window, &theme, &font) {
                            stick(m, id, b"the applications menu");
                        }
                    }
                    continue;
                }
                // Bounded by what the bar is showing: the first `task_capacity` on the current
                // desktop, not of the global list. An entry past it is neither drawn nor
                // clickable, so minimizing it would leave no way back.
                let cap = task_capacity(&desktops, &entries, current_desktop, &theme, &font, screen);
                let shown_now: alloc::vec::Vec<u32> =
                    visible_entries(&entries, current_desktop, cap).iter().map(|e| e.id).collect();
                if id == HOTKEY_MINIMIZE
                    && let Some(e) = entries.iter_mut().find(|e| shown_now.contains(&e.id) && e.focused && !e.minimized)
                {
                    let wid = e.id;
                    sent_request = true;
                    if minimize_window(m, e) {
                        Line::new().s(b"desktop-shell: Super+H minimized window ").u(wid as u64).end();
                        list_dirty = true;
                    }
                }
            }
        }
        if let Some(m) = manager.as_mut() {
            sync_current(m, &desktops, current_desktop, &mut told_desktop);
        }
        if session.pump().is_err() {
            fail(b"desktop-shell: compositor connection lost\n");
        }
        if !hotkey_done && let Some(m) = manager.as_mut() {
            hotkey_done = true;
            sent_request = true;
            use librsproto::surface::{MOD_META, MOD_SHIFT, MgrHotkey, OP_MGR_REGISTER_HOTKEY};
            let hk = MgrHotkey { id: HOTKEY_MINIMIZE, mods: MOD_META, code: KEY_H };
            for n in 1..=CHORD_DESKTOPS {
                let code = KEY_1 + (n as u16 - 1);
                for (base, mods) in [(HOTKEY_SWITCH_BASE, MOD_META), (HOTKEY_MOVE_BASE, MOD_META | MOD_SHIFT)] {
                    let chord = MgrHotkey { id: base + n, mods, code };
                    let mut b = [0u8; core::mem::size_of::<MgrHotkey>()];
                    if chord.write(&mut b).is_none() {
                        kprint(b"desktop-shell: a desktop chord would not serialise\n");
                        continue;
                    }
                    let mut reply = [0u8; 64];
                    if m.request(OP_MGR_REGISTER_HOTKEY, &b, None, &mut reply).is_err() {
                        Line::new()
                            .s(b"desktop-shell: registering a desktop chord was refused (id ")
                            .u((base + n) as u64)
                            .s(b")")
                            .end();
                    }
                }
            }
            let rename_hk = MgrHotkey { id: HOTKEY_RENAME, mods: MOD_META, code: KEY_R };
            let mut rb = [0u8; core::mem::size_of::<MgrHotkey>()];
            if rename_hk.write(&mut rb).is_some() {
                let mut reply = [0u8; 64];
                if m.request(OP_MGR_REGISTER_HOTKEY, &rb, None, &mut reply).is_err() {
                    kprint(b"desktop-shell: registering Super+R was refused\n");
                }
            } else {
                kprint(b"desktop-shell: the Super+R registration would not serialise\n");
            }
            // `Super+A`, not a tap of `Super`: every chord here begins with `Super` going down.
            let apps_hk = MgrHotkey { id: HOTKEY_APPS, mods: MOD_META, code: KEY_A };
            let mut ab = [0u8; core::mem::size_of::<MgrHotkey>()];
            if apps_hk.write(&mut ab).is_some() {
                let mut reply = [0u8; 64];
                if m.request(OP_MGR_REGISTER_HOTKEY, &ab, None, &mut reply).is_err() {
                    kprint(b"desktop-shell: registering Super+A was refused\n");
                }
            } else {
                // A body that would not serialise would leave a gate waiting on a line nobody
                // printed.
                kprint(b"desktop-shell: the Super+A registration would not serialise\n");
            }
            Line::new()
                .s(b"desktop-shell: Super+1..")
                .u(CHORD_DESKTOPS as u64)
                .s(b" switches, Super+Shift+N moves, Super+R names, Super+A opens applications")
                .end();

            let mut body = [0u8; core::mem::size_of::<MgrHotkey>()];
            if hk.write(&mut body).is_none() {
                kprint(b"desktop-shell: a RegisterHotkey body would not serialise\n");
            } else {
                let mut reply = [0u8; 64];
                if m.request(OP_MGR_REGISTER_HOTKEY, &body, None, &mut reply).is_ok() {
                    kprint(b"desktop-shell: Super+H minimizes the focused window\n");
                } else {
                    kprint(b"desktop-shell: registering Super+H was refused\n");
                }
            }
        }

        while let Some((w, event)) = session.next_event() {
            if bar.id() == Some(w) {
                // A press elsewhere dismisses it, and so does losing the keyboard: a press on the
                // desktop or a panel raises nothing, so `Focus(false)` alone misses it.
                // `InputLost` is queue overflow, and reading it as either would close the menu on
                // a burst of motion.
                if matches!(event, libsurface::WindowEvent::Dismissed | libsurface::WindowEvent::Focus(false)) {
                    bar.close(&mut session);
                    continue;
                }
                let mut chosen: Option<MenuMsg> = None;
                let mut finished = false;
                if let libsurface::WindowEvent::Key(k) = event {
                    let table = bar.table(&theme);
                    match bar.state.key(&k, &table) {
                        KeyOutcome::Chose { menu, item } => {
                            chosen = table.get(menu).and_then(|m| m.items.get(item)).and_then(|it| match it {
                                Item::Action { msg, enabled: true, .. } => Some(*msg),
                                _ => None,
                            });
                            finished = true;
                        }
                        KeyOutcome::Dismissed => finished = true,
                        KeyOutcome::Changed => {
                            if let (Some(id), Some(m)) =
                                (bar.sync(&mut session, window, &theme, &font), manager.as_mut())
                            {
                                stick(m, id, bar.what().as_bytes());
                            }
                        }
                        KeyOutcome::Ignored => {
                            if bar.state.open() == Some(panel::APPS)
                                && k.pressed != 0
                                && bar.query.apply(k.keycode, k.modifiers, k.locks)
                            {
                                let table = bar.table(&theme);
                                bar.state.select_first(&table);
                                bar.redraw(&mut session, &theme, &font);
                                // A receipt per character, and a count: `check-login` types into
                                // the filter, so a lost keystroke must be visible, and what is
                                // typed is a program someone is about to run, so the number is
                                // logged, not the text.
                                let listed = table.get(panel::APPS).map_or(0, |m| {
                                    m.items.iter().filter(|it| matches!(it, Item::Action { enabled: true, .. })).count()
                                });
                                Line::new().s(b"desktop-shell: applications menu listing ").u(listed as u64).end();
                            }
                        }
                    }
                }
                if let libsurface::WindowEvent::Pointer(_) = event {
                    let msgs = bar.route(&font, &theme, &event);
                    chosen = msgs.first().copied();
                    if chosen.is_some() {
                        finished = true;
                    } else {
                        bar.redraw(&mut session, &theme, &font);
                    }
                }
                match chosen {
                    Some(MenuMsg::Launch(i)) => {
                        if let Some(app) = programs.get(i) {
                            launcher.launch(app.exec.as_str(), &[]);
                        }
                    }
                    Some(MenuMsg::Place(i)) => {
                        if let Some(p) = places.get(i) {
                            Line::new().s(b"desktop-shell: opening place ").s(p.name.as_bytes()).end();
                            launcher.launch(FOLDER_OPENER, &[p.path.as_str()]);
                        }
                    }
                    // The power menu: begin ending the session; a second choice while one is
                    // under way changes nothing. A Restart or a Shut down the policy would refuse
                    // is refused here, before a window is asked to close: found after the windows
                    // had gone, it would have cost the person them for nothing.
                    Some(MenuMsg::End(ending)) => {
                        if closing.is_none() && powering.is_none() {
                            if let Some((d, _, _)) = refused.take() {
                                d.close(&mut session);
                            }
                            Line::new().s(b"desktop-shell: ending the session: ").s(ending.title().as_bytes()).end();
                            let refusal = ending.power_args().and_then(|_| power_refusal(session_ns));
                            match refusal {
                                Some(r) => show_refusal(
                                    &mut session,
                                    &mut refused,
                                    ending,
                                    r,
                                    window,
                                    ending_anchor,
                                    &theme,
                                    &font,
                                ),
                                None => closing = Some(Closing::begin(Asked::Person(ending), now_ns().unwrap_or(0))),
                            }
                        }
                    }
                    Some(MenuMsg::Nothing) | None => {}
                }
                if finished {
                    bar.close(&mut session);
                }
                continue;
            }
            // The waiting dialog's own input: End anyway, Cancel, its close button, or Escape. A
            // press elsewhere does not dismiss it: the person presses elsewhere to answer an
            // application's question.
            if ending_win.as_ref().map(|c| c.id()) == Some(w) {
                let mut chose = None;
                match &event {
                    libsurface::WindowEvent::Key(k) if k.pressed != 0 && k.keycode == KEY_ESC => {
                        chose = Some(EndingMsg::Cancel);
                    }
                    libsurface::WindowEvent::Pointer(_) => {
                        if let (Some(d), Some(c)) = (ending_win.as_mut(), closing.as_ref()) {
                            let view = ending_view(c, d.hovered_key(), &theme);
                            chose = d.route(&view, &font, &theme, &event).first().copied();
                            if chose.is_none() {
                                let view = ending_view(c, d.hovered_key(), &theme);
                                d.present(&mut session, &view, &font, &theme);
                            }
                        }
                    }
                    _ => {}
                }
                match chose {
                    Some(EndingMsg::Cancel) => {
                        if let Some(d) = ending_win.take() {
                            d.close(&mut session);
                        }
                        closing = None;
                        kprint(b"desktop-shell: ending the session cancelled; it goes on\n");
                    }
                    Some(EndingMsg::EndAnyway) => {
                        if let (Some(c), Some(m)) = (closing.as_mut(), manager.as_mut()) {
                            let left = c.end_anyway();
                            Line::new()
                                .s(b"desktop-shell: ending anyway, closing ")
                                .u(left.len() as u64)
                                .s(b" window(s)")
                                .end();
                            for id in left {
                                insist_on_close(m, id);
                            }
                            sent_request = true;
                        }
                    }
                    Some(EndingMsg::Nothing) | None => {}
                }
                continue;
            }
            if refused.as_ref().map(|(c, _, _)| c.id()) == Some(w) {
                let mut close_it = matches!(event, libsurface::WindowEvent::Dismissed);
                match &event {
                    libsurface::WindowEvent::Key(k)
                        if k.pressed != 0 && (k.keycode == KEY_ESC || k.keycode == KEY_ENTER) =>
                    {
                        close_it = true;
                    }
                    libsurface::WindowEvent::Pointer(_) => {
                        if let Some((d, e, r)) = refused.as_mut() {
                            let view = panel::refusal_dialog(*e, r, d.hovered_key(), &theme);
                            let chose = d.route(&view, &font, &theme, &event).first().copied();
                            close_it = chose == Some(RefusalMsg::Close);
                            if chose.is_none() {
                                let view = panel::refusal_dialog(*e, r, d.hovered_key(), &theme);
                                d.present(&mut session, &view, &font, &theme);
                            }
                        }
                    }
                    _ => {}
                }
                if close_it && let Some((d, _, _)) = refused.take() {
                    d.close(&mut session);
                    kprint(b"desktop-shell: refusal dialog closed\n");
                }
                continue;
            }
            if prompt.as_ref().map(|c| c.id()) == Some(w) {
                if matches!(event, libsurface::WindowEvent::Dismissed | libsurface::WindowEvent::Focus(false)) {
                    close_prompt(&mut session, &mut prompt, &mut name);
                    continue;
                }
                if let libsurface::WindowEvent::Key(k) = event
                    && k.pressed != 0
                {
                    if k.keycode == KEY_ESC {
                        close_prompt(&mut session, &mut prompt, &mut name);
                    } else if k.keycode == KEY_ENTER {
                        // Naming is what makes a desktop persist. Capped at what the wire can
                        // carry: `write_list` refuses a whole `List` reply rather than truncating
                        // a name.
                        let full = name.text();
                        let cut = &full[..full
                            .char_indices()
                            .map(|(i, c)| i + c.len_utf8())
                            .take_while(|&e| e <= librsproto::desktop::MAX_DESKTOP_NAME)
                            .last()
                            .unwrap_or(0)];
                        let chosen = alloc::string::String::from(cut);
                        if let Some(d) = desktops.iter_mut().find(|d| d.id == current_desktop) {
                            d.name.clear();
                            d.name.push_str(&chosen);
                        }
                        Line::new().s(b"desktop-shell: named this desktop ").untrusted(chosen.as_bytes()).end();
                        list_dirty = true;
                        close_prompt(&mut session, &mut prompt, &mut name);
                        normalize_desktops(&mut desktops, &entries, &mut current_desktop, &mut next_desktop_id);
                    } else if name.apply(k.keycode, k.modifiers, k.locks) {
                        // A receipt per character: a dropped PS/2 batch eats a keystroke
                        // unacknowledged. A name is a label the person sees, so it is logged as
                        // text.
                        Line::new().s(b"desktop-shell: name so far ").untrusted(name.text().as_bytes()).end();
                        if let Some(c) = prompt.as_mut() {
                            c.present(&mut session, &panel::name_prompt(&name, &theme), &font, &theme);
                        }
                    }
                }
                continue;
            }
            if Some(w) == overview {
                if let libsurface::WindowEvent::Key(k) = event
                    && k.pressed != 0
                    && k.keycode == KEY_ESC
                {
                    close_overview(&mut session, &mut overview, &mut shots, &mut dragging, &mut over_addrs, screen);
                    continue;
                }
                if let libsurface::WindowEvent::Pointer(p) = event
                    && p.kind == librsproto::surface::POINTER_BUTTON
                {
                    let pressed = p.flags & librsproto::surface::POINTER_PRESSED != 0;
                    if pressed {
                        dragging = window_in_card_at(p.x, p.y, &desktops, &entries, screen);
                        if let Some(id) = dragging {
                            Line::new().s(b"desktop-shell: dragging window ").u(id as u64).end();
                        }
                    } else {
                        // Release: which of three gestures this was. A press always picks a
                        // thumbnail up, so where the release lands decides:
                        //
                        // - over a sidebar row, holding a thumbnail: move that window there;
                        // - over the same thumbnail it started on: a click on a window, so
                        //   activate it;
                        // - over a sidebar row, holding nothing: a click on a desktop, so go
                        //   there.
                        let picked = dragging.take();
                        let row = desktop_shell::card_at(p.x, p.y, desktops.len(), screen);
                        let under = window_in_card_at(p.x, p.y, &desktops, &entries, screen);
                        // A window's box is inside a card, so the click arm is tried first, or a
                        // click would become a move to the desktop the window is already on.
                        let same_desktop = picked
                            .and_then(|wid| entries.iter().find(|e| e.id == wid))
                            .zip(row)
                            .is_some_and(|(e, i)| e.desktop == desktops[i].id);
                        match (picked, row) {
                            (Some(wid), _) if under == Some(wid) => {
                                let home = entries.iter().find(|e| e.id == wid).map(|e| e.desktop);
                                if let Some(m) = manager.as_mut()
                                    && let Some(home) = home
                                {
                                    sent_request = true;
                                    // Not switched when already there. No `normalize_desktops`
                                    // after: the target holds this window.
                                    let there = home == current_desktop
                                        || switch_desktop(m, &desktops, &mut current_desktop, &mut told_desktop, home);
                                    if there
                                        && let Some(e) = entries.iter_mut().find(|e| e.id == wid)
                                        && raise_window(m, e)
                                    {
                                        list_dirty = true;
                                        Line::new().s(b"desktop-shell: overview raised window ").u(wid as u64).end();
                                        close_overview(
                                            &mut session,
                                            &mut overview,
                                            &mut shots,
                                            &mut dragging,
                                            &mut over_addrs,
                                            screen,
                                        );
                                    }
                                }
                            }
                            (Some(_), Some(_)) if same_desktop => {}
                            (Some(wid), Some(i)) => {
                                if let Some(m) = manager.as_mut() {
                                    let to = desktops[i].id;
                                    sent_request = true;
                                    if window_value(m, OP_MGR_SET_WINDOW_DESKTOP, wid, to) {
                                        if let Some(e) = entries.iter_mut().find(|e| e.id == wid) {
                                            e.desktop = to;
                                        }
                                        Line::new()
                                            .s(b"desktop-shell: dropped window ")
                                            .u(wid as u64)
                                            .s(b" on ")
                                            .s(desktop_label(&desktops, to).as_bytes())
                                            .end();
                                        normalize_desktops(
                                            &mut desktops,
                                            &entries,
                                            &mut current_desktop,
                                            &mut next_desktop_id,
                                        );
                                        list_dirty = true;
                                        close_overview(
                                            &mut session,
                                            &mut overview,
                                            &mut shots,
                                            &mut dragging,
                                            &mut over_addrs,
                                            screen,
                                        );
                                    }
                                }
                            }
                            (None, Some(i)) if desktops[i].id == current_desktop => {
                                close_overview(
                                    &mut session,
                                    &mut overview,
                                    &mut shots,
                                    &mut dragging,
                                    &mut over_addrs,
                                    screen,
                                );
                            }
                            (None, Some(i)) => {
                                if let Some(m) = manager.as_mut() {
                                    let to = desktops[i].id;
                                    sent_request = true;
                                    if switch_desktop(m, &desktops, &mut current_desktop, &mut told_desktop, to) {
                                        list_dirty = true;
                                        normalize_desktops(
                                            &mut desktops,
                                            &entries,
                                            &mut current_desktop,
                                            &mut next_desktop_id,
                                        );
                                        overview_dirty = true;
                                    }
                                }
                            }
                            // A click on the overview's background dismisses it; it covers the
                            // bar, so a second click on the desktop's name lands here.
                            (None, None) => {
                                close_overview(
                                    &mut session,
                                    &mut overview,
                                    &mut shots,
                                    &mut dragging,
                                    &mut over_addrs,
                                    screen,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                continue;
            }
            // The top bar's two words, routed through the toolkit: a click opens its menu, or
            // closes it if it is the one open. A click on the open menu's own word reopens it:
            // the press dismisses the popup and the release is a click on a word whose menu is
            // shut.
            if w == window {
                let view = panel::top_bar(&shown_clock, bar.state.open(), top.hovered_key(), &theme);
                top.present(&mut session, &view, &font, &panel_theme);
                for msg in top.route(&view, &font, &panel_theme, &event) {
                    let TopMsg::Menu(i) = msg;
                    bar.state.toggle(i);
                    if let (Some(id), Some(m)) = (bar.sync(&mut session, window, &theme, &font), manager.as_mut()) {
                        stick(m, id, bar.what().as_bytes());
                    }
                }
                continue;
            }
            if Some(w) == bottom
                && let Some(bb) = bottom_bar.as_mut()
            {
                let lit = shown.as_ref().is_some_and(|s| s.desktop == current_desktop);
                let view =
                    bottom_view(&entries, &desktops, current_desktop, lit, bb.hovered_key(), &theme, &font, screen);
                bb.present(&mut session, &view, &font, &panel_theme);
                // The middle button closes; a message cannot say which button made it, so the
                // event is asked.
                let middle = matches!(
                    event,
                    libsurface::WindowEvent::Pointer(p) if p.button == libkern::abi::BTN_MIDDLE
                );
                for msg in bb.route(&view, &font, &panel_theme, &event) {
                    let Some(m) = manager.as_mut() else { continue };
                    match msg {
                        BottomMsg::Task(wid) => {
                            let Some(e) = entries.iter_mut().find(|e| e.id == wid) else { continue };
                            sent_request = true;
                            if middle {
                                ask_to_close(m, wid, &mut asked_to_close);
                                continue;
                            }
                            if e.focused && !e.minimized {
                                if minimize_window(m, e) {
                                    Line::new().s(b"desktop-shell: minimized window ").u(wid as u64).end();
                                }
                            } else if raise_window(m, e) {
                                Line::new().s(b"desktop-shell: raised window ").u(wid as u64).end();
                            } else {
                                Line::new().s(b"desktop-shell: raising window ").u(wid as u64).s(b" was refused").end();
                            }
                            list_dirty = true;
                        }
                        _ if middle => {}
                        BottomMsg::ShowDesktop => {
                            sent_request = true;
                            let cap = task_capacity(&desktops, &entries, current_desktop, &theme, &font, screen);
                            show_desktop(m, &mut entries, current_desktop, cap, &mut shown);
                            list_dirty = true;
                        }
                        BottomMsg::Desktop(_) | BottomMsg::Previous | BottomMsg::Next => {
                            let at = desktops.iter().position(|d| d.id == current_desktop).unwrap_or(0);
                            let to = match msg {
                                BottomMsg::Desktop(i) => i,
                                BottomMsg::Previous => at.saturating_sub(1),
                                _ => at + 1,
                            };
                            let Some(to) = desktops.get(to).map(|d| d.id) else { continue };
                            if to == current_desktop {
                                continue;
                            }
                            sent_request = true;
                            if switch_desktop(m, &desktops, &mut current_desktop, &mut told_desktop, to) {
                                list_dirty = true;
                                normalize_desktops(&mut desktops, &entries, &mut current_desktop, &mut next_desktop_id);
                                overview_dirty = overview.is_some();
                            }
                        }
                        BottomMsg::Overview => {
                            if overview.is_some() {
                                continue;
                            }
                            sent_request = true;
                            let cap = task_capacity(&desktops, &entries, current_desktop, &theme, &font, screen);
                            recapture(m, &entries, current_desktop, &mut shots, cap, screen);
                            overview = open_overview(
                                &mut session,
                                window,
                                &theme,
                                &font,
                                &shots,
                                &desktops,
                                current_desktop,
                                &entries,
                                &mut over_addrs,
                                wallpaper.as_ref().map(|w| w.picture.as_slice()),
                                screen,
                            );
                            if let Some(id) = overview {
                                stick(m, id, b"the overview");
                                Line::new()
                                    .s(b"desktop-shell: overview open, window ")
                                    .u(id as u64)
                                    .s(b" showing ")
                                    .u(shots.len() as u64)
                                    .s(b" of ")
                                    .u(desktops.len() as u64)
                                    .s(b" desktops")
                                    .end();
                            }
                        }
                    }
                }
                continue;
            }
        }
        if let Some(m) = manager.as_mut() {
            sync_current(m, &desktops, current_desktop, &mut told_desktop);
        }
        asked_to_close.retain(|(id, _)| entries.iter().any(|e| e.id == *id));

        // The clock, when the minute it shows has changed. Compared as text: an unset clock
        // formats to nothing every time, so it repaints zero times, not once a minute.
        if let Some(now) = now_ns()
            && now >= next_tick
        {
            next_tick = next_minute(now);
            let want = clock_text();
            if want != shown_clock {
                shown_clock = want;
            }
        }
        let view = panel::top_bar(&shown_clock, bar.state.open(), top.hovered_key(), &theme);
        if !top.present(&mut session, &view, &font, &panel_theme) {
            kprint(b"desktop-shell: top bar Commit failed\n");
        }

        // Show-desktop lets go the moment anything on that desktop is back, but not a window the
        // press could not reach (`ShownDesktop::holds`): the second press means "bring back what
        // the first put away", which is no longer coherent once the desktop is not what the first
        // press left.
        if let Some(s) = shown.as_ref()
            && !s.holds(&windows_of(&entries))
        {
            shown = None;
            list_dirty = true;
        }
        if list_dirty {
            let cap = task_capacity(&desktops, &entries, current_desktop, &theme, &font, screen);
            let listed = visible_entries(&entries, current_desktop, cap);
            log_window_list(&listed, &desktop_label(&desktops, current_desktop), desktops.len());
            list_dirty = false;
        }
        present_bottom(
            &mut session,
            bottom_bar.as_mut(),
            &entries,
            &desktops,
            current_desktop,
            &shown,
            &theme,
            &panel_theme,
            &font,
            screen,
        );

        if overview_dirty {
            overview_dirty = false;
            if let Some(id) = overview
                && let Some(m) = manager.as_mut()
            {
                sent_request = true;
                let cap = task_capacity(&desktops, &entries, current_desktop, &theme, &font, screen);
                recapture(m, &entries, current_desktop, &mut shots, cap, screen);
                present_overview(
                    &mut session,
                    id,
                    &theme,
                    &font,
                    &shots,
                    &desktops,
                    current_desktop,
                    &entries,
                    &over_addrs,
                    wallpaper.as_ref().map(|w| w.picture.as_slice()),
                    screen,
                );
                Line::new()
                    .s(b"desktop-shell: overview now showing ")
                    .u(shots.len() as u64)
                    .s(b" on ")
                    .s(desktop_label(&desktops, current_desktop).as_bytes())
                    .end();
            }
        }
    }
}

/// The monotonic clock, in nanoseconds, or `None` if it will not answer.
///
/// Every caller treats `None` as nothing to do this round, not zero: a deadline nothing can
/// evaluate is a wait that returns immediately for ever, a spin harmful machine-wide.
fn now_ns() -> Option<u64> {
    libkern::clock_read(libkern::abi::CLOCK_MONOTONIC).ok()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"desktop-shell: PANIC\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
