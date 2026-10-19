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
use compositor::server::{Connection, Outcome, SurfaceError, disconnect, dispatch};
use compositor::{BufferSource, WindowStack};
use libdraw::format::Rgb;
use libdraw::framebuffer::RawFramebuffer;
use libdraw::geom::{Point, Rect};
use libinput::Interpreter;
use libkern::abi::CLOCK_MONOTONIC;
use libkern::abi::{INPUT_EVENT_LEN, InputEvent};
use libkern::debug::Line;
use libkern::error::KError;
use libkern::{
    SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_CLOCK_READ, SYS_HANDLE_CLOSE,
    SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_WAIT, exit, kprint, syscall2, syscall4, syscall5,
};
use librsproto::namespace::{OBJECT_KIND_CHANNEL, resolve_reply};
use librsproto::surface::{
    ConfigureEvent, FocusEvent, KeyEvent, MgrCapture, OP_ATTACH_BUFFER, OP_CONFIGURE, OP_FOCUS_EVENT, OP_KEY_EVENT,
    OP_MGR_CAPTURE, OP_POINTER_EVENT, OP_RELEASE, PointerEvent,
};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const MSG_LEN: usize = 4096;
const PAYLOAD_OFF: usize = 24;
/// The background the screen is cleared to; the display gate's reference render fills with the same
/// `scene::BACKGROUND`, so the two must be one constant.
const BACKGROUND: Rgb = libdraw::scene::BACKGROUND;

/// Largest Surface request body, sized by `SetTitle`, the only variable-length one; the assertion
/// below ties it to `MAX_TITLE`.
///
/// A smaller body cut titles at a raw byte offset, which failed `Malformed` mid-character.
const MAX_BODY: usize = 4 + librsproto::surface::MAX_TITLE;
const _: () = assert!(MAX_BODY >= 4 + librsproto::surface::MAX_TITLE);

/// Four less than the wait limit: the forwarding, input, manager and control channels.
const MAX_SESSIONS: usize = libkern::abi::MAX_WAIT_HANDLES - 4;

const _: () = assert!(4 + MAX_SESSIONS <= libkern::abi::MAX_WAIT_HANDLES);

/// How many client-driven rejections get logged **per session** before the tap closes.
const MAX_LOGGED_REJECTIONS: u32 = 8;

/// How many routed input events get logged before the tap closes; serial is slow and input
/// continuous.
const MAX_LOGGED_ROUTES: u32 = 8;

/// How many edge-triggered input diagnostics (a press and where it landed, a dropped batch) get
/// logged. Generous, since any gate needs a handful, but not absent: serial is slow.
const MAX_LOGGED_INPUT_DIAGS: u32 = 256;

static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
/// Sized by the kernel's limit: `sys_channel_recv` takes no capacity and copies the sender's
/// `handle_count * 8` bytes, bounded only by `IPC_HANDLE_MAX`.
static mut RECV_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] = [0; libkern::abi::IPC_HANDLE_MAX];
static mut RECV_COUNT: u64 = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] = [0; libkern::abi::IPC_HANDLE_MAX];
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
/// The manager's channel, or `0`. One, not a table: two managers placing windows race with no
/// arbiter, so a second resolve is refused.
static mut MANAGER_CH: u64 = 0;
static mut ROUTES_LOGGED: u32 = 0;
static mut INPUT_DIAGS_LOGGED: u32 = 0;
static mut CLOCK_BUF: u64 = 0;

/// How long to sleep before retrying a parked message, in nanoseconds.
///
/// A parked message has no wakeup of its own (a draining client signals nothing), so the wait is
/// bounded while any is parked. Ten ms is the scheduler tick.
const RETRY_INTERVAL_NS: u64 = 10_000_000;

/// How long a new window waits for the manager to place it before it is shown anyway: a slow shell
/// must delay a window, never lose it. 200ms outlasts an idle round trip.
const CONFIGURE_DEADLINE_NS: u64 = 200_000_000;

const MAX_LOGGED_OVERFLOWS: u32 = 8;
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] = [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] = [0; 24 * libkern::abi::MAX_WAIT_HANDLES];

struct MappedBuffer {
    window: u32,
    buffer: u32,
    addr: *mut u8,
    len: usize,
}

impl Drop for MappedBuffer {
    /// Unmap when the record goes away: the `Vec::retain` calls that shrink `Server::buffers` rely
    /// on it.
    fn drop(&mut self) {
        // SAFETY: `addr`/`len` came from a successful `sys_memory_map` in
        // `map_attached_buffer`; a record is dropped once, so this unmaps once.
        unsafe { syscall2(libkern::SYS_MEMORY_UNMAP, self.addr as u64, self.len as u64) };
    }
}

struct Server {
    stack: WindowStack,
    conns: [Connection; MAX_SESSIONS],
    buffers: alloc::vec::Vec<MappedBuffer>,
    /// Device triples → logical events. Reset by a `SYN_DROPPED` the server forwards.
    interp: Interpreter,
    router: InputRouter,
    /// The consumer channel from `/dev/input/new`, or 0 if input is unavailable.
    input_ch: u64,
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
}

impl BufferSource for Server {
    fn pixels(&self, window: u32, buffer: u32) -> Option<&[u8]> {
        let b = self.buffers.iter().find(|b| b.window == window && b.buffer == buffer)?;
        // SAFETY: `addr`/`len` come from a successful `sys_memory_map` of the client's
        // `MemoryObject`. The mapping lives exactly as long as its `MappedBuffer` record —
        // `Drop` unmaps — and this slice borrows `&self`, so no `&mut` path that could drop
        // the record can run while it is alive.
        Some(unsafe { core::slice::from_raw_parts(b.addr, b.len) })
    }
}

/// Receive-ring depth for a client **session** channel: the kernel's `IPC_DEFAULT_QUEUE_DEPTH`. A
/// stalled client's motion coalesces only after this many queue; 128 KiB of kernel memory per
/// session.
const SESSION_QUEUE_DEPTH: u64 = 16;

