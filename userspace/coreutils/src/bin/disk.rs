//! `disk` — the machine's disks: what each holds, and mounting and unmounting what it can serve.
//!
//! ```text
//! disk --list                        # every block device, its filesystem, where it is mounted
//! disk --mount /dev/blk/1 [LABEL]    # mount it writable at /storage/<label>
//! disk --unmount LABEL               # unmount it, leaving the filesystem clean
//! disk --eject NAME                  # unmount the stick holding /storage/<name>, to pull it
//! disk --partition DISK [--mbr|--gpt] # one partition spanning a removable disk
//! disk --format DEVICE fat|ext4 [LABEL] [--mbr|--gpt]  # a filesystem; on a disk, a table first
//! ```
//!
//! ## Two endpoints, two authorities
//!
//! **`--list` reads what every session can**: `/dev/storage/all.tsm`, the storage service's
//! table, through the session endpoint every login binds (administration Part C.6). It needs no
//! grant, and it is the same table a pipeline can `open` and `filter`.
//!
//! **So does `--eject`** (Phase 6 Part F): a **media session**, `/dev/storage/media` on the same
//! endpoint, carries `Eject` by the name a stick is mounted under — the table's `mounted` column,
//! not its `label`. The service runs the unmount chain on **every** filesystem it mounted on that
//! removable drive, or on none, since a stick is pulled whole; an internal disk's is refused,
//! naming `--unmount`, which still needs the grant.
//!
//! **`--mount` and `--unmount` need the `storage` grant.** They speak `Storage`
//! (`rsproto-storage-ops.md`) on `/dev/storage/admin`, which the view broker binds only into a
//! view whose profile grants `storage`. Without it that path reaches the session endpoint instead,
//! at the base `/info`, where `admin` is no table and nothing answers: `disk` says so and names
//! `with`. **The service does the refusing that matters** — a device in use, one holding nothing it
//! can serve, a label taken, a file still open — and its reason is printed as it gave it.
//!
//! ## Partitioning and formatting (Phase 6 Part G)
//!
//! **`--partition` and `--format` write the device themselves**, through the raw device the
//! `disks` grant gives (`/dev/blk/<n>`), as `nxinstall` does: the long writes stay out of the
//! storage service, which is one thread. Then they ask the service, on the admin session, to
//! **read the device again** (`Reread`): it mounts what it finds by the rules a device arriving
//! meets, and a disk's table is read again by the kernel, whose new partition arrives as a stick's
//! does. So **both grants are needed**: `with admin`.
//!
//! **Everything is decided before a byte is written** (`coreutils::format`): the disk the machine
//! started from is refused, the target or its disk, by `/dev/devices`' `boot` flag; a table goes
//! only on a removable disk; the default table follows the filesystem, an MBR for FAT and a GPT for
//! ext4, and a GPT for either from 2 TiB; the filesystem fits its partition; the label fits the
//! filesystem; and the kernel's entropy answers, for the IDs. **A mounted device is never
//! written**: it is refused here by name, and the grant would not hold it anyway — unmount it,
//! format it, and the service mounts it again.
//!
//! **For a disk, `disk` waits for the new partition's row** on a watch, bounded: after `--format`
//! mounted, or saying why not; after `--partition`, holding nothing, and named `blk-<n>` for the
//! `--format` that follows.
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
use coreutils::format::{self, Fs, Label, Plan, Scheme};
use libfsserver::BlockWriter;
use libfsserver::disk::PartitionIo;
use libgpt::{mbr, table};
use libkern::abi::{BlockDeviceInfo, IO_OPCODE_FLUSH, IoOp};
use librsproto::storage::{OP_STORAGE_EJECT, OP_STORAGE_MOUNT, OP_STORAGE_REREAD, OP_STORAGE_UNMOUNT, build_mount};
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
/// A media session, through the session endpoint (Phase 6 Part F).
const MEDIA: &[u8] = b"/dev/storage/media";
/// A watch, through the session endpoint (Phase 6 Part F): pinged when the mounts or the devices
/// change.
const WATCH: &[u8] = b"/dev/storage/watch";
/// `/dev/devices`' table: what each block device is, its disk, its driver, and the boot flag.
const DEVICES: &[u8] = b"/dev/devices/all.tsm";
/// **How long a disk's new partition is waited for**, arrived and, after `--format`, mounted: the
/// storage service waits as long for a filesystem server's `Ready`.
const ARRIVAL_NS: u64 = 30_000_000_000;

