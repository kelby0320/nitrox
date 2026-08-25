//! Cross-CPU TLB shootdown — architecture-neutral coordination.
//!
//! x86 (and every SMP arch we target) keeps *memory* coherent across CPUs in
//! hardware, but **not** the per-CPU TLB / paging-structure caches. After a CPU
//! edits a page table — unmapping a page, tightening its protection, or freeing a
//! page-table / kernel-stack frame for reuse — every *other* CPU that might hold a
//! cached translation for that address must invalidate it before the edit can be
//! relied upon (and, for a freed frame, before it is handed back to the allocator).
//!
//! This module drives that invalidation: it publishes what to invalidate, signals
//! the other online CPUs via the architecture's shootdown IPI
//! ([`crate::arch::send_shootdown_ipi`]), and **waits** until every target has
//! acknowledged. The transport (the IPI itself, the dense-index → APIC-id map) is
//! in the arch layer; the protocol here is arch-neutral.
//!
//! ## Model (v1: broadcast + synchronous, IF-robust)
//! One shootdown at a time system-wide (serialised by [`LOCK`]); it targets
//! **every online CPU, including the initiator's own** (a self-IPI rather than a
//! local invalidation — see below), rather than only those currently running the
//! affected address space. That is always correct and is required for the kernel
//! vmap (kernel-stack mappings live in every address space); per-address-space
//! targeting is a later optimisation.
//!
//! The whole request — lock acquisition, IPIs, ack spin — runs with **interrupts
//! enabled**, saving and restoring the caller's interrupt state (F1, decision log
//! 2026-07-21). Two consequences:
//!
//! - **No mutual-wait deadlock, from any caller.** Callers reach here from
//!   IF-masked contexts (syscall bodies run masked end-to-end; the ring-3
//!   exception path) via `reap_pending → KernelStack::Drop`. An IF-masked
//!   spinner — on [`LOCK`] or on the acks — could never service *another*
//!   initiator's shootdown IPI, so two IF=0 initiators deadlocked. Enabling IF
//!   for the window restores the invariant the ack protocol depends on: anyone
//!   waiting can always take an incoming shootdown IPI.
//! - **The initiator may be preempted — and migrate — mid-window**, so the
//!   request must not depend on *where* the initiator runs. Targeting every
//!   online CPU (self included) makes the target set position-independent:
//!   whichever CPU the initiator resumes on, every CPU that could hold the stale
//!   translation invalidates exactly once, and the ack count is exact.
//!
//! ## Caller contract
//! Call from **preemptible kernel context only** — a thread body, a syscall, or
//! the ring-3 exception path — never from an IRQ handler or DPC (the window
//! enables interrupts, and frees — the only initiators — are already forbidden
//! there), and **never while holding any spinlock** (in particular not `SCHED`:
//! remote CPUs spin for it IF-masked and could not ack; and preemption while
//! holding a lock stalls every other waiter). Shootdown sites live in the mm
//! layer (unmap / frame free), reached via `reap_pending` outside all locks,
//! which satisfies this.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::cpu::ArchCpu;
use crate::arch::paging::ArchPaging;
use crate::arch::smp::ArchSmp;
use crate::arch::{Cpu, MAX_CPUS, Paging, Smp, send_shootdown_ipi};
use crate::libkern::SpinLock;
use crate::mm::VirtAddr;
use crate::libkern::lockrank::LockRank;

/// Serialises shootdown initiators: only one request is in flight system-wide, so
/// the single global request block below is unambiguous. A **plain** spinlock (not
/// IRQ-masking) so a CPU spinning for acknowledgements still takes shootdown IPIs.
///
/// Interrupt work therefore runs beneath this lock — a DPC drain at an interrupt tail, a
/// timer tick taking `SCHED`. That is ordinary rather than exceptional: the rank order
/// restarts at an interrupt boundary (see `libkern::lockrank` § Interrupt scopes), so this
/// is an ordinary rank and not, as it was when it first landed, an exemption. The caller
/// contract above — **no other lock held** — is stricter than the rank and is asserted
/// separately in debug builds.
static LOCK: SpinLock<()> = SpinLock::new(LockRank::TlbShootdown, ());

