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
use libui::layout::{Layout, layout, locate};
use libui::paint::{FontMetrics, Theme, paint};
use libui::route::Router;
use libui::window::Child;
use nxfiles::{
    App, EDIT_MENU_KEY, Entry, FILE_MENU_KEY, FileOp, Gesture, Menu, Msg, TITLE,
};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// `alloc` backing — the element tree and the listing both allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Buffers shared with the compositor. Two is the minimum the protocol permits.
const BUFFERS: usize = 2;


/// Carry out a filesystem operation, and say in one line what happened.
///
/// **The browser performs these itself.** It holds `/home` — the session bound it there — and
/// that binding *is* the authority these need; routing them through `desktop-shell` would be
/// asking a supervisor to do what the application is already entitled to do, which is the
/// opposite of the argument that keeps `Desktop::Open` on the shell's side. Opening a *program*
/// needs `/bin` and a namespace, and this has neither; renaming a file in a directory it can
/// already list needs nothing it does not hold.
///
/// The returned string is what the strip shows, so it is written for the person looking at the
/// window rather than for a log.
fn perform(ns: u64, op: &FileOp) -> &'static str {
    // **The destination is tested first, and that is what makes "nothing overwrites" true.**
    // Three of the four refusals this used to claim were unreachable, and one of them was worse
    // than unreachable (PR #268 review, worth fixing 1):
    //
    // - `libfs::create_file` is documented *idempotent* — `fs-server-ext4` resolves with
    //   `RESOLVE_CREATE`, discards "already exists" and grows the file to zero, which is a
    //   no-op. So *new file* onto an existing name **succeeded**, destroyed nothing, and told
    //   the person a new empty file was there while the old one and its contents still were.
    // - `libfs::rename`'s `map_rename_error` deliberately does not distinguish an occupied
    //   destination — its own doc says a caller that cares should test with `file_size` first,
    //   which is what the `copy` coreutil does. So a refused rename or move was correct and
    //   reported "could not rename it", which reads like a fault rather than an answer.
    //
    // Testing here gives all of them the sentence written for them, and puts the promise in this
    // program rather than in what a server happens to return.
    let taken = |path: &String| {
        libfs::file_size(ns, path.as_bytes()).is_some() || libfs::is_dir(ns, path.as_bytes())
    };
    match op {
        FileOp::Create { path, dir } => {
            if taken(path) {
                return "that name is taken";
            }
            let made = if *dir {
                libfs::mkdir(ns, path.as_bytes()).is_ok()
            } else {
                libfs::create_file(ns, path.as_bytes()).is_ok()
            };
            if made { "created" } else { "could not create it" }
        }
        // **Never `replace`**, for rename, copy and move alike: overwriting is a second
        // question, and a browser that answered it silently would be one whose most ordinary
        // mistake — typing a name that is already there — destroys a file.
        FileOp::Rename { from, to } => {
            if taken(to) {
                return "that name is taken";
            }
            match libfs::rename(ns, from.as_bytes(), to.as_bytes(), false) {
                Ok(()) => "renamed",
                Err(_) => "could not rename it",
            }
        }
        FileOp::MoveInto { from, to } => {
            if taken(to) {
                return "there is one there already";
            }
            match libfs::rename(ns, from.as_bytes(), to.as_bytes(), false) {
                Ok(()) => "moved",
                Err(_) => "could not move it",
            }
        }
        // **`copy_file`, which maps both sides and copies between the mappings** with no heap at
        // all, bounded by `libfs::MAX_COPY`. `read_file` is the one function here that allocates
        // a whole file, and the wrong one for a copy to call. A *folder* takes `copy_tree`, which
        // was always there — `copy_file` on one merely fails.
        FileOp::Copy { from, to, dir } => {
            if taken(to) {
                return "that name is taken";
            }
            if *dir {
                return match libfs::copy_tree(ns, from.as_bytes(), to.as_bytes(), false, &mut |_, _, _| {})
                {
                    Ok(()) => "copied",
                    Err(_) => "could not copy it",
                };
            }
            match libfs::copy_file(ns, from.as_bytes(), to.as_bytes(), false) {
                Ok(_) => "copied",
                Err(libfs::FileError::TooLarge) => "too large to copy",
                Err(_) => "could not copy it",
            }
        }
        FileOp::Delete { path, dir } => {
            let r = if *dir {
                libfs::remove_tree(ns, path.as_bytes(), &mut |_, _| {}).is_ok()
            } else {
                libfs::unlink_at(ns, path.as_bytes()).is_ok()
            };
            if r { "deleted" } else { "could not delete it" }
        }
    }
}

