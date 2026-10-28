use super::*;
use crate::handle::FAIL_NEXT_ACQUIRE;
use crate::handle::entry::FREE_NEXT_TAIL;
use crate::libkern::KBox;
use crate::mm::test_support::init_global_heap;
use crate::object::header::test_probe;
use crate::object::{Process, Thread};

fn fresh_table() -> HandleTable {
    init_global_heap();
    HandleTable::try_new(0xCAFE_BABE_DEAD_BEEF).unwrap()
}

fn mk_process(pid: u32) -> *mut () {
    KBox::into_raw(Process::try_new(pid).unwrap()).as_ptr() as *mut ()
}

fn mk_thread(tid: u32, owner_pid: u32) -> *mut () {
    KBox::into_raw(Thread::try_new(tid, owner_pid).unwrap()).as_ptr() as *mut ()
}

fn fake_obj(addr: usize) -> *mut () {
    addr as *mut ()
}

fn close_release(t: &HandleTable, h: RawHandle, pid: u32) -> Result<(), HandleError> {
    let co = t.close(h, pid)?;
    // SAFETY: `co` carries exactly the handle's one reference; we
    // account for it once.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    Ok(())
}

fn process_magic_ok(r: &ObjectRef) -> bool {
    debug_assert_eq!(r.object_type(), KObjectType::Process);
    // SAFETY: `r` pins a live `Process`.
    unsafe { &*(r.as_ptr() as *const Process) }.magic_ok()
}

fn sig() -> Rights {
    Rights::SIGNAL
}
fn sigterm() -> Rights {
    Rights::SIGNAL | Rights::TERMINATE
}

#[test]
fn try_new_eagerly_allocates_segment_zero() {
    let t = fresh_table();
    assert_eq!(t.segments_allocated(), 1);
    assert_eq!(t.allocated_count(), 0);
}

#[test]
fn allocate_returns_non_null_handle() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    assert!(!h.is_null());
    close_release(&t, h, 1).unwrap();
}

#[test]
fn allocate_lookup_round_trip() {
    let t = fresh_table();
    let p = mk_process(7);
    let h = t.allocate(7, p, KObjectType::Process, sigterm()).unwrap();
    let ok = t.lookup(h, 7, sig()).unwrap();
    assert_eq!(ok.object.as_ptr(), p);
    assert_eq!(ok.object.object_type(), KObjectType::Process);
    assert!(ok.rights.contains(Rights::SIGNAL));
    assert!(ok.rights.contains(Rights::TERMINATE));
    drop(ok);
    close_release(&t, h, 7).unwrap();
}

fn sweep_owned(t: &HandleTable, pid: u32) -> usize {
    let mut cursor = SweepCursor::START;
    let mut total = 0;
    loop {
        let mut batch: [Option<ClosedObject>; 4] = [None; 4];
        let (n, more) = t.close_owned_batch(pid, &mut cursor, &mut batch);
        for co in batch[..n].iter().flatten() {
            // SAFETY: each carries exactly the closed handle's one reference.
            drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
        }
        total += n;
        if !more {
            return total;
        }
    }
}

#[test]
fn sweep_closes_every_handle_of_one_process_and_no_others() {
    let t = fresh_table();
    let mut doomed = Vec::new();
    let mut survivor = Vec::new();
    for i in 0..5 {
        doomed.push(t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap());
        survivor.push(t.allocate(2, mk_process(2), KObjectType::Process, sig()).unwrap());
        let _ = i;
    }

    assert_eq!(sweep_owned(&t, 1), 5);

    for h in &doomed {
        assert_eq!(t.lookup(*h, 1, sig()).unwrap_err(), HandleError::InvalidHandle);
    }
    for h in &survivor {
        assert!(t.lookup(*h, 2, sig()).is_ok());
    }
    for h in survivor {
        close_release(&t, h, 2).unwrap();
    }
}

#[test]
fn sweep_releases_the_objects() {
    let t = fresh_table();
    let before = test_probe::process_destroys();
    for _ in 0..3 {
        t.allocate(9, mk_process(9), KObjectType::Process, sig()).unwrap();
    }
    assert_eq!(test_probe::process_destroys(), before, "nothing destroyed yet");
    assert_eq!(sweep_owned(&t, 9), 3);
    assert_eq!(test_probe::process_destroys(), before + 3, "each swept handle held the last reference to its object");
}

#[test]
fn sweep_resumes_across_batches() {
    let t = fresh_table();
    for _ in 0..17 {
        t.allocate(3, mk_process(3), KObjectType::Process, sig()).unwrap();
    }
    assert_eq!(sweep_owned(&t, 3), 17);
    assert_eq!(sweep_owned(&t, 3), 0, "a second sweep finds nothing left");
}

