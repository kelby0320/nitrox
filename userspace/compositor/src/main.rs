//! `compositor` — the resource server behind `/dev/draw`.
//!
//! Everything that decides is in the library half ([`compositor`]), host-tested; this file is the
//! IPC loop, the mapping of clients' `MemoryObject`s, and the framebuffer.
//!
//! 1. Acquire `/dev/framebuffer`: authority is the binding (`display-substrate.md` §3).
//! 2. Send `Meta::Ready` on the control channel, transferring the end the supervisor binds.
//! 3. Serve. A resolve of `new` mints a session channel; a request's identity is its endpoint.
//!
//! `AttachBuffer` maps a `MemoryObject` once, kept for the buffer's life (`display-substrate.md`
//! §4).

#![no_std]
#![no_main]

extern crate alloc;

use compositor::Screen;
use compositor::input::InputRouter;
use compositor::manager::{self, MgrOutcome};
use compositor::outbox::{MgrEvent, MgrOutbox, Outbound, Outbox};
use compositor::server::{
    Attached, Connection, Outcome, SurfaceError, attach_memory, disconnect, dispatch, forget_window, holds,
    record_attach, refuse_attach,
};
use compositor::{BufferSource, WindowStack};
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::Relaxed;
use libdraw::format::Rgb;
use libdraw::framebuffer::RawFramebuffer;
use libdraw::geom::{Point, Rect};
use libdraw::pixels::Pixels;
use libinput::Interpreter;
use libkern::abi::{INPUT_EVENT_LEN, InputEvent, MAX_WAIT_HANDLES};
use libkern::error::KError;
use libkern::{CTRL_OP_SHUTDOWN, Rights};
use libos::boot::Start;
use libos::control::{self, Control};
use libos::log::{self, Line};
use libos::{
    Carried, Channel, Deadline, Handle, MapRead, MapReadWrite, Mapping, Memory, Msg, Namespace, NsReadOnly, Only, Ref,
    WaitSet, block_on, time,
};
use librsproto::surface::{
    ConfigureEvent, FocusEvent, KeyEvent, MgrCapture, OP_ATTACH_BUFFER, OP_CONFIGURE, OP_FOCUS_EVENT, OP_KEY_EVENT,
    OP_MGR_CAPTURE, OP_POINTER_EVENT, OP_RELEASE, PointerEvent,
};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_REPLY, decode, server};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The background the screen is cleared to; the display gate's reference render fills with the same
/// `scene::BACKGROUND`, so the two must be one constant.
const BACKGROUND: Rgb = libdraw::scene::BACKGROUND;

/// Largest Surface request body: whatever one message carries, so no op's is ever cut.
const MAX_BODY: usize = librsproto::RS_MAX_BODY;

/// Four less than the wait limit: the forwarding, input, manager and control channels.
const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 4;

const _: () = assert!(4 + MAX_SESSIONS <= MAX_WAIT_HANDLES);

/// How many client-driven rejections get logged **per session** before the tap closes.
const MAX_LOGGED_REJECTIONS: u32 = 8;

/// How many routed input events get logged before the tap closes; serial is slow and input
/// continuous.
const MAX_LOGGED_ROUTES: u32 = 8;

/// How many edge-triggered input diagnostics (a press and where it landed, a dropped batch) get
/// logged. Generous, since any gate needs a handful, but not absent: serial is slow.
const MAX_LOGGED_INPUT_DIAGS: u32 = 256;

static ROUTES_LOGGED: AtomicU32 = AtomicU32::new(0);
static INPUT_DIAGS_LOGGED: AtomicU32 = AtomicU32::new(0);

/// How long to sleep before retrying a parked message, in nanoseconds.
///
/// A parked message has no wakeup of its own (a draining client signals nothing), so the wait is
/// bounded while any is parked. Ten ms is the scheduler tick.
const RETRY_INTERVAL_NS: u64 = 10_000_000;

/// How long a new window waits for the manager to place it before it is shown anyway: a slow shell
/// must delay a window, never lose it. 200ms outlasts an idle round trip.
const CONFIGURE_DEADLINE_NS: u64 = 200_000_000;

const MAX_LOGGED_OVERFLOWS: u32 = 8;

type Endpoint = Handle<Channel, Only>;

/// A channel that may be absent, so a send to it is a refusal rather than a panic.
type Peer<'a> = Option<Ref<'a, Channel, Only>>;

struct Server {
    stack: WindowStack,
    conns: [Connection; MAX_SESSIONS],
    /// One per session slot, `None` while the slot is free.
    sessions: [Option<Endpoint>; MAX_SESSIONS],
    /// The manager's channel. One, not a table: two managers placing windows race with no
    /// arbiter, so a second resolve is refused.
    manager: Option<Endpoint>,
    buffers: alloc::vec::Vec<Attached<Mapping<MapRead>>>,
    /// Device triples → logical events. Reset by a `SYN_DROPPED` the server forwards.
    interp: Interpreter,
    router: InputRouter,
    /// The consumer channel from `/dev/input/new`, or `None` if input is unavailable.
    input: Option<Endpoint>,
    repeat: Option<compositor::Repeat>,
    /// `(window, deadline_ns)` of windows holding their first `Configure`; none may be dropped
    /// without one sent: see [`release_configure`].
    pending_configure: alloc::vec::Vec<(u32, u64)>,
    screen: Rect,
    last_layout: Option<librsproto::surface::MgrLayout>,
    /// The gesture outline's rectangle, or `None`. Held here, not asked of the router, because
    /// `repaint_region` takes the server immutably.
    outline: Option<Rect>,
    announced_focus: Option<u32>,
    /// Pending messages per session, on the heap: thirty inline queues would overrun the 32 KiB
    /// stack.
    outbox: alloc::vec::Vec<Outbox>,
    mgr_outbox: MgrOutbox,
    lights_sent: Option<u8>,
}

impl Server {
    fn session_of(&self, window: u32) -> Option<usize> {
        (0..MAX_SESSIONS).find(|&i| self.conns[i].owns(window))
    }

    fn session(&self, slot: usize) -> Peer<'_> {
        self.sessions[slot].as_ref().map(Handle::as_ref)
    }

    fn manager(&self) -> Peer<'_> {
        self.manager.as_ref().map(Handle::as_ref)
    }

    /// The mapping of `buffer` on `window`, or `None` if it is not mapped here.
    fn buffer(&self, window: u32, buffer: u32) -> Option<&Mapping<MapRead>> {
        Some(&self.buffers.iter().find(|b| b.window == window && b.buffer == buffer)?.memory)
    }
}

impl BufferSource for Server {
    fn pixels(&self, window: u32, buffer: u32) -> Option<Pixels<'_>> {
        Some(Pixels::Shared(self.buffer(window, buffer)?))
    }
}

/// Receive-ring depth for a client **session** channel: the kernel's `IPC_DEFAULT_QUEUE_DEPTH`. A
/// stalled client's motion coalesces only after this many queue; 128 KiB of kernel memory per
/// session.
const SESSION_QUEUE_DEPTH: u32 = 16;

fn now_ns() -> u64 {
    time::now().unwrap_or(0)
}

fn reply_resolve_error(serve_end: Ref<'_, Channel, Only>, request_id: u64, err: KError) -> bool {
    server::reply_error(serve_end, OP_NS_RESOLVE, request_id, err, b"")
}

fn reply_on_session(session: Peer<'_>, op: u16, request_id: u64, body: &[u8]) -> bool {
    session.is_some_and(|ch| server::send(ch, op, request_id, RS_FLAG_REPLY, body, &mut Carried::new()).is_ok())
}

fn reply_error_on_session(session: Peer<'_>, op: u16, request_id: u64, err: KError) -> bool {
    session.is_some_and(|ch| server::reply_error(ch, op, request_id, err, b""))
}

/// Send one server-initiated record, with no request id. `false` if the channel would not take it:
/// the caller leaves the message at the head of its outbox and retries.
fn send_input(session: Peer<'_>, op: u16, body: &[u8]) -> bool {
    reply_on_session(session, op, 0, body)
}

/// Tell the input server the lights the locks call for, if not what it was last told.
///
/// `lights_sent` moves only on a send, so a full channel delays the lights rather than losing them.
fn send_lights(srv: &mut Server) {
    let lights = libinput::lights(srv.interp.locks());
    let Some(input) = &srv.input else { return };
    if srv.lights_sent == Some(lights) {
        return;
    }
    if server::send(input, librsproto::OP_INPUT_LIGHTS, 0, 0, &[lights], &mut Carried::new()).is_ok() {
        srv.lights_sent = Some(lights);
    }
}

