//! `with` — run a program in a **view**: this session's namespace plus what the view grants, when
//! `/system/views.toml` says the person asking may.
//!
//! **The policy is changed by `--show` and `--install`**, not edited in place: `--show FILE`
//! writes a copy, any editor changes it, and `--install FILE` has the broker check it — it must
//! read, and an account that exists must still be able to administer — and replace the policy
//! atomically. Both reach the broker through `/dev/policy`, which only a view
//! with the `views` grant binds, so outside one they say which view to use.
//!
//! **What `with` never gets is the authority.** It sends the view broker a *copy* of its own
//! namespace (`sys_ns_derive`); the broker builds the view there, and the program runs in a
//! namespace `with` holds no handle to. `with` relays: its streams move to the program, and what
//! comes back is an exit status.
//!
//! **It reads the password, not the broker** — on the terminal its shell handed it, echo off, with
//! `libprompt`. The broker holds each check for the session's delay after a wrong one, and ends
//! a request after three.
//!
//! **The request itself is `libviews`'**: the desktop's Restart and Shut down make the same one,
//! for `shutdown` in the `power` view. `with`'s own are the prompt, the relay and the policy.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::{IPC_PAYLOAD_SIZE, KIND_TERMINATE_REQUESTED};
use libkern::{Rights, scrub};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Carried, Channel, Deadline, Handle, Only, WaitSet, exit, time};
use libprompt::ask_password_on;
use libprompt::ipc::{call_on, lookup_in, ready, receive, transmit};
use librsproto::views::*;
use libstream::channel::ChannelSink;
use libstream::diag::Level;
use libstream::table::TableWriter;
use libstream::wire::{Value, write_value};
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag};
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

/// What a session channel to the broker is asked for.
const SESSION: Rights = Rights::from_bits(Rights::SEND.bits() | Rights::RECV.bits() | Rights::WAIT.bits());

/// When a token from this terminal stops being worth waiting for: two seconds from now.
fn token_deadline() -> u64 {
    time::now().unwrap_or(0).saturating_add(2_000_000_000)
}

/// A second handle to `h` with every right it has, to hand over while `with` keeps its own.
fn dup(h: &Handle<Channel, Only>) -> Option<Handle<Channel, Only>> {
    h.duplicate(Rights::from_bits(u64::MAX)).ok()
}

fn broker(stage: &Stage) -> Handle<Channel, Only> {
    let Some(ch) = lookup_in(&stage.namespace, "/dev/views", SESSION) else {
        stage.die(b"with: this session has no view broker (/dev/views)\n", EXIT_FAILURE);
    };
    ch
}

fn say(stage: &Stage, what: &str) {
    say_at(stage, Level::Error, what);
}

fn say_at(stage: &Stage, level: Level, what: &str) {
    let mut line = String::from("with: ");
    line.push_str(what);
    line.push('\n');
    stage.diag_at(level, line.as_bytes());
}

/// A broker's answer said in words: a notice when it did what was asked, an error when it did not.
fn level_of(o: Outcome) -> Level {
    if o == Outcome::Started { Level::Notice } else { Level::Error }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let mut stage = Stage::enter(start);
    // Owned, so `run` can take the stage's streams while it holds the arguments.
    let words: Vec<String> = stage.argv.iter().skip(1).cloned().collect();
    let argv: Vec<&str> = words.iter().map(|s| s.as_str()).collect();
    match argv.first().copied() {
        Some("--help") => stage.answer(HELP),
        Some("--version") => stage.answer(VERSION),
        Some("--list") if argv.len() == 1 => list(&stage),
        Some("--forget") if argv.len() == 1 => forget(&stage),
        Some("--check") if argv.len() == 2 => check(&stage, argv[1]),
        Some("--show") if argv.len() <= 2 => show(&stage, argv.get(1).copied()),
        Some("--install") if argv.len() == 2 => install(&stage, argv[1]),
        Some(flag) if flag.starts_with("--") => stage.die(HELP, EXIT_USAGE),
        Some(_) if argv.len() >= 2 => run(&mut stage, argv[0], argv[1], &argv[2..]),
        _ => stage.die(HELP, EXIT_USAGE),
    }
}

/// `with --forget`: every password this session has remembered, every terminal's, forgotten — as
/// `sudo -k`, and silent as it is.
fn forget(stage: &Stage) -> ! {
    let ch = broker(stage);
    match libviews::forget(ch.raw().0, u64::MAX) {
        Ok(()) => exit(EXIT_OK),
        Err(_) => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    }
}

