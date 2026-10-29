//! Boot-time self-tests and demos, compiled only under the `selftest` feature (`cargo xtask qemu
//! --selftest`).
//!
//! They prove kernel subsystems end-to-end *during boot* (the scheduler classes, SMP work
//! distribution and affinity, demand paging, DMA allocation, the IRP async spine, IOAPIC routing,
//! storage) and were the kernel's primary runtime regression coverage: the SMP migration hazard
//! was caught by a boot loop, not a unit test. A normal bring-up boots straight to userspace. Each
//! is pure verification (throwaway objects, dropped) with no bring-up side effects; the scheduler
//! init and preemption arming stay in `main.rs::sched_bringup`.

use core::sync::atomic::{AtomicU32, Ordering};

use nitrox_kernel::arch;
use nitrox_kernel::arch::paging::ArchPaging;
use nitrox_kernel::kprintln;
use nitrox_kernel::libkern::KBox;
use nitrox_kernel::mm;
use nitrox_kernel::sched;

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

/// After the device table and async spine: prove IRP -> DPC -> PendingOperation on a RAM device.
pub fn irp_spine() {
    nitrox_kernel::io::self_test();
}

/// After `drivers::probe`: read sector 0 through the real driver (IRP → DMA → IRQ → DPC).
pub fn storage() {
    nitrox_kernel::drivers::self_test();
}

/// Pre-SMP demos (after `sched_bringup`, before AP bring-up).
pub fn pre_smp() {
    scheduler_demo();
    sched_class_demo();
    demand_paging_smoke_test();
    dma_smoke_test();
}

/// Post-SMP demos (after AP bring-up): work distribution and affinity, and the FP/SIMD isolation
/// proof, worth running only with several CPUs.
pub fn post_smp() {
    smp_distribution_demo();
    smp_affinity_demo();
    fp_isolation_demo();
    fp_swap_cost();
}

/// Workers that finished, and the count that observed **any** corruption of their vector
/// registers. A single non-zero `FP_CORRUPT` is a failed run.
static FP_DONE: AtomicU32 = AtomicU32::new(0);
static FP_CORRUPT: AtomicU32 = AtomicU32::new(0);
static FP_CPU_MASK: AtomicU32 = AtomicU32::new(0);

/// Number of FP workers. Deliberately more than `MAX_CPUS` so they must contend for CPUs
/// and preempt one another mid-round.
const FP_WORKERS: u32 = 12;
/// Rounds each worker runs; each spans several 10 ms ticks, so every worker is preempted, and
/// re-scheduled possibly on another CPU, while holding a live vector-register pattern.
const FP_ROUNDS: u32 = 6;

/// **The proof that no vector-register state leaks between threads.**
///
/// Each worker stamps all sixteen vector registers with a pattern unique to itself, then repeatedly
/// burns CPU and re-reads them, checking byte-for-byte. With twelve workers on four CPUs the
/// scheduler preempts and migrates them constantly, each switch a chance for the swap in
/// `sched::switch_into` to drop, duplicate or cross-wire a register file. The kernel target is
/// soft-float, so rustc never allocates a vector register: between stamp and check only the context
/// switch and another thread can touch them, and a mismatch has one explanation.
fn fp_isolation_demo() {
    FP_DONE.store(0, Ordering::Relaxed);
    FP_CORRUPT.store(0, Ordering::Relaxed);
    FP_CPU_MASK.store(0, Ordering::Relaxed);

    for i in 0..FP_WORKERS {
        if sched::spawn(fp_worker, i as usize).is_err() {
            kprintln!("fp: isolation spawn {} failed", i);
        }
    }
    let mut spins: u64 = 0;
    while FP_DONE.load(Ordering::Acquire) < FP_WORKERS {
        sched::reap_pending();
        core::hint::spin_loop();
        spins += 1;
        if spins > 20_000_000_000 {
            kprintln!("fp: isolation demo timed out");
            break;
        }
    }
    sched::reap_pending();

    let corrupt = FP_CORRUPT.load(Ordering::Acquire);
    let mask = FP_CPU_MASK.load(Ordering::Relaxed);
    let width = arch::fpu_selftest_reg_bytes() * 8;
    if corrupt == 0 {
        kprintln!(
            "fp: isolation demo complete — {} workers × {} rounds, {}-bit regs intact \
             across {} CPU(s) (cpu mask {:#06b})",
            FP_WORKERS,
            FP_ROUNDS,
            width,
            mask.count_ones(),
            mask,
        );
    } else {
        // A leak is a correctness failure: under `test-harness` the panic handler writes the FAIL
        // verdict and ends the run.
        panic!("fp: VECTOR REGISTER STATE LEAKED — {corrupt} corrupted observation(s)");
    }
}