fn log_route(rec: &Outbound) {
    // A drop or a dismissal bypasses the cap: one record per gesture, long after it is spent, and
    // the only evidence the compositor acted (`check-login` waits on `drop win=`, `dismissed
    // win=`).
    if !matches!(rec, Outbound::Dropped { .. } | Outbound::Dismissed { .. }) {
        let n = ROUTES_LOGGED.load(Relaxed);
        if n > MAX_LOGGED_ROUTES {
            return;
        }
        ROUTES_LOGGED.store(n + 1, Relaxed);
        if n == MAX_LOGGED_ROUTES {
            log::write(b"compositor: (further routed input not logged)\n");
            return;
        }
    }
    let mut l = Line::new();
    match rec {
        Outbound::Key { event } => {
            l.s(b"compositor: key win=").u(event.window as u64);
            l.s(b" code=").u(event.keycode as u64);
            l.s(b" down=").u(event.pressed as u64);
        }
        Outbound::Pointer { event } => {
            l.s(b"compositor: ptr win=").u(event.window as u64);
            l.s(b" kind=").u(event.kind as u64);
            l.s(b" x=").i(event.x as i64).s(b" y=").i(event.y as i64);
        }
        Outbound::Release { window, buffer } => {
            l.s(b"compositor: rel win=").u(*window as u64);
            l.s(b" buf=").u(*buffer as u64);
        }
        Outbound::Focus { window, focused } => {
            l.s(b"compositor: focus win=").u(*window as u64);
            l.s(b" has=").u(*focused as u64);
        }
        Outbound::CloseRequested { window } => {
            l.s(b"compositor: close-requested win=").u(*window as u64);
        }
        Outbound::WindowState { window, state } => {
            l.s(b"compositor: state win=").u(*window as u64).s(b" state=").u(*state as u64);
        }
        Outbound::Dismissed { window } => {
            l.s(b"compositor: dismissed win=").u(*window as u64);
        }
        Outbound::Dropped { window, acceptor, kind, name, x, y, .. } => {
            // The name, never the path: a console line naming a file in somebody's home is a log of
            // what they were doing.
            l.s(b"compositor: drop win=").u(*window as u64);
            l.s(b" on=").untrusted(acceptor.as_bytes());
            l.s(b" kind=").u(*kind as u64);
            l.s(b" name=").untrusted(name.as_bytes());
            l.s(b" at ").i(*x as i64).s(b",").i(*y as i64);
        }
        Outbound::Configure { window, width, height, x, y } => {
            l.s(b"compositor: mgr-configure win=").u(*window as u64);
            l.s(b" ").u(*width as u64).s(b"x").u(*height as u64);
            l.s(b" at ").i(*x as i64).s(b",").i(*y as i64);
        }
    }
    l.end();
}

/// Take an outline change and put it on screen: repaint the strips of where it was and is, not
/// their union, which would starve input.
fn apply_outline(srv: &mut Server, screen: &mut Screen<RawFramebuffer>, outline: Option<compositor::input::Outline>) {
    let Some(o) = outline else { return };
    srv.outline = o.now;
    for r in o.was.into_iter().chain(o.now).flat_map(compositor::outline_edges) {
        if !r.is_empty() {
            repaint_region(srv, screen, r);
        }
    }
}

fn deliver(srv: &mut Server, out: &[Outbound]) {
    for rec in out {
        let Some(slot) = srv.session_of(rec.window()) else {
            continue;
        };
        log_route(rec);
        enqueue(srv, slot, rec.clone());
    }
}

fn fire_repeat(srv: &mut Server) {
    let now = now_ns();
    let Some(mut r) = srv.repeat else { return };
    // Checked here, not at each focus-moving site, so a new way to move focus cannot forget: a
    // held key must not keep typing into the window that lost focus. No gate covers it
    // (`check-input` never moves focus while a key is held).
    if srv.stack.focus_candidate() != Some(r.window) {
        srv.repeat = None;
        return;
    }
    if !r.due(now) {
        return;
    }
    srv.repeat = Some(r);
    let Some(slot) = srv.session_of(r.window) else {
        srv.repeat = None;
        return;
    };
    let rec = Outbound::Key { event: r.event() };
    log_route(&rec);
    enqueue(srv, slot, rec);
}

/// Tell the affected windows if focus moved since the last announcement.
fn announce_focus(srv: &mut Server) {
    let now = srv.stack.focus_candidate();
    let Some((was, now)) = compositor::focus_transition(srv.announced_focus, now) else {
        return;
    };
    srv.announced_focus = now;
    // The manager hears every transition, including a window whose client has gone.
    if let Some(old) = was {
        mgr_emit(srv, MgrEvent::Focus { window: old, focused: false });
    }
    if let Some(new) = now {
        mgr_emit(srv, MgrEvent::Focus { window: new, focused: true });
    }
    if let Some(old) = was
        && let Some(slot) = srv.session_of(old)
    {
        let rec = Outbound::Focus { window: old, focused: false };
        log_route(&rec);
        enqueue(srv, slot, rec);
    }
    if let Some(new) = now
        && let Some(slot) = srv.session_of(new)
    {
        let rec = Outbound::Focus { window: new, focused: true };
        log_route(&rec);
        enqueue(srv, slot, rec);
    }
}

fn enqueue(srv: &mut Server, slot: usize, rec: Outbound) {
    if srv.outbox[slot].push(rec) {
        let n = srv.outbox[slot].dropped();
        if n <= MAX_LOGGED_OVERFLOWS {
            let mut l = Line::new();
            l.s(b"compositor: session ").u(slot as u64).s(b" outbox overflow, discarded ").u(n as u64);
            if n == MAX_LOGGED_OVERFLOWS {
                l.s(b" (further discards not logged)");
            }
            l.end();
        }
    }
}

/// Push as much of each session's queue down its channel as it will take.
///
/// Returns `true` if anything is parked, giving the next `sys_wait` a [`RETRY_INTERVAL_NS`]
/// deadline (there is no writability signal). Stops at the first refusal: skipping would reorder a
/// client's stream.
fn flush_outboxes(srv: &mut Server) -> bool {
    for (ch, outbox) in srv.sessions.iter().zip(&mut srv.outbox) {
        let Some(ch) = ch else {
            outbox.clear();
            continue;
        };
        while let Some(rec) = outbox.front() {
            if !send_outbound(ch.as_ref(), rec) {
                break;
            }
            outbox.pop();
        }
    }
    // Drained as the clients' are: a manager that lost a `created` would never place that window.
    match &srv.manager {
        None => srv.mgr_outbox.clear(),
        Some(mgr) => {
            while let Some(ev) = srv.mgr_outbox.front() {
                if !send_mgr_event(mgr.as_ref(), ev) {
                    break;
                }
                srv.mgr_outbox.pop();
            }
        }
    }
    (0..MAX_SESSIONS).any(|i| !srv.outbox[i].is_empty()) || !srv.mgr_outbox.is_empty()
}

/// Announce everything the stack recorded since the last call, after every dispatch, as the op
/// cannot name what changed.
fn drain_stack_events(srv: &mut Server) {
    announce_layout(srv);
    for window in srv.stack.take_geometry_changes() {
        let Some(w) = srv.stack.window(window) else { continue };
        let b = w.bounds();
        let ev = ConfigureEvent { window, width: b.size.w, height: b.size.h, x: b.origin.x, y: b.origin.y };
        mgr_emit(srv, MgrEvent::Geometry(ev));
    }
    for window in srv.stack.take_removed() {
        forget_window(&mut srv.conns, &mut srv.buffers, window);
        mgr_emit(srv, MgrEvent::Destroyed { window });
    }
}

/// Send `window`'s held first `Configure` and make it compositable. `true` if this was the one.
///
/// The client is blocked on it: every path removing a window from [`Server::pending_configure`]
/// must come through here. Geometry is read now, so a manager's placement is honoured.
fn release_configure(srv: &mut Server, screen: &mut Screen<RawFramebuffer>, window: u32) -> bool {
    let Some(i) = srv.pending_configure.iter().position(|&(w, _)| w == window) else {
        return false;
    };
    srv.pending_configure.remove(i);
    let Some(w) = srv.stack.window(window) else {
        return false;
    };
    let (width, height) = w.size;
    let origin = w.origin;
    let bounds = w.painted_bounds();
    srv.stack.mark_configured(window);
    let sent = configure_window(srv, window, width, height, origin);
    // Both belong here, as releasing changes visibility on four paths. Only the window's rectangle
    // is repainted: this runs on every creation while a manager is attached.
    repaint_region(srv, screen, bounds);
    announce_focus(srv);
    sent
}

fn fire_configure_deadlines(srv: &mut Server, screen: &mut Screen<RawFramebuffer>) -> bool {
    // Free when nothing is held, as this runs every pass: no clock read in the hot path.
    if srv.pending_configure.is_empty() {
        return false;
    }
    let now = now_ns();
    let due: alloc::vec::Vec<u32> =
        srv.pending_configure.iter().filter(|&&(_, at)| at <= now).map(|&(w, _)| w).collect();
    let fired = !due.is_empty();
    for window in due {
        let mut l = Line::new();
        // `check-display` waits on this line, and `check-login` fails if it appears.
        l.s(b"compositor: no manager answer for window ").u(window as u64).s(b"; showing it");
        l.end();
        release_configure(srv, screen, window);
    }
    fired
}

fn mgr_emit(srv: &mut Server, ev: MgrEvent) {
    if srv.manager.is_none() {
        return;
    }
    if srv.mgr_outbox.push(ev) {
        let n = srv.mgr_outbox.dropped();
        if n <= MAX_LOGGED_OVERFLOWS {
            let mut l = Line::new();
            l.s(b"compositor: manager outbox overflow, discarded ").u(n as u64);
            if n == MAX_LOGGED_OVERFLOWS {
                l.s(b" (further discards not logged)");
            }
            l.end();
        }
    }
}

/// A manager event that would not serialise: log it and let the queue move on. Logged, unlike the
/// client-facing equivalent, since losing one leaves a manager's window list wrong forever.
fn unserialisable(what: &[u8]) -> bool {
    let mut l = Line::new();
    l.s(b"compositor: manager ").s(what).s(b" would not serialise; dropped");
    l.end();
    // `true` lets the queue move on; retrying would wedge every event behind it.
    true
}

