//! `mkdir` — create directories.
//!
//! The third Nitrox coreutil (shell design §10c/§10d), and the first of Milestone 2.
//! Where [`copy`](../copy) builds trees as a side effect of duplicating them, `mkdir`
//! is the direct form: one directory-protocol operation per name.
//!
//! ```text
//! mkdir PATH...              # create each, parent must already exist
//! mkdir --parents PATH...    # create missing intermediates; an existing leaf is fine
//! ```
//!
//! ## Deliberate behaviours
//!
//! - **`--parents` makes an existing leaf a success, not an error.** That is the whole
//!   point of the flag — "ensure this path exists" — and it is why the flag also relaxes
//!   the exists check rather than only creating intermediates.
//! - **Existence is learned from the attempt, and only its *kind* is asked about.** The
//!   server answers a colliding create with `KError::AlreadyExists` (since the 2026-07-30
//!   ABI pass; it was indistinguishable from a malformed request before), so the common
//!   path — the component is missing — is one round trip rather than a probe and a
//!   create. What the code *cannot* infer is whether the occupant is a directory, and
//!   `--parents` accepts only a directory, so [`fs::is_dir`] is asked exactly there: on
//!   the collision, not ahead of every component.
//! - **A failure stops the run**, as in `copy`. The rows already emitted say what was
//!   created before the failure; the exit status says the run did not succeed. Fail
//!   loud, don't fail silent (§1).
//! - **It emits a table.** `Table<{path: String, created: Bool}>`, one row per operand.
//!   `created` is `false` when `--parents` found the leaf already there — a pipeline can
//!   then tell "I made this" from "this was already so", which a bare exit status cannot.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::fs;
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::error::KError;
use libkern::{exit, kprint};
use librsproto::session::{Dir, DirError};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: path building and the TSM1 encoder both allocate.
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

/// One directory named on the command line, and whether this run created it.
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
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
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

/// Create `path`, whose parent must already exist. Diverges on failure.
fn make_one(stage: &Stage, path: &[u8]) -> bool {
    let parent = fs::parent(path);
    let name = fs::basename(path);
    if name.is_empty() {
        stage.die(b"mkdir: cannot create the root directory\n", EXIT_USAGE);
    }
    let mut buf = [0u8; 4096];
    let mut dir = match Dir::open(stage.namespace, parent, &mut buf) {
        Ok(d) => d,
        Err(_) => stage.die(b"mkdir: parent directory does not exist\n", EXIT_FAILURE),
    };
    let r = dir.mkdir(name);
    dir.close();
    match r {
        Ok(()) => true,
        // The server says the name is taken, so the message can say so without a
        // confirming round trip — which is the whole point of the error existing.
        Err(e) if is_already_exists(&e) => {
            stage.die(b"mkdir: path already exists (use --parents to allow it)\n", EXIT_FAILURE)
        }
        Err(_) => stage.die(b"mkdir: cannot create directory\n", EXIT_FAILURE),
    }
}

/// Whether a directory-op failure is the server reporting an occupied name.
fn is_already_exists(e: &DirError) -> bool {
    matches!(e, DirError::Server(k) if *k == KError::AlreadyExists.as_i32())
}

/// Create `path` and any missing components above it. Returns whether the **leaf** was
/// created by this run (`false` when it already existed). Diverges on failure.
fn make_parents(stage: &Stage, path: &[u8]) -> bool {
    // Walk the prefixes shortest-first and just try to create each one. A component that
    // is already there answers `AlreadyExists`, which is the ordinary case for the upper
    // prefixes and costs nothing extra; the probe is spent only on that answer, to check
    // the occupant is a directory. (Before the error existed this had to probe *every*
    // component first, because a collision could not be told from a real failure.)
    let mut created_leaf = false;
    for end in component_ends(path) {
        let prefix = &path[..end];
        let parent = fs::parent(prefix);
        let name = fs::basename(prefix);
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
                // `--parents` means "ensure this path exists as a directory". A file
                // wearing the name does not satisfy that, and silently continuing would
                // report success for a tree that was never built.
                if !fs::is_dir(stage.namespace, prefix) {
                    stage.die(
                        b"mkdir: path exists and is not a directory\n",
                        EXIT_FAILURE,
                    );
                }
                false
            }
            Err(_) => stage.die(b"mkdir: cannot create directory\n", EXIT_FAILURE),
        };
    }
    created_leaf
}

/// Byte offsets just past each path component, shortest prefix first.
///
/// `/a/b/c` yields the ends of `/a`, `/a/b`, `/a/b/c`. A leading slash is not itself a
/// component (the root always exists), and repeated slashes collapse.
fn component_ends(path: &[u8]) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut i = 0;
    while i < path.len() {
        if path[i] == b'/' {
            if i > 0 {
                ends.push(i);
            }
            // Skip a run of slashes.
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

/// Write the rows as a TSM1 table on the `stdout` stream.
fn emit(stage: &Stage, stdout: u64, made: &[Made]) {
    let schema = Schema::new()
        .field("path", TypeTag::String, TypeModifiers::NONE)
        .field("created", TypeTag::Bool, TypeModifiers::NONE);

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
