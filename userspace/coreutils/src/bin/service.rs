//! `service` — the system's services: what each is doing, and starting, stopping and restarting
//! them (administration Part E.2).
//!
//! ```text
//! service --list               # every declared service, its state, and how often it restarted
//! service --start NAME         # start a service that is not running
//! service --stop NAME          # ask a running service to stop
//! service --restart NAME       # stop it and start it again
//! ```
//!
//! ## Two endpoints, two authorities
//!
//! **`--list` reads what every session can**: `/dev/services/all.tsm`, `service-mgr`'s table,
//! through the session endpoint every login binds. It needs no grant, and it is the same table a
//! pipeline can `open` and `filter`.
//!
//! **`--start`, `--stop` and `--restart` need the `services` grant.** They speak `Services`
//! (`rsproto-services-ops.md`) on `/dev/services/admin`, which the view broker binds only into a
//! view whose profile grants `services`. Without it that path reaches the session endpoint, where
//! `admin` is no table and nothing answers: `service` says so and names `with`. **`service-mgr`
//! does the refusing that matters** — an essential service, a name nothing declares, a stop not
//! honoured — and its reason is printed as it gave it.
//!
//! Each is answered once it has happened: a stop once the service has exited, a start once it is
//! up. `service` waits for that answer.
//!
//! ## A typed result, and a line on the console
//!
//! Each verb writes a table on stdout, text when there is none, like `disk`. And each says what
//! happened on the console too, since a terminal on a release image renders nothing a gate can
//! read.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::{IPC_HEADER_SIZE, IPC_MSG_SIZE, IPC_PAYLOAD_SIZE};
use libkern::debug::Line;
use libkern::error::KError;
use libkern::{exit, kprint};
use librsproto::services::{OP_SERVICES_RESTART, OP_SERVICES_START, OP_SERVICES_STOP};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::wire::Table;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: the TSM1 encoder and decoder allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The table of every service, through the session endpoint.
const TABLE: &[u8] = b"/dev/services/all.tsm";
/// Where the `services` grant binds the admin endpoint.
const ADMIN: &[u8] = b"/dev/services/admin";

const FLAGS: [Flag; 4] = [
    Flag::long_only("list", "every declared service, its state, and its restarts"),
    Flag::long_only("start", "start the service NAME (needs the services grant)"),
    Flag::long_only("stop", "ask the service NAME to stop (needs the services grant)"),
    Flag::long_only("restart", "stop the service NAME and start it again (needs the services grant)"),
];

const HELP: &[u8] = b"usage: service --list | --start NAME | --stop NAME | --restart NAME\n\
    \n\
    --list            every declared service, whether it is starting, running, stopped or\n\
    \x20                 failed, and how often it has restarted, as a table\n\
    --start NAME      start a service that is not running, and wait until it is up\n\
    --stop NAME       ask a running service to stop, and wait until it has\n\
    --restart NAME    stop it and start it again\n\
    \n\
    Starting and stopping need the services grant: run them with `with admin`. An essential\n\
    service cannot be stopped or restarted.\n\
    \n\
    \x20     --help    show this help and exit\n\
    \x20     --version show version information and exit\n";

const VERSION: &[u8] = b"service (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);
    let args = match parse(&stage.argv, &FLAGS) {
        Ok(a) => a,
        Err(_) => stage.die(b"service: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }
    let ops: Vec<&str> = args.operands.iter().map(|s| s.as_str()).collect();
    let verbs = [args.has("list"), args.has("start"), args.has("stop"), args.has("restart")];
    let code = match (verbs, ops.as_slice()) {
        ([true, false, false, false], []) => list(&stage),
        ([false, true, false, false], [name]) => act(&stage, Verb::Start, name),
        ([false, false, true, false], [name]) => act(&stage, Verb::Stop, name),
        ([false, false, false, true], [name]) => act(&stage, Verb::Restart, name),
        _ => stage.die(b"service: one of --list, --start NAME, --stop NAME or --restart NAME (try --help)\n", EXIT_USAGE),
    };
    exit(code)
}

/// Say `text` where the person reads it and on the console, where a gate does.
///
/// **Once on the console, and escaped there.** Without a `stderr`, `diag` is the console already.
/// The console's copy goes through [`Line::untrusted`], since it carries a name someone typed and
/// `service-mgr`'s reason.
fn say(stage: &Stage, text: &str) {
    if stage.streams.stderr.is_some() {
        stage.diag(text.as_bytes());
    }
    Line::new().untrusted(text.trim_end_matches('\n').as_bytes()).end();
}

/// `service --list`: `service-mgr`'s table, as it gave it.
fn list(stage: &Stage) -> i64 {
    let table = match libfs::read_file(stage.namespace, TABLE).ok().and_then(|b| Table::decode(&b).ok()) {
        Some(t) => t,
        None => {
            say(stage, "service: no /dev/services/all.tsm in this namespace\n");
            return EXIT_FAILURE;
        }
    };
    let rows = table.rows.len();
    let sink: &[u8] = match stage.streams.stdout {
        Some(h) => {
            write_table(stage, h, &table.schema, &table.rows);
            b"table"
        }
        None => {
            let mut text = String::new();
            for row in &table.rows {
                let cells: Vec<String> = row.iter().map(cell).collect();
                text.push_str(&cells.join("  "));
                text.push('\n');
            }
            stage.diag(text.as_bytes());
            b"text"
        }
    };
    Line::new().s(b"service: listed ").u(rows as u64).s(b" service(s) (").s(sink).s(b")").end();
    EXIT_OK
}

/// A value as a table cell's text.
fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::from("-"),
        Value::Bool(b) => String::from(if *b { "yes" } else { "no" }),
        Value::Int(i) => format!("{i}"),
        Value::Str(s) => s.clone(),
        _ => String::from("?"),
    }
}

