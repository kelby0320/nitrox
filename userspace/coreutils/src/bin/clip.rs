//! `clip` — read and write the clipboard from a pipeline.
//!
//! M12 decision 4, and the more interesting half of it: *"a clipboard that only graphical
//! applications can reach would be the first resource in this system that a pipeline cannot."*
//! There is no generic read/write verb here — a resource server speaks its own ops, as the tty
//! does — so shell access is a small utility either side of the pipe rather than a new file
//! interface.
//!
//! ```text
//! clip                       # write the newest entry to stdout
//! clip 2                     # write the third-newest (0 is the newest)
//! clip --list                # what the ring holds: index, kind, length
//! ... | clip --copy          # push the upstream stream's text onto the ring
//! ```
//!
//! ## Deliberate behaviours
//!
//! - **`clip` is a paste and takes no serial.** Decision 3: an ordinary paste asks for index 0
//!   and gets whatever was last copied, by anyone. The serial exists for *cycling*, which is a
//!   continuation of a paste inside one application — a shell command is never in the middle of
//!   one, so it never has one to carry.
//! - **The output is a text-fallback stream**, one row per line. That is the "Unix floor" this
//!   system defines for plain text (`libstream::write_text_fallback`), so every generic operator
//!   still works on it and `nxsh` prints exactly what was copied.
//! - **`--list` says how long each entry is, not what it says.** The server's `List` carries no
//!   bytes — a reply holding [`CLIP_RING`] entries at the cap would not fit one message — and a
//!   listing that quietly showed a prefix of somebody's password would be a worse answer than a
//!   length.
//! - **`--copy` renders, because a clipboard holds text.** The upstream stream is typed, so
//!   this turns each row into a line: fields joined by tabs, each rendered the way a person
//!   would read it. A stream that was already text (one `line: String` column, which is what
//!   every plain-text producer here emits) comes back out byte-identical.
//! - **An entry over the cap is refused by the server**, not truncated here. `TODO(clipboard-chunking)`
//!   is the trigger; a utility that silently copied the first 3964 bytes would make that trigger
//!   unobservable.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
use libkern::abi::{IPC_MSG_SIZE, IPC_PAYLOAD_SIZE};
use libkern::{exit, kprint};
use librsproto::clipboard::{
    CLIP_ANY_SERIAL, CLIP_KIND_TEXT, CLIP_RING, ClipError, ClipInfo, Clipboard, MAX_CLIP_BYTES,
};
use libstream::channel::{ChannelReceiver, ChannelSink, IpcPort};
use libstream::table::{TableReader, TableWriter, write_text_fallback};
use libstream::wire::Value;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag};

/// `alloc` backing: the TSM1 codec and the rendering both allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const COPY: Flag = Flag::new("copy", 'c', "read stdin and push it onto the ring");
const LIST: Flag = Flag::new("list", 'l', "describe the ring: index, kind, length");

const HELP: &[u8] = b"usage: clip [INDEX]\n\
           clip --copy\n\
           clip --list\n\
    \n\
    Read and write the clipboard (/dev/clipboard).\n\
    \n\
    With no operand, writes the newest entry to stdout as a text stream.\n\
    INDEX reaches further back: 0 is the newest, 1 the one before it.\n\
    \n\
      -c, --copy    read stdin and push its text onto the ring\n\
      -l, --list    emit Table<{index: Int, kind: Int, bytes: Int}>\n\
          --help    show this help and exit\n\
          --version show version information and exit\n";

const VERSION: &[u8] = b"clip (nitrox coreutils) 0.1.0\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let stage = Stage::enter(notif, ns, endpoint, arg0);

    let args = match parse(&stage.argv, &[COPY, LIST]) {
        Ok(a) => a,
        Err(_) => stage.die(b"clip: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.diag(HELP);
        exit(EXIT_OK);
    }
    if args.version() {
        stage.diag(VERSION);
        exit(EXIT_OK);
    }
    let (copying, listing) = (args.has("copy"), args.has("list"));
    if copying && listing {
        stage.die(b"clip: --copy and --list are different jobs (try --help)\n", EXIT_USAGE);
    }
    if (copying || listing) && !args.operands.is_empty() {
        stage.die(b"clip: an index is only meaningful for a paste (try --help)\n", EXIT_USAGE);
    }
    let index = match args.operands.first() {
        None => 0u32,
        Some(s) => match parse_index(s) {
            Some(i) => i,
            None => stage.die(b"clip: the index must be a number (try --help)\n", EXIT_USAGE),
        },
    };

    let mut buf = [0u8; IPC_MSG_SIZE];
    let mut clip = match Clipboard::connect(stage.namespace, &mut buf) {
        Ok(c) => c,
        // **Three causes, and the message says which class.** A session built without a
        // clipboard, a server that did not start, and a malformed reply are different repairs,
        // and "clipboard unavailable" for all three is the diagnosis-of-the-operand mistake
        // this tree keeps finding.
        Err(ClipError::Transport(_)) => {
            stage.die(b"clip: no /dev/clipboard in this namespace\n", EXIT_FAILURE)
        }
        Err(_) => stage.die(b"clip: the clipboard did not answer\n", EXIT_FAILURE),
    };

    // Each of the three diverges — they own the exit status, and a shared `exit(EXIT_OK)`
    // after them would be unreachable code claiming otherwise.
    if copying {
        do_copy(&stage, &mut clip)
    } else if listing {
        do_list(&stage, &mut clip)
    } else {
        do_paste(&stage, &mut clip, index)
    }
}