#[test]
fn sweep_of_a_process_with_no_handles_is_a_no_op() {
    let t = fresh_table();
    let h = t.allocate(4, mk_process(4), KObjectType::Process, sig()).unwrap();
    assert_eq!(sweep_owned(&t, 99), 0);
    assert!(t.lookup(h, 4, sig()).is_ok());
    close_release(&t, h, 4).unwrap();
}

#[test]
fn swept_slots_are_reusable() {
    let t = fresh_table();
    let before = t.allocated_count();
    for _ in 0..8 {
        t.allocate(5, mk_process(5), KObjectType::Process, sig()).unwrap();
    }
    assert_eq!(t.allocated_count(), before + 8);
    assert_eq!(sweep_owned(&t, 5), 8);
    let h = t.allocate(6, mk_process(6), KObjectType::Process, sig()).unwrap();
    close_release(&t, h, 6).unwrap();
    assert_eq!(t.allocated_count(), before, "swept slots were not returned to the freelist");
}

#[test]
fn allocate_rejects_incompatible_rights_for_type() {
    let t = fresh_table();
    let err = t.allocate(1, fake_obj(0x1000), KObjectType::Process, Rights::MAP_WRITE).unwrap_err();
    assert_eq!(err, HandleError::BadRights);
}

#[test]
fn allocate_many_handles_in_a_row() {
    let t = fresh_table();
    let mut handles = [RawHandle::NULL; 32];
    for (i, h) in handles.iter_mut().enumerate() {
        *h = t.allocate(1, mk_process(i as u32), KObjectType::Process, sig()).unwrap();
    }
    for i in 0..handles.len() {
        for j in (i + 1)..handles.len() {
            assert_ne!(handles[i], handles[j], "duplicate handle at {i} {j}");
        }
    }
    assert_eq!(t.allocated_count(), handles.len());
    for h in handles {
        close_release(&t, h, 1).unwrap();
    }
}

#[test]
fn lookup_wrong_owner_pid_returns_not_owner() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    assert_eq!(t.lookup(h, 2, Rights::empty()).unwrap_err(), HandleError::NotOwner);
    close_release(&t, h, 1).unwrap();
}

#[test]
fn lookup_correct_owner_succeeds_for_zero_pid_too() {
    let t = fresh_table();
    let h = t.allocate(0, mk_process(0), KObjectType::Process, sig()).unwrap();
    assert!(t.lookup(h, 0, sig()).is_ok());
    close_release(&t, h, 0).unwrap();
}

#[test]
fn lookup_insufficient_rights_returns_no_access() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    assert_eq!(t.lookup(h, 1, Rights::TERMINATE).unwrap_err(), HandleError::NoAccess);
    close_release(&t, h, 1).unwrap();
}

#[test]
fn lookup_superset_rights_request_returns_no_access() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    assert_eq!(t.lookup(h, 1, sigterm()).unwrap_err(), HandleError::NoAccess,);
    close_release(&t, h, 1).unwrap();
}

#[test]
fn lookup_subset_rights_request_succeeds() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sigterm()).unwrap();
    assert!(t.lookup(h, 1, Rights::SIGNAL).is_ok());
    assert!(t.lookup(h, 1, Rights::TERMINATE).is_ok());
    assert!(t.lookup(h, 1, sigterm()).is_ok());
    assert!(t.lookup(h, 1, Rights::empty()).is_ok());
    close_release(&t, h, 1).unwrap();
}

#[test]
fn null_handle_lookup_returns_null_handle() {
    let t = fresh_table();
    assert_eq!(t.lookup(RawHandle::NULL, 0, Rights::empty()).unwrap_err(), HandleError::NullHandle,);
}

