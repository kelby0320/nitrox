//! `account` — the accounts on this machine: who they are, and adding, removing and changing their
//! passwords (`docs/planning/administration.md` § Part D).
//!
//! ```text
//! account --list                        # every account, its sessions, and who administers
//! account --password                    # change your own, proved with the current one
//! account --add NAME                    # add NAME, asking for its password twice
//! account --remove NAME [--home]        # remove NAME, and its home only if asked
//! account --password NAME               # set NAME's, with no current one to prove
//! account --password NAME --users FILE  # set NAME's in a users file directly: recovery
//! ```
//!
//! ## Three authorities
//!
//! **`--list` and your own `--password` need none.** They ask the view broker on this session's
//! `/dev/views`, which knows whose session it is: the list is anyone's to read, as a Unix `passwd`
//! file is, and your own password is proved with the current one, checked under the session's delay
//! after a wrong one (`rsproto-views-ops.md` § `Accounts`, `ChangePassword`).
//!
//! **`--add`, `--remove` and `--password NAME` need the `accounts` grant.** They speak on
//! `/dev/accounts`, which the broker binds only into a view whose profile grants `accounts`, so
//! they run as `with admin account …`. Without it that path is not there, and `account` says so
//! and names `with`. **The broker does the refusing that matters** — a removal while the account is
//! logged in, one that would leave nobody able to administer — and its reason is printed as given.
//!
//! **`--users FILE` needs no service at all**: it edits a users file with `libusers`, writing
//! `FILE.new` and renaming it over. That is recovery from the live image, where an installed
//! disk's `/system/users` is reachable once `with admin disk` has mounted it writable. The running
//! system's own file is never under `/storage`, so this cannot reach it (administration Part C.5).
//!
//! ## Passwords
//!
//! Asked on the terminal the shell handed this stage, echo off, a new one twice
//! (`coreutils::prompt`, shared with `with`). **Never from a stream**, and never echoed, logged or
//! kept: every copy is zeroed once it has been sent. What the broker or the file edit said is
//! written to `stderr` and, escaped, on the console, since a terminal on a release image renders
//! nothing a gate can read.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::ipc::{call, lookup, outcome};
use coreutils::prompt::{ask_new_password, ask_password};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::debug::Line;
use libkern::syscall::{
    SYS_ENTROPY_CREATE, SYS_ENTROPY_READ, SYS_HANDLE_CLOSE, SYS_WAIT, syscall0, syscall1, syscall3, syscall4,
};
use libkern::{RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT, exit, scrub};
use librsproto::auth::build_account_request;
use librsproto::views::*;
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: the TSM1 encoder allocates.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where the `accounts` grant binds the broker's accounts endpoint.
const ACCOUNTS: &[u8] = b"/dev/accounts";
/// This session's view broker.
const VIEWS: &[u8] = b"/dev/views";

const FLAGS: [Flag; 5] = [
    Flag::long_only("list", "every account, its home, its sessions, and whether it administers"),
    Flag::long_only("add", "add NAME, asking for its password twice (needs the accounts grant)"),
    Flag::long_only("remove", "remove NAME (needs the accounts grant)"),
    Flag::long_only("home", "with --remove: remove the account's home too"),
    Flag::long_only("password", "change your own password, or set NAME's (needs the accounts grant)"),
];

/// `--users` is a flag of its own so that `FILE` is an operand, as `disk --mount DEVICE` is.
const USERS: Flag = Flag::long_only("users", "with --password NAME: set it in FILE, a users file, directly");

const HELP: &[u8] = b"usage: account --list\n\
    \x20      account --password\n\
    \x20      account --add NAME\n\
    \x20      account --remove NAME [--home]\n\
    \x20      account --password NAME [--users FILE]\n\
    \n\
    --list                  every account, its home, how many sessions it has open, and\n\
    \x20                       whether it could administer, as a table\n\
    --password              change your own password: the current one, then a new one twice\n\
    --add NAME              add NAME, with a home at /home/NAME, asking for its password twice\n\
    --remove NAME [--home]  remove NAME, keeping its home unless --home; refused while NAME\n\
    \x20                       is logged in, or if nobody left could administer\n\
    --password NAME         set NAME's password, asking for it twice\n\
    --users FILE            with --password NAME: set it in FILE directly, for recovery from\n\
    \x20                       the live image\n\
    \n\
    --add, --remove and --password NAME need the accounts grant: run them with `with admin`.\n\
    \n\
    \x20     --help    show this help and exit\n\
    \x20     --version show version information and exit\n";

