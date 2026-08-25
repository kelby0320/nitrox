//! `desktop-session-mgr` — the graphical login supervisor.
//!
//! `session-mgr`'s twin. It runs the same sequence through the same `libsession` —
//! authenticate, construct the namespace, spawn the leader, reap, tear down — and differs in
//! exactly one place: its **greeter is a window**, not a terminal prompt. That is the split
//! `graphical-session.md` §4 draws, and the reason `libsession` exists.
//!
//! **The greeter is a compositor client that exists before anyone has logged in**, which makes
//! this the first process to hold a `/dev/draw` connection in a *release* image. Everything
//! graphical before it was `selftest`-gated. It is closer to `gdm`'s `class=greeter` than to
//! anything `session-mgr` does: it draws first and outlives every session it starts.
//!
//! **No `/dev/console` in a graphical session.** `libsession::NamespaceSpec::bind_console` is
//! false here, and this is that flag's first caller — governing decision 3.
//!
//! `#![no_std]` + `#![no_main]`, with `alloc`: the same reasoning as `session-mgr`'s, plus a
//! toolkit that builds an `Element` tree per frame.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;
use libkern::debug::Line;
use libkern::*;
use librsproto::surface::{CreateWindowRequest, Role};
use libsurface::Session;
use libsurface::ipc::ChannelTransport;
use libui::element::{Element, Insets, column, padding, row, sized, text};
use libui::layout::layout;
use libui::paint::{FontMetrics, Theme, paint};
use libui::widget::{Palette, TextFieldState, WidgetState, text_field};

/// `alloc` backing: the toolkit builds an element tree per frame and `libsession` builds the
/// session's environment record.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where the font comes from — the same path every other graphical client reads.
const SYSTEM_FONT_PATH: &str = "/system/fonts/DejaVuSansMono.ttf";

/// The greeter window's size. Fixed rather than screen-relative: the compositor places it at
/// the origin today, and a greeter that resized itself would be the first client to have a
/// placement opinion — which is `desktop-shell`'s job from Part E.
const GREETER_W: u32 = 420;
/// See [`GREETER_W`].
const GREETER_H: u32 = 200;
/// Bytes per row. `WIDTH * 4` exactly: nothing here needs the padded pitch the reference UI
/// uses to catch stride bugs, and an unpadded one keeps the buffer copy a memcpy.
const GREETER_PITCH: usize = (GREETER_W as usize) * 4;
/// Text size, in pixels per em.
const FONT_PX: f32 = 16.0;
/// How many buffers the greeter attaches.
const BUFFERS: usize = 2;

/// Write one line to the debug console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Report and exit. A greeter that cannot draw is not a degraded greeter.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// Which field the keyboard is going to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The username field.
    User,
    /// The password field.
    Password,
}

/// Everything the greeter draws from.
struct Greeter {
    /// The username being typed.
    user: TextFieldState,
    /// The password being typed. Rendered masked.
    password: TextFieldState,
    /// Which field has the caret.
    focus: Focus,
    /// Whether the last attempt was refused.
    denied: bool,
}

impl Greeter {
    /// An empty greeter, caret in the username field.
    fn new() -> Self {
        Self {
            user: TextFieldState::new(),
            password: TextFieldState::new(),
            focus: Focus::User,
            denied: false,
        }
    }

    /// The element tree for the current state.
    ///
    /// Rebuilt per frame, which is the toolkit's model: `view(&state) -> Element`.
    fn view(&self) -> Element<()> {
        let palette = Palette::default();
        let active = |f: Focus| WidgetState { active: self.focus == f, ..Default::default() };
        let mut rows = alloc::vec::Vec::with_capacity(6);
        rows.push(text("nitrox"));
        if self.denied {
            // Said once, above the fields, and cleared on the next keystroke. The serial
            // column prints `login incorrect` for the same reason: a refusal a user cannot
            // see is a login that appears to have done nothing.
            rows.push(text("login incorrect"));
        }
        rows.push(row(alloc::vec![
            sized(Size::new(90, 0), text("username")),
            text_field(&self.user, false, active(Focus::User), &palette).flex(1),
        ]));
        rows.push(row(alloc::vec![
            sized(Size::new(90, 0), text("password")),
            text_field(&self.password, true, active(Focus::Password), &palette).flex(1),
        ]));
        padding(Insets::all(16), column(rows))
    }