#[test]
fn out_of_range_segment_returns_invalid_handle() {
    let t = fresh_table();
    let bogus = RawHandle::encode((DIRECTORY_LEN - 1) as u32, 0, 1);
    assert_eq!(t.lookup(bogus, 0, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
}

#[test]
fn close_makes_handle_invalid() {
    let t = fresh_table();
    let p = mk_process(1);
    let h = t.allocate(1, p, KObjectType::Process, sig()).unwrap();
    let prev = t.close(h, 1).unwrap();
    assert_eq!(prev.0, p);
    assert_eq!(prev.1, KObjectType::Process);
    // SAFETY: the table's reference, which `close` handed back.
    drop(unsafe { ObjectRef::from_raw(prev.0, prev.1) });
    assert_eq!(t.lookup(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
}

#[test]
fn close_rejects_wrong_owner() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    assert_eq!(t.close(h, 2).unwrap_err(), HandleError::NotOwner);
    assert!(t.lookup(h, 1, sig()).is_ok());
    close_release(&t, h, 1).unwrap();
}

#[test]
fn double_close_returns_invalid_on_second() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    close_release(&t, h, 1).unwrap();
    assert_eq!(t.close(h, 1).unwrap_err(), HandleError::InvalidHandle);
}

#[test]
fn close_then_allocate_reuses_slot_with_new_generation() {
    let t = fresh_table();
    let h1 = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    let (seg1, slot1, gen1) = h1.decode();
    close_release(&t, h1, 1).unwrap();
    let h2 = t.allocate(1, mk_process(2), KObjectType::Process, sig()).unwrap();
    let (seg2, slot2, gen2) = h2.decode();
    assert_eq!((seg1, slot1), (seg2, slot2), "the freed slot must be the one reused");
    assert_ne!(gen1, gen2, "generation must bump on slot reuse");
    assert_eq!(t.lookup(h1, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
    assert!(t.lookup(h2, 1, sig()).is_ok());
    close_release(&t, h2, 1).unwrap();
}

#[test]
fn generation_wraps_at_max_without_retiring_the_slot() {
    let t = fresh_table();
    let h1 = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    let (seg, slot, _) = h1.decode();

    {
        let _guard = t.inner.lock();
        let entries_ptr = t.directory[seg as usize].load(Ordering::Acquire);
        assert!(!entries_ptr.is_null());
        // SAFETY: segment published once, outlives the table; we hold the
        // rank-3 lock so we are the only writer.
        let entry = unsafe { &(*entries_ptr)[slot as usize] };
        let _wg = WriteGuard::new(entry);
        entry.generation.store(RawHandle::GENERATION_MAX, Ordering::Relaxed);
    }
    let h_max = RawHandle::encode(seg, slot, RawHandle::GENERATION_MAX);
    // A max-generation handle still has bit 63 clear — never a negative isize.
    assert!((h_max.bits() as i64) >= 0, "max-generation handle aliases an error code",);
    assert!(t.lookup(h_max, 1, sig()).is_ok());

    // Reallocate: the slot is recycled, not retired, and wraps
    // `GENERATION_MAX` to 0.
    close_release(&t, h_max, 1).unwrap();
    let h_next = t.allocate(1, mk_process(2), KObjectType::Process, sig()).unwrap();
    let (nseg, nslot, ngen) = h_next.decode();
    assert_eq!((nseg, nslot), (seg, slot), "slot must be recycled, not retired");
    assert_eq!(ngen, 0, "generation wraps from GENERATION_MAX to 0");
    assert!((h_next.bits() as i64) >= 0, "wrapped handle stays non-negative");
    assert_eq!(t.lookup(h_max, 1, sig()).unwrap_err(), HandleError::InvalidHandle,);
    close_release(&t, h_next, 1).unwrap();
}

#[test]
fn restrict_cannot_amplify_rights() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    t.restrict(h, 1, Rights::TERMINATE).unwrap();
    assert_eq!(t.lookup(h, 1, Rights::SIGNAL).unwrap_err(), HandleError::NoAccess,);
    assert!(t.lookup(h, 1, Rights::empty()).is_ok());
    close_release(&t, h, 1).unwrap();
}

#[test]
fn restrict_drops_rights() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sigterm()).unwrap();
    t.restrict(h, 1, Rights::SIGNAL).unwrap();
    assert!(t.lookup(h, 1, Rights::SIGNAL).is_ok());
    assert_eq!(t.lookup(h, 1, Rights::TERMINATE).unwrap_err(), HandleError::NoAccess,);
    close_release(&t, h, 1).unwrap();
}

#[test]
fn restrict_rejects_wrong_owner() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    assert_eq!(t.restrict(h, 2, Rights::SIGNAL).unwrap_err(), HandleError::NotOwner);
    close_release(&t, h, 1).unwrap();
}

// The entry points validate in different orders, which decides how an input
// must be built to isolate one guard:
//
//   close/restrict:  null → seg|slot range → segment present → generation → owner
//                    → object non-null
//   lookup:          null → seg range → segment present → slot range → seqlock snapshot
//                    → object non-null → refcount → seq re-check → generation → owner
//                    → rights
//
// `lookup` rejects a null object at step 6, before generation and owner, so a
// closed slot isolates `restrict`'s object-null guard but not `lookup`'s
// generation guard. For a closed handle and a non-owner caller `lookup`
// answers `InvalidHandle` and `close`/`restrict` answer `NotOwner`: a
// divergence from the spec's ordered steps, filed as
// `TODO(handle-validation-order)` and marked at the two ladders. Each test
// below isolates its guard; otherwise it passes with that guard deleted.

