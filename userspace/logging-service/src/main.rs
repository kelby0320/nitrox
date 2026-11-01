//! `logging-service` — the userspace **logging service**.
//!
//! A namespace-bound resource server that collects log records from any process holding a
//! logging endpoint, stamps them with **trusted, capability-derived** provenance, and fans
//! them out to sinks. A client resolves `<tier>/<principal>[/<source>]` under the path a
//! supervisor bound it at and gets a per-principal write channel; the server stamps
//! `principal`/`tier` (from *which* channel a record arrived on), `timestamp` and `sequence`.
//! See `docs/architecture/logging.md`.
//!
//! **And it reads back.** Resolving `read-endpoint` on the serving endpoint mints a **read
//! endpoint**, which the view broker's `logs` grant binds at `/dev/logs`; any resolve on that
//! opens a **read session**, a channel that answers `Log::Read` from the ring
//! (`docs/spec/rsproto-log-ops.md`). Only a holder of the root namespace reaches
//! `read-endpoint`, as with every server's admin endpoint.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libkern::*;
use librsproto::error::error_body;
use librsproto::log::{LEVEL_INFO, OP_LOG_READ, level_name, parse_append, parse_read_request};
use librsproto::namespace::{OBJECT_KIND_CHANNEL, parse_resolve_request, resolve_reply};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, RS_HEADER_LEN, decode, encode};
use logging_service::path::{self, tier_name};
use logging_service::ring::{NAME_MAX, Record, Ring, claimed_source};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The `IpcMsg` payload offset, after its 24-byte header.
const PAYLOAD_OFF: usize = 24;
/// The size of each IPC message buffer.
const MSG_LEN: usize = 4096;
/// Read endpoints held at once: the view broker's, and `boot-probe`'s in a test image. A third
/// is refused `WouldBlock`.
const MAX_READ_ENDPOINTS: usize = 2;
/// Read sessions open at once: a `log` running, and one more. A reader closes its session when
/// done, so this bounds concurrent readers.
const MAX_READ_SESSIONS: usize = 2;
/// The most per-principal log channels read at once.
///
/// **Derived, not chosen**: the server waits on one set holding the serving endpoint, the control
/// channel, the read endpoints and sessions and every source, so the ceiling is the kernel's
/// fan-out limit less the rest. Raising [`MAX_WAIT_HANDLES`] moves it.
/// Escaping the limit rather than raising it is `TODO(server-fanout)`; see
/// `docs/architecture/logging.md`.
const MAX_SOURCES: usize = MAX_WAIT_HANDLES - 2 - MAX_READ_ENDPOINTS - MAX_READ_SESSIONS;
/// `KError::PeerClosed`: unlike `WouldBlock`, stop waiting on the handle. See [`drain_source`].
const E_PEER_CLOSED: i64 = -13;
/// The resolve suffix that mints a read endpoint.
const READ_ENDPOINT: &str = "read-endpoint";
const READ_BODY_CAP: usize = IPC_PAYLOAD_SIZE - RS_HEADER_LEN;

/// A per-principal log channel the server holds the read end of.
struct Source {
    /// The kept (read) endpoint.
    handle: u64,
    principal: String,
    tier: u8,
    /// The resolve path's sub-label, stamped on records that carry no `source` of their own.
    label: Option<String>,
}

/// A destination for stamped records. The ring is not one: `Read` answers from it, so it is
/// [`Log`]'s own.
trait Sink {
    fn write(&mut self, rec: &Record);
}

/// Formats each record to a line on the serial console (via `sys_kprint`).
struct SerialSink;
impl Sink for SerialSink {
    fn write(&mut self, rec: &Record) {
        let src = match &rec.source {
            Some(s) => {
                let mut t = String::from(".");
                t.push_str(s);
                t
            }
            None => String::new(),
        };
        let line = format!(
            "[{} t={}] {}/{}{} {}: {}\n",
            rec.sequence,
            rec.timestamp,
            tier_name(rec.tier),
            rec.principal,
            src,
            level_name(rec.level),
            rec.message,
        );
        kprint(line.as_bytes());
    }
}