/// Write `rows` under `schema` on the stdout stream.
fn write_table(stage: &Stage, stdout: u64, schema: &Schema, rows: &[Vec<Value>]) {
    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, schema).and_then(|()| {
        for row in rows {
            tw.write_row(row)?;
        }
        tw.finish_with_status(0)
    });
    match wrote.and_then(|()| tw.into_sink().finish()) {
        Ok(()) | Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"service: write failed\n", EXIT_FAILURE),
    }
}

/// A request, and the words it is said in.
#[derive(Copy, Clone)]
enum Verb {
    Start,
    Stop,
    Restart,
}

impl Verb {
    fn op(self) -> u16 {
        match self {
            Verb::Start => OP_SERVICES_START,
            Verb::Stop => OP_SERVICES_STOP,
            Verb::Restart => OP_SERVICES_RESTART,
        }
    }

    /// The verb, as `--<verb>` and "cannot <verb>".
    fn word(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
        }
    }

    /// What happened, as "<name> <done>".
    fn done(self) -> &'static str {
        match self {
            Verb::Start => "started",
            Verb::Stop => "stopped",
            Verb::Restart => "restarted",
        }
    }

    /// The state the service is in once it has.
    fn state(self) -> &'static str {
        match self {
            Verb::Stop => "stopped",
            Verb::Start | Verb::Restart => "running",
        }
    }
}

/// Why a `Services` request failed.
enum Failure {
    /// This namespace has no admin endpoint: the `services` grant was not given.
    NoGrant,
    /// The admin endpoint is here and opened no session, for this reason: every admin session in
    /// use is `WouldBlock`.
    Unopened(KError),
    /// The session broke, or its reply did not parse.
    Transport,
    /// `service-mgr` refused, with its reason.
    Refused(String),
}

/// Open an admin session on `/dev/services/admin`, send one request, and wait for its answer —
/// which comes once the thing is done.
///
/// **No grant, told apart from a refusal.** Without the grant the path resolves through the
/// session endpoint, where `admin` is no table, or through nothing at all: `NotFound` either way.
/// That is the one answer that means "not in a view with `services`".
fn ask(stage: &Stage, op: u16, body: &[u8]) -> Result<(), Failure> {
    let (st, session) = libfs::lookup_wait(stage.namespace, ADMIN, librsproto::session::DIR_SESSION_RIGHTS);
    if st == KError::NotFound.as_i32() {
        return Err(Failure::NoGrant);
    }
    if st != 0 || session == 0 {
        return Err(Failure::Unopened(KError::from_i32(st)));
    }
    let mut buf = alloc::vec![0u8; IPC_MSG_SIZE];
    let reply = librsproto::session::round_trip(session, &mut buf, 1, op, body);
    // SAFETY: closing the admin session this call opened.
    unsafe { libkern::syscall::syscall1(libkern::syscall::SYS_HANDLE_CLOSE, session) };
    let len = reply.map_err(|_| Failure::Transport)?;
    let msg = librsproto::decode(&buf[IPC_HEADER_SIZE..IPC_HEADER_SIZE + len]).map_err(|_| Failure::Transport)?;
    if msg.op != op {
        return Err(Failure::Transport);
    }
    if msg.flags & librsproto::RS_FLAG_ERROR != 0 {
        let why = librsproto::error::parse_error(msg.body)
            .map(|e| String::from_utf8_lossy(e.msg).into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| String::from("service-mgr refused, and did not say why"));
        return Err(Failure::Refused(why));
    }
    Ok(())
}

/// `service --start`, `--stop` or `--restart NAME`.
fn act(stage: &Stage, verb: Verb, name: &str) -> i64 {
    if let Err(f) = ask(stage, verb.op(), name.as_bytes()) {
        let word = verb.word();
        let text = match f {
            Failure::NoGrant => format!(
                "service: cannot {word} here -- starting and stopping need the services grant: `with admin service --{word} ...`\n"
            ),
            Failure::Unopened(e) => format!("service: {name} not {}: service-mgr opened no admin session ({e:?})\n", verb.done()),
            Failure::Transport => format!("service: lost service-mgr while asking to {word} {name}\n"),
            Failure::Refused(why) => format!("service: {name} not {}: {why}\n", verb.done()),
        };
        say(stage, &text);
        return EXIT_FAILURE;
    }
    match stage.streams.stdout {
        Some(h) => {
            let schema = Schema::new()
                .field("name", TypeTag::String, TypeModifiers::NONE)
                .field("state", TypeTag::String, TypeModifiers::NONE);
            let row = alloc::vec![Value::Str(String::from(name)), Value::Str(String::from(verb.state()))];
            write_table(stage, h, &schema, &[row]);
        }
        None => stage.diag(format!("{name} {}\n", verb.done()).as_bytes()),
    }
    Line::new().s(b"service: ").s(verb.done().as_bytes()).s(b" ").untrusted(name.as_bytes()).end();
    EXIT_OK
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"service: panic\n");
    exit(EXIT_FAILURE)
}
