//! `nxterm` — the terminal client: the window, the buffers, and the event pump.
//!
//! Everything with behaviour is in the library half and host-tested there. What is here is the
//! part that needs an OS: connecting to `/dev/draw`, sharing buffers with the compositor,
//! draining input, and turning the toolkit's damage and the grid's into one `Commit`.
//!
//! ## Two damage systems meeting
//!
//! `libui`'s diff reports **one rectangle** for what the widget tree changed; `libterm`'s grid
//! reports **a list of cell rows**. A frame's damage is their union, in window coordinates, and
//! the `custom` node's paint callback turns the clip it is handed back into rows. Getting that
//! wrong in the cheap direction repaints the window per keystroke, which is exactly the cost
//! the diff exists to avoid; getting it wrong in the other leaves stale pixels.

#![no_std]
#![no_main]

extern crate alloc;

mod backend;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::{Font, SYSTEM_FONT_PATH, load};
use libkern::{exit, kprint};
use librsproto::surface::{CreateWindowRequest, Role};
use libsurface::buffers::BufferPool;
use libsurface::{Session, WindowEvent, ipc::ChannelTransport};
use libterm::render::Metrics;
use libui::diff::Tree;
use libui::damage::union_opt;
use libui::layout::{Constraints, Layout, layout, locate, measure};
use libui::paint::FontMetrics;
use libui::paint::{Theme, paint};
use libui::route::Router;
use alloc::boxed::Box;
use nxterm::{App, GRID_KEY, GRID_KIND, MENU_ITEM_KEY, Msg, rows_in};

/// `alloc` backing — the element tree, the grid and the render all allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Buffers shared with the compositor. Two is the minimum the protocol permits, and it is what
/// lets a frame be drawn while the other is on screen.
const BUFFERS: usize = 2;

/// The terminal's size in cells **at startup**. A `Configure` changes it (M9 Part D).
const COLS: usize = 80;
const ROWS: usize = 24;

/// Text size, matching the toolkit's default theme so the chrome and the grid agree.
const FONT_PX: f32 = 16.0;

/// A private framebuffer of `size` to compose a frame into.
///
/// **Drawn here and copied into whichever buffer is free**, rather than painted directly into
/// it: the toolkit's damage describes what changed since the *last frame*, and the free buffer
/// holds the frame before that. Painting a one-row damage straight into it would leave the row
/// from two frames ago everywhere else. `libui::damage`'s per-buffer accumulation is the real
/// answer; a copy is correct now and is one `memcpy` of a window.
fn compose_buffer(size: Size) -> Option<MemFramebuffer> {
    let pitch = (size.w as usize).checked_mul(4)?;
    Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888).map(MemFramebuffer::new)
}

/// Report and end the run.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: under the test-harness kernel this terminates QEMU; elsewhere it is
    // `Unsupported` and falls through.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
}

/// The menu's window, alive only while the menu is open.
///
/// **A `popup`, not a layer.** `libui`'s `offset` clips at its parent's edge, which is right one
/// level down and wrong for a menu — "a menu clipped to its window is not a menu"
/// (`display-substrate.md` §4a). Until M6 C3 the menu was a `Stack` layer hoisted to the whole
/// window, which worked only because it happened to fit inside the terminal. As a `popup` window
/// it is parented to the terminal, positioned by *this* client at the anchor the layout gives,
/// and clipped only by the screen.
struct Popup {
    id: u32,
    /// The pixels, allocated and mapped by `libsurface` and unmapped when this is dropped.
    pool: BufferPool,
    /// Composed here and copied into whichever buffer is free — the same reason the terminal
    /// window does it: the toolkit's damage describes the last frame, not the free buffer's.
    scratch: MemFramebuffer,
    /// Diff state, separate from the terminal's because this is a different tree.
    tree: Tree,
    /// Routing state, likewise: focus within the menu is not focus within the terminal.
    router: Router,
    size: Size,
}

