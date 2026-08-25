//! `desktop-shell` — the graphical session's leader, and the compositor's first real manager.
//!
//! **What M6 built for a manager has been exercised by a test client until now.** Placement,
//! restacking, focus, the initial-configure hold and the five manager events all have gates,
//! and every one of those gates is `ui-testclient` pretending. This is the process they were
//! for.
//!
//! It draws a **top bar** across the screen, reserving space with a `panel` strut so ordinary
//! windows do not sit under it.
//!
//! **It does not bind its own endpoint.** `desktop-session-mgr` binds `/dev/desktop` into the
//! session namespace; the shell holds `BIND_NAMESPACE` to construct *application* namespaces
//! continuously, not to register itself once (`graphical-session.md` §3).
//!
//! `#![no_std]` + `#![no_main]`, with `alloc` — the toolkit builds an element tree per frame.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Geometry, MemFramebuffer};
use libdraw::geom::Rect;
use libdraw::text::Font;
use libkern::debug::Line;
use libkern::*;
use librsproto::surface::{CreateWindowRequest, Edge, Role};
use libsurface::Session;
use libsurface::ipc::ChannelTransport;
use libui::element::{Element, Insets, column, padding, row, sized, text};
use libui::layout::layout;
use libui::paint::{FontMetrics, Theme, paint};
use libui::widget::{ListRow, ListState, Palette, TextFieldState, WidgetState, list_view, text_field};

/// `alloc` backing: the toolkit builds an element tree per frame.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where the font comes from — the same path every other graphical client reads.
const SYSTEM_FONT_PATH: &str = "/system/fonts/DejaVuSansMono.ttf";

/// The screen's width, which the top bar spans.
///
/// Fixed rather than queried: the compositor has no "what size is the screen" op, and adding
/// one to draw a bar would be a protocol change made for a stub's convenience. `check-display`
/// already hardcodes the same 1280×800.
const SCREEN_W: u32 = 1280;
/// The top bar's height.
const BAR_H: u32 = 24;
/// Bytes per row.
const BAR_PITCH: usize = (SCREEN_W as usize) * 4;
/// Text size, in pixels per em.
const FONT_PX: f32 = 16.0;
/// How many buffers the bar attaches.
const BUFFERS: usize = 2;

/// Write one line to the debug console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
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

/// How wide the applications button is, in pixels.
///
/// The modal's **only** trigger for now. `desktop-shell.md` §4 gives it two — this button and
/// the Super key — but the Super key is a *global hotkey*, which §8 makes a capability rather
/// than an ambient grab, and the compositor has none. A `panel` does not take keyboard focus,
/// so a key would not reach this process at all; a click routes to the window under the
/// pointer whatever holds focus, which is why the button is the half that can exist yet.
const APPS_BUTTON_W: u32 = 120;

/// The top bar's element tree.
fn bar_view() -> Element<()> {
    row(alloc::vec![
        sized(
            libdraw::geom::Size::new(APPS_BUTTON_W, 0),
            padding(Insets { top: 4, right: 8, bottom: 4, left: 8 }, text("applications")),
        ),
        padding(Insets { top: 4, right: 8, bottom: 4, left: 8 }, text("nitrox")),
    ])
}

/// The applications modal's size.
const MODAL_W: u32 = 320;
/// See [`MODAL_W`].
const MODAL_H: u32 = 240;
/// Bytes per row in the modal.
const MODAL_PITCH: usize = (MODAL_W as usize) * 4;
/// How tall one entry is.
const ROW_H: u32 = 20;