const FLAGS: [Flag; 8] = [
    Flag::long_only("list", "every block device, its filesystem, and where it is mounted"),
    Flag::long_only("mount", "mount DEVICE writable at /storage/<label> (needs the storage grant)"),
    Flag::long_only("unmount", "unmount the filesystem called LABEL (needs the storage grant)"),
    Flag::long_only("eject", "unmount the removable drive at /storage/NAME, so it can be pulled"),
    Flag::long_only("partition", "one partition spanning the removable DISK (needs with admin)"),
    Flag::long_only("format", "a fat or ext4 filesystem on DEVICE (needs with admin)"),
    Flag::long_only("mbr", "with --partition or --format of a disk: an MBR"),
    Flag::long_only("gpt", "with --partition or --format of a disk: a GPT"),
];

const HELP: &[u8] = b"usage: disk --list | --mount DEVICE [LABEL] | --unmount LABEL | --eject NAME\n\
    \x20      | --partition DISK [--mbr|--gpt] | --format DEVICE fat|ext4 [LABEL] [--mbr|--gpt]\n\
    \n\
    --list              every block device, what it holds, where it is mounted, and whether\n\
    \x20                   it was left clean, as a table\n\
    --mount DEVICE [L]  mount DEVICE (/dev/blk/N or blk-N) writable at /storage/L, or under\n\
    \x20                   the label the storage service chooses\n\
    --unmount LABEL     write back every file, flush the drive, and unmount; refused while a\n\
    \x20                   file on it is open\n\
    --eject NAME        the same for every filesystem on the removable drive holding\n\
    \x20                   /storage/NAME, after which the drive can be pulled out\n\
    --partition DISK    one partition spanning a removable DISK, in an MBR or a GPT, from\n\
    \x20                   2 TiB a GPT; everything on DISK is lost\n\
    --format DEVICE FS  a fat or ext4 filesystem on DEVICE; on a disk, its table first, an MBR\n\
    \x20                   for fat and a GPT for ext4; everything on DEVICE is lost\n\
    \n\
    Mounting and unmounting need the storage grant, and partitioning and formatting the disks\n\
    grant too: run them with `with admin`. Ejecting a removable drive does not.\n\
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
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    let ops: Vec<&str> = args.operands.iter().map(|s| s.as_str()).collect();
    let verbs = [
        args.has("list"),
        args.has("mount"),
        args.has("unmount"),
        args.has("eject"),
        args.has("partition"),
        args.has("format"),
    ];
    let (mbr, gpt) = (args.has("mbr"), args.has("gpt"));
    let writes = verbs[4] || verbs[5];
    let code = match (verbs, ops.as_slice()) {
        _ if (mbr || gpt) && !writes => stage.die(b"disk: --mbr and --gpt go with --partition or --format\n", EXIT_USAGE),
        ([true, false, false, false, false, false], []) => list(&stage),
        ([false, true, false, false, false, false], [device]) => mount(&stage, device, ""),
        ([false, true, false, false, false, false], [device, label]) => mount(&stage, device, label),
        ([false, false, true, false, false, false], [label]) => unmount(&stage, label),
        ([false, false, false, true, false, false], [name]) => eject(&stage, name),
        ([false, false, false, false, true, false], [disk]) => write(&stage, disk, None, None, mbr, gpt),
        ([false, false, false, false, false, true], [device, fs]) => format_device(&stage, device, fs, None, mbr, gpt),
        ([false, false, false, false, false, true], [device, fs, label]) => {
            format_device(&stage, device, fs, Some(label), mbr, gpt)
        }
        _ => stage.die(
            b"disk: one of --list, --mount DEVICE [LABEL], --unmount LABEL, --eject NAME, --partition DISK or \
              --format DEVICE fat|ext4 [LABEL] (try --help)\n",
            EXIT_USAGE,
        ),
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
            stage.note(text.as_bytes());
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
    /// Nothing at the path answered: for the admin endpoint, the `storage` grant was not given.
    NotHere,
    /// The endpoint is here and opened no session, for this reason: every session of the kind in
    /// use is `WouldBlock`.
    Unopened(KError),
    /// The session broke, or its reply did not parse.
    Transport,
    /// The service refused, with its reason.
    Refused(String),
}

