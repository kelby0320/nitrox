//! `nxsh` — the Nitrox shell, host side.
//!
//! Milestone 3 Part A ships the *language* ([`nxsh`] the library: lexer, parser, AST) and
//! this binary is deliberately a stub around it. Part A's deliverable is that the design
//! doc's own examples parse, which is a host-test claim, not a boot-time one.
//!
//! What lands here later, in order: Part C wires pipelines — spawning a stage per
//! external command, `libstream::setup::pipe` between them, `PipelineStatus` collected
//! from the process handles the shell already holds. Part F adds the console line-reader.
//!
//! Until then this exists so the crate builds for `x86_64-unknown-nitrox` alongside the
//! host tests: a language that only compiles for the host would be a language that has
//! quietly stopped being part of the OS.

#![no_std]
#![no_main]

extern crate alloc;

use libkern::{exit, kprint};

/// `alloc` backing: the parser allocates.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Exit status for "this build cannot do anything yet", distinct from a real failure.
const EXIT_UNIMPLEMENTED: i64 = 70;

#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _ns: u64, _endpoint: u64, _arg0: u64) -> ! {
    // Proves the language links and runs in ring 3 — the parser is exercised properly by
    // the host suite, which is where a parser belongs.
    let parsed = nxsh::parse_script("let greeting = \"nxsh\"\n").is_ok();
    kprint(if parsed {
        b"nxsh: language links; the interpreter lands in Part C\n"
    } else {
        b"nxsh: parser failed on its own smoke input\n"
    });
    exit(if parsed { EXIT_UNIMPLEMENTED } else { 1 })
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"nxsh: panic\n");
    exit(1)
}
