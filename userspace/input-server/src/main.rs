//! `input-server` — the userspace resource server behind `/dev/input/new` (plan M3 Part B).
//!
//! **Everything that decides is in the library half** ([`input_server`]): the merge and what
//! a slow consumer is owed, both host-tested without a kernel. This file is the parts that
//! cannot be: reading the raw device nodes, serving forwarded resolves, and sending on
//! channels.
//!
//! ## Shape
//!
//! Modelled on `tty-server`, which sits over `/dev/console` the same way:
//!
//! 1. **Subscribe to `/svc/devices/input`** (administration Part B.3). The device manager
//!    replays every keyboard and mouse as an `Arrived` carrying its node, then says `Settled`
//!    (`docs/spec/rsproto-devices-ops.md`). The class has one owner, and this is it: a second
//!    reader of a raw device would drain events meant for the first (`input-subsystem.md` §5).
//! 2. **Serve from `Settled`, with whatever arrived** — none included. A machine without a
//!    mouse, or without the manager, still has an input server, and later arrivals join it.
//!    Mint a forwarding channel pair; send `Meta::Ready` on the control channel transferring
//!    the kernel end, which the supervisor binds at `/dev/input/new`.
//! 3. Serve. A forwarded resolve of `new` mints a **consumer channel**: the server keeps its
//!    end and hands the other back as the resolve's answer, the directory-session shape
//!    `/dev/draw/new` and `/dev/tty` already use.
//!
//! ## The read loop is the interesting part
//!
//! Every device is read with `sys_io_submit(Read)`, which returns a `PendingOperation` —
//! and a PO is waitable, so one `sys_wait` covers the forwarding endpoint, the subscription,
//! every outstanding read and every consumer channel. There is no polling and no thread per
//! device.
//!
//! Each wakeup harvests whatever completed, merges it (`input_server::merge`), and forwards
//! it — one batch for a keyboard and a mouse, more for more devices, each ending on a group
//! boundary. Ordering is **batch-scoped** — see `docs/spec/rsproto-input-ops.md`, which
//! states it normatively and says why a global order is not on offer.

#![no_std]
#![no_main]

use input_server::devices::{Arrival, Notice, Table, notice};
use input_server::{BATCH_MAX, Consumer, FRAME_MAX, MAX_DEVICES, MERGE_MAX, PER_DEVICE, batches, merge};
use libkern::abi::{INPUT_EVENT_LEN, InputEvent};
use libkern::debug::Line;
use libkern::device::DeviceKind;
use libkern::error::KError;
use libkern::{
    CLOCK_MONOTONIC, IO_OPCODE_READ, IoOp, RIGHT_MAP_READ, RIGHT_MAP_WRITE, RIGHT_RECV,
    RIGHT_SEND, RIGHT_WAIT, SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_RECV,
    SYS_CHANNEL_SEND, SYS_CLOCK_READ, SYS_HANDLE_CLOSE, SYS_IO_SUBMIT, SYS_MEMORY_CREATE,
    SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_NS_LOOKUP, SYS_WAIT, exit, kprint, syscall2, syscall4,
    syscall5,
};
use librsproto::namespace::{OBJECT_KIND_CHANNEL, resolve_reply};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

/// `alloc` backing. Nothing here allocates on the event path; the heap exists because
/// `librsproto` and the panic path expect one.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC message buffer length.
const MSG_LEN: usize = 4096;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;
/// One page for each device's read buffer.
const PAGE: u64 = 4096;

/// Where the device manager hands over input devices: resolving it is the subscription.
const SUBSCRIPTION: &[u8] = b"/svc/devices/input";

/// How long to wait for the manager's `Settled` before serving with what has arrived.
///
/// The manager queues the whole replay before the subscription's resolve completes, so `Settled`
/// is waiting the moment the channel is; this bounds a manager that is not behaving, well inside
/// the thirty seconds `init` gives a `Ready`. Anything that arrives after it still joins.
const SETTLE_TIMEOUT_NS: u64 = 5_000_000_000;

/// Consumers served at once.
///
/// One is the compositor; the rest leave room for a hotkey daemon or a diagnostic reader
/// without making the wait set interesting.
const MAX_CONSUMERS: usize = 4;