/// Open a session at `at` — an admin session on `/dev/storage/admin`, or a media session on
/// `/dev/storage/media` — send one request, and return the reply's body.
///
/// **No grant, told apart from a busy one.** Without the grant the admin path resolves through the
/// session endpoint at the base `/info`, where `info/admin` is no table, or through nothing at all:
/// `NotFound` either way. That is the one answer that means "not in a view with `storage`". Any
/// other is the service's, and is printed as such: telling a person with the grant to go and get
/// it is worse than saying nothing.
fn ask(stage: &Stage, at: &[u8], op: u16, body: &[u8]) -> Result<Vec<u8>, Failure> {
    let (st, session) = libfs::lookup_wait(stage.namespace, at, librsproto::session::DIR_SESSION_RIGHTS);
    if st == KError::NotFound.as_i32() {
        return Err(Failure::NotHere);
    }
    if st != 0 || session == 0 {
        return Err(Failure::Unopened(KError::from_i32(st)));
    }
    let mut buf = alloc::vec![0u8; IPC_MSG_SIZE];
    let reply = librsproto::session::round_trip(session, &mut buf, 1, op, body);
    // SAFETY: closing the session this call opened.
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
        Failure::NotHere if verb == "eject" => {
            String::from("disk: cannot eject here -- no /dev/storage/media in this namespace\n")
        }
        Failure::NotHere => format!(
            "disk: cannot {verb} here -- mounting and unmounting need the storage grant: `with admin disk --{verb} ...`\n"
        ),
        Failure::Unopened(KError::WouldBlock) if verb == "eject" => {
            format!("disk: {what} not ejected: every media session is in use; try again\n")
        }
        Failure::Unopened(e) => {
            let kind = if verb == "eject" { "media" } else { "admin" };
            format!("disk: {what} not {verb}ed: the storage service opened no {kind} session ({e:?})\n")
        }
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
    let reply = match ask(stage, ADMIN, OP_STORAGE_MOUNT, &body[..n]) {
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
        None => stage.note(format!("{device} mounted at {at}\n").as_bytes()),
    }
    Line::new().s(b"disk: mounted ").s(device.as_bytes()).s(b" at ").untrusted(at.as_bytes()).end();
    EXIT_OK
}

/// `disk --unmount LABEL`.
fn unmount(stage: &Stage, label: &str) -> i64 {
    if let Err(f) = ask(stage, ADMIN, OP_STORAGE_UNMOUNT, label.as_bytes()) {
        return failed(stage, "unmount", label, f);
    }
    match stage.streams.stdout {
        Some(h) => {
            let schema = Schema::new()
                .field("label", TypeTag::String, TypeModifiers::NONE)
                .field("unmounted", TypeTag::Bool, TypeModifiers::NONE);
            write_table(stage, h, &schema, &[alloc::vec![Value::Str(String::from(label)), Value::Bool(true)]]);
        }
        // Not "and left clean": a read-only mount leaves a filesystem as it found it. `--list`
        // says how each was left.
        None => stage.note(format!("{label} unmounted\n").as_bytes()),
    }
    Line::new().s(b"disk: unmounted ").untrusted(label.as_bytes()).end();
    EXIT_OK
}

/// `disk --eject NAME`: a media session's `Eject`, answered once the stick can be pulled. `NAME`
/// may be given as its path, `/storage/NAME`, as `--list` shows it.
///
/// **The drive goes whole**: the service unmounts every filesystem it mounted on the same stick,
/// and answers with their names, a row each — two partitions are two rows, whichever was named.
fn eject(stage: &Stage, name: &str) -> i64 {
    let name = name.strip_prefix("/storage/").unwrap_or(name);
    let reply = match ask(stage, MEDIA, OP_STORAGE_EJECT, name.as_bytes()) {
        Ok(r) => r,
        Err(f) => return failed(stage, "eject", name, f),
    };
    let mut names: Vec<String> =
        librsproto::storage::ejected_names(&reply).map(|n| String::from_utf8_lossy(n).into_owned()).collect();
    if names.is_empty() {
        names.push(String::from(name));
    }
    let said = names.join(", ");
    match stage.streams.stdout {
        Some(h) => {
            let schema = Schema::new()
                .field("name", TypeTag::String, TypeModifiers::NONE)
                .field("ejected", TypeTag::Bool, TypeModifiers::NONE);
            let rows: Vec<Vec<Value>> =
                names.iter().map(|n| alloc::vec![Value::Str(n.clone()), Value::Bool(true)]).collect();
            write_table(stage, h, &schema, &rows);
        }
        None => stage.note(format!("{said} ejected: the drive can be removed\n").as_bytes()),
    }
    Line::new().s(b"disk: ejected ").untrusted(said.as_bytes()).end();
    EXIT_OK
}

