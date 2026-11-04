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
use libkern::device::DeviceKind;
use libkern::error::KError;
use libkern::{IO_OPCODE_READ, IO_OPCODE_WRITE, IoOp, MAX_WAIT_HANDLES, Rights};
use libos::boot::Start;
use libos::control::{self, Control};
use libos::log::{self, Line};
use libos::{
    Carried, Channel, Deadline, Handle, MapReadWrite, Mapping, Memory, Msg, Namespace, NsReadOnly, Only, Op, ReadWrite,
    Resource, Signalled, WaitSet, block_on, time,
};
use librsproto::server;
use librsproto::{OP_INPUT_EVENTS, OP_INPUT_LIGHTS, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode};

// Nothing allocates on the event path; `librsproto` and the panic path expect a heap.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const PAGE: usize = 4096;

/// Where the device manager hands over input devices: resolving it is the subscription.
const SUBSCRIPTION: &str = "/svc/devices/input";

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
const CONSUMER_QUEUE_DEPTH: u32 = 16;

const CONTROL_QUEUE_DEPTH: u32 = 4;

/// Overruns to report before going quiet; a stopped consumer would print one every 5 ms.
const MAX_LOGGED_OVERRUNS: u32 = 16;

static OVERRUNS_LOGGED: AtomicU32 = AtomicU32::new(0);

/// Retry interval for a consumer holding deferred motion: short enough that a person cannot see
/// the catch-up, long enough that a stalled one costs a handful of wakeups rather than a spin
/// (about twenty across a ~100 ms emulated recompose).
const FLUSH_INTERVAL_NS: u64 = 5_000_000;

/// The wait set holds the forwarding endpoint, the control channel, the subscription, every
/// device's read and lights write, and every consumer.
const _: () = assert!(3 + 2 * MAX_DEVICES + MAX_CONSUMERS <= MAX_WAIT_HANDLES);

type Endpoint = Handle<Channel, Only>;
type Node = Handle<Resource, ReadWrite>;

/// What a wait's index names.
#[derive(Copy, Clone)]
enum Waited {
    Forward,
    Control,
    Subscription,
    Device(usize),
    Lights(usize),
    Consumer(usize),
}

/// The monotonic clock in nanoseconds, or `None` if the read failed.
///
/// Checked because a failed read has no time to give, so the flush deadline would be in the past
/// and the 5 ms retry a spin; callers turn `None` into "wait indefinitely".
fn now_ns() -> Option<u64> {
    time::now().ok()
}

/// One device: its node, read buffer and outstanding read.
struct Device {
    /// The in-flight read, or `None` when none is outstanding. First, so that it drops before
    /// the buffer it reads into: a device that is still reading is waited out.
    op: Option<Op>,
    /// A lights write the keyboard has not answered, and whether newer lights wait on it.
    lights: Option<Op>,
    lights_owed: bool,
    map: Mapping<MapReadWrite>,
    mem: Handle<Memory, MapReadWrite>,
    node: Node,
    /// Decides whether it is written the lights.
    kind: DeviceKind,
    /// Its registry id, which the manager's `Departed` names.
    id: u32,
    /// See [`input_server::leaving`].
    leaving: Leaving,
    /// Both its node and the manager have said it has gone: the serve loop retires its slot.
    retire_due: bool,
}

