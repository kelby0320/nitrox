//! `hello` — the first Nitrox userspace program (a throwaway Phase-1 proof).
//!
//! A freestanding ring-3 program that exercises the syscall surface available
//! so far and reports each step over the debug `sys_kprint` log:
//!
//! 1. prints a greeting;
//! 2. `sys_memory_create` → a one-page anonymous `MemoryObject`;
//! 3. `sys_memory_map` → maps it read/write and round-trips a byte through the
//!    mapped page (the proof that user PTEs point at the object's frames);
//! 4. `sys_handle_stat` / `duplicate` / `restrict` / `close` on the memory
//!    handle — the handle-operation syscalls' first end-to-end ring-3 run;
//! 4b. `sys_clock_read(Monotonic)` twice — the monotonic clock must advance;
//! 5. `sys_memory_unmap`, then `sys_process_exit`.
//!
//! It exists only to demonstrate the kernel can load an ELF, build a process +
//! address space, schedule a thread into ring 3, service syscalls, and tear
//! the process down. It is replaced by the real `init` (PID 1) once an
//! initramfs exists.
//!
//! Built as a **static, non-PIE `ET_EXEC`** at a low user virtual address —
//! see `user.ld` and `.cargo/config.toml`.

#![no_std]
#![no_main]

use core::arch::asm;

// --- Syscall numbers (must match `kernel/src/syscall/table.rs`) ----------
//
// Stable handle/memory ops are sequential from 0; the debug syscalls live in
// a high, non-ABI-stable range.
const SYS_HANDLE_CLOSE: u64 = 0;
const SYS_HANDLE_DUPLICATE: u64 = 1;
const SYS_HANDLE_RESTRICT: u64 = 2;
const SYS_HANDLE_STAT: u64 = 3;
const SYS_MEMORY_CREATE: u64 = 4;
const SYS_MEMORY_MAP: u64 = 5;
const SYS_MEMORY_UNMAP: u64 = 6;
const SYS_CLOCK_READ: u64 = 7;
const SYS_DEBUG_KPRINT: u64 = 0xFFFF_0000;
const SYS_PROCESS_EXIT: u64 = 0xFFFF_0001;

// --- Rights bits (must match `kernel/src/libkern/handle.rs`) -------------
const RIGHT_DUPLICATE: u64 = 1 << 0;
const RIGHT_INSPECT: u64 = 1 << 2;
const RIGHT_MAP_READ: u64 = 1 << 15;
const RIGHT_MAP_WRITE: u64 = 1 << 16;

const PAGE: u64 = 4096;

/// `ClockId::Monotonic` (`kernel/src/libkern/clock.rs`).
const CLOCK_MONOTONIC: u64 = 0;

/// Object-type discriminant for a `MemoryObject` (`KObjectType::MemoryObject`).
const KOBJ_MEMORY_OBJECT: u32 = 4;

const MSG: &[u8] = b"hello from ring 3 (pid 1)\n";

/// Scratch buffer for `sys_handle_stat`'s `HandleInfo` out-parameter. Lives in
/// the writable `.bss` segment (mapped R/W by the loader). Layout matches the
/// kernel's `#[repr(C)] HandleInfo { rights: u64, object_type: u32,
/// generation: u32 }`.
#[repr(C, align(8))]
struct HandleInfoBuf {
    rights: u64,
    object_type: u32,
    generation: u32,
}
static mut STAT_BUF: HandleInfoBuf = HandleInfoBuf {
    rights: 0,
    object_type: 0,
    generation: 0,
};

/// Out-parameter for `sys_clock_read`. Writable `.bss`, naturally 8-aligned.
static mut CLOCK_BUF: u64 = 0;

/// Issue a syscall with up to four arguments. ABI: `rax` = number, args in
/// `rdi`/`rsi`/`rdx`/`r10`; `syscall` clobbers `rcx`/`r11`; result in `rax`.
///
/// # Safety
/// The caller must pass a valid syscall number and arguments; some syscalls
/// (e.g. exit) never return — use the dedicated path for those.
#[inline]
unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret;
    // SAFETY: a register-only syscall; the asm touches no memory we don't own
    // and clobbers only the documented scratch registers.
    unsafe {
        asm!(
            "syscall",
            in("rax") nr,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            out("rcx") _,
            out("r11") _,
            lateout("rax") ret,
        );
    }
    ret
}

#[inline]
unsafe fn syscall1(nr: u64, a0: u64) -> i64 {
    // SAFETY: see `syscall4`.
    unsafe { syscall4(nr, a0, 0, 0, 0) }
}

#[inline]
unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> i64 {
    // SAFETY: see `syscall4`.
    unsafe { syscall4(nr, a0, a1, 0, 0) }
}

