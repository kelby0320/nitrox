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
use libui::element::{Element, Insets, padding, text};
use libui::layout::layout;
use libui::paint::{FontMetrics, Theme, paint};

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

/// The top bar's element tree.
fn bar_view() -> Element<()> {
    padding(Insets { top: 4, right: 8, bottom: 4, left: 8 }, text("nitrox"))
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

/// Wait set for the compositor's event channel.
static mut WAIT_HANDLES: [u64; 1] = [0; 1];
/// One 24-byte `IoResult`.
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// Bootstrap registers, as `libsession::spawn_leader` fills them: `rdi` = notification
/// channel, `rsi` = the **session** namespace, `rdx` = the Tier-1 setup channel carrying
/// `argv` and the environment, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, session_ns: u64, _setup: u64, _arg0: u64) -> ! {
    kprint(b"desktop-shell: up (graphical session leader)\n");

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
    Line::new()
        .s(b"desktop-shell: top bar presented, window ")
        .u(window as u64)
        .s(b" ")
        .u(SCREEN_W as u64)
        .s(b"x")
        .u(BAR_H as u64)
        .end();

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
        while session.next_event().is_some() {}
    }
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
