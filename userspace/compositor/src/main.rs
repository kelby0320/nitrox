//! `compositor` — the resource server behind `/dev/draw` (plan M2 Part B).
//!
//! **Everything that decides is in the library half** ([`compositor`]): the window stack,
//! roles and struts, request dispatch, and compositing, all host-tested without a kernel.
//! This file is the parts that cannot be: the IPC loop, mapping clients' transferred
//! `MemoryObject`s, and driving the framebuffer.
//!
//! ## Shape
//!
//! Modelled on `profile-server`, which serves `/bin` the same way:
//!
//! 1. Acquire `/dev/framebuffer` — **authority is the binding**, so a compositor is simply
//!    a process whose namespace contains it (`display-substrate.md` §3).
//! 2. Mint a forwarding channel pair; send `Meta::Ready` on the control channel
//!    transferring the kernel end, which the supervisor binds at `/dev/draw`.
//! 3. Serve. A forwarded resolve of `new` mints a **session channel**: the compositor
//!    keeps the server end and hands the client end back as the resolve's answer. There is
//!    no distinct "connection" object — a connection *is* a channel, which is what makes
//!    the per-connection ownership rule enforceable: a request's identity is the endpoint
//!    it arrived on.
//!
//! ## The one genuinely new mechanism
//!
//! `AttachBuffer` carries a `MemoryObject` handle in the message's transfer slot. The
//! compositor maps it **once** and keeps the mapping for the buffer's life — that is the
//! whole point of the design (`display-substrate.md` §4: the handle is transferred once,
//! not per frame). Everything before this milestone was logic testable on the host; this
//! part can only be proven by booting.

#![no_std]
#![no_main]

extern crate alloc;

use compositor::input::InputRouter;
use compositor::outbox::{Outbound, Outbox};
use compositor::manager::{self, MgrOutcome};
use compositor::server::{Connection, Outcome, SurfaceError, disconnect, dispatch};
use compositor::{BufferSource, WindowStack};
use libdraw::format::Rgb;
use libdraw::framebuffer::{Framebuffer, RawFramebuffer};
use libdraw::geom::{Point, Rect};
use libinput::Interpreter;
use libkern::abi::{INPUT_EVENT_LEN, InputEvent};
use libkern::abi::CLOCK_MONOTONIC;
use libkern::{
    SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_CLOCK_READ,
    SYS_HANDLE_CLOSE, SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_WAIT, exit, kprint, syscall2,
    syscall4, syscall5,
};
use libkern::debug::Line;
use libkern::error::KError;
use librsproto::namespace::{OBJECT_KIND_CHANNEL, resolve_reply};
use librsproto::surface::{
    ConfigureEvent, FocusEvent, KeyEvent, OP_ATTACH_BUFFER, OP_CONFIGURE, OP_FOCUS_EVENT,
    OP_KEY_EVENT,
    OP_POINTER_EVENT,
    OP_RELEASE, PointerEvent,
};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

/// `alloc` backing — the window stack and its buffer lists allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC message buffer length.
const MSG_LEN: usize = 4096;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;
/// The background the screen is cleared to where no window covers.
const BACKGROUND: Rgb = Rgb::new(0x0E, 0x14, 0x1B);

/// Largest Surface request body (`AttachBuffer`/`Commit` are 24 bytes).
const MAX_BODY: usize = 64;

/// Two less than the wait limit: the forwarding endpoint takes the first slot and the
/// input-server consumer channel the second.
///
/// It is `- 2` rather than `- 1` because the wait set is built from *all* of them at once.
/// Leaving it at `- 1` would overrun `WAIT_HANDLES` by exactly one entry on the boot where
/// every session slot is in use and input connected — the rarest configuration, and the
/// only one that would ever have shown it.
const MAX_SESSIONS: usize = libkern::abi::MAX_WAIT_HANDLES - 3;

/// The wait-set bound, as a compile error rather than a comment.
///
/// The invariant is now spread across two constants, and the serve loop's `SAFETY` note used
/// to assert `1 + MAX_SESSIONS` — true before the input channel joined the set and quietly
/// false after. Anyone adding a fixed handle should be stopped by the compiler, not by
/// re-reading prose (PR #180 review, finding 4).
///
/// It worked: the manager channel is the **third** fixed handle (M6 Part B), and this line is
/// what said so.
const _: () = assert!(3 + MAX_SESSIONS <= libkern::abi::MAX_WAIT_HANDLES);

/// How many client-driven rejections get logged **per session** before the tap closes.
///
/// Per session, not per process. A single global budget is spent by whichever client
/// misbehaves first and never refills — on a selftest image the churn probe burns all of it
/// in the first second, so on exactly the builds where a rejection would be diagnostic,
/// no later one is ever logged (PR #175 review, finding 6). A session's counter resets when
/// the slot is reused, which is also when the client behind it is a different program.
const MAX_LOGGED_REJECTIONS: u32 = 8;

/// How many routed input events get logged before the tap closes.
///
/// Bounded for the same reason as [`MAX_LOGGED_REJECTIONS`], but more urgently: input is
/// *continuous*. An unbounded line per event turns a moving mouse into an unbroken stream
/// of serial output, which on this machine is a slow synchronous device — the compositor
/// would spend its time printing rather than compositing, and the klog ring would carry
/// nothing but cursor positions.
const MAX_LOGGED_ROUTES: u32 = 8;

/// How many *input diagnostics* — a press and where it landed, a dropped batch — get logged.
///
/// A separate, far larger bound than [`MAX_LOGGED_ROUTES`] because the thing that constant
/// argues about does not apply: these are **edge-triggered**, one line per real click or per
/// genuine loss, not one per event in a continuous stream. A moving mouse produces none of
/// them. But the argument for having *a* bound survives — serial is a slow synchronous device
/// and an hour of desktop clicking should not fill the klog ring with press lines — so this is
/// generous rather than absent: any gate needs a handful, and 256 outlives every one of them.
const MAX_LOGGED_INPUT_DIAGS: u32 = 256;

static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
/// Sized by the **kernel's** limit, not by what this server expects.
///
/// `sys_channel_recv` takes no capacity argument: it copies `handle_count * 8` bytes here,
/// where the count is the sender's and is bounded only by `IPC_HANDLE_MAX`. A `[u64; 4]`
/// therefore let any client with a session overrun this static by 32 bytes into whatever
/// the linker placed next, by sending eight transfers (PR #175 review, finding 1). Every
/// other resource server in the tree already sizes it this way.
static mut RECV_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] =
    [0; libkern::abi::IPC_HANDLE_MAX];