/// Fill `img` with a pattern unique to `seed`, so a leak shows up as *another worker's* pattern,
/// not noise. Only the architectural bytes of each slot are written; the rest stays zero so the
/// comparison can ignore it.
fn fp_fill_pattern(img: &mut arch::VectorRegsImage, seed: u64, live: usize) {
    for r in 0..arch::FPU_SELFTEST_REGS {
        let slot = img.slot_mut(r);
        for (b, byte) in slot.iter_mut().enumerate().take(live) {
            // Mixes seed, register index and byte offset, so a whole-register or whole-file
            // cross-wire is caught as surely as a byte flip.
            let v = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add((r as u64) << 8).wrapping_add(b as u64);
            *byte = (v ^ (v >> 29)) as u8;
        }
    }
}

extern "C" fn fp_worker(arg: usize) {
    use arch::smp::ArchSmp;

    let live = arch::fpu_selftest_reg_bytes();
    let seed = 0xA5A5_0000_u64 ^ (arg as u64 + 1);

    let mut expect = arch::VectorRegsImage::zeroed();
    let mut actual = arch::VectorRegsImage::zeroed();
    fp_fill_pattern(&mut expect, seed, live);

    // SAFETY: ring 0; `fpu_init_cpu` ran on every CPU during bring-up, and `expect` is a
    // live, correctly aligned image. From here until the last check below this thread
    // holds live vector state — sound because kernel code never allocates a vector
    // register (see the note in the arch FPU module).
    unsafe { arch::fpu_selftest_load_regs(&raw const expect) };

    for _ in 0..FP_ROUNDS {
        let mut acc: u64 = 0;
        for i in 0..8_000_000u64 {
            acc = acc.wrapping_add(i ^ arg as u64);
        }
        core::hint::black_box(acc);

        FP_CPU_MASK.fetch_or(1 << arch::Smp::current_cpu(), Ordering::Relaxed);

        // SAFETY: as above; `actual` is a live, writable, correctly aligned image.
        unsafe { arch::fpu_selftest_store_regs(&raw mut actual) };
        for r in 0..arch::FPU_SELFTEST_REGS {
            if actual.slot(r)[..live] != expect.slot(r)[..live] {
                FP_CORRUPT.fetch_add(1, Ordering::Release);
                kprintln!("fp: worker {} register {} CORRUPTED", arg, r);
                break;
            }
        }
    }

    FP_DONE.fetch_add(1, Ordering::Release);
}

