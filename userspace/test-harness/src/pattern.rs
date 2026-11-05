//! `test-pattern` — write a known pattern into a file **through a mapping, without a sync**, or
//! check that a file holds it.
//!
//! ```text
//! test-pattern --write PATH   # create PATH, map it writable, fill it, unmap, exit: no sync
//! test-pattern --check PATH   # read PATH and compare it with the pattern
//! test-pattern --eject-held PATH NAME   # hold PATH open, and ask for NAME's eject meanwhile
//! test-pattern --read-device PATH   # read a block of the block device at PATH, and say how it went
//! ```
//!
//! **Why a test program.** No release program lets go of a mapping without a sync (`libfs`,
//! `nxsh` and `nxedit` all sync), so the case an unmount's write-back exists for has no writer
//! outside a test image. `cargo xtask check-storage` runs this at a serial prompt on a live test
//! image, against a SATA disk it then reads on the host.
//!
//! **`--eject-held` is a second process a shell cannot be**: `nxsh` has no background jobs, so
//! nothing at a prompt can hold a file while `disk --eject` runs. This holds `PATH`, sends the
//! `Eject` itself on a media session, and says the answer — a refusal, a file on it being held.
//!
//! **`--read-device` asks an ejected stick for more**, as nothing in a release image does once the
//! storage service has marked it: the kernel must refuse the read before it reaches the stick.
//!
//! **A refusal is named** with the kernel's status; the gate matches `NoAccess` on a read-only
//! mount rather than a generic "cannot create".
//! **The pattern is written down twice**: here, and in `xtask`'s `check-storage`, which reads the
//! file back on the host. A gate does not take its aim from the program under test.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use libkern::{KError, Rights};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Channel, File, Handle, MapRead, MapReadWrite, Memory, Namespace, NsReadOnly, Only, ReadOnly, Resource};
use libos::{Msg, block_on, fs};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The file's length: three whole pages and part of a fourth, so a lost page, a misplaced page
/// and a lost tail each show.
const LEN: usize = 3 * 4096 + 1234;

/// Byte `i` of the pattern: each page is its own sequence, so no two pages hold the same bytes
/// and a page of zeroes matches nowhere. `xtask`'s `STORAGE_PATTERN_LEN` and
/// `storage_pattern_byte` must match.
fn byte(i: usize) -> u8 {
    ((i >> 12) as u8).wrapping_mul(0x5B) ^ (i as u8) ^ 0xA5
}

/// Entry point: dispatch `--write`, `--check`, `--eject-held` or `--read-device` from the setup
/// message's `argv`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let endpoint = start.take_handle0::<Channel, Only>();
    let setup = endpoint
        .as_ref()
        .filter(|_| libstream::setup::setup_is_pending(start.arg0))
        .and_then(|e| libstream::setup::receive_setup(e, libos::Deadline::NEVER).ok().flatten());
    let args: Vec<&str> = setup.as_ref().map_or(&[][..], |s| &s.argv[..]).iter().map(String::as_str).collect();
    let Some(ns) = start.root.as_ref() else {
        log::write(b"test-pattern: no namespace\n");
        libos::exit(1);
    };
    let code = match args.as_slice() {
        [_, "--write", path] => write(ns, path),
        [_, "--check", path] => check(ns, path),
        [_, "--eject-held", path, name] => eject_held(ns, path, name.as_bytes()),
        [_, "--read-device", path] => read_device(ns, path),
        _ => {
            log::write(
                b"usage: test-pattern --write PATH | --check PATH | --eject-held PATH NAME | --read-device PATH\n",
            );
            2
        }
    };
    libos::exit(code as i32)
}

fn status_name(status: i32) -> String {
    alloc::format!("{:?}", KError::from_i32(status))
}

/// `--write`: create `path`, write the pattern through a writable mapping, **do not sync**.
fn write(ns: &Handle<Namespace, NsReadOnly>, path: &str) -> i64 {
    let rights = Rights::MAP_READ | Rights::MAP_WRITE;
    let file: Handle<File, MapReadWrite> = match block_on(fs::create(ns, path, LEN as u64, rights)) {
        Ok(file) => file,
        Err(e) => {
            let status = e.code().as_i32();
            Line::new()
                .s(b"test-pattern: ")
                .untrusted(path.as_bytes())
                .s(b" refused: ")
                .s(status_name(status).as_bytes())
                .s(b" (")
                .i(status as i64)
                .s(b")")
                .end();
            return 1;
        }
    };
    let mut mapping = match file.map(LEN) {
        Ok(mapping) => mapping,
        Err(e) => {
            drop(file);
            Line::new()
                .s(b"test-pattern: ")
                .untrusted(path.as_bytes())
                .s(b" would not map: ")
                .s(status_name(e.code().as_i32()).as_bytes())
                .end();
            return 1;
        }
    };
    let pattern: Vec<u8> = (0..LEN).map(byte).collect();
    let _ = mapping.write_from(0, &pattern);
    // **Let go without a sync**: the kernel keeps the pages dirty until something writes them back.
    drop(mapping);
    drop(file);
    Line::new()
        .s(b"test-pattern: wrote ")
        .u(LEN as u64)
        .s(b" bytes to ")
        .untrusted(path.as_bytes())
        .s(b" through a mapping, and did not sync")
        .end();
    0
}

