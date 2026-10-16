//! `desktop-shell` — the graphical session's leader, and the compositor's first real manager.
//!
//! **What M6 built for a manager has been exercised by a test client until now.** Placement,
//! restacking, focus, the initial-configure hold and the five manager events all have gates,
//! and every one of those gates is `ui-testclient` pretending. This is the process they were
//! for. (Of the five events, four are now consumed here as well; `WindowDestroyed` has no gate
//! that reaches it, because no gate closes a `normal` window in a session.)
//!
//! It draws a **top bar** across the screen and, since M8 Part C, a **bottom bar** carrying one
//! entry per `normal` window — click to raise, click the focused one to minimize, `Super+H` to
//! minimize without reaching for the bar. Both reserve space with a `panel` strut so ordinary
//! windows do not sit under them.
//!
//! **It does not bind its own endpoint**, which is what reconciles a process that both serves
//! and constructs with `syscaps.md` (`graphical-session.md` §3): it holds `BIND_NAMESPACE` to
//! construct *application* namespaces continuously, not to register itself once.
//!
//! Since M8 Part F it **serves `/dev/desktop`** and binds it into every application namespace
//! it constructs — not into the session namespace, which is its own and where nothing would
//! resolve it. A `desktop` command in `/bin` is the consumer that proves the binding is
//! reachable, which had to exist because a process cannot verify a binding of *itself* by
//! using it: a resolve is forwarded to whoever serves the path.
//!
//! `#![no_std]` + `#![no_main]`, with `alloc` — the toolkit builds an element tree per frame.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;
use libkern::debug::Line;
use libkern::*;

/// `EV_KEY` code for Escape — the name prompt's and the overview's dismissal.
const KEY_ESC: u16 = 1;
/// `EV_KEY` code for Enter — the name prompt's answer.
const KEY_ENTER: u16 = 28;
/// `EV_KEY` code for `h`, the minimize chord's key.
///
/// Named here rather than taken from `libkern::abi`, which publishes the keys the *ABI* needs
/// (modifiers, the function row, the keypad) and not every letter. A shell binding a chord is
/// the first thing that wants one.
const KEY_H: u16 = 35;

/// The shell's id for its minimize chord. Manager-chosen, and never zero.
const HOTKEY_MINIMIZE: u32 = 1;
/// `EV_KEY` code for `r`, the rename chord's key.
const KEY_R: u16 = 19;
/// The shell's id for its rename chord.
const HOTKEY_RENAME: u32 = 2;
/// `EV_KEY` code for `a`, the applications chord's key.
const KEY_A: u16 = 30;
/// The shell's id for the chord that opens the Applications menu.
const HOTKEY_APPS: u32 = 3;
/// How many desktops the number chords reach.
///
/// **Four, not nine.** `MAX_HOTKEYS` is sixteen and each desktop costs two chords — one to
/// switch, one to move — so nine would leave no room for the minimize and rename chords here,
/// let alone Part E's overview. Desktops past the fourth exist and hold windows; they are
/// reached by the switcher and the overview rather than by a chord.
const CHORD_DESKTOPS: u32 = 4;
/// Ids `HOTKEY_SWITCH_BASE + n` switch to desktop `n`; `HOTKEY_MOVE_BASE + n` move to it.
const HOTKEY_SWITCH_BASE: u32 = 10;
/// See [`HOTKEY_SWITCH_BASE`].
const HOTKEY_MOVE_BASE: u32 = 20;
/// `EV_KEY` code for `1`. `2`, `3` and `4` follow it.
const KEY_1: u16 = 2;
use desktop_shell::ending::{Asked, Closing, Ending, Next, POWER_PROGRAM, POWER_VIEW, Refusal, failed};
use desktop_shell::panel::{self, BottomMsg, EndingMsg, MenuMsg, RefusalMsg, TopMsg};
use desktop_shell::{
    Application, BAR_H, CARD_BORDER, CARD_CAPTION_GAP, CARD_CAPTION_H, Screen, ShownDesktop, parse_entry,
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
mod desktop_sessions;
mod menus;
mod overview;
mod placement;
mod wallpaper;
mod windows;

use apps::*;
use desktop_sessions::*;
use menus::*;
use overview::*;
use placement::*;
use wallpaper::*;
use windows::*;

/// `alloc` backing: the toolkit builds an element tree per frame.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The wall clock as the top bar shows it, or empty when there is none.
///
/// **`YYYY-MM-DD HH:MM`, and no month names** (M11 Part E batch 9). There is no timezone database
/// and no locale, so a localised form would be a fiction — the same reason `date` emits fields and
/// prints UTC. It also means the bar and the command agree about what time it is, which is worth
/// more here than looking like somebody else's desktop.
///
/// **Empty when the clock is unset**, rather than 1970. The kernel reports `Unsupported` when the
/// RTC could not be read at boot rather than inventing an epoch, and a bar that showed a
/// fabricated date would be undoing that decision one layer up.
fn clock_text() -> alloc::string::String {
    let mut nanos = 0u64;
    // SAFETY: `&mut nanos` is a valid writable u64 out-param for the clock read.
    let r =
        unsafe { libkern::syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_REALTIME, (&raw mut nanos) as u64) };
    if r < 0 {
        return alloc::string::String::new();
    }
    let c = libtime::civil_from_unix(nanos);
    // `format_civil` is `YYYY-MM-DD HH:MM:SS`; a bar that counted seconds would repaint sixty
    // times a minute to show something nobody reads at that resolution.
    let full = libtime::format_civil(&c);
    alloc::string::String::from(full.get(..16).unwrap_or(full.as_str()))
}

/// When the displayed minute next changes, as a monotonic deadline.
///
/// **Aligned to the minute rather than "a minute from now"**, so the bar changes when the clock
/// does. The alignment is read from the *wall* clock and the deadline is *monotonic*, which is
/// not a mix-up: one says how far into the minute we are, the other is what `sys_wait` counts.
fn next_minute(now: u64) -> u64 {
    let mut nanos = 0u64;
    // SAFETY: as above.
    let r =
        unsafe { libkern::syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_REALTIME, (&raw mut nanos) as u64) };
    const MINUTE: u64 = 60 * 1_000_000_000;
    if r < 0 {
        // No clock to follow. Wake in a minute anyway, so a clock that is set later appears
        // without a restart.
        return now.saturating_add(MINUTE);
    }
    now.saturating_add(MINUTE - nanos % MINUTE)
}

/// How many buffers the bar attaches.
const BUFFERS: usize = 2;

/// Write one line to the debug console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// **A shutdown asked this shell to stop** (administration Part E.4c): its session's supervisor
/// passed the request on. Since Part F.2 the windows are asked to close first — the loop's
/// `Asked::Stop`, bounded at `ending::STOP_WAIT_NS` with no dialog — and this is the exit once
/// they have, or the bound has run out, or there was nothing to ask.
fn stop() -> ! {
    kprint(b"desktop-shell: asked to stop, exiting\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 0, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// Read every notification queued on `notif`, and say whether a terminate request was among them.
/// The rest are dropped: this shell reaps nothing, so the `ChildExited` of what it launched is of
/// no use to it.
fn stop_asked(notif: u64) -> bool {
    let mut asked = false;
    loop {
        // SAFETY: NOTIF is a valid 64-byte writable out-param.
        if unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) } != 0 {
            return asked;
        }
        // SAFETY: the kernel wrote a Notification into NOTIF.
        asked |= unsafe { (&raw const NOTIF.kind).read() } == libkern::abi::KIND_TERMINATE_REQUESTED;
    }
}

/// Report and exit.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// `theme`, with a bar's ground in place of a window's.
///
/// One place, so the two bars cannot disagree about what a panel is made of.
///
/// **`theme.panel`, since the desktop refresh's Part C.** It was `face` — "a bar is not paper: it
/// is a face" (M11 Part E) — until the design gave the bars a colour of their own, which Part A
/// added as a key and this is the first use of: lighter than a button's face in the light scheme
/// and the darkest surface on the screen in the dark one.
fn panel(theme: &Theme) -> Theme {
    Theme { background: theme.panel, ..*theme }
}

/// A string field of an environment record, by name.
fn env_str<'a>(env: &'a libstream::wire::Record, name: &str) -> Option<&'a str> {
    env.schema.fields.iter().position(|f| f.name == name).and_then(|i| env.values.get(i)).and_then(|v| v.as_str())
}

/// Allocate a shared memory object of `len` bytes and map it writable.
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

/// Receive and discard one message, waiting for it. `false` if the wait or receive failed.
fn recv_message(ch: u64) -> bool {
    if !wait_one(ch) {
        return false;
    }
    // SAFETY: valid recv out-params.
    let r = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    r == 0
}

/// Receive one message and return its first transferred handle, or `0`.
fn recv_handle(ch: u64) -> u64 {
    if !recv_message(ch) {
        return 0;
    }
    // SAFETY: the kernel wrote `RECV_COUNT` transferred handles into `RECV_HANDLES`.
    unsafe { if RECV_COUNT == 0 { 0 } else { RECV_HANDLES[0] } }
}

