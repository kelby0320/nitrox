use core::cell::RefCell;

use super::*;
use crate::drivers::xhci::context::DCI_EP0;
use crate::drivers::xhci::{Awaited, Trb, code, fake, hub, install, kind, on_event};
use crate::libkern::IrqSpinLock;
use crate::libkern::lockrank::LockRank;
use crate::mm::test_support::init_global_heap;
use crate::object::Thread;
use crate::sched::{pending_op_is_signaled, with_current};

/// One record, as the controller's: a test's install and take-back.
struct Slot(RefCell<Option<Completer>>);

impl Record for &Slot {
    fn take_back(&self, po: *mut ()) -> Option<Completer> {
        self.0.borrow_mut().take_if(|c| c.is_for(po))
    }
}

/// **A record the DPC takes as its waiter's wait runs out**, completing it with `(5, 9)`.
struct Racing<'s>(&'s Slot);

impl Record for Racing<'_> {
    fn take_back(&self, _: *mut ()) -> Option<Completer> {
        let taken = self.0.0.borrow_mut().take();
        if let Some(c) = taken {
            c.complete(5, 9);
        }
        None
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

/// `armed`'s wait, polled: the host clock reads 0, so a bound of 0 is a deadline already passed.
fn polled<R: Record>(armed: Armed<R>) -> Option<(u8, u64)> {
    with_current(Ref::new(Thread::try_new(0, 0).unwrap()), || armed.wait(0))
}

#[test]
fn a_second_arm_is_refused_and_an_unwaited_one_takes_its_record_back() {
    init_global_heap();
    let po = operation();
    let at = po.as_object().as_ptr();
    let slot = Slot(RefCell::new(None));
    let first = arm(po, &slot, into(&slot)).unwrap();
    assert_eq!(arm(operation(), &slot, into(&slot)).err(), Some("armed"));
    assert!(slot.0.borrow().as_ref().is_some_and(|c| c.is_for(at)), "the first kept");
    drop(first);
    assert!(slot.0.borrow().is_none(), "taken back by its waiter, dropped unwaited");
}

/// **A wait forgotten leaks its operation**, so the `Completer` it armed completes live memory: the
/// `Armed` owns the reference that pins it. Miri's to judge.
#[test]
fn a_forgotten_wait_leaks_its_operation_for_its_completer() {
    init_global_heap();
    let slot = Slot(RefCell::new(None));
    core::mem::forget(arm(operation(), &slot, into(&slot)).unwrap());
    let completer = slot.0.borrow_mut().take().unwrap();
    completer.complete(0, 7);
}

/// **A wait that runs out takes its record back** and answers `None`: nothing will complete the
/// operation it is about to let go of.
#[test]
fn a_wait_that_runs_out_takes_its_record_back() {
    init_global_heap();
    let slot = Slot(RefCell::new(None));
    let armed = arm(operation(), &slot, into(&slot)).unwrap();
    assert_eq!(polled(armed), None);
    assert!(slot.0.borrow().is_none(), "taken back");
}

/// **A wait that runs out as the DPC takes its record waits out the completion**, and answers it.
#[test]
fn a_wait_that_runs_out_as_the_dpc_takes_its_record_waits_out_the_completion() {
    init_global_heap();
    let slot = Slot(RefCell::new(None));
    let armed = arm(operation(), Racing(&slot), into(&slot)).unwrap();
    assert_eq!(polled(armed), Some((5, 9)));
}

/// **A completion event completes the operation the controller's record awaits**: a command's
/// with the slot it names, a default endpoint's Status stage with what its Data stage left.
#[test]
fn a_completion_event_completes_the_operation_its_record_awaits() {
    init_global_heap();
    let x = fake();
    let success = (code::SUCCESS as u32) << 24;
    let cases = [
        (Awaited::Command { trb: 0x40 }, Trb([0x40, 0, success, kind::COMMAND_COMPLETION << 10 | 3 << 24]), 3),
        (
            Awaited::Transfer { slot: 3, status: 0x1020 },
            Trb([0x1020, 0, success, kind::TRANSFER_EVENT << 10 | 3 << 24 | (DCI_EP0 as u32) << 16]),
            0,
        ),
    ];
    for (awaited, event, result) in cases {
        let po = operation();
        let seen = po.clone();
        let armed = arm(po, &x, |c| install(&x.waiting, awaited, c)).unwrap();
        on_event(&x, &event);
        assert!(pending_op_is_signaled(&seen), "{awaited:?}: completed by its event");
        assert_eq!(polled(armed), Some((code::SUCCESS, result)), "{awaited:?}");
    }
}

/// **A wait takes back its own completer only**: the DPC has taken the first wait's record, and
/// the second, armed in it since, keeps its own when the first runs out.
#[test]
fn a_wait_takes_back_its_own_completer_only() {
    init_global_heap();
    let x = fake();
    let awaited = Awaited::Command { trb: 0x40 };
    assert!(install(&x.waiting, awaited, Completer::for_test(1)).is_ok());
    let first = x.waiting.lock().take().unwrap().completer;
    assert!(install(&x.waiting, awaited, Completer::for_test(2)).is_ok(), "the second armed");
    assert_eq!((&x).take_back(first.po), None, "the first's is the DPC's");
    let second = core::ptr::without_provenance_mut(2);
    assert_eq!((&x).take_back(second), Some(Completer::for_test(2)), "the second's kept for it");
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
