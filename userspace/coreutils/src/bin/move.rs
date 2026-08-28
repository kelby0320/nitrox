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
//!   the kernel's `CrossDevice` verdict — which `libfs` documents as *"a caller's
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
//! - **A cross-mount *directory* move copies the tree, then removes it.** Both halves are
//!   [`libfs::copy_tree`] / [`libfs::remove_tree`] — the same walks `copy` and `remove` use, not
//!   a third copy of the loop. This was refused until 2026-07-30, not because it was hard
//!   but because the image had no second writable mount to test it against, and writing it
//!   blind was worse than refusing it.
//! - **A partial copy is left where it fell.** Cleaning it up would mean deleting whatever
//!   of the destination already existed. The source is untouched, which is the property
//!   that matters, and the exit status says the move did not happen.
//! - **A namespace binding is refused as a source**, as in `remove`: a binding is a mount
//!   point, so recursing into one would copy *through* a mount and then delete another
//!   server's tree.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use libfs::FileError;
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::{exit, kprint};
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
    let dest_owned = stage.path(dest[0].as_bytes());
    let dest = dest_owned.as_slice();
    let dest_is_dir = libfs::is_dir(stage.namespace, dest);

    // As in `copy`: several sources only make sense into a directory, since with a file
    // destination they would each overwrite the last.
    if sources.len() > 1 && !dest_is_dir {
        stage.die(b"move: destination must be a directory when moving several sources\n", EXIT_USAGE);
    }

    let mut done: Vec<Moved> = Vec::new();
    for source in sources {
        let src_owned = stage.path(source.as_bytes());
        let src = src_owned.as_slice();
        let target = if dest_is_dir {
            libfs::join(dest, libfs::basename(src))
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
    match libfs::rename(stage.namespace, src, dst, force) {
        Ok(()) => return "rename",
        // The one error that is not a failure: the two paths are on different mounts, so
        // the cheap operation cannot express this and the expensive one must.
        Err(FileError::CrossDevice) => {}
        Err(FileError::NotFound) => stage.die(b"move: no such path\n", EXIT_FAILURE),
        Err(_) => stage.die(b"move: cannot move\n", EXIT_FAILURE),
    }

    // --- the fallback: copy, then remove the original ------------------------
    // A namespace binding is a mount point, not content. Recursing into one would copy
    // *through* a mount and then delete another server's tree, so it is refused at the
    // operand — the same rule `remove` states, and it has to be stated here too because a
    // tree walker cannot tell whether the path it was handed was a mistake.
    if is_binding(stage, src) {
        stage.die(b"move: source is a namespace binding, not a file\n", EXIT_FAILURE);
    }

    let is_dir = libfs::is_dir(stage.namespace, src);
    if libfs::copy_tree(stage.namespace, src, dst, force, &mut |_, _, _| {}).is_err() {
        // The copy failed partway. The source is untouched, which is the property that
        // matters; a partial destination is left where it fell rather than cleaned up,
        // because removing it would delete whatever of the destination already existed.
        stage.die(b"move: cross-mount copy failed\n", EXIT_FAILURE);
    }

    // The copy landed. From here a failure leaves *two* copies, which is not a move — so
    // it is reported as a failure and no row is emitted for it. Saying which state the
    // filesystem is in is the whole value of the message: "both now exist" tells an
    // operator that re-running is safe, and that nothing was lost.
    let removed = if is_dir {
        libfs::remove_tree(stage.namespace, src, &mut |_, _| {}).is_ok()
    } else {
        libfs::unlink_at(stage.namespace, src).is_ok()
    };
    if !removed {
        stage.die(
            b"move: copied, but could not remove the original (both now exist)\n",
            EXIT_FAILURE,
        );
    }
    "copy"
}

/// Is `path` a namespace binding rather than a filesystem entry? Asked of the parent's
/// bindings, which is where a mount point appears. Mirrors `remove`'s check, and for the
/// same reason: this program can now delete a tree.
fn is_binding(stage: &Stage, path: &[u8]) -> bool {
    let name = libfs::basename(path);
    libfs::ns_children(stage.namespace, libfs::parent(path))
        .iter()
        .any(|(n, _)| n.as_bytes() == name)
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
