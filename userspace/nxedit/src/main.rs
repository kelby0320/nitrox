//! `nxedit` — the text editor: the window, the file, and the event pump.
//!
//! Everything with behaviour is in the library half and host-tested there. What is here is the
//! part that needs an OS: reading the file, writing it back safely, connecting to `/dev/draw`,
//! sharing buffers with the compositor, draining input.
//!
//! ## The one thing this file decides
//!
//! **How a save fails.** [`save`] writes a temporary beside the target and renames it over —
//! the sequence `coreutils` has used since Milestone 3.5, and the reason `libfs::rename` was
//! already there to be used. A save that truncated and then failed would leave neither the old
//! file nor the new one; a rename either happened or did not, and the file the person can still
//! see on screen is the buffer, which is kept whatever the answer.
//!
//! **Where the path comes from.** `argv[1]`, which is what `desktop-shell` puts there when a
//! client asks it to open something (`Desktop::Open`). With no argument there is nothing to
//! edit and the editor says so rather than opening an untitled buffer it could never save.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::{Font, SYSTEM_FONT_PATH, load};
use libkern::{exit, kprint};
use librsproto::surface::{CreateWindowRequest, Role};
use libsurface::buffers::BufferPool;
use libsurface::{Session, WindowEvent, ipc::ChannelTransport};
use libui::diff::Tree;
use libui::layout::{Layout, layout};
use libui::paint::{FontMetrics, Theme, paint};
use libui::route::Router;
use nxedit::{App, Msg, to_bytes};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// `alloc` backing — the element tree and the buffer both allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Buffers shared with the compositor. Two is the minimum the protocol permits.
const BUFFERS: usize = 2;

/// Text size, matching the toolkit's default theme.
const FONT_PX: f32 = 16.0;

/// The suffix a save's temporary carries.
///
/// **Beside the target, not in a temporary directory**, because a rename is only atomic within
/// one filesystem: a temporary elsewhere would be a cross-device rename, which the kernel
/// refuses as one operation and which would degrade the safe sequence into a copy.
const TEMP_SUFFIX: &str = ".nxedit-tmp";

/// Report and end the run.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: under the test-harness kernel this terminates QEMU; elsewhere it is
    // `Unsupported` and falls through.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
}

/// A private framebuffer of `size` to compose a frame into.
fn compose_buffer(size: Size) -> Option<MemFramebuffer> {
    let pitch = (size.w as usize).checked_mul(4)?;
    Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888).map(MemFramebuffer::new)
}

/// Read the file into `app`, or say why it could not be.
///
/// **A path that is not there is not a failure** — opening one is how a file gets made — but a
/// path that is a *directory*, or one that fails to read for any other reason, blocks the
/// buffer. See `App::blocked`: the danger is the empty window, which saved over a file is that
/// file destroyed by an editor that never showed it.
fn open_into(app: &mut App, ns: u64, path: &str) {
    if libfs::is_dir(ns, path.as_bytes()) {
        app.blocked("that is a directory");
        kprint(b"nxedit: refusing to edit a directory\n");
        return;
    }
    match libfs::read_file(ns, path.as_bytes()) {
        Ok(bytes) => match core::str::from_utf8(&bytes) {
            Ok(text) => {
                app.loaded(text, &bytes);
                libkern::debug::Line::new()
                    .s(b"nxedit: opened ")
                    .s(path.as_bytes())
                    .s(b" - ")
                    .u(bytes.len() as u64)
                    .s(b" bytes")
                    .end();
            }
            // **Refused rather than mangled.** A lossy conversion would show a file that is not
            // the file, and saving it would write the mangling back.
            Err(_) => {
                app.blocked("not text");
                kprint(b"nxedit: refusing to edit a file that is not UTF-8\n");
            }
        },
        Err(libfs::FileError::NotFound) => {
            app.absent();
            libkern::debug::Line::new().s(b"nxedit: new file ").s(path.as_bytes()).end();
        }
        Err(_) => {
            app.blocked("could not be read");
            libkern::debug::Line::new().s(b"nxedit: cannot read ").s(path.as_bytes()).end();
        }
    }
}

/// Write `bytes` to `path` the safe way: a temporary beside it, renamed over it.
///
/// The error is the message the status strip shows, so it is written for the person looking at
/// the window rather than for a log.
fn save(ns: u64, path: &str, bytes: &[u8]) -> Result<usize, &'static str> {
    let mut temp = String::from(path);
    temp.push_str(TEMP_SUFFIX);
    if libfs::write_file(ns, temp.as_bytes(), bytes).is_err() {
        // **The temporary is removed on the way out**, or a failed save leaves a file beside the
        // one being edited that nobody asked for and the browser then lists.
        let _ = libfs::unlink_at(ns, temp.as_bytes());
        return Err("the file could not be written");
    }
    if libfs::rename(ns, temp.as_bytes(), path.as_bytes(), true).is_err() {
        let _ = libfs::unlink_at(ns, temp.as_bytes());
        return Err("the file could not be replaced");
    }
    Ok(bytes.len())
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
    // No `custom` nodes: everything this application draws is a widget the toolkit owns.
    paint(fb, font, theme, ui, l, damage, &mut |_, _, _, _: &mut MemFramebuffer| {});
}

