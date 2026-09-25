//! `disk` — the machine's disks: what each holds, and mounting and unmounting what it can serve.
//!
//! ```text
//! disk --list                        # every block device, its filesystem, where it is mounted
//! disk --mount /dev/blk/1 [LABEL]    # mount it writable at /storage/<label>
//! disk --unmount LABEL               # unmount it, leaving the filesystem clean
//! ```
//!
//! ## Two endpoints, two authorities
//!
//! **`--list` reads what every session can**: `/dev/storage/all.tsm`, the storage service's
//! table, through the session endpoint every login binds (administration Part C.6). It needs no
//! grant, and it is the same table a pipeline can `open` and `filter`.
//!
//! **`--mount` and `--unmount` need the `storage` grant.** They speak `Storage`
//! (`rsproto-storage-ops.md`) on `/dev/storage/admin`, which the view broker binds only into a
//! view whose profile grants `storage`. Without it that path reaches the session endpoint instead,
//! at the base `/info`, where `admin` is no table and nothing answers: `disk` says so and names
//! `with`. **The service does the refusing that matters** — a device in use, one holding nothing it
//! can serve, a label taken, a file still open — and its reason is printed as it gave it.
//!
//! ## A typed result, and a line on the console
//!
//! Each verb writes a table on stdout, text when there is none, like every `--list` here. And
//! each says what happened on the console too, since a terminal on a release image renders
//! nothing a gate can read.

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
use librsproto::storage::{OP_STORAGE_MOUNT, OP_STORAGE_UNMOUNT, build_mount};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::wire::Table;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: the TSM1 encoder and decoder allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The table of what each disk holds, through the session endpoint.
const TABLE: &[u8] = b"/dev/storage/all.tsm";
/// Where the `storage` grant binds the admin endpoint.
const ADMIN: &[u8] = b"/dev/storage/admin";

const FLAGS: [Flag; 3] = [
    Flag::long_only("list", "every block device, its filesystem, and where it is mounted"),
    Flag::long_only("mount", "mount DEVICE writable at /storage/<label> (needs the storage grant)"),
    Flag::long_only("unmount", "unmount the filesystem called LABEL (needs the storage grant)"),
];

const HELP: &[u8] = b"usage: disk --list | --mount DEVICE [LABEL] | --unmount LABEL\n\
    \n\
    --list              every block device, what it holds, where it is mounted, and whether\n\
    \x20                   it was left clean, as a table\n\
    --mount DEVICE [L]  mount DEVICE (/dev/blk/N or blk-N) writable at /storage/L, or under\n\
    \x20                   the label the storage service chooses\n\
    --unmount LABEL     write back every file, flush the drive, and unmount; refused while a\n\
    \x20                   file on it is open\n\
    \n\
    Mounting and unmounting need the storage grant: run them with `with admin`.\n\
    \n\
    \x20     --help    show this help and exit\n\
    \x20     --version show version information and exit\n";

const VERSION: &[u8] = b"disk (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);
    let args = match parse(&stage.argv, &FLAGS) {
        Ok(a) => a,
        Err(_) => stage.die(b"disk: unrecognized option (try --help)\n", EXIT_USAGE),
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
    let verbs = [args.has("list"), args.has("mount"), args.has("unmount")];
    let code = match (verbs, ops.as_slice()) {
        ([true, false, false], []) => list(&stage),
        ([false, true, false], [device]) => mount(&stage, device, ""),
        ([false, true, false], [device, label]) => mount(&stage, device, label),
        ([false, false, true], [label]) => unmount(&stage, label),
        _ => stage.die(b"disk: one of --list, --mount DEVICE [LABEL] or --unmount LABEL (try --help)\n", EXIT_USAGE),
    };
    exit(code)
}

/// Say `text` where the person reads it and on the console, where a gate does.
///
/// **Once on the console, and escaped there.** Without a `stderr`, `diag` is the console already.
/// The console's copy goes through [`Line::untrusted`], since it carries a label someone typed and
/// the service's reason.
fn say(stage: &Stage, text: &str) {
    if stage.streams.stderr.is_some() {
        stage.diag(text.as_bytes());
    }
    Line::new().untrusted(text.trim_end_matches('\n').as_bytes()).end();
}

/// `disk --list`: the storage service's table, as it gave it.
fn list(stage: &Stage) -> i64 {
    let table = match libfs::read_file(stage.namespace, TABLE).ok().and_then(|b| Table::decode(&b).ok()) {
        Some(t) => t,
        None => {
            say(stage, "disk: no /dev/storage/all.tsm in this namespace -- is the storage service running?\n");
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
    Line::new().s(b"disk: listed ").u(rows as u64).s(b" device(s) (").s(sink).s(b")").end();
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
        Err(_) => stage.die(b"disk: write failed\n", EXIT_FAILURE),
    }
}

/// A device operand as the tables name it: `blk-<n>`, from `/dev/blk/<n>` or as given.
fn device_name(operand: &str) -> Option<String> {
    let n = operand.strip_prefix("/dev/blk/").or_else(|| operand.strip_prefix("blk-"))?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| format!("blk-{n}"))
}

