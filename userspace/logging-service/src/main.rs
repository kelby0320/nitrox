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

use libkern::{CTRL_OP_SHUTDOWN, IPC_PAYLOAD_SIZE, KError, MAX_WAIT_HANDLES};
use libos::boot::Start;
use libos::control::{self, Control};
use libos::{Carried, Channel, Deadline, Handle, Msg, Only, WaitSet, log, time};
use librsproto::log::{LEVEL_INFO, OP_LOG_READ, level_name, parse_append, parse_read_request};
use librsproto::namespace::parse_resolve_request;
use librsproto::server;
use librsproto::{OP_NS_RESOLVE, RS_FLAG_REPLY, RS_HEADER_LEN, decode};
use logging_service::path::{self, tier_name};
use logging_service::ring::{NAME_MAX, Record, Ring, claimed_source};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

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
/// The resolve suffix that mints a read endpoint.
const READ_ENDPOINT: &str = "read-endpoint";
const READ_BODY_CAP: usize = IPC_PAYLOAD_SIZE - RS_HEADER_LEN;

type Endpoint = Handle<Channel, Only>;

/// A per-principal log channel the server holds the read end of.
struct Source {
    /// The kept (read) endpoint.
    handle: Endpoint,
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

/// Formats each record to a line on the serial console.
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
        log::write(line.as_bytes());
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
            timestamp: time::now().unwrap_or(0),
            time: time::wall().ok(),
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

/// What a forwarded resolve on the serving endpoint asked for.
enum Asked {
    /// A log channel for `(tier, principal, source label)`.
    Source(u8, String, Option<String>),
    /// A read endpoint.
    ReadEndpoint,
}

/// The three tables the serve loop waits on. Fixed, so a slot's index names it in a wake.
struct Tables {
    sources: [Option<Source>; MAX_SOURCES],
    read_ends: [Option<Endpoint>; MAX_READ_ENDPOINTS],
    sessions: [Option<Endpoint>; MAX_READ_SESSIONS],
}

/// What a wait's index names.
#[derive(Copy, Clone)]
enum Waited {
    Serve,
    Control,
    ReadEnd(usize),
    Session(usize),
    Source(usize),
}

/// Add `h` to `set` as `what`.
fn watch<'a>(set: &mut WaitSet<'a, MAX_WAIT_HANDLES>, names: &mut [Waited], h: &'a Endpoint, what: Waited) {
    if let Ok(i) = set.add(h) {
        names[i] = what;
    }
}

fn refuse(ch: &Endpoint, op: u16, request_id: u64, err: KError) {
    server::reply_error(ch, op, request_id, err, b"");
}

/// Handle one forwarded `Namespace::Resolve`: `read-endpoint` mints a read endpoint; anything
/// else is classified as a log path and mints a per-principal channel (keep the read end tagged,
/// transfer the write end). Bad paths and over-capacity get an error reply.
fn process_resolve(msg: &Msg, serve_end: &Endpoint, tables: &mut Tables) {
    let (request_id, asked) = match decode(msg.payload()) {
        Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
            Some(r) => match core::str::from_utf8(r.suffix).ok() {
                Some(suffix) if suffix.trim_matches('/') == READ_ENDPOINT => (m.request_id, Some(Asked::ReadEndpoint)),
                Some(suffix) => match path::classify(suffix) {
                    // A principal is identity, so one too long to keep whole is refused,
                    // not cut.
                    Some(c) if c.principal.len() > NAME_MAX || c.source.is_some_and(|l| l.len() > NAME_MAX) => {
                        refuse(serve_end, m.op, m.request_id, KError::InvalidArgument);
                        (m.request_id, None)
                    }
                    Some(c) => {
                        // Owned copies: `c` borrows the receive buffer the next message reuses.
                        let label = c.source.map(String::from);
                        (m.request_id, Some(Asked::Source(c.tier, String::from(c.principal), label)))
                    }
                    None => {
                        refuse(serve_end, m.op, m.request_id, KError::NotFound);
                        (m.request_id, None)
                    }
                },
                None => {
                    refuse(serve_end, m.op, m.request_id, KError::NotFound);
                    (m.request_id, None)
                }
            },
            None => {
                refuse(serve_end, m.op, m.request_id, KError::InvalidArgument);
                (m.request_id, None)
            }
        },
        Ok(m) => {
            refuse(serve_end, m.op, m.request_id, KError::Unsupported);
            (m.request_id, None)
        }
        Err(_) => (0, None),
    };
    let (tier, principal, label) = match asked {
        Some(Asked::Source(tier, principal, label)) => (tier, principal, label),
        Some(Asked::ReadEndpoint) => return mint_read_endpoint(serve_end, request_id, &mut tables.read_ends),
        None => return,
    };

    let Some(slot) = tables.sources.iter().position(Option::is_none) else {
        log::write(b"logging-service: source table full, refusing new log channel\n");
        refuse(serve_end, OP_NS_RESOLVE, request_id, KError::Unsupported);
        return;
    };

    let Ok((read_end, write_end)) = Handle::pair(4) else {
        refuse(serve_end, OP_NS_RESOLVE, request_id, KError::KernelError);
        return;
    };
    if server::reply_channel(serve_end, request_id, write_end) {
        match &label {
            Some(l) => {
                log::write(format!("logging-service: opened {}/{}/{}\n", tier_name(tier), principal, l).as_bytes())
            }
            None => log::write(format!("logging-service: opened {}/{}\n", tier_name(tier), principal).as_bytes()),
        }
        tables.sources[slot] = Some(Source { handle: read_end, principal, tier, label });
    }
}

