//! `mkdir` — create directories.
//!
//! Where [`copy`](../copy) builds trees as a side effect of duplicating them, `mkdir`
//! is the direct form: one directory-protocol operation per name.
//!
//! ## Deliberate behaviours
//!
//! - **`--parents` makes an existing leaf a success, not an error**: "ensure this path exists".
//! - **Existence is learned from the attempt, and only its *kind* is asked about.** The
//!   server answers a colliding create with `KError::AlreadyExists`, so the common path is
//!   one round trip. `--parents` accepts only a directory, so [`libfs::is_dir`] is asked
//!   on the collision, not ahead of every component.
//! - **A failure stops the run**, as in `copy`; the rows already emitted say what was created.
//! - **It emits a table.** `Table<{path: String, created: Bool}>`, one row per operand.
//!   `created` is `false` when `--parents` found the leaf already there.

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
use librsproto::session::{Dir, DirError};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const PARENTS: Flag = Flag::new("parents", 'p', "create missing parents; existing is not an error");

const HELP: &[u8] = b"usage: mkdir [--parents] PATH...\n\
    \n\
    Create directories.\n\
    Emits Table<{path: String, created: Bool}> on stdout.\n\
    \n\
      -p, --parents create missing parent directories, and treat an\n\
                    existing PATH as success rather than an error\n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"mkdir (nitrox coreutils) 0.1.0\n";

struct Made {
    path: String,
    created: bool,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[PARENTS]) {
        Ok(a) => a,
        Err(_) => stage.die(b"mkdir: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    if args.operands.is_empty() {
        stage.die(b"mkdir: need at least one path (try --help)\n", EXIT_USAGE);
    }

    let parents = args.has("parents");
    let mut made: Vec<Made> = Vec::new();
    for operand in &args.operands {
        let resolved = stage.path(operand.as_bytes());
        let path = trim_trailing_slashes(&resolved);
        if path.is_empty() {
            stage.die(b"mkdir: empty path\n", EXIT_USAGE);
        }
        let created = if parents { make_parents(&stage, path) } else { make_one(&stage, path) };
        made.push(Made { path: String::from_utf8_lossy(path).into_owned(), created });
    }

    match stage.streams.stdout {
        Some(h) => emit(&stage, h, &made),
        None => {
            for m in &made {
                report_text(m);
            }
        }
    }
    exit(EXIT_OK)
}

fn make_one(stage: &Stage, path: &[u8]) -> bool {
    // Checked here as well as in `libfs::mkdir`: this exits with a *usage* status and a
    // sentence about the root, where the library has only an error to return.
    if libfs::basename(path).is_empty() {
        stage.die(b"mkdir: cannot create the root directory\n", EXIT_USAGE);
    }
    match libfs::mkdir(stage.namespace, path) {
        Ok(()) => true,
        // The server says the name is taken, so no confirming round trip is needed.
        Err(TreeError::MakeDir(k)) if k == KError::AlreadyExists.as_i32() => {
            stage.die(b"mkdir: path already exists (use --parents to allow it)\n", EXIT_FAILURE)
        }
        Err(TreeError::OpenDir) => stage.die(b"mkdir: parent directory does not exist\n", EXIT_FAILURE),
        Err(_) => stage.die(b"mkdir: cannot create directory\n", EXIT_FAILURE),
    }
}

fn is_already_exists(e: &DirError) -> bool {
    matches!(e, DirError::Server(k) if *k == KError::AlreadyExists.as_i32())
}

/// Create `path` and any missing components above it; whether the **leaf** was created by this
/// run. Diverges on failure.
fn make_parents(stage: &Stage, path: &[u8]) -> bool {
    // Walk the prefixes shortest-first and try to create each one; `AlreadyExists` is the
    // ordinary answer for the upper prefixes, and only then is the occupant's kind probed.
    let mut created_leaf = false;
    for end in component_ends(path) {
        let prefix = &path[..end];
        let parent = libfs::parent(prefix);
        let name = libfs::basename(prefix);
        if name.is_empty() {
            continue;
        }
        let mut buf = [0u8; 4096];
        let mut dir = match Dir::open(stage.namespace, parent, &mut buf) {
            Ok(d) => d,
            Err(_) => stage.die(b"mkdir: cannot open a parent directory\n", EXIT_FAILURE),
        };
        let r = dir.mkdir(name);
        dir.close();
        created_leaf = match r {
            Ok(()) => true,
            Err(e) if is_already_exists(&e) => {
                // A file wearing the name does not satisfy "exists as a directory".
                if !libfs::is_dir(stage.namespace, prefix) {
                    stage.die(b"mkdir: path exists and is not a directory\n", EXIT_FAILURE);
                }
                false
            }
            Err(_) => stage.die(b"mkdir: cannot create directory\n", EXIT_FAILURE),
        };
    }
    created_leaf
}

/// Byte offsets just past each path component, shortest prefix first: `/a/b/c` yields the ends
/// of `/a`, `/a/b`, `/a/b/c`. The root is not a component, and repeated slashes collapse.
fn component_ends(path: &[u8]) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut i = 0;
    while i < path.len() {
        if path[i] == b'/' {
            if i > 0 {
                ends.push(i);
            }
            while i < path.len() && path[i] == b'/' {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    if !path.is_empty() && path != b"/" {
        ends.push(path.len());
    }
    ends
}

/// Drop trailing slashes so `mkdir /a/b/` names the same directory as `mkdir /a/b`.
/// A path that is only slashes trims to `/`, which the caller rejects as the root.
fn trim_trailing_slashes(path: &[u8]) -> &[u8] {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

fn emit(stage: &Stage, stdout: u64, made: &[Made]) {
    let schema = Schema::new().field("path", TypeTag::String, TypeModifiers::NONE).field(
        "created",
        TypeTag::Bool,
        TypeModifiers::NONE,
    );

    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for m in made {
            tw.write_row(&[Value::Str(m.path.clone()), Value::Bool(m.created)])?;
        }
        tw.finish_with_status(0)
    });
    let flushed = wrote.and_then(|()| tw.into_sink().finish());
    match flushed {
        Ok(()) => {}
        Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"mkdir: write failed\n", EXIT_FAILURE),
    }
}

/// The Tier-0 path: no `stdout`, so report in plain text. See
/// `TODO(tier0-output-sink)` — the kernel log is scaffolding, not the destination.
fn report_text(m: &Made) {
    let mut line = String::from(if m.created { "created " } else { "exists " });
    line.push_str(&m.path);
    line.push('\n');
    kprint(line.as_bytes());
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"mkdir: panic\n");
    exit(EXIT_FAILURE)
}
