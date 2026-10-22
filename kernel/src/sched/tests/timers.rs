use super::*;

#[test]
fn heap_pops_in_deadline_order() {
    init_global_heap();
    let mut h: KVec<deadline::Entry> = KVec::new();
    h.try_reserve(deadline::HEAP_RESERVE).unwrap();
    for &d in &[50u64, 10, 30, 20, 40] {
        deadline::push(&mut h, entry(d, d as usize)).unwrap();
    }
    let mut got = [0u64; 5];
    for slot in got.iter_mut() {
        *slot = deadline::pop_min(&mut h).unwrap().deadline_ns;
    }
    assert_eq!(got, [10, 20, 30, 40, 50]);
    assert!(deadline::pop_min(&mut h).is_none());
    assert!(deadline::peek(&h).is_none());
}

#[test]
fn heap_peek_is_min_and_remove_targets() {
    init_global_heap();
    let mut h: KVec<deadline::Entry> = KVec::new();
    h.try_reserve(deadline::HEAP_RESERVE).unwrap();
    deadline::push(&mut h, entry(30, 0xC)).unwrap();
    deadline::push(&mut h, entry(10, 0xA)).unwrap();
    deadline::push(&mut h, entry(20, 0xB)).unwrap();
    assert_eq!(deadline::peek(&h).unwrap().target, 0xA);
    assert!(deadline::remove(&mut h, 0xB, deadline::DeadlineKind::Timer));
    assert!(!deadline::remove(&mut h, 0xB, deadline::DeadlineKind::Timer)); // gone
    assert!(!deadline::remove(&mut h, 0xA, deadline::DeadlineKind::Thread)); // wrong kind
    assert_eq!(deadline::pop_min(&mut h).unwrap().target, 0xA);
    assert_eq!(deadline::pop_min(&mut h).unwrap().target, 0xC);
    assert!(deadline::pop_min(&mut h).is_none());
}

#[test]
fn heap_remove_distinguishes_kind_for_same_target() {
    init_global_heap();
    let mut h: KVec<deadline::Entry> = KVec::new();
    h.try_reserve(deadline::HEAP_RESERVE).unwrap();
    // The same `target` can hold entries of different kinds, each removable by `(target, kind)`.
    deadline::push(
        &mut h,
        deadline::Entry { deadline_ns: 10, target: 0xA, kind: deadline::DeadlineKind::Thread, channel: 0 },
    )
    .unwrap();
    deadline::push(
        &mut h,
        deadline::Entry { deadline_ns: 20, target: 0xA, kind: deadline::DeadlineKind::PendingSend, channel: 0xBEEF },
    )
    .unwrap();
    assert!(!deadline::remove(&mut h, 0xA, deadline::DeadlineKind::Timer));
    assert!(deadline::remove(&mut h, 0xA, deadline::DeadlineKind::Thread));
    let e = deadline::peek(&h).unwrap();
    assert_eq!(e.kind, deadline::DeadlineKind::PendingSend);
    assert_eq!(e.channel, 0xBEEF);
    assert!(deadline::remove(&mut h, 0xA, deadline::DeadlineKind::PendingSend));
    assert!(deadline::peek(&h).is_none());
}

#[test]
fn heap_push_refuses_over_reserve() {
    init_global_heap();
    let mut h: KVec<deadline::Entry> = KVec::new();
    h.try_reserve(2).unwrap();
    // Fill to the actual capacity (try_reserve may round up), then the next
    // push must refuse rather than grow under the (eventual) lock.
    let cap = h.capacity();
    for i in 0..cap {
        assert!(deadline::push(&mut h, entry(i as u64 + 1, i + 1)).is_ok());
    }
    assert!(deadline::push(&mut h, entry(9999, 9999)).is_err());
    assert_eq!(h.len(), cap);
}

#[test]
fn removing_an_entry_whose_replacement_is_smaller_than_its_parent_keeps_the_heap_ordered() {
    init_global_heap();
    let mut h: KVec<deadline::Entry> = KVec::new();
    h.try_reserve(deadline::HEAP_RESERVE).unwrap();
    for (i, d) in [1u64, 10, 5, 11, 12, 6, 7].into_iter().enumerate() {
        deadline::push(&mut h, entry(d, i)).unwrap();
    }
    // Removing 11 moves 7 under 10, which it must rise above.
    assert!(deadline::remove(&mut h, 3, deadline::DeadlineKind::Timer));
    for i in 1..h.len() {
        assert!(h[(i - 1) / 2].deadline_ns <= h[i].deadline_ns, "{:?}", h.iter().map(|e| e.deadline_ns));
    }
}

// --- timers on the deadline heap ----------------------------------------------

