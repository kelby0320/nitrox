//! `logging-service` — the userspace **logging service** (Phase 3).
//!
//! A namespace-bound resource server that collects structured log records from any
//! process holding a logging endpoint, stamps them with **trusted, capability-derived**
//! provenance, and fans them out to sinks. Bound at a logging path by a supervisor
//! (init/service-mgr); a client resolves `<tier>/<principal>[/<source>]` under it and the
//! server hands back a per-principal write channel (an `OBJECT_KIND_CHANNEL` resolve
//! reply). The client then streams raw `LogRecord` appends on that channel; the server
//! stamps `principal`/`tier` (from *which* channel the record arrived on),
//! `timestamp`/`sequence`, and routes to sinks. See `docs/architecture/logging.md`.
//!
//! **And it reads back** (administration Part E.6). Resolving `read-endpoint` on the serving
//! endpoint mints a **read endpoint**, which the view broker's `logs` grant binds at `/dev/logs`;
//! any resolve on that opens a **read session**, a channel that answers `Log::Read` from the ring —
//! the most recent records, bounded by bytes (`docs/spec/rsproto-log-ops.md`). Only a holder of
//! the root namespace reaches `read-endpoint`, as with every server's admin endpoint.
//!
//! `#![no_std]` + `#![no_main]`; `libkern` + `libheap` + `librsproto`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libkern::*;
use librsproto::error::error_body;
use librsproto::log::{
    LEVEL_INFO, OP_LOG_READ, ReadRecord, ReadReplyWriter, level_name, parse_append, parse_read_request,
};
use librsproto::namespace::{OBJECT_KIND_CHANNEL, parse_resolve_request, resolve_reply};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, RS_HEADER_LEN, decode, encode};
use logging_service::path::{self, tier_name};
use logging_service::ring::{NAME_MAX, Record, Ring};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// Read endpoints held at once (administration Part E.6): the view broker's, which it keeps for the
/// boot once a `logs` grant has been used, and one more — `boot-probe`'s, in a test image. A third
/// is refused `WouldBlock`.
const MAX_READ_ENDPOINTS: usize = 2;
/// Read sessions open at once: a `log` running, and one more. A session is closed by its reader
/// when it has read what it wanted, so this bounds readers at the same moment, not in a boot.
const MAX_READ_SESSIONS: usize = 2;
/// The most per-principal log channels the server reads from at once.
///
/// **Derived, not chosen**: the server waits on one set holding the serving endpoint, the control
/// channel (administration Part E.4), the read endpoints and sessions (Part E.6) and every source,
/// so the ceiling is the kernel's fan-out limit less the rest. Written
/// this way rather than restating the number so that raising [`MAX_WAIT_HANDLES`] moves it
/// — this and `fs-server-ext4`'s session cap were separately-written `7`s until Slice C3.
/// Escaping the limit rather than raising it is `TODO(server-fanout)`; see
/// `docs/architecture/logging.md`.
const MAX_SOURCES: usize = MAX_WAIT_HANDLES - 2 - MAX_READ_ENDPOINTS - MAX_READ_SESSIONS;
/// `KError::PeerClosed` — the writer of a source channel is gone. Distinguished from
/// `WouldBlock` because the two demand opposite responses: wait again, or stop waiting
/// **forever**. See [`drain_source`].
const E_PEER_CLOSED: i64 = -13;
/// The resolve suffix, on the serving endpoint, that mints a read endpoint.
const READ_ENDPOINT: &str = "read-endpoint";
/// A `Read` reply's body: an IPC payload less the rsproto header.
const READ_BODY_CAP: usize = IPC_PAYLOAD_SIZE - RS_HEADER_LEN;

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_HANDLES: [u64; MAX_WAIT_HANDLES] = [0; MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; MAX_WAIT_HANDLES * WAIT_RESULT_SIZE] =
    [0; MAX_WAIT_HANDLES * WAIT_RESULT_SIZE];
static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;
static mut SRC_OUT0: u64 = 0;
static mut SRC_OUT1: u64 = 0;
static mut READ_BODY: [u8; READ_BODY_CAP] = [0; READ_BODY_CAP];

/// A per-principal log channel the server holds the read end of.
struct Source {
    /// The kept (read) endpoint; the client holds the transferred write end.
    handle: u64,
    principal: String,
    tier: u8,
    /// A named-source sub-label from the resolve path (`<principal>/<label>`), stamped on
    /// every record on this channel unless the record carries its own `source`.
    label: Option<String>,
}

