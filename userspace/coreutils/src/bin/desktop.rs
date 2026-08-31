//! `desktop` — list, switch and name the graphical session's desktops.
//!
//! ```text
//! desktop                  # list them, marking the current one
//! desktop switch 2         # make the second one current
//! desktop name 2 code      # name it, which is what makes it persist
//! ```
//!
//! ## Why this exists at all
//!
//! **It is `/dev/desktop`'s first consumer, and it shipped in the same part as the binding.**
//! the `desktop-endpoint` deferral refused to bind an endpoint nothing resolved, on the grounds that
//! this milestone had three times shipped a capability that was specified, tested in isolation,
//! and unreachable on the path a caller actually uses. So the endpoint and something that
//! reaches it land together, and if this command had slipped the binding would have slipped
//! with it.
//!
//! It is also the first evidence that desktops are a *resource* rather than a shell feature:
//! the same model the bar and the overview drive is reachable from a command line, through a
//! namespace path, by an ordinary program with no special authority beyond what its namespace
//! was given.
//!
//! ## Naming is the interesting verb
//!
//! An unnamed desktop disappears when its last window leaves; a named one stays. So `desktop
//! name` is not cosmetic — it is how a desktop is made to persist, which is
//! `ui-composition-model.md` §6's "name it if it turns out to matter" turned into the lifecycle
//! rule itself.
//!
//! ## What is no longer here
//!
//! **The resolve, the send, the `sys_wait` and the reply decode.** They were this file's until
//! M10 Part D made `nxfiles` the second client of `/dev/desktop` — a browser asking the shell to
//! open a file — and they live in [`librsproto::desktop::Desktop`] now. The code is the same
//! code; what changed is that there is one of it. Even the error type came along, because the
//! distinction it draws (transport, refusal, and the refusal's `KError`) was learned here and
//! would have had to be learned again.
//!
//! ## Positions, not ids
//!
//! Every operand is a **position**, one-based, as the indicator counts them. Ids are stable and
//! never reused, so after a few desktops have come and gone they stop matching what anyone sees.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use coreutils::args::parse;
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::{exit, kprint};
use libkern::debug::Line;
use librsproto::desktop::{Desktop, DesktopError};