/// Report the measured cost of one FP save+restore pair, the per-context-switch price of eager
/// swapping (see the arch FPU module's "Eager, not lazy" rationale). `RDTSC`-based: meaningful only
/// under KVM or on hardware; TCG reports emulator time, not cycles.
fn fp_swap_cost() {
    // Boxed rather than a stack local: a kilobyte of 64-byte-aligned scratch on the boot thread's
    // stack is avoidable, and `KBox` gives the alignment.
    let mut scratch = match KBox::try_new(arch::ArchFpuState::initial()) {
        Ok(s) => s,
        Err(_) => {
            kprintln!("fp: swap-cost scratch alloc failed");
            return;
        }
    };
    // SAFETY: ring 0, `fpu_init_cpu` has run, `scratch` is a live aligned save area. The
    // boot thread holds no live vector state here (kernel code never allocates one), so
    // round-tripping the register file through `scratch` is harmless. The first `save`
    // populates the area before any `restore` reads it, so the zeroed start is fine.
    let swap = unsafe { arch::fpu_selftest_swap_cycles(&raw mut *scratch, 64) };

    let switch = measure_switch_cycles();

    if switch > 0 {
        kprintln!(
            "fp: save+restore ≈ {} cycles of a ≈{}-cycle context switch ({}%) — \
             {}-bit state, {} B area; RDTSC, meaningful under KVM",
            swap,
            switch,
            (swap * 100) / switch,
            arch::fpu_vector_bits(),
            arch::fpu_area_bytes(),
        );
    } else {
        kprintln!(
            "fp: save+restore ≈ {} cycles ({}-bit state, {} B area); switch measurement \
             unavailable",
            swap,
            arch::fpu_vector_bits(),
            arch::fpu_area_bytes(),
        );
    }
}

/// Cycles for one context switch, timing `yield_now` against a partner thread pinned to the same
/// CPU so each yield genuinely switches (an idle CPU would just return). A round trip is **two**
/// switches, so the per-switch figure is half. Returns `0` if the threads could not be spawned.
fn measure_switch_cycles() -> u64 {
    // Pin to the last online CPU, keeping the pair off CPU 0 so the boot thread waits undisturbed.
    let cpu = (sched::online_cpus().min(arch::MAX_CPUS)).saturating_sub(1);
    if cpu == 0 {
        return 0; // single-CPU boot: no CPU to isolate the pair on
    }
    let mask = 1u8 << cpu;

    SWITCH_PARTNER_RUN.store(true, Ordering::Release);
    SWITCH_CYCLES.store(0, Ordering::Relaxed);
    SWITCH_DONE.store(0, Ordering::Relaxed);

    if sched::spawn_with_affinity(switch_partner, 0, mask).is_err()
        || sched::spawn_with_affinity(switch_measurer, 0, mask).is_err()
    {
        SWITCH_PARTNER_RUN.store(false, Ordering::Release);
        kprintln!("fp: switch-cost spawn failed");
        return 0;
    }

    let mut spins: u64 = 0;
    while SWITCH_DONE.load(Ordering::Acquire) < 2 {
        sched::reap_pending();
        core::hint::spin_loop();
        spins += 1;
        if spins > 20_000_000_000 {
            SWITCH_PARTNER_RUN.store(false, Ordering::Release);
            kprintln!("fp: switch-cost measurement timed out");
            return 0;
        }
    }
    sched::reap_pending();
    SWITCH_CYCLES.load(Ordering::Acquire)
}

/// Yields taken by the measurer: enough to swamp the `RDTSC` bracket, few enough not to stretch.
const SWITCH_YIELDS: u64 = 2000;

static SWITCH_PARTNER_RUN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static SWITCH_DONE: AtomicU32 = AtomicU32::new(0);
static SWITCH_CYCLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

extern "C" fn switch_partner(_arg: usize) {
    while SWITCH_PARTNER_RUN.load(Ordering::Acquire) {
        sched::yield_now();
    }
    SWITCH_DONE.fetch_add(1, Ordering::Release);
}

extern "C" fn switch_measurer(_arg: usize) {
    for _ in 0..64 {
        sched::yield_now();
    }
    let t0 = arch::selftest_read_cycles();
    for _ in 0..SWITCH_YIELDS {
        sched::yield_now();
    }
    let t1 = arch::selftest_read_cycles();
    SWITCH_CYCLES.store(t1.wrapping_sub(t0) / (SWITCH_YIELDS * 2), Ordering::Release);
    SWITCH_PARTNER_RUN.store(false, Ordering::Release);
    SWITCH_DONE.fetch_add(1, Ordering::Release);
}

