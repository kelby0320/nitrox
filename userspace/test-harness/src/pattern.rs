//! `test-pattern` — write a known pattern into a file **through a mapping, without a sync**, or
//! check that a file holds it (administration Part C.8).
//!
//! ```text
//! test-pattern --write PATH   # create PATH, map it writable, fill it, unmap, exit: no sync
//! test-pattern --check PATH   # read PATH and compare it with the pattern
//! test-pattern --eject-held PATH NAME   # hold PATH open, and ask for NAME's eject meanwhile
//! ```
//!
//! **Why a test program.** No release program writes through a mapping and lets go without a
//! sync: `libfs`, `nxsh` and `nxedit` all sync. So the case an unmount's write-back exists for has
//! no writer outside a test image. `cargo xtask check-storage` runs this at a serial prompt on a
//! live test image, against a SATA disk it then reads on the host.
//!
//! **`--eject-held` is a second process a shell cannot be** (Phase 6 Part F): `nxsh` has no
//! background jobs yet, so nothing at a prompt can hold a file while `disk --eject` runs. This holds
//! `PATH` and sends the `Eject` itself, on a media session as `disk` and Files open one, and says
//! what the service answered — which, a file on it being held, must be a refusal.
//!
//! **A refusal is named**, with the kernel's status: on a read-only mount the create is refused
//! `NoAccess`, and the gate matches that rather than a generic "cannot create".
//!
//! **The pattern is written down twice**: here, and in `xtask`'s `check-storage`, which reads the
//! file back on the host. A gate does not take its aim from the program under test.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use libkern::debug::Line;
use libkern::{
    KError, RIGHT_MAP_READ, RIGHT_MAP_WRITE, SYS_FILE_CREATE, SYS_HANDLE_CLOSE, SYS_MEMORY_MAP,
    SYS_MEMORY_UNMAP, SYS_WAIT, exit, kprint, syscall1, syscall2, syscall4, syscall5,
};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The file's length: three whole pages and part of a fourth, so a lost page, a page written
/// to the wrong place and a lost tail each show.
const LEN: usize = 3 * 4096 + 1234;

/// Byte `i` of the pattern. Each page is its own sequence — the page index, times an odd
/// constant, into the low byte of the offset — so no two pages hold the same bytes, and a page
/// of zeroes matches nowhere. `xtask`'s `STORAGE_PATTERN_LEN` and `storage_pattern_byte` must
/// match these.
fn byte(i: usize) -> u8 {
    ((i >> 12) as u8).wrapping_mul(0x5B) ^ (i as u8) ^ 0xA5
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let argv: Vec<String> = match libstream::setup::bootstrap(notif, ns, endpoint, arg0).setup() {
        Some(Ok(s)) => s.argv,
        _ => Vec::new(),
    };
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let code = match args.as_slice() {
        [_, "--write", path] => write(ns, path.as_bytes()),
        [_, "--check", path] => check(ns, path.as_bytes()),
        [_, "--eject-held", path, name] => eject_held(ns, path.as_bytes(), name.as_bytes()),
        _ => {
            kprint(b"usage: test-pattern --write PATH | --check PATH | --eject-held PATH NAME\n");
            2
        }
    };
    exit(code)
}

/// A kernel status by its name.
fn status_name(status: i32) -> String {
    alloc::format!("{:?}", KError::from_i32(status))
}