/// `disk --format DEVICE FS [LABEL]`: [`write`], with the filesystem named.
fn format_device(stage: &Stage, device: &str, fs: &str, label: Option<&str>, mbr: bool, gpt: bool) -> i64 {
    let Some(fs) = Fs::parse(fs) else {
        say(stage, "disk: a filesystem is fat or ext4\n");
        return EXIT_USAGE;
    };
    write(stage, device, Some(fs), label, mbr, gpt)
}

/// `/dev/devices`' rows, as [`format::target`] reads them; `None` if the table would not read.
fn device_rows(ns: u64) -> Option<Vec<format::Row>> {
    let t = libfs::read_file(ns, DEVICES).ok().and_then(|b| Table::decode(&b).ok())?;
    let col = |name: &str| t.schema.fields.iter().position(|f| f.name == name);
    let (name, kind, size, parent, driver, boot) =
        (col("name")?, col("kind")?, col("size")?, col("parent")?, col("driver")?, col("boot")?);
    let text = |v: &Value| if let Value::Str(s) = v { Some(s.clone()) } else { None };
    Some(
        t.rows
            .iter()
            .map(|r| format::Row {
                name: text(&r[name]).unwrap_or_default(),
                kind: text(&r[kind]).unwrap_or_default(),
                size: if let Value::Int(i) = r[size] { Some(i) } else { None },
                parent: text(&r[parent]),
                driver: text(&r[driver]),
                boot: r[boot] == Value::Bool(true),
            })
            .collect(),
    )
}

/// One row of the storage service's table, as `--partition` and `--format` read it.
struct Held {
    name: String,
    filesystem: Option<String>,
    mounted: Option<String>,
    note: Option<String>,
}

/// The storage service's table, as [`Held`] rows; `None` if it would not read.
fn storage_rows(ns: u64) -> Option<Vec<Held>> {
    let t = libfs::read_file(ns, TABLE).ok().and_then(|b| Table::decode(&b).ok())?;
    let col = |name: &str| t.schema.fields.iter().position(|f| f.name == name);
    let (name, filesystem, mounted) = (col("name")?, col("filesystem")?, col("mounted")?);
    let note = col("note");
    let text = |v: Option<&Value>| if let Some(Value::Str(s)) = v { Some(s.clone()) } else { None };
    Some(
        t.rows
            .iter()
            .map(|r| Held {
                name: text(r.get(name)).unwrap_or_default(),
                filesystem: text(r.get(filesystem)),
                mounted: text(r.get(mounted)),
                note: note.and_then(|i| text(r.get(i))),
            })
            .collect(),
    )
}

/// **The device's own facts**, from its `info` leaf: its logical sector and how many.
fn device_info(ns: u64, path: &str) -> Option<BlockDeviceInfo> {
    let leaf = format!("{path}/info");
    let (st, h) = libfs::lookup_wait(ns, leaf.as_bytes(), libkern::RIGHT_MAP_READ);
    if st != 0 || h == 0 {
        return None;
    }
    // SAFETY: register-only syscall; `h` is ours.
    let addr = unsafe { libkern::syscall::syscall4(libkern::syscall::SYS_MEMORY_MAP, h, 0, 4096, libkern::RIGHT_MAP_READ) };
    close(h);
    if addr < 0 {
        return None;
    }
    let size = core::mem::size_of::<BlockDeviceInfo>();
    // SAFETY: `addr` maps a page read-only, and `size` is under it.
    let info = BlockDeviceInfo::read(unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, size) });
    // SAFETY: unmapping what was mapped above; nothing refers to it now.
    unsafe { libkern::syscall::syscall2(libkern::syscall::SYS_MEMORY_UNMAP, addr as u64, 0) };
    info
}