impl Popup {
    /// Create the menu's window at `anchor`, sized to what the menu measures.
    ///
    /// **Measured rather than guessed.** A popup window needs its extent before it exists, and
    /// a hardcoded size would silently stop matching the menu the first time an item is added.
    /// `Fill` measures as zero, so the backing layer does not inflate this.
    fn open(
        session: &mut Session<Box<ChannelTransport>>,
        parent: u32,
        anchor: Rect,
        app: &App,
        font: &Font,
    ) -> Option<Self> {
        let menu = app.menu_view();
        let m = FontMetrics::new(font, FONT_PX);
        let size = measure(&menu, Constraints::loose(Size::new(u32::MAX / 4, u32::MAX / 4)), &m);
        if size.w == 0 || size.h == 0 {
            return None;
        }
        // Directly under the item it drops from, in the parent's coordinates — which is what
        // the offset in `CreateWindow` means.
        let id = session
            .create(
                &CreateWindowRequest::at(
                    size.w,
                    size.h,
                    Role::Popup { parent },
                    anchor.origin.x,
                    anchor.bottom() as i32,
                ),
                BUFFERS,
            )
            .ok()?;

        // **Everything after this destroys the window on the way out.** An abandoned popup is
        // worse than no popup: `Session::create` waited for its first `Configure`, so it is
        // *configured*, and a configured `popup` is focusable — it becomes the compositor's
        // topmost focus candidate and stays there. Having committed nothing it is never drawn,
        // so the result is an invisible window silently eating every keystroke, and the caller
        // treats the failure as recoverable and carries on (PR #223 review, finding 4).
        let built = (|| {
            let scratch = compose_buffer(size)?;
            let pool = BufferPool::new(&mut session.window(id)?, size, BUFFERS)?;
            Some((pool, scratch))
        })();
        let Some((pool, scratch)) = built else {
            if let Some(w) = session.window(id) {
                let _ = w.destroy();
            }
            return None;
        };
        Some(Self { id, pool, scratch, tree: Tree::new(), router: Router::new(), size })
    }

    /// Paint the menu and put it on screen **when something changed**, and say whether all is
    /// well.
    ///
    /// **Gated on the diff, like the terminal window is.** Committing every frame instead is
    /// not merely wasteful: with two buffers the third commit blocks in `acquire` until the
    /// compositor releases one, and that block is inside the render half of the loop — so the
    /// tty is never pumped and the shell's output never arrives. The terminal appeared to hang
    /// with its menu open.
    ///
    /// The region is the whole window when it does repaint: it is tiny, and tracking damage
    /// within it would be more state than it saves.
    fn present(
        &mut self,
        session: &mut Session<Box<ChannelTransport>>,
        app: &App,
        font: &Font,
        theme: &Theme,
    ) -> bool {
        let menu = app.menu_view();
        let bounds = Rect::new(0, 0, self.size.w, self.size.h);
        let l = layout(&menu, bounds, &FontMetrics::new(font, FONT_PX));
        match self.tree.update(&menu, &l) {
            Ok(None) => return true, // nothing changed; the frame on screen is still right
            Ok(Some(_)) => {}
            Err(_) => return false,
        }
        paint(&mut self.scratch, font, theme, &menu, &l, bounds, &mut |_, _, _, _| {});
        let Some(mut w) = session.window(self.id) else { return false };
        let Ok(b) = self.pool.acquire(&mut w, self.size) else { return false };
        if !self.pool.write(b, self.scratch.bytes()) {
            return false;
        }
        session
            .window(self.id)
            .is_some_and(|mut w| w.commit(b, (0, 0, self.size.w, self.size.h)).is_ok())
    }

    /// Destroy the window and give the client's half of the pixels back.
    ///
    /// The compositor drops its mapping when the window goes; the mapping on *this* side is
    /// the pool's, released when this value is dropped at the end of this function — a menu
    /// opened and closed a hundred times would otherwise grow this process by a hundred
    /// buffers.
    fn close(self, session: &mut Session<Box<ChannelTransport>>) {
        if let Some(w) = session.window(self.id) {
            let _ = w.destroy();
        }
    }
}

