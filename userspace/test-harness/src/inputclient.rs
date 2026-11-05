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
use libkern::{KError, Rights};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Channel, Deadline, Handle, Msg as IpcMsg, Namespace, NsReadOnly, Only, Ref, WaitSet, block_on, time};
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
/// Wait on `channel` until `deadline`: whether it signalled.
fn ready(channel: &Handle<Channel, Only>, deadline: Deadline) -> bool {
    let mut set = WaitSet::<1>::new();
    set.add(channel).is_ok() && set.wait(deadline).is_ok_and(|s| !s.is_empty())
}

/// The consumer end of `/dev/input/new`, and the last batch received on it.
struct Stream {
    channel: Handle<Channel, Only>,
    msg: IpcMsg,
    /// The harness pressed [`START_CODE`], asking for the motion phase.
    start_requested: bool,
    /// The idle deadline passed rather than the channel failing: nobody is driving this run.
    timed_out: bool,
}

impl Stream {
    fn open(root: Ref<'_, Namespace, NsReadOnly>) -> Option<Self> {
        let channel =
            block_on(root.lookup::<Channel, Only>("/dev/input/new", Rights::SEND | Rights::RECV | Rights::WAIT))
                .ok()?;
        Some(Self { channel, msg: IpcMsg::new(), start_requested: false, timed_out: false })
    }

