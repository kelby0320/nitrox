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

use libkern::CTRL_OP_SHUTDOWN;
use liblog::Logger;
use libos::boot::Start;
use libos::control::{self, Control};
use libos::log::{self, Line};
use libos::{Carried, Channel, Deadline, Handle, Msg, Namespace, NsReadOnly, Only, Ref, WaitSet, time};
use libstream::wire::Result as StreamResult;
use libstream::{Schema, SliceSink, TableWriter, TypeModifiers, TypeTag, TypedRecord, Value, WireError};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

type Endpoint = Handle<Channel, Only>;

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
fn emit_beat_typed(log_ep: Option<Ref<'_, Channel, Only>>, beat: &Beat) {
    let Some(ep) = log_ep else {
        Line::new().s(b"heartbeat: beat ").u(beat.seq as u64).end();
        return;
    };
    let mut msg = Msg::new();
    // One that does not fit is dropped.
    let Some(n) = encode_beat(msg.payload_mut(), beat) else { return };
    if msg.set_len(n).is_ok() {
        // Refused at once when full, so a slow sink never holds up the beat loop.
        let _ = ep.send(&msg, &mut Carried::new());
    }
}

/// Wait up to 2 s for the log endpoint service-mgr moves over the control channel as its
/// first message (`None` if none); the spawn ABI passes only the control handle.
fn recv_log_handoff(control: &Endpoint) -> Option<Endpoint> {
    let mut set = WaitSet::<1>::new();
    set.add(control).ok()?;
    if set.wait(Deadline::after(2_000_000_000).ok()?).ok()?.is_empty() {
        return None; // no handoff (e.g. no logging service) — run without structured logging
    }
    // The log endpoint is the one handle the handoff brings; any more close as `carried` drops.
    let mut msg = Msg::new();
    let mut carried = control.recv(&mut msg).ok()?;
    carried.take(0)
}

/// The daemon loop: beat on a deadline, exit on a control shutdown.
fn run_daemon(mut control: Option<Endpoint>, logger: &Logger, root: Option<&Handle<Namespace, NsReadOnly>>) -> ! {
    log::write(b"heartbeat: up (daemon)\n");
    // The app-facing path: records on this source carry source="worker" from the path label.
    if let Some(root) = root {
        Logger::open(root, "/log/system/heartbeat/worker").info("worker source online");
    }
    if logger.is_valid() {
        logger.info("up (daemon)"); // text status via liblog (LogRecord path)
    }
    let start_ns = time::now().unwrap_or(0);
    let mut beat = 1i64;
    // Beat once before the timed loop so a fast boot's verdict still sees a typed beat.
    emit_beat_typed(logger.endpoint(), &Beat { seq: beat, uptime_ns: 0, healthy: true });
    beat += 1;
    loop {
        let now = time::now().unwrap_or(start_ns);
        let deadline = Deadline::at(now.saturating_add(BEAT_INTERVAL_NS));
        let waited = {
            let mut set = WaitSet::<1>::new();
            if let Some(c) = &control {
                let _ = set.add(c);
            }
            set.wait(deadline)
        };
        match waited {
            Ok(fired) if !fired.is_empty() => match control::recv(&mut control) {
                Control::Op(CTRL_OP_SHUTDOWN) => {
                    log::write(b"heartbeat: shutdown requested, exiting\n");
                    libos::exit(0);
                }
                // The channel stays signalled once closed; waiting again would spin.
                Control::Closed => {
                    log::write(b"heartbeat: its supervisor has gone, exiting\n");
                    libos::exit(0);
                }
                _ => { /* unknown/empty op — ignore and keep beating */ }
            },
            // A wait that failed says nothing of the time, so the beat waits it out.
            failed => {
                if failed.is_err() {
                    time::sleep_until(deadline);
                }
                let uptime_ns = now.saturating_sub(start_ns) as i64;
                emit_beat_typed(logger.endpoint(), &Beat { seq: beat, uptime_ns, healthy: true });
                beat = beat.wrapping_add(1);
            }
        }
    }
}

/// Entry point: `rsi` = namespace, `rdx` = control endpoint from service-mgr (0 if none).
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    let control: Option<Endpoint> = start.take_handle0();
    if control.is_some() {
        // Receive the log endpoint service-mgr transfers over the control channel, then log
        // through it. None (no handoff) → the logger is a no-op and beats fall back to the console.
        let logger = control.as_ref().and_then(recv_log_handoff).map_or(Logger::none(), Logger::new);
        run_daemon(control, &logger, start.root.as_ref());
    }
    // No control channel: a bounded run so we never hang unsupervised.
    log::write(b"heartbeat: up (no control channel; bounded run)\n");
    for beat in 1..=FALLBACK_BEATS {
        Line::new().s(b"heartbeat: beat ").u(beat as u64).end();
    }
    log::write(b"heartbeat: done, exiting 0\n");
    libos::exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"heartbeat: PANIC\n");
    libos::exit(1);
}
