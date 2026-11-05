//! `move` — relocate files, re-pointing the name where possible and copying where not.
//!
//! `move` composes two operations, and its correctness lives on a **failure path**: the
//! interesting behaviour only happens once the cheap operation has been refused.
//!
//! ## Deliberate behaviours
//!
//! - **Rename first, copy only if forced to.** A rename is O(1) and preserves the file's
//!   identity; `move` falls back only on the kernel's `CrossDevice` verdict, which `libfs`
//!   documents as the cue to fall back to `libfs::copy` + unlink rather than an error to report.
//! - **It reports which method it used.** `Table<{from, to, method: String}>` with `method`
//!   one of `rename` or `copy`: the two differ in cost and in whether the file keeps its
//!   identity, and a gate uses it to prove the same-mount path is not silently copying.
//! - **A failed fallback leaves both copies and says so.** If the copy succeeds but removing
//!   the source does not, the result is a duplicate, not a move: the exit status is non-zero
//!   and the row is not emitted.
//! - **A cross-mount *directory* move copies the tree, then removes it**, with the walks
//!   [`libfs::copy_recursive`] and [`libfs::remove_recursive`] that `copy` and `remove` use.
//! - **A partial copy is left where it fell**: cleaning it up would delete whatever of the
//!   destination already existed. The source is untouched, and the exit status says the move
//!   did not happen.
//! - **A namespace binding is refused as a source**, as in `remove`: recursing into a mount
//!   point would copy *through* it and then delete another server's tree.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Port, Stage};
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

struct Moved {
    from: String,
    to: String,
    method: &'static str,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);

    let args = match parse(&stage.argv, &[FORCE]) {
        Ok(a) => a,
        Err(_) => stage.die(b"move: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    if args.operands.len() < 2 {
        stage.die(b"move: need a source and a destination (try --help)\n", EXIT_USAGE);
    }

    let force = args.has("force");
    let (sources, dest) = args.operands.split_at(args.operands.len() - 1);
    let dest = stage.path(dest[0].as_bytes());
    let dest_is_dir = libfs::is_directory(&stage.namespace, &dest);

    // As in `copy`: with a file destination, several sources would each overwrite the last.
    if sources.len() > 1 && !dest_is_dir {
        stage.die(b"move: destination must be a directory when moving several sources\n", EXIT_USAGE);
    }

    let mut done: Vec<Moved> = Vec::new();
    for source in sources {
        let src = stage.path(source.as_bytes());
        let target =
            if dest_is_dir { libfs::join(dest.as_bytes(), libfs::basename(src.as_bytes())) } else { dest.clone() };
        if target == src {
            stage.die(b"move: source and destination are the same file\n", EXIT_USAGE);
        }
        let method = move_one(&stage, &src, &target, force);
        done.push(Moved { from: src, to: target, method });
    }

    match stage.stdout() {
        Some(port) => emit(&stage, port, &done),
        None => {
            for m in &done {
                report_text(m);
            }
        }
    }
    exit(EXIT_OK)
}

fn move_one(stage: &Stage, src: &str, dst: &str, force: bool) -> &'static str {
    match libfs::rename_path(&stage.namespace, src, dst, force) {
        Ok(()) => return "rename",
        // Not a failure: the paths are on different mounts, so only a copy can express this.
        Err(FileError::CrossDevice) => {}
        Err(FileError::NotFound) => stage.die(b"move: no such path\n", EXIT_FAILURE),
        Err(_) => stage.die(b"move: cannot move\n", EXIT_FAILURE),
    }

    // The fallback: copy, then remove the original. A binding is refused at the operand, as in
    // `remove`, since a tree walker cannot tell whether the path it was handed was a mistake.
    if is_binding(stage, src) {
        stage.die(b"move: source is a namespace binding, not a file\n", EXIT_FAILURE);
    }

    let is_dir = libfs::is_directory(&stage.namespace, src);
    if libfs::copy_recursive(&stage.namespace, src, dst, force, &mut |_, _, _| {}).is_err() {
        // The source is untouched; a partial destination is left where it fell.
        stage.die(b"move: cross-mount copy failed\n", EXIT_FAILURE);
    }

    // From here a failure leaves *two* copies, which is not a move: reported as a failure, no
    // row emitted. "Both now exist" tells an operator that nothing was lost.
    let removed = if is_dir {
        libfs::remove_recursive(&stage.namespace, src, &mut |_, _| {}).is_ok()
    } else {
        libfs::remove_entry(&stage.namespace, src).is_ok()
    };
    if !removed {
        stage.die(b"move: copied, but could not remove the original (both now exist)\n", EXIT_FAILURE);
    }
    "copy"
}

/// Is `path` a namespace binding rather than a filesystem entry? Asked of the parent's
/// bindings, which is where a mount point appears. Mirrors `remove`'s check.
fn is_binding(stage: &Stage, path: &str) -> bool {
    let name = libfs::basename_str(path);
    let parent = core::str::from_utf8(libfs::parent(path.as_bytes())).unwrap_or(path);
    libfs::bindings_under(&stage.namespace, parent).iter().any(|(n, _)| n == name)
}

fn emit(stage: &Stage, stdout: Port<'_>, done: &[Moved]) {
    let schema = Schema::new()
        .field("from", TypeTag::String, TypeModifiers::NONE)
        .field("to", TypeTag::String, TypeModifiers::NONE)
        .field("method", TypeTag::String, TypeModifiers::NONE);

    let mut tw = TableWriter::new(ChannelSink::new(stdout, IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for m in done {
            tw.write_row(&[Value::Str(m.from.clone()), Value::Str(m.to.clone()), Value::Str(String::from(m.method))])?;
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
    log::write(line.as_bytes());
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"move: panic\n");
    exit(EXIT_FAILURE)
}
