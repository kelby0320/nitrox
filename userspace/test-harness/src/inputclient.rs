//! `input-testclient` — proves the i8042 driver and the input path deliver events, which bring-up
//! alone does not show. It prints what arrives on `/dev/input/new` (the **merged** stream from
//! `input-server`), then drives a window and the toolkit; `cargo xtask check-input` injects
//! keystrokes and clicks over QMP and checks the decoded events.
//!
//! **It prints `listening` once its reads are parked**, and the gate waits for that line before
//! injecting: an earlier event would land before anyone reads.

#![no_std]
#![no_main]

extern crate alloc;

use libkern::abi::{INPUT_EVENT_LEN, InputEvent};
use libkern::debug::Line;
use libkern::{
    RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT, SYS_NS_LOOKUP, SYS_WAIT, exit, kprint, syscall2, syscall4, syscall5,
};
use librsproto::surface::{CreateWindowRequest, KeyEvent, PointerEvent, Role};
use libsurface::{Session, WindowEvent, ipc::ChannelTransport};
use libui::diff::Tree;
use libui::element::{Element, custom};
use libui::layout::{FixedCell, layout};
use libui::route::Router;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The invisible window's size: nothing is ever committed, so the compositor skips it; **larger
/// than any screen**, as new windows land at `(0, 0)`, so it contains the cursor and the gate can
/// click without steering it first.
const WIN_W: u32 = 2048;
const WIN_H: u32 = 2048;

/// How long the stall stage refuses to drain, in ns, while the harness floods motion over QMP.
const STALL_NS: u64 = 1_500_000_000;

/// The key injected *after* the flood; its arrival proves nothing was dropped.
const LATE_CODE: u16 = 46;

/// How long the motion phase refuses to drain the **device** stream, in ns.
const INPUT_STALL_NS: u64 = 3_000_000_000;

/// The key the harness presses to ask for the motion phase, noticed by the raw-stream pump: a
/// clock or wait of its own would delay every display gate; `F2` is unbound.
const START_CODE: u16 = 60;

/// How long a drain waits with nothing arriving before calling the stream quiet: with the i8042's
/// interrupts off (`check-input --no-ps2-irq`) a burst arrives over *seconds*.
const QUIET_NS: u64 = 4_000_000_000;

const GRID: u32 = 1;

/// What the toolkit hands back when an event reaches the widget.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Msg {
    Key(KeyEvent),
    Ptr(PointerEvent),
    /// The wheel, which reaches a *different* handler — see `Element::on_wheel`.
    Wheel(PointerEvent),
}

const EVENT_LEN: usize = INPUT_EVENT_LEN;
/// The last thing the harness injects, so what "done" means: a sentinel, not a record count.
const DONE_CODE: u16 = libkern::abi::BTN_LEFT;
const PAYLOAD_OFF: usize = 24;

static mut CLOCK_BUF: u64 = 0;
static mut WAIT_HANDLES: [u64; 2] = [0; 2];
static mut WAIT_RESULTS: [u8; 48] = [0; 48];

fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    if waited != 1 {
        return (-1, 0);
    }
    // SAFETY: the kernel filled one 24-byte result record.
    unsafe {
        let st = i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]);
        let res = u64::from_le_bytes([
            WAIT_RESULTS[16],
            WAIT_RESULTS[17],
            WAIT_RESULTS[18],
            WAIT_RESULTS[19],
            WAIT_RESULTS[20],
            WAIT_RESULTS[21],
            WAIT_RESULTS[22],
            WAIT_RESULTS[23],
        ]);
        (st, res)
    }
}

fn lookup(ns: u64, path: &[u8], rights: u64) -> Option<u64> {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return None;
    }
    let (status, resolved) = po_wait(po as u64);
    if status != 0 || resolved == 0 { None } else { Some(resolved) }
}

/// The consumer end of `/dev/input/new`, and the last batch received on it.
struct Stream {
    channel: u64,
    msg: [u8; 4096],
    /// The harness pressed [`START_CODE`], asking for the motion phase.
    start_requested: bool,
    /// The idle deadline passed rather than the channel failing: nobody is driving this run.
    timed_out: bool,
}

