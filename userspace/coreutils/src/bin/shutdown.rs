//! `shutdown` — stop the machine, or restart it.
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
use libkern::Rights;
use libkern::error::KError;
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Channel, Msg, Only, block_on, exit};
use librsproto::services::{OP_SERVICES_SHUTDOWN, shutdown_body};
use libstream::diag::Level;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where the `power` grant binds `service-mgr`'s power endpoint.
const POWER: &str = "/dev/power";

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
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);
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

fn say_at(stage: &Stage, level: Level, text: &str) {
    if stage.streams.stderr.is_some() {
        stage.diag_at(level, text.as_bytes());
    }
    Line::new().untrusted(text.trim_end_matches('\n').as_bytes()).end();
}

/// Ask `service-mgr` to shut down, or reboot: a power session on `/dev/power`, one `Shutdown`, and
/// its answer, which comes as soon as the shutdown has begun.
fn shut_down(stage: &Stage, reboot: bool) -> i32 {
    let rights = Rights::from_bits(librsproto::session::DIR_SESSION_RIGHTS);
    let session = match block_on(stage.namespace.lookup::<Channel, Only>(POWER, rights)) {
        Ok(session) => session,
        Err(e) if e.code() == KError::NotFound => {
            say(stage, "shutdown: this needs the power grant: `with power shutdown`\n");
            return EXIT_FAILURE;
        }
        Err(e) => {
            say(stage, &format!("shutdown: service-mgr opened no power session ({:?})\n", e.code()));
            return EXIT_FAILURE;
        }
    };
    let mut msg = Msg::new();
    let reply = librsproto::session::exchange(&session, &mut msg, 1, OP_SERVICES_SHUTDOWN, &shutdown_body(reboot));
    drop(session);
    let refused = match reply {
        Err(_) => Some(String::from("lost service-mgr while asking")),
        Ok(_) => match librsproto::decode(msg.payload()) {
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
    log::write(b"shutdown: panic\n");
    exit(EXIT_FAILURE)
}
