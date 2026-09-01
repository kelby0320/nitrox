//! `nxfiles` — the file browser: the window, the buffers, and the event pump.
//!
//! Everything with behaviour is in the library half and host-tested there. What is here is the
//! part that needs an OS: connecting to `/dev/draw`, sharing buffers with the compositor,
//! reading a directory, draining input.
//!
//! ## The one thing this file decides
//!
//! **Where the browser starts.** `HOME` from the Tier-1 environment record, which
//! `desktop-shell::build_app_namespace` binds to the user's subtree — so `/home` here is the
//! user's own directory and not the `/home` above it. An application launched with no setup
//! message at all falls back to `/`, which is the honest answer for a process that was told
//! nothing: it can still list what its namespace contains.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::{Font, load_ui};
use libkern::{exit, kprint};
use librsproto::surface::{CreateWindowRequest, Role};
use libsurface::buffers::BufferPool;
use libsurface::{Session, WindowEvent, ipc::ChannelTransport};
use libui::diff::Tree;
use libui::layout::{Layout, layout};
use libui::paint::{FontMetrics, Theme, paint};
use libui::route::Router;
use nxfiles::{App, Entry, Msg, TITLE};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// `alloc` backing — the element tree and the listing both allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Buffers shared with the compositor. Two is the minimum the protocol permits.
const BUFFERS: usize = 2;


/// Report and end the run.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: under the test-harness kernel this terminates QEMU; elsewhere it is
    // `Unsupported` and falls through.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
}

/// A private framebuffer of `size` to compose a frame into.
///
/// Drawn here and copied into whichever buffer is free, for the reason `nxterm` gives: the
/// toolkit's damage describes what changed since the *last frame*, and the free buffer holds
/// the frame before that.
fn compose_buffer(size: Size) -> Option<MemFramebuffer> {
    let pitch = (size.w as usize).checked_mul(4)?;
    Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888).map(MemFramebuffer::new)
}

/// Read `path` and hand the listing to `app`.
///
/// **A failed listing leaves the browser where it was**, saying so on the debug console rather
/// than clearing the window: a directory that cannot be read is a thing that happens — a
/// permission a namespace does not carry, a path that went away between the press and the read
/// — and a browser that emptied itself in response would lose the one thing the user could
/// still act on, which is the listing they came from.
fn navigate(app: &mut App, ns: u64, path: &str) {
    match libfs::list_dir(ns, path.as_bytes()) {
        Ok(entries) => {
            let rows: Vec<Entry> = entries.iter().filter_map(App::entry_of).collect();
            libkern::debug::Line::new()
                .s(b"nxfiles: listed ")
                .untrusted(path.as_bytes())
                .s(b" - ")
                .u(rows.len() as u64)
                .s(b" entries")
                .end();
            app.show(path, rows);
        }
        Err(_) => {
            libkern::debug::Line::new()
                .s(b"nxfiles: cannot list ")
                .untrusted(path.as_bytes())
                .end();
        }
    }
}

/// Ask `desktop-shell` to open `path`, and say whether it took the request.
///
/// **A session per request, rather than one held for the browser's run.** The shell allows a
/// bounded number of `/dev/desktop` sessions and this one is used once per file opened — where a
/// held session would be a slot occupied for the whole run of every browser window, to save a
/// resolve that happens at human speed. It is also the shape that cannot leak one: the session
/// is closed on every path out of this function.
///
/// The answer is about the *request*: the shell replies once it has launched something, which is
/// before that program has read anything. What the file turned out to be is the opener's to
/// report, in its own window.
fn ask_shell_to_open(root_ns: u64, path: &str) -> bool {
    use librsproto::desktop::Desktop;
    let mut buf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
    let Ok(mut desktop) = Desktop::connect(root_ns, &mut buf) else {
        kprint(b"nxfiles: no /dev/desktop; cannot open anything\n");
        return false;
    };
    let ok = desktop.open(path.as_bytes()).is_ok();
    desktop.close();
    if ok {
        libkern::debug::Line::new().s(b"nxfiles: asked to open ").s(path.as_bytes()).end();
    } else {
        libkern::debug::Line::new().s(b"nxfiles: could not open ").s(path.as_bytes()).end();
    }
    ok
}