/// **Where a stamped record goes**: every sink, then the ring. Holds the sequence counter.
struct Log {
    sinks: Vec<Box<dyn Sink>>,
    ring: Ring,
    seq: u64,
}

impl Log {
    fn record(&mut self, principal: String, tier: u8, level: u8, message: String, source: Option<String>) {
        self.seq += 1;
        let rec = Record {
            principal,
            tier,
            timestamp: clock_now(),
            time: wall_now(),
            sequence: self.seq,
            level,
            message,
            source,
        };
        for sink in self.sinks.iter_mut() {
            sink.write(&rec);
        }
        self.ring.push(rec);
    }
}

fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

fn exit(code: i64) -> ! {
    // SAFETY: SYS_PROCESS_EXIT terminates this process.
    unsafe { syscall1(SYS_PROCESS_EXIT, code as u64) };
    loop {
        core::hint::spin_loop();
    }
}

fn clock_now() -> u64 {
    let mut out: u64 = 0;
    // SAFETY: `&out` is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut out) as u64) };
    out
}

/// Read the wall clock (nanoseconds since the epoch), or `None` while it is not set.
fn wall_now() -> Option<u64> {
    let mut out: u64 = 0;
    // SAFETY: `&out` is a valid writable u64 out-param.
    let r = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut out) as u64) };
    (r == 0).then_some(out)
}

/// A channel pair of depth 4: `(a, b)`.
fn make_channel() -> Option<(u64, u64)> {
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, 4, 0) };
    (cr == 0).then_some((a, b))
}

/// **The receive buffer**, one whole message, which [`Inbox::recv`] fills. What it holds is lent
/// borrowed from it, so the next receive cannot overwrite a message still being read.
struct Inbox([u8; MSG_LEN]);

impl Inbox {
    /// Receive one message on `endpoint`; any handle it brought closes as the [`Received`] drops.
    fn recv(&mut self, endpoint: u64) -> Result<Received, i64> {
        channel_recv(endpoint, &mut self.0)
    }

    /// The payload of the message last received.
    fn payload(&self) -> &[u8] {
        let len = u32::from_le_bytes([self.0[4], self.0[5], self.0[6], self.0[7]]) as usize;
        &self.0[PAYLOAD_OFF..PAYLOAD_OFF + len.min(MSG_LEN - PAYLOAD_OFF)]
    }

    /// The rsproto message last received.
    fn message(&self) -> Option<librsproto::Message<'_>> {
        decode(self.payload()).ok()
    }
}

/// Send `body` on `ch`, moving `handles`, from a whole message's buffer, as the kernel copies one.
/// NoBlock: a reader that does not read its replies loses them, and never holds this service up.
fn send(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handles: &[u64]) -> bool {
    let (mut msg, mut carried) = ([0u8; MSG_LEN], [0u64; 8]);
    let Some(rs_len) = encode(&mut msg[PAYLOAD_OFF..], op, request_id, flags, body, handles.len() as u16) else {
        return false;
    };
    msg[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
    msg[8] = handles.len() as u8;
    carried[..handles.len()].copy_from_slice(handles);
    let (msg, carried) = (msg.as_ptr() as u64, carried.as_ptr() as u64);
    // SAFETY: a whole message, and the handles it carries, which this process holds.
    unsafe { syscall5(SYS_CHANNEL_SEND, ch, msg, carried, handles.len() as u64, SENDMODE_NOBLOCK) == 0 }
}

/// Send `Meta::Ready` on the control channel, transferring `kernel_end` (the endpoint the
/// supervisor binds at the logging path).
fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let body_len = match librsproto::meta::ready(&mut body, b"logging-service") {
        Some(n) => n,
        None => return false,
    };
    send(control, librsproto::OP_READY, 0, 0, &body[..body_len], &[kernel_end])
}

/// Reply to a forwarded resolve, transferring `write_end` as the resolved channel. `true` once
/// the handle has moved to the caller.
fn reply_channel(serve_end: u64, request_id: u64, write_end: u64) -> bool {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    send(serve_end, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, &[write_end])
}

