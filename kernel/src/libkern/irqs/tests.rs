use super::*;
use crate::libkern::IrqSpinLock;
use crate::libkern::lockrank::LockRank;

#[test]
#[should_panic(expected = "interrupts observed masked, and they were on")]
fn interrupts_observed_masked_while_on_are_refused() {
    force(true);
    let _ = IrqsOff::observed();
}

#[test]
fn a_mask_restores_what_was() {
    force(true);
    let off = IrqsOff::mask();
    assert!(masked());
    drop(off);
    assert!(!masked());
    force(false);
    drop(IrqsOff::mask());
    assert!(masked());
    drop(IrqsOff::observed());
    assert!(masked(), "an observation restored interrupts it never masked");
}

/// A switch refuses a live `IrqsOff`, which keeps a thread on no CPU across it.
#[test]
#[should_panic(expected = "a switch with an `IrqsOff` live")]
fn a_switch_with_a_mask_live_is_refused() {
    let lock = IrqSpinLock::new(LockRank::Leaf, ());
    let _off = IrqsOff::mask();
    assert_no_live_masks(&lock.lock().pin());
}

#[test]
fn a_switch_with_only_its_lock_held_is_accepted() {
    let lock = IrqSpinLock::new(LockRank::Leaf, ());
    drop(IrqsOff::mask());
    force(false);
    drop(IrqsOff::observed());
    assert_no_live_masks(&lock.lock().pin());
}
