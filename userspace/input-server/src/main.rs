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
//! 1. Open `/dev/input/raw/<n>` — **authority is the binding**, so the input server is
//!    simply the process whose namespace contains them, and nothing else's should
//!    (`input-subsystem.md` §5: that is a constraint on the supervisor, not on this code).
//! 2. Mint a forwarding channel pair; send `Meta::Ready` on the control channel transferring
//!    the kernel end, which the supervisor binds at `/dev/input/new`.
//! 3. Serve. A forwarded resolve of `new` mints a **consumer channel**: the server keeps its
//!    end and hands the other back as the resolve's answer, the directory-session shape
//!    `/dev/draw/new` and `/dev/tty` already use.
//!
//! ## The read loop is the interesting part
//!
//! Both devices are read with `sys_io_submit(Read)`, which returns a `PendingOperation` —
//! and a PO is waitable, so one `sys_wait` covers the forwarding endpoint, both outstanding
//! reads and every consumer channel. There is no polling and no thread per device.
//!
//! Each wakeup harvests whatever completed, merges it (`input_server::merge`), and forwards
//! one batch. Ordering is **batch-scoped** — see `docs/spec/rsproto-input-ops.md`, which
//! states it normatively and says why a global order is not on offer.

#![no_std]
#![no_main]

use input_server::{BATCH_MAX, Consumer, FRAME_MAX, PER_DEVICE, merge};
use libkern::abi::{INPUT_EVENT_LEN, InputEvent};
use libkern::error::KError;
use libkern::{
    CLOCK_MONOTONIC, IO_OPCODE_READ, IoOp, RIGHT_MAP_READ, RIGHT_MAP_WRITE, RIGHT_READ,
    SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_CLOCK_READ,
    SYS_HANDLE_CLOSE, SYS_IO_SUBMIT, SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_NS_LOOKUP, SYS_WAIT,
    exit, kprint, syscall2, syscall4, syscall5,
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

/// Raw device nodes this server reads.
const DEVICES: [&[u8]; 2] = [b"/dev/input/raw/0", b"/dev/input/raw/1"];
/// Index of the keyboard within [`DEVICES`].
const KBD: usize = 0;
/// Index of the mouse.
const MOUSE: usize = 1;

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

/// How long to wait before retrying a consumer that is holding deferred motion.
///
/// Short enough that a person cannot see the catch-up, long enough that a stalled consumer costs
/// a handful of wakeups rather than a spin: the compositor's worst recompose is ~100 ms under
/// emulation, so this is about twenty attempts across one, each of which is a `sys_wait` return
/// and one non-blocking send.
const FLUSH_INTERVAL_NS: u64 = 5_000_000;

/// Scratch for [`now_ns`].
static mut CLOCK_BUF: u64 = 0;

/// The monotonic clock, in nanoseconds.
///
/// Only the flush path needs this: every event the devices produce already carries the time its
/// interrupt fired.
fn now_ns() -> u64 {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: on success the kernel wrote the ns count.
    unsafe { (&raw const CLOCK_BUF).read() }
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

/// One raw device: its node, its read buffer, and the read currently outstanding on it.
struct Device {
    node: u64,
    buf_h: u64,
    buf_addr: u64,
    /// The in-flight read's `PendingOperation`, or `0` when none is outstanding.
    po: u64,
}

impl Device {
    /// Open `path` and map a read buffer for it.
    fn open(root_ns: u64, path: &[u8]) -> Option<Self> {
        let node = lookup(root_ns, path, RIGHT_READ)?;
        // SAFETY: register-only syscall.
        let buf_h = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
        if buf_h <= 0 {
            return None;
        }
        // SAFETY: a fresh `MemoryObject` handle with full MAP rights.
        let addr = unsafe {
            syscall4(SYS_MEMORY_MAP, buf_h as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
        };
        if addr <= 0 {
            return None;
        }
        Some(Self { node, buf_h: buf_h as u64, buf_addr: addr as u64, po: 0 })
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
    devices: [Device; 2],
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
            Some(n) => srv.consumers[i].defer(&framed[..n]),
            // It did not fit, so nothing was cleared and the batch itself is the whole debt.
            None => srv.consumers[i].defer(batch),
        }
    }
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

/// The serve loop: the forwarding endpoint, both device reads, and every consumer channel,
/// all under one `sys_wait`.
fn serve_loop(serve_end: u64, srv: &mut Server) -> ! {
    kprint(b"input-server: serving /dev/input/new\n");
    let mut kbd_buf = [InputEvent::default(); PER_DEVICE];
    let mut mouse_buf = [InputEvent::default(); PER_DEVICE];
    let mut batch = [InputEvent::default(); BATCH_MAX];

    loop {
        srv.devices[KBD].arm();
        srv.devices[MOUSE].arm();

        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots; `n` is bounded by
        // 1 + 2 + MAX_CONSUMERS, far inside it.
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
            for d in &srv.devices {
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
            let deadline = if srv.consumers.iter().any(Consumer::owes_send) {
                now_ns().saturating_add(FLUSH_INTERVAL_NS)
            } else {
                u64::MAX
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
                forward(srv, &[], now_ns());
                continue;
            }
            // Otherwise the error path — retrying it silently spins the loop at 100% with
            // nothing to show for it.
            kprint(b"input-server: sys_wait FAILED\n");
            continue;
        }

        let mut forward_pending = false;
        let mut kbd_n = 0usize;
        let mut mouse_n = 0usize;
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
            } else if h == srv.devices[KBD].po {
                kbd_n = srv.devices[KBD].harvest(&mut kbd_buf);
            } else if h == srv.devices[MOUSE].po {
                mouse_n = srv.devices[MOUSE].harvest(&mut mouse_buf);
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
                    // SAFETY: closing our end of a channel whose peer is gone.
                    unsafe { syscall4(SYS_HANDLE_CLOSE, srv.channels[slot], 0, 0, 0) };
                    srv.channels[slot] = 0;
                } else if rr == 0 {
                    // A message from a consumer. This category has no consumer→server op
                    // yet, so drain and ignore rather than guess — but say so, because a
                    // client sending into silence is worth one line in the log.
                    kprint(b"input-server: ignoring an unexpected message from a consumer\n");
                }
            }
        }

        if kbd_n > 0 || mouse_n > 0 {
            let n = merge(&kbd_buf[..kbd_n], &mouse_buf[..mouse_n], &mut batch);
            if n > 0 {
                // The stamp on the loss marker and on recovered motion only: every real event
                // already carries the time its interrupt fired, which is the point of `time_ns`.
                let now = batch[n - 1].time_ns;
                forward(srv, &batch[..n], now);
            }
        } else if srv.consumers.iter().any(Consumer::owes_send) {
            // Woken by something else — a resolve, a consumer's message — with motion still
            // owed. Sending it now costs one message and saves a whole flush interval.
            forward(srv, &[], now_ns());
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

    // Authority is the binding: this server is the process whose namespace holds the raw
    // nodes. If they are absent there is nothing to serve — a machine with no i8042 boots
    // without an input server rather than with a broken one.
    let Some(kbd) = Device::open(root_ns, DEVICES[KBD]) else {
        kprint(b"input-server: no /dev/input/raw/0 -- cannot serve\n");
        exit(1);
    };
    let Some(mouse) = Device::open(root_ns, DEVICES[MOUSE]) else {
        kprint(b"input-server: no /dev/input/raw/1 -- cannot serve\n");
        exit(1);
    };
    let mut srv = Server {
        devices: [kbd, mouse],
        channels: [0; MAX_CONSUMERS],
        consumers: [Consumer::new(); MAX_CONSUMERS],
    };

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
