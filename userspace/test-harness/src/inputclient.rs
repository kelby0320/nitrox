//! `input-testclient` — proves the i8042 driver actually delivers events (plan M3 Part A).
//!
//! Bring-up is not the same as working. Before this, Part A could report
//! `ps2: keyboard mouse armed` on every boot with the ISR, the event ring and the parked
//! read having never run against a real keystroke — which is precisely the shape that let a
//! **one-way** Surface protocol ship in PR #174 behind three green CI jobs and 1213 passing
//! tests. A bound server that never answers, and an armed driver that never delivers, read
//! identically from the outside.
//!
//! So this consumes `/dev/input/new` — the **merged** stream from the `input-server` — and
//! prints what arrives. The host side (`cargo xtask check-input`) injects the keystrokes and
//! clicks over QMP and checks the decoded events against what it sent, which exercises the
//! whole path: i8042 → driver ring → raw node → server → merge → channel → here.
//!
//! It read the raw nodes directly until M3 Part B. It cannot any more, and that is the
//! design working: the driver is single-reader per device and the server now holds both, so
//! a second reader gets `WouldBlock`. Reading a raw node unfiltered is a keylogger, and the
//! binding is the whole of that boundary (`input-subsystem.md` §5).
//!
//! ## Why it announces itself first
//!
//! Injection is a host action against a guest that must already be waiting. The client
//! prints `listening` once its reads are parked, the harness waits for that line, and only
//! then injects — otherwise the event lands in the ring before anyone is reading, or worse,
//! before the device node has been resolved, and the test becomes a race it loses
//! intermittently.

#![no_std]
#![no_main]

extern crate alloc;

use libkern::abi::{INPUT_EVENT_LEN, InputEvent};
use libkern::debug::Line;
use libkern::{
    RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT, SYS_CHANNEL_RECV, SYS_NS_LOOKUP, SYS_WAIT, exit, kprint,
    syscall2, syscall4, syscall5,
};
use librsproto::surface::{KeyEvent, PointerEvent, Role};
use libsurface::{Window, WindowEvent, ipc::ChannelTransport};
use libui::diff::Tree;
use libui::element::{Element, custom};
use libui::layout::{FixedCell, layout};
use libui::route::Router;

/// `alloc` backing — `libsurface` holds its buffers and event queue on the heap.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The invisible window's size.
///
/// **Nothing is ever committed to it**, so the compositor skips it when compositing — a
/// window that has created buffers but not drawn shows background, which is a real state and
/// not a trick. That is what lets this client hold a focusable, hit-testable window through
/// a boot without disturbing `cargo xtask check-display`, which compares the screen against
/// `ui-testclient`'s scene pixel for pixel.
///
/// **Larger than any screen, deliberately.** New windows land at `(0, 0)`, so a window this
/// size contains the cursor wherever the compositor parked it — which means the gate can
/// click without first driving the cursor somewhere known. That matters more than it sounds:
/// steering the cursor takes a dozen PS/2 motion events, each of which becomes a
/// `PointerEvent` on this client's session ring, and the first attempt at this gate lost the
/// keystroke behind exactly that flood. The compositor clips, so an oversized window is
/// ordinary — a maximised one is the same shape.
const WIN_W: u32 = 2048;
const WIN_H: u32 = 2048;

/// How long phase 3 refuses to drain, in nanoseconds.
///
/// Long enough for the harness to see `stalling`, inject a flood over QMP, and for the
/// compositor to fill the ring and start parking — sub-second would race the host round trip.
const STALL_NS: u64 = 1_500_000_000;

/// The key the harness injects *after* the flood, and the one whose arrival proves nothing
/// was dropped while the ring was full.
const LATE_CODE: u16 = 46;

/// The one widget this client builds: a `custom` node filling the window, which is the shape
/// Milestone 5's terminal grid takes.
const GRID: u32 = 1;

/// What the toolkit hands back when an event reaches the widget.
///
/// Part B's whole claim is that an injected keystroke reaches a *widget* — not merely a
/// window — so the gate needs something the router produced rather than something the
/// window received. These are that something.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Msg {
    Key(KeyEvent),
    Ptr(PointerEvent),
}

/// Bytes per record, from the shared ABI rather than a local literal — an earlier version
/// hardcoded `16` and read fields at literal offsets, so a kernel-side layout change would
/// have gone unnoticed until this gate happened to run (PR #178 review).
const EVENT_LEN: usize = INPUT_EVENT_LEN;
/// The last thing the harness injects, and therefore what "done" means here.
///
/// **A sentinel rather than a record count.** Counting was the first attempt and it was both
/// wrong (nine records, not ten — the motion is `REL_X`, `REL_Y`, `SYN`) and brittle for a
/// worse reason: how many records an injection produces is the driver's business, so a count
/// would need updating whenever the mouse packet framing changed. Waiting for the event that
/// *means* the sequence finished does not.
const DONE_CODE: u16 = libkern::abi::BTN_LEFT;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;

