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
//! **It reads the password, not the broker** — on the terminal its shell handed it, echo off. The
//! broker holds each check for the session's delay after a wrong one, and ends a request after
//! three. **An older reader on the same backend gets the line first**: input goes to the oldest
//! terminal with a read pending, so a program still reading one — an earlier stage of this
//! pipeline, or one a previous command left behind — receives what is typed at the prompt. `sudo`
//! has the same limit; `docs/planning/administration.md` records it.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::{IPC_MSG_SIZE, IPC_PAYLOAD_SIZE, KIND_TERMINATE_REQUESTED, Notification};
use libkern::scrub;
use libkern::syscall::{
    SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE, SYS_HANDLE_DUPLICATE, SYS_NOTIF_RECV,
    SYS_NS_DERIVE, SYS_WAIT, syscall1, syscall2, syscall4, syscall5,
};
use libkern::{KError, RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT, SENDMODE_NOBLOCK, exit};
use librsproto::views::*;
use librsproto::{OP_TTY_INTERRUPT, OP_TTY_READ_LINE, OP_TTY_SET_MODE, OP_TTY_WRITE, TTY_MODE_ECHO};
use libstream::channel::{ChannelSink, IpcPort};
use libstream::table::TableWriter;
use libstream::wire::{Value, write_value};
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const HELP: &[u8] = b"usage: with VIEW PROGRAM [ARG...]\n\
    \x20      with --list\n\
    \x20      with --check FILE\n\
    \x20      with --show [FILE]\n\
    \x20      with --install FILE\n\
    \n\
    Run PROGRAM in VIEW: this session's namespace plus what the view grants,\n\
    when /system/views.toml says you may. Asks for your password when the\n\
    policy does. PROGRAM is a bare name; its output goes where with's would.\n\
    \n\
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

fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

fn dup(h: u64) -> u64 {
    // SAFETY: duplicating a handle this process owns, with the rights a hand-over needs.
    let d = unsafe { syscall2(SYS_HANDLE_DUPLICATE, h, u64::MAX) };
    if d > 0 { d as u64 } else { 0 }
}

/// Wait on `handles`; the index of one that is ready.
fn wait(handles: &[u64]) -> Option<usize> {
    let mut results = [0u8; 24 * 4];
    // SAFETY: valid buffers; at most four handles.
    let n = unsafe {
        syscall4(
            SYS_WAIT,
            handles.as_ptr() as u64,
            handles.len() as u64,
            results.as_mut_ptr() as u64,
            u64::MAX,
        )
    };
    if n < 1 {
        return None;
    }
    let h = u64::from_le_bytes(results[..8].try_into().ok()?);
    handles.iter().position(|&x| x == h)
}

/// A received message: `(op, request_id, is_error, body)`.
type Msg = (u16, u64, bool, Vec<u8>);

/// Receive one message on `ch`. `Ok(None)` if nothing was queued — a wake with nothing behind
/// it — and `Err(())` if the peer has gone, which are different answers to a caller waiting on
/// the program's exit.
fn recv(ch: u64) -> Result<Option<Msg>, ()> {
    let mut buf = [0u8; IPC_MSG_SIZE];
    let mut hs = [0u64; 8];
    let mut count = 0usize;
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            buf.as_mut_ptr() as u64,
            hs.as_mut_ptr() as u64,
            (&raw mut count) as u64,
        )
    };
    if rr == KError::PeerClosed.as_i32() as i64 {
        return Err(());
    }
    if rr != 0 {
        return Ok(None);
    }
    for &h in &hs[..count.min(8)] {
        close(h);
    }
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let msg = librsproto::decode(&buf[24..24 + len.min(IPC_PAYLOAD_SIZE)])
        .ok()
        .map(|m| (m.op, m.request_id, m.is_error(), m.body.to_vec()));
    // A line read at the password prompt came through here; the caller has its copy.
    scrub(&mut buf);
    Ok(msg)
}

/// Send `op` on `ch`, moving `handles`.
fn send(ch: u64, op: u16, request_id: u64, body: &[u8], handles: &[u64]) -> bool {
    let mut buf = [0u8; IPC_MSG_SIZE];
    let count = handles.len() as u16;
    let Some(n) = librsproto::encode(&mut buf[24..], op, request_id, 0, body, count) else {
        return false;
    };
    buf[4..8].copy_from_slice(&(n as u32).to_le_bytes());
    buf[8] = handles.len() as u8;
    // SAFETY: valid message buffer and handle array.
    let sent = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            buf.as_ptr() as u64,
            handles.as_ptr() as u64,
            handles.len() as u64,
            SENDMODE_NOBLOCK,
        ) == 0
    };
    // So did the password on its way to the broker.
    scrub(&mut buf);
    sent
}

/// Send `op` and wait for the reply to it, as `(is_error, body)`.
fn call(
    ch: u64,
    op: u16,
    request_id: u64,
    body: &[u8],
    handles: &[u64],
) -> Option<(bool, Vec<u8>)> {
    if !send(ch, op, request_id, body, handles) {
        return None;
    }
    loop {
        wait(&[ch])?;
        match recv(ch) {
            Ok(Some((_, rid, err, body))) if rid == request_id => return Some((err, body)),
            Ok(_) => continue,
            Err(()) => return None,
        }
    }
}

/// One exchange with the terminal, stepping over an `Interrupt` — which it records in
/// `interrupted`, since `Ctrl-C` at a password prompt means "never mind".
fn tty(term: u64, op: u16, body: &[u8], interrupted: &mut bool) -> Option<(bool, Vec<u8>)> {
    if !send(term, op, 1, body, &[]) {
        return None;
    }
    loop {
        wait(&[term])?;
        match recv(term) {
            Ok(Some((OP_TTY_INTERRUPT, 0, _, _))) => *interrupted = true,
            Ok(Some((_, 1, err, body))) => return Some((err, body)),
            Ok(_) => continue,
            Err(()) => return None,
        }
    }
}

