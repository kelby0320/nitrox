//! `heartbeat` — a trivial supervised service, the demo subject for `service-mgr`.
//!
//! A minimal long-running daemon: it beats on a timer and **listens on its control
//! channel** for a shutdown command (`service-mgr` keeps the other end). On a
//! `CTRL_OP_SHUTDOWN` it stops cleanly. It waits on the control endpoint with a
//! per-beat deadline, so the same `sys_wait` both paces the beats (on timeout) and
//! delivers control messages (on signal) — no second thread.
//!
//! If spawned without a control channel (`control == 0`) it falls back to a bounded
//! run so it never hangs unsupervised.
//!
//! It emits its beats as a **typed stream** (TSM1, via `libstream`) to its `log` endpoint
//! (handed in by service-mgr at `rcx`): each beat is a one-row typed table
//! `{ seq, uptime_ns, healthy }` — the logging service detects the `TSM1` magic and renders
//! the decoded row. Its "up" status and the app-facing **self-registration** `worker`
//! source (`liblog::open_source`) still send text `LogRecord`s via `liblog`, so both log
//! paths — and the logging service's magic-based routing between them — are exercised.
//!
//! `#![no_std]` + `#![no_main]`; `alloc` (via `libheap`) for the typed encoder;
//! `libkern` + `libstream` + `liblog`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use libkern::*;
use liblog::Logger;
use libstream::wire::Result as StreamResult;
use libstream::{Schema, SliceSink, TableWriter, TypeModifiers, TypeTag, TypedRecord, Value, WireError};

/// The global allocator — the typed-beat encoder allocates a small schema + row.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The `IpcMsg` payload offset (after the 24-byte header), mirroring `liblog`.
const PAYLOAD_OFF: usize = 24;

/// One beat: a row of the typed stream heartbeat emits on its log endpoint.
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
        vec![
            Value::Int(self.seq),
            Value::Int(self.uptime_ns),
            Value::Bool(self.healthy),
        ]
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

/// Beat interval — the deadline between beats while idle-waiting on control.
const BEAT_INTERVAL_NS: u64 = 300_000_000; // 300 ms
/// Fallback beat count when spawned without a control channel.
const FALLBACK_BEATS: u32 = 3;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut CLOCK_BUF: u64 = 0;
static mut RECV_MSG: IpcMsg = IpcMsg::ZEROED;
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: u64 = 0;

/// Emit `msg` to the serial console via the debug kprint syscall.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`; the slice is valid.
    unsafe {
        syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0);
    }
}

/// Print a small unsigned decimal.
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

/// Exit the process with `code` (does not return).
fn exit(code: i64) -> ! {
    // SAFETY: SYS_PROCESS_EXIT terminates this process; it does not return.
    unsafe { syscall1(SYS_PROCESS_EXIT, code as u64) };
    loop {
        core::hint::spin_loop();
    }
}

/// Read the control-channel message that just signalled and return its opcode (the
/// first payload byte), or `None` on a failed/empty receive.
fn recv_control_op(control: u64) -> Option<u8> {
    // SAFETY: valid recv out-params; on success the kernel writes the message.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            control,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr != 0 {
        return None;
    }
    // SAFETY: on success the kernel filled RECV_MSG's header + payload.
    let plen = unsafe { (&raw const RECV_MSG.header.payload_len).read() };
    if plen < 1 {
        return None;
    }
    // SAFETY: payload[0] is within the message buffer.
    Some(unsafe { (&raw const RECV_MSG.payload[0]).read() })
}

/// Read the monotonic clock (nanoseconds).
fn clock_now() -> u64 {
    // SAFETY: CLOCK_BUF is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: the kernel wrote the ns count into CLOCK_BUF.
    unsafe { (&raw const CLOCK_BUF).read() }
}

/// Encode `beat` as a complete one-row TSM1 stream into `payload` (an `IpcMsg` body),
/// returning the byte length, or `None` if it does not fit.
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

