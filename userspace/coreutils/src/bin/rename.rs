//! `rename` — re-point a name at the same file.
//!
//! One `fs::rename`, no fallback. Where [`move`](../move) will copy when it has to,
//! `rename` only ever re-points a directory entry — O(1), identity-preserving, and it either
//! works or says why.
//!
//! ## Deliberate behaviours
//!
//! - **No copy fallback, on purpose.** That is `move`'s job: a caller who needs the cheap,
//!   identity-preserving operation is told when it is impossible, rather than silently
//!   getting an expensive copy. A cross-mount `rename` is an error here.
//! - **An existing destination is an error** unless `--force`, matching `copy`.
//! - **It emits a table.** `Table<{from: String, to: String}>`, one row.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Port, Stage};
use coreutils::usage::RENAME;
use libfs::FileError;
use libkern::abi::IPC_PAYLOAD_SIZE;
use libos::boot::Start;
use libos::exit;
use libos::log;
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

    let args = match RENAME.parse(&stage.argv) {
        Ok(a) => a,
        Err(_) => stage.die(b"rename: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(RENAME.help().as_bytes());
    }
    if args.version() {
        stage.answer(RENAME.version().as_bytes());
    }
    if args.operands.len() != 2 {
        stage.die(b"rename: need exactly OLD and NEW (try --help)\n", EXIT_USAGE);
    }

    let from = stage.path(args.operands[0].as_bytes());
    // Both resolve against `PWD`: the kernel refuses a relative path, so a NEW taken as typed made
    // `rename ./a ./b` fail wherever it ran.
    let to = stage.path(args.operands[1].as_bytes());
    if from == to {
        stage.die(b"rename: source and destination are the same name\n", EXIT_USAGE);
    }

    match libfs::rename_path(&stage.namespace, &from, &to, args.has("force")) {
        Ok(()) => {}
        // Not a filesystem failure: this operation cannot express it, and `move` can.
        Err(FileError::CrossDevice) => {
            stage.die(b"rename: source and destination are on different mounts (use `move`)\n", EXIT_FAILURE)
        }
        Err(FileError::NotFound) => stage.die(b"rename: no such path\n", EXIT_FAILURE),
        Err(_) => stage.die(b"rename: cannot rename\n", EXIT_FAILURE),
    }

    match stage.stdout() {
        Some(port) => emit(&stage, port, &from, &to),
        None => {
            let mut line = String::from("renamed ");
            line.push_str(&from);
            line.push_str(" -> ");
            line.push_str(&to);
            line.push('\n');
            log::write(line.as_bytes());
        }
    }
    exit(EXIT_OK)
}

fn emit(stage: &Stage, stdout: Port<'_>, from: &str, to: &str) {
    let schema = Schema::new().field("from", TypeTag::String, TypeModifiers::NONE).field(
        "to",
        TypeTag::String,
        TypeModifiers::NONE,
    );

    let mut tw = TableWriter::new(ChannelSink::new(stdout, IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        tw.write_row(&[Value::Str(String::from(from)), Value::Str(String::from(to))])?;
        tw.finish_with_status(0)
    });
    let flushed = wrote.and_then(|()| tw.into_sink().finish());
    match flushed {
        Ok(()) => {}
        Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"rename: write failed\n", EXIT_FAILURE),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"rename: panic\n");
    exit(EXIT_FAILURE)
}
