//! The faulting worker, whose registers `test-harness` checks one by one after its `SegFault`.

use crate::{Notif, PAGE, refused, wait_readable};
use core::arch::asm;
use libkern::{KError, KIND_SEG_FAULT, REG_RIP};
use libos::log::{self, Line};
use libos::thread::{self, Disposition, Stack};
use libos::{Handle, MapReadWrite, Memory, exit, time};

const FAULT_ADDR: u64 = 0xdead_0000;

/// The value the worker gives the register at `index` of [`RegisterValues::regs`] before it faults.
const fn mark(index: usize) -> u64 {
    0x5afe_0000_0000_0000 | index as u64
}

/// The registers the worker sets, by index: all an `asm!` may name but `rax`, the address.
const MARKED: [usize; 12] = [2, 3, 4, 5, 8, 9, 10, 11, 12, 13, 14, 15];

/// The worker's entry: fault on a store with each register in [`MARKED`] at its [`mark`], once
/// the main thread has had time to reach its wait, so the fault finds it waiting.
extern "C" fn worker_fault(_: u64, _: u64, _: u64, _: u64) -> ! {
    let _ = time::sleep(10_000_000);
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

/// **The kernel cannot write through a read-only user mapping**: a syscall's copy-out to one
/// faults on the user side, and recovery answers `FaultFromUser`, the page untouched.
fn a_copy_out_to_a_read_only_page_is_refused() {
    let page = Handle::<Memory, MapReadWrite>::create(PAGE)
        .and_then(Handle::into_read_only)
        .and_then(|mem| mem.map(PAGE).map(|mapping| (mem, mapping)));
    let Ok((_mem, mapping)) = page else {
        log::write(b"test-harness: read-only page FAIL\n");
        exit(1);
    };
    let read = refused::clock_read_into(mapping.as_ptr());
    let mut after = [0u8; 8];
    if read != KError::FaultFromUser as i64 || mapping.read_into(0, &mut after).is_err() || after != [0; 8] {
        Line::new().s(b"test-harness: a copy to a read-only page landed: ").i(read).s(b" FAIL").end();
        exit(1);
    }
    log::write(b"test-harness: a copy to a read-only page is refused ok\n");
}

/// The exception demo: see the worker's `SegFault` on `notif`, check its registers, terminate it.
pub(crate) fn run(notif: &Notif) {
    a_copy_out_to_a_read_only_page_is_refused();
    log::write(b"test-harness: mapping a worker stack\n");
    let Ok(stack) = Stack::new(PAGE) else {
        log::write(b"test-harness: worker stack create FAIL\n");
        exit(1);
    };
    let Ok(worker) = thread::start(worker_fault, stack, 0) else {
        log::write(b"test-harness: thread_create FAIL\n");
        exit(1);
    };
    log::write(b"test-harness: created worker thread; awaiting its fault\n");

    loop {
        if !wait_readable(notif) {
            log::write(b"test-harness: wait FAIL\n");
            exit(1);
        }
        // `WouldBlock`: re-block on the channel.
        let Ok(Some(n)) = notif.recv() else { continue };
        if n.kind == KIND_SEG_FAULT {
            break;
        }
    }

    let regs = match worker.fault_registers() {
        Ok(r) if r.regs[0] == FAULT_ADDR && MARKED.iter().all(|&i| r.regs[i] == mark(i)) && r.regs[REG_RIP] != 0 => {
            r.regs
        }
        _ => {
            log::write(b"test-harness: get_registers FAIL (not the registers the worker faulted with)\n");
            exit(1);
        }
    };
    let rip = regs[REG_RIP];
    Line::new().s(b"test-harness: worker faulted @ rip=").x(rip).s(b" ; terminating").end();

    if worker.settle(Disposition::Terminate(7)).is_err() {
        log::write(b"test-harness: exception_resume FAIL\n");
        exit(1);
    }
    // The worker is not this process's last thread, so its termination produces no `ChildExited`.
    log::write(b"test-harness: worker terminated\n");
}