impl Device {
    /// Map a read buffer for `node`; `None`, with the node closed, if it cannot be had.
    fn adopt(node: Node, kind: DeviceKind, id: u32) -> Option<Self> {
        let mem = Handle::<Memory, MapReadWrite>::create(PAGE).ok()?;
        let map = mem.map(PAGE).ok()?;
        let leaving = Leaving::Present;
        Some(Self { op: None, lights: None, lights_owed: false, map, mem, node, kind, id, leaving, retire_due: false })
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
        if self.op.is_some() || !self.leaving.reading() {
            return;
        }
        let io = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: self.mem.raw().0,
            buf_offset: 0,
            offset: 0,
            length: (PER_DEVICE * INPUT_EVENT_LEN) as u64,
        };
        match Op::submit(self.node.raw(), &io) {
            Ok(op) => self.op = Some(op),
            // Its device has gone and its node has nothing left to read.
            Err(e) if e.code() == KError::PeerClosed => self.peer_closed(),
            // The device drops out of the wait set and may stop delivering; say so.
            Err(_) => log::write(b"input-server: read submit FAILED -- device may stall\n"),
        }
    }

    /// Harvest a completed read into `out`, consuming its `Op` so the next [`arm`](Self::arm)
    /// submits a fresh one. Returns how many events arrived.
    fn harvest(&mut self, signalled: &Signalled<MAX_WAIT_HANDLES>, out: &mut [InputEvent]) -> usize {
        let Some(done) = self.op.as_mut().and_then(|op| op.completion_in(signalled)) else {
            return 0;
        };
        self.op = None;
        let done = match done {
            Ok(done) => done,
            // A read that was waiting when its device went, with no releases to answer it.
            Err(e) if e.code() == KError::PeerClosed => {
                self.peer_closed();
                return 0;
            }
            Err(_) => {
                log::write(b"input-server: read completed with an error; events lost\n");
                return 0;
            }
        };
        let n = (done.result as usize / INPUT_EVENT_LEN).min(out.len());
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            // In the page: `n` is bounded by `PER_DEVICE`, and the node delivers whole records
            // only (`io-operation.md`, record-stream char devices).
            let mut bytes = [0u8; INPUT_EVENT_LEN];
            if self.map.read_into(i * INPUT_EVENT_LEN, &mut bytes).is_err() {
                return i;
            }
            match InputEvent::read(&bytes) {
                Some(e) => *slot = e,
                None => return i,
            }
        }
        n
    }
}

fn send_events(channel: &Endpoint, events: &[InputEvent]) -> bool {
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
    server::send(channel, OP_INPUT_EVENTS, 0, 0, &body[..n], &mut Carried::new()).is_ok()
}

/// The page a lights write is read from.
struct LightsPage {
    /// Before `mem`, so it is unmapped before the object is closed.
    map: Mapping<MapReadWrite>,
    mem: Handle<Memory, MapReadWrite>,
}

/// Everything the serve loop owns.
struct Server {
    devices: [Option<Device>; MAX_DEVICES],
    table: Table,
    /// The subscription to [`SUBSCRIPTION`], or `None` when the manager was not there or has gone.
    subscription: Option<Endpoint>,
    /// Consumer channels, `None` for a free slot.
    channels: [Option<Endpoint>; MAX_CONSUMERS],
    consumers: [Consumer; MAX_CONSUMERS],
    /// The control channel, for `service --stop`; `None` once its supervisor has gone, when it
    /// would stay signalled for good.
    control: Option<Endpoint>,
    /// The lights a consumer last asked for; until one does, a keyboard keeps its driver's.
    lights: Option<u8>,
    lights_page: Option<LightsPage>,
}

/// Write `lights` to `d` if it is a keyboard, **never waiting for its answer**: the driver reads
/// the byte at submit, so one page serves every keyboard, and the write stays outstanding in the
/// wait set. Lights asked while one is outstanding are written when it is answered.
fn write_lights(page: &mut Option<LightsPage>, d: &mut Device, lights: u8) {
    if d.kind != DeviceKind::Keyboard {
        return;
    }
    if d.lights.is_some() {
        d.lights_owed = true;
        return;
    }
    if page.is_none() {
        *page = Handle::<Memory, MapReadWrite>::create(PAGE)
            .and_then(|mem| mem.map(PAGE).map(|map| LightsPage { map, mem }))
            .ok();
    }
    let Some(page) = page else {
        log::write(b"input-server: no page for the lights; not written\n");
        return;
    };
    if page.map.write_from(0, &[lights]).is_err() {
        log::write(b"input-server: no page for the lights; not written\n");
        return;
    }
    let io = IoOp { opcode: IO_OPCODE_WRITE, flags: 0, buffer: page.mem.raw().0, buf_offset: 0, offset: 0, length: 1 };
    match Op::submit(d.node.raw(), &io) {
        Ok(op) => d.lights = Some(op),
        Err(e) => {
            Line::new().s(b"input-server: a keyboard refused its lights: ").i(e.code().as_i32() as i64).end();
        }
    }
}

fn set_lights(srv: &mut Server, lights: u8) {
    srv.lights = Some(lights);
    for d in srv.devices.iter_mut().flatten() {
        write_lights(&mut srv.lights_page, d, lights);
    }
}

fn consumer_lights(msg: &Msg) -> Option<u8> {
    let m = decode(msg.payload()).ok()?;
    if m.op != OP_INPUT_LIGHTS || m.flags & (RS_FLAG_REPLY | RS_FLAG_ERROR) != 0 {
        return None;
    }
    lights_request(m.body)
}