/// A slot freed and handed to a new object of the same owner, returning
/// `(stale, live)`: the only shape where the generation check stands alone.
fn reused_slot(t: &HandleTable) -> (RawHandle, RawHandle) {
    let stale = t.allocate(1, mk_process(1), KObjectType::Process, sigterm()).unwrap();
    let (seg1, slot1, gen1) = stale.decode();
    close_release(t, stale, 1).unwrap();
    let live = t.allocate(1, mk_process(2), KObjectType::Process, sigterm()).unwrap();
    let (seg2, slot2, gen2) = live.decode();
    assert_eq!((seg1, slot1), (seg2, slot2), "the freed slot must be the one reused");
    assert_ne!(gen1, gen2, "reuse must bump the generation");
    (stale, live)
}

/// `close`'s generation check alone. After a close the slot is empty, so a
/// second close in `double_close_returns_invalid_on_second` is rejected by the
/// null-object check instead. Here the slot is live, and a stale handle that
/// closed it would destroy an object its holder had no capability for.
#[test]
fn close_rejects_a_stale_handle_whose_slot_was_reused() {
    let t = fresh_table();
    let (stale, live) = reused_slot(&t);
    assert_eq!(t.close(stale, 1).unwrap_err(), HandleError::InvalidHandle);
    assert!(t.lookup(live, 1, sigterm()).is_ok(), "the live object was closed by a stale handle");
    close_release(&t, live, 1).unwrap();
}

/// The same for `restrict`.
#[test]
fn restrict_rejects_a_stale_handle_whose_slot_was_reused() {
    let t = fresh_table();
    let (stale, live) = reused_slot(&t);
    assert_eq!(t.restrict(stale, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle);
    let r = t.lookup(live, 1, sigterm()).expect("a stale restrict stripped the live handle");
    drop(r);
    close_release(&t, live, 1).unwrap();
}

/// `restrict`'s null-object check alone: `close` leaves the generation (spec
/// § "Generation counter behavior") and `owner_pid`, so on a closed slot only
/// this check can fire.
#[test]
fn restrict_rejects_a_handle_whose_object_was_closed() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sigterm()).unwrap();
    close_release(&t, h, 1).unwrap();
    assert_eq!(t.restrict(h, 1, sig()).unwrap_err(), HandleError::InvalidHandle);
}

#[test]
fn the_null_handle_is_rejected_by_lookup_close_and_restrict() {
    let t = fresh_table();
    assert_eq!(t.lookup(RawHandle::NULL, 1, Rights::empty()).unwrap_err(), HandleError::NullHandle,);
    assert_eq!(t.close(RawHandle::NULL, 1).unwrap_err(), HandleError::NullHandle);
    assert_eq!(t.restrict(RawHandle::NULL, 1, Rights::empty()).unwrap_err(), HandleError::NullHandle,);
}

