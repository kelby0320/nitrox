//! `heartbeat` — a trivial supervised service, the demo subject for `service-mgr`.
//!
//! Beats on a timer while waiting on its control channel with a per-beat deadline, so one
//! `sys_wait` paces the beats and delivers `CTRL_OP_SHUTDOWN`. Beats are a typed TSM1 stream to
//! the `log` endpoint; the "up" status and the `worker` source send text `LogRecord`s, so a gate
//! sees both log paths and the logging service's magic-based routing. With no control channel
//! it runs a bounded loop.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use libkern::control::Control;
use libkern::*;
use liblog::Logger;
use libstream::wire::Result as StreamResult;
use libstream::{Schema, SliceSink, TableWriter, TypeModifiers, TypeTag, TypedRecord, Value, WireError};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const PAYLOAD_OFF: usize = 24;

struct Beat {
    seq: i64,
    uptime_ns: i64,
    healthy: bool,
}

impl TypedRecord for Beat {
    fn schema() -> Schema {
        Schema::new()
            .field("seq", TypeTag::Int, TypeModifiers::NONE)
            .field("uptime_ns", TypeTag::Int, TypeModifiers::NONE)
            .field("healthy", TypeTag::Bool, TypeModifiers::NONE)
    }

    fn to_values(&self) -> Vec<Value> {
        vec![Value::Int(self.seq), Value::Int(self.uptime_ns), Value::Bool(self.healthy)]
    }

    fn from_values(v: &[Value]) -> StreamResult<Self> {
        if v.len() != 3 {
            return Err(WireError::SchemaMismatch);
        }
        Ok(Beat {
            seq: v[0].as_int().ok_or(WireError::SchemaMismatch)?,
            uptime_ns: v[1].as_int().ok_or(WireError::SchemaMismatch)?,
            healthy: v[2].as_bool().ok_or(WireError::SchemaMismatch)?,
        })
    }
}

/// Deadline between beats while waiting on control.
const BEAT_INTERVAL_NS: u64 = 300_000_000; // 300 ms
/// Beats when spawned without a control channel.
const FALLBACK_BEATS: u32 = 3;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut CLOCK_BUF: u64 = 0;
static mut RECV_MSG: [u8; IPC_MSG_SIZE] = [0; IPC_MSG_SIZE];

fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`; the slice is valid.
    unsafe {
        syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0);
    }
}

fn kprint_u32(mut v: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if v == 0 {
        kprint(b"0");
        return;
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    kprint(&buf[i..]);
}

fn exit(code: i64) -> ! {
    // SAFETY: SYS_PROCESS_EXIT terminates this process; it does not return.
    unsafe { syscall1(SYS_PROCESS_EXIT, code as u64) };
    loop {
        core::hint::spin_loop();
    }
}

fn clock_now() -> u64 {
    // SAFETY: CLOCK_BUF is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
    unsafe { (&raw const CLOCK_BUF).read() }
}

/// Encode `beat` as a one-row TSM1 stream into `payload`; `None` if it does not fit.
fn encode_beat(payload: &mut [u8], beat: &Beat) -> Option<usize> {
    let mut sink = SliceSink::new(payload);
    {
        let mut tw = TableWriter::new(&mut sink);
        tw.write_schema_for::<Beat>().ok()?;
        tw.write_record(beat).ok()?;
        tw.finish_with_status(0).ok()?;
    }
    Some(sink.len())
}

/// Emit a beat to the log endpoint, or to the serial console when there is none.
fn emit_beat_typed(log_ep: u64, beat: &Beat) {
    if log_ep == 0 {
        kprint(b"heartbeat: beat ");
        kprint_u32(beat.seq as u32);
        kprint(b"\n");
        return;
    }
    let mut buf = [0u8; IPC_MSG_SIZE];
    let body_len = match encode_beat(&mut buf[PAYLOAD_OFF..], beat) {
        Some(n) => n,
        None => return, // does not fit — drop this beat
    };
    // IpcMsg header: payload_len at 4, handle_count at 8.
    buf[4..8].copy_from_slice(&(body_len as u32).to_le_bytes());
    buf[8] = 0;
    let no_handles = [0u64; 1];
    // SAFETY: `buf` is a valid IpcMsg (header + `body_len` payload bytes at offset 24);
    // no transferred handles; NoBlock so a slow/full sink never blocks the beat loop.
    unsafe {
        syscall5(SYS_CHANNEL_SEND, log_ep, buf.as_ptr() as u64, no_handles.as_ptr() as u64, 0, SENDMODE_NOBLOCK);
    }
}

/// Wait up to 2 s for the log endpoint service-mgr moves over the control channel as its
/// first message (`0` if none); the spawn ABI passes only the control handle.
fn recv_log_handoff(control: u64) -> u64 {
    // SAFETY: CLOCK_BUF is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
    let now = unsafe { (&raw const CLOCK_BUF).read() };
    let deadline = now.saturating_add(2_000_000_000); // 2 s
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = control;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline)
    };
    if waited < 1 {
        return 0; // no handoff (e.g. no logging service) — run without structured logging
    }
    // The log endpoint is the one handle the handoff brings; any more close as `sent` drops.
    // SAFETY: single-threaded; `RECV_MSG` is lent for the call.
    match unsafe { channel_recv_raw(control, &raw mut RECV_MSG) } {
        Ok(mut sent) => sent.take(0).unwrap_or(0),
        Err(_) => 0,
    }
}

/// The daemon loop: beat on a deadline, exit on a control shutdown.
fn run_daemon(control: u64, log_ep: u64, logger: &Logger, root_ns: u64) -> ! {
    kprint(b"heartbeat: up (daemon)\n");
    // The app-facing path: records on this source carry source="worker" from the path label.
    if root_ns != 0 {
        let worker = liblog::open_source(root_ns, b"/log/system/heartbeat/worker");
        worker.info("worker source online");
    }
    if logger.is_valid() {
        logger.info("up (daemon)"); // text status via liblog (LogRecord path)
    }
    let start_ns = clock_now();
    let mut beat = 1i64;
    // Beat once before the timed loop so a fast boot's verdict still sees a typed beat.
    emit_beat_typed(log_ep, &Beat { seq: beat, uptime_ns: 0, healthy: true });
    beat += 1;
    loop {
        let now = clock_now();
        let deadline = now.saturating_add(BEAT_INTERVAL_NS);
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
        let waited = unsafe {
            WAIT_HANDLES[0] = control;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline)
        };
        if waited >= 1 {
            match libkern::control::recv(control) {
                Control::Op(CTRL_OP_SHUTDOWN) => {
                    kprint(b"heartbeat: shutdown requested, exiting\n");
                    exit(0);
                }
                // The channel stays signalled once closed; waiting again would spin.
                Control::Closed => {
                    kprint(b"heartbeat: its supervisor has gone, exiting\n");
                    exit(0);
                }
                _ => { /* unknown/empty op — ignore and keep beating */ }
            }
        } else {
            emit_beat_typed(log_ep, &Beat { seq: beat, uptime_ns: now.saturating_sub(start_ns) as i64, healthy: true });
            beat = beat.wrapping_add(1);
        }
    }
}

/// Entry point: `rsi` = namespace, `rdx` = control endpoint from service-mgr (0 if none).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    // Receive the log endpoint service-mgr transfers over the control channel, then log
    // through it. `0` (no handoff) → the logger is a no-op and beats fall back to kprint.
    let log_ep = if control != 0 { recv_log_handoff(control) } else { 0 };
    let logger = Logger::borrowing(log_ep);
    if control != 0 {
        run_daemon(control, log_ep, &logger, root_ns);
    }
    // No control channel: a bounded run so we never hang unsupervised.
    kprint(b"heartbeat: up (no control channel; bounded run)\n");
    let mut beat = 1u32;
    while beat <= FALLBACK_BEATS {
        kprint(b"heartbeat: beat ");
        kprint_u32(beat);
        kprint(b"\n");
        beat += 1;
    }
    kprint(b"heartbeat: done, exiting 0\n");
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"heartbeat: PANIC\n");
    exit(1);
}
