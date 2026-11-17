//! `man` — read the manual.
//!
//! The pages are files under `/system/manual`, one directory a section, staged from the
//! repository's `docs/manual` and its specs. The format and its rendering are
//! [`coreutils::manual`]'s; this is what reaches the filesystem.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::args::{Flag, parse};
use coreutils::manual;
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Port, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libos::boot::Start;
use libos::{exit, log};
use libstream::channel::ChannelSink;
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The width a page is rendered at: a terminal's size is not known to a program yet.
const WIDTH: usize = 80;

const FLAGS: [Flag; 1] =
    [Flag { long: "keyword", short: 'k', help: "list the pages whose name or summary holds WORD" }];

const HELP: &[u8] = b"usage: man [SECTION] NAME\n       man [--keyword WORD]\n\
    \n\
    Print a page of the manual, the first of that name in sections 1, 8, 2, 5, 7 or\n\
    the one in SECTION. With no NAME, list every page as\n\
    Table<{name: String, section: Int, summary: String}>.\n\
    \n\
      -k, --keyword     list the pages whose name or summary holds WORD\n\
          --help        show this help and exit\n\
          --version     show version information and exit\n";

const VERSION: &[u8] = b"man (nitrox coreutils) 0.1.0\n";

/// One page found under [`manual::ROOT`].
struct Page {
    section: u8,
    name: String,
    text: String,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);

    let args = match parse(&stage.argv, &FLAGS) {
        Ok(a) => a,
        Err(_) => stage.die(b"man: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(HELP);
    }
    if args.version() {
        stage.answer(VERSION);
    }
    let pages = pages(&stage);
    if pages.is_empty() {
        stage.die(b"man: no manual here (/system/manual is empty or not bound)\n", EXIT_FAILURE);
    }

    let ops: Vec<&str> = args.operands.iter().map(String::as_str).collect();
    if args.has("keyword") {
        let [word] = ops.as_slice() else { stage.die(b"man: --keyword takes one WORD (try --help)\n", EXIT_USAGE) };
        let found: Vec<&Page> =
            pages.iter().filter(|p| manual::mentions(&p.name, &manual::summary(&p.text), word)).collect();
        list(&stage, &found);
    }
    let (section, name) = match ops.as_slice() {
        [] => list(&stage, &pages.iter().collect::<Vec<_>>()),
        [name] => (None, *name),
        [section, name] => match section.parse::<u8>() {
            Ok(s) => (Some(s), *name),
            Err(_) => stage.die(b"man: SECTION is a number (try --help)\n", EXIT_USAGE),
        },
        _ => stage.die(b"man: too many operands (try --help)\n", EXIT_USAGE),
    };
    let names: Vec<(u8, String)> = pages.iter().map(|p| (p.section, p.name.clone())).collect();
    let Some(i) = manual::choose(&names, section, name) else {
        let mut msg = String::from("man: no manual page for ");
        msg.push_str(name);
        msg.push('\n');
        stage.die(msg.as_bytes(), EXIT_FAILURE);
    };
    stage.answer(manual::render(&pages[i].text, WIDTH, false).as_bytes())
}

/// Every page under [`manual::ROOT`], by section and then by name.
fn pages(stage: &Stage) -> Vec<Page> {
    let mut out = Vec::new();
    let Ok(mut sections) = libfs::list_directory(&stage.namespace, manual::ROOT) else { return out };
    sections.sort_by(|a, b| a.name().cmp(b.name()));
    for s in &sections {
        let Some(section) = core::str::from_utf8(s.name()).ok().and_then(|n| n.parse::<u8>().ok()) else { continue };
        let dir = libfs::join(manual::ROOT.as_bytes(), s.name());
        let Ok(mut files) = libfs::list_directory(&stage.namespace, &dir) else { continue };
        files.sort_by(|a, b| a.name().cmp(b.name()));
        for f in &files {
            let Some(name) = core::str::from_utf8(f.name()).ok().and_then(|n| n.strip_suffix(".md")) else { continue };
            let Ok(bytes) = libfs::read(&stage.namespace, &libfs::join(dir.as_bytes(), f.name())) else { continue };
            let text = String::from_utf8_lossy(libfs::unpadded(&bytes)).into_owned();
            out.push(Page { section, name: String::from(name), text });
        }
    }
    out
}

/// Emit `pages` as the table `man` lists, and exit.
fn list(stage: &Stage, pages: &[&Page]) -> ! {
    let Some(port) = stage.stdout() else {
        for p in pages {
            let mut line = p.name.clone();
            line.push('\n');
            log::write(line.as_bytes());
        }
        exit(EXIT_OK)
    };
    emit(stage, port, pages);
    exit(EXIT_OK)
}

fn emit(stage: &Stage, stdout: Port<'_>, pages: &[&Page]) {
    let schema = Schema::new()
        .field("name", TypeTag::String, TypeModifiers::NONE)
        .field("section", TypeTag::Int, TypeModifiers::NONE)
        .field("summary", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(stdout, IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for p in pages {
            let row =
                [Value::Str(p.name.clone()), Value::Int(i64::from(p.section)), Value::Str(manual::summary(&p.text))];
            tw.write_row(&row)?;
        }
        tw.finish_with_status(0)
    });
    match wrote.and_then(|()| tw.into_sink().finish()) {
        Ok(()) | Err(libstream::wire::WireError::PeerClosed) => {}
        Err(_) => stage.die(b"man: write failed\n", EXIT_FAILURE),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"man: panic\n");
    exit(EXIT_FAILURE)
}