/// Block until `h` is signalled.
fn wait_one(h: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers, and `WAIT_RESULTS` holds one
    // `IoResult` per handle in the set — see its declaration for why that is the requirement.
    let waited = unsafe {
        WAIT_HANDLES[0] = h;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    waited == 1
}

/// Recv buffers for the setup channel.
static mut RECV_MSG: [u8; 4096] = [0; 4096];
/// See [`RECV_MSG`].
static mut RECV_HANDLES: [u64; 8] = [0; 8];
/// See [`RECV_MSG`].
static mut RECV_COUNT: usize = 0;

/// Wait set: the compositor's event channel, the manager channel, `/dev/desktop` and its
/// sessions, the notification channel, where a shutdown's terminate request arrives
/// (administration Part E.4c), and the view broker's while a Restart or a Shut down it started
/// runs (Part F.3).
static mut WAIT_HANDLES: [u64; 5 + MAX_DESKTOP_SESSIONS] = [0; 5 + MAX_DESKTOP_SESSIONS];
/// The shell's notification channel: set once at startup, and waited on with the rest.
static mut SHELL_NOTIF: u64 = 0;
/// Where [`stop_asked`] reads a notification.
static mut NOTIF: Notification = Notification::zeroed();
/// One 24-byte `IoResult` **per handle in the set**, because that is what the kernel writes.
///
/// `sys_wait` takes no length for this buffer: it writes one record for *every signalled*
/// handle (`kernel/src/syscall/table.rs`, `k * size_of::<IoResult>()`), and the caller is
/// required to have room for all of them. This was sized at one record while the handle set
/// grew to seven — so any wake where two handles signalled together wrote 24 bytes past the
/// end of a static. Nothing had noticed because the shell reads only the first record, and the
/// bytes past it land in whatever the linker put next.
///
/// Every other server in this tree already sizes it this way (`compositor`,
/// `logging-service`, `fs-server-ext4`); `session-mgr` waits on exactly one handle, where one
/// record is right. Found by the PR #257 reviewer while reading something else.
static mut WAIT_RESULTS: [u8; 24 * (5 + MAX_DESKTOP_SESSIONS)] = [0; 24 * (5 + MAX_DESKTOP_SESSIONS)];

/// Bootstrap registers, as `libsession::spawn_leader` fills them: `rdi` = notification
/// channel, `rsi` = the **session** namespace, `rdx` = the Tier-1 setup channel carrying
/// `argv` and the environment, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, session_ns: u64, setup: u64, arg0: u64) -> ! {
    kprint(b"desktop-shell: up (graphical session leader)\n");
    // SAFETY: single-threaded; written once, here, before the loop waits on it.
    unsafe { SHELL_NOTIF = notif };

    // Two messages arrive on the setup channel, in order: the Tier-1 `argv` + environment,
    // then the compositor's forwarding endpoint. The second is what lets this process build
    // application namespaces — a `/dev/draw` *binding* resolves to a kernel registration and
    // never back to an endpoint, so the shell cannot re-bind what its own namespace holds.
    // **The shell's own environment, kept rather than discarded**, because every application
    // it launches gets one derived from it — `$env.HOME` and `PATH` should mean the same thing
    // in a window as at a serial prompt.
    let (argv, env) = match libstream::setup::bootstrap(notif, session_ns, setup, arg0).setup() {
        Some(Ok(s)) => (s.argv, s.env),
        _ => (alloc::vec::Vec::new(), libstream::wire::Record::default()),
    };
    // **The theme, read once and carried on the environment every application already gets**
    // (M11 Part C). One reader in the session rather than one per application: a client that
    // opened this file itself would need it in its namespace and would repeat the parse, and a
    // client launched before the file existed would disagree with one launched after.
    let theme = read_theme(session_ns);
    let env = env.with_str_field("THEME", &theme.to_config());
    // `argv[1]` is the user's real home, e.g. `/home/alice` — see `spawn_leader`'s `argv_rest`.
    // Empty means an application's `/home` cannot be scoped, so it is left unbound rather than
    // bound to something wider than the session's own.
    let home: &str = argv.get(1).map(|s| s.as_str()).unwrap_or("");
    if home.is_empty() {
        kprint(b"desktop-shell: no home in argv; applications get no /home\n");
    }
    // **Every server's is `service-mgr`'s route to it** (administration Part E.1b), which reaches
    // that server alone and whichever instance of it is running; `fs` and `profile` are `init`'s
    // servers' own. So what this shell binds into an application reaches a restarted server, and
    // holding them with `BIND_NAMESPACE` reaches nothing a route does not lead to.
    let draw_endpoint = recv_handle(setup);
    let fs_endpoint = recv_handle(setup);
    let tty_endpoint = recv_handle(setup);
    let profile_endpoint = recv_handle(setup);
    // The clipboard server's forwarding endpoint (M12 Part E), for the same reason as the
    // other four: a `/dev/clipboard` *binding* resolves to a kernel registration and never
    // back to an endpoint, so the shell cannot re-bind what its own namespace holds.
    let clipboard_endpoint = recv_handle(setup);
    // The view broker's (administration Part A.4), and `argv[2]`, the session's base: both
    // bound into every application namespace so `with` works in a terminal launched here. Either
    // absent means the session has no `/dev/views`, and applications get none.
    let views_endpoint = recv_handle(setup);
    let views_base: &str = argv.get(2).map(|s| s.as_str()).unwrap_or("");
    // An info-only endpoint of the device manager's (administration Part B.4), bound at
    // `/dev/devices` with the base `/info` in every application namespace — info-only, so that
    // however this shell binds it, nothing reached through it can subscribe to a device. Absent
    // means applications get no device listing.
    let devices_endpoint = recv_handle(setup);
    // The storage service's session endpoint (administration Part C.6), bound at `/storage` and
    // `/dev/storage` in every application namespace. It answers the filesystems and the table and
    // nothing else, so this shell cannot mount anything with it, however it binds it. Absent means
    // applications get no `/storage`.
    let storage_endpoint = recv_handle(setup);
    // `service-mgr`'s services endpoint (administration Part E.2b), bound at `/dev/services` in
    // every application namespace: the table of services, and nothing that starts or stops one.
    let services_endpoint = recv_handle(setup);
    if draw_endpoint == 0 {
        kprint(b"desktop-shell: no compositor endpoint; cannot launch applications\n");
    }
    if fs_endpoint == 0 || tty_endpoint == 0 {
        // Degraded rather than fatal, and specific: an application without fonts draws
        // rectangles, and one without a tty cannot be a terminal. Both are worth naming,
        // because the symptom is a launched program that exits without saying why.
        kprint(b"desktop-shell: applications will have no fonts or no terminal\n");
    }

    // **Resolved from the session namespace, not from a root one.** This process has no root
    // handle: `spawn_leader` runs it in the namespace `desktop-session-mgr` constructed, which
    // is where `/dev/draw` was bound. That is the whole point — an application's namespace
    // will get a *narrower* bind, and the difference is what gates the manager channel.
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

    // **The screen's size, asked** (Phase 5 Part E) — every bar, the wallpaper, the overview and
    // the placement cascade are laid out on it. It was written down as 1280×800 until then, and
    // on a 768-row screen that put the window list below the last row.
    //
    // **Fatal if it cannot be read**, which is narrower than it sounds: the leaf resolves through
    // the same `/dev/draw` binding the connect above just used, so the one way to reach this is a
    // compositor that does not serve it. A shell that guessed a size instead would be the bug this
    // replaced, drawn somewhere a person cannot see.
    // SAFETY: `session_ns` is live for this process's whole run.
    let screen = match unsafe { libsurface::screen::read(session_ns) } {
        Ok(s) => Screen { width: s.width, height: s.height },
        Err(_) => fail(b"desktop-shell: /dev/draw/screen unreadable; cannot lay out the desktop\n"),
    };
    Line::new().s(b"desktop-shell: screen ").u(screen.width as u64).s(b"x").u(screen.height as u64).end();

    // **The wallpaper, before anything else this shell creates** (M12 Part F). Creation order
    // is bottom-first in the compositor's stack, so making it first is what makes it bottom-most
    // — no `Manage::Lower` needed, and nothing this shell raises later can get underneath it.
    let wallpaper = open_wallpaper(session_ns, &mut session, &theme, screen);
    // Read often enough to be worth naming once. `0` is not a window id, so it is the "none"
    // every `ours`-style check already treats as absent.
    let wallpaper_window = wallpaper.as_ref().map_or(0, |w| w.window);

    // `panel`, not `normal`: the role is what reserves the strut, so ordinary windows are
    // placed below the bar rather than under it. M6 Part A built that and nothing but a test
    // client has asked for it.
    //
    // `reserve` is stated separately from the height on purpose — the role's own doc explains
    // that deriving it would make a bar that reserves less than it occupies inexpressible.
    // A bar wants them equal.
    // **The panel's ground and the design's bar** (desktop refresh, Part C): a `Child`, like
    // every application's windows, so its words are routed and light under the pointer rather
    // than hit-tested by hand against a width written down beside them.
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
    // **Said once, because an absent clock is otherwise indistinguishable from a broken bar.**
    // The kernel reports the wall clock as unsupported when the RTC could not be read at boot,
    // and the bar's answer to that is to show nothing — which looks identical to a clock that
    // failed to draw. One line at startup tells the two apart.
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
    // **Build one application namespace and check it**, before anything is launched into it.
    // Every launch builds one the same way; doing it once here is
    // what makes the narrow bind observable — and the shell refusing to launch when the check
    // fails is the behaviour, not the test.
    // **The startup check gates, rather than only reporting.** It used to `kprint` and let
    // `launch` stay reachable, so a shell that knew its namespaces were wrong would launch into
    // them anyway (PR #237 review, finding 3).
    //
    // It runs here, *before* the manager channel is taken, which is the only moment a `manage`
    // resolve gets an honest answer from a namespace that does not bind it.
    // **The desktop endpoint, created before the first application namespace is built.**
    // `build_app_namespace` binds it, and the startup check below verifies it — so it has to
    // exist by then or the check would be verifying its absence.
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

    // **Who the session is for**, read once from the session's own namespace (administration Part
    // F.1): an application gets it as `/session/user`. It does not change while the session lasts.
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
    // **An application's namespace, built and checked before any is launched**, by the builder a
    // launch uses: a shell that cannot build a gated one launches nothing.
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

    // **The bottom bar is created here, beside the top one, and *placed* much later.**
    //
    // It sat after `manage()` in the first version of Part C, on the reasoning that it has to be
    // placed and only a manager can place. That deadlocks the shell against its own manager
    // hold: a `panel` is held for the manager exactly like a `normal` window — only a `popup` is
    // exempt — and `Session::create` blocks until the first `Configure` arrives. So the shell
    // parked inside `create`, unable to drain the manager channel and therefore unable to send
    // the `Place` that would have released it, and only the 200 ms configure deadline broke the
    // tie. Every session start paid it, and `no manager answer for window N` — a line that
    // exists to name a wedged or absent manager — fired on every healthy boot (PR #242 review,
    // blocking 1).
    //
    // `Place` works on an already-created window, so the create belongs here where no manager
    // is attached and nothing is held; only the placement has to wait.
    let mut entries: alloc::vec::Vec<WinEntry> = alloc::vec::Vec::new();
    let mut list_dirty = true;
    // **One desktop to start, which is also the scratch slot** — empty and unnamed, so the
    // first window created lands on it and `normalize_desktops` appends its successor.
    let mut desktops: alloc::vec::Vec<Desktop> = alloc::vec![Desktop { id: 1, name: alloc::string::String::new() }];
    let mut next_desktop_id: u32 = 2;
    let mut current_desktop: u32 = 1;
    // What the compositor was last told is current — its own starting value, 1. See
    // `sync_current` for why this is not always `current_desktop`.
    let mut told_desktop: u32 = 1;
    // **A `Child`, like the top bar** (desktop refresh, Part C): its buttons are routed rather than
    // hit-tested by dividing x by a width written down beside the layout, which a switcher whose
    // width changes with the desktop's name could not have survived.
    //
    // **Created undrawn**, as the hand-built bar it replaced was: the compositor docks a panel at
    // the origin until it is placed, and it cannot be placed until the manager channel below is
    // held — so a frame now would sit over the top bar for the length of that. Its first frame is
    // the loop's first `present_bottom`, after it has been placed.
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
    // **What show-desktop put away, and on which desktop** — the restore set, which is the
    // shell's because minimising is a manager operation and the compositor keeps no policy (the
    // plan's review, finding 6). `None` when the desktop is not being shown.
    let mut shown: Option<ShownDesktop> = None;

    // **The manager channel, which makes this the compositor's first real manager.**
    //
    // Resolved from the session namespace, which binds the `/dev/draw` subtree unscoped and
    // therefore reaches `manage`. An application's namespace binds `/dev/draw/new` alone and
    // does not — that asymmetry is the whole of what closed `manage-ungated`, and holding this
    // channel is the other half of it being a capability rather than a race.
    //
    // **Attaching a manager changes the compositor's behaviour**: a `normal` window's first
    // `Configure` is held until the manager acts (M6 Part B4), so from here on nothing reaches
    // the screen unless this process places it. That is the point — it is also why the top bar
    // is created *before* this, since a `panel` that waited on a manager that did not exist yet
    // would be waiting on itself.
    // SAFETY: `session_ns` is live for this process's whole run.
    let mut manager = match unsafe { ChannelTransport::manage(session_ns) } {
        Ok(m) => {
            kprint(b"desktop-shell: manager channel held\n");
            Some(m)
        }
        Err(_) => {
            // Not fatal: a shell that cannot manage is a shell that draws a bar and launches
            // things, which is worse but not nothing. Say so — the alternative is a session
            // where windows silently never appear.
            kprint(b"desktop-shell: /dev/draw/manage unavailable; windows will not be placed\n");
            None
        }
    };
    // **The scheme, to the one process that cannot read it** (desktop refresh, Part A). The
    // compositor draws every window's shadow, the design makes a dark scheme's much darker, and
    // `init` started the compositor long before anyone read a theme. Sent before any application
    // is launched, so no window is ever drawn with the wrong one.
    if let Some(m) = manager.as_mut() {
        tell_scheme(m, theme.scheme);
    }
    // **The work area, from the compositor rather than from arithmetic here.** The shell's own
    // two bars are not the only struts a session can have — any `panel`-role client declares
    // one — and a maximised window computed from the screen's height less two bars would sit under the next
    // one with nothing able to notice. Kept current by `LayoutChanged` below (M9 Part B).
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
    // **And the snap zones the work area implies** (M9 Part F). Registered here rather than
    // computed by the compositor: which region means which rectangle is policy, and the
    // compositor's whole part is to match a pointer against a table it was given.
    if let Some(m) = manager.as_mut() {
        register_snap_zones(m, &layout);
    }

    // Where a maximised window came from, so restoring it has somewhere to go.
    //
    // **The shell's, not the compositor's.** A `maximized` flag there would be a second source
    // of truth about a rectangle, and the rectangle a window returns to is a decision — this is
    // the process that made it.
    let mut restore = RestoreList::new();
    // Windows already asked to close, whose next middle-click destroys them.
    //
    // **Asked first, insisted on afterwards.** A window holds a process's work, and a taskbar
    // that destroyed it would take the decision away from the only participant that knows
    // whether that matters. A client that ignores the request gets `Manage::Close` instead,
    // which is the only answer available to a desktop whose applications draw their own chrome
    // (M9 Part C).
    //
    // **What insisting waits for is the person, not a clock** (M12 Part A). This was a
    // two-second grace period, and it was safe for exactly as long as no client could decline:
    // every application in the tree answered `CloseRequested` by exiting, so the timer never
    // fired outside a wedge. The editor's confirmation is the first client that deliberately
    // does not answer — it is asking the person the shell's own question — and against it a
    // timer *loses the buffer*, two seconds after a click, with no way to intervene. A shell
    // cannot tell "wedged" from "asking"; the person looking at the dialog can, so the second
    // click is what says "I meant it", which is what a Force Quit is everywhere else.
    //
    // Considered and rejected: cancelling the grace when a `dialog` parented to that window
    // appears. It reads well and only works for clients that ask in the way this one happens
    // to — a client asking in an overlay, or one genuinely busy, is punished for it — and it
    // makes a visible policy depend on a coincidence of roles.
    //
    // **And the arming expires, because the shell cannot see an answer** (PR #267 review,
    // blocking 1). The first version armed on the ask and disarmed only on the destroy, so a
    // person who middle-clicked, read the question and chose *keep editing* left the entry
    // armed for the life of the window — and a middle-click ten minutes later went straight to
    // `Manage::Close` with no question and nothing on screen that had ever said so. That is the
    // outcome this whole change exists to prevent, with the two-second bound replaced by an
    // unbounded one.
    //
    // There is no signal that says a client declined: `CloseRequested` has no refusal by
    // design, and inferring one from a dialog appearing or going away is the coincidence-of-
    // roles coupling rejected above. So the second click insists only while it is still *part of
    // the first gesture* — the rule M12's kill ring settles for cycling, for the same reason:
    // a continuation is valid immediately after the thing it continues, and a stale one must be
    // unreachable rather than merely unlikely.
    let mut asked_to_close: alloc::vec::Vec<(u32, u64)> = alloc::vec::Vec::new();
    let mut next_origin = BAR_H as i32 + CASCADE_STEP;

    // Registered on the first pass of the loop, once the manager channel is known to exist.
    let mut hotkey_done = manager.is_none();

    // **Placed now that a manager exists — created long before it.** A dock edge tells the
    // compositor how much space to reserve; it does not move the window, so a bottom bar that
    // is never placed sits at the origin under the top one.
    if let Some(m) = manager.as_mut() {
        // **Both bars are made sticky, and without this they vanish on the first switch.**
        // The compositor stamps every new window with its current desktop, so panels created
        // at startup live on desktop 1 — and `visible_on` is the single predicate behind
        // compositing, focus *and* hit-testing. From the moment `Super+2` succeeded there was
        // no window list, no applications button and no indicator on screen, and the only way
        // back to the chrome was a chord, because hotkeys are routed compositor-side and do not
        // need a visible window (PR #243 review, blocking 1).
        //
        // `STICKY_DESKTOP` is the reserved value for exactly this, specified in Part A and
        // unused until now: chrome belongs to the screen rather than to one desktop.
        //
        // **The wallpaper is in this list too, and was not** (PR #272 review, blocking 1). It
        // is created at startup like the bars, so it is stamped with desktop 1, and every other
        // desktop showed the bare ground colour until you switched back — which reads as
        // flicker rather than as a missing feature. A picture behind everything belongs to the
        // screen for exactly the reason the chrome does.
        //
        // **The gate could not have caught it**: `check-login` asserts the wallpaper line once,
        // at startup, and its `Super+2` comes hundreds of lines later. What catches it is the
        // step added below.
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
    // **Its first frame, now that it is where it belongs** — not at the loop's first pass, which
    // may sit in `sys_wait` until something happens.
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

    // The Applications menu's entries, read once. `desktop-shell.md` §4: they are desktop
    // entries, and that falls out of decisions already made — they are ordinary files in the
    // namespace, so type-to-filter runs over them with no special mechanism.
    let programs = read_applications(session_ns);
    Line::new().s(b"desktop-shell: /applications lists ").u(programs.len() as u64).s(b" application(s)").end();
    // **The Places menu's, from the home an application sees** — the `HOME` this shell forwards,
    // which is `/home` inside an application's namespace — because a place is launched *into*
    // one. The session's own `argv[1]` spelling of the home is true only here.
    let app_home = env_str(&env, "HOME").unwrap_or("/home");
    let places = libfs::places(app_home);
    // **The two menus the top bar opens, and the one popup showing whichever is open** (desktop
    // refresh, Part C). The state is `libui`'s, the same value every window's menu bar keeps, so
    // arrows, Enter, Escape and Left/Right between the two menus behave as they do everywhere
    // else — and Left/Right is the only way to reach `Places` without a pointer.
    let mut bar = BarMenus {
        // Three: Applications, Places, and the power menu (administration Part F.2), which Left
        // and Right move between as a menu bar's do.
        state: MenuState::new(3),
        win: None,
        query: TextFieldState::new(),
        programs: &programs,
        places: &places,
        home: app_home,
        anchors: menu_anchors(&shown_clock, &theme, &font, screen),
    };
    // **The desktop-name prompt, a popup of its own** since the menu stopped being a modal it
    // could borrow.
    let mut prompt: Option<Child> = None;
    // **A session end under way** (administration Part F.2): the windows asked to close, and the
    // dialog naming what is left. `Closing` decides what happens; this loop asks and draws.
    let mut closing: Option<Closing> = None;
    let mut ending_win: Option<Child> = None;
    // **The window that last gained the keyboard**, whatever its role — which the window list
    // does not say, since a dialog is not in it. The waiting dialog gives the keyboard back to
    // an application's question it would otherwise take it from (PR #346 review, finding 2).
    let mut last_focus: u32 = 0;
    // **A Restart or a Shut down asked for** (administration Part F.3): the view broker's channel
    // while `shutdown` runs, and what it is for. The terminate request `service-mgr`'s sequence
    // then sends is what ends this shell; the channel is how a `shutdown` that failed is heard.
    let mut powering: Option<(u64, Ending)> = None;
    // **Why one did not happen**, while the dialog saying so is up.
    let mut refused: Option<(Child, Ending, Refusal)> = None;
    // Where that dialog hangs: right-aligned under the power button, as the power menu does, so it
    // never covers a question an application centred on its own window.
    let ending_anchor = ending_anchor(&shown_clock, &theme, &font, screen);
    let mut name = TextFieldState::new();
    // The overview: its window, the thumbnails it is showing, and which one is being dragged.
    let mut overview: Option<u32> = None;
    let mut over_addrs = [core::ptr::null_mut::<u8>(); BUFFERS];
    let mut shots: alloc::vec::Vec<(u32, u32, u32, alloc::vec::Vec<u8>)> = alloc::vec::Vec::new();
    let mut dragging: Option<u32> = None;
    // The overview is showing a desktop that has changed under it — by a click on its own
    // sidebar, or by a chord while it was open. It re-captures and re-presents rather than
    // closing: `desktop-shell.md` §6 is explicit that switching desktops inside the overview
    // "fetches a different set of images", and that is the whole of what it costs.
    let mut overview_dirty = false;

    // Blocks on the compositor's event channel, never spins — a spinning leader keeps a run
    // queue non-empty, so the idle thread never runs and deferred reclamation stops for the
    // whole machine (the 2026-07-31 `logging-service` bug).
    let ev = session.wait_handle();
    // Set whenever a manager request goes out; see the deadline below.
    let mut sent_request = false;
    // **The clock's next change, as a wait deadline** (M11 Part E batch 9). No timer handle is
    // needed: `sys_wait` already takes an absolute monotonic deadline, and the shell already
    // computes one for the close it may have to insist on. This is one more candidate for the
    // same minimum — a bar that ticks costs one wake a minute and no new kernel object.
    let mut next_tick = now_ns().map(next_minute).unwrap_or(u64::MAX);
    loop {
        // Both channels in one wait: the session's events and the manager's. Polling one
        // while blocked on the other would make a held window wait for a keystroke.
        // **A zero deadline when a request has just been sent.** `sys_wait` knows only about
        // the kernel queues; a manager event that arrived while a `Place`/`SetMinimized`/
        // `RegisterHotkey` reply was being waited for is parked *inside* the transport, with
        // nothing left in the kernel queue to wake this. Polling once after any request is the
        // belt: the drain below sees the parked events and the next iteration blocks normally
        // (PR #242 review, optional 10 — unverified there, and cheap enough to close).
        // **The clock's minute is the only deadline left.** A close used to put one here too;
        // it is a click rather than a timer now, so an outstanding close costs no wakeups at
        // all and the shell sleeps until something happens or the minute turns.
        //
        // **Which is what turned the belt above into a load-bearing one** (M12 Part A). The
        // `sent_request` flag covers *manager* requests, and the session's own — a `create`
        // waiting for its first `Configure`, an `acquire` waiting for a buffer release — park
        // arriving input inside the transport in exactly the same way, with nothing left in a
        // kernel queue to wake this. Those parked events were rescued by whatever wake happened
        // next, and until this part there was almost always one within two seconds: the close
        // grace period. Take the timer away and a press that landed while the modal was
        // committing a frame sat in the transport until the *minute* turned — which presented
        // as a launcher row that could be clicked and did nothing, once per run, entirely
        // reproducibly.
        //
        // So the queue is asked rather than assumed. `pump` is a non-blocking poll that moves
        // whatever the transport is holding into the per-window queues the drain below reads,
        // and a session with anything in them does not block. It cannot spin: the drain empties
        // what this counts.
        if session.pump().is_err() {
            fail(b"desktop-shell: compositor connection lost\n");
        }
        // **Drive a session end** (administration Part F.2) before choosing how long to sleep: ask any
        // window not asked yet — every one the first time, and one opened while the end waits — and
        // do what `Closing` says, which may be a deadline of its own.
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
                    // **The keyboard goes back to the question it was taken from** (PR #346 review,
                    // finding 2). A new window is the topmost that takes focus, so the dialog took
                    // the keyboard from the editor's "discard?" — and answers only Escape, as
                    // Cancel, so a person answering the editor cancelled the logout instead.
                    // **Only an application's transient window gets it back** — its question, or a
                    // menu it had open — since focus is a raise, and raising a normal window could
                    // cover the dialog. One in the window list, or one of ours, keeps it where it is.
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
        // **Every window is gone.** Log out and a stop end the shell here; Restart and Shut down ask
        // the view broker for `shutdown`, and the shell goes on until `service-mgr`'s sequence asks
        // it to stop — or says why not, and the session goes on without its windows.
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
            // **The desktop endpoint and its sessions wait alongside the rest.** Polling them
            // between compositor events would make a `desktop list` wait on a keystroke, and
            // blocking on them separately would make the shell stop drawing while a command
            // thought about it.
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
        // **A stop asks the windows too** (administration Part F.2): every one is asked to close, and
        // the shell goes once they have or after `STOP_WAIT_NS`, inside the leader's bound — with no
        // dialog, since the machine is going down. It overrides a person's end already under way.
        // With nothing to ask, or no clock to bound the wait with, it goes at once, as it did.
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
        // **What became of a Restart or a Shut down asked for** (administration Part F.3), unless
        // the stop it was for has come. `shutdown` exits once `service-mgr` has begun, so a clean
        // exit is only that; anything else did not happen, and the dialog says so.
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
        // **Drained before the compositor's events**, so a `Switch` that changes what the bar
        // shows is reflected by the same iteration's redraw rather than the next one's.
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
            // **The compositor told of any move the desktop endpoint just made, before a window
            // can be drained.** A `Name` there normalises, and can move the shell off an emptied
            // desktop; a window drained after that is recorded on the shell's current desktop and
            // must have been created on the same one (PR #314 review, optional 6).
            sync_current(m, &desktops, current_desktop, &mut told_desktop);
            // The shell's own windows are never listed: the bars and the wallpaper are
            // `panel`s and the menus and the name prompt `popup`s, so the role filter already
            // covers them, but
            // naming them is what keeps that true if a future shell window is `normal`.
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
            let mut dropped: alloc::vec::Vec<librsproto::surface::ConfigureEvent> = alloc::vec::Vec::new();
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
                screen,
            );

            // **A gesture the user finished, answered with the `Configure` it asked for**
            // (M9 Parts E and F). The compositor drew the outline and applied nothing: changing
            // a window's geometry is this process's, so there is one path to it rather than two
            // that can disagree. What arrives is the rectangle the gesture asks for — where a
            // resize was let go, or the target of the zone a move was dropped in — and the
            // answer is the same request a maximise sends, which is why the event carries a
            // `ConfigureEvent` and this is a loop rather than a translation.
            for c in dropped {
                // **A window the user has resized or snapped is no longer the one that was
                // maximised.** Its restore rectangle described where it came from before a
                // maximise it has now left by hand; keeping it would make the next restore put
                // it somewhere it never was.
                restore.retain(|(id, _)| *id != c.window);
                sent_request = true;
                configure_window(m, c.window, c.x, c.y, c.width, c.height, b"drop");
            }
            // **What a client asked to be, decided here.** The compositor forwarded the
            // question and applied nothing: minimising is a manager request and maximising is a
            // `Configure` to a rectangle only this process can compute, because only this
            // process knows what a maximised window should return to.
            for s in states {
                use librsproto::surface::{WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL};
                let Some(e) = entries.iter_mut().find(|e| e.id == s.window) else { continue };
                match s.state {
                    WINDOW_STATE_MINIMIZED => {
                        if minimize_window(m, e) {
                            sent_request = true;
                            list_dirty = true;
                            Line::new().s(b"desktop-shell: client asked to minimize window ").u(s.window as u64).end();
                        }
                    }
                    WINDOW_STATE_MAXIMIZED => {
                        // **Back on screen first.** A minimized window asked to maximise is
                        // asking to be *seen* maximised; configuring it where it is would resize
                        // something that is not on screen and leave it that way.
                        // `sent_request` is not set here, and it is not an omission: the
                        // `configure_window` below sets it unconditionally on every path
                        // through this arm. Setting it twice was a dead store the compiler
                        // pointed at once a rebuild stopped being cached.
                        if e.minimized && raise_window(m, e) {
                            list_dirty = true;
                        }
                        // Remembered before it moves, and only the first time: maximising an
                        // already-maximised window must not overwrite where it came from with
                        // the work area itself, which is a restore that does nothing.
                        if !restore.iter().any(|(id, _)| *id == s.window) {
                            restore.push((s.window, (e.origin.0, e.origin.1, e.size.0, e.size.1)));
                        }
                        sent_request = true;
                        // **What was asked for, not what happened.** A `Configure` is a request
                        // the client may decline — legally, and a fixed-size window is an
                        // ordinary thing — so the line `configure_window` prints is the shell's
                        // decision, and the window's own geometry event is what says whether it
                        // took it. `nxterm` declined every one until M9 Part D and now accepts.
                        configure_window(
                            m,
                            s.window,
                            layout.work_x,
                            layout.work_y,
                            layout.work_w,
                            layout.work_h,
                            b"maximize",
                        );
                    }
                    WINDOW_STATE_NORMAL => {
                        if e.minimized && raise_window(m, e) {
                            sent_request = true;
                            list_dirty = true;
                        }
                        if let Some(i) = restore.iter().position(|(id, _)| *id == s.window) {
                            let (_, (x, y, w, h)) = restore.remove(i);
                            sent_request = true;
                            configure_window(m, s.window, x, y, w, h, b"restore");
                        }
                    }
                    _ => {}
                }
            }

            // A window arriving or leaving can empty or fill a desktop, so the rule is
            // reconsidered after every drain rather than only where a desktop is switched.
            list_dirty |= normalize_desktops(&mut desktops, &entries, &mut current_desktop, &mut next_desktop_id);
            for id in fired {
                // **Bounded exactly as the click is**, and the reason is the bar's capacity's own
                // (`task_capacity`): an entry past it is neither drawn nor clickable, so minimizing one would take
                // the window off screen with no way to bring it back — "a window you cannot get
                // back rather than a cosmetic problem", which is what that bound exists to
                // prevent. Unbounded, the chord invalidated its own justification
                // (PR #242 review, finding 3).
                // **`Super+N`: switch.** The chord names the *Nth* desktop, not desktop id N —
                // ids are stable and never reused, so after a few have come and gone they stop
                // matching what a person sees on the switcher.
                if (HOTKEY_SWITCH_BASE + 1..=HOTKEY_SWITCH_BASE + CHORD_DESKTOPS).contains(&id) {
                    let n = (id - HOTKEY_SWITCH_BASE) as usize;
                    if let Some(d) = desktops.get(n - 1) {
                        let to = d.id;
                        if switch_desktop(m, &desktops, &mut current_desktop, &mut told_desktop, to) {
                            sent_request = true;
                            list_dirty = true;
                            normalize_desktops(&mut desktops, &entries, &mut current_desktop, &mut next_desktop_id);
                            // **An open overview follows the switch.** A chord works while it
                            // is up — it is a popup, not a modal grab — and leaving it showing
                            // the desktop you just left is a lie on screen rather than a
                            // missing feature.
                            overview_dirty = overview.is_some();
                        }
                    }
                    continue;
                }
                // **`Super+Shift+N`: move the focused window there.** One attribute write, and
                // then the rule is reconsidered — the move can empty the desktop it left and
                // fill the one it joined, and both halves matter.
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
                            // **Here, which is where the doc always said it happens.** The
                            // first version relied on the next iteration's drain to re-apply
                            // the rule, so the bar was rendered from a `desktops` this move had
                            // already invalidated — a full-width panel blit showing a stale
                            // count, immediately followed by another with the right one
                            // (PR #243 review, finding 6).
                            normalize_desktops(&mut desktops, &entries, &mut current_desktop, &mut next_desktop_id);
                            list_dirty = true;
                        } else {
                            kprint(b"desktop-shell: SetWindowDesktop was refused\n");
                        }
                    }
                    continue;
                }
                if id == HOTKEY_RENAME {
                    // The prompt is a popup, and for the reason the menus are: a `panel` takes
                    // no keyboard focus, so the bar could never read a typed name.
                    //
                    // **Only when nothing else is up.** A menu holds the keyboard, and a prompt
                    // opened over it would leave two popups each believing the next keystroke
                    // was its own.
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
                    // **The keyboard's way in, and the only way in on a machine with no
                    // pointer** (Phase 5). The Applications word lives on a `panel`, which takes
                    // no keyboard focus, so it can only be *clicked* — and the laptop this phase
                    // targets has no working pointing device until USB lands in Phase 6.
                    // Everything past this point is keyboard-driven: the menu is a popup and
                    // takes the keyboard, typing filters, Enter launches the lit row, and
                    // Left/Right reach `Places`.
                    //
                    // **A second press puts it away — or whatever popup is up.** A key that only
                    // opens is a key you cannot undo, and the name prompt is a popup this chord
                    // has always closed; each says which it closed, because a dismissal read as
                    // the wrong one sends a later gate looking for a line under the other name
                    // (PR #243 review, optional 8; PR #304 review, finding 1).
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
                // **Bounded by what the bar is *showing*, which since Part D is not the first
                // `task_capacity` of the global list but the first `task_capacity` on the current
                // desktop.** With seven windows on another desktop and one here, the one here
                // is drawn, clickable and focused — and its index in `entries` is 7, so a bound
                // over the global list never reached it and the chord silently did nothing for
                // a window the bar was showing (PR #243 review, finding 5).
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
        // **The compositor follows any move the drain made** — a window closing can empty the
        // current desktop and move the shell off it. Before the next window can be created on
        // the wrong one: windows arrive only through the drain above.
        if let Some(m) = manager.as_mut() {
            sync_current(m, &desktops, current_desktop, &mut told_desktop);
        }
        if session.pump().is_err() {
            fail(b"desktop-shell: compositor connection lost\n");
        }
        // **A chord to minimize what has the keyboard**, registered once the manager exists.
        // The bar gives you a window back; this is how you put one away without reaching for
        // it, which is the half a taskbar alone does not cover.
        if !hotkey_done && let Some(m) = manager.as_mut() {
            hotkey_done = true;
            sent_request = true;
            use librsproto::surface::{MOD_META, MOD_SHIFT, MgrHotkey, OP_MGR_REGISTER_HOTKEY};
            let hk = MgrHotkey { id: HOTKEY_MINIMIZE, mods: MOD_META, code: KEY_H };
            // **The desktop chords, registered in the same pass.** `Super+N` switches and
            // `Super+Shift+N` moves the focused window — both ending in one attribute write,
            // which is what makes the move available without the overview open.
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
            // Named `rename_hk` for the chord it is: the prompt it opens is `prompt`, and a
            // name that could be read as either is the shape where a later edit silently reads
            // the wrong one.
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
            // **`Super+A`, and not a tap of `Super`.** A bare modifier is what a full launcher
            // will want — one field that searches applications, files and settings — and this
            // is the Applications menu, so it takes a letter and leaves the tap unspent
            // (maintainer, 2026-09-15). A tap would also have to be told from the start of
            // every chord here, all of which begin with `Super` going down.
            let apps_hk = MgrHotkey { id: HOTKEY_APPS, mods: MOD_META, code: KEY_A };
            let mut ab = [0u8; core::mem::size_of::<MgrHotkey>()];
            if apps_hk.write(&mut ab).is_some() {
                let mut reply = [0u8; 64];
                if m.request(OP_MGR_REGISTER_HOTKEY, &ab, None, &mut reply).is_err() {
                    kprint(b"desktop-shell: registering Super+A was refused\n");
                }
            } else {
                // Silence here is what the minimize block below has a paragraph about: a body
                // that would not serialise skipped both lines, and the only evidence was a gate
                // waiting for a line nobody printed.
                kprint(b"desktop-shell: the Super+A registration would not serialise\n");
            }
            Line::new()
                .s(b"desktop-shell: Super+1..")
                .u(CHORD_DESKTOPS as u64)
                .s(b" switches, Super+Shift+N moves, Super+R names, Super+A opens applications")
                .end();

            let mut body = [0u8; core::mem::size_of::<MgrHotkey>()];
            if hk.write(&mut body).is_none() {
                // **Said out loud.** The first version had no `else` at all, so a body that
                // would not serialise skipped the success line *and* the refusal line, and the
                // only evidence was a gate timing out on a line nobody printed.
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
            // **Keys go to the open menu**, which is a popup and therefore takes the keyboard —
            // the property `check-terminal` relies on when it says "an open menu is a topmost
            // popup and takes the keyboard". The top bar could never receive these.
            if bar.id() == Some(w) {
                // **A press elsewhere dismisses it, and losing the keyboard does too.**
                //
                // Two signals rather than one, because neither covers the other. `Focus(false)`
                // arrives when something *raises* — clicking another window, or a chord that
                // restacks — and a press on the desktop or on a panel raises nothing and changes
                // no focus (reported by the maintainer; M11 Part E batch 5). `Dismissed` is the
                // compositor saying the press landed elsewhere, which is the half a client cannot
                // see for itself.
                //
                // `InputLost` is neither of these — that is queue overflow, and reading it as one
                // would close the menu on a burst of pointer motion.
                if matches!(event, libsurface::WindowEvent::Dismissed | libsurface::WindowEvent::Focus(false)) {
                    bar.close(&mut session);
                    continue;
                }
                let mut chosen: Option<MenuMsg> = None;
                // Closing waits until the choice has been acted on — see below.
                let mut finished = false;
                if let libsurface::WindowEvent::Key(k) = event {
                    let table = bar.table(&theme);
                    match bar.state.key(&k, &table) {
                        // **From the outcome, not from `open()`** — choosing closes, so the open
                        // menu is only named here (PR #280 review, blocking 1).
                        KeyOutcome::Chose { menu, item } => {
                            chosen = table.get(menu).and_then(|m| m.items.get(item)).and_then(|it| match it {
                                Item::Action { msg, enabled: true, .. } => Some(*msg),
                                _ => None,
                            });
                            finished = true;
                        }
                        KeyOutcome::Dismissed => finished = true,
                        // Left and Right move to the other menu; Up and Down move the lit row.
                        KeyOutcome::Changed => {
                            if let (Some(id), Some(m)) =
                                (bar.sync(&mut session, window, &theme, &font), manager.as_mut())
                            {
                                stick(m, id, bar.what().as_bytes());
                            }
                        }
                        // **Everything the menu does not claim is the filter's**, on the
                        // Applications menu — a Places menu has nothing to type into.
                        KeyOutcome::Ignored => {
                            if bar.state.open() == Some(panel::APPS)
                                && k.pressed != 0
                                && bar.query.apply(k.keycode, k.modifiers, k.locks)
                            {
                                let table = bar.table(&theme);
                                // **The top match is lit**, so Enter launches what the person can
                                // see — the modal launched "the top hit" with nothing to say
                                // which one that was.
                                bar.state.select_first(&table);
                                bar.redraw(&mut session, &theme, &font);
                                // **A receipt per character, and it is a count** (M12 Part A).
                                // `check-login` types into the filter and then clicks a row, so
                                // a keystroke lost to a dropped PS/2 batch has to be visible —
                                // and what somebody types into a launcher is a program they
                                // are about to run, so the number is logged and not the text.
                                let listed = table.get(panel::APPS).map_or(0, |m| {
                                    m.items.iter().filter(|it| matches!(it, Item::Action { enabled: true, .. })).count()
                                });
                                Line::new().s(b"desktop-shell: applications menu listing ").u(listed as u64).end();
                            }
                        }
                    }
                }
                if let libsurface::WindowEvent::Pointer(_) = event {
                    // **Routed through the toolkit, against the tree the popup last presented**
                    // — `Child` keeps that, and the hover a gesture must see, which is the pair
                    // of rules this loop used to keep by hand (M12 Part B and D).
                    let msgs = bar.route(&font, &theme, &event);
                    chosen = msgs.first().copied();
                    if chosen.is_some() {
                        finished = true;
                    } else {
                        bar.redraw(&mut session, &theme, &font);
                    }
                }
                match chosen {
                    // **The entry's `exec`, not its display name** — "Text Editor" is what a
                    // person reads, `nxedit` is what `/bin` resolves.
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
                    // **The power menu** (administration Parts F.2 and F.3): begin ending the
                    // session. The loop's top asks the windows and waits; a second choice while one
                    // is under way, or while a Restart or a Shut down is, changes nothing.
                    //
                    // **A Restart or a Shut down the policy would refuse is refused here**, before a
                    // window is asked to close: found after the windows had gone, the refusal would
                    // have cost the person them for nothing.
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
                // **Launched, then closed** — the modal's order, and the one a reader of the log
                // expects: the choice is what the menu was for, and its going away is the
                // consequence. `check-login` and `shot` read the two lines in this order.
                if finished {
                    bar.close(&mut session);
                }
                continue;
            }
            // **The waiting dialog's own input** (administration Part F.2): End anyway, Cancel — its
            // buttons, its close button, or Escape. **It is not dismissed by a press elsewhere**, as
            // a menu is: the person is expected to press elsewhere, to answer an application's own
            // question, and the dialog stays until it is answered or has nothing left to name.
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
            // **The refusal's own input** (administration Part F.3): Close, the frame's close,
            // Escape, Enter, or a press elsewhere — it is a report, and there is nothing to decide.
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
            // **The name prompt's own input.** Typing names; Enter keeps it; Escape, a press
            // elsewhere, or losing the keyboard puts it away unnamed.
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
                        // **Naming is what makes a desktop persist**, so this is the one
                        // gesture that changes the lifecycle rather than the view.
                        // **Capped at what the wire can carry.** `write_list` refuses a whole
                        // `List` reply rather than truncating a name, which is right — but the
                        // text field has no cap of its own, so a 33-character label typed here
                        // made every later `List` fail for *all* desktops, permanently, since
                        // the name persists (PR #245 review, finding 4).
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
                        // Naming changes which desktops survive, so the rule applies here too.
                        normalize_desktops(&mut desktops, &entries, &mut current_desktop, &mut next_desktop_id);
                    } else if name.apply(k.keycode, k.modifiers, k.locks) {
                        // **A receipt per character.** Injection is relative and unacknowledged,
                        // so a dropped PS/2 batch silently eats a keystroke — a desktop named
                        // `wok` instead of `work`, which is a gate failure that looks like a
                        // logic bug. A desktop's name is a label the person is choosing and can
                        // see, so it is logged as text.
                        Line::new().s(b"desktop-shell: name so far ").untrusted(name.text().as_bytes()).end();
                        if let Some(c) = prompt.as_mut() {
                            c.present(&mut session, &panel::name_prompt(&name, &theme), &font, &theme);
                        }
                    }
                }
                continue;
            }
            // **The overview's own input**: Escape closes it, a press picks a thumbnail up, a
            // release over a sidebar row drops it there.
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
                        // **Picked up by which thumbnail, not by where inside it.** The
                        // press-relative offset the `scroll-grab` deferral was about matters when the
                        // thing being dragged is *drawn* following the cursor; here the
                        // thumbnail stays put and only the drop target is read, so the offset
                        // has nothing to be wrong about. That deferral named this as its second
                        // consumer; it is re-deferred rather than answered, and the reason is
                        // that this drag does not need what it is about.
                        dragging = window_in_card_at(p.x, p.y, &desktops, &entries, screen);
                        if let Some(id) = dragging {
                            Line::new().s(b"desktop-shell: dragging window ").u(id as u64).end();
                        }
                    } else {
                        // **Release: which of three gestures this was.** A press over a
                        // thumbnail always picks it up — it cannot know yet whether the pointer
                        // is about to move — so "was something picked up" does not separate a
                        // click from a drag. Where the release lands does:
                        //
                        // - over a sidebar row, holding a thumbnail → move that window there;
                        // - over the **same** thumbnail it started on → no movement, so it was
                        //   a click on a window: activate it;
                        // - over a sidebar row, holding nothing → a click on a desktop: go
                        //   there.
                        //
                        // The last two were dead until 2026-08-26: a press on a row set no drag
                        // and its release matched no arm, and a press-and-release on a thumbnail
                        // was discarded as "a drag abandoned". So the two most obvious gestures
                        // in an overview did nothing, while `desktop-shell.md` §6 said in as
                        // many words that "you can switch desktops from inside it". Only the
                        // drag was wired up, and only the drag was gated — which is how an
                        // unimplemented affordance passed for a tested one.
                        let picked = dragging.take();
                        let row = desktop_shell::card_at(p.x, p.y, desktops.len(), screen);
                        let under = window_in_card_at(p.x, p.y, &desktops, &entries, screen);
                        // **A window's box is *inside* a card**, so every release over a window
                        // is also a release over a card (desktop refresh, Part E). The click
                        // arm therefore has to be tried first: matching the drop arm first would
                        // turn "click the window you pressed" into "move it to the desktop it is
                        // already on", which is a manager round trip for nothing and closes the
                        // overview instead of raising anything.
                        let same_desktop = picked
                            .and_then(|wid| entries.iter().find(|e| e.id == wid))
                            .zip(row)
                            .is_some_and(|(e, i)| e.desktop == desktops[i].id);
                        match (picked, row) {
                            // **A press and a release on the same window: a click, so go to
                            // it** — and *going to* a window on another desktop means switching
                            // there first (PR #323 review, blocking 1).
                            //
                            // Until cards, only the current desktop's windows had a target at
                            // all; now every card's do, and `raise_window` alone reorders a
                            // stack nobody is looking at. `WindowStack::raise` does not change
                            // which desktop is composited, so the overview closed and the
                            // screen was exactly as it had been — on a card holding a maximised
                            // window that is most of the card, which is also most of the area
                            // "click a card to switch" was supposed to cover.
                            (Some(wid), _) if under == Some(wid) => {
                                let home = entries.iter().find(|e| e.id == wid).map(|e| e.desktop);
                                if let Some(m) = manager.as_mut()
                                    && let Some(home) = home
                                {
                                    sent_request = true;
                                    // **The switch is not attempted when it is already there**,
                                    // because `switch_desktop` answers `false` for that and
                                    // would read as a refusal. No `normalize_desktops` after
                                    // it: the desktop being switched to holds this window, so
                                    // it is occupied by definition and the trailing scratch
                                    // slot is untouched — which is the one thing that call is
                                    // for.
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
                            // Dropped back on the desktop it came from: nothing to ask for.
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
                                        // The overview is a snapshot of a desktop that has just
                                        // changed, so it is closed rather than left showing a
                                        // window that is no longer here.
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
                            // **The row you are already on dismisses.** Clicking a desktop is
                            // "go there"; clicking it again, now that you are there, is the
                            // natural way to say "and I am done" — and without it an empty
                            // desktop is a dead end, because the way out of an overview was to
                            // click a window and there are none. Escape works and is not
                            // discoverable (reported from a real session, 2026-08-26).
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
                                        // Stays open, showing the desktop just switched to.
                                        overview_dirty = true;
                                    }
                                }
                            }
                            // **A click on the overview's own background dismisses it**, which
                            // is what clicking outside a menu does everywhere else. It also
                            // makes the desktop's name a toggle for free: the overview covers
                            // the bar, so a second click where the name is lands here.
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
                            // A drag let go over nothing, which is not an error — abandoning a
                            // drag is not the same gesture as clicking the background, and
                            // dismissing on it would make a mis-aimed drop close the overview.
                            _ => {}
                        }
                    }
                }
                continue;
            }
            // **The top bar's two words**, routed through the toolkit rather than hit-tested
            // against a width written down beside them: a click on one opens its menu, or closes
            // it if it is the one open. The pointer lights them as it lights every other control.
            //
            // **A click on the open menu's own word reopens it**, as it does on every window's
            // menu bar: the press dismisses the popup — the compositor tells it the press landed
            // elsewhere — and the release is then a click on a word whose menu is shut.
            if w == window {
                let view = panel::top_bar(&shown_clock, bar.state.open(), top.hovered_key(), &theme);
                // **Presented before it is routed**, so the tree a press is routed against is the
                // one this view describes: a menu closed earlier in this drain un-lights its word,
                // and a lit word is a different shape from a quiet one — the id mismatch that
                // loses a click (M12 Part D). Nothing is drawn when nothing changed.
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
            // **The bottom bar**, routed through the toolkit: show-desktop, one button per window,
            // and the switcher. The tree is the one presented at the top of this pass, which is
            // what a press has to be routed against.
            if Some(w) == bottom
                && let Some(bb) = bottom_bar.as_mut()
            {
                let lit = shown.as_ref().is_some_and(|s| s.desktop == current_desktop);
                let view =
                    bottom_view(&entries, &desktops, current_desktop, lit, bb.hovered_key(), &theme, &font, screen);
                // Presented before it is routed, for the top bar's reason: the manager's events
                // earlier in this pass may have changed the windows it shows.
                bb.present(&mut session, &view, &font, &panel_theme);
                // **The middle button closes**, which is what every taskbar this borrows from
                // does. A message cannot say which button made it, so the event is asked.
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
                            // It *asks*: a window holds a process's work, and the shell insists
                            // only on a second click (M9 Part C, M12 Part A).
                            if middle {
                                ask_to_close(m, wid, &mut asked_to_close);
                                continue;
                            }
                            // **Clicking the focused window puts it away**, which is what every
                            // taskbar does and the only gesture that needs no second control.
                            // Clicking anything else brings it forward, restoring it first if it
                            // was minimized.
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
                        // Everything below is the left button's: a middle click on a control
                        // that closes nothing does nothing.
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
                        // **The desktop's name opens the overview** — the switcher's "all
                        // desktops", which is what `desktop-shell.md` §7 always said the
                        // indicator in its place did.
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
        // And any the events made — a window dropped on another desktop from the overview can
        // empty this one too.
        if let Some(m) = manager.as_mut() {
            sync_current(m, &desktops, current_desktop, &mut told_desktop);
        }
        // **A window that went on its own is no longer owed an insist.** Ids are never reused,
        // so a stale entry can never name a later window and nothing here is about correctness:
        // it is that this vector would otherwise hold an entry per window ever asked about, for
        // the life of the session (PR #267 review, optional 5 — the first version of this
        // comment claimed the stale entry could arm the *next* window, which its own first
        // clause rules out).
        asked_to_close.retain(|(id, _)| entries.iter().any(|e| e.id == *id));

        // **The clock, when the minute it shows has changed.** Compared as text rather than by
        // the deadline having passed: an unset clock formats to nothing every time, so this
        // repaints zero times instead of once a minute forever — and a wake that finds the same
        // string costs nothing but the comparison.
        if let Some(now) = now_ns()
            && now >= next_tick
        {
            next_tick = next_minute(now);
            let want = clock_text();
            if want != shown_clock {
                shown_clock = want;
            }
        }
        // **The top bar, whenever anything it shows may have moved** — the minute, which word
        // the pointer is over, which menu is open. Asked every pass and cheap when nothing did:
        // `Child::present` diffs the tree and commits only what changed, so a pass that changed
        // nothing costs a layout and no frame. A menu closed by a key, a choice or a dismissal is
        // then un-lit on the same pass as it went, with no flag for each of those to remember.
        let view = panel::top_bar(&shown_clock, bar.state.open(), top.hovered_key(), &theme);
        if !top.present(&mut session, &view, &font, &panel_theme) {
            kprint(b"desktop-shell: top bar Commit failed\n");
        }

        // **Show-desktop lets go the moment anything on that desktop is back** — a window
        // restored from its button, a new one opened, one moved there — but not a window the press
        // could not reach, which was never put away (`ShownDesktop::holds`; PR #314 review,
        // blocking 1). Its second press means "bring back what the first put away", which stops
        // being a coherent request once the desktop is not what the first press left; and the
        // button's light is what says whether there is anything to bring back.
        if let Some(s) = shown.as_ref()
            && !s.holds(&windows_of(&entries))
        {
            shown = None;
            list_dirty = true;
        }
        // **The window list in the log, when it changed** — every gate reads the bar from here.
        if list_dirty {
            let cap = task_capacity(&desktops, &entries, current_desktop, &theme, &font, screen);
            let listed = visible_entries(&entries, current_desktop, cap);
            log_window_list(&listed, &desktop_label(&desktops, current_desktop), desktops.len());
            list_dirty = false;
        }
        // **The bottom bar, every pass, as the top one is** — `Child::present` diffs and commits
        // only what changed, so a pass that changed nothing costs a layout and no frame.
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

        // **Re-render an open overview whose desktop changed.** Its thumbnails are of one
        // desktop, so a switch — from its own sidebar, or from a chord while it is up — makes
        // every one of them stale. `desktop-shell.md` §6 chose this over closing: "switching
        // desktops inside the overview is trivial, it fetches a different set of images".
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

/// Scratch for [`now_ns`].
static mut CLOCK_BUF: u64 = 0;

/// The monotonic clock, in nanoseconds, or `None` if it will not answer.
///
/// **The clock on the top bar is what needs it**, since M11 Part E batch 9 — it ticks off this
/// rather than off a timer object. The close grace period used to be the only caller and is
/// gone: insisting on a close is a second click now rather than an elapsed two seconds, so
/// nothing about closing a window depends on the clock answering at all.
///
/// `None` is a clock that has stopped, and every caller treats it as *nothing to do this round*
/// rather than as zero: a deadline nothing can evaluate is a wait that returns immediately for
/// ever, which is the spin this file calls machine-wide harmful.
fn now_ns() -> Option<u64> {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    let r = unsafe {
        libkern::syscall::syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64)
    };
    if r != 0 {
        return None;
    }
    // SAFETY: the call succeeded, so the kernel wrote the ns count.
    Some(unsafe { (&raw const CLOCK_BUF).read() })
}

/// How long a close request stays armed for the click that insists on it.
///
/// **Not the grace period coming back.** That timer *acted* when it expired — it destroyed a
/// window whose client had not answered. This one only forgets: when it runs out the next
/// middle-click asks again, which is the safe direction and the one a person can recover from
/// by clicking once more.
///
/// It is how long a second click is still part of the first, and the person is the clock it is
/// measured against: they click, watch for a moment, and click again when nothing happened.
/// Long enough for that; short enough that a click made after reading a question and answering
/// it is a fresh intention rather than a continuation. `check-login`'s two clicks are 63 ms
/// apart on the accelerator CI uses, so the gate has two orders of magnitude of room.
const INSIST_WINDOW_NS: u64 = 5_000_000_000;

/// Ask a window's client to close — or, if the ask is still in hand, insist.
///
/// **The second click is the insist**, which is the whole of M12 Part A's close-policy change:
/// the first middle-click asks, and if the window is still there when the person clicks again
/// they have said they meant it. Before this the answer was a two-second timer, which was safe
/// only while no client could decline — see `asked_to_close`'s declaration for why the editor's
/// confirmation dialog is exactly the client that makes a timer lose work, and for why the arm
/// expires rather than lasting the window's life.
///
/// **A clock that will not answer means nothing is ever armed**, so the taskbar can ask and
/// never insist. That is this file's stance on `now_ns` everywhere: the failure that leaves the
/// machine working is the one where a window stays, and a window that cannot be forced shut from
/// the bar can still be closed by its own button.
///
/// A window that goes away on its own leaves the entry that named it, so the ordinary case never
/// reaches `Manage::Close` at all.
fn ask_to_close(mgr: &mut ChannelTransport, window: u32, asked: &mut alloc::vec::Vec<(u32, u64)>) {
    let now = now_ns();
    let armed = asked.iter().position(|&(id, until)| id == window && now.is_some_and(|n| n < until));
    if let Some(i) = armed {
        if insist_on_close(mgr, window) {
            asked.remove(i);
        }
        return;
    }
    if !request_close(mgr, window) {
        return;
    }
    // An expired entry for this window is replaced rather than joined: one window is being
    // asked about once, however many times the question has been put.
    asked.retain(|&(id, _)| id != window);
    if let Some(n) = now {
        asked.push((window, n.saturating_add(INSIST_WINDOW_NS)));
    }
}

/// **Ask a window's client to close it** — `Manage::RequestClose`, the polite half. `true` if the
/// compositor forwarded it. The taskbar's ask ([`ask_to_close`]) and a session end
/// (administration Part F.2) both come here.
fn request_close(mgr: &mut ChannelTransport, window: u32) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_REQUEST_CLOSE};
    let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
    if (MgrWindowRef { window, other: 0 }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_REQUEST_CLOSE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: RequestClose refused for window ").u(window as u64).end();
        return false;
    }
    Line::new().s(b"desktop-shell: asked window ").u(window as u64).s(b" to close").end();
    true
}