fn list(stage: &Stage) -> ! {
    let ch = broker(stage);
    let rows = match libviews::list(ch.raw().0, u64::MAX) {
        Ok(rows) => rows,
        Err(libviews::Failed::Refused) => {
            stage.die(b"with: the broker could not list your views (does the policy read?)\n", EXIT_FAILURE)
        }
        Err(libviews::Failed::Garbled) => stage.die(b"with: the broker's listing did not read\n", EXIT_FAILURE),
        Err(_) => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    };
    match stage.stdout() {
        Some(port) => {
            let schema = Schema::new()
                .field("view", TypeTag::String, TypeModifiers::NONE)
                .field("run", TypeTag::String, TypeModifiers::NONE)
                .field("password", TypeTag::Bool, TypeModifiers::NONE);
            let mut tw = TableWriter::new(ChannelSink::new(port, IPC_PAYLOAD_SIZE));
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
                say_at(stage, Level::Notice, l);
            }
        }
    }
    exit(EXIT_OK)
}

fn check(stage: &Stage, file: &str) -> ! {
    let path = stage.path(file.as_bytes());
    let Ok(text) = libfs::read(&stage.namespace, &path) else {
        say(stage, &alloc::format!("cannot read `{file}`"));
        exit(EXIT_FAILURE);
    };
    let ch = broker(stage);
    let (o, why) = match call_on(&ch, OP_VIEWS_CHECK, 1, &text, &mut Carried::new()) {
        Some((false, body)) => outcome(&body),
        _ => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    };
    say_at(stage, level_of(o), &alloc::format!("{file}: {why}"));
    exit(if o == Outcome::Started { EXIT_OK } else { EXIT_FAILURE })
}

/// The broker's policy endpoint, through the `views` grant's `/dev/policy`. Outside a view with the
/// grant there is none, and `with` says which view to use rather than failing without a word.
fn policy_endpoint(stage: &Stage, verb: &str) -> Handle<Channel, Only> {
    let Some(ch) = lookup_in(&stage.namespace, "/dev/policy", SESSION) else {
        say(
            stage,
            &alloc::format!(
                "cannot {verb} the policy here -- that needs the views grant: `with admin with --{verb} ...`"
            ),
        );
        log::write(alloc::format!("with: cannot {verb} the policy without the views grant\n").as_bytes());
        exit(EXIT_FAILURE);
    };
    ch
}

/// `with --show [FILE]`: the policy, printed, or written to FILE for editing. **A file, not a
/// pipe**: the shell's `save` writes a record per line as `{ line: … }`, so a copy made through it
/// would not read back as a policy.
fn show(stage: &Stage, file: Option<&str>) -> ! {
    let ch = policy_endpoint(stage, "show");
    let text = match call_on(&ch, OP_VIEWS_SHOW, 1, &[], &mut Carried::new()) {
        Some((false, body)) => body,
        _ => stage.die(b"with: the broker would not show the policy\n", EXIT_FAILURE),
    };
    match file {
        Some(f) => {
            let path = stage.path(f.as_bytes());
            if libfs::write(&stage.namespace, &path, &text).is_err() {
                say(stage, &alloc::format!("cannot write `{f}`"));
                exit(EXIT_FAILURE);
            }
            say_at(stage, Level::Notice, &alloc::format!("wrote the policy to {f} ({} bytes)", text.len()));
            log::write(alloc::format!("with: wrote the policy to a copy ({} bytes)\n", text.len()).as_bytes());
        }
        // On `stdout`, as what was asked for: text, however long — one diagnostic could hold only
        // a message's worth, and a longer policy went to the kernel log.
        None => {
            if !stage.text_out(&text) {
                stage.note(&text);
            }
        }
    }
    exit(EXIT_OK)
}

fn install(stage: &Stage, file: &str) -> ! {
    let path = stage.path(file.as_bytes());
    let Ok(text) = libfs::read(&stage.namespace, &path) else {
        say(stage, &alloc::format!("cannot read `{file}`"));
        exit(EXIT_FAILURE);
    };
    if text.len() > POLICY_MAX {
        say(stage, &alloc::format!("{file}: a policy is at most {POLICY_MAX} bytes"));
        exit(EXIT_FAILURE);
    }
    let ch = policy_endpoint(stage, "install");
    let (o, why) = match call_on(&ch, OP_VIEWS_INSTALL, 1, &text, &mut Carried::new()) {
        Some((false, body)) => outcome(&body),
        _ => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    };
    say_at(stage, level_of(o), &alloc::format!("{file}: {why}"));
    // On the console too, since a terminal on a release image renders nothing a gate can read.
    // Escaped: a reason can quote the policy's own text back.
    Line::new().s(b"with: install: ").untrusted(why.as_bytes()).end();
    exit(if o == Outcome::Started { EXIT_OK } else { EXIT_FAILURE })
}