static mut RECV_COUNT: u64 = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] =
    [0; libkern::abi::IPC_HANDLE_MAX];
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
/// The manager's channel, or `0` when nobody is managing.
///
/// **One, not a table.** Two managers placing windows is a race with no arbiter, and the failure
/// looks like windows moving on their own — so a second resolve is refused rather than served.
static mut MANAGER_CH: u64 = 0;
/// Routed input events logged so far — see [`MAX_LOGGED_ROUTES`].
static mut ROUTES_LOGGED: u32 = 0;
/// Input diagnostics logged so far — see [`MAX_LOGGED_INPUT_DIAGS`].
static mut INPUT_DIAGS_LOGGED: u32 = 0;
/// Scratch for `sys_clock_read`.
static mut CLOCK_BUF: u64 = 0;

/// How long to sleep before retrying a parked message, in nanoseconds.
///
/// **A parked message has no wakeup of its own.** A channel endpoint signals when it has
/// something to *read*, so a client draining its receive ring produces no signal here — the
/// compositor would sit in an infinite `sys_wait` holding a message the client is waiting
/// for. For input that is merely late; for a `Release` it is the permanent hang this whole
/// mechanism exists to prevent, and worse than the drop-and-log it replaced, because at
/// least that said something (PR #181 review, finding 1).
///
/// Ten milliseconds because that is the scheduler tick, so a shorter deadline buys nothing.
/// The cost is bounded to exactly the periods when something is parked: with every outbox
/// empty the wait is still infinite and an idle compositor does not wake at all.
const RETRY_INTERVAL_NS: u64 = 10_000_000;

/// How many outbox discards get logged **per session** before the tap closes.
///
/// Bounded for the same reason as [`MAX_LOGGED_REJECTIONS`], and the argument that a client
/// generating these "has a problem worth the lines" is the same one that was made about
/// rejections before a churn probe buried every other service's output. A wedged client with
/// a key held at repeat rate is tens of lines a second on a shared console.
const MAX_LOGGED_OVERFLOWS: u32 = 8;
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] =
    [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] =
    [0; 24 * libkern::abi::MAX_WAIT_HANDLES];

/// A client buffer the compositor has mapped.
struct MappedBuffer {
    window: u32,
    buffer: u32,
    addr: *mut u8,
    len: usize,
}

impl Drop for MappedBuffer {
    /// Unmap when the record goes away — **the record is not the mapping**.
    ///
    /// Both places that shrink `Server::buffers` do it with `Vec::retain`, which drops the
    /// removed records and nothing else. Dropping a `MappedBuffer` used to leave the VMA
    /// behind, so the comment at the destroy site — "otherwise a client looping
    /// create/attach/destroy grows the compositor's address space without bound" — described
    /// an intent the code did not carry out. It leaked on the ordinary application
    /// lifecycle: every window closed, every client that exits.
    ///
    /// Worse than the address space, it pinned the **client's** memory. `map_attached_buffer`
    /// closes its handle immediately and relies on the mapping to hold the object alive, so
    /// a stale mapping meant a client's framebuffer was never freed after that client was
    /// gone — at a real window size, megabytes per window.
    fn drop(&mut self) {
        // SAFETY: `addr`/`len` came from a successful `sys_memory_map` in
        // `map_attached_buffer`; a record is dropped once, so this unmaps once.
        unsafe { syscall2(libkern::SYS_MEMORY_UNMAP, self.addr as u64, self.len as u64) };
    }
}

/// Everything the serve loop owns.
struct Server {
    stack: WindowStack,
    /// Per-session connection state, parallel to `SESSION_CH`.
    conns: [Connection; MAX_SESSIONS],
    /// Mapped client buffers, in attach order.
    buffers: alloc::vec::Vec<MappedBuffer>,
    /// Device triples → logical events. Reset by a `SYN_DROPPED` the server forwards.
    interp: Interpreter,
    /// Cursor, crossing state and the implicit grab.
    router: InputRouter,
    /// The consumer channel from `/dev/input/new`, or 0 if input is unavailable.
    input_ch: u64,
    /// The key currently repeating, if one is held.
    repeat: Option<compositor::Repeat>,
    /// The window last told it has the keyboard, if any.
    ///
    /// Kept so a focus change can be *detected* rather than re-announced: `focus_candidate`
    /// is recomputed after anything that could move it, and most of those do not. Announcing
    /// unconditionally would send a `FocusEvent` on every commit.
    announced_focus: Option<u32>,
    /// Pending messages per session, parallel to `SESSION_CH`.
    ///
    /// On the heap rather than in this struct by value: `Server` lives on `_start`'s stack,
    /// and thirty inline queues would be tens of kilobytes against a 32 KiB user stack —
    /// the same trap `libsurface` documents for holding a transport by value.
    outbox: alloc::vec::Vec<Outbox>,
}

impl Server {
    /// The session slot owning `window`, if any.
    ///
    /// A window's owner is the connection that created it, which is the only connection
    /// allowed to name it — so this is a search of the same table `dispatch` authorises
    /// against, not a second notion of ownership that could disagree with it.
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

/// Receive-ring depth for a client **session** channel.
///
/// Sixteen, which is the kernel's own `IPC_DEFAULT_QUEUE_DEPTH`. The previous value was
/// `4` — not chosen, but a literal copied into every resource server in the tree, and a
/// quarter of the system default by accident rather than by argument.
///
/// **This is 4× the old threshold, not a different kind of bound**, and it is worth being
/// precise about that. Coalescing bounds the *outbox*, not this ring: two motions collapse
/// only while both are queued, and a compositor that flushes every loop iteration sends each
/// one as its own message — for a PS/2 mouse, one per IRQ. What actually removes the cliff is
/// the retry: a refused send parks at the head of the outbox instead of being dropped, so the
/// ring's depth decides how long a stalled client can go before its motion starts coalescing,
/// not whether anything is lost (PR #181 review, finding 4).
///
/// It is not free: a slot is a whole 4 KiB `IpcMsg` whatever the payload, and both endpoints
/// get one ring, so this is 128 KiB of kernel memory per session against 32 KiB before.
const SESSION_QUEUE_DEPTH: u64 = 16;

/// The monotonic clock, in nanoseconds.
fn now_ns() -> u64 {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: on success the kernel wrote the ns count.
    unsafe { (&raw const CLOCK_BUF).read() }
}

/// Create a connected channel pair with a `depth`-slot ring each. Returns `(a, b)`.
fn make_channel(depth: u64) -> Option<(u64, u64)> {
    // SAFETY: CTRL_OUT0/CTRL_OUT1 are valid writable out-params.
    let cr = unsafe {
        syscall4(
            SYS_CHANNEL_CREATE,
            (&raw mut CTRL_OUT0) as u64,
            (&raw mut CTRL_OUT1) as u64,
            depth,
            0,
        )
    };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const CTRL_OUT0).read(), (&raw const CTRL_OUT1).read()) })
}

