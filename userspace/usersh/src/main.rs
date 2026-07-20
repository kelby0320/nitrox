//! `usersh` — the throwaway user shell (Phase 3, auth + session slice).
//!
//! The leaf a login lands in: session-mgr spawns it into a **constructed per-user
//! namespace** (its `/home` is the user's home subtree of the fs-server, RW; it holds
//! **empty** syscaps — a real sandbox). Its job is to prove the session works: write a
//! file to `$HOME` and read it back. This is a deliberate **throwaway** — the real
//! shell is Phase 4 — so it is intentionally minimal.
//!
//! Under `test-harness` it runs the home-write proof and **exits** with its verdict
//! (session-mgr gates the boot verdict on the exit code). Otherwise it prints a welcome
//! and drops into a minimal console loop (the interactive login's shell).
//!
//! `#![no_std]` + `#![no_main]`, **no `alloc`**, `libkern` only — the eshell family's
//! rules. See `userspace/usersh/CLAUDE.md`.

#![no_std]
#![no_main]

use core::arch::asm;
use libkern::*;

const PAGE: u64 = 4096;
/// The file the shell writes into its home to prove the session is writable.
const HOME_FILE: &[u8] = b"/home/greeting";
/// Its content — a fixed marker the shell writes then reads back to verify.
const GREETING: &[u8] = b"hello from alice\n";

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// Emit `msg` to the serial console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Exit the process (does not return).
fn exit(code: i64) -> ! {
    // SAFETY: SYS_PROCESS_EXIT terminates this process.
    unsafe { syscall1(SYS_PROCESS_EXIT, code as u64) };
    loop {
        // SAFETY: `pause` is always valid in ring 3 with no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}

/// Wait on a single PO handle; returns `(status, result)` and closes the PO.
fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let w = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
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
    if w != 1 { (-1, 0) } else { (status, result) }
}

/// Resolve `path` in `ns` with `rights`; returns `(status, handle)`.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return (po as i32, 0);
    }
    po_wait(po as u64)
}

/// The home-write proof: create `/home/greeting` in the session namespace (it forwards
/// to the user's home subtree of the fs-server), write the greeting, sync, then
/// re-resolve with a plain lookup and confirm the bytes persisted. Returns `true` on
/// success. This exercises the whole session: the constructed namespace, subtree
/// scoping, and the fs-server write path — all from an unprivileged sandbox.
fn home_write_proof(session_ns: u64) -> bool {
    let new_size = PAGE;
    // 1. Create + grow-resolve the file (create-on-resolve).
    let po = unsafe {
        syscall5(
            SYS_FILE_CREATE,
            session_ns,
            HOME_FILE.as_ptr() as u64,
            HOME_FILE.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
            new_size,
        )
    };
    if po < 0 {
        kprint(b"usersh: create submit FAIL\n");
        return false;
    }
    let (st, fh) = po_wait(po as u64);
    if st != 0 || fh == 0 {
        kprint(b"usersh: create FAIL (cannot write home?)\n");
        return false;
    }
    // 2. Map it writable + write the greeting at offset 0, then sync to disk.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, new_size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        kprint(b"usersh: map FAIL\n");
        return false;
    }
    let base = addr as u64;
    for (i, b) in GREETING.iter().enumerate() {
        // SAFETY: offset `i` is within the mapped first page.
        unsafe { ((base + i as u64) as *mut u8).write_volatile(*b) };
    }
    // SAFETY: `fh` is our writable file handle.
    if unsafe { syscall1(SYS_FILE_SYNC, fh) } != 0 {
        kprint(b"usersh: sync FAIL\n");
    }
    // SAFETY: unmap + close (we re-resolve fresh below).
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, base, new_size);
        syscall1(SYS_HANDLE_CLOSE, fh);
    }
    // 3. Re-resolve with a plain lookup (proves it's on disk) and verify the bytes.
    let (st2, fh2) = ns_lookup(session_ns, HOME_FILE, RIGHT_MAP_READ);
    if st2 != 0 || fh2 == 0 {
        kprint(b"usersh: re-read FAIL\n");
        return false;
    }
    let addr2 = unsafe { syscall4(SYS_MEMORY_MAP, fh2, 0, new_size, RIGHT_MAP_READ) };
    if addr2 < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh2) };
        return false;
    }
    let base2 = addr2 as u64;
    let mut ok = true;
    for (i, b) in GREETING.iter().enumerate() {
        // SAFETY: within the mapped first page.
        if unsafe { ((base2 + i as u64) as *const u8).read_volatile() } != *b {
            ok = false;
            break;
        }
    }
    // SAFETY: unmap + close.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, base2, new_size);
        syscall1(SYS_HANDLE_CLOSE, fh2);
    }
    ok
}