/// Messages a consumer's ring holds before a send starts failing.
///
/// **Sized against a repaint, not against the mouse.** One group is one message, so a 100 Hz
/// mouse fills four slots in 40 ms — less than a single full-screen recompose takes the
/// compositor under emulation. Deferral (see [`Consumer`]) is what makes the *cursor* immune to
/// that, but a key or a button in an overflowing batch is still a real gap, and a gap resets the
/// consumer's modifier and grab state. Sixteen covers a repaint at TCG speeds; each slot is a
/// 4 KiB kernel message, so this is 64 KiB per consumer and worth it.
const CONSUMER_QUEUE_DEPTH: u64 = 16;

/// Messages the control ring holds. Requests here are rare and answered immediately.
const CONTROL_QUEUE_DEPTH: u64 = 4;

/// How many overruns to report before going quiet.
///
/// A bound rather than a rate limit: the interesting fact is *that* a consumer overran, and a
/// consumer that has stopped reading for good would otherwise print one of these every 5 ms.
const MAX_LOGGED_OVERRUNS: u32 = 16;

/// How many overruns have been reported. See [`MAX_LOGGED_OVERRUNS`].
static mut OVERRUNS_LOGGED: u32 = 0;

/// How long to wait before retrying a consumer that is holding deferred motion.
///
/// Short enough that a person cannot see the catch-up, long enough that a stalled consumer costs
/// a handful of wakeups rather than a spin: the compositor's worst recompose is ~100 ms under
/// emulation, so this is about twenty attempts across one, each of which is a `sys_wait` return
/// and one non-blocking send.
const FLUSH_INTERVAL_NS: u64 = 5_000_000;

/// Scratch for [`now_ns`].
static mut CLOCK_BUF: u64 = 0;

/// The monotonic clock in nanoseconds, or `None` if the read failed.
///
/// Only the flush path needs this: every event the devices produce already carries the time its
/// interrupt fired.
///
/// **The return is checked, and the second `SAFETY` used to assert what it did not check.**
/// `CLOCK_MONOTONIC` with a valid out-pointer cannot fail today, so this is defensive — but the
/// failure it defends against is not benign: `CLOCK_BUF` would keep a stale value, the flush
/// deadline would land in the past on every iteration, and the 5 ms retry would become a spin
/// (PR #246 review, optional 9). Callers turn `None` into "wait indefinitely" instead.
fn now_ns() -> Option<u64> {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    let r = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    if r != 0 {
        return None;
    }
    // SAFETY: the call succeeded, so the kernel wrote the ns count.
    Some(unsafe { (&raw const CLOCK_BUF).read() })
}

static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] = [0; libkern::abi::IPC_HANDLE_MAX];
static mut RECV_COUNT: u64 = 0;
static mut SEND_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] = [0; libkern::abi::IPC_HANDLE_MAX];
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] =
    [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] =
    [0; 24 * libkern::abi::MAX_WAIT_HANDLES];

/// Close `h` if it is a handle at all.
fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process owns and will not use again.
        unsafe { syscall4(SYS_HANDLE_CLOSE, h, 0, 0, 0) };
    }
}

/// One device: its node, its read buffer, and the read currently outstanding on it.
struct Device {
    node: u64,
    buf_h: u64,
    buf_addr: u64,
    /// The in-flight read's `PendingOperation`, or `0` when none is outstanding.
    po: u64,
}

impl Device {
    /// Take `node` — the handle an `Arrived` carried — and map a read buffer for it. `None`, with
    /// the node closed, if the buffer cannot be had.
    fn adopt(node: u64) -> Option<Self> {
        // SAFETY: register-only syscall.
        let buf_h = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
        if buf_h <= 0 {
            close(node);
            return None;
        }
        // SAFETY: a fresh `MemoryObject` handle with full MAP rights.
        let addr = unsafe {
            syscall4(SYS_MEMORY_MAP, buf_h as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
        };
        if addr <= 0 {
            close(buf_h as u64);
            close(node);
            return None;
        }
        Some(Self { node, buf_h: buf_h as u64, buf_addr: addr as u64, po: 0 })
    }

    /// Let the device go: its read, its buffer and its node.
    ///
    /// **A read still parked in the kernel is safe to walk away from**: the driver holds its own
    /// reference to the buffer object (`ps2`'s `submit_read` clones it), so what it writes lands
    /// in memory the kernel keeps alive, not in this process's freed mapping.
    fn retire(self) {
        close(self.po);
        // SAFETY: unmapping this device's own read buffer, which nothing here reads again.
        unsafe { syscall2(SYS_MEMORY_UNMAP, self.buf_addr, PAGE) };
        close(self.buf_h);
        close(self.node);
    }

    /// Submit a read if none is outstanding. Idempotent, so the caller can call it after
    /// every harvest without tracking state.
    fn arm(&mut self) {
        if self.po != 0 {
            return;
        }
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: self.buf_h,
            buf_offset: 0,
            offset: 0,
            length: (PER_DEVICE * INPUT_EVENT_LEN) as u64,
        };
        // SAFETY: a char `DeviceNode` with READ, and a valid `IoOp`.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.node, (&op as *const IoOp) as u64) };
        if po > 0 {
            self.po = po as u64;
        } else {
            // The device drops out of the wait set until something else wakes the loop, and
            // on a quiet machine nothing may — so this is a device that stops delivering.
            // Diagnosable rather than silent (PR #179 review, finding 8).
            kprint(b"input-server: read submit FAILED -- device may stall\n");
        }
    }

