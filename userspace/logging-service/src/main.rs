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
//! `#![no_std]` + `#![no_main]`; `libkern` + `libheap` + `librsproto`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libkern::*;
use librsproto::error::error_body;
use librsproto::log::{
    LEVEL_CRITICAL, LEVEL_DEBUG, LEVEL_ERROR, LEVEL_INFO, LEVEL_TRACE, LEVEL_WARN, parse_append,
};
use librsproto::namespace::{OBJECT_KIND_CHANNEL, parse_resolve_request, resolve_reply};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use logging_service::path::{self, tier_name};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// Kernel `MAX_WAIT_HANDLES` is 8; one slot is the serving endpoint, so we can wait on at
/// most this many per-principal log channels at once. Scaling past this (an aggregate
/// waitable / raised limit) is deferred — see `docs/architecture/logging.md`.
const MAX_SOURCES: usize = 7;
/// In-memory keep-recent ring capacity (records).
const RING_CAP: usize = 256;

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_RESULTS: [u8; 8 * 24] = [0; 8 * 24];
static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;
static mut SRC_OUT0: u64 = 0;
static mut SRC_OUT1: u64 = 0;

/// A per-principal log channel the server holds the read end of.
struct Source {
    /// The kept (read) endpoint; the client holds the transferred write end.
    handle: u64,
    principal: String,
    tier: u8,
}

/// A stamped log record. The trusted fields (`principal`/`tier`/`timestamp`/`sequence`)
/// are supplied by the server; the rest are the emitter's claims.
#[derive(Clone)]
struct Record {
    principal: String,
    tier: u8,
    timestamp: u64,
    sequence: u64,
    level: u8,
    message: String,
    source: Option<String>,
}

/// A destination for stamped records. Slice 1 ships serial + in-memory ring; a disk /
/// network sink slots in behind this trait later.
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

/// A bounded keep-recent ring of stamped records (for later read-back).
struct RingSink {
    buf: VecDeque<Record>,
}
impl Sink for RingSink {
    fn write(&mut self, rec: &Record) {
        if self.buf.len() == RING_CAP {
            self.buf.pop_front();
        }
        self.buf.push_back(rec.clone());
    }
}

fn level_name(level: u8) -> &'static str {
    match level {
        LEVEL_TRACE => "TRACE",
        LEVEL_DEBUG => "DEBUG",
        LEVEL_INFO => "INFO",
        LEVEL_WARN => "WARN",
        LEVEL_ERROR => "ERROR",
        LEVEL_CRITICAL => "CRIT",
        _ => "?",
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

/// Handle one forwarded `Namespace::Resolve` on the serving endpoint: classify the path,
/// mint a per-principal channel (keep the read end tagged, transfer the write end), and
/// reply. Bad paths / over-capacity get an error reply.
fn process_resolve(serve_end: u64, sources: &mut Vec<Source>) {
    // Decode the rsproto request from the IpcMsg payload (offset 24, payload_len).
    // SAFETY: read the header length + form a bounded read-only slice over RECV_MSG.
    let (op, request_id, req_ok) = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        match decode(req) {
            Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                Some(r) => match core::str::from_utf8(r.suffix).ok().and_then(path::classify) {
                    Some(c) => {
                        // Owned copies before we touch the shared buffers again.
                        (m.op, m.request_id, Some((c.tier, String::from(c.principal))))
                    }
                    None => {
                        reply_error(serve_end, m.request_id, m.op, KError::NotFound.as_i32());
                        (m.op, m.request_id, None)
                    }
                },
                None => {
                    reply_error(serve_end, m.request_id, m.op, KError::InvalidArgument.as_i32());
                    (m.op, m.request_id, None)
                }
            },
            Ok(m) => {
                reply_error(serve_end, m.request_id, m.op, KError::Unsupported.as_i32());
                (m.op, m.request_id, None)
            }
            Err(_) => (0, 0, None),
        }
    };
    let _ = op;
    let (tier, principal) = match req_ok {
        Some(v) => v,
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
        sources.push(Source { handle: read_end, principal, tier });
    } else {
        // Reply failed (the write end did not move): reclaim both ends.
        // SAFETY: closing our own handles.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, read_end);
            syscall1(SYS_HANDLE_CLOSE, write_end);
        }
    }
}

/// Drain and stamp every queued `LogRecord` on the source channel `h`, routing each to
/// the sinks. `seq` is the global monotonic sequence counter.
fn drain_source(h: u64, sources: &[Source], sinks: &mut [Box<dyn Sink>], seq: &mut u64) {
    let (principal, tier) = match sources.iter().find(|s| s.handle == h) {
        Some(s) => (s.principal.clone(), s.tier),
        None => return, // unknown handle (shouldn't happen)
    };
    loop {
        if recv(h) != 0 {
            break; // WouldBlock: drained
        }
        // SAFETY: read payload_len then a bounded read-only slice over RECV_MSG.
        let la = unsafe {
            let payload_len =
                u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let body = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            parse_append(body)
        };
        let la = match la {
            Some(la) => la,
            None => continue, // malformed record: drop
        };
        *seq += 1;
        let rec = Record {
            principal: principal.clone(),
            tier,
            timestamp: clock_now(),
            sequence: *seq,
            level: la.level,
            message: String::from(core::str::from_utf8(la.message).unwrap_or("<non-utf8>")),
            source: la
                .source
                .map(|s| String::from(core::str::from_utf8(s).unwrap_or("?"))),
        };
        for sink in sinks.iter_mut() {
            sink.write(&rec);
        }
    }
}

/// The serve loop: multi-wait on the serving endpoint + every per-principal channel;
/// forwarded resolves mint channels, log appends are stamped and sunk. Never returns.
fn serve_loop(serve_end: u64, sinks: &mut [Box<dyn Sink>]) -> ! {
    kprint(b"logging-service: serving\n");
    let mut sources: Vec<Source> = Vec::new();
    let mut seq: u64 = 0;
    loop {
        // Build the wait set: [serve_end] + each source read end.
        let n_src = sources.len();
        // SAFETY: WAIT_HANDLES has 8 slots; 1 + n_src <= 1 + MAX_SOURCES = 8.
        unsafe {
            WAIT_HANDLES[0] = serve_end;
            for i in 0..n_src {
                WAIT_HANDLES[1 + i] = sources[i].handle;
            }
        }
        let count = 1 + n_src;
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers sized for `count` <= 8.
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
        // Each signaled handle is one 24-byte IoResult (handle @0); drain it.
        for j in 0..(waited as usize) {
            let off = j * 24;
            // SAFETY: `waited` records were written; `off + 8 <= 8*24`.
            let h = unsafe {
                u64::from_le_bytes([
                    WAIT_RESULTS[off], WAIT_RESULTS[off + 1], WAIT_RESULTS[off + 2],
                    WAIT_RESULTS[off + 3], WAIT_RESULTS[off + 4], WAIT_RESULTS[off + 5],
                    WAIT_RESULTS[off + 6], WAIT_RESULTS[off + 7],
                ])
            };
            if h == serve_end {
                // Drain every queued forwarded resolve.
                while recv(serve_end) == 0 {
                    process_resolve(serve_end, &mut sources);
                }
            } else {
                drain_source(h, &sources, sinks, &mut seq);
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
    sinks.push(Box::new(RingSink { buf: VecDeque::new() }));
    serve_loop(serve_end, &mut sinks);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"logging-service: PANIC\n");
    exit(1);
}
