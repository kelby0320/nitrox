//! `log` — read a log back: the service log (administration Part E.6), or the kernel's (Phase 6
//! Part B.7). **The system's log viewer**, a source an option: `--kernel` today, and `--service`
//! and `--application` when what tells them apart in the service log is designed.
//!
//! ```text
//! with admin log                              # every record the logging service still holds
//! with admin log view-broker                  # one principal's: the view broker's, its audit too
//! with admin log --kernel | last 20           # the kernel's log, its last twenty lines
//! with admin log --kernel | filter message ~= /usb/
//! ```
//!
//! **The service log needs the `logs` grant**, which binds the logging service's read endpoint at
//! `/dev/logs`; a resolve there opens a read session, on which `Log::Read` answers from the
//! service's ring (`rsproto-log-ops.md`). **The kernel's needs the `kernel-log` grant**, which binds
//! a snapshot of the kernel's `/dev/log` at the same path, taken as the view is made. Without a
//! grant its path is not there, and `log` says which grant to ask `with` for.
//!
//! **One table shape for every source**, so a pipeline reads any of them alike:
//! `Table<{sequence, time, principal, tier, level, message}>`, oldest first. A kernel row is a line
//! of the kernel's log: its `principal` is `kernel`, its `sequence` its line's place in the log, and
//! its `time`, `tier` and `level` null, which the kernel's lines do not carry. A service row:
//! - `sequence` — the logging service's count, which orders the records: `time` can step.
//! - `time` — the wall clock when the service took the record, in Unix epoch seconds as `list`'s
//!   `modified` is; null for one taken while the clock was not set.
//! - `principal` — whose channel it came on, which the emitter cannot choose; a named source
//!   follows a `.`, as the serial console shows it (`heartbeat.worker`). `PRINCIPAL` keeps one
//!   principal's records, its named sources' included.
//! - `tier`, `level`, and `message` — the last two the emitter's claim.
//!
//! **What the ring no longer holds is said**, on stderr: it keeps the most recent records, so a
//! machine up long enough has dropped its first — and a ring busy enough can drop some while `log`
//! reads it, which is said too (PR #344 review).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::{IPC_HEADER_SIZE, IPC_MSG_SIZE, IPC_PAYLOAD_SIZE};
use libkern::error::KError;
use libkern::{exit, kprint};
use librsproto::log::{OP_LOG_READ, ReadRecord, dropped, level_name, parse_read_reply, read_request, tier_name};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: the replies, the rows and the TSM1 encoder allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where the `logs` grant binds the logging service's read endpoint.
const LOGS: &[u8] = b"/dev/logs";
/// Where the `kernel-log` grant binds a snapshot of the kernel's log (Phase 6 Part B.7).
const KERNEL_LOG: &[u8] = b"/dev/log";

/// The options: one source today besides the service log.
const FLAGS: &[Flag] = &[Flag::long_only("kernel", "read the kernel's log instead of the service log")];

const HELP: &[u8] = b"usage: log [PRINCIPAL]\n\
    \x20      log --kernel\n\
    \n\
    Read a log back, oldest first. With no option, the service log: every record the logging\n\
    service still holds, or PRINCIPAL's alone. With --kernel, the kernel's log, a row a line.\n\
    Emits Table<{sequence: Int, time: Int?, principal: String, tier: String?, level: String?,\n\
    message: String}> on stdout. `time` is Unix epoch seconds, and null for a record taken while\n\
    the clock was not set. A kernel row's principal is `kernel`, its sequence its line's place,\n\
    and its time, tier and level null.\n\
    \n\
    The service log needs the logs grant and the kernel's the kernel-log grant: run it as\n\
    `with admin log`.\n\
    \n\
    \x20     --kernel  read the kernel's log instead of the service log\n\
    \x20     --help    show this help and exit\n\
    \x20     --version show version information and exit\n";

