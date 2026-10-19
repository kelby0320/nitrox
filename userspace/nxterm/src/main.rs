//! `nxterm` — the terminal client: the window, the buffers, and the event pump.
//!
//! The logic is in the library half; this half needs an OS. A frame's damage is the union of
//! `libui`'s one rectangle and `libterm`'s list of cell rows, and the `custom` node's paint
//! callback turns its clip back into rows.

#![no_std]
#![no_main]

extern crate alloc;

mod backend;

use libdraw::framebuffer::{Framebuffer, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::{Font, load_mono, load_ui};
use libkern::debug::Line;
use libkern::{exit, kprint};
use librsproto::clipboard::{CLIP_ANY_SERIAL, CLIP_KIND_TEXT, Clipboard, MAX_CLIP_BYTES};
use librsproto::surface::Role;

use libsurface::{Session, WindowEvent, ipc::ChannelTransport};
use libterm::render::Metrics;

use libui::damage::union_opt;
use libui::layout::layout;
use libui::paint::FontMetrics;
use libui::paint::Theme;

use libui::menu::{Item, KeyOutcome, Menu};
use libui::window::Child;
use nxterm::{App, GRID_KEY, GRID_KIND, Msg, rows_in};

/// `alloc` backing — the element tree, the grid and the render all allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Buffers shared with the compositor; two lets a frame be drawn while the other is on screen.
const BUFFERS: usize = 2;

/// The terminal's size in cells at startup; a `Configure` changes it.
const COLS: usize = 80;
const ROWS: usize = 20;

/// Report and end the run.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: under the test-harness kernel this terminates QEMU; elsewhere it is
    // `Unsupported` and falls through.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
}

/// The message on row `i` of menu `open`, if that row is an enabled action.
///
/// Read from `App::menu_table`, as a click's message is, so Enter and a click cannot disagree.
fn chosen(table: &[Menu<Msg>], open: Option<usize>, i: usize) -> Option<Msg> {
    match table.get(open?)?.items.get(i)? {
        Item::Action { msg, enabled: true, .. } => Some(*msg),
        _ => None,
    }
}

/// Say which item a menu produced, under the harness; `check-terminal` matches `menu chose`.
#[allow(unused_variables, reason = "`msg` is read only when the test harness is compiled in")]
fn chose(msg: Msg) {
    #[cfg(feature = "test-harness")]
    {
        let name: &[u8] = match msg {
            Msg::Copy => b"Copy",
            Msg::Paste => b"Paste",
            Msg::Clear => b"Clear",
            Msg::Reset => b"Reset",
            Msg::Close => b"Close Window",
            _ => return,
        };
        libkern::debug::Line::new().s(b"nxterm: menu chose ").s(name).end();
    }
}

/// Print the text of grid row `row`, trailing blanks trimmed.
/// The harness's window onto the grid, which `check-terminal` matches as `grid> `.
#[cfg(feature = "test-harness")]
fn report_row(app: &App, row: usize) {
    let mut buf = [0u8; 256];
    let mut n = 0;
    for col in 0..app.grid().cols() {
        let Some(cell) = app.grid().view_cell(app.view_line(), row, col) else { break };
        let mut enc = [0u8; 4];
        let s = cell.ch.encode_utf8(&mut enc);
        if n + s.len() > buf.len() {
            break;
        }
        buf[n..n + s.len()].copy_from_slice(s.as_bytes());
        n += s.len();
    }
    while n > 0 && buf[n - 1] == b' ' {
        n -= 1;
    }
    if n > 0 {
        libkern::debug::Line::new().s(b"nxterm: grid> ").s(&buf[..n]).end();
    }
}

/// Block until any of `handles` has something.
///
/// One `sys_wait` over all of them, so neither a quiet shell nor a quiet keyboard blocks the other.
fn wait_any(handles: &[u64]) -> bool {
    let mut results = alloc::vec![0u8; handles.len() * libkern::abi::WAIT_RESULT_SIZE];
    // SAFETY: a valid handle array and a result buffer sized for one record per handle.
    let n = unsafe {
        libkern::syscall4(
            libkern::SYS_WAIT,
            handles.as_ptr() as u64,
            handles.len() as u64,
            results.as_mut_ptr() as u64,
            u64::MAX,
        )
    };
    // Checked: the kernel rejects a `count` above `MAX_WAIT_HANDLES` and returns at once, so
    // ignoring the answer would spin the render loop. `MAX_TABS` keeps the count under the limit.
    n as i64 > 0
}

