//! `copy` — duplicate files and directories.
//!
//! Where [`list`](../list) exercises the *read* side of the filesystem, `copy` exercises the
//! *mutation* side: creating files, writing their contents, and building directory trees.
//!
//! ## Deliberate behaviours
//!
//! - **Directories copy recursively with no flag.** Unlike `remove`, copying has no
//!   destructive-by-default hazard.
//! - **An existing destination is an error** unless `--force`.
//! - **Overwriting a *longer* file shrinks it first**: creating an existing file is idempotent
//!   and growing it smaller is a no-op, so without an explicit truncate the old tail would
//!   survive past the new content.
//! - **Measured against Linux on real storage.** On the laptop, copying to a USB stick takes half
//!   Linux's time for small files and 1.65 times it for 8 MiB ones; the costs left are the
//!   filesystems': a grow's zeroes (`TODO(grow-zeroes)`) and ext4's metadata written a file at
//!   a time (`TODO(ext4-metadata-writes)`). The runs are in `docs/decision-log.md`
//!   (2026-10-13); measure with `nxsh`'s `time` before changing what a copy asks for.
//! - **It emits a table.** `Table<{source: String, destination: String, bytes: Int}>`, one
//!   row per file copied (`copy a b | display`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libfs::{FileError, TreeError};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::{exit, kprint};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

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
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    if args.operands.len() < 2 {
        stage.die(b"copy: need a source and a destination (try --help)\n", EXIT_USAGE);
    }

    let force = args.has("force");
    let (sources, dest) = args.operands.split_at(args.operands.len() - 1);
    let dest_owned = stage.path(dest[0].as_bytes());
    let dest = dest_owned.as_slice();
    let dest_is_dir = libfs::is_dir(stage.namespace, dest);

    // With a file destination, several sources would each overwrite the last.
    if sources.len() > 1 && !dest_is_dir {
        stage.die(b"copy: destination must be a directory when copying several sources\n", EXIT_USAGE);
    }

    let mut done: Vec<Copied> = Vec::new();
    for source in sources {
        let src_owned = stage.path(source.as_bytes());
        let src = src_owned.as_slice();
        // Into an existing directory a source keeps its name; otherwise `dest` names the copy.
        let target = if dest_is_dir {
            libfs::join(dest, libfs::basename(src))
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

/// Copy `src` to `dst` — a file, or a directory and everything under it — with
/// [`libfs::copy_tree`], shared with `move`; what stays here is one row per file and the message
/// each failure earns.
fn copy_any(stage: &Stage, src: &[u8], dst: &[u8], force: bool, done: &mut Vec<Copied>) {
    let r = libfs::copy_tree(stage.namespace, src, dst, force, &mut |s, d, bytes| {
        done.push(Copied {
            source: String::from_utf8_lossy(s).into_owned(),
            destination: String::from_utf8_lossy(d).into_owned(),
            bytes,
        });
    });
    // A partial copy is reported by the rows already emitted, and the run fails.
    match r {
        Ok(()) => {}
        Err(TreeError::TooDeep) => stage.die(b"copy: maximum recursion depth exceeded\n", EXIT_FAILURE),
        Err(TreeError::Copy(e)) => stage.die(describe(src, dst, e).as_bytes(), EXIT_FAILURE),
        Err(TreeError::MakeDir(_)) => stage.die(b"copy: cannot create the destination directory\n", EXIT_FAILURE),
        Err(TreeError::OpenDir) => stage.die(b"copy: cannot open a directory on the path\n", EXIT_FAILURE),
        Err(TreeError::ReadDir) => stage.die(b"copy: cannot enumerate the source directory\n", EXIT_FAILURE),
        Err(TreeError::IntoItself) => stage.die(b"copy: cannot copy a directory into itself\n", EXIT_FAILURE),
        Err(TreeError::Name) => stage.die(b"copy: a name in the tree is not one entry's\n", EXIT_FAILURE),
        Err(_) => stage.die(b"copy: cannot copy\n", EXIT_FAILURE),
    }
}

/// Write the report as a TSM1 table on `stdout`. As in `list`, a closed consumer is a
/// clean end, not a failure.
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

fn report_text(c: &Copied) {
    let mut line = String::from("copied ");
    line.push_str(&c.source);
    line.push_str(" -> ");
    line.push_str(&c.destination);
    line.push('\n');
    kprint(line.as_bytes());
}

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
        // Cannot arise (`copy` never renames); named so a new `FileError` still breaks this match.
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
