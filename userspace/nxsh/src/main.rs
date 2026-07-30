//! `nxsh` — the Nitrox shell, host side.
//!
//! Milestone 3 Parts A and B ship the *language* — lexer, parser, evaluator — and this
//! binary is deliberately thin around it. It runs one script through the whole pipeline
//! in ring 3, which is the part the host suite cannot claim: that the same code works on
//! the target, where the heap and hardware `f64` are real.
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
    // Part B's deliverable, run in ring 3: parse and evaluate a script, in process. The
    // language is exercised properly by the host suite — this is the proof that the same
    // code path works on the target, where `f64` formatting and the heap are real.
    // The expected rendering is checked *here*, so a wrong answer changes the exit
    // status. An exit code alone would pass on any result the evaluator happened to
    // produce, which would make the in-guest demo assert nothing about the language.
    const SCRIPT: &str = "let x = 2 + 3\nlet y = x * 1.5\ny";
    const EXPECT: &str = "7.5";
    match nxsh::Interp::eval_str(SCRIPT) {
        Ok(v) => {
            let got = v.render();
            let mut line = alloc::string::String::from("nxsh: evaluated -> ");
            line.push_str(&got);
            line.push('\n');
            kprint(line.as_bytes());
            if got != EXPECT {
                kprint(b"nxsh: WRONG - expected 7.5\n");
                exit(1)
            }
        }
        Err(e) => {
            let mut line = alloc::string::String::from("nxsh: ");
            line.push_str(&e.message);
            line.push('\n');
            kprint(line.as_bytes());
            exit(1)
        }
    }
    kprint(b"nxsh: pipelines and external commands arrive in Part C\n");
    exit(EXIT_UNIMPLEMENTED)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"nxsh: panic\n");
    exit(1)
}