/// Parse a decimal index, refusing anything that is not entirely digits.
///
/// `str::parse` would do, but the bound matters: an index past the ring is a `NotFound` from
/// the server, and an index past `u32` is a number this protocol cannot carry at all.
fn parse_index(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok()
}

/// Write entry `index` to stdout as a text-fallback stream.
fn do_paste(stage: &Stage, clip: &mut Clipboard<'_>, index: u32) -> ! {
    let mut bytes = [0u8; MAX_CLIP_BYTES];
    let (_, kind, len) = match clip.paste(index, CLIP_ANY_SERIAL, &mut bytes) {
        Ok(t) => t,
        Err(e) if e.is_empty() => {
            // **Not an error.** An empty ring is a clipboard nobody has copied into, and an
            // index past the end is a question with a legitimate answer. Both produce an empty
            // stream and exit zero, so `clip | ...` in a script does not have to special-case
            // "nothing has been copied yet".
            emit_text(stage, "");
            exit(EXIT_OK)
        }
        Err(_) => stage.die(b"clip: the clipboard refused the read\n", EXIT_FAILURE),
    };
    if kind != CLIP_KIND_TEXT {
        // The tag exists so a second kind is not a second clipboard; this program only speaks
        // one of them, and saying so beats printing an image's bytes at a terminal.
        stage.die(b"clip: the newest entry is not text\n", EXIT_FAILURE);
    }
    if len > bytes.len() {
        // Cannot happen while the server caps at `MAX_CLIP_BYTES`, and worth checking anyway:
        // `paste` returns the entry's **whole** length precisely so a caller can tell that its
        // buffer was too small rather than pasting a truncated value silently.
        stage.die(b"clip: the entry is larger than this program can hold\n", EXIT_FAILURE);
    }
    match core::str::from_utf8(&bytes[..len]) {
        Ok(text) => emit_text(stage, text),
        Err(_) => stage.die(b"clip: the entry is not valid UTF-8\n", EXIT_FAILURE),
    }
    exit(EXIT_OK)
}

/// Write `text` as a text-fallback stream, one row per line.
fn emit_text(stage: &Stage, text: &str) {
    let lines: Vec<&str> = if text.is_empty() { Vec::new() } else { text.split('\n').collect() };
    match stage.streams.stdout {
        Some(h) => {
            let mut sink = ChannelSink::new(IpcPort::new(h), IPC_PAYLOAD_SIZE);
            let wrote = write_text_fallback(&mut sink, &lines, 0).and_then(|()| sink.finish());
            match wrote {
                Ok(()) | Err(libstream::wire::WireError::PeerClosed) => {}
                Err(_) => stage.die(b"clip: write failed\n", EXIT_FAILURE),
            }
        }
        None => {
            // The Tier-0 path — see `TODO(tier0-output-sink)`. The kernel log is scaffolding.
            for line in &lines {
                kprint(line.as_bytes());
                kprint(b"\n");
            }
        }
    }
}

