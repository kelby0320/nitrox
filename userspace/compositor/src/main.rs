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
use compositor::outbox::{MgrEvent, MgrOutbox, Outbound, Outbox};
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
    SYS_HANDLE_CLOSE, SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_WAIT, exit, kprint,
    syscall2,
    syscall4, syscall5,
};
use libkern::debug::Line;
use libkern::error::KError;
use librsproto::namespace::{OBJECT_KIND_CHANNEL, resolve_reply};
use librsproto::surface::{
    ConfigureEvent, FocusEvent, KeyEvent, MgrCapture, OP_ATTACH_BUFFER, OP_CONFIGURE,
    OP_FOCUS_EVENT, OP_KEY_EVENT, OP_MGR_CAPTURE,
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
///
/// **Taken from `libdraw::scene` rather than declared here**, which it was until M8 Part B. The
/// two were the same literal, and the display gate has always depended on that: its reference
/// render fills with `scene::BACKGROUND` and is compared against pixels this constant painted,
/// so a change to either alone would have failed the gate with a colour mismatch rather than
/// naming the duplicate. One constant cannot drift from itself.
const BACKGROUND: Rgb = libdraw::scene::BACKGROUND;

/// Largest Surface request body.
///
/// **Sized by `SetTitle`, which is the only variable-length one** — the fixed records are 24
/// bytes or fewer. At 64 this silently capped a title at 60 bytes while the spec, the
/// deferral and `truncate_title` all said 256, and the cut was at a raw byte offset: a title
/// whose 61st byte began a multi-byte character came back `Malformed` and the rename never
/// happened, which is the exact corruption `truncate_title` exists to prevent, reintroduced
/// one frame further up. It also made `truncate_title`'s walk-back and `note_title_truncated`
/// unreachable in the shipped binary, because `title.len()` could never exceed 60 (PR #233
/// review, finding 1).
///
/// The assertion below is the point: this is a number two files away from the one it has to
/// track, and nothing else would notice them drifting apart again.
const MAX_BODY: usize = 4 + librsproto::surface::MAX_TITLE;
const _: () = assert!(MAX_BODY >= 4 + librsproto::surface::MAX_TITLE);

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

/// How long a new window waits for the manager to place it before it is shown anyway (M6 B4).
///
/// **A deadline, not a promise the manager will answer.** A wedged or slow shell must delay a
/// window, never lose it — a client blocked forever in `Window::new` because nobody ran the
/// desktop is a worse failure than a window that appears where the compositor put it. 200ms is
/// far longer than a scheduled round trip on an idle machine and short enough that a user who
/// hits it sees a slow launch rather than a hung one.
const CONFIGURE_DEADLINE_NS: u64 = 200_000_000;

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
    /// Windows created while a manager is attached, still holding their first `Configure`.
    ///
    /// `(window, deadline_ns)`. The client is blocked waiting for that configure, so nothing
    /// here may be dropped without sending one — see [`release_configure`] and
    /// [`CONFIGURE_DEADLINE_NS`].
    pending_configure: alloc::vec::Vec<(u32, u64)>,
    /// The screen, as the framebuffer reports it. Fixed for this process's life.
    ///
    /// Kept here so that the work area — which every path that changes a strut has to be able to
    /// re-answer — does not need the framebuffer threaded through it.
    screen: Rect,
    /// The layout last announced to the manager, so a change can be told from a repeat.
    last_layout: Option<librsproto::surface::MgrLayout>,
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
    /// Events owed to the manager, if one is attached. See [`MgrOutbox`].
    mgr_outbox: MgrOutbox,
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
        Outbound::Configure { window, width, height, x, y } => {
            l.s(b"compositor: mgr-configure win=").u(*window as u64);
            l.s(b" ").u(*width as u64).s(b"x").u(*height as u64);
            l.s(b" at ").i(*x as i64).s(b",").i(*y as i64);
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
        event: KeyEvent::new(
            r.window,
            r.keycode,
            librsproto::surface::KEY_REPEAT,
            r.modifiers,
        ),
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
    // **The manager hears about every transition, including ones no session owns.** A window
    // whose client has gone still leaves focus, and a manager tracking who has the keyboard
    // needs that edge as much as the client that gained it does.
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
    // The manager's events drain the same way and for the same reason: a manager whose ring is
    // briefly full must not lose a `created`, because unlike a dropped input event that is a
    // window it will never place and never hear about again.
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

/// Announce everything the stack recorded since this was last called — moves, then removals.
///
/// **Called after every dispatch, not only after the ops that obviously change something.** A
/// client disconnecting destroys its windows, one destroy removes a whole menu chain, and a
/// commit can resize a window — so what changed is not something a call site can name from the
/// op it just handled. Draining unconditionally also keeps the stack's logs from growing while
/// no manager is attached, because they are emptied either way.
fn drain_stack_events(srv: &mut Server) {
    // **Here, because every path that can change a strut ends here.** A panel is created,
    // destroyed, re-placed or committed at a different size through four different requests, and
    // all four drain their stack events — so one comparison in the one place they meet is what
    // keeps a manager's work area current without hunting for the causes (M9 Part B).
    announce_layout(srv);
    for window in srv.stack.take_geometry_changes() {
        // Gone already — destroyed in the same batch that moved it. Its removal is announced
        // just below; a rectangle for a window that no longer exists is not.
        let Some(w) = srv.stack.window(window) else { continue };
        let b = w.bounds();
        let ev = ConfigureEvent {
            window,
            width: b.size.w,
            height: b.size.h,
            x: b.origin.x,
            y: b.origin.y,
        };
        mgr_emit(srv, MgrEvent::Geometry(ev));
    }
    for window in srv.stack.take_removed() {
        // **The owner stops claiming it here**, whoever removed it. A client's own
        // `DestroyWindow` prunes its connection inline, but a manager's `Manage::Close` and a
        // transitive destroy of someone else's child do not pass through that code — and this
        // loop is the one place every removal, from every cause, is already enumerated. A
        // connection that outlives its window is exactly the wedged client `Manage::Close`
        // exists for, so this is not a hypothetical: see [`Connection::disown`].
        for conn in srv.conns.iter_mut() {
            conn.disown(window);
        }
        mgr_emit(srv, MgrEvent::Destroyed { window });
    }
}

/// Send `window`'s held first `Configure` and make it compositable. `true` if this was the one.
///
/// **The client is blocked on this.** Every path that removes a window from
/// [`Server::pending_configure`] must come through here or send a configure itself, or that
/// client waits forever in `Window::new`. The geometry is read from the stack at this moment
/// rather than from what was stashed at creation, so a manager that placed the window first
/// releases it *at the placed origin* — which is the entire point of holding it.
fn release_configure(srv: &mut Server, fb: &mut RawFramebuffer, window: u32) -> bool {
    let Some(i) = srv.pending_configure.iter().position(|&(w, _)| w == window) else {
        return false;
    };
    srv.pending_configure.remove(i);
    let Some(w) = srv.stack.window(window) else {
        // Destroyed while pending. Nobody is owed a configure for a window that is gone, and
        // the client learns it went by the destroy it asked for.
        return false;
    };
    let (width, height) = w.size;
    let origin = w.origin;
    let bounds = w.bounds();
    srv.stack.mark_configured(window);
    let sent = configure_window(srv, window, width, height, origin);
    // **Both of these belong here rather than at the call sites**, because releasing is what
    // changes the window's visibility and there are four paths that release. Two of them
    // originally did neither, which is a window that becomes drawable and is not drawn until
    // some unrelated damage arrives, and a keyboard that stays where it was (PR #218 review,
    // findings 3 and 7).
    //
    // The window's own rectangle, not the whole screen: nothing else changed, and the ordinary
    // release runs on every window creation while a manager is attached.
    repaint_region(srv, fb, bounds);
    announce_focus(srv);
    sent
}

/// Show every window whose manager never answered in time.
///
/// **The window appears where the compositor put it rather than not at all.** A shell that is
/// wedged, slow to start, or simply not interested must cost a launch some latency, never a
/// window — a client blocked forever in `Window::new` is the failure this deadline exists to
/// rule out.
fn fire_configure_deadlines(srv: &mut Server, fb: &mut RawFramebuffer) -> bool {
    // Free when nothing is held, which is the overwhelming majority of iterations — and it has
    // to be free, because this runs every time round the serve loop rather than only on a
    // timeout. Reading the clock unconditionally would put a syscall in the hot path.
    if srv.pending_configure.is_empty() {
        return false;
    }
    let now = now_ns();
    let due: alloc::vec::Vec<u32> = srv
        .pending_configure
        .iter()
        .filter(|&&(_, at)| at <= now)
        .map(|&(w, _)| w)
        .collect();
    let fired = !due.is_empty();
    for window in due {
        let mut l = Line::new();
        l.s(b"compositor: no manager answer for window ").u(window as u64).s(b"; showing it");
        l.end();
        release_configure(srv, fb, window);
    }
    fired
}

/// Queue one event for the manager, if there is one. Logs a discard the way `enqueue` does.
///
/// **A no-op with no manager attached**, rather than an error: M6 has no manager in the boot
/// path at all, and the compositor manages itself perfectly well without one. The events exist
/// for whoever holds the channel, and nobody holding it is the ordinary case.
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

/// A manager event that would not serialise: log it, and let the queue move on.
///
/// **Logged, unlike the client-facing equivalent in [`send_outbound`].** These events are
/// queued precisely because losing one leaves a manager's window list wrong forever with no
/// resync op, so the one path that can still lose one must not do it quietly.
fn unserialisable(what: &[u8]) -> bool {
    let mut l = Line::new();
    l.s(b"compositor: manager ").s(what).s(b" would not serialise; dropped");
    l.end();
    // `true` means "the queue may move on". Retrying forever would wedge every event behind
    // a record that cannot be written no matter how often it is tried.
    true
}

/// Send one queued manager event. `false` if the channel would not take it.
fn send_mgr_event(ch: u64, ev: &MgrEvent) -> bool {
    use librsproto::surface::{
        MgrHotkey, MgrWindowCreated, MgrWindowRef, OP_MGR_HOTKEY, OP_MGR_WINDOW_CREATED,
        OP_MGR_WINDOW_DESTROYED, OP_MGR_WINDOW_FOCUS, OP_MGR_WINDOW_GEOMETRY,
        OP_MGR_WINDOW_TITLE,
    };
    // Sized **from the types**, not from the byte counts the spec publishes — the same rule
    // `send_outbound` states and for the same reason: widening `PointerEvent` left a
    // hand-written `[0u8; 16]` there, and `write` refuses a short buffer by returning `None`,
    // so every event of that kind would have been dropped with the spec still saying it was
    // sent. A queue whose purpose is that nothing is lost must not be one field away from
    // losing everything (PR #217 review, finding 5).
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
                Some(n) => {
                    send_input(ch, librsproto::surface::OP_MGR_WINDOW_STATE_REQUEST, &body[..n])
                }
                None => unserialisable(b"WindowStateRequest"),
            }
        }
        MgrEvent::Title { window, title } => {
            // The one variable-length manager body, so its buffer is sized from the cap rather
            // than from a type — `MAX_TITLE` plus the window id.
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
        Outbound::CloseRequested { window } => {
            let mut body = [0u8; 4];
            match (librsproto::surface::WindowRef { window: *window }).write(&mut body) {
                Some(n) => send_input(ch, librsproto::surface::OP_CLOSE_REQUESTED, &body[..n]),
                None => true,
            }
        }
        Outbound::Configure { window, width, height, x, y } => {
            let mut body = [0u8; 20];
            let ev = ConfigureEvent {
                window: *window,
                width: *width,
                height: *height,
                x: *x,
                y: *y,
            };
            match ev.write(&mut body) {
                Some(_) => send_input(ch, OP_CONFIGURE, &body),
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

/// Batches to take from the input channel in one pass.
///
/// **A bound, not a target.** The loop drains what is queued and stops; this only keeps a
/// continuously-moving mouse from starving the client and manager channels in the same wait set.
/// One batch is one group, so this is about a second of a 100 Hz mouse.
const INPUT_DRAIN_MAX: usize = 128;

/// Drain every queued `Input::Events` batch, route them, and repaint once. `false` if the
/// channel died.
///
/// **Once for the whole drain, not once per batch.** Each repaint is a compose plus a write to
/// the framebuffer, and it happens with no input being read; doing it per message made the
/// compositor's throughput the ceiling on how fast a mouse could move, and everything past that
/// ceiling piled up in a queue the input server had to give up on (2026-08-26). Coalescing costs
/// nothing visually: the cursor is drawn at one place at a time, so painting the intermediate
/// positions of a movement already finished is work whose result is immediately overwritten.
fn serve_input(srv: &mut Server, fb: &mut RawFramebuffer) -> bool {
    let mut out = alloc::vec::Vec::new();
    // The regions a restack disturbed this pass, plus the cursor's old and new positions. The
    // cursor's *old* position is where it was last painted, which is where this pass starts —
    // intermediate positions were never drawn, so they need no erasing.
    let mut damage: alloc::vec::Vec<Rect> = alloc::vec::Vec::new();
    let cursor_was = srv.router.pointer();
    let mut alive = true;
    let mut drained = 0;
    while drained < INPUT_DRAIN_MAX {
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
            // The ordinary end of the drain: nothing more is queued. `PeerClosed` means the
            // input server is gone and the handle must leave the wait set, or the loop spins on
            // it forever; anything else is a spurious signal with nothing behind it.
            alive = rr != KError::PeerClosed.as_i32() as i64;
            break;
        }
        drained += 1;
        let now = now_ns();
        route_one_batch(srv, &mut out, &mut damage, now);
    }

    deliver(srv, &out);
    // **What routing did to the stack, told to the manager.** Input is the third path that
    // mutates windows — a click raises one, and since M9 Part A a drag moves one — and it was
    // the only one that never drained: the manager and session paths both do it after their
    // dispatch, so a geometry change made by the pointer sat in the log until some *other*
    // request happened to flush it. The drag at the end of a gesture is exactly that case.
    drain_stack_events(srv);
    // A click that raised a window moved focus with it.
    announce_focus(srv);
    // Erase the cursor where it was last drawn and draw it where it is now, along with anything
    // a restack disturbed. One `present_into` for the lot: overlapping rectangles compose the
    // same pixels twice, which is cheap, while a second call would be a second traversal.
    let cursor_now = srv.router.pointer();
    if cursor_now != cursor_was {
        damage.push(compositor::cursor_rect(cursor_was));
        damage.push(compositor::cursor_rect(cursor_now));
    }
    if !damage.is_empty() {
        srv.stack.present_into(fb, BACKGROUND, srv, &damage, cursor_now);
    }
    alive
}

/// Route the batch sitting in `RECV_MSG`, appending client records to `out` and repaint regions
/// to `damage`.
fn route_one_batch(
    srv: &mut Server,
    out: &mut alloc::vec::Vec<Outbound>,
    damage: &mut alloc::vec::Vec<Rect>,
    now: u64,
) {
    // SAFETY: bounded read of the payload the kernel just wrote.
    unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let msg = core::slice::from_raw_parts(
            (&raw const RECV_MSG[PAYLOAD_OFF]) as *const u8,
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        let Ok(m) = decode(msg) else {
            return;
        };
        if m.op != librsproto::OP_INPUT_EVENTS {
            return;
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
                let routed = srv.router.route(l, &mut srv.stack, out);
                if let Some(r) = routed.restacked {
                    damage.push(r);
                }
                // A dragged window disturbs the union of where it was and where it now is,
                // which `drag_to` computed from state read before the move.
                if let Some(r) = routed.moved {
                    damage.push(r);
                }
                // **Drained per event, not per batch.** A chord and the window events it
                // causes have to reach the manager in the order they happened, and the
                // manager acts on this batch before the next one is routed.
                for hk in srv.router.take_hotkeys() {
                    mgr_emit(srv, MgrEvent::Hotkey(hk));
                }
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
                // **And `Super` arriving at all, which is the third question a stuck session
                // asks.** Every chord this system binds is `Super`-something, and a host desktop
                // binds `Super` too — so "the Super key does nothing" has two entirely different
                // causes (the host kept the keystroke; the guest got it and matched no chord)
                // and no way to tell them apart from outside. One line per transition says which.
                //
                // **The modifier only, never the keycode beside it.** A log of what was typed is
                // a log of the password typed at the greeter; a log of whether the meta key went
                // down is not, and it is the whole of what this question needs.
                let meta_change = match *l {
                    libinput::Logical::Key { keycode, pressed, .. }
                        if keycode == libkern::abi::KEY_LEFTMETA
                            || keycode == libkern::abi::KEY_RIGHTMETA =>
                    {
                        Some(pressed)
                    }
                    _ => None,
                };
                let diag = meta_change.is_some()
                    || match *l {
                        libinput::Logical::Button { pressed: true, .. } => true,
                        libinput::Logical::Dropped => true,
                        _ => false,
                    };
                // Already inside the enclosing `unsafe` block, so no inner one: the
                // justification is that this is a single-threaded server and the counter is
                // touched only from the serve loop, as `ROUTES_LOGGED` is.
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
                //
                // **Except a key the router consumed**, which is the one case where "reached no
                // window" is a decision rather than an accident. A registered chord that armed a
                // repeat would deliver its key to the focused window 400 ms later and 25 times a
                // second after that — bypassing the router entirely, since `fire_repeat` enqueues
                // straight to the focused session. Holding `Super+1` while already on desktop 1
                // filled the terminal with `1`s (PR #241 review, blocking 1).
                if let libinput::Logical::Key { keycode, pressed, modifiers } = *l
                    && !routed.consumed
                {
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
    // **Sized from the constant, not from the number that was right when this was written.**
    // `WindowInfo` grew from 32 to 40 bytes in M8 Part A, and a literal here would have made
    // `write` refuse — turning every `info` resolve into `KernelError` — or, if `write` had
    // filled what it could instead, published a window whose desktop and flags were whatever
    // the stack memory held.
    let mut bytes = [0u8; librsproto::surface::WINDOW_INFO_LEN];
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
/// Queue a `Configure` for `window`'s client. `false` if there is no such session.
///
/// **Queued, not sent.** It used to go out directly with `SENDMODE_NOBLOCK` and the result
/// discarded, so a client whose receive ring was briefly full — mid-motion-burst, say — simply
/// never resized, with nothing logged and the manager told it had succeeded. Every other
/// server-initiated record goes through the outbox precisely so it holds its place and is
/// retried (PR #216 review, finding 4).
fn configure_window(
    srv: &mut Server,
    window: u32,
    width: u32,
    height: u32,
    origin: Point,
) -> bool {
    let Some(slot) = srv.session_of(window) else { return false };
    // SAFETY: reading our own slot table.
    if unsafe { SESSION_CH[slot] } == 0 {
        return false;
    }
    enqueue(
        srv,
        slot,
        Outbound::Configure { window, width, height, x: origin.x, y: origin.y },
    );
    true
}

/// Scale a window into the manager's buffer and answer. `false` if the manager is gone.
///
/// **The compositor gains an operation and no allocation policy.** The manager allocated the
/// object and sent it; this maps it, writes, unmaps and forgets it — the mirror of a client
/// allocating a buffer the compositor reads.
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
    // What is on screen for this window, which is its *committed* buffer — the same thing
    // compositing reads, so a thumbnail cannot show a frame the screen never did.
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
    let addr = unsafe {
        syscall4(
            SYS_MEMORY_MAP,
            obj,
            0,
            len as u64,
            libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE,
        )
    };
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
    let (op, request_id, body, carried) = unsafe {
        // **Close every transfer this message carried, except the one `Capture` needs.**
        // `sys_channel_recv` takes no capacity argument and the kernel installs whatever the
        // sender attached into this process's table whether or not anything here looks at it.
        // Left alone they pin a slot in the *global* handle table for the compositor's life,
        // and a `MemoryObject` would pin the sender's frames with it. `serve_session` closes
        // its surplus for exactly this reason (PR #175 review, finding 2); the manager path
        // was the sibling that got missed (PR #216 review, finding 3). In M6 any `/dev/draw`
        // holder can be the manager — until M7 Part E made the binding the gate — so "the
        // manager would not
        // do that" is not a bound.
        let hcount = ((&raw const RECV_COUNT).read() as usize).min(libkern::abi::IPC_HANDLE_MAX);
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        let decoded = decode(req);
        // **The first handle survives only for the op that takes one**, and every other is
        // closed here as before. Keeping them all would pin a slot in the *global* handle
        // table for the compositor's life, and a `MemoryObject` would pin the sender's frames
        // with it (PR #175 review finding 2, PR #216 review finding 3).
        let takes_handle = matches!(&decoded, Ok(m) if m.op == OP_MGR_CAPTURE);
        let mut carried = 0u64;
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
        // Handled here rather than in `manager::dispatch`, which is given a `WindowStack` and
        // nothing else: this needs the handle, the pixel source, and syscalls to map with.
        let ok = do_capture(srv, ch, request_id, &body, carried);
        // SAFETY: the object was installed in this process's table by the recv above; the
        // capture has finished with it either way.
        if carried != 0 {
            unsafe { syscall4(SYS_HANDLE_CLOSE, carried, 0, 0, 0) };
        }
        return ok;
    }
    if op == librsproto::surface::OP_MGR_REQUEST_CLOSE {
        // **Handled here rather than in `dispatch`**, which sees only the stack: asking a client
        // to close means putting a record in *that client's* outbox, and the outboxes are the
        // server's. Nothing about the window changes — this is the polite half.
        let Some(req) = librsproto::surface::MgrWindowRef::read(&body) else {
            return reply_error_on_session(ch, op, request_id, KError::InvalidArgument);
        };
        let Some(slot) = srv.session_of(req.window) else {
            // No window, or no session owns it — one answer for both, like every other
            // ownership question here.
            return reply_error_on_session(ch, op, request_id, KError::NotFound);
        };
        enqueue(srv, slot, Outbound::CloseRequested { window: req.window });
        Line::new()
            .s(b"compositor: asked window ")
            .u(req.window as u64)
            .s(b" to close")
            .end();
        return reply_on_session(ch, op, request_id, &[]);
    }
    if op == librsproto::surface::OP_MGR_QUERY_LAYOUT {
        // Handled here for the reason `Capture` is: `manager::dispatch` sees a `WindowStack` and
        // nothing else, and this answer needs the *screen*, which is the framebuffer's.
        let layout = current_layout(srv);
        let mut body = [0u8; core::mem::size_of::<librsproto::surface::MgrLayout>()];
        return match layout.write(&mut body) {
            Some(n) => reply_on_session(ch, op, request_id, &body[..n]),
            None => reply_error_on_session(ch, op, request_id, KError::InvalidArgument),
        };
    }
    let mgr_outcome = manager::dispatch(&mut srv.stack, op, &body);
    // **A manager acting on a window invalidates what that window last *asked* to be.** The
    // dedup on `RequestState` compares against a value only client requests were writing, and
    // the manager changes a window's state by four other routes — a taskbar click, a chord, the
    // overview. After one of those the shadow was stale, and the *next* identical request was
    // dropped as a repeat: a minimise button that worked once and then never again, with the
    // client told it had succeeded (PR #249 review, blocking 1).
    //
    // Cleared here rather than in each op, so the next manager op added cannot forget to.
    if let MgrOutcome::Applied { window: Some(w), .. } | MgrOutcome::Configure { window: w, .. } =
        mgr_outcome
    {
        srv.stack.clear_state_request(w);
    }
    drain_stack_events(srv);
    match mgr_outcome {
        MgrOutcome::Applied { window, dirty } => {
            // **The manager has acted on this window, so its held configure is answered.**
            // A manager that only wants to position a window sends `Place` and nothing else;
            // making it wait out the deadline would mean every launch is slow by design. The
            // configure goes out carrying the origin the manager just set.
            // `None` is a request that named no window — `SetCurrentDesktop` — so there is
            // no held configure to release.
            if let Some(window) = window {
                release_configure(srv, fb, window);
            }
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
        MgrOutcome::RegisterHotkey(hk) => {
            // **Applied here because the table is the router's**, and answered with the same
            // empty body every other manager request gets: the manager chose the id, so there
            // is nothing to tell it back. A refusal is a refusal — never a silent replacement,
            // which would leave a manager holding two chords under one id and wondering why
            // one of them never fires.
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
                    // **`InvalidArgument` for all three, which is what the spec publishes and
                    // what `surface_errno` maps `Rejected` to for every other request here.**
                    // The first version answered a duplicate id and a full table with
                    // `WouldBlock` — and this server uses `WouldBlock` for a genuinely
                    // transient condition, a `manage` resolve arriving while another manager
                    // holds the channel. There is no unregister, so a full table never empties:
                    // a manager that read "busy, retry" would retry forever (PR #241 review,
                    // finding 4).
                    kprint(b"compositor: a hotkey registration was refused\n");
                    reply_error_on_session(ch, op, request_id, KError::InvalidArgument);
                }
            }
        }
        MgrOutcome::Configure { window, width, height, origin } => {
            // Forwarded to the window's *client*, which is a third party: the manager asked,
            // the client is told. Nothing changes on screen until that client commits, so the
            // reply says the compositor accepted and queued the request — never that the
            // client adopted it, which is not knowable here.
            // **A manager `Configure` on a held window *is* its initial configure**, which is
            // why the op carries an origin as well as a size: one message answers both halves,
            // so a window neither jumps nor resizes after it is first painted. A manager that
            // wants to set both should use this rather than `Place` followed by `Configure` —
            // the first of those pair releases the hold, and the second then arrives as an
            // ordinary later configure, after first paint.
            // The manager's answer *is* this window's initial configure, so the hold is
            // discharged here rather than by `release_configure` — the configure below carries
            // the manager's geometry, which is more than the stack knows.
            let was_held = srv.pending_configure.iter().any(|&(w, _)| w == window);
            srv.pending_configure.retain(|&(w, _)| w != window);
            let became_visible = srv.stack.mark_configured(window);
            // **The origin is applied, not merely forwarded.** The comment above has always said
            // this op answers "where does this go" as well as "how big", and told managers to use
            // it rather than `Place` followed by `Configure` — while nothing here wrote the
            // origin, so a manager that followed that advice set only the size. Part B is the
            // first production caller, so it was never exercised: maximise would have resized a
            // window it could not move, leaving it hanging off the screen from wherever it was
            // dragged (PR #249 review, blocking 2).
            //
            // Through `place`, so the same rules apply as to any other placement — including the
            // refusal for a window the user is dragging, which a manager must not fight.
            let moved = match srv.stack.place(window, origin) {
                Ok(d) => Some(d.rect()),
                Err(compositor::StackError::Dragging) => {
                    return reply_error_on_session(ch, op, request_id, KError::WouldBlock);
                }
                Err(_) => None,
            };
            // **Drained again, because this arm mutates after the drain above.** Every other
            // manager op does its work inside `dispatch`, which is why one drain immediately
            // after it has always been enough; this one places from out here, so the geometry
            // change it produces would otherwise wait for the next unrelated request — the same
            // gap the input path had in Part A.
            drain_stack_events(srv);
            if configure_window(srv, window, width, height, origin) {
                if let Some(r) = moved.filter(|r| !r.is_empty()) {
                    repaint_region(srv, fb, r);
                }
                if became_visible || was_held {
                    // Newly drawable, and newly a focus candidate — see `release_configure`.
                    let r = srv.stack.window(window).map(|w| w.bounds());
                    if let Some(r) = r {
                        repaint_region(srv, fb, r);
                    }
                    announce_focus(srv);
                }
                reply_on_session(ch, op, request_id, &[]);
            } else {
                // No session owns that window, so nobody can be told: a refusal, not a
                // success with nothing behind it.
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
///
/// **A capability by binding, and in Milestone 6 that binding gates nothing** — `/dev/draw` is
/// bound unscoped into init's root namespace and every graphical client inherits it, so any of
/// them could ask. What actually separates them here is *order*: the intended manager resolves
/// first. That is written down as an ordering rather than dressed up as a capability —
/// Closed by M7 Part E's per-application namespaces: an application's binds `/dev/draw/new`
/// alone, so `manage` resolves to nothing there.
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
///
/// **Clears the event queue with it.** Anything still queued describes windows as they were
/// while the departed manager was watching; handing that backlog to whoever attaches next
/// would tell a fresh manager about creations it never saw and, after enough churn, about
/// windows that no longer exist — with no resync op to repair the picture.
fn close_manager(srv: &mut Server, fb: &mut RawFramebuffer) {
    srv.mgr_outbox.clear();
    // **The chord table goes with it**, for the reason the queued events do: it is routing
    // policy the departed manager asked for. Left behind, every registered chord keeps being
    // consumed and delivered to nobody — `mgr_emit` early-returns with no channel — so the key
    // silently reaches nothing for the life of the compositor, and a replacement manager
    // inherits the dead one's ids and is refused its own (PR #241 review, finding 3).
    srv.router.clear_hotkeys();
    // **Every window it was going to place is shown now, not after the deadline.** The clients
    // holding those windows are blocked, and waiting out a timer for a manager that has
    // demonstrably gone is latency bought for nothing.
    let waiting: alloc::vec::Vec<u32> = srv.pending_configure.iter().map(|&(w, _)| w).collect();
    for window in waiting {
        release_configure(srv, fb, window);
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
    drain_stack_events(srv);
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
    // **The mapping this replaces goes first, not last.** Re-attaching an id is a resize
    // (M9 Part D), and `dispatch` has already accepted the *new* geometry for it — so a
    // moment where the old, smaller mapping is still what `Server::pixels` finds is a
    // moment where compositing reads a large geometry out of a small allocation.
    // `Vec::retain` drops the record, and dropping a `MappedBuffer` unmaps it.
    //
    // Dropping it before the new mapping is also what makes a *failed* map safe: the window
    // is left with no pixels for that buffer, which composites as nothing, rather than with
    // the wrong ones.
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
        // **`WouldBlock`, because it is true again in a moment.** A `Place` refused because the
        // user is dragging that window is not a malformed request and not a permanent no: the
        // manager asked for something reasonable at a moment when the pointer owns the window.
        // `InvalidArgument` would tell it to fix its request, which is not the problem.
        SurfaceError::Rejected(compositor::StackError::Dragging) => KError::WouldBlock,
        SurfaceError::Rejected(_) => KError::InvalidArgument,
    }
}

/// The screen and the work area, as they are now.
///
/// **The work area is the compositor's to compute** — every `panel` declares a strut and only
/// this process sees all of them. A manager that subtracted its own bars would be right only for
/// as long as it owned every panel.
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

/// Tell the manager when the work area is not what it was.
///
/// **Compared rather than triggered**, and the comparison is the whole design: what a manager
/// needs to know is that the answer changed, not why. Today only a panel appearing or going away
/// can change it — a window's role, and so its strut, is written once at creation — so a
/// cause-driven version would have exactly two triggers and would silently grow a third the day
/// a strut becomes settable.
fn announce_layout(srv: &mut Server) {
    let now = current_layout(srv);
    if srv.last_layout == Some(now) {
        return;
    }
    srv.last_layout = Some(now);
    mgr_emit(srv, MgrEvent::LayoutChanged(now));
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

    // Copy the body out rather than holding a borrow of `RECV_MSG` across dispatch; the
    // alternative is two live `static mut` borrows at once. `MAX_BODY` is sized so a whole
    // `SetTitle` fits — this `min` is a bounds check, not a policy, and when it *was* a policy
    // it silently shortened titles to 60 bytes.
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

    // **`StartMove` is answered here rather than in `dispatch`**, for the reason `Capture` is:
    // it needs the router, which owns the pointer grab this request is checked against, and
    // `dispatch` deliberately sees only a connection and the stack. Ownership is checked the same
    // way every other op checks it — a window belonging to another connection answers `NotFound`,
    // so a reply cannot be used to probe for other clients' ids.
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
                    // **The catch-up is painted here.** The pointer has already moved by the
                    // time this request lands, so the window is somewhere new the instant the
                    // drag begins — and `Logical::Button` reports no movement, so a
                    // press-flick-release with no motion event after it would leave the window
                    // drawn where it used to be until something unrelated repainted
                    // (PR #248 review, finding 3).
                    if let Some(r) = damage.filter(|r| !r.is_empty()) {
                        repaint_region(srv, fb, r);
                    }
                    Line::new()
                        .s(b"compositor: interactive move of window ")
                        .u(req.window as u64)
                        .end();
                    None
                }
                // The caller does not hold the grab, or the window has gone. One answer for
                // both, like every other ownership question here — but **said out loud**,
                // because a refusal that logged nothing is a client whose drag silently does
                // not happen, and the gate that found this could not tell "refused" from
                // "never arrived".
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

    // **`RequestState` is forwarded, not applied.** Minimising is `Manage::SetMinimized` and
    // maximising is a `Configure` to a rectangle computed from the work area; both are manager
    // operations, and a client that could reach either could put another client's window away or
    // place itself. So the compositor's whole part is to check the caller owns the window and
    // hand the manager the question (M9 Part B).
    if op == librsproto::surface::OP_REQUEST_STATE {
        if handle != 0 {
            // SAFETY: closing a handle this process owns and will never interpret.
            unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        }
        let err = match librsproto::surface::WindowState::read(body) {
            None => Some(KError::InvalidArgument),
            Some(req) if req.state > librsproto::surface::WINDOW_STATE_MAXIMIZED => {
                Some(KError::InvalidArgument)
            }
            Some(req) if !srv.conns[slot].owns(req.window) => Some(KError::NotFound),
            Some(req) => {
                // **Repeats produce nothing**, which is the bound this event needs: it is the
                // only manager event a client's own rate drives, and the manager's queue does
                // not coalesce and discards its oldest — so a client looping on one state would
                // otherwise push a `WindowCreated` off the front of the shell's view of the
                // world. The same argument `SetTitle` already makes for an unchanged title.
                //
                // What it cannot dedup is *alternation*, and that is correct rather than a gap:
                // a window asked to maximise and then to restore has changed state twice, and a
                // manager that missed either would be wrong about where the window belongs.
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
    let outcome = unsafe {
        dispatch(
            &mut srv.conns[slot],
            &mut srv.stack,
            op,
            body,
            &mut REPLY_MSG[PAYLOAD_OFF + 16..],
        )
    };
    drain_stack_events(srv);

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
            // **The manager hears about it, with the role and the size the client asked for.**
            // An id alone is useless to a manager: a panel is not placed like a normal window,
            // a popup is placed by its own client, and centring needs a size. All of it is
            // already known here, so an event that made the shell ask a follow-up question
            // would be a seam with a round trip in it (M6 B3).
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
            // **Held when a manager is attached: this is M6 B4.** The client may not commit
            // until it has this configure, so holding it is what gives the manager a window of
            // opportunity to place the window *before* anyone sees it. Without the hold the
            // interval between `CreateWindow` and the first `Commit` is the client's — it
            // issues `AttachBuffer` and `Commit` back to back — and the manager, a different
            // process that must be woken and scheduled, loses that race often enough to make
            // every launch visibly jump.
            //
            // With no manager there is nobody to ask, so it goes out at once and nothing about
            // the client's behaviour changes.
            //
            // **A popup is never held**, however many managers are attached. Its position is
            // its creator's business — a menu drops from an item only the client knows the
            // position of — so there is nobody to wait for, and holding it would spend the full
            // deadline on the interaction most sensitive to latency: every menu open would cost
            // 200 ms the moment a shell attached (M6 C1).
            //
            // **A `dialog` is held like a `normal`.** It names a parent, but a manager places
            // it, so there *is* somebody to wait for. Exempting it alongside popups was C1
            // treating two roles as one because they share a wire shape; what they share is a
            // parent field, not a placement rule.
            let placed_by_creator = srv
                .stack
                .window(configure.window)
                .is_some_and(|w| matches!(w.role, librsproto::surface::Role::Popup { .. }));
            // SAFETY: reading our own manager slot.
            if sent && !placed_by_creator && unsafe { MANAGER_CH } != 0 {
                let mut now: u64 = 0;
                // SAFETY: a valid out-pointer for one u64.
                unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
                srv.pending_configure
                    .push((configure.window, now.saturating_add(CONFIGURE_DEADLINE_NS)));
            } else {
                let mut cfg = [0u8; core::mem::size_of::<ConfigureEvent>()];
                srv.stack.mark_configured(configure.window);
                if sent && configure.write(&mut cfg).is_some() && !send_input(ch, OP_CONFIGURE, &cfg)
                {
                    // The client will wait for this and nothing else will produce it, so a
                    // silent failure parks it forever — the same reason the reply above says so.
                    kprint(b"compositor: a window's first configure did not send\n");
                }
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
        Outcome::TitleSet { window } => {
            // Nothing on screen changed — the compositor draws no titles — so there is no
            // repaint here, only the manager event.
            let title = srv.stack.window(window).map(|w| w.title.clone()).unwrap_or_default();
            mgr_emit(srv, MgrEvent::Title { window, title });
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
        // **Every iteration, not only on a timeout.** Checked in the `TimedOut` arm alone, the
        // configure deadline would be a floor rather than a bound: any iteration where a handle
        // was ready would skip it, so a busy compositor could hold a window well past 200 ms.
        // It self-corrects at the first idle instant, which is why this is a bound rather than
        // a bug fix — but a deadline that only holds when nothing is happening is not one worth
        // documenting as 200 ms (PR #218 review, finding 8). Free when nothing is held.
        //
        // **Flushed here, not left to the next pass.** Releasing *queues* the configure, and
        // the client is blocked on it; going straight back to `sys_wait` with it unsent means
        // sleeping until something unrelated happens — and having just emptied the pending
        // list, there is no longer a deadline to wake for.
        if fire_configure_deadlines(srv, &mut fb) {
            parked = flush_outboxes(srv);
        }
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
            // A third source: a window still waiting for a manager that may never answer.
            for &(_, at) in &srv.pending_configure {
                deadline = deadline.min(at);
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
                    close_manager(srv, &mut fb);
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

        // A forwarded resolve: `new` mints a session, `manage` mints the manager channel
        // (one holder), `<N>/info` answers with window metadata. A bare `<N>` is a later
        // milestone; `<N>/ports/...` is unscheduled — see `TODO(port-shape-rework)`.
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
        pending_configure: alloc::vec::Vec::new(),
        announced_focus: None,
        screen: fb.geometry().bounds(),
        last_layout: None,
        outbox: (0..MAX_SESSIONS).map(|_| Outbox::new()).collect(),
        mgr_outbox: MgrOutbox::new(),
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