/// **Mint a read endpoint**: a channel whose other end is the answer to the resolve, which a
/// supervisor binds as a server endpoint (the view broker, at `/dev/logs`), so every resolve
/// there arrives here, on the end kept. At most [`MAX_READ_ENDPOINTS`].
fn mint_read_endpoint(serve_end: &Endpoint, request_id: u64, read_ends: &mut [Option<Endpoint>]) {
    let Some(slot) = read_ends.iter().position(Option::is_none) else {
        return refuse(serve_end, OP_NS_RESOLVE, request_id, KError::WouldBlock);
    };
    let Ok((ours, theirs)) = Handle::pair(4) else {
        return refuse(serve_end, OP_NS_RESOLVE, request_id, KError::KernelError);
    };
    if server::reply_channel(serve_end, request_id, theirs) {
        read_ends[slot] = Some(ours);
        log::write(b"logging-service: a read endpoint minted\n");
    }
}

/// **A resolve on a read endpoint opens a read session**, whatever its suffix, at most
/// [`MAX_READ_SESSIONS`] at once. `false` once the binding is gone, so the endpoint leaves the
/// wait set rather than being signalled forever.
fn serve_read_endpoint(msg: &mut Msg, from: &Endpoint, sessions: &mut [Option<Endpoint>]) -> bool {
    loop {
        let _sent = match from.recv(msg) {
            Ok(sent) => sent,
            Err(e) if e.code() == KError::PeerClosed => return false,
            Err(_) => return true,
        };
        let Ok(m) = decode(msg.payload()) else { continue };
        let (op, request_id) = (m.op, m.request_id);
        if op != OP_NS_RESOLVE {
            refuse(from, op, request_id, KError::Unsupported);
            continue;
        }
        let Some(slot) = sessions.iter().position(Option::is_none) else {
            refuse(from, op, request_id, KError::WouldBlock);
            continue;
        };
        let Ok((ours, theirs)) = Handle::pair(4) else {
            refuse(from, op, request_id, KError::KernelError);
            continue;
        };
        if server::reply_channel(from, request_id, theirs) {
            sessions[slot] = Some(ours);
        }
    }
}

/// **A read session**: each `Log::Read` answered from the ring. `false` once its reader closed it.
fn serve_read_session(msg: &mut Msg, h: &Endpoint, ring: &Ring) -> bool {
    loop {
        let _sent = match h.recv(msg) {
            Ok(sent) => sent,
            Err(e) if e.code() == KError::PeerClosed => return false,
            Err(_) => return true,
        };
        let Ok(m) = decode(msg.payload()) else { continue };
        let (op, request_id) = (m.op, m.request_id);
        if op != OP_LOG_READ {
            refuse(h, op, request_id, KError::Unsupported);
            continue;
        }
        match parse_read_request(m.body) {
            Some((after, max)) => reply_records(h, request_id, ring, after, max),
            None => refuse(h, op, request_id, KError::InvalidArgument),
        }
    }
}