impl Stream {
    fn open(root_ns: u64) -> Option<Self> {
        let channel = lookup(root_ns, b"/dev/input/new", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT)?;
        Some(Self { channel, msg: [0; 4096], start_requested: false, timed_out: false })
    }

    /// Sum relative motion until the stream is quiet: `(dx, dy, announced, widest)`.
    ///
    /// It runs first, as a device stream cannot be delimited from inside the guest. **`widest`
    /// tells a consumer a deferral happened**: recovery is invisible, but a folded group carries
    /// the *sum* of its batches, so it is wider than any injected delta.
    fn sum_motion(&mut self) -> (i64, i64, u64, i64) {
        let (mut dx, mut dy, mut announced, mut widest) = (0i64, 0i64, 0u64, 0i64);
        loop {
            let deadline = deadline_ns(QUIET_NS);
            // SAFETY: waiting on this process's own channel handle.
            let waited = unsafe {
                WAIT_HANDLES[0] = self.channel;
                syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline)
            };
            if waited != 1 {
                return (dx, dy, announced, widest); // quiet for long enough
            }
            let _sent = match libkern::channel_recv(self.channel, &mut self.msg) {
                Ok(sent) => sent,
                Err(rr) if rr == libkern::error::KError::WouldBlock.as_i32() as i64 => continue,
                Err(_) => return (dx, dy, announced, widest),
            };
            let payload_len = u32::from_le_bytes([self.msg[4], self.msg[5], self.msg[6], self.msg[7]]) as usize;
            let req = &self.msg[PAYLOAD_OFF..PAYLOAD_OFF + payload_len.min(4096 - PAYLOAD_OFF)];
            let Ok(m) = librsproto::decode(req) else { return (dx, dy, announced, widest) };
            if m.op != librsproto::OP_INPUT_EVENTS {
                continue;
            }
            for i in 0..(m.body.len() / EVENT_LEN) {
                let Some(ev) = InputEvent::read(&m.body[i * EVENT_LEN..(i + 1) * EVENT_LEN]) else {
                    continue;
                };
                match (ev.kind, ev.code) {
                    (libkern::abi::EV_REL, libkern::abi::REL_X) => {
                        dx += ev.value as i64;
                        widest = widest.max((ev.value as i64).abs());
                    }
                    (libkern::abi::EV_REL, libkern::abi::REL_Y) => dy += ev.value as i64,
                    (libkern::abi::EV_SYN, libkern::abi::SYN_DROPPED) => {
                        announced += ev.value.max(0) as u64;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Block for one `Input::Events` message and print its records; whether it held [`DONE_CODE`]'s
    /// press.
    fn pump(&mut self) -> Option<bool> {
        // **Bounded**: the next step creates a window that takes every click.
        let deadline = deadline_ns(IDLE_LIMIT_NS);
        loop {
            // SAFETY: waiting on this process's own channel handle.
            let waited = unsafe {
                WAIT_HANDLES[0] = self.channel;
                syscall5(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline, 0)
            };
            if waited != 1 {
                self.timed_out = deadline_ns(0) >= deadline;
                return None;
            }
            let _sent = match libkern::channel_recv(self.channel, &mut self.msg) {
                Ok(sent) => sent,
                Err(rr) if rr == libkern::error::KError::WouldBlock.as_i32() as i64 => continue, // woken with nothing to take
                Err(_) => return None,
            };
            let payload_len = u32::from_le_bytes([self.msg[4], self.msg[5], self.msg[6], self.msg[7]]) as usize;
            let req = &self.msg[PAYLOAD_OFF..PAYLOAD_OFF + payload_len.min(4096 - PAYLOAD_OFF)];
            let Ok(m) = librsproto::decode(req) else { return None };
            if m.op != librsproto::OP_INPUT_EVENTS {
                continue;
            }
            let n = m.body.len() / EVENT_LEN;
            let mut done = false;
            for i in 0..n {
                let ev = InputEvent::read(&m.body[i * EVENT_LEN..])?;
                if ev.kind == libkern::abi::EV_KEY && ev.code == DONE_CODE && ev.value == 1 {
                    done = true;
                }
                if ev.kind == libkern::abi::EV_KEY && ev.code == START_CODE && ev.value == 1 {
                    self.start_requested = true;
                }
                Line::new()
                    .s(b"input-testclient: ev")
                    .s(b" kind=")
                    .u(ev.kind as u64)
                    .s(b" code=")
                    .u(ev.code as u64)
                    .s(b" value=")
                    .i(ev.value as i64)
                    .end();
            }
            return Some(done);
        }
    }
}

/// Entry point; `root_ns` is the process's root namespace.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"input-testclient: up\n");

    let Some(mut stream) = Stream::open(root_ns) else {
        kprint(b"input-testclient: /dev/input/new FAILED\n");
        exit(1);
    };

    // The harness waits for this before injecting.
    kprint(b"input-testclient: listening\n");

    // Pump until the button press arrives, the last injection; message boundaries are timing only.
    loop {
        match stream.pump() {
            Some(true) => break,
            Some(false) => {
                // ---- The motion phase, asked for by a keystroke ----
                // Covers server→consumer, where a relative delta can be *destroyed*, not delayed.
                // **The stall is this client refusing to read**, so the overrun means the same
                // under TCG and KVM. It runs inside the pump, on a key, not on its own clock or
                // wait.
                if core::mem::take(&mut stream.start_requested) {
                    kprint(b"input-testclient: input stalled\n");
                    sleep_ns(notif, INPUT_STALL_NS);
                    let (dx, dy, announced, widest) = stream.sum_motion();
                    Line::new()
                        .s(b"input-testclient: motion sum dx=")
                        .i(dx)
                        .s(b" dy=")
                        .i(dy)
                        .s(b" announced=")
                        .u(announced)
                        .s(b" widest=")
                        .i(widest)
                        .end();
                }
            }
            None if stream.timed_out => {
                // Nothing is driving this run; exiting keeps this client out of other gates' way.
                // **A distinct message from the window-stage one**: here no window exists yet, so a
                // gate may act at once.
                kprint(b"input-testclient: idle before the window phase\n");
                exit(0);
            }
            None => {
                kprint(b"input-testclient: stream FAILED\n");
                exit(1);
            }
        }
    }

    // ---- The window stage: the same events through a *window*, created after the raw stage ----
    // SAFETY: `root_ns` is this process's live root namespace handle
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => t,
        Err(_) => {
            kprint(b"input-testclient: /dev/draw connect FAILED\n");
            exit(1);
        }
    };
    let mut win = Session::new(alloc::boxed::Box::new(transport));
    let Ok(window_id) = win.create(&CreateWindowRequest::new(WIN_W, WIN_H, Role::Normal), 2) else {
        kprint(b"input-testclient: Session::create FAILED\n");
        exit(1);
    };

    // ---- The toolkit, driven by real events from a QMP injection ----
    let view: Element<Msg> = custom(GRID, libdraw::geom::Size::new(WIN_W, WIN_H))
        .on_key(|k| Some(Msg::Key(k)))
        .on_pointer(Msg::Ptr)
        // **A separate handler, the thing worth proving**: `on_pointer` never receives a wheel.
        .on_wheel(Msg::Wheel)
        .focusable();
    let bounds = libdraw::geom::Rect::new(0, 0, WIN_W, WIN_H);
    let cells = FixedCell { w: 8, h: 16 };
    let laid = layout(&view, bounds, &cells);
    let mut tree = Tree::new();
    if tree.update(&view, &laid).is_err() {
        kprint(b"input-testclient: toolkit diff FAILED\n");
        exit(1);
    }
    let mut router = Router::new();
    match tree.root().map(|w| w.id) {
        Some(id) if router.focus(&tree, &view, id) => {}
        _ => {
            kprint(b"input-testclient: toolkit focus FAILED\n");
            exit(1);
        }
    }

    // The second synchronisation point, like `listening`.
    Line::new().s(b"input-testclient: window ready id=").u(window_id as u64).end();

    // **A button *press* ends this phase**: the raw stream's leftover release arrives first. **The
    // first event must be the focus change**: asserting the *order* catches a compositor announcing
    // focus late.
    let mut first_event_reported = false;

    let (mut saw_key, mut saw_press) = (false, false);
    while !(saw_key && saw_press) {
        let ev = match wait_event_before(&mut win, deadline_ns(IDLE_LIMIT_NS)) {
            Some(Ok((_, e))) => e,
            Some(Err(_)) => {
                kprint(b"input-testclient: window stream FAILED\n");
                exit(1);
            }
            // **Nothing is driving this run.** Exiting releases the 2048×2048 window, which takes
            // every click and keystroke; `check-terminal` found its click landing here.
            None => {
                // **An announcement, not a completion**: the window outlives this line, so a gate
                // clicking on seeing it can still land on it.
                kprint(b"input-testclient: releasing the window (still up)\n");
                exit(0);
            }
        };
        if !first_event_reported {
            first_event_reported = true;
            let what: &[u8] = match ev {
                WindowEvent::Focus(_) => b"focus",
                WindowEvent::Key(_) => b"key",
                WindowEvent::Pointer(_) => b"pointer",
                WindowEvent::InputLost => b"dropped",
                WindowEvent::Drop { .. } => b"drop",
                WindowEvent::CloseRequested => b"close",
                WindowEvent::Dismissed => b"dismissed",
                WindowEvent::Configure { .. } => b"configure",
                WindowEvent::State(_) => b"state",
            };
            Line::new().s(b"input-testclient: first win event=").s(what).end();
        }
        match ev {
            WindowEvent::Key(k) => {
                Line::new()
                    .s(b"input-testclient: win key code=")
                    .u(k.keycode as u64)
                    .s(b" down=")
                    .u(k.pressed as u64)
                    .s(b" mods=")
                    .u(k.modifiers as u64)
                    .end();
                if k.pressed == librsproto::surface::KEY_REPEAT {
                    // A held key repeating, reported apart from a press.
                    Line::new().s(b"input-testclient: win repeat code=").u(k.keycode as u64).end();
                }
                if let Some(Msg::Key(rk)) = router.key(&tree, &view, k) {
                    Line::new()
                        .s(b"input-testclient: widget key code=")
                        .u(rk.keycode as u64)
                        .s(b" down=")
                        .u(rk.pressed as u64)
                        .end();
                }
                saw_key = true;
            }
            WindowEvent::Pointer(pe) => {
                Line::new()
                    .s(b"input-testclient: win ptr kind=")
                    .u(pe.kind as u64)
                    .s(b" btn=")
                    .u(pe.button as u64)
                    .s(b" buttons=")
                    .u(pe.buttons as u64)
                    .s(b" wheel=")
                    .i(pe.wheel as i64)
                    .s(b" x=")
                    .i(pe.x as i64)
                    .s(b" y=")
                    .i(pe.y as i64)
                    .end();
                for m in router.pointer(&tree, &view, &laid, pe).0 {
                    match m {
                        Msg::Ptr(rp) => {
                            Line::new()
                                .s(b"input-testclient: widget ptr kind=")
                                .u(rp.kind as u64)
                                .s(b" x=")
                                .i(rp.x as i64)
                                .s(b" y=")
                                .i(rp.y as i64)
                                .end();
                        }
                        Msg::Wheel(rp) => {
                            Line::new().s(b"input-testclient: widget wheel dz=").i(rp.wheel as i64).end();
                        }
                        Msg::Key(_) => {}
                    }
                }
                if pe.kind == librsproto::surface::POINTER_BUTTON
                    && pe.flags & librsproto::surface::POINTER_PRESSED != 0
                {
                    saw_press = true;
                }
            }
            WindowEvent::Focus(f) => {
                router.set_window_focused(f);
                // Widget focus is the toolkit's, window focus the compositor's.
                Line::new().s(b"input-testclient: win focus has=").u(u64::from(f)).end();
            }
            WindowEvent::InputLost => kprint(b"input-testclient: win events DROPPED\n"),
            WindowEvent::Drop { .. } => kprint(b"input-testclient: a drop arrived\n"),
            // **Reported and ignored, deliberately**: a window that vanished when asked politely
            // would end the run.
            WindowEvent::CloseRequested => kprint(b"input-testclient: asked to close, staying\n"),
            WindowEvent::Dismissed | WindowEvent::State(_) => {}
            // A fixed-size window may decline a resize; this one is 2048×2048 on purpose.
            WindowEvent::Configure { .. } => {}
        }
    }

    // ---- The stall stage: stop draining, and prove nothing is lost ----
    // The phases above drain between injections, so park-and-retry never runs. So stall: the
    // harness floods motion until the 16-slot ring is full, then injects a key. On waking it must
    // still arrive; drop-on-refusal would lose it, and park-with-no-wakeup would never re-send it.
    kprint(b"input-testclient: stalling\n");
    sleep_ns(notif, STALL_NS);

    let mut saw_late_key = false;
    while !saw_late_key {
        let ev = match win.wait_event().map(|(_, e)| e) {
            Ok(e) => e,
            Err(_) => {
                kprint(b"input-testclient: window stream FAILED\n");
                exit(1);
            }
        };
        match ev {
            WindowEvent::Key(k) => {
                Line::new().s(b"input-testclient: late key code=").u(k.keycode as u64).end();
                if k.keycode == LATE_CODE {
                    saw_late_key = true;
                }
            }
            WindowEvent::Pointer(_) | WindowEvent::Focus(_) => {}
            WindowEvent::InputLost => kprint(b"input-testclient: win events DROPPED\n"),
            WindowEvent::Drop { .. } => kprint(b"input-testclient: a drop arrived\n"),
            // **Reported and ignored, deliberately**, as in the window stage.
            WindowEvent::CloseRequested => kprint(b"input-testclient: asked to close, staying\n"),
            WindowEvent::Dismissed | WindowEvent::State(_) => {}
            WindowEvent::Configure { .. } => {}
        }
    }

    kprint(b"input-testclient: PASSED\n");
    exit(0);
}

/// How long the window phase waits for the harness to start injecting before giving up.
const IDLE_LIMIT_NS: u64 = 20_000_000_000;

fn deadline_ns(ns: u64) -> u64 {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
    unsafe { (&raw const CLOCK_BUF).read() }.saturating_add(ns)
}

/// Wait for a window event until `deadline`, or `None`: `Session::wait_event` blocks forever.
fn wait_event_before(
    win: &mut Session<alloc::boxed::Box<ChannelTransport>>,
    deadline: u64,
) -> Option<Result<(u32, WindowEvent), libsurface::UiError>> {
    loop {
        match win.poll_event() {
            Ok(Some(e)) => return Some(Ok(e)),
            Err(e) => return Some(Err(e)),
            Ok(None) => {}
        }
        let h = win.wait_handle();
        if h == 0 {
            return Some(win.wait_event());
        }
        // **The window handle alone**: the notification handle is never drained, so waiting on it
        // spins.
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
        let waited = unsafe {
            WAIT_HANDLES[0] = h;
            syscall5(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline, 0)
        };
        if waited < 1 {
            return None; // the deadline passed
        }
    }
}

/// Sleep for `ns` by waiting on a handle that never signals.
fn sleep_ns(notif: u64, ns: u64) {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
    let deadline = unsafe { (&raw const CLOCK_BUF).read() }.saturating_add(ns);
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    unsafe {
        WAIT_HANDLES[0] = notif;
        syscall5(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline, 0);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"input-testclient: panic\n");
    exit(2);
}