fn send_mgr_event(ch: Ref<'_, Channel, Only>, ev: &MgrEvent) -> bool {
    let ch = Some(ch);
    use librsproto::surface::{
        MgrHotkey, MgrWindowCreated, MgrWindowRef, OP_MGR_HOTKEY, OP_MGR_WINDOW_CREATED, OP_MGR_WINDOW_DESTROYED,
        OP_MGR_WINDOW_FOCUS, OP_MGR_WINDOW_GEOMETRY, OP_MGR_WINDOW_TITLE,
    };
    match ev {
        MgrEvent::Hotkey(hk) => {
            let mut body = [0u8; core::mem::size_of::<MgrHotkey>()];
            match hk.write(&mut body) {
                Some(n) => send_input(ch, OP_MGR_HOTKEY, &body[..n]),
                None => unserialisable(b"Hotkey"),
            }
        }
        MgrEvent::Created(c) => {
            let mut body = [0u8; core::mem::size_of::<MgrWindowCreated>()];
            match c.write(&mut body) {
                Some(n) => send_input(ch, OP_MGR_WINDOW_CREATED, &body[..n]),
                None => unserialisable(b"WindowCreated"),
            }
        }
        MgrEvent::Destroyed { window } => {
            let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
            let r = MgrWindowRef { window: *window, other: 0 };
            match r.write(&mut body) {
                Some(n) => send_input(ch, OP_MGR_WINDOW_DESTROYED, &body[..n]),
                None => unserialisable(b"WindowDestroyed"),
            }
        }
        MgrEvent::Geometry(g) => {
            let mut body = [0u8; core::mem::size_of::<ConfigureEvent>()];
            match g.write(&mut body) {
                Some(n) => send_input(ch, OP_MGR_WINDOW_GEOMETRY, &body[..n]),
                None => unserialisable(b"WindowGeometry"),
            }
        }
        MgrEvent::LayoutChanged(l) => {
            let mut body = [0u8; core::mem::size_of::<librsproto::surface::MgrLayout>()];
            match l.write(&mut body) {
                Some(n) => send_input(ch, librsproto::surface::OP_MGR_LAYOUT_CHANGED, &body[..n]),
                None => unserialisable(b"LayoutChanged"),
            }
        }
        MgrEvent::StateRequest(s) => {
            let mut body = [0u8; core::mem::size_of::<librsproto::surface::WindowState>()];
            match s.write(&mut body) {
                Some(n) => send_input(ch, librsproto::surface::OP_MGR_WINDOW_STATE_REQUEST, &body[..n]),
                None => unserialisable(b"WindowStateRequest"),
            }
        }
        MgrEvent::DragEnded(g) => {
            let mut body = [0u8; core::mem::size_of::<librsproto::surface::MgrDragEnded>()];
            match g.write(&mut body) {
                Some(n) => send_input(ch, librsproto::surface::OP_MGR_DRAG_ENDED, &body[..n]),
                None => unserialisable(b"DragEnded"),
            }
        }
        MgrEvent::Title { window, title } => {
            let mut body = [0u8; 4 + librsproto::surface::MAX_TITLE];
            match librsproto::surface::title::write(*window, title, &mut body) {
                Some(n) => send_input(ch, OP_MGR_WINDOW_TITLE, &body[..n]),
                None => unserialisable(b"WindowTitle"),
            }
        }
        MgrEvent::Focus { window, focused } => {
            let mut body = [0u8; core::mem::size_of::<FocusEvent>()];
            let e = FocusEvent { focused: u16::from(*focused), _pad: 0, window: *window };
            match e.write(&mut body) {
                Some(n) => send_input(ch, OP_MGR_WINDOW_FOCUS, &body[..n]),
                None => unserialisable(b"WindowFocus"),
            }
        }
    }
}

fn send_outbound(ch: Ref<'_, Channel, Only>, rec: &Outbound) -> bool {
    let ch = Some(ch);
    // Sized from the types, not the spec's byte counts: `write` refuses a short buffer, which would
    // drop every event of that kind.
    match rec {
        Outbound::Key { event, .. } => {
            let mut body = [0u8; core::mem::size_of::<KeyEvent>()];
            match event.write(&mut body) {
                Some(_) => send_input(ch, OP_KEY_EVENT, &body),
                None => true,
            }
        }
        Outbound::Pointer { event, .. } => {
            let mut body = [0u8; core::mem::size_of::<PointerEvent>()];
            match event.write(&mut body) {
                Some(_) => send_input(ch, OP_POINTER_EVENT, &body),
                None => true,
            }
        }
        Outbound::Focus { window, focused } => {
            let mut body = [0u8; core::mem::size_of::<FocusEvent>()];
            let e = FocusEvent { focused: u16::from(*focused), _pad: 0, window: *window };
            match e.write(&mut body) {
                Some(_) => send_input(ch, OP_FOCUS_EVENT, &body),
                None => true,
            }
        }
        Outbound::Release { window, buffer } => {
            let mut body = [0u8; librsproto::surface::RELEASE_EVENT_LEN];
            match librsproto::surface::build_release_event(&mut body, *window, *buffer) {
                Some(n) => reply_on_session(ch, OP_RELEASE, 0, &body[..n]),
                None => true,
            }
        }
        Outbound::CloseRequested { window } => {
            let mut body = [0u8; 4];
            match (librsproto::surface::WindowRef { window: *window }).write(&mut body) {
                Some(n) => send_input(ch, librsproto::surface::OP_CLOSE_REQUESTED, &body[..n]),
                None => true,
            }
        }
        Outbound::WindowState { window, state } => {
            let mut body = [0u8; 8];
            match (librsproto::surface::WindowState { window: *window, state: *state }).write(&mut body) {
                Some(n) => send_input(ch, librsproto::surface::OP_WINDOW_STATE, &body[..n]),
                None => true,
            }
        }
        Outbound::Dismissed { window } => {
            let mut body = [0u8; 4];
            match (librsproto::surface::WindowRef { window: *window }).write(&mut body) {
                Some(n) => send_input(ch, librsproto::surface::OP_DISMISSED, &body[..n]),
                None => true,
            }
        }
        Outbound::Dropped { window, acceptor, kind, path, name, x, y } => {
            use librsproto::surface::{DroppedEvent, MAX_EVENT_BODY, OP_DROPPED};
            // The constant the client sizes its receive buffer from, so a sender cannot emit more
            // than a receiver takes.
            let mut body = [0u8; MAX_EVENT_BODY];
            let ev = DroppedEvent { window: *window, kind: *kind, x: *x, y: *y, acceptor_len: 0, path_len: 0 };
            match ev.write(&mut body, acceptor.as_bytes(), path.as_bytes(), name.as_bytes()) {
                Some(n) => send_input(ch, OP_DROPPED, &body[..n]),
                // Unreachable with the caps enforced at both edges; reported, as a vanished drop is
                // a gesture the system forgot.
                None => {
                    log::write(b"compositor: a drop would not serialise\n");
                    true
                }
            }
        }
        Outbound::Configure { window, width, height, x, y } => {
            let mut body = [0u8; 20];
            let ev = ConfigureEvent { window: *window, width: *width, height: *height, x: *x, y: *y };
            match ev.write(&mut body) {
                Some(_) => send_input(ch, OP_CONFIGURE, &body),
                None => true,
            }
        }
    }
}

/// Resolve `/dev/input/new` and return the consumer channel, or `None`. Not fatal: a machine with
/// no i8042 has no input server, and the display is still worth having.
fn connect_input(root: &Handle<Namespace, NsReadOnly>) -> Option<Endpoint> {
    // The input server mints one consumer per resolve, exactly as `/dev/draw/new` mints one session.
    block_on(root.lookup::<Channel, Only>("/dev/input/new", Rights::RECV | Rights::SEND | Rights::WAIT)).ok()
}

/// Serve every request already queued on the manager's channel and every client's, between
/// receiving an input pass and routing it, so each batch is routed against every request sent
/// before it arrived.
///
/// Otherwise a key and a menu's destroy ready together were served input first: the key went to a
/// menu the client had let go and `libsurface` dropped it (`check-login`, `nxfiles: name so far 1
/// chars`). Routing nothing until the drain also keeps the cursor's bookkeeping honest: a repaint
/// mid-pass would draw the cursor where a batch moved it, with nothing to erase it.
///
/// One pass per channel, bounded by `SESSION_QUEUE_DEPTH`, so a sending client cannot hold input
/// off. The manager goes first: it arranges the stack.
fn serve_queued_requests(srv: &mut Server, screen: &mut Screen<RawFramebuffer>) {
    for _ in 0..SESSION_QUEUE_DEPTH {
        if srv.manager.is_none() {
            break;
        }
        match serve_manager(srv, screen) {
            Served::One => {}
            Served::Empty => break,
            Served::Closed => {
                close_manager(srv, screen);
                break;
            }
        }
    }
    for slot in 0..MAX_SESSIONS {
        for _ in 0..SESSION_QUEUE_DEPTH {
            if srv.sessions[slot].is_none() {
                break;
            }
            match serve_session(slot, srv, screen) {
                Served::One => {}
                Served::Empty => break,
                // As the wait loop closes one: through `repaint`, so the pointer survives a client
                // dying.
                Served::Closed => {
                    close_session(slot, srv);
                    repaint(srv, screen);
                    break;
                }
            }
        }
    }
}

/// Batches to take from the input channel in one pass: a bound, so a moving mouse cannot starve the
/// client and manager channels (about a second of a 100 Hz mouse).
const INPUT_DRAIN_MAX: usize = 128;