/// Destroy a window whose client did not answer — `Manage::Close`.
///
/// Reached only from [`ask_to_close`]'s second click. `true` if the compositor took it.
fn insist_on_close(mgr: &mut ChannelTransport, window: u32) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_CLOSE};
    let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
    if (MgrWindowRef { window, other: 0 }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_CLOSE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: Close refused for window ").u(window as u64).end();
        return false;
    }
    Line::new().s(b"desktop-shell: window ").u(window as u64).s(b" did not answer; closed it").end();
    true
}



/// Where a dialog of `size` goes over `base`, kept inside the work area.
///
/// **Centred on its parent, then clamped**, in that order: a dialog wider than the window it
/// belongs to would otherwise hang off the screen's edge with half its buttons unreachable, and
/// a dialog is exactly the window a person must be able to answer. One that will not fit at all
/// is put at the work area's origin rather than negative, so what is lost is its bottom-right
/// rather than its title bar.
///
/// `base` is `(x, y, w, h)` — the parent's rectangle, or the work area when there is no parent
/// this shell knows about.
fn centre_dialog(base: (i32, i32, u32, u32), size: (u32, u32), work: &MgrLayout) -> (i32, i32) {
    let x = base.0.saturating_add((base.2 as i32 - size.0 as i32) / 2);
    let y = base.1.saturating_add((base.3 as i32 - size.1 as i32) / 2);
    // `.max(0)` keeps the clamp's bounds ordered when the dialog is larger than the work area;
    // `clamp` panics on an inverted range, and this file's whole stance is that a shell must
    // not be the thing that dies.
    let max_x = work.work_x.saturating_add((work.work_w as i32 - size.0 as i32).max(0));
    let max_y = work.work_y.saturating_add((work.work_h as i32 - size.1 as i32).max(0));
    (x.clamp(work.work_x, max_x), y.clamp(work.work_y, max_y))
}