const VERSION: &[u8] = b"log (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);
    let args = match parse(&stage.argv, FLAGS) {
        Ok(a) => a,
        Err(_) => stage.die(b"log: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    if args.has("kernel") {
        if !args.operands.is_empty() {
            stage.die(b"log: --kernel takes no PRINCIPAL (try --help)\n", EXIT_USAGE);
        }
        kernel(&stage);
    }
    let principal = match args.operands.as_slice() {
        [] => None,
        [p] => Some(p.as_str()),
        _ => stage.die(b"log: takes at most one PRINCIPAL (try --help)\n", EXIT_USAGE),
    };
    let (st, session) = libfs::lookup_wait(stage.namespace, LOGS, librsproto::session::DIR_SESSION_RIGHTS);
    if st == KError::NotFound.as_i32() {
        stage.die(b"log: cannot read the log here -- that needs the logs grant: `with admin log ...`\n", EXIT_FAILURE);
    }
    if st == KError::WouldBlock.as_i32() {
        stage.die(b"log: the log is being read twice already; try again\n", EXIT_FAILURE);
    }
    if st != 0 || session == 0 {
        let why = format!("log: the logging service opened no read session ({:?})\n", KError::from_i32(st));
        stage.die(why.as_bytes(), EXIT_FAILURE);
    }
    let mut out = Out::new(&stage);
    let code = read(&stage, session, principal, &mut out);
    // SAFETY: closing the read session this program opened.
    unsafe { libkern::syscall::syscall1(libkern::syscall::SYS_HANDLE_CLOSE, session) };
    out.finish(&stage);
    exit(code)
}

/// **The kernel's log, a row a line** (Phase 6 Part B.7), from the snapshot the `kernel-log` grant
/// bound, and exit.
fn kernel(stage: &Stage) -> ! {
    let bytes = match libfs::read_file(stage.namespace, KERNEL_LOG) {
        Ok(b) => b,
        Err(libfs::FileError::NotFound) => stage.die(
            b"log: cannot read the kernel's log here -- that needs the kernel-log grant: `with admin log --kernel`\n",
            EXIT_FAILURE,
        ),
        Err(e) => {
            let why = format!("log: the kernel's log did not read ({e:?})\n");
            stage.die(why.as_bytes(), EXIT_FAILURE)
        }
    };
    let mut out = Out::new(stage);
    for (i, line) in coreutils::text::lines(&bytes).enumerate() {
        if !out.kernel_row(i as i64 + 1, line) {
            // Whoever read `log`'s output has stopped reading.
            break;
        }
    }
    out.finish(stage);
    exit(EXIT_OK)
}

/// **Read the ring from the start**, a reply at a time, until a reply holds nothing, writing
/// `principal`'s records (or every one) to `out`. Each reply must begin after the last one ended,
/// or a service answering the same records forever would never let this stop.
fn read(stage: &Stage, session: u64, principal: Option<&str>, out: &mut Out) -> i64 {
    let mut buf = alloc::vec![0u8; IPC_MSG_SIZE];
    let mut after = 0u64;
    let mut request_id = 0u64;
    loop {
        request_id += 1;
        let request = read_request(after, 0);
        let Ok(len) = librsproto::session::round_trip(session, &mut buf, request_id, OP_LOG_READ, &request) else {
            stage.diag(b"log: lost the logging service while reading\n");
            return EXIT_FAILURE;
        };
        let reply = match librsproto::decode(&buf[IPC_HEADER_SIZE..IPC_HEADER_SIZE + len]) {
            Ok(m) if m.op == OP_LOG_READ && m.flags & librsproto::RS_FLAG_ERROR == 0 => parse_read_reply(m.body),
            _ => None,
        };
        let Some(reply) = reply else {
            stage.diag(b"log: the logging service's answer did not read\n");
            return EXIT_FAILURE;
        };
        let lost = dropped(after, reply.oldest);
        if lost != 0 {
            let said = if after == 0 {
                format!("log: the {lost} oldest records are no longer kept\n")
            } else {
                format!("log: {lost} records were dropped from the ring while it was read\n")
            };
            stage.warn(said.as_bytes());
        }
        if reply.count == 0 {
            return EXIT_OK;
        }
        for rec in reply.records() {
            if rec.sequence <= after {
                stage.diag(b"log: the logging service answered records already read\n");
                return EXIT_FAILURE;
            }
            after = rec.sequence;
            if principal.is_none_or(|p| rec.principal == p.as_bytes()) && !out.row(&rec) {
                // Whoever read `log`'s output has stopped reading.
                return EXIT_OK;
            }
        }
    }
}