/// `alloc` backing.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const HELP: &[u8] = b"usage: desktop [switch N | name N LABEL]\n\
    \n\
    With no operands, list the session's desktops and mark the current one.\n\
    switch N    make the Nth desktop current\n\
    name N L    name the Nth desktop, which is what makes it persist\n\
    \n\
    N is a position as the desktop indicator counts them, starting at 1.\n\
    \n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"desktop (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);
    let args = match parse(&stage.argv, &[]) {
        Ok(a) => a,
        Err(_) => stage.die(b"desktop: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }

    // **Every diagnostic here goes to the debug console as well as to the stage.** A command
    // run in a windowed terminal writes into that terminal's grid, and the grid renders under
    // `test-harness` only — so on a release image, which is what `check-login` boots, the
    // stage's output is invisible and a failure would look like the command never ran.
    kprint(b"desktop: running\n");
    let mut buf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
    let Ok(mut desktop) = Desktop::connect(ns, &mut buf) else {
        kprint(b"desktop: /dev/desktop did not resolve\n");
        stage.die(
            b"desktop: no /dev/desktop in this namespace -- not a graphical session\n",
            EXIT_FAILURE,
        );
    };

    let ops: alloc::vec::Vec<&str> = args.operands.iter().map(|s| s.as_str()).collect();
    let code = match ops.as_slice() {
        [] => list(&stage, &mut desktop),
        ["switch", n] => match parse_index(n) {
            Some(i) => switch(&stage, &mut desktop, i),
            None => stage.die(b"desktop: switch takes a position, starting at 1\n", EXIT_USAGE),
        },
        ["name", n, rest @ ..] if !rest.is_empty() => match parse_index(n) {
            Some(i) => name(&stage, &mut desktop, i, rest),
            None => stage.die(b"desktop: name takes a position, starting at 1\n", EXIT_USAGE),
        },
        _ => stage.die(b"desktop: bad operands (try --help)\n", EXIT_USAGE),
    };
    desktop.close();
    exit(code)
}

/// One-based position, refusing zero — the shell counts from one and so does the indicator.
fn parse_index(s: &str) -> Option<u32> {
    let n: u32 = s.parse().ok()?;
    (n >= 1).then_some(n)
}

/// `desktop` with no operands.
fn list(stage: &Stage, desktop: &mut Desktop<'_>) -> i64 {
    use librsproto::desktop::{DesktopList, OP_DESKTOP_LIST};
    let mut out = alloc::vec![0u8; 1024];
    let n = match desktop.request(OP_DESKTOP_LIST, &[], &mut out) {
        Ok(n) => n,
        Err(e) => return fail(stage, b"list the desktops", e),
    };
    let Some(list) = DesktopList::read(&out[..n]) else {
        stage.diag(b"desktop: the shell's reply did not parse\n");
        return EXIT_FAILURE;
    };
    // **A `Table` on stdout, with text only when there is no stdout.** This is the one command
    // in `/bin` whose product is a table, and the first version wrote it to `stage.diag` —
    // stderr, which `pipeline-stdio.md` calls a *shared diagnostic sink, not a per-adjacency
    // pipe*. `desktop | sort name` produced nothing, and the listing interleaved with every
    // other stage's diagnostics on the shared channel (PR #245 review, finding 5). `list` and
    // `whoami` both branch on `stage.streams.stdout`; so does this now.
    let sink: &[u8] = match stage.streams.stdout {
        Some(h) => {
            emit(stage, h, &list);
            b"table"
        }
        None => {
            let mut text = String::new();
            for (i, e) in list.entries().enumerate() {
                let pos = i + 1;
                text.push(if pos as u32 == list.current { '*' } else { ' ' });
                text.push(' ');
                push_num(&mut text, pos as u32);
                text.push(' ');
                if e.name.is_empty() {
                    text.push_str("(unnamed)");
                } else {
                    text.push_str(e.name);
                }
                text.push('\n');
            }
            if list.truncated {
                text.push_str("  ... more than this command can show\n");
            }
            stage.diag(text.as_bytes());
            b"text"
        }
    };
    // **Also on the debug console.** A command run in a windowed terminal writes into that
    // terminal's grid, which renders only under `test-harness` — so a gate booting a release
    // image can see a stream's *contents* nowhere else. This is the one line that says the
    // endpoint answered.
    // **Naming the sink, which is the only way a gate can tell the two apart.** A release
    // image renders no grid, so the table's bytes are invisible from the host and both branches
    // otherwise print the identical line — an assertion that would have passed just as well for
    // the stderr version this replaced. `check-login` matches on `(table)`.
    Line::new()
        .s(b"desktop: listed ")
        .u(list.count as u64)
        .s(b" desktops (")
        .s(sink)
        .s(b")")
        .end();
    EXIT_OK
}

/// Emit the listing as `Table<{position: Int, id: Int, name: String, current: Bool}>`.
///
/// `position` first because it is what every other operand here means, and `current` as a
/// column rather than a marker so a pipeline can filter on it.
fn emit(stage: &Stage, stdout: u64, list: &librsproto::desktop::DesktopList<'_>) {
    use libkern::abi::IPC_PAYLOAD_SIZE;
    use libstream::channel::{ChannelSink, IpcPort};
    use libstream::table::TableWriter;
    use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

    let schema = Schema::new()
        .field("position", TypeTag::Int, TypeModifiers::NONE)
        .field("id", TypeTag::Int, TypeModifiers::NONE)
        .field("name", TypeTag::String, TypeModifiers::NONE)
        .field("current", TypeTag::Bool, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for (i, e) in list.entries().enumerate() {
            let pos = (i + 1) as i64;
            tw.write_row(&[
                Value::Int(pos),
                Value::Int(e.id as i64),
                Value::Str(String::from(e.name)),
                Value::Bool(pos as u32 == list.current),
            ])?;
        }
        tw.finish_with_status(0)
    });
    let flushed = wrote.and_then(|()| tw.into_sink().finish());
    match flushed {
        Ok(()) => {}
        Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"desktop: write failed\n", EXIT_FAILURE),
    }
}