/// Emit `Table<{index: Int, kind: Int, bytes: Int}>`, newest first.
fn do_list(stage: &Stage, clip: &mut Clipboard<'_>) -> ! {
    let mut rows = [ClipInfo::default(); CLIP_RING];
    let (_, n) = match clip.list(&mut rows) {
        Ok(t) => t,
        Err(_) => stage.die(b"clip: the clipboard refused the listing\n", EXIT_FAILURE),
    };
    match stage.streams.stdout {
        Some(h) => {
            let schema = Schema::new()
                .field("index", TypeTag::Int, TypeModifiers::NONE)
                .field("kind", TypeTag::Int, TypeModifiers::NONE)
                .field("bytes", TypeTag::Int, TypeModifiers::NONE);
            let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(h), IPC_PAYLOAD_SIZE));
            let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
                for (i, r) in rows[..n].iter().enumerate() {
                    tw.write_row(&[
                        Value::Int(i as i64),
                        Value::Int(r.kind as i64),
                        Value::Int(r.len as i64),
                    ])?;
                }
                tw.finish_with_status(0)
            });
            let flushed = wrote.and_then(|()| tw.into_sink().finish());
            match flushed {
                Ok(()) | Err(libstream::wire::WireError::PeerClosed) => {}
                Err(_) => stage.die(b"clip: write failed\n", EXIT_FAILURE),
            }
        }
        None => {
            for (i, r) in rows[..n].iter().enumerate() {
                let mut line = String::new();
                push_int(&mut line, i as i64);
                line.push(' ');
                push_int(&mut line, r.len as i64);
                line.push('\n');
                kprint(line.as_bytes());
            }
        }
    }
    exit(EXIT_OK)
}

/// Read stdin and push its text onto the ring.
fn do_copy(stage: &Stage, clip: &mut Clipboard<'_>) -> ! {
    let Some(stdin) = stage.streams.stdin else {
        // **Not a paste with a typo.** `clip --copy` with nothing upstream would otherwise
        // block forever on a stream that will never arrive, which reads as a hung shell.
        stage.die(b"clip: --copy needs something upstream (try `... | clip --copy`)\n", EXIT_USAGE);
    };
    let bytes = match ChannelReceiver::new(IpcPort::new(stdin)).receive() {
        Ok(b) => b,
        Err(_) => stage.die(b"clip: could not read the upstream stream\n", EXIT_FAILURE),
    };
    let text = match render(&bytes) {
        Some(t) => t,
        None => stage.die(b"clip: the upstream stream did not decode\n", EXIT_FAILURE),
    };
    if text.len() > MAX_CLIP_BYTES {
        // Refused rather than truncated — see this module's header.
        stage.die(b"clip: that is larger than one clipboard entry (TODO(clipboard-chunking))\n", EXIT_FAILURE);
    }
    match clip.copy(CLIP_KIND_TEXT, text.as_bytes()) {
        Ok(_) => exit(EXIT_OK),
        Err(_) => stage.die(b"clip: the clipboard refused the copy\n", EXIT_FAILURE),
    }
}

/// Turn a typed stream into the text a person would read: one line per row, fields joined by
/// tabs.
///
/// **A text-fallback stream comes back byte-identical**, which is the case that matters: every
/// plain-text producer in this system emits one `line: String` column, so joining "all the
/// fields" of a one-field row is the identity and the tabs never appear.
fn render(bytes: &[u8]) -> Option<String> {
    let mut tr = TableReader::new(bytes).ok()?;
    let mut out = String::new();
    let mut first = true;
    while let Some(item) = tr.next() {
        let row = match item {
            Ok(libstream::Item::Row(r)) => r,
            // A status marker or a decode failure: the rows before it are still what was
            // produced, so this stops rather than discarding them.
            _ => break,
        };
        if !first {
            out.push('\n');
        }
        first = false;
        for (i, v) in row.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            push_value(&mut out, v);
        }
    }
    Some(out)
}

/// Render one field the way a person would read it.
///
/// The composite kinds get a placeholder rather than a serialisation: a clipboard holds text,
/// and pasting `[list]` is a visibly wrong answer where pasting a struct's wire bytes is an
/// invisibly wrong one.
fn push_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => {}
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => push_int(out, *i),
        Value::Float(f) => push_int(out, *f as i64),
        Value::Str(s) => out.push_str(s),
        Value::Bytes(b) => {
            out.push_str("[bytes ");
            push_int(out, b.len() as i64);
            out.push(']');
        }
        Value::Handle(_) => out.push_str("[handle]"),
        Value::List(_) => out.push_str("[list]"),
        Value::Record(_) => out.push_str("[record]"),
        Value::Table(_) => out.push_str("[table]"),
    }
}

/// Append a decimal integer. `format!` would pull in `core::fmt`'s machinery for one number.
fn push_int(out: &mut String, mut v: i64) {
    if v < 0 {
        out.push('-');
        // `i64::MIN` has no positive counterpart; the wrapping negation is the standard
        // two's-complement handling and the digits below read it unsigned.
        v = v.wrapping_neg();
    }
    let mut digits = [0u8; 20];
    let mut n = 0;
    let mut u = v as u64;
    loop {
        digits[n] = b'0' + (u % 10) as u8;
        u /= 10;
        n += 1;
        if u == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(digits[i] as char);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"clip: panic\n");
    exit(EXIT_FAILURE)
}
