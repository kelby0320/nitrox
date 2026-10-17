//! `list` — enumerate a directory as a typed stream.
//!
//! Emits `Table<{name: String, size: Int, kind: String, modified: Int}>` as TSM1 on `stdout`,
//! one row per entry: `list /system | filter size > 1000 | sort name`.
//!
//! Named `list`, not `ls`: the Unix-familiar short name is a namespace bind pointing at it.
//!
//! `name` is the entry name, or the path relative to the listed directory under `--recursive`;
//! `size` is bytes (a directory reports its own directory-data size); `kind` is `"file"`,
//! `"dir"`, `"symlink"` or `"unknown"`, a string because the consumer is a shell predicate;
//! `modified` is Unix epoch seconds. `.` and `..` are **not** emitted: structure, not content.
//!
//! With a `stdout` stream the table goes there as TSM1; without one (Tier 0) it renders as plain
//! text to the kernel log.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::{exit, kprint};
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE, DIRENT_KIND_SYMLINK, OwnedEntry};
use librsproto::session::DirError;
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const RECURSIVE: Flag = Flag::new("recursive", 'r', "list subdirectories recursively");

const HELP: &[u8] = b"usage: list [--recursive] [path...]\n\
    \n\
    Emit a directory listing as a typed stream (TSM1) on stdout:\n\
    Table<{name: String, size: Int, kind: String, modified: Int}>\n\
    \n\
      -r, --recursive   list subdirectories recursively\n\
          --help        show this help and exit\n\
          --version     show version information and exit\n";

const VERSION: &[u8] = b"list (nitrox coreutils) 0.1.0\n";

const DEFAULT_PATH: &[u8] = b"/";