/// Segment and slot indices past the end, on all three entry points.
/// `close`/`restrict` fold them into one `||`, so each half needs an input
/// that trips only it.
#[test]
fn out_of_range_ids_are_rejected_by_lookup_close_and_restrict() {
    let t = fresh_table();
    let bad_seg = RawHandle::encode(DIRECTORY_LEN as u32, 0, 1);
    let bad_slot = RawHandle::encode(0, SEGMENT_LEN as u32, 1);
    for h in [bad_seg, bad_slot] {
        assert_eq!(t.lookup(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
        assert_eq!(t.close(h, 1).unwrap_err(), HandleError::InvalidHandle);
        assert_eq!(t.restrict(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
    }
}

/// A segment in range but never brought online: `try_new` grows only
/// segment 0, so segment 1 is a null directory slot.
#[test]
fn an_unpopulated_segment_is_rejected_by_lookup_close_and_restrict() {
    let t = fresh_table();
    assert_eq!(t.segments_allocated(), 1, "the fixture must leave segment 1 absent");
    let h = RawHandle::encode(1, 0, 1);
    assert_eq!(t.lookup(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
    assert_eq!(t.close(h, 1).unwrap_err(), HandleError::InvalidHandle);
    assert_eq!(t.restrict(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
}

#[test]
fn duplicate_requires_duplicate_right() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    assert_eq!(t.duplicate(h, 1, sig()).unwrap_err(), HandleError::NoAccess,);
    close_release(&t, h, 1).unwrap();
}

#[test]
fn duplicate_yields_independent_handle_with_intersected_rights() {
    let t = fresh_table();
    let original = t
        .allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL | Rights::TERMINATE | Rights::DUPLICATE)
        .unwrap();
    let copy = t.duplicate(original, 1, Rights::SIGNAL | Rights::DUPLICATE).unwrap();
    assert_ne!(copy, original);
    assert!(t.lookup(copy, 1, Rights::SIGNAL).is_ok());
    assert_eq!(t.lookup(copy, 1, Rights::TERMINATE).unwrap_err(), HandleError::NoAccess,);
    assert!(t.lookup(original, 1, Rights::TERMINATE).is_ok());
    close_release(&t, copy, 1).unwrap();
    assert!(t.lookup(original, 1, Rights::SIGNAL).is_ok());
    close_release(&t, original, 1).unwrap();
}

#[test]
fn duplicate_refcount_accounting_destroys_once_at_last_close() {
    let t = fresh_table();
    test_probe::reset();
    let original = t.allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL | Rights::DUPLICATE).unwrap(); // object refcount = 1 (one handle)
    let copy = t.duplicate(original, 1, Rights::SIGNAL | Rights::DUPLICATE).unwrap(); // refcount = 2 (two handles)
    close_release(&t, copy, 1).unwrap();
    assert_eq!(test_probe::process_destroys(), 0, "destroyed while a handle remains");
    close_release(&t, original, 1).unwrap();
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn duplicate_allocate_error_reclaims_ref() {
    let t = fresh_table();
    test_probe::reset();
    let original = t.allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL | Rights::DUPLICATE).unwrap(); // refcount = 1
    FAIL_NEXT_ALLOCATE.with(|f| f.set(true));
    assert_eq!(t.duplicate(original, 1, Rights::SIGNAL | Rights::DUPLICATE).unwrap_err(), HandleError::OutOfMemory,);
    assert_eq!(test_probe::process_destroys(), 0);
    assert!(t.lookup(original, 1, Rights::SIGNAL).is_ok());
    close_release(&t, original, 1).unwrap();
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn lookup_holds_ref_until_lookupok_dropped() {
    let t = fresh_table();
    test_probe::reset();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    let ok = t.lookup(h, 1, sig()).unwrap(); // refcount = 2
    close_release(&t, h, 1).unwrap(); // refcount = 1 (the ObjectRef)
    assert_eq!(test_probe::process_destroys(), 0, "destroyed while ObjectRef held");
    assert!(process_magic_ok(&ok.object), "object freed under a held ref");
    drop(ok); // refcount = 0 -> destroy
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn close_does_not_destroy_until_caller_drops_token() {
    let t = fresh_table();
    test_probe::reset();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    let co = t.close(h, 1).unwrap();
    assert_eq!(test_probe::process_destroys(), 0);
    // SAFETY: `co` still holds the table's reference, so `close` freed nothing.
    assert!(unsafe { &*(co.0 as *const Process) }.magic_ok());
    // SAFETY: the table's reference, which `close` handed back.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn stat_returns_snapshot_when_inspect_granted() {
    let t = fresh_table();
    let h = t.allocate(42, mk_process(42), KObjectType::Process, Rights::SIGNAL | Rights::INSPECT).unwrap();
    let s = t.stat(h, 42).unwrap();
    assert_eq!(s.object_type, KObjectType::Process);
    assert!(s.rights.contains(Rights::SIGNAL));
    assert_eq!(s.owner_pid, 42);
    let (_, _, generation) = h.decode();
    assert_eq!(s.generation, generation);
    close_release(&t, h, 42).unwrap();
}

#[test]
fn stat_requires_inspect_right() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    assert_eq!(t.stat(h, 1).unwrap_err(), HandleError::NoAccess);
    close_release(&t, h, 1).unwrap();
}

#[test]
fn segment_grows_when_first_segment_full() {
    let t = fresh_table();
    let mut handles = KVec::<RawHandle>::new();
    handles.try_reserve(SEGMENT_LEN + 1).unwrap();
    for i in 0..SEGMENT_LEN {
        handles.try_push(t.allocate(1, mk_process(i as u32), KObjectType::Process, sig()).unwrap()).unwrap();
    }
    assert_eq!(t.segments_allocated(), 1);
    let h = t.allocate(1, mk_process(0xFFFF), KObjectType::Process, sig()).unwrap();
    handles.try_push(h).unwrap();
    assert_eq!(t.segments_allocated(), 2);
    let (seg, _, _) = h.decode();
    assert_eq!(seg, 1, "second segment id");
    for i in 0..handles.len() {
        close_release(&t, handles[i], 1).unwrap();
    }
}

#[test]
fn freelist_invariant_count_matches_chain() {
    let t = fresh_table();
    let mut handles = [RawHandle::NULL; 64];
    for (i, h) in handles.iter_mut().enumerate() {
        *h = t.allocate(1, mk_process(i as u32), KObjectType::Process, sig()).unwrap();
    }
    for i in (0..64).step_by(2) {
        close_release(&t, handles[i], 1).unwrap();
    }
    let h_temp = t.allocate(1, mk_process(0xAAAA), KObjectType::Process, sig()).unwrap();
    close_release(&t, h_temp, 1).unwrap();
    let guard = t.inner.lock();
    let free_head = guard.segment_meta[0].free_head;
    let free_count = guard.segment_meta[0].free_count;
    drop(guard);
    let entries_ptr = t.directory[0].load(Ordering::Acquire);
    assert!(!entries_ptr.is_null());
    // SAFETY: segment 0's entries, which the table keeps for its life.
    let entries = unsafe { &*entries_ptr };
    let mut idx = free_head;
    let mut walked = 0u32;
    while idx != FREE_NEXT_TAIL {
        assert!((idx as usize) < SEGMENT_LEN, "freelist idx out of range");
        walked += 1;
        assert!(walked <= SEGMENT_LEN as u32 + 1, "freelist appears cyclic");
        idx = entries[idx as usize].free_next.load(Ordering::Relaxed);
    }
    assert_eq!(walked, free_count, "free_count mismatch with chain length");
    for i in (1..64).step_by(2) {
        close_release(&t, handles[i], 1).unwrap();
    }
}

#[test]
fn failed_acquire_refcount_returns_invalid_handle() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    FAIL_NEXT_ACQUIRE.with(|f| f.set(true));
    assert_eq!(t.lookup(h, 1, sig()).unwrap_err(), HandleError::InvalidHandle,);
    assert!(t.lookup(h, 1, sig()).is_ok());
    close_release(&t, h, 1).unwrap();
}

#[test]
fn destructor_dispatches_on_object_type() {
    let t = fresh_table();
    test_probe::reset();
    let hp = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    let ht = t.allocate(1, mk_thread(1, 1), KObjectType::Thread, sig()).unwrap();
    close_release(&t, hp, 1).unwrap();
    assert_eq!(test_probe::process_destroys(), 1);
    assert_eq!(test_probe::thread_destroys(), 0, "wrong destructor for Process");
    close_release(&t, ht, 1).unwrap();
    assert_eq!(test_probe::process_destroys(), 1);
    assert_eq!(test_probe::thread_destroys(), 1, "Thread destructor did not run");
}

/// Each thread owns its own PID space and loops allocate/lookup/close on
/// real objects. Cross-pid lookups always fail; at the end no handles remain
/// and every object was destroyed exactly once.
#[test]
fn concurrent_allocate_lookup_close_pid_isolation() {
    use std::sync::Arc;
    use std::thread;

    let t = Arc::new(fresh_table());
    const N_THREADS: usize = 8;
    const ITERS: usize = 2000;

    let workers: Vec<_> = (0..N_THREADS)
        .map(|tid| {
            let t = Arc::clone(&t);
            let my_pid = (tid as u32) + 1;
            thread::spawn(move || {
                test_probe::reset();
                for i in 0..ITERS {
                    let obj = mk_process(my_pid * 1_000_000 + i as u32);
                    let h = t.allocate(my_pid, obj, KObjectType::Process, sig()).expect("allocate");
                    let ok = t.lookup(h, my_pid, sig()).expect("lookup");
                    assert!(process_magic_ok(&ok.object));
                    drop(ok);
                    let other_pid = if my_pid == 1 { 2 } else { 1 };
                    assert_eq!(t.lookup(h, other_pid, sig()).unwrap_err(), HandleError::NotOwner,);
                    close_release(&t, h, my_pid).expect("close");
                }
                test_probe::process_destroys()
            })
        })
        .collect();
    let mut total_destroys = 0usize;
    for w in workers {
        total_destroys += w.join().expect("join");
    }
    assert_eq!(total_destroys, N_THREADS * ITERS, "every object destroyed once");
    let h = t.allocate(99, mk_process(99), KObjectType::Process, sig()).unwrap();
    close_release(&t, h, 99).unwrap();
    assert!(t.allocated_count() <= 1, "stray handles after stress: {}", t.allocated_count());
}

/// One writer closes and reallocates a real `Process` in one slot while
/// readers look it up. A `LookupOk` pins the object, so its sentinel and owner
/// must be consistent: the seqlock catches torn reads, the refcount keeps the
/// object alive.
#[test]
fn concurrent_torn_read_torture() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    let t = Arc::new(fresh_table());
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let t = Arc::clone(&t);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut pid = 1u32;
            let mut cycles_since_yield = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let obj = mk_process(pid);
                if let Ok(h) = t.allocate(pid, obj, KObjectType::Process, sig()) {
                    let _ = close_release(&t, h, pid);
                } else {
                    // SAFETY: adopts the object's creation reference.
                    drop(unsafe { ObjectRef::from_raw(obj, KObjectType::Process) });
                }
                pid = pid.wrapping_add(1);
                if pid == 0 {
                    pid = 1;
                }
                cycles_since_yield += 1;
                if cycles_since_yield == 8 {
                    thread::yield_now();
                    cycles_since_yield = 0;
                }
            }
        })
    };

    let mut readers = Vec::new();
    for _ in 0..2 {
        let t = Arc::clone(&t);
        let stop = Arc::clone(&stop);
        readers.push(thread::spawn(move || {
            let mut iterations = 0u32;
            while !stop.load(Ordering::Relaxed) && iterations < 1_000 {
                for slot in 0..4u32 {
                    for pid in 1..8u32 {
                        for generation in 1..8u32 {
                            let h = RawHandle::encode(0, slot, generation);
                            if let Ok(ok) = t.lookup(h, pid, sig()) {
                                // SAFETY: `ok.object` pins the object, and this test stores only `Process`es.
                                let p = unsafe { &*(ok.object.as_ptr() as *const Process) };
                                assert!(p.magic_ok(), "torn/UAF read: bad magic");
                                assert_eq!(p.pid(), pid, "object pid != owner pid");
                            }
                        }
                    }
                }
                iterations += 1;
                thread::yield_now();
            }
        }));
    }

    std::thread::sleep(std::time::Duration::from_millis(30));
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
}