fn open_consumer(serve_end: &Endpoint, request_id: u64, srv: &mut Server) {
    let Some(slot) = srv.channels.iter().position(Option::is_none) else {
        server::reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::OutOfHandles, b"");
        return;
    };
    let Ok((consumer_end, server_end)) = Handle::pair(CONSUMER_QUEUE_DEPTH) else {
        server::reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::OutOfMemory, b"");
        return;
    };
    srv.channels[slot] = Some(server_end);
    srv.consumers[slot] = Consumer::new();
    if !server::reply_channel(serve_end, request_id, consumer_end) {
        // The consumer never received its end: drop both rather than hold a slot for no peer.
        srv.channels[slot] = None;
    }
}

/// Whether any **live** consumer is owed a send.
///
/// A debt against a disconnected consumer can never be cleared; counting it would arm the 5 ms
/// retry forever.
fn owes_send(srv: &Server) -> bool {
    (0..MAX_CONSUMERS).any(|i| srv.channels[i].is_some() && srv.consumers[i].owes_send())
}

/// Forward one merged batch to every consumer.
///
/// A failed send is not retried: the records go back to the consumer (`rsproto-input-ops.md`
/// § Loss), so there is no flow control and a slow consumer never stalls the others. An empty
/// `batch` is the flush pass.
fn forward(srv: &mut Server, batch: &[InputEvent], now_ns: u64) {
    let mut framed = [InputEvent::default(); FRAME_MAX];
    for i in 0..MAX_CONSUMERS {
        let Some(channel) = &srv.channels[i] else {
            continue;
        };
        match srv.consumers[i].frame(batch, now_ns, &mut framed) {
            // Nothing new and nothing owed: an empty message would wake the consumer for nothing.
            Some(0) => {}
            Some(n) if send_events(channel, &framed[..n]) => {}
            // Hand back what was framed, not the batch: `frame` cleared both debts as it wrote
            // them, so the batch alone would forget the marker and recovered motion.
            Some(n) => {
                // Deferral hides an overrun from the consumer, so a gate that asserts motion
                // survived one needs this line to know the ring really filled.
                let logged = OVERRUNS_LOGGED.load(Relaxed);
                if logged < MAX_LOGGED_OVERRUNS {
                    OVERRUNS_LOGGED.store(logged + 1, Relaxed);
                    Line::new()
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
fn apply(srv: &mut Server, n: Notice, handles: &mut Carried) -> bool {
    let say = |id: u32, kind: DeviceKind, what: &[u8]| {
        Line::new().s(b"input-server: ").s(kind_word(kind)).s(b" ").u(id as u64).s(what).end();
    };
    match n {
        // `notice` classifies an arrival as one only with exactly one handle: its node.
        Notice::Arrived { id, kind } => match srv.table.arrive(id) {
            Arrival::Slot(slot) => match handles.take(0).and_then(|node| Device::adopt(node, kind, id)) {
                Some(mut d) => {
                    // A new keyboard is told the lights already on, or it would show Caps Lock off.
                    if let Some(l) = srv.lights {
                        write_lights(&mut srv.lights_page, &mut d, l);
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
                drop(core::mem::take(handles));
                say(id, kind, b" arrived again; the second node closed");
            }
            Arrival::Full => {
                drop(core::mem::take(handles));
                say(id, kind, b": every slot is taken; not read");
            }
        },
        Notice::NotInput { id, kind } => {
            drop(core::mem::take(handles));
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
            drop(core::mem::take(handles));
            log::write(b"input-server: a malformed message from the device manager; ignored\n");
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
        }
    }
}

/// Take every message waiting on the subscription. Returns whether one of them was `Settled`.
///
/// A manager that has gone takes nothing with it: devices already read stay read.
fn take_notices(srv: &mut Server) -> bool {
    let mut settled = false;
    while let Some(subscription) = &srv.subscription {
        let mut msg = Msg::new();
        let mut handles = match subscription.recv(&mut msg) {
            Ok(handles) => handles,
            Err(e) if e.code() == KError::PeerClosed => {
                log::write(b"input-server: the device manager has gone; the devices already read stay read\n");
                srv.subscription = None;
                break;
            }
            Err(_) => break,
        };
        let n = match decode(msg.payload()) {
            Ok(m) => notice(m.op, m.body, handles.len()),
            Err(_) => Notice::Malformed,
        };
        // `apply` keeps or closes each of these.
        settled |= apply(srv, n, &mut handles);
    }
    settled
}

/// Take the manager's replay until `Settled` or [`SETTLE_TIMEOUT_NS`]. Returns whether it settled.
fn settle(srv: &mut Server) -> bool {
    let deadline = now_ns().map(|t| Deadline::at(t.saturating_add(SETTLE_TIMEOUT_NS)));
    while srv.subscription.is_some() {
        if take_notices(srv) {
            return true;
        }
        // No clock, no deadline to wait against: take what was queued and serve.
        let (Some(deadline), Some(subscription)) = (deadline, &srv.subscription) else { break };
        let mut set = WaitSet::<1>::new();
        if set.add(subscription).is_err() || !set.wait(deadline).is_ok_and(|s| !s.is_empty()) {
            break;
        }
    }
    false
}

/// Handle a forwarded resolve on the serving endpoint. Returns `false` if the endpoint died.
fn serve_forward(serve_end: &Endpoint, srv: &mut Server) -> bool {
    let mut msg = Msg::new();
    let _carried = match serve_end.recv(&mut msg) {
        Ok(carried) => carried,
        Err(e) => return e.code() != KError::PeerClosed,
    };
    let (request_id, is_new, ok) = match decode(msg.payload()) {
        Ok(m) if m.op == OP_NS_RESOLVE => {
            let suffix = librsproto::namespace::parse_resolve_request(m.body).map(|r| r.suffix).unwrap_or(b"");
            // The empty suffix is the match: this server is bound at the leaf
            // `/dev/input/new`, so nothing is left over. (A subtree at `/dev/input` would
            // collide with the kernel's `/dev/input/raw`.)
            (m.request_id, suffix.is_empty(), true)
        }
        Ok(m) => (m.request_id, false, true),
        Err(_) => (0, false, false),
    };
    if !ok {
        return true;
    }
    if is_new {
        open_consumer(serve_end, request_id, srv);
    } else {
        server::reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::NotFound, b"");
    }
    true
}

/// Add `h` to `set` as `what`.
fn watch<'a>(set: &mut WaitSet<'a, MAX_WAIT_HANDLES>, names: &mut [Waited], h: &'a Endpoint, what: Waited) {
    if let Ok(i) = set.add(h) {
        names[i] = what;
    }
}

fn serve_loop(serve_end: &Endpoint, srv: &mut Server) -> ! {
    log::write(b"input-server: serving /dev/input/new\n");
    let mut harvested = [[InputEvent::default(); PER_DEVICE]; MAX_DEVICES];
    let mut merged = [InputEvent::default(); MERGE_MAX];

    loop {
        for d in srv.devices.iter_mut().flatten() {
            d.arm();
        }
        // An arm refused `PeerClosed` may have made a slot due.
        retire_due(srv);

        let mut names = [Waited::Forward; MAX_WAIT_HANDLES];
        let signalled = {
            let mut set = WaitSet::new();
            watch(&mut set, &mut names, serve_end, Waited::Forward);
            if let Some(c) = &srv.control {
                watch(&mut set, &mut names, c, Waited::Control);
            }
            if let Some(s) = &srv.subscription {
                watch(&mut set, &mut names, s, Waited::Subscription);
            }
            for (slot, d) in srv.devices.iter().enumerate() {
                if let Some(op) = d.as_ref().and_then(|d| d.op.as_ref())
                    && let Ok(i) = set.add_op(op)
                {
                    names[i] = Waited::Device(slot);
                }
                if let Some(op) = d.as_ref().and_then(|d| d.lights.as_ref())
                    && let Ok(i) = set.add_op(op)
                {
                    names[i] = Waited::Lights(slot);
                }
            }
            for (slot, c) in srv.channels.iter().enumerate() {
                if let Some(c) = c {
                    watch(&mut set, &mut names, c, Waited::Consumer(slot));
                }
            }
            // Bounded only while something is owed; otherwise an indefinite sleep. No clock
            // means no deadline rather than one in the past, which would spin.
            let deadline = match owes_send(srv).then(now_ns).flatten() {
                Some(t) => Deadline::at(t.saturating_add(FLUSH_INTERVAL_NS)),
                None => Deadline::NEVER,
            };
            set.wait(deadline)
        };
        let signalled = match signalled {
            // A timeout is the flush pass, not an error.
            Ok(s) if s.is_empty() => {
                // The stamp reaches only synthesised records, so no clock costs a zero.
                forward(srv, &[], now_ns().unwrap_or(0));
                continue;
            }
            Ok(s) => s,
            // Retrying an error silently would spin the loop.
            Err(_) => {
                log::write(b"input-server: sys_wait FAILED\n");
                continue;
            }
        };

        let mut forward_pending = false;
        // The subscription is read after the harvest: a departure closes a device's read, and a
        // later record in this wait's results could name that handle.
        let mut notices_pending = false;
        let mut counts = [0usize; MAX_DEVICES];
        for (i, _) in signalled.iter() {
            match names[i] {
                Waited::Forward => forward_pending = true,
                Waited::Control => {
                    if control::recv(&mut srv.control) == Control::Op(CTRL_OP_SHUTDOWN) {
                        log::write(b"input-server: asked to stop, exiting\n");
                        libos::exit(0);
                    }
                }
                Waited::Subscription => notices_pending = true,
                Waited::Device(slot) => {
                    if let Some(d) = srv.devices[slot].as_mut() {
                        counts[slot] = d.harvest(&signalled, &mut harvested[slot]);
                    }
                    // A read that completed `PeerClosed` is retired at the top of the next pass,
                    // after what this one harvested from the other devices is forwarded.
                }
                Waited::Lights(slot) => {
                    // Its answer is not needed: only that the keyboard can take the next.
                    if let Some(d) = srv.devices[slot].as_mut()
                        && d.lights.as_mut().and_then(|op| op.completion_in(&signalled)).is_some()
                    {
                        d.lights = None;
                        if let Some(l) = srv.lights.filter(|_| core::mem::take(&mut d.lights_owed)) {
                            write_lights(&mut srv.lights_page, d, l);
                        }
                    }
                }
                Waited::Consumer(slot) => {
                    let Some(ch) = &srv.channels[slot] else { continue };
                    let mut msg = Msg::new();
                    // A signal means the receive queue is non-empty or the peer is gone; treating
                    // both as gone would silently cut the stream of any consumer that ever sends.
                    match ch.recv(&mut msg) {
                        Err(e) if e.code() == KError::PeerClosed => {
                            srv.channels[slot] = None;
                            // Its debt goes with it: nothing could deliver it.
                            srv.consumers[slot] = Consumer::new();
                        }
                        Err(_) => {}
                        // A consumer's message is the lights it wants; anything else is drained
                        // and ignored, with a line.
                        Ok(_carried) => match consumer_lights(&msg) {
                            Some(l) => set_lights(srv, l),
                            None => log::write(b"input-server: ignoring an unexpected message from a consumer\n"),
                        },
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
            log::write(b"input-server: forwarding endpoint closed\n");
            libos::exit(1);
        }
    }
}

/// ELF entry; `rdx` is the control channel the supervisor spawned this server with.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, ctrl: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, ctrl, arg0) };
    log::write(b"input-server: up\n");
    let control: Option<Endpoint> = start.take_handle0();

    let mut srv = Server {
        devices: [const { None }; MAX_DEVICES],
        table: Table::new(),
        subscription: None,
        channels: [const { None }; MAX_CONSUMERS],
        consumers: [Consumer::new(); MAX_CONSUMERS],
        control: None,
        lights: None,
        lights_page: None,
    };
    // Devices come from the manager with no fallback to raw paths: a path that runs only when
    // the first is broken is one nobody tests. With no manager or devices it still serves an
    // empty stream.
    srv.subscription = start.root.as_ref().and_then(|root: &Handle<Namespace, NsReadOnly>| {
        block_on(root.lookup(SUBSCRIPTION, Rights::SEND | Rights::RECV | Rights::WAIT)).ok()
    });
    if srv.subscription.is_none() {
        Line::new()
            .s(b"input-server: could not subscribe at ")
            .s(SUBSCRIPTION.as_bytes())
            .s(b" -- serving no devices")
            .end();
    } else {
        let settled = settle(&mut srv);
        Line::new()
            .s(b"input-server: ")
            .u(srv.table.len() as u64)
            .s(b" device(s) from the device manager")
            .s(if settled { b"" } else { b", which did not settle; serving them" })
            .end();
    }

    let Ok((kernel_end, serve_end)) = Handle::pair(CONTROL_QUEUE_DEPTH) else {
        log::write(b"input-server: channel create FAIL\n");
        libos::exit(1);
    };
    if !control.as_ref().is_some_and(|c| server::ready(c, b"input-server", kernel_end)) {
        log::write(b"input-server: Ready send FAIL\n");
        libos::exit(1);
    }
    srv.control = control;

    serve_loop(&serve_end, &mut srv);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"input-server: panic\n");
    libos::exit(2);
}