fn now_ns() -> u64 {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: on success the kernel wrote the ns count.
    unsafe { (&raw const CLOCK_BUF).read() }
}

fn make_channel(depth: u64) -> Option<(u64, u64)> {
    // SAFETY: CTRL_OUT0/CTRL_OUT1 are valid writable out-params.
    let cr =
        unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL_OUT0) as u64, (&raw mut CTRL_OUT1) as u64, depth, 0) };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const CTRL_OUT0).read(), (&raw const CTRL_OUT1).read()) })
}

fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(body_len) = librsproto::meta::ready(&mut body, b"compositor") else { return false };
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto message goes at PAYLOAD_OFF.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], librsproto::OP_READY, 0, 0, &body[..body_len], 1) {
            Some(n) => n,
            None => return false,
        }
    };
    // SAFETY: stamp the IpcMsg header (payload_len @4, handle_count @8) + handle slot.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = kernel_end;
    }
    // SAFETY: valid endpoint + message + 1-handle transfer.
    let sr = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            control,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    sr == 0
}

fn reply_session(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid; one handle rides the reply.
    unsafe {
        let Some(rs_len) = encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1)
        else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = client_end;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

fn reply_resolve_error(serve_end: u64, request_id: u64, err: KError) -> bool {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = librsproto::error::error_body(&mut ebody, err.as_i32(), 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer; no handles transferred.
    unsafe {
        let Some(rs_len) = encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            OP_NS_RESOLVE,
            request_id,
            RS_FLAG_REPLY | RS_FLAG_ERROR,
            &ebody[..elen],
            0,
        ) else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

fn reply_on_session(session: u64, op: u16, request_id: u64, body: &[u8]) -> bool {
    // SAFETY: REPLY_MSG is a valid buffer; no handles transferred.
    unsafe {
        let Some(rs_len) = encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, RS_FLAG_REPLY, body, 0) else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            session,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Send one server-initiated record, with no request id. `false` if the channel would not take it:
/// the caller leaves the message at the head of its outbox and retries.
fn send_input(session: u64, op: u16, body: &[u8]) -> bool {
    reply_on_session(session, op, 0, body)
}

/// Tell the input server the lights the locks call for, if not what it was last told.
///
/// `lights_sent` moves only on a send, so a full channel delays the lights rather than losing them.
fn send_lights(srv: &mut Server) {
    let lights = libinput::lights(srv.interp.locks());
    if srv.input_ch == 0 || srv.lights_sent == Some(lights) {
        return;
    }
    // SAFETY: REPLY_MSG is a valid buffer; no handles transferred.
    let sent = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], librsproto::OP_INPUT_LIGHTS, 0, 0, &[lights], 0) {
            Some(rs_len) => {
                REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
                REPLY_MSG[8] = 0;
                syscall5(
                    SYS_CHANNEL_SEND,
                    srv.input_ch,
                    (&raw const REPLY_MSG) as u64,
                    (&raw const REPLY_HANDLES) as u64,
                    0,
                    SENDMODE_NOBLOCK,
                ) == 0
            }
            None => false,
        }
    };
    if sent {
        srv.lights_sent = Some(lights);
    }
}

fn log_route(rec: &Outbound) {
    // A drop or a dismissal bypasses the cap: one record per gesture, long after it is spent, and
    // the only evidence the compositor acted (`check-login` waits on `drop win=`, `dismissed
    // win=`).
    if !matches!(rec, Outbound::Dropped { .. } | Outbound::Dismissed { .. }) {
        // SAFETY: single-threaded server; this counter is touched only from the serve loop.
        let n = unsafe { ROUTES_LOGGED };
        if n > MAX_LOGGED_ROUTES {
            return;
        }
        // SAFETY: as above.
        unsafe { ROUTES_LOGGED = n + 1 };
        if n == MAX_LOGGED_ROUTES {
            kprint(b"compositor: (further routed input not logged)\n");
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
    #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
    for slot in 0..MAX_SESSIONS {
        // SAFETY: reading our own slot table.
        let ch = unsafe { SESSION_CH[slot] };
        if ch == 0 {
            srv.outbox[slot].clear();
            continue;
        }
        while let Some(rec) = srv.outbox[slot].front() {
            if !send_outbound(ch, rec) {
                break;
            }
            srv.outbox[slot].pop();
        }
    }
    // Drained as the clients' are: a manager that lost a `created` would never place that window.
    // SAFETY: reading our own manager slot.
    let mgr = unsafe { MANAGER_CH };
    if mgr == 0 {
        srv.mgr_outbox.clear();
    } else {
        while let Some(ev) = srv.mgr_outbox.front() {
            if !send_mgr_event(mgr, ev) {
                break;
            }
            srv.mgr_outbox.pop();
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
        // The owner stops claiming it, whoever removed it: a manager's `Manage::Close` or a
        // transitive destroy bypasses the client's inline prune. See [`Connection::disown`].
        for conn in srv.conns.iter_mut() {
            conn.disown(window);
        }
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
    // SAFETY: reading our own manager slot.
    if unsafe { MANAGER_CH } == 0 {
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

fn send_mgr_event(ch: u64, ev: &MgrEvent) -> bool {
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

fn send_outbound(ch: u64, rec: &Outbound) -> bool {
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
                    kprint(b"compositor: a drop would not serialise\n");
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
fn connect_input(root_ns: u64) -> Option<u64> {
    use libkern::handle::{RawHandle, Rights};
    use libos::{Handle, Namespace, NsReadOnly, Only, Resource, block_on};

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run; `borrow`
    // never closes it.
    let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: `/dev/input/new` resolves to a channel endpoint — the input server mints one
    // consumer per resolve, exactly as `/dev/draw/new` mints one session.
    let ch =
        block_on(unsafe { ns.lookup::<Resource, Only>("/dev/input/new", Rights::RECV | Rights::SEND | Rights::WAIT) })
            .ok()?;
    Some(ch.into_raw().0)
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
        // SAFETY: reading our own manager slot.
        if unsafe { MANAGER_CH } == 0 {
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
    #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
    for slot in 0..MAX_SESSIONS {
        for _ in 0..SESSION_QUEUE_DEPTH {
            // SAFETY: reading our own slot table.
            if unsafe { SESSION_CH[slot] } == 0 {
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
    // Each batch is copied out because `RECV_MSG` is shared by every receive.
    let mut batches: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    while batches.len() < INPUT_DRAIN_MAX {
        // SAFETY: valid recv out-params (an events batch carries no transferred handles).
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                srv.input_ch,
                (&raw mut RECV_MSG) as u64,
                (&raw mut RECV_HANDLES) as u64,
                (&raw mut RECV_COUNT) as u64,
            )
        };
        if rr != 0 {
            // The ordinary end of the drain. `PeerClosed` means the input server is gone and the
            // handle must leave the wait set or the loop spins; anything else is spurious.
            alive = rr != KError::PeerClosed.as_i32() as i64;
            break;
        }
        // SAFETY: bounded read of the payload the kernel just wrote.
        let payload = unsafe {
            let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            core::slice::from_raw_parts(&raw const RECV_MSG[PAYLOAD_OFF], payload_len.min(MSG_LEN - PAYLOAD_OFF))
        };
        batches.push(payload.to_vec());
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
    // SAFETY: the input-diagnostics counter below is a `static mut` touched only from the serve
    // loop of this single-threaded server.
    unsafe {
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
                // Inside the enclosing `unsafe` block: single-threaded, touched only from the serve
                // loop.
                let logged = INPUT_DIAGS_LOGGED;
                if diag && logged < MAX_LOGGED_INPUT_DIAGS {
                    INPUT_DIAGS_LOGGED = logged + 1;
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

fn reply_window_info(serve_end: u64, request_id: u64, srv: &Server, id: u32) -> bool {
    let Some(info) = srv.stack.info(id) else {
        return reply_resolve_error(serve_end, request_id, KError::NotFound);
    };
    let mut bytes = [0u8; librsproto::surface::WINDOW_INFO_LEN];
    if info.write(&mut bytes).is_none() {
        return reply_resolve_error(serve_end, request_id, KError::KernelError);
    }
    reply_with_object(serve_end, request_id, &bytes)
}

fn reply_screen_info(serve_end: u64, request_id: u64, srv: &Server) -> bool {
    let info = librsproto::surface::ScreenInfo { width: srv.screen.size.w, height: srv.screen.size.h };
    let mut bytes = [0u8; librsproto::surface::SCREEN_INFO_LEN];
    if info.write(&mut bytes).is_none() {
        return reply_resolve_error(serve_end, request_id, KError::KernelError);
    }
    reply_with_object(serve_end, request_id, &bytes)
}

fn reply_with_object(serve_end: u64, request_id: u64, bytes: &[u8]) -> bool {
    // SAFETY: a plain anonymous object of `bytes.len()`.
    let obj = unsafe { syscall4(SYS_MEMORY_CREATE, bytes.len() as u64, 0, 0, 0) };
    if obj <= 0 {
        return reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
    }
    // SAFETY: mapping an object this process just created, to fill it.
    let addr = unsafe {
        syscall4(SYS_MEMORY_MAP, obj as u64, 0, bytes.len() as u64, libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE)
    };
    if addr <= 0 {
        // SAFETY: closing the object we just made and cannot use.
        unsafe { syscall4(SYS_HANDLE_CLOSE, obj as u64, 0, 0, 0) };
        return reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
    }
    // SAFETY: `addr` maps at least `bytes.len()` writable bytes.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
    // Unmap before replying: the mapping holds a reference that would pin the frames, and any
    // client can resolve `info` in a loop.
    // SAFETY: unmapping a range this process mapped moments ago and never reads again.
    unsafe { syscall2(libkern::SYS_MEMORY_UNMAP, addr as u64, bytes.len() as u64) };

    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    if resolve_reply(&mut body, librsproto::namespace::OBJECT_KIND_MEMOBJ, bytes.len() as u32).is_none() {
        // SAFETY: nothing was sent, so the object is still ours to drop.
        unsafe { syscall4(SYS_HANDLE_CLOSE, obj as u64, 0, 0, 0) };
        return reply_resolve_error(serve_end, request_id, KError::KernelError);
    }
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid; the object rides the reply.
    let sent = unsafe {
        let Some(rs_len) = encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1)
        else {
            // SAFETY: nothing was sent, so the object is still ours to drop.
            syscall4(SYS_HANDLE_CLOSE, obj as u64, 0, 0, 0);
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = obj as u64;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        ) == 0
    };
    if !sent {
        // A failed send leaves the transfers untaken, so the object is still ours.
        // SAFETY: closing a handle this process still owns.
        unsafe { syscall4(SYS_HANDLE_CLOSE, obj as u64, 0, 0, 0) };
    }
    sent
}

fn open_session(serve_end: u64, request_id: u64, srv: &mut Server) -> bool {
    // SAFETY: single-threaded server scanning its own slot table.
    let Some(slot) = (unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == 0) }) else {
        return reply_resolve_error(serve_end, request_id, KError::OutOfHandles);
    };
    let Some((client_end, server_end)) = make_channel(SESSION_QUEUE_DEPTH) else {
        return reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
    };
    // SAFETY: `slot` is free; recording our end and a fresh connection for it.
    unsafe {
        SESSION_CH[slot] = server_end;
    }
    srv.conns[slot] = Connection::new();
    if !reply_session(serve_end, request_id, client_end) {
        // SAFETY: the reply failed, so the client never received its end; drop both.
        unsafe {
            syscall4(SYS_HANDLE_CLOSE, client_end, 0, 0, 0);
            syscall4(SYS_HANDLE_CLOSE, server_end, 0, 0, 0);
            SESSION_CH[slot] = 0;
        }
        return false;
    }
    true
}

/// Queue a `Configure` for `window`'s client. `false` if there is no such session.
///
/// Queued, not sent: a briefly full ring would otherwise drop it, unlogged.
fn configure_window(srv: &mut Server, window: u32, width: u32, height: u32, origin: Point) -> bool {
    let Some(slot) = srv.session_of(window) else { return false };
    // SAFETY: reading our own slot table.
    if unsafe { SESSION_CH[slot] } == 0 {
        return false;
    }
    enqueue(srv, slot, Outbound::Configure { window, width, height, x: origin.x, y: origin.y });
    true
}

fn do_capture(srv: &mut Server, ch: u64, request_id: u64, body: &[u8], obj: u64) -> bool {
    let Some(req) = MgrCapture::read(body) else {
        return reply_error_on_session(ch, OP_MGR_CAPTURE, request_id, KError::InvalidArgument);
    };
    if obj == 0 || req.width == 0 || req.height == 0 || req.pitch == 0 {
        return reply_error_on_session(ch, OP_MGR_CAPTURE, request_id, KError::InvalidArgument);
    }
    let Some(w) = srv.stack.window(req.window) else {
        return reply_error_on_session(ch, OP_MGR_CAPTURE, request_id, KError::NotFound);
    };
    // The committed buffer, as compositing reads, so a thumbnail never shows a frame the screen did
    // not.
    let (Some(buffer_id), id) = (w.committed, w.id) else {
        return reply_error_on_session(ch, OP_MGR_CAPTURE, request_id, KError::WouldBlock);
    };
    let Some(b) = w.buffers.iter().find(|b| b.id == buffer_id).map(|b| b.geometry) else {
        return reply_error_on_session(ch, OP_MGR_CAPTURE, request_id, KError::WouldBlock);
    };
    let dst_geom = match libdraw::framebuffer::Geometry::with_pitch(
        req.width,
        req.height,
        req.pitch as usize,
        libdraw::format::PixelFormat::XRGB8888,
    ) {
        Some(g) => g,
        None => {
            return reply_error_on_session(ch, OP_MGR_CAPTURE, request_id, KError::InvalidArgument);
        }
    };
    let len = dst_geom.byte_len();
    // SAFETY: mapping an object the manager sent, writable, for exactly its own length.
    let addr =
        unsafe { syscall4(SYS_MEMORY_MAP, obj, 0, len as u64, libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE) };
    if addr <= 0 {
        return reply_error_on_session(ch, OP_MGR_CAPTURE, request_id, KError::InvalidArgument);
    }
    // SAFETY: `addr` maps `len` writable bytes, which is what `box_downscale` writes.
    let dst = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len) };
    let ok = match srv.pixels(id, buffer_id) {
        Some(src) => libdraw::scale::box_downscale(src, b, dst, dst_geom),
        None => false,
    };
    // SAFETY: unmapping what was just mapped, at the same length.
    unsafe { syscall4(SYS_MEMORY_UNMAP, addr as u64, len as u64, 0, 0) };
    if !ok {
        return reply_error_on_session(ch, OP_MGR_CAPTURE, request_id, KError::InvalidArgument);
    }
    Line::new()
        .s(b"compositor: captured window ")
        .u(id as u64)
        .s(b" into ")
        .u(req.width as u64)
        .s(b"x")
        .u(req.height as u64)
        .end();
    reply_on_session(ch, OP_MGR_CAPTURE, request_id, &[])
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
    // SAFETY: reading our own manager slot and valid recv out-params.
    let ch = unsafe { MANAGER_CH };
    // SAFETY: the recv buffers are this process's statics, only touched from this loop, and the
    // kernel fills them on return
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr != 0 {
        return if rr == KError::PeerClosed.as_i32() as i64 { Served::Closed } else { Served::Empty };
    }
    if handle_manager_request(ch, srv, screen) { Served::One } else { Served::Closed }
}

fn handle_manager_request(ch: u64, srv: &mut Server, screen: &mut Screen<RawFramebuffer>) -> bool {
    // SAFETY: bounded read-only slice over the just-received message.
    let (op, request_id, body, carried) = unsafe {
        // Close every transfer except the one `Capture` needs: unclosed, each pins a slot in the
        // global handle table.
        let hcount = ((&raw const RECV_COUNT).read() as usize).min(libkern::abi::IPC_HANDLE_MAX);
        let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        let decoded = decode(req);
        let takes_handle = matches!(&decoded, Ok(m) if m.op == OP_MGR_CAPTURE);
        let mut carried = 0u64;
        #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
        for i in 0..hcount {
            if i == 0 && takes_handle {
                carried = RECV_HANDLES[0];
                continue;
            }
            syscall4(SYS_HANDLE_CLOSE, RECV_HANDLES[i], 0, 0, 0);
        }
        match decoded {
            Ok(m) => (m.op, m.request_id, m.body.to_vec(), carried),
            Err(_) => {
                if carried != 0 {
                    syscall4(SYS_HANDLE_CLOSE, carried, 0, 0, 0);
                }
                return true;
            }
        }
    };
    if op == OP_MGR_CAPTURE {
        // Handled here, not in `manager::dispatch`, which sees only the stack: this needs the
        // handle, the pixel source and syscalls to map with.
        let ok = do_capture(srv, ch, request_id, &body, carried);
        if carried != 0 {
            // SAFETY: the object was installed in this process's table by the recv above; the
            // capture has finished with it either way
            unsafe { syscall4(SYS_HANDLE_CLOSE, carried, 0, 0, 0) };
        }
        return ok;
    }
    if op == librsproto::surface::OP_MGR_REQUEST_CLOSE {
        // Handled here: asking a client to close puts a record in that client's outbox, which the
        // server owns. The window itself is untouched. `check-login` waits on this line.
        let Some(req) = librsproto::surface::MgrWindowRef::read(&body) else {
            return reply_error_on_session(ch, op, request_id, KError::InvalidArgument);
        };
        let Some(slot) = srv.session_of(req.window) else {
            return reply_error_on_session(ch, op, request_id, KError::NotFound);
        };
        enqueue(srv, slot, Outbound::CloseRequested { window: req.window });
        Line::new().s(b"compositor: asked window ").u(req.window as u64).s(b" to close").end();
        return reply_on_session(ch, op, request_id, &[]);
    }
    if op == librsproto::surface::OP_MGR_QUERY_LAYOUT {
        let layout = current_layout(srv);
        let mut body = [0u8; core::mem::size_of::<librsproto::surface::MgrLayout>()];
        return match layout.write(&mut body) {
            Some(n) => reply_on_session(ch, op, request_id, &body[..n]),
            None => reply_error_on_session(ch, op, request_id, KError::InvalidArgument),
        };
    }
    let mgr_outcome = manager::dispatch(&mut srv.stack, op, &body);
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
            reply_on_session(ch, op, request_id, &[]);
        }
        MgrOutcome::State { window, state } => {
            // To the client only: the compositor keeps no notion of a window's state.
            let Some(slot) = srv.session_of(window) else {
                return reply_error_on_session(ch, op, request_id, KError::NotFound);
            };
            let rec = Outbound::WindowState { window, state };
            log_route(&rec);
            enqueue(srv, slot, rec);
            reply_on_session(ch, op, request_id, &[]);
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
                    reply_on_session(ch, op, request_id, &[]);
                }
                Err(_) => {
                    kprint(b"compositor: a snap zone registration was refused\n");
                    reply_error_on_session(ch, op, request_id, KError::InvalidArgument);
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
                    reply_on_session(ch, op, request_id, &[]);
                }
                Err(_) => {
                    // `InvalidArgument` for all three, as the spec publishes: a full table never
                    // empties, so `WouldBlock` would make a manager retry forever.
                    kprint(b"compositor: a hotkey registration was refused\n");
                    reply_error_on_session(ch, op, request_id, KError::InvalidArgument);
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
                    return reply_error_on_session(ch, op, request_id, KError::WouldBlock);
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
                reply_on_session(ch, op, request_id, &[]);
            } else {
                // No session owns that window, so nobody can be told: a refusal, not a hollow
                // success.
                reply_error_on_session(ch, op, request_id, surface_errno(SurfaceError::NotFound));
            }
        }
        MgrOutcome::Failed(e) => {
            reply_error_on_session(ch, op, request_id, surface_errno(e));
        }
    }
    true
}

/// Mint the manager channel, or refuse because one is already attached.
fn open_manager(serve_end: u64, request_id: u64) -> bool {
    // SAFETY: single-threaded server reading its own state.
    if unsafe { MANAGER_CH } != 0 {
        // Refused, not replaced: a second manager would silently depose the first.
        kprint(b"compositor: a second manager was refused\n");
        return reply_resolve_error(serve_end, request_id, KError::WouldBlock);
    }
    let Some((client_end, server_end)) = make_channel(SESSION_QUEUE_DEPTH) else {
        return reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
    };
    if !reply_session(serve_end, request_id, client_end) {
        // SAFETY: the reply failed, so the manager never received its end; drop both.
        unsafe {
            syscall4(SYS_HANDLE_CLOSE, client_end, 0, 0, 0);
            syscall4(SYS_HANDLE_CLOSE, server_end, 0, 0, 0);
        }
        return false;
    }
    // SAFETY: recording our end of the channel we just handed out.
    unsafe { MANAGER_CH = server_end };
    kprint(b"compositor: a manager attached\n");
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
        kprint(b"compositor: scheme light (the manager that named another went away)\n");
    }
    // Every window it was going to place is shown now: those clients are blocked, and waiting out a
    // timer for a manager that has gone is latency for nothing.
    let waiting: alloc::vec::Vec<u32> = srv.pending_configure.iter().map(|&(w, _)| w).collect();
    for window in waiting {
        release_configure(srv, screen, window);
    }
    // SAFETY: closing our own endpoint and clearing the slot.
    unsafe {
        if MANAGER_CH != 0 {
            syscall4(SYS_HANDLE_CLOSE, MANAGER_CH, 0, 0, 0);
            MANAGER_CH = 0;
        }
    }
    kprint(b"compositor: the manager went away\n");
}

fn close_session(slot: usize, srv: &mut Server) {
    // SAFETY: closing our own endpoint and clearing the slot.
    unsafe {
        if SESSION_CH[slot] != 0 {
            syscall4(SYS_HANDLE_CLOSE, SESSION_CH[slot], 0, 0, 0);
            SESSION_CH[slot] = 0;
        }
    }
    srv.outbox[slot].clear();
    disconnect(&mut srv.conns[slot], &mut srv.stack);
    drain_stack_events(srv);
    srv.buffers.retain(|b| srv.stack.window(b.window).is_some());
    announce_focus(srv);
}

/// Map a client's transferred `MemoryObject` and record it for `BufferSource`, once at attach.
/// Always consumes `handle`, on every path.
fn map_attached_buffer(srv: &mut Server, body: &[u8], handle: u64) -> bool {
    let Some(req) = librsproto::surface::parse_attach_buffer_request(body) else {
        // SAFETY: we are not mapping it, so drop it rather than leaking the object.
        unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        return false;
    };
    let len = req.pitch as usize * req.height as usize;
    // The replaced mapping goes first: `dispatch` accepted the new geometry, and a failed map must
    // leave no pixels, not the wrong ones.
    srv.buffers.retain(|b| !(b.window == req.window && b.buffer == req.buffer));
    // SAFETY: `handle` is a `MemoryObject` the client transferred; mapping it read-only
    // with the size its own geometry declares.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, handle, 0, len as u64, libkern::RIGHT_MAP_READ) };
    if addr <= 0 {
        // SAFETY: the map failed, so nothing references the object; drop our handle rather
        // than leaking it for the compositor's life.
        unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        return false;
    }
    srv.buffers.push(MappedBuffer { window: req.window, buffer: req.buffer, addr: addr as *mut u8, len });
    // SAFETY: closing our reference; the mapping outlives it.
    unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
    true
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

fn reply_error_on_session(session: u64, op: u16, request_id: u64, err: KError) -> bool {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = librsproto::error::error_body(&mut ebody, err.as_i32(), 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer; no handles transferred.
    unsafe {
        let Some(rs_len) =
            encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &ebody[..elen], 0)
        else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            session,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

fn serve_session(slot: usize, srv: &mut Server, screen: &mut Screen<RawFramebuffer>) -> Served {
    // SAFETY: SESSION_CH is only written by this single-threaded loop, and `slot` indexes it in bounds
    let ch = unsafe { SESSION_CH[slot] };
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr != 0 {
        return if rr == KError::PeerClosed.as_i32() as i64 { Served::Closed } else { Served::Empty };
    }
    if handle_session_request(slot, ch, srv, screen) { Served::One } else { Served::Closed }
}

fn handle_session_request(slot: usize, ch: u64, srv: &mut Server, screen: &mut Screen<RawFramebuffer>) -> bool {
    // Copied out to avoid two live `static mut` borrows. `MAX_BODY` fits a whole `SetTitle`, so
    // this `min` is a bounds check, not a policy.
    let mut body_buf = [0u8; MAX_BODY];
    // SAFETY: RECV_MSG and RECV_HANDLES were filled by the recv in the caller; the slice is clamped to the payload
    // region and is copied out before anything else touches the statics
    let (op, request_id, body_len, handle) = unsafe {
        let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(&raw const RECV_MSG[PAYLOAD_OFF], payload_len.min(MSG_LEN - PAYLOAD_OFF));
        // Take the handles out before anything can fail: a message that does not decode may still
        // carry some, and the surplus past the first is closed at once.
        let hcount = ((&raw const RECV_COUNT).read() as usize).min(libkern::abi::IPC_HANDLE_MAX);
        #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
        for i in 1..hcount {
            // SAFETY: closing a handle this process owns and no op can name.
            syscall4(SYS_HANDLE_CLOSE, RECV_HANDLES[i], 0, 0, 0);
        }
        let h = if hcount > 0 { RECV_HANDLES[0] } else { 0 };
        let Ok(m) = decode(req) else {
            if h != 0 {
                // SAFETY: closing a handle this process owns and will never interpret.
                syscall4(SYS_HANDLE_CLOSE, h, 0, 0, 0);
            }
            return true;
        };
        let n = m.body.len().min(MAX_BODY);
        body_buf[..n].copy_from_slice(&m.body[..n]);
        (m.op, m.request_id, n, h)
    };
    let body = &body_buf[..body_len];

    // `StartMove` is answered here as `dispatch` sees no router, which owns the grab this is
    // checked against. A foreign window answers `NotFound`.
    if op == librsproto::surface::OP_START_MOVE {
        if handle != 0 {
            // SAFETY: closing a handle this process owns and will never interpret.
            unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        }
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
            Some(e) => reply_error_on_session(ch, op, request_id, e),
            None => reply_on_session(ch, op, request_id, &[]),
        };
    }

    // `StartResize` is answered here for the reason `StartMove` is. It resizes nothing: an outline
    // follows the pointer and the manager hears one event at the release.
    if op == librsproto::surface::OP_START_RESIZE {
        if handle != 0 {
            // SAFETY: closing a handle this process owns and will never interpret.
            unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        }
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
            Some(e) => reply_error_on_session(ch, op, request_id, e),
            None => reply_on_session(ch, op, request_id, &[]),
        };
    }

    // A window says what it takes, once. The compositor holds the table so "would this window take
    // this drag?" is answered per motion without a round trip.
    if op == librsproto::surface::OP_DECLARE_ACCEPTOR {
        if handle != 0 {
            // SAFETY: closing a handle this process owns and will never interpret.
            unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        }
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
            Some(e) => reply_error_on_session(ch, op, request_id, e),
            None => reply_on_session(ch, op, request_id, &[]),
        };
    }

    // A drag is a gesture the compositor runs, refused like a move and a resize without the grab;
    // it ends in a payload for the window it was let go over.
    if op == librsproto::surface::OP_START_DRAG {
        if handle != 0 {
            // SAFETY: closing a handle this process owns and will never interpret.
            unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        }
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
            Some(e) => reply_error_on_session(ch, op, request_id, e),
            None => reply_on_session(ch, op, request_id, &[]),
        };
    }

    // `RequestState` is forwarded, not applied: minimising and maximising are manager operations,
    // so the compositor only checks ownership and hands the manager the question.
    if op == librsproto::surface::OP_REQUEST_STATE {
        if handle != 0 {
            // SAFETY: closing a handle this process owns and will never interpret.
            unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        }
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
            Some(e) => reply_error_on_session(ch, op, request_id, e),
            None => reply_on_session(ch, op, request_id, &[]),
        };
    }

    // SAFETY: a bounded mutable slice over the reply buffer; `body` is a local copy.
    let outcome =
        unsafe { dispatch(&mut srv.conns[slot], &mut srv.stack, op, body, &mut REPLY_MSG[PAYLOAD_OFF + 16..]) };
    drain_stack_events(srv);

    // Map only after `dispatch` accepted the attach, or a rejected one would record the client's
    // memory under ids it does not own. The handle is disposed of exactly once.
    let mut consumed = false;
    if op == OP_ATTACH_BUFFER && handle != 0 && matches!(outcome, Outcome::Applied { .. }) {
        consumed = true;
        if !map_attached_buffer(srv, body, handle) {
            return true;
        }
    }
    if handle != 0 && !consumed {
        // SAFETY: closing a handle this process owns and will not use.
        unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
    }

    match outcome {
        Outcome::Reply(len) => {
            let mut body = [0u8; MAX_BODY];
            let n = len.min(MAX_BODY);
            // SAFETY: `dispatch` wrote `len` bytes at this offset in REPLY_MSG, and `n` is clamped to MAX_BODY; copy
            // them out before `reply_on_session` re-encodes into the same buffer
            unsafe {
                body[..n].copy_from_slice(&REPLY_MSG[PAYLOAD_OFF + 16..PAYLOAD_OFF + 16 + n]);
            }
            if !reply_on_session(ch, op, request_id, &body[..n]) {
                kprint(b"compositor: DROPPED a reply (client receive ring full)\n");
            }
        }
        Outcome::Created { reply_len, configure } => {
            // The reply first, then the configure, on the same channel: the client reads its id and
            // then waits for the configure before it may commit.
            let mut body = [0u8; MAX_BODY];
            let n = reply_len.min(MAX_BODY);
            // SAFETY: `dispatch` wrote `reply_len` bytes at this offset in REPLY_MSG, and `n` is clamped to MAX_BODY;
            // copy them out before `reply_on_session` re-encodes into the same buffer
            unsafe {
                body[..n].copy_from_slice(&REPLY_MSG[PAYLOAD_OFF + 16..PAYLOAD_OFF + 16 + n]);
            }
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
            let sent = reply_on_session(ch, op, request_id, &body[..n]);
            if !sent {
                kprint(b"compositor: a create reply did not send\n");
            }
            // Held when a manager is attached, so it can place the window before anyone sees it. A
            // popup is never held: its creator places it, and every menu open would cost the
            // deadline. A `dialog` is held like a `normal`.
            let placed_by_creator = srv
                .stack
                .window(configure.window)
                .is_some_and(|w| matches!(w.role, librsproto::surface::Role::Popup { .. }));
            // SAFETY: reading our own manager slot.
            if sent && !placed_by_creator && unsafe { MANAGER_CH } != 0 {
                let mut now: u64 = 0;
                // SAFETY: a valid out-pointer for one u64.
                unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
                srv.pending_configure.push((configure.window, now.saturating_add(CONFIGURE_DEADLINE_NS)));
            } else {
                let mut cfg = [0u8; core::mem::size_of::<ConfigureEvent>()];
                srv.stack.mark_configured(configure.window);
                if sent && configure.write(&mut cfg).is_some() && !send_input(ch, OP_CONFIGURE, &cfg) {
                    // The client waits for this and nothing else produces it; a silent failure
                    // parks it forever.
                    kprint(b"compositor: a window's first configure did not send\n");
                }
            }
        }
        Outcome::Applied { release, dirty } => {
            // A destroy is transitive, so drop every mapping whose window is gone, or a looping
            // client grows the compositor's address space.
            srv.buffers.retain(|b| srv.stack.window(b.window).is_some());

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
                kprint(match e {
                    SurfaceError::Malformed => b"compositor: malformed request\n" as &[u8],
                    SurfaceError::Unsupported => b"compositor: unsupported op\n",
                    SurfaceError::NotFound => b"compositor: no such window for this connection\n",
                    SurfaceError::Rejected(_) => b"compositor: request rejected\n",
                });
                if n == MAX_LOGGED_REJECTIONS {
                    kprint(b"compositor: (further rejections not logged)\n");
                }
            }
            reply_error_on_session(ch, op, request_id, surface_errno(e));
        }
    }

    // After the request, whatever it was: `announce_focus` compares with the last announcement, so
    // no list of focus-moving ops need stay correct (create replies without reaching `Applied`).
    // After the reply, as a client cannot use focus for an id it lacks.
    announce_focus(srv);
    true
}

fn serve_loop(serve_end: u64, mut screen: Screen<RawFramebuffer>, srv: &mut Server, mut control: u64) -> ! {
    kprint(b"compositor: serving /dev/draw\n");
    let mut parked = false;
    loop {
        // Every iteration, not only on a timeout, or the configure deadline holds only when idle.
        // Flushed here: a released configure is queued and the client is blocked on it.
        if fire_configure_deadlines(srv, &mut screen) {
            parked = flush_outboxes(srv);
        }
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots; `n` is bounded by
        // `4 + MAX_SESSIONS` — `serve_end`, the control channel until its supervisor has gone, the
        // input channel when connected, the manager channel when one is attached, then the
        // sessions — which the `const _` beside `MAX_SESSIONS` holds to that limit.
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
            if control != 0 {
                WAIT_HANDLES[n] = control;
                n += 1;
            }
            if srv.input_ch != 0 {
                WAIT_HANDLES[n] = srv.input_ch;
                n += 1;
            }
            if MANAGER_CH != 0 {
                WAIT_HANDLES[n] = MANAGER_CH;
                n += 1;
            }
            #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
            for i in 0..MAX_SESSIONS {
                if SESSION_CH[i] != 0 {
                    WAIT_HANDLES[n] = SESSION_CH[i];
                    n += 1;
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
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, n as u64, (&raw mut WAIT_RESULTS) as u64, deadline)
        };
        if waited < 1 {
            fire_repeat(srv);
            parked = flush_outboxes(srv);
            continue;
        }

        let mut serve_signalled = false;
        for j in 0..(waited as usize) {
            let off = j * 24;
            // SAFETY: `waited` records were written; `off + 8` stays inside WAIT_RESULTS.
            let h = unsafe {
                u64::from_le_bytes([
                    WAIT_RESULTS[off],
                    WAIT_RESULTS[off + 1],
                    WAIT_RESULTS[off + 2],
                    WAIT_RESULTS[off + 3],
                    WAIT_RESULTS[off + 4],
                    WAIT_RESULTS[off + 5],
                    WAIT_RESULTS[off + 6],
                    WAIT_RESULTS[off + 7],
                ])
            };
            if h == serve_end {
                serve_signalled = true;
                continue;
            }
            // `service --stop`: every window goes with this process and its clients find their
            // channels closed; a restarted compositor takes the screen again.
            if control != 0 && h == control {
                match libkern::control::recv(control) {
                    libkern::control::Control::Op(libkern::abi::CTRL_OP_SHUTDOWN) => {
                        kprint(b"compositor: asked to stop, exiting\n");
                        exit(0);
                    }
                    libkern::control::Control::Closed => control = 0,
                    _ => {}
                }
                continue;
            }
            if srv.input_ch != 0 && h == srv.input_ch && !serve_input(srv, &mut screen) {
                // The input server died: carry on serving the display rather than exit because a
                // mouse went away.
                kprint(b"compositor: input server gone\n");
                // SAFETY: closing an endpoint this process owns and stops using.
                unsafe { syscall4(libkern::SYS_HANDLE_CLOSE, srv.input_ch, 0, 0, 0) };
                srv.input_ch = 0;
                continue;
            }
            // SAFETY: reading our own manager slot.
            if unsafe { MANAGER_CH } != 0 && h == unsafe { MANAGER_CH } {
                if serve_manager(srv, &mut screen) == Served::Closed {
                    close_manager(srv, &mut screen);
                }
                continue;
            }
            // SAFETY: scanning our own slot table for the signalled endpoint.
            if let Some(slot) = unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == h) }
                && serve_session(slot, srv, &mut screen) == Served::Closed
            {
                close_session(slot, srv);
                repaint(srv, &mut screen);
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
        // SAFETY: valid recv out-params (a Resolve carries no transferred handles).
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                serve_end,
                (&raw mut RECV_MSG) as u64,
                (&raw mut RECV_HANDLES) as u64,
                (&raw mut RECV_COUNT) as u64,
            )
        };
        if rr != 0 {
            continue;
        }
        // SAFETY: bounded read of the payload, then of the resolve's suffix.
        let (request_id, resolved, ok) = unsafe {
            let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let req =
                core::slice::from_raw_parts(&raw const RECV_MSG[PAYLOAD_OFF], payload_len.min(MSG_LEN - PAYLOAD_OFF));
            match decode(req) {
                Ok(m) if m.op == OP_NS_RESOLVE => {
                    let suffix = librsproto::namespace::parse_resolve_request(m.body).map(|r| r.suffix).unwrap_or(b"");
                    (m.request_id, classify(suffix), true)
                }
                Ok(m) => (m.request_id, Resolved::Unknown, true),
                Err(_) => (0, Resolved::Unknown, false),
            }
        };
        if !ok {
            continue;
        }
        match resolved {
            Resolved::New => {
                open_session(serve_end, request_id, srv);
            }
            Resolved::Manage => {
                open_manager(serve_end, request_id);
            }
            Resolved::Info(id) => {
                reply_window_info(serve_end, request_id, srv, id);
            }
            Resolved::Screen => {
                reply_screen_info(serve_end, request_id, srv);
            }
            Resolved::Unknown => {
                reply_resolve_error(serve_end, request_id, KError::NotFound);
            }
        }
    }
}

/// Called by the kernel's ELF entry; `ctrl` is the control channel the supervisor spawned us with.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, ctrl: u64) -> ! {
    kprint(b"compositor: up\n");

    // The shadow buffer is allocated before the screen changes hands: acquiring `/dev/framebuffer`
    // stops the kernel console drawing, so a later refusal would be unreadable on a machine with no
    // serial port (`check-fbcon`). Reading the geometry does not take the screen; `acquire` does.
    //
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let shadow =
        match unsafe { libdraw::acquire::read_info(root_ns) }.and_then(|info| libdraw::acquire::geometry_from(&info)) {
            Ok(geometry) => match libdraw::framebuffer::MemFramebuffer::try_new(geometry) {
                Some(shadow) => shadow,
                None => {
                    kprint(b"compositor: no memory for a shadow buffer -- cannot serve\n");
                    exit(1);
                }
            },
            Err(_) => {
                kprint(b"compositor: cannot read the framebuffer's shape -- cannot serve\n");
                exit(1);
            }
        };

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let (aperture, info) = match unsafe { libdraw::acquire::acquire(root_ns) } {
        Ok(pair) => pair,
        Err(_) => {
            kprint(b"compositor: no /dev/framebuffer -- cannot serve\n");
            exit(1);
        }
    };
    let _ = info;
    // From here the aperture is reachable only through `screen`: frames are composed in the shadow
    // buffer and copied out, so nothing is scanned out half-painted. There is no serving without
    // it: composing into the write-combining framebuffer reads it back uncached, a second a frame.
    let mut screen = Screen::with_shadow(aperture, shadow);

    let Some((kernel_end, serve_end)) = make_channel(SESSION_QUEUE_DEPTH) else {
        kprint(b"compositor: channel create FAIL\n");
        exit(1);
    };
    let screen_bounds = screen.geometry().bounds();
    let mut srv = Server {
        stack: WindowStack::new(),
        conns: core::array::from_fn(|_| Connection::new()),
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
        input_ch: 0,
        lights_sent: None,
    };
    match connect_input(root_ns) {
        Some(ch) => {
            srv.input_ch = ch;
            // `check-input` waits on this line.
            kprint(b"compositor: input connected\n");
            send_lights(&mut srv);
        }
        None => kprint(b"compositor: no /dev/input/new -- display only\n"),
    }

    // Clear before announcing readiness, so `Meta::Ready` means the screen is taken: announcing
    // first left the clear racing the next spawn, and on `-smp 4` two processes held
    // `/dev/framebuffer` mapped read-write. The pointer is drawn here too, so it exists before the
    // mouse first moves.
    repaint(&srv, &mut screen);

    if !send_ready(ctrl, kernel_end) {
        kprint(b"compositor: Ready send FAIL\n");
        exit(1);
    }

    serve_loop(serve_end, screen, &mut srv, ctrl);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"compositor: panic\n");
    exit(2);
}
