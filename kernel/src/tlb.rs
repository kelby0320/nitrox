//! Cross-CPU TLB shootdown: architecture-neutral coordination.
//!
//! Hardware keeps *memory* coherent across CPUs but not the per-CPU TLB and paging-structure
//! caches. After a page-table edit (an unmap, a tighter protection, a page-table or kernel-stack
//! frame freed for reuse) every other CPU that might cache the translation must invalidate it
//! before the edit can be relied on, and before a freed frame goes back to the allocator.
//!
//! This module publishes what to invalidate, signals the online CPUs via the arch shootdown IPI
//! ([`crate::arch::send_shootdown_ipi`]), and **waits** until every target has acknowledged.
//!
//! ## Model
//!
//! One shootdown at a time system-wide (serialised by [`LOCK`]), targeting **every online CPU, the
//! initiator's own included** (a self-IPI): always correct, and required for the kernel vmap, whose
//! kernel-stack mappings live in every address space.
//!
//! The whole request runs with **interrupts enabled** (the caller's state is restored; decision log
//! 2026-07-21), because callers arrive IF-masked (syscall bodies, the ring-3 exception path, via
//! `reap_pending` and `KernelStack::Drop`) and an IF-masked spinner, on [`LOCK`] or on the acks,
//! could never service another initiator's IPI: two such initiators deadlock. With IF on, anyone
//! waiting can always take an incoming shootdown IPI.
//!
//! ## Caller contract
//!
//! Call from **preemptible kernel context only**, never from an IRQ handler or DPC (the window
//! enables interrupts, and frees, the only initiators, are forbidden there) and **never while
//! holding a spinlock**: not `SCHED` (remote CPUs spin for it IF-masked and could not ack), and
//! preempting a lock holder stalls every waiter. The mm layer's shootdown sites are reached via
//! `reap_pending` outside all locks.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::cpu::ArchCpu;
use crate::arch::paging::ArchPaging;
use crate::arch::smp::ArchSmp;
use crate::arch::{Cpu, MAX_CPUS, Paging, Smp, send_shootdown_ipi};
use crate::libkern::SpinLock;
use crate::libkern::lockrank::LockRank;
use crate::mm::VirtAddr;

/// Serialises initiators: one request in flight system-wide, so the single request block below is
/// unambiguous. A **plain** spinlock (not IRQ-masking) so a CPU spinning for acknowledgements still
/// takes shootdown IPIs, which means interrupt work (a DPC drain, a tick taking `SCHED`) runs
/// beneath it; the rank order restarts at an interrupt boundary (see `libkern::lockrank` §
/// Interrupt scopes). The caller contract above, **no other lock held**, is stricter than the rank
/// and is asserted separately in debug builds.
static LOCK: SpinLock<()> = SpinLock::new(LockRank::TlbShootdown, ());

/// `true` for a whole-TLB flush (CR3 reload), `false` for a single-page `invlpg`.
static REQUEST_ALL: AtomicBool = AtomicBool::new(false);
/// The page to invalidate when `REQUEST_ALL == false` (linear address).
static REQUEST_VA: AtomicU64 = AtomicU64::new(0);
// `on_ipi` shifts by `current_cpu()` with no range guard, since a CPU servicing an IPI has an
// identity by construction; this states that a dense index must fit in [`ACKED`]'s bits.
const _: () = assert!(MAX_CPUS <= 64, "ACKED is a u64 bitmask indexed by dense CPU id");

/// Bitmask of CPUs that have acknowledged the current request (dense index → bit).
///
/// **A mask rather than a countdown, as a correctness choice.** A count cannot say *which* CPU is
/// outstanding, and [`shootdown`] must tell a CPU that parked between the target snapshot and its
/// acknowledgement from one that is merely late. A late decrement would also silently satisfy the
/// *next* request, whereas a stale `fetch_or` only sets the bit of a CPU that has left the online
/// set, which no later request targets.
static ACKED: AtomicU64 = AtomicU64::new(0);

/// Invalidate the translation for `va` on this CPU and every other online CPU,
/// returning once all remote CPUs have done so. Call after the page-table edit
/// that made the old translation stale (and before freeing any frame it named).
pub fn shootdown_page(va: VirtAddr) {
    shootdown(Some(va));
}

/// Invalidate **all** non-global translations on this CPU and every other online
/// CPU (a CR3 reload per CPU). For edits that touch many pages at once.
#[allow(dead_code, reason = "for broad invalidations to come, such as an address space's teardown")]
pub fn shootdown_all() {
    shootdown(None);
}

/// Perform the local invalidation for a request. `None` = whole TLB.
///
/// # Safety
/// Ring-0. The caller owns the page-table change this reflects.
unsafe fn invalidate_local(va: Option<VirtAddr>) {
    // SAFETY: forwarded — a ring-0 TLB invalidation for a caller-owned change.
    unsafe {
        match va {
            Some(v) => Paging::flush_tlb_page(v),
            None => Paging::flush_tlb_all(),
        }
    }
}