fn heap_with(timer: *mut (), deadline_ns: u64, interval_ns: u64) -> SchedState {
    let mut st = test_state();
    st.deadlines.try_reserve(deadline::HEAP_RESERVE).unwrap();
    deadline::push(&mut st.deadlines, entry(deadline_ns, timer as usize)).unwrap();
    // SAFETY: the caller pins `timer`; single-threaded test.
    unsafe {
        Timer::set_armed(timer, deadline_ns, interval_ns);
        Timer::set_in_heap(timer, true);
    }
    st
}

#[test]
fn a_timer_fires_at_its_deadline_and_not_before() {
    init_global_heap();
    let t = new_timer();
    let mut st = heap_with(t.as_ptr(), 1_000, 0);
    fire_expired_deadlines(&mut st, 999);
    assert_eq!(st.deadlines.len(), 1);
    fire_expired_deadlines(&mut st, 1_000);
    assert!(st.deadlines.is_empty());
    // SAFETY: pinned by `t`; single-threaded.
    assert!(!unsafe { Timer::in_heap(t.as_ptr()) });
}

#[test]
fn a_one_shot_timer_fired_with_no_waiter_still_reads_signaled() {
    init_global_heap();
    let t = new_timer();
    let mut st = heap_with(t.as_ptr(), 1_000, 0);
    fire_expired_deadlines(&mut st, 1_000);
    // SAFETY: pinned by `t`; single-threaded.
    assert!(unsafe { Timer::already_signaled(t.as_ptr(), 1_500) });
    // A waiter on another CPU read the clock before this tick, and took `SCHED` after it.
    // SAFETY: as above.
    assert!(unsafe { Timer::already_signaled(t.as_ptr(), 999) }, "a clock read before the fire");
}

#[test]
fn a_periodic_timer_rearms_an_interval_after_the_tick_that_fired_it() {
    init_global_heap();
    let t = new_timer();
    let mut st = heap_with(t.as_ptr(), 1_000, 250);
    fire_expired_deadlines(&mut st, 1_100);
    assert_eq!(deadline::peek(&st.deadlines).map(|e| e.deadline_ns), Some(1_350));
    // SAFETY: pinned by `t`; single-threaded.
    unsafe {
        assert_eq!(Timer::deadline(t.as_ptr()), 1_350);
        assert!(Timer::in_heap(t.as_ptr()));
    }
}

#[test]
fn a_periodic_timer_whose_next_fire_would_wrap_waits_for_ever_instead() {
    init_global_heap();
    let t = new_timer();
    let mut st = heap_with(t.as_ptr(), 1_000, u64::MAX - 10);
    fire_expired_deadlines(&mut st, 1_000);
    assert_eq!(deadline::peek(&st.deadlines).map(|e| e.deadline_ns), Some(u64::MAX));
}

/// `timer_arm` and `Timer::drop` take the global `SCHED`, whose heap only `init` reserves.
fn reserve_global_deadlines() {
    let mut h: KVec<deadline::Entry> = KVec::new();
    h.try_reserve(deadline::HEAP_RESERVE).unwrap();
    let mut g = SCHED.lock();
    if g.deadlines.capacity() == 0 {
        core::mem::swap(&mut g.deadlines, &mut h);
    }
    drop(g);
}

fn global_heap_names(timer: *mut ()) -> bool {
    SCHED.lock().deadlines.iter().any(|e| e.target == timer as usize && e.kind == deadline::DeadlineKind::Timer)
}

/// The one test that arms timers on the global heap, so no other can reuse a freed timer's
/// address in between.
#[test]
fn a_timer_destroyed_while_armed_leaves_nothing_in_the_heap_naming_it() {
    init_global_heap();
    test_probe::reset();
    reserve_global_deadlines();
    for interval in [0, 1_000] {
        let t = new_timer();
        let p = t.as_ptr();
        // SAFETY: pinned by `t`.
        unsafe { timer_arm(p, 1 << 40, interval) }.unwrap();
        assert!(global_heap_names(p));
        drop(t);
        assert!(!global_heap_names(p), "interval {interval}: the heap names a freed timer");
    }
    // A periodic timer the tick has fired is in the heap again.
    let t = new_timer();
    let p = t.as_ptr();
    // SAFETY: pinned by `t`.
    unsafe { timer_arm(p, 1_000, 500) }.unwrap();
    fire_expired_deadlines(&mut SCHED.lock(), 1_000);
    assert!(global_heap_names(p));
    drop(t);
    assert!(!global_heap_names(p), "the heap names a freed timer the tick re-armed");
    assert_eq!(test_probe::timer_destroys(), 3);

    let t = new_timer();
    // SAFETY: pinned by `t`.
    unsafe {
        timer_arm(t.as_ptr(), 1 << 40, 0).unwrap();
        timer_arm(t.as_ptr(), 0, 0).unwrap();
    }
    assert!(!global_heap_names(t.as_ptr()), "a disarmed timer stays in the heap");
}
