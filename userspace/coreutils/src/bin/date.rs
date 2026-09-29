//! `date` — report the wall-clock time, or set it.
//!
//! Milestone 2 Part D, and `--set` administration Part E.5. Almost all of this utility is
//! calendar arithmetic, which lives in [`coreutils::time`] and is tested on the host; what
//! remains here is a syscall or two and the choice of what to publish.
//!
//! ```text
//! date                                       # the current time
//! date --unix                                # …as a bare epoch value, for arithmetic
//! with admin date --set 2026-09-28T14:30:00Z # set it, and the machine's hardware clock
//! ```
//!
//! ## Deliberate behaviours
//!
//! - **It emits fields, not a formatted string.** `Table<{unix, year, month, day, hour,
//!   minute, second}>`, one row. A string would force every consumer to parse it back
//!   apart, which is exactly the Unix habit the typed-stream model exists to avoid — the
//!   shell can format it, and a pipeline that wants the year can select the year.
//! - **UTC only, and no `--format`.** There is no timezone database and no locale, so an
//!   offset would be a fiction and a format string would imply a flexibility that has no
//!   backing. Both are additions to make when there is something real behind them.
//! - **An unset clock is an error, not a zero.** `CLOCK_REALTIME` reports `Unsupported`
//!   on a machine whose RTC could not be read rather than inventing an epoch, and this
//!   passes that through instead of printing 1970 as though it were true.
//! - **`--set` takes one form, in UTC**: `YYYY-MM-DDTHH:MM:SSZ` ([`coreutils::time::parse_utc`]),
//!   from 2000 to 2099, which is what the hardware clock holds. It needs the `clock` grant — the
//!   kernel's `SYSTEM_CLOCK`, given at spawn — and then reports the time as `date` does, so what
//!   it prints is the clock read after the set. A hardware clock that did not take the time is
//!   said, since the time then lasts until the next boot.

#![no_std]
#![no_main]

extern crate alloc;

use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use coreutils::time::{civil_from_unix, format_civil, parse_utc};
use libkern::abi::{CLOCK_REALTIME, CLOCK_SET_THIS_BOOT, IPC_PAYLOAD_SIZE};
use libkern::error::KError;
use libkern::syscall::{SYS_CLOCK_READ, SYS_CLOCK_SET, syscall2};
use libkern::{exit, kprint};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: the TSM1 encoder allocates.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Out-param for `sys_clock_read`.
static mut CLOCK_BUF: u64 = 0;

const UNIX: Flag = Flag::new("unix", 'u', "report the bare epoch value only");
const SET: Flag = Flag::long_only("set", "set the clock to TIME, in UTC (needs the clock grant)");

const HELP: &[u8] = b"usage: date [--unix]\n\
    \x20      date --set TIME [--unix]\n\
    \n\
    Report the current wall-clock time (UTC).\n\
    Emits Table<{unix: Int, year: Int, month: Int, day: Int,\n\
                 hour: Int, minute: Int, second: Int}> on stdout.\n\
    \n\
    With --set, first set the clock, and the machine's hardware clock, to TIME: UTC, as\n\
    2026-09-28T14:30:00Z, from 2000 to 2099. That needs the clock grant:\n\
    `with admin date --set TIME`.\n\
    \n\
      -u, --unix    report the bare epoch value only\n\
          --set     set the clock to TIME first (needs the clock grant)\n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"date (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[UNIX, SET]) {
        Ok(a) => a,
        Err(_) => stage.die(b"date: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }
    if args.has("set") {
        let [time] = args.operands.as_slice() else {
            let why = b"date: --set takes one TIME, as 2026-09-28T14:30:00Z (try --help)\n";
            stage.die(why, EXIT_USAGE);
        };
        set(&stage, time);
    } else if !args.operands.is_empty() {
        stage.die(b"date: takes no operands (try --help)\n", EXIT_USAGE);
    }

    // SAFETY: `CLOCK_BUF` is a valid writable `u64` out-param; this process is
    // single-threaded.
    let r = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut CLOCK_BUF) as u64) };
    if r != 0 {
        // The clock is unset (no readable RTC), which is a different thing from "the time
        // is zero" — say so rather than reporting 1970 as fact.
        stage.die(b"date: the wall clock is not set on this machine\n", EXIT_FAILURE);
    }
    // SAFETY: written by the syscall above.
    let nanos = unsafe { (&raw const CLOCK_BUF).read() };
    let civil = civil_from_unix(nanos);
    let unix_secs = (nanos / 1_000_000_000) as i64;

    match stage.streams.stdout {
        Some(h) => emit(&stage, h, unix_secs, &civil, args.has("unix")),
        None => {
            let mut line = if args.has("unix") {
                let mut s = alloc::string::String::new();
                push_i64(&mut s, unix_secs);
                s
            } else {
                format_civil(&civil)
            };
            line.push('\n');
            kprint(line.as_bytes());
        }
    }
    exit(EXIT_OK)
}

