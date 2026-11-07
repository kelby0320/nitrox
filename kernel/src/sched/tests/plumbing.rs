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

/// Give the global `deferred_drops` the reserve `init` gives it, which no host test runs.
fn reserve_deferred_drops() {
    let fresh = Reserved::with_capacity(DEFERRED_DROP_RESERVE).unwrap();
    let spare = {
        let mut g = SCHED.lock();
        if g.deferred_drops.capacity() == 0 { core::mem::replace(&mut g.deferred_drops, fresh) } else { fresh }
    };
    drop(spare);
}

fn take_parked(mine: &[*mut ()]) -> std::vec::Vec<ObjectRef> {
    let mut parked = std::vec::Vec::new();
    let mut g = SCHED.lock();
    while let Some(i) = g.deferred_drops.iter().position(|r| mine.contains(&r.as_ptr())) {
        parked.push(g.deferred_drops.remove(i));
    }
    parked
}

/// **An interrupt's bag frees nothing**: what it deferred waits in `deferred_drops` for a thread.
#[test]
fn an_interrupts_bag_moves_what_it_deferred_to_deferred_drops() {
    init_global_heap();
    test_probe::reset();
    reserve_deferred_drops();
    let (a, b) = (po(), po());
    let mine = [a.as_ptr(), b.as_ptr()];
    {
        let mut bag = Deferred::<2>::in_interrupt();
        let mut g = bag.lock();
        g.defer(a);
        g.defer(b);
    }
    assert_eq!(test_probe::pending_op_destroys(), 0, "nothing freed");
    assert_eq!(take_parked(&mine).len(), 2, "both wait in deferred_drops");
}

/// **The tick's seed wake frees nothing**: each waiter it completes waits in `deferred_drops`.
#[test]
fn the_seed_wake_completes_its_waiters_and_frees_none() {
    init_global_heap();
    reserve_deferred_drops();
    let (a, b) = (po(), po());
    let mine = [a.as_ptr(), b.as_ptr()];
    wake_seed_waiters(|out| {
        out[0] = Some(a.clone());
        out[1] = Some(b.clone());
        2
    });
    // SAFETY: each PO is pinned by the test.
    assert!(mine.iter().all(|&p| unsafe { pending_op_is_signaled(p) }), "completed");
    assert_eq!(take_parked(&mine).len(), 2, "and waiting in deferred_drops");
}

/// **A thread no queue takes drops once `SCHED` is released**, its last reference.
#[test]
fn a_started_thread_no_queue_takes_drops_after_the_hold() {
    use crate::object::thread::test_support::{user_process_ref, user_thread_no_stack};
    init_global_heap();
    test_probe::reset();
    let (process, _root) = user_process_ref(9);
    let thread = into_objref(user_thread_no_stack(1, process, 0x40_0000, 0x7fff_f000));
    // The global run queues are reserved only by `init`, which no host test runs.
    assert!(start_user(thread).is_err());
    assert_eq!(test_probe::thread_destroys(), 1);
}

/// A thread blocked on each of `objs`, registered as `wait_on` registers it.
fn waiting_on(st: &mut SchedState, objs: &[*mut ()]) -> *mut () {
    let th = inert_ref(1);
    let t = th.as_ptr();
    let addrs: std::vec::Vec<usize> = objs.iter().map(|&o| o as usize).collect();
    // SAFETY: each object is pinned by its test; single-threaded test.
    unsafe {
        Thread::wait_register(t, &addrs, false);
        for &o in objs {
            obj_add_waiter(o, t).unwrap();
        }
        Thread::set_state(t, ThreadState::Blocked);
    }
    assert!(st.blocked.push(th).is_ok());
    t
}

fn queued(st: &SchedState, t: *mut ()) -> usize {
    st.ready.iter().map(|q| q.iter().filter(|r| r.as_ptr() == t).count()).sum()
}