/// The applications modal's element tree: a filter field over a list of `/bin` programs.
fn modal_view(query: &TextFieldState, rows: &[ListRow<'_>], state: ListState) -> (Element<()>, ListState) {
    let palette = Palette::default();
    let field = text_field(query, false, WidgetState { active: true, ..Default::default() }, &palette);
    // The list is given the space left after the field, so `visible` matches what is drawn.
    let list_h = MODAL_H.saturating_sub(40);
    let (list, state) = list_view(rows, state, list_h, ROW_H, |_| (), &palette);
    (
        padding(
            Insets::all(8),
            column(alloc::vec![field, sized(libdraw::geom::Size::new(0, list_h), list)]),
        ),
        state,
    )
}

/// Render the modal.
fn render_modal(font: &Font, query: &TextFieldState, rows: &[ListRow<'_>], state: ListState) -> (MemFramebuffer, ListState) {
    let geometry = Geometry::with_pitch(MODAL_W, MODAL_H, MODAL_PITCH, PixelFormat::XRGB8888)
        .expect("the modal pitch is wide enough for a row");
    let mut fb = MemFramebuffer::new(geometry);
    let (ui, state) = modal_view(query, rows, state);
    let bounds = Rect::new(0, 0, MODAL_W, MODAL_H);
    let metrics = FontMetrics::new(font, FONT_PX);
    let l = layout(&ui, bounds, &metrics);
    let theme = Theme { font_px: FONT_PX, ..Theme::default() };
    paint(&mut fb, font, &theme, &ui, &l, bounds, &mut |_, _, _, _: &mut MemFramebuffer| {});
    (fb, state)
}

/// Read the programs `/bin` projects, as the modal's entries.
///
/// **`/bin` is a forwarded directory, not a set of bindings**, so `SYS_NS_ENUMERATE` does not
/// see inside it — that walks the namespace's own bindings and `/bin` is one of them. The
/// entries come from a directory session, the same way `list /bin` gets them.
fn read_bin(ns: u64) -> alloc::vec::Vec<alloc::string::String> {
    use librsproto::session::Dir;
    let mut names = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    let Ok(mut dir) = Dir::open(ns, b"/bin", &mut buf) else {
        kprint(b"desktop-shell: /bin did not open; the modal will be empty\n");
        return names;
    };
    let _ = dir.read_dir(|e| {
        if e.name != b"." && e.name != b".." {
            names.push(alloc::string::String::from_utf8_lossy(e.name).into_owned());
        }
        true
    });
    dir.close();
    names.sort();
    names
}

/// Render the top bar.
fn render_bar(font: &Font) -> MemFramebuffer {
    let geometry = Geometry::with_pitch(SCREEN_W, BAR_H, BAR_PITCH, PixelFormat::XRGB8888)
        .expect("the bar pitch is wide enough for a row");
    let mut fb = MemFramebuffer::new(geometry);
    let ui = bar_view();
    let bounds = Rect::new(0, 0, SCREEN_W, BAR_H);
    let metrics = FontMetrics::new(font, FONT_PX);
    let l = layout(&ui, bounds, &metrics);
    let theme = Theme { font_px: FONT_PX, ..Theme::default() };
    paint(&mut fb, font, &theme, &ui, &l, bounds, &mut |_, _, _, _: &mut MemFramebuffer| {});
    fb
}

/// Allocate a shared memory object of `len` bytes and map it writable.
fn shared_buffer(len: usize) -> Option<(u64, *mut u8)> {
    // SAFETY: a plain anonymous object of `len` bytes.
    let h = unsafe { syscall4(SYS_MEMORY_CREATE, len as u64, 0, 0, 0) };
    if h <= 0 {
        return None;
    }
    // SAFETY: maps the object read/write at a kernel-chosen address.
    let base = unsafe {
        syscall4(SYS_MEMORY_MAP, h as u64, 0, len as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if base < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
        return None;
    }
    Some((h as u64, base as usize as *mut u8))
}

/// Construct the namespace one application runs in, and return its handle.
///
/// **The load-bearing part of the shell**, and the reason it holds `BIND_NAMESPACE` at all.
/// `ui-composition-model.md` §5a rests the guarantee that *an application cannot compose other
/// applications* on the shell being the process that built their namespaces — so authority is
/// what the shell binds, not what an application asks for.
///
/// **`/dev/draw/new` is bound as its own path, with subtree base `/new`** — not the
/// `/dev/draw` subtree. That single choice is what closes `the `manage-ungated` deferral`, and it needs
/// no protocol change and no second endpoint:
///
/// - Resolving `/dev/draw/new` is an **exact match** against the binding, so the forwarded
///   suffix is empty, the base supplies the whole of it, and the compositor classifies `new`
///   and mints a session.
/// - Resolving `/dev/draw/manage` is **not a component-boundary prefix match** against a
///   binding of `/dev/draw/new` (`kernel/src/object/namespace.rs`, `match_suffix_offset`), so
///   nothing in this namespace answers it.
///
/// A first draft of this milestone specified a second forwarding endpoint for management,
/// couriered `init` → `service-mgr` → `desktop-session-mgr`, reasoning that the compositor
/// classifies by suffix with no caller identity so binding could not distinguish. Both
/// premises are true and the conclusion does not follow: what a namespace can *reach* is
/// decided by what it **binds**, not by how the server on the far side dispatches
/// (PR #225 review, finding 1).
///
/// **The caveat, because it decides how long this lasts.** A narrow bind expresses "`new` and
/// not `manage`". It cannot express "the `/dev/draw` subtree *minus* `manage`" — so the first
/// application that needs `/dev/draw/<id>/info` for ids it does not know in advance forces a
/// subtree bind, and `manage` comes back with it. Today nothing in `libsurface`, `libui`,
/// `libdraw` or `nxterm` resolves anything but `new`. The second endpoint is the fallback and
/// that is its trigger.
fn build_app_namespace(draw: u64) -> u64 {
    let ns = unsafe { syscall0(SYS_NS_CREATE) };
    if ns < 0 {
        kprint(b"desktop-shell: application ns_create FAIL\n");
        return 0;
    }
    let ns = ns as u64;

    // `/dev/draw/new`, narrow. See this function's doc for why the base is `/new`.
    let path = b"/dev/draw/new";
    let base = b"/new";
    // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
    let dr = unsafe {
        syscall6(
            SYS_NS_BIND,
            ns,
            path.as_ptr() as u64,
            path.len() as u64,
            draw,
            base.as_ptr() as u64,
            base.len() as u64,
        )
    };
    if dr != 0 {
        kprint(b"desktop-shell: application /dev/draw/new bind FAIL\n");
        // SAFETY: closing the namespace we just created.
        unsafe { syscall1(SYS_HANDLE_CLOSE, ns) };
        return 0;
    }

    // **No `/system/fonts` here yet, and the reason is a real gap rather than a choice.**
    // Re-binding it needs the fs-server *endpoint*, and this process does not have one: its
    // session namespace holds a `/system/fonts` **binding**, which resolves to a kernel
    // registration and never back to the endpoint that would let it be bound elsewhere. The
    // same asymmetry that stops an application re-binding `/bin` stops the shell here.
    //
    // Nothing this part launches renders text, so it costs nothing today. Part F is where it
    // bites — `nxterm` in an application namespace needs a font — and the fix is the trip the
    // compositor's endpoint already makes: `desktop-session-mgr` hands the shell the fs
    // endpoint at spawn.
    ns
}

/// Check the application namespace grants `new` and withholds `manage`, before anything runs
/// in it.
///
/// **Verified rather than assumed, and by the process that built it.** The narrow bind is the
/// whole of the `manage-ungated` deferral's answer, and it rests on a kernel matching rule
/// (`match_suffix_offset`) that this file does not own. A shell that constructed the namespace
/// wrongly and launched into it anyway would hand an application the manager channel — the
/// exact thing the deferral is about — and nothing downstream would notice, because an
/// application that *can* reach `manage` simply never says so.
///
/// Returns `false` if the namespace is not what it should be; the caller declines to launch.
fn verify_app_namespace(ns: u64) -> bool {
    let (new_st, new_h) = ns_lookup(ns, b"/dev/draw/new", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if new_h != 0 {
        // SAFETY: closing a session this check minted; the application will make its own.
        unsafe { syscall1(SYS_HANDLE_CLOSE, new_h) };
    }
    let (manage_st, manage_h) =
        ns_lookup(ns, b"/dev/draw/manage", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if manage_h != 0 {
        // SAFETY: closing a handle this check should never have obtained.
        unsafe { syscall1(SYS_HANDLE_CLOSE, manage_h) };
    }
    if new_st != 0 || new_h == 0 {
        kprint(b"desktop-shell: application namespace cannot reach /dev/draw/new\n");
        return false;
    }
    if manage_st == 0 && manage_h != 0 {
        kprint(b"desktop-shell: application namespace REACHES /dev/draw/manage -- refusing\n");
        return false;
    }
    kprint(b"desktop-shell: application namespace grants new, withholds manage\n");
    true
}

/// Resolve `path` in `ns`, returning `(status, handle)`.
///
/// **Async, like every potentially-blocking syscall here**: `SYS_NS_LOOKUP` returns a
/// `PendingOperation` to wait on, and the status and handle are read out of the wait result —
/// not an out-param. A first version of this wrote it synchronously and every resolve
/// "failed", which is what a `PendingOperation` handle looks like when you read it as a
/// status.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights)
    };
    if po < 0 {
        return (po as i32, 0);
    }
    if !wait_one(po as u64) {
        // SAFETY: closing our own PO.
        unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
        return (-1, 0);
    }
    // SAFETY: the kernel wrote one 24-byte IoResult: status at 8, handle at 16.
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([
                WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11],
            ]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    (status, if status == 0 { handle } else { 0 })
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
    unsafe {
        if RECV_COUNT == 0 { 0 } else { RECV_HANDLES[0] }
    }
}

/// Block until `h` is signalled.
fn wait_one(h: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
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

/// Wait set for the compositor's event channel.
static mut WAIT_HANDLES: [u64; 1] = [0; 1];
/// One 24-byte `IoResult`.
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// Bootstrap registers, as `libsession::spawn_leader` fills them: `rdi` = notification
/// channel, `rsi` = the **session** namespace, `rdx` = the Tier-1 setup channel carrying
/// `argv` and the environment, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, session_ns: u64, setup: u64, _arg0: u64) -> ! {
    kprint(b"desktop-shell: up (graphical session leader)\n");

    // Two messages arrive on the setup channel, in order: the Tier-1 `argv` + environment,
    // then the compositor's forwarding endpoint. The second is what lets this process build
    // application namespaces — a `/dev/draw` *binding* resolves to a kernel registration and
    // never back to an endpoint, so the shell cannot re-bind what its own namespace holds.
    let _ = recv_message(setup);
    let draw_endpoint = recv_handle(setup);
    if draw_endpoint == 0 {
        kprint(b"desktop-shell: no compositor endpoint; cannot launch applications\n");
    }

    // **Resolved from the session namespace, not from a root one.** This process has no root
    // handle: `spawn_leader` runs it in the namespace `desktop-session-mgr` constructed, which
    // is where `/dev/draw` was bound. That is the whole point — an application's namespace
    // will get a *narrower* bind, and the difference is what gates the manager channel.
    // SAFETY: `session_ns` is this process's namespace, live for its whole run.
    let font = match unsafe { libdraw::text::load(session_ns, SYSTEM_FONT_PATH) } {
        Ok(f) => f,
        Err(_) => fail(b"desktop-shell: font load FAILED (is /system readable in the session?)\n"),
    };

    // SAFETY: `session_ns` is live for this process's whole run.
    let transport = match unsafe { ChannelTransport::connect(session_ns) } {
        Ok(t) => t,
        Err(_) => fail(b"desktop-shell: connect to /dev/draw FAILED\n"),
    };
    let mut session = Session::new(transport);

    // `panel`, not `normal`: the role is what reserves the strut, so ordinary windows are
    // placed below the bar rather than under it. M6 Part A built that and nothing but a test
    // client has asked for it.
    //
    // `reserve` is stated separately from the height on purpose — the role's own doc explains
    // that deriving it would make a bar that reserves less than it occupies inexpressible.
    // A bar wants them equal.
    let role = Role::Panel { dock: Edge::Top, reserve: BAR_H };
    let window = match session.create(&CreateWindowRequest::new(SCREEN_W, BAR_H, role), BUFFERS) {
        Ok(id) => id,
        Err(_) => fail(b"desktop-shell: top bar CreateWindow FAILED\n"),
    };

    let picture = render_bar(&font).into_bytes();
    let len = BAR_PITCH * BAR_H as usize;
    if picture.len() != len {
        fail(b"desktop-shell: top bar render is not the size it declares\n");
    }
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            fail(b"desktop-shell: top bar buffer alloc FAILED\n");
        };
        // SAFETY: `addr` maps `len` writable bytes and `picture` holds exactly `len`; the two
        // regions are distinct allocations, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(picture.as_ptr(), addr, len) };
        let Some(mut w) = session.window(window) else {
            fail(b"desktop-shell: top bar window vanished\n");
        };
        if w.attach(i as u32, SCREEN_W, BAR_H, BAR_PITCH as u32, handle).is_err() {
            fail(b"desktop-shell: top bar AttachBuffer FAILED\n");
        }
    }
    let Some(mut w) = session.window(window) else {
        fail(b"desktop-shell: top bar window vanished\n");
    };
    if w.commit(0, (0, 0, SCREEN_W, BAR_H)).is_err() {
        fail(b"desktop-shell: top bar Commit FAILED\n");
    }
    // **Build one application namespace and check it**, before anything is launched into it.
    // Part E's applications modal is what will call this per launch; doing it once here is
    // what makes the narrow bind observable — and the shell refusing to launch when the check
    // fails is the behaviour, not the test.
    if draw_endpoint != 0 {
        let app_ns = build_app_namespace(draw_endpoint);
        if app_ns != 0 {
            let ok = verify_app_namespace(app_ns);
            // SAFETY: closing the namespace; nothing has been launched into it yet.
            unsafe { syscall1(SYS_HANDLE_CLOSE, app_ns) };
            if !ok {
                kprint(b"desktop-shell: application namespaces are not gated; not launching\n");
            }
        }
    }

    Line::new()
        .s(b"desktop-shell: top bar presented, window ")
        .u(window as u64)
        .s(b" ")
        .u(SCREEN_W as u64)
        .s(b"x")
        .u(BAR_H as u64)
        .end();

    // The modal's entries, read once. `desktop-shell.md` §4: they are `/bin` programs, and
    // that falls out of decisions already made — they are ordinary files in the namespace, so
    // type-to-filter runs over them with no special mechanism.
    let programs = read_bin(session_ns);
    Line::new()
        .s(b"desktop-shell: /bin lists ")
        .u(programs.len() as u64)
        .s(b" programs")
        .end();
    let mut modal: Option<u32> = None;
    let mut modal_addrs = [core::ptr::null_mut::<u8>(); BUFFERS];
    let query = TextFieldState::new();

    // Blocks on the compositor's event channel, never spins — a spinning leader keeps a run
    // queue non-empty, so the idle thread never runs and deferred reclamation stops for the
    // whole machine (the 2026-07-31 `logging-service` bug).
    let ev = session.wait_handle();
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
        unsafe {
            WAIT_HANDLES[0] = ev;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if session.pump().is_err() {
            fail(b"desktop-shell: compositor connection lost\n");
        }
        while let Some((w, event)) = session.next_event() {
            // A press on the applications button opens the modal. **A press, not a key**: a
            // `panel` takes no keyboard focus, so a key never reaches this process — see
            // `APPS_BUTTON_W`.
            if w == window && modal.is_none() {
                if let libsurface::WindowEvent::Pointer(p) = event {
                    if p.kind == librsproto::surface::POINTER_BUTTON
                        && p.flags & librsproto::surface::POINTER_PRESSED != 0
                        && p.x >= 0
                        && (p.x as u32) < APPS_BUTTON_W
                    {
                        modal = open_modal(&mut session, window, &font, &programs, &mut modal_addrs, &query);
                    }
                }
            }
        }
    }
}

/// Open the applications modal as a popup parented to the top bar.
///
/// **A `popup`, which is what M6 Part C made it possible to be.** A menu was a `Stack` layer
/// over its window until then, and worked only because it happened to fit inside one; a modal
/// wider than the bar it hangs from could not have been drawn that way at all. It is
/// positioned by its creator and clipped by the *screen*, not by its parent.
fn open_modal(
    session: &mut Session<ChannelTransport>,
    parent: u32,
    font: &Font,
    programs: &[alloc::string::String],
    addrs: &mut [*mut u8; BUFFERS],
    query: &TextFieldState,
) -> Option<u32> {
    let rows: alloc::vec::Vec<ListRow<'_>> = programs
        .iter()
        .enumerate()
        // Keyed by index into the **unfiltered** list, which is what `ListRow::key`'s doc asks
        // for: a filter reorders and shortens the rows, and an index into the filtered view
        // would pair row 2's widget with row 3's element the moment a character is typed.
        .map(|(i, name)| ListRow { key: i as u64, label: name.as_str() })
        .collect();
    let (picture, _) = render_modal(font, query, &rows, ListState::default());
    let bytes = picture.into_bytes();
    let len = MODAL_PITCH * MODAL_H as usize;
    if bytes.len() != len {
        kprint(b"desktop-shell: modal render is not the size it declares\n");
        return None;
    }
    let role = Role::Popup { parent };
    let id = match session.create(&CreateWindowRequest::new(MODAL_W, MODAL_H, role), BUFFERS) {
        Ok(id) => id,
        Err(_) => {
            kprint(b"desktop-shell: modal CreateWindow FAILED\n");
            return None;
        }
    };
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            kprint(b"desktop-shell: modal buffer alloc FAILED\n");
            return None;
        };
        addrs[i] = addr;
        // SAFETY: `addr` maps `len` writable bytes and `bytes` holds exactly `len`; distinct
        // allocations, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr, len) };
        let Some(mut w) = session.window(id) else {
            return None;
        };
        if w.attach(i as u32, MODAL_W, MODAL_H, MODAL_PITCH as u32, handle).is_err() {
            kprint(b"desktop-shell: modal AttachBuffer FAILED\n");
            return None;
        }
    }
    let Some(mut w) = session.window(id) else {
        return None;
    };
    if w.commit(0, (0, 0, MODAL_W, MODAL_H)).is_err() {
        kprint(b"desktop-shell: modal Commit FAILED\n");
        return None;
    }
    Line::new()
        .s(b"desktop-shell: applications modal open, window ")
        .u(id as u64)
        .s(b" listing ")
        .u(programs.len() as u64)
        .end();
    Some(id)
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