    /// Harvest a completed read into `out`, returning how many events arrived.
    ///
    /// Consumes the `PendingOperation` handle, so the next [`arm`](Self::arm) submits a
    /// fresh read.
    fn harvest(&mut self, out: &mut [InputEvent]) -> usize {
        if self.po == 0 {
            return 0;
        }
        let (status, result) = po_completion(self.po);
        // SAFETY: closing this read's PO; a new one is created by the next `arm`.
        unsafe { syscall4(SYS_HANDLE_CLOSE, self.po, 0, 0, 0) };
        self.po = 0;
        if status != 0 {
            kprint(b"input-server: read completed with an error; events lost\n");
            return 0;
        }
        let n = (result as usize / INPUT_EVENT_LEN).min(out.len());
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            let base = self.buf_addr + (i * INPUT_EVENT_LEN) as u64;
            // SAFETY: `base + INPUT_EVENT_LEN` is within the mapped page — `n` is bounded by
            // `PER_DEVICE`, and the node delivers whole records only (`io-operation.md`,
            // record-stream char devices).
            let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, INPUT_EVENT_LEN) };
            match InputEvent::read(bytes) {
                Some(e) => *slot = e,
                None => return i,
            }
        }
        n
    }
}

/// Resolve `path`, blocking on the lookup's `PendingOperation`.
fn lookup(ns: u64, path: &[u8], rights: u64) -> Option<u64> {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po <= 0 {
        return None;
    }
    let po = po as u64;
    wait_one(po);
    let (status, resolved) = po_completion(po);
    // SAFETY: the lookup's PO is done with.
    unsafe { syscall4(SYS_HANDLE_CLOSE, po, 0, 0, 0) };
    if status != 0 || resolved == 0 { None } else { Some(resolved) }
}

/// Block until `h` signals.
fn wait_one(h: u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers for one waiter.
    unsafe {
        WAIT_HANDLES[0] = h;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        );
    }
}

/// Read a completed `PendingOperation`'s `(status, result)` from the last wait's records.
///
/// Re-waits with an immediate deadline rather than trusting a stale record: a PO that has
/// completed is **level**-signalled and is never consumed by a wait (unlike an
/// `InterruptObject`), so this returns at once.
///
/// **It clobbers `WAIT_RESULTS[0..24]`, and that is only safe because of a fact worth
/// stating.** `serve_loop` is iterating that same buffer when it calls this. The inner wait
/// passes `count = 1`, so the kernel writes exactly one record — and the outer loop has
/// already read the record for the handle it is currently processing. Widening this call to
/// wait on more than one handle would silently corrupt the loop's iteration (PR #179 review,
/// which verified the current form and noted nothing said why it holds).
fn po_completion(po: u64) -> (i32, u64) {
    // SAFETY: one waiter, valid buffers; a completed PO is already signalled.
    let n = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            0,
        )
    };
    if n != 1 {
        return (-1, 0);
    }
    // SAFETY: the kernel wrote one 24-byte result record.
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

