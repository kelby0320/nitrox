//! `with` — run a program in a **view**: this session's namespace plus what the view grants, when
//! `/system/views.toml` says the person asking may (`docs/planning/administration.md` § Part A).
//!
//! ```text
//! with admin disk --mount /dev/blk/1     # mount a disk, through the view's storage grant
//! with --list                            # the views you may use
//! with --check policy.toml               # whether a file is a valid policy
//! with admin with --show ./views.toml    # a copy of the policy to edit (the `views` grant)
//! with admin with --install ./views.toml # install an edited copy (the `views` grant)
//! ```
//!
//! **The policy is changed by `--show` and `--install`** (administration Part D.2), not edited in
//! place: `--show FILE` writes a copy, any editor changes it, and `--install FILE` has the broker
//! check it — it must read, and an account that exists must still be able to administer — and
//! replace the policy atomically. Both reach the broker through `/dev/policy`, which only a view
//! with the `views` grant binds, so outside one they say which view to use.
//!
//! **What `with` never gets is the authority.** It sends the view broker a *copy* of its own
//! namespace (`sys_ns_derive`), the broker copies that again and builds the view there, and the
//! program runs in a namespace `with` holds no handle to. `with` relays: the program's streams are
//! its own, moved to the program, and what comes back is an exit status.
//!
//! **It reads the password, not the broker** — on the terminal its shell handed it, echo off, with
//! `libprompt`, which `account` shares. The broker holds each check for the session's delay
//! after a wrong one, and ends a request after three.
//!
//! **The request itself is `libviews`'** (administration Part F.3): the desktop's Restart and Shut
//! down make the same one, for `shutdown` in the `power` view. What is `with`'s is the prompt, the
//! relay, and the policy's `--show` and `--install`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use libprompt::ask_password;
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::{IPC_PAYLOAD_SIZE, KIND_TERMINATE_REQUESTED, Notification};
use libkern::scrub;
use libkern::syscall::{
    SYS_CLOCK_READ, SYS_HANDLE_DUPLICATE, SYS_NOTIF_RECV, SYS_NS_DERIVE, syscall1, syscall2, syscall4,
};
use libkern::{RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT, exit};
use librsproto::views::*;
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::wire::{Value, write_value};
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag};
use libprompt::ipc::{call, close, lookup, recv, send, wait};
use libviews::outcome;
use libviews::{Handed, REQUEST_ID};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const HELP: &[u8] = b"usage: with VIEW PROGRAM [ARG...]\n\
    \x20      with --forget\n\
    \x20      with --list\n\
    \x20      with --check FILE\n\
    \x20      with --show [FILE]\n\
    \x20      with --install FILE\n\
    \n\
    Run PROGRAM in VIEW: this session's namespace plus what the view grants,\n\
    when /system/views.toml says you may. Asks for your password when the\n\
    policy does, and remembers it for five minutes on this terminal, for that\n\
    view. PROGRAM is a bare name; its output goes where with's would.\n\
    \n\
    \x20     --forget   forget every password this session has remembered\n\
    \x20     --list     the views you may use, as a table\n\
    \x20     --check    whether FILE is a valid policy\n\
    \x20     --show     the policy, or a copy of it written to FILE (needs the views grant)\n\
    \x20     --install  check FILE and install it as the policy (needs the views grant)\n\
    \x20     --help     show this help and exit\n\
    \x20     --version  show version information and exit\n";

const VERSION: &[u8] = b"with (nitrox coreutils) 0.1.0\n";

/// Wrong passwords before the broker ends the request — the prompt counts them for the person.
const TRIES: u8 = 3;

static mut NOTIF: Notification = Notification::zeroed();

/// When a token from this terminal stops being worth waiting for: two seconds from now. The terminal
/// server answers from a table; this only bounds a terminal handle that is not one.
fn token_deadline() -> u64 {
    let mut ns: u64 = 0;
    // SAFETY: a valid writable `u64` out-param.
    unsafe { syscall2(SYS_CLOCK_READ, libkern::abi::CLOCK_MONOTONIC, (&raw mut ns) as u64) };
    ns.saturating_add(2_000_000_000)
}