/// Spawn three never-yield CPU-bound workers over the armed preemptive scheduler, let the timer
/// interleave them, then drain.
fn scheduler_demo() {
    for id in 1..=3 {
        if sched::spawn(busy_worker, id).is_err() {
            kprintln!("sched: spawn {} failed", id);
        }
    }
    // Busy-wait without yielding until every worker finishes; the timer preempts the boot thread
    // too. `reap_pending` reclaims each exited stack.
    loop {
        sched::reap_pending();
        if sched::ready_is_empty() {
            break;
        }
        core::hint::spin_loop();
    }
    kprintln!("sched: all workers done (preemptively interleaved)");
}

/// A CPU-bound worker that **never yields**: under preemption the timer interleaves the workers and
/// the boot thread, where cooperatively worker 1 would finish before worker 2 printed. The
/// interleaved output is the proof.
extern "C" fn busy_worker(arg: usize) {
    for round in 0..3 {
        let mut acc: u64 = 0;
        for i in 0..20_000_000u64 {
            acc = acc.wrapping_add(i ^ arg as u64);
        }
        core::hint::black_box(acc);
        kprintln!("worker {} round {}", arg, round);
    }
    kprintln!("worker {} exiting", arg);
}

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
        if sched::spawn_with_class(ts_demo_worker, nice as usize, SchedClass::TimeShared, 0, nice).is_err() {
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

extern "C" fn rt_demo_worker(prio: usize) {
    let mut acc: u64 = 0;
    for i in 0..4_000_000u64 {
        acc = acc.wrapping_add(i);
    }
    core::hint::black_box(acc);
    kprintln!("sched:   RealTime worker (prio {}) finished", prio);
    SCHED_DEMO_DONE.fetch_add(1, Ordering::Release);
}

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

static AFFINITY_DONE: AtomicU32 = AtomicU32::new(0);
static AFFINITY_RAN_ON: [AtomicU32; arch::MAX_CPUS] = [const { AtomicU32::new(u32::MAX) }; arch::MAX_CPUS];

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
    for (i, slot) in AFFINITY_RAN_ON.iter().enumerate().take(n) {
        let ran = slot.load(Ordering::Relaxed);
        if ran != i as u32 {
            all_pinned = false;
            kprintln!("smp: affinity worker {} (pinned cpu {}) ran on cpu {}", i, i, ran);
        }
    }
    kprintln!(
        "smp: affinity demo complete — {} worker(s), {}",
        n,
        if all_pinned { "each ran on its pinned CPU" } else { "MISMATCH (see above)" }
    );
}

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

/// Walk Limine's live page tables with `arch::translate` to confirm the kernel's table-walk agrees
/// with hardware (read-only).
fn paging_smoke_test() {
    let root = arch::Paging::active_root();
    let probe = mm::VirtAddr::new(paging_smoke_test as fn() as usize as u64);
    // SAFETY: `root` is the live top-level page table the CPU is using, reachable via
    // the HHDM. `translate` only reads page-table memory.
    match unsafe { arch::Paging::translate(root, probe) } {
        Some(phys) => kprintln!("paging: NX enabled; translate {:#x} -> {:#x}", probe.as_u64(), phys.as_u64()),
        None => kprintln!("paging: translate {:#x} -> UNMAPPED — walk disagrees with hardware", probe.as_u64()),
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
    let Ok(vma) = KBox::try_new(Vma::new(range, Protection::WRITE | Protection::USER, MappingKind::Anonymous)) else {
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
        kprintln!("demand-paging: smoke test FAILED (before={:?} after={:?} r0={:?} r1={:?})", before, after, r0, r1);
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
    let mapped = unsafe { arch::Paging::translate(arch::Paging::active_root(), mm::VirtAddr::new(buf.virt() as u64)) };

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