fn reply_error(serve_end: u64, request_id: u64, op: u16, kerror: i32) {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = error_body(&mut ebody, kerror, 0, b"").unwrap_or(0);
    let _ = send(serve_end, op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &ebody[..elen], &[]);
}

/// What a forwarded resolve on the serving endpoint asked for.
enum Asked {
    /// A log channel for `(tier, principal, source label)`.
    Source(u8, String, Option<String>),
    /// A read endpoint.
    ReadEndpoint,
}

/// Handle one forwarded `Namespace::Resolve`: `read-endpoint` mints a read endpoint; anything
/// else is classified as a log path and mints a per-principal channel (keep the read end tagged,
/// transfer the write end). Bad paths and over-capacity get an error reply.
fn process_resolve(inbox: &Inbox, serve_end: u64, sources: &mut Vec<Source>, read_ends: &mut Vec<u64>) {
    let (request_id, asked) = match decode(inbox.payload()) {
        Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
            Some(r) => match core::str::from_utf8(r.suffix).ok() {
                Some(suffix) if suffix.trim_matches('/') == READ_ENDPOINT => (m.request_id, Some(Asked::ReadEndpoint)),
                Some(suffix) => match path::classify(suffix) {
                    // A principal is identity, so one too long to keep whole is refused,
                    // not cut.
                    Some(c) if c.principal.len() > NAME_MAX || c.source.is_some_and(|l| l.len() > NAME_MAX) => {
                        reply_error(serve_end, m.request_id, m.op, KError::InvalidArgument.as_i32());
                        (m.request_id, None)
                    }
                    Some(c) => {
                        // Owned copies: `c` borrows the receive buffer the next message reuses.
                        let label = c.source.map(String::from);
                        (m.request_id, Some(Asked::Source(c.tier, String::from(c.principal), label)))
                    }
                    None => {
                        reply_error(serve_end, m.request_id, m.op, KError::NotFound.as_i32());
                        (m.request_id, None)
                    }
                },
                None => {
                    reply_error(serve_end, m.request_id, m.op, KError::NotFound.as_i32());
                    (m.request_id, None)
                }
            },
            None => {
                reply_error(serve_end, m.request_id, m.op, KError::InvalidArgument.as_i32());
                (m.request_id, None)
            }
        },
        Ok(m) => {
            reply_error(serve_end, m.request_id, m.op, KError::Unsupported.as_i32());
            (m.request_id, None)
        }
        Err(_) => (0, None),
    };
    let (tier, principal, label) = match asked {
        Some(Asked::Source(tier, principal, label)) => (tier, principal, label),
        Some(Asked::ReadEndpoint) => return mint_read_endpoint(serve_end, request_id, read_ends),
        None => return,
    };

    if sources.len() >= MAX_SOURCES {
        kprint(b"logging-service: source table full, refusing new log channel\n");
        reply_error(serve_end, request_id, OP_NS_RESOLVE, KError::Unsupported.as_i32());
        return;
    }

    let (read_end, write_end) = match make_channel() {
        Some(pair) => pair,
        None => {
            reply_error(serve_end, request_id, OP_NS_RESOLVE, KError::KernelError.as_i32());
            return;
        }
    };
    if reply_channel(serve_end, request_id, write_end) {
        match &label {
            Some(l) => kprint(format!("logging-service: opened {}/{}/{}\n", tier_name(tier), principal, l).as_bytes()),
            None => kprint(format!("logging-service: opened {}/{}\n", tier_name(tier), principal).as_bytes()),
        }
        sources.push(Source { handle: read_end, principal, tier, label });
    } else {
        // The reply failed, so the write end did not move: close both.
        close(read_end);
        close(write_end);
    }
}

fn close(h: u64) {
    // SAFETY: closing a handle this process owns.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
}

