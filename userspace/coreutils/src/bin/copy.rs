//! `copy` — duplicate files and directories.
//!
//! The second Nitrox coreutil (shell design §10c/§10d). Where [`list`](../list) exercises
//! the *read* side of the filesystem, `copy` exercises the *mutation* side: creating
//! files, growing them, writing their contents, and building directory trees.
//!
//! ```text
//! copy SOURCE DEST          # file → file, or directory → directory
//! copy SOURCE... DIR        # several sources into an existing directory
//! copy --force SOURCE DEST  # overwrite an existing destination
//! ```
//!
//! ## Deliberate behaviours
//!
//! - **Directories copy recursively with no flag.** Unlike `remove`, which requires
//!   `--recursive` as a safety rail, copying a directory has no destructive-by-default
//!   hazard — so demanding a flag would be ceremony (§10d).
//! - **An existing destination is an error** unless `--force`. Fail loud, don't fail
//!   silent (§1).
//! - **Overwriting a *longer* file shrinks it first.** Creating an existing file is
//!   idempotent and growing it smaller is a no-op, so without an explicit truncate the
//!   old tail would survive past the new content — a file that is neither the old one nor
//!   the new one. `copy` refused that case outright until the filesystem gained truncate.
//! - **It emits a table.** `Table<{source: String, destination: String, bytes: Int}>`, one
//!   row per file copied. A stage that produced nothing would leave a downstream consumer
//!   waiting on a stream that never arrives, and "what did it actually copy" is exactly
//!   what a pipeline wants to see (`copy a b | display`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::fs::{self, FileError, TreeError};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::{exit, kprint};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: path building and the TSM1 encoder both allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const FORCE: Flag = Flag::new("force", 'f', "overwrite an existing destination");

const HELP: &[u8] = b"usage: copy [--force] SOURCE... DEST\n\
    \n\
    Copy files and directories. Directories are copied recursively.\n\
    Emits Table<{source: String, destination: String, bytes: Int}> on stdout.\n\
    \n\
      -f, --force   overwrite an existing destination\n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"copy (nitrox coreutils) 0.1.0\n";

/// One completed file copy, as reported on stdout.
struct Copied {
    source: String,
    destination: String,
    bytes: u64,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[FORCE]) {
        Ok(a) => a,
        Err(_) => stage.die(b"copy: unrecognized option (try --help)\n", EXIT_USAGE),
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
        stage.die(b"copy: need a source and a destination (try --help)\n", EXIT_USAGE);
    }

    let force = args.has("force");
    let (sources, dest) = args.operands.split_at(args.operands.len() - 1);
    let dest_owned = stage.path(dest[0].as_bytes());
    let dest = dest_owned.as_slice();
    let dest_is_dir = fs::is_dir(stage.namespace, dest);

    // Several sources only make sense into a directory: with a file destination they
    // would each overwrite the last, which is never what was meant.
    if sources.len() > 1 && !dest_is_dir {
        stage.die(b"copy: destination must be a directory when copying several sources\n", EXIT_USAGE);
    }

    let mut done: Vec<Copied> = Vec::new();
    for source in sources {
        let src_owned = stage.path(source.as_bytes());
        let src = src_owned.as_slice();
        // Into an existing directory, a source keeps its own name; otherwise the
        // destination path names the copy itself.
        let target = if dest_is_dir {
            fs::join(dest, fs::basename(src))
        } else {
            String::from_utf8_lossy(dest).into_owned()
        };
        if target.as_bytes() == src {
            stage.die(b"copy: source and destination are the same file\n", EXIT_USAGE);
        }
        copy_any(&stage, src, target.as_bytes(), force, &mut done);
    }

    match stage.streams.stdout {
        Some(h) => emit(&stage, h, &done),
        None => {
            for c in &done {
                report_text(c);
            }
        }
    }
    exit(EXIT_OK)
}

