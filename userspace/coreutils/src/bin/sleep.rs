//! `sleep` — wait for a duration.
//!
//! The waiting is a timer and a wait; the *parsing* is where this can be wrong, so it lives in
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
use libos::boot::Start;
use libos::{Deadline, Handle, Only, Timer, WaitSet, exit, log, time};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

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
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);

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

    let Ok(timer) = Handle::<Timer, Only>::create() else {
        stage.die(b"sleep: cannot create a timer\n", EXIT_FAILURE);
    };

    // Read the monotonic clock and arm an **absolute** deadline, so whatever time passes
    // between the read and the arm is inside the wait rather than added to it. A failed read
    // leaves the last value.
    let mut now = time::now().unwrap_or(0);
    let fire_at = now.saturating_add(nanos);

    if timer.set(fire_at).is_err() {
        stage.die(b"sleep: cannot arm the timer\n", EXIT_FAILURE);
    }

    // **Wait on the notification queue as well as the timer**, so a `Ctrl-C` during `sleep 30`
    // returns the prompt now rather than in thirty seconds. A stage that ignores this is not
    // broken, merely not interruptible.
    loop {
        let mut set = WaitSet::<2>::new();
        let added = set.add(&timer).and_then(|_| stage.notif.as_ref().map_or(Ok(0), |n| set.add(n)));
        if added.is_err() || !set.wait(Deadline::NEVER).is_ok_and(|signalled| !signalled.is_empty()) {
            stage.die(b"sleep: the wait did not complete\n", EXIT_FAILURE);
        }
        if stage.terminate_requested() {
            // Asked to stop, so stopping is success: the caller got what it wanted.
            exit(EXIT_OK)
        }
        // Woken by something else — check whether the timer is what fired by asking the
        // clock, since a notification wake says nothing about the deadline.
        if let Ok(read) = time::now() {
            now = read;
        }
        if now >= fire_at {
            break;
        }
    }
    drop(timer);
    exit(EXIT_OK)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"sleep: panic\n");
    exit(EXIT_FAILURE)
}
