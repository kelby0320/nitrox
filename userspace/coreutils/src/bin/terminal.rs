//! `terminal` — the size of the terminal the stage was handed, as its window last said
//! (`docs/spec/rsproto-tty-ops.md` § Size). **A serial line cannot say**, so there it fails rather
//! than invent 80 by 24.

#![no_std]
#![no_main]

extern crate alloc;

use coreutils::args::parse;
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Port, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libos::boot::Start;
use libos::exit;
use libos::log::{self, Line};
use librsproto::tty::Size;
use libstream::channel::ChannelSink;
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const HELP: &[u8] = b"usage: terminal\n\
    \n\
    Report the size of the terminal this stage was handed, as its window last said.\n\
    Fails on a terminal that cannot say, as a serial line cannot, and with none.\n\
    Emits Table<{columns: Int, rows: Int}> on stdout.\n\
    \n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"terminal (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);

    let args = match parse(&stage.argv, &[]) {
        Ok(a) => a,
        Err(_) => stage.die(b"terminal: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    if !args.operands.is_empty() {
        stage.die(b"terminal: takes no operands (try --help)\n", EXIT_USAGE);
    }
    if stage.terminal.is_none() {
        stage.die(b"terminal: this stage was handed no terminal\n", EXIT_FAILURE);
    }
    let Some(size) = stage.terminal_size() else {
        stage.die(b"terminal: its size is not known (a serial line cannot say)\n", EXIT_FAILURE);
    };

    match stage.stdout() {
        Some(port) => emit(&stage, port, size),
        None => Line::new().u(u64::from(size.cols)).s(b"x").u(u64::from(size.rows)).end(),
    }
    exit(EXIT_OK)
}

fn emit(stage: &Stage, stdout: Port<'_>, size: Size) {
    let schema = Schema::new().field("columns", TypeTag::Int, TypeModifiers::NONE).field(
        "rows",
        TypeTag::Int,
        TypeModifiers::NONE,
    );
    let mut tw = TableWriter::new(ChannelSink::new(stdout, IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        tw.write_row(&[Value::Int(i64::from(size.cols)), Value::Int(i64::from(size.rows))])?;
        tw.finish_with_status(0)
    });
    match wrote.and_then(|()| tw.into_sink().finish()) {
        Ok(()) | Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"terminal: write failed\n", EXIT_FAILURE),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"terminal: panic\n");
    exit(EXIT_FAILURE)
}
