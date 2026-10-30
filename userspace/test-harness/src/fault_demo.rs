//! The faulting worker, whose registers `test-harness` checks one by one after its `SegFault`.

use core::arch::asm;
use libkern::debug::Line;
use libkern::*;
use libos::thread_create;

/// One page; the worker thread's stack size.
const PAGE: u64 = 4096;

static mut NOTIF: Notification = Notification::zeroed();
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut WAIT_HANDLES: [u64; 1] = [0];
/// `sys_exception_resume` disposition: terminate the thread with a code.
const DISPOSITION_TERMINATE: u64 = 2;

static mut WORKER_ARGS: ThreadArgs = ThreadArgs {
    entry: 0,
    user_sp: 0,
    arg0: 0,
    class: 0, // TimeShared
    rt_priority: 0,
    nice: 0,
    cpu_affinity: 0, // no restriction
    _reserved: [0; 36],
};
static mut WORKER_REGS: RegisterValues = RegisterValues { regs: [0; 18] };

const FAULT_ADDR: u64 = 0xdead_0000;

/// The value the worker gives the register at `index` of [`RegisterValues::regs`] before it faults.
const fn mark(index: usize) -> u64 {
    0x5afe_0000_0000_0000 | index as u64
}

/// The registers the worker sets, by index: all an `asm!` may name but `rax`, the address.
const MARKED: [usize; 12] = [2, 3, 4, 5, 8, 9, 10, 11, 12, 13, 14, 15];

/// Wait `ns` on a channel nothing signals, `sys_wait`'s deadline its only way out.
fn nap(ns: u64) {
    let (mut now, mut ours, mut theirs) = (0u64, 0u64, 0u64);
    let mut result = [0u8; 24];
    // SAFETY: each pointer is a valid writable out-parameter, and each handle this thread's own.
    unsafe {
        syscall2(SYS_CLOCK_READ, abi::CLOCK_MONOTONIC, (&raw mut now) as u64);
        if syscall4(SYS_CHANNEL_CREATE, (&raw mut ours) as u64, (&raw mut theirs) as u64, 1, 0) != 0 {
            return;
        }
        syscall4(SYS_WAIT, (&raw const ours) as u64, 1, result.as_mut_ptr() as u64, now.saturating_add(ns));
        syscall1(SYS_HANDLE_CLOSE, ours);
        syscall1(SYS_HANDLE_CLOSE, theirs);
    }
}

/// The worker's entry: fault on a store with each register in [`MARKED`] at its [`mark`], once
/// the main thread has had time to reach its wait, so the fault finds it waiting.
extern "C" fn worker_fault() -> ! {
    nap(10_000_000);
    // SAFETY: this is the whole point — `FAULT_ADDR` is an unmapped user address, so the store
    // traps; the kernel never lets it complete, and the registers it sets are only inputs.
    unsafe {
        asm!(
            "mov qword ptr [rax], rcx",
            in("rax") FAULT_ADDR, in("rcx") mark(2), in("rdx") mark(3), in("rsi") mark(4), in("rdi") mark(5),
            in("r8") mark(8), in("r9") mark(9), in("r10") mark(10), in("r11") mark(11),
            in("r12") mark(12), in("r13") mark(13), in("r14") mark(14), in("r15") mark(15),
            options(nostack),
        );
    }
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}

/// The exception demo: see the worker's `SegFault` on `notif`, check its registers, terminate it.
pub(crate) fn run(notif: u64) {
    kprint(b"test-harness: mapping a worker stack\n");
    // SAFETY: register-only syscalls with valid arguments.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem < 0 {
        kprint(b"test-harness: worker stack create FAIL\n");
        exit(1);
    }
    // SAFETY: maps the memory object read/write at a kernel-chosen address.
    let base = unsafe { syscall4(SYS_MEMORY_MAP, mem as u64, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if base < 0 {
        kprint(b"test-harness: worker stack map FAIL\n");
        exit(1);
    }
    let stack_top = base as u64 + PAGE; // stacks grow down from the top

    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: WORKER_ARGS is our static, exclusively initialised here, then lent to the wrapper.
    let worker = match unsafe {
        WORKER_ARGS.entry = worker_fault as *const () as usize as u64;
        WORKER_ARGS.user_sp = stack_top;
        WORKER_ARGS.arg0 = 0;
        thread_create(&*(&raw const WORKER_ARGS))
    } {
        Ok(w) => w,
        Err(_) => {
            kprint(b"test-harness: thread_create FAIL\n");
            exit(1);
        }
    };
    kprint(b"test-harness: created worker thread; awaiting its fault\n");

    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        if waited < 1 {
            kprint(b"test-harness: wait FAIL\n");
            exit(1);
        }
        // SAFETY: NOTIF is a valid 64-byte writable out-param.
        let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
        if r != 0 {
            continue; // WouldBlock: re-block on the channel
        }
        // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
        let kind = unsafe { (&raw const NOTIF.kind).read() };
        if kind == KIND_SEG_FAULT {
            break;
        }
    }

    // SAFETY: WORKER_REGS is a valid writable RegisterValues out-param.
    let gr = unsafe { syscall4(SYS_THREAD_GET_REGISTERS, worker.raw().0, (&raw mut WORKER_REGS) as u64, 0, 0) };
    // SAFETY: the kernel wrote the 18-register snapshot into WORKER_REGS, or nothing.
    let regs = unsafe { (&raw const WORKER_REGS.regs).read() };
    let marked = MARKED.iter().all(|&i| regs[i] == mark(i));
    if gr != 0 || regs[0] != FAULT_ADDR || !marked || regs[REG_RIP] == 0 {
        kprint(b"test-harness: get_registers FAIL (not the registers the worker faulted with)\n");
        exit(1);
    }
    let rip = regs[REG_RIP];
    Line::new().s(b"test-harness: worker faulted @ rip=").x(rip).s(b" ; terminating").end();

    // SAFETY: register-only syscall.
    let er = unsafe { syscall4(SYS_EXCEPTION_RESUME, worker.raw().0, DISPOSITION_TERMINATE, 7, 0) };
    if er != 0 {
        kprint(b"test-harness: exception_resume FAIL\n");
        exit(1);
    }
    // The worker is not this process's last thread, so its termination produces no `ChildExited`.
    kprint(b"test-harness: worker terminated\n");
}