    /// Sum relative motion until the stream is quiet: `(dx, dy, announced, widest)`.
    ///
    /// It runs first, as a device stream cannot be delimited from inside the guest. **`widest`
    /// tells a consumer a deferral happened**: recovery is invisible, but a folded group carries
    /// the *sum* of its batches, so it is wider than any injected delta.
    fn sum_motion(&mut self) -> (i64, i64, u64, i64) {
        let (mut dx, mut dy, mut announced, mut widest) = (0i64, 0i64, 0u64, 0i64);
        loop {
            if !ready(&self.channel, deadline_after(QUIET_NS)) {
                return (dx, dy, announced, widest); // quiet for long enough
            }
            match self.channel.recv(&mut self.msg) {
                Ok(_) => {}
                Err(e) if e.code() == KError::WouldBlock => continue,
                Err(_) => return (dx, dy, announced, widest),
            }
            let Ok(m) = librsproto::decode(self.msg.payload()) else { return (dx, dy, announced, widest) };
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
        let deadline = deadline_after(IDLE_LIMIT_NS);
        loop {
            if !ready(&self.channel, deadline) {
                self.timed_out = time::now().is_ok_and(|now| now >= deadline.ns());
                return None;
            }
            match self.channel.recv(&mut self.msg) {
                Ok(_) => {}
                Err(e) if e.code() == KError::WouldBlock => continue, // woken with nothing to take
                Err(_) => return None,
            }
            let Ok(m) = librsproto::decode(self.msg.payload()) else { return None };
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

/// Entry point; the root namespace is the process's own.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, boot2: u64, arg0: u64) -> ! {
    log::write(b"input-testclient: up\n");
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(notif, root_ns, boot2, arg0) };
    let Some(root) = start.root.as_ref().map(Handle::as_ref) else {
        log::write(b"input-testclient: /dev/input/new FAILED\n");
        libos::exit(1);
    };

    let Some(mut stream) = Stream::open(root) else {
        log::write(b"input-testclient: /dev/input/new FAILED\n");
        libos::exit(1);
    };

    // The harness waits for this before injecting.
    log::write(b"input-testclient: listening\n");

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
                    log::write(b"input-testclient: input stalled\n");
                    let _ = time::sleep(INPUT_STALL_NS);
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
                log::write(b"input-testclient: idle before the window phase\n");
                libos::exit(0);
            }
            None => {
                log::write(b"input-testclient: stream FAILED\n");
                libos::exit(1);
            }
        }
    }

    // ---- The window stage: the same events through a *window*, created after the raw stage ----
    let transport = match ChannelTransport::open(root) {
        Ok(t) => t,
        Err(_) => {
            log::write(b"input-testclient: /dev/draw connect FAILED\n");
            libos::exit(1);
        }
    };
    let mut win = Session::new(alloc::boxed::Box::new(transport));
    let Ok(window_id) = win.create(&CreateWindowRequest::new(WIN_W, WIN_H, Role::Normal), 2) else {
        log::write(b"input-testclient: Session::create FAILED\n");
        libos::exit(1);
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
        log::write(b"input-testclient: toolkit diff FAILED\n");
        libos::exit(1);
    }
    let mut router = Router::new();
    match tree.root().map(|w| w.id) {
        Some(id) if router.focus(&tree, &view, id) => {}
        _ => {
            log::write(b"input-testclient: toolkit focus FAILED\n");
            libos::exit(1);
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
        let ev = match wait_event_before(&mut win, deadline_after(IDLE_LIMIT_NS)) {
            Some(Ok((_, e))) => e,
            Some(Err(_)) => {
                log::write(b"input-testclient: window stream FAILED\n");
                libos::exit(1);
            }
            // **Nothing is driving this run.** Exiting releases the 2048×2048 window, which takes
            // every click and keystroke; `check-terminal` found its click landing here.
            None => {
                // **An announcement, not a completion**: the window outlives this line, so a gate
                // clicking on seeing it can still land on it.
                log::write(b"input-testclient: releasing the window (still up)\n");
                libos::exit(0);
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
            WindowEvent::InputLost => log::write(b"input-testclient: win events DROPPED\n"),
            WindowEvent::Drop { .. } => log::write(b"input-testclient: a drop arrived\n"),
            // **Reported and ignored, deliberately**: a window that vanished when asked politely
            // would end the run.
            WindowEvent::CloseRequested => log::write(b"input-testclient: asked to close, staying\n"),
            WindowEvent::Dismissed | WindowEvent::State(_) => {}
            // A fixed-size window may decline a resize; this one is 2048×2048 on purpose.
            WindowEvent::Configure { .. } => {}
        }
    }

    // ---- The stall stage: stop draining, and prove nothing is lost ----
    // The phases above drain between injections, so park-and-retry never runs. So stall: the
    // harness floods motion until the 16-slot ring is full, then injects a key. On waking it must
    // still arrive; drop-on-refusal would lose it, and park-with-no-wakeup would never re-send it.
    log::write(b"input-testclient: stalling\n");
    let _ = time::sleep(STALL_NS);

    let mut saw_late_key = false;
    while !saw_late_key {
        let ev = match win.wait_event().map(|(_, e)| e) {
            Ok(e) => e,
            Err(_) => {
                log::write(b"input-testclient: window stream FAILED\n");
                libos::exit(1);
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
            WindowEvent::InputLost => log::write(b"input-testclient: win events DROPPED\n"),
            WindowEvent::Drop { .. } => log::write(b"input-testclient: a drop arrived\n"),
            // **Reported and ignored, deliberately**, as in the window stage.
            WindowEvent::CloseRequested => log::write(b"input-testclient: asked to close, staying\n"),
            WindowEvent::Dismissed | WindowEvent::State(_) => {}
            WindowEvent::Configure { .. } => {}
        }
    }

    log::write(b"input-testclient: PASSED\n");
    libos::exit(0);
}

/// How long the window phase waits for the harness to start injecting before giving up.
const IDLE_LIMIT_NS: u64 = 20_000_000_000;

fn deadline_after(ns: u64) -> Deadline {
    Deadline::after(ns).unwrap_or(Deadline::at(ns))
}

/// Wait for a window event until `deadline`, or `None`: `Session::wait_event` blocks forever.
fn wait_event_before(
    win: &mut Session<alloc::boxed::Box<ChannelTransport>>,
    deadline: Deadline,
) -> Option<Result<(u32, WindowEvent), libsurface::UiError>> {
    loop {
        match win.poll_event() {
            Ok(Some(e)) => return Some(Ok(e)),
            Err(e) => return Some(Err(e)),
            Ok(None) => {}
        }
        // **The window handle alone**: the notification handle is never drained, so waiting on it
        // spins.
        let mut set = WaitSet::<1>::new();
        if set.add(win.waitable()).is_err() || !set.wait(deadline).is_ok_and(|s| !s.is_empty()) {
            return None; // the deadline passed
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"input-testclient: panic\n");
    libos::exit(2);
}
