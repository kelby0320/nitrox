//! `remove` — delete files and directories.
//!
//! ## Deliberate behaviours
//!
//! - **A directory needs `--recursive`, unlike `copy`**: removing is destructive by default.
//!   Without the flag a directory operand is refused, even an empty one.
//! - **It walks the filesystem only — never namespace bindings.** `list` shows the *union* of
//!   the filesystem and the bindings beneath a path, which is wrong for deleting: a binding is
//!   a mount point, and `remove --recursive /` must not try to unbind `/dev/console`. The
//!   descent uses `Dir::read_dir` alone, and an operand that names a binding is refused.
//! - **`--force` suppresses only "not there"**; other failures stay failures.
//! - **A failure stops the run**, as in `copy`; the rows already emitted say what was removed.
//! - **It emits a table.** `Table<{path: String, kind: String}>`, one row per entry actually
//!   removed, including those removed *inside* a `--recursive` descent.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libfs::TreeError;
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::error::KError;
use libkern::{exit, kprint};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const RECURSIVE: Flag = Flag::new("recursive", 'r', "remove directories and their contents");
const FORCE: Flag = Flag::new("force", 'f', "ignore paths that do not exist");

const HELP: &[u8] = b"usage: remove [--recursive] [--force] PATH...\n\
    \n\
    Remove files and directories.\n\
    Emits Table<{path: String, kind: String}> on stdout.\n\
    \n\
      -r, --recursive remove directories and everything under them\n\
      -f, --force     a path that does not exist is not an error\n\
          --help      show this help and exit\n\
          --version   show version information and exit\n";

const VERSION: &[u8] = b"remove (nitrox coreutils) 0.1.0\n";

struct Removed {
    path: String,
    kind: &'static str,
}

enum What {
    Dir,
    File,
    Missing,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[RECURSIVE, FORCE]) {
        Ok(a) => a,
        Err(_) => stage.die(b"remove: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    if args.operands.is_empty() {
        stage.die(b"remove: need at least one path (try --help)\n", EXIT_USAGE);
    }

    let recursive = args.has("recursive");
    let force = args.has("force");
    let mut removed: Vec<Removed> = Vec::new();

    for operand in &args.operands {
        let resolved = stage.path(operand.as_bytes());
        let path = trim_trailing_slashes(&resolved);
        if path.is_empty() || path == b"/" {
            stage.die(b"remove: refusing to remove the root directory\n", EXIT_USAGE);
        }
        // Refuse a binding by name rather than let it fall through to "no such path".
        if is_binding(&stage, path) {
            stage.die(b"remove: path is a namespace binding, not a file\n", EXIT_USAGE);
        }
        match classify(&stage, path) {
            What::Missing => {
                if !force {
                    stage.die(b"remove: no such path\n", EXIT_FAILURE);
                }
            }
            What::File => {
                unlink_one(&stage, path);
                removed.push(Removed { path: as_string(path), kind: "file" });
            }
            What::Dir => {
                if !recursive {
                    stage.die(b"remove: path is a directory (use --recursive)\n", EXIT_USAGE);
                }
                remove_tree(&stage, path, &mut removed);
            }
        }
    }

    match stage.streams.stdout {
        Some(h) => emit(&stage, h, &removed),
        None => {
            for r in &removed {
                report_text(r);
            }
        }
    }
    exit(EXIT_OK)
}

/// Remove `path` and everything under it, appending a row per entry. Diverges on failure.
///
/// The walk is [`libfs::remove_tree`], shared with `move`'s cross-mount fallback; it enumerates
/// **filesystem entries only**, so it cannot delete through a mount point. The check that
/// `path` *itself* is not a binding stays at the operand.
fn remove_tree(stage: &Stage, path: &[u8], out: &mut Vec<Removed>) {
    let r = libfs::remove_tree(stage.namespace, path, &mut |p, is_dir| {
        out.push(Removed { path: as_string(p), kind: if is_dir { "directory" } else { "file" } });
    });
    match r {
        Ok(()) => {}
        Err(TreeError::TooDeep) => stage.die(b"remove: maximum recursion depth exceeded\n", EXIT_FAILURE),
        Err(TreeError::OpenDir) => stage.die(b"remove: cannot open directory\n", EXIT_FAILURE),
        Err(TreeError::ReadDir) => stage.die(b"remove: cannot read directory\n", EXIT_FAILURE),
        Err(TreeError::Unlink) => stage.die(b"remove: cannot remove file\n", EXIT_FAILURE),
        Err(TreeError::Name) => stage.die(b"remove: a name in the tree is not one entry's\n", EXIT_FAILURE),
        // The descent emptied the directory first, so `NotEmpty` means something else added an
        // entry meanwhile.
        Err(TreeError::Rmdir(k)) if k == KError::NotEmpty.as_i32() => {
            stage.die(b"remove: directory was refilled while being emptied\n", EXIT_FAILURE)
        }
        Err(_) => stage.die(b"remove: cannot remove directory\n", EXIT_FAILURE),
    }
}

fn unlink_one(stage: &Stage, path: &[u8]) {
    if libfs::unlink_at(stage.namespace, path).is_err() {
        stage.die(b"remove: cannot remove file\n", EXIT_FAILURE);
    }
}

fn classify(stage: &Stage, path: &[u8]) -> What {
    if libfs::is_dir(stage.namespace, path) {
        What::Dir
    } else if libfs::file_size(stage.namespace, path).is_some() {
        What::File
    } else {
        What::Missing
    }
}

/// Is `path` a namespace binding rather than a filesystem entry? Asked of the parent's
/// bindings, which is where a mount point appears.
fn is_binding(stage: &Stage, path: &[u8]) -> bool {
    let name = libfs::basename(path);
    libfs::ns_children(stage.namespace, libfs::parent(path)).iter().any(|(n, _)| n.as_bytes() == name)
}

fn trim_trailing_slashes(path: &[u8]) -> &[u8] {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

fn as_string(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn emit(stage: &Stage, stdout: u64, removed: &[Removed]) {
    let schema = Schema::new().field("path", TypeTag::String, TypeModifiers::NONE).field(
        "kind",
        TypeTag::String,
        TypeModifiers::NONE,
    );

    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for r in removed {
            tw.write_row(&[Value::Str(r.path.clone()), Value::Str(String::from(r.kind))])?;
        }
        tw.finish_with_status(0)
    });
    let flushed = wrote.and_then(|()| tw.into_sink().finish());
    match flushed {
        Ok(()) => {}
        Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"remove: write failed\n", EXIT_FAILURE),
    }
}

/// The Tier-0 path: no `stdout`, so report in plain text. See
/// `TODO(tier0-output-sink)` — the kernel log is scaffolding, not the destination.
fn report_text(r: &Removed) {
    let mut line = String::from("removed ");
    line.push_str(r.kind);
    line.push(' ');
    line.push_str(&r.path);
    line.push('\n');
    kprint(line.as_bytes());
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"remove: panic\n");
    exit(EXIT_FAILURE)
}