/// Block until the compositor has something to say.
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

/// Entry point.
///
/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, endpoint: u64, arg0: u64) -> ! {
    kprint(b"nxedit: up\n");

    // **`argv` and nothing else from the setup message.** `nxfiles` reads `HOME` from the
    // environment because a browser with no argument still has somewhere to start; an editor
    // with no file has nothing to edit, so the path is the whole of what this program is told.
    let argv = match libstream::setup::bootstrap(notif, root_ns, endpoint, arg0).setup() {
        Some(Ok(s)) => s.argv,
        _ => Vec::new(),
    };
    // **`argv[1]` or nothing.** An editor with no file has nothing to save to, and an untitled
    // buffer would be a promise this application cannot keep: there is no save-as, because there
    // is no file dialog and no way to ask for a name.
    let Some(path) = argv.get(1).cloned() else {
        kprint(b"nxedit: no file to edit (argv[1] is the path)\n");
        exit(2);
    };

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let font = match unsafe { load(root_ns, SYSTEM_FONT_PATH) } {
        Ok(f) => f,
        Err(_) => fail(b"nxedit: could not load the system font\n"),
    };

    let mut app = App::new(&path);
    open_into(&mut app, root_ns, &path);
    // **The file being edited can change**, since a drop replaces it — so this is the binary's
    // copy of `app.path()` rather than the argument it started from.
    let mut editing = path.clone();

    let mut size = app.window_size();
    // SAFETY: `root_ns` is this process's live root namespace.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => Box::new(t),
        Err(_) => fail(b"nxedit: connect to /dev/draw FAILED\n"),
    };
    let mut win = Session::new(transport);
    let window_id =
        match win.create(&CreateWindowRequest::new(size.w, size.h, Role::Normal), BUFFERS) {
            Ok(w) => w,
            Err(_) => fail(b"nxedit: CreateWindow FAILED\n"),
        };
    // **Set once, and it is the name alone.** The modified marker lives in the window's own
    // title bar; retitling on every keystroke would be a message per keystroke to say something
    // the window already shows.
    if let Some(mut w) = win.window(window_id)
        && w.set_title(libfs::basename_str(&path)).is_err()
    {
        kprint(b"nxedit: SetTitle refused\n");
    }
    // **What this window takes, said once** (M10 Part E). Files only: a directory has no
    // contents to put in a buffer, and an editor that accepted one would have to invent an
    // answer for it. The compositor matches against this while the pointer moves, so a drag
    // carrying a folder is never highlighted over this window at all.
    if let Some(mut w) = win.window(window_id)
        && w.declare_acceptor(nxedit::ACCEPTOR, librsproto::surface::DROP_KIND_FILE).is_err()
    {
        kprint(b"nxedit: DeclareAcceptor refused\n");
    }

    let mut scratch = match compose_buffer(size) {
        Some(fb) => fb,
        None => fail(b"nxedit: impossible window geometry\n"),
    };
    let mut pool = {
        let Some(mut w) = win.window(window_id) else {
            fail(b"nxedit: our own window is gone\n");
        };
        match BufferPool::new(&mut w, size, BUFFERS) {
            Some(p) => p,
            None => fail(b"nxedit: buffer alloc FAILED\n"),
        }
    };

    let theme = Theme { font_px: FONT_PX, ..Theme::default() };
    let mut bounds = Rect::new(0, 0, size.w, size.h);
    let mut tree = Tree::new();
    let mut router = Router::new();
    let ev = win.wait_handle();
    let mut reported = app.revision();

    loop {
        // **One line per edit, and it carries a count rather than the text.** A gate driving a
        // release image cannot read this window — the pixels are the only echo an editor has —
        // so without a receipt it would type at whatever speed it liked and discover a dropped
        // keystroke as a wrong file three steps later. What somebody types into an editor is
        // theirs; the number says a keystroke landed and nothing else.
        let rev = app.revision();
        if rev != reported {
            reported = rev;
            libkern::debug::Line::new().s(b"nxedit: buffer rev ").u(rev).end();
        }
        // ---- render ----
        let ui = app.view();
        let l = layout(&ui, bounds, &FontMetrics::new(&font, FONT_PX));
        let damage = match tree.update(&ui, &l) {
            Ok(d) => d,
            // A malformed tree is a bug in `view`, not a runtime condition.
            Err(_) => fail(b"nxedit: the view is not diffable\n"),
        };
        if let Some(d) = damage {
            draw(&mut scratch, &ui, &l, &font, &theme, d);
            let Some(mut w) = win.window(window_id) else {
                fail(b"nxedit: our own window is gone\n");
            };
            let Ok(b) = pool.acquire(&mut w, app.window_size()) else {
                fail(b"nxedit: no buffer to draw into\n");
            };
            if !pool.write(b, scratch.bytes()) {
                fail(b"nxedit: the frame did not fit its buffer\n");
            }
            if w.commit(b, (d.origin.x as u32, d.origin.y as u32, d.size.w, d.size.h)).is_err() {
                fail(b"nxedit: Commit FAILED\n");
            }
        }

        // ---- the requests this frame owes ----
        if app.take_move_request()
            && let Some(mut w) = win.window(window_id)
            && w.start_move().is_err()
        {
            kprint(b"nxedit: the compositor refused the move\n");
        }
        if let Some(edges) = app.take_resize_request()
            && let Some(mut w) = win.window(window_id)
            && w.start_resize(edges).is_err()
        {
            kprint(b"nxedit: the compositor refused the resize\n");
        }
        if let Some(state) = app.take_state_request()
            && let Some(mut w) = win.window(window_id)
            && w.request_state(state).is_err()
        {
            kprint(b"nxedit: the state request was refused\n");
        }
        // **The write happens here, not in `update`.** `update` is a function of values; writing
        // a file is a syscall, so the application says it wants to save and the `main` that owns
        // the namespace performs it — the same outbox `nxfiles` uses for a directory read.
        if let Some(text) = app.take_save() {
            let bytes = to_bytes(&text);
            let result = save(root_ns, &editing, &bytes);
            match result {
                Ok(n) => libkern::debug::Line::new()
                    .s(b"nxedit: saved ")
                    .s(editing.as_bytes())
                    .s(b" - ")
                    .u(n as u64)
                    .s(b" bytes")
                    .end(),
                Err(why) => libkern::debug::Line::new()
                    .s(b"nxedit: save FAILED for ")
                    .s(editing.as_bytes())
                    .s(b" - ")
                    .s(why.as_bytes())
                    .end(),
            }
            app.saved(result);
            // Round again rather than waiting: the status strip has changed and nothing else is
            // going to arrive to prompt a redraw.
            continue;
        }

        if app.closing() {
            kprint(b"nxedit: closing\n");
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
                    kprint(b"nxedit: the compositor went away\n");
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
                    // **Every key reaches the buffer today, and the router branch is a
                    // placeholder rather than live routing.** `Router::key` returns early
                    // without a focused widget, this editor focuses none, and no toolkit widget
                    // sets `on_key` — so the `if` has never once been taken. It is kept because
                    // the first widget that wants a key (a find field) needs exactly this shape,
                    // and reading it as dispatch that already happens is the mistake: today the
                    // save button has **no keyboard path**, which is worth knowing before M12
                    // adds one (PR #259 review, optional 7).
                    if let Some(msg) = router.key(&tree, &ui, k) {
                        app.update(msg);
                    } else {
                        app.update(Msg::Key(k));
                    }
                }
                WindowEvent::Pointer(p) => {
                    let (msgs, _) = router.pointer(&tree, &ui, &l, p);
                    for m in msgs {
                        app.update(m);
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
                            .s(b"nxedit: resized to ")
                            .u(u64::from(width))
                            .s(b"x")
                            .u(u64::from(height))
                            .end();
                    }
                }
                // **Answered by exiting, and the unsaved buffer goes with it.** An editor with
                // somewhere to put a question would ask it — that is what a `CloseRequested`
                // rather than a destruction is *for* — and this one has no dialog to ask in.
                // Naming the gap is the honest half; the confirmation belongs with the rest of
                // the editor's second pass, which M12 owns.
                WindowEvent::CloseRequested => {
                    kprint(b"nxedit: asked to close, exiting\n");
                    app.update(Msg::Close);
                }
                // **Somebody dragged a file onto this window** (M10 Part E). Routed by position
                // like a press, so the text area takes it and the title bar does not — and the
                // buffer is only given up if there is nothing to lose.
                WindowEvent::Drop { ref path, ref name, x, y, .. } => {
                    let taken = router.drop_at(&tree, &ui, &l, x, y).is_some();
                    libkern::debug::Line::new()
                        .s(b"nxedit: drop of ")
                        .s(name.as_bytes())
                        .s(if taken { b" on the document" as &[u8] } else { b" outside it" })
                        .end();
                    if taken && app.accept_drop(path) {
                        open_into(&mut app, root_ns, path);
                        // The path is the window's identity now: the title, and what a save
                        // writes to. Both are read from `app`, so the one copy `main` keeps has
                        // to follow — this was a `let` bound once at startup.
                        editing = String::from(app.path());
                        if let Some(mut w) = win.window(window_id)
                            && w.set_title(libfs::basename_str(&editing)).is_err()
                        {
                            kprint(b"nxedit: SetTitle refused\n");
                        }
                    }
                }
                WindowEvent::InputLost => kprint(b"nxedit: input dropped\n"),
            }
        }
        if resized {
            size = app.window_size();
            match compose_buffer(size) {
                Some(fb) => scratch = fb,
                None => fail(b"nxedit: impossible window geometry\n"),
            }
            bounds = Rect::new(0, 0, size.w, size.h);
            tree = Tree::new();
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxedit: panic\n");
    exit(2);
}
