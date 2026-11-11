use super::test_support::*;
use super::*;
use crate::mm::test_support::init_global_heap;

#[test]
fn inert_thread_has_no_stack_and_is_ready() {
    init_global_heap();
    let mut t = Thread::try_new(7, 3).unwrap();
    assert_eq!(t.tid(), 7);
    assert_eq!(t.owner_pid(), 3);
    assert!(t.stack.is_none());
    assert_eq!(t.sched.get_mut().state, ThreadState::Ready);
}

#[test]
fn boot_thread_is_running_with_no_stack() {
    init_global_heap();
    let mut t = Thread::try_new_boot(0, 0).unwrap();
    assert_eq!(t.sched.get_mut().state, ThreadState::Running);
    assert!(t.stack.is_none());
}

#[test]
fn state_accessors_round_trip() {
    init_global_heap();
    let mut tok = SchedToken::for_test();
    let t = Ref::new(Thread::try_new(1, 1).unwrap());
    assert_eq!(t.state(&tok), ThreadState::Ready);
    t.set_state(&mut tok, ThreadState::Running);
    assert_eq!(t.state(&tok), ThreadState::Running);
    t.set_state(&mut tok, ThreadState::Exited);
    assert_eq!(t.state(&tok), ThreadState::Exited);
}

/// **A parked context is read only once its guard is clear**: the switch writes the saved stack
/// pointer after `SCHED` is released, so a read while the guard is raised could race it.
#[test]
fn the_saved_sp_is_read_through_its_slot_and_only_with_the_guard_clear() {
    init_global_heap();
    let tok = SchedToken::for_test();
    let t = Ref::new(Thread::try_new(1, 1).unwrap());
    assert_eq!(t.parked_sp(&tok), Some(0));
    t.raise_on_cpu(&tok);
    assert_eq!(t.parked_sp(&tok), None, "mid-switch-out");
    // SAFETY: the slot and the guard byte are the thread's, which `t` pins; written as the switch
    // writes them, the slot first.
    unsafe {
        t.sp_slot().write(0xDEAD_0000);
        t.on_cpu_ptr().write(0);
    }
    assert_eq!(t.parked_sp(&tok), Some(0xDEAD_0000));
}

#[test]
fn user_thread_records_descent_params_and_root() {
    init_global_heap();
    let (proc_ref, root) = user_process_ref(1);
    let t = user_thread_no_stack(1, proc_ref, 0x40_0000, 0x7fff_f000);
    assert_eq!(t.user_entry(), Some((0x40_0000, 0x7fff_f000)));
    assert_eq!(t.addr_space_root(), Some(root));
    assert_eq!(t.process().map(Process::pid), Some(1));
}

#[test]
fn dropping_a_user_thread_releases_its_process_no_cycle() {
    use crate::object::header::test_probe;
    init_global_heap();
    test_probe::reset();
    let (proc_ref, _root) = user_process_ref(1);
    let t = user_thread_no_stack(1, proc_ref, 0x40_0000, 0x7fff_f000);
    let tref = crate::object::ObjectRef::new(t);
    assert_eq!(test_probe::process_destroys(), 0);
    assert_eq!(test_probe::thread_destroys(), 0);
    drop(tref);
    assert_eq!(test_probe::thread_destroys(), 1);
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn wait_bookkeeping_round_trips() {
    init_global_heap();
    let mut tok = SchedToken::for_test();
    let t = Ref::new(Thread::try_new(1, 1).unwrap());
    assert_eq!(t.wait_phase(), WaitPhase::Running as u8);

    t.wait_register(&mut tok, &[0xAA, 0xBB], true);
    assert_eq!(t.wait_phase(), WaitPhase::Waiting as u8);
    assert!(t.wait_has_deadline(&tok));

    assert!(t.wait_mark_signaled(&mut tok, 0xBB));
    assert!(!t.wait_mark_signaled(&mut tok, 0xCC));

    assert_eq!(t.wait_signaled(&tok)[..3], [false, true, false]);

    assert!(t.wait_try_wake());
    assert!(!t.wait_try_wake());
    assert_eq!(t.wait_phase(), WaitPhase::Woken as u8);

    t.wait_clear(&mut tok);
    assert_eq!(t.wait_phase(), WaitPhase::Running as u8);
    assert!(!t.wait_has_deadline(&tok));
    assert_eq!(t.wait_signaled(&tok), [false; MAX_WAIT_HANDLES], "none past a count of 0");

    // A second wait over as many objects begins with none of them signalled: what the first
    // wait's wake marked is not this one's result.
    t.wait_register(&mut tok, &[0xAA, 0xBB], false);
    assert_eq!(t.wait_signaled(&tok)[..2], [false, false]);
}

/// **The switch path's save area may be written through a shared thread**: `XSAVE` stores
/// through it on every switch, while the thread's lists and handles hold it shared.
#[test]
fn the_fpu_area_pointer_may_be_written_through() {
    init_global_heap();
    let t = Ref::new(Thread::try_new_boot(0, 0).unwrap());
    let shared: &Thread = &t;
    let area = shared.fpu_area();
    assert!(!area.is_null());
    // SAFETY: the area is the thread's, which `t` pins; one byte inside it is written as a save
    // would write it.
    unsafe {
        area.cast::<u8>().add(512).write(0xAA);
        assert_eq!(area.cast::<u8>().add(512).read(), 0xAA);
    }
    assert_eq!(shared.tid(), 0, "the shared borrow stands beside the write");
    assert!(Ref::new(Thread::try_new(1, 0).unwrap()).fpu_area().is_null(), "an inert thread has none");
}
