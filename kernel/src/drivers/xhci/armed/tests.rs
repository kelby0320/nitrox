use core::cell::RefCell;

use super::*;
use crate::drivers::xhci::{Awaited, hub, install};
use crate::libkern::IrqSpinLock;
use crate::libkern::lockrank::LockRank;
use crate::mm::test_support::init_global_heap;

/// One record, as the controller's: a test's install and take-back.
struct Slot(RefCell<Option<Completer>>);

impl Record for &Slot {
    fn take_back(&self) -> Option<Completer> {
        self.0.borrow_mut().take()
    }
}

fn into(slot: &Slot) -> impl FnOnce(Completer) -> Result<(), (Completer, &'static str)> + '_ {
    move |c| {
        let mut s = slot.0.borrow_mut();
        if s.is_some() {
            return Err((c, "armed"));
        }
        *s = Some(c);
        Ok(())
    }
}

fn operation() -> Ref<PendingOperation> {
    Ref::new(PendingOperation::try_new().unwrap())
}

#[test]
fn a_second_arm_is_refused_and_an_unwaited_one_takes_its_record_back() {
    init_global_heap();
    let (po, other) = (operation(), operation());
    let slot = Slot(RefCell::new(None));
    let first = arm(&po, &slot, into(&slot)).unwrap();
    assert_eq!(arm(&other, &slot, into(&slot)).err(), Some("armed"));
    assert!(slot.0.borrow().as_ref().is_some_and(|c| c.po == po.as_object().as_ptr()), "the first kept");
    drop(first);
    assert!(slot.0.borrow().is_none(), "taken back by its waiter, dropped unwaited");
}

#[test]
fn a_second_wait_on_the_controller_is_refused_and_the_first_kept() {
    let slot = IrqSpinLock::new(LockRank::Leaf, None);
    let awaited = Awaited::Command { trb: 0x40 };
    assert!(install(&slot, awaited, Completer::for_test(1)).is_ok());
    let refused = install(&slot, awaited, Completer::for_test(2));
    assert!(matches!(refused, Err((c, hub::Failed::Device(_))) if c == Completer::for_test(2)));
    assert!(slot.lock().as_ref().is_some_and(|w| w.completer == Completer::for_test(1)));
}

#[test]
#[should_panic(expected = "dropped unspent")]
fn a_completer_dropped_unspent_panics() {
    drop(Completer { po: core::ptr::null_mut(), lax: false });
}
