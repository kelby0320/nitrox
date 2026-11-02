use super::test_support::*;
use super::*;
use crate::mm::test_support::init_global_heap;

#[test]
fn inert_thread_has_no_stack_and_is_ready() {
    init_global_heap();
    let t = Thread::try_new(7, 3).unwrap();
    assert_eq!(t.tid(), 7);
    assert_eq!(t.owner_pid(), 3);
    assert!(t.stack.is_none());
    assert_eq!(t.state, ThreadState::Ready);
}

#[test]
fn boot_thread_is_running_with_no_stack() {
    init_global_heap();
    let t = Thread::try_new_boot(0, 0).unwrap();
    assert_eq!(t.state, ThreadState::Running);
    assert!(t.stack.is_none());
}

#[test]
fn state_accessors_round_trip() {
    init_global_heap();
    let t = Thread::try_new(1, 1).unwrap();
    let obj = KBox::into_raw(t).as_ptr() as *mut ();
    // SAFETY: `obj` is a live Thread; this test is single-threaded and
    // holds the only reference, satisfying the accessor contract.
    unsafe {
        assert_eq!(Thread::state(obj), ThreadState::Ready);
        Thread::set_state(obj, ThreadState::Running);
        assert_eq!(Thread::state(obj), ThreadState::Running);
        Thread::set_state(obj, ThreadState::Exited);
        assert_eq!(Thread::state(obj), ThreadState::Exited);
        drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Thread)));
    }
}

#[test]
fn saved_sp_accessor_reads_and_writes_through_slot() {
    init_global_heap();
    let t = Thread::try_new(1, 1).unwrap();
    let obj = KBox::into_raw(t).as_ptr() as *mut ();
    // SAFETY: as above.
    unsafe {
        assert_eq!(Thread::saved_sp(obj), 0);
        *Thread::saved_sp_mut_ptr(obj) = 0xDEAD_0000;
        assert_eq!(Thread::saved_sp(obj), 0xDEAD_0000);
        drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Thread)));
    }
}

#[test]
fn user_thread_records_descent_params_and_root() {
    init_global_heap();
    let (proc_ref, root) = user_process_ref(1);
    let t = user_thread_no_stack(1, proc_ref, 0x40_0000, 0x7fff_f000);
    let obj = KBox::into_raw(t).as_ptr() as *mut ();
    // SAFETY: live Thread, single-threaded test.
    unsafe {
        assert_eq!(Thread::user_entry(obj), Some((0x40_0000, 0x7fff_f000)));
        assert_eq!(Thread::addr_space_root(obj), Some(root));
        drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Thread)));
    }
}

#[test]
fn dropping_a_user_thread_releases_its_process_no_cycle() {
    use crate::object::header::test_probe;
    init_global_heap();
    test_probe::reset();
    let (proc_ref, _root) = user_process_ref(1);
    let t = user_thread_no_stack(1, proc_ref, 0x40_0000, 0x7fff_f000);
    let tref = {
        let ptr = KBox::into_raw(t).as_ptr() as *mut ();
        // SAFETY: into_raw yielded the single creation ref; adopt it.
        unsafe { ObjectRef::from_raw(ptr, KObjectType::Thread) }
    };
    assert_eq!(test_probe::process_destroys(), 0);
    assert_eq!(test_probe::thread_destroys(), 0);
    drop(tref);
    assert_eq!(test_probe::thread_destroys(), 1);
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn wait_bookkeeping_round_trips() {
    init_global_heap();
    let t = Thread::try_new(1, 1).unwrap();
    let obj = KBox::into_raw(t).as_ptr() as *mut ();
    // SAFETY: live Thread, single-threaded test (stands in for SCHED).
    unsafe {
        assert_eq!(Thread::wait_phase(obj), WaitPhase::Running as u8);

        Thread::wait_register(obj, &[0xAA, 0xBB], true);
        assert_eq!(Thread::wait_phase(obj), WaitPhase::Waiting as u8);
        assert!(Thread::wait_has_deadline(obj));

        assert!(Thread::wait_mark_signaled(obj, 0xBB));
        assert!(!Thread::wait_mark_signaled(obj, 0xCC));

        let mut out = [(0usize, false); MAX_WAIT_HANDLES];
        let n = Thread::wait_snapshot(obj, &mut out);
        assert_eq!(n, 2);
        assert_eq!(out[0], (0xAA, false));
        assert_eq!(out[1], (0xBB, true));

        assert!(Thread::wait_try_wake(obj));
        assert!(!Thread::wait_try_wake(obj));
        assert_eq!(Thread::wait_phase(obj), WaitPhase::Woken as u8);

        Thread::wait_clear(obj);
        assert_eq!(Thread::wait_phase(obj), WaitPhase::Running as u8);
        assert!(!Thread::wait_has_deadline(obj));
        assert_eq!(Thread::wait_snapshot(obj, &mut out), 0);

        drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Thread)));
    }
}

/// **The switch path's save-area pointer may write**: `XSAVE` stores through it on every switch,
/// and one taken through a shared borrow of the area's box allowed reads only.
#[test]
fn the_fpu_area_pointer_may_be_written_through() {
    init_global_heap();
    let obj = KBox::into_raw(Thread::try_new_boot(0, 0).unwrap()).as_ptr() as *mut ();
    // SAFETY: `obj` is a live Thread this test alone holds; one byte inside its area is written
    // as a save would write it, and the box is rebuilt from the pointer it gave.
    unsafe {
        let area = Thread::fpu_area_ptr(obj);
        assert!(!area.is_null());
        area.cast::<u8>().add(512).write(0xAA);
        assert_eq!(area.cast::<u8>().add(512).read(), 0xAA);
        drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Thread)));
    }
}