/// Ask for a password on `term`, echo off. `None` if the person pressed `Ctrl-C` or `Ctrl-D`, or
/// the terminal failed.
fn ask_password(term: u64, prompt: &[u8]) -> Option<Vec<u8>> {
    let mut interrupted = false;
    let _ = tty(term, OP_TTY_SET_MODE, &[0], &mut interrupted);
    let _ = tty(term, OP_TTY_WRITE, prompt, &mut interrupted);
    let line = tty(term, OP_TTY_READ_LINE, &[], &mut interrupted);
    // **Echo back on before anything else**: this terminal goes to the program next, and an
    // elevated shell that inherited echo off would type blind.
    let _ = tty(term, OP_TTY_SET_MODE, &[TTY_MODE_ECHO], &mut interrupted);
    let _ = tty(term, OP_TTY_WRITE, b"\r\n", &mut interrupted);
    match line {
        Some((false, bytes)) if !interrupted => Some(bytes),
        Some((_, mut bytes)) => {
            scrub(&mut bytes);
            None
        }
        None => None,
    }
}

fn lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    let (st, h) = libfs::lookup_wait(ns, path, rights);
    if st == 0 { h } else { 0 }
}

/// The view broker, through this session's `/dev/views`.
fn broker(stage: &Stage) -> u64 {
    let ch = lookup(stage.namespace, b"/dev/views", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if ch == 0 {
        stage.die(b"with: this session has no view broker (/dev/views)\n", EXIT_FAILURE);
    }
    ch
}

fn outcome(body: &[u8]) -> (Outcome, String) {
    match parse_outcome(body) {
        Some((o, why)) => (o, String::from_utf8_lossy(why).into_owned()),
        None => {
            let why = String::from("the broker's answer did not read");
            (Outcome::Denied { retry: false }, why)
        }
    }
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
        Some("--check") if argv.len() == 2 => check(&stage, argv[1]),
        Some("--show") if argv.len() <= 2 => show(&stage, argv.get(1).copied()),
        Some("--install") if argv.len() == 2 => install(&stage, argv[1]),
        Some(flag) if flag.starts_with("--") => stage.die(HELP, EXIT_USAGE),
        Some(_) if argv.len() >= 2 => run(&stage, argv[0], argv[1], &argv[2..]),
        _ => stage.die(HELP, EXIT_USAGE),
    }
}

/// `with --list`: the views this session's person may use, as `Table<{view, run, password}>`.
fn list(stage: &Stage) -> ! {
    let ch = broker(stage);
    let Some((false, body)) = call(ch, OP_VIEWS_LIST, 1, &[], &[]) else {
        let why = b"with: the broker could not list your views (does the policy read?)\n";
        stage.die(why, EXIT_FAILURE);
    };
    let mut rows: Vec<(String, String, bool)> = Vec::new();
    let parsed = parse_rows(&body, |r| {
        rows.push((
            String::from_utf8_lossy(r.view).into_owned(),
            String::from_utf8_lossy(r.run).into_owned(),
            r.password,
        ))
    });
    if parsed.is_none() {
        stage.die(b"with: the broker's listing did not read\n", EXIT_FAILURE);
    }
    match stage.streams.stdout {
        Some(h) => {
            let schema = Schema::new()
                .field("view", TypeTag::String, TypeModifiers::NONE)
                .field("run", TypeTag::String, TypeModifiers::NONE)
                .field("password", TypeTag::Bool, TypeModifiers::NONE);
            let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(h), IPC_PAYLOAD_SIZE));
            let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
                for (view, run, password) in &rows {
                    let row =
                        [Value::Str(view.clone()), Value::Str(run.clone()), Value::Bool(*password)];
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
                .map(|(v, r, p)| {
                    alloc::format!("{v}  {r}  {}", if *p { "password" } else { "no password" })
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
    let mut bits = 0u8;
    let mut handles: Vec<u64> = alloc::vec![copy as u64];
    if let Some(h) = stage.streams.stdin {
        bits |= REQ_STDIN;
        handles.push(h);
    }
    if let Some(h) = stage.streams.stdout {
        bits |= REQ_STDOUT;
        handles.push(h);
    }
    if let Some(h) = stage.streams.stderr.map(dup).filter(|&d| d != 0) {
        bits |= REQ_STDERR;
        handles.push(h);
    }
    let term = stage.terminal.unwrap_or(0);
    if term != 0 {
        let d = dup(term);
        if d != 0 {
            bits |= REQ_TERMINAL;
            handles.push(d);
        }
    }
    let arg_bytes: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
    let mut body = alloc::vec![0u8; IPC_PAYLOAD_SIZE - 64];
    let (v, p) = (view.as_bytes(), program.as_bytes());
    let Some(n) = build_request(&mut body, bits, v, p, &arg_bytes, &env) else {
        stage.die(b"with: the request is too large\n", EXIT_USAGE);
    };
    let mut rid = 1u64;
    let (mut o, mut why) = match call(ch, OP_VIEWS_REQUEST, rid, &body[..n], &handles) {
        Some((false, body)) => outcome(&body),
        _ => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
    };
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
        let answer = call(ch, OP_VIEWS_PASSWORD, rid, &pw, &[]);
        scrub(&mut pw);
        (o, why) = match answer {
            Some((false, body)) => outcome(&body),
            _ => stage.die(b"with: the broker did not answer\n", EXIT_FAILURE),
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