/// Create a connected channel pair with a `depth`-message ring each. Returns `(kernel_end,
/// serve_end)`.
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
    let Some(body_len) = librsproto::meta::ready(&mut body, b"input-server") else { return false };
    // SAFETY: SEND_MSG is a valid buffer; the rsproto message goes at PAYLOAD_OFF.
    let rs_len = unsafe {
        match encode(&mut SEND_MSG[PAYLOAD_OFF..], librsproto::OP_READY, 0, 0, &body[..body_len], 1)
        {
            Some(n) => n,
            None => return false,
        }
    };
    // SAFETY: stamp the IpcMsg header + handle slot, then send with one transfer.
    unsafe {
        SEND_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        SEND_MSG[8] = 1;
        SEND_HANDLES[0] = kernel_end;
        syscall5(
            SYS_CHANNEL_SEND,
            control,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Answer a forwarded resolve by handing back `client_end` as a channel.
fn reply_session(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    if resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0).is_none() {
        return false;
    }
    // SAFETY: SEND_MSG/SEND_HANDLES are valid; one handle rides the reply.
    unsafe {
        let Some(rs_len) =
            encode(&mut SEND_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1)
        else {
            return false;
        };
        SEND_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        SEND_MSG[8] = 1;
        SEND_HANDLES[0] = client_end;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Answer a forwarded resolve with an error.
fn reply_resolve_error(serve_end: u64, request_id: u64, err: KError) -> bool {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = librsproto::error::error_body(&mut ebody, err.as_i32(), 0, b"").unwrap_or(0);
    // SAFETY: SEND_MSG is valid; no handles transferred.
    unsafe {
        let Some(rs_len) = encode(
            &mut SEND_MSG[PAYLOAD_OFF..],
            OP_NS_RESOLVE,
            request_id,
            RS_FLAG_REPLY | RS_FLAG_ERROR,
            &ebody[..elen],
            0,
        ) else {
            return false;
        };
        SEND_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        SEND_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Send one `Input::Events` batch to a consumer. Returns whether it went.
fn send_events(channel: u64, events: &[InputEvent]) -> bool {
    // `FRAME_MAX`, not `BATCH_MAX`: what goes on the wire is a *framed* batch, which can carry
    // a loss marker and a recovered motion group in front of it.
    let mut body = [0u8; FRAME_MAX * INPUT_EVENT_LEN];
    let n = events.len() * INPUT_EVENT_LEN;
    if n > body.len() {
        return false;
    }
    for (i, e) in events.iter().enumerate() {
        let off = i * INPUT_EVENT_LEN;
        body[off..off + 2].copy_from_slice(&e.kind.to_le_bytes());
        body[off + 2..off + 4].copy_from_slice(&e.code.to_le_bytes());
        body[off + 4..off + 8].copy_from_slice(&e.value.to_le_bytes());
        body[off + 8..off + 16].copy_from_slice(&e.time_ns.to_le_bytes());
    }
    // SAFETY: SEND_MSG is valid; no handles transferred.
    unsafe {
        let Some(rs_len) =
            encode(&mut SEND_MSG[PAYLOAD_OFF..], librsproto::OP_INPUT_EVENTS, 0, 0, &body[..n], 0)
        else {
            return false;
        };
        SEND_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        SEND_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            channel,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Everything the serve loop owns.
struct Server {
    /// The devices, by slot — `table` says which registry id holds each.
    devices: [Option<Device>; MAX_DEVICES],
    table: Table,
    /// The subscription to [`SUBSCRIPTION`], or `0` when there is none: the manager was not
    /// there, or has gone. The devices already read stay read either way.
    subscription: u64,
    /// Consumer channel handles, `0` for a free slot.
    channels: [u64; MAX_CONSUMERS],
    /// What each consumer is owed, parallel to `channels`.
    consumers: [Consumer; MAX_CONSUMERS],
}

/// Mint a consumer channel and answer the resolve with it.
fn open_consumer(serve_end: u64, request_id: u64, srv: &mut Server) {
    let Some(slot) = srv.channels.iter().position(|&c| c == 0) else {
        reply_resolve_error(serve_end, request_id, KError::OutOfHandles);
        return;
    };
    let Some((consumer_end, server_end)) = make_channel(CONSUMER_QUEUE_DEPTH) else {
        reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
        return;
    };
    srv.channels[slot] = server_end;
    srv.consumers[slot] = Consumer::new();
    if !reply_session(serve_end, request_id, consumer_end) {
        // The reply failed, so the consumer never received its end: drop both rather than
        // leaving a slot occupied by a peer that does not exist.
        // SAFETY: closing two handles this process owns and will not use.
        unsafe {
            syscall4(SYS_HANDLE_CLOSE, consumer_end, 0, 0, 0);
            syscall4(SYS_HANDLE_CLOSE, server_end, 0, 0, 0);
        }
        srv.channels[slot] = 0;
    }
}

/// Whether any **live** consumer is owed a send.
///
/// **Live, which is the whole of the fix.** `forward` skips a slot whose channel is zero, so a
/// debt recorded against a consumer that then disconnected can never be cleared — and the two
/// callers of this decide whether to arm a 5 ms retry. Scanning `consumers` alone made a
/// graphical session that ended mid-debt into a permanent 200 Hz wakeup loop for the rest of the
/// boot, doing nothing on each pass (PR #246 review, finding 2). The reap also clears the debt;
/// this makes the question un-askable wrongly rather than merely answered right once.
fn owes_send(srv: &Server) -> bool {
    (0..MAX_CONSUMERS).any(|i| srv.channels[i] != 0 && srv.consumers[i].owes_send())
}

/// Forward one merged batch to every consumer.
///
/// A send that fails is **not** retried here and now: the records are handed back to the
/// consumer, which carries the relative motion among them forward and counts the rest as a gap
/// to announce with `SYN_DROPPED` (`rsproto-input-ops.md` § Loss). That is what lets this server
/// have no flow control — a consumer that falls behind is caught up on the next send rather than
/// stalling everyone else — and it is why the cursor cannot drift: **the motion is deferred, not
/// dropped**.
///
/// `batch` may be empty, which is the flush pass: a consumer that owes deferred motion is sent it
/// on the next wakeup rather than on the next thing the user happens to do.
fn forward(srv: &mut Server, batch: &[InputEvent], now_ns: u64) {
    let mut framed = [InputEvent::default(); FRAME_MAX];
    for i in 0..MAX_CONSUMERS {
        if srv.channels[i] == 0 {
            continue;
        }
        match srv.consumers[i].frame(batch, now_ns, &mut framed) {
            // Nothing new and nothing owed: an empty message would wake the consumer to read
            // no events, which on a flush pass is every consumer that was already up to date.
            Some(0) => {}
            Some(n) if send_events(srv.channels[i], &framed[..n]) => {}
            // **What was framed, not the batch it came from.** `frame` clears both debts as it
            // writes them, so handing back the batch would forget the marker and the recovered
            // motion that had just been prepended to it.
            Some(n) => {
                // **Said out loud, bounded.** An overrun is invisible from outside — the whole
                // point of deferral is that the consumer cannot tell — so a gate asserting that
                // motion survived one has no way to know an overrun happened, and would pass
                // just as well against a run where the ring never filled. That is exactly how
                // the first version of this guard passed under KVM (PR #246 review, blocking 1).
                // This line is its precondition.
                // SAFETY: single-threaded server; the counter is touched only from this loop.
                unsafe {
                    if OVERRUNS_LOGGED < MAX_LOGGED_OVERRUNS {
                        OVERRUNS_LOGGED += 1;
                        libkern::debug::Line::new()
                            .s(b"input-server: consumer ")
                            .u(i as u64)
                            .s(b" overran; ")
                            .u(n as u64)
                            .s(b" records deferred")
                            .end();
                    }
                }
                srv.consumers[i].defer(&framed[..n])
            }
            // It did not fit, so nothing was cleared and the batch itself is the whole debt.
            None => srv.consumers[i].defer(batch),
        }
    }
}

/// The word for `kind` in a log line.
fn kind_word(kind: DeviceKind) -> &'static [u8] {
    match kind {
        DeviceKind::Keyboard => b"keyboard",
        DeviceKind::Mouse => b"mouse",
        _ => b"device",
    }
}

/// Act on one message from the device manager, whose `handles` came with it. Returns whether it
/// was `Settled`.
fn apply(srv: &mut Server, n: Notice, handles: &[u64]) -> bool {
    let say = |id: u32, kind: DeviceKind, what: &[u8]| {
        Line::new().s(b"input-server: ").s(kind_word(kind)).s(b" ").u(id as u64).s(what).end();
    };
    match n {
        // `notice` classifies an arrival as one only with exactly one handle: its node.
        Notice::Arrived { id, kind } => match srv.table.arrive(id) {
            Arrival::Slot(slot) => match Device::adopt(handles[0]) {
                Some(d) => {
                    srv.devices[slot] = Some(d);
                    Line::new()
                        .s(b"input-server: reading ")
                        .s(kind_word(kind))
                        .s(b" ")
                        .u(id as u64)
                        .s(b" in slot ")
                        .u(slot as u64)
                        .end();
                }
                None => {
                    srv.table.depart(id);
                    say(id, kind, b" has no read buffer; not read");
                }
            },
            Arrival::Already => {
                close(handles[0]);
                say(id, kind, b" arrived again; the second node closed");
            }
            Arrival::Full => {
                close(handles[0]);
                say(id, kind, b": every slot is taken; not read");
            }
        },
        Notice::NotInput { id, kind } => {
            handles.iter().for_each(|&h| close(h));
            Line::new()
                .s(b"input-server: device ")
                .u(id as u64)
                .s(b" of kind ")
                .u(kind.as_u32() as u64)
                .s(b" is not input; refused")
                .end();
        }
        Notice::Settled(_) => return true,
        Notice::Departed(id) => {
            if let Some(slot) = srv.table.depart(id) {
                if let Some(d) = srv.devices[slot].take() {
                    d.retire();
                }
                Line::new().s(b"input-server: device ").u(id as u64).s(b" departed from slot ").u(slot as u64).end();
            }
        }
        Notice::Malformed => {
            handles.iter().for_each(|&h| close(h));
            kprint(b"input-server: a malformed message from the device manager; ignored\n");
        }
    }
    false
}

/// Take every message waiting on the subscription. Returns whether one of them was `Settled`.
///
/// **A manager that has gone takes nothing with it**: the devices already read keep being read,
/// and only arrivals stop — which is all the manager was for.
fn take_notices(srv: &mut Server) -> bool {
    let mut settled = false;
    while srv.subscription != 0 {
        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                srv.subscription,
                (&raw mut RECV_MSG) as u64,
                (&raw mut RECV_HANDLES) as u64,
                (&raw mut RECV_COUNT) as u64,
            )
        };
        if rr == KError::PeerClosed.as_i32() as i64 {
            kprint(b"input-server: the device manager has gone; the devices already read stay read\n");
            close(srv.subscription);
            srv.subscription = 0;
            break;
        }
        if rr != 0 {
            break;
        }
        let mut handles = [0u64; libkern::abi::IPC_HANDLE_MAX];
        // SAFETY: the kernel wrote the count, the header and that many handles, and this process
        // is single-threaded, so nothing writes the buffers while they are read here. The payload
        // read is bounded by the buffer.
        let (n, count) = unsafe {
            let count = ((&raw const RECV_COUNT).read() as usize).min(libkern::abi::IPC_HANDLE_MAX);
            let received: &[u64; libkern::abi::IPC_HANDLE_MAX] = &*(&raw const RECV_HANDLES);
            handles[..count].copy_from_slice(&received[..count]);
            let len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let payload = core::slice::from_raw_parts(
                (&raw const RECV_MSG[PAYLOAD_OFF]) as *const u8,
                len.min(MSG_LEN - PAYLOAD_OFF),
            );
            let n = match decode(payload) {
                Ok(m) => notice(m.op, m.body, count),
                Err(_) => Notice::Malformed,
            };
            (n, count)
        };
        settled |= apply(srv, n, &handles[..count]);
    }
    settled
}

/// Take the manager's replay — every keyboard and mouse it has — until `Settled`, or until
/// [`SETTLE_TIMEOUT_NS`] says to serve with what came. Returns whether it settled.
fn settle(srv: &mut Server) -> bool {
    let deadline = now_ns().map(|t| t.saturating_add(SETTLE_TIMEOUT_NS));
    while srv.subscription != 0 {
        if take_notices(srv) {
            return true;
        }
        // No clock, no deadline to wait against: take what was queued and serve.
        let (Some(deadline), true) = (deadline, srv.subscription != 0) else { break };
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter, with a deadline.
        let waited = unsafe {
            WAIT_HANDLES[0] = srv.subscription;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline)
        };
        if waited < 1 {
            break;
        }
    }
    false
}

/// Handle a forwarded resolve on the serving endpoint. Returns `false` if the endpoint died.
fn serve_forward(serve_end: u64, srv: &mut Server) -> bool {
    // SAFETY: valid recv out-params.
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
        return rr != KError::PeerClosed.as_i32() as i64;
    }
    // SAFETY: bounded read of the payload, then of the resolve's suffix.
    let (request_id, is_new, ok) = unsafe {
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
                // **The empty suffix is the match**, not `b"new"`. This server is bound
                // at the *leaf* `/dev/input/new`, so a resolve of that path arrives with
                // nothing left over. The compositor's `b"new"` check looks similar and is
                // not the same case: it is bound at the subtree `/dev/draw`, so
                // `/dev/draw/new` leaves `new` behind. Binding a subtree at `/dev/input`
                // instead would collide with the kernel's `/dev/input/raw`.
                (m.request_id, suffix.is_empty(), true)
            }
            Ok(m) => (m.request_id, false, true),
            Err(_) => (0, false, false),
        }
    };
    if !ok {
        return true;
    }
    if is_new {
        open_consumer(serve_end, request_id, srv);
    } else {
        reply_resolve_error(serve_end, request_id, KError::NotFound);
    }
    true
}

