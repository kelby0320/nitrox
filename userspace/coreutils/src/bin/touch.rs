//! `touch` — create a file if it is absent, and stamp its modification time.
//!
//! Two behaviours in one verb: creating a file is a directory mutation, while stamping one is an
//! inode mutation on a file that already exists.
//!
//! ## Deliberate behaviours
//!
//! - **The time comes from the filesystem, never from here.** There is no `--date`, and
//!   the wire carries no timestamp: one a caller could choose would be forgeable metadata,
//!   so the server reads its own clock.
//! - **`--no-create` skips rather than fails**: erroring on absence would make it useless for
//!   touching a set of paths without conjuring the missing ones.
//! - **It emits a table.** `Table<{path: String, created: Bool}>` — the same shape as `mkdir`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Port, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libos::boot::Start;
use libos::log;
use libos::{Msg, exit};
use librsproto::session::Dir;
use libstream::channel::ChannelSink;
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const NO_CREATE: Flag = Flag::new("no-create", 'c', "stamp only; do not create a missing path");

const HELP: &[u8] = b"usage: touch [--no-create] PATH...\n\
    \n\
    Create each PATH if absent, and stamp its modification time.\n\
    The time is the filesystem's own clock; it cannot be supplied.\n\
    Emits Table<{path: String, created: Bool}> on stdout.\n\
    \n\
      -c, --no-create stamp only; skip a PATH that does not exist\n\
          --help      show this help and exit\n\
          --version   show version information and exit\n";

const VERSION: &[u8] = b"touch (nitrox coreutils) 0.1.0\n";

struct Touched {
    path: String,
    created: bool,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);

    let args = match parse(&stage.argv, &[NO_CREATE]) {
        Ok(a) => a,
        Err(_) => stage.die(b"touch: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    if args.operands.is_empty() {
        stage.die(b"touch: need at least one path (try --help)\n", EXIT_USAGE);
    }

    let no_create = args.has("no-create");
    let mut done: Vec<Touched> = Vec::new();

    for operand in &args.operands {
        let resolved = stage.path(operand.as_bytes());
        let path = resolved.as_str();
        if path.is_empty() {
            stage.die(b"touch: empty path\n", EXIT_USAGE);
        }
        if libfs::is_directory(&stage.namespace, path) {
            stage.die(b"touch: path is a directory\n", EXIT_USAGE);
        }
        let exists = libfs::file_len(&stage.namespace, path).is_some();

        if !exists {
            if no_create {
                continue; // the flag's whole purpose: skip, do not fail
            }
            if libfs::ensure_file(&stage.namespace, path).is_err() {
                stage.die(b"touch: cannot create file\n", EXIT_FAILURE);
            }
            // A just-created inode is stamped by the server as it creates it.
            done.push(Touched { path: String::from(path), created: true });
            continue;
        }

        stamp(&stage, path);
        done.push(Touched { path: String::from(path), created: false });
    }

    match stage.stdout() {
        Some(port) => emit(&stage, port, &done),
        None => {
            for t in &done {
                report_text(t);
            }
        }
    }
    exit(EXIT_OK)
}

/// Stamp `path`'s modification time via a session on its parent directory. Diverges on
/// failure.
fn stamp(stage: &Stage, path: &str) {
    let mut msg = Msg::new();
    let parent = core::str::from_utf8(libfs::parent(path.as_bytes())).unwrap_or(path);
    let mut dir = match Dir::resolve(&stage.namespace, parent, &mut msg) {
        Ok(d) => d,
        Err(_) => stage.die(b"touch: cannot open the parent directory\n", EXIT_FAILURE),
    };
    let r = dir.touch(libfs::basename(path.as_bytes()));
    dir.close();
    if r.is_err() {
        stage.die(b"touch: cannot stamp the modification time\n", EXIT_FAILURE);
    }
}

fn emit(stage: &Stage, stdout: Port<'_>, done: &[Touched]) {
    let schema = Schema::new().field("path", TypeTag::String, TypeModifiers::NONE).field(
        "created",
        TypeTag::Bool,
        TypeModifiers::NONE,
    );

    let mut tw = TableWriter::new(ChannelSink::new(stdout, IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for t in done {
            tw.write_row(&[Value::Str(t.path.clone()), Value::Bool(t.created)])?;
        }
        tw.finish_with_status(0)
    });
    let flushed = wrote.and_then(|()| tw.into_sink().finish());
    match flushed {
        Ok(()) => {}
        Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"touch: write failed\n", EXIT_FAILURE),
    }
}

/// The Tier-0 path: no `stdout`, so report in plain text. See
/// `TODO(tier0-output-sink)` — the kernel log is scaffolding, not the destination.
fn report_text(t: &Touched) {
    let mut line = String::from(if t.created { "created " } else { "touched " });
    line.push_str(&t.path);
    line.push('\n');
    log::write(line.as_bytes());
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"touch: panic\n");
    exit(EXIT_FAILURE)
}
