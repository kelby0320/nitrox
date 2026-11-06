use super::*;

fn po() -> ObjectRef {
    ObjectRef::new(PendingOperation::try_new().unwrap())
}

/// A thread made this CPU's current in `st`, as a switch finds it.
fn running(st: &mut SchedState, tid: u32) -> *mut () {
    let r = inert_ref(tid);
    let obj = r.as_ptr();
    *st.cur_slot() = Some(r);
    obj
}

/// **Each `Park` puts the outgoing thread on its own list, in the state that list means**, and
/// out of `current`.
#[test]
fn each_park_puts_the_thread_on_its_own_list_in_its_state() {
    init_global_heap();
    let cases = [
        (Park::Ready, ThreadState::Ready),
        (Park::Blocked, ThreadState::Blocked),
        (Park::Suspended, ThreadState::Suspended),
        (Park::Exited, ThreadState::Exited),
        (Park::Reaper, ThreadState::Blocked),
    ];
    for (i, (to, state)) in cases.into_iter().enumerate() {
        let mut st = reserved_state();
        let obj = running(&mut st, 1);
        let prev = take_current(&mut st);
        park(&mut st, prev, to);
        let holds = |l: &Reserved<ObjectRef>| l.iter().any(|r| r.as_ptr() == obj);
        let reaper = st.reaper.as_ref().is_some_and(|r| r.as_ptr() == obj);
        let on = [holds(&st.ready[0]), holds(&st.blocked), holds(&st.suspended), holds(&st.reap[0]), reaper];
        assert_eq!(on, core::array::from_fn(|list| list == i), "{to:?}: the lists holding it");
        // SAFETY: pinned by the list it is on; single-threaded test.
        assert_eq!(unsafe { Thread::state_now(obj) }, state, "{to:?}");
        assert!(st.cur_ref().is_none());
    }
}

/// The idle thread yields to its own slot, never a run queue, where a pick would run it as work.
#[test]
fn the_idle_thread_yields_to_its_own_slot() {
    init_global_heap();
    let mut st = reserved_state();
    let obj = running(&mut st, IDLE_TID);
    st.set_idle_addr(obj as usize);
    let prev = take_current(&mut st);
    park(&mut st, prev, Park::Ready);
    assert!(st.ready[0].is_empty());
    assert_eq!(st.idle_slot().as_ref().map(ObjectRef::as_ptr), Some(obj));
}

/// **What a hold defers drops once `SCHED` is released**: under it, the drop assertion would stop
/// the test.
#[test]
fn a_bag_drops_what_its_hold_deferred_once_it_is_released() {
    init_global_heap();
    test_probe::reset();
    let (a, b) = (po(), po());
    {
        let mut bag = Deferred::<2>::new();
        let mut g = bag.lock();
        g.defer(a);
        g.defer(b);
        assert_eq!(test_probe::pending_op_destroys(), 0);
    }
    assert_eq!(test_probe::pending_op_destroys(), 2);
}

#[test]
#[should_panic(expected = "deferred drops: reserve exhausted")]
fn a_bag_deferring_past_its_size_ends_the_machine() {
    init_global_heap();
    let (a, b) = (po(), po());
    let mut bag = Deferred::<1>::new();
    let mut g = bag.lock();
    g.defer(a);
    g.defer(b);
}

/// **An interrupt's bag frees nothing**: what it deferred waits in `deferred_drops` for a thread.
#[test]
fn an_interrupts_bag_moves_what_it_deferred_to_deferred_drops() {
    init_global_heap();
    test_probe::reset();
    let fresh = Reserved::with_capacity(DEFERRED_DROP_RESERVE).unwrap();
    let spare = {
        let mut g = SCHED.lock();
        if g.deferred_drops.capacity() == 0 { core::mem::replace(&mut g.deferred_drops, fresh) } else { fresh }
    };
    drop(spare);
    let (a, b) = (po(), po());
    let mine = [a.as_ptr(), b.as_ptr()];
    {
        let mut bag = Deferred::<2>::in_interrupt();
        let mut g = bag.lock();
        g.defer(a);
        g.defer(b);
    }
    assert_eq!(test_probe::pending_op_destroys(), 0, "nothing freed");
    let mut parked = std::vec::Vec::new();
    {
        let mut g = SCHED.lock();
        while let Some(i) = g.deferred_drops.iter().position(|r| mine.contains(&r.as_ptr())) {
            parked.push(g.deferred_drops.remove(i));
        }
    }
    assert_eq!(parked.len(), 2, "both wait in deferred_drops");
}

/// Any reference, not only the last: one dropped under `SCHED` is the last whenever another CPU
/// lets go first.
#[test]
#[should_panic(expected = "PendingOperation reference dropped under SCHED")]
fn a_reference_dropped_under_sched_stops_its_test() {
    init_global_heap();
    let po = po();
    let _g = SCHED.lock();
    drop(po.clone());
}
