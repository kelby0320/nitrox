//! `touch` — create a file if it is absent, and stamp its modification time.
//!
//! Milestone 2 Part C. Two behaviours in one verb, and they are not variations of each
//! other: creating a file is a directory mutation, while stamping one is an inode
//! mutation on a file that already exists.
//!
//! ```text
//! touch PATH...              # create if absent, otherwise stamp mtime
//! touch --no-create PATH...  # stamp only; a missing PATH is skipped, not created
//! ```
//!
//! ## Deliberate behaviours
//!
//! - **The time comes from the filesystem, never from here.** There is no `--date`, and
//!   the wire carries no timestamp: one a caller could choose would be forgeable
//!   metadata, so the server reads its own clock. That is why this utility cannot
//!   express "set mtime to X" and does not pretend to.
//! - **`--no-create` skips rather than fails.** The flag says "I only want to stamp what
//!   is there"; erroring on absence would make it useless for its one job, which is
//!   touching a set of paths without conjuring the missing ones.
//! - **It emits a table.** `Table<{path: String, created: Bool}>` — the same shape as
//!   `mkdir`, because the same question ("did this exist already?") is the interesting
//!   one, and a pipeline can answer it without a second `list`.
//!
//! ## Why the session op had to be added first
//!
//! `File::Touch` already existed, but only on the **kernel↔server control channel**: it
//! is how the kernel reports a Model A write that the fs-server cannot otherwise observe
//! (Slice C4). It is path-addressed, fire-and-forget, and has no client behind it, so a
//! client session that sent it got `Unsupported`. Part C therefore began by giving it a
//! session-scoped form — name-addressed inside an open directory, returning a status,
//! exactly like `mkdir`/`unlink`/`rmdir` — rather than working around its absence.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use libfs;
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::{exit, kprint};
use librsproto::session::Dir;
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: path building and the TSM1 encoder both allocate.
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

/// One path this run touched, and whether it had to create it.
struct Touched {
    path: String,
    created: bool,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[NO_CREATE]) {
        Ok(a) => a,
        Err(_) => stage.die(b"touch: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }
    if args.operands.is_empty() {
        stage.die(b"touch: need at least one path (try --help)\n", EXIT_USAGE);
    }

    let no_create = args.has("no-create");
    let mut done: Vec<Touched> = Vec::new();

    for operand in &args.operands {
        let resolved = stage.path(operand.as_bytes());
        let path = resolved.as_slice();
        if path.is_empty() {
            stage.die(b"touch: empty path\n", EXIT_USAGE);
        }
        if libfs::is_dir(stage.namespace, path) {
            stage.die(b"touch: path is a directory\n", EXIT_USAGE);
        }
        let exists = libfs::file_size(stage.namespace, path).is_some();

        if !exists {
            if no_create {
                continue; // the flag's whole purpose: skip, do not fail
            }
            if libfs::create_file(stage.namespace, path).is_err() {
                stage.die(b"touch: cannot create file\n", EXIT_FAILURE);
            }
            // A just-created inode is stamped by the server as it creates it, so there is
            // nothing further to do — and stamping again would be a second round trip for
            // a timestamp that is already now.
            done.push(Touched { path: as_string(path), created: true });
            continue;
        }

        stamp(&stage, path);
        done.push(Touched { path: as_string(path), created: false });
    }

    match stage.streams.stdout {
        Some(h) => emit(&stage, h, &done),
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
fn stamp(stage: &Stage, path: &[u8]) {
    let mut buf = [0u8; 4096];
    let mut dir = match Dir::open(stage.namespace, libfs::parent(path), &mut buf) {
        Ok(d) => d,
        Err(_) => stage.die(b"touch: cannot open the parent directory\n", EXIT_FAILURE),
    };
    let r = dir.touch(libfs::basename(path));
    dir.close();
    if r.is_err() {
        stage.die(b"touch: cannot stamp the modification time\n", EXIT_FAILURE);
    }
}

fn as_string(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// Write the rows as a TSM1 table on the `stdout` stream.
fn emit(stage: &Stage, stdout: u64, done: &[Touched]) {
    let schema = Schema::new()
        .field("path", TypeTag::String, TypeModifiers::NONE)
        .field("created", TypeTag::Bool, TypeModifiers::NONE);

    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
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
    kprint(line.as_bytes());
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"touch: panic\n");
    exit(EXIT_FAILURE)
}