/// Minimal interactive console loop for the non-test boot (the real shell is Phase 4).
/// Reads `/dev/console` (bound into the session namespace), echoes, and on a line runs
/// `cat <path>` / `exit`. Returns when the user types `exit`.
#[cfg(not(feature = "test-harness"))]
fn console_loop(session_ns: u64) {
    let (st, console) = ns_lookup(session_ns, b"/dev/console", RIGHT_READ);
    if st != 0 || console == 0 {
        kprint(b"usersh: no console; nothing to do\n");
        return;
    }
    // A one-page read buffer (kernel writes input here; we read it back).
    let buf_h = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if buf_h < 0 {
        return;
    }
    let buf_h = buf_h as u64;
    let buf_addr =
        unsafe { syscall4(SYS_MEMORY_MAP, buf_h, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if buf_addr < 0 {
        return;
    }
    let buf_addr = buf_addr as u64;
    let mut line = [0u8; 128];
    let mut len = 0usize;
    kprint(b"alice@nitrox:~$ ");
    loop {
        let op = IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: buf_h, buf_offset: 0, offset: 0, length: 256 };
        // SAFETY: `console` is a char DeviceNode with READ; `&op` is a valid IoOp.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, console, (&op as *const IoOp) as u64) };
        if po < 0 {
            continue;
        }
        let (status, n) = po_wait(po as u64);
        if status != 0 {
            continue;
        }
        for i in 0..(n as usize).min(256) {
            // SAFETY: `buf_addr + i` is within the mapped read buffer.
            let b = unsafe { ((buf_addr + i as u64) as *const u8).read_volatile() };
            match b {
                b'\r' | b'\n' => {
                    kprint(b"\r\n");
                    if &line[..len] == b"exit" {
                        return;
                    } else if len == 0 {
                        // nothing
                    } else if line[..len].starts_with(b"cat ") {
                        cat(session_ns, &line[4..len]);
                    } else {
                        kprint(b"usersh: commands: cat <path>, exit\r\n");
                    }
                    len = 0;
                    kprint(b"alice@nitrox:~$ ");
                }
                0x08 | 0x7F => {
                    if len > 0 {
                        len -= 1;
                        kprint(b"\x08 \x08");
                    }
                }
                0x20..=0x7E => {
                    if len < line.len() {
                        line[len] = b;
                        len += 1;
                        kprint(&[b]);
                    }
                }
                _ => {}
            }
        }
    }
}

/// `cat <path>`: resolve, map, and print a file (used by the interactive loop).
#[cfg(not(feature = "test-harness"))]
fn cat(session_ns: u64, path: &[u8]) {
    let (st, h) = ns_lookup(session_ns, path, RIGHT_MAP_READ | RIGHT_INSPECT);
    if st != 0 || h == 0 {
        kprint(b"cat: cannot open\r\n");
        return;
    }
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: valid HandleInfo out-param.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, h, (&mut info as *mut HandleInfo) as u64) };
    if sr < 0 || info.size == 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        return;
    }
    let size = info.size;
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, size, RIGHT_MAP_READ) };
    if addr >= 0 {
        // SAFETY: `addr..addr+size` maps `size` valid bytes.
        let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, size as usize) };
        let n = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        kprint(&bytes[..n]);
        // SAFETY: unmap.
        unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, size) };
    }
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
}

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the **session
/// namespace** session-mgr constructed (this shell's sandboxed root), `rdx`/`rcx`
/// unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, session_ns: u64, _h0: u64, _a0: u64) -> ! {
    let ok = home_write_proof(session_ns);
    if ok {
        kprint(b"usersh: wrote + verified /home/greeting (session works)\n");
    } else {
        kprint(b"usersh: home write FAILED\n");
    }

    // Under the test harness, the exit code is session-mgr's verdict signal.
    #[cfg(feature = "test-harness")]
    exit(if ok { 0 } else { 1 });

    // Interactive boot: a minimal throwaway shell on the console.
    #[cfg(not(feature = "test-harness"))]
    {
        kprint(b"\r\nusersh: welcome, alice\r\n");
        console_loop(session_ns);
        exit(0);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"usersh: PANIC\n");
    exit(1);
}
