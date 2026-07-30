//! `rename` — re-point a name at the same file.
//!
//! Milestone 2 Part B, and the thin half of it: one `sys_file_rename`, no fallback. Where
//! [`move`](../move) will copy when it has to, `rename` only ever re-points a directory
//! entry — so it is O(1), it preserves the file's identity, and it either works or says
//! why.
//!
//! ```text
//! rename OLD NEW            # within a directory, or across directories
//! rename --force OLD NEW    # replace an existing NEW
//! ```
//!
//! ## Deliberate behaviours
//!
//! - **No copy fallback, on purpose.** That is `move`'s job. Keeping the two apart means
//!   a caller who needs the cheap, identity-preserving operation can *ask for it* and be
//!   told when it is impossible, rather than silently getting an expensive copy. A
//!   cross-mount `rename` is an error here.
//! - **An existing destination is an error** unless `--force`, matching `copy` (§1).
//! - **It emits a table.** `Table<{from: String, to: String}>` — one row, but a row, so a
//!   pipeline sees a stream rather than silence.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use coreutils::args::{Flag, parse};
use coreutils::fs::{self, FileError};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::{exit, kprint};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: the TSM1 encoder allocates.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const FORCE: Flag = Flag::new("force", 'f', "replace an existing destination");

const HELP: &[u8] = b"usage: rename [--force] OLD NEW\n\
    \n\
    Re-point a name at the same file. Never copies: a rename across mounts\n\
    is an error (use `move`, which falls back to copy).\n\
    Emits Table<{from: String, to: String}> on stdout.\n\
    \n\
      -f, --force   replace an existing NEW\n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"rename (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[FORCE]) {
        Ok(a) => a,
        Err(_) => stage.die(b"rename: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }
    if args.operands.len() != 2 {
        stage.die(b"rename: need exactly OLD and NEW (try --help)\n", EXIT_USAGE);
    }

    let from = args.operands[0].as_bytes();
    let to = args.operands[1].as_bytes();
    if from == to {
        stage.die(b"rename: source and destination are the same name\n", EXIT_USAGE);
    }

    match fs::rename(stage.namespace, from, to, args.has("force")) {
        Ok(()) => {}
        // The one error worth naming specifically: it is not a failure of the filesystem
        // but a statement that this operation cannot express what was asked, and the
        // caller has a different tool for it.
        Err(FileError::CrossDevice) => stage.die(
            b"rename: source and destination are on different mounts (use `move`)\n",
            EXIT_FAILURE,
        ),
        Err(FileError::NotFound) => stage.die(b"rename: no such path\n", EXIT_FAILURE),
        Err(_) => stage.die(b"rename: cannot rename\n", EXIT_FAILURE),
    }

    let from_s = String::from_utf8_lossy(from).into_owned();
    let to_s = String::from_utf8_lossy(to).into_owned();
    match stage.streams.stdout {
        Some(h) => emit(&stage, h, &from_s, &to_s),
        None => {
            let mut line = String::from("renamed ");
            line.push_str(&from_s);
            line.push_str(" -> ");
            line.push_str(&to_s);
            line.push('\n');
            kprint(line.as_bytes());
        }
    }
    exit(EXIT_OK)
}

/// Write the single row as a TSM1 table on the `stdout` stream.
fn emit(stage: &Stage, stdout: u64, from: &str, to: &str) {
    let schema = Schema::new()
        .field("from", TypeTag::String, TypeModifiers::NONE)
        .field("to", TypeTag::String, TypeModifiers::NONE);

    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
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
    kprint(b"rename: panic\n");
    exit(EXIT_FAILURE)
}
