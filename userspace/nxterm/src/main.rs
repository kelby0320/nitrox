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
use libdraw::text::{Font, load_mono, load_ui};
use libkern::debug::Line;
use libkern::{exit, kprint};
use librsproto::clipboard::{CLIP_ANY_SERIAL, CLIP_KIND_TEXT, Clipboard, MAX_CLIP_BYTES};
use librsproto::surface::{CreateWindowRequest, Role};
use libsurface::buffers::BufferPool;
use libsurface::{Session, WindowEvent, ipc::ChannelTransport};
use libterm::render::Metrics;
use libui::diff::Tree;
use libui::damage::union_opt;
use libui::layout::{Layout, layout, locate};
use libui::paint::FontMetrics;
use libui::paint::{Theme, paint};
use libui::route::Router;
use libui::window::Child;
use libui::menu::{Item, KeyOutcome, Menu};
use nxterm::{App, GRID_KEY, GRID_KIND, MENU_BAR_KEY, MENU_COUNT, Msg, rows_in};

/// `alloc` backing — the element tree, the grid and the render all allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Buffers shared with the compositor. Two is the minimum the protocol permits, and it is what
/// lets a frame be drawn while the other is on screen.
const BUFFERS: usize = 2;

/// The terminal's size in cells **at startup**. A `Configure` changes it (M9 Part D).
const COLS: usize = 80;
const ROWS: usize = 24;


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

/// The message on row `i` of menu `open`, if that row is an enabled action.
///
/// **The keyboard's half of choosing.** A pointer click produces its message through the router,
/// which reads it off the element the press landed on; the keyboard has no element, so it names a
/// row and this reads the same table back. Both ends therefore come from `App::menu_table`, and a
/// reordering cannot make Enter and a click do different things.
fn chosen(table: &[Menu<Msg>], open: Option<usize>, i: usize) -> Option<Msg> {
    match table.get(open?)?.items.get(i)? {
        Item::Action { msg, enabled: true, .. } => Some(*msg),
        _ => None,
    }
}