static mut CLOCK_BUF: u64 = 0;
static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// Wait for a `PendingOperation` and return `(status, result)`.
fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    if waited != 1 {
        return (-1, 0);
    }
    // SAFETY: the kernel filled one 24-byte result record.
    unsafe {
        let st = i32::from_le_bytes([
            WAIT_RESULTS[8],
            WAIT_RESULTS[9],
            WAIT_RESULTS[10],
            WAIT_RESULTS[11],
        ]);
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

/// Resolve `path`, blocking on the lookup's `PendingOperation`.
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
    handles: [u64; 8],
    count: u64,
}

impl Stream {
    /// Resolve `/dev/input/new`, which mints a per-consumer channel.
    fn open(root_ns: u64) -> Option<Self> {
        let channel = lookup(root_ns, b"/dev/input/new", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT)?;
        Some(Self { channel, msg: [0; 4096], handles: [0; 8], count: 0 })
    }

    /// Block for one `Input::Events` message and print every record it carries.
    ///
    /// Returns whether the batch contained [`DONE_CODE`]'s press, or `None` if the channel
    /// failed.
    fn pump(&mut self) -> Option<bool> {
        loop {
            // SAFETY: waiting on this process's own channel handle.
            let waited = unsafe {
                WAIT_HANDLES[0] = self.channel;
                syscall4(
                    SYS_WAIT,
                    (&raw const WAIT_HANDLES) as u64,
                    1,
                    (&raw mut WAIT_RESULTS) as u64,
                    u64::MAX,
                )
            };
            if waited != 1 {
                return None;
            }
            // SAFETY: valid recv out-params on a live endpoint.
            let rr = unsafe {
                syscall4(
                    SYS_CHANNEL_RECV,
                    self.channel,
                    (&raw mut self.msg) as u64,
                    (&raw mut self.handles) as u64,
                    (&raw mut self.count) as u64,
                )
            };
            if rr == libkern::error::KError::WouldBlock.as_i32() as i64 {
                continue; // woken with nothing to take
            }
            if rr != 0 {
                return None;
            }
            let payload_len =
                u32::from_le_bytes([self.msg[4], self.msg[5], self.msg[6], self.msg[7]]) as usize;
            let req = &self.msg[PAYLOAD_OFF..PAYLOAD_OFF + payload_len.min(4096 - PAYLOAD_OFF)];
            let Ok(m) = librsproto::decode(req) else { return None };
            if m.op != librsproto::OP_INPUT_EVENTS {
                continue;
            }
            let n = m.body.len() / EVENT_LEN;
            let mut done = false;
            for i in 0..n {
                let Some(ev) = InputEvent::read(&m.body[i * EVENT_LEN..]) else { return None };
                if ev.kind == libkern::abi::EV_KEY && ev.code == DONE_CODE && ev.value == 1 {
                    done = true;
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

/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments; `root_ns` is this
/// process's root namespace.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"input-testclient: up\n");

    let Some(mut stream) = Stream::open(root_ns) else {
        kprint(b"input-testclient: /dev/input/new FAILED\n");
        exit(1);
    };

    // The harness waits for this before injecting: an event delivered before the consumer
    // channel exists is one the server has nowhere to send.
    kprint(b"input-testclient: listening\n");

    // Pump until the button press arrives — the last injection. Message boundaries are
    // timing, not protocol: the server batches whatever is ready, so how the nine records
    // split across messages varies run to run and neither the client nor the harness should
    // depend on it.
    loop {
        match stream.pump() {
            Some(true) => break,
            Some(false) => {}
            None => {
                kprint(b"input-testclient: stream FAILED\n");
                exit(1);
            }
        }
    }

    // ---- Phase 2: the same events, but through a *window* ----
    //
    // The window is created **after** phase 1 rather than up front. With no window, the
    // compositor has nothing to route phase 1's events to and sends nothing; created early,
    // its records would queue on this client's session channel while it was blocked reading
    // the raw stream. That mattered when a refused send was simply dropped; since M3 Part D3
    // the compositor parks and retries, so the phases are sequenced now for clarity rather
    // than for safety — and phase 3 below exercises the parking deliberately.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => t,
        Err(_) => {
            kprint(b"input-testclient: /dev/draw connect FAILED\n");
            exit(1);
        }
    };
    let Ok(mut win) = Window::new(
        alloc::boxed::Box::new(transport),
        WIN_W,
        WIN_H,
        Role::Normal,
        2,
    ) else {
        kprint(b"input-testclient: Window::new FAILED\n");
        exit(1);
    };

    // ---- The toolkit, driven by real events ----
    //
    // `element -> layout -> diff -> route -> handler`, the whole of Part B, with events that
    // came from a QMP injection through the i8042 driver, the input server, the compositor's
    // own router and `libsurface`. Everything below this line is unit-tested in `libui`; what
    // the gate adds is that the pieces are wired to each other.
    let view: Element<Msg> = custom(GRID, libdraw::geom::Size::new(WIN_W, WIN_H))
        .on_key(|k| Some(Msg::Key(k)))
        .on_pointer(Msg::Ptr)
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
    // Focus the grid, as a real client would on creation: it is the only focusable widget.
    let grid_id = match tree.root().map(|w| w.id) {
        Some(id) if router.focus(&tree, &view, id) => id,
        _ => {
            kprint(b"input-testclient: toolkit focus FAILED\n");
            exit(1);
        }
    };
    let _ = grid_id;

    // The second synchronisation point. The window has to exist before the harness injects
    // at it, for the same reason `listening` exists: a keystroke routed before there is a
    // focusable window is one the compositor correctly drops.
    Line::new().s(b"input-testclient: window ready id=").u(win.id() as u64).end();

    // **A button *press* is the sentinel**, for the same reason `DONE_CODE` is one for the
    // raw stream: it is the last thing the harness injects. Accepting any button record
    // ended the phase on the release left over from phase 1 — which arrives before anything
    // aimed at this window — so the client printed `PASSED` and exited while the harness was
    // still asserting, and the compositor correctly routed the remaining keys to whatever
    // window was left. Diagnosed from `compositor: key win=1` after `window ready id=130`.
    // **The first event this window receives must be its focus change**, and asserting the
    // *order* is what catches a compositor that announces focus late. Announced on the create
    // itself, it precedes any input the window could be routed. Announced from somewhere that
    // only runs later — an `Applied` on some other session, or the next input event — a
    // pointer record arrives first, and the window spends that interval owning the keyboard
    // without knowing it. The gate could not tell the two apart until this line existed
    // (PR #184 review, finding 2).
    let mut first_event_reported = false;

    let (mut saw_key, mut saw_press) = (false, false);
    while !(saw_key && saw_press) {
        let ev = match win.wait_event() {
            Ok(e) => e,
            Err(_) => {
                kprint(b"input-testclient: window stream FAILED\n");
                exit(1);
            }
        };
        if !first_event_reported {
            first_event_reported = true;
            let what: &[u8] = match ev {
                WindowEvent::Focus(_) => b"focus",
                WindowEvent::Key(_) => b"key",
                WindowEvent::Pointer(_) => b"pointer",
                WindowEvent::Dropped => b"dropped",
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
                // ...and through the toolkit, which is the part Part B is about.
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
                    .s(b" x=")
                    .i(pe.x as i64)
                    .s(b" y=")
                    .i(pe.y as i64)
                    .end();
                for m in router.pointer(&tree, &view, &laid, pe).0 {
                    if let Msg::Ptr(rp) = m {
                        Line::new()
                            .s(b"input-testclient: widget ptr kind=")
                            .u(rp.kind as u64)
                            .s(b" x=")
                            .i(rp.x as i64)
                            .s(b" y=")
                            .i(rp.y as i64)
                            .end();
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
                // The *other* half of the two-focus rule: widget focus is the toolkit's,
                // window focus is the compositor's, and a client needs both to know whether
                // a caret should blink. Announced on the change, so it arrives once when
                // this window becomes the topmost focusable one.
                Line::new().s(b"input-testclient: win focus has=").u(u64::from(f)).end();
            }
            WindowEvent::Dropped => kprint(b"input-testclient: win events DROPPED\n"),
        }
    }

    // ---- Phase 3: stop draining, and prove nothing is lost ----
    //
    // The phases above never exercise the compositor's park-and-retry: this client drains in
    // `wait_event` between injections, so a send is never refused and the outbox never holds
    // anything (PR #181 review, finding 3 — demonstrated by replacing retry with the old
    // drop-on-refusal and watching the gate pass anyway).
    //
    // So stall deliberately. While this sleeps, the harness floods motion until the 16-slot
    // ring is full and then injects a key. Every motion after the ring fills parks in the
    // compositor's outbox and coalesces to one; the key queues behind it. On waking, the
    // key must still arrive — under drop-on-refusal it would not, and under park-with-no-
    // wakeup (the same review's finding 1) it would never be re-sent, because a client
    // draining its own ring signals nothing to the compositor.
    kprint(b"input-testclient: stalling\n");
    sleep_ns(notif, STALL_NS);

    let mut saw_late_key = false;
    while !saw_late_key {
        let ev = match win.wait_event() {
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
            WindowEvent::Dropped => kprint(b"input-testclient: win events DROPPED\n"),
        }
    }

    kprint(b"input-testclient: PASSED\n");
    exit(0);
}

/// Sleep for `ns` by waiting on a handle that never signals.
///
/// No timer handle needed: `sys_wait` takes an absolute monotonic deadline and returns
/// `TimedOut`, and this process's notification handle is signalled by nothing it does.
fn sleep_ns(notif: u64, ns: u64) {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: the kernel wrote the ns count.
    let deadline = unsafe { (&raw const CLOCK_BUF).read() }.saturating_add(ns);
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    unsafe {
        WAIT_HANDLES[0] = notif;
        syscall5(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            deadline,
            0,
        );
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"input-testclient: panic\n");
    exit(2);
}