/// Drain every queued `Input::Events` batch, serve the requests already queued, route the batches,
/// and repaint once. `false` if the channel died.
///
/// Once for the drain: a repaint is a compose with no input read, so one per message capped mouse
/// speed.
fn serve_input(srv: &mut Server, screen: &mut Screen<RawFramebuffer>) -> bool {
    let mut out = alloc::vec::Vec::new();
    // Regions a restack disturbed this pass, plus the cursor's old and new positions; intermediate
    // positions were never drawn.
    let mut damage: alloc::vec::Vec<Rect> = alloc::vec::Vec::new();
    let cursor_was = srv.router.pointer();
    let mut alive = true;
    // Receive the whole pass, then serve the requests, then route (see [`serve_queued_requests`]).
    // Each batch is copied out because one message buffer is shared by every receive.
    let mut batches: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    let mut msg = Msg::new();
    while batches.len() < INPUT_DRAIN_MAX {
        let Some(input) = &srv.input else { break };
        if let Err(e) = input.recv(&mut msg) {
            // The ordinary end of the drain. `PeerClosed` means the input server is gone and
            // the handle must leave the wait set or the loop spins; anything else is spurious.
            alive = e.code() != KError::PeerClosed;
            break;
        }
        batches.push(msg.payload().to_vec());
    }
    serve_queued_requests(srv, screen);
    for batch in &batches {
        let now = now_ns();
        route_one_batch(srv, &mut out, &mut damage, batch, now);
    }
    send_lights(srv);

    deliver(srv, &out);
    // What routing did to the stack, told to the manager: input is the third path that mutates
    // windows and was the only one that never drained, so a pointer-made change sat in the log.
    drain_stack_events(srv);
    announce_focus(srv);
    // Erase the cursor where last drawn and draw it where it is, with any restack damage, in one
    // `present_into`.
    let cursor_now = srv.router.pointer();
    if cursor_now != cursor_was {
        damage.push(compositor::cursor_rect(cursor_was));
        damage.push(compositor::cursor_rect(cursor_now));
    }
    if !damage.is_empty() {
        screen.present(&srv.stack, BACKGROUND, srv, &damage, cursor_now, srv.outline);
    }
    alive
}