/// **Mint a read endpoint**: a channel whose other end is the answer to the resolve, which a
/// supervisor binds as a server endpoint (the view broker, at `/dev/logs`), so every resolve
/// there arrives here, on the end kept. At most [`MAX_READ_ENDPOINTS`].
fn mint_read_endpoint(serve_end: u64, request_id: u64, read_ends: &mut Vec<u64>) {
    if read_ends.len() >= MAX_READ_ENDPOINTS {
        return reply_error(serve_end, request_id, OP_NS_RESOLVE, KError::WouldBlock.as_i32());
    }
    let Some((ours, theirs)) = make_channel() else {
        return reply_error(serve_end, request_id, OP_NS_RESOLVE, KError::KernelError.as_i32());
    };
    if reply_channel(serve_end, request_id, theirs) {
        read_ends.push(ours);
        kprint(b"logging-service: a read endpoint minted\n");
    } else {
        close(ours);
        close(theirs);
    }
}

/// **A resolve on a read endpoint opens a read session**, whatever its suffix, at most
/// [`MAX_READ_SESSIONS`] at once. `false` once the binding is gone, so the endpoint leaves the
/// wait set rather than being signalled forever.
fn serve_read_endpoint(inbox: &mut Inbox, from: u64, sessions: &mut Vec<u64>) -> bool {
    loop {
        let _sent = match inbox.recv(from) {
            Ok(sent) => sent,
            Err(E_PEER_CLOSED) => return false,
            Err(_) => return true,
        };
        let Some(m) = inbox.message() else { continue };
        let (op, request_id) = (m.op, m.request_id);
        if op != OP_NS_RESOLVE {
            reply_error(from, request_id, op, KError::Unsupported.as_i32());
            continue;
        }
        if sessions.len() >= MAX_READ_SESSIONS {
            reply_error(from, request_id, op, KError::WouldBlock.as_i32());
            continue;
        }
        let Some((ours, theirs)) = make_channel() else {
            reply_error(from, request_id, op, KError::KernelError.as_i32());
            continue;
        };
        if reply_channel(from, request_id, theirs) {
            sessions.push(ours);
        } else {
            close(ours);
            close(theirs);
        }
    }
}

/// **A read session**: each `Log::Read` answered from the ring. `false` once its reader closed it.
fn serve_read_session(inbox: &mut Inbox, h: u64, ring: &Ring) -> bool {
    loop {
        let _sent = match inbox.recv(h) {
            Ok(sent) => sent,
            Err(E_PEER_CLOSED) => return false,
            Err(_) => return true,
        };
        let Some(m) = inbox.message() else { continue };
        let (op, request_id) = (m.op, m.request_id);
        if op != OP_LOG_READ {
            reply_error(h, request_id, op, KError::Unsupported.as_i32());
            continue;
        }
        match parse_read_request(m.body) {
            Some((after, max)) => reply_records(h, request_id, ring, after, max),
            None => reply_error(h, request_id, op, KError::InvalidArgument.as_i32()),
        }
    }
}

/// Answer a `Read` with [`Ring::fill_reply`]. None left is an empty reply, which is how a
/// reader knows it has read to the end.
fn reply_records(to: u64, request_id: u64, ring: &Ring, after: u64, max: u32) {
    let mut body = [0u8; READ_BODY_CAP];
    let Some(len) = ring.fill_reply(after, max, &mut body) else {
        return reply_error(to, request_id, OP_LOG_READ, KError::KernelError.as_i32());
    };
    let _ = send(to, OP_LOG_READ, request_id, RS_FLAG_REPLY, &body[..len], &[]);
}