fn check(ns: &Handle<Namespace, NsReadOnly>, path: &str) -> i64 {
    let bytes = match libfs::read(ns, path) {
        Ok(b) => b,
        Err(e) => {
            Line::new()
                .s(b"test-pattern: ")
                .untrusted(path.as_bytes())
                .s(b" would not read: ")
                .s(alloc::format!("{e:?}").as_bytes())
                .s(b" FAIL")
                .end();
            return 1;
        }
    };
    let first_wrong = (0..LEN).find(|&i| bytes.get(i) != Some(&byte(i)));
    match (first_wrong, bytes.len() == LEN) {
        (None, true) => {
            Line::new()
                .s(b"test-pattern: ")
                .untrusted(path.as_bytes())
                .s(b" holds the pattern, ")
                .u(LEN as u64)
                .s(b" bytes ok")
                .end();
            0
        }
        (at, _) => {
            let mut l = Line::new();
            l.s(b"test-pattern: ").untrusted(path.as_bytes()).s(b" is ").u(bytes.len() as u64).s(b" bytes");
            if let Some(i) = at {
                l.s(b", and differs from the pattern at byte ").u(i as u64);
            }
            l.s(b" FAIL").end();
            1
        }
    }
}

/// `--eject-held`: hold `path`, ask the storage service to eject `name` through a media session,
/// let go — and succeed only if the eject was refused for the file held.
fn eject_held(ns: &Handle<Namespace, NsReadOnly>, path: &str, name: &[u8]) -> i64 {
    use librsproto::storage::OP_STORAGE_EJECT;
    let held = match block_on(ns.lookup::<File, MapRead>(path, Rights::MAP_READ)) {
        Ok(held) => held,
        Err(e) => {
            Line::new()
                .s(b"test-pattern: ")
                .untrusted(path.as_bytes())
                .s(b" would not open to hold: ")
                .s(status_name(e.code().as_i32()).as_bytes())
                .s(b" FAIL")
                .end();
            return 1;
        }
    };
    let media = block_on(
        ns.lookup::<Channel, Only>("/dev/storage/media", Rights::from_bits(librsproto::session::DIR_SESSION_RIGHTS)),
    );
    let st = media.as_ref().err().map_or(0, |e| e.code().as_i32());
    let answer = if let Ok(media) = media {
        let mut buf = Msg::new();
        let reply = librsproto::session::exchange(&media, &mut buf, 1, OP_STORAGE_EJECT, name);
        drop(media);
        reply.ok().and_then(|len| librsproto::decode(&buf.payload()[..len]).ok()).map(|m| {
            let refusal = (m.flags & librsproto::RS_FLAG_ERROR != 0)
                .then(|| librsproto::error::parse_error(m.body).map(|e| (e.kerror, alloc::vec::Vec::from(e.msg))))
                .flatten();
            (m.op, refusal)
        })
    } else {
        None
    };
    drop(held);
    let mut l = Line::new();
    l.s(b"test-pattern: eject of ").untrusted(name).s(b" with ").untrusted(path.as_bytes()).s(b" held: ");
    match answer {
        Some((OP_STORAGE_EJECT, Some((kerror, why)))) if kerror == KError::WouldBlock.as_i32() => {
            l.s(b"refused, ").untrusted(&why).s(b" ok").end();
            0
        }
        Some((_, Some((kerror, why)))) => {
            l.s(b"refused ").s(status_name(kerror).as_bytes()).s(b", ").untrusted(&why).s(b" FAIL").end();
            1
        }
        Some(_) => {
            l.s(b"ejected FAIL").end();
            1
        }
        None => {
            l.s(b"no answer (status ").i(st as i64).s(b") FAIL").end();
            1
        }
    }
}

/// `--read-device`: the first 4 KiB of the block device at `path`, and how the read went.
fn read_device(ns: &Handle<Namespace, NsReadOnly>, path: &str) -> i64 {
    const LEN: u64 = 4096;
    let node = block_on(ns.lookup::<Resource, ReadOnly>(path, Rights::READ));
    let mem = Handle::<Memory, MapReadWrite>::create(LEN as usize);
    let mut l = Line::new();
    l.s(b"test-pattern: a read of ").untrusted(path.as_bytes()).s(b": ");
    let (node, mem) = match (node, mem) {
        (Ok(node), Ok(mem)) => (node, mem),
        (Err(e), _) | (_, Err(e)) => {
            l.s(b"would not open: ").s(status_name(e.code().as_i32()).as_bytes()).end();
            return 1;
        }
    };
    let read = block_on(node.read(&mem, 0, 0, LEN));
    drop(mem);
    drop(node);
    match read {
        Ok(moved) => l.s(b"read ").u(moved as u64).s(b" bytes").end(),
        Err(e) => {
            let status = e.code().as_i32();
            l.s(b"refused: ").s(status_name(status).as_bytes()).s(b" (").i(status as i64).s(b")").end()
        }
    }
    0
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"test-pattern: PANIC\n");
    libos::exit(1)
}