/// Where the waiting dialog hangs (administration Part F.2): right-aligned under the power button,
/// as the power menu is, [`libui::widget::DIALOG_W`] wide.
fn ending_anchor(clock: &str, theme: &Theme, font: &Font, screen: Screen) -> (i32, i32) {
    let view = panel::top_bar(clock, None, None, theme);
    let l = layout(&view, Rect::new(0, 0, screen.width, BAR_H), &FontMetrics::new(font, theme.font_px));
    libui::layout::locate(&view, &l, panel::POWER_KEY).map_or(
        (screen.width as i32 - libui::widget::DIALOG_W as i32 - panel::MENU_INSET_X, BAR_H as i32 + panel::MENU_DROP),
        |word| panel::menu_anchor_right(word, libui::widget::DIALOG_W),
    )
}

/// The waiting dialog as `c` has it now. A stop has no dialog, so only a person's end is drawn.
fn ending_view(c: &Closing, hovered: Option<u64>, theme: &Theme) -> Element<EndingMsg> {
    let ending = match c.asked() {
        Asked::Person(e) => e,
        Asked::Stop => Ending::LogOut,
    };
    panel::ending_dialog(ending, &c.question(), hovered, theme)
}

/// **Bring the waiting dialog up, or draw it again** with what it names now (administration Part
/// F.2). A popup of the top bar's, as the menus are, rather than a `dialog`: a dialog is held for
/// the manager to place, and the manager is this shell, which would be waiting on itself. Its
/// origin is logged, since `check-logout` aims at its buttons from it.
fn show_ending_dialog(
    session: &mut Session<ChannelTransport>,
    win: &mut Option<Child>,
    c: &Closing,
    parent: u32,
    at: (i32, i32),
    theme: &Theme,
    font: &Font,
) {
    if let Some(d) = win.as_mut() {
        let view = ending_view(c, d.hovered_key(), theme);
        d.present(session, &view, font, theme);
        Line::new().s(b"desktop-shell: ").s(c.question().0.as_bytes()).end();
        return;
    }
    let view = ending_view(c, None, theme);
    let Some(mut d) = Child::open(session, Role::Popup { parent }, at, &view, font, theme, BUFFERS) else {
        kprint(b"desktop-shell: waiting dialog CreateWindow FAILED\n");
        return;
    };
    if !d.present(session, &view, font, theme) {
        d.close(session);
        return;
    }
    Line::new()
        .s(b"desktop-shell: waiting dialog open, window ")
        .u(d.id() as u64)
        .s(b" at ")
        .i(at.0 as i64)
        .s(b",")
        .i(at.1 as i64)
        .s(b": ")
        .s(c.question().0.as_bytes())
        .end();
    *win = Some(d);
}

