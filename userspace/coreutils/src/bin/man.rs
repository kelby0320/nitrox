//! `man` — read the manual.
//!
//! The pages are files under `/system/manual`, one directory a section, staged from the
//! repository's `docs/manual` and its specs. The format and its rendering are
//! [`coreutils::manual`]'s; this is what reaches the filesystem. Where the shell would show its
//! output on a terminal, it is paged there ([`coreutils::pager`]).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use coreutils::manual::{self, Found, Request};
use coreutils::pager::{self, Shown};
use coreutils::stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Port, Stage};
use coreutils::usage::MAN;
use libkern::abi::IPC_PAYLOAD_SIZE;
use libos::boot::Start;
use libos::{exit, log};
use libstream::channel::ChannelSink;
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The width a page is rendered at when its terminal cannot say, as the serial console cannot.
const WIDTH: usize = 80;

#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let stage = Stage::enter(start);

    let args = match MAN.parse(&stage.argv) {
        Ok(a) => a,
        Err(_) => stage.die(b"man: unrecognized option (try --help)\n", EXIT_USAGE),
    };
    if args.help() {
        stage.answer(MAN.help().as_bytes());
    }
    if args.version() {
        stage.answer(MAN.version().as_bytes());
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
    let width = stage.terminal_size().map_or(WIDTH, |s| usize::from(s.cols));
    let page = manual::answer(&pages, section, name, width).unwrap_or_else(|e| stage.die(e.as_bytes(), EXIT_FAILURE));
    let lines: Vec<String> = page.lines().map(String::from).collect();
    if pager::show(&stage, &lines, 0) == Shown::Paged {
        exit(EXIT_OK);
    }
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

/// Emit `pages` as the table `man` lists, or page it, and exit.
fn list(stage: &Stage, pages: &[&Found]) -> ! {
    let table = listing(pages);
    let (lines, head) = pager::table_lines(&table);
    if pager::show(stage, &lines, head) == Shown::Paged {
        exit(EXIT_OK);
    }
    let Some(port) = stage.stdout() else {
        for p in pages {
            let mut line = p.name.clone();
            line.push('\n');
            log::write(line.as_bytes());
        }
        exit(EXIT_OK)
    };
    emit(stage, port, &table);
    exit(EXIT_OK)
}

fn listing(pages: &[&Found]) -> Table {
    let schema = Schema::new()
        .field("name", TypeTag::String, TypeModifiers::NONE)
        .field("section", TypeTag::Int, TypeModifiers::NONE)
        .field("summary", TypeTag::String, TypeModifiers::NONE);
    let rows = pages
        .iter()
        .map(|p| alloc::vec![Value::Str(p.name.clone()), Value::Int(i64::from(p.section)), Value::Str(p.summary())])
        .collect();
    Table { flags: StreamFlags::NONE, schema, rows }
}

fn emit(stage: &Stage, stdout: Port<'_>, table: &Table) {
    let mut tw = TableWriter::new(ChannelSink::new(stdout, IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(table.flags, &table.schema).and_then(|()| {
        for row in &table.rows {
            tw.write_row(row)?;
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
