use super::*;

fn po() -> ObjectRef {
    ObjectRef::new(PendingOperation::try_new().unwrap())
}

/// A thread made this CPU's current in `st`, as a switch finds it.
fn running(st: &mut SchedState, tid: u32) -> Ref<Thread> {
    let r = inert_ref(tid);
    *st.cur_slot() = Some(r.clone());
    r
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
        let th = running(&mut st, 1);
        let obj = th.as_ptr();
        let prev = take_current(&mut st);
        park(&mut st, prev, to);
        let holds = |l: &Reserved<Ref<Thread>>| l.iter().any(|r| r.as_ptr() == obj);
        let reaper = st.reaper.as_ref().is_some_and(|r| r.as_ptr() == obj);
        let on = [holds(&st.ready[0]), holds(&st.blocked), holds(&st.suspended), holds(&st.reap[0]), reaper];
        assert_eq!(on, core::array::from_fn(|list| list == i), "{to:?}: the lists holding it");
        assert_eq!(th.state(&st.tok), state, "{to:?}");
        assert!(st.cur_ref().is_none());
    }
}

/// The idle thread yields to its own slot, never a run queue, where a pick would run it as work.
#[test]
fn the_idle_thread_yields_to_its_own_slot() {
    init_global_heap();
    let mut st = reserved_state();
    let obj = running(&mut st, IDLE_TID).as_ptr();
    st.set_idle_addr(obj as usize);
    let prev = take_current(&mut st);
    park(&mut st, prev, Park::Ready);
    assert!(st.ready[0].is_empty());
    assert_eq!(st.idle_slot().as_ref().map(Ref::as_ptr), Some(obj));
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
    let (a, b) = (Ref::new(PendingOperation::try_new().unwrap()), Ref::new(PendingOperation::try_new().unwrap()));
    let mine = [a.as_object().as_ptr(), b.as_object().as_ptr()];
    wake_seed_waiters(|out| {
        out[0] = Some(a.clone());
        out[1] = Some(b.clone());
        2
    });
    assert!(pending_op_is_signaled(&a) && pending_op_is_signaled(&b), "completed");
    assert_eq!(take_parked(&mine).len(), 2, "and waiting in deferred_drops");
}

/// **A thread no queue takes drops once `SCHED` is released**, its last reference.
#[test]
fn a_started_thread_no_queue_takes_drops_after_the_hold() {
    use crate::object::thread::test_support::{user_process_ref, user_thread_no_stack};
    init_global_heap();
    test_probe::reset();
    let (process, _root) = user_process_ref(9);
    let thread = Ref::new(user_thread_no_stack(1, process, 0x40_0000, 0x7fff_f000));
    // The global run queues are reserved only by `init`, which no host test runs.
    assert!(start_user(thread).is_err());
    assert_eq!(test_probe::thread_destroys(), 1);
}

/// A thread blocked on each of `objs`, registered as `wait_on` registers it.
fn waiting_on(st: &mut SchedState, objs: &[&ObjectRef]) -> Ref<Thread> {
    let th = inert_ref(1);
    let addrs: std::vec::Vec<usize> = objs.iter().map(|o| o.as_ptr() as usize).collect();
    th.wait_register(&mut st.tok, &addrs, false);
    th.set_state(&mut st.tok, ThreadState::Blocked);
    for o in objs {
        obj_add_waiter(&mut st.tok, o, addr(&*th)).unwrap();
    }
    assert!(st.blocked.push(th.clone()).is_ok());
    th
}

fn queued(st: &SchedState, t: &Thread) -> usize {
    st.ready.iter().map(|q| q.iter().filter(|r| core::ptr::eq(&***r, t)).count()).sum()
}