    /// Render the current state into a fresh framebuffer.
    fn render(&self, font: &Font) -> MemFramebuffer {
        let geometry = Geometry::with_pitch(GREETER_W, GREETER_H, GREETER_PITCH, PixelFormat::XRGB8888)
            .expect("the greeter pitch is wide enough for a row");
        let mut fb = MemFramebuffer::new(geometry);
        let ui = self.view();
        let bounds = Rect::new(0, 0, GREETER_W, GREETER_H);
        let metrics = FontMetrics::new(font, FONT_PX);
        let l = layout(&ui, bounds, &metrics);
        let theme = Theme { font_px: FONT_PX, ..Theme::default() };
        paint(&mut fb, font, &theme, &ui, &l, bounds, &mut |_, _, _, _: &mut MemFramebuffer| {});
        fb
    }
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

/// Bootstrap registers, as `service-mgr`'s spawn fills them: `rdi` = the notification channel
/// this supervisor reaps its session leader on, `rsi` = the inherited LOOKUP-only root
/// namespace, `rdx` = the control channel the endpoints arrive over, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, _control: u64, _arg0: u64) -> ! {
    kprint(b"desktop-session-mgr: up\n");

    // The font, before the window: a greeter that cannot draw text has nothing to show, and
    // failing here reports the real cause rather than an empty window.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let font = match unsafe { libdraw::text::load(root_ns, SYSTEM_FONT_PATH) } {
        Ok(f) => f,
        Err(_) => fail(b"desktop-session-mgr: font load FAILED\n"),
    };

    // SAFETY: `root_ns` is live for this process's whole run.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => t,
        Err(_) => fail(b"desktop-session-mgr: connect to /dev/draw FAILED\n"),
    };
    let mut session = Session::new(transport);
    // **This window lands at the origin, and it is created before every other client's.**
    // `service-mgr` brings the login chain up before it starts declared services, so the
    // greeter is bottom-most and the reference windows `check-display` and `check-terminal`
    // depend on stack above it. That is load-bearing rather than incidental: a greeter
    // created *after* them would cover the regions those gates compare, and the failure would
    // read as a compositing regression. The plan's Milestone 6 Part D says the same thing
    // about `init`'s spawn order — "a stale comment asserting a retired invariant is what the
    // last two milestones each shipped once".
    let window = match session.create(&CreateWindowRequest::new(GREETER_W, GREETER_H, Role::Normal), BUFFERS) {
        Ok(id) => id,
        Err(_) => fail(b"desktop-session-mgr: greeter CreateWindow FAILED\n"),
    };

    let greeter = Greeter::new();
    let picture = greeter.render(&font).into_bytes();
    let len = GREETER_PITCH * GREETER_H as usize;
    if picture.len() != len {
        fail(b"desktop-session-mgr: greeter render is not the size it declares\n");
    }
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            fail(b"desktop-session-mgr: greeter buffer alloc FAILED\n");
        };
        // SAFETY: `addr` maps `len` writable bytes and `picture` holds exactly `len`; the two
        // regions are distinct allocations, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(picture.as_ptr(), addr, len) };
        let Some(mut w) = session.window(window) else {
            fail(b"desktop-session-mgr: greeter window vanished\n");
        };
        if w.attach(i as u32, GREETER_W, GREETER_H, GREETER_PITCH as u32, handle).is_err() {
            fail(b"desktop-session-mgr: greeter AttachBuffer FAILED\n");
        }
    }
    let Some(mut w) = session.window(window) else {
        fail(b"desktop-session-mgr: greeter window vanished\n");
    };
    if w.commit(0, (0, 0, GREETER_W, GREETER_H)).is_err() {
        fail(b"desktop-session-mgr: greeter Commit FAILED\n");
    }
    Line::new()
        .s(b"desktop-session-mgr: greeter presented, window ")
        .u(window as u64)
        .s(b" ")
        .u(GREETER_W as u64)
        .s(b"x")
        .u(GREETER_H as u64)
        .end();

    // The login flow arrives next; for now the greeter is on screen and this supervisor
    // **blocks** on its event channel.
    //
    // Blocks, never spins. A supervisor that spins keeps a run queue non-empty, the idle
    // thread never runs, and deferred handle reclamation lives there — so every exited
    // process on the system stops being reaped and their pipes never close. That is the
    // 2026-07-31 `logging-service` bug, found from a hung shell three subsystems away, and
    // `session-mgr`'s own park comment records it.
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
        // Drain whatever arrived so the channel does not stay signalled. Nothing acts on
        // these yet; the login flow is what reads them.
        let _ = session.pump();
        while session.next_event().is_some() {}
    }
}

/// Wait set for the greeter's event channel.
static mut WAIT_HANDLES: [u64; 1] = [0; 1];
/// One 24-byte `IoResult`.
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"desktop-session-mgr: PANIC\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
