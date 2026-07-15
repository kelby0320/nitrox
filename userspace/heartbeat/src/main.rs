//! `heartbeat` — a trivial supervised service, the demo subject for `service-mgr`
//! slice A.
//!
//! It exists to *be supervised*: it announces itself, emits a bounded number of
//! "beats", then exits — so `service-mgr` can observe the exit and (once the restart
//! path lands) restart it per policy. A real long-running daemon would beat on a
//! timer forever; this bounded version gives the supervision/restart demo a
//! deterministic, observable lifecycle in a boot run.
//!
//! `#![no_std]` + `#![no_main]`, **no `alloc`**, `libkern` only — the init family's
//! rules (it is a leaf service, not a runtime consumer).

#![no_std]
#![no_main]

use libkern::*;

/// How many beats before exiting. Bounded so a boot run terminates the process
/// deterministically (the supervision/restart demo watches for the exit).
const BEATS: u32 = 3;

/// Emit `msg` to the serial console via the debug kprint syscall.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`; the slice is valid.
    unsafe {
        syscall4(
            SYS_DEBUG_KPRINT,
            msg.as_ptr() as u64,
            msg.len() as u64,
            0,
            0,
        );
    }
}

/// Print a small unsigned decimal (single digit is all we need for `BEATS`).
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

/// Bootstrap registers (see init): `rdi` = notification channel, `rsi` = namespace.
/// heartbeat needs neither — it only writes to the serial log — so all are ignored.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, _handle0: u64, _arg0: u64) -> ! {
    kprint(b"heartbeat: up\n");
    let mut beat = 1u32;
    while beat <= BEATS {
        kprint(b"heartbeat: beat ");
        kprint_u32(beat);
        kprint(b"/");
        kprint_u32(BEATS);
        kprint(b"\n");
        beat += 1;
    }
    kprint(b"heartbeat: done, exiting 0\n");
    // SAFETY: SYS_PROCESS_EXIT terminates this process with the given code; it does
    // not return.
    unsafe { syscall1(SYS_PROCESS_EXIT, 0) };
    // Unreachable — process_exit diverges.
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"heartbeat: PANIC\n");
    // SAFETY: terminate with a non-zero code; does not return.
    unsafe { syscall1(SYS_PROCESS_EXIT, 1) };
    loop {
        core::hint::spin_loop();
    }
}