/// Paint `damage` of `app` into `fb`.
fn draw(
    fb: &mut MemFramebuffer,
    ui: &libui::element::Element<Msg>,
    l: &Layout,
    font: &Font,
    theme: &Theme,
    damage: Rect,
) {
    // No `custom` nodes: everything this application draws is a widget the toolkit owns, which
    // is the difference between it and `nxterm` — and the point of building it second.
    paint(fb, font, theme, ui, l, damage, &mut |_, _, _, _: &mut MemFramebuffer| {});
}

/// `HOME` from the setup record, or `/` for a process that was told nothing.
fn home_of(env: &libstream::wire::Record) -> String {
    env.schema
        .fields
        .iter()
        .position(|f| f.name == "HOME")
        .and_then(|i| env.values.get(i))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| String::from("/"))
}

/// Block until the compositor has something to say.
///
/// One handle, unlike `nxterm`'s two: a browser has no second source of work. It reads a
/// directory when it is told to and otherwise waits on the session channel alone.
fn wait_one(h: u64) {
    let handles = [h];
    let mut results = [0u8; 24];
    // SAFETY: a valid one-handle array and a result buffer sized for one record.
    unsafe {
        libkern::syscall4(
            libkern::SYS_WAIT,
            handles.as_ptr() as u64,
            1,
            results.as_mut_ptr() as u64,
            u64::MAX,
        )
    };
}

/// The theme the shell handed this application, or the built-in one.
///
/// **Absent is normal rather than an error**: a client started outside a graphical session — by
/// `init`, or by a test harness — gets no setup record at all, and a desktop that refused to draw
/// without one would be a client that cannot be run on its own.
fn theme_of(env: &libstream::wire::Record) -> Theme {
    env.field_str("THEME").map(|s| Theme::from_config(s).0).unwrap_or_default()
}

