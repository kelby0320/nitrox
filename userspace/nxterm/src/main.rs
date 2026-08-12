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

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::Rect;
use libdraw::text::{Font, SYSTEM_FONT_PATH, load};
use libkern::debug::Line;
use libkern::{SYS_MEMORY_CREATE, SYS_MEMORY_MAP, exit, kprint, syscall4};
use librsproto::surface::{KEY_DOWN, KEY_REPEAT, Role};
use libsurface::{Window, WindowEvent, ipc::ChannelTransport};
use libterm::render::Metrics;
use libui::diff::Tree;
use libui::damage::union_opt;
use libui::layout::{Layout, layout, locate};
use libui::paint::FontMetrics;
use libui::paint::{Theme, paint};
use libui::route::Router;
use nxterm::{App, GRID_KIND, MENU_ITEM_KEY, Msg, rows_in};

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
    paint(fb, font, theme, ui, l, damage, &mut |kind, _rect, clip, fb: &mut MemFramebuffer| {
        if kind != GRID_KIND {
            return;
        }
        let rows = rows_in(clip, origin, &m, grid.rows());
        libterm::render::render_rows(fb, grid, font, &m, &palette, origin, &rows);
    });
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
    let mut encoded = [0u8; libterm::encode::MAX_ENCODED];

    // A banner, so a screenshot of a fresh terminal is not an empty rectangle and so the
    // guest's log says the grid is live before any key arrives.
    app.feed(b"nxterm ready\r\n");

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
        let mut damage = ui_damage;
        // The grid's own rows, in window coordinates, unioned in.
        let origin = app.grid_origin();
        for row in app.grid.take_damage() {
            let r = app.metrics.row_rect(row, app.grid.cols());
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

        // ---- wait for something to do ----
        let Ok(event) = win.wait_event() else {
            kprint(b"nxterm: the compositor went away\n");
            exit(0);
        };
        match event {
            WindowEvent::Key(k) => {
                // Presses and repeats type; releases do not. A terminal sends on the way down,
                // and a repeat is that press continuing.
                if k.pressed != KEY_DOWN && k.pressed != KEY_REPEAT {
                    continue;
                }
                // The chrome gets first refusal — the toolkit's routing decides whether a
                // focused widget claims the key — and anything it does not claim is text.
                if let Some(msg) = router.key(&tree, &ui, k) {
                    app.update(msg);
                    continue;
                }
                let n = libterm::encode::encode(k.keycode, k.modifiers, &mut encoded);
                if n > 0 {
                    // **The loopback.** Part C replaces this one line with a write to the tty
                    // server's backend channel.
                    app.loopback(&encoded[..n]);
                    Line::new().s(b"nxterm: typed ").u(n as u64).s(b" byte(s)").end();
                }
            }
            WindowEvent::Pointer(p) => {
                let (msgs, _hit) = router.pointer(&tree, &ui, &l, p);
                for m in msgs {
                    app.update(m);
                }
            }
            WindowEvent::Focus(f) => router.set_window_focused(f),
            // Everything accumulated about held keys is a guess now. This client keeps none —
            // modifiers arrive on each event — so there is nothing to discard, and saying so
            // is the point: a client that silently ignored this would be wrong the moment it
            // started tracking anything.
            WindowEvent::Dropped => kprint(b"nxterm: input dropped\n"),
        }
        let _ = &maps;
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxterm: panic\n");
    exit(2);
}