/// **The session ends** (administration Part F.2), every window closed or ended anyway: the shell
/// exits, and `desktop-session-mgr` does the rest — tells the broker, closes the namespace, and
/// presents the greeter. A stop exits as it always has (E.4c). Restart and Shut down do not come
/// here: they ask the broker, and a stop follows ([`ask_to_power`]).
fn end_session(asked: Asked) -> ! {
    if asked == Asked::Stop {
        stop();
    }
    kprint(b"desktop-shell: logging out, exiting\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 0, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// **How long the shell waits for the view broker** before calling it gone. It answers a listing or
/// a request at once — a request's program is spawned, not waited for — and the shell has a
/// screen to keep answering, so this bounds only a broker that has stopped.
const BROKER_WAIT_NS: u64 = 5_000_000_000;

/// When a wait on the broker begun now gives up.
fn broker_deadline() -> u64 {
    now_ns().map_or(u64::MAX, |n| n.saturating_add(BROKER_WAIT_NS))
}

/// **Whether this session's policy would refuse a Restart or a Shut down** (administration Part
/// F.3), asked of the broker with `Decide` before any window is asked to close — its own rule,
/// running nothing. `None` to go on. The request after the windows close is still what decides.
fn power_refusal(session_ns: u64) -> Option<Refusal> {
    let ch = libviews::broker(session_ns);
    if ch == 0 {
        return Some(Refusal::Failed(alloc::string::String::from(failed::NO_BROKER)));
    }
    let answer = libviews::decide(ch, POWER_VIEW, POWER_PROGRAM, broker_deadline());
    libprompt::ipc::close(ch);
    match answer {
        Ok((outcome, why)) => Refusal::from_answer(outcome, why),
        Err(f) => Some(Refusal::Failed(alloc::string::String::from(f.why()))),
    }
}

/// **Ask the view broker for `shutdown`** (administration Part F.3), the windows gone: in the
/// `power` view, `--reboot` for Restart, as `with power shutdown` asks — the same request, from
/// `libviews`. The broker's channel while `shutdown` runs, or why it is not running.
///
/// **An application's namespace goes to the broker, not this shell's.** The session's binds
/// `/dev/draw` whole and so reaches `manage`; a view derived from it would hand `shutdown` that too.
/// F.1's builder makes the one an application gets, which reaches `/dev/draw/new` alone.
///
/// **A password is not asked for**: the desktop has no prompt to ask with, so the channel is
/// closed — the broker's cue to drop the request — and the refusal says so.
fn ask_to_power(l: &Launcher<'_>, ending: Ending) -> Result<u64, Refusal> {
    use libviews::Outcome;
    let failure = |why: &str| Refusal::Failed(alloc::string::String::from(why));
    let ch = libviews::broker(l.session_ns);
    if ch == 0 {
        return Err(failure(failed::NO_BROKER));
    }
    let Some(built) = build_app_namespace(l) else {
        libprompt::ipc::close(ch);
        return Err(failure(failed::NO_NAMESPACE));
    };
    // **Verified as a launch's is**, by the process that built it: that `shutdown`'s view cannot
    // reach `manage` rests on the same narrow bind.
    let (home, desktop, devices, storage, user) =
        (!l.home.is_empty(), l.desktop != 0, l.devices != 0, l.storage != 0, !l.user.is_empty());
    if !verify_app_namespace(built.ns, home, desktop, devices, storage, user) {
        libprompt::ipc::close(built.ns);
        libprompt::ipc::close(ch);
        kprint(b"desktop-shell: application namespace is not gated; not asking the broker\n");
        return Err(failure(failed::NO_NAMESPACE));
    }
    let args = ending.power_args().unwrap_or(&[]);
    let handed = libviews::Handed { ns: built.ns, ..libviews::Handed::default() };
    let refusal = match libviews::request(ch, POWER_VIEW, POWER_PROGRAM, args, &[], handed, broker_deadline()) {
        Ok((Outcome::Started, _)) => {
            Line::new()
                .s(b"desktop-shell: ")
                .s(ending.title().as_bytes())
                .s(b": the broker started ")
                .s(POWER_PROGRAM.as_bytes())
                .s(b" in the ")
                .s(POWER_VIEW.as_bytes())
                .s(b" view")
                .end();
            return Ok(ch);
        }
        Ok((Outcome::NeedPassword, _)) => Refusal::Password,
        Ok((Outcome::Denied { .. }, why)) => Refusal::Refused(why),
        Err(f) => failure(f.why()),
    };
    libprompt::ipc::close(ch);
    Err(refusal)
}

/// **Say why a Restart or a Shut down did not happen** (administration Part F.3): on the console,
/// and in a dialog hung where the waiting dialog is, under the power button, replacing any refusal
/// already up. Its origin is logged, as the waiting dialog's is.
#[allow(clippy::too_many_arguments, reason = "the refusal, where it goes and how to draw it")]
fn show_refusal(
    session: &mut Session<ChannelTransport>,
    slot: &mut Option<(Child, Ending, Refusal)>,
    ending: Ending,
    refusal: Refusal,
    parent: u32,
    at: (i32, i32),
    theme: &Theme,
    font: &Font,
) {
    // Escaped: a broker's reason can quote the policy's own text back.
    Line::new().s(b"desktop-shell: ").s(ending.title().as_bytes()).s(b" ").untrusted(refusal.log().as_bytes()).end();
    if let Some((d, _, _)) = slot.take() {
        d.close(session);
    }
    let view = panel::refusal_dialog(ending, &refusal, None, theme);
    let Some(mut d) = Child::open(session, Role::Popup { parent }, at, &view, font, theme, BUFFERS) else {
        kprint(b"desktop-shell: refusal dialog CreateWindow FAILED\n");
        return;
    };
    if !d.present(session, &view, font, theme) {
        d.close(session);
        return;
    }
    Line::new()
        .s(b"desktop-shell: refusal dialog open, window ")
        .u(d.id() as u64)
        .s(b" at ")
        .i(at.0 as i64)
        .s(b",")
        .i(at.1 as i64)
        .end();
    *slot = Some((d, ending, refusal));
}

/// Open the desktop-name prompt: above the bottom bar's right-hand end, where the desktop's name
/// is — or below the top bar's, on a session with no bottom bar.
fn open_prompt(
    session: &mut Session<ChannelTransport>,
    top: u32,
    bottom: Option<u32>,
    name: &TextFieldState,
    theme: &Theme,
    font: &Font,
    screen: Screen,
) -> Option<Child> {
    let view = panel::name_prompt(name, theme);
    let h = libui::layout::measure(
        &view,
        libui::layout::Constraints::loose(libdraw::geom::Size::new(u32::MAX / 4, u32::MAX / 4)),
        &FontMetrics::new(font, theme.font_px),
    )
    .h as i32;
    let x = screen.width as i32 - panel::PROMPT_W as i32 - panel::MENU_INSET_X;
    // A popup's offset is from its parent's origin, so above the bottom bar is a negative one.
    let (parent, at) = match bottom {
        Some(b) => (b, (x, -(h + panel::MENU_DROP))),
        None => (top, (x, BAR_H as i32 + panel::MENU_DROP)),
    };
    let Some(mut c) = Child::open(session, Role::Popup { parent }, at, &view, font, theme, BUFFERS) else {
        kprint(b"desktop-shell: name prompt CreateWindow FAILED\n");
        return None;
    };
    if !c.present(session, &view, font, theme) {
        c.close(session);
        return None;
    }
    Line::new().s(b"desktop-shell: name prompt open, window ").u(c.id() as u64).end();
    Some(c)
}

/// Destroy the name prompt, forgetting what was typed into it.
fn close_prompt(session: &mut Session<ChannelTransport>, prompt: &mut Option<Child>, name: &mut TextFieldState) {
    if let Some(c) = prompt.take() {
        let id = c.id();
        c.close(session);
        name.clear();
        Line::new().s(b"desktop-shell: name prompt closed, window ").u(id as u64).end();
    }
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
