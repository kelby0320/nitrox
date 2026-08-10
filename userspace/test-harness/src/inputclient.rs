//! `input-testclient` — proves the i8042 driver actually delivers events (plan M3 Part A).
//!
//! Bring-up is not the same as working. Before this, Part A could report
//! `ps2: keyboard mouse armed` on every boot with the ISR, the event ring and the parked
//! read having never run against a real keystroke — which is precisely the shape that let a
//! **one-way** Surface protocol ship in PR #174 behind three green CI jobs and 1213 passing
//! tests. A bound server that never answers, and an armed driver that never delivers, read
//! identically from the outside.
//!
//! So this reads `/dev/input/raw/0` and `/dev/input/raw/1` and prints what arrives. The host
//! side (`cargo xtask check-input`) injects the keystrokes and clicks over QMP and checks
//! the decoded events against what it sent.
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
use libkern::{
    IO_OPCODE_READ, IoOp, RIGHT_MAP_READ, RIGHT_MAP_WRITE, RIGHT_READ, SYS_IO_SUBMIT,
    SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_NS_LOOKUP, SYS_WAIT, exit, kprint, syscall2, syscall4,
};

/// One page for the read buffer.
const PAGE: u64 = 4096;
/// Bytes per record, from the shared ABI rather than a local literal — an earlier version
/// hardcoded `16` and read fields at literal offsets, so a kernel-side layout change would
/// have gone unnoticed until this gate happened to run (PR #178 review).
const EVENT_LEN: usize = INPUT_EVENT_LEN;
/// Events to request per read.
const READ_EVENTS: u64 = 16;

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

/// A line built in one buffer and emitted with a **single** `kprint`.
///
/// This is not tidiness. The console is shared and unsynchronised **between processes**: one
/// `kprint` is atomic (it takes the serial lock) but a sequence of them is not, so a line
/// assembled from six calls gets shredded by whatever else is booting. That is exactly how
/// the first version of this client failed — the events arrived, the client printed them,
/// and the harness saw `input-testclient: kbdtest-stage: setup message …` because another
/// process wrote between the tag and the value.
struct Line {
    buf: [u8; 96],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self { buf: [0; 96], len: 0 }
    }

    fn str(&mut self, s: &[u8]) -> &mut Self {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s[..n]);
        self.len += n;
        self
    }

    fn u64(&mut self, n: u64) -> &mut Self {
        if n == 0 {
            return self.str(b"0");
        }
        let mut d = [0u8; 20];
        let (mut i, mut v) = (0usize, n);
        while v > 0 {
            d[i] = b'0' + (v % 10) as u8;
            v /= 10;
            i += 1;
        }
        let mut out = [0u8; 20];
        for j in 0..i {
            out[j] = d[i - 1 - j];
        }
        self.str(&out[..i])
    }

    fn i32(&mut self, v: i32) -> &mut Self {
        if v < 0 {
            self.str(b"-").u64((-(v as i64)) as u64)
        } else {
            self.u64(v as u64)
        }
    }

    /// Emit the whole line in one syscall, newline included.
    fn emit(&mut self) {
        self.str(b"\n");
        kprint(&self.buf[..self.len]);
        self.len = 0;
    }
}

/// A read buffer plus the device it reads.
struct Reader {
    device: u64,
    buf_h: u64,
    buf_addr: u64,
}

impl Reader {
    fn open(root_ns: u64, path: &[u8]) -> Option<Self> {
        let device = lookup(root_ns, path, RIGHT_READ)?;
        // SAFETY: register-only syscall.
        let buf_h = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
        if buf_h < 0 {
            return None;
        }
        // SAFETY: a fresh MemoryObject handle with full MAP rights.
        let buf_addr = unsafe {
            syscall4(SYS_MEMORY_MAP, buf_h as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
        };
        if buf_addr < 0 {
            return None;
        }
        Some(Self { device, buf_h: buf_h as u64, buf_addr: buf_addr as u64 })
    }

    /// Submit one read and block for it. Returns the number of whole events delivered.
    fn read_events(&self) -> Option<usize> {
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: self.buf_h,
            buf_offset: 0,
            offset: 0,
            length: READ_EVENTS * EVENT_LEN as u64,
        };
        // SAFETY: a char DeviceNode with READ, and a valid `IoOp`.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.device, (&op as *const IoOp) as u64) };
        if po < 0 {
            return None;
        }
        let (status, n) = po_wait(po as u64);
        if status != 0 {
            return None;
        }
        Some(n as usize / EVENT_LEN)
    }

    /// Print event `i` from the buffer in the form the harness matches on.
    fn print_event(&self, tag: &[u8], i: usize) {
        let base = self.buf_addr + (i * EVENT_LEN) as u64;
        // SAFETY: `base + EVENT_LEN` is within the mapped page (`i < READ_EVENTS`), and the
        // driver writes whole records only.
        let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, EVENT_LEN) };
        let Some(ev) = InputEvent::read(bytes) else { return };
        let (kind, code, value) = (ev.kind, ev.code, ev.value);
        Line::new()
            .str(tag)
            .str(b" kind=")
            .u64(kind as u64)
            .str(b" code=")
            .u64(code as u64)
            .str(b" value=")
            .i32(value)
            .emit();
    }
}

/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments; `root_ns` is this
/// process's root namespace.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"input-testclient: up\n");

    let Some(kbd) = Reader::open(root_ns, b"/dev/input/raw/0") else {
        kprint(b"input-testclient: /dev/input/raw/0 FAILED\n");
        exit(1);
    };
    let Some(mouse) = Reader::open(root_ns, b"/dev/input/raw/1") else {
        kprint(b"input-testclient: /dev/input/raw/1 FAILED\n");
        exit(1);
    };

    // The harness waits for this before injecting: a keystroke delivered before the read is
    // parked would sit in the ring unread, and one delivered before the lookup would be
    // dropped entirely.
    kprint(b"input-testclient: listening\n");

    // Keyboard first. The harness sends one key down and up, so this expects two groups —
    // each an `EV_KEY` followed by its `EV_SYN`.
    for _ in 0..2 {
        let Some(n) = kbd.read_events() else {
            kprint(b"input-testclient: keyboard read FAILED\n");
            exit(1);
        };
        for i in 0..n {
            kbd.print_event(b"input-testclient: kbd", i);
        }
    }
    kprint(b"input-testclient: keyboard ok\n");

    // Then the mouse: a motion and a button, which arrive as their own `SYN` groups.
    for _ in 0..2 {
        let Some(n) = mouse.read_events() else {
            kprint(b"input-testclient: mouse read FAILED\n");
            exit(1);
        };
        for i in 0..n {
            mouse.print_event(b"input-testclient: mouse", i);
        }
    }
    kprint(b"input-testclient: mouse ok\n");

    kprint(b"input-testclient: PASSED\n");
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"input-testclient: panic\n");
    exit(2);
}
