use core::cell::Cell;

use super::*;

std::thread_local! {
    static DROPPED: Cell<u32> = const { Cell::new(0) };
}

/// A value that counts its drops.
#[derive(Debug, PartialEq)]
struct Counted(u32);

impl Drop for Counted {
    fn drop(&mut self) {
        DROPPED.with(|d| d.set(d.get() + 1));
    }
}

fn dropped() -> u32 {
    DROPPED.with(Cell::get)
}

#[test]
fn a_second_park_is_refused_and_the_first_kept() {
    let mut h: Handoff<Counted> = Handoff::new();
    assert!(h.park(Counted(1)).is_ok());
    assert_eq!(h.park(Counted(2)).err().map(|c| c.0), Some(2), "handed back");
    assert_eq!(dropped(), 1, "the second alone, by the test");
    assert_eq!(h.take_parked().map(|c| c.0), Some(1), "the first kept");
}

#[test]
fn nothing_parks_while_one_is_lent_or_owed_and_what_is_owed_is_thread_contexts() {
    let mut h: Handoff<Counted> = Handoff::new();
    assert!(h.park(Counted(1)).is_ok());
    let loan = h.lend().unwrap();
    assert_eq!(loan.0, 1);
    assert!(h.lend().is_none() && !h.is_parked() && !h.is_empty());
    assert!(h.park(Counted(2)).is_err(), "lent");
    h.give_back(loan);
    assert!(h.park(Counted(3)).is_err(), "owed");
    let before = dropped();
    let [owed] = h.take_all();
    assert_eq!(dropped(), before, "nothing dropped in the hand-off");
    assert_eq!(owed.map(|c| c.0), Some(1));
    assert!(h.is_empty() && h.park(Counted(4)).is_ok());
}

#[test]
fn every_parked_value_is_lent_and_given_back_to_its_own_slot() {
    let mut h: Handoff<Counted, 3> = Handoff::new();
    assert!(h.park(Counted(1)).is_ok() && h.park(Counted(2)).is_ok());
    let [a, b, c] = h.lend_all();
    assert!(c.is_none());
    assert!(h.park(Counted(3)).is_ok(), "the free slot");
    h.give_back(b.unwrap());
    let owed = h.take_all();
    assert_eq!(owed.iter().map(|o| o.as_ref().map(|c| c.0)).collect::<std::vec::Vec<_>>(), [None, Some(2), None]);
    h.give_back(a.unwrap());
    assert_eq!(h.take_all()[0].as_ref().map(|c| c.0), Some(1));
    assert_eq!(h.take_parked().map(|c| c.0), Some(3), "parked, never lent");
}

#[test]
fn a_value_parked_with_is_made_only_where_a_slot_is_free() {
    let mut h: Handoff<Counted> = Handoff::new();
    assert!(h.park_with(|| Counted(1)));
    assert!(!h.park_with(|| unreachable!("no free slot")), "refused, nothing made");
    assert_eq!(h.take_parked().map(|c| c.0), Some(1));
}

#[test]
#[should_panic(expected = "a loan dropped")]
fn a_loan_dropped_panics() {
    let mut h: Handoff<Counted> = Handoff::new();
    assert!(h.park(Counted(1)).is_ok());
    drop(h.lend());
}

#[test]
#[should_panic(expected = "a loan given back where it was not lent")]
fn a_loan_given_back_to_another_handoff_is_refused() {
    let (mut a, mut b): (Handoff<Counted>, Handoff<Counted>) = (Handoff::new(), Handoff::new());
    assert!(a.park(Counted(1)).is_ok() && b.park(Counted(2)).is_ok());
    let (from_a, _from_b) = (a.lend().unwrap(), b.lend().unwrap());
    b.give_back(from_a);
}

/// **A lend takes the first parked slot**, past one already lent.
#[test]
fn a_lend_passes_over_a_slot_already_lent() {
    let mut h: Handoff<Counted, 2> = Handoff::new();
    assert!(h.park(Counted(1)).is_ok() && h.park(Counted(2)).is_ok());
    let (first, second) = (h.lend().unwrap(), h.lend().unwrap());
    assert_eq!((first.0, second.0), (1, 2));
    assert!(h.lend().is_none(), "none parked");
    h.give_back(first);
    h.give_back(second);
    assert_eq!(h.take_all().map(|c| c.map(|c| c.0)), [Some(1), Some(2)]);
}
