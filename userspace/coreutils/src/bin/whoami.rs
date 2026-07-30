//! `whoami` — report the session's user.
//!
//! Milestone 2 Part E, and the one that needed a design decision rather than an
//! implementation (open question B2).
//!
//! ```text
//! whoami          # the user this session was opened for
//! ```
//!
//! ## Where identity lives, and why it is not a syscall
//!
//! Nitrox has **no kernel user identity**. Authority is held in handles, not derived from
//! a UID, so there is nothing for the kernel to report and `/proc/self/*` deliberately
//! carries none. Identity is a *session* concept: `session-mgr` authenticates a login
//! against `auth-service` and then **constructs the session's namespace** — binding the
//! user's home at `/home`, the console at `/dev/console`, and omitting everything else,
//! because absence is the sandbox.
//!
//! So this asks the namespace, which is the mechanism by which a process is *told* about
//! its world: `/session/user` is a readable object holding the name. That is the same
//! answer-by-construction the shell already relies on for `/home` — it does not ask
//! anyone where home is, it simply sees it — and asking a service instead would be closer
//! to ambient lookup than to capabilities.
//!
//! ## Why reading it looks like this, and will not change
//!
//! Today `/session/user` is a **direct-handle bind** of a memory object: a snapshot, which
//! is correct because a session's user is immutable for its lifetime (changing user means
//! a new session, not a mutated one). Session metadata will grow, and the first genuinely
//! *mutable* member forces a resource server behind `/session/*` instead.
//!
//! That migration does not touch this file. A userspace-server binding answers a resolve
//! with `OBJECT_KIND_MEMOBJ`, which the kernel cross-context-installs — so `lookup + map +
//! read` is byte-identical whether the path is a direct handle or a server. The namespace
//! is precisely what hides the difference. See `docs/rationale/deferred-decisions.md`
//! (`TODO(session-metadata-server)`).
//!
//! ## No session, no answer
//!
//! A process spawned outside any session has nothing bound at `/session/user`, and this
//! says so and exits non-zero. It does not invent a default: reporting `root`, or an empty
//! name, would be a fabricated fact — the same reason `date` refuses to print 1970 when
//! the clock is unset.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use coreutils::args::parse;
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::handle::RIGHT_MAP_READ;
use libkern::syscall::{SYS_HANDLE_CLOSE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, syscall1, syscall4};
use libkern::{exit, kprint};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: the TSM1 encoder allocates.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where a session publishes its user. See the module docs for why this is a namespace
/// path rather than a syscall or a service call.
const SESSION_USER: &[u8] = b"/session/user";

/// Longest name accepted from the binding. A session's user name is short; a bound object
/// is at least a page, so the read must be bounded by something other than the mapping.
const NAME_MAX: usize = 256;

const HELP: &[u8] = b"usage: whoami\n\
    \n\
    Report the user this session was opened for, read from /session/user.\n\
    Fails if there is no session (nothing is bound there).\n\
    Emits Table<{user: String}> on stdout.\n\
    \n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"whoami (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[]) {
        Ok(a) => a,
        Err(_) => stage.die(b"whoami: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }
    if !args.operands.is_empty() {
        stage.die(b"whoami: takes no operands (try --help)\n", EXIT_USAGE);
    }

    let user = match read_session_user(&stage) {
        Some(u) if !u.is_empty() => u,
        // Absent, or bound but empty. Both mean the same thing — nothing authoritative
        // said who this is — and neither is worth guessing about.
        _ => stage.die(b"whoami: no session identity (/session/user is not bound)\n", EXIT_FAILURE),
    };

    match stage.streams.stdout {
        Some(h) => emit(&stage, h, &user),
        None => {
            let mut line = user.clone();
            line.push('\n');
            kprint(line.as_bytes());
        }
    }
    exit(EXIT_OK)
}

/// Resolve `/session/user` and read the name out of it, or `None` if it is not bound.
fn read_session_user(stage: &Stage) -> Option<String> {
    let (st, handle) = coreutils::fs::lookup_wait(stage.namespace, SESSION_USER, RIGHT_MAP_READ);
    if st != 0 || handle == 0 {
        return None;
    }
    // SAFETY: mapping a readable object this process just resolved, at a
    // kernel-chosen address, for one page — enough for any name we accept.
    let base = unsafe { syscall4(SYS_MEMORY_MAP, handle, 0, 4096, RIGHT_MAP_READ) };
    if base < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
        return None;
    }
    // SAFETY: `base` is a live read-only mapping of at least one page; the read is
    // bounded by `NAME_MAX` and stops at the first NUL or newline.
    let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, NAME_MAX) };
    let end = bytes
        .iter()
        .position(|&b| b == 0 || b == b'\n')
        .unwrap_or(NAME_MAX);
    let name = String::from_utf8_lossy(&bytes[..end]).into_owned();

    // SAFETY: unmapping the mapping made above, then closing our own handle.
    unsafe {
        syscall4(SYS_MEMORY_UNMAP, base as u64, 4096, 0, 0);
        syscall1(SYS_HANDLE_CLOSE, handle);
    }
    Some(name)
}

/// Write the single row as a TSM1 table on the `stdout` stream.
fn emit(stage: &Stage, stdout: u64, user: &str) {
    let schema = Schema::new().field("user", TypeTag::String, TypeModifiers::NONE);

    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        tw.write_row(&[Value::Str(String::from(user))])?;
        tw.finish_with_status(0)
    });
    let flushed = wrote.and_then(|()| tw.into_sink().finish());
    match flushed {
        Ok(()) => {}
        Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"whoami: write failed\n", EXIT_FAILURE),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"whoami: panic\n");
    exit(EXIT_FAILURE)
}
