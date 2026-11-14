use super::*;
use crate::{NsMutable, sys};

/// A namespace binding a new `/dev/entropy` node, as a session's does.
fn with_entropy() -> Handle<Namespace, NsMutable> {
    let ns = Handle::<Namespace, NsMutable>::create().unwrap();
    let node: Handle<Resource, ReadOnly> =
        Handle::fresh(sys::entropy_node(), Rights::READ | Rights::DUPLICATE | Rights::TRANSFER);
    ns.bind(PATH, &node).unwrap();
    ns
}

#[test]
fn a_fill_reads_in_pieces_and_closes_what_it_opened() {
    sys::reset();
    let ns = with_entropy();
    let before = sys::open_handles();
    let mut buf = [0u8; 600];
    fill(&ns, &mut buf).unwrap();
    assert!(buf[256..].iter().any(|&b| b != 0) && buf[599] != 0, "every piece filled");
    assert_eq!(sys::open_handles(), before, "no source left open");
}

#[test]
fn a_namespace_that_binds_no_generator_has_none() {
    sys::reset();
    let ns = Handle::<Namespace, NsMutable>::create().unwrap();
    assert!(fill(&ns, &mut [0u8; 8]).err().unwrap().is_not_found());
}

#[test]
fn a_try_before_the_seeding_is_refused_and_leaves_nothing_open() {
    sys::reset();
    let ns = with_entropy();
    let before = sys::open_handles();
    sys::unseeded(1);
    let mut buf = [0u8; 8];
    let source = Source::open(&ns).unwrap();
    let then = crate::time::now().unwrap();
    assert_eq!(source.try_fill(&mut buf).map_err(|e| e.code()), Err(KError::WouldBlock));
    assert!(crate::time::now().unwrap() - then < 1_000_000, "a poll, which lets no time pass");
    drop(source);
    assert_eq!(sys::open_handles(), before, "neither the source nor the read's operation left open");
    try_fill(&ns, &mut buf).unwrap();
    assert!(buf.iter().any(|&b| b != 0), "seeded, it fills");
}

/// **Every place for a waiter taken is a read to retry, not a failure**: the kernel refuses a
/// reader past `SEED_WAITERS_MAX` `WouldBlock`, and each `try_fill` before the seeding holds one.
#[test]
fn a_fill_that_finds_no_place_to_wait_reads_again() {
    sys::reset();
    let ns = with_entropy();
    let before = sys::open_handles();
    sys::unseeded(2);
    sys::seed_waiters_full(1);
    let slept = sys::sleeps();
    let mut buf = [0u8; 8];
    fill(&ns, &mut buf).unwrap();
    assert!(buf.iter().any(|&b| b != 0));
    assert!(sys::sleeps() > slept, "a pause before reading again, not a spin");
    assert_eq!(sys::open_handles(), before, "every operation and the source closed");
}

/// One source reads again, held open between reads, and a source handed over reads as one opened
/// here does.
#[test]
fn a_source_reads_again_and_a_handed_one_reads_alike() {
    sys::reset();
    let ns = with_entropy();
    let source = Source::open(&ns).unwrap();
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    source.fill(&mut a).unwrap();
    source.try_fill(&mut b).unwrap();
    assert!(a.iter().any(|&x| x != 0) && b.iter().any(|&x| x != 0));
    let node: Handle<Resource, ReadOnly> = block_on(ns.lookup(PATH, Rights::READ | Rights::DUPLICATE)).unwrap();
    let blind = node.duplicate(Rights::INSPECT).unwrap();
    let mut c = [0u8; 8];
    Source::from_handle(node).unwrap().fill(&mut c).unwrap();
    assert!(c.iter().any(|&x| x != 0));
    let refused = Source::from_handle(blind).unwrap().fill(&mut c).map_err(|e| e.code());
    assert_eq!(refused, Err(KError::NoAccess), "read from the handle it was handed, with its rights");
}

#[test]
fn an_unseeded_read_answers_nothing_and_is_read_again() {
    sys::reset();
    let ns = with_entropy();
    let before = sys::open_handles();
    sys::unseeded(1);
    let mut buf = [0u8; 8];
    fill(&ns, &mut buf).unwrap();
    assert!(buf.iter().any(|&b| b != 0));
    assert_eq!(sys::open_handles(), before, "the waited operation closed too");
}
