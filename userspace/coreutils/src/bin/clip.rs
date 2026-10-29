//! `clip` — read and write the clipboard from a pipeline.
//!
//! A clipboard that only graphical applications can reach would be the first resource in this
//! system that a pipeline cannot. A resource server speaks its own ops, as the tty does, so shell
//! access is a small utility either side of the pipe rather than a file interface.
//!
//! ## Deliberate behaviours
//!
//! - **`clip` is a paste and takes no serial.** An ordinary paste asks for index 0 and gets
//!   whatever was last copied; the serial exists for *cycling* inside one application.
//! - **The output is a text-fallback stream**, one row per line (`libstream::write_text_fallback`),
//!   so every generic operator works on it and `nxsh` prints exactly what was copied.
//! - **`--list` says how long each entry is, not what it says.** The server's `List` carries no
//!   bytes — a reply holding [`CLIP_RING`] entries at the cap would not fit one message — and
//!   showing a prefix of somebody's password would be worse than a length.
//! - **`--copy` renders, because a clipboard holds text**: each row of the typed upstream
//!   stream becomes a line, fields joined by tabs. A stream that was already text (one
//!   `line: String` column) comes back out byte-identical.
//! - **An entry over the cap is refused by the server**, not truncated here, so
//!   `TODO(clipboard-chunking)`'s trigger stays observable.

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

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Cap on the upstream stream read before rendering; the same as nxsh's capture cap.
const MAX_CAPTURE: usize = 1 << 20;

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
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
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
        // Three causes, and the message says which: a session built without a clipboard, a
        // server that did not start, and a malformed reply are different repairs.
        Err(ClipError::Transport(_)) => stage.die(b"clip: no /dev/clipboard in this namespace\n", EXIT_FAILURE),
        Err(_) => stage.die(b"clip: the clipboard did not answer\n", EXIT_FAILURE),
    };

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
/// An index past the ring is a `NotFound` from the server; past `u32` the protocol cannot carry it.
fn parse_index(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok()
}

fn do_paste(stage: &Stage, clip: &mut Clipboard<'_>, index: u32) -> ! {
    let mut bytes = [0u8; MAX_CLIP_BYTES];
    let (_, kind, len) = match clip.paste(index, CLIP_ANY_SERIAL, &mut bytes) {
        Ok(t) => t,
        Err(e) if e.is_empty() => {
            // **Not an error**: an empty ring, or an index past the end, gives an empty stream
            // and exit zero, so a script need not special-case "nothing copied yet".
            emit_text(stage, "");
            exit(EXIT_OK)
        }
        Err(_) => stage.die(b"clip: the clipboard refused the read\n", EXIT_FAILURE),
    };
    if kind != CLIP_KIND_TEXT {
        // This program only speaks text; saying so beats printing an image's bytes at a terminal.
        stage.die(b"clip: the newest entry is not text\n", EXIT_FAILURE);
    }
    if len > bytes.len() {
        // Cannot happen while the server caps at `MAX_CLIP_BYTES`; `paste` returns the **whole**
        // length so a too-small buffer is not pasted truncated.
        stage.die(b"clip: the entry is larger than this program can hold\n", EXIT_FAILURE);
    }
    match core::str::from_utf8(&bytes[..len]) {
        Ok(text) => emit_text(stage, text),
        Err(_) => stage.die(b"clip: the entry is not valid UTF-8\n", EXIT_FAILURE),
    }
    exit(EXIT_OK)
}

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
                    tw.write_row(&[Value::Int(i as i64), Value::Int(r.kind as i64), Value::Int(r.len as i64)])?;
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

fn do_copy(stage: &Stage, clip: &mut Clipboard<'_>) -> ! {
    let Some(stdin) = stage.streams.stdin else {
        // `clip --copy` with nothing upstream would otherwise block forever on a stream that
        // will never arrive.
        stage.die(b"clip: --copy needs something upstream (try `... | clip --copy`)\n", EXIT_USAGE);
    };
    let bytes = match ChannelReceiver::new(IpcPort::new(stdin)).receive(MAX_CAPTURE) {
        Ok(b) => b,
        Err(_) => stage.die(b"clip: could not read the upstream stream\n", EXIT_FAILURE),
    };
    let text = match render(&bytes) {
        Some(t) => t,
        None => stage.die(b"clip: the upstream stream did not decode\n", EXIT_FAILURE),
    };
    if text.len() > MAX_CLIP_BYTES {
        stage.die(b"clip: that is larger than one clipboard entry (TODO(clipboard-chunking))\n", EXIT_FAILURE);
    }
    match clip.copy(CLIP_KIND_TEXT, text.as_bytes()) {
        Ok(_) => exit(EXIT_OK),
        Err(_) => stage.die(b"clip: the clipboard refused the copy\n", EXIT_FAILURE),
    }
}

/// Turn a typed stream into one line per row, fields joined by tabs. **A text-fallback stream
/// comes back byte-identical**: a one-field row joins to itself.
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

/// Render one field as a person would read it. The composite kinds get a placeholder, not a
/// serialisation: `[list]` is a visibly wrong paste where wire bytes would be an invisible one.
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

fn push_int(out: &mut String, mut v: i64) {
    if v < 0 {
        out.push('-');
        // `i64::MIN` has no positive counterpart; the digits below read the negation unsigned.
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
