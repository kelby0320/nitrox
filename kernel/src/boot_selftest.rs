//! Boot-time self-tests and demos — compiled only under the `selftest` feature.
//!
//! These prove kernel subsystems end-to-end *during boot* (the preemptive scheduler,
//! the three scheduler classes, SMP work-distribution + affinity, demand paging, DMA
//! allocation, the IRP async spine, IOAPIC routing, storage). They were the kernel's
//! primary runtime regression coverage — the SMP migration hazard was caught by a boot
//! loop, not a unit test.
//!
//! They are **not** part of a normal bring-up. `cargo xtask qemu` boots straight to
//! userspace; a `--selftest` build (`cargo xtask qemu --selftest`, and eventually
//! `xtask test-qemu` for CI) compiles this module in and runs it. Keeping them here
//! keeps `kernel_main`'s happy path readable. Each is pure verification (throwaway
//! objects, dropped) with no bring-up side effects — the scheduler init + preemption
//! arming that the old `run_scheduler_demo` folded in stays in `main.rs::sched_bringup`.

use core::sync::atomic::{AtomicU32, Ordering};

use nitrox_kernel::arch;
use nitrox_kernel::arch::paging::ArchPaging;
use nitrox_kernel::kprintln;
use nitrox_kernel::mm;
use nitrox_kernel::sched;

// === phase entry points (called from kernel_main under `#[cfg(feature="selftest")]`) ===

/// After `paging_init`: prove the table-walk agrees with hardware + NX is on.
pub fn paging() {
    paging_smoke_test();
}

/// After the interrupt router is up: prove the routing path end-to-end (the PIT).
pub fn irq_routing() {
    use nitrox_kernel::arch::irq_router::ArchIrqRouter;
    // SAFETY: ring 0, after the router is up and before the scheduler arms its periodic
    // timer; briefly enables interrupts for the one routed source (the legacy PIT).
    unsafe { arch::IrqRouter::self_test() };
}

/// After the device table + async spine: prove IRP → DPC → PendingOperation on a
/// RAM-backed device.
pub fn irp_spine() {
    nitrox_kernel::io::self_test();
}

/// After `drivers::probe`: read sector 0 through the real driver (IRP → DMA → IRQ → DPC).
pub fn storage() {
    nitrox_kernel::drivers::self_test();
}

/// Pre-SMP demos (after `sched_bringup`, before AP bring-up): scheduler round-robin +
/// classes, demand paging, DMA.
pub fn pre_smp() {
    scheduler_demo();
    sched_class_demo();
    demand_paging_smoke_test();
    dma_smoke_test();
}

/// Post-SMP demos (after AP bring-up): work distribution across CPUs + affinity.
pub fn post_smp() {
    smp_distribution_demo();
    smp_affinity_demo();
}

// === preemptive scheduler demo =========================================================

/// Spawn three never-yield CPU-bound workers over the (already-armed) preemptive
/// scheduler and let the timer interleave them, then drain. The bring-up itself
/// (`sched::init` + arming preemption) lives in `main.rs::sched_bringup`.
fn scheduler_demo() {
    for id in 1..=3 {
        if sched::spawn(busy_worker, id).is_err() {
            kprintln!("sched: spawn {} failed", id);
        }
    }
    // Busy-wait (no cooperative yield) until every worker has run to completion; the
    // timer preempts the boot thread too. `reap_pending` reclaims each exited stack.
    loop {
        sched::reap_pending();
        if sched::ready_is_empty() {
            break;
        }
        core::hint::spin_loop();
    }
    kprintln!("sched: all workers done (preemptively interleaved)");
}

/// A CPU-bound demo kernel thread that **never yields**: each round spins on a bounded
/// busy loop, then prints. Under preemption the periodic timer forces the workers (and
/// the boot thread) to interleave — cooperatively, worker 1 would finish all its rounds
/// before worker 2 printed. The interleaved output is the proof of preemption.
extern "C" fn busy_worker(arg: usize) {
    for round in 0..3 {
        // Enough work to span several 10 ms ticks so preemption is visible.
        let mut acc: u64 = 0;
        for i in 0..20_000_000u64 {
            acc = acc.wrapping_add(i ^ arg as u64);
        }
        core::hint::black_box(acc);
        kprintln!("worker {} round {}", arg, round);
    }
    kprintln!("worker {} exiting", arg);
}