/// Send `Meta::Ready` on the control channel, transferring `kernel_end`.
fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(body_len) = librsproto::meta::ready(&mut body, b"compositor") else { return false };
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto message goes at PAYLOAD_OFF.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], librsproto::OP_READY, 0, 0, &body[..body_len], 1)
        {
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

/// Reply to a forwarded resolve by handing back `client_end` as the answer.
///
/// A connection has no distinct object kind: it *is* a live channel, the same shape
/// `profile-server` uses for a directory session.
fn reply_session(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid; one handle rides the reply.
    unsafe {
        let Some(rs_len) =
            encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1)
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

/// Reply to a forwarded resolve with an error.
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

/// Send a reply body on a **session** channel.
///
/// The half of the protocol that was missing: `dispatch` produced replies and this never
/// sent them, so a client could not learn its window id and never saw a `Release`. The
/// spec has said "Reply, 4 bytes: the new `window` id" since Part A.
fn reply_on_session(session: u64, op: u16, request_id: u64, body: &[u8]) -> bool {
    // SAFETY: REPLY_MSG is a valid buffer; no handles transferred.
    unsafe {
        let Some(rs_len) =
            encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, RS_FLAG_REPLY, body, 0)
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

/// Send one server-initiated record. Returns `false` if the channel would not take it.
///
/// No request id: nothing asked for these. A refusal is **not** a drop any more — the caller
/// leaves the message at the head of the session's outbox and tries again next time round,
/// which is the whole difference between this and the `NOBLOCK`-and-forget it replaced.
fn send_input(session: u64, op: u16, body: &[u8]) -> bool {
    reply_on_session(session, op, 0, body)
}

/// Log a routed record, up to [`MAX_LOGGED_ROUTES`] of them.
fn log_route(rec: &Outbound) {
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
    let mut l = Line::new();
    match rec {
        Outbound::Key { window, event } => {
            l.s(b"compositor: key win=").u(*window as u64);
            l.s(b" code=").u(event.keycode as u64);
            l.s(b" down=").u(event.pressed as u64);
        }
        Outbound::Pointer { window, event } => {
            l.s(b"compositor: ptr win=").u(*window as u64);
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
    }
    l.end();
}

/// Queue everything the router produced.
///
/// Records addressed to a window whose session has already gone are dropped rather than
/// broadcast: `session_of` returning `None` means the owner disconnected between the event
/// arriving and this running.
fn deliver(srv: &mut Server, out: &[Outbound]) {
    for rec in out {
        let Some(slot) = srv.session_of(rec.window()) else {
            continue;
        };
        log_route(rec);
        enqueue(srv, slot, *rec);
    }
}

/// Send a repeat if one is due.
///
/// Queued through the outbox like every other record, so it cannot be displaced by input and
/// arrives in order with the press it continues.
fn fire_repeat(srv: &mut Server) {
    let now = now_ns();
    let Some(mut r) = srv.repeat else { return };
    // **The whole reason repeat is compositor-side**, and checked here rather than cleared
    // at each focus-moving site: a key held while focus moves must not keep typing into the
    // window that lost it, and no client can know that happened. Validating against the
    // stack means a *new* way to move focus cannot forget to stop the repeat — the same
    // reasoning as `Router::prune` asking the tree rather than being told.
    //
    // **Not covered end to end**, and the comment says so rather than implying otherwise:
    // `check-input` never moves focus while a key is held, because doing so needs a second
    // windowed client. Removing this check leaves the gate green. `Repeat`'s *timing* is
    // host-tested; this predicate is not.
    if srv.stack.focus_candidate() != Some(r.window) {
        srv.repeat = None;
        return;
    }
    if !r.due(now) {
        return;
    }
    srv.repeat = Some(r);
    // The window may have gone since the key went down — the client exited mid-keystroke —
    // in which case the repeat is dropped and the state with it.
    let Some(slot) = srv.session_of(r.window) else {
        srv.repeat = None;
        return;
    };
    let rec = Outbound::Key {
        window: r.window,
        event: KeyEvent {
            keycode: r.keycode,
            pressed: librsproto::surface::KEY_REPEAT,
            modifiers: r.modifiers,
            _pad: 0,
        },
    };
    log_route(&rec);
    enqueue(srv, slot, rec);
}

/// Tell the affected windows if focus moved since the last announcement.
///
/// **Called after anything that could move it** — a window created, destroyed, raised, or a
/// session closed — rather than from each of those sites, because the question "did focus
/// change" has one answer and four ways to provoke it. Comparing against what was last
/// announced is what keeps a commit from producing a `FocusEvent`.
///
/// Both halves go out: the window that lost the keyboard is told, and so is the one that
/// gained it. A client that only heard about gaining would keep a caret blinking behind
/// whatever took focus from it.
fn announce_focus(srv: &mut Server) {
    let now = srv.stack.focus_candidate();
    let Some((was, now)) = compositor::focus_transition(srv.announced_focus, now) else {
        return;
    };
    srv.announced_focus = now;
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

/// Queue one message for a session, logging if the queue had to discard.
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

/// Push as much of each session's queue down its channel as the channel will take.
///
/// Returns `true` if anything is still parked, which is what makes the serve loop's next
/// `sys_wait` bounded rather than infinite.
///
/// **Head-of-line, and it stops at the first refusal.** Skipping a stuck message to deliver
/// a later one would reorder a client's event stream — a release arriving before the commit
/// it answers, or a button before the motion that positioned it.
///
/// **There is no writability signal to wait on.** A channel endpoint signals when it has
/// something to read, so a client draining its ring does not wake the compositor at all.
/// That is why this reports whether anything is still parked: the serve loop then waits with
/// a [`RETRY_INTERVAL_NS`] deadline instead of forever, and polls *only* while it owes
/// somebody a message.
fn flush_outboxes(srv: &mut Server) -> bool {
    for slot in 0..MAX_SESSIONS {
        // SAFETY: reading our own slot table.
        let ch = unsafe { SESSION_CH[slot] };
        if ch == 0 {
            srv.outbox[slot].clear();
            continue;
        }
        while let Some(rec) = srv.outbox[slot].front() {
            if !send_outbound(ch, &rec) {
                break;
            }
            srv.outbox[slot].pop();
        }
    }
    (0..MAX_SESSIONS).any(|i| !srv.outbox[i].is_empty())
}

/// Send one queued message. Returns `false` if the channel would not take it.
fn send_outbound(ch: u64, rec: &Outbound) -> bool {
    // Sized **from the types**, not from the byte counts the spec publishes. Widening
    // `PointerEvent` to carry modifiers left a hand-written `[0u8; 16]` here, and `write`
    // refuses a short buffer by returning `None` — so every pointer event would have been
    // silently dropped, with the compositor and the spec both still saying it was sent.
    match rec {
        Outbound::Key { event, .. } => {
            let mut body = [0u8; core::mem::size_of::<KeyEvent>()];
            match event.write(&mut body) {
                Some(_) => send_input(ch, OP_KEY_EVENT, &body),
                // Unserialisable is not "retry forever": dropping it clears the queue head
                // so everything behind it can still move.
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
    }
}

/// Resolve `/dev/input/new` and return the consumer channel, or `None` if there is none.
///
/// Not fatal. A machine with no i8042 has no input server, and a compositor that refused to
/// start without one would take the display down with it — the screen is still worth having.
fn connect_input(root_ns: u64) -> Option<u64> {
    use libkern::handle::{RawHandle, Rights};
    use libos::{Handle, Namespace, NsReadOnly, Only, Resource, block_on};

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run;
    // `borrow` never closes it.
    let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: `/dev/input/new` resolves to a channel endpoint — the input server mints one
    // consumer per resolve, exactly as `/dev/draw/new` mints one session.
    let ch = block_on(unsafe {
        ns.lookup::<Resource, Only>("/dev/input/new", Rights::RECV | Rights::WAIT)
    })
    .ok()?;
    Some(ch.into_raw().0)
}

/// Drain one `Input::Events` batch and route it. Returns `false` if the channel died.
fn serve_input(srv: &mut Server, fb: &mut RawFramebuffer) -> bool {
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
        // Anything but `PeerClosed` is transient — a spurious signal with nothing queued —
        // and retrying next time round is right. `PeerClosed` means the input server is
        // gone and the handle must leave the wait set, or the loop spins on it forever.
        return rr != KError::PeerClosed.as_i32() as i64;
    }

    let mut out = alloc::vec::Vec::new();
    let mut restacked = false;
    let now = now_ns();
    let cursor_was = srv.router.pointer();
    // SAFETY: bounded read of the payload the kernel just wrote.
    unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let msg = core::slice::from_raw_parts(
            (&raw const RECV_MSG[PAYLOAD_OFF]) as *const u8,
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        let Ok(m) = decode(msg) else {
            return true;
        };
        if m.op != librsproto::OP_INPUT_EVENTS {
            return true;
        }
        // A trailing partial record is dropped rather than guessed at. `chunks_exact`
        // says so at the type level; `chunks` would hand `read` a short slice and get
        // `None`, which reads as "malformed" for a batch that is merely truncated.
        for raw in m.body.chunks_exact(INPUT_EVENT_LEN) {
            let Some(ev) = InputEvent::read(raw) else {
                continue;
            };
            let mut logical = [libinput::Logical::Dropped; libinput::MAX_PER_GROUP];
            let n = srv.interp.feed(ev, &mut logical);
            for l in &logical[..n] {
                restacked |= srv.router.route(l, &mut srv.stack, &mut out);
                // **Where a press landed, always — not through `log_route`.** That path is
                // capped at `MAX_LOGGED_ROUTES` and only sees records that were *delivered*,
                // so the two things a failing gate most needs are exactly the two it cannot
                // get: a click that hit no window logs nothing at all, and by the time a
                // gate has moved the cursor the cap is long spent. A press is rare — one
                // line per click — so this is not the log volume the cap exists to bound.
                // **And a dropped batch, which is the other half of the same question.**
                // `input-server` increments a per-consumer loss counter and announces it as
                // `SYN_DROPPED`; `libinput` turns that into `Logical::Dropped`. Nothing on
                // that path prints anything, so a lost batch is invisible — and a lost batch
                // is the *only* mechanism that moves a press off the point the injector
                // computed. Without this, a transcript can say the press landed somewhere
                // unexpected but not whether input was lost getting there, which is exactly
                // the fork a recurrence needs. Per-consumer accounting also means
                // `input-testclient` cannot cover it: a loss on the compositor's slot never
                // reaches the client's event dump.
                let diag = match *l {
                    libinput::Logical::Button { pressed: true, .. } => true,
                    libinput::Logical::Dropped => true,
                    _ => false,
                };
                // SAFETY: single-threaded server; this counter is touched only from the
                // serve loop, as `ROUTES_LOGGED` is.
                let logged = unsafe { INPUT_DIAGS_LOGGED };
                if diag && logged < MAX_LOGGED_INPUT_DIAGS {
                    // SAFETY: as above.
                    unsafe { INPUT_DIAGS_LOGGED = logged + 1 };
                    let mut pl = Line::new();
                    match *l {
                        libinput::Logical::Dropped => {
                            pl.s(b"compositor: input batch DROPPED (SYN_DROPPED)");
                        }
                        _ => {
                            let p = srv.router.pointer();
                            pl.s(b"compositor: press at x=")
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
                // Repeat follows the *physical* key, so it is armed from the interpreted
                // transition rather than from what the router decided to do with it: a key
                // that reached no window still stops repeating when it comes up.
                if let libinput::Logical::Key { keycode, pressed, modifiers } = *l {
                    srv.repeat = compositor::Repeat::after_key(
                        srv.repeat,
                        keycode,
                        pressed,
                        modifiers,
                        srv.stack.focus_candidate(),
                        now,
                    );
                }
                // A `SYN_DROPPED` means the held-key set is a guess, so a repeat started
                // from it is too — `libinput` has already reset what it accumulated.
                if matches!(l, libinput::Logical::Dropped) {
                    srv.repeat = None;
                }
            }
        }
    }

    deliver(srv, &out);
    // A click that raised a window moved focus with it.
    announce_focus(srv);
    // Erase the cursor's old position and draw it at the new one. Skipped when a restack is
    // about to repaint everything anyway — which also draws the cursor, because `repaint`
    // is the only thing here that touches the screen.
    if restacked {
        // A click raised a window, so what is on screen no longer matches the stack.
        repaint(srv, fb);
    } else {
        repaint_cursor_move(srv, fb, cursor_was, srv.router.pointer());
    }
    true
}

/// What a forwarded resolve under `/dev/draw` is asking for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Resolved {
    /// `new` — mint a session.
    New,
    /// `manage` — mint *the* manager channel.
    Manage,
    /// `<N>/info` — that window's metadata.
    Info(u32),
    /// Anything else.
    Unknown,
}

/// Classify a resolve suffix.
///
/// Suffixes arrive without a leading separator (`/dev/draw/1/info` -> `1/info`), the
/// convention `Namespace::resolve` uses and one that cost a boot to rediscover in M1.
fn classify(suffix: &[u8]) -> Resolved {
    if suffix == b"new" {
        return Resolved::New;
    }
    if suffix == b"manage" {
        return Resolved::Manage;
    }
    if let Some(slash) = suffix.iter().position(|&c| c == b'/')
        && &suffix[slash + 1..] == b"info"
        && let Some(id) = parse_u32(&suffix[..slash])
    {
        return Resolved::Info(id);
    }
    Resolved::Unknown
}

/// Parse a decimal window id. Rejects empty input, non-digits and overflow — a resolve
/// suffix is client-supplied text.
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

/// Answer `<N>/info` with a read-only object holding the window's `WindowInfo`.
///
/// The same shape `/dev/framebuffer/info` uses: a resolve answers with an object the caller
/// maps, not with a message. A fresh object per resolve, holding a snapshot — handing out a
/// live shared mapping would let readers observe mid-update state.
fn reply_window_info(serve_end: u64, request_id: u64, srv: &Server, id: u32) -> bool {
    let Some(info) = srv.stack.info(id) else {
        return reply_resolve_error(serve_end, request_id, KError::NotFound);
    };
    let mut bytes = [0u8; 32];
    if info.write(&mut bytes).is_none() {
        return reply_resolve_error(serve_end, request_id, KError::KernelError);
    }
    // SAFETY: a plain anonymous object of `bytes.len()`.
    let obj = unsafe { syscall4(SYS_MEMORY_CREATE, bytes.len() as u64, 0, 0, 0) };
    if obj <= 0 {
        return reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
    }
    // SAFETY: mapping an object this process just created, to fill it.
    let addr = unsafe {
        syscall4(
            SYS_MEMORY_MAP,
            obj as u64,
            0,
            bytes.len() as u64,
            libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE,
        )
    };
    if addr <= 0 {
        // SAFETY: closing the object we just made and cannot use.
        unsafe { syscall4(SYS_HANDLE_CLOSE, obj as u64, 0, 0, 0) };
        return reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
    }
    // SAFETY: `addr` maps at least `bytes.len()` writable bytes.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
    // **Unmap before replying.** The mapping was only ever a way to fill the object, and it
    // holds its own reference to it — so leaving it behind would pin the frames for the
    // compositor's life even after the handle is transferred away, and nothing records
    // `addr` to reclaim them later. Any holder of `/dev/draw` can resolve `info` in a loop,
    // which made this an unbounded leak drivable by any client (PR #175 review, finding 1).
    // SAFETY: unmapping a range this process mapped moments ago and never reads again.
    unsafe { syscall2(libkern::SYS_MEMORY_UNMAP, addr as u64, bytes.len() as u64) };

    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    if resolve_reply(&mut body, librsproto::namespace::OBJECT_KIND_MEMOBJ, bytes.len() as u32)
        .is_none()
    {
        // SAFETY: nothing was sent, so the object is still ours to drop.
        unsafe { syscall4(SYS_HANDLE_CLOSE, obj as u64, 0, 0, 0) };
        return reply_resolve_error(serve_end, request_id, KError::KernelError);
    }
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid; the object rides the reply.
    let sent = unsafe {
        let Some(rs_len) = encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            OP_NS_RESOLVE,
            request_id,
            RS_FLAG_REPLY,
            &body,
            1,
        ) else {
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
        // A failed send leaves the transfers untaken, so the object is still ours — the
        // same rule `open_session` follows for the endpoint it failed to hand over.
        // SAFETY: closing a handle this process still owns.
        unsafe { syscall4(SYS_HANDLE_CLOSE, obj as u64, 0, 0, 0) };
    }
    sent
}

/// Open a session: mint a channel, keep the server end, hand the client end back.
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

/// Tell `window`'s client the compositor would like it at this geometry.
///
/// **Sent to the window's own session**, found through the same ownership table `dispatch`
/// authorises against — so there is no second notion of who owns a window that could disagree
/// with the first. A window whose session has gone is silently skipped: the manager is allowed
/// to be a moment behind, and a manager that had to be told about every teardown before it could
/// speak would need a synchronous protocol.
fn configure_window(srv: &Server, window: u32, width: u32, height: u32, origin: Point) {
    let Some(slot) = srv.session_of(window) else { return };
    // SAFETY: reading our own slot table.
    let ch = unsafe { SESSION_CH[slot] };
    if ch == 0 {
        return;
    }
    let mut body = [0u8; 20];
    let ev = ConfigureEvent { window, width, height, x: origin.x, y: origin.y };
    if ev.write(&mut body).is_some() {
        send_input(ch, OP_CONFIGURE, &body);
    }
}

/// Handle one request on the manager channel. Returns `false` if the manager is gone.
fn serve_manager(srv: &mut Server, fb: &mut RawFramebuffer) -> bool {
    // SAFETY: reading our own manager slot and valid recv out-params.
    let ch = unsafe { MANAGER_CH };
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
        // `PeerClosed` means the manager exited. Anything else is an empty ring.
        return rr != KError::PeerClosed.as_i32() as i64;
    }
    // SAFETY: bounded read-only slice over the just-received message.
    let (op, request_id, body) = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        match decode(req) {
            Ok(m) => (m.op, m.request_id, m.body.to_vec()),
            Err(_) => return true,
        }
    };
    match manager::dispatch(&mut srv.stack, op, &body) {
        MgrOutcome::Applied { dirty } => {
            match dirty {
                // Nothing on screen changed — placing a window that has not committed, which
                // is the manager's ordinary case during the handshake.
                Some(r) if r.size.w == 0 || r.size.h == 0 => {}
                Some(r) => repaint_region(srv, fb, r),
                None => repaint(srv, fb),
            }
            // **Focus may have moved.** A restack changes who is topmost-focusable, and the
            // clients on either side of that have to be told — the same announcement
            // click-to-focus makes, for the same reason.
            announce_focus(srv);
            reply_on_session(ch, op, request_id, &[]);
        }
        MgrOutcome::Configure { window, width, height, origin } => {
            // Forwarded to the window's *client*, which is a third party: the manager asked,
            // the client is told. Nothing changes on screen until that client commits.
            configure_window(srv, window, width, height, origin);
            reply_on_session(ch, op, request_id, &[]);
        }
        MgrOutcome::Failed(e) => {
            reply_error_on_session(ch, op, request_id, surface_errno(e));
        }
    }
    true
}

/// Mint the manager channel, or refuse because one is already attached.
///
/// **A capability by binding, and in Milestone 6 that binding gates nothing** — `/dev/draw` is
/// bound unscoped into init's root namespace and every graphical client inherits it, so any of
/// them could ask. What actually separates them here is *order*: the intended manager resolves
/// first. That is written down as an ordering rather than dressed up as a capability —
/// `TODO(manage-ungated)`, closed by Milestone 7's per-client namespaces.
fn open_manager(serve_end: u64, request_id: u64) -> bool {
    // SAFETY: single-threaded server reading its own state.
    if unsafe { MANAGER_CH } != 0 {
        // **Refused, not replaced.** Handing the channel to a second asker would silently
        // depose the first, and two managers placing windows is a race with no arbiter.
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

/// Drop the manager channel — it went away, so the compositor manages itself again.
fn close_manager() {
    // SAFETY: closing our own endpoint and clearing the slot.
    unsafe {
        if MANAGER_CH != 0 {
            syscall4(SYS_HANDLE_CLOSE, MANAGER_CH, 0, 0, 0);
            MANAGER_CH = 0;
        }
    }
    kprint(b"compositor: the manager went away\n");
}

/// Close session `slot` and destroy everything the client had on screen.
fn close_session(slot: usize, srv: &mut Server) {
    // SAFETY: closing our own endpoint and clearing the slot.
    unsafe {
        if SESSION_CH[slot] != 0 {
            syscall4(SYS_HANDLE_CLOSE, SESSION_CH[slot], 0, 0, 0);
            SESSION_CH[slot] = 0;
        }
    }
    // Nothing queued for a closed session is owed to whoever reuses the slot.
    srv.outbox[slot].clear();
    // A client that exits without destroying its windows must not leave them on screen.
    disconnect(&mut srv.conns[slot], &mut srv.stack);
    srv.buffers.retain(|b| srv.stack.window(b.window).is_some());
    // Its windows are gone, so focus has almost certainly moved — and the *departing*
    // session must not be told, which `session_of` handles by no longer finding it.
    announce_focus(srv);
}

/// Map a client's transferred `MemoryObject` and record it for `BufferSource`.
///
/// Mapped **once, at attach**, and kept for the buffer's life — the handle crosses the
/// channel a single time, not per frame.
///
/// **Always consumes `handle`**, on every path. A transferred handle nobody closes keeps
/// its object — and so the client's frames — alive for the compositor's life.
fn map_attached_buffer(srv: &mut Server, body: &[u8], handle: u64) -> bool {
    let Some(req) = librsproto::surface::parse_attach_buffer_request(body) else {
        // SAFETY: we are not mapping it, so drop it rather than leaking the object.
        unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        return false;
    };
    let len = req.pitch as usize * req.height as usize;
    // SAFETY: `handle` is a `MemoryObject` the client transferred; mapping it read-only
    // with the size its own geometry declares.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, handle, 0, len as u64, libkern::RIGHT_MAP_READ) };
    if addr <= 0 {
        // SAFETY: the map failed, so nothing references the object; drop our handle rather
        // than leaking it for the compositor's life.
        unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        return false;
    }
    srv.buffers.push(MappedBuffer {
        window: req.window,
        buffer: req.buffer,
        addr: addr as *mut u8,
        len,
    });
    // The mapping holds its own reference to the object, so the handle can go.
    // SAFETY: closing our reference; the mapping outlives it.
    unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
    true
}

/// Recomposite the whole screen.
///
/// **The fallback, not the norm, since M5 Part B.** `Outcome::Applied` names the region it
/// dirtied and the serve loop repaints that; this is what an `Applied { dirty: None }` — "I
/// cannot name what I changed" — and the first frame use. The comment that stood here said
/// damage-bounded repaint was an optimisation the compositor did not yet exploit, "and this
/// milestone has one client"; with an 812×480 terminal in the stack, recompositing 1280×800 on
/// every request cost a permanently busy CPU. See the decision log, 2026-08-12.
fn repaint(srv: &Server, fb: &mut RawFramebuffer) {
    let bounds = fb.geometry().bounds();
    repaint_region(srv, fb, bounds);
}

/// Recompose `region` and draw the cursor over it.
///
/// **The cursor goes on last.** A cursor under a window is not a cursor, and compositing it
/// into the stack would make it a window — with a position in the stacking order, a client
/// that could cover it, and hit-testing that would have to skip it.
fn repaint_region(srv: &Server, fb: &mut RawFramebuffer, region: Rect) {
    srv.stack.present_into(fb, BACKGROUND, srv, &[region], srv.router.pointer());
}

/// Repaint what the pointer moving from `was` to `now` disturbed.
///
/// **Both rectangles**, because the cursor is drawn rather than composited: the pixels it
/// covered are still on screen after it moves, and only recomposing where it *is* leaves a
/// trail of arrows behind it. The same rule the toolkit's diff follows for a widget that
/// moved, one layer up.
fn repaint_cursor_move(srv: &Server, fb: &mut RawFramebuffer, was: Point, now: Point) {
    if was == now {
        return;
    }
    repaint_region(srv, fb, compositor::cursor_rect(was));
    repaint_region(srv, fb, compositor::cursor_rect(now));
}

/// The wire error a rejected request reports.
fn surface_errno(e: SurfaceError) -> KError {
    match e {
        SurfaceError::Malformed => KError::InvalidArgument,
        SurfaceError::Unsupported => KError::Unsupported,
        // A window belonging to another connection reports exactly what a nonexistent one
        // does. **This is ownership enforcement, not secrecy** — a distinction worth being
        // exact about, because this comment used to claim the reply "cannot be used to probe
        // which ids exist" and that has not been true since `/dev/draw/<N>/info` landed:
        // a namespace resolve answers the same question for any holder of `/dev/draw`
        // (PR #175 review, finding 2). What collapsing the two cases still buys is that a
        // client learns nothing *from acting* — it cannot tell "not yours" from "not there",
        // so there is no oracle to walk by attempting operations, and the session channel
        // remains the only place anything can be changed. Enumeration lives at the
        // namespace, deliberately; see `docs/spec/rsproto-surface-ops.md`.
        SurfaceError::NotFound => KError::NotFound,
        SurfaceError::Rejected(_) => KError::InvalidArgument,
    }
}

/// Send an error reply on a session channel.
fn reply_error_on_session(session: u64, op: u16, request_id: u64, err: KError) -> bool {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = librsproto::error::error_body(&mut ebody, err.as_i32(), 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer; no handles transferred.
    unsafe {
        let Some(rs_len) = encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            op,
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
            session,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Serve one request on session `slot`. Returns `false` if the session should close.
fn serve_session(slot: usize, srv: &mut Server, fb: &mut RawFramebuffer) -> bool {
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
        // `PeerClosed` is the client going away, which is the normal end of a session.
        return rr != KError::PeerClosed.as_i32() as i64;
    }

    // Copy the body out rather than holding a borrow of `RECV_MSG` across dispatch. Every
    // Surface body is at most 24 bytes, so this is a copy of nothing; the alternative is
    // two live `static mut` borrows at once.
    let mut body_buf = [0u8; MAX_BODY];
    let (op, request_id, body_len, handle) = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            (&raw const RECV_MSG[PAYLOAD_OFF]) as *const u8,
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        // Take the handles out **before** anything can fail. A message that does not decode
        // can still have carried one, and returning early without closing it pins the
        // client's object for the compositor's life — a leak a client drives by sending one
        // malformed message.
        //
        // **Every** transfer, not just the first. No Surface op takes more than one handle,
        // but a client is free to send up to `IPC_HANDLE_MAX`, and the kernel installs all
        // of them in this process's table whether or not anything here looks at them. The
        // surplus is closed immediately: it can never be wanted, and leaving it was the
        // same unbounded leak as the three fixed above, differing only in how it arrived
        // (PR #175 review, finding 2).
        let hcount =
            ((&raw const RECV_COUNT).read() as usize).min(libkern::abi::IPC_HANDLE_MAX);
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

    // SAFETY: a bounded mutable slice over the reply buffer; `body` is a local copy.
    let outcome = unsafe {
        dispatch(
            &mut srv.conns[slot],
            &mut srv.stack,
            op,
            body,
            &mut REPLY_MSG[PAYLOAD_OFF + 16..],
        )
    };

    // **Map only after `dispatch` accepted the attach.** Mapping first would record the
    // client's memory under ids it does not own: `dispatch` would answer `NotFound`, the
    // entry would stay, and `Server::pixels` resolves by first match — so another client's
    // pixels would be painted into the rightful owner's window. Doing it in this order
    // makes the ownership boundary cover the pixels, not just the stack.
    //
    // Whatever happens, the handle is **disposed of exactly once**. It is only ever consumed
    // by `map_attached_buffer`, which closes it on all of its own paths; everything else —
    // a rejected attach, or a handle riding an op that has no business carrying one, which a
    // client is free to send — closes it here. Anything left unclosed keeps the client's
    // object, and therefore its frames, alive for the compositor's life.
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
            // SAFETY: `dispatch` wrote `len` bytes at this offset in REPLY_MSG. Copy them
            // out before `reply_on_session` re-encodes into the same buffer.
            let mut body = [0u8; MAX_BODY];
            let n = len.min(MAX_BODY);
            unsafe {
                body[..n].copy_from_slice(&REPLY_MSG[PAYLOAD_OFF + 16..PAYLOAD_OFF + 16 + n]);
            }
            if !reply_on_session(ch, op, request_id, &body[..n]) {
                // The client is blocked in `request` waiting for exactly this. A silent
                // failure parks it forever, so say so — see `send_release` below.
                kprint(b"compositor: DROPPED a reply (client receive ring full)\n");
            }
            // **No repaint.** The only request that replies is `CreateWindow`, and a window
            // with no committed buffer contributes no pixels. This used to recomposite the
            // whole screen; see `Outcome::Applied`'s `dirty`.
        }
        Outcome::Created { reply_len, configure } => {
            // **The reply first, then the configure**, in that order and on the same channel.
            // The client is blocked in `Window::new` reading the reply for its id, and then
            // waits for the configure before it may commit — so sending them the other way
            // round would park every client at startup.
            // SAFETY: `dispatch` wrote `reply_len` bytes at this offset in REPLY_MSG. Copy them
            // out before `reply_on_session` re-encodes into the same buffer.
            let mut body = [0u8; MAX_BODY];
            let n = reply_len.min(MAX_BODY);
            unsafe {
                body[..n].copy_from_slice(&REPLY_MSG[PAYLOAD_OFF + 16..PAYLOAD_OFF + 16 + n]);
            }
            let sent = reply_on_session(ch, op, request_id, &body[..n]);
            if !sent {
                kprint(b"compositor: a create reply did not send\n");
            }
            let mut cfg = [0u8; 20];
            if sent && configure.write(&mut cfg).is_some() && !send_input(ch, OP_CONFIGURE, &cfg) {
                // The client will wait for this and nothing else will produce it, so a silent
                // failure parks it forever — the same reason the reply above says so.
                kprint(b"compositor: a window's first configure did not send\n");
            }
            // **No repaint.** A window with no committed buffer contributes no pixels.
        }
        Outcome::Applied { release, dirty } => {
            // A destroy is transitive, so drop every mapping whose window is gone.
            // Otherwise a client looping create/attach/destroy grows the compositor's
            // address space without bound.
            srv.buffers.retain(|b| srv.stack.window(b.window).is_some());

            // **Paint before releasing.** A `Release` is the client's evidence that the
            // commit it displaced has been dealt with; sending it first tells the client
            // the frame is done while the pixels are still unpainted. The client then
            // reports progress, and anything pacing off that — the display gate, or a
            // second client — observes a screen that has not caught up. Same shape as
            // announcing `Ready` before clearing the screen.
            match dirty {
                // Nothing changed on screen — an attach, or a destroy of a window that was
                // never committed.
                Some(r) if r.size.w == 0 || r.size.h == 0 => {}
                Some(r) => repaint_region(srv, fb, r),
                // A request that could not name its region. Correct, and the thing this
                // milestone stopped doing on every request.
                None => repaint(srv, fb),
            }
            if let Some((window, buffer)) = release {
                // **Through the outbox, like everything else.** Sent directly it competed
                // with input on the same ring, and input is continuous while a release is
                // not, so the cheap message reliably evicted the expensive one. Queued, it
                // holds its place ahead of anything arriving later and cannot be coalesced
                // away — a release is never motion.
                enqueue(srv, slot, Outbound::Release { window, buffer });
            }
        }
        Outcome::Failed(e) => {
            // **Logged a bounded number of times.** A rejection is client-driven, so this
            // is an unbounded write to a shared console: one misbehaving client can bury
            // every other service's output, and the churn probe alone produced 80 identical
            // lines per boot (PR #175 review, finding 6). The first few are the diagnostic;
            // the rest are noise, and the error still goes back to the client that caused
            // it either way.
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

    // **After the request, whatever it was.** `announce_focus` compares against what was
    // last announced, so an op that did not move focus costs a comparison and sends nothing
    // — which means there is no list of focus-moving ops to keep correct here, and that is
    // the point. An earlier version called this inside the `Applied` arm and named create as
    // one of the ops it covered; create is the one op that replies with a window id, so it
    // returns `Outcome::Reply` and never reached it. A new window goes on top of the stack
    // and `focus_candidate` is topmost-focusable, so focus had already moved and neither
    // client was told — the old window kept a caret it no longer owned while its keys went
    // to a window that had not painted yet (PR #184 review, finding 2).
    //
    // After the reply rather than before it: a client cannot make sense of gaining focus for
    // a window whose id it has not been handed yet.
    announce_focus(srv);
    true
}

/// The serve loop: the forwarding endpoint plus every open session.
fn serve_loop(serve_end: u64, mut fb: RawFramebuffer, srv: &mut Server) -> ! {
    kprint(b"compositor: serving /dev/draw\n");
    let mut parked = false;
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots; `n` is bounded by
        // `3 + MAX_SESSIONS` — `serve_end`, the input channel when connected, the manager
        // channel when one is attached, then the sessions — which the `const _` beside
        // `MAX_SESSIONS` holds to that limit.
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
            if srv.input_ch != 0 {
                WAIT_HANDLES[n] = srv.input_ch;
                n += 1;
            }
            if MANAGER_CH != 0 {
                WAIT_HANDLES[n] = MANAGER_CH;
                n += 1;
            }
            for i in 0..MAX_SESSIONS {
                if SESSION_CH[i] != 0 {
                    WAIT_HANDLES[n] = SESSION_CH[i];
                    n += 1;
                }
            }
            // Bounded while something is parked **or** a key is repeating — the two
            // deadline sources, whichever is sooner. An idle compositor with nothing held
            // still sleeps indefinitely, which is what keeps this from becoming a poll.
            let now = now_ns();
            let mut deadline = u64::MAX;
            if parked {
                deadline = deadline.min(now.saturating_add(RETRY_INTERVAL_NS));
            }
            if let Some(r) = srv.repeat {
                deadline = deadline.min(r.next_at);
            }
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                n as u64,
                (&raw mut WAIT_RESULTS) as u64,
                deadline,
            )
        };
        if waited < 1 {
            // `TimedOut` is a tick, not an error: whichever deadline expired, do its work
            // and go back to waiting rather than spinning.
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
            if srv.input_ch != 0 && h == srv.input_ch && !serve_input(srv, &mut fb) {
                // The input server died. Close the endpoint and carry on serving the
                // display — the alternative is a compositor that exits because a mouse
                // went away.
                kprint(b"compositor: input server gone\n");
                // SAFETY: closing an endpoint this process owns and stops using.
                unsafe { syscall4(libkern::SYS_HANDLE_CLOSE, srv.input_ch, 0, 0, 0) };
                srv.input_ch = 0;
                continue;
            }
            // SAFETY: reading our own manager slot.
            if unsafe { MANAGER_CH } != 0 && h == unsafe { MANAGER_CH } {
                if !serve_manager(srv, &mut fb) {
                    close_manager();
                }
                continue;
            }
            // SAFETY: scanning our own slot table for the signalled endpoint.
            if let Some(slot) = unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == h) }
                && !serve_session(slot, srv, &mut fb)
            {
                close_session(slot, srv);
                // Through `repaint`, so the pointer survives a client dying under it. It did
                // not: this composed directly and nothing redrew the cursor afterwards, so it
                // stayed gone until the next mouse movement (PR #185 review, finding 1).
                repaint(srv, &mut fb);
            }
        }
        // **Before the early `continue`.** Everything above may have queued messages, and
        // the common iteration — input arrived, no resolve — takes that `continue`. Flushing
        // only on the resolve path would leave every routed event sitting in its queue until
        // a client happened to connect.
        fire_repeat(srv);
        parked = flush_outboxes(srv);

        if !serve_signalled {
            continue;
        }

        // A forwarded resolve: `new` mints a session, `<N>/info` answers with window
        // metadata. A bare `<N>` and `<N>/ports/...` are later milestones.
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
            let payload_len =
                u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let req = core::slice::from_raw_parts(
                (&raw const RECV_MSG[PAYLOAD_OFF]) as *const u8,
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match decode(req) {
                Ok(m) if m.op == OP_NS_RESOLVE => {
                    let suffix = librsproto::namespace::parse_resolve_request(m.body)
                        .map(|r| r.suffix)
                        .unwrap_or(b"");
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
            Resolved::Unknown => {
                reply_resolve_error(serve_end, request_id, KError::NotFound);
            }
        }
    }
}

/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments; `ctrl` is the
/// control channel the supervisor spawned this server with.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, ctrl: u64) -> ! {
    kprint(b"compositor: up\n");

    // Authority is the binding: a compositor is a process whose namespace has this.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let (mut fb, info) = match unsafe { libdraw::acquire::acquire(root_ns) } {
        Ok(pair) => pair,
        Err(_) => {
            kprint(b"compositor: no /dev/framebuffer -- cannot serve\n");
            exit(1);
        }
    };
    let _ = info;

    let Some((kernel_end, serve_end)) = make_channel(SESSION_QUEUE_DEPTH) else {
        kprint(b"compositor: channel create FAIL\n");
        exit(1);
    };
    let mut srv = Server {
        stack: WindowStack::new(),
        conns: core::array::from_fn(|_| Connection::new()),
        buffers: alloc::vec::Vec::new(),
        repeat: None,
        announced_focus: None,
        outbox: (0..MAX_SESSIONS).map(|_| Outbox::new()).collect(),
        interp: Interpreter::new(),
        // The router clamps the cursor to the screen it was told about, so it has to be the
        // screen this compositor actually acquired — not a constant that happens to match.
        router: InputRouter::new(fb.geometry().bounds()),
        input_ch: 0,
    };
    match connect_input(root_ns) {
        Some(ch) => {
            srv.input_ch = ch;
            kprint(b"compositor: input connected\n");
        }
        None => kprint(b"compositor: no /dev/input/new -- display only\n"),
    }

    // Clear **before** announcing readiness, so `Meta::Ready` means "I have taken the
    // screen" rather than "I am about to".
    //
    // Announcing first left the clear racing whatever init spawned next: `bind_compositor`
    // would return while the clear was still pending, and on `-smp 4` two processes then
    // held `/dev/framebuffer` mapped read-write with nothing arbitrating. Ordering it here
    // costs nothing and removes the window entirely, where pacing it against a later signal
    // would only narrow it.
    // The pointer is drawn here too, so it exists from the moment the compositor owns the
    // screen rather than from whenever the mouse first moves — which on a machine with no
    // client and no mouse activity was never.
    repaint(&srv, &mut fb);

    if !send_ready(ctrl, kernel_end) {
        kprint(b"compositor: Ready send FAIL\n");
        exit(1);
    }

    serve_loop(serve_end, fb, &mut srv);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"compositor: panic\n");
    exit(2);
}