const VERSION: &[u8] = b"account (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);
    let flags = [FLAGS[0], FLAGS[1], FLAGS[2], FLAGS[3], FLAGS[4], USERS];
    let args = match parse(&stage.argv, &flags) {
        Ok(a) => a,
        Err(_) => stage.die(b"account: unrecognized option (try --help)\n", EXIT_USAGE),
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
    let verbs = [args.has("list"), args.has("add"), args.has("remove"), args.has("password")];
    let (home, users) = (args.has("home"), args.has("users"));
    let code = match (verbs, home, users, ops.as_slice()) {
        ([true, false, false, false], false, false, []) => list(&stage),
        ([false, true, false, false], false, false, [name]) => add(&stage, name),
        ([false, false, true, false], _, false, [name]) => remove(&stage, name, home),
        ([false, false, false, true], false, false, []) => change_own(&stage),
        ([false, false, false, true], false, false, [name]) => set(&stage, name),
        ([false, false, false, true], false, true, [name, file]) => offline(&stage, name, file),
        _ => stage.die(
            b"account: one of --list, --password [NAME [--users FILE]], --add NAME or \
              --remove NAME [--home] (try --help)\n",
            EXIT_USAGE,
        ),
    };
    exit(code)
}

/// Say `text` where the person reads it, and on the console, escaped, where a gate does.
fn say(stage: &Stage, text: &str) {
    if stage.streams.stderr.is_some() {
        let mut line = String::from("account: ");
        line.push_str(text);
        line.push('\n');
        stage.diag(line.as_bytes());
    }
    Line::new().s(b"account: ").untrusted(text.as_bytes()).end();
}

/// The terminal to ask on, or a failure saying there is none.
fn terminal(stage: &Stage) -> Result<u64, i64> {
    stage.terminal.ok_or_else(|| {
        say(stage, "a password is needed and there is no terminal to ask on");
        EXIT_FAILURE
    })
}