/// Emit a beat as a typed stream to the log endpoint. With no endpoint (unsupervised /
/// no logging service) it beats visibly to the serial console instead.
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
    // Stamp the IpcMsg header: payload_len @4, handle_count @8 = 0 (mirrors liblog).
    buf[4..8].copy_from_slice(&(body_len as u32).to_le_bytes());
    buf[8] = 0;
    let no_handles = [0u64; 1];
    // SAFETY: `buf` is a valid IpcMsg (header + `body_len` payload bytes at offset 24);
    // no transferred handles; NoBlock so a slow/full sink never blocks the beat loop.
    unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            log_ep,
            buf.as_ptr() as u64,
            no_handles.as_ptr() as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// Wait (bounded) for service-mgr's log-endpoint handoff on the control channel and
/// return the transferred handle (`0` if none arrives in time). The handoff is the
/// service's **first** control message — one moved handle, empty payload — so this runs
/// once at startup before the beat loop. The spawn ABI delivers only the control handle
/// to a register; the log endpoint arrives here instead.
fn recv_log_handoff(control: u64) -> u64 {
    // SAFETY: CLOCK_BUF is a writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: the kernel wrote the ns count into CLOCK_BUF.
    let now = unsafe { (&raw const CLOCK_BUF).read() };
    let deadline = now.saturating_add(2_000_000_000); // 2 s
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = control;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            deadline,
        )
    };
    if waited < 1 {
        return 0; // no handoff (e.g. no logging service) — run without structured logging
    }
    // SAFETY: valid recv out-params; on success the log endpoint rides in handles[0].
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            control,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr != 0 {
        return 0;
    }
    // SAFETY: the kernel wrote the transferred-handle count + values.
    let count = unsafe { (&raw const RECV_COUNT).read() };
    if count >= 1 {
        unsafe { (&raw const RECV_HANDLES[0]).read() }
    } else {
        0
    }
}

/// The daemon loop: beat on a deadline, exit on a control shutdown. Beats emit as log
/// records through `logger` (the service's `log` endpoint).
fn run_daemon(control: u64, log_ep: u64, logger: &Logger, root_ns: u64) -> ! {
    kprint(b"heartbeat: up (daemon)\n");
    // Self-registration demo: resolve a named `worker` source ourselves and log through
    // it — the app-facing register-then-log path. Records on it carry source="worker"
    // (from the path label, stamped by the logging service).
    if root_ns != 0 {
        let worker = liblog::open_source(root_ns, b"/log/system/heartbeat/worker");
        worker.info("worker source online");
    }
    if logger.is_valid() {
        logger.info("up (daemon)"); // text status via liblog (LogRecord path)
    }
    let start_ns = clock_now();
    let mut beat = 1i64;
    // Emit the first typed beat immediately (before the timed loop), so the typed path is
    // demonstrated even under a fast boot where the verdict ends the run within one beat
    // interval. Subsequent beats fire on the timer below.
    emit_beat_typed(
        log_ep,
        &Beat {
            seq: beat,
            uptime_ns: 0,
            healthy: true,
        },
    );
    beat += 1;
    loop {
        // Arm the beat deadline (absolute monotonic) and wait on the control endpoint.
        let now = clock_now();
        let deadline = now.saturating_add(BEAT_INTERVAL_NS);
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
        let waited = unsafe {
            WAIT_HANDLES[0] = control;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                deadline,
            )
        };
        if waited >= 1 {
            // The control endpoint signalled — a message is waiting.
            match recv_control_op(control) {
                Some(CTRL_OP_SHUTDOWN) => {
                    kprint(b"heartbeat: shutdown requested, exiting\n");
                    exit(0);
                }
                _ => { /* unknown/empty op — ignore and keep beating */ }
            }
        } else {
            // Deadline reached, no control message — emit a typed beat.
            emit_beat_typed(
                log_ep,
                &Beat {
                    seq: beat,
                    uptime_ns: now.saturating_sub(start_ns) as i64,
                    healthy: true,
                },
            );
            beat = beat.wrapping_add(1);
        }
    }
}

/// Bootstrap registers (see init): `rdi` = notification channel, `rsi` = namespace,
/// `rdx` = the control-channel endpoint moved in by service-mgr (0 if none). The `log`
/// endpoint is *not* a spawn register — service-mgr hands it over the control channel
/// (see [`recv_log_handoff`]).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    // Receive the log endpoint service-mgr transfers over the control channel, then log
    // through it. `0` (no handoff) → the logger is a no-op and beats fall back to kprint.
    let log_ep = if control != 0 { recv_log_handoff(control) } else { 0 };
    let logger = Logger::new(log_ep);
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
