//! `shutdown` — stop the machine, or restart it (administration Part E.4).
//!
//! ```text
//! with power shutdown            # halt: "It is now safe to turn off your computer."
//! with power shutdown --reboot   # restart
//! ```
//!
//! **It asks, and `service-mgr` does the rest.** `shutdown` sends one `Shutdown` on a power session
//! opened at `/dev/power` (`rsproto-services-ops.md`), and `service-mgr` answers as soon as the
//! shutdown has begun. Then it asks the sessions to end, this one included, stops the services
//! last first, and tells `init` to unmount its filesystems and stop the machine
//! (`service-manager.md` § *Shutdown*).
//!
//! **It needs the `power` grant**, which binds `/dev/power`. The seeded policy gives that to
//! everyone, for `shutdown` alone and with no password, as a desktop's power button would. Without
//! it `/dev/power` is not there, and `shutdown` says to use `with`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::{IPC_HEADER_SIZE, IPC_MSG_SIZE};
use libkern::debug::Line;
use libkern::error::KError;
use libkern::{exit, kprint};
use librsproto::services::{OP_SERVICES_SHUTDOWN, shutdown_body};
use libstream::diag::Level;

/// `alloc` backing: the request buffer and the messages allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where the `power` grant binds `service-mgr`'s power endpoint.
const POWER: &[u8] = b"/dev/power";

const FLAGS: [Flag; 1] = [Flag::long_only("reboot", "restart the machine rather than halting it")];

const HELP: &[u8] = b"usage: shutdown [--reboot]\n\
    \n\
    Ask every session and service to stop, write back and unmount every filesystem, and stop the\n\
    machine: with nothing, halt, saying when it is safe to turn it off; with --reboot, restart.\n\
    \n\
    It needs the power grant: run it as `with power shutdown`.\n\
    \n\
    \x20     --reboot  restart the machine rather than halting it\n\
    \x20     --help    show this help and exit\n\
    \x20     --version show version information and exit\n";

const VERSION: &[u8] = b"shutdown (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);
    let args = match parse(&stage.argv, &FLAGS) {
        Ok(a) => a,
        Err(_) => stage.die(b"shutdown: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    if !args.operands.is_empty() {
        stage.die(b"shutdown: takes no operands (try --help)\n", EXIT_USAGE);
    }
    exit(shut_down(&stage, args.has("reboot")))
}

/// Say `text` where the person reads it and on the console, where a gate does — and where it is
/// seen on a machine whose sessions are about to go.
fn say(stage: &Stage, text: &str) {
    say_at(stage, Level::Error, text);
}

/// [`say`], at `level`.
fn say_at(stage: &Stage, level: Level, text: &str) {
    if stage.streams.stderr.is_some() {
        stage.diag_at(level, text.as_bytes());
    }
    Line::new().untrusted(text.trim_end_matches('\n').as_bytes()).end();
}

/// Ask `service-mgr` to shut down, or reboot: a power session on `/dev/power`, one `Shutdown`, and
/// its answer, which comes as soon as the shutdown has begun.
fn shut_down(stage: &Stage, reboot: bool) -> i64 {
    let (st, session) = libfs::lookup_wait(stage.namespace, POWER, librsproto::session::DIR_SESSION_RIGHTS);
    if st == KError::NotFound.as_i32() {
        say(stage, "shutdown: this needs the power grant: `with power shutdown`\n");
        return EXIT_FAILURE;
    }
    if st != 0 || session == 0 {
        say(stage, &format!("shutdown: service-mgr opened no power session ({:?})\n", KError::from_i32(st)));
        return EXIT_FAILURE;
    }
    let mut buf = alloc::vec![0u8; IPC_MSG_SIZE];
    let reply = librsproto::session::round_trip(session, &mut buf, 1, OP_SERVICES_SHUTDOWN, &shutdown_body(reboot));
    // SAFETY: closing the power session this call opened.
    unsafe { libkern::syscall::syscall1(libkern::syscall::SYS_HANDLE_CLOSE, session) };
    let refused = match reply {
        Err(_) => Some(String::from("lost service-mgr while asking")),
        Ok(len) => match librsproto::decode(&buf[IPC_HEADER_SIZE..IPC_HEADER_SIZE + len]) {
            Ok(m) if m.op == OP_SERVICES_SHUTDOWN && m.flags & librsproto::RS_FLAG_ERROR == 0 => None,
            Ok(m) if m.op == OP_SERVICES_SHUTDOWN => Some(
                librsproto::error::parse_error(m.body)
                    .map(|e| String::from_utf8_lossy(e.msg).into_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| String::from("service-mgr refused, and did not say why")),
            ),
            _ => Some(String::from("service-mgr's answer did not read")),
        },
    };
    if let Some(why) = refused {
        say(stage, &format!("shutdown: not shutting down: {why}\n"));
        return EXIT_FAILURE;
    }
    say_at(stage, Level::Notice, if reboot { "shutdown: restarting\n" } else { "shutdown: shutting down\n" });
    EXIT_OK
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"shutdown: panic\n");
    exit(EXIT_FAILURE)
}