/// Route one input batch, appending client records to `out` and repaint regions to `damage`.
fn route_one_batch(
    srv: &mut Server,
    out: &mut alloc::vec::Vec<Outbound>,
    damage: &mut alloc::vec::Vec<Rect>,
    msg: &[u8],
    now: u64,
) {
    let Ok(m) = decode(msg) else {
        return;
    };
    if m.op != librsproto::OP_INPUT_EVENTS {
        return;
    }
    for raw in m.body.as_chunks::<INPUT_EVENT_LEN>().0 {
        let Some(ev) = InputEvent::read(raw) else {
            continue;
        };
        let mut logical = [libinput::Logical::Dropped { time_ns: 0 }; libinput::MAX_PER_GROUP];
        let n = srv.interp.feed(ev, &mut logical);
        for l in &logical[..n] {
            let routed = srv.router.route(l, &mut srv.stack, out);
            if let Some(r) = routed.restacked {
                damage.push(r);
            }
            if let Some(r) = routed.moved {
                damage.push(r);
            }
            // The outline's damage is its edges: repainting the union per motion starves input.
            if let Some(o) = routed.outline {
                srv.outline = o.now;
                for r in o.was.into_iter().chain(o.now).flat_map(compositor::outline_edges) {
                    damage.push(r);
                }
            }
            // A press landed outside a popup, so its owner is told. Reported, not acted on:
            // whether a dismissal ends what it offered is the client's to decide.
            if let Some(window) = routed.dismissed
                && let Some(slot) = srv.session_of(window)
            {
                // `log_route` and `enqueue`, as at every site here, or no gate can see it
                // (`check-login`).
                let rec = Outbound::Dismissed { window };
                log_route(&rec);
                enqueue(srv, slot, rec);
            }
            // One event for the gesture, at the release; the manager answers with a
            // `Configure`, so a window's geometry has one path.
            if let Some(e) = routed.ended {
                let (a, f) = (e.asked, e.from);
                Line::new()
                    .s(b"compositor: gesture on window ")
                    .u(e.window as u64)
                    .s(b" ended at ")
                    .i(a.origin.x as i64)
                    .s(b",")
                    .i(a.origin.y as i64)
                    .s(b" ")
                    .u(a.size.w as u64)
                    .s(b"x")
                    .u(a.size.h as u64)
                    .end();
                let rect = |r: libdraw::geom::Rect| ConfigureEvent {
                    window: e.window,
                    width: r.size.w,
                    height: r.size.h,
                    x: r.origin.x,
                    y: r.origin.y,
                };
                let flags = if e.moved { librsproto::surface::DRAG_ENDED_MOVED } else { 0 };
                let ended = librsproto::surface::MgrDragEnded { asked: rect(a), from: rect(f), flags };
                mgr_emit(srv, MgrEvent::DragEnded(ended));
            }
            // Drained per event: a chord and the window events it causes must reach the manager
            // in order.
            for hk in srv.router.take_hotkeys() {
                mgr_emit(srv, MgrEvent::Hotkey(hk));
            }
            // Diagnostics always, not through `log_route`, whose cap is spent by the time a
            // gate moves the cursor and which never sees a press that hit no window. A dropped
            // batch too: nothing else prints a `SYN_DROPPED`. `Super` arriving separates the
            // host keeping the keystroke from the guest matching no chord; the modifier only,
            // never the keycode, which would log passwords.
            // `check-input`, `check-login` and `check-install` match the press and Super lines.
            let meta_change = match *l {
                libinput::Logical::Key { keycode, pressed, .. }
                    if keycode == libkern::abi::KEY_LEFTMETA || keycode == libkern::abi::KEY_RIGHTMETA =>
                {
                    Some(pressed)
                }
                _ => None,
            };
            // Releases as well as presses, so a lost release is told from a client that ignored
            // the click (`check-login`). A button is not a keystroke.
            let diag = meta_change.is_some()
                || matches!(*l, libinput::Logical::Button { .. } | libinput::Logical::Dropped { .. });
            let logged = INPUT_DIAGS_LOGGED.load(Relaxed);
            if diag && logged < MAX_LOGGED_INPUT_DIAGS {
                INPUT_DIAGS_LOGGED.store(logged + 1, Relaxed);
                let mut pl = Line::new();
                match *l {
                    _ if meta_change == Some(true) => {
                        pl.s(b"compositor: Super down");
                    }
                    _ if meta_change == Some(false) => {
                        pl.s(b"compositor: Super up");
                    }
                    // A press whose release never appears was QEMU holding it (its PS/2 queue
                    // holds a packet until the next injected event); the gates flush with
                    // `expect_after_pointer`.
                    libinput::Logical::Dropped { .. } => {
                        pl.s(b"compositor: input batch DROPPED (SYN_DROPPED)");
                    }
                    _ => {
                        let p = srv.router.pointer();
                        let down = matches!(*l, libinput::Logical::Button { pressed: true, .. });
                        pl.s(if down {
                            b"compositor: press at x=".as_slice()
                        } else {
                            b"compositor: release at x=".as_slice()
                        })
                        .i(p.x as i64)
                        .s(b" y=")
                        .i(p.y as i64);
                        match srv.router.grab() {
                            Some(w) => {
                                pl.s(b" win=").u(w as u64);
                            }
                            None => {
                                pl.s(b" win=none");
                            }
                        }
                    }
                }
                pl.end();
            }
            // Repeat follows the physical key, except a consumed chord: `fire_repeat` bypasses
            // the router and would type it into the focused window.
            if let libinput::Logical::Key { keycode, pressed, modifiers, locks, .. } = *l
                && !routed.consumed
            {
                srv.repeat = compositor::Repeat::after_key(
                    srv.repeat,
                    keycode,
                    pressed,
                    modifiers,
                    locks,
                    srv.stack.focus_candidate(),
                    now,
                );
            }
            // A `SYN_DROPPED` makes the held-key set a guess, so a repeat from it is too.
            if matches!(l, libinput::Logical::Dropped { .. }) {
                srv.repeat = None;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Resolved {
    New,
    Manage,
    Info(u32),
    Screen,
    Unknown,
}

fn classify(suffix: &[u8]) -> Resolved {
    if suffix == b"new" {
        return Resolved::New;
    }
    if suffix == b"manage" {
        return Resolved::Manage;
    }
    if suffix == b"screen" {
        return Resolved::Screen;
    }
    if let Some(slash) = suffix.iter().position(|&c| c == b'/')
        && &suffix[slash + 1..] == b"info"
        && let Some(id) = parse_u32(&suffix[..slash])
    {
        return Resolved::Info(id);
    }
    Resolved::Unknown
}

fn parse_u32(b: &[u8]) -> Option<u32> {
    if b.is_empty() || b.len() > 10 {
        return None;
    }
    let mut n: u32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((c - b'0') as u32)?;
    }
    Some(n)
}

fn reply_window_info(serve_end: Ref<'_, Channel, Only>, request_id: u64, srv: &Server, id: u32) -> bool {
    let Some(info) = srv.stack.info(id) else {
        return reply_resolve_error(serve_end, request_id, KError::NotFound);
    };
    let mut bytes = [0u8; librsproto::surface::WINDOW_INFO_LEN];
    if info.write(&mut bytes).is_none() {
        return reply_resolve_error(serve_end, request_id, KError::KernelError);
    }
    server::reply_object(serve_end, request_id, &bytes)
}

fn reply_screen_info(serve_end: Ref<'_, Channel, Only>, request_id: u64, srv: &Server) -> bool {
    let info = librsproto::surface::ScreenInfo { width: srv.screen.size.w, height: srv.screen.size.h };
    let mut bytes = [0u8; librsproto::surface::SCREEN_INFO_LEN];
    if info.write(&mut bytes).is_none() {
        return reply_resolve_error(serve_end, request_id, KError::KernelError);
    }
    server::reply_object(serve_end, request_id, &bytes)
}

fn open_session(serve_end: Ref<'_, Channel, Only>, request_id: u64, srv: &mut Server) -> bool {
    let Some(slot) = srv.sessions.iter().position(Option::is_none) else {
        return reply_resolve_error(serve_end, request_id, KError::OutOfHandles);
    };
    let Ok((client_end, server_end)) = Handle::pair(SESSION_QUEUE_DEPTH) else {
        return reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
    };
    srv.conns[slot] = Connection::new();
    // A reply that fails closes the client's end, and `server_end` with it as it drops.
    if !server::reply_channel(serve_end, request_id, client_end) {
        return false;
    }
    srv.sessions[slot] = Some(server_end);
    true
}

/// Queue a `Configure` for `window`'s client. `false` if there is no such session.
///
/// Queued, not sent: a briefly full ring would otherwise drop it, unlogged.
fn configure_window(srv: &mut Server, window: u32, width: u32, height: u32, origin: Point) -> bool {
    let Some(slot) = srv.session_of(window) else { return false };
    if srv.sessions[slot].is_none() {
        return false;
    }
    enqueue(srv, slot, Outbound::Configure { window, width, height, x: origin.x, y: origin.y });
    true
}

fn do_capture(srv: &Server, request_id: u64, body: &[u8], obj: Option<Handle<Memory, MapReadWrite>>) -> bool {
    let fail = |err| reply_error_on_session(srv.manager(), OP_MGR_CAPTURE, request_id, err);
    let Some(req) = MgrCapture::read(body) else {
        return fail(KError::InvalidArgument);
    };
    let Some(obj) = obj.filter(|_| req.width != 0 && req.height != 0 && req.pitch != 0) else {
        return fail(KError::InvalidArgument);
    };
    let Some(w) = srv.stack.window(req.window) else {
        return fail(KError::NotFound);
    };
    // The committed buffer, as compositing reads, so a thumbnail never shows a frame the screen did
    // not.
    let (Some(buffer_id), id) = (w.committed, w.id) else {
        return fail(KError::WouldBlock);
    };
    let Some(b) = w.buffers.iter().find(|b| b.id == buffer_id).map(|b| b.geometry) else {
        return fail(KError::WouldBlock);
    };
    let dst_geom = match libdraw::framebuffer::Geometry::with_pitch(
        req.width,
        req.height,
        req.pitch as usize,
        libdraw::format::PixelFormat::XRGB8888,
    ) {
        Some(g) => g,
        None => {
            return fail(KError::InvalidArgument);
        }
    };
    let len = dst_geom.byte_len();
    if !obj.stat().is_ok_and(|info| holds(&info, len)) {
        return fail(KError::InvalidArgument);
    }
    let Ok(mut target) = obj.map(len) else {
        return fail(KError::InvalidArgument);
    };
    let mut scaled = alloc::vec::Vec::new();
    if scaled.try_reserve_exact(len).is_err() {
        return fail(KError::OutOfMemory);
    }
    scaled.resize(len, 0);
    let ok = match srv.buffer(id, buffer_id) {
        Some(src) => libdraw::scale::box_downscale_shared(src, b, &mut scaled, dst_geom),
        None => false,
    } && target.write_from(0, &scaled).is_ok();
    drop(target);
    if !ok {
        return fail(KError::InvalidArgument);
    }
    Line::new()
        .s(b"compositor: captured window ")
        .u(id as u64)
        .s(b" into ")
        .u(req.width as u64)
        .s(b"x")
        .u(req.height as u64)
        .end();
    reply_on_session(srv.manager(), OP_MGR_CAPTURE, request_id, &[])
}

/// What one call to [`serve_manager`] or [`serve_session`] found; `Empty` lets
/// [`serve_queued_requests`] stop draining a channel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Served {
    One,
    Empty,
    Closed,
}

fn serve_manager(srv: &mut Server, screen: &mut Screen<RawFramebuffer>) -> Served {
    let mut msg = Msg::new();
    let Some(ch) = &srv.manager else { return Served::Empty };
    match ch.recv(&mut msg) {
        Ok(carried) => {
            if handle_manager_request(srv, screen, &msg, carried) {
                Served::One
            } else {
                Served::Closed
            }
        }
        Err(e) if e.code() == KError::PeerClosed => Served::Closed,
        Err(_) => Served::Empty,
    }
}

fn handle_manager_request(
    srv: &mut Server,
    screen: &mut Screen<RawFramebuffer>,
    msg: &Msg,
    mut carried: Carried,
) -> bool {
    // A message that does not decode closes what it carried as `carried` drops.
    let Ok(m) = decode(msg.payload()) else {
        return true;
    };
    let (op, request_id, body) = (m.op, m.request_id, m.body);
    // Only `Capture` takes a handle; every other one closes as `carried` drops.
    let object = if op == OP_MGR_CAPTURE { carried.take::<Memory, MapReadWrite>(0) } else { None };
    drop(carried);
    if op == OP_MGR_CAPTURE {
        // Handled here, not in `manager::dispatch`, which sees only the stack: this needs the
        // handle, the pixel source and mappings.
        return do_capture(srv, request_id, body, object);
    }
    if op == librsproto::surface::OP_MGR_REQUEST_CLOSE {
        // Handled here: asking a client to close puts a record in that client's outbox, which the
        // server owns. The window itself is untouched. `check-login` waits on this line.
        let Some(req) = librsproto::surface::MgrWindowRef::read(body) else {
            return reply_error_on_session(srv.manager(), op, request_id, KError::InvalidArgument);
        };
        let Some(slot) = srv.session_of(req.window) else {
            return reply_error_on_session(srv.manager(), op, request_id, KError::NotFound);
        };
        enqueue(srv, slot, Outbound::CloseRequested { window: req.window });
        Line::new().s(b"compositor: asked window ").u(req.window as u64).s(b" to close").end();
        return reply_on_session(srv.manager(), op, request_id, &[]);
    }
    if op == librsproto::surface::OP_MGR_QUERY_LAYOUT {
        let layout = current_layout(srv);
        let mut body = [0u8; core::mem::size_of::<librsproto::surface::MgrLayout>()];
        return match layout.write(&mut body) {
            Some(n) => reply_on_session(srv.manager(), op, request_id, &body[..n]),
            None => reply_error_on_session(srv.manager(), op, request_id, KError::InvalidArgument),
        };
    }
    let mgr_outcome = manager::dispatch(&mut srv.stack, op, body);
    // Said on the console, changed or not: the one proof a boot can give that the shell's theme
    // reached this process (`check-login`).
    if op == librsproto::surface::OP_MGR_SET_SCHEME && matches!(mgr_outcome, MgrOutcome::Applied { .. }) {
        Line::new().s(b"compositor: scheme ").s(srv.stack.scheme().as_str().as_bytes()).end();
    }
    if let Some(w) = mgr_outcome.acted_on() {
        srv.stack.clear_state_request(w);
    }
    drain_stack_events(srv);
    match mgr_outcome {
        MgrOutcome::Applied { window, dirty } => {
            // The manager has acted on this window, so its held configure is answered; `None` names
            // no window.
            if let Some(window) = window {
                release_configure(srv, screen, window);
            }
            match dirty {
                Some(r) if r.size.w == 0 || r.size.h == 0 => {}
                Some(r) => repaint_region(srv, screen, r),
                None => repaint(srv, screen),
            }
            announce_focus(srv);
            reply_on_session(srv.manager(), op, request_id, &[]);
        }
        MgrOutcome::State { window, state } => {
            // To the client only: the compositor keeps no notion of a window's state.
            let Some(slot) = srv.session_of(window) else {
                return reply_error_on_session(srv.manager(), op, request_id, KError::NotFound);
            };
            let rec = Outbound::WindowState { window, state };
            log_route(&rec);
            enqueue(srv, slot, rec);
            reply_on_session(srv.manager(), op, request_id, &[]);
        }
        MgrOutcome::RegisterZone(z) => {
            // Applied here because the table is the router's, and replacing rather than refusing an
            // id already registered, as a zone table is a layout recomputed on a work-area change.
            match srv.router.register_zone(z) {
                Ok(disturbed) => {
                    // A registration that replaced the zone a drag is previewing moves the outline
                    // with it.
                    if let Some(o) = disturbed {
                        srv.outline = o.now;
                        for r in o.was.into_iter().chain(o.now).flat_map(compositor::outline_edges) {
                            if !r.is_empty() {
                                repaint_region(srv, screen, r);
                            }
                        }
                    }
                    Line::new()
                        .s(b"compositor: snap zone ")
                        .u(z.id as u64)
                        .s(b" registered, target ")
                        .i(z.target_x as i64)
                        .s(b",")
                        .i(z.target_y as i64)
                        .s(b" ")
                        .u(z.target_w as u64)
                        .s(b"x")
                        .u(z.target_h as u64)
                        .end();
                    reply_on_session(srv.manager(), op, request_id, &[]);
                }
                Err(_) => {
                    log::write(b"compositor: a snap zone registration was refused\n");
                    reply_error_on_session(srv.manager(), op, request_id, KError::InvalidArgument);
                }
            }
        }
        MgrOutcome::RegisterHotkey(hk) => {
            // Applied here because the table is the router's; a refusal is never a silent
            // replacement.
            match srv.router.register_hotkey(hk) {
                Ok(()) => {
                    Line::new()
                        .s(b"compositor: hotkey ")
                        .u(hk.id as u64)
                        .s(b" registered (mods ")
                        .u(hk.mods as u64)
                        .s(b", code ")
                        .u(hk.code as u64)
                        .s(b")")
                        .end();
                    reply_on_session(srv.manager(), op, request_id, &[]);
                }
                Err(_) => {
                    // `InvalidArgument` for all three, as the spec publishes: a full table never
                    // empties, so `WouldBlock` would make a manager retry forever.
                    log::write(b"compositor: a hotkey registration was refused\n");
                    reply_error_on_session(srv.manager(), op, request_id, KError::InvalidArgument);
                }
            }
        }
        MgrOutcome::Configure { window, width, height, origin } => {
            // A manager `Configure` on a held window is its initial configure, which is why it
            // carries an origin: one message answers both halves. The reply says the request was
            // queued, never that the client adopted it.
            let was_held = srv.pending_configure.iter().any(|&(w, _)| w == window);
            srv.pending_configure.retain(|&(w, _)| w != window);
            let became_visible = srv.stack.mark_configured(window);
            // The origin is applied through `place`, so its rules hold, including the refusal for a
            // dragged window.
            let moved = match srv.stack.place(window, origin) {
                Ok(d) => Some(d.rect()),
                Err(compositor::StackError::Dragging) => {
                    return reply_error_on_session(srv.manager(), op, request_id, KError::WouldBlock);
                }
                Err(_) => None,
            };
            // Drained again: this arm mutates after the drain above, so its geometry change would
            // wait for the next unrelated request.
            drain_stack_events(srv);
            if configure_window(srv, window, width, height, origin) {
                if let Some(r) = moved.filter(|r| !r.is_empty()) {
                    repaint_region(srv, screen, r);
                }
                if became_visible || was_held {
                    let r = srv.stack.window(window).map(|w| w.painted_bounds());
                    if let Some(r) = r {
                        repaint_region(srv, screen, r);
                    }
                    announce_focus(srv);
                }
                reply_on_session(srv.manager(), op, request_id, &[]);
            } else {
                // No session owns that window, so nobody can be told: a refusal, not a hollow
                // success.
                reply_error_on_session(srv.manager(), op, request_id, surface_errno(SurfaceError::NotFound));
            }
        }
        MgrOutcome::Failed(e) => {
            reply_error_on_session(srv.manager(), op, request_id, surface_errno(e));
        }
    }
    true
}

/// Mint the manager channel, or refuse because one is already attached.
fn open_manager(serve_end: Ref<'_, Channel, Only>, request_id: u64, srv: &mut Server) -> bool {
    if srv.manager.is_some() {
        // Refused, not replaced: a second manager would silently depose the first.
        log::write(b"compositor: a second manager was refused\n");
        return reply_resolve_error(serve_end, request_id, KError::WouldBlock);
    }
    let Ok((client_end, server_end)) = Handle::pair(SESSION_QUEUE_DEPTH) else {
        return reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
    };
    // A reply that fails closes the manager's end, and `server_end` with it as it drops.
    if !server::reply_channel(serve_end, request_id, client_end) {
        return false;
    }
    srv.manager = Some(server_end);
    log::write(b"compositor: a manager attached\n");
    true
}

/// Drop the manager channel, so the compositor manages itself again, and clear its event queue: a
/// backlog would describe windows the next manager never saw.
fn close_manager(srv: &mut Server, screen: &mut Screen<RawFramebuffer>) {
    srv.mgr_outbox.clear();
    // The chord table goes with it: left behind, its chords are consumed and delivered to nobody
    // (`mgr_emit` returns with no channel), and a replacement inherits the dead one's ids.
    srv.router.clear_hotkeys();
    // And the snap zones, which can be drawing something: a shell exiting mid-drag over a zone
    // leaves a previewed rectangle that nothing would take down.
    if let Some(o) = srv.router.clear_zones() {
        srv.outline = o.now;
        for r in o.was.into_iter().flat_map(compositor::outline_edges) {
            if !r.is_empty() {
                repaint_region(srv, screen, r);
            }
        }
    }
    // And the scheme it named: what is drawn next, the greeter after a logout, uses the built-in
    // one, and a dark session's shadows would fall too strongly under it.
    if srv.stack.set_scheme(libdraw::theme::Scheme::Light) {
        repaint(srv, screen);
        log::write(b"compositor: scheme light (the manager that named another went away)\n");
    }
    // Every window it was going to place is shown now: those clients are blocked, and waiting out a
    // timer for a manager that has gone is latency for nothing.
    let waiting: alloc::vec::Vec<u32> = srv.pending_configure.iter().map(|&(w, _)| w).collect();
    for window in waiting {
        release_configure(srv, screen, window);
    }
    srv.manager = None;
    log::write(b"compositor: the manager went away\n");
}

fn close_session(slot: usize, srv: &mut Server) {
    srv.sessions[slot] = None;
    srv.outbox[slot].clear();
    disconnect(&mut srv.conns[slot], &mut srv.stack);
    drain_stack_events(srv);
    announce_focus(srv);
}

/// Map a client's transferred `MemoryObject` at attach, as [`attach_memory`] rules, no handle
/// refused with the rest. Always consumes `handle`, on every path.
fn map_attached_buffer(
    body: &[u8],
    handle: Option<Handle<Memory, MapRead>>,
) -> Option<Attached<Option<Mapping<MapRead>>>> {
    let handle = handle?;
    // The mapping holds its own reference to the object, so it outlives `handle`.
    attach_memory(body, handle.stat().ok().as_ref(), |len| handle.map(len).ok())
}

/// Recomposite the whole screen: the fallback for `Applied { dirty: None }` and the first frame.
fn repaint(srv: &Server, screen: &mut Screen<RawFramebuffer>) {
    let bounds = screen.geometry().bounds();
    repaint_region(srv, screen, bounds);
}

/// Recompose `region` and draw the cursor over it, last: compositing it into the stack would make
/// it a window with a stacking position and a hit-test to skip.
fn repaint_region(srv: &Server, screen: &mut Screen<RawFramebuffer>, region: Rect) {
    screen.present(&srv.stack, BACKGROUND, srv, &[region], srv.router.pointer(), srv.outline);
}

fn surface_errno(e: SurfaceError) -> KError {
    match e {
        SurfaceError::Malformed => KError::InvalidArgument,
        SurfaceError::Unsupported => KError::Unsupported,
        // A window of another connection reports what a nonexistent one does: ownership
        // enforcement, not secrecy (a namespace resolve answers the same question), but acting
        // teaches nothing.
        SurfaceError::NotFound => KError::NotFound,
        // `WouldBlock`, because it is true again in a moment: a `Place` refused for a drag is
        // neither malformed nor a permanent no.
        SurfaceError::Rejected(compositor::StackError::Dragging) => KError::WouldBlock,
        SurfaceError::Rejected(_) => KError::InvalidArgument,
    }
}

/// The screen and the work area, as they are now. The work area is the compositor's to compute:
/// only this process sees every panel's strut.
fn current_layout(srv: &Server) -> librsproto::surface::MgrLayout {
    let screen = srv.screen;
    let work = srv.stack.work_area(screen);
    librsproto::surface::MgrLayout {
        screen_w: screen.size.w,
        screen_h: screen.size.h,
        work_x: work.origin.x,
        work_y: work.origin.y,
        work_w: work.size.w,
        work_h: work.size.h,
    }
}

/// Tell the manager when the work area is not what it was: compared, not triggered, since it needs
/// the change and not its cause.
fn announce_layout(srv: &mut Server) {
    let now = current_layout(srv);
    if srv.last_layout == Some(now) {
        return;
    }
    srv.last_layout = Some(now);
    mgr_emit(srv, MgrEvent::LayoutChanged(now));
}

fn serve_session(slot: usize, srv: &mut Server, screen: &mut Screen<RawFramebuffer>) -> Served {
    let mut msg = Msg::new();
    let Some(ch) = &srv.sessions[slot] else { return Served::Empty };
    match ch.recv(&mut msg) {
        Ok(carried) => {
            if handle_session_request(slot, srv, screen, &msg, carried) {
                Served::One
            } else {
                Served::Closed
            }
        }
        Err(e) if e.code() == KError::PeerClosed => Served::Closed,
        Err(_) => Served::Empty,
    }
}

fn handle_session_request(
    slot: usize,
    srv: &mut Server,
    screen: &mut Screen<RawFramebuffer>,
    msg: &Msg,
    mut carried: Carried,
) -> bool {
    // A message that does not decode closes what it carried as `carried` drops.
    let Ok(m) = decode(msg.payload()) else {
        return true;
    };
    let (op, request_id, body) = (m.op, m.request_id, m.body);
    // Only an attach takes a handle, and only its first; the rest close as `carried` drops.
    let handle = if op == OP_ATTACH_BUFFER { carried.take::<Memory, MapRead>(0) } else { None };
    drop(carried);

    // `StartMove` is answered here as `dispatch` sees no router, which owns the grab this is
    // checked against. A foreign window answers `NotFound`.
    if op == librsproto::surface::OP_START_MOVE {
        let err = match librsproto::surface::StartMove::read(body) {
            None => Some(KError::InvalidArgument),
            Some(req) if !srv.conns[slot].owns(req.window) => Some(KError::NotFound),
            Some(req) => match srv.router.start_move(req.window, &mut srv.stack) {
                Ok(damage) => {
                    // The catch-up is painted here: with no motion event after a
                    // press-flick-release, the window would stay drawn where it was.
                    if let Some(r) = damage.filter(|r| !r.is_empty()) {
                        repaint_region(srv, screen, r);
                    }
                    // `check-login` waits on this line and on the resize and drag lines below.
                    Line::new().s(b"compositor: interactive move of window ").u(req.window as u64).end();
                    None
                }
                // One answer for no grab or a gone window, but said out loud: a silent refusal
                // looks like a request that never arrived.
                Err(_) => {
                    Line::new()
                        .s(b"compositor: refused a move of window ")
                        .u(req.window as u64)
                        .s(b": the pointer is not holding it")
                        .end();
                    Some(KError::NotFound)
                }
            },
        };
        return match err {
            Some(e) => reply_error_on_session(srv.session(slot), op, request_id, e),
            None => reply_on_session(srv.session(slot), op, request_id, &[]),
        };
    }

    // `StartResize` is answered here for the reason `StartMove` is. It resizes nothing: an outline
    // follows the pointer and the manager hears one event at the release.
    if op == librsproto::surface::OP_START_RESIZE {
        let err = match librsproto::surface::StartResize::read(body) {
            None => Some(KError::InvalidArgument),
            Some(req) if !srv.conns[slot].owns(req.window) => Some(KError::NotFound),
            Some(req) => match srv.router.start_resize(req.window, req.edges, &mut srv.stack) {
                Ok(outline) => {
                    srv.outline = outline;
                    for r in outline.into_iter().flat_map(compositor::outline_edges) {
                        if !r.is_empty() {
                            repaint_region(srv, screen, r);
                        }
                    }
                    Line::new()
                        .s(b"compositor: interactive resize of window ")
                        .u(req.window as u64)
                        .s(b" edges ")
                        .u(req.edges as u64)
                        .end();
                    None
                }
                // Refused: no grab, no window, or edges that name no gesture. Said out loud like a
                // refused move.
                Err(e) => {
                    Line::new()
                        .s(b"compositor: refused a resize of window ")
                        .u(req.window as u64)
                        .s(if matches!(e, compositor::StackError::BadGeometry) {
                            b": those edges are not a gesture" as &[u8]
                        } else {
                            b": the pointer is not holding it"
                        })
                        .end();
                    Some(match e {
                        compositor::StackError::BadGeometry => KError::InvalidArgument,
                        _ => KError::NotFound,
                    })
                }
            },
        };
        return match err {
            Some(e) => reply_error_on_session(srv.session(slot), op, request_id, e),
            None => reply_on_session(srv.session(slot), op, request_id, &[]),
        };
    }

    // A window says what it takes, once. The compositor holds the table so "would this window take
    // this drag?" is answered per motion without a round trip.
    if op == librsproto::surface::OP_DECLARE_ACCEPTOR {
        let err = match librsproto::surface::DeclareAcceptor::read(body) {
            None => Some(KError::InvalidArgument),
            Some((req, _)) if !srv.conns[slot].owns(req.window) => Some(KError::NotFound),
            Some((req, name)) => match srv.stack.declare_acceptor(req.window, name, req.kinds) {
                Ok(()) => {
                    Line::new()
                        .s(b"compositor: window ")
                        .u(req.window as u64)
                        .s(b" accepts ")
                        .untrusted(name.as_bytes())
                        .s(b" kinds ")
                        .u(req.kinds as u64)
                        .end();
                    None
                }
                Err(compositor::StackError::TooManyAcceptors) => {
                    Line::new()
                        .s(b"compositor: window ")
                        .u(req.window as u64)
                        .s(b" has no room for another acceptor")
                        .end();
                    Some(KError::Unsupported)
                }
                Err(_) => Some(KError::NotFound),
            },
        };
        return match err {
            Some(e) => reply_error_on_session(srv.session(slot), op, request_id, e),
            None => reply_on_session(srv.session(slot), op, request_id, &[]),
        };
    }

    // A drag is a gesture the compositor runs, refused like a move and a resize without the grab;
    // it ends in a payload for the window it was let go over.
    if op == librsproto::surface::OP_START_DRAG {
        let err = match librsproto::surface::StartDrag::read(body) {
            None => Some(KError::InvalidArgument),
            Some((req, ..)) if !srv.conns[slot].owns(req.window) => Some(KError::NotFound),
            Some((req, path, name)) => {
                match srv.router.start_drag(req.window, req.kind, path, name, &srv.stack) {
                    Ok(outline) => {
                        apply_outline(srv, screen, outline);
                        // The kind and the display name, never the path: the console is no record
                        // of which files somebody moves. `check-login` waits on this line.
                        Line::new()
                            .s(b"compositor: drag from window ")
                            .u(req.window as u64)
                            .s(b" kind ")
                            .u(req.kind as u64)
                            .s(b" name ")
                            .untrusted(name.as_bytes())
                            .end();
                        None
                    }
                    Err(_) => {
                        Line::new()
                            .s(b"compositor: refused a drag from window ")
                            .u(req.window as u64)
                            .s(b": the pointer is not holding it")
                            .end();
                        Some(KError::NotFound)
                    }
                }
            }
        };
        return match err {
            Some(e) => reply_error_on_session(srv.session(slot), op, request_id, e),
            None => reply_on_session(srv.session(slot), op, request_id, &[]),
        };
    }

    // `RequestState` is forwarded, not applied: minimising and maximising are manager operations,
    // so the compositor only checks ownership and hands the manager the question.
    if op == librsproto::surface::OP_REQUEST_STATE {
        let err = match librsproto::surface::WindowState::read(body) {
            None => Some(KError::InvalidArgument),
            Some(req) if req.state > librsproto::surface::WINDOW_STATE_MAXIMIZED => Some(KError::InvalidArgument),
            Some(req) if !srv.conns[slot].owns(req.window) => Some(KError::NotFound),
            Some(req) => {
                // Repeats produce nothing: a client's own rate drives this event and the manager's
                // queue does not coalesce.
                if srv.stack.note_state_request(req.window, req.state) {
                    mgr_emit(srv, MgrEvent::StateRequest(req));
                }
                None
            }
        };
        return match err {
            Some(e) => reply_error_on_session(srv.session(slot), op, request_id, e),
            None => reply_on_session(srv.session(slot), op, request_id, &[]),
        };
    }

    // **An attach brings memory, or it is refused**: asked before `dispatch`, so the stack takes no
    // geometry without it, and recorded after, as a rejected one would put the client's memory
    // under ids it does not own. The handle is disposed of exactly once.
    let attached = if op == OP_ATTACH_BUFFER { map_attached_buffer(body, handle) } else { None };
    let mut reply = [0u8; MAX_BODY];
    let outcome = if op == OP_ATTACH_BUFFER && attached.is_none() {
        refuse_attach(&srv.conns[slot], body)
    } else {
        dispatch(&mut srv.conns[slot], &mut srv.stack, op, body, &mut reply)
    };
    drain_stack_events(srv);
    if let Some(attached) = attached {
        record_attach(&mut srv.buffers, &outcome, attached);
    }

    match outcome {
        Outcome::Reply(len) => {
            if !reply_on_session(srv.session(slot), op, request_id, &reply[..len.min(MAX_BODY)]) {
                log::write(b"compositor: DROPPED a reply (client receive ring full)\n");
            }
        }
        Outcome::Created { reply_len, configure } => {
            // The reply first, then the configure, on the same channel: the client reads its id and
            // then waits for the configure before it may commit.
            let n = reply_len.min(MAX_BODY);
            // The manager hears of it with the role and size asked for: a follow-up question would
            // be a seam with a round trip in it.
            if let Some(w) = srv.stack.window(configure.window) {
                let ev = librsproto::surface::MgrWindowCreated::for_role(
                    configure.window,
                    w.role,
                    configure.width,
                    configure.height,
                );
                mgr_emit(srv, MgrEvent::Created(ev));
            }
            let sent = reply_on_session(srv.session(slot), op, request_id, &reply[..n]);
            if !sent {
                log::write(b"compositor: a create reply did not send\n");
            }
            // Held when a manager is attached, so it can place the window before anyone sees it. A
            // popup is never held: its creator places it, and every menu open would cost the
            // deadline. A `dialog` is held like a `normal`.
            let placed_by_creator = srv
                .stack
                .window(configure.window)
                .is_some_and(|w| matches!(w.role, librsproto::surface::Role::Popup { .. }));
            if sent && !placed_by_creator && srv.manager.is_some() {
                srv.pending_configure.push((configure.window, now_ns().saturating_add(CONFIGURE_DEADLINE_NS)));
            } else {
                let mut cfg = [0u8; core::mem::size_of::<ConfigureEvent>()];
                srv.stack.mark_configured(configure.window);
                if sent && configure.write(&mut cfg).is_some() && !send_input(srv.session(slot), OP_CONFIGURE, &cfg) {
                    // The client waits for this and nothing else produces it; a silent failure
                    // parks it forever.
                    log::write(b"compositor: a window's first configure did not send\n");
                }
            }
        }
        Outcome::Applied { release, dirty } => {
            // Paint before releasing: a `Release` tells the client the frame is done, and pacing
            // off that would see an unpainted screen.
            match dirty {
                Some(r) if r.size.w == 0 || r.size.h == 0 => {}
                Some(r) => repaint_region(srv, screen, r),
                None => repaint(srv, screen),
            }
            if let Some((window, buffer)) = release {
                // Through the outbox: sent directly it competed with continuous input on the same
                // ring and was evicted. Queued, it holds its place and is never coalesced away.
                enqueue(srv, slot, Outbound::Release { window, buffer });
            }
        }
        Outcome::TitleSet { window } => {
            let title = srv.stack.window(window).map(|w| w.title.clone()).unwrap_or_default();
            mgr_emit(srv, MgrEvent::Title { window, title });
        }
        Outcome::Failed(e) => {
            // Logged a bounded number of times: a rejection is client-driven. The error still goes
            // to the client.
            srv.conns[slot].rejections_logged += 1;
            let n = srv.conns[slot].rejections_logged;
            if n <= MAX_LOGGED_REJECTIONS {
                log::write(match e {
                    SurfaceError::Malformed => b"compositor: malformed request\n" as &[u8],
                    SurfaceError::Unsupported => b"compositor: unsupported op\n",
                    SurfaceError::NotFound => b"compositor: no such window for this connection\n",
                    SurfaceError::Rejected(_) => b"compositor: request rejected\n",
                });
                if n == MAX_LOGGED_REJECTIONS {
                    log::write(b"compositor: (further rejections not logged)\n");
                }
            }
            reply_error_on_session(srv.session(slot), op, request_id, surface_errno(e));
        }
    }

    // After the request, whatever it was: `announce_focus` compares with the last announcement, so
    // no list of focus-moving ops need stay correct (create replies without reaching `Applied`).
    // After the reply, as a client cannot use focus for an id it lacks.
    announce_focus(srv);
    true
}

/// What a wait's index names.
#[derive(Copy, Clone)]
enum Waited {
    Resolve,
    Control,
    Input,
    Manager,
    Session(usize),
}

/// Add `h` to `set` as `what`.
fn watch<'a>(set: &mut WaitSet<'a, MAX_WAIT_HANDLES>, names: &mut [Waited], h: &'a Endpoint, what: Waited) {
    if let Ok(i) = set.add(h) {
        names[i] = what;
    }
}

fn serve_loop(
    serve_end: Endpoint,
    mut screen: Screen<RawFramebuffer>,
    srv: &mut Server,
    mut control: Option<Endpoint>,
) -> ! {
    log::write(b"compositor: serving /dev/draw\n");
    let mut parked = false;
    loop {
        // Every iteration, not only on a timeout, or the configure deadline holds only when idle.
        // Flushed here: a released configure is queued and the client is blocked on it.
        if fire_configure_deadlines(srv, &mut screen) {
            parked = flush_outboxes(srv);
        }
        // At most `4 + MAX_SESSIONS` — `serve_end`, the control channel until its supervisor has
        // gone, the input channel when connected, the manager channel when one is attached, then the
        // sessions — which the `const _` beside `MAX_SESSIONS` holds to the wait limit.
        let mut names = [Waited::Resolve; MAX_WAIT_HANDLES];
        let waited = {
            let mut set = WaitSet::new();
            watch(&mut set, &mut names, &serve_end, Waited::Resolve);
            if let Some(c) = &control {
                watch(&mut set, &mut names, c, Waited::Control);
            }
            if let Some(i) = &srv.input {
                watch(&mut set, &mut names, i, Waited::Input);
            }
            if let Some(m) = &srv.manager {
                watch(&mut set, &mut names, m, Waited::Manager);
            }
            for (slot, s) in srv.sessions.iter().enumerate() {
                if let Some(s) = s {
                    watch(&mut set, &mut names, s, Waited::Session(slot));
                }
            }
            // Bounded while something is parked or a key is repeating, whichever is sooner; an idle
            // compositor still sleeps indefinitely, so this is not a poll.
            let now = now_ns();
            let mut deadline = u64::MAX;
            if parked {
                deadline = deadline.min(now.saturating_add(RETRY_INTERVAL_NS));
            }
            if let Some(r) = srv.repeat {
                deadline = deadline.min(r.next_at);
            }
            for &(_, at) in &srv.pending_configure {
                deadline = deadline.min(at);
            }
            set.wait(Deadline::at(deadline))
        };
        let signalled = match waited {
            Ok(signalled) if !signalled.is_empty() => signalled,
            _ => {
                fire_repeat(srv);
                parked = flush_outboxes(srv);
                continue;
            }
        };

        let mut serve_signalled = false;
        for (i, _) in signalled.iter() {
            match names[i] {
                Waited::Resolve => serve_signalled = true,
                // `service --stop`: every window goes with this process and its clients find their
                // channels closed; a restarted compositor takes the screen again.
                Waited::Control => {
                    if control::recv(&mut control) == Control::Op(CTRL_OP_SHUTDOWN) {
                        log::write(b"compositor: asked to stop, exiting\n");
                        libos::exit(0);
                    }
                }
                Waited::Input => {
                    if srv.input.is_some() && !serve_input(srv, &mut screen) {
                        // The input server died: carry on serving the display rather than exit because a
                        // mouse went away.
                        log::write(b"compositor: input server gone\n");
                        srv.input = None;
                    }
                }
                Waited::Manager => {
                    if serve_manager(srv, &mut screen) == Served::Closed {
                        close_manager(srv, &mut screen);
                    }
                }
                Waited::Session(slot) => {
                    if serve_session(slot, srv, &mut screen) == Served::Closed {
                        close_session(slot, srv);
                        repaint(srv, &mut screen);
                    }
                }
            }
        }
        // Before the early `continue`: everything above may have queued messages, and the common
        // iteration (input, no resolve) takes that `continue`.
        fire_repeat(srv);
        parked = flush_outboxes(srv);

        if !serve_signalled {
            continue;
        }

        // A forwarded resolve: `new` mints a session, `manage` the manager channel, `<N>/info`
        // window metadata, `screen` the screen's size. `<N>/ports/...` is unscheduled:
        // `TODO(port-shape-rework)`.
        let mut msg = Msg::new();
        let Ok(_carried) = serve_end.recv(&mut msg) else {
            continue;
        };
        let (request_id, resolved) = match decode(msg.payload()) {
            Ok(m) if m.op == OP_NS_RESOLVE => {
                let suffix = librsproto::namespace::parse_resolve_request(m.body).map(|r| r.suffix).unwrap_or(b"");
                (m.request_id, classify(suffix))
            }
            Ok(m) => (m.request_id, Resolved::Unknown),
            Err(_) => continue,
        };
        match resolved {
            Resolved::New => {
                open_session(serve_end.as_ref(), request_id, srv);
            }
            Resolved::Manage => {
                open_manager(serve_end.as_ref(), request_id, srv);
            }
            Resolved::Info(id) => {
                reply_window_info(serve_end.as_ref(), request_id, srv, id);
            }
            Resolved::Screen => {
                reply_screen_info(serve_end.as_ref(), request_id, srv);
            }
            Resolved::Unknown => {
                reply_resolve_error(serve_end.as_ref(), request_id, KError::NotFound);
            }
        }
    }
}

/// Called by the kernel's ELF entry; `control` is the channel the supervisor spawned us with.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"compositor: up\n");
    let control: Option<Endpoint> = start.take_handle0();

    // The shadow buffer is allocated before the screen changes hands: acquiring `/dev/framebuffer`
    // stops the kernel console drawing, so a later refusal would be unreadable on a machine with no
    // serial port (`check-fbcon`). Reading the geometry does not take the screen; `acquire` does.
    let geometry = start
        .root
        .as_ref()
        .and_then(|root| libdraw::acquire::framebuffer_info(root).ok())
        .and_then(|info| libdraw::acquire::geometry_from(&info).ok());
    let shadow = match geometry {
        Some(geometry) => match libdraw::framebuffer::MemFramebuffer::try_new(geometry) {
            Some(shadow) => shadow,
            None => {
                log::write(b"compositor: no memory for a shadow buffer -- cannot serve\n");
                libos::exit(1);
            }
        },
        None => {
            log::write(b"compositor: cannot read the framebuffer's shape -- cannot serve\n");
            libos::exit(1);
        }
    };

    // SAFETY: the mapping is kept for the process's life, and from here the aperture is reachable
    // only through `screen`, the one writer once acquiring has stopped the kernel console drawing.
    let acquired = start.root.as_ref().and_then(|root| unsafe { libdraw::acquire::map_framebuffer(root) }.ok());
    let Some((aperture, _info)) = acquired else {
        log::write(b"compositor: no /dev/framebuffer -- cannot serve\n");
        libos::exit(1);
    };
    // From here the aperture is reachable only through `screen`: frames are composed in the shadow
    // buffer and copied out, so nothing is scanned out half-painted. There is no serving without
    // it: composing into the write-combining framebuffer reads it back uncached, a second a frame.
    let mut screen = Screen::with_shadow(aperture, shadow);

    let Ok((kernel_end, serve_end)) = Handle::pair(SESSION_QUEUE_DEPTH) else {
        log::write(b"compositor: channel create FAIL\n");
        libos::exit(1);
    };
    let screen_bounds = screen.geometry().bounds();
    let mut srv = Server {
        stack: WindowStack::new(),
        conns: core::array::from_fn(|_| Connection::new()),
        sessions: [const { None }; MAX_SESSIONS],
        manager: None,
        buffers: alloc::vec::Vec::new(),
        repeat: None,
        pending_configure: alloc::vec::Vec::new(),
        announced_focus: None,
        screen: screen_bounds,
        last_layout: None,
        outline: None,
        outbox: (0..MAX_SESSIONS).map(|_| Outbox::new()).collect(),
        mgr_outbox: MgrOutbox::new(),
        interp: Interpreter::new(),
        // The cursor clamps to the screen actually acquired, not a constant that happens to match.
        router: InputRouter::new(screen_bounds),
        input: None,
        lights_sent: None,
    };
    match start.root.as_ref().and_then(connect_input) {
        Some(input) => {
            srv.input = Some(input);
            // `check-input` waits on this line.
            log::write(b"compositor: input connected\n");
            send_lights(&mut srv);
        }
        None => log::write(b"compositor: no /dev/input/new -- display only\n"),
    }

    // Clear before announcing readiness, so `Meta::Ready` means the screen is taken: announcing
    // first left the clear racing the next spawn, and on `-smp 4` two processes held
    // `/dev/framebuffer` mapped read-write. The pointer is drawn here too, so it exists before the
    // mouse first moves.
    repaint(&srv, &mut screen);

    if !control.as_ref().is_some_and(|c| server::ready(c, b"compositor", kernel_end)) {
        log::write(b"compositor: Ready send FAIL\n");
        libos::exit(1);
    }

    serve_loop(serve_end, screen, &mut srv, control);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"compositor: panic\n");
    libos::exit(2);
}