/// Copy `src` to `dst` — a file, or a directory and everything under it.
///
/// The walk itself is [`fs::copy_tree`], shared with `move`'s cross-mount fallback: it was
/// duplicated here until `move` needed the recursive case, and a recursive walker is not a
/// thing to keep two of. What stays here is this program's *reporting* — one row per file,
/// and the message each failure earns.
fn copy_any(stage: &Stage, src: &[u8], dst: &[u8], force: bool, done: &mut Vec<Copied>) {
    let r = fs::copy_tree(stage.namespace, src, dst, force, &mut |s, d, bytes| {
        done.push(Copied {
            source: String::from_utf8_lossy(s).into_owned(),
            destination: String::from_utf8_lossy(d).into_owned(),
            bytes,
        });
    });
    // A partial copy is reported by the rows already emitted; the run is not silently
    // called a success.
    match r {
        Ok(()) => {}
        Err(TreeError::TooDeep) => {
            stage.die(b"copy: maximum recursion depth exceeded\n", EXIT_FAILURE)
        }
        Err(TreeError::Copy(e)) => stage.die(describe(src, dst, e).as_bytes(), EXIT_FAILURE),
        Err(TreeError::MakeDir) => {
            stage.die(b"copy: cannot create the destination directory\n", EXIT_FAILURE)
        }
        Err(TreeError::OpenDir) => {
            stage.die(b"copy: cannot open a directory on the path\n", EXIT_FAILURE)
        }
        Err(TreeError::ReadDir) => {
            stage.die(b"copy: cannot enumerate the source directory\n", EXIT_FAILURE)
        }
        Err(_) => stage.die(b"copy: cannot copy\n", EXIT_FAILURE),
    }
}

/// Whether `path` names a directory — it resolves to a directory *session*, which a file
/// never does.

/// Write the report as a TSM1 table on `stdout`. As in `list`, a closed consumer is a
/// clean end, not a failure (design §1).
fn emit(stage: &Stage, stdout: u64, done: &[Copied]) {
    let schema = Schema::new()
        .field("source", TypeTag::String, TypeModifiers::NONE)
        .field("destination", TypeTag::String, TypeModifiers::NONE)
        .field("bytes", TypeTag::Int, TypeModifiers::NONE);

    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for c in done {
            tw.write_row(&[
                Value::Str(c.source.clone()),
                Value::Str(c.destination.clone()),
                Value::Int(c.bytes as i64),
            ])?;
        }
        tw.finish_with_status(0)
    });
    let flushed = wrote.and_then(|()| tw.into_sink().finish());
    match flushed {
        Ok(()) => {}
        Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"copy: write failed\n", EXIT_FAILURE),
    }
}

/// The Tier-0 rendering (no `stdout` stream): one line per copy on the kernel log.
fn report_text(c: &Copied) {
    let mut line = String::from("copied ");
    line.push_str(&c.source);
    line.push_str(" -> ");
    line.push_str(&c.destination);
    line.push('\n');
    kprint(line.as_bytes());
}

/// A diagnostic naming the paths and the specific reason.
fn describe(src: &[u8], dst: &[u8], e: FileError) -> String {
    let mut s = String::from("copy: ");
    s.push_str(&String::from_utf8_lossy(src));
    s.push_str(" -> ");
    s.push_str(&String::from_utf8_lossy(dst));
    s.push_str(match e {
        FileError::NotFound => ": source not found\n",
        FileError::Exists => ": destination exists (use --force)\n",
        FileError::TruncateFailed => {
            ": could not shrink the destination to the source's length — refusing to \
             leave a corrupt tail\n"
        }
        FileError::TooLarge => ": file is too large to copy in one mapping\n",
        // `copy` never renames, so this cannot arise here; named rather than folded into
        // a wildcard so that adding a `FileError` still breaks this match loudly.
        FileError::CrossDevice => ": destination is on a different filesystem\n",
        FileError::Io(_) => ": I/O error\n",
    });
    s
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"copy: panic\n");
    exit(EXIT_FAILURE);
}