/// Recursion depth cap: a symlink loop must stop with an error, not truncate silently.
const MAX_DEPTH: u32 = 32;

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[RECURSIVE]) {
        Ok(a) => a,
        Err(_) => stage.die(b"list: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }

    // Several operands list each in turn, into one stream: a stage emits one table.
    let mut paths: Vec<String> = args.operands.clone();
    if paths.is_empty() {
        // A bare `list` means "here" when there is a working directory.
        paths.push(match &stage.cwd {
            Some(cwd) => cwd.clone(),
            None => owned(DEFAULT_PATH),
        });
    }
    let paths: Vec<String> =
        paths.iter().map(|p| String::from_utf8_lossy(&stage.path(p.as_bytes())).into_owned()).collect();

    let mut rows: Vec<(String, OwnedEntry)> = Vec::new();
    for path in &paths {
        if let Err(e) = collect(&stage, path.as_bytes(), "", args.has("recursive"), 0, &mut rows) {
            stage.diag(describe(path.as_bytes(), e).as_bytes());
            exit(EXIT_FAILURE);
        }
    }

    match stage.streams.stdout {
        Some(h) => emit_stream(&stage, h, &rows),
        None => emit_text(&rows),
    }
    exit(EXIT_OK)
}

/// Enumerate the directory at `path`, appending `(reported_name, entry)` for each entry.
///
/// `prefix` is the reported path of `path` **relative to the directory the user named**: empty at
/// the top level, `"sub/dir"` inside a recursive descent, so same-named files in different
/// subdirectories stay distinguishable.
///
/// Each directory's session is **closed before descending**, so a deep tree holds one session at
/// a time: the server's concurrent-session cap (`MAX_SESSIONS`) is small.
fn collect(
    stage: &Stage,
    path: &[u8],
    prefix: &str,
    recursive: bool,
    depth: u32,
    out: &mut Vec<(String, OwnedEntry)>,
) -> Result<(), DirError> {
    if depth > MAX_DEPTH {
        stage.diag(b"list: maximum recursion depth exceeded\n");
        return Err(DirError::Protocol);
    }
    // The union of the filesystem under this path and the namespace bindings beneath it, with
    // bindings shadowing; `libfs` owns that rule.
    let entries = libfs::list_dir(stage.namespace, path)?;

    for e in &entries {
        out.push((reported(prefix, e.name()), *e));
    }
    if recursive {
        for e in &entries {
            if e.kind == DIRENT_KIND_DIR {
                let child_path = libfs::join(path, e.name());
                let child_prefix = reported(prefix, e.name());
                collect(stage, child_path.as_bytes(), &child_prefix, true, depth + 1, out)?;
            }
        }
    }
    Ok(())
}

/// Bytes to an owned `String`, replacing invalid UTF-8: a listing must not be unprintable
/// because one name is not valid UTF-8.
fn owned(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn reported(prefix: &str, name: &[u8]) -> String {
    if prefix.is_empty() {
        return owned(name);
    }
    let mut s = String::from(prefix);
    s.push('/');
    s.push_str(&owned(name));
    s
}

/// Write the rows as a TSM1 table. A `PeerClosed` — the consumer closed its read end, the
/// `yes | head -1` case — is a **clean** end: the stage exits `0`. Any other write error fails.
fn emit_stream(stage: &Stage, stdout: u64, rows: &[(String, OwnedEntry)]) -> ! {
    let schema = Schema::new()
        .field("name", TypeTag::String, TypeModifiers::NONE)
        .field("size", TypeTag::Int, TypeModifiers::NONE)
        .field("kind", TypeTag::String, TypeModifiers::NONE)
        .field("modified", TypeTag::Int, TypeModifiers::NONE);

    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    if let Err(e) = tw.write_schema(StreamFlags::NONE, &schema) {
        finish_write(stage, e);
    }
    for (name, e) in rows {
        let row = [
            Value::Str(name.clone()),
            Value::Int(e.size as i64),
            Value::Str(String::from(kind_name(e.kind))),
            Value::Int(e.mtime),
        ];
        if let Err(err) = tw.write_row(&row) {
            finish_write(stage, err);
        }
    }
    if let Err(e) = tw.finish_with_status(0) {
        finish_write(stage, e);
    }
    if let Err(e) = tw.into_sink().finish() {
        finish_write(stage, e);
    }
    exit(EXIT_OK)
}

/// Resolve a stream-write error into an exit: a closed consumer is a clean stop, anything
/// else is a failure.
fn finish_write(stage: &Stage, e: libstream::wire::WireError) -> ! {
    if e == libstream::wire::WireError::PeerClosed {
        exit(EXIT_OK);
    }
    stage.die(b"list: write failed\n", EXIT_FAILURE)
}

/// Render the rows as plain text to the kernel log: the Tier-0 path, with no `stdout` stream.
///
/// `TODO(tier0-output-sink)`: the kernel log is the wrong destination for program output.
/// `kprint` is a kernel *diagnostic* path and the klog is a bounded ring, so this evicts
/// kernel diagnostics to print a directory listing. Acceptable while init and the test
/// harness are the only spawners; it should not survive the shell. See
/// `docs/rationale/deferred-decisions.md`.
fn emit_text(rows: &[(String, OwnedEntry)]) {
    for (name, e) in rows {
        let mut line = String::new();
        line.push_str(kind_name(e.kind));
        line.push(' ');
        push_u64(&mut line, e.size);
        line.push(' ');
        line.push_str(name);
        line.push('\n');
        kprint(line.as_bytes());
    }
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        DIRENT_KIND_FILE => "file",
        DIRENT_KIND_DIR => "dir",
        DIRENT_KIND_SYMLINK => "symlink",
        _ => "unknown",
    }
}

fn push_u64(s: &mut String, mut v: u64) {
    if v == 0 {
        s.push('0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut n = 0;
    while v > 0 {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        s.push(digits[n] as char);
    }
}

fn describe(path: &[u8], e: DirError) -> String {
    let mut s = String::from("list: ");
    s.push_str(&owned(path));
    s.push_str(match e {
        DirError::Server(_) => ": cannot list (not a directory, or not found)\n",
        DirError::Transport(_) => ": filesystem unreachable\n",
        DirError::Protocol => ": malformed reply from the filesystem\n",
    });
    s
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"list: panic\n");
    // Not critical-path (unlike init/eshell): exit, and the supervisor sees the status.
    exit(EXIT_FAILURE);
}