/// `true` for a whole-TLB flush (CR3 reload), `false` for a single-page `invlpg`.
static REQUEST_ALL: AtomicBool = AtomicBool::new(false);
/// The page to invalidate when `REQUEST_ALL == false` (linear address).
static REQUEST_VA: AtomicU64 = AtomicU64::new(0);
/// A dense CPU index must fit in [`ACKED`]'s bits. `on_ipi` shifts by `current_cpu()` with
/// no range guard of its own — deliberately, since a CPU servicing an IPI has an identity by
/// construction — so this is where that assumption is stated rather than assumed.
const _: () = assert!(MAX_CPUS <= 64, "ACKED is a u64 bitmask indexed by dense CPU id");

/// Bitmask of CPUs that have acknowledged the current request (dense index → bit).
///
/// **A mask rather than a countdown, and that is a correctness choice.** A count cannot say
/// *which* CPU is outstanding, and [`shootdown`] now has to answer exactly that: a CPU that
/// parks between the target snapshot and its acknowledgement can never send one, and the
/// initiator must be able to tell that case from "not yet". A count also makes a late
/// decrement dangerous — it would silently satisfy the *next* request — whereas a stale
/// `fetch_or` can only set a bit for a CPU that has left the online set, which no later
/// request targets.
static ACKED: AtomicU64 = AtomicU64::new(0);

/// Invalidate the translation for `va` on this CPU and every other online CPU,
/// returning once all remote CPUs have done so. Call after the page-table edit
/// that made the old translation stale (and before freeing any frame it named).
pub fn shootdown_page(va: VirtAddr) {
    shootdown(Some(va));
}

/// Invalidate **all** non-global translations on this CPU and every other online
/// CPU (a CR3 reload per CPU). For edits that touch many pages at once.
#[allow(dead_code)] // used as broad-invalidation sites land (e.g. address-space teardown)
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

    // Fast path: sole online CPU (or pre-SMP boot) — just invalidate locally.
    // With no other CPU there is no cross-CPU stale entry, no possible second
    // initiator, and no preemption target to migrate to.
    if online & !(1u64 << me) == 0 {
        // SAFETY: ring-0; local invalidation of a caller-owned page-table change.
        unsafe { invalidate_local(va) };
        return;
    }

    // Run the whole request with **preemption disabled** but interrupts
    // **enabled**, restoring the caller's IF state after. The split matters:
    // - IF enabled (F1): an IF-masked spinner here could never service another
    //   initiator's shootdown IPI — two IF=0 initiators deadlock.
    // - Preemption disabled (F12): a holder of [`LOCK`] descheduled mid-window
    //   starves every spinner for a scheduling round — and *forever* when the
    //   holder is the idle thread (reaping stacks), which is never re-picked
    //   while the spinners keep every CPU busy. Only the switch is deferred;
    //   IRQ handlers (this CPU's own shootdown IPI included) still run.
    // With preemption off the initiator cannot migrate mid-window; the
    // all-online-CPUs target set (self-IPI included) is kept anyway — it is
    // simpler than a local-invalidate special case and immune to revisiting.
    crate::sched::preempt_disable();
    let prev_if = Cpu::interrupts_enabled();
    // SAFETY: ring-0, preemptible kernel context (the caller contract); the IDT
    // and timer are live (APs are online). Restored below.
    unsafe { Cpu::interrupts_enable() };

    {
        let _guard = LOCK.lock();

        // Publish the request before signalling any target (Release pairs with
        // the targets' Acquire loads in `on_ipi`).
        REQUEST_ALL.store(va.is_none(), Ordering::Relaxed);
        REQUEST_VA.store(va.map(|v| v.as_u64()).unwrap_or(0), Ordering::Relaxed);
        ACKED.store(0, Ordering::Release);

        // Signal every online CPU — including the one we are running on (a
        // self-IPI, serviced during the ack spin below). A CPU that comes
        // online after this snapshot cannot hold the stale translation: the
        // caller cleared the PTEs *before* initiating, so a later walk caches
        // only the new state.
        for cpu in 0..MAX_CPUS {
            if online & (1u64 << cpu) != 0 {
                send_shootdown_ipi(cpu);
            }
        }

        // Wait for every target (self included) to acknowledge. Interrupts are
        // enabled, so this CPU services its own IPI — and any other initiator's
        // — while spinning.
        //
        // **Stop waiting on a target that has left the online set.** `online` was sampled
        // before the IPIs went out; a CPU that parks inside that window has already run
        // `sched::leave_online` and is on its way to `cli; hlt`, so its IPI is either
        // unserviced or never will be — and waiting for it hangs this request forever,
        // taking [`LOCK`] and every later initiator with it. That is the same
        // whole-machine deadlock a park caused before it cleared its bit (2026-08-13); this
        // closes the residual window the fix left open.
        //
        // **Skipping it is sound because the CPU is gone, not merely slow.** A bit is
        // cleared only by `leave_online`, whose sole caller is `Cpu::halt_loop` — after
        // which that CPU executes nothing but the halt loop's own text forever. It cannot
        // use a stale translation, so the invalidation it never performed cannot be
        // observed. This is *not* a timeout: a live CPU that is merely late is still in the
        // mask and is still waited for, however long it takes.
        while !wait_satisfied(online, ACKED.load(Ordering::Acquire), crate::sched::online_mask())
        {
            core::hint::spin_loop();
        }
    }

    // **This caller needs `interrupts_restore` to be unconditional**, and is the only one
    // that does: `prev_if` is false (SFMASK masked `IF` at syscall entry), but *this function
    // enabled interrupts itself* above, so "restoring" false has real work to do. While the
    // `else` arm was a no-op — commented "leave IF clear — it already is" — the rest of every
    // SMP `sys_memory_unmap` ran with `IF` set, out through the `sysretq` stub's window on the
    // user stack. Do not reintroduce that no-op; `syscall_dispatch` asserts against it.
    // SAFETY: ring-0; restore the interrupt state captured above.
    unsafe { Cpu::interrupts_restore(prev_if) };
    // Re-enable preemption last; a reschedule latched during the window (a
    // wake IPI aimed at this CPU, or a tick expiry) is replayed here.
    crate::sched::preempt_enable();
}