/// The serve loop: the forwarding endpoint, the subscription, every device's read, and every
/// consumer channel, all under one `sys_wait`.
fn serve_loop(serve_end: u64, srv: &mut Server) -> ! {
    kprint(b"input-server: serving /dev/input/new\n");
    let mut harvested = [[InputEvent::default(); PER_DEVICE]; MAX_DEVICES];
    let mut merged = [InputEvent::default(); MERGE_MAX];

    loop {
        for d in srv.devices.iter_mut().flatten() {
            d.arm();
        }

        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots; `n` is bounded by
        // 1 + 1 + MAX_DEVICES + MAX_CONSUMERS, inside it.
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
            if srv.subscription != 0 {
                WAIT_HANDLES[n] = srv.subscription;
                n += 1;
            }
            for d in srv.devices.iter().flatten() {
                if d.po != 0 {
                    WAIT_HANDLES[n] = d.po;
                    n += 1;
                }
            }
            for &c in &srv.channels {
                if c != 0 {
                    WAIT_HANDLES[n] = c;
                    n += 1;
                }
            }
            // **Bounded while anything is owed.** A consumer that could not be sent to is
            // holding movement the user already made; waiting indefinitely would hold it until
            // the next event, so the cursor would stop short and then jump when the mouse was
            // next touched. With nothing owed this is still an indefinite sleep, not a poll.
            // A clock that will not answer means no deadline rather than one in the past: an
            // expired deadline is a spin, and waiting for the next event is a delay.
            let deadline = match owes_send(srv).then(now_ns).flatten() {
                Some(t) => t.saturating_add(FLUSH_INTERVAL_NS),
                None => u64::MAX,
            };
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                n as u64,
                (&raw mut WAIT_RESULTS) as u64,
                deadline,
            )
        };
        if waited < 1 {
            // **A timeout is the flush pass**, not an error: the deadline above is set only
            // when a consumer owes deferred motion, so waking with no handle ready means it is
            // time to try that consumer again.
            if waited == KError::TimedOut.as_i32() as i64 {
                // The stamp only reaches records this server synthesises — a loss marker and a
                // recovered motion group — and nothing reads it today, so a clock that will not
                // answer costs a zero rather than a dropped flush.
                forward(srv, &[], now_ns().unwrap_or(0));
                continue;
            }
            // Otherwise the error path — retrying it silently spins the loop at 100% with
            // nothing to show for it.
            kprint(b"input-server: sys_wait FAILED\n");
            continue;
        }

        let mut forward_pending = false;
        // **The subscription is read after the harvest, not during it.** A departure closes a
        // device's read, and a later record in this same wait's results could name that handle.
        let mut notices_pending = false;
        let mut counts = [0usize; MAX_DEVICES];
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
                forward_pending = true;
            } else if srv.subscription != 0 && h == srv.subscription {
                notices_pending = true;
            } else if let Some(slot) = srv.devices.iter().position(|d| d.as_ref().is_some_and(|d| d.po == h)) {
                if let Some(d) = srv.devices[slot].as_mut() {
                    counts[slot] = d.harvest(&mut harvested[slot]);
                }
            } else if let Some(slot) = srv.channels.iter().position(|&c| c == h && c != 0) {
                // **A signal here means one of two things, and they must be told apart.**
                // The kernel signals an endpoint when its receive queue is non-empty *or*
                // its peer is gone. Treating both as "gone" — which this did first — cuts
                // the input stream of any consumer that ever sends, with no error to either
                // side: a Part C hotkey registration, a `Meta::QueryCaps`, or a stray reply
                // from a client library would all silently unsubscribe it
                // (PR #179 review, finding 4). Nothing sends today, which is exactly why the
                // bug would have waited for the consumer that does.
                //
                // SAFETY: valid recv out-params on a live endpoint.
                let rr = unsafe {
                    syscall4(
                        SYS_CHANNEL_RECV,
                        srv.channels[slot],
                        (&raw mut RECV_MSG) as u64,
                        (&raw mut RECV_HANDLES) as u64,
                        (&raw mut RECV_COUNT) as u64,
                    )
                };
                if rr == KError::PeerClosed.as_i32() as i64 {
                    close(srv.channels[slot]);
                    srv.channels[slot] = 0;
                    // **And what it was owed goes with it.** A debt outlives its consumer
                    // otherwise: nothing can deliver it, and `owes_send` would keep arming the
                    // flush deadline over it forever.
                    srv.consumers[slot] = Consumer::new();
                } else if rr == 0 {
                    // A message from a consumer. This category has no consumer→server op
                    // yet, so drain and ignore rather than guess — but say so, because a
                    // client sending into silence is worth one line in the log.
                    kprint(b"input-server: ignoring an unexpected message from a consumer\n");
                }
            }
        }

        if counts.iter().any(|&c| c > 0) {
            let sources: [&[InputEvent]; MAX_DEVICES] = core::array::from_fn(|i| &harvested[i][..counts[i]]);
            let n = merge(&sources, &mut merged);
            // **One batch for a keyboard and a mouse; more, in order, for more devices** — each
            // ends on a group boundary, so no group is split across messages.
            for run in batches(&merged[..n], BATCH_MAX) {
                // The stamp on the loss marker and on recovered motion only: every real event
                // already carries the time its interrupt fired, which is the point of `time_ns`.
                //
                // **The batch's *first* timestamp, because both are written in front of it.**
                // Stamping them with the last one made `time_ns` run backwards inside a single
                // framed message, and `rsproto-input-ops.md` § Ordering invites a consumer to
                // sort by it for a total order — which would move the recovered motion after the
                // batch, the one placement `frame` documents as wrong. Nothing reads `time_ns`
                // today; that is a reason to fix it cheaply, not to leave it (PR #246 review,
                // optional 8).
                let stamp = merged[run.start].time_ns;
                forward(srv, &merged[run], stamp);
            }
        } else if owes_send(srv) {
            // Woken by something else — a resolve, a consumer's message — with motion still
            // owed. Sending it now costs one message and saves a whole flush interval.
            forward(srv, &[], now_ns().unwrap_or(0));
        }
        if notices_pending {
            take_notices(srv);
        }
        if forward_pending && !serve_forward(serve_end, srv) {
            kprint(b"input-server: forwarding endpoint closed\n");
            exit(1);
        }
    }
}

