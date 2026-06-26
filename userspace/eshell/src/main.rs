//! `eshell` — the emergency shell (Phase 2 slice 9).
//!
//! The first interactive userspace program: a minimal command shell on the serial
//! console. init spawns it after boot (an interactive prompt) and on a critical-path
//! failure. Keyboard input comes from `/dev/console` (a char `DeviceNode`) through
//! the universal `sys_io_submit(Read)` + `sys_wait` path; eshell does its own echo +
//! line editing and runs a few inspection commands. Output is `sys_kprint`.
//!
//! `#![no_std]` + `#![no_main]`, **no `alloc`**, `libkern` only (no rsproto/libos) —
//! the init family's rules. See `userspace/eshell/CLAUDE.md`.

#![no_std]
#![no_main]

use core::arch::asm;
use libkern::*;

/// Page size (the read buffer is one page).
const PAGE: u64 = 4096;
/// Longest command line we buffer; excess input past it is dropped (with a bell).
const LINE_MAX: usize = 128;
/// Bytes requested per console read (matches the kernel console ring capacity).
const READ_LEN: u64 = 256;
/// Highest `/dev/blk/<n>` index `lsblk` probes before giving up.
const LSBLK_MAX: usize = 16;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

// --- syscall helpers (eshell speaks the raw surface, like init) --------------

/// Wait on a single `PendingOperation` handle; returns `(status, result)` and
/// closes the PO. `(-1, 0)` if the wait did not signal exactly one handle.
fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    let result = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ])
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, result) }
}

/// Resolve `path` in namespace `ns` with `rights`, waiting for the lookup PO;
/// returns `(status, handle)` (`handle == 0` on failure).
fn ns_lookup_wait(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return (po as i32, 0);
    }
    let (status, resolved) = po_wait(po as u64);
    (status, resolved)
}

// --- output ------------------------------------------------------------------

/// Echo one byte to the console.
fn putb(b: u8) {
    kprint(&[b]);
}

/// The interactive prompt.
fn prompt() {
    kprint(b"eshell> ");
}

// --- the line editor ---------------------------------------------------------

/// ASCII control codes the line editor recognises.
const CR: u8 = 0x0D;
const LF: u8 = 0x0A;
const BS: u8 = 0x08;
const DEL: u8 = 0x7F;
const BEL: u8 = 0x07;

/// Process one received byte against the current line buffer. On end-of-line
/// (CR/LF) it echoes a newline, dispatches the line, clears it, and reprints the
/// prompt. Returns nothing — the buffer/len are updated in place.
fn handle_byte(b: u8, line: &mut [u8; LINE_MAX], len: &mut usize, root_ns: u64) {
    match b {
        CR | LF => {
            kprint(b"\r\n");
            dispatch(&line[..*len], root_ns);
            *len = 0;
            prompt();
        }
        BS | DEL => {
            if *len > 0 {
                *len -= 1;
                // Erase the last glyph: backspace, space, backspace.
                kprint(b"\x08 \x08");
            }
        }
        0x20..=0x7E => {
            if *len < LINE_MAX {
                line[*len] = b;
                *len += 1;
                putb(b); // echo
            } else {
                putb(BEL); // line full
            }
        }
        _ => {} // ignore other control bytes
    }
}

// --- command dispatch --------------------------------------------------------

/// Parse and run one command line (no trailing newline). Empty lines are ignored.
fn dispatch(line: &[u8], root_ns: u64) {
    let line = trim(line);
    if line.is_empty() {
        return;
    }
    let (cmd, args) = split_first_word(line);
    match cmd {
        b"help" => cmd_help(),
        b"echo" => {
            kprint(trim(args));
            kprint(b"\r\n");
        }
        b"lsblk" => cmd_lsblk(root_ns),
        b"cat" => cmd_cat(root_ns, trim(args)),
        _ => {
            kprint(b"eshell: unknown command: ");
            kprint(cmd);
            kprint(b"\r\n(type 'help')\r\n");
        }
    }
}

fn cmd_help() {
    kprint(
        b"commands:\r\n  help          this list\r\n  echo <text>   print text\r\n  \
          lsblk         list block devices\r\n  cat <path>    print a file\r\n",
    );
}

/// Print a file (or any mappable, sized resource): resolve the path, `stat` it for
/// its size, map it read-only, and write its bytes. A `FileObject` faults its pages
/// in from the fs-server as they're read (the slice-8 lazy page cache).
fn cmd_cat(root_ns: u64, path: &[u8]) {
    if path.is_empty() {
        kprint(b"cat: usage: cat <path>\r\n");
        return;
    }
    // Need MAP_READ (map+read) and INSPECT (stat for the size).
    let (st, h) = ns_lookup_wait(root_ns, path, RIGHT_MAP_READ | RIGHT_INSPECT);
    if st != 0 || h == 0 {
        kprint(b"cat: cannot open: ");
        kprint(path);
        kprint(b"\r\n");
        return;
    }
    // Stat for the byte size (the slice-9 `HandleInfo.size`).
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `&mut info` is a valid 24-byte HandleInfo out-param.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, h, (&mut info as *mut HandleInfo) as u64) };
    if sr < 0 {
        kprint(b"cat: stat failed\r\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        return;
    }
    let size = info.size;
    if size == 0 {
        // SAFETY: closing our own handle (empty resource — nothing to print).
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        return;
    }
    // Map read-only (the syscall rounds the length up to whole pages).
    // SAFETY: `h` is a mappable resource handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, size, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(b"cat: map failed\r\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        return;
    }
    // Write the bytes (a FileObject demand-faults its pages here). Trim a
    // zero-padded tail: a `MemoryObject`'s `size` is page-rounded, so a snapshot
    // resource (e.g. an initramfs file) has trailing NULs past its content.
    // SAFETY: `addr..addr+size` maps `size` valid bytes of the resource.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, size as usize) };
    let len = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    if len > 0 {
        kprint(&bytes[..len]);
        if bytes[len - 1] != b'\n' {
            kprint(b"\r\n"); // ensure the prompt starts on a fresh line
        }
    }
    // SAFETY: unmap our mapping + close our handle (eshell runs forever — don't leak).
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
        syscall1(SYS_HANDLE_CLOSE, h);
    }
}