/// Whether the initiator may stop waiting: every target has either acknowledged, or has
/// left the online set and so can never acknowledge.
///
/// Split out from the spin so the rule is pinned by a host test. **Not a timeout** — a live
/// target that is merely slow stays in `still_online` and is still waited for.
fn wait_satisfied(targets: u64, acked: u64, still_online: u64) -> bool {
    // One clause, not two: `outstanding == 0` is already covered, since an empty set
    // intersects nothing. An earlier version spelt both out and no test could tell them
    // apart, which is a fair definition of dead code.
    let outstanding = targets & !acked;
    outstanding & still_online == 0
}

/// Handle an incoming TLB-shootdown IPI on this CPU: invalidate as the current
/// request directs, then acknowledge. Called by the arch IPI dispatcher (which
/// EOIs afterward). Never blocks.
pub fn on_ipi() {
    let va = if REQUEST_ALL.load(Ordering::Acquire) {
        None
    } else {
        Some(VirtAddr::new(REQUEST_VA.load(Ordering::Acquire)))
    };
    // SAFETY: ring-0 IPI context; performs only a TLB invalidation.
    unsafe { invalidate_local(va) };
    // Acknowledge after the invalidation is issued, so the initiator observes
    // completion only once this CPU can no longer use the stale translation.
    ACKED.fetch_or(1u64 << (Smp::current_cpu() as usize), Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::wait_satisfied;

    /// The initiator waits for a live target and gives up on a departed one — and nothing
    /// else.
    ///
    /// The third case is the bug this exists for: before it, a CPU that parked between the
    /// target snapshot and its acknowledgement hung the request forever, holding `LOCK` and
    /// with it every later shootdown on the machine.
    #[test]
    fn waiting_ends_on_acknowledgement_or_departure_but_never_on_lateness() {
        let targets = 0b1111;

        // Nobody has acked yet, everyone is up: keep waiting.
        assert!(!wait_satisfied(targets, 0b0000, 0b1111));

        // All acked: done, regardless of who is still up.
        assert!(wait_satisfied(targets, 0b1111, 0b1111));
        assert!(wait_satisfied(targets, 0b1111, 0b0001));

        // CPU 2 has not acked and has left the online set: it never will — stop.
        assert!(wait_satisfied(targets, 0b1011, 0b1011));

        // CPU 2 has not acked and is still up: it is merely late — keep waiting. This is
        // what makes the rule a departure check and not a timeout.
        assert!(!wait_satisfied(targets, 0b1011, 0b1111));

        // Two outstanding, one departed and one still up: the live one still holds it.
        assert!(!wait_satisfied(targets, 0b1001, 0b1101));

        // A CPU that was never a target cannot hold the request up, however it is marked.
        assert!(wait_satisfied(0b0011, 0b0011, 0b1111));
    }
}
