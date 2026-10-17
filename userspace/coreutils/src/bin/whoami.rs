//! `whoami` — report the session's user.
//!
//! ## Where identity lives
//!
//! Nitrox has **no kernel user identity**; authority is held in handles. Identity is a
//! *session* concept: `session-mgr` authenticates a login and **constructs the session's
//! namespace**, in which `/session/user` is a readable object holding the name. Asking the
//! namespace is answer-by-construction, as for `/home`; asking a service would be closer to
//! ambient lookup than to capabilities.
//!
//! `/session/user` is a **direct-handle bind** of a memory object, a snapshot: correct because a
//! session's user is immutable for its lifetime. A userspace server behind `/session/*` would
//! answer a resolve with `OBJECT_KIND_MEMOBJ` as well, so `lookup + map + read` stays the same.
//! See `docs/rationale/deferred-decisions.md` (`TODO(session-metadata-server)`).
//!
//! ## No session, no answer
//!
//! A process spawned outside any session has nothing bound at `/session/user`: this says so
//! and exits non-zero, rather than invent a default (`root`, an empty name) — as `date` refuses
//! to print 1970 when the clock is unset.

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

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where a session publishes its user.
const SESSION_USER: &[u8] = b"/session/user";

/// Longest name accepted: a bound object is at least a page, so the read needs a bound of its own.
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
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
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

fn read_session_user(stage: &Stage) -> Option<String> {
    let (st, handle) = libfs::lookup_wait(stage.namespace, SESSION_USER, RIGHT_MAP_READ);
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
    let end = bytes.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(NAME_MAX);
    let name = String::from_utf8_lossy(&bytes[..end]).into_owned();

    // SAFETY: unmapping the mapping made above, then closing our own handle.
    unsafe {
        syscall4(SYS_MEMORY_UNMAP, base as u64, 4096, 0, 0);
        syscall1(SYS_HANDLE_CLOSE, handle);
    }
    Some(name)
}

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
