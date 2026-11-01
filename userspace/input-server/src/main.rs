//! `input-server` — the userspace resource server behind `/dev/input/new`.
//!
//! Decisions live in the library half ([`input_server`]); this file reads device nodes, serves
//! resolves and sends.
//!
//! 1. Subscribe to `/svc/devices/input`: the manager replays every keyboard and mouse as an
//!    `Arrived` carrying its node, then `Settled` (`docs/spec/rsproto-devices-ops.md`). This is
//!    the class's one reader; a second would drain events meant for the first.
//! 2. Serve from `Settled` with whatever arrived, none included: send `Meta::Ready` transferring
//!    the kernel end of a forwarding pair, which the supervisor binds at `/dev/input/new`.
//! 3. A forwarded resolve of `new` mints a consumer channel; the other end is the answer.
//!
//! Every read is a waitable `PendingOperation`, so one `sys_wait` covers the forwarding endpoint,
//! the subscription, every read and every consumer: no polling, no thread per device. Ordering
//! is batch-scoped (`docs/spec/rsproto-input-ops.md`).

#![no_std]
#![no_main]

use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::Relaxed;
use input_server::devices::{Arrival, Notice, Table, notice};
use input_server::leaving::{Act, Leaving};
use input_server::{
    BATCH_MAX, Consumer, FRAME_MAX, MAX_DEVICES, MERGE_MAX, PER_DEVICE, batches, lights_request, merge,
};
use libkern::abi::{CTRL_OP_SHUTDOWN, INPUT_EVENT_LEN, InputEvent};
use libkern::channel_recv_raw;
use libkern::control::Control;
use libkern::debug::Line;
use libkern::device::DeviceKind;
use libkern::error::KError;
use libkern::{
    CLOCK_MONOTONIC, IO_OPCODE_READ, IO_OPCODE_WRITE, IoOp, RIGHT_MAP_READ, RIGHT_MAP_WRITE, RIGHT_RECV, RIGHT_SEND,
    RIGHT_WAIT, SENDMODE_NOBLOCK, SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE, SYS_IO_SUBMIT, SYS_MEMORY_CREATE, SYS_MEMORY_MAP,
    SYS_MEMORY_UNMAP, SYS_NS_LOOKUP, SYS_WAIT, exit, kprint, syscall2, syscall4, syscall5,
};
use librsproto::namespace::{OBJECT_KIND_CHANNEL, resolve_reply};
use librsproto::{OP_INPUT_LIGHTS, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

// Nothing allocates on the event path; `librsproto` and the panic path expect a heap.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const MSG_LEN: usize = 4096;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;
const PAGE: u64 = 4096;

/// Where the device manager hands over input devices: resolving it is the subscription.
const SUBSCRIPTION: &[u8] = b"/svc/devices/input";

/// How long to wait for `Settled` before serving with what arrived: it only bounds a
/// misbehaving manager, well inside the thirty seconds `service-mgr` gives a `Ready`.
const SETTLE_TIMEOUT_NS: u64 = 5_000_000_000;

/// Consumers served at once: the compositor, and room for a hotkey daemon or a diagnostic reader.
const MAX_CONSUMERS: usize = 4;

/// Messages a consumer's ring holds before a send starts failing.
///
/// Sized against a repaint: a 100 Hz mouse fills four slots in 40 ms, less than a recompose
/// under emulation. Deferral keeps the cursor exact, but a lost key or button resets the
/// consumer's modifier and grab state. Each slot is 4 KiB, so 64 KiB per consumer.
const CONSUMER_QUEUE_DEPTH: u64 = 16;

const CONTROL_QUEUE_DEPTH: u64 = 4;

/// Overruns to report before going quiet; a stopped consumer would print one every 5 ms.
const MAX_LOGGED_OVERRUNS: u32 = 16;

static OVERRUNS_LOGGED: AtomicU32 = AtomicU32::new(0);

/// Retry interval for a consumer holding deferred motion: short enough that a person cannot see
/// the catch-up, long enough that a stalled one costs a handful of wakeups rather than a spin
/// (about twenty across a ~100 ms emulated recompose).
const FLUSH_INTERVAL_NS: u64 = 5_000_000;

/// The monotonic clock in nanoseconds, or `None` if the read failed.
///
/// Checked because a failed read has no time to give, so the flush deadline would be in the past
/// and the 5 ms retry a spin; callers turn `None` into "wait indefinitely".
fn now_ns() -> Option<u64> {
    libkern::clock_read(CLOCK_MONOTONIC).ok()
}

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] = [0; libkern::abi::IPC_HANDLE_MAX];
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] = [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] = [0; 24 * libkern::abi::MAX_WAIT_HANDLES];

fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process owns and will not use again.
        unsafe { syscall4(SYS_HANDLE_CLOSE, h, 0, 0, 0) };
    }
}

/// One device: its node, read buffer and outstanding read.
struct Device {
    node: u64,
    /// Decides whether it is written the lights.
    kind: DeviceKind,
    /// Its registry id, which the manager's `Departed` names.
    id: u32,
    /// See [`input_server::leaving`].
    leaving: Leaving,
    /// Both its node and the manager have said it has gone: the serve loop retires its slot.
    retire_due: bool,
    buf_h: u64,
    buf_addr: u64,
    /// The in-flight read's `PendingOperation`, or `0` when none is outstanding.
    po: u64,
}

impl Device {
    /// Map a read buffer for `node`; `None`, with the node closed, if it cannot be had.
    fn adopt(node: u64, kind: DeviceKind, id: u32) -> Option<Self> {
        // SAFETY: register-only syscall.
        let buf_h = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
        if buf_h <= 0 {
            close(node);
            return None;
        }
        // SAFETY: a fresh `MemoryObject` handle with full MAP rights.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, buf_h as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
        if addr <= 0 {
            close(buf_h as u64);
            close(node);
            return None;
        }
        let (buf_h, buf_addr) = (buf_h as u64, addr as u64);
        Some(Self { node, kind, id, leaving: Leaving::Present, retire_due: false, buf_h, buf_addr, po: 0 })
    }

    /// A read still parked in the kernel is safe to abandon: the driver holds its own reference
    /// to the buffer object, so it never writes freed memory.
    fn retire(self) {
        close(self.po);
        // SAFETY: unmapping this device's own read buffer, which nothing here reads again.
        unsafe { syscall2(SYS_MEMORY_UNMAP, self.buf_addr, PAGE) };
        close(self.buf_h);
        close(self.node);
    }

    /// Its node answered `PeerClosed`: not read again, logged once.
    fn peer_closed(&mut self) {
        let (news, act) = self.leaving.peer_closed();
        if news {
            Line::new().s(b"input-server: ").s(kind_word(self.kind)).s(b" ").u(self.id as u64).s(b" left").end();
        }
        self.retire_due |= act == Act::Retire;
    }

    /// Submit a read if none is outstanding and its node is still read. Idempotent.
    fn arm(&mut self) {
        if self.po != 0 || !self.leaving.reading() {
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
        } else if po == KError::PeerClosed.as_i32() as i64 {
            // Its device has gone and its node has nothing left to read.
            self.peer_closed();
        } else {
            // The device drops out of the wait set and may stop delivering; say so.
            kprint(b"input-server: read submit FAILED -- device may stall\n");
        }
    }

