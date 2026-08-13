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
use libdraw::geom::Rect;
use libdraw::text::{Font, SYSTEM_FONT_PATH, load};
use libkern::{SYS_MEMORY_CREATE, SYS_MEMORY_MAP, exit, kprint, syscall4};
use librsproto::surface::Role;
use libsurface::{Window, WindowEvent, ipc::ChannelTransport};
use libterm::render::Metrics;
use libui::diff::Tree;
use libui::damage::union_opt;
use libui::layout::{Layout, layout, locate};
use libui::paint::FontMetrics;
use libui::paint::{Theme, paint};
use libui::route::Router;
use nxterm::{App, GRID_KEY, GRID_KIND, MENU_ITEM_KEY, Msg, rows_in};

/// `alloc` backing — the element tree, the grid and the render all allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Buffers shared with the compositor. Two is the minimum the protocol permits, and it is what
/// lets a frame be drawn while the other is on screen.
const BUFFERS: usize = 2;

/// The terminal's size in cells. Fixed for this milestone — M6 owns resize.
const COLS: usize = 80;
const ROWS: usize = 24;

/// Text size, matching the toolkit's default theme so the chrome and the grid agree.
const FONT_PX: f32 = 16.0;

/// Create a `MemoryObject` of `len` bytes and map it read-write.
fn shared_buffer(len: usize) -> Option<(u64, *mut u8)> {
    // SAFETY: a plain anonymous object of `len` bytes.
    let h = unsafe { syscall4(SYS_MEMORY_CREATE, len as u64, 0, 0, 0) };
    if h <= 0 {
        return None;
    }
    // SAFETY: mapping an object this process just created, read-write.
    let addr = unsafe {
        syscall4(
            SYS_MEMORY_MAP,
            h as u64,
            0,
            len as u64,
            libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE,
        )
    };
    if addr <= 0 {
        // SAFETY: the map failed, so nothing references the object; closing our only handle.
        unsafe { syscall4(libkern::SYS_HANDLE_CLOSE, h as u64, 0, 0, 0) };
        return None;
    }
    Some((h as u64, addr as *mut u8))
}

/// Report and end the run.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: under the test-harness kernel this terminates QEMU; elsewhere it is
    // `Unsupported` and falls through.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
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
    paint(fb, font, theme, ui, l, damage, &mut |kind, _rect, clip, fb: &mut MemFramebuffer| {
        if kind != GRID_KIND {
            return;
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
pub extern "C" fn _start(_notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"nxterm: up\n");

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
    let mut win = match Window::new(transport, size.w, size.h, Role::Normal, BUFFERS) {
        Ok(w) => w,
        Err(_) => fail(b"nxterm: CreateWindow FAILED\n"),
    };

    // Two shared buffers, and a scratch framebuffer to compose into.
    //
    // **Drawn once here and copied per frame**, rather than painted directly into whichever
    // buffer is free: the toolkit's damage describes what changed since the *last frame*, and
    // the free buffer holds the frame before that. Painting a one-row damage straight into it
    // would leave the row from two frames ago everywhere else. `libui::damage`'s per-buffer
    // accumulation is the real answer and it belongs in Part B's successor; a copy is correct
    // now and is one `memcpy` of a window.
    let pitch = size.w as usize * 4;
    let len = pitch * size.h as usize;
    let geometry = match Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888) {
        Some(g) => g,
        None => fail(b"nxterm: impossible window geometry\n"),
    };
    let mut scratch = MemFramebuffer::new(geometry);
    let mut maps: [*mut u8; BUFFERS] = [core::ptr::null_mut(); BUFFERS];
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            fail(b"nxterm: buffer alloc FAILED\n");
        };
        maps[i] = addr;
        if win.attach(i as u32, size.w, size.h, pitch as u32, handle).is_err() {
            fail(b"nxterm: AttachBuffer FAILED\n");
        }
    }

    let theme = Theme { font_px: FONT_PX, ..Theme::default() };
    let bounds = Rect::new(0, 0, size.w, size.h);
    let mut tree = Tree::new();
    let mut router = Router::new();

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
            if unsafe { backend::spawn_shell(root_ns, terminal) } < 0 {
                kprint(b"nxterm: no shell\n");
            }
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
            let Ok(b) = win.acquire() else {
                fail(b"nxterm: no buffer released\n");
            };
            // SAFETY: `maps[b]` maps `len` writable bytes and `scratch` holds exactly `len`;
            // the two are distinct allocations.
            unsafe {
                core::ptr::copy_nonoverlapping(scratch.bytes().as_ptr(), maps[b as usize], len)
            };
            if win.commit(b, (d.origin.x as u32, d.origin.y as u32, d.size.w, d.size.h)).is_err() {
                fail(b"nxterm: Commit FAILED\n");
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
        let mut events: alloc::vec::Vec<WindowEvent> = alloc::vec::Vec::new();
        match backend.as_ref().map(|b| b.channel).filter(|_| win.wait_handle() != 0) {
            Some(bch) => {
                loop {
                    match win.poll_event() {
                        Ok(Some(e)) => events.push(e),
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
                Ok(e) => events.push(e),
                Err(_) => {
                    kprint(b"nxterm: the compositor went away\n");
                    exit(0);
                }
            },
        }

        for event in events {
        match event {
            // **Everything goes through the router**, including the keys that end up as text:
            // the grid is a focusable widget with an `on_key`, so "typed a character" and
            // "pressed a menu accelerator" are the same path with a different widget claiming
            // it. Whether a key types at all — a repeat does, a release does not — is
            // `App::key`'s, because it is a fact about terminals and not about routing.
            WindowEvent::Key(k) => {
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
            }
            // Everything accumulated about held keys is a guess now. This client keeps none —
            // modifiers arrive on each event — so there is nothing to discard, and saying so
            // is the point: a client that silently ignored this would be wrong the moment it
            // started tracking anything.
            WindowEvent::Dropped => kprint(b"nxterm: input dropped\n"),
        }
        }
        let _ = &maps;
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxterm: panic\n");
    exit(2);
}