fn dup(h: u64) -> u64 {
    // SAFETY: duplicating a handle this process owns, with the rights a hand-over needs.
    let d = unsafe { syscall2(SYS_HANDLE_DUPLICATE, h, u64::MAX) };
    if d > 0 { d as u64 } else { 0 }
}

/// The view broker, through this session's `/dev/views`.
fn broker(stage: &Stage) -> u64 {
    let ch = libviews::broker(stage.namespace);
    if ch == 0 {
        stage.die(b"with: this session has no view broker (/dev/views)\n", EXIT_FAILURE);
    }
    ch
}

fn say(stage: &Stage, what: &str) {
    let mut line = String::from("with: ");
    line.push_str(what);
    line.push('\n');
    stage.diag(line.as_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);
    let argv: Vec<&str> = stage.argv.iter().skip(1).map(|s| s.as_str()).collect();
    match argv.first().copied() {
        Some("--help") => stage.die(HELP, EXIT_OK),
        Some("--version") => stage.die(VERSION, EXIT_OK),
        Some("--list") if argv.len() == 1 => list(&stage),
        Some("--forget") if argv.len() == 1 => forget(&stage),
        Some("--check") if argv.len() == 2 => check(&stage, argv[1]),
        Some("--show") if argv.len() <= 2 => show(&stage, argv.get(1).copied()),
        Some("--install") if argv.len() == 2 => install(&stage, argv[1]),
        Some(flag) if flag.starts_with("--") => stage.die(HELP, EXIT_USAGE),
        Some(_) if argv.len() >= 2 => run(&stage, argv[0], argv[1], &argv[2..]),
        _ => stage.die(HELP, EXIT_USAGE),
    }
}

/// `with --forget`: every password this session has remembered, every terminal's, forgotten — as
/// `sudo -k`, and silent as it is.
fn forget(stage: &Stage) -> ! {
    let ch = broker(stage);
    match libviews::forget(ch, u64::MAX) {
        Ok(()) => exit(EXIT_OK),
        Err(_) => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    }
}

/// `with --list`: the views this session's person may use, as `Table<{view, run, password}>`.
fn list(stage: &Stage) -> ! {
    let ch = broker(stage);
    let rows = match libviews::list(ch, u64::MAX) {
        Ok(rows) => rows,
        Err(libviews::Failed::Refused) => {
            stage.die(b"with: the broker could not list your views (does the policy read?)\n", EXIT_FAILURE)
        }
        Err(libviews::Failed::Garbled) => stage.die(b"with: the broker's listing did not read\n", EXIT_FAILURE),
        Err(_) => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    };
    match stage.streams.stdout {
        Some(h) => {
            let schema = Schema::new()
                .field("view", TypeTag::String, TypeModifiers::NONE)
                .field("run", TypeTag::String, TypeModifiers::NONE)
                .field("password", TypeTag::Bool, TypeModifiers::NONE);
            let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(h), IPC_PAYLOAD_SIZE));
            let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
                for r in &rows {
                    let row = [Value::Str(r.view.clone()), Value::Str(r.run.clone()), Value::Bool(r.password)];
                    tw.write_row(&row)?;
                }
                tw.finish_with_status(0)
            });
            if wrote.and_then(|()| tw.into_sink().finish()).is_err() {
                stage.die(b"with: write failed\n", EXIT_FAILURE);
            }
        }
        None => {
            let lines: Vec<String> = rows
                .iter()
                .map(|r| {
                    alloc::format!("{}  {}  {}", r.view, r.run, if r.password { "password" } else { "no password" })
                })
                .collect();
            for l in &lines {
                say(stage, l);
            }
        }
    }
    exit(EXIT_OK)
}

/// `with --check FILE`: whether FILE is a policy the broker would accept.
fn check(stage: &Stage, file: &str) -> ! {
    let path = stage.path(file.as_bytes());
    let Ok(text) = libfs::read_file(stage.namespace, &path) else {
        say(stage, &alloc::format!("cannot read `{file}`"));
        exit(EXIT_FAILURE);
    };
    let ch = broker(stage);
    let (o, why) = match call(ch, OP_VIEWS_CHECK, 1, &text, &[]) {
        Some((false, body)) => outcome(&body),
        _ => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    };
    say(stage, &alloc::format!("{file}: {why}"));
    exit(if o == Outcome::Started { EXIT_OK } else { EXIT_FAILURE })
}