/// Tell the compositor what a newly created window of this terminal is called.
fn dress<T: libsurface::Transport>(win: &mut Session<T>, id: u32) {
    if let Some(mut w) = win.window(id)
        && w.set_title(nxterm::TITLE).is_err()
    {
        kprint(b"nxterm: SetTitle refused\n");
    }
}

/// The theme the shell handed this application, or the built-in one.
fn theme_of(env: &libstream::wire::Record) -> Theme {
    env.field_str("THEME").map(|s| Theme::from_config(s).0).unwrap_or_default()
}

/// Push `bytes` onto the kill ring, connecting per operation: a session would cost the server a
/// wait-set slot.
fn clipboard_copy(ns: u64, bytes: &[u8]) -> Result<(), &'static str> {
    let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
    let mut clip = Clipboard::connect(ns, &mut buf).map_err(|_| "no /dev/clipboard")?;
    let r = clip.copy(CLIP_KIND_TEXT, bytes).map(|_| ());
    clip.close();
    r.map_err(|_| "the clipboard refused it")
}

/// Read the newest entry into `out`; returns how many bytes it wrote.
fn clipboard_paste(ns: u64, out: &mut [u8]) -> Result<usize, &'static str> {
    let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
    let mut clip = Clipboard::connect(ns, &mut buf).map_err(|_| "no /dev/clipboard")?;
    let r = clip.paste(0, CLIP_ANY_SERIAL, out);
    clip.close();
    match r {
        Ok((_, _, len)) => Ok(len.min(out.len())),
        // An empty ring is not a failure: nothing was copied, so there is nothing to type.
        Err(e) if e.is_empty() => Ok(0),
        Err(_) => Err("the clipboard refused it"),
    }
}