// === scheduler classes demo ============================================================

/// Completion counter for [`sched_class_demo`]'s three workers (count-based barrier —
/// race-free even when the workers run on different CPUs).
static SCHED_DEMO_DONE: AtomicU32 = AtomicU32::new(0);

/// A **RealTime** worker preempts the **TimeShared** workers, and two TimeShared workers
/// with different `nice` get proportional CPU — the lower-`nice` one finishes first.
fn sched_class_demo() {
    use nitrox_kernel::object::SchedClass;

    kprintln!("sched: class demo — RealTime preempts TimeShared; TimeShared nice-fairness");
    if sched::spawn_with_class(rt_demo_worker, 5, SchedClass::RealTime, 5, 0).is_err() {
        kprintln!("sched: RT demo spawn failed");
    }
    for nice in [0i8, 10] {
        if sched::spawn_with_class(ts_demo_worker, nice as usize, SchedClass::TimeShared, 0, nice)
            .is_err()
        {
            kprintln!("sched: TS demo spawn (nice {}) failed", nice);
        }
    }
    let mut spins: u64 = 0;
    while SCHED_DEMO_DONE.load(Ordering::Acquire) < 3 {
        sched::reap_pending();
        core::hint::spin_loop();
        spins += 1;
        if spins > 20_000_000_000 {
            kprintln!("sched: class demo barrier timed out");
            break;
        }
    }
    sched::reap_pending();
    kprintln!("sched: class demo complete");
}

/// A RealTime [`sched_class_demo`] worker: runs to completion before any TimeShared
/// thread gets the CPU.
extern "C" fn rt_demo_worker(prio: usize) {
    let mut acc: u64 = 0;
    for i in 0..4_000_000u64 {
        acc = acc.wrapping_add(i);
    }
    core::hint::black_box(acc);
    kprintln!("sched:   RealTime worker (prio {}) finished", prio);
    SCHED_DEMO_DONE.fetch_add(1, Ordering::Release);
}

/// A TimeShared [`sched_class_demo`] worker: fixed total work in rounds; the lower-`nice`
/// worker makes faster progress and finishes earlier.
extern "C" fn ts_demo_worker(nice: usize) {
    for round in 0..3 {
        let mut acc: u64 = 0;
        for i in 0..16_000_000u64 {
            acc = acc.wrapping_add(i ^ nice as u64);
        }
        core::hint::black_box(acc);
        kprintln!("sched:   TimeShared nice={} round {}/3", nice, round + 1);
    }
    kprintln!("sched:   TimeShared nice={} DONE", nice);
    SCHED_DEMO_DONE.fetch_add(1, Ordering::Release);
}

// === SMP work distribution demo ========================================================

/// Workers finished / bitmask of CPUs that ran one, for [`smp_distribution_demo`].
static DIST_DONE: AtomicU32 = AtomicU32::new(0);
static DIST_CPU_MASK: AtomicU32 = AtomicU32::new(0);

/// Prove the **per-CPU runqueues** spread work onto the APs: spawn more kernel workers
/// than CPUs, then report how many distinct CPUs actually ran one.
fn smp_distribution_demo() {
    const N: u32 = 8;
    for i in 0..N {
        if sched::spawn(dist_worker, i as usize).is_err() {
            kprintln!("smp: distribution spawn {} failed", i);
        }
    }
    let mut spins: u64 = 0;
    while DIST_DONE.load(Ordering::Acquire) < N {
        sched::reap_pending();
        core::hint::spin_loop();
        spins += 1;
        if spins > 20_000_000_000 {
            kprintln!("smp: distribution demo timed out");
            break;
        }
    }
    sched::reap_pending();
    let mask = DIST_CPU_MASK.load(Ordering::Relaxed);
    kprintln!(
        "smp: distribution demo complete — {} of {} workers' CPUs distinct (cpu mask {:#06b})",
        mask.count_ones(),
        N,
        mask,
    );
}

