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
use coreutils::manual::{self, Found, Request};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Port, Stage};
use libkern::abi::IPC_PAYLOAD_SIZE;
use libos::boot::Start;
use libos::{exit, log};
use libstream::channel::ChannelSink;
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The width a page is rendered at, assumed while a program cannot learn its terminal's,
/// `TODO(tty-size)`.
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
    let ops: Vec<&str> = args.operands.iter().map(String::as_str).collect();
    let request = manual::request(&ops, args.has("keyword")).unwrap_or_else(|e| stage.die(e.as_bytes(), EXIT_USAGE));
    let pages = pages(&stage).unwrap_or_else(|e| stage.die(e.as_bytes(), EXIT_FAILURE));
    if pages.is_empty() {
        stage.die(b"man: no manual here (/system/manual holds no page)\n", EXIT_FAILURE);
    }
    let (section, name) = match request {
        Request::List => list(&stage, &pages.iter().collect::<Vec<_>>()),
        Request::Keyword(word) => {
            list(&stage, &pages.iter().filter(|p| manual::mentions(&p.name, &p.summary(), word)).collect::<Vec<_>>())
        }
        Request::Page(section, name) => (section, name),
    };
    let page = manual::answer(&pages, section, name, WIDTH).unwrap_or_else(|e| stage.die(e.as_bytes(), EXIT_FAILURE));
    // A reader that has gone wanted no more; any other failure is one.
    match stage.text(page.as_bytes()) {
        Ok(true) | Err(libstream::wire::WireError::PeerClosed) => {}
        Ok(false) => log::write(page.as_bytes()),
        Err(_) => stage.die(b"man: write failed\n", EXIT_FAILURE),
    }
    exit(EXIT_OK)
}

/// Every page under [`manual::ROOT`], by name: one that cannot be read is kept, with why, so it is
/// reported as such rather than as absent. `Err` says why the directory could not be listed.
fn pages(stage: &Stage) -> Result<Vec<Found>, String> {
    let mut out = Vec::new();
    let mut files = libfs::list_directory(&stage.namespace, manual::ROOT)
        .map_err(|e| alloc::format!("man: cannot list {}: {e:?}\n", manual::ROOT))?;
    files.sort_by(|a, b| a.name().cmp(b.name()));
    for f in &files {
        let Some((name, section)) = core::str::from_utf8(f.name()).ok().and_then(manual::page_of) else { continue };
        let text = match libfs::read(&stage.namespace, &libfs::join(manual::ROOT.as_bytes(), f.name())) {
            Ok(bytes) => Ok(String::from_utf8_lossy(libfs::unpadded(&bytes)).into_owned()),
            Err(e) => Err(alloc::format!("{e:?}")),
        };
        out.push(Found { section, name, text });
    }
    Ok(out)
}

/// Emit `pages` as the table `man` lists, and exit.
fn list(stage: &Stage, pages: &[&Found]) -> ! {
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

fn emit(stage: &Stage, stdout: Port<'_>, pages: &[&Found]) {
    let schema = Schema::new()
        .field("name", TypeTag::String, TypeModifiers::NONE)
        .field("section", TypeTag::Int, TypeModifiers::NONE)
        .field("summary", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(stdout, IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for p in pages {
            let row = [Value::Str(p.name.clone()), Value::Int(i64::from(p.section)), Value::Str(p.summary())];
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
