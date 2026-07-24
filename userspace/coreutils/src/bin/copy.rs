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
//! - **Overwriting a *longer* file is refused**, even with `--force`. The filesystem has
//!   no truncate operation: the old tail would survive past the new content, producing a
//!   file that is neither the old one nor the new one. Refusing beats corrupting; see
//!   `deferred-decisions.md`.
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
use coreutils::fs::{self, FileError};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::{exit, kprint};
use librsproto::file::{DIRENT_KIND_DIR, OwnedEntry};
use librsproto::session::{Dir, DirError};
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

/// Depth cap, as in `list`: a pathological tree must stop rather than run forever.
const MAX_DEPTH: u32 = 32;

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
    let dest = dest[0].as_bytes();
    let dest_is_dir = is_dir(stage.namespace, dest);

    // Several sources only make sense into a directory: with a file destination they
    // would each overwrite the last, which is never what was meant.
    if sources.len() > 1 && !dest_is_dir {
        stage.die(b"copy: destination must be a directory when copying several sources\n", EXIT_USAGE);
    }

    let mut done: Vec<Copied> = Vec::new();
    for source in sources {
        let src = source.as_bytes();
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
        copy_any(&stage, src, target.as_bytes(), force, 0, &mut done);
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

/// Copy `src` to `dst`, dispatching on what `src` is. Any failure is fatal and exits —
/// a partially-completed copy is reported by the rows already emitted, but the run is not
/// silently called a success.
fn copy_any(
    stage: &Stage,
    src: &[u8],
    dst: &[u8],
    force: bool,
    depth: u32,
    done: &mut Vec<Copied>,
) {
    if depth > MAX_DEPTH {
        stage.die(b"copy: maximum recursion depth exceeded\n", EXIT_FAILURE);
    }
    if is_dir(stage.namespace, src) {
        copy_dir(stage, src, dst, force, depth, done);
    } else {
        match fs::copy_file(stage.namespace, src, dst, force) {
            Ok(bytes) => done.push(Copied {
                source: String::from_utf8_lossy(src).into_owned(),
                destination: String::from_utf8_lossy(dst).into_owned(),
                bytes,
            }),
            Err(e) => stage.die(describe(src, dst, e).as_bytes(), EXIT_FAILURE),
        }
    }
}

/// Copy a directory: create the destination, then copy every entry into it.
///
/// Entries are collected and the source session **closed before recursing**, so a deep
/// tree holds one session at a time rather than one per level (`MAX_SESSIONS` is small).
fn copy_dir(
    stage: &Stage,
    src: &[u8],
    dst: &[u8],
    force: bool,
    depth: u32,
    done: &mut Vec<Copied>,
) {
    // Create the destination directory via its parent's session — directory ops are
    // name-addressed, so making `/a/b` means opening `/a` and asking for `b`.
    let parent = fs::parent(dst);
    let name = fs::basename(dst);
    let mut pbuf = [0u8; 4096];
    let mut pdir = match Dir::open(stage.namespace, parent, &mut pbuf) {
        Ok(d) => d,
        Err(_) => stage.die(b"copy: cannot open the destination's parent directory\n", EXIT_FAILURE),
    };
    let made = pdir.mkdir(name);
    pdir.close();
    if let Err(e) = made {
        // An existing destination directory is only acceptable under --force: merging
        // into someone else's tree is exactly the surprise the fail-loud default avoids.
        if !(force && matches!(e, DirError::Server(_)) && is_dir(stage.namespace, dst)) {
            stage.die(b"copy: cannot create the destination directory\n", EXIT_FAILURE);
        }
    }

    let mut entries: Vec<OwnedEntry> = Vec::new();
    {
        let mut sbuf = [0u8; 4096];
        let mut sdir = match Dir::open(stage.namespace, src, &mut sbuf) {
            Ok(d) => d,
            Err(_) => stage.die(b"copy: cannot read the source directory\n", EXIT_FAILURE),
        };
        let r = sdir.read_dir(|e| {
            if e.name != b"." && e.name != b".." {
                entries.push(OwnedEntry::from_entry(e));
            }
            true
        });
        sdir.close();
        if r.is_err() {
            stage.die(b"copy: cannot enumerate the source directory\n", EXIT_FAILURE);
        }
    }

    for e in &entries {
        let child_src = fs::join(src, e.name());
        let child_dst = fs::join(dst, e.name());
        if e.kind == DIRENT_KIND_DIR {
            copy_dir(stage, child_src.as_bytes(), child_dst.as_bytes(), force, depth + 1, done);
        } else {
            copy_any(stage, child_src.as_bytes(), child_dst.as_bytes(), force, depth + 1, done);
        }
    }
}

/// Whether `path` names a directory — it resolves to a directory *session*, which a file
/// never does.
fn is_dir(ns: u64, path: &[u8]) -> bool {
    let mut buf = [0u8; 4096];
    match Dir::open(ns, path, &mut buf) {
        Ok(d) => {
            d.close();
            true
        }
        Err(_) => false,
    }
}

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
        FileError::WouldTruncate => {
            ": destination is longer than the source, and this filesystem cannot \
             truncate — refusing to leave a corrupt tail\n"
        }
        FileError::TooLarge => ": file is too large to copy in one mapping\n",
        FileError::Io(_) => ": I/O error\n",
    });
    s
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"copy: panic\n");
    exit(EXIT_FAILURE);
}