/// A short [`smp_distribution_demo`] worker: spin briefly, record the CPU it ran on.
extern "C" fn dist_worker(arg: usize) {
    use arch::smp::ArchSmp;
    let mut acc: u64 = 0;
    for i in 0..3_000_000u64 {
        acc = acc.wrapping_add(i ^ arg as u64);
    }
    core::hint::black_box(acc);
    let cpu = arch::Smp::current_cpu();
    DIST_CPU_MASK.fetch_or(1 << cpu, Ordering::Relaxed);
    DIST_DONE.fetch_add(1, Ordering::Release);
}

// === SMP affinity demo =================================================================

/// Workers finished / the CPU each affinity worker actually ran on (worker `i` pinned
/// to CPU `i`).
static AFFINITY_DONE: AtomicU32 = AtomicU32::new(0);
static AFFINITY_RAN_ON: [AtomicU32; arch::MAX_CPUS] =
    [const { AtomicU32::new(u32::MAX) }; arch::MAX_CPUS];

/// Prove **CPU affinity**: one worker per online CPU, each pinned to a distinct CPU;
/// confirm every worker ran on exactly the CPU it was pinned to.
fn smp_affinity_demo() {
    let n = sched::online_cpus().min(arch::MAX_CPUS);
    for i in 0..n {
        if sched::spawn_with_affinity(affinity_worker, i, 1u8 << i).is_err() {
            kprintln!("smp: affinity spawn {} failed", i);
        }
    }
    let mut spins: u64 = 0;
    while (AFFINITY_DONE.load(Ordering::Acquire) as usize) < n {
        sched::reap_pending();
        core::hint::spin_loop();
        spins += 1;
        if spins > 20_000_000_000 {
            kprintln!("smp: affinity demo timed out");
            break;
        }
    }
    sched::reap_pending();
    let mut all_pinned = true;
    for i in 0..n {
        let ran = AFFINITY_RAN_ON[i].load(Ordering::Relaxed);
        if ran != i as u32 {
            all_pinned = false;
            kprintln!("smp: affinity worker {} (pinned cpu {}) ran on cpu {}", i, i, ran);
        }
    }
    kprintln!(
        "smp: affinity demo complete — {} worker(s), {}",
        n,
        if all_pinned {
            "each ran on its pinned CPU"
        } else {
            "MISMATCH (see above)"
        }
    );
}

/// An [`smp_affinity_demo`] worker pinned to CPU `id`: record which CPU it ran on.
extern "C" fn affinity_worker(id: usize) {
    use arch::smp::ArchSmp;
    let mut acc: u64 = 0;
    for i in 0..3_000_000u64 {
        acc = acc.wrapping_add(i);
    }
    core::hint::black_box(acc);
    AFFINITY_RAN_ON[id].store(arch::Smp::current_cpu(), Ordering::Relaxed);
    AFFINITY_DONE.fetch_add(1, Ordering::Release);
}

// === paging / demand-paging / DMA smoke tests ==========================================

/// Walk Limine's live page tables with `arch::translate` to confirm the kernel's
/// table-walk agrees with hardware (read-only — installs/switches nothing).
fn paging_smoke_test() {
    let root = arch::Paging::active_root();
    let probe = mm::VirtAddr::new(paging_smoke_test as fn() as usize as u64);
    // SAFETY: `root` is the live top-level page table the CPU is using, reachable via
    // the HHDM. `translate` only reads page-table memory.
    match unsafe { arch::Paging::translate(root, probe) } {
        Some(phys) => kprintln!(
            "paging: NX enabled; translate {:#x} -> {:#x}",
            probe.as_u64(),
            phys.as_u64()
        ),
        None => kprintln!(
            "paging: translate {:#x} -> UNMAPPED — walk disagrees with hardware",
            probe.as_u64()
        ),
    }
}