fn run(stage: &mut Stage, view: &str, program: &str, args: &[&str]) -> ! {
    let ch = broker(stage);
    // **A copy of this namespace, not this namespace** — the one a process is spawned with cannot
    // be sent, and the broker copies what it is sent again anyway.
    let Ok(copy) = stage.namespace.copy() else {
        stage.die(b"with: could not copy this session's namespace\n", EXIT_FAILURE);
    };
    let mut env = Vec::new();
    let _ = write_value(&mut env, &Value::Record(Arc::new(stage.env.clone())));
    // The program's streams are `with`'s own: stdin and stdout move to it. `stderr` and the
    // terminal go as **duplicates** — `with` still needs one to report on and the other to ask
    // for a password on.
    let stderr = stage.streams.stderr.as_ref().and_then(dup);
    if stage.streams.stderr.is_some() && stderr.is_none() {
        // **Said, not degraded quietly**, as `nxsh` says it of a stage: a program with no `stderr`
        // writes to the kernel log, which on a machine with no serial port reaches nobody.
        let why = alloc::format!("`{program}` gets no diagnostic channel; its messages go to the kernel log");
        say_at(stage, Level::Warning, &why);
    }
    // **A token from this terminal**, so a password typed here can be remembered for it. The broker
    // redeems it with the terminal server itself; the terminal handle below is not asked which
    // terminal it is. None, and the request is asked for a password as before.
    let token = stage.terminal.as_ref().and_then(|t| libviews::token(t.raw().0, token_deadline()));
    let handed = Handed {
        ns: copy.into_raw().0,
        stdin: stage.streams.stdin.take().map(|h| h.into_raw().0),
        stdout: stage.streams.stdout.take().map(|h| h.into_raw().0),
        stderr: stderr.map(|h| h.into_raw().0),
        terminal: stage.terminal.as_ref().and_then(dup).map(|h| h.into_raw().0),
        token,
    };
    let (mut o, mut why) = match libviews::request(ch.raw().0, view, program, args, &env, handed, u64::MAX) {
        Ok(answer) => answer,
        Err(libviews::Failed::TooLarge) => stage.die(b"with: the request is too large\n", EXIT_USAGE),
        Err(_) => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    };
    let mut rid = REQUEST_ID;
    let mut tries = 0u8;
    while o == Outcome::NeedPassword || o == (Outcome::Denied { retry: true }) {
        let Some(term) = stage.terminal.as_ref() else {
            let why = b"with: a password is needed and there is no terminal to ask on\n";
            stage.die(why, EXIT_FAILURE);
        };
        tries += 1;
        // **Why the last one was refused goes on the terminal, in the same write as the next
        // prompt.** On `stderr` it could reach the screen after the prompt, and `test-interactive`
        // scanned past the prompt it was about to wait for.
        let mut prompt = String::new();
        if o == (Outcome::Denied { retry: true }) {
            prompt.push_str("with: ");
            prompt.push_str(&why);
            prompt.push_str("\r\n");
        }
        prompt.push_str(&alloc::format!("[with {view}] password ({tries} of {TRIES}): "));
        let Some(mut pw) = ask_password_on(term, prompt.as_bytes()) else {
            // Nobody answered. Closing the channel is the broker's cue to drop the request.
            stage.die(b"with: cancelled\n", EXIT_FAILURE);
        };
        rid += 1;
        let answer = libviews::password(ch.raw().0, rid, &pw);
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
    drop(stage.terminal.take());
    wait_for_exit(stage, ch)
}

fn wait_for_exit(stage: &Stage, ch: Handle<Channel, Only>) -> ! {
    let mut asked = false;
    loop {
        let mut set = WaitSet::<2>::new();
        let added = set.add(&ch).and_then(|_| stage.notif.as_ref().map_or(Ok(0), |n| set.add(n)));
        let Some(i) = added.ok().and_then(|_| ready(&set, Deadline::NEVER)) else {
            exit(EXIT_FAILURE);
        };
        if i == 1 {
            // A request to stop, from the shell: pass it on, once. The program decides.
            while let Some(n) = stage.notif.as_ref().and_then(|q| q.recv().ok().flatten()) {
                if n.kind == KIND_TERMINATE_REQUESTED && !asked {
                    asked = true;
                    let _ = transmit(&ch, OP_VIEWS_STOP, 99, &[], &mut Carried::new());
                }
            }
            continue;
        }
        match receive(&ch) {
            Ok(Some((OP_VIEWS_EXITED, 0, _, body))) => {
                let (code, crashed) = parse_exited(&body).unwrap_or((1, true));
                exit(if crashed { EXIT_FAILURE } else { code })
            }
            Ok(_) => continue,
            // The broker went away before saying: the program's fate is unknown.
            Err(_) => exit(EXIT_FAILURE),
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"with: panic\n");
    exit(EXIT_FAILURE)
}