/// The path an operation acts on, for the line that reports it.
fn subject(op: &FileOp) -> &str {
    match op {
        FileOp::Create { path, .. } | FileOp::Delete { path, .. } => path,
        FileOp::Rename { to, .. } | FileOp::Copy { to, .. } | FileOp::MoveInto { to, .. } => to,
    }
}

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
    // The open menu's window, and which menu it is showing — two things because the *same*
    // popup cannot serve both: choosing `Edit` while `File` is open has to replace the window,
    // not redraw it at the other anchor.
    let mut menu: Option<Child> = None;
    let mut menu_shown: Option<Menu> = None;
    let mut menu_hovered: Option<u64> = None;
    // The delete question's window, alive only while one is being asked (M12 Part A's shape).
    let mut confirm: Option<Child> = None;
    let mut confirm_hovered: Option<u64> = None;
    let ev = win.wait_handle();
    // The name prompt's receipt, reported on change the way `nxedit` reports its buffer's.
    let mut reported_prompt = app.prompt_len();

    loop {
        // ---- render ----
        // The widget under the pointer, from the router that has always known and that nothing
        // had ever asked (M11 Part E batch 3).
        let ui = app.view(&theme, router.hovered_key(&tree));
        let l = layout(&ui, bounds, &FontMetrics::new(&font, theme.font_px));
        // Where each menu drops from, read every frame rather than when one opens: a bar item's
        // position is a fact about the layout, and before the first one there is nowhere to put
        // a popup at all.
        app.menu_anchor = [locate(&ui, &l, FILE_MENU_KEY), locate(&ui, &l, EDIT_MENU_KEY)];
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
        // **The dialog's own title bar, on the dialog's own window.** A `StartMove` names a
        // window id, so this cannot share the branch above.
        if app.take_confirm_move()
            && let Some(c) = confirm.as_ref()
            && let Some(mut w) = win.window(c.id())
            && w.start_move().is_err()
        {
            kprint(b"nxfiles: the compositor refused the dialog's move\n");
        }

        // ---- the menu's window ----
        //
        // Opened and destroyed with the menu, because `popup` is a transient role: the
        // compositor takes it with its parent, it holds the keyboard while it is up, and a
        // hidden one would still be a window in the stack.
        if menu_shown != app.menu() {
            if let Some(m) = menu.take() {
                m.close(&mut win);
            }
            menu_hovered = None;
            menu_shown = app.menu();
            if let Some(which) = menu_shown {
                let anchor = app.menu_anchor[usize::from(which != Menu::File)];
                match anchor {
                    Some(a) => {
                        let view = app.menu_view(which, &theme, None);
                        menu = Child::open(
                            &mut win,
                            Role::Popup { parent: window_id },
                            (a.origin.x, a.bottom() as i32),
                            &view,
                            &font,
                            &theme,
                            BUFFERS,
                        );
                    }
                    None => menu = None,
                }
                match menu.as_ref() {
                    // **Where it is and how big, unconditionally.** `check-login` boots the
                    // release image, so a `test-harness` line would not exist in the binary it
                    // runs — and this is the only thing that says the menu became a window. The
                    // gate divides the height by the number of rows rather than deriving one
                    // from the theme's text size, which is the metric that is *not* fixed.
                    Some(m) => {
                        let o = win
                            .window(m.id())
                            .and_then(|w| w.configured())
                            .map_or((0, 0), |c| (c.x, c.y));
                        libkern::debug::Line::new()
                            .s(b"nxfiles: menu popup ")
                            .u(m.id() as u64)
                            .s(b" at ")
                            .i(o.0 as i64)
                            .s(b",")
                            .i(o.1 as i64)
                            .s(b" ")
                            .u(m.size().w as u64)
                            .s(b"x")
                            .u(m.size().h as u64)
                            .end();
                    }
                    // Not fatal: the browser is usable without its menu, and saying so beats a
                    // window that silently never appears.
                    None => {
                        kprint(b"nxfiles: could not open the menu\n");
                        app.dismiss_menu();
                        menu_shown = None;
                    }
                }
            }
        }
        if let Some(m) = menu.as_mut() {
            let now = m.hovered_key();
            menu_hovered = now;
            let view = menu_shown.map(|w| app.menu_view(w, &theme, now));
            if let Some(view) = view
                && !m.present(&mut win, &view, &font, &theme)
            {
                kprint(b"nxfiles: the menu could not be drawn\n");
            }
        }

        // **One line per character typed into the name prompt.** Injection is relative and
        // unacknowledged, so without a receipt a gate types at whatever speed it likes and finds
        // out about a dropped keystroke as a wrong filename several steps later. A count, not
        // the text — what somebody is naming a file is theirs, and the listing shows it anyway.
        //
        // **Emitted here rather than in `render`, which is where it was**, so the *first* one —
        // `0 chars`, the one a gate waits on before it starts typing — comes out after the menu
        // popup has been told to close rather than before. A `popup` holds the keyboard while it
        // is up, and the prompt is a keyboard *mode* inside this window rather than a window of
        // its own, so a key injected between the receipt and the close was routed to the menu and
        // dropped. `check-login` timed out on that (PR #278 review, blocking 1).
        //
        // **What it proves and what it does not.** The destroy has been *sent* when this prints;
        // the compositor processing it is a separate round trip this cannot see. That is strictly
        // stronger than before and is not a guarantee — `compositor: focus win=… has=1` would be
        // the real one, and `log_route` caps routed-input lines at eight, long past by here.
        let typed = app.prompt_len();
        if typed != reported_prompt {
            reported_prompt = typed;
            if let Some(n) = typed {
                libkern::debug::Line::new()
                    .s(b"nxfiles: name so far ")
                    .u(n as u64)
                    .s(b" chars")
                    .end();
            }
        }

        // ---- the question's window ----
        match (app.confirming().is_some(), confirm.is_some()) {
            (true, false) => {
                // **Said before the window is asked for.** A dialog's first `Configure` is held
                // for the manager, so a line printed after `Child::open` returned would be
                // downstream of the shell and racing it to the console (M12 Part A, PR #267).
                kprint(b"nxfiles: asking before deleting\n");
                let ask = app.confirm_view(&theme, None);
                confirm = Child::open(
                    &mut win,
                    Role::Dialog { parent: window_id },
                    // (0, 0): this client does not know where it is on screen, and a dialog's
                    // offset is a preference the manager overrides anyway.
                    (0, 0),
                    &ask,
                    &font,
                    &theme,
                    BUFFERS,
                );
                if confirm.is_none() {
                    kprint(b"nxfiles: could not open the confirmation dialog\n");
                    app.confirm_failed();
                }
            }
            (false, true) => {
                if let Some(c) = confirm.take() {
                    c.close(&mut win);
                }
                confirm_hovered = None;
            }
            _ => {}
        }
        if let Some(c) = confirm.as_mut() {
            let now = c.hovered_key();
            confirm_hovered = now;
            let ask = app.confirm_view(&theme, now);
            if !c.present(&mut win, &ask, &font, &theme) {
                kprint(b"nxfiles: the confirmation dialog could not be drawn\n");
            }
        }

        // **The filesystem work, here because it is syscalls.** `update` produced a value saying
        // what should happen; this is where it happens, and the listing is read again afterwards
        // so that what the person sees is what is on disk rather than what was asked for.
        if let Some(op) = app.take_op() {
            let said = perform(root_ns, &op);
            libkern::debug::Line::new()
                .s(b"nxfiles: ")
                .s(said.as_bytes())
                .s(b" ")
                .untrusted(subject(&op).as_bytes())
                .end();
            app.operated(said);
            let here = String::from(app.path());
            navigate(&mut app, root_ns, &here);
            // The notice `navigate` cleared is the answer to what just happened, so it is put
            // back after the listing rather than before it.
            app.operated(said);
            continue;
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
            // **The menu's window routes through the menu's tree.** Same `App`, so a row's `Msg`
            // updates the same state a click in the list does; a different tree and router,
            // because they describe a different window.
            if menu.as_ref().is_some_and(|m| m.id() == from) {
                // A press landed outside the menu, so it goes away — the one thing a popup's
                // owner cannot work out for itself, because it never sees a press aimed
                // elsewhere (M11 Part E batch 5).
                if matches!(event, WindowEvent::Dismissed) {
                    app.dismiss_menu();
                    continue;
                }
                let msgs = match (menu_shown, menu.as_mut()) {
                    (Some(which), Some(m)) => {
                        let view = app.menu_view(which, &theme, menu_hovered);
                        m.route(&view, &font, &theme, &event)
                    }
                    _ => Vec::new(),
                };
                for msg in msgs {
                    app.update(msg);
                }
                continue;
            }
            if confirm.as_ref().is_some_and(|c| c.id() == from) {
                let ask = app.confirm_view(&theme, confirm_hovered);
                let mut msgs = confirm
                    .as_mut()
                    .map(|c| c.route(&ask, &font, &theme, &event))
                    .unwrap_or_default();
                match event {
                    // `Esc` is the dialog's, and nothing else is: no key deletes.
                    WindowEvent::Key(k) => msgs.extend(app.confirm_key(k)),
                    WindowEvent::Focus(f) => app.confirm_focused = f,
                    // A dialog is not dismissed by a press elsewhere — that is a popup's event,
                    // and a question stays until it is answered.
                    WindowEvent::Dismissed => {}
                    // A manager asking the *dialog* to close means the same as its own close
                    // button: the question goes away and the entry does not.
                    WindowEvent::CloseRequested => msgs.push(Msg::KeepIt),
                    _ => {}
                }
                for msg in msgs {
                    // Unconditional, because `check-login` boots the release image: these two
                    // lines are the only outside sign of which answer was given.
                    match msg {
                        Msg::ConfirmDelete => kprint(b"nxfiles: confirmed the delete\n"),
                        Msg::KeepIt => kprint(b"nxfiles: delete cancelled\n"),
                        _ => {}
                    }
                    app.update(msg);
                }
                continue;
            }
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
                    // **A drag is this window's until it leaves it** (M12 Part B). Past the
                    // slop the browser tracks the row under the pointer itself and the payload
                    // never reaches the compositor; only a pointer that goes outside hands the
                    // gesture over. The compositor could not have delivered an internal drop
                    // anyway — it skips the source window when it looks for a target.
                    match app.pointer_moved(p.x, p.y, p.buttons) {
                        Gesture::HandOff => {
                            if let Some((entry, path)) = app.take_drag() {
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
                                        // Refused means the pointer is not holding this window
                                        // — the press ended between the motion and this
                                        // request, which is ordinary rather than an error.
                                        Err(_) => {
                                            kprint(b"nxfiles: the compositor refused the drag\n")
                                        }
                                    }
                                }
                            }
                        }
                        // The op is taken below, with every other one.
                        Gesture::Dropped | Gesture::Moved | Gesture::None => {}
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
                // A dismissal is a popup's event; this window is not one.
                WindowEvent::Dismissed => {}
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