/// A destination for stamped records. The serial sink is one; a disk or network sink slots in
/// behind this trait later. The ring is not one: `Read` answers from it, so it is [`Log`]'s own.
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

/// **Where a stamped record goes**: every sink, then the ring. Holds the sequence counter, so
/// every record is stamped by the one place that keeps them.
struct Log {
    sinks: Vec<Box<dyn Sink>>,
    ring: Ring,
    seq: u64,
}

impl Log {
    /// Stamp a record with the trusted fields and send it on.
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

/// Emit `msg` to the serial console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Exit the process (does not return).
fn exit(code: i64) -> ! {
    // SAFETY: SYS_PROCESS_EXIT terminates this process.
    unsafe { syscall1(SYS_PROCESS_EXIT, code as u64) };
    loop {
        core::hint::spin_loop();
    }
}

/// Read the monotonic clock (nanoseconds).
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

/// Create a connected channel pair into `(o0, o1)`; returns `(end0, end1)` or `None`.
fn make_channel(o0: *mut u64, o1: *mut u64) -> Option<(u64, u64)> {
    // SAFETY: o0/o1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, o0 as u64, o1 as u64, 4, 0) };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { (o0.read(), o1.read()) })
}

/// Receive one message on `endpoint` into the RECV_* statics. Returns the syscall result
/// (0 = a message was received; non-zero = WouldBlock / error).
fn recv(endpoint: u64) -> i64 {
    // SAFETY: RECV_* are valid writable buffers.
    unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            endpoint,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    }
}

/// Send `Meta::Ready` on the control channel, transferring `kernel_end` (the endpoint the
/// supervisor binds at the logging path). `false` on any failure.
fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let body_len = match librsproto::meta::ready(&mut body, b"logging-service") {
        Some(n) => n,
        None => return false,
    };
    // SAFETY: REPLY_MSG is a valid 4 KiB buffer; the rsproto message goes at offset 24.
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
    // SAFETY: valid endpoint + message + 1-handle transfer. NoBlock: the control inbox
    // starts empty.
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

/// Reply to a forwarded resolve on `serve_end`, transferring `write_end` as the resolved
/// `OBJECT_KIND_CHANNEL` capability (the client's per-principal log channel). `true` on a
/// successful send (the handle has moved to the caller).
fn reply_channel(serve_end: u64, request_id: u64, write_end: u64) -> bool {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    // content_len is unused for a channel; the handle rides in handles[0].
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset 24.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1)
        {
            Some(n) => n,
            None => return false,
        }
    };
    // SAFETY: stamp the header + the transferred-handle slot.
    let sr = unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = write_end;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    sr == 0
}

