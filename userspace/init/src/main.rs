//! `init` — PID 1, bootstrapping form (Phase 2 slice 4).
//!
//! Part 3 (this) is the bare-target skeleton: receive the initial handle set from
//! the kernel, stand up the static-arena allocator, prove `alloc` works, and exit
//! cleanly. Later parts add `init.toml` parsing (Part 4) and the reaping loop +
//! becoming the real PID 1 (Part 5). Per `userspace/init/CLAUDE.md`, init uses
//! `libkern` + `alloc` only and never `panic!`s in normal operation.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::asm;
use init::heap::BumpAlloc;
use libkern::{exit, kprint, kprint_u64};

#[global_allocator]
static ALLOC: BumpAlloc = BumpAlloc;

/// Bootstrap registers (`kernel/src/syscall/table.rs`): `rdi` = notification
/// channel, `rsi` = root namespace (LOOKUP-only), `rdx` = the first installed
/// handle (`0` when spawned with none), `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, handle0: u64, _arg0: u64) -> ! {
    kprint(b"init: up (bootstrapping)\n");

    // Report the initial handle set received from the kernel.
    let count = (notif != 0) as u64 + (root_ns != 0) as u64 + (handle0 != 0) as u64;
    kprint(b"init: received ");
    kprint_u64(count);
    kprint(b" handles (notif=");
    kprint_u64(notif);
    kprint(b", ns=");
    kprint_u64(root_ns);
    kprint(b")\n");

    // Prove the static-arena allocator works (Vec growth exercises alloc +
    // realloc + memcpy): build a small Vec and reduce it.
    let mut squares: Vec<u32> = Vec::new();
    for i in 0..8u32 {
        squares.push(i * i);
    }
    let sum: u32 = squares.iter().sum();
    kprint(b"init: alloc ok (sum of squares 0..8 = ");
    kprint_u64(sum as u64);
    kprint(b")\n");

    kprint(b"init: exiting\n");
    exit(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // init must not panic in normal operation (`userspace/init/CLAUDE.md`); this
    // is the last-ditch handler. Report and spin (no eshell handoff yet — Part 5+).
    kprint(b"init: PANIC\n");
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