/// Entry point.
///
/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, endpoint: u64, arg0: u64) -> ! {
    kprint(b"nxfiles: up\n");

    let env = match libstream::setup::bootstrap(notif, root_ns, endpoint, arg0).setup() {
        Some(Ok(s)) => s.env,
        _ => libstream::wire::Record::default(),
    };
    let start = home_of(&env);
    // **From the environment, not from a default** (M11 Part C), and read *here* — beside the
    // other thing the session tells this program — rather than at the point it is first drawn
    // with. A value logged where it is learned appears in the order a reader expects it, which
    // is what an assertion about it can be placed against.
    let theme = theme_of(&env);
    // **Said out loud because a theme that arrives is otherwise invisible to a gate.** Colours
    // are pixels and `check-login` boots a release image with no rendered grid to read; the size
    // is one number that came from a file, and printing it is what makes the whole path — disk,
    // shell, setup record, client — assertable from a console.
    libkern::debug::Line::new().s(b"nxfiles: theme font_px ").u(theme.font_px as u64).end();

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let (font, _) = match unsafe { load_ui(root_ns, &theme, b"nxfiles") } {
        Ok(loaded) => loaded,
        Err(e) => {
            libkern::debug::Line::new().s(b"nxfiles: the UI font ").s(e.why()).end();
            fail(b"nxfiles: font load FAILED\n");
        }
    };

    let mut app = App::new(&start);
    navigate(&mut app, root_ns, &start);

    let mut size = app.window_size();
    // SAFETY: `root_ns` is this process's live root namespace.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        // Boxed for the reason every client here boxes it: ~9 KiB of message buffers has no
        // business in a stack frame beside everything else.
        Ok(t) => Box::new(t),
        Err(_) => fail(b"nxfiles: connect to /dev/draw FAILED\n"),
    };
    let mut win = Session::new(transport);
    let window_id =
        match win.create(&CreateWindowRequest::new(size.w, size.h, Role::Normal), BUFFERS) {
            Ok(w) => w,
            Err(_) => fail(b"nxfiles: CreateWindow FAILED\n"),
        };
    if let Some(mut w) = win.window(window_id)
        && w.set_title(TITLE).is_err()
    {
        kprint(b"nxfiles: SetTitle refused\n");
    }

    let mut scratch = match compose_buffer(size) {
        Some(fb) => fb,
        None => fail(b"nxfiles: impossible window geometry\n"),
    };
    let mut pool = {
        let Some(mut w) = win.window(window_id) else {
            fail(b"nxfiles: our own window is gone\n");
        };
        match BufferPool::new(&mut w, size, BUFFERS) {
            Some(p) => p,
            None => fail(b"nxfiles: buffer alloc FAILED\n"),
        }
    };

    let mut bounds = Rect::new(0, 0, size.w, size.h);
    let mut tree = Tree::new();
    let mut router = Router::new();
    let ev = win.wait_handle();

    loop {
        // ---- render ----
        // The widget under the pointer, from the router that has always known and that nothing
        // had ever asked (M11 Part E batch 3).
        let ui = app.view(&theme, router.hovered_key(&tree));
        let l = layout(&ui, bounds, &FontMetrics::new(&font, theme.font_px));
        let damage = match tree.update(&ui, &l) {
            Ok(d) => d,
            // A malformed tree is a bug in `view`, not a runtime condition.
            Err(_) => fail(b"nxfiles: the view is not diffable\n"),
        };
        if let Some(d) = damage {
            draw(&mut scratch, &ui, &l, &font, &theme, d);
            let Some(mut w) = win.window(window_id) else {
                fail(b"nxfiles: our own window is gone\n");
            };
            let Ok(b) = pool.acquire(&mut w, app.window_size()) else {
                fail(b"nxfiles: no buffer to draw into\n");
            };
            if !pool.write(b, scratch.bytes()) {
                fail(b"nxfiles: the frame did not fit its buffer\n");
            }
            if w.commit(b, (d.origin.x as u32, d.origin.y as u32, d.size.w, d.size.h)).is_err() {
                fail(b"nxfiles: Commit FAILED\n");
            }
        }

        // ---- the requests this frame owes the compositor ----
        if app.take_move_request()
            && let Some(mut w) = win.window(window_id)
            && w.start_move().is_err()
        {
            kprint(b"nxfiles: the compositor refused the move\n");
        }
        if let Some(edges) = app.take_resize_request()
            && let Some(mut w) = win.window(window_id)
            && w.start_resize(edges).is_err()
        {
            kprint(b"nxfiles: the compositor refused the resize\n");
        }
        if let Some(state) = app.take_state_request()
            && let Some(mut w) = win.window(window_id)
            && w.request_state(state).is_err()
        {
            kprint(b"nxfiles: the state request was refused\n");
        }
        // **The listing is read here, not in `update`.** `update` is a function of values; a
        // directory read is a syscall, and the application says where it wants to be rather
        // than going there itself.
        // **Asking the shell to open a file, which is where a browser's authority stops.** It
        // has no `/bin`, no way to build a namespace and no business spawning anything; what it
        // has is `/dev/desktop`, bound into every application namespace by the shell that built
        // it. So the browser names a path and the shell decides what opens it (M10 Part D).
        if let Some(path) = app.take_open() {
            let ok = ask_shell_to_open(root_ns, &path);
            app.opened(&path, ok);
        }
        if let Some(to) = app.take_goto() {
            navigate(&mut app, root_ns, &to);
            // **Round again rather than waiting**, or the listing just installed is not drawn
            // until the *next* event happens to arrive. In practice that is the key's own
            // release a moment later, which is why hand testing and the gate both pass — and
            // why it is worth removing rather than reasoning about (PR #257 review, finding 4).
            continue;
        }

        // **Asked to close, by its own button or by the shell.**
        if app.closing() {
            kprint(b"nxfiles: closing\n");
            exit(0);
        }

        // ---- events ----
        if win.events_pending() == 0 {
            wait_one(ev);
        }
        let mut events = Vec::new();
        loop {
            match win.poll_event() {
                Ok(Some(e)) => events.push(e),
                Ok(None) => break,
                Err(_) => {
                    kprint(b"nxfiles: the compositor went away\n");
                    exit(0);
                }
            }
        }
        let mut resized = false;
        for (from, event) in events {
            if from != window_id {
                continue;
            }
            match event {
                WindowEvent::Key(k) => {
                    if let Some(msg) = router.key(&tree, &ui, k) {
                        app.update(msg);
                    } else {
                        // Arrow keys and Enter are the browser's own, not any widget's: nothing
                        // in this tree is focusable that would want them, and a listing a person
                        // cannot drive from the keyboard is one they have to aim at.
                        app.update(Msg::Key(k));
                    }
                }
                WindowEvent::Pointer(p) => {
                    let (msgs, _) = router.pointer(&tree, &ui, &l, p);
                    for m in msgs {
                        app.update(m);
                    }
                    // **The distance the toolkit does not measure.** `libui` routes messages,
                    // and what makes a press a *drag* is how far it has travelled since — so the
                    // application reads the record it already has. `buttons` is on every pointer
                    // record for exactly this reason (M10 Part E).
                    if app.pointer_moved(p.x, p.y, p.buttons)
                        && let Some((entry, path)) = app.take_drag()
                    {
                        let kind = if entry.is_dir {
                            librsproto::surface::DROP_KIND_DIR
                        } else {
                            librsproto::surface::DROP_KIND_FILE
                        };
                        if let Some(mut w) = win.window(window_id) {
                            match w.start_drag(kind, &path, &entry.name) {
                                Ok(()) => libkern::debug::Line::new()
                                    .s(b"nxfiles: dragging ")
                                    .untrusted(entry.name.as_bytes())
                                    .end(),
                                // Refused means the pointer is not holding this window — the
                                // press ended between the motion and this request, which is
                                // ordinary rather than an error.
                                Err(_) => kprint(b"nxfiles: the compositor refused the drag\n"),
                            }
                        }
                    }
                }
                WindowEvent::Focus(f) => {
                    router.set_window_focused(f);
                    app.focused = f;
                }
                WindowEvent::Configure { width, height, .. } => {
                    if app.resize(Size::new(width, height)) {
                        resized = true;
                        libkern::debug::Line::new()
                            .s(b"nxfiles: resized to ")
                            .u(u64::from(width))
                            .s(b"x")
                            .u(u64::from(height))
                            .end();
                    }
                }
                // **The shell asking, answered the way the close button is.** Exiting is the
                // whole of it: the kernel closes this process's handles and the compositor
                // tears its windows down with its session.
                WindowEvent::CloseRequested => {
                    kprint(b"nxfiles: asked to close, exiting\n");
                    app.update(Msg::Close);
                }
                // **Everything accumulated about held keys is a guess now.** This client keeps
                // none, so there is nothing to discard — and saying so is the point: a client
                // that silently ignored this would be wrong the moment it started tracking
                // anything.
                // **This browser offers drags and takes none** (M10 Part E). Moving a file by
                // dropping it into another folder is a file *operation*, which is M12's along
                // with rename, delete and copy — and a window that declared an acceptor without
                // implementing one would be highlighted for drops it then swallowed.
                WindowEvent::Drop { ref name, .. } => {
                    libkern::debug::Line::new()
                        .s(b"nxfiles: ignoring a drop of ")
                        .untrusted(name.as_bytes())
                        .end();
                }
                WindowEvent::InputLost => kprint(b"nxfiles: input dropped\n"),
            }
        }
        if resized {
            size = app.window_size();
            match compose_buffer(size) {
                Some(fb) => scratch = fb,
                None => fail(b"nxfiles: impossible window geometry\n"),
            }
            bounds = Rect::new(0, 0, size.w, size.h);
            // A tree diffed against a layout from the old bounds reports damage in the old
            // coordinates; starting again reports the whole window, which is what a resize is.
            tree = Tree::new();
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxfiles: panic\n");
    exit(2);
}