/// Paint `damage` of `app` into `fb`.
///
/// The join between the two damage systems: `paint` walks the widget tree and calls back for
/// the `custom` node with the clip it survived, and that clip becomes the rows `libterm`
/// renders. Nothing here decides *what* changed — the diff and the grid did that.
fn draw(
    fb: &mut MemFramebuffer,
    app: &App,
    ui: &libui::element::Element<Msg>,
    l: &Layout,
    font: &Font,
    theme: &Theme,
    damage: Rect,
) {
    let origin = app.grid_origin();
    let m = app.metrics;
    let grid = &app.grid;
    let palette = app.palette;
    let top = app.view_line();
    paint(fb, font, theme, ui, l, damage, &mut |kind, rect, clip, fb: &mut MemFramebuffer| {
        if kind != GRID_KIND {
            return;
        }
        // **The cells do not always fill the node** (M9 Part D). A maximised window is exactly
        // the work area and the grid is the whole cells that fit, so up to a cell's width and a
        // cell's height of the node is not covered by any row — and `render_view` paints cells,
        // nothing else. Left alone that margin holds whatever the last frame put there, which
        // after a resize is a strip of the old window.
        //
        // Filled only when the node really is bigger than its cells, so the ordinary
        // one-row-per-keystroke damage costs nothing extra.
        let cells = m.pixel_size(grid.cols(), grid.rows());
        if rect.size.w > cells.w || rect.size.h > cells.h {
            fb.fill_rect(clip, palette.background);
        }
        let rows = rows_in(clip, origin, &m, grid.rows());
        libterm::render::render_view(fb, grid, font, &m, &palette, origin, top, &rows);
    });
}

