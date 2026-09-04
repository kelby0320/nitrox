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


use libdraw::geom::{Rect, Size};
use libdraw::text::{Font, load_ui};
use libkern::{exit, kprint};
use librsproto::surface::{Role};

use libsurface::{Session, WindowEvent, ipc::ChannelTransport};

use libui::layout::{layout, locate};
use libui::paint::{FontMetrics, Theme};

use libui::window::Child;
use libui::menu::{Item, KeyOutcome};
use nxfiles::{
    App, Entry, FileOp, Gesture, MENU_BAR_KEY, MENU_COUNT, Msg, TITLE,
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

/// Tell the compositor what a newly created window of this browser is.
///
/// **Extracted so a second window gets the same treatment** (M14 Part B): it was inline before
/// the loop, which was fine while a window was created once and would have been the first thing
/// New Window quietly failed to do.
fn dress<T: libsurface::Transport>(win: &mut Session<T>, id: u32) {
    if let Some(mut w) = win.window(id)
        && w.set_title(TITLE).is_err()
    {
        kprint(b"nxfiles: SetTitle refused\n");
    }
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

    let size = app.window_size();
    // SAFETY: `root_ns` is this process's live root namespace.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        // Boxed for the reason every client here boxes it: ~9 KiB of message buffers has no
        // business in a stack frame beside everything else.
        Ok(t) => Box::new(t),
        Err(_) => fail(b"nxfiles: connect to /dev/draw FAILED\n"),
    };
    let mut win = Session::new(transport);
    // **The window is a `libui::window::Child`** (M14 Part B), top-level role and all — the same
    // value the menu and the dialog have always been. Everything a window needs to be drawn and
    // routed lives in there, so a second window is a second value rather than a second copy of
    // this loop.
    let top = {
        let ui = app.view(&theme, None);
        match Child::open_sized(
            &mut win,
            Role::Normal,
            (0, 0),
            size,
            &ui,
            &font,
            &theme,
            BUFFERS,
        ) {
            Some(t) => t,
            None => fail(b"nxfiles: CreateWindow FAILED\n"),
        }
    };
    dress(&mut win, top.id());

    let bounds = Rect::new(0, 0, size.w, size.h);
    // The open menu's window, and which menu it is showing — two things because the *same*
    // popup cannot serve both: choosing `Edit` while `File` is open has to replace the window,
    // not redraw it at the other anchor.
    let menu: Option<Child> = None;
    let menu_shown: Option<usize> = None;
    let menu_hovered: Option<u64> = None;
    // The delete question's window, alive only while one is being asked (M12 Part A's shape).
    let confirm: Option<Child> = None;
    let confirm_hovered: Option<u64> = None;
    let ev = win.wait_handle();
    // The name prompt's receipt, reported on change the way `nxedit` reports its buffer's.
    let reported_prompt = app.prompt_len();

    /// Everything one window of this browser is.
    ///
    /// **A window, not the application** (M14 Part B). Its own panes and tabs, its own surface
    /// and retained tree, its own menu and its own question, and its own receipt. The `Session`,
    /// the font and the theme are not here: one connection, one face and one palette serve every
    /// window this process owns.
    struct Win {
        top: Child,
        app: App,
        size: Size,
        bounds: Rect,
        menu: Option<Child>,
        menu_shown: Option<usize>,
        menu_hovered: Option<u64>,
        confirm: Option<Child>,
        confirm_hovered: Option<u64>,
        reported_prompt: Option<usize>,
    }

    /// Open a window of this browser, dressed and ready to be serviced.
    ///
    /// **The same path for the first window and every later one**, so a New Window cannot end up
    /// subtly different from the original — an untitled bar being the obvious way.
    fn open_window<T: libsurface::Transport>(
        win: &mut Session<T>,
        mut app: App,
        font: &Font,
        theme: &Theme,
    ) -> Option<Win> {
        let size = app.window_size();
        let ui = app.view(theme, None);
        let top = Child::open_sized(win, Role::Normal, (0, 0), size, &ui, font, theme, BUFFERS)?;
        dress(win, top.id());
        let reported_prompt = app.prompt_len();
        Some(Win {
            top,
            app,
            size,
            bounds: Rect::new(0, 0, size.w, size.h),
            menu: None,
            menu_shown: None,
            menu_hovered: None,
            confirm: None,
            confirm_hovered: None,
            reported_prompt,
        })
    }

    let mut quit_pending = false;
    let mut wins = alloc::vec![Win {
        top,
        app,
        size,
        bounds,
        menu,
        menu_shown,
        menu_hovered,
        confirm,
        confirm_hovered,
        reported_prompt,
    }];

    loop {
        // **Every window this process owns, each serviced exactly as one used to be.**
        //
        // Destructured rather than dotted through, which is what keeps this loop readable across
        // the conversion: every name below means what it meant when there was one window.
        // Set by a window that owes a frame *now* — see the sites that set it.
        let mut redraw_now = false;
        for wi in 0..wins.len() {
        let Win {
            top,
            app,
            // The resize acts on the window whose `Configure` it was, in the dispatch below.
            size: _,
            bounds,
            menu,
            menu_shown,
            menu_hovered,
            confirm,
            confirm_hovered,
            reported_prompt,
        } = &mut wins[wi];
        let window_id = top.id();
        // ---- render ----
        // The widget under the pointer, from the router that has always known and that nothing
        // had ever asked (M11 Part E batch 3).
        let ui = app.view(&theme, top.hovered_key());
        let l = layout(&ui, *bounds, &FontMetrics::new(&font, theme.font_px));
        // Where each menu drops from, read every frame rather than when one opens: a bar item's
        // position is a fact about the layout, and before the first one there is nowhere to put
        // a popup at all.
        app.menus.set_anchors(
            (0..MENU_COUNT).map(|i| locate(&ui, &l, MENU_BAR_KEY + i as u64)).collect(),
        );
        // The layout is computed here rather than inside `present` because the menu bar's
        // anchors are read from it — see `present_laid_out`.
        if !top.present_laid_out(&mut win, &ui, &l, &font, &theme) {
            fail(b"nxfiles: the window could not be drawn\n");
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
        if *menu_shown != app.menus.open() {
            if let Some(m) = menu.take() {
                m.close(&mut win);
            }
            *menu_hovered = None;
            *menu_shown = app.menus.open();
            if let Some(which) = menu_shown {
                match app.menus.anchor() {
                    Some(a) => {
                        let view = app.menu_view(*which, &theme, None);
                        *menu = Child::open(
                            &mut win,
                            Role::Popup { parent: window_id },
                            a,
                            &view,
                            &font,
                            &theme,
                            BUFFERS,
                        );
                    }
                    None => *menu = None,
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
                        app.menus.close();
                        *menu_shown = None;
                    }
                }
            }
        }
        if let Some(m) = menu.as_mut() {
            let now = m.hovered_key();
            if now != *menu_hovered {
                *menu_hovered = now;
                // **Which row the pointer is over, unconditionally** (M14 Part A). `check-login`
                // boots the release image, so a `test-harness` line would not exist in the binary
                // it runs — and since the File menu grew a separator, dividing the popup's height
                // by a row count names the wrong row. The gate walks down and stops when this
                // says it is over the one it means to click. A key is a number this program
                // chose: not a label, not a position, and nothing anybody typed.
                let mut l = libkern::debug::Line::new();
                l.s(b"nxfiles: menu hover ");
                match now {
                    Some(k) => {
                        l.u(k);
                    }
                    None => {
                        l.s(b"none");
                    }
                }
                l.end();
            }
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
        if typed != *reported_prompt {
            *reported_prompt = typed;
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
                *confirm = Child::open(
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
                *confirm_hovered = None;
            }
            _ => {}
        }
        if let Some(c) = confirm.as_mut() {
            let now = c.hovered_key();
            *confirm_hovered = now;
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
            navigate(app, root_ns, &here);
            // The notice `navigate` cleared is the answer to what just happened, so it is put
            // back after the listing rather than before it.
            app.operated(said);
            // **Round again without waiting**, which is what this `continue` meant
            // when the body serviced one window. Inside the per-window loop it means
            // "next window", and the wait below would then block with a frame owed —
            // the stale-listing bug PR #257 removed, reintroduced by the conversion
            // (PR #283 review, worth fixing 4). The flag is what carries the old
            // meaning across the new shape.
            redraw_now = true;
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
            navigate(app, root_ns, &to);
            // **Round again rather than waiting**, or the listing just installed is not drawn
            // until the *next* event happens to arrive. In practice that is the key's own
            // release a moment later, which is why hand testing and the gate both pass — and
            // why it is worth removing rather than reasoning about (PR #257 review, finding 4).
            // **Round again without waiting**, which is what this `continue` meant
            // when the body serviced one window. Inside the per-window loop it means
            // "next window", and the wait below would then block with a frame owed —
            // the stale-listing bug PR #257 removed, reintroduced by the conversion
            // (PR #283 review, worth fixing 4). The flag is what carries the old
            // meaning across the new shape.
            redraw_now = true;
            continue;
        }

        // **Asked to close, by its own button or by the shell.**

        }

        // ---- what the windows asked for ----
        //
        // **After every window has been serviced**, never inside that loop: creating or
        // destroying a window while iterating over them is the one thing that shape cannot do.
        let mut opened = 0usize;
        for w in wins.iter_mut() {
            if w.app.take_new_window() {
                opened += 1;
            }
            if w.app.take_quit() {
                kprint(b"nxfiles: quitting\n");
                quit_pending = true;
            }
        }
        for _ in 0..opened {
            // **A new window starts at home**, not at this window's directory: New Window is a
            // second browser, and one that opened where the first happens to be looking would be
            // a copy rather than a window.
            match open_window(&mut win, App::new(&start), &font, &theme) {
                Some(w) => {
                    kprint(b"nxfiles: opened another window\n");
                    wins.push(w);
                }
                None => kprint(b"nxfiles: could not open another window\n"),
            }
        }
        // **Quit closes every window.** This browser has nothing unsaved to ask about, so there
        // is no dialog to cancel and no window that can refuse — the editor is where decision 4's
        // interesting half lives, and this is the same rule with nothing in its way.
        if quit_pending {
            for w in wins.iter_mut() {
                w.app.update(Msg::Close);
            }
        }
        let mut i = 0;
        while i < wins.len() {
            if wins[i].app.closing() {
                let w = wins.remove(i);
                if let Some(m) = w.menu {
                    m.close(&mut win);
                }
                if let Some(c) = w.confirm {
                    c.close(&mut win);
                }
                w.top.close(&mut win);
                kprint(b"nxfiles: closed a window\n");
            } else {
                i += 1;
            }
        }
        if wins.is_empty() {
            kprint(b"nxfiles: closing\n");
            exit(0);
        }

        // **A frame is owed, so do not block for an event.** Without this the window that
        // asked would not repaint until the next event happened to arrive — in practice the
        // key's own release, which is why this reads as a lag rather than a hang.
        if redraw_now {
            continue;
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
        for (from, event) in events {
            // **Which window is this for?** A record naming none of them is dropped rather than
            // routed into whichever window happened to be first.
            let Some(wi) = wins.iter().position(|w| {
                w.top.id() == from
                    || w.menu.as_ref().is_some_and(|m| m.id() == from)
                    || w.confirm.as_ref().is_some_and(|c| c.id() == from)
            }) else {
                continue;
            };
            let Win {
                top,
                app,
                size,
                bounds,
                menu,
                menu_shown,
                menu_hovered,
                confirm,
                confirm_hovered,
                ..
            } = &mut wins[wi];
            let window_id = top.id();
            // Rebuilt for *this* window: routing is against this window's tree, and another
            // window's would report a widget that is not the one under the pointer. **No layout
            // here** — `Child::route` lays the content out itself, and this browser has no
            // `drop_at` to hand one to.
            let ui = app.view(&theme, top.hovered_key());
            let mut resized = false;
            // **The menu's window routes through the menu's tree.** Same `App`, so a row's `Msg`
            // updates the same state a click in the list does; a different tree and router,
            // because they describe a different window.
            if menu.as_ref().is_some_and(|m| m.id() == from) {
                // A press landed outside the menu, so it goes away — the one thing a popup's
                // owner cannot work out for itself, because it never sees a press aimed
                // elsewhere (M11 Part E batch 5).
                if matches!(event, WindowEvent::Dismissed) {
                    app.menus.close();
                    continue;
                }
                // **Arrows, Esc and Enter drive the open menu**, which is possible only here: a
                // popup holds the keyboard while it is up, so these arrive naming *its* window
                // and never reach the browser's router below. `Chose` carries the row, and the
                // message comes from the same table the popup drew — so Enter and a click on the
                // same row cannot do different things.
                if let WindowEvent::Key(k) = event {
                    let table = app.menu_table();
                    match app.menus.key(&k, &table) {
                        KeyOutcome::Chose { menu, item: i } => {
                            // **From the outcome rather than from `menu_shown`**, which happened
                            // to be right here and is a second copy of the same fact.
                            if let Some(msg) = table
                                .get(menu)
                                .and_then(|m| m.items.get(i))
                                .and_then(|it| match it {
                                    Item::Action { msg, enabled: true, .. } => Some(msg.clone()),
                                    _ => None,
                                })
                            {
                                app.update(msg);
                            }
                            continue;
                        }
                        KeyOutcome::Dismissed | KeyOutcome::Changed => continue,
                        KeyOutcome::Ignored => {}
                    }
                }
                let msgs = match (*menu_shown, menu.as_mut()) {
                    (Some(which), Some(m)) => {
                        let view = app.menu_view(which, &theme, *menu_hovered);
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
                let ask = app.confirm_view(&theme, *confirm_hovered);
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
                    if let Some(msg) = top.route_key(&ui, k) {
                        app.update(msg);
                    } else {
                        // Arrow keys and Enter are the browser's own, not any widget's: nothing
                        // in this tree is focusable that would want them, and a listing a person
                        // cannot drive from the keyboard is one they have to aim at.
                        app.update(Msg::Key(k));
                    }
                }
                WindowEvent::Pointer(p) => {
                    let msgs = top.route(&ui, &font, &theme, &WindowEvent::Pointer(p));
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
                    top.route(&ui, &font, &theme, &WindowEvent::Focus(f));
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
        if resized {
            *size = app.window_size();
            // `Child::resize` reallocates and throws the retained tree away: a tree diffed
            // against a layout from the old bounds reports damage in the old coordinates, and
            // starting again reports the whole window, which is what a resize is.
            if top.resize(*size) == Some(false) {
                fail(b"nxfiles: impossible window geometry\n");
            }
            *bounds = Rect::new(0, 0, size.w, size.h);
        }
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxfiles: panic\n");
    exit(2);
}