/// Write `msg` to the kernel serial log via the debug syscall.
fn kprint(msg: &[u8]) {
    // SAFETY: passes a valid (ptr, len) the kernel copies from; returns a count.
    unsafe {
        syscall2(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64);
    }
}

/// Exit the process. Never returns.
fn exit(status: i64) -> ! {
    // SAFETY: process exit diverges in the kernel; control never returns here.
    unsafe {
        asm!(
            "syscall",
            in("rax") SYS_PROCESS_EXIT,
            in("rdi") status,
            options(noreturn, nostack),
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    kprint(MSG);

    // 1. Create a one-page memory object.
    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let h = unsafe { syscall2(SYS_MEMORY_CREATE, PAGE, 0) };
    if h < 0 {
        kprint(b"memory: create FAIL\n");
        exit(1);
    }
    let h = h as u64;

    // 2. Map it read/write (anywhere).
    // SAFETY: valid syscall; returns the mapped address or a negative KError.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"memory: map FAIL\n");
        exit(1);
    }
    let addr = addr as u64;

    // 3. Round-trip a byte through the mapped page. Volatile so the write and
    //    read are not elided — the point is to touch the object's frame.
    let p = addr as *mut u8;
    // SAFETY: `addr` is a page the kernel just mapped R/W into our address
    // space; a single in-bounds byte access is sound.
    let got = unsafe {
        p.write_volatile(0xA5);
        p.read_volatile()
    };
    if got == 0xA5 {
        kprint(b"memory: roundtrip ok\n");
    } else {
        kprint(b"memory: roundtrip FAIL\n");
    }

    // 4. Exercise the handle-operation syscalls on the memory handle. Each
    //    step prints its own failure so a regression is easy to localise.
    let mut handle_ops_ok = true;

    // stat → expect type MemoryObject and the MAP_READ right present.
    // SAFETY: STAT_BUF is a valid writable out-param of the right layout.
    let stat_ret = unsafe { syscall2(SYS_HANDLE_STAT, h, (&raw mut STAT_BUF) as u64) };
    // SAFETY: on success the kernel wrote a HandleInfo into STAT_BUF.
    let (ot, rights) = unsafe {
        let sb = &raw const STAT_BUF;
        ((*sb).object_type, (*sb).rights)
    };
    if stat_ret != 0 || ot != KOBJ_MEMORY_OBJECT || (rights & RIGHT_MAP_READ) == 0 {
        handle_ops_ok = false;
    }

    // duplicate → a second handle with only INSPECT|DUPLICATE.
    // SAFETY: valid syscall; returns a new handle or a negative KError.
    let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, h, RIGHT_INSPECT | RIGHT_DUPLICATE) };
    if dup < 0 {
        handle_ops_ok = false;
    } else {
        let dup = dup as u64;
        // restrict the duplicate to INSPECT only, then close it.
        // SAFETY: valid syscalls operating on our own handle.
        let rr = unsafe { syscall2(SYS_HANDLE_RESTRICT, dup, RIGHT_INSPECT) };
        let cr = unsafe { syscall1(SYS_HANDLE_CLOSE, dup) };
        if rr != 0 || cr != 0 {
            handle_ops_ok = false;
        }
    }

    if handle_ops_ok {
        kprint(b"handle-ops ok\n");
    } else {
        kprint(b"handle-ops FAIL\n");
    }

    // 4b. Read the monotonic clock twice with work in between; it must advance.
    // SAFETY: CLOCK_BUF is a valid writable u64 out-parameter.
    let r1 = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: on success the kernel wrote the nanosecond count into CLOCK_BUF.
    let t1 = unsafe { (&raw const CLOCK_BUF).read() };
    // A little observable work so the counter advances measurably between reads.
    let mut spin = 0u64;
    for _ in 0..200_000 {
        // SAFETY-free: a volatile-style accumulate the optimiser can't drop.
        spin = spin.wrapping_add(core::hint::black_box(1));
    }
    let _ = spin;
    // SAFETY: as the first read.
    let r2 = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut CLOCK_BUF) as u64) };
    // SAFETY: as the first read.
    let t2 = unsafe { (&raw const CLOCK_BUF).read() };
    if r1 == 0 && r2 == 0 && t2 > t1 {
        kprint(b"clock: monotonic advancing\n");
    } else {
        kprint(b"clock: monotonic FAIL\n");
    }

    // 5. Unmap and exit.
    // SAFETY: valid syscall on a region we mapped above.
    let ur = unsafe { syscall2(SYS_MEMORY_UNMAP, addr, PAGE) };
    if ur == 0 {
        kprint(b"memory: unmap ok\n");
    } else {
        kprint(b"memory: unmap FAIL\n");
    }

    exit(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