/// `--write`: create `path` at the pattern's length, write the pattern through a writable
/// mapping, and let go **without a sync**.
fn write(ns: u64, path: &[u8]) -> i64 {
    // SAFETY: a valid path slice and the namespace this process was given.
    let po = unsafe {
        syscall5(
            SYS_FILE_CREATE,
            ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
            LEN as u64,
        )
    };
    let (status, file) = if po < 0 { (po as i32, 0) } else { po_wait(po as u64) };
    if status != 0 || file == 0 {
        let status = if status == 0 { KError::KernelError.as_i32() } else { status };
        Line::new()
            .s(b"test-pattern: ")
            .untrusted(path)
            .s(b" refused: ")
            .s(status_name(status).as_bytes())
            .s(b" (")
            .i(status as i64)
            .s(b")")
            .end();
        return 1;
    }
    // SAFETY: mapping the file this call created, with the rights it was created with.
    let addr =
        unsafe { syscall4(SYS_MEMORY_MAP, file, 0, LEN as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        // SAFETY: closing the handle this call created.
        unsafe { syscall1(SYS_HANDLE_CLOSE, file) };
        Line::new()
            .s(b"test-pattern: ")
            .untrusted(path)
            .s(b" would not map: ")
            .s(status_name(addr as i32).as_bytes())
            .end();
        return 1;
    }
    for i in 0..LEN {
        // SAFETY: `LEN` bytes are mapped writable at `addr`.
        unsafe { ((addr as usize + i) as *mut u8).write(byte(i)) };
    }
    // **Let go without a sync**: unmapped and closed, and nothing else. The kernel keeps the
    // file's pages dirty in its cache until something writes them back.
    // SAFETY: unmapping this call's own mapping, then closing its own handle.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, LEN as u64);
        syscall1(SYS_HANDLE_CLOSE, file);
    }
    Line::new()
        .s(b"test-pattern: wrote ")
        .u(LEN as u64)
        .s(b" bytes to ")
        .untrusted(path)
        .s(b" through a mapping, and did not sync")
        .end();
    0
}

/// `--check`: read `path` and compare it with the pattern.
fn check(ns: u64, path: &[u8]) -> i64 {
    let bytes = match libfs::read_file(ns, path) {
        Ok(b) => b,
        Err(e) => {
            Line::new()
                .s(b"test-pattern: ")
                .untrusted(path)
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
                .untrusted(path)
                .s(b" holds the pattern, ")
                .u(LEN as u64)
                .s(b" bytes ok")
                .end();
            0
        }
        (at, _) => {
            let mut l = Line::new();
            l.s(b"test-pattern: ").untrusted(path).s(b" is ").u(bytes.len() as u64).s(b" bytes");
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
fn eject_held(ns: u64, path: &[u8], name: &[u8]) -> i64 {
    use librsproto::storage::OP_STORAGE_EJECT;
    let (st, held) = libfs::lookup_wait(ns, path, RIGHT_MAP_READ);
    if st != 0 || held == 0 {
        Line::new()
            .s(b"test-pattern: ")
            .untrusted(path)
            .s(b" would not open to hold: ")
            .s(status_name(st).as_bytes())
            .s(b" FAIL")
            .end();
        return 1;
    }
    let (st, media) = libfs::lookup_wait(ns, b"/dev/storage/media", librsproto::session::DIR_SESSION_RIGHTS);
    let answer = if st == 0 && media != 0 {
        let mut buf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
        let reply = librsproto::session::round_trip(media, &mut buf, 1, OP_STORAGE_EJECT, name);
        // SAFETY: closing the media session this call opened.
        unsafe { syscall1(SYS_HANDLE_CLOSE, media) };
        let off = libkern::abi::IPC_HEADER_SIZE;
        reply.ok().and_then(|len| librsproto::decode(&buf[off..off + len]).ok()).map(|m| {
            let refusal = (m.flags & librsproto::RS_FLAG_ERROR != 0)
                .then(|| librsproto::error::parse_error(m.body).map(|e| (e.kerror, alloc::vec::Vec::from(e.msg))))
                .flatten();
            (m.op, refusal)
        })
    } else {
        None
    };
    // SAFETY: closing the file this call held.
    unsafe { syscall1(SYS_HANDLE_CLOSE, held) };
    let mut l = Line::new();
    l.s(b"test-pattern: eject of ").untrusted(name).s(b" with ").untrusted(path).s(b" held: ");
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

/// Wait for a `PendingOperation`, returning `(status, result)` and closing it.
fn po_wait(po: u64) -> (i32, u64) {
    let handles = [po];
    let mut r = [0u8; 24];
    // SAFETY: a valid one-entry handle array and result buffer.
    let waited =
        unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, r.as_mut_ptr() as u64, u64::MAX) };
    // SAFETY: closing the PO this process owns; a created file is a separate handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 {
        return (KError::KernelError.as_i32(), 0);
    }
    let status = i32::from_le_bytes([r[8], r[9], r[10], r[11]]);
    let result = u64::from_le_bytes([r[16], r[17], r[18], r[19], r[20], r[21], r[22], r[23]]);
    (status, result)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"test-pattern: PANIC\n");
    exit(1)
}