/// Print the text of grid row `row`, trailing blanks trimmed.
///
/// The harness's window onto the grid. Deliberately the *grid* and not the pixels: what a
/// shell prints is not fixed by this milestone, so a pixel comparison would pin it — the
/// display gate covers the rendering separately, against a fixed reference.
#[cfg(feature = "test-harness")]
fn report_row(app: &App, row: usize) {
    let mut buf = [0u8; 256];
    let mut n = 0;
    for col in 0..app.grid.cols() {
        let Some(cell) = app.grid.view_cell(app.view_line(), row, col) else { break };
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

/// Block until either handle has something.
///
/// Both in one `sys_wait`, which is the whole point: waiting on them in turn would mean a
/// keystroke could not be seen while the shell was quiet, or the reverse.
fn wait_two(a: u64, b: u64) {
    let handles = [a, b];
    let mut results = [0u8; 48];
    // SAFETY: a valid two-handle array and a result buffer sized for two records.
    unsafe {
        libkern::syscall4(
            libkern::SYS_WAIT,
            handles.as_ptr() as u64,
            2,
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
    kprint(b"nxterm: up\n");

    // **Its own setup message, so the shell it hosts gets a real environment.** `nxterm` took
    // none until M7 Part F and handed `nxsh` a `Record::default()`, which was invisible while
    // `init` spawned it with nothing either. Launched from `desktop-shell` it would otherwise
    // be the one shell on the system with no `$env.HOME`.
    //
    // Absent is normal, not an error: a Tier-0 spawn has no setup message at all, which is
    // what `bootstrap_arg0(false)` means and what `init` used to do. An empty `Record` then
    // says exactly what it did before.
    let env = match libstream::setup::bootstrap(notif, root_ns, endpoint, arg0).setup() {
        Some(Ok(s)) => s.env,
        Some(Err(_)) => libstream::wire::Record::default(),
        None => libstream::wire::Record::default(),
    };

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let font = match unsafe { load(root_ns, SYSTEM_FONT_PATH) } {
        Ok(f) => f,
        Err(_) => fail(b"nxterm: could not load the system font\n"),
    };
    let metrics = Metrics::new(&font, FONT_PX);
    let mut app = App::new(COLS, ROWS, metrics);

    let size = app.window_size();
    // SAFETY: `root_ns` is this process's live root namespace.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        // Boxed for the reason `ui-testclient` boxes its own: ~9 KiB of message buffers has no
        // business in a stack frame beside everything else here.
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => fail(b"nxterm: connect to /dev/draw FAILED\n"),
    };
    // **A session, not a window.** `libsurface` hands out a session since M6 C3, because a
    // client may hold several windows on one connection — which is what the menu becoming a
    // real popup needs (C3 part 3). Today this holds exactly one.
    let mut win = Session::new(transport);
    let window_id = match win.create(&CreateWindowRequest::new(size.w, size.h, Role::Normal), BUFFERS) {
        Ok(w) => w,
        Err(_) => fail(b"nxterm: CreateWindow FAILED\n"),
    };

    // **A name, because something now shows it.** `SetTitle` shipped in M7 Part A with a
    // compositor that stores titles and a manager event that reports them, and nothing in the
    // tree ever sent one — so M8 Part C's window list read `window 6` for every entry, and the
    // shell's title arm was code no boot could reach (PR #242 review, optional 7).
    if let Some(mut w) = win.window(window_id)
        && w.set_title(nxterm::TITLE).is_err()
    {
        kprint(b"nxterm: SetTitle refused\n");
    }

    // Two shared buffers, and a scratch framebuffer to compose into.
    //
    // **Drawn once here and copied per frame**, rather than painted directly into whichever
    // buffer is free: the toolkit's damage describes what changed since the *last frame*, and
    // the free buffer holds the frame before that. Painting a one-row damage straight into it
    // would leave the row from two frames ago everywhere else. `libui::damage`'s per-buffer
    // accumulation is the real answer and it belongs in Part B's successor; a copy is correct
    // now and is one `memcpy` of a window.
    let mut scratch = match compose_buffer(size) {
        Some(fb) => fb,
        None => fail(b"nxterm: impossible window geometry\n"),
    };
    // **The buffers and the resize belong to `libsurface`** since M9 Part D: allocating shared
    // memory, attaching it, and — the part with an ordering rule in it — replacing it at a new
    // size without touching what the compositor is reading. This client had its own copy of the
    // first half and none of the second.
    let mut pool = {
        let Some(mut w) = win.window(window_id) else {
            fail(b"nxterm: our own window is gone\n");
        };
        match BufferPool::new(&mut w, size, BUFFERS) {
            Some(p) => p,
            None => fail(b"nxterm: buffer alloc FAILED\n"),
        }
    };

    let theme = Theme { font_px: FONT_PX, ..Theme::default() };
    let mut bounds = Rect::new(0, 0, size.w, size.h);
    let mut tree = Tree::new();
    let mut router = Router::new();
    // The menu's window while it is open, and nothing at all while it is not — a popup is
    // transient by role, so closing the menu destroys it rather than hiding it.
    let mut popup: Option<Popup> = None;
    // Set once, by the harness click below, to open the menu — see there.
    #[cfg(feature = "test-harness")]
    let mut opened_for_harness = false;

    // **The tty, and the shell on the other end of it.** Part C's whole point: the terminal is
    // obtained like any program's, this process becomes its backend, and the terminal itself is
    // handed to the shell it spawns. What was a loopback is now a pty.
    //
    // A terminal that cannot get one still runs — it draws, it takes input, it just has nobody
    // to talk to. Failing the window instead would turn a tty-server problem into a blank
    // screen, which is the harder thing to diagnose.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let mut backend = match unsafe { backend::attach(root_ns) } {
        Some((terminal, b)) => {
            // SAFETY: `root_ns` is live and `terminal` is a channel this process owns until the
            // spawn moves it.
            // **Said on success as well as failure.** This path only ever reported when it
            // went wrong, so a release image could not tell a terminal hosting a shell from a
            // window that opened with nothing in it — which is exactly what a missing `/bin`
            // in an application namespace looks like, and what M7 Part F hit first. The grid
            // report that would otherwise show it is `test-harness`-only.
            if unsafe { backend::spawn_shell(root_ns, terminal, &env) } < 0 {
                kprint(b"nxterm: no shell\n");
            }
            // The success line is emitted by `spawn_shell` itself, beside the send that earns
            // it — see the note there.
            Some(b)
        }
        None => {
            app.feed(b"nxterm: no terminal available\r\n");
            None
        }
    };

    loop {
        // ---- render ----
        let ui = app.view();
        let l = layout(&ui, bounds, &FontMetrics::new(&font, FONT_PX));
        // The anchor for the next frame's popup. Read every frame rather than only when the
        // menu opens: the item's position is a fact about the layout, not about the menu.
        app.menu_anchor = locate(&ui, &l, MENU_ITEM_KEY);

        // ---- the menu's window ----
        //
        // **Opened and destroyed with the menu**, because `popup` is a transient role: the
        // compositor takes it with its parent, it takes keyboard focus while it is up, and a
        // hidden one would still be a window in the stack. The anchor has to exist first —
        // before the first layout there is nowhere to put it, which is why this reads the
        // anchor computed just above rather than the one from when the menu was toggled.
        match (app.menu_open, popup.is_some(), app.menu_anchor) {
            (true, false, Some(anchor)) => {
                popup = Popup::open(&mut win, window_id, anchor, &app, &font);
                // **Where it is and how big**, because the gate has no other way to see a
                // second window: it reads the serial log, and this is the only thing that
                // says the menu became a window rather than a layer. The origin is in screen
                // coordinates, which is what a click has to be aimed at.
                #[cfg(feature = "test-harness")]
                if let Some(p) = popup.as_ref() {
                    let o = win
                        .window(p.id)
                        .and_then(|w| w.configured())
                        .map_or((0, 0), |c| (c.x, c.y));
                    libkern::debug::Line::new()
                        .s(b"nxterm: menu popup ").u(p.id as u64)
                        .s(b" at ").i(o.0 as i64).s(b",").i(o.1 as i64)
                        .s(b" ").u(p.size.w as u64).s(b"x").u(p.size.h as u64)
                        .end();
                }
                if popup.is_none() {
                    // Not fatal: the terminal is still usable without its menu, and saying so
                    // beats a window that silently never appears.
                    kprint(b"nxterm: could not open the menu popup\n");
                    app.menu_open = false;
                }
            }
            (false, true, _) => {
                if let Some(p) = popup.take() {
                    p.close(&mut win);
                }
            }
            _ => {}
        }
        if let Some(p) = popup.as_mut()
            && !p.present(&mut win, &app, &font, &theme)
        {
            kprint(b"nxterm: the menu popup could not be drawn\n");
        }

        let ui_damage = match tree.update(&ui, &l) {
            Ok(d) => d,
            // A malformed tree is a bug in `view`, not a runtime condition. Refusing loudly
            // beats painting something that pairs the wrong widgets.
            Err(_) => fail(b"nxterm: the view is not diffable\n"),
        };

        // **The grid holds the keyboard whenever the menu is not open.** Focus has to start
        // somewhere, and the toolkit will not guess: `focus_next` lands on the menu button,
        // being first in tree order. Re-asserted every frame rather than only on the first,
        // because clicking a button takes focus — and a terminal whose keyboard stays with the
        // menu after you used it is a terminal you have to click back into.
        if !app.menu_open
            && let Some(id) = tree.find_by_key(GRID_KEY)
        {
            router.focus(&tree, &ui, id);
        }

        let mut damage = ui_damage;
        // The grid's own rows, in window coordinates, unioned in. **Viewport rows** — the
        // translation from the grid's screen rows is `App::damage_rows`', because it is the
        // half that knows where the view is anchored.
        let origin = app.grid_origin();
        let cols = app.grid.cols();
        for row in app.damage_rows() {
            let r = app.metrics.row_rect(row, cols);
            let r = Rect::new(origin.x + r.origin.x, origin.y + r.origin.y, r.size.w, r.size.h);
            damage = union_opt(damage, Some(r));
        }

        if let Some(d) = damage {
            draw(&mut scratch, &app, &ui, &l, &font, &theme, d);
            let Some(mut w) = win.window(window_id) else {
                fail(b"nxterm: our own window is gone\n");
            };
            // **The size is asked for here rather than remembered**, so a buffer left at the
            // old shape by a resize is replaced at the moment it is next drawn into — which is
            // the frame after the one that committed the new size, when its release arrives.
            let Ok(b) = pool.acquire(&mut w, app.window_size()) else {
                fail(b"nxterm: no buffer released\n");
            };
            if !pool.write(b, scratch.bytes()) {
                fail(b"nxterm: the frame did not fit its buffer\n");
            }
            if w.commit(b, (d.origin.x as u32, d.origin.y as u32, d.size.w, d.size.h)).is_err() {
                fail(b"nxterm: Commit FAILED\n");
            }
        }

        // **Asked to close, by its own button or by the shell.** Exiting is the whole of it: the
        // kernel closes this process's handles, the compositor sees the session go and destroys
        // the windows on it, and the shell hears `WindowDestroyed` like any other close.
        if app.closing() {
            kprint(b"nxterm: closing\n");
            exit(0);
        }

        // **The title bar was dragged.** Performed here rather than in `update`, which has no
        // syscalls — and performed *before* the frame below, so the compositor is already moving
        // the window while this client is still painting.
        if app.take_move_request()
            && let Some(mut w) = win.window(window_id)
        {
            match w.start_move() {
                Ok(()) => kprint(b"nxterm: dragging its own title bar\n"),
                // Refused when the grab is not this window's, which is a press that arrived
                // through some path other than a real one. Not fatal, and not silent.
                Err(_) => kprint(b"nxterm: the compositor refused the move\n"),
            }
        }

        // **A title-bar button.** Asked for, not done: minimising and maximising are the
        // manager's, and the reply says the compositor forwarded the question rather than that
        // anything happened — a shell may decide otherwise, and this terminal will find out the
        // way it finds out about any other geometry, through `Configure`.
        // **Taken before the window is looked up, deliberately** — the same shape as the move
        // request above. A request for a window that has gone is dropped rather than retried:
        // there is nothing to ask about, and holding it would mean asking on behalf of a window
        // whose id the compositor may have moved past.
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

        // ---- the tty ----
        //
        // **Both directions, every frame.** What the user typed goes out; whatever the server
        // has sent comes in and goes through the parser. Done here rather than in the event
        // arm because output arrives unprompted — the shell prints when it likes, and a
        // terminal that only looked after a keystroke would show a prompt one keypress late.
        if let Some(b) = &mut backend {
            let out = app.take_outbox();
            if !out.is_empty() && !b.typed(&out) {
                kprint(b"nxterm: input did not reach the tty\n");
            }
            while let Some(bytes) = b.output() {
                #[cfg(feature = "test-harness")]
                let before = app.grid.cursor().0;
                app.feed(bytes);
                // Under the harness only: report a line once the cursor has left it, which is
                // what makes `check-terminal` able to assert on the grid's *contents*.
                #[cfg(feature = "test-harness")]
                {
                    // **Every row the cursor passed**, not just the one it started on: a
                    // single message routinely completes several lines, and reporting only
                    // the first prints a blank when the chunk begins with a newline — which
                    // is exactly what the shell's banner does.
                    let now = app.grid.cursor().0;
                    // **Rows the cursor left, and the row it is on.** Both, because they
                    // answer different questions and the gate asks both: a line the shell
                    // *finished* is only in the first set (the banner, which arrives in a
                    // chunk that ends two rows below it), and a line still being typed is
                    // only in the second (`/> whoami`, which never completes until Enter).
                    for row in before..now {
                        report_row(&app, row);
                    }
                    report_row(&app, now);
                }
            }
            if b.is_gone() {
                kprint(b"nxterm: the terminal ended\n");
                exit(0);
            }
        }

        // ---- wait for something to do ----
        //
        // **Two sources.** `wait_event` blocks on the compositor alone, which would render the
        // shell's output only after the next keystroke — a prompt one keypress late. So the
        // window's handle and the backend's go into one `sys_wait`, and the events are drained
        // non-blockingly afterwards. A terminal with no backend keeps the simple path.
        let mut events: alloc::vec::Vec<(u32, WindowEvent)> = alloc::vec::Vec::new();
        match backend.as_ref().map(|b| b.channel).filter(|_| win.wait_handle() != 0) {
            Some(bch) => {
                loop {
                    match win.poll_event() {
                        // **The id is kept.** With the menu open this session holds two
                        // windows, and a click on the menu is not a click on the terminal.
                        Ok(Some(ev)) => events.push(ev),
                        Ok(None) => break,
                        Err(_) => {
                            kprint(b"nxterm: the compositor went away\n");
                            exit(0);
                        }
                    }
                }
                if events.is_empty() {
                    wait_two(win.wait_handle(), bch);
                    continue; // round again: drain both sources from the top
                }
            }
            None => match win.wait_event() {
                Ok(ev) => events.push(ev),
                Err(_) => {
                    kprint(b"nxterm: the compositor went away\n");
                    exit(0);
                }
            },
        }

        // Set by a `Configure` that actually changed the shape; acted on once, below.
        let mut resized = false;
        for (from, event) in events {
        // **The menu's window routes through the menu's tree.** Same `App`, so an item's `Msg`
        // updates the same state; different tree, layout and router, because they describe a
        // different window. A record for a window that is not one of these two is not possible
        // — `Session` filtered it — but a stale one for a popup just destroyed is, and it is
        // dropped rather than routed into the terminal.
        if popup.as_ref().is_some_and(|p| p.id == from) {
            let menu = app.menu_view();
            let bounds = Rect::new(0, 0, popup.as_ref().map_or(0, |p| p.size.w), popup.as_ref().map_or(0, |p| p.size.h));
            let ml = layout(&menu, bounds, &FontMetrics::new(&font, FONT_PX));
            let msgs: alloc::vec::Vec<Msg> = match event {
                WindowEvent::Key(k) => popup
                    .as_mut()
                    .and_then(|p| p.router.key(&p.tree, &menu, k))
                    .into_iter()
                    .collect(),
                WindowEvent::Pointer(pt) => popup
                    .as_mut()
                    .map(|p| p.router.pointer(&p.tree, &menu, &ml, pt).0)
                    .unwrap_or_default(),
                _ => alloc::vec::Vec::new(),
            };
            for msg in msgs {
                // **The routing proof.** A record arrived naming the popup's window, was routed
                // through the popup's own tree, and produced a message — three things that were
                // each impossible before C3's parts 1 and 2.
                #[cfg(feature = "test-harness")]
                match msg {
                    Msg::Clear => kprint(b"nxterm: menu chose Clear\n"),
                    Msg::Reset => kprint(b"nxterm: menu chose Reset\n"),
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
            // **Everything goes through the router**, including the keys that end up as text:
            // the grid is a focusable widget with an `on_key`, so "typed a character" and
            // "pressed a menu accelerator" are the same path with a different widget claiming
            // it. Whether a key types at all — a repeat does, a release does not — is
            // `App::key`'s, because it is a fact about terminals and not about routing.
            WindowEvent::Key(k) => {
                // **F1 opens the menu, under the harness only.** The gate cannot click the bar
                // button that normally opens it: `nxterm` is created before `ui-testclient`'s
                // windows, so its top-left is underneath them. A key it can inject, and doing
                // it on the gate's schedule matters — an open menu is topmost and takes the
                // keyboard, so opening it earlier would swallow everything typed at the shell.
                #[cfg(feature = "test-harness")]
                if k.keycode == 59 && k.pressed != 0 && !opened_for_harness {
                    opened_for_harness = true;
                    app.update(Msg::ToggleMenu);
                    continue;
                }
                if let Some(msg) = router.key(&tree, &ui, k) {
                    app.update(msg);
                }
            }
            WindowEvent::Pointer(p) => {
                // A button press means the compositor decided this window is under the cursor
                // — and, since click-to-focus raises, that it is now the topmost focusable one
                // and will get the keyboard. Reported under the harness because that is the
                // only evidence a gate has that its click landed *here*: nothing else changes
                // observably, and a focus *change* is not sent when the window already had it.
                #[cfg(feature = "test-harness")]
                if p.kind == librsproto::surface::POINTER_BUTTON
                    && p.flags & librsproto::surface::POINTER_PRESSED != 0
                {
                    kprint(b"nxterm: clicked\n");
                }
                let (msgs, _hit) = router.pointer(&tree, &ui, &l, p);
                for m in msgs {
                    app.update(m);
                }
            }
            WindowEvent::Focus(f) => {
                // Reported under the harness because a gate that types has to know the
                // keyboard arrived first: the compositor sends keys to the topmost focusable
                // window, and `nxterm` is created first — so it is at the *bottom* until a
                // click raises it. Without waiting for this, the gate injects a click and six
                // keystrokes back to back and the keys race the raise.
                #[cfg(feature = "test-harness")]
                libkern::debug::Line::new()
                    .s(b"nxterm: focus=")
                    .u(u64::from(f))
                    .end();
                router.set_window_focused(f);
                // The title bar shows which window has the keyboard, so this is content as well
                // as routing state.
                app.focused = f;
            }
            // **The shell asking, answered the same way the close button is.** There is nothing
            // to refuse with and nothing to save: what a client with unsaved work would do here
            // is open a dialog, which is why this arrives as a request rather than a destruction.
            WindowEvent::CloseRequested => {
                kprint(b"nxterm: asked to close, exiting\n");
                app.update(Msg::Close);
            }
            // Everything accumulated about held keys is a guess now. This client keeps none —
            // modifiers arrive on each event — so there is nothing to discard, and saying so
            // is the point: a client that silently ignored this would be wrong the moment it
            // started tracking anything.
            WindowEvent::Dropped => kprint(b"nxterm: input dropped\n"),
            // **Accepted, as of M9 Part D.** Declining stayed legal and this client did it for
            // three milestones — "a different problem, not a parameter of this one" — which
            // made maximise, snap and every other sized gesture a no-op on the only application
            // there is. Now the window becomes exactly the size asked for, the grid becomes the
            // cells that fit inside it, and the history rewraps.
            //
            // **Same size is the ordinary case and costs nothing**: a `Configure` follows every
            // *move* as well, carrying the origin, and reallocating a window's buffers each
            // time it was dragged would be a resize per pointer motion.
            WindowEvent::Configure { width, height, .. } => {
                // **Counted before, reported beside what it became.** A rewrap moves where the
                // line breaks are and must not create or destroy *lines* — so these two numbers
                // are equal, and an implementation that joined every adjacent row would collapse
                // the history to one and say so here. It is the only assertion about the reflow
                // that a gate on a release image can make: a terminal's rows are somebody's
                // session, and the serial log is not the place for them.
                let lines_before = app.grid.logical_lines();
                if app.resize(Size::new(width, height)) {
                    resized = true;
                    libkern::debug::Line::new()
                        .s(b"nxterm: resized to ")
                        .u(u64::from(width))
                        .s(b"x")
                        .u(u64::from(height))
                        .s(b", grid ")
                        .u(app.grid.cols() as u64)
                        .s(b"x")
                        .u(app.grid.rows() as u64)
                        .s(b", lines ")
                        .u(lines_before as u64)
                        .s(b"->")
                        .u(app.grid.logical_lines() as u64)
                        .end();
                }
            }
        }
        }

        // **The window changed shape, so everything about the frame does.** The compose buffer
        // is a different size, the layout has a different rect to fill, the diff's record
        // describes a tree laid out at the old one, and the grid has different rows. Each of
        // those is cheap and none of them is optional; a frame that missed one would paint the
        // new size through the old arithmetic.
        //
        // Done at the end of the iteration, so the render at the top of the next one is the
        // first to see the new size — and the buffers themselves are replaced there, by
        // `BufferPool::acquire`, because that is where a *free* one is in hand.
        if resized {
            let size = app.window_size();
            match compose_buffer(size) {
                Some(fb) => scratch = fb,
                None => fail(b"nxterm: impossible window geometry\n"),
            }
            bounds = Rect::new(0, 0, size.w, size.h);
            // A tree diffed against a layout from the old bounds reports damage in the old
            // coordinates. Starting again reports the whole window, which is what a resize is.
            tree = Tree::new();
            app.grid.damage_all();
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxterm: panic\n");
    exit(2);
}
