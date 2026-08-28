//! `list` — enumerate a directory as a typed stream.
//!
//! The first Nitrox coreutil (shell design §10c/§10d). Emits
//! `Table<{name: String, size: Int, kind: String, modified: Int}>` as TSM1 on `stdout`,
//! one row per entry, so the rest of a pipeline operates on *data* rather than on
//! re-parsed text: `list /system | filter size > 1000 | sort name`.
//!
//! Named `list`, not `ls` — the friendly name is the real program, and the Unix-familiar
//! short name is a namespace bind pointing at it (§10e), not a shell alias.
//!
//! ## What it emits
//!
//! - `name` — the entry name, or the path relative to the listed directory under
//!   `--recursive` (otherwise a recursive listing could not distinguish two same-named
//!   files in different subdirectories).
//! - `size` — bytes. A directory reports its own directory-data size, as the server does.
//! - `kind` — `"file"` / `"dir"` / `"symlink"` / `"unknown"`. A string rather than an
//!   integer because the consumer is a shell predicate (`filter kind == "dir"`), not C.
//! - `modified` — Unix epoch seconds. Rendering it as a date belongs to `display`/`date`,
//!   not here: a stage emits data, and formatting is the terminal end of a pipeline.
//!
//! `.` and `..` are **not** emitted. They are real directory entries and the protocol
//! carries them, but they are structure rather than content — every consumer would filter
//! them, so filtering belongs here once.
//!
//! ## Streams
//!
//! With a `stdout` stream (a shell-spawned Tier-1 stage) the table goes there as TSM1.
//! Without one (Tier 0 — spawned directly, before a shell exists) it renders as plain
//! text to the kernel log, so the program is observable on its own. That is the text
//! floor, not a second data path: the typed stream is the contract.

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
use librsproto::session::{Dir, DirError};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: entry accumulation and the TSM1 encoder both allocate.
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

/// The directory listed when no operand is given.
///
/// A shell would supply the working directory, but `cd` is a shell-state builtin that
/// does not exist yet (§3) and there is no ambient cwd in the kernel by design — so the
/// namespace root is the honest default until the shell can pass one.
const DEFAULT_PATH: &[u8] = b"/";