fn close(h: u64) {
    // SAFETY: closing a handle this process holds.
    unsafe { libkern::syscall::syscall1(libkern::syscall::SYS_HANDLE_CLOSE, h) };
}

/// Seconds since the epoch, or 0 if the clock will not read.
fn now_secs() -> i64 {
    let mut nanos: u64 = 0;
    // SAFETY: a valid writable `u64` out-param.
    let r = unsafe {
        libkern::syscall::syscall2(libkern::syscall::SYS_CLOCK_READ, libkern::abi::CLOCK_REALTIME, (&raw mut nanos) as u64)
    };
    if r < 0 { 0 } else { (nanos / 1_000_000_000) as i64 }
}

/// The monotonic clock, in nanoseconds.
fn monotonic_ns() -> u64 {
    let mut now = 0u64;
    // SAFETY: a valid writable `u64` out-param.
    unsafe { libkern::syscall::syscall2(libkern::syscall::SYS_CLOCK_READ, libkern::abi::CLOCK_MONOTONIC, (&raw mut now) as u64) };
    now
}

/// **Have the drive write its cache to its medium**, and wait until it has.
fn flush(device: u64) -> bool {
    let op = IoOp { opcode: IO_OPCODE_FLUSH, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
    // SAFETY: `device` is a block device handle with WRITE; `&op` is a valid `IoOp`.
    let po = unsafe { libkern::syscall::syscall2(libkern::syscall::SYS_IO_SUBMIT, device, (&op as *const IoOp) as u64) };
    po >= 0 && libfsserver::disk::po_wait(po as u64).0 == 0
}

/// **Wait for a ping on `watch` until `deadline`**, and take every one queued: `false` at the
/// deadline, or if the watch has gone.
fn wait_ping(watch: u64, deadline: u64) -> bool {
    let handles = [watch];
    let mut results = [0u8; 24];
    // SAFETY: a valid one-entry handle array and result buffer on this frame.
    let waited = unsafe {
        libkern::syscall::syscall4(libkern::syscall::SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, deadline)
    };
    if waited != 1 {
        return false;
    }
    let mut buf = alloc::vec![0u8; IPC_MSG_SIZE];
    loop {
        let mut got_handles = [0u64; 8];
        let mut count: usize = 0;
        // SAFETY: valid message, handle and count out-params.
        let got = unsafe {
            libkern::syscall::syscall4(
                libkern::syscall::SYS_CHANNEL_RECV,
                watch,
                buf.as_mut_ptr() as u64,
                got_handles.as_mut_ptr() as u64,
                (&raw mut count) as u64,
            )
        };
        if got == KError::WouldBlock.as_i32() as i64 {
            return true;
        }
        if got != 0 {
            return false;
        }
        // A watch carries no handles; any that came are closed.
        got_handles.iter().take(count.min(8)).for_each(|&h| close(h));
    }
}

/// Write `len` zero bytes at `at` through `io`, 64 KiB at a time.
fn zero(io: &PartitionIo, at: u64, len: u64) -> Result<(), String> {
    let zeros = alloc::vec![0u8; 64 * 1024];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(zeros.len() as u64);
        io.write_at(at + done, &zeros[..n as usize]).map_err(|_| String::from("a write to the device failed"))?;
        done += n;
    }
    Ok(())
}

/// **Say how far a long write is**, a line a quarter, for anything of more than a few steps: a
/// drive's FATs are hundreds of mebibytes, and a still screen for that long looks like a hang.
fn progress<'a>(stage: &'a Stage, what: &'static str) -> impl FnMut(u32, u32) + 'a {
    let mut said = 0;
    move |done: u32, total: u32| {
        let quarter = (done as u64 * 4 / total.max(1) as u64) as u32;
        if total > 16 && quarter > said && quarter < 4 {
            said = quarter;
            say(stage, &format!("disk: {what}: {}%\n", quarter * 25));
        }
    }
}

/// The IDs a write needs, read from the kernel's entropy before anything is written.
struct Ids {
    disk_id: u32,
    disk_guid: [u8; 16],
    partition_guid: [u8; 16],
    volume_id: u32,
    uuid: [u8; 16],
}