/// **Every kind of waitable wakes through the one loop**, which marks its slot in the waiter's
/// wait and makes the thread runnable.
#[test]
fn every_kind_of_waitable_marks_its_slot_and_wakes_its_waiter() {
    init_global_heap();
    super::online_set::force(0b1);
    let (ep, peer) = IpcChannel::try_new_pair(1).unwrap();
    let ep = ep.erase();
    let chan = ObjectRef::new(NotificationChannel::try_new().unwrap());
    let irq = ObjectRef::new(InterruptObject::try_new().unwrap());
    let (po, timer) = (po(), new_timer());
    let fire = |g: &mut SchedState, timer: &ObjectRef| {
        let target = deadline::Target::Timer(timer.as_ptr() as usize);
        deadline::push(&mut g.deadlines, deadline::Entry { deadline_ns: 1, target }).unwrap();
        fire_expired_deadlines(g, 2);
    };
    type Signal<'a> = &'a dyn Fn(&mut SchedState, &ObjectRef);
    let signals: [(&ObjectRef, Signal<'_>); 5] = [
        (&chan, &|g, o| {
            signal_channel(g, o.downcast_ref().unwrap());
        }),
        (&ep, &|g, _| wake_peer(g, &peer)),
        (&po, &|g, o| signal_pending_op(g, o.downcast_ref().unwrap(), 0)),
        (&irq, &|g, o| signal_interrupt_in(g, o.downcast_ref().unwrap())),
        (timer.as_object(), &fire),
    ];
    for (obj, signal) in signals {
        let mut st = reserved_state();
        let t = waiting_on(&mut st, &[obj]);
        signal(&mut st, obj);
        let kind = obj.object_type();
        assert!(t.wait_signaled(&st.tok)[0], "{kind:?}: its slot marked");
        assert_eq!(queued(&st, &t), 1, "{kind:?}: its waiter runnable");
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
    let t = waiting_on(&mut st, &[&a, &b]);
    assert!(signal_channel(&mut st, a.downcast_ref().unwrap()), "the first wakes it");
    assert!(!signal_channel(&mut st, b.downcast_ref().unwrap()), "the second wakes nothing");
    assert_eq!(t.wait_signaled(&st.tok)[..2], [true, true]);
    assert_eq!(queued(&st, &t), 1);
}

fn ending(st: &mut SchedState, blocked: bool) {
    let th = inert_user_ref(1, 7);
    // A blocked thread is in a wait, to be claimed.
    if blocked {
        th.wait_register(&mut st.tok, &[], false);
    }
    th.set_state(&mut st.tok, if blocked { ThreadState::Blocked } else { ThreadState::Suspended });
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

/// **A terminate request reaches the process's own channel, and wakes what waits on it**: the
/// channel the borrowed `Process` holds. A process with none cannot be asked.
#[test]
fn a_terminate_request_reaches_the_channel_its_process_holds() {
    init_global_heap();
    let channel = Ref::new(NotificationChannel::try_new().unwrap());
    let mut process = Process::try_new(9).unwrap();
    assert!(!request_terminate(&process), "no channel to ask on");
    process.set_notification_channel(channel.clone());
    let waiter = inert_ref(1);
    let (t, on) = (addr(&*waiter), channel.as_ptr().cast::<()>());
    {
        let mut g = SCHED.lock();
        waiter.wait_register(&mut g.tok, &[on as usize], false);
        channel.add_waiter(&mut g.tok, t).unwrap();
    }
    assert!(request_terminate(&process));
    let woken = {
        let mut g = SCHED.lock();
        let marked = waiter.wait_signaled(&g.tok)[0];
        channel.remove_waiter(&mut g.tok, t);
        waiter.wait_clear(&mut g.tok);
        marked
    };
    assert!(woken, "its waiter woken");
    let heard = notif_try_recv(&channel).expect("the request");
    assert_eq!(heard.as_bytes(), Notification::terminate_requested().as_bytes());
}

/// **A kind no thread waits on is never ready, and a wait that would block on it is refused**, its
/// registration on the rest unwound: the PO beside it asserts, as it drops, that no waiter is left.
#[test]
fn a_wait_that_would_block_on_a_kind_no_thread_waits_on_is_refused() {
    init_global_heap();
    let process = ObjectRef::new(Process::try_new(9).unwrap());
    let po = Ref::new(PendingOperation::try_new().unwrap());
    with_current(Ref::new(Thread::try_new(0, 0).unwrap()), || {
        assert!(matches!(wait_on(&[&process], 0, 0), WaitResult::TimedOut), "a poll finds it not ready");
        for deadline in [5, u64::MAX] {
            let refused = wait_on(&[po.as_object(), &process], deadline, 0);
            assert!(matches!(refused, WaitResult::OutOfMemory), "deadline {deadline}");
        }
    });
    drop(po);
}

/// **A wait takes as many objects as it holds slots for**, and one more is refused before any is
/// read.
#[test]
fn a_wait_on_one_object_more_than_it_holds_is_refused() {
    init_global_heap();
    let pos: std::vec::Vec<Ref<PendingOperation>> =
        (0..=MAX_WAIT_HANDLES).map(|_| Ref::new(PendingOperation::try_new().unwrap())).collect();
    complete_pending_op(&pos[MAX_WAIT_HANDLES - 1], 0, 0);
    let objs: std::vec::Vec<&ObjectRef> = pos.iter().map(Ref::as_object).collect();
    with_current(Ref::new(Thread::try_new(0, 0).unwrap()), || {
        let held = wait_on(&objs[..MAX_WAIT_HANDLES], 0, 0);
        assert!(matches!(held, WaitResult::Signaled(b) if b[MAX_WAIT_HANDLES - 1]), "its last slot read");
        assert!(matches!(wait_on(&objs, 0, 0), WaitResult::OutOfMemory), "one more");
    });
}

/// **An endpoint whose last reference goes while its peer sends into it is borrowed by neither**:
/// the send, under `SCHED`, writes the closing endpoint's ring through the peer's back-pointer, so
/// nothing may hold a `&mut` to that endpoint until its closing has taken `SCHED` and unlinked it.
/// Only Miri sees such a `&mut`; natively, the endpoint's `Drop` asserts its closing ran first.
#[test]
fn an_endpoint_closing_while_its_peer_sends_into_it_is_borrowed_by_neither() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Moved<T>(T);
    // SAFETY: the test hands each reference to one thread at a time.
    unsafe impl<T> Send for Moved<T> {}
    init_global_heap();
    let (a, b) = IpcChannel::try_new_pair(4).unwrap();
    let b = Moved(b);
    let locked = Arc::new(AtomicBool::new(false));
    let sender = {
        let locked = locked.clone();
        std::thread::spawn(move || {
            let b = b;
            let msg = crate::libkern::KBox::<StoredMsg>::try_new_zeroed().unwrap();
            let mut none: [Option<TransferRef>; crate::libkern::ipc::IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
            let mut g = SCHED.lock();
            locked.store(true, Ordering::Release);
            for _ in 0..64 {
                std::thread::yield_now();
            }
            let sent = b.0.send_push(&mut g.tok, &msg, &mut none);
            drop(g);
            (b, sent)
        })
    };
    while !locked.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    drop(a);
    let (b, sent) = sender.join().unwrap();
    assert!(matches!(sent, SendOutcome::Sent { .. } | SendOutcome::PeerClosed));
    drop(b);
}

/// **An endpoint's closing wakes the receivers blocked on its peer**, which then read `PeerClosed`.
#[test]
fn closing_an_endpoint_wakes_a_receiver_blocked_on_its_peer() {
    init_global_heap();
    let (a, b) = IpcChannel::try_new_pair(4).unwrap();
    let waiter = inert_ref(1);
    let (t, on) = (addr(&*waiter), a.as_ptr().cast::<()>());
    {
        let mut g = SCHED.lock();
        waiter.wait_register(&mut g.tok, &[on as usize], false);
        a.add_waiter(&mut g.tok, t).unwrap();
    }
    drop(b);
    let woken = {
        let mut g = SCHED.lock();
        let marked = waiter.wait_signaled(&g.tok)[0];
        a.remove_waiter(&mut g.tok, t);
        waiter.wait_clear(&mut g.tok);
        marked
    };
    assert!(woken, "its receiver woken");
    assert_eq!(ipc_recv_peek(&a), RecvState::PeerClosed);
}

/// **A borrowed thread stands beside what other CPUs write in it**: one CPU reads it through a
/// `&Thread` while another sets its affinity under `SCHED`, and a third, switching it out, writes
/// its saved stack pointer with the lock let go and then clears its guard, as the switch's asm
/// does. Miri reports a race between such a borrow and any write outside a cell; and the stack
/// pointer is read only once the guard is seen clear.
#[test]
fn a_borrowed_thread_stands_beside_its_scheduler_and_its_switch() {
    use core::sync::atomic::AtomicU8;
    init_global_heap();
    let th = Ref::new(Thread::try_new(7, 3).unwrap());
    let shared: &Thread = &th;
    th.raise_on_cpu(&SCHED.lock().tok);
    std::thread::scope(|s| {
        s.spawn(|| (1..=4).for_each(|mask| set_thread_affinity(&th, mask)));
        s.spawn(|| {
            // SAFETY: the slot and the guard byte are the thread's, which `th` pins, and only this
            // switch writes them while the guard is raised: the slot, then the guard.
            unsafe {
                th.sp_slot().write(0x1000);
                AtomicU8::from_ptr(th.on_cpu_ptr()).store(0, Ordering::Release);
            }
        });
        for _ in 0..8 {
            assert_eq!((shared.tid(), shared.owner_pid()), (7, 3));
        }
        let sp = loop {
            if let Some(sp) = shared.parked_sp(&SCHED.lock().tok) {
                break sp;
            }
            std::hint::spin_loop();
        };
        assert_eq!(sp, 0x1000, "read once the switch had committed it");
    });
    assert_eq!(shared.cpu_mask(&SCHED.lock().tok), 4);
}
