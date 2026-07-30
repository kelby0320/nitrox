//! `move` — relocate files, re-pointing the name where possible and copying where not.
//!
//! Milestone 2 Part B. `move` is the first coreutil that *composes* two existing
//! operations rather than wrapping one, and the first whose correctness lives on a
//! **failure path**: the interesting behaviour only happens once the cheap operation has
//! been refused.
//!
//! ```text
//! move SOURCE DEST          # file → file
//! move SOURCE... DIR        # several sources into an existing directory
//! move --force SOURCE DEST  # replace an existing destination
//! ```
//!
//! ## Deliberate behaviours
//!
//! - **Rename first, copy only if forced to.** A rename is O(1) and preserves the file's
//!   identity; a copy is neither. So `move` asks for the rename and falls back only on
//!   the kernel's `CrossDevice` verdict — which `coreutils::fs` documents as *"a caller's
//!   cue to fall back to `copy_file` + unlink rather than an error to report"*.
//! - **It reports which method it used.** `Table<{from, to, method: String}>` with
//!   `method` one of `rename` or `copy`. This is not decoration: the two differ in cost
//!   and in whether the file keeps its identity, so "did this actually copy?" is a
//!   question a pipeline can reasonably ask — and it is the only way to *prove* the
//!   same-mount path is not silently copying.
//! - **A failed fallback leaves both copies and says so.** If the copy succeeds but
//!   removing the source does not, the result is a duplicate, not a move. That is
//!   reported as a failure rather than papered over: the exit status is non-zero and the
//!   row is not emitted, so nothing downstream is told the move happened.
//! - **A cross-mount *directory* move is refused.** The fallback copies a regular file;
//!   recursive cross-mount relocation is a larger operation and there is currently no
//!   second writable mount to exercise it against (see the Milestone 2 notes). Refusing
//!   is honest; silently copying half a tree is not.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::fs::{self, FileError};
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

const FORCE: Flag = Flag::new("force", 'f', "replace an existing destination");

const HELP: &[u8] = b"usage: move [--force] SOURCE... DEST\n\
    \n\
    Relocate files. Re-points the name when source and destination share a\n\
    mount; copies and removes the original when they do not.\n\
    Emits Table<{from: String, to: String, method: String}> on stdout,\n\
    where method is \"rename\" or \"copy\".\n\
    \n\
      -f, --force   replace an existing destination\n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"move (nitrox coreutils) 0.1.0\n";

/// One completed relocation, as reported on stdout.
struct Moved {
    from: String,
    to: String,
    method: &'static str,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[FORCE]) {
        Ok(a) => a,
        Err(_) => stage.die(b"move: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }
    if args.operands.len() < 2 {
        stage.die(b"move: need a source and a destination (try --help)\n", EXIT_USAGE);
    }

    let force = args.has("force");
    let (sources, dest) = args.operands.split_at(args.operands.len() - 1);
    let dest = dest[0].as_bytes();
    let dest_is_dir = fs::is_dir(stage.namespace, dest);

    // As in `copy`: several sources only make sense into a directory, since with a file
    // destination they would each overwrite the last.
    if sources.len() > 1 && !dest_is_dir {
        stage.die(b"move: destination must be a directory when moving several sources\n", EXIT_USAGE);
    }

    let mut done: Vec<Moved> = Vec::new();
    for source in sources {
        let src = source.as_bytes();
        let target = if dest_is_dir {
            fs::join(dest, fs::basename(src))
        } else {
            String::from_utf8_lossy(dest).into_owned()
        };
        if target.as_bytes() == src {
            stage.die(b"move: source and destination are the same file\n", EXIT_USAGE);
        }
        let method = move_one(&stage, src, target.as_bytes(), force);
        done.push(Moved {
            from: String::from_utf8_lossy(src).into_owned(),
            to: target,
            method,
        });
    }

    match stage.streams.stdout {
        Some(h) => emit(&stage, h, &done),
        None => {
            for m in &done {
                report_text(m);
            }
        }
    }
    exit(EXIT_OK)
}

/// Relocate `src` to `dst`, returning the method used. Diverges on failure.
fn move_one(stage: &Stage, src: &[u8], dst: &[u8], force: bool) -> &'static str {
    match fs::rename(stage.namespace, src, dst, force) {
        Ok(()) => return "rename",
        // The one error that is not a failure: the two paths are on different mounts, so
        // the cheap operation cannot express this and the expensive one must.
        Err(FileError::CrossDevice) => {}
        Err(FileError::NotFound) => stage.die(b"move: no such path\n", EXIT_FAILURE),
        Err(_) => stage.die(b"move: cannot move\n", EXIT_FAILURE),
    }

    // --- the fallback: copy, then remove the original ------------------------
    if fs::is_dir(stage.namespace, src) {
        // `TODO(cross-mount-move)`: the fallback copies a regular file. A recursive
        // cross-mount relocation is a larger operation, and there is currently no second
        // writable mount to exercise it against — so it is refused rather than written
        // blind. See `docs/rationale/deferred-decisions.md`.
        stage.die(
            b"move: cross-mount directory move is not supported (copy then remove)\n",
            EXIT_FAILURE,
        );
    }
    if fs::copy_file(stage.namespace, src, dst, force).is_err() {
        stage.die(b"move: cross-mount copy failed\n", EXIT_FAILURE);
    }
    // The copy landed. From here a failure leaves *two* copies, which is not a move — so
    // it is reported as a failure and no row is emitted for it.
    let mut buf = [0u8; 4096];
    let mut dir = match Dir::open(stage.namespace, fs::parent(src), &mut buf) {
        Ok(d) => d,
        Err(_) => stage.die(b"move: copied, but cannot open the source directory to remove it\n", EXIT_FAILURE),
    };
    let r = dir.unlink(fs::basename(src));
    dir.close();
    if r.is_err() {
        stage.die(b"move: copied, but could not remove the original (both now exist)\n", EXIT_FAILURE);
    }
    "copy"
}

/// Write the rows as a TSM1 table on the `stdout` stream.
fn emit(stage: &Stage, stdout: u64, done: &[Moved]) {
    let schema = Schema::new()
        .field("from", TypeTag::String, TypeModifiers::NONE)
        .field("to", TypeTag::String, TypeModifiers::NONE)
        .field("method", TypeTag::String, TypeModifiers::NONE);

    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for m in done {
            tw.write_row(&[
                Value::Str(m.from.clone()),
                Value::Str(m.to.clone()),
                Value::Str(String::from(m.method)),
            ])?;
        }
        tw.finish_with_status(0)
    });
    let flushed = wrote.and_then(|()| tw.into_sink().finish());
    match flushed {
        Ok(()) => {}
        Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"move: write failed\n", EXIT_FAILURE),
    }
}

/// The Tier-0 path: no `stdout`, so report in plain text. See
/// `TODO(tier0-output-sink)` — the kernel log is scaffolding, not the destination.
fn report_text(m: &Moved) {
    let mut line = String::from("moved (");
    line.push_str(m.method);
    line.push_str(") ");
    line.push_str(&m.from);
    line.push_str(" -> ");
    line.push_str(&m.to);
    line.push('\n');
    kprint(line.as_bytes());
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"move: panic\n");
    exit(EXIT_FAILURE)
}