impl Ids {
    fn fresh() -> Option<Ids> {
        let mut b = [0u8; 56];
        coreutils::entropy::fill(&mut b).then(|| Ids {
            disk_id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            disk_guid: b[4..20].try_into().unwrap_or_default(),
            partition_guid: b[20..36].try_into().unwrap_or_default(),
            volume_id: u32::from_le_bytes([b[36], b[37], b[38], b[39]]),
            uuid: b[40..56].try_into().unwrap_or_default(),
        })
    }
}

/// **Write `plan` to the device `handle`** of `t`: the wipes and the table, then the filesystem in
/// its partition, then the drive's cache flushed. `name` is the GPT partition's name.
fn write_plan(stage: &Stage, handle: u64, t: &format::Target, plan: &Plan, ids: &Ids, name: &[u8]) -> Result<(), String> {
    let fs = plan.fs.map(|(fs, ..)| fs);
    if let Some((scheme, first, n)) = plan.table {
        let disk = PartitionIo::new(handle, 0, t.sectors * format::SECTOR).ok_or("no transfer buffer for the device")?;
        for (at, len) in format::wipes(t.sectors) {
            zero(&disk, at, len)?;
        }
        match scheme {
            Scheme::Mbr => {
                let fat = match plan.fs {
                    Some((Fs::Fat, Label::Fat(label), _, len)) => {
                        let p = fs_server_fat::mkfs::Params { sectors: len / format::SECTOR, label, volume_id: 0, hidden: 0, now: 0 };
                        fs_server_fat::mkfs::plan(&p).ok().map(|g| g.kind)
                    }
                    _ => None,
                };
                let entry = mbr::Partition { kind: format::mbr_type(fs, fat), first_lba: first, blocks: n };
                let mut sector = [0u8; mbr::LEN];
                mbr::build(t.sectors, ids.disk_id, &[entry], &mut sector).map_err(|e| format!("the MBR would not build: {e:?}"))?;
                disk.write_at(0, &sector).map_err(|_| "writing the MBR failed")?;
            }
            Scheme::Gpt => {
                let entry = table::Partition::new(format::gpt_type(fs), ids.partition_guid, first, first + n - 1, name);
                let (mut front, mut back) = (alloc::vec![0u8; table::FRONT_BYTES], alloc::vec![0u8; table::BACK_BYTES]);
                table::build(t.sectors, ids.disk_guid, &[entry], &mut front, &mut back)
                    .map_err(|e| format!("the GPT would not build: {e:?}"))?;
                disk.write_at(0, &front).map_err(|_| "writing the GPT failed")?;
                let back_at = (t.sectors - table::ARRAY_BLOCKS - 1) * format::SECTOR;
                disk.write_at(back_at, &back).map_err(|_| "writing the GPT's backup failed")?;
            }
        }
    }
    if let Some((fs, label, at, len)) = plan.fs {
        let io = PartitionIo::new(handle, at, len).ok_or("no transfer buffer for the device")?;
        let now = now_secs();
        match (fs, label) {
            (Fs::Fat, Label::Fat(label)) => {
                // The hidden sectors are where the partition starts on its disk where `disk` wrote
                // the table, and 0 where it did not; only booting reads them.
                let hidden = (at / format::SECTOR) as u32;
                let p = fs_server_fat::mkfs::Params { sectors: len / format::SECTOR, label, volume_id: ids.volume_id, hidden, now };
                fs_server_fat::mkfs::format(&io, &p, &mut progress(stage, "zeroing the FATs"))
                    .map_err(|e| format!("the FAT would not lay out: {e:?}"))?;
            }
            (Fs::Ext4, Label::Ext4(label)) => {
                // **Its first mebibyte zeroed first**, as the FAT formatter zeroes its own: no old
                // boot sector outlives the format for a reader to find.
                zero(&io, 0, len.min(1 << 20))?;
                let p = format::ext4_params(len, label, ids.uuid, now);
                fs_server_ext4::mkfs::format(&io, &p, &mut progress(stage, "laying out the groups"))
                    .map_err(|e| format!("the ext4 would not lay out: {e:?}"))?;
            }
            _ => return Err(String::from("a label for another filesystem")),
        }
    }
    if !flush(handle) {
        return Err(String::from("the drive's cache could not be flushed"));
    }
    Ok(())
}

