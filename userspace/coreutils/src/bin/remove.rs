//! `remove` — delete files and directories.
//!
//! The fourth Nitrox coreutil (shell design §10c/§10d), and the destructive half of
//! Milestone 2 Part A.
//!
//! ```text
//! remove PATH...                # files, and empty directories only with --recursive
//! remove --recursive PATH...    # directories and everything under them
//! remove --force PATH...        # a path that is not there is not an error
//! ```
//!
//! ## Deliberate behaviours
//!
//! - **A directory needs `--recursive`, unlike `copy`.** Copying a directory has no
//!   destructive-by-default hazard, so `copy` needs no flag; removing one does, so this
//!   is the safety rail (§10d). Without the flag a directory operand is refused, even if
//!   it happens to be empty — "this is a directory" is the fact worth reporting, and
//!   requiring the flag for the empty case too keeps the rule one sentence long.
//! - **It walks the filesystem only — never namespace bindings.** `list` shows a path's
//!   contents as the *union* of the filesystem under it and the namespace bindings
//!   beneath it, which is right for looking. It is emphatically wrong for deleting: a
//!   binding is a mount point, not a file, and `remove --recursive /` must not try to
//!   unbind `/dev/console`. So the descent here uses `Dir::read_dir` alone, and an
//!   operand that names a binding is refused by name.
//! - **`--force` suppresses only "not there".** It does not make other failures
//!   survivable — that would turn a broken filesystem into a silent success.
//! - **A failure stops the run**, as in `copy`: the rows already emitted say what was
//!   removed before it, and the exit status says the run did not succeed.
//! - **It emits a table.** `Table<{path: String, kind: String}>`, one row per entry
//!   actually removed — including the ones removed *inside* a `--recursive` descent, so
//!   the stream is a record of what was destroyed rather than only what was named.

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
use librsproto::file::{DIRENT_KIND_DIR, OwnedEntry};
use librsproto::session::{Dir, DirError};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: path building and the TSM1 encoder both allocate.
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

/// Depth cap, as in `list` and `copy`: a pathological tree must stop rather than run
/// forever.
const MAX_DEPTH: u32 = 32;

/// One entry this run removed.
struct Removed {
    path: String,
    kind: &'static str,
}

/// What an operand names, as far as the *filesystem* is concerned.
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
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }
    if args.operands.is_empty() {
        stage.die(b"remove: need at least one path (try --help)\n", EXIT_USAGE);
    }

    let recursive = args.has("recursive");
    let force = args.has("force");
    let mut removed: Vec<Removed> = Vec::new();

    for operand in &args.operands {
        let path = trim_trailing_slashes(operand.as_bytes());
        if path.is_empty() || path == b"/" {
            stage.die(b"remove: refusing to remove the root directory\n", EXIT_USAGE);
        }
        // A namespace binding is a mount point, not a file. Refuse it by name rather
        // than letting it fall through to "no such path", which would be true of the
        // filesystem and misleading about what is actually there.
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
                remove_tree(&stage, path, 0, &mut removed);
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

/// Remove `path` and everything under it, depth-first, appending a row per entry.
/// Diverges on failure.
fn remove_tree(stage: &Stage, path: &[u8], depth: u32, out: &mut Vec<Removed>) {
    if depth > MAX_DEPTH {
        stage.die(b"remove: maximum recursion depth exceeded\n", EXIT_FAILURE);
    }

    // Read the children first and close the session before recursing: a session is a
    // scarce server resource (`MAX_SESSIONS`), and holding one open per level would cap
    // the depth at the session table rather than at `MAX_DEPTH`.
    //
    // Filesystem entries only — deliberately *not* `fs::ns_children`. See the module
    // docs: a binding beneath this path is a mount point, and unbinding is not this
    // program's job.
    let mut entries: Vec<OwnedEntry> = Vec::new();
    {
        let mut buf = [0u8; 4096];
        let mut dir = match Dir::open(stage.namespace, path, &mut buf) {
            Ok(d) => d,
            Err(_) => stage.die(b"remove: cannot open directory\n", EXIT_FAILURE),
        };
        let r = dir.read_dir(|e| {
            if e.name != b"." && e.name != b".." {
                entries.push(OwnedEntry::from_entry(e));
            }
            true
        });
        dir.close();
        if r.is_err() {
            stage.die(b"remove: cannot read directory\n", EXIT_FAILURE);
        }
    }

    for e in &entries {
        let child = fs::join(path, e.name());
        if e.kind == DIRENT_KIND_DIR {
            remove_tree(stage, child.as_bytes(), depth + 1, out);
        } else {
            unlink_one(stage, child.as_bytes());
            out.push(Removed { path: child, kind: "file" });
        }
    }

    // Now empty, so `rmdir` can take it.
    rmdir_one(stage, path);
    out.push(Removed { path: as_string(path), kind: "directory" });
}

/// `unlink` the entry named by `path`. Diverges on failure.
fn unlink_one(stage: &Stage, path: &[u8]) {
    let mut buf = [0u8; 4096];
    let mut dir = match Dir::open(stage.namespace, fs::parent(path), &mut buf) {
        Ok(d) => d,
        Err(_) => stage.die(b"remove: cannot open parent directory\n", EXIT_FAILURE),
    };
    let r = dir.unlink(fs::basename(path));
    dir.close();
    if r.is_err() {
        stage.die(b"remove: cannot remove file\n", EXIT_FAILURE);
    }
}

/// `rmdir` the (now empty) directory named by `path`. Diverges on failure.
fn rmdir_one(stage: &Stage, path: &[u8]) {
    let mut buf = [0u8; 4096];
    let mut dir = match Dir::open(stage.namespace, fs::parent(path), &mut buf) {
        Ok(d) => d,
        Err(_) => stage.die(b"remove: cannot open parent directory\n", EXIT_FAILURE),
    };
    let r = dir.rmdir(fs::basename(path));
    dir.close();
    match r {
        Ok(()) => {}
        // This descent emptied the directory before asking for it, so `NotEmpty` here is
        // not "you forgot --recursive" — it means something else added an entry while the
        // walk was in progress. Naming the race beats reporting an unexplained failure,
        // and it is the reason `NotEmpty` is worth a discriminant to a client that never
        // deliberately removes a populated directory.
        Err(e) if matches!(e, DirError::Server(k) if k == KError::NotEmpty.as_i32()) => stage
            .die(
                b"remove: directory was refilled while being emptied\n",
                EXIT_FAILURE,
            ),
        Err(_) => stage.die(b"remove: cannot remove directory\n", EXIT_FAILURE),
    }
}

/// What the filesystem says `path` is.
fn classify(stage: &Stage, path: &[u8]) -> What {
    if fs::is_dir(stage.namespace, path) {
        What::Dir
    } else if fs::file_size(stage.namespace, path).is_some() {
        What::File
    } else {
        What::Missing
    }
}

/// Is `path` a namespace binding rather than a filesystem entry? Asked of the parent's
/// bindings, which is where a mount point appears.
fn is_binding(stage: &Stage, path: &[u8]) -> bool {
    let name = fs::basename(path);
    fs::ns_children(stage.namespace, fs::parent(path))
        .iter()
        .any(|(n, _)| n.as_bytes() == name)
}

/// Drop trailing slashes so `remove /a/b/` names the same entry as `remove /a/b`.
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

/// Write the rows as a TSM1 table on the `stdout` stream.
fn emit(stage: &Stage, stdout: u64, removed: &[Removed]) {
    let schema = Schema::new()
        .field("path", TypeTag::String, TypeModifiers::NONE)
        .field("kind", TypeTag::String, TypeModifiers::NONE);

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
