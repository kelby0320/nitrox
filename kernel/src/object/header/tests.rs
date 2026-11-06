use super::*;
use crate::mm::test_support::init_global_heap;

fn fresh_process(pid: u32) -> ObjectRef {
    init_global_heap();
    ObjectRef::new(Process::try_new(pid).unwrap())
}

fn header_of(obj: &ObjectRef) -> &KObjectHeader {
    obj.header()
}

/// The tags of the types `$t` that `$obj` is.
macro_rules! tags_matching {
    ($obj:expr, $($t:ident),*) => {{
        let mut matching = std::vec::Vec::new();
        $(if $obj.is::<$t>() { matching.push($t::TYPE) })*
        matching
    }};
}

/// One object of each kind a host test makes without a driver or a server behind it.
fn one_of_each() -> std::vec::Vec<ObjectRef> {
    init_global_heap();
    let (a, b) = IpcChannel::try_new_pair(1).unwrap();
    std::vec![
        ObjectRef::new(Process::try_new(1).unwrap()),
        ObjectRef::new(Thread::try_new(1, 1).unwrap()),
        ObjectRef::new(MemoryObject::try_new(4096).unwrap()),
        ObjectRef::new(Timer::try_new().unwrap()),
        ObjectRef::new(NotificationChannel::try_new().unwrap()),
        ObjectRef::new(a),
        ObjectRef::new(b),
        ObjectRef::new(PendingOperation::try_new().unwrap()),
        ObjectRef::new(Namespace::try_new().unwrap()),
        ObjectRef::new(EntropyObject::try_new().unwrap()),
        ObjectRef::new(InterruptObject::try_new().unwrap()),
        ObjectRef::new(SystemControl::try_new().unwrap()),
    ]
}

#[test]
fn a_fresh_header_is_tagged_by_its_type_with_refcount_one() {
    let obj = fresh_process(7);
    assert_eq!(header_of(&obj).refcount(), 1);
    assert_eq!(header_of(&obj).object_type(), KObjectType::Process);
    assert_eq!(obj.object_type(), Process::TYPE);
}

#[test]
fn each_object_is_its_own_kind_and_no_other() {
    for obj in one_of_each() {
        let tags = tags_matching!(
            obj,
            Process,
            Thread,
            MemoryObject,
            Timer,
            NotificationChannel,
            IpcChannel,
            PendingOperation,
            Namespace,
            EntropyObject,
            DeviceNode,
            InterruptObject,
            UserspaceServerReg,
            FileObject,
            SystemControl
        );
        assert_eq!(tags, [obj.object_type()], "the kinds a {:?} downcasts to", obj.object_type());
    }
}

#[test]
fn a_downcast_to_another_kind_is_none_and_gives_the_reference_back() {
    test_probe::reset();
    let obj = fresh_process(3);
    assert!(obj.downcast_ref::<MemoryObject>().is_none());
    assert_eq!(obj.downcast_ref::<Process>().map(Process::pid), Some(3));
    let obj = obj.downcast::<Thread>().expect_err("a Process is not a Thread");
    assert_eq!(header_of(&obj).refcount(), 1, "the failed downcast kept its one reference");
    let p = obj.downcast::<Process>().expect("a Process");
    assert_eq!(p.pid(), 3);
    let erased = p.clone().erase();
    assert_eq!(erased.object_type(), KObjectType::Process);
    drop((p, erased));
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn a_typed_reference_of_an_unshared_kind_still_counts() {
    init_global_heap();
    test_probe::reset();
    let t = Ref::new(Thread::try_new(4, 1).unwrap());
    let t2 = t.clone();
    assert_eq!(t.as_ptr(), t2.as_ptr());
    drop(t);
    assert_eq!(test_probe::thread_destroys(), 0);
    drop(t2);
    assert_eq!(test_probe::thread_destroys(), 1);
}

#[test]
fn try_acquire_increments_above_zero() {
    let obj = fresh_process(1);
    // SAFETY: `obj` keeps the object live across the call.
    let second = unsafe { ObjectRef::try_acquire(obj.as_ptr()) }.expect("count above zero");
    assert_eq!(header_of(&obj).refcount(), 2);
    drop(second);
    assert_eq!(header_of(&obj).refcount(), 1);
}

#[test]
fn a_typed_acquire_counts_as_the_untyped_one() {
    init_global_heap();
    let p = Ref::new(Process::try_new(5).unwrap());
    // SAFETY: `p` keeps the `Process` live across the call.
    let q = unsafe { Ref::try_acquire(p.as_ptr()) }.expect("count above zero");
    assert_eq!((q.pid(), header_of(p.as_object()).refcount()), (5, 2));
}

#[test]
fn try_acquire_fails_at_zero() {
    init_global_heap();
    let raw = KBox::into_raw(Process::try_new(1).unwrap());
    // SAFETY: the box was just given up; nothing else holds it.
    let header = unsafe { &*(raw.as_ptr() as *const KObjectHeader) };
    // Release the creation ref via the header: the count reaches zero without a destroy.
    assert!(header.release());
    assert!(!header.try_acquire());
    assert_eq!(header.refcount(), 0);
    // SAFETY: the count is zero and nothing else holds the storage, which came from a `KBox`.
    drop(unsafe { KBox::from_raw(raw) });
}

#[test]
fn release_reports_last_ref_exactly_once() {
    init_global_heap();
    let raw = KBox::into_raw(Process::try_new(1).unwrap());
    // SAFETY: the box was just given up; it is freed only after the last read below.
    let header = unsafe { &*(raw.as_ptr() as *const KObjectHeader) };
    for _ in 0..3 {
        assert!(header.try_acquire());
    }
    assert!(!header.release());
    assert!(!header.release());
    assert!(!header.release());
    assert!(header.release());
    // SAFETY: the count is zero and nothing else holds the storage, which came from a `KBox`.
    drop(unsafe { KBox::from_raw(raw) });
}

#[test]
fn objectref_clone_then_drop_balances_and_destroys_once() {
    test_probe::reset();
    let r = fresh_process(1);
    let r2 = r.clone();
    assert_eq!(header_of(&r).refcount(), 2);
    drop(r);
    assert_eq!(test_probe::process_destroys(), 0);
    drop(r2);
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn drop_runs_correct_destructor_per_type() {
    test_probe::reset();
    let objects = one_of_each();
    let kinds: std::vec::Vec<KObjectType> = objects.iter().map(ObjectRef::object_type).collect();
    drop(objects);
    for ty in kinds.iter().copied() {
        let made = kinds.iter().filter(|&&k| k == ty).count();
        assert_eq!(test_probe::destroys(ty), made, "{ty:?}'s arm ran once for each made");
    }
}

#[test]
fn into_raw_then_from_raw_no_double_decrement() {
    test_probe::reset();
    let r = fresh_process(1);
    let (ptr, ty) = r.into_raw();
    assert_eq!(ty, KObjectType::Process);
    assert_eq!(test_probe::process_destroys(), 0);
    // SAFETY: re-adopts the reference `into_raw` gave up.
    let r2 = unsafe { ObjectRef::from_raw(ptr) };
    assert_eq!(header_of(&r2).refcount(), 1);
    drop(r2);
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
#[should_panic(expected = "refcount overflow")]
fn refcount_overflow_guard_panics() {
    let obj = fresh_process(1);
    header_of(&obj).refcount.store(MAX_REFCOUNT + 1, Ordering::Relaxed);
    header_of(&obj).bump();
}