/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments; `ctrl` is the
/// control channel the supervisor spawned this server with.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, ctrl: u64) -> ! {
    kprint(b"input-server: up\n");

    let mut srv = Server {
        devices: [const { None }; MAX_DEVICES],
        table: Table::new(),
        subscription: 0,
        channels: [0; MAX_CONSUMERS],
        consumers: [Consumer::new(); MAX_CONSUMERS],
    };
    // **Devices come from the manager, and there is no fallback to the raw paths**
    // (administration Part B): a second path that runs only when the first is broken is a path
    // nobody tests. Without a manager, or with no devices, this still serves — a consumer gets a
    // stream with nothing in it, which is the truth — where it used to exit for want of a mouse.
    srv.subscription = lookup(root_ns, SUBSCRIPTION, RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT).unwrap_or(0);
    if srv.subscription == 0 {
        Line::new().s(b"input-server: could not subscribe at ").s(SUBSCRIPTION).s(b" -- serving no devices").end();
    } else {
        let settled = settle(&mut srv);
        Line::new()
            .s(b"input-server: ")
            .u(srv.table.len() as u64)
            .s(b" device(s) from the device manager")
            .s(if settled { b"" } else { b", which did not settle; serving them" })
            .end();
    }

    let Some((kernel_end, serve_end)) = make_channel(CONTROL_QUEUE_DEPTH) else {
        kprint(b"input-server: channel create FAIL\n");
        exit(1);
    };
    if !send_ready(ctrl, kernel_end) {
        kprint(b"input-server: Ready send FAIL\n");
        exit(1);
    }

    serve_loop(serve_end, &mut srv);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"input-server: panic\n");
    exit(2);
}