/// Answer a `Read` with [`Ring::fill_reply`]. None left is an empty reply, which is how a
/// reader knows it has read to the end.
fn reply_records(to: &Endpoint, request_id: u64, ring: &Ring, after: u64, max: u32) {
    let mut body = [0u8; READ_BODY_CAP];
    let Some(len) = ring.fill_reply(after, max, &mut body) else {
        return refuse(to, OP_LOG_READ, request_id, KError::KernelError);
    };
    // NoBlock: a reader that does not read its replies loses them, and never holds this service up.
    let _ = server::send(to, OP_LOG_READ, request_id, RS_FLAG_REPLY, &body[..len], &mut Carried::new());
}

/// Decode a `TSM1` typed-stream message and render each row as a `name=value …` record, after a
/// one-time marker naming the schema's columns. A malformed or truncated stream is dropped,
/// never fatal.
fn render_typed(body: &[u8], principal: &str, tier: u8, chan_label: &Option<String>, log: &mut Log) {
    let mut tr = match libstream::TableReader::new(body) {
        Ok(t) => t,
        Err(_) => {
            log::write(b"logging-service: malformed typed stream (drop)\n");
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
    log::write(cols.as_bytes());

    loop {
        match tr.next() {
            Some(Ok(libstream::Item::Row(values))) => {
                let source = chan_label.clone().or_else(|| Some(String::from("typed")));
                log.record(String::from(principal), tier, LEVEL_INFO, format_typed_row(&schema, &values), source);
            }
            Some(Ok(libstream::Item::End(_))) | None => break,
            Some(Ok(libstream::Item::Error(_))) => continue, // pass over in-stream errors
            Some(Err(_)) => {
                log::write(b"logging-service: typed decode error (drop rest)\n");
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

/// Drain and stamp every queued `LogRecord` on source channel `source`. `true` once its writer is gone.
fn drain_source(msg: &mut Msg, source: &Source, log: &mut Log) -> bool {
    let (principal, tier, chan_label) = (&source.principal, source.tier, &source.label);
    loop {
        let _sent = match source.handle.recv(msg) {
            Ok(sent) => sent,
            // **Terminal, and never retried.** A closed peer is permanently `signaled`, so a
            // dead handle left in the wait set makes `sys_wait` return at once, forever: this
            // service spins, the idle thread never runs, and with it no exited process on the
            // system is reclaimed (decision log, 2026-07-31).
            Err(e) if e.code() == KError::PeerClosed => return true,
            // WouldBlock: drained
            Err(_) => break,
        };
        let body = msg.payload();
        // A TSM1 typed stream is rendered; anything else is a text `LogRecord` append.
        if body.starts_with(&libstream::wire::MAGIC) {
            render_typed(body, principal, tier, chan_label, log);
            continue;
        }
        let la = match parse_append(body) {
            Some(la) => la,
            None => continue, // malformed record: drop
        };
        let message = String::from(core::str::from_utf8(la.message).unwrap_or("<non-utf8>"));
        log.record(principal.clone(), tier, la.level, message, claimed_source(la.source, chan_label));
    }
    false // drained, still live
}

/// The serve loop: multi-wait on the serving endpoint, the control channel, the read endpoints and
/// sessions, and every per-principal channel. Never returns.
///
/// **It exits on `CTRL_OP_SHUTDOWN`**, after sinking every record already queued: a shutdown asks
/// the services in the reverse of their start order, so by then everything started after the log
/// has said its last. `service --stop` never sends it, since the log is `essential`.
fn serve_loop(serve_end: Endpoint, mut control: Option<Endpoint>, log: &mut Log) -> ! {
    log::write(b"logging-service: serving\n");
    let mut tables = Tables {
        sources: [const { None }; MAX_SOURCES],
        read_ends: [const { None }; MAX_READ_ENDPOINTS],
        sessions: [const { None }; MAX_READ_SESSIONS],
    };
    let mut msg = Msg::new();
    loop {
        let mut names = [Waited::Serve; MAX_WAIT_HANDLES];
        // Each table is capped (the sources at MAX_SOURCES, what is left after the rest), so the
        // set never passes MAX_WAIT_HANDLES.
        let signalled = {
            let mut set = WaitSet::new();
            watch(&mut set, &mut names, &serve_end, Waited::Serve);
            if let Some(c) = &control {
                watch(&mut set, &mut names, c, Waited::Control);
            }
            for (i, e) in tables.read_ends.iter().enumerate() {
                if let Some(e) = e {
                    watch(&mut set, &mut names, e, Waited::ReadEnd(i));
                }
            }
            for (i, s) in tables.sessions.iter().enumerate() {
                if let Some(s) = s {
                    watch(&mut set, &mut names, s, Waited::Session(i));
                }
            }
            for (i, s) in tables.sources.iter().enumerate() {
                if let Some(s) = s {
                    watch(&mut set, &mut names, &s.handle, Waited::Source(i));
                }
            }
            set.wait(Deadline::NEVER)
        };
        let Ok(signalled) = signalled else { continue };
        // **Closes before resolves, in every wake**: a reader that lets go of a session or an
        // endpoint and asks for another at once must find the first retired, or it is refused a
        // slot that is free. So the sessions and sources first, then the read endpoints, then the
        // serving endpoint.
        for (i, _) in signalled.iter() {
            match names[i] {
                Waited::Control => {
                    if control::recv(&mut control) == Control::Op(CTRL_OP_SHUTDOWN) {
                        for s in tables.sources.iter().flatten() {
                            drain_source(&mut msg, s, log);
                        }
                        log::write(b"logging-service: asked to stop, exiting\n");
                        libos::exit(0);
                    }
                }
                Waited::Session(i) => {
                    if let Some(s) = &tables.sessions[i]
                        && !serve_read_session(&mut msg, s, &log.ring)
                    {
                        tables.sessions[i] = None;
                    }
                }
                Waited::Source(i) => {
                    if let Some(s) = &tables.sources[i]
                        && drain_source(&mut msg, s, log)
                    {
                        tables.sources[i] = None;
                    }
                }
                Waited::Serve | Waited::ReadEnd(_) => {}
            }
        }
        for (i, _) in signalled.iter() {
            if let Waited::ReadEnd(i) = names[i]
                && let Some(e) = &tables.read_ends[i]
                && !serve_read_endpoint(&mut msg, e, &mut tables.sessions)
            {
                tables.read_ends[i] = None;
            }
        }
        if signalled.iter().any(|(i, _)| matches!(names[i], Waited::Serve)) {
            loop {
                let _sent = match serve_end.recv(&mut msg) {
                    Ok(sent) => sent,
                    // A forwarding endpoint with no peer is the end of this server, not a message to skip: the
                    // kernel keeps a peer-closed channel signalled, so going round again would spin a CPU.
                    Err(e) if e.code() == KError::PeerClosed => {
                        log::write(b"logging-service: forwarding endpoint closed\n");
                        libos::exit(1);
                    }
                    Err(_) => break,
                };
                process_resolve(&msg, &serve_end, &mut tables);
            }
        }
    }
}

/// Bootstrap registers: `rdx` is the control-channel endpoint the supervisor installed; the rest
/// are unused, as clients bring their own log endpoint by resolving.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"logging-service: up\n");

    let control: Option<Endpoint> = start.take_handle0();
    let Ok((kernel_end, serve_end)) = Handle::pair(4) else {
        log::write(b"logging-service: channel create FAIL\n");
        libos::exit(1);
    };
    if !control.as_ref().is_some_and(|c| server::ready(c, b"logging-service", kernel_end)) {
        log::write(b"logging-service: Ready send FAIL\n");
        libos::exit(1);
    }

    let sinks: Vec<Box<dyn Sink>> = alloc::vec![Box::new(SerialSink)];
    let mut log = Log { sinks, ring: Ring::new(), seq: 0 };
    serve_loop(serve_end, control, &mut log);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"logging-service: PANIC\n");
    libos::exit(1);
}
