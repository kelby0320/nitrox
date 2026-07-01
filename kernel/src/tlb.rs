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
//! ## Model (v1: broadcast + synchronous)
//! One shootdown at a time system-wide (serialised by [`LOCK`]); it targets **all**
//! other online CPUs rather than only those currently running the affected address
//! space. That is always correct and is required for the kernel vmap (kernel-stack
//! mappings live in every address space); per-address-space targeting is a later
//! optimisation. The initiator spins for acknowledgements; because `LOCK` is a
//! plain (non-IRQ-masking) spinlock and callers run with interrupts enabled, a
//! CPU waiting here still services an incoming shootdown IPI, so two initiators
//! cannot deadlock.
//!
//! ## Caller contract
//! Invoke with **interrupts enabled** and **without holding a lock that a remote
//! CPU could be waiting on with interrupts masked** (in particular not the `SCHED`
//! run-queue lock). Shootdown sites live in the mm layer (unmap / protect / frame
//! free), which satisfies this.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::arch::paging::ArchPaging;
use crate::arch::smp::ArchSmp;
use crate::arch::{MAX_CPUS, Paging, Smp, send_shootdown_ipi};
use crate::libkern::SpinLock;
use crate::mm::VirtAddr;

/// Serialises shootdown initiators: only one request is in flight system-wide, so
/// the single global request block below is unambiguous. A **plain** spinlock (not
/// IRQ-masking) so a CPU spinning for acknowledgements still takes shootdown IPIs.
static LOCK: SpinLock<()> = SpinLock::new(());

/// `true` for a whole-TLB flush (CR3 reload), `false` for a single-page `invlpg`.
static REQUEST_ALL: AtomicBool = AtomicBool::new(false);
/// The page to invalidate when `REQUEST_ALL == false` (linear address).
static REQUEST_VA: AtomicU64 = AtomicU64::new(0);
/// Number of target CPUs that have not yet acknowledged the current request.
static PENDING: AtomicU32 = AtomicU32::new(0);

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
    let others = crate::sched::online_mask() & !(1u64 << me);

    // Fast path: sole online CPU (or pre-SMP boot) — just invalidate locally.
    if others == 0 {
        // SAFETY: ring-0; local invalidation of a caller-owned page-table change.
        unsafe { invalidate_local(va) };
        return;
    }

    let _guard = LOCK.lock();

    // Publish the request before signalling any target (Release pairs with the
    // targets' Acquire loads in `on_ipi`).
    REQUEST_ALL.store(va.is_none(), Ordering::Relaxed);
    REQUEST_VA.store(va.map(|v| v.as_u64()).unwrap_or(0), Ordering::Relaxed);
    PENDING.store(others.count_ones(), Ordering::Release);

    // Signal every other online CPU.
    for cpu in 0..MAX_CPUS {
        if others & (1u64 << cpu) != 0 {
            send_shootdown_ipi(cpu);
        }
    }

    // Invalidate locally while the targets work.
    // SAFETY: ring-0; local invalidation of a caller-owned page-table change.
    unsafe { invalidate_local(va) };

    // Wait for every target to acknowledge. Interrupts are enabled (caller
    // contract + non-masking `LOCK`), so this CPU still services any shootdown
    // IPI aimed at it while spinning — no mutual-wait deadlock.
    while PENDING.load(Ordering::Acquire) != 0 {
        core::hint::spin_loop();
    }
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
    PENDING.fetch_sub(1, Ordering::Release);
}