/// **Every kind of waitable wakes through the one loop**, which marks its slot in the waiter's
/// wait and makes the thread runnable.
#[test]
fn every_kind_of_waitable_marks_its_slot_and_wakes_its_waiter() {
    init_global_heap();
    super::online_set::force(0b1);
    let (ep, peer) = IpcChannel::try_new_pair(1).unwrap();
    let (ep, _peer) = (ObjectRef::new(ep), ObjectRef::new(peer));
    let chan = ObjectRef::new(NotificationChannel::try_new().unwrap());
    let irq = ObjectRef::new(InterruptObject::try_new().unwrap());
    let (po, timer) = (po(), new_timer());
    let fire = |g: &mut SchedState, timer: *mut ()| {
        let target = deadline::Target::Timer(timer as usize);
        deadline::push(&mut g.deadlines, deadline::Entry { deadline_ns: 1, target }).unwrap();
        fire_expired_deadlines(g, 2);
    };
    type Signal = fn(&mut SchedState, *mut ());
    let signals: [(&ObjectRef, Signal); 5] = [
        (&chan, |g, o| {
            signal_channel(g, o);
        }),
        (&ep, signal_ipc_endpoint),
        (&po, |g, o| signal_pending_op(g, o, 0)),
        (&irq, signal_interrupt_in),
        (&timer, fire),
    ];
    for (obj, signal) in signals {
        let mut st = reserved_state();
        let t = waiting_on(&mut st, &[obj.as_ptr()]);
        signal(&mut st, obj.as_ptr());
        let kind = obj.object_type();
        // SAFETY: pinned by its run queue; single-threaded test.
        assert!(unsafe { Thread::wait_signaled(t) }[0], "{kind:?}: its slot marked");
        assert_eq!(queued(&st, t), 1, "{kind:?}: its waiter runnable");
    }
}

/// **A waiter is woken once**: the second of two objects it waits on to signal marks its slot and
/// wakes nothing.
#[test]
fn a_thread_waiting_on_two_objects_is_woken_by_the_first_alone() {
    init_global_heap();
    super::online_set::force(0b1);
    let mut st = reserved_state();
    let channel = || ObjectRef::new(NotificationChannel::try_new().unwrap());
    let (a, b) = (channel(), channel());
    let t = waiting_on(&mut st, &[a.as_ptr(), b.as_ptr()]);
    assert!(signal_channel(&mut st, a.as_ptr()), "the first wakes it");
    assert!(!signal_channel(&mut st, b.as_ptr()), "the second wakes nothing");
    // SAFETY: pinned by its run queue; single-threaded test.
    assert_eq!(unsafe { Thread::wait_signaled(t) }[..2], [true, true]);
    assert_eq!(queued(&st, t), 1);
}

fn ending(st: &mut SchedState, blocked: bool) {
    let th = inert_user_ref(1, 7);
    // SAFETY: pinned by `th`; single-threaded test. A blocked thread is in a wait, to be claimed.
    unsafe {
        if blocked {
            Thread::wait_register(th.as_ptr(), &[], false);
        }
        Thread::set_state(th.as_ptr(), if blocked { ThreadState::Blocked } else { ThreadState::Suspended });
    }
    let list = if blocked { &mut st.blocked } else { &mut st.suspended };
    *list = thread_list().unwrap();
    assert!(list.push(th).is_ok());
    end_siblings(st, 7);
}

/// A placement that ends the machine says which it was, a wake's or a resume's.
#[test]
#[should_panic(expected = "wake placement")]
fn a_wake_no_queue_takes_ends_the_machine_saying_so() {
    init_global_heap();
    super::online_set::force(0b1);
    ending(&mut test_state(), true);
}

#[test]
#[should_panic(expected = "resume placement")]
fn a_resume_no_queue_takes_ends_the_machine_saying_so() {
    init_global_heap();
    super::online_set::force(0b1);
    ending(&mut test_state(), false);
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