/// **Headline TOCTOU test.** Workers hammer `duplicate(H)` on one long-lived
/// handle, closing each duplicate, while others `lookup(H)` and verify the
/// pinned object. `H` pins it throughout, so only the final close destroys it;
/// a use-after-free corrupts the sentinel, a refcount bug destroys early or
/// leaks.
#[test]
fn concurrent_duplicate_vs_close_toctou_torture() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    let t = Arc::new(fresh_table());
    test_probe::reset();
    let h0 = t.allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL | Rights::DUPLICATE).unwrap();
    let stop = Arc::new(AtomicBool::new(false));

    let mut workers = Vec::new();
    for _ in 0..3 {
        let t = Arc::clone(&t);
        let stop = Arc::clone(&stop);
        workers.push(thread::spawn(move || {
            let src = h0;
            test_probe::reset();
            while !stop.load(Ordering::Relaxed) {
                if let Ok(dup) = t.duplicate(src, 1, Rights::SIGNAL | Rights::DUPLICATE) {
                    if let Ok(ok) = t.lookup(dup, 1, Rights::SIGNAL) {
                        assert!(process_magic_ok(&ok.object), "UAF via duplicate");
                        drop(ok);
                    }
                    let _ = close_release(&t, dup, 1);
                }
            }
            test_probe::process_destroys()
        }));
    }
    for _ in 0..2 {
        let t = Arc::clone(&t);
        let stop = Arc::clone(&stop);
        workers.push(thread::spawn(move || {
            let src = h0;
            test_probe::reset();
            while !stop.load(Ordering::Relaxed) {
                if let Ok(ok) = t.lookup(src, 1, Rights::SIGNAL) {
                    assert!(process_magic_ok(&ok.object), "UAF via lookup");
                    drop(ok);
                }
            }
            test_probe::process_destroys()
        }));
    }

    std::thread::sleep(std::time::Duration::from_millis(40));
    stop.store(true, Ordering::Relaxed);
    let mut worker_destroys = 0usize;
    for w in workers {
        worker_destroys += w.join().unwrap();
    }
    assert_eq!(worker_destroys, 0, "object destroyed while H was held");
    let ok = t.lookup(h0, 1, Rights::SIGNAL).unwrap();
    assert!(process_magic_ok(&ok.object));
    drop(ok);
    test_probe::reset();
    close_release(&t, h0, 1).unwrap();
    assert_eq!(test_probe::process_destroys(), 1, "final destroy did not happen exactly once");
}