/// `desktop switch N`.
fn switch(stage: &Stage, desktop: &mut Desktop<'_>, index: u32) -> i64 {
    use librsproto::desktop::{DesktopIndex, OP_DESKTOP_SWITCH};
    let mut body = [0u8; 4];
    if (DesktopIndex { index }).write(&mut body).is_none() {
        return EXIT_FAILURE;
    }
    let mut out = [0u8; 16];
    if let Err(e) = desktop.request(OP_DESKTOP_SWITCH, &body, &mut out) {
        return fail(stage, b"switch", e);
    }
    Line::new().s(b"desktop: switched to ").u(index as u64).end();
    EXIT_OK
}

/// `desktop name N LABEL...`.
fn name(stage: &Stage, desktop: &mut Desktop<'_>, index: u32, words: &[&str]) -> i64 {
    use librsproto::desktop::{DesktopIndex, MAX_DESKTOP_NAME, OP_DESKTOP_NAME};
    let mut label = String::new();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            label.push(' ');
        }
        label.push_str(w);
    }
    if label.len() > MAX_DESKTOP_NAME {
        stage.diag(b"desktop: that name is too long\n");
        return EXIT_USAGE;
    }
    let mut body = alloc::vec![0u8; 4 + label.len()];
    if (DesktopIndex { index }).write(&mut body).is_none() {
        return EXIT_FAILURE;
    }
    body[4..].copy_from_slice(label.as_bytes());
    let mut out = [0u8; 16];
    if let Err(e) = desktop.request(OP_DESKTOP_NAME, &body, &mut out) {
        return fail(stage, b"name that desktop", e);
    }
    Line::new().s(b"desktop: named ").u(index as u64).s(b" ").s(label.as_bytes()).end();
    EXIT_OK
}

/// Report a failed request, and return the exit status for it.
///
/// `verb` completes "desktop: could not <verb>". The refusal path names the operand only when
/// the shell said the operand was the problem: `InvalidArgument` is what its `bad()` sends for
/// a position that does not exist, and anything else gets its code printed rather than a
/// diagnosis this command is in no position to make.
fn fail(stage: &Stage, verb: &[u8], e: DesktopError) -> i64 {
    let mut text = String::from("desktop: ");
    match e {
        DesktopError::Refused(Some(c)) if c == libkern::error::KError::InvalidArgument.as_i32() => {
            text.push_str("no such desktop\n");
        }
        DesktopError::Refused(code) => {
            text.push_str("the shell refused to ");
            text.push_str(core::str::from_utf8(verb).unwrap_or("act"));
            if let Some(c) = code {
                text.push_str(" (error ");
                push_num(&mut text, c.unsigned_abs());
                text.push(')');
            }
            text.push('\n');
        }
        DesktopError::Transport(_) => {
            text.push_str("lost the connection to the graphical session\n");
        }
        // The request did not fit its buffer — this command's operands are a number and a
        // bounded name, so it means a bug here rather than anything about the session.
        DesktopError::Protocol => {
            text.push_str("could not encode that request\n");
        }
    }
    stage.diag(text.as_bytes());
    // Also on the console: in a windowed terminal a release build renders nothing, so this is
    // the only place the failure is visible to a gate.
    kprint(text.as_bytes());
    EXIT_FAILURE
}

/// Append a decimal number.
fn push_num(s: &mut String, mut n: u32) {
    let mut digits = [0u8; 10];
    let mut i = 0;
    if n == 0 {
        digits[0] = b'0';
        i = 1;
    }
    while n > 0 {
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        s.push(digits[i] as char);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"desktop: panic\n");
    exit(EXIT_FAILURE)
}