fn shootdown(va: Option<VirtAddr>) {
    let me = Smp::current_cpu() as usize;
    let online = crate::sched::online_mask();

    // Sole online CPU (or pre-SMP boot): invalidate locally; no other CPU holds a stale entry.
    if online & !(1u64 << me) == 0 {
        // SAFETY: ring-0; local invalidation of a caller-owned page-table change.
        unsafe { invalidate_local(va) };
        return;
    }

    // Preemption **disabled**, interrupts **enabled**, the caller's IF restored after. IF on, so a
    // spinner here still services another initiator's IPI. Preemption off, because a descheduled
    // holder of [`LOCK`] starves every spinner, *forever* if it is the idle thread (reaping
    // stacks), which is never re-picked while the spinners keep every CPU busy. Only the switch is
    // deferred; IRQ handlers, this CPU's own shootdown IPI included, still run. The initiator
    // therefore cannot migrate mid-window; the all-online target set is kept as simpler than a
    // local special case.
    crate::sched::preempt_disable();
    let prev_if = Cpu::interrupts_enabled();
    // SAFETY: ring-0, preemptible kernel context (the caller contract); the IDT
    // and timer are live (APs are online). Restored below.
    unsafe { Cpu::interrupts_enable() };

    {
        let _guard = LOCK.lock();

        // Publish the request before signalling any target (Release pairs with `on_ipi`'s Acquire).
        REQUEST_ALL.store(va.is_none(), Ordering::Relaxed);
        REQUEST_VA.store(va.map(|v| v.as_u64()).unwrap_or(0), Ordering::Relaxed);
        ACKED.store(0, Ordering::Release);

        // Signal every online CPU, this one included (a self-IPI, serviced in the ack spin). A CPU
        // that comes online after this snapshot cannot hold the stale translation: the caller
        // cleared the PTEs *before* initiating, so a later walk caches only the new state.
        for cpu in 0..MAX_CPUS {
            if online & (1u64 << cpu) != 0 {
                send_shootdown_ipi(cpu);
            }
        }

        // Wait for every target to acknowledge; interrupts are on, so this CPU services its own IPI
        // and any other initiator's while spinning.
        //
        // **Stop waiting on a target that has left the online set.** `online` was sampled before
        // the IPIs went out; a CPU that parks in that window has run `sched::leave_online` and is
        // on its way to `cli; hlt`, so it may never service its IPI, and waiting would hang this
        // request, [`LOCK`] and every later initiator (the whole-machine deadlock of 2026-08-13).
        // Sound because the CPU is gone, not slow: a bit is cleared only by `leave_online`, whose
        // sole caller is `Cpu::halt_loop`, after which the CPU runs nothing but the halt loop's own
        // text and cannot use a stale translation. Not a timeout: a live CPU that is merely late
        // stays in the mask and is waited for however long it takes.
        while !wait_satisfied(online, ACKED.load(Ordering::Acquire), crate::sched::online_mask()) {
            core::hint::spin_loop();
        }
    }

    // Unconditional `interrupts_restore`: on the syscall path `prev_if` is false (SFMASK masked
    // `IF`) but this function enabled interrupts itself, so restoring false has real work to do. A
    // no-op there ran the rest of every SMP `sys_memory_unmap` with `IF` set, out through the
    // `sysretq` stub's window on the user stack; `syscall_dispatch` asserts against it.
    // SAFETY: ring-0; restore the interrupt state captured above.
    unsafe { Cpu::interrupts_restore(prev_if) };
    // Re-enable preemption last; a reschedule latched during the window is replayed here.
    crate::sched::preempt_enable();
}

/// Whether the initiator may stop waiting: every target has either acknowledged or left the online
/// set and so can never acknowledge. Split out so a host test pins the rule. **Not a timeout**: a
/// live target that is merely slow stays in `still_online` and is waited for.
fn wait_satisfied(targets: u64, acked: u64, still_online: u64) -> bool {
    // One clause: an empty `outstanding` already intersects nothing.
    let outstanding = targets & !acked;
    outstanding & still_online == 0
}

/// Handle an incoming TLB-shootdown IPI on this CPU: invalidate as the current request directs,
/// then acknowledge. Called by the arch IPI dispatcher (which EOIs afterward). Never blocks.
pub fn on_ipi() {
    let va = if REQUEST_ALL.load(Ordering::Acquire) {
        None
    } else {
        Some(VirtAddr::new(REQUEST_VA.load(Ordering::Acquire)))
    };
    // SAFETY: ring-0 IPI context; performs only a TLB invalidation.
    unsafe { invalidate_local(va) };
    // Acknowledge after the invalidation is issued: the initiator sees completion only once this
    // CPU can no longer use the stale translation.
    ACKED.fetch_or(1u64 << (Smp::current_cpu() as usize), Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::wait_satisfied;

    /// The initiator waits for a live target and gives up on a departed one, and nothing else.
    #[test]
    fn waiting_ends_on_acknowledgement_or_departure_but_never_on_lateness() {
        let targets = 0b1111;

        assert!(!wait_satisfied(targets, 0b0000, 0b1111));

        assert!(wait_satisfied(targets, 0b1111, 0b1111));
        assert!(wait_satisfied(targets, 0b1111, 0b0001));

        // CPU 2 has not acked and has left the online set: it never will.
        assert!(wait_satisfied(targets, 0b1011, 0b1011));

        // CPU 2 has not acked and is still up: merely late, so keep waiting (not a timeout).
        assert!(!wait_satisfied(targets, 0b1011, 0b1111));

        assert!(!wait_satisfied(targets, 0b1001, 0b1101));

        assert!(wait_satisfied(0b0011, 0b0011, 0b1111));
    }
}
