//! `page` — its input a page at a time on the terminal, as the shell would show it, where it is
//! shown there (`page(1)`); anywhere else, and when one page holds it, the input passes on unchanged.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use coreutils::pager::{self, Shown};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use coreutils::usage::PAGE;
use libkern::abi::IPC_PAYLOAD_SIZE;
use libos::boot::Start;
use libos::exit;
use libos::log;
use libstream::ByteSink;
use libstream::channel::{ChannelReceiver, ChannelSink};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The most input read, the shell's own capture cap.
const MAX_INPUT: usize = 1 << 20;

#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);

    let args = match PAGE.parse(&stage.argv) {
        Ok(a) => a,
        Err(_) => stage.die(b"page: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(PAGE.help().as_bytes());
    }
    if args.version() {
        stage.answer(PAGE.version().as_bytes());
    }
    if !args.operands.is_empty() {
        stage.die(b"page: takes no operands; it pages its input (try --help)\n", EXIT_USAGE);
    }
    let Some(stdin) = stage.stdin() else { exit(EXIT_OK) };
    let input = match ChannelReceiver::new(stdin).receive(MAX_INPUT) {
        Ok(b) => b,
        Err(libstream::wire::WireError::TooLarge) => stage.die(b"page: the input is over a megabyte\n", EXIT_FAILURE),
        Err(_) => stage.die(b"page: could not read the input\n", EXIT_FAILURE),
    };
    if stage.streams.shown
        && let Ok(table) = libstream::Table::decode(&input)
    {
        let (lines, head): (alloc::vec::Vec<String>, usize) = pager::table_lines(&table);
        if pager::show(&stage, &lines, head) == Shown::Paged {
            exit(EXIT_OK);
        }
    }
    let Some(stdout) = stage.stdout() else { exit(EXIT_OK) };
    let mut sink = ChannelSink::new(stdout, IPC_PAYLOAD_SIZE);
    match sink.put(&input).and_then(|()| sink.finish()) {
        Ok(()) | Err(libstream::wire::WireError::PeerClosed) => exit(EXIT_OK),
        Err(_) => stage.die(b"page: write failed\n", EXIT_FAILURE),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"page: panic\n");
    exit(EXIT_FAILURE)
}