    /// Harvest a completed read into `out`, consuming its `PendingOperation` so the next
    /// [`arm`](Self::arm) submits a fresh one. Returns how many events arrived.
    fn harvest(&mut self, out: &mut [InputEvent]) -> usize {
        if self.po == 0 {
            return 0;
        }
        let (status, result) = po_completion(self.po);
        // SAFETY: closing this read's PO; a new one is created by the next `arm`.
        unsafe { syscall4(SYS_HANDLE_CLOSE, self.po, 0, 0, 0) };
        self.po = 0;
        if status == KError::PeerClosed.as_i32() {
            // A read that was waiting when its device went, with no releases to answer it.
            self.peer_closed();
            return 0;
        }
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

fn wait_one(h: u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers for one waiter.
    unsafe {
        WAIT_HANDLES[0] = h;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX);
    }
}

/// Read a completed `PendingOperation`'s `(status, result)` from the last wait's records.
///
/// A completed PO is level-signalled and never consumed by a wait, so the immediate-deadline
/// re-wait returns at once. It clobbers `WAIT_RESULTS[0..24]` while `serve_loop` iterates that
/// buffer, which is safe because `count = 1` writes one record and the loop has already read
/// its current one; waiting on more handles here would corrupt it.
fn po_completion(po: u64) -> (i32, u64) {
    // SAFETY: one waiter, valid buffers; a completed PO is already signalled.
    let n = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, 0)
    };
    if n != 1 {
        return (-1, 0);
    }
    // SAFETY: the kernel wrote one 24-byte result record.
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

/// Returns `(kernel_end, serve_end)`, each with a `depth`-message ring.
fn make_channel(depth: u64) -> Option<(u64, u64)> {
    libkern::channel_create(depth).ok()
}

fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(body_len) = librsproto::meta::ready(&mut body, b"input-server") else { return false };
    // SAFETY: SEND_MSG is a valid buffer; the rsproto message goes at PAYLOAD_OFF.
    let rs_len = unsafe {
        match encode(&mut SEND_MSG[PAYLOAD_OFF..], librsproto::OP_READY, 0, 0, &body[..body_len], 1) {
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

fn reply_session(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    if resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0).is_none() {
        return false;
    }
    // SAFETY: SEND_MSG/SEND_HANDLES are valid; one handle rides the reply.
    unsafe {
        let Some(rs_len) = encode(&mut SEND_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1)
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

fn send_events(channel: u64, events: &[InputEvent]) -> bool {
    // `FRAME_MAX`, not `BATCH_MAX`: a framed batch also carries a loss marker and recovered motion.
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
        let Some(rs_len) = encode(&mut SEND_MSG[PAYLOAD_OFF..], librsproto::OP_INPUT_EVENTS, 0, 0, &body[..n], 0)
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
    devices: [Option<Device>; MAX_DEVICES],
    table: Table,
    /// The subscription to [`SUBSCRIPTION`], or `0` when the manager was not there or has gone.
    subscription: u64,
    /// Consumer channel handles, `0` for a free slot.
    channels: [u64; MAX_CONSUMERS],
    consumers: [Consumer; MAX_CONSUMERS],
    /// The control channel, for `service --stop`; `0` once its supervisor has gone, when it would
    /// stay signalled for good.
    control: u64,
    /// The lights a consumer last asked for; until one does, a keyboard keeps its driver's.
    lights: Option<u8>,
    /// The page a lights write is read from, `(handle, address)`.
    lights_buf: Option<(u64, u64)>,
}

fn lights_buffer(srv: &mut Server) -> Option<(u64, u64)> {
    if srv.lights_buf.is_none() {
        // SAFETY: register-only syscall.
        let h = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
        if h <= 0 {
            return None;
        }
        // SAFETY: a fresh `MemoryObject` handle with full MAP rights.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, h as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
        if addr <= 0 {
            close(h as u64);
            return None;
        }
        srv.lights_buf = Some((h as u64, addr as u64));
    }
    srv.lights_buf
}

/// Fire and forget: the driver reads the byte at submit, so one page serves every keyboard, and
/// nothing waits for the keyboard's answer.
fn write_lights(srv: &mut Server, d: &Device, lights: u8) {
    if d.kind != DeviceKind::Keyboard {
        return;
    }
    let Some((buf_h, buf_addr)) = lights_buffer(srv) else {
        kprint(b"input-server: no page for the lights; not written\n");
        return;
    };
    // SAFETY: the first byte of this process's own mapped lights page.
    unsafe { (buf_addr as *mut u8).write_volatile(lights) };
    let op = IoOp { opcode: IO_OPCODE_WRITE, flags: 0, buffer: buf_h, buf_offset: 0, offset: 0, length: 1 };
    // SAFETY: a char `DeviceNode` with WRITE, and a valid `IoOp`.
    let po = unsafe { syscall2(SYS_IO_SUBMIT, d.node, (&op as *const IoOp) as u64) };
    if po > 0 {
        close(po as u64);
    } else {
        Line::new().s(b"input-server: a keyboard refused its lights: ").i(po).end();
    }
}

fn set_lights(srv: &mut Server, lights: u8) {
    srv.lights = Some(lights);
    for slot in 0..MAX_DEVICES {
        if let Some(d) = srv.devices[slot].take() {
            write_lights(srv, &d, lights);
            srv.devices[slot] = Some(d);
        }
    }
}

fn consumer_lights() -> Option<u8> {
    // SAFETY: a bounded read of the payload the kernel just wrote.
    let payload = unsafe {
        let len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        core::slice::from_raw_parts(&raw const RECV_MSG[PAYLOAD_OFF], len.min(MSG_LEN - PAYLOAD_OFF))
    };
    let m = decode(payload).ok()?;
    if m.op != OP_INPUT_LIGHTS || m.flags & (RS_FLAG_REPLY | RS_FLAG_ERROR) != 0 {
        return None;
    }
    lights_request(m.body)
}

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
        // The consumer never received its end: drop both rather than hold a slot for no peer.
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
/// A debt against a disconnected consumer can never be cleared; counting it would arm the 5 ms
/// retry forever.
fn owes_send(srv: &Server) -> bool {
    (0..MAX_CONSUMERS).any(|i| srv.channels[i] != 0 && srv.consumers[i].owes_send())
}

/// Forward one merged batch to every consumer.
///
/// A failed send is not retried: the records go back to the consumer (`rsproto-input-ops.md`
/// § Loss), so there is no flow control and a slow consumer never stalls the others. An empty
/// `batch` is the flush pass.
fn forward(srv: &mut Server, batch: &[InputEvent], now_ns: u64) {
    let mut framed = [InputEvent::default(); FRAME_MAX];
    for i in 0..MAX_CONSUMERS {
        if srv.channels[i] == 0 {
            continue;
        }
        match srv.consumers[i].frame(batch, now_ns, &mut framed) {
            // Nothing new and nothing owed: an empty message would wake the consumer for nothing.
            Some(0) => {}
            Some(n) if send_events(srv.channels[i], &framed[..n]) => {}
            // Hand back what was framed, not the batch: `frame` cleared both debts as it wrote
            // them, so the batch alone would forget the marker and recovered motion.
            Some(n) => {
                // Deferral hides an overrun from the consumer, so a gate that asserts motion
                // survived one needs this line to know the ring really filled.
                let logged = OVERRUNS_LOGGED.load(Relaxed);
                if logged < MAX_LOGGED_OVERRUNS {
                    OVERRUNS_LOGGED.store(logged + 1, Relaxed);
                    libkern::debug::Line::new()
                        .s(b"input-server: consumer ")
                        .u(i as u64)
                        .s(b" overran; ")
                        .u(n as u64)
                        .s(b" records deferred")
                        .end();
                }
                srv.consumers[i].defer(&framed[..n])
            }
            // It did not fit, so nothing was cleared and the batch itself is the whole debt.
            None => srv.consumers[i].defer(batch),
        }
    }
}

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
            Arrival::Slot(slot) => match Device::adopt(handles[0], kind, id) {
                Some(d) => {
                    // A new keyboard is told the lights already on, or it would show Caps Lock off.
                    if let Some(l) = srv.lights {
                        write_lights(srv, &d, l);
                    }
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
        // Retired once its node has answered `PeerClosed` too: until then its ring may still hold
        // releases, so it is read.
        Notice::Departed(id) => {
            if let Some(slot) = srv.table.slot(id)
                && let Some(d) = srv.devices[slot].as_mut()
            {
                d.retire_due |= d.leaving.departed() == Act::Retire;
                if !d.retire_due {
                    let mut line = Line::new();
                    line.s(b"input-server: device ").u(id as u64).s(b" departing; reading what it has left").end();
                }
            }
            retire_due(srv);
        }
        Notice::Malformed => {
            handles.iter().for_each(|&h| close(h));
            kprint(b"input-server: a malformed message from the device manager; ignored\n");
        }
    }
    false
}

fn retire_due(srv: &mut Server) {
    for slot in 0..MAX_DEVICES {
        if !srv.devices[slot].as_ref().is_some_and(|d| d.retire_due) {
            continue;
        }
        if let Some(d) = srv.devices[slot].take() {
            srv.table.depart(d.id);
            Line::new().s(b"input-server: device ").u(d.id as u64).s(b" departed from slot ").u(slot as u64).end();
            d.retire();
        }
    }
}

/// Take every message waiting on the subscription. Returns whether one of them was `Settled`.
///
/// A manager that has gone takes nothing with it: devices already read stay read.
fn take_notices(srv: &mut Server) -> bool {
    let mut settled = false;
    while srv.subscription != 0 {
        // SAFETY: single-threaded; `RECV_MSG` is lent for the call.
        let mut sent = match unsafe { channel_recv_raw(srv.subscription, &raw mut RECV_MSG) } {
            Ok(sent) => sent,
            Err(rr) if rr == KError::PeerClosed.as_i32() as i64 => {
                kprint(b"input-server: the device manager has gone; the devices already read stay read\n");
                close(srv.subscription);
                srv.subscription = 0;
                break;
            }
            Err(_) => break,
        };
        // `apply` keeps or closes each of these.
        let count = sent.count();
        let handles = sent.take_all();
        // SAFETY: this process is single-threaded, so nothing writes the buffer while it is read
        // here. The payload read is bounded by the buffer.
        let n = unsafe {
            let len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let payload = core::slice::from_raw_parts(&raw const RECV_MSG[PAYLOAD_OFF], len.min(MSG_LEN - PAYLOAD_OFF));
            match decode(payload) {
                Ok(m) => notice(m.op, m.body, count),
                Err(_) => Notice::Malformed,
            }
        };
        settled |= apply(srv, n, &handles[..count]);
    }
    settled
}

/// Take the manager's replay until `Settled` or [`SETTLE_TIMEOUT_NS`]. Returns whether it settled.
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
    // SAFETY: single-threaded; `RECV_MSG` is lent for the call.
    let _sent = match unsafe { channel_recv_raw(serve_end, &raw mut RECV_MSG) } {
        Ok(sent) => sent,
        Err(rr) => return rr != KError::PeerClosed.as_i32() as i64,
    };
    // SAFETY: bounded read of the payload, then of the resolve's suffix.
    let (request_id, is_new, ok) = unsafe {
        let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(&raw const RECV_MSG[PAYLOAD_OFF], payload_len.min(MSG_LEN - PAYLOAD_OFF));
        match decode(req) {
            Ok(m) if m.op == OP_NS_RESOLVE => {
                let suffix = librsproto::namespace::parse_resolve_request(m.body).map(|r| r.suffix).unwrap_or(b"");
                // The empty suffix is the match: this server is bound at the leaf
                // `/dev/input/new`, so nothing is left over. (A subtree at `/dev/input` would
                // collide with the kernel's `/dev/input/raw`.)
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

fn serve_loop(serve_end: u64, srv: &mut Server) -> ! {
    kprint(b"input-server: serving /dev/input/new\n");
    let mut harvested = [[InputEvent::default(); PER_DEVICE]; MAX_DEVICES];
    let mut merged = [InputEvent::default(); MERGE_MAX];

    loop {
        for d in srv.devices.iter_mut().flatten() {
            d.arm();
        }
        // An arm refused `PeerClosed` may have made a slot due.
        retire_due(srv);

        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots; `n` is bounded by
        // 1 + 1 + 1 + MAX_DEVICES + MAX_CONSUMERS, inside it.
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
            if srv.control != 0 {
                WAIT_HANDLES[n] = srv.control;
                n += 1;
            }
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
            // Bounded only while something is owed; otherwise an indefinite sleep. No clock
            // means no deadline rather than one in the past, which would spin.
            let deadline = match owes_send(srv).then(now_ns).flatten() {
                Some(t) => t.saturating_add(FLUSH_INTERVAL_NS),
                None => u64::MAX,
            };
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, n as u64, (&raw mut WAIT_RESULTS) as u64, deadline)
        };
        if waited < 1 {
            // A timeout is the flush pass, not an error.
            if waited == KError::TimedOut.as_i32() as i64 {
                // The stamp reaches only synthesised records, so no clock costs a zero.
                forward(srv, &[], now_ns().unwrap_or(0));
                continue;
            }
            // Retrying an error silently would spin the loop.
            kprint(b"input-server: sys_wait FAILED\n");
            continue;
        }

        let mut forward_pending = false;
        // The subscription is read after the harvest: a departure closes a device's read, and a
        // later record in this wait's results could name that handle.
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
            } else if srv.control != 0 && h == srv.control {
                if let Control::Op(CTRL_OP_SHUTDOWN) = libkern::control::recv(&mut srv.control) {
                    kprint(b"input-server: asked to stop, exiting\n");
                    exit(0);
                }
            } else if srv.subscription != 0 && h == srv.subscription {
                notices_pending = true;
            } else if let Some(slot) = srv.devices.iter().position(|d| d.as_ref().is_some_and(|d| d.po == h)) {
                if let Some(d) = srv.devices[slot].as_mut() {
                    counts[slot] = d.harvest(&mut harvested[slot]);
                }
                // A read that completed `PeerClosed` is retired at the top of the next pass, after
                // what this one harvested from the other devices is forwarded.
            } else if let Some(slot) = srv.channels.iter().position(|&c| c == h && c != 0) {
                // A signal means the receive queue is non-empty or the peer is gone; treating
                // both as gone would silently cut the stream of any consumer that ever sends.
                // SAFETY: single-threaded; `RECV_MSG` is lent for the call.
                let rr = unsafe { channel_recv_raw(srv.channels[slot], &raw mut RECV_MSG) };
                if rr.as_ref().is_err_and(|&e| e == KError::PeerClosed.as_i32() as i64) {
                    close(srv.channels[slot]);
                    srv.channels[slot] = 0;
                    // Its debt goes with it: nothing could deliver it.
                    srv.consumers[slot] = Consumer::new();
                } else if rr.is_ok() {
                    // A consumer's message is the lights it wants; anything else is drained and
                    // ignored, with a line.
                    match consumer_lights() {
                        Some(l) => set_lights(srv, l),
                        None => kprint(b"input-server: ignoring an unexpected message from a consumer\n"),
                    }
                }
            }
        }

        if counts.iter().any(|&c| c > 0) {
            let sources: [&[InputEvent]; MAX_DEVICES] = core::array::from_fn(|i| &harvested[i][..counts[i]]);
            let n = merge(&sources, &mut merged);
            for run in batches(&merged[..n], BATCH_MAX) {
                // Stamps the marker and recovered motion written in front of the batch, so it is
                // the batch's first time: a later one would run `time_ns` backwards.
                let stamp = merged[run.start].time_ns;
                forward(srv, &merged[run], stamp);
            }
        } else if owes_send(srv) {
            // Woken by something else with motion still owed: send it now.
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

/// ELF entry; `ctrl` is the control channel the supervisor spawned this server with.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, ctrl: u64) -> ! {
    kprint(b"input-server: up\n");

    let mut srv = Server {
        devices: [const { None }; MAX_DEVICES],
        table: Table::new(),
        subscription: 0,
        channels: [0; MAX_CONSUMERS],
        consumers: [Consumer::new(); MAX_CONSUMERS],
        control: ctrl,
        lights: None,
        lights_buf: None,
    };
    // Devices come from the manager with no fallback to raw paths: a path that runs only when
    // the first is broken is one nobody tests. With no manager or devices it still serves an
    // empty stream.
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