/// **Set the clock to `time`** (administration Part E.5), or die saying why not. Returns once the
/// clock is set, whether or not the hardware clock took it — which is said when it did not.
fn set(stage: &Stage, time: &str) {
    let Some(secs) = parse_utc(time) else {
        stage.die(b"date: TIME is UTC, as 2026-09-28T14:30:00Z\n", EXIT_USAGE);
    };
    // `parse_utc` reads four digits of year, so a time before 1970 is the only negative, and the
    // kernel refuses it with everything else outside 2000-2099.
    let ns = u64::try_from(secs).unwrap_or(0).saturating_mul(1_000_000_000);
    // SAFETY: no pointers; the kernel checks the clock, the time and `SYSTEM_CLOCK`.
    let r = unsafe { syscall2(SYS_CLOCK_SET, CLOCK_REALTIME, ns) };
    if r == KError::NoAccess.as_i32() as i64 {
        stage.die(
            b"date: cannot set the clock here -- that needs the clock grant: `with admin date --set ...`\n",
            EXIT_FAILURE,
        );
    }
    if r == KError::InvalidArgument.as_i32() as i64 {
        stage.die(b"date: the clock can be set to a time from 2000 to 2099\n", EXIT_FAILURE);
    }
    if r < 0 {
        stage.die(b"date: the clock was not set\n", EXIT_FAILURE);
    }
    if r as u64 == CLOCK_SET_THIS_BOOT {
        stage.diag(
            b"date: the clock is set, but the hardware clock would not take it: it lasts until the \
              next boot\n",
        );
    }
}

/// Write the single row as a TSM1 table on the `stdout` stream.
///
/// `--unix` narrows the schema to the one field rather than emitting all of them and
/// letting the consumer pick: a stage that asked for the epoch value should not have to
/// know the calendar fields exist.
fn emit(stage: &Stage, stdout: u64, unix_secs: i64, c: &coreutils::time::Civil, unix_only: bool) {
    let schema = if unix_only {
        Schema::new().field("unix", TypeTag::Int, TypeModifiers::NONE)
    } else {
        Schema::new()
            .field("unix", TypeTag::Int, TypeModifiers::NONE)
            .field("year", TypeTag::Int, TypeModifiers::NONE)
            .field("month", TypeTag::Int, TypeModifiers::NONE)
            .field("day", TypeTag::Int, TypeModifiers::NONE)
            .field("hour", TypeTag::Int, TypeModifiers::NONE)
            .field("minute", TypeTag::Int, TypeModifiers::NONE)
            .field("second", TypeTag::Int, TypeModifiers::NONE)
    };

    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        if unix_only {
            tw.write_row(&[Value::Int(unix_secs)])?;
        } else {
            tw.write_row(&[
                Value::Int(unix_secs),
                Value::Int(c.year),
                Value::Int(i64::from(c.month)),
                Value::Int(i64::from(c.day)),
                Value::Int(i64::from(c.hour)),
                Value::Int(i64::from(c.minute)),
                Value::Int(i64::from(c.second)),
            ])?;
        }
        tw.finish_with_status(0)
    });
    let flushed = wrote.and_then(|()| tw.into_sink().finish());
    match flushed {
        Ok(()) => {}
        Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"date: write failed\n", EXIT_FAILURE),
    }
}

fn push_i64(out: &mut alloc::string::String, v: i64) {
    if v < 0 {
        out.push('-');
    }
    let mut digits = [0u8; 20];
    let mut n = 0;
    let mut v = v.unsigned_abs();
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(digits[i] as char);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"date: panic\n");
    exit(EXIT_FAILURE)
}