#[test]
fn close_then_allocate_drains_immediately_on_single_context() {
    let t = fresh_table();
    for i in 0..1024 {
        let h = t.allocate(1, mk_process(i), KObjectType::Process, sig()).unwrap();
        close_release(&t, h, 1).unwrap();
    }
    assert_eq!(t.allocated_count(), 0);
    let h = t.allocate(1, mk_process(0xFEED), KObjectType::Process, sig()).unwrap();
    close_release(&t, h, 1).unwrap();
}

/// The object `h`'s entry holds, read without a lookup.
fn entry_object(t: &HandleTable, h: RawHandle) -> *mut () {
    let (seg, slot, _) = h.decode();
    let entries = t.directory[seg as usize].load(Ordering::Acquire);
    // SAFETY: a published segment, which outlives the table.
    unsafe { (*entries)[slot as usize].object.load(Ordering::Acquire) }
}

/// **A closed handle's object outlives every lookup about to take a reference to it.**
fn a_closer_waits_for_a_lookup_about_to_take_its_reference(close: fn(&HandleTable, RawHandle)) {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::{Arc, Mutex};
    let t: &'static HandleTable = std::boxed::Box::leak(std::boxed::Box::new(fresh_table()));
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    let (freed, closer) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(None)));
    let (f, c) = (freed.clone(), closer.clone());
    BEFORE_ACQUIRE.with(|b| {
        b.set(Some(std::boxed::Box::new(move || {
            let f2 = f.clone();
            *c.lock().unwrap() = Some(std::thread::spawn(move || {
                close(t, h);
                f2.fetch_add(test_probe::process_destroys(), SeqCst);
            }));
            let start = std::time::Instant::now();
            while !entry_object(t, h).is_null() && start.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert_eq!(f.load(SeqCst), 0, "freed under a lookup about to take a reference");
        })))
    });
    let before = test_probe::process_destroys();
    assert_eq!(t.lookup(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle);
    closer.lock().unwrap().take().unwrap().join().unwrap();
    assert_eq!(freed.load(SeqCst) + test_probe::process_destroys() - before, 1, "destroyed once");
}