/// Decode a `TSM1` typed-stream message and render each row as a `name=value …` record, after a
/// one-time marker naming the schema's columns. A malformed or truncated stream is dropped,
/// never fatal.
fn render_typed(body: &[u8], principal: &str, tier: u8, chan_label: &Option<String>, log: &mut Log) {
    let mut tr = match libstream::TableReader::new(body) {
        Ok(t) => t,
        Err(_) => {
            kprint(b"logging-service: malformed typed stream (drop)\n");
            return;
        }
    };
    let schema = tr.schema().clone();
    let mut cols = String::from("logging-service: typed stream from ");
    cols.push_str(principal);
    cols.push_str(" [");
    for (i, f) in schema.fields.iter().enumerate() {
        if i > 0 {
            cols.push_str(", ");
        }
        cols.push_str(&f.name);
    }
    cols.push_str("]\n");
    kprint(cols.as_bytes());

    loop {
        match tr.next() {
            Some(Ok(libstream::Item::Row(values))) => {
                let source = chan_label.clone().or_else(|| Some(String::from("typed")));
                log.record(String::from(principal), tier, LEVEL_INFO, format_typed_row(&schema, &values), source);
            }
            Some(Ok(libstream::Item::End(_))) | None => break,
            Some(Ok(libstream::Item::Error(_))) => continue, // pass over in-stream errors
            Some(Err(_)) => {
                kprint(b"logging-service: typed decode error (drop rest)\n");
                break;
            }
        }
    }
}

fn format_typed_row(schema: &libstream::Schema, values: &[libstream::Value]) -> String {
    use core::fmt::Write;
    use libstream::Value;
    let mut s = String::new();
    for (i, field) in schema.fields.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{}=", field.name);
        match values.get(i) {
            Some(Value::Bool(b)) => s.push_str(if *b { "true" } else { "false" }),
            Some(Value::Int(v)) => {
                let _ = write!(s, "{}", v);
            }
            Some(Value::Float(v)) => {
                let _ = write!(s, "{}", v);
            }
            Some(Value::Str(v)) => s.push_str(v),
            Some(Value::Bytes(v)) => {
                let _ = write!(s, "<{}B>", v.len());
            }
            Some(Value::Handle(v)) => {
                let _ = write!(s, "{:#x}", v);
            }
            // Collection cells are summarised so a log line stays one line.
            Some(Value::List(items)) => {
                let _ = write!(s, "<list:{}>", items.len());
            }
            Some(Value::Record(r)) => {
                let _ = write!(s, "<record:{}>", r.schema.fields.len());
            }
            Some(Value::Table(t)) => {
                let _ = write!(s, "<table:{}rows>", t.rows.len());
            }
            Some(Value::Null) | None => s.push_str("null"),
        }
    }
    s
}

/// Drain and stamp every queued `LogRecord` on source channel `h`. `true` once its writer is gone.
fn drain_source(inbox: &mut Inbox, h: u64, sources: &[Source], log: &mut Log) -> bool {
    let (principal, tier, chan_label) = match sources.iter().find(|s| s.handle == h) {
        Some(s) => (s.principal.clone(), s.tier, s.label.clone()),
        // An unknown handle is no live source: reported dead, it leaves the wait set.
        None => return true,
    };
    loop {
        let rc = inbox.recv(h);
        if rc.as_ref().is_err_and(|&e| e == E_PEER_CLOSED) {
            // **Terminal, and never retried.** A closed peer is permanently `signaled`, so a
            // dead handle left in the wait set makes `sys_wait` return at once, forever: this
            // service spins, the idle thread never runs, and with it no exited process on the
            // system is reclaimed (decision log, 2026-07-31).
            return true;
        }
        let Ok(_sent) = rc else {
            break; // WouldBlock: drained
        };
        let body = inbox.payload();
        // A TSM1 typed stream is rendered; anything else is a text `LogRecord` append.
        if body.starts_with(&libstream::wire::MAGIC) {
            render_typed(body, &principal, tier, &chan_label, log);
            continue;
        }
        let la = match parse_append(body) {
            Some(la) => la,
            None => continue, // malformed record: drop
        };
        let message = String::from(core::str::from_utf8(la.message).unwrap_or("<non-utf8>"));
        log.record(principal.clone(), tier, la.level, message, claimed_source(la.source, &chan_label));
    }
    false // drained, still live
}