/// The broker's policy endpoint, through the `views` grant's `/dev/policy`. Outside a view with the
/// grant there is none, and `with` says which view to use rather than failing without a word.
fn policy_endpoint(stage: &Stage, verb: &str) -> u64 {
    let ch = lookup(stage.namespace, b"/dev/policy", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if ch == 0 {
        say(
            stage,
            &alloc::format!(
                "cannot {verb} the policy here -- that needs the views grant: `with admin with --{verb} ...`"
            ),
        );
        libkern::kprint(alloc::format!("with: cannot {verb} the policy without the views grant\n").as_bytes());
        exit(EXIT_FAILURE);
    }
    ch
}

/// `with --show [FILE]`: the policy, printed, or written to FILE for editing. **A file, not a
/// pipe**: the shell's `save` writes a record per line as `{ line: … }`, so a copy made through it
/// would not read back as a policy.
fn show(stage: &Stage, file: Option<&str>) -> ! {
    let ch = policy_endpoint(stage, "show");
    let text = match call(ch, OP_VIEWS_SHOW, 1, &[], &[]) {
        Some((false, body)) => body,
        _ => stage.die(b"with: the broker would not show the policy\n", EXIT_FAILURE),
    };
    match file {
        Some(f) => {
            let path = stage.path(f.as_bytes());
            if libfs::write_file(stage.namespace, &path, &text).is_err() {
                say(stage, &alloc::format!("cannot write `{f}`"));
                exit(EXIT_FAILURE);
            }
            say(stage, &alloc::format!("wrote the policy to {f} ({} bytes)", text.len()));
            libkern::kprint(alloc::format!("with: wrote the policy to a copy ({} bytes)\n", text.len()).as_bytes());
        }
        None => stage.diag(&text),
    }
    exit(EXIT_OK)
}

/// `with --install FILE`: check FILE and install it as the policy, through the broker.
fn install(stage: &Stage, file: &str) -> ! {
    let path = stage.path(file.as_bytes());
    let Ok(text) = libfs::read_file(stage.namespace, &path) else {
        say(stage, &alloc::format!("cannot read `{file}`"));
        exit(EXIT_FAILURE);
    };
    if text.len() > POLICY_MAX {
        say(stage, &alloc::format!("{file}: a policy is at most {POLICY_MAX} bytes"));
        exit(EXIT_FAILURE);
    }
    let ch = policy_endpoint(stage, "install");
    let (o, why) = match call(ch, OP_VIEWS_INSTALL, 1, &text, &[]) {
        Some((false, body)) => outcome(&body),
        _ => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    };
    say(stage, &alloc::format!("{file}: {why}"));
    // On the console too, since a terminal on a release image renders nothing a gate can read.
    // Escaped: a reason can quote the policy's own text back.
    libkern::debug::Line::new().s(b"with: install: ").untrusted(why.as_bytes()).end();
    exit(if o == Outcome::Started { EXIT_OK } else { EXIT_FAILURE })
}

/// `with VIEW PROGRAM ARGS…`: ask, prove, relay.
fn run(stage: &Stage, view: &str, program: &str, args: &[&str]) -> ! {
    let ch = broker(stage);
    // **A copy of this namespace, not this namespace** — the one a process is spawned with cannot
    // be sent, and the broker copies what it is sent again anyway.
    // SAFETY: a namespace handle this process holds.
    let copy = unsafe { syscall1(SYS_NS_DERIVE, stage.namespace) };
    if copy <= 0 {
        stage.die(b"with: could not copy this session's namespace\n", EXIT_FAILURE);
    }
    let mut env = Vec::new();
    let _ = write_value(&mut env, &Value::Record(Arc::new(stage.env.clone())));
    // The program's streams are `with`'s own: stdin and stdout move to it. `stderr` and the
    // terminal go as **duplicates** — `with` still needs one to report on and the other to ask
    // for a password on.
    let term = stage.terminal.unwrap_or(0);
    let stderr = stage.streams.stderr.map(dup).filter(|&d| d != 0);
    if stage.streams.stderr.is_some() && stderr.is_none() {
        // **Said, not degraded quietly**, as `nxsh` says it of a stage: a program with no `stderr`
        // writes to the kernel log, which on a machine with no serial port reaches nobody. Until
        // the shell gave its stages `DUPLICATE` this was every program `with` ran, and the first
        // install from a view printed nothing after the password (2026-09-30).
        say(stage, &alloc::format!("`{program}` gets no diagnostic channel; its messages go to the kernel log"));
    }
    // **A token from this terminal**, so a password typed here can be remembered for it. The broker
    // redeems it with the terminal server itself; the terminal handle below is not asked which
    // terminal it is. None, and the request is asked for a password as before.
    let token = (term != 0).then(|| libviews::token(term, token_deadline())).flatten();
    let handed = Handed {
        ns: copy as u64,
        stdin: stage.streams.stdin,
        stdout: stage.streams.stdout,
        stderr,
        terminal: Some(term).filter(|&t| t != 0).map(dup).filter(|&d| d != 0),
        token,
    };
    let (mut o, mut why) = match libviews::request(ch, view, program, args, &env, handed, u64::MAX) {
        Ok(answer) => answer,
        Err(libviews::Failed::TooLarge) => stage.die(b"with: the request is too large\n", EXIT_USAGE),
        Err(_) => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    };
    let mut rid = REQUEST_ID;
    let mut tries = 0u8;
    while o == Outcome::NeedPassword || o == (Outcome::Denied { retry: true }) {
        if term == 0 {
            let why = b"with: a password is needed and there is no terminal to ask on\n";
            stage.die(why, EXIT_FAILURE);
        }
        tries += 1;
        // **Why the last one was refused goes on the terminal, in the same write as the next
        // prompt.** Sent to `stderr` it reached the screen whenever the shell next drained that
        // sink — which could be after the prompt, so a person saw the second prompt and then
        // "wrong password" under it, and `test-interactive` scanned past the prompt it was about
        // to wait for. One channel, one write: the order is the order it was written in.
        let mut prompt = String::new();
        if o == (Outcome::Denied { retry: true }) {
            prompt.push_str("with: ");
            prompt.push_str(&why);
            prompt.push_str("\r\n");
        }
        prompt.push_str(&alloc::format!("[with {view}] password ({tries} of {TRIES}): "));
        let Some(mut pw) = ask_password(term, prompt.as_bytes()) else {
            // Nobody answered. Closing the channel is the broker's cue to drop the request.
            stage.die(b"with: cancelled\n", EXIT_FAILURE);
        };
        rid += 1;
        let answer = libviews::password(ch, rid, &pw);
        scrub(&mut pw);
        (o, why) = match answer {
            Ok(answer) => answer,
            Err(_) => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
        };
    }
    if o != Outcome::Started {
        say(stage, &why);
        exit(EXIT_FAILURE);
    }
    // The program has the terminal now; `with` asks nothing more.
    close(term);
    wait_for_exit(stage, ch)
}

/// Relay the program's exit status, and pass a request to stop on to it.
fn wait_for_exit(stage: &Stage, ch: u64) -> ! {
    let mut asked = false;
    loop {
        let Some(i) = wait(&[ch, stage.notif]) else {
            exit(EXIT_FAILURE);
        };
        if i == 1 {
            // A request to stop, from the shell: pass it on, once. The program decides.
            loop {
                // SAFETY: NOTIF is a valid 64-byte out-param.
                let r =
                    unsafe { syscall4(SYS_NOTIF_RECV, stage.notif, (&raw mut NOTIF) as u64, 0, 0) };
                if r != 0 {
                    break;
                }
                // SAFETY: the kernel wrote a notification into NOTIF.
                let kind = unsafe { (&raw const NOTIF.kind).read() };
                if kind == KIND_TERMINATE_REQUESTED && !asked {
                    asked = true;
                    let _ = send(ch, OP_VIEWS_STOP, 99, &[], &[]);
                }
            }
            continue;
        }
        match recv(ch) {
            Ok(Some((OP_VIEWS_EXITED, 0, _, body))) => {
                let (code, crashed) = parse_exited(&body).unwrap_or((1, true));
                exit(if crashed { EXIT_FAILURE } else { code as i64 })
            }
            Ok(_) => continue,
            // The broker went away before saying: the program's fate is unknown.
            Err(()) => exit(EXIT_FAILURE),
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    libkern::kprint(b"with: panic\n");
    exit(EXIT_FAILURE)
}
