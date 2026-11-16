//! The numbers: dense, in family order, each reaching its call, and none past the last served.

use super::*;

#[test]
fn unknown_number_is_unsupported() {
    assert_eq!(dispatch(0xDEAD, 0, 0, 0, 0, 0, 0), KError::Unsupported.as_isize());
}

/// **A vacant number answers `Unsupported`**, before it reads an argument, and to the arguments
/// each call refuses otherwise: so a vacant number served by any call fails here.
#[test]
fn a_number_past_the_last_is_unsupported_before_it_reads_an_argument() {
    as_a_caller(|| {
        for vacant in (SYSCALLS..=64).chain([SYS_DEBUG_KPRINT - 1, SYS_DEBUG_KPRINT + 1, SYS_DEBUG_KPRINT + 3]) {
            let refused = dispatch(vacant, 0xDEAD, USER_VIRT_END, 0, 0, 0, 0);
            assert_eq!(refused, KError::Unsupported.as_isize(), "number {vacant}");
            for nr in 0..SYSCALLS {
                let [a0, a1, a2, a3, a4, a5] = refused_by(nr);
                let refused = dispatch(vacant, a0, a1, a2, a3, a4, a5);
                assert_eq!(refused, KError::Unsupported.as_isize(), "number {vacant}, given {nr}'s arguments");
            }
        }
    });
}

/// **Every number below `SYSCALLS` reaches its call**: a dropped or misrouted arm would answer
/// `Unsupported`, which `sys_process_terminate` also answers for a target with nowhere to say so.
/// The two exits do not return, and are left out.
#[test]
fn every_number_below_the_last_reaches_a_call() {
    let exits = [SYS_PROCESS_EXIT, SYS_THREAD_EXIT];
    let answers: Vec<(u64, isize)> = as_a_caller(|| {
        let numbers = (0..SYSCALLS).filter(|n| !exits.contains(n));
        let answer = |nr| {
            let [a0, a1, a2, a3, a4, a5] = refused_by(nr);
            (nr, dispatch(nr, a0, a1, a2, a3, a4, a5))
        };
        numbers.map(answer).collect()
    });
    assert_eq!(answers.len() as u64, SYSCALLS - 2);
    for (nr, answer) in answers {
        assert!(answer < 0 && answer != KError::Unsupported.as_isize(), "number {nr} answered {answer}");
    }
}

/// What a call is handed to refuse it before it acts: a handle no table holds and addresses past
/// user space, or, for `sys_exception_resume`, whose disposition comes first, a valid one.
fn refused_by(nr: u64) -> [u64; 6] {
    let end = USER_VIRT_END;
    if nr == SYS_EXCEPTION_RESUME { [u64::MAX, 0, 0, 0, 0, 0] } else { [u64::MAX, end, end, end, end, end] }
}

/// `f` as a thread of no process, with the global handle table there to refuse it.
fn as_a_caller<R>(f: impl FnOnce() -> R) -> R {
    init_global_heap();
    crate::handle::global::init_for_tests();
    crate::sched::with_current(crate::object::header::Ref::new(crate::object::Thread::try_new(0, 0).unwrap()), f)
}

#[test]
fn the_numbers_are_dense_and_in_family_order() {
    let family_order = [
        SYS_HANDLE_CLOSE,
        SYS_HANDLE_DUPLICATE,
        SYS_HANDLE_RESTRICT,
        SYS_HANDLE_STAT,
        SYS_MEMORY_CREATE,
        SYS_MEMORY_MAP,
        SYS_MEMORY_UNMAP,
        SYS_CLOCK_READ,
        SYS_TIMER_CREATE,
        SYS_TIMER_SET,
        SYS_WAIT,
        SYS_NOTIF_RECV,
        SYS_CHANNEL_CREATE,
        SYS_CHANNEL_SEND,
        SYS_CHANNEL_RECV,
        SYS_PROCESS_SPAWN,
        SYS_PROCESS_EXIT,
        SYS_PROCESS_TERMINATE,
        SYS_THREAD_EXIT,
        SYS_THREAD_CREATE,
        SYS_THREAD_GET_REGISTERS,
        SYS_EXCEPTION_RESUME,
        SYS_NS_CREATE,
        SYS_NS_LOOKUP,
        SYS_NS_BIND,
        SYS_NS_UNBIND,
        SYS_NS_ENUMERATE,
        SYS_IO_SUBMIT,
    ];
    assert!(family_order.iter().copied().eq(0..SYSCALLS));
}