/// Entry point: the kernel's ELF entry, with the standard bootstrap arguments.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, endpoint: u64, arg0: u64) -> ! {
    kprint(b"nxterm: up\n");

    // Its own setup message, so the shell it hosts gets a real environment. Absent is normal (a
    // Tier-0 spawn has none): an empty `Record`.
    let env = match libstream::setup::bootstrap(notif, root_ns, endpoint, arg0).setup() {
        Some(Ok(s)) => s.env,
        Some(Err(_)) => libstream::wire::Record::default(),
        None => libstream::wire::Record::default(),
    };

    // Read here, before anything is sized with it: the grid's cells are built from `font_px`.
    let theme = theme_of(&env);

    // Two fonts: the menu bar and popup are widgets and take the proportional face; the grid needs
    // the fixed-advance one, as `libterm` measures a cell from one glyph's advance.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let (ui_font, _) = match unsafe { load_ui(root_ns, &theme, b"nxterm") } {
        Ok(loaded) => loaded,
        Err(e) => {
            libkern::debug::Line::new().s(b"nxterm: the UI font ").s(e.why()).end();
            fail(b"nxterm: font load FAILED\n");
        }
    };
    // SAFETY: as above.
    let (mono_font, mono_path) = match unsafe { load_mono(root_ns, &theme, b"nxterm") } {
        Ok(loaded) => loaded,
        Err(e) => {
            libkern::debug::Line::new().s(b"nxterm: the grid font ").s(e.why()).end();
            fail(b"nxterm: font load FAILED\n");
        }
    };
    let metrics = Metrics::new(&mono_font, theme.font_px);
    // Logged for `check-terminal`, which re-measures the same face on the host and compares: a
    // proportional face here gives a plausible cell and a misaligned grid the guest cannot see.
    // `mono_path` is the path the load returned (the built-in one if the theme's would not open),
    // and the size is two integers because `.u()` truncates; `from_config` rounds to hundredths.
    let (px_whole, px_cents) = libdraw::theme::px_parts(theme.font_px);
    libkern::debug::Line::new()
        .s(b"nxterm: grid font ")
        .s(mono_path.as_str().as_bytes())
        .s(b", cell ")
        .u(metrics.cell_w as u64)
        .s(b"x")
        .u(metrics.cell_h as u64)
        .s(b" at ")
        .u(px_whole)
        .s(b".")
        // Zero-padded: `.5` hundredths is `.05`.
        .s(if px_cents < 10 { &b"0"[..] } else { &b""[..] })
        .u(px_cents)
        .s(b"px")
        .end();
    let mut app = App::new(COLS, ROWS, metrics);
    // `check-login` reads this to tell that a maximise grew the grid.
    libkern::debug::Line::new()
        .s(b"nxterm: started, grid ")
        .u(app.grid().cols() as u64)
        .s(b"x")
        .u(app.grid().rows() as u64)
        .end();

    let size = app.window_size();
    // SAFETY: `root_ns` is this process's live root namespace.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => fail(b"nxterm: connect to /dev/draw FAILED\n"),
    };
    // A session, not a window: a client may hold several windows on one connection.
    let mut win = Session::new(transport);
    // The window is a `libui::window::Child` with the top-level role; a second window is a second
    // value.
    let top = {
        let ui = app.view(&theme, None);
        match Child::open_sized(&mut win, Role::Normal, (0, 0), size, &ui, &ui_font, &theme, BUFFERS) {
            Some(t) => t,
            None => fail(b"nxterm: CreateWindow FAILED\n"),
        }
    };
    dress(&mut win, top.id());

    let bounds = Rect::new(0, 0, size.w, size.h);
    // The menu's window is a `popup`, not a layer: `libui`'s `offset` clips at its parent's edge,
    // and a menu clipped to its window is not a menu. Only the screen clips it; it is destroyed,
    // not hidden, on close.
    let popup: Option<Child> = None;
    // The item the pointer was over at the last paint of the menu, kept only so a change can be
    // reported: the view reads `Child::hovered_key`.
    let menu_hovered: Option<u64> = None;
    // The menu the live popup was opened for, so switching menus rebuilds the window at the new
    // word and size.
    let menu_shown: Option<usize> = None;

    /// Everything one window of this terminal is: its own tabs (so ttys and shells), surface and
    /// menu popup. The `Session`, fonts and theme are shared.
    struct Win {
        top: Child,
        app: App,
        backends: alloc::vec::Vec<TabBackend>,
        bounds: Rect,
        popup: Option<Child>,
        menu_shown: Option<usize>,
        menu_hovered: Option<u64>,
    }

    /// Open a window of this terminal, dressed and with a shell in its first tab.
    ///
    /// # Safety
    ///
    /// `root_ns` must be this process's live root namespace.
    unsafe fn open_window<T: libsurface::Transport>(
        win: &mut Session<T>,
        mut app: App,
        ui_font: &Font,
        theme: &Theme,
        root_ns: u64,
        env: &libstream::wire::Record,
    ) -> Option<Win> {
        let size = app.window_size();
        let ui = app.view(theme, None);
        let top = Child::open_sized(win, Role::Normal, (0, 0), size, &ui, ui_font, theme, BUFFERS)?;
        dress(win, top.id());
        let mut backends = alloc::vec::Vec::new();
        // SAFETY: the caller's contract — `root_ns` is this process's live root namespace.
        unsafe { reconcile_backends(&mut app, &mut backends, root_ns, env) };
        Some(Win {
            top,
            app,
            backends,
            bounds: Rect::new(0, 0, size.w, size.h),
            popup: None,
            menu_shown: None,
            menu_hovered: None,
        })
    }

    let mut quit_pending = false;

    /// A tab's other half: the tty this process is the backend for, and the shell on it.
    ///
    /// Paired with a `Term` by key, not index, as closing a tab renumbers the rest.
    struct TabBackend {
        key: u64,
        backend: backend::Backend,
    }

    /// Give every tab that has no backend a terminal and a shell.
    ///
    /// # Safety
    ///
    /// `root_ns` must be this process's live root namespace.
    unsafe fn reconcile_backends(
        app: &mut App,
        backends: &mut alloc::vec::Vec<TabBackend>,
        root_ns: u64,
        env: &libstream::wire::Record,
    ) {
        // Tabs that went away drop their backend; `Backend`'s `Drop` closes the channel, which ends
        // the `nxsh` on it.
        backends.retain(|b| app.tabs().iter().any(|t| t.key() == b.key));
        let want: alloc::vec::Vec<u64> =
            app.tabs().iter().map(|t| t.key()).filter(|k| !backends.iter().any(|b| b.key == *k)).collect();
        for key in want {
            // SAFETY: the caller's contract — `root_ns` is this process's live root namespace.
            match unsafe { backend::attach(root_ns) } {
                Some((terminal, b)) => {
                    // SAFETY: `terminal` is a channel this process owns until the spawn moves it.
                    // Said on success too: a release image could not otherwise tell a hosting
                    // terminal from an empty window.
                    if unsafe { backend::spawn_shell(root_ns, terminal, env) } < 0 {
                        kprint(b"nxterm: no shell\n");
                    }
                    backends.push(TabBackend { key, backend: b });
                }
                // The tab stays and says so, rather than a chord that appears to do nothing.
                None => app.feed_tab(key, b"nxterm: no terminal available\r\n"),
            }
        }
    }

    let mut backends: alloc::vec::Vec<TabBackend> = alloc::vec::Vec::new();
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    unsafe { reconcile_backends(&mut app, &mut backends, root_ns, &env) };
    let mut wins = alloc::vec![Win { top, app, backends, bounds, popup, menu_shown, menu_hovered }];

    loop {
        for entry in wins.iter_mut() {
            let Win { top, app, backends, bounds, popup, menu_shown, menu_hovered } = entry;
            let window_id = top.id();
            let ui = app.view(&theme, top.hovered_key());
            let l = layout(&ui, *bounds, &FontMetrics::new(&ui_font, theme.font_px));
            // One anchor per bar word, read every frame: a word's position is a fact about the
            // layout, and reading on open would precede the first layout.
            app.place_menus(&ui, &l);

            // Opened and destroyed with the menu, since a `popup` takes the keyboard while up.
            // Keyed on which menu, not whether one is open: switching File to Edit must rebuild the
            // window, as `Child::present` lays into a rectangle fixed at open and the new menu's
            // extra rows would be clipped.
            if *menu_shown != app.menus.open() {
                if let Some(p) = popup.take() {
                    p.close(&mut win);
                }
                // Nothing is under the pointer in a destroyed window; left set, the next menu would
                // report no change.
                *menu_hovered = None;
                *menu_shown = app.menus.open();
            }
            match (menu_shown.is_some(), popup.is_some(), app.menus.anchor()) {
                (true, false, Some(anchor)) => {
                    // Measured with no hover: the pointer is over the bar item, and a highlight
                    // does not change a menu's size.
                    let menu = app.menu_view(&theme, None);
                    *popup = Child::open(
                        &mut win,
                        Role::Popup { parent: window_id },
                        anchor,
                        &menu,
                        &ui_font,
                        &theme,
                        BUFFERS,
                    );
                    // The gate reads the serial log, and this says the menu became a window
                    // (`check-terminal` matches `menu popup`); the origin is in screen coordinates,
                    // for aiming a click.
                    #[cfg(feature = "test-harness")]
                    if let Some(p) = popup.as_ref() {
                        let o = win.window(p.id()).and_then(|w| w.configured()).map_or((0, 0), |c| (c.x, c.y));
                        libkern::debug::Line::new()
                            .s(b"nxterm: menu popup ")
                            .u(p.id() as u64)
                            .s(b" at ")
                            .i(o.0 as i64)
                            .s(b",")
                            .i(o.1 as i64)
                            .s(b" ")
                            .u(p.size().w as u64)
                            .s(b"x")
                            .u(p.size().h as u64)
                            .end();
                    }
                    if popup.is_none() {
                        // Not fatal: the terminal is usable without its menu.
                        kprint(b"nxterm: could not open the menu popup\n");
                        app.menus.close();
                        *menu_shown = None;
                    }
                }
                (false, true, _) => {
                    if let Some(p) = popup.take() {
                        p.close(&mut win);
                    }
                    *menu_hovered = None;
                }
                _ => {}
            }
            if let Some(p) = popup.as_mut() {
                let now = p.hovered_key();
                if now != *menu_hovered {
                    *menu_hovered = now;
                    // The item's key is a number this program chose, never typed text;
                    // `check-terminal` matches `menu hover`.
                    #[cfg(feature = "test-harness")]
                    {
                        let mut l = libkern::debug::Line::new();
                        l.s(b"nxterm: menu hover ");
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
                }
                let menu = app.menu_view(&theme, now);
                if !p.present(&mut win, &menu, &ui_font, &theme) {
                    kprint(b"nxterm: the menu popup could not be drawn\n");
                }
            }

            // The grid holds the keyboard whenever the menu is not open; a button click takes
            // focus, so it is re-asserted every frame.
            if app.menus.open().is_none() {
                top.focus_key(&ui, GRID_KEY);
            }

            // The grid's rows in window coordinates: damage the diff cannot see, as a `custom` node
            // is fingerprinted by kind and size. `App::damage_rows` translates from screen rows.
            let mut extra = None;
            let origin = app.grid_origin();
            let cols = app.grid().cols();
            for row in app.damage_rows() {
                let r = app.metrics.row_rect(row, cols);
                let r = Rect::new(origin.x + r.origin.x, origin.y + r.origin.y, r.size.w, r.size.h);
                extra = union_opt(extra, Some(r));
            }

            let ok = {
                let (m, palette, view_top) = (app.metrics, app.palette, app.view_line());
                let grid = app.grid();
                top.present_custom(
                    &mut win,
                    &ui,
                    &l,
                    &ui_font,
                    &theme,
                    extra,
                    &mut |kind, rect, clip, fb: &mut MemFramebuffer| {
                        if kind != GRID_KIND {
                            return;
                        }
                        // The cells may not fill the node (a maximised window): clear the margin,
                        // or a resize leaves a strip of the old window.
                        let cells = m.pixel_size(grid.cols(), grid.rows());
                        if rect.size.w > cells.w || rect.size.h > cells.h {
                            fb.fill_rect(clip, palette.background);
                        }
                        let rows = rows_in(clip, origin, &m, grid.rows());
                        libterm::render::render_view(fb, grid, &mono_font, &m, &palette, origin, view_top, &rows);
                    },
                )
            };
            if !ok {
                fail(b"nxterm: the window could not be drawn\n");
            }

            // Asked to close, by its button or the shell: exiting closes the handles, and the
            // compositor destroys the session's windows. `check-login` matches `closing`.
            if app.closing() {
                kprint(b"nxterm: closing\n");
                exit(0);
            }

            // The title bar was dragged. Done here as `update` has no syscalls, and before the
            // frame so the compositor moves the window while this client paints. `check-login`
            // matches the log line.
            if app.take_move_request()
                && let Some(mut w) = win.window(window_id)
            {
                match w.start_move() {
                    Ok(()) => kprint(b"nxterm: dragging its own title bar\n"),
                    Err(_) => kprint(b"nxterm: the compositor refused the move\n"),
                }
            }

            // The corner grip: asked for, not done. The compositor holds the grab and answers with
            // a `Configure` at the end of the gesture.
            if let Some(edges) = app.take_resize_request()
                && let Some(mut w) = win.window(window_id)
            {
                match w.start_resize(edges) {
                    Ok(()) => kprint(b"nxterm: dragging its own corner\n"),
                    Err(_) => kprint(b"nxterm: the compositor refused the resize\n"),
                }
            }

            // A title-bar button: asked for, not done; the reply says the compositor forwarded the
            // question, and a shell may decide otherwise. A request for a window that has gone is
            // dropped, not retried. `check-login` matches the log line.
            if let Some(state) = app.take_state_request()
                && let Some(mut w) = win.window(window_id)
            {
                match w.request_state(state) {
                    Ok(()) => {
                        libkern::debug::Line::new()
                            .s(b"nxterm: asked the shell for window state ")
                            .u(state as u64)
                            .end();
                    }
                    Err(_) => kprint(b"nxterm: the state request was refused\n"),
                }
            }

            // Here because `App::update` is a function of values and `/dev/clipboard` is IPC. A
            // pasted entry goes down the pty as if typed.
            if let Some(req) = app.take_clip_request() {
                match req {
                    nxterm::ClipRequest::Copy(text) => match clipboard_copy(root_ns, text.as_bytes()) {
                        // A length, never the text: a selection is as private as typing.
                        // `check-login` matches `pasted N bytes`.
                        Ok(()) => Line::new().s(b"nxterm: copied ").u(text.len() as u64).s(b" bytes").end(),
                        Err(why) => Line::new().s(b"nxterm: the copy failed: ").s(why.as_bytes()).end(),
                    },
                    nxterm::ClipRequest::Paste => {
                        let mut got = [0u8; MAX_CLIP_BYTES];
                        match clipboard_paste(root_ns, &mut got) {
                            Ok(n) => {
                                if let Ok(text) = core::str::from_utf8(&got[..n]) {
                                    app.pasted(text);
                                    Line::new().s(b"nxterm: pasted ").u(n as u64).s(b" bytes").end();
                                } else {
                                    kprint(b"nxterm: the clipboard entry is not text\n");
                                }
                            }
                            Err(why) => Line::new().s(b"nxterm: the paste failed: ").s(why.as_bytes()).end(),
                        }
                    }
                }
                continue; // round again: a paste has put bytes in the outbox
            }

            // Every tab, both directions, every frame: typed bytes go to their tab and any shell's
            // output through that tab's parser, so a background tab is current when switched to.
            // Done here, not in the event arm, because output arrives unprompted and a prompt would
            // show one keypress late.
            let mut ended: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
            for tb in backends.iter_mut() {
                let out =
                    app.tabs_mut().iter_mut().find(|t| t.key() == tb.key).map(|t| t.take_outbox()).unwrap_or_default();
                if !out.is_empty() && !tb.backend.typed(&out) {
                    kprint(b"nxterm: input did not reach the tty\n");
                }
                while let Some(bytes) = tb.backend.output() {
                    #[cfg(feature = "test-harness")]
                    let before = app.grid().cursor().0;
                    #[cfg(feature = "test-harness")]
                    let current = tb.key == app.current_tab();
                    app.feed_tab(tb.key, bytes);
                    // Harness only: report a line once the cursor has left it, so `check-terminal`
                    // can assert on the grid's contents. The current tab only, so "what is on
                    // screen" stays unambiguous.
                    #[cfg(feature = "test-harness")]
                    if current {
                        // Every row the cursor passed (a chunk can complete several lines) and the
                        // row it is on: a finished line is only in the first set, a line still
                        // being typed only in the second.
                        let now = app.grid().cursor().0;
                        for row in before..now {
                            report_row(app, row);
                        }
                        report_row(app, now);
                    }
                }
                // Once per change: `nxsh` announces its directory as `OSC 7`, `libterm` reads it
                // and the title bar shows it. `check-terminal` asserts on this line, as the three
                // crates' agreement cannot be host-tested.
                if let Some(t) = app.tabs_mut().iter_mut().find(|t| t.key() == tb.key)
                    && t.take_directory_change()
                    && let Some(dir) = t.directory()
                {
                    libkern::debug::Line::new().s(b"nxterm: shell is at ").s(dir.as_bytes()).end();
                }
                if tb.backend.is_gone() {
                    ended.push(tb.key);
                }
            }
            // A shell that exited closes its tab, and the last one closes the window, as
            // `Msg::CloseTab` does. `check-login` matches `the terminal ended`.
            for key in ended {
                kprint(b"nxterm: the terminal ended\n");
                app.update(Msg::CloseTab(key));
            }
            // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
            unsafe { reconcile_backends(app, backends, root_ns, &env) };
        }

        // After every window is serviced: a window cannot be created or destroyed mid-iteration.
        let mut opened = 0usize;
        for w in wins.iter_mut() {
            if w.app.take_new_window() {
                opened += 1;
            }
            if w.app.take_quit() {
                kprint(b"nxterm: quitting\n");
                quit_pending = true;
            }
        }
        for _ in 0..opened {
            // A new window starts with one fresh tab, not a copy of this one's shells.
            // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
            match unsafe { open_window(&mut win, App::new(COLS, ROWS, metrics), &ui_font, &theme, root_ns, &env) } {
                Some(w) => {
                    kprint(b"nxterm: opened another window\n");
                    wins.push(w);
                }
                None => kprint(b"nxterm: could not open another window\n"),
            }
        }
        // Quit closes every window as its own close button does.
        if quit_pending {
            for w in wins.iter_mut() {
                w.app.update(Msg::Close);
            }
        }
        let mut i = 0;
        while i < wins.len() {
            if wins[i].app.closing() {
                let w = wins.remove(i);
                if let Some(p) = w.popup {
                    p.close(&mut win);
                }
                // The backends go with the window; their `Drop` closes each channel, which ends
                // every shell on them.
                w.top.close(&mut win);
                kprint(b"nxterm: closed a window\n");
            } else {
                i += 1;
            }
        }
        if wins.is_empty() {
            kprint(b"nxterm: closing\n");
            exit(0);
        }

        // One `sys_wait` over the compositor and every tab, then non-blocking drains: waiting on
        // the compositor alone would show a shell's output only after the next keystroke.
        let mut events: alloc::vec::Vec<(u32, WindowEvent)> = alloc::vec::Vec::new();
        let win_handle = win.wait_handle();
        // Every tab of every window, so a shell printing in a background tab never waits.
        let any_backend = wins.iter().any(|w| !w.backends.is_empty());
        if any_backend && win_handle != 0 {
            loop {
                match win.poll_event() {
                    Ok(Some(ev)) => events.push(ev),
                    Ok(None) => break,
                    Err(_) => {
                        kprint(b"nxterm: the compositor went away\n");
                        exit(0);
                    }
                }
            }
            if events.is_empty() {
                let mut handles = alloc::vec::Vec::with_capacity(8);
                handles.push(win_handle);
                for w in wins.iter() {
                    handles.extend(w.backends.iter().map(|b| b.backend.channel));
                }
                if !wait_any(&handles) {
                    fail(b"nxterm: the wait was refused\n");
                }
                continue; // round again: drain every source from the top
            }
        } else {
            match win.wait_event() {
                Ok(ev) => events.push(ev),
                Err(_) => {
                    kprint(b"nxterm: the compositor went away\n");
                    exit(0);
                }
            }
        }

        for (from, event) in events {
            // A record naming none of the windows is dropped, not routed to the first.
            let Some(wi) =
                wins.iter().position(|w| w.top.id() == from || w.popup.as_ref().is_some_and(|p| p.id() == from))
            else {
                continue;
            };
            let Win { top, app, backends: _, bounds, popup, menu_shown: _, menu_hovered: _ } = &mut wins[wi];
            let window_id = top.id();
            // Rebuilt for this window, as routing is against its tree. No layout here:
            // `Child::route` lays out.
            let ui = app.view(&theme, top.hovered_key());
            // Per event, not per drain: a `Configure` would otherwise leave it set for later events
            // belonging to other windows.
            let mut resized = false;
            // The menu's window routes through the menu's tree; a stale record for a just-destroyed
            // popup is dropped, not routed into the terminal.
            if popup.as_ref().is_some_and(|p| p.id() == from) {
                // A press outside the menu dismisses it; only the compositor knows. Answered here,
                // not in `Child::route`, as what a dismissal means is the caller's.
                if matches!(event, WindowEvent::Dismissed) {
                    app.menus.close();
                    continue;
                }
                // Arrows, Esc and Enter drive the open menu: the popup holds the keyboard, so these
                // never reach the terminal's router.
                if let WindowEvent::Key(k) = event {
                    let table = app.menu_table();
                    match app.menus.key(&k, &table) {
                        KeyOutcome::Chose { menu, item } => {
                            // The menu comes from the outcome, not the state: choosing closes, so
                            // `open()` is already `None`.
                            if let Some(m) = chosen(&table, Some(menu), item) {
                                chose(m);
                                app.update(m);
                            }
                            continue;
                        }
                        // Neither reaches the grid.
                        KeyOutcome::Dismissed | KeyOutcome::Changed => continue,
                        KeyOutcome::Ignored => {}
                    }
                }
                let menu = app.menu_view(&theme, popup.as_ref().and_then(|p| p.hovered_key()));
                let msgs = popup.as_mut().map(|p| p.route(&menu, &ui_font, &theme, &event)).unwrap_or_default();
                for msg in msgs {
                    chose(msg);
                    // The menu closes itself: `App::update` asks the table.
                    app.update(msg);
                }
                continue;
            }
            if from != window_id {
                continue;
            }
            match event {
                // Everything goes through the router, keys that type included; whether a key types
                // at all is `App::key`'s.
                WindowEvent::Key(k) => {
                    // F1 opens the menu, under the harness only: the gate cannot click the bar, as
                    // nxterm is created under `ui-testclient`'s windows. An open menu takes the
                    // keyboard, so the gate picks when it opens.
                    #[cfg(feature = "test-harness")]
                    if k.keycode == 59 && k.pressed != 0 {
                        // A plain toggle, so the keyboard half of an open menu can be reopened.
                        app.update(Msg::MenuBar(nxterm::HARNESS_MENU));
                        continue;
                    }
                    if let Some(msg) = top.route_key(&ui, k) {
                        app.update(msg);
                    }
                }
                WindowEvent::Pointer(p) => {
                    // A press means the compositor put this window under the cursor and will give
                    // it the keyboard. Reported under the harness (`check-terminal` matches
                    // `clicked`) as the only evidence the click landed here.
                    #[cfg(feature = "test-harness")]
                    if p.kind == librsproto::surface::POINTER_BUTTON
                        && p.flags & librsproto::surface::POINTER_PRESSED != 0
                    {
                        kprint(b"nxterm: clicked\n");
                    }
                    // Counted before routing so the press can ask which click it was; the record's
                    // own time is used, so a stalled client does not read two clicks as a double.
                    if p.kind == librsproto::surface::POINTER_BUTTON
                        && p.flags & librsproto::surface::POINTER_PRESSED != 0
                        && p.button == libkern::abi::BTN_LEFT
                    {
                        app.note_press(libdraw::geom::Point::new(p.x, p.y), p.time_ms);
                    }
                    let msgs = top.route(&ui, &ui_font, &theme, &WindowEvent::Pointer(p));
                    for m in msgs {
                        app.update(m);
                    }
                }
                WindowEvent::Focus(f) => {
                    // Reported under the harness (`check-terminal` waits on `focus=1`): keys go to
                    // the topmost focusable window and nxterm starts at the bottom until a click
                    // raises it, so early keys race the raise.
                    #[cfg(feature = "test-harness")]
                    libkern::debug::Line::new().s(b"nxterm: focus=").u(u64::from(f)).end();
                    top.route(&ui, &ui_font, &theme, &WindowEvent::Focus(f));
                    app.focused = f;
                }
                // A `normal` window is not dismissed by a press elsewhere — the event is a popup's.
                WindowEvent::Dismissed => {}
                // The shell asking, answered like the close button; `check-login` matches the log
                // line.
                WindowEvent::CloseRequested => {
                    kprint(b"nxterm: asked to close, exiting\n");
                    app.update(Msg::Close);
                }
                // A terminal declares no acceptor, so a drop never arrives; the match is
                // exhaustive. `InputLost` discards nothing: modifiers arrive on each event.
                WindowEvent::Drop { .. } => kprint(b"nxterm: a drop arrived; this window takes none\n"),
                WindowEvent::InputLost => kprint(b"nxterm: input dropped\n"),
                // Accepted: the window becomes the size asked for, the grid the cells that fit, and
                // the history rewraps. Same size costs nothing.
                WindowEvent::Configure { width, height, .. } => {
                    // Counted before and reported beside what it became (`check-login` and
                    // `check-terminal` match `resized to`): a rewrap must not create or destroy
                    // lines, so the two counts are equal.
                    let lines_before = app.grid().logical_lines();
                    if let Some(r) = app.resize(Size::new(width, height)) {
                        resized = true;
                        libkern::debug::Line::new()
                            .s(b"nxterm: resized to ")
                            .u(u64::from(width))
                            .s(b"x")
                            .u(u64::from(height))
                            .s(b", grid ")
                            .u(app.grid().cols() as u64)
                            .s(b"x")
                            .u(app.grid().rows() as u64)
                            .s(b", lines ")
                            .u(lines_before as u64)
                            .s(b"->")
                            .u(app.grid().logical_lines() as u64)
                            // The eviction: narrowing makes more rows from the same text, so a deep
                            // history loses its oldest to the ring, not to the rewrap.
                            .s(b", ")
                            .u(r.evicted as u64)
                            .s(b" evicted")
                            .end();
                    }
                }
            }

            // The window changed shape, so everything about the frame does: compose buffer, layout,
            // the diff's record and the grid's rows. The next render is the first to see it.
            if resized {
                let size = app.window_size();
                // `Child::resize` throws the retained tree away: a diff against the old bounds
                // reports old coordinates.
                if top.resize(size) == Some(false) {
                    fail(b"nxterm: impossible window geometry\n");
                }
                *bounds = Rect::new(0, 0, size.w, size.h);
                app.damage_all();
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxterm: panic\n");
    exit(2);
}