/// Send an error reply on `serve_end` (no transferred handle).
fn reply_error(serve_end: u64, request_id: u64, op: u16, kerror: i32) {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = error_body(&mut ebody, kerror, 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer.
    let rs_len = unsafe {
        match encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            op,
            request_id,
            RS_FLAG_REPLY | RS_FLAG_ERROR,
            &ebody[..elen],
            0,
        ) {
            Some(n) => n,
            None => return,
        }
    };
    // SAFETY: stamp the header; no transferred handles.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// What a forwarded resolve on the serving endpoint asked for.
enum Asked {
    /// A log channel for `(tier, principal, source label)`.
    Source(u8, String, Option<String>),
    /// A read endpoint (administration Part E.6).
    ReadEndpoint,
}

/// Handle one forwarded `Namespace::Resolve` on the serving endpoint: `read-endpoint` mints a read
/// endpoint; anything else is classified as a log path, and mints a per-principal channel (keep
/// the read end tagged, transfer the write end). Bad paths / over-capacity get an error reply.
fn process_resolve(serve_end: u64, sources: &mut Vec<Source>, read_ends: &mut Vec<u64>) {
    // Decode the rsproto request from the IpcMsg payload (offset 24, payload_len).
    // SAFETY: read the header length + form a bounded read-only slice over RECV_MSG.
    let (request_id, asked) = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        match decode(req) {
            Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                Some(r) => match core::str::from_utf8(r.suffix).ok() {
                    Some(suffix) if suffix.trim_matches('/') == READ_ENDPOINT => {
                        (m.request_id, Some(Asked::ReadEndpoint))
                    }
                    Some(suffix) => match path::classify(suffix) {
                        // **Names are bounded** (Part E.6): a principal is identity, so one too
                        // long to keep whole is refused rather than cut.
                        Some(c) if c.principal.len() > NAME_MAX || c.source.is_some_and(|l| l.len() > NAME_MAX) => {
                            reply_error(serve_end, m.request_id, m.op, KError::InvalidArgument.as_i32());
                            (m.request_id, None)
                        }
                        Some(c) => {
                            // Owned copies before we touch the shared buffers again.
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
        }
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

    let (read_end, write_end) = match make_channel(&raw mut SRC_OUT0, &raw mut SRC_OUT1) {
        Some(pair) => pair,
        None => {
            reply_error(serve_end, request_id, OP_NS_RESOLVE, KError::KernelError.as_i32());
            return;
        }
    };
    // Transfer the write end to the resolving client; keep the read end tagged.
    if reply_channel(serve_end, request_id, write_end) {
        match &label {
            Some(l) => kprint(
                format!("logging-service: opened {}/{}/{}\n", tier_name(tier), principal, l)
                    .as_bytes(),
            ),
            None => {
                kprint(format!("logging-service: opened {}/{}\n", tier_name(tier), principal).as_bytes())
            }
        }
        sources.push(Source { handle: read_end, principal, tier, label });
    } else {
        // Reply failed (the write end did not move): reclaim both ends.
        close(read_end);
        close(write_end);
    }
}

/// Close one of this service's handles.
fn close(h: u64) {
    // SAFETY: closing a handle this process owns.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
}

/// **Mint a read endpoint** (administration Part E.6): a channel whose other end is the answer to
/// the resolve, which a supervisor binds as a server endpoint — the view broker, at `/dev/logs` —
/// so every resolve there arrives here, on the end kept. At most [`MAX_READ_ENDPOINTS`].
fn mint_read_endpoint(serve_end: u64, request_id: u64, read_ends: &mut Vec<u64>) {
    if read_ends.len() >= MAX_READ_ENDPOINTS {
        return reply_error(serve_end, request_id, OP_NS_RESOLVE, KError::WouldBlock.as_i32());
    }
    let Some((ours, theirs)) = make_channel(&raw mut SRC_OUT0, &raw mut SRC_OUT1) else {
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

/// The rsproto message in `RECV_MSG`, and **every handle it carried closed**: nothing a reader
/// sends needs one, and one kept unread would leak.
fn received() -> Option<librsproto::Message<'static>> {
    // SAFETY: RECV_COUNT/RECV_HANDLES were written by the `recv` that filled RECV_MSG; this
    // process is single-threaded, so nothing writes them while the slice below is read.
    unsafe {
        for i in 0..RECV_COUNT.min(8) {
            close(RECV_HANDLES[i]);
        }
        RECV_COUNT = 0;
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        decode(req).ok()
    }
}

/// **A resolve on a read endpoint opens a read session**, whatever its suffix: a channel of its
/// own, at most [`MAX_READ_SESSIONS`] at once. `false` once the endpoint's binding is gone, so it
/// leaves the wait set rather than being signalled forever.
fn serve_read_endpoint(from: u64, sessions: &mut Vec<u64>) -> bool {
    loop {
        let rc = recv(from);
        if rc == E_PEER_CLOSED {
            return false;
        }
        if rc != 0 {
            return true;
        }
        let Some(m) = received() else { continue };
        let (op, request_id) = (m.op, m.request_id);
        if op != OP_NS_RESOLVE {
            reply_error(from, request_id, op, KError::Unsupported.as_i32());
            continue;
        }
        if sessions.len() >= MAX_READ_SESSIONS {
            reply_error(from, request_id, op, KError::WouldBlock.as_i32());
            continue;
        }
        let Some((ours, theirs)) = make_channel(&raw mut SRC_OUT0, &raw mut SRC_OUT1) else {
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

/// **A read session**: each `Log::Read` answered from the ring, and anything else refused.
/// `false` once its reader has closed it.
fn serve_read_session(h: u64, ring: &Ring) -> bool {
    loop {
        let rc = recv(h);
        if rc == E_PEER_CLOSED {
            return false;
        }
        if rc != 0 {
            return true;
        }
        let Some(m) = received() else { continue };
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

/// Answer a `Read`: the records after `after`, oldest first, as many as one reply holds and no more
/// than `max` (`0` for no limit). None left is an empty reply, which is how a reader knows it has
/// read to the end. Every record fits an empty reply ([`logging_service::ring::MESSAGE_KEPT`]), so
/// a reply that has any to give gives at least one.
fn reply_records(to: u64, request_id: u64, ring: &Ring, after: u64, max: u32) {
    // SAFETY: READ_BODY is this single-threaded service's scratch, used by no one else.
    let body = unsafe { &mut *(&raw mut READ_BODY) };
    let Some(mut w) = ReadReplyWriter::new(body, ring.oldest()) else {
        return reply_error(to, request_id, OP_LOG_READ, KError::KernelError.as_i32());
    };
    for r in ring.after(after) {
        if max != 0 && w.count() >= max {
            break;
        }
        let rec = ReadRecord {
            sequence: r.sequence,
            time: r.time,
            timestamp: r.timestamp,
            tier: r.tier,
            level: r.level,
            principal: r.principal.as_bytes(),
            source: r.source.as_deref().map(str::as_bytes),
            message: r.message.as_bytes(),
        };
        if !w.push(&rec) {
            break;
        }
    }
    let len = w.finish();
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset 24, and READ_BODY is
    // not REPLY_MSG.
    let rs_len = unsafe {
        let body = &(&*(&raw const READ_BODY))[..len];
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_LOG_READ, request_id, RS_FLAG_REPLY, body, 0) {
            Some(n) => n,
            None => return,
        }
    };
    // SAFETY: stamp the header; no transferred handles. NoBlock: a reader that does not read its
    // replies loses them, and never holds this service up.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            to,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// Decode a `TSM1` typed-stream message and render its rows through the sinks (each row as
/// a `name=value …` line, stamped like any other record), plus a one-time marker naming the
/// schema's columns — the end-to-end demonstration of a typed stream flowing through the log
/// channel. A malformed / truncated stream is dropped, never fatal.
fn render_typed(body: &[u8], principal: &str, tier: u8, chan_label: &Option<String>, log: &mut Log) {
    let mut tr = match libstream::TableReader::new(body) {
        Ok(t) => t,
        Err(_) => {
            kprint(b"logging-service: malformed typed stream (drop)\n");
            return;
        }
    };
    let schema = tr.schema().clone();
    // One-time marker: the typed header decoded; show which columns arrived.
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

/// Render one decoded row as `name=value name=value …`.
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
            // Collection cells are summarised compactly — a log line stays one line.
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

/// Drain and stamp every queued `LogRecord` on the source channel `h`, routing each through
/// `log` to the sinks and the ring.
fn drain_source(h: u64, sources: &[Source], log: &mut Log) -> bool {
    let (principal, tier, chan_label) = match sources.iter().find(|s| s.handle == h) {
        Some(s) => (s.principal.clone(), s.tier, s.label.clone()),
        // An unknown handle is not a live source, so reporting it dead is also what gets
        // it out of the wait set rather than leaving it to be re-signaled forever.
        None => return true,
    };
    loop {
        let rc = recv(h);
        if rc == E_PEER_CLOSED {
            // The principal holding the write end exited. **Terminal, and it has to be
            // acted on** — this is the one recv outcome that must not be retried.
            //
            // A closed peer is permanently `signaled`, so a dead handle left in the wait
            // set makes `sys_wait` return instantly, every time, forever: this service
            // spins at 100% of a CPU. The cost is not the wasted cycles. Deferred handle
            // reclamation runs in the **idle thread**, and a run queue that is never
            // empty means the CPU never idles — so *no exited process on the system is
            // ever reclaimed*, and every pipe they held stays open. One coreutil leaving
            // a log channel behind hung the shell on an unrelated pipe, three subsystems
            // away. See the 2026-07-31 decision-log entry.
            return true;
        }
        if rc != 0 {
            break; // WouldBlock: drained
        }
        // SAFETY: read payload_len, then a bounded read-only slice over RECV_MSG.
        let body: &[u8] = unsafe {
            let payload_len =
                u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            )
        };
        // Route by the leading magic: a TSM1 typed stream is decoded + rendered; anything
        // else is a text LogRecord append.
        if body.starts_with(&libstream::wire::MAGIC) {
            render_typed(body, &principal, tier, &chan_label, log);
            continue;
        }
        let la = match parse_append(body) {
            Some(la) => la,
            None => continue, // malformed record: drop
        };
        let message = String::from(core::str::from_utf8(la.message).unwrap_or("<non-utf8>"));
        // A record's own `source` wins; otherwise the channel's named-source label.
        let source = la
            .source
            .map(|s| String::from(core::str::from_utf8(s).unwrap_or("?")))
            .or_else(|| chan_label.clone());
        log.record(principal.clone(), tier, la.level, message, source);
    }
    false // drained, still live
}

/// The serve loop: multi-wait on the serving endpoint, the control channel, the read endpoints and
/// sessions, and every per-principal channel; forwarded resolves mint channels or read endpoints,
/// log appends are stamped and sunk, and `Read`s answered from the ring. Never returns.
///
/// **It exits on `CTRL_OP_SHUTDOWN`** (administration Part E.4), after sinking every record
/// already queued: a shutdown asks the services in the reverse of their start order, so by then
/// everything started after the log has said its last. `service --stop` never sends it, since
/// the log is `essential`.
fn serve_loop(serve_end: u64, mut control: u64, log: &mut Log) -> ! {
    kprint(b"logging-service: serving\n");
    let mut sources: Vec<Source> = Vec::new();
    let mut read_ends: Vec<u64> = Vec::new();
    let mut sessions: Vec<u64> = Vec::new();
    let mut dead: Vec<u64> = Vec::new();
    loop {
        // Build the wait set: [serve_end, control?] + read endpoints + read sessions + sources.
        // SAFETY: WAIT_HANDLES has MAX_WAIT_HANDLES slots, and each list is capped — the sources
        // at MAX_SOURCES, which is what is left of MAX_WAIT_HANDLES after the rest — so `count`
        // never passes it.
        let count = unsafe {
            let mut count = 0;
            let mut put = |h: u64| {
                WAIT_HANDLES[count] = h;
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
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers sized for `count`.
        let waited = unsafe {
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                count as u64,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited < 1 {
            continue;
        }
        // Each signaled handle is one 24-byte IoResult (handle @0).
        let signalled: Vec<u64> = (0..waited as usize)
            .map(|j| {
                let off = j * 24;
                // SAFETY: `waited` records were written; `off + 8` stays inside WAIT_RESULTS.
                unsafe {
                    u64::from_le_bytes([
                        WAIT_RESULTS[off], WAIT_RESULTS[off + 1], WAIT_RESULTS[off + 2],
                        WAIT_RESULTS[off + 3], WAIT_RESULTS[off + 4], WAIT_RESULTS[off + 5],
                        WAIT_RESULTS[off + 6], WAIT_RESULTS[off + 7],
                    ])
                }
            })
            .collect();
        // **Closes before resolves, in every wake** (the lesson of the view broker's, PR #343's
        // E.4a): a reader that lets go of a session or an endpoint and asks for another at once
        // must find the first retired, or it is refused a slot that is free. So the sessions and
        // sources first, then the read endpoints, then the serving endpoint.
        for &h in &signalled {
            if h == control {
                match libkern::control::recv(control) {
                    libkern::control::Control::Op(CTRL_OP_SHUTDOWN) => {
                        for s in &sources {
                            drain_source(s.handle, &sources, log);
                        }
                        kprint(b"logging-service: asked to stop, exiting\n");
                        exit(0);
                    }
                    libkern::control::Control::Closed => control = 0,
                    _ => {}
                }
            } else if sessions.contains(&h) {
                if !serve_read_session(h, &log.ring) {
                    sessions.retain(|&e| e != h);
                    close(h);
                }
            } else if h != serve_end && !read_ends.contains(&h) && drain_source(h, &sources, log) {
                dead.push(h);
            }
        }
        // Retire dead sources *after* their sweep — `drain_source` borrows `sources`, and a
        // source removed mid-iteration would shift the handles the results still name. **A dead
        // source is retired, not waited on**: a closed peer is signalled forever.
        for h in dead.drain(..) {
            sources.retain(|s| s.handle != h);
            close(h);
        }
        for &h in &signalled {
            if read_ends.contains(&h) && !serve_read_endpoint(h, &mut sessions) {
                read_ends.retain(|&e| e != h);
                close(h);
            }
        }
        if signalled.contains(&serve_end) {
            // Drain every queued forwarded resolve.
            while recv(serve_end) == 0 {
                process_resolve(serve_end, &mut sources, &mut read_ends);
            }
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the inherited root
/// namespace (unused — clients bring their own log endpoint by resolving), `rdx` = the
/// control-channel endpoint the supervisor installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"logging-service: up\n");

    let (kernel_end, serve_end) = match make_channel(&raw mut CTRL_OUT0, &raw mut CTRL_OUT1) {
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

    let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
    sinks.push(Box::new(SerialSink));
    let mut log = Log { sinks, ring: Ring::new(), seq: 0 };
    serve_loop(serve_end, control, &mut log);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"logging-service: PANIC\n");
    exit(1);
}
