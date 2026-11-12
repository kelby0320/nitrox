use super::*;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{IrqSpinLock, IrqsOff};

/// A pin an outer lock's early release unmasked under is refused: the thread could move.
#[test]
#[should_panic(expected = "a CPU pin with interrupts on and preemption enabled")]
fn a_pin_an_outer_lock_unmasked_is_refused() {
    crate::libkern::irqs::force(true);
    let (outer, inner) = (IrqSpinLock::new(LockRank::Leaf, ()), IrqSpinLock::new(LockRank::Leaf, ()));
    let held = outer.lock();
    let pinned = inner.lock();
    drop(held);
    CpuIndex::current(&pinned.pin());
}

/// …and accepted with preemption off, which keeps the thread on its CPU with interrupts on.
#[test]
fn the_same_pin_with_preemption_off_is_accepted() {
    crate::libkern::irqs::force(true);
    let off = crate::sched::preempt_disable();
    let (outer, inner) = (IrqSpinLock::new(LockRank::Leaf, ()), IrqSpinLock::new(LockRank::Leaf, ()));
    let held = outer.lock();
    let pinned = inner.lock();
    drop(held);
    assert_eq!(CpuIndex::current(&pinned.pin()), CpuIndex::BSP);
    drop(pinned);
    drop(off);
}

#[test]
fn a_pin_under_a_mask_is_accepted() {
    crate::libkern::irqs::force(true);
    assert_eq!(CpuIndex::current(&IrqsOff::mask().pin()), CpuIndex::BSP);
}

#[test]
fn each_cpu_has_a_slot_of_its_own() {
    static SLOTS: PerCpu<u64> = PerCpu::new([0; MAX_CPUS]);
    let last = CpuIndex::new(MAX_CPUS - 1).unwrap();
    // SAFETY: this test's own static, which no other thread reaches.
    unsafe {
        SLOTS.slot(last).write(7);
        assert_eq!((SLOTS.slot(CpuIndex::BSP).read(), SLOTS.slot(last).read()), (0, 7));
    }
}
