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
    let Some(ch) = open_desktop(ns) else {
        kprint(b"desktop: /dev/desktop did not resolve\n");
        stage.die(
            b"desktop: no /dev/desktop in this namespace -- not a graphical session\n",
            EXIT_FAILURE,
        );
    };

    let ops: alloc::vec::Vec<&str> = args.operands.iter().map(|s| s.as_str()).collect();
    let code = match ops.as_slice() {
        [] => list(&stage, ch),
        ["switch", n] => match parse_index(n) {
            Some(i) => switch(&stage, ch, i),
            None => stage.die(b"desktop: switch takes a position, starting at 1\n", EXIT_USAGE),
        },
        ["name", n, rest @ ..] if !rest.is_empty() => match parse_index(n) {
            Some(i) => name(&stage, ch, i, rest),
            None => stage.die(b"desktop: name takes a position, starting at 1\n", EXIT_USAGE),
        },
        _ => stage.die(b"desktop: bad operands (try --help)\n", EXIT_USAGE),
    };
    exit(code)
}

/// One-based position, refusing zero — the shell counts from one and so does the indicator.
fn parse_index(s: &str) -> Option<u32> {
    let n: u32 = s.parse().ok()?;
    (n >= 1).then_some(n)
}

/// Resolve `/dev/desktop` and return the session channel.
fn open_desktop(ns: u64) -> Option<u64> {
    use libkern::handle::{RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT};
    use libkern::syscall::{SYS_NS_LOOKUP, syscall4};
    let path = b"/dev/desktop";
    // SAFETY: a lookup on this process's own namespace, with a valid path slice.
    let pending = unsafe {
        syscall4(
            SYS_NS_LOOKUP,
            ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT,
        )
    };
    if pending < 0 {
        return None;
    }
    // **Async, like every potentially-blocking syscall**: the lookup returns a
    // `PendingOperation` and the answer is read out of the wait result, not an out-param.
    let mut results = [0u8; 24];
    let handles = [pending as u64];
    // SAFETY: waiting on the pending operation this process just created.
    unsafe {
        libkern::syscall::syscall4(
            libkern::syscall::SYS_WAIT,
            handles.as_ptr() as u64,
            1,
            results.as_mut_ptr() as u64,
            u64::MAX,
        )
    };
    let status = i32::from_le_bytes([results[8], results[9], results[10], results[11]]);
    let handle = u64::from_le_bytes([
        results[16], results[17], results[18], results[19], results[20], results[21],
        results[22], results[23],
    ]);
    (status == 0 && handle != 0).then_some(handle)
}

/// Why a request produced no reply body.
///
/// **The distinction is the point.** The first version collapsed all three into `None`, so a
/// channel that died and a shell that answered `Unsupported` both printed *no such desktop* —
/// a diagnosis of the operand for a fault that had nothing to do with it (PR #245 review,
/// finding 9). `Refused` carries the shell's own `KError` code so the message can name it.
enum ReqError {
    /// The send failed, the recv failed, or the reply did not decode: the channel is gone or
    /// the peer is not speaking this protocol.
    Transport,
    /// The shell replied with `RS_FLAG_ERROR`. The code is its `KError` as an `i32`, or `None`
    /// if the error reply carried no body.
    Refused(Option<i32>),
}

/// Send one request and return its reply body.
fn request(ch: u64, op: u16, body: &[u8], out: &mut [u8]) -> Result<usize, ReqError> {
    use libkern::abi::SENDMODE_NOBLOCK;
    use libkern::syscall::{SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_WAIT, syscall4, syscall5};
    let mut msg = alloc::vec![0u8; 4096];
    let Some(n) = librsproto::encode(&mut msg[24..], op, 1, 0, body, 0) else {
        return Err(ReqError::Transport);
    };
    msg[4..8].copy_from_slice(&(n as u32).to_le_bytes());
    let handles = [0u64; 1];
    // SAFETY: a send on a channel this process owns, with valid buffers.
    let r = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            msg.as_ptr() as u64,
            handles.as_ptr() as u64,
            0,
            SENDMODE_NOBLOCK,
        )
    };
    if r != 0 {
        return Err(ReqError::Transport);
    }
    // Wait for the reply rather than polling: the shell answers on its next loop pass.
    let wait = [ch];
    let mut results = [0u8; 24];
    // SAFETY: waiting on the channel handle this process owns.
    unsafe { syscall4(SYS_WAIT, wait.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
    let mut rmsg = alloc::vec![0u8; 4096];
    // Sized from the ABI rather than from what the shell is expected to send: `recv` copies
    // out the *sender's* count, bounded only by `IPC_HANDLE_MAX`.
    let mut rhandles = [0u64; libkern::abi::IPC_HANDLE_MAX];
    let mut rcount = 0usize;
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            rmsg.as_mut_ptr() as u64,
            rhandles.as_mut_ptr() as u64,
            (&raw mut rcount) as u64,
        )
    };
    if rr != 0 {
        return Err(ReqError::Transport);
    }
    let payload = u32::from_le_bytes([rmsg[4], rmsg[5], rmsg[6], rmsg[7]]) as usize;
    let Ok(m) = librsproto::decode(&rmsg[24..24 + payload.min(4096 - 24)]) else {
        return Err(ReqError::Transport);
    };
    if m.flags & librsproto::RS_FLAG_ERROR != 0 {
        // The shell's `bad()` sends the `KError` as four little-endian bytes.
        let code = (m.body.len() >= 4)
            .then(|| i32::from_le_bytes([m.body[0], m.body[1], m.body[2], m.body[3]]));
        return Err(ReqError::Refused(code));
    }
    let len = m.body.len().min(out.len());
    out[..len].copy_from_slice(&m.body[..len]);
    Ok(len)
}

/// `desktop` with no operands.
fn list(stage: &Stage, ch: u64) -> i64 {
    use librsproto::desktop::{DesktopList, OP_DESKTOP_LIST};
    let mut out = alloc::vec![0u8; 1024];
    let n = match request(ch, OP_DESKTOP_LIST, &[], &mut out) {
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
fn switch(stage: &Stage, ch: u64, index: u32) -> i64 {
    use librsproto::desktop::{DesktopIndex, OP_DESKTOP_SWITCH};
    let mut body = [0u8; 4];
    if (DesktopIndex { index }).write(&mut body).is_none() {
        return EXIT_FAILURE;
    }
    let mut out = [0u8; 16];
    if let Err(e) = request(ch, OP_DESKTOP_SWITCH, &body, &mut out) {
        return fail(stage, b"switch", e);
    }
    Line::new().s(b"desktop: switched to ").u(index as u64).end();
    EXIT_OK
}

/// `desktop name N LABEL...`.
fn name(stage: &Stage, ch: u64, index: u32, words: &[&str]) -> i64 {
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
    if let Err(e) = request(ch, OP_DESKTOP_NAME, &body, &mut out) {
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
fn fail(stage: &Stage, verb: &[u8], e: ReqError) -> i64 {
    let mut text = String::from("desktop: ");
    match e {
        ReqError::Refused(Some(c)) if c == libkern::error::KError::InvalidArgument.as_i32() => {
            text.push_str("no such desktop\n");
        }
        ReqError::Refused(code) => {
            text.push_str("the shell refused to ");
            text.push_str(core::str::from_utf8(verb).unwrap_or("act"));
            if let Some(c) = code {
                text.push_str(" (error ");
                push_num(&mut text, c.unsigned_abs());
                text.push(')');
            }
            text.push('\n');
        }
        ReqError::Transport => {
            text.push_str("lost the connection to the graphical session\n");
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