/// Recursion depth cap. A symlink loop or a pathological tree must not run forever, and
/// the fail-loud default says stop with an error rather than silently truncating.
const MAX_DEPTH: u32 = 32;

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[RECURSIVE]) {
        Ok(a) => a,
        Err(_) => stage.die(b"list: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }

    // No operand: list the default directory. Several operands list each in turn, into
    // one stream — a pipeline stage emits one table, not one per argument.
    // Resolved at the boundary, against this stage's own `PWD` (M3.5 Part B) — the shell
    // passes the argument through as written and hands over the working directory, so
    // `list .` means the same thing here as `open ./x` does in the shell.
    let mut paths: Vec<String> = args.operands.clone();
    if paths.is_empty() {
        // With no operand, the default is the working directory when there is one — which
        // is what makes a bare `list` mean "here" rather than always meaning the root.
        paths.push(match &stage.cwd {
            Some(cwd) => cwd.clone(),
            None => owned(DEFAULT_PATH),
        });
    }
    let paths: Vec<String> = paths
        .iter()
        .map(|p| String::from_utf8_lossy(&stage.path(p.as_bytes())).into_owned())
        .collect();

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
/// `prefix` is the reported path of `path` **relative to the directory the user named**:
/// empty at the top level (so a plain listing reports bare names, as one would expect),
/// and `"sub/dir"` inside a recursive descent. Without it a recursive listing could not
/// distinguish two same-named files in different subdirectories — every row would read as
/// a bare name with no indication of where it came from.
///
/// Each directory's entries are collected and its session **closed before descending**, so
/// a deep tree holds one session at a time rather than one per level — the server's
/// concurrent-session cap is small (`MAX_SESSIONS`), and a recursive listing must not be
/// what exhausts it.
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
    // A path's listing is the **union** of the filesystem under it and the namespace
    // bindings directly beneath it — which is just how mount points appear in a parent's
    // listing. So there is no "which mechanism?" decision: ask both, merge, and let each
    // answer for the part it owns. `/dev` is then unremarkable (all bindings, no
    // filesystem); `/system` is unremarkable the other way; `/` genuinely needs both.
    let ns_entries = libfs::ns_children(stage.namespace, path);

    let mut entries: Vec<OwnedEntry> = Vec::new();
    let mut buf = [0u8; 4096];
    match Dir::open(stage.namespace, path, &mut buf) {
        Ok(mut dir) => {
            let r = dir.read_dir(|e| {
                if e.name != b"." && e.name != b".." {
                    entries.push(OwnedEntry::from_entry(e));
                }
                true
            });
            dir.close();
            r?;
        }
        // No filesystem here. That is an error only if the namespace has nothing beneath
        // this path either — otherwise it is an ordinary kernel-served directory like
        // `/dev`, and the bindings below are the whole answer.
        Err(e) => {
            if ns_entries.is_empty() {
                return Err(e);
            }
        }
    }

    // Bindings shadow same-named filesystem entries, as a mount point shadows the
    // directory it covers.
    for (name, kind) in &ns_entries {
        entries.retain(|e| e.name() != name.as_bytes());
        entries.push(OwnedEntry::binding(name.as_bytes(), *kind));
    }

    for e in &entries {
        out.push((reported(prefix, e.name()), *e));
    }
    if recursive {
        for e in &entries {
            if e.kind == DIRENT_KIND_DIR {
                // Two different paths: the filesystem path to open, and the relative
                // path to report. Conflating them would either break the open (a
                // relative path does not resolve) or the report (an absolute path
                // buries what the user asked about).
                let child_path = libfs::join(path, e.name());
                let child_prefix = reported(prefix, e.name());
                collect(stage, child_path.as_bytes(), &child_prefix, true, depth + 1, out)?;
            }
        }
    }
    Ok(())
}

/// Bytes to an owned `String`, replacing any invalid UTF-8 rather than failing: a name on
/// disk is a byte string, and a listing must not be unprintable because one entry is not
/// valid UTF-8. (`String::from_utf8_lossy_owned` would be tidier but is a nightly library
/// feature, which this project does not use.)
fn owned(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The name a row reports: bare at the top level, `prefix/name` inside a descent.
fn reported(prefix: &str, name: &[u8]) -> String {
    if prefix.is_empty() {
        return owned(name);
    }
    let mut s = String::from(prefix);
    s.push('/');
    s.push_str(&owned(name));
    s
}

/// Write the rows as a TSM1 table on the `stdout` stream.
///
/// A `PeerClosed` — the consumer closed its read end, the `yes | head -1` case — is a
/// **clean** end, not a failure: the stage stops producing and exits `0` (design §1). Any
/// other write error is a real failure.
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

/// Render the rows as plain text to the kernel log — the Tier-0 path, where the program
/// was spawned without a shell and so has no `stdout` stream to write a table to.
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

/// The wire kind as the string the table carries.
fn kind_name(kind: u8) -> &'static str {
    match kind {
        DIRENT_KIND_FILE => "file",
        DIRENT_KIND_DIR => "dir",
        DIRENT_KIND_SYMLINK => "symlink",
        _ => "unknown",
    }
}

/// Append `v` in decimal. (`core` has no `format!` without `alloc`'s machinery, and the
/// text path is a fallback, not the contract — a tiny helper beats pulling in more.)
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

/// A diagnostic naming both the path and what went wrong — "list: /nope: not found"
/// beats "list: failed".
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
    // A coreutil is not critical-path (unlike init/eshell), so a panic exits rather than
    // hanging: its supervisor observes the non-zero status.
    exit(EXIT_FAILURE);
}