/// Say which item a menu produced, under the harness.
///
/// **A label, not a keystroke.** `check-terminal` needs to know its click reached a row and which
/// one; the row's label is a constant this program compiled in, so there is nothing here anybody
/// typed. Silent in a release image like every other harness receipt.
#[allow(unused_variables)]
fn chose(msg: Msg) {
    #[cfg(feature = "test-harness")]
    {
        let name: &[u8] = match msg {
            Msg::Copy => b"Copy",
            Msg::Paste => b"Paste",
            Msg::Clear => b"Clear",
            Msg::Reset => b"Reset",
            Msg::Close => b"Close Window",
            // Not a menu row: the bar's own words toggle, and everything else arrives from the
            // grid or the title bar. Nothing to report, rather than a line that says "some item".
            _ => return,
        };
        libkern::debug::Line::new().s(b"nxterm: menu chose ").s(name).end();
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
    ui_font: &Font,
    mono_font: &Font,
    theme: &Theme,
    damage: Rect,
) {
    let origin = app.grid_origin();
    let m = app.metrics;
    let grid = &app.grid;
    let palette = app.palette;
    let top = app.view_line();
    paint(fb, ui_font, theme, ui, l, damage, &mut |kind, rect, clip, fb: &mut MemFramebuffer| {
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
        libterm::render::render_view(fb, grid, mono_font, &m, &palette, origin, top, &rows);
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
/// Push `bytes` onto the kill ring.
///
/// **Connects per operation rather than holding a session open**, and that is deliberate: a
/// terminal copies once every few minutes at most, a session costs the server a wait-set slot,
/// and this endpoint has more clients than any other in a graphical session. The cost is one
/// resolve per copy, which is a round trip nobody is waiting on.
fn clipboard_copy(ns: u64, bytes: &[u8]) -> Result<(), &'static str> {
    let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
    let mut clip = Clipboard::connect(ns, &mut buf).map_err(|_| "no /dev/clipboard")?;
    let r = clip.copy(CLIP_KIND_TEXT, bytes).map(|_| ());
    clip.close();
    r.map_err(|_| "the clipboard refused it")
}

/// Read the newest entry into `out`; returns how many bytes it wrote.
///
/// **The newest, with no serial** — see [`nxterm::ClipRequest::Paste`] for why a terminal does
/// not cycle.
fn clipboard_paste(ns: u64, out: &mut [u8]) -> Result<usize, &'static str> {
    let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
    let mut clip = Clipboard::connect(ns, &mut buf).map_err(|_| "no /dev/clipboard")?;
    let r = clip.paste(0, CLIP_ANY_SERIAL, out);
    clip.close();
    match r {
        Ok((_, _, len)) => Ok(len.min(out.len())),
        // An empty ring is not a failure — nobody has copied anything yet, so there is nothing
        // to type and nothing to report.
        Err(e) if e.is_empty() => Ok(0),
        Err(_) => Err("the clipboard refused it"),
    }
}

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

    // **From the environment, not from a default, and read here** — beside the other thing the
    // session tells this program, and before anything is sized with it (M11 Part C). The grid's
    // cells are built from `font_px` below, so a theme read later would size the window from one
    // number and draw it with another.
    let theme = theme_of(&env);

    // **Two fonts, and this is the one window in the system that needs both** (M11 Part D). The
    // menu bar and its popup are widgets and take the proportional face; the grid takes the
    // fixed-advance one, because `libterm` measures a cell from a single glyph's advance and a
    // proportional font has no cell width at all. Loading one and using it for both is what the
    // whole desktop did until this part, and here it would have been visible as a terminal whose
    // columns did not line up.
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
    // **The grid's cell size follows the theme like the chrome does.** A terminal whose window
    // frame drew at one size and whose cells drew at another would be two type scales in one
    // window — the same mistake as laying out with one size and painting with another, one
    // surface further in (PR #263 review, blocking 1).
    let metrics = Metrics::new(&mono_font, theme.font_px);
    // **The cell, said out loud, because a grid drawn in the wrong font is not a crash.**
    // `Metrics` takes a cell's width from one glyph's advance, so a proportional face here
    // produces a plausible number and then draws every column at the wrong x. `check-terminal`
    // recomputes this from the same face on the host and compares, which is a claim the pixels
    // cannot make from inside the guest (M11 Part D).
    //
    // **`mono_path`, not `theme.font_mono`** — the path the load *returned*, which is the
    // built-in one whenever the theme named a font that would not open. Naming the requested
    // file here would put a font this process never read on a line the gate feeds straight back
    // into `host_font` (PR #264 review, finding 1).
    //
    // **And the size as two integers**, because `.u()` truncates and the host re-measures at
    // whatever this says: a grid drawn at 13.5 and reported as 13 is a cell a pixel short and a
    // gate blaming the font for it. `from_config` rounds to a hundredth, so these two are the
    // whole value.
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
        // Zero-padded: `.5` hundredths is `.05`, and a line reading `13.5` would be re-measured
        // at 13.5 rather than the 13.05 it meant.
        .s(if px_cents < 10 { &b"0"[..] } else { &b""[..] })
        .u(px_cents)
        .s(b"px")
        .end();
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

    let mut bounds = Rect::new(0, 0, size.w, size.h);
    let mut tree = Tree::new();
    let mut router = Router::new();
    // What the menu's window is, and why it is one.
    //
    // **The arithmetic that used to live here is `MenuState::anchor`'s** (M14 Part A) — "directly
    // under the word it drops from, in the parent's coordinates" is the same sentence in every menu,
    // and it was written twice. What is left is the reasoning, which is not.
    //
    // **A `popup`, not a layer.** `libui`'s `offset` clips at its parent's edge, which is right one
    // level down and wrong for a menu — "a menu clipped to its window is not a menu"
    // (`display-substrate.md` §4a). Until M6 C3 the menu was a `Stack` layer hoisted to the whole
    // window, which worked only because it happened to fit inside the terminal. As a `popup` window
    // it is parented to the terminal, positioned by *this* client at the anchor the layout gives,
    // and clipped only by the screen.
    //
    // **The window itself is a [`Child`]** (M12 Part A). This file held a `Popup` struct — an id, a
    // pool, a scratch framebuffer, a tree and a router, with `open`/`present`/`close` over them —
    // until an editor's confirmation dialog wanted the same six fields and the same three
    // operations. Two consumers is when a helper goes down a layer, so it did; nothing about the
    // menu changed, and the lessons that struct had learned went with it.
    //
    // The menu's window while it is open, and nothing at all while it is not — a popup is
    // transient by role, so closing the menu destroys it rather than hiding it.
    let mut popup: Option<Child> = None;
    // Which item the pointer was over at the last paint of the menu.
    //
    // **Kept so a change can be *reported*, not so the view can be built** — the view reads
    // `Child::hovered_key` directly, which is the one source. This is a receipt: a gate driving
    // a release image sees nothing of a highlight, and hover is the first thing in this system
    // that reacted to the pointer moving without a button held, so "the path works" needed
    // something to assert (M11 Part E batch 3).
    let mut menu_hovered: Option<u64> = None;
    // Which menu the live popup was opened for, so a *change* — not merely open-versus-shut —
    // rebuilds the window at the new word and at the new menu's size. The other two applications
    // have carried this since they grew a second menu.
    let mut menu_shown: Option<usize> = None;

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
        // **The widget under the pointer, from the router that already knew.** `Router::inside`
        // has reported it since M4; nothing had ever asked, so nothing in this system reacted to
        // the pointer moving over it (M11 Part E batch 3).
        let ui = app.view(&theme, router.hovered_key(&tree));
        let l = layout(&ui, bounds, &FontMetrics::new(&ui_font, theme.font_px));
        // The anchors for the next frame's popup — **one per bar word**. Read every frame rather
        // than only when a menu opens: a word's position is a fact about the layout, not about
        // the menu, and reading it on open means reading it before the first layout exists.
        app.menus.set_anchors(
            (0..MENU_COUNT).map(|i| locate(&ui, &l, MENU_BAR_KEY + i as u64)).collect(),
        );

        // ---- the menu's window ----
        //
        // **Opened and destroyed with the menu**, because `popup` is a transient role: the
        // compositor takes it with its parent, it takes keyboard focus while it is up, and a
        // hidden one would still be a window in the stack. The anchor has to exist first —
        // before the first layout there is nowhere to put it, which is why this reads the
        // anchor computed just above rather than the one from when the menu was toggled.
        //
        // **Which menu, not whether one is open** (PR #280 review, blocking 2). The discriminator
        // was `open().is_some()`, so switching from File to Edit — which `MenuState::key` does on
        // Left and Right — left the existing window at the old word and, worse, at the size
        // measured for the old menu: `Child::present` lays the new tree into a rectangle fixed at
        // `Child::open`, so the rows a five-row menu grew were simply clipped away. It was
        // unreachable before this part, because this terminal had one menu and no arrow keys.
        if menu_shown != app.menus.open() {
            if let Some(p) = popup.take() {
                p.close(&mut win);
            }
            // The window is gone, so nothing is under the pointer in it. Left set, the next menu
            // would open believing a row was already highlighted and report no change when one
            // really was.
            menu_hovered = None;
            menu_shown = app.menus.open();
        }
        match (menu_shown.is_some(), popup.is_some(), app.menus.anchor()) {
            (true, false, Some(anchor)) => {
                // **Measured from the menu with no hover**, because the pointer is over the
                // *bar* item that opened this rather than over the popup — and a highlight does
                // not change what a menu measures.
                let menu = app.menu_view(&theme, None);
                popup = Child::open(
                    &mut win,
                    Role::Popup { parent: window_id },
                    anchor,
                    &menu,
                    &ui_font,
                    &theme,
                    BUFFERS,
                );
                // **Where it is and how big**, because the gate has no other way to see a
                // second window: it reads the serial log, and this is the only thing that
                // says the menu became a window rather than a layer. The origin is in screen
                // coordinates, which is what a click has to be aimed at.
                #[cfg(feature = "test-harness")]
                if let Some(p) = popup.as_ref() {
                    let o = win
                        .window(p.id())
                        .and_then(|w| w.configured())
                        .map_or((0, 0), |c| (c.x, c.y));
                    libkern::debug::Line::new()
                        .s(b"nxterm: menu popup ").u(p.id() as u64)
                        .s(b" at ").i(o.0 as i64).s(b",").i(o.1 as i64)
                        .s(b" ").u(p.size().w as u64).s(b"x").u(p.size().h as u64)
                        .end();
                }
                if popup.is_none() {
                    // Not fatal: the terminal is still usable without its menu, and saying so
                    // beats a window that silently never appears.
                    kprint(b"nxterm: could not open the menu popup\n");
                    app.menus.close();
                    menu_shown = None;
                }
            }
            (false, true, _) => {
                if let Some(p) = popup.take() {
                    p.close(&mut win);
                }
                menu_hovered = None;
            }
            _ => {}
        }
        if let Some(p) = popup.as_mut() {
            let now = p.hovered_key();
            if now != menu_hovered {
                menu_hovered = now;
                // The item's key, which is a number this program chose — not a label, and not a
                // position. There is nothing here a person typed.
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
        if app.menus.open().is_none()
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
            draw(&mut scratch, &app, &ui, &l, &ui_font, &mono_font, &theme, d);
            let Some(mut w) = win.window(window_id) else {
                fail(b"nxterm: our own window is gone\n");
            };
            // **The size is asked for here rather than remembered**, so a buffer left at the
            // old shape by a resize is replaced at the moment it is next drawn into — which is
            // the frame after the one that committed the new size, when its release arrives.
            let Ok(b) = pool.acquire(&mut w, app.window_size()) else {
                // Two causes since Part D, and the message names both: no buffer came back from
                // the compositor, or the memory for one at the new size could not be had.
                fail(b"nxterm: no buffer to draw into\n");
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

        // **The corner grip.** Asked for, not done — like the move above and for the same
        // reason: the compositor holds the grab this press opened, and it is the only
        // participant that can follow a pointer without a round trip per motion. What comes
        // back is an ordinary `Configure` at the end of the gesture, from the shell.
        if let Some(edges) = app.take_resize_request()
            && let Some(mut w) = win.window(window_id)
        {
            match w.start_resize(edges) {
                Ok(()) => kprint(b"nxterm: dragging its own corner\n"),
                Err(_) => kprint(b"nxterm: the compositor refused the resize\n"),
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

        // ---- the clipboard ----
        //
        // **Here rather than in `update`**, for the reason every other syscall in this loop is:
        // `App::update` is a function of values, and `/dev/clipboard` is IPC. A copy is one
        // round trip; a paste is one round trip and then the bytes go down the pty exactly as
        // if they had been typed.
        if let Some(req) = app.take_clip_request() {
            match req {
                nxterm::ClipRequest::Copy(text) => match clipboard_copy(root_ns, text.as_bytes()) {
                    // **A length, never the text** — this is a terminal, and what a person
                    // selected is as much theirs as what they typed.
                    Ok(()) => Line::new()
                        .s(b"nxterm: copied ")
                        .u(text.len() as u64)
                        .s(b" bytes")
                        .end(),
                    Err(why) => Line::new().s(b"nxterm: the copy failed: ").s(why.as_bytes()).end(),
                },
                nxterm::ClipRequest::Paste => {
                    let mut got = [0u8; MAX_CLIP_BYTES];
                    match clipboard_paste(root_ns, &mut got) {
                        Ok(n) => {
                            if let Ok(text) = core::str::from_utf8(&got[..n]) {
                                app.pasted(text);
                                Line::new()
                                    .s(b"nxterm: pasted ")
                                    .u(n as u64)
                                    .s(b" bytes")
                                    .end();
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
        if popup.as_ref().is_some_and(|p| p.id() == from) {
            // **A press landed outside the menu, so it goes away** (M11 Part E batch 5). This is
            // the one thing a popup's owner cannot work out for itself: it never sees a press
            // aimed at another window, and until the compositor said so a menu stayed open over
            // whatever was clicked next.
            //
            // **Answered here rather than in `Child::route`**, because what a dismissal *means*
            // is the caller's: this menu goes away, where another client's child window might
            // save something first.
            if matches!(event, WindowEvent::Dismissed) {
                app.menus.close();
                continue;
            }
            // **Arrows, Esc and Enter drive the open menu**, which is possible only here: the
            // popup holds the keyboard while it is up, so these keys arrive naming *its* window
            // and never reach the terminal's router below. `KeyOutcome` says what the press did,
            // and `Chose` carries the menu and the row, so the message comes from the same table
            // — not from a second list that could disagree about which row is which.
            if let WindowEvent::Key(k) = event {
                let table = app.menu_table();
                match app.menus.key(&k, &table) {
                    KeyOutcome::Chose { menu, item } => {
                        // **The menu comes from the outcome, not from the state**: choosing
                        // closes, so `app.menus.open()` is already `None` here and asking it
                        // would drop every message Enter produces (PR #280 review, blocking 1).
                        if let Some(m) = chosen(&table, Some(menu), item) {
                            chose(m);
                            app.update(m);
                        }
                        continue;
                    }
                    // Dismissed already closed it; Changed moved the cursor and the frame above
                    // will redraw with it. Both are handled, and neither reaches the grid.
                    KeyOutcome::Dismissed | KeyOutcome::Changed => continue,
                    KeyOutcome::Ignored => {}
                }
            }
            let menu = app.menu_view(&theme, popup.as_ref().and_then(|p| p.hovered_key()));
            let msgs = popup
                .as_mut()
                .map(|p| p.route(&menu, &ui_font, &theme, &event))
                .unwrap_or_default();
            for msg in msgs {
                // **The routing proof.** A record arrived naming the popup's window, was routed
                // through the popup's own tree, and produced a message — three things that were
                // each impossible before C3's parts 1 and 2.
                chose(msg);
                // The menu closes itself: `App::update` asks the table whether the message is a
                // row's, so neither this loop nor any `update` arm has to remember.
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
                if k.keycode == 59 && k.pressed != 0 {
                    // **A plain toggle since PR #280's review.** It was a one-shot, guarded so
                    // the menu could not open before the gate had finished typing at the shell —
                    // but *when* it opens is the gate's schedule, not this flag's, and the
                    // one-shot meant the menu could never be reopened. The keyboard half of an
                    // open menu had no coverage at all as a result, which is where both of that
                    // review's blocking findings were living.
                    app.update(Msg::MenuBar(nxterm::HARNESS_MENU));
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
            // A `normal` window is not dismissed by a press elsewhere — the event is a popup's.
            WindowEvent::Dismissed => {}
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
            // **A terminal declares no acceptor, so this never arrives** — the compositor
            // matches before it highlights. The arm exists because the match is exhaustive, and
            // that is the point: every client now has to decide what a drop means to it rather
            // than inheriting a default. What a dropped file should do here — type its path? open
            // it? — is a shell question this milestone deliberately does not answer.
            WindowEvent::Drop { .. } => kprint(b"nxterm: a drop arrived; this window takes none\n"),
            WindowEvent::InputLost => kprint(b"nxterm: input dropped\n"),
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
                if let Some(r) = app.resize(Size::new(width, height)) {
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
                        // **The eviction, because without it the difference is unattributable.**
                        // Narrowing makes more rows out of the same text, so a deep history
                        // loses its oldest to the ring rather than to the rewrap — and a reader
                        // of this line who could not tell those apart would go looking for a
                        // reflow bug that is not there (PR #252 review, finding 2).
                        .s(b", ")
                        .u(r.evicted as u64)
                        .s(b" evicted")
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