/// **`disk --partition DISK` and `disk --format DEVICE FS [LABEL]`**: decided, refused before a
/// byte is written, written, read again by the storage service, and — for a disk — waited for.
fn write(stage: &Stage, operand: &str, fs: Option<Fs>, label: Option<&str>, mbr: bool, gpt: bool) -> i64 {
    let verb = if fs.is_some() { "formatted" } else { "partitioned" };
    let ns = stage.namespace;
    let Some(name) = device_name(operand) else {
        say(stage, "disk: a device is /dev/blk/N or blk-N\n");
        return EXIT_USAGE;
    };
    let refuse = |why: &str| {
        say(stage, &format!("disk: {name} not {verb}: {why}; nothing was written\n"));
        EXIT_FAILURE
    };
    let Some(rows) = device_rows(ns) else {
        return refuse("/dev/devices/all.tsm would not read");
    };
    let Some(mut t) = format::target(&rows, &name) else {
        return refuse("no block device has that name");
    };
    let decide = |t: &format::Target| match fs {
        Some(fs) => format::format(t, fs, label, mbr, gpt),
        None => format::partition(t, mbr, gpt),
    };
    if let Err(r) = decide(&t) {
        return refuse(&format!("{r}"));
    }
    // **Mounted, it is not written**: said by name, since the grant withholds it with no reason.
    let mut on_it = alloc::vec![name.clone()];
    if t.kind != format::Kind::Partition {
        on_it.extend(format::partitions_of(&rows, &name));
    }
    if let Some(held) = storage_rows(ns) {
        let mounted: Vec<String> =
            held.iter().filter(|h| on_it.contains(&h.name)).filter_map(|h| h.mounted.clone()).collect();
        if !mounted.is_empty() {
            let names = mounted.iter().map(|m| m.trim_start_matches("/storage/")).collect::<Vec<_>>().join(", ");
            return refuse(&format!("it is mounted, as {names}: unmount it first (with admin disk --unmount NAME)"));
        }
    }
    let path = format!("/dev/blk/{}", &name["blk-".len()..]);
    let (st, handle) = libfs::lookup_wait(ns, path.as_bytes(), libkern::RIGHT_READ | libkern::RIGHT_WRITE);
    if st != 0 || handle == 0 {
        let flag = if fs.is_some() { "format" } else { "partition" };
        return refuse(&if libfs::ns_children(ns, b"/dev/blk").is_empty() {
            format!("this session holds no disks: run it as `with admin disk --{flag} ...`")
        } else {
            String::from("this view does not hold it: the disks grant withholds a device in use")
        });
    }
    // **The device's own size and sector**, which are what is written by.
    let Some(info) = device_info(ns, &path) else {
        close(handle);
        return refuse("its info would not read");
    };
    t.sectors = info.block_count;
    t.sector_bytes = info.logical_block_size;
    let plan = match decide(&t) {
        Ok(p) => p,
        Err(r) => {
            close(handle);
            return refuse(&format!("{r}"));
        }
    };
    let Some(ids) = Ids::fresh() else {
        close(handle);
        return refuse("the kernel's entropy source did not answer, and the IDs written must be random");
    };
    // The disk's partitions before, so the one its new table brings is told from them; and a watch
    // opened before anything changes, so no ping is missed.
    let before = format::partitions_of(&rows, &name);
    let watch = if t.kind == format::Kind::Partition {
        None
    } else {
        let (st, w) = libfs::lookup_wait(ns, WATCH, librsproto::session::DIR_SESSION_RIGHTS);
        (st == 0 && w != 0).then_some(w)
    };
    let gpt_name = match fs {
        Some(Fs::Fat) => label.unwrap_or(format::FAT_LABEL),
        Some(Fs::Ext4) => label.unwrap_or(format::EXT4_LABEL),
        None => "",
    };
    let wrote = write_plan(stage, handle, &t, &plan, &ids, gpt_name.as_bytes());
    close(handle);
    let scheme = plan.table.map(|(s, ..)| if s == Scheme::Mbr { "mbr" } else { "gpt" });
    let fs_word = fs.map(|f| if f == Fs::Fat { "fat" } else { "ext4" });
    if let Err(why) = wrote {
        watch.map(close);
        say(stage, &format!("disk: {name} not {verb}: {why}; what is on it now is unknown\n"));
        return EXIT_FAILURE;
    }
    let what = match (fs_word, scheme) {
        (Some(f), Some(s)) => format!("{f} ({s})"),
        (Some(f), None) => String::from(f),
        (None, Some(s)) => format!("({s})"),
        (None, None) => String::new(),
    };
    Line::new().s(b"disk: wrote ").s(name.as_bytes()).s(b" ").s(what.as_bytes()).end();

    // **Read again**, then, for a disk, the new partition waited for.
    let mounted = match ask(stage, ADMIN, OP_STORAGE_REREAD, name.as_bytes()) {
        Ok(reply) => librsproto::storage::ejected_names(&reply).map(|n| String::from_utf8_lossy(n).into_owned()).collect::<Vec<_>>(),
        Err(f) => {
            watch.map(close);
            let why = match f {
                Failure::NotHere => String::from("this session holds no storage grant: run it as `with admin`"),
                Failure::Unopened(e) => format!("the storage service opened no admin session ({e:?})"),
                Failure::Transport => String::from("the storage service went away"),
                Failure::Refused(why) => why,
            };
            say(stage, &format!("disk: {name} {verb}, but not read again: {why}\n"));
            return EXIT_FAILURE;
        }
    };
    let (partition, held) = if t.kind == format::Kind::Partition {
        let held = storage_rows(ns).and_then(|h| h.into_iter().find(|h| h.name == name));
        (None, held)
    } else {
        let Some(watch) = watch else {
            say(stage, &format!("disk: {name} {verb} {what}; no watch to wait on, so see disk --list\n"));
            return EXIT_OK;
        };
        let deadline = monotonic_ns().saturating_add(ARRIVAL_NS);
        // **Its row, once there**: a row is put in the table in the turn its partition arrived,
        // with its mount made, so the first one seen says all there is to say.
        let found = loop {
            let new = device_rows(ns).and_then(|rows| format::partitions_of(&rows, &name).into_iter().find(|p| !before.contains(p)));
            let row = new.and_then(|p| storage_rows(ns)?.into_iter().find(|h| h.name == p));
            if row.is_some() || !wait_ping(watch, deadline) {
                break row;
            }
        };
        close(watch);
        let Some(row) = found else {
            say(stage, &format!("disk: {name} {verb} {what}, but its new partition did not arrive in time: see disk --list\n"));
            return EXIT_FAILURE;
        };
        (Some(row.name.clone()), Some(row))
    };
    let at = held.as_ref().and_then(|h| h.mounted.clone()).or_else(|| mounted.first().map(|m| format!("/storage/{m}")));
    let note = held.as_ref().and_then(|h| h.note.clone());
    let holds = held.as_ref().and_then(|h| h.filesystem.clone());
    // What became of it, in words: mounted, why not, or holding nothing.
    let outcome = match (&at, &note, &holds) {
        (Some(at), _, _) => format!("mounted at {at}"),
        (None, Some(note), _) => format!("not mounted: {note}"),
        (None, None, Some(fs)) => format!("{fs}, not mounted"),
        (None, None, None) => String::from("holding nothing"),
    };
    let said = match &partition {
        Some(p) => format!("{name} {verb} {what}: {p}, {outcome}"),
        None => format!("{name} {verb} {what}: {outcome}"),
    };
    match stage.streams.stdout {
        Some(h) => {
            let nullable = TypeModifiers::NULLABLE;
            let schema = Schema::new()
                .field("device", TypeTag::String, TypeModifiers::NONE)
                .field("table", TypeTag::String, nullable)
                .field("filesystem", TypeTag::String, nullable)
                .field("partition", TypeTag::String, nullable)
                .field("mounted", TypeTag::String, nullable)
                .field("note", TypeTag::String, nullable);
            let cell = |v: Option<&str>| v.map_or(Value::Null, |s| Value::Str(String::from(s)));
            let row = alloc::vec![
                Value::Str(name.clone()),
                cell(scheme),
                cell(fs_word),
                cell(partition.as_deref()),
                cell(at.as_deref()),
                cell(note.as_deref()),
            ];
            write_table(stage, h, &schema, &[row]);
        }
        None => stage.note(format!("{said}\n").as_bytes()),
    }
    Line::new().s(b"disk: ").untrusted(said.as_bytes()).end();
    EXIT_OK
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"disk: panic\n");
    exit(EXIT_FAILURE)
}