/// List block devices by probing `/dev/blk/0..` until one is not found.
fn cmd_lsblk(root_ns: u64) {
    let mut found = 0;
    for i in 0..LSBLK_MAX {
        let mut path = [0u8; 24];
        let n = blk_path(i, &mut path);
        let (st, h) = ns_lookup_wait(root_ns, &path[..n], RIGHT_READ);
        if st != 0 || h == 0 {
            break; // no more devices
        }
        kprint(&path[..n]);
        kprint(b"\r\n");
        found += 1;
        // SAFETY: closing the handle we just resolved.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
    if found == 0 {
        kprint(b"(no block devices)\r\n");
    }
}

// --- small string helpers (no alloc) -----------------------------------------

/// Write `/dev/blk/<i>` into `buf`, returning its length. `i < LSBLK_MAX`.
fn blk_path(i: usize, buf: &mut [u8]) -> usize {
    const PREFIX: &[u8] = b"/dev/blk/";
    buf[..PREFIX.len()].copy_from_slice(PREFIX);
    let mut n = PREFIX.len();
    // Decimal i (i < 100 here).
    if i >= 10 {
        buf[n] = b'0' + (i / 10) as u8;
        n += 1;
    }
    buf[n] = b'0' + (i % 10) as u8;
    n += 1;
    n
}

/// Trim leading/trailing ASCII spaces.
fn trim(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && s[a] == b' ' {
        a += 1;
    }
    while b > a && s[b - 1] == b' ' {
        b -= 1;
    }
    &s[a..b]
}

/// Split `s` (already left-trimmed by the caller's `trim`) into its first
/// space-delimited word and the remainder.
fn split_first_word(s: &[u8]) -> (&[u8], &[u8]) {
    match s.iter().position(|&c| c == b' ') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, &[]),
    }
}

// --- entry -------------------------------------------------------------------

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the
/// inherited root namespace, `rdx`/`rcx` unused. eshell resolves `/dev/console`
/// (and `/dev/blk/*`) through the inherited namespace.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, _h0: u64, _a0: u64) -> ! {
    kprint(b"\r\neshell: interactive console (type 'help')\r\n");

    // Resolve the console (read-only input).
    let (st, console) = ns_lookup_wait(root_ns, b"/dev/console", RIGHT_READ);
    if st != 0 || console == 0 {
        kprint(b"eshell: /dev/console not found\r\n");
        halt();
    }

    // A one-page read buffer: the kernel writes input into it (MAP_WRITE), we read
    // it back (MAP_READ).
    // SAFETY: register-only syscall.
    let buf_h = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if buf_h < 0 {
        kprint(b"eshell: read buffer alloc failed\r\n");
        halt();
    }
    let buf_h = buf_h as u64;
    // SAFETY: `buf_h` is a fresh MemoryObject handle with full MAP rights.
    let buf_addr =
        unsafe { syscall4(SYS_MEMORY_MAP, buf_h, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if buf_addr < 0 {
        kprint(b"eshell: read buffer map failed\r\n");
        halt();
    }
    let buf_addr = buf_addr as u64;

    let mut line = [0u8; LINE_MAX];
    let mut len = 0usize;
    prompt();

    // The read/edit loop. Each console read returns ≥1 raw byte; eshell echoes and
    // line-edits, running a command on each CR/LF.
    loop {
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: buf_h,
            buf_offset: 0,
            offset: 0,
            length: READ_LEN,
        };
        // SAFETY: `console` is a char DeviceNode with READ; `&op` is a valid IoOp.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, console, (&op as *const IoOp) as u64) };
        if po < 0 {
            continue;
        }
        let (status, n) = po_wait(po as u64);
        if status != 0 {
            continue;
        }
        let n = (n as usize).min(READ_LEN as usize);
        for i in 0..n {
            // SAFETY: `buf_addr + i` is within the mapped read buffer (`i < n ≤
            // READ_LEN ≤ PAGE`).
            let b = unsafe { ((buf_addr + i as u64) as *const u8).read_volatile() };
            handle_byte(b, &mut line, &mut len, root_ns);
        }
    }
}

/// Spin forever (a fatal setup error; eshell has nowhere to hand off to yet).
fn halt() -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"eshell: PANIC\r\n");
    halt();
}