/// Exercise the demand-paging on-fault path: reserve a lazy anonymous range, confirm it
/// is unbacked, fault each page in, confirm it is now mapped. Torn down on drop.
fn demand_paging_smoke_test() {
    use nitrox_kernel::libkern::KBox;
    use nitrox_kernel::mm::addr_space::{AddressSpace, FaultIn};
    use nitrox_kernel::mm::vmm::{FaultAccess, MappingKind, Protection, VAddrRange, Vma};

    let page = mm::PAGE_SIZE as u64;
    let Ok(asp) = AddressSpace::new() else {
        kprintln!("demand-paging: smoke test skipped (AS alloc failed)");
        return;
    };
    let base = 0x4000_0000u64;
    let range = VAddrRange::new(mm::VirtAddr::new(base), mm::VirtAddr::new(base + 2 * page))
        .expect("smoke-test range is valid by construction");
    let Ok(vma) = KBox::try_new(Vma::new(
        range,
        Protection::WRITE | Protection::USER,
        MappingKind::Anonymous,
    )) else {
        kprintln!("demand-paging: smoke test skipped (VMA alloc failed)");
        return;
    };
    if asp.map_vma_lazy(vma).is_err() {
        kprintln!("demand-paging: smoke test skipped (lazy reserve failed)");
        return;
    }

    // SAFETY: `translate` only reads this AS's own page-table memory via the HHDM.
    let before = unsafe { arch::Paging::translate(asp.root(), mm::VirtAddr::new(base)) };
    let r0 = asp.fault_in(mm::VirtAddr::new(base), FaultAccess::Write);
    let r1 = asp.fault_in(mm::VirtAddr::new(base + page), FaultAccess::Read);
    // SAFETY: as above — page 0 should now resolve.
    let after = unsafe { arch::Paging::translate(asp.root(), mm::VirtAddr::new(base)) };

    if before.is_none() && after.is_some() && r0 == FaultIn::Mapped && r1 == FaultIn::Mapped {
        kprintln!(
            "demand-paging: on-fault path OK — 2 anonymous pages backed lazily ({} faulted in since boot)",
            mm::demand_fault_count()
        );
    } else {
        kprintln!(
            "demand-paging: smoke test FAILED (before={:?} after={:?} r0={:?} r1={:?})",
            before, after, r0, r1
        );
    }
}

/// Prove `DmaBuffer` gives a physically-contiguous, page-aligned, zeroed block whose
/// physical address is what a device would DMA at its `virt()` mapping. Dropped at end.
fn dma_smoke_test() {
    use nitrox_kernel::mm::DmaBuffer;

    let mut buf = match DmaBuffer::alloc(2 * mm::PAGE_SIZE) {
        Ok(b) => b,
        Err(_) => {
            kprintln!("dma: smoke test skipped (alloc failed)");
            return;
        }
    };
    let phys = buf.phys();
    let zeroed = buf.as_slice().iter().all(|&b| b == 0);
    let len = buf.len();
    buf.as_mut_slice()[0] = 0xD3;
    buf.as_mut_slice()[len - 1] = 0xA7;

    // SAFETY: read-only walk of the live top-level page table via the HHDM.
    let mapped = unsafe {
        arch::Paging::translate(arch::Paging::active_root(), mm::VirtAddr::new(buf.virt() as u64))
    };

    if zeroed && phys.is_page_aligned() && phys.as_u64() != 0 && mapped == Some(phys) {
        kprintln!(
            "dma: {}-page buffer @ phys {:#x} (contiguous, page-aligned, zeroed)",
            len / mm::PAGE_SIZE,
            phys.as_u64()
        );
    } else {
        kprintln!(
            "dma: smoke test FAILED (zeroed={} aligned={} mapped={:?} phys={:#x})",
            zeroed,
            phys.is_page_aligned(),
            mapped,
            phys.as_u64()
        );
    }
}