/// Where the rows go: a table on `stdout`, or lines on the console when there is none.
enum Out {
    Table(TableWriter<ChannelSink<IpcPort>>),
    Console,
}

impl Out {
    fn new(stage: &Stage) -> Out {
        let Some(stdout) = stage.streams.stdout else {
            return Out::Console;
        };
        let nullable = TypeModifiers::NULLABLE;
        // `tier` and `level` nullable since Phase 6 Part B.7: a kernel row has neither.
        let schema = Schema::new()
            .field("sequence", TypeTag::Int, TypeModifiers::NONE)
            .field("time", TypeTag::Int, nullable)
            .field("principal", TypeTag::String, TypeModifiers::NONE)
            .field("tier", TypeTag::String, nullable)
            .field("level", TypeTag::String, nullable)
            .field("message", TypeTag::String, TypeModifiers::NONE);
        let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
        if tw.write_schema(StreamFlags::NONE, &schema).is_err() {
            stage.die(b"log: write failed\n", EXIT_FAILURE);
        }
        Out::Table(tw)
    }

    /// Write one record; `false` once nobody reads what is written.
    fn row(&mut self, rec: &ReadRecord<'_>) -> bool {
        let principal = match rec.source {
            Some(source) => format!("{}.{}", lossy(rec.principal), lossy(source)),
            None => lossy(rec.principal),
        };
        let seconds = rec.time.map(|ns| (ns / 1_000_000_000) as i64);
        match self {
            Out::Table(tw) => {
                let row: Vec<Value> = alloc::vec![
                    Value::Int(rec.sequence as i64),
                    seconds.map_or(Value::Null, Value::Int),
                    Value::Str(principal),
                    Value::Str(String::from(tier_name(rec.tier))),
                    Value::Str(String::from(level_name(rec.level))),
                    Value::Str(lossy(rec.message)),
                ];
                tw.write_row(&row).is_ok()
            }
            Out::Console => {
                let time = seconds.map_or(String::from("-"), |s| format!("{s}"));
                let line = format!(
                    "[{}] {} {}/{} {}: {}\n",
                    rec.sequence,
                    time,
                    tier_name(rec.tier),
                    principal,
                    level_name(rec.level),
                    lossy(rec.message)
                );
                kprint(line.as_bytes());
                true
            }
        }
    }

    /// Write line `sequence` of the kernel's log; `false` once nobody reads what is written.
    fn kernel_row(&mut self, sequence: i64, line: &[u8]) -> bool {
        match self {
            Out::Table(tw) => {
                let row: Vec<Value> = alloc::vec![
                    Value::Int(sequence),
                    Value::Null,
                    Value::Str(String::from("kernel")),
                    Value::Null,
                    Value::Null,
                    Value::Str(lossy(line)),
                ];
                tw.write_row(&row).is_ok()
            }
            Out::Console => {
                kprint(format!("[{sequence}] - kernel: {}\n", lossy(line)).as_bytes());
                true
            }
        }
    }

    /// End the table.
    fn finish(self, stage: &Stage) {
        let Out::Table(mut tw) = self else { return };
        let flushed = tw.finish_with_status(0).and_then(|()| tw.into_sink().finish());
        match flushed {
            Ok(()) | Err(libstream::wire::WireError::PeerClosed) => {}
            Err(_) => stage.diag(b"log: write failed\n"),
        }
    }
}

/// Bytes as text, a byte that is not UTF-8 shown as the replacement character.
fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"log: panic\n");
    exit(EXIT_FAILURE)
}