/// A channel on this session's view broker.
fn views(stage: &Stage) -> Result<u64, i64> {
    let ch = lookup(stage.namespace, VIEWS, RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if ch == 0 {
        say(stage, "this session has no view broker (/dev/views)");
        return Err(EXIT_FAILURE);
    }
    Ok(ch)
}

/// A channel on the broker's accounts endpoint, which only the `accounts` grant binds. Without it,
/// a failure that names the view to use rather than one that says nothing.
fn accounts(stage: &Stage, verb: &str) -> Result<u64, i64> {
    let ch = lookup(stage.namespace, ACCOUNTS, RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if ch == 0 {
        say(stage, &format!("cannot {verb} here -- that needs the accounts grant: `with admin account ...`"));
        return Err(EXIT_FAILURE);
    }
    Ok(ch)
}

/// Ask the broker `op` on `ch`, and report its answer: `EXIT_OK` if it did it.
fn ask(stage: &Stage, ch: u64, op: u16, body: &[u8]) -> i64 {
    let answer = call(ch, op, 1, body, &[]);
    let (o, why) = match answer {
        Some((false, body)) => outcome(&body),
        Some((true, _)) => (Outcome::Denied { retry: false }, String::from("the broker refused the request")),
        None => (Outcome::Denied { retry: false }, String::from("the broker did not answer")),
    };
    say(stage, &why);
    if o == Outcome::Started { EXIT_OK } else { EXIT_FAILURE }
}

/// A name checked here against the rules, so a mistyped one is said before any password is asked.
fn checked_name(stage: &Stage, name: &str) -> Result<(), i64> {
    if libusers::valid_name(name.as_bytes()) {
        return Ok(());
    }
    say(stage, core::str::from_utf8(libusers::Refusal::BadName.why()).unwrap_or("not a valid name"));
    Err(EXIT_USAGE)
}

/// `account --list`: `Table<{name, home, sessions, administers}>`, or a line per account.
fn list(stage: &Stage) -> i64 {
    let ch = match views(stage) {
        Ok(ch) => ch,
        Err(code) => return code,
    };
    let Some((false, body)) = call(ch, OP_VIEWS_ACCOUNTS, 1, &[], &[]) else {
        say(stage, "the broker could not list the accounts");
        return EXIT_FAILURE;
    };
    let mut rows: Vec<(String, String, u16, bool)> = Vec::new();
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if parse_accounts(&body, |r| rows.push((text(r.name), text(r.home), r.sessions, r.administers))).is_none() {
        say(stage, "the broker's list did not read");
        return EXIT_FAILURE;
    }
    let Some(stdout) = stage.streams.stdout else {
        for (name, home, sessions, administers) in &rows {
            let role = if *administers { ", administers" } else { "" };
            say(stage, &format!("{name}  {home}  {sessions} session(s){role}"));
        }
        return EXIT_OK;
    };
    let schema = Schema::new()
        .field("name", TypeTag::String, TypeModifiers::NONE)
        .field("home", TypeTag::String, TypeModifiers::NONE)
        .field("sessions", TypeTag::Int, TypeModifiers::NONE)
        .field("administers", TypeTag::Bool, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for (name, home, sessions, administers) in &rows {
            let row = [
                Value::Str(name.clone()),
                Value::Str(home.clone()),
                Value::Int(*sessions as i64),
                Value::Bool(*administers),
            ];
            tw.write_row(&row)?;
        }
        tw.finish_with_status(0)
    });
    if wrote.and_then(|()| tw.into_sink().finish()).is_err() {
        say(stage, "write failed");
        return EXIT_FAILURE;
    }
    EXIT_OK
}

/// `account --add NAME`: the name checked, the password asked twice, then `AddAccount`.
fn add(stage: &Stage, name: &str) -> i64 {
    if let Err(code) = checked_name(stage, name) {
        return code;
    }
    let (ch, term) = match (accounts(stage, "add an account"), terminal(stage)) {
        (Ok(ch), Ok(term)) => (ch, term),
        (Err(code), _) | (_, Err(code)) => return code,
    };
    let mut pw = match ask_new_password(term, format!("new password for {name}: ").as_bytes()) {
        Ok(pw) => pw,
        Err(e) => {
            say(stage, e.why());
            return EXIT_FAILURE;
        }
    };
    let mut body = [0u8; 256];
    let n = build_account_request(&mut body, name.as_bytes(), &pw);
    scrub(&mut pw);
    let code = match n {
        Some(n) => ask(stage, ch, OP_VIEWS_ADD_ACCOUNT, &body[..n]),
        None => {
            say(stage, "the request does not fit");
            EXIT_FAILURE
        }
    };
    scrub(&mut body);
    code
}

/// `account --remove NAME [--home]`.
fn remove(stage: &Stage, name: &str, home: bool) -> i64 {
    if let Err(code) = checked_name(stage, name) {
        return code;
    }
    let ch = match accounts(stage, "remove an account") {
        Ok(ch) => ch,
        Err(code) => return code,
    };
    let mut body = [0u8; 64];
    match build_remove_account(&mut body, name.as_bytes(), home) {
        Some(n) => ask(stage, ch, OP_VIEWS_REMOVE_ACCOUNT, &body[..n]),
        None => EXIT_USAGE,
    }
}

/// `account --password NAME`: an administrator setting it, with no current one to prove.
fn set(stage: &Stage, name: &str) -> i64 {
    if let Err(code) = checked_name(stage, name) {
        return code;
    }
    let (ch, term) = match (accounts(stage, "set another account's password"), terminal(stage)) {
        (Ok(ch), Ok(term)) => (ch, term),
        (Err(code), _) | (_, Err(code)) => return code,
    };
    let mut pw = match ask_new_password(term, format!("new password for {name}: ").as_bytes()) {
        Ok(pw) => pw,
        Err(e) => {
            say(stage, e.why());
            return EXIT_FAILURE;
        }
    };
    let mut body = [0u8; 256];
    let n = build_account_request(&mut body, name.as_bytes(), &pw);
    scrub(&mut pw);
    let code = match n {
        Some(n) => ask(stage, ch, OP_VIEWS_SET_PASSWORD, &body[..n]),
        None => {
            say(stage, "the request does not fit");
            EXIT_FAILURE
        }
    };
    scrub(&mut body);
    code
}

/// `account --password`: your own — the current one, then a new one twice, then `ChangePassword`,
/// which the broker checks under the session's delay after a wrong one.
fn change_own(stage: &Stage) -> i64 {
    let (ch, term) = match (views(stage), terminal(stage)) {
        (Ok(ch), Ok(term)) => (ch, term),
        (Err(code), _) | (_, Err(code)) => return code,
    };
    let Some(mut current) = ask_password(term, b"current password: ") else {
        say(stage, "cancelled");
        return EXIT_FAILURE;
    };
    let mut new = match ask_new_password(term, b"new password: ") {
        Ok(pw) => pw,
        Err(e) => {
            scrub(&mut current);
            say(stage, e.why());
            return EXIT_FAILURE;
        }
    };
    let mut body = [0u8; 512];
    let n = build_password_change(&mut body, &current, &new);
    scrub(&mut current);
    scrub(&mut new);
    let code = match n {
        Some(n) => ask(stage, ch, OP_VIEWS_CHANGE_PASSWORD, &body[..n]),
        None => {
            say(stage, "the request does not fit");
            EXIT_FAILURE
        }
    };
    scrub(&mut body);
    code
}

/// `account --password NAME --users FILE`: **recovery, with no service.** `FILE` is read, `NAME`'s
/// record given a new password under a fresh salt, and the result written to `FILE.new` and renamed
/// over `FILE` — so a failure part way leaves `FILE` as it was.
fn offline(stage: &Stage, name: &str, file: &str) -> i64 {
    if let Err(code) = checked_name(stage, name) {
        return code;
    }
    let path = stage.path(file.as_bytes());
    let before = match libfs::read_file(stage.namespace, &path) {
        Ok(b) => b,
        Err(_) => {
            say(stage, &format!("cannot read {file}"));
            return EXIT_FAILURE;
        }
    };
    if before.len() > libusers::MAX_FILE {
        say(stage, &format!("{file} is larger than a users file can be ({} bytes)", libusers::MAX_FILE));
        return EXIT_FAILURE;
    }
    if libusers::find(&before, name.as_bytes()).is_none() {
        say(stage, &format!("{file} has no account named {name}"));
        return EXIT_FAILURE;
    }
    let term = match terminal(stage) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let mut pw = match ask_new_password(term, format!("new password for {name}: ").as_bytes()) {
        Ok(pw) => pw,
        Err(e) => {
            say(stage, e.why());
            return EXIT_FAILURE;
        }
    };
    let Some(salt) = fresh_salt() else {
        scrub(&mut pw);
        say(stage, "the kernel's entropy source did not answer, so there is no salt");
        return EXIT_FAILURE;
    };
    let mut after = [0u8; libusers::MAX_FILE];
    let edited = libusers::set_password(&before, &mut after, name.as_bytes(), &pw, &salt);
    scrub(&mut pw);
    let n = match edited {
        Ok(n) => n,
        Err(r) => {
            say(stage, core::str::from_utf8(r.why()).unwrap_or("the file could not be edited"));
            return EXIT_FAILURE;
        }
    };
    let mut new_path = path.clone();
    new_path.extend_from_slice(b".new");
    let written = libfs::write_file(stage.namespace, &new_path, &after[..n])
        .and_then(|()| libfs::rename(stage.namespace, &new_path, &path, true));
    if written.is_err() {
        say(stage, &format!("{file} could not be written; it is as it was"));
        return EXIT_FAILURE;
    }
    say(stage, &format!("set a new password for {name} in {file}"));
    EXIT_OK
}

/// A new password's salt, `libusers::SALT_LEN` bytes from the kernel's entropy source — which may
/// answer with a pending operation to wait on before it has been seeded.
fn fresh_salt() -> Option<[u8; libusers::SALT_LEN]> {
    // SAFETY: register-only syscall; returns a fresh entropy handle.
    let ent = unsafe { syscall0(SYS_ENTROPY_CREATE) };
    if ent <= 0 {
        return None;
    }
    let ent = ent as u64;
    let mut salt = [0u8; libusers::SALT_LEN];
    let got = loop {
        // SAFETY: a valid out-buffer of SALT_LEN bytes, and an entropy handle with READ.
        let r = unsafe { syscall3(SYS_ENTROPY_READ, ent, salt.as_mut_ptr() as u64, salt.len() as u64) };
        if r == 0 {
            break true;
        }
        if r < 0 {
            break false;
        }
        // Not yet seeded: wait for the pending operation, then read again.
        let handles = [r as u64];
        let mut results = [0u8; 24];
        // SAFETY: a valid one-entry handle array and result buffer on this frame.
        let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
        // SAFETY: closing the pending operation this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, r as u64) };
        let status = i32::from_le_bytes([results[8], results[9], results[10], results[11]]);
        if waited != 1 || status != 0 {
            break false;
        }
    };
    // SAFETY: closing the entropy handle this process owns.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ent) };
    got.then_some(salt)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    libkern::kprint(b"account: panic\n");
    exit(EXIT_FAILURE)
}