/// Why a `Storage` request failed.
enum Failure {
    /// This namespace has no admin endpoint: the `storage` grant was not given.
    NoGrant,
    /// The admin endpoint is here and opened no session, for this reason: every admin session in
    /// use is `WouldBlock`.
    Unopened(KError),
    /// The session broke, or its reply did not parse.
    Transport,
    /// The service refused, with its reason.
    Refused(String),
}

/// Open an admin session on `/dev/storage/admin`, send one request, and return the reply's body.
///
/// **No grant, told apart from a busy one.** Without the grant the path resolves through the
/// session endpoint at the base `/info`, where `info/admin` is no table, or through nothing at all:
/// `NotFound` either way. That is the one answer that means "not in a view with `storage`". Any
/// other is the service's, and is printed as such: telling a person with the grant to go and get
/// it is worse than saying nothing.
fn ask(stage: &Stage, op: u16, body: &[u8]) -> Result<Vec<u8>, Failure> {
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
            .unwrap_or_else(|| String::from("the storage service refused, and did not say why"));
        return Err(Failure::Refused(why));
    }
    Ok(msg.body.to_vec())
}

/// Report a failed request about `what`, and return the exit status for it.
fn failed(stage: &Stage, verb: &str, what: &str, f: Failure) -> i64 {
    let text = match f {
        Failure::NoGrant => format!(
            "disk: cannot {verb} here -- mounting and unmounting need the storage grant: `with admin disk --{verb} ...`\n"
        ),
        Failure::Unopened(e) => format!("disk: {what} not {verb}ed: the storage service opened no admin session ({e:?})\n"),
        Failure::Transport => format!("disk: lost the storage service while asking to {verb} {what}\n"),
        Failure::Refused(why) => format!("disk: {what} not {verb}ed: {why}\n"),
    };
    say(stage, &text);
    EXIT_FAILURE
}

/// `disk --mount DEVICE [LABEL]`.
fn mount(stage: &Stage, operand: &str, label: &str) -> i64 {
    let Some(device) = device_name(operand) else {
        say(stage, "disk: a device is /dev/blk/N or blk-N\n");
        return EXIT_USAGE;
    };
    let mut body = alloc::vec![0u8; librsproto::storage::MOUNT_PREFIX_LEN + device.len() + label.len()];
    let Some(n) = build_mount(&mut body, device.as_bytes(), label.as_bytes()) else {
        say(stage, "disk: that label is too long\n");
        return EXIT_USAGE;
    };
    let reply = match ask(stage, OP_STORAGE_MOUNT, &body[..n]) {
        Ok(r) => r,
        Err(f) => return failed(stage, "mount", &device, f),
    };
    let label = String::from_utf8_lossy(&reply).into_owned();
    let at = format!("/storage/{label}");
    match stage.streams.stdout {
        Some(h) => {
            let schema = Schema::new()
                .field("device", TypeTag::String, TypeModifiers::NONE)
                .field("label", TypeTag::String, TypeModifiers::NONE)
                .field("mounted", TypeTag::String, TypeModifiers::NONE);
            let row = alloc::vec![Value::Str(device.clone()), Value::Str(label.clone()), Value::Str(at.clone())];
            write_table(stage, h, &schema, &[row]);
        }
        None => stage.diag(format!("{device} mounted at {at}\n").as_bytes()),
    }
    Line::new().s(b"disk: mounted ").s(device.as_bytes()).s(b" at ").untrusted(at.as_bytes()).end();
    EXIT_OK
}

/// `disk --unmount LABEL`.
fn unmount(stage: &Stage, label: &str) -> i64 {
    if let Err(f) = ask(stage, OP_STORAGE_UNMOUNT, label.as_bytes()) {
        return failed(stage, "unmount", label, f);
    }
    match stage.streams.stdout {
        Some(h) => {
            let schema = Schema::new()
                .field("label", TypeTag::String, TypeModifiers::NONE)
                .field("unmounted", TypeTag::Bool, TypeModifiers::NONE);
            write_table(stage, h, &schema, &[alloc::vec![Value::Str(String::from(label)), Value::Bool(true)]]);
        }
        None => stage.diag(format!("{label} unmounted, and left clean\n").as_bytes()),
    }
    Line::new().s(b"disk: unmounted ").untrusted(label.as_bytes()).end();
    EXIT_OK
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"disk: panic\n");
    exit(EXIT_FAILURE)
}
