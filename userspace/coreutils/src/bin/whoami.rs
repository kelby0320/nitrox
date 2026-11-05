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
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Port, Stage};
use libkern::Rights;
use libkern::abi::IPC_PAYLOAD_SIZE;
use libos::boot::Start;
use libos::log;
use libos::{File, MapRead, block_on, exit};
use libstream::channel::ChannelSink;
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where a session publishes its user.
const SESSION_USER: &str = "/session/user";

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
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);

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

    match stage.stdout() {
        Some(port) => emit(&stage, port, &user),
        None => {
            let mut line = user.clone();
            line.push('\n');
            log::write(line.as_bytes());
        }
    }
    exit(EXIT_OK)
}

fn read_session_user(stage: &Stage) -> Option<String> {
    let file = block_on(stage.namespace.lookup::<File, MapRead>(SESSION_USER, Rights::MAP_READ)).ok()?;
    // One page at a kernel-chosen address: enough for any name we accept.
    let mapping = file.map(4096).ok()?;
    let mut bytes = [0u8; NAME_MAX];
    mapping.read_into(0, &mut bytes).ok()?;
    let end = bytes.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(NAME_MAX);
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

fn emit(stage: &Stage, stdout: Port<'_>, user: &str) {
    let schema = Schema::new().field("user", TypeTag::String, TypeModifiers::NONE);

    let mut tw = TableWriter::new(ChannelSink::new(stdout, IPC_PAYLOAD_SIZE));
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
    log::write(b"whoami: panic\n");
    exit(EXIT_FAILURE)
}