/// The serve loop: multi-wait on the serving endpoint, the control channel, the read endpoints and
/// sessions, and every per-principal channel. Never returns.
///
/// **It exits on `CTRL_OP_SHUTDOWN`**, after sinking every record already queued: a shutdown asks
/// the services in the reverse of their start order, so by then everything started after the log
/// has said its last. `service --stop` never sends it, since the log is `essential`.
fn serve_loop(serve_end: u64, mut control: u64, log: &mut Log) -> ! {
    kprint(b"logging-service: serving\n");
    let mut sources: Vec<Source> = Vec::new();
    let mut read_ends: Vec<u64> = Vec::new();
    let mut sessions: Vec<u64> = Vec::new();
    let mut dead: Vec<u64> = Vec::new();
    let mut inbox = Inbox([0; MSG_LEN]);
    loop {
        // Each list is capped (the sources at MAX_SOURCES, what is left after the rest), so `count`
        // never passes MAX_WAIT_HANDLES.
        let (mut handles, mut results) = ([0u64; MAX_WAIT_HANDLES], [0u8; MAX_WAIT_HANDLES * WAIT_RESULT_SIZE]);
        let count = {
            let mut count = 0;
            let mut put = |h: u64| {
                handles[count] = h;
                count += 1;
            };
            put(serve_end);
            if control != 0 {
                put(control);
            }
            read_ends.iter().chain(&sessions).copied().for_each(&mut put);
            sources.iter().for_each(|s| put(s.handle));
            count
        };
        // SAFETY: `count` handles, and room for as many results.
        let waited =
            unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, count as u64, results.as_mut_ptr() as u64, u64::MAX) };
        if waited < 1 {
            continue;
        }
        // Each signaled handle is one 24-byte `IoResult` (handle @0).
        let signalled: Vec<u64> = (0..waited as usize)
            .map(|j| u64::from_le_bytes(results[j * 24..j * 24 + 8].try_into().unwrap_or([0; 8])))
            .collect();
        // **Closes before resolves, in every wake**: a reader that lets go of a session or an
        // endpoint and asks for another at once must find the first retired, or it is refused a
        // slot that is free. So the sessions and sources first, then the read endpoints, then the
        // serving endpoint.
        for &h in &signalled {
            if h == control {
                if let libkern::control::Control::Op(CTRL_OP_SHUTDOWN) = libkern::control::recv(&mut control) {
                    for s in &sources {
                        drain_source(&mut inbox, s.handle, &sources, log);
                    }
                    kprint(b"logging-service: asked to stop, exiting\n");
                    exit(0);
                }
            } else if sessions.contains(&h) {
                if !serve_read_session(&mut inbox, h, &log.ring) {
                    sessions.retain(|&e| e != h);
                    close(h);
                }
            } else if h != serve_end && !read_ends.contains(&h) && drain_source(&mut inbox, h, &sources, log) {
                dead.push(h);
            }
        }
        // Retired after the sweep, since `drain_source` borrows `sources`.
        for h in dead.drain(..) {
            sources.retain(|s| s.handle != h);
            close(h);
        }
        for &h in &signalled {
            if read_ends.contains(&h) && !serve_read_endpoint(&mut inbox, h, &mut sessions) {
                read_ends.retain(|&e| e != h);
                close(h);
            }
        }
        if signalled.contains(&serve_end) {
            while let Ok(_sent) = inbox.recv(serve_end) {
                process_resolve(&inbox, serve_end, &mut sources, &mut read_ends);
            }
        }
    }
}

/// Bootstrap registers: `rdx` is the control-channel endpoint the supervisor installed; the rest
/// are unused, as clients bring their own log endpoint by resolving.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"logging-service: up\n");

    let (kernel_end, serve_end) = match make_channel() {
        Some(pair) => pair,
        None => {
            kprint(b"logging-service: channel create FAIL\n");
            exit(1);
        }
    };
    if !send_ready(control, kernel_end) {
        kprint(b"logging-service: Ready send FAIL\n");
        exit(1);
    }

    let sinks: Vec<Box<dyn Sink>> = alloc::vec![Box::new(SerialSink)];
    let mut log = Log { sinks, ring: Ring::new(), seq: 0 };
    serve_loop(serve_end, control, &mut log);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"logging-service: PANIC\n");
    exit(1);
}
