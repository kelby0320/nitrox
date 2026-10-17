//! `sleep` — wait for a duration.
//!
//! The waiting is three syscalls; the *parsing* is where this can be wrong, so it lives in
//! [`coreutils::time::parse_duration`] and is tested on the host.
//!
//! ## Deliberate behaviours
//!
//! - **It blocks on a timer, it does not spin.** A one-shot timer armed at an absolute
//!   monotonic deadline, then `sys_wait`: nothing blocks *inside* a syscall.
//! - **The deadline is absolute, not a delay.** It is computed once from the monotonic
//!   clock, so time spent between the read and the arm does not extend the wait.
//! - **It emits nothing.** A stage that published a row would put a table into a
//!   pipeline that asked for a pause. The exit status is the whole result.
//! - **A malformed duration is a usage error, not a zero-length sleep.** Silently
//!   sleeping for no time would make `sleep --forever-typo` look like it worked.

#![no_std]
#![no_main]

extern crate alloc;

use coreutils::args::parse;
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use coreutils::time::parse_duration;
use libkern::abi::CLOCK_MONOTONIC;
use libkern::syscall::{
    SYS_CLOCK_READ, SYS_HANDLE_CLOSE, SYS_TIMER_CREATE, SYS_TIMER_SET, SYS_WAIT, syscall1, syscall2, syscall4,
};
use libkern::{exit, kprint};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

static mut CLOCK_BUF: u64 = 0;
/// `sys_wait` in/out buffers: the timer, and this process's notification channel, so a request
/// to stop is noticed while the sleep is in progress.
static mut WAIT_HANDLES: [u64; 2] = [0; 2];
static mut WAIT_RESULTS: [u8; 48] = [0; 48];

const HELP: &[u8] = b"usage: sleep DURATION\n\
    \n\
    Wait for DURATION, then exit. A bare number is seconds; the suffixes\n\
    ns, us, ms, s, m and h are accepted, with an optional fraction.\n\
    Emits nothing.\n\
    \n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"sleep (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[]) {
        Ok(a) => a,
        Err(_) => stage.die(b"sleep: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    if args.operands.len() != 1 {
        stage.die(b"sleep: need exactly one duration (try --help)\n", EXIT_USAGE);
    }

    let nanos = match parse_duration(&args.operands[0]) {
        Some(n) => n,
        None => stage.die(b"sleep: malformed duration (try --help)\n", EXIT_USAGE),
    };
    if nanos == 0 {
        exit(EXIT_OK); // a zero wait is a valid request, and needs no timer
    }

    // SAFETY: valid syscall; returns a handle (>= 0) or a negative KError.
    let th = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th < 0 {
        stage.die(b"sleep: cannot create a timer\n", EXIT_FAILURE);
    }
    let th = th as u64;

    // Read the monotonic clock and arm an **absolute** deadline, so whatever time passes
    // between the read and the arm is inside the wait rather than added to it.
    // SAFETY: `CLOCK_BUF` is a valid writable `u64` out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
    // leaves its last value.
    let now = unsafe { (&raw const CLOCK_BUF).read() };
    let fire_at = now.saturating_add(nanos);

    // SAFETY: arming this process's own timer handle (absolute monotonic, one-shot).
    if unsafe { syscall4(SYS_TIMER_SET, th, fire_at, 0, 0) } < 0 {
        stage.die(b"sleep: cannot arm the timer\n", EXIT_FAILURE);
    }

    // **Wait on the notification channel as well as the timer**, so a `Ctrl-C` during `sleep 30`
    // returns the prompt now rather than in thirty seconds. A stage that ignores this is not
    // broken, merely not interruptible.
    loop {
        // SAFETY: valid wait buffers; two handles.
        let waited = unsafe {
            WAIT_HANDLES[0] = th;
            WAIT_HANDLES[1] = stage.notif;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 2, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        if waited < 1 {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, th) };
            stage.die(b"sleep: the wait did not complete\n", EXIT_FAILURE);
        }
        if stage.terminate_requested() {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, th) };
            // Asked to stop, so stopping is success: the caller got what it wanted.
            exit(EXIT_OK)
        }
        // Woken by something else — check whether the timer is what fired by asking the
        // clock, since a notification wake says nothing about the deadline.
        // SAFETY: valid clock out-param.
        unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
        // SAFETY: `CLOCK_BUF` is a plain `u64` static only this thread touches; a failed clock read
        // leaves its last value.
        if unsafe { (&raw const CLOCK_BUF).read() } >= fire_at {
            break;
        }
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, th) };
    exit(EXIT_OK)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"sleep: panic\n");
    exit(EXIT_FAILURE)
}
