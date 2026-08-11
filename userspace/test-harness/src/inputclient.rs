//! `input-testclient` — proves the i8042 driver actually delivers events (plan M3 Part A).
//!
//! Bring-up is not the same as working. Before this, Part A could report
//! `ps2: keyboard mouse armed` on every boot with the ISR, the event ring and the parked
//! read having never run against a real keystroke — which is precisely the shape that let a
//! **one-way** Surface protocol ship in PR #174 behind three green CI jobs and 1213 passing
//! tests. A bound server that never answers, and an armed driver that never delivers, read
//! identically from the outside.
//!
//! So this consumes `/dev/input/new` — the **merged** stream from the `input-server` — and
//! prints what arrives. The host side (`cargo xtask check-input`) injects the keystrokes and
//! clicks over QMP and checks the decoded events against what it sent, which exercises the
//! whole path: i8042 → driver ring → raw node → server → merge → channel → here.
//!
//! It read the raw nodes directly until M3 Part B. It cannot any more, and that is the
//! design working: the driver is single-reader per device and the server now holds both, so
//! a second reader gets `WouldBlock`. Reading a raw node unfiltered is a keylogger, and the
//! binding is the whole of that boundary (`input-subsystem.md` §5).
//!
//! ## Why it announces itself first
//!
//! Injection is a host action against a guest that must already be waiting. The client
//! prints `listening` once its reads are parked, the harness waits for that line, and only
//! then injects — otherwise the event lands in the ring before anyone is reading, or worse,
//! before the device node has been resolved, and the test becomes a race it loses
//! intermittently.

#![no_std]
#![no_main]

use libkern::abi::{INPUT_EVENT_LEN, InputEvent};
use libkern::debug::Line;
use libkern::{
    RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT, SYS_CHANNEL_RECV, SYS_NS_LOOKUP, SYS_WAIT, exit, kprint,
    syscall4,
};

/// Bytes per record, from the shared ABI rather than a local literal — an earlier version
/// hardcoded `16` and read fields at literal offsets, so a kernel-side layout change would
/// have gone unnoticed until this gate happened to run (PR #178 review).
const EVENT_LEN: usize = INPUT_EVENT_LEN;
/// The last thing the harness injects, and therefore what "done" means here.
///
/// **A sentinel rather than a record count.** Counting was the first attempt and it was both
/// wrong (nine records, not ten — the motion is `REL_X`, `REL_Y`, `SYN`) and brittle for a
/// worse reason: how many records an injection produces is the driver's business, so a count
/// would need updating whenever the mouse packet framing changed. Waiting for the event that
/// *means* the sequence finished does not.
const DONE_CODE: u16 = libkern::abi::BTN_LEFT;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// Wait for a `PendingOperation` and return `(status, result)`.
fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    if waited != 1 {
        return (-1, 0);
    }
    // SAFETY: the kernel filled one 24-byte result record.
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

/// Resolve `path`, blocking on the lookup's `PendingOperation`.
fn lookup(ns: u64, path: &[u8], rights: u64) -> Option<u64> {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return None;
    }
    let (status, resolved) = po_wait(po as u64);
    if status != 0 || resolved == 0 { None } else { Some(resolved) }
}

/// The consumer end of `/dev/input/new`, and the last batch received on it.
struct Stream {
    channel: u64,
    msg: [u8; 4096],
    handles: [u64; 8],
    count: u64,
}

impl Stream {
    /// Resolve `/dev/input/new`, which mints a per-consumer channel.
    fn open(root_ns: u64) -> Option<Self> {
        let channel = lookup(root_ns, b"/dev/input/new", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT)?;
        Some(Self { channel, msg: [0; 4096], handles: [0; 8], count: 0 })
    }

    /// Block for one `Input::Events` message and print every record it carries.
    ///
    /// Returns whether the batch contained [`DONE_CODE`]'s press, or `None` if the channel
    /// failed.
    fn pump(&mut self) -> Option<bool> {
        loop {
            // SAFETY: waiting on this process's own channel handle.
            let waited = unsafe {
                WAIT_HANDLES[0] = self.channel;
                syscall4(
                    SYS_WAIT,
                    (&raw const WAIT_HANDLES) as u64,
                    1,
                    (&raw mut WAIT_RESULTS) as u64,
                    u64::MAX,
                )
            };
            if waited != 1 {
                return None;
            }
            // SAFETY: valid recv out-params on a live endpoint.
            let rr = unsafe {
                syscall4(
                    SYS_CHANNEL_RECV,
                    self.channel,
                    (&raw mut self.msg) as u64,
                    (&raw mut self.handles) as u64,
                    (&raw mut self.count) as u64,
                )
            };
            if rr == libkern::error::KError::WouldBlock.as_i32() as i64 {
                continue; // woken with nothing to take
            }
            if rr != 0 {
                return None;
            }
            let payload_len =
                u32::from_le_bytes([self.msg[4], self.msg[5], self.msg[6], self.msg[7]]) as usize;
            let req = &self.msg[PAYLOAD_OFF..PAYLOAD_OFF + payload_len.min(4096 - PAYLOAD_OFF)];
            let Ok(m) = librsproto::decode(req) else { return None };
            if m.op != librsproto::OP_INPUT_EVENTS {
                continue;
            }
            let n = m.body.len() / EVENT_LEN;
            let mut done = false;
            for i in 0..n {
                let Some(ev) = InputEvent::read(&m.body[i * EVENT_LEN..]) else { return None };
                if ev.kind == libkern::abi::EV_KEY && ev.code == DONE_CODE && ev.value == 1 {
                    done = true;
                }
                Line::new()
                    .s(b"input-testclient: ev")
                    .s(b" kind=")
                    .u(ev.kind as u64)
                    .s(b" code=")
                    .u(ev.code as u64)
                    .s(b" value=")
                    .i(ev.value as i64)
                    .end();
            }
            return Some(done);
        }
    }
}

/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments; `root_ns` is this
/// process's root namespace.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"input-testclient: up\n");

    let Some(mut stream) = Stream::open(root_ns) else {
        kprint(b"input-testclient: /dev/input/new FAILED\n");
        exit(1);
    };

    // The harness waits for this before injecting: an event delivered before the consumer
    // channel exists is one the server has nowhere to send.
    kprint(b"input-testclient: listening\n");

    // Pump until the button press arrives — the last injection. Message boundaries are
    // timing, not protocol: the server batches whatever is ready, so how the nine records
    // split across messages varies run to run and neither the client nor the harness should
    // depend on it.
    loop {
        match stream.pump() {
            Some(true) => break,
            Some(false) => {}
            None => {
                kprint(b"input-testclient: stream FAILED\n");
                exit(1);
            }
        }
    }

    kprint(b"input-testclient: PASSED\n");
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"input-testclient: panic\n");
    exit(2);
}