#[test]
fn a_close_waits_for_a_lookup_about_to_take_its_reference() {
    a_closer_waits_for_a_lookup_about_to_take_its_reference(|t, h| close_release(t, h, 1).unwrap());
}

#[test]
fn a_sweep_waits_for_a_lookup_about_to_take_its_reference() {
    a_closer_waits_for_a_lookup_about_to_take_its_reference(|t, _| assert_eq!(sweep_owned(t, 1), 1));
}

fn moveable() -> Rights {
    Rights::SIGNAL | Rights::TRANSFER
}

/// **A handle moves once**: a second claim of it fails until the first commits or drops.
#[test]
fn a_claimed_handle_cannot_be_claimed_again_and_commits_to_closed() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, moveable()).unwrap();
    let before = test_probe::process_destroys();
    let (ok, claim) = t.claim(h, 1, Rights::TRANSFER).unwrap();
    assert_eq!(t.claim(h, 1, Rights::TRANSFER).err(), Some(HandleError::InvalidHandle));
    assert!(t.lookup(h, 1, Rights::SIGNAL).is_ok(), "in use until the move commits");
    claim.commit();
    assert_eq!(t.lookup(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle);
    assert_eq!(test_probe::process_destroys(), before, "the move's reference keeps it");
    drop(ok);
    assert_eq!(test_probe::process_destroys(), before + 1);
}

#[test]
fn a_dropped_claim_gives_the_handle_back() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, moveable()).unwrap();
    drop(t.claim(h, 1, Rights::TRANSFER).unwrap());
    let (_ok, claim) = t.claim(h, 1, Rights::TRANSFER).unwrap();
    drop(claim);
    close_release(&t, h, 1).unwrap();
    assert_eq!(t.claim(h, 1, Rights::SIGNAL).err(), Some(HandleError::InvalidHandle));
}

/// A claim whose handle its owner closed, and whose slot went to another handle, leaves it alone.
#[test]
fn a_stale_claim_leaves_its_slots_next_handle_alone() {
    let t = fresh_table();
    for commit in [true, false] {
        let h = t.allocate(1, mk_process(1), KObjectType::Process, moveable()).unwrap();
        let (_ok, stale) = t.claim(h, 1, Rights::TRANSFER).unwrap();
        close_release(&t, h, 1).unwrap();
        let again = t.allocate(1, mk_process(2), KObjectType::Process, moveable()).unwrap();
        assert_eq!(again.decode().1, h.decode().1, "the slot reused");
        let (_ok, live) = t.claim(again, 1, Rights::TRANSFER).unwrap();
        if commit {
            stale.commit();
        } else {
            drop(stale);
        }
        assert_eq!(t.claim(again, 1, Rights::TRANSFER).err(), Some(HandleError::InvalidHandle), "{commit}");
        drop(live);
        close_release(&t, again, 1).unwrap();
    }
}

#[test]
fn a_claim_needs_the_rights_it_asks_for() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
    assert_eq!(t.claim(h, 1, Rights::TRANSFER).err(), Some(HandleError::NoAccess));
    assert_eq!(t.claim(h, 2, Rights::empty()).err(), Some(HandleError::NotOwner));
}
