//! `terminal` — what is known of the terminal the stage was handed: its size, as its window last
//! said (`docs/spec/rsproto-tty-ops.md` § Size), null where it is not known, as on a serial line,
//! rather than an invented 80 by 24. The record grows a column per thing learned; a setting will
//! be an option, a request to the window, as a stage's own terminal is only its sibling.

#![no_std]
#![no_main]

extern crate alloc;

use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Port, Stage};
use coreutils::usage::TERMINAL;
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

#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);

    let args = match TERMINAL.parse(&stage.argv) {
        Ok(a) => a,
        Err(_) => stage.die(b"terminal: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(TERMINAL.help().as_bytes());
    }
    if args.version() {
        stage.answer(TERMINAL.version().as_bytes());
    }
    if !args.operands.is_empty() {
        stage.die(b"terminal: takes no operands (try --help)\n", EXIT_USAGE);
    }
    if stage.terminal.is_none() {
        stage.die(b"terminal: this stage was handed no terminal\n", EXIT_FAILURE);
    }
    let size = stage.terminal_size();

    match (stage.stdout(), size) {
        (Some(port), _) => emit(&stage, port, size),
        (None, Some(s)) => Line::new().u(u64::from(s.cols)).s(b"x").u(u64::from(s.rows)).end(),
        (None, None) => log::write(b"size unknown\n"),
    }
    exit(EXIT_OK)
}

fn emit(stage: &Stage, stdout: Port<'_>, size: Option<Size>) {
    let schema = Schema::new().field("columns", TypeTag::Int, TypeModifiers::NULLABLE).field(
        "rows",
        TypeTag::Int,
        TypeModifiers::NULLABLE,
    );
    let side = |n: Option<u16>| n.map_or(Value::Null, |n| Value::Int(i64::from(n)));
    let mut tw = TableWriter::new(ChannelSink::new(stdout, IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        tw.write_row(&[side(size.map(|s| s.cols)), side(size.map(|s| s.rows))])?;
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
