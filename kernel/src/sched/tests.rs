// --- leave_online's decision half ------------------------------------------
//
// Separate tests, because the online set is per-thread under `cfg(test)` (see `online_set`):
// each case sets its own starting set. Against a shared global they passed only by being
// fast; forcing the interleaving fails `a_park_clears_its_own_bit_and_only_its_own`.

// --- the two preemption guards ------------------------------------------------
//
// Both are `debug_assert`s, testable on the host because the preempt counter is per-thread
// under `cfg(test)`. Each pair is positive-and-negative on purpose: the `should_panic` half
// alone would pass against a guard that fires unconditionally.

/// Raise the depth on *this thread's* counter and put it back, whatever the test does.
///
/// RAII because two tests panic while it is alive and cleanup after the assert would not
/// run. Nothing depends on that today (libtest gives every test a fresh OS thread), so it
/// is defensive against a harness that shares a thread.
struct PreemptRaised;
impl PreemptRaised {
    fn new() -> Self {
        super::with_preempt_off(super::SchedState::this_cpu(), |c| {
            c.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        });
        PreemptRaised
    }
}
impl Drop for PreemptRaised {
    fn drop(&mut self) {
        // `fetch_sub`, not `store(0)`: zeroing would make this fixture unusable inside a
        // held `SpinLock`, whose guard drop then reaches `preempt_enable` and trips its
        // underflow assert — reporting a wrap that never happened.
        super::with_preempt_off(super::SchedState::this_cpu(), |c| {
            c.fetch_sub(1, core::sync::atomic::Ordering::Relaxed)
        });
    }
}

#[test]
fn a_syscall_from_ring_3_with_preemption_enabled_is_accepted() {
    super::assert_user_entry_preempt_safe();
}

/// The ring-3 half: a kernel path that leaked a `preempt_disable` is caught at the next
/// syscall entry rather than silently stopping this CPU preempting.
#[test]
#[should_panic(expected = "entered a syscall from ring 3 with preemption disabled")]
fn a_syscall_from_ring_3_with_preemption_disabled_is_refused() {
    let _raised = PreemptRaised::new();
    super::assert_user_entry_preempt_safe();
}

#[test]
fn a_voluntary_switch_with_preemption_enabled_is_accepted() {
    super::assert_switch_preempt_safe();
}

/// The voluntary half: the involuntary paths latch into `RESCHED_PENDING`, so only the
/// voluntary ones need this check.
#[test]
#[should_panic(expected = "context switch with preemption disabled")]
fn a_voluntary_switch_inside_a_preempt_critical_window_is_refused() {
    let _raised = PreemptRaised::new();
    super::assert_switch_preempt_safe();
}

/// A permanent park must clear **its own** bit.
#[test]
fn a_park_clears_its_own_bit_and_only_its_own() {
    super::online_set::force(0b1011);
    super::clear_online_bit(true, 1);
    assert_eq!(super::online_mask(), 0b1001);
}

/// …and must clear **nobody's** when it has no identity of its own.
///
/// `current_cpu()` reports `0` for a CPU that never established an identity, so an
/// unguarded park clears the *running BSP's* bit — dropping it from the TLB-shootdown set,
/// leaving a freed frame reachable through a stale translation. The range check does not
/// catch it: `0` is in range.
#[test]
fn a_park_with_no_identity_clears_nobodys_bit() {
    super::online_set::force(0b1011);
    super::clear_online_bit(false, 0);
    assert_eq!(super::online_mask(), 0b1011, "an unbound CPU cleared someone else's bit");
}

/// An out-of-range index is ignored — asserted at **64**, not `MAX_CPUS`.
///
/// With `MAX_CPUS` = 8, deleting the range check leaves `0b1011 & !(1 << 8)` = `0b1011`, so
/// an assertion at `MAX_CPUS` passes for the broken implementation too. A shift only
/// misbehaves at the type's width: `1u64 << 64` panics in debug and wraps to `1 << 0` in
/// release, clearing **CPU 0's** bit.
#[test]
fn a_park_with_an_out_of_range_index_clears_nobodys_bit() {
    super::online_set::force(0b1011);
    super::clear_online_bit(true, 64);
    assert_eq!(super::online_mask(), 0b1011, "an out-of-range index cleared a bit");
}

use super::*;
use crate::mm::test_support::init_global_heap;
use crate::object::header::test_probe;

// These tests exercise run-queue bookkeeping and refcounts at the data-structure level; they
// do NOT call `context_switch` or run a thread body (validated only under QEMU).

extern "C" fn noop(_arg: usize) {}

/// Build an inert Thread ObjectRef without a kernel stack (no host paging needed).
fn inert_ref(tid: u32) -> ObjectRef {
    into_objref(Thread::try_new(tid, 0).unwrap())
}

/// A fresh `SchedState` for data-structure tests (no idle thread; the refs are inert).
fn test_state() -> SchedState {
    SchedState {
        ready: [const { Reserved::unreserved() }; MAX_CPUS],
        cpu_online: {
            let mut o = [false; MAX_CPUS];
            o[0] = true;
            o
        },
        current: [const { None }; MAX_CPUS],
        reap: [const { Reserved::unreserved() }; MAX_CPUS],
        suspended: Reserved::unreserved(),
        next_tid: 1,
        next_pid: 2,
        quantum: [QUANTUM_TICKS; MAX_CPUS],
        min_vruntime: [0; MAX_CPUS],
        idle: [const { None }; MAX_CPUS],
        idle_addr: [0; MAX_CPUS],
        blocked: Reserved::unreserved(),
        deadlines: Reserved::unreserved(),
        deadline_slots: [0; 3],
        stats: [stats::Counters::ZERO; MAX_CPUS],
        deferred_drops: Reserved::unreserved(),
        ended_pids: Reserved::unreserved(),
        reaper: None,
        threads: 0,
    }
}

#[test]
fn dequeue_front_is_round_robin() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0] = thread_list().unwrap();
    for tid in 1..=3 {
        assert!(st.ready[0].push(inert_ref(tid)).is_ok());
    }
    let a = dequeue_front(&mut st).unwrap();
    // SAFETY: pinned, single-threaded test.
    let a_tid = unsafe { &*(a.as_ptr() as *const Thread) }.tid();
    assert_eq!(a_tid, 1);
    assert!(st.ready[0].push(a).is_ok());
    let b = dequeue_front(&mut st).unwrap();
    // SAFETY: `b` pins a live `Thread`.
    let b_tid = unsafe { &*(b.as_ptr() as *const Thread) }.tid();
    assert_eq!(b_tid, 2, "round-robin must pick the next, not repeat");
    assert!(st.ready[0].push(b).is_ok());
}

#[test]
fn dequeue_front_empty_is_none() {
    init_global_heap();
    let mut st = test_state();
    assert!(dequeue_front(&mut st).is_none());
}

fn tid_of(r: &ObjectRef) -> u32 {
    // SAFETY: pinned, single-threaded test.
    unsafe { &*(r.as_ptr() as *const Thread) }.tid()
}

#[test]
fn nice_weight_and_delta_are_sane() {
    assert_eq!(nice_weight(0), NICE_0_WEIGHT);
    assert!(nice_weight(-20) > nice_weight(0));
    assert!(nice_weight(19) < nice_weight(0));
    assert_eq!(nice_weight(-128), nice_weight(-20));
    assert_eq!(nice_weight(127), nice_weight(19));
    assert_eq!(vruntime_delta(0), TICK_NS);
    assert!(vruntime_delta(-20) < vruntime_delta(0));
    assert!(vruntime_delta(19) > vruntime_delta(0));
}

#[test]
fn dequeue_prefers_realtime_by_priority_then_timeshared() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0] = thread_list().unwrap();
    let ts = inert_ref(1); // TimeShared (default), vruntime 0
    let rt_lo = inert_ref(2); // RealTime, priority 20
    let rt_hi = inert_ref(3); // RealTime, priority 5 (higher)
    // SAFETY: pinned, single-threaded test.
    unsafe {
        Thread::set_sched(rt_lo.as_ptr(), SchedClass::RealTime, 20, 0);
        Thread::set_sched(rt_hi.as_ptr(), SchedClass::RealTime, 5, 0);
    }
    assert!(st.ready[0].push(ts).is_ok());
    assert!(st.ready[0].push(rt_lo).is_ok());
    assert!(st.ready[0].push(rt_hi).is_ok());
    assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 3, "RealTime prio 5 first");
    assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 2, "RealTime prio 20 next");
    assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 1, "TimeShared last");
}

#[test]
fn dequeue_timeshared_picks_min_vruntime_and_advances_floor() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0] = thread_list().unwrap();
    let a = inert_ref(1);
    let b = inert_ref(2);
    let c = inert_ref(3);
    // SAFETY: pinned, single-threaded test.
    unsafe {
        Thread::set_vruntime(a.as_ptr(), 500);
        Thread::set_vruntime(b.as_ptr(), 100); // smallest
        Thread::set_vruntime(c.as_ptr(), 300);
    }
    assert!(st.ready[0].push(a).is_ok());
    assert!(st.ready[0].push(b).is_ok());
    assert!(st.ready[0].push(c).is_ok());
    assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 2, "smallest vruntime first");
    assert_eq!(st.min_vruntime[0], 100, "floor advanced to the picked vruntime");
    assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 3, "next-smallest vruntime");
    assert_eq!(st.min_vruntime[0], 300);
}

#[test]
fn seed_vruntime_floors_new_and_boosts_woken() {
    init_global_heap();
    let mut st = test_state();
    let base = 2 * TICK_NS;
    st.min_vruntime[0] = base;
    let n = inert_ref(1);
    seed_vruntime(&st, n.as_ptr(), 0, false);
    // SAFETY: pinned, single-threaded test.
    assert_eq!(unsafe { Thread::vruntime(n.as_ptr()) }, base);
    let w = inert_ref(2);
    seed_vruntime(&st, w.as_ptr(), 0, true);
    // SAFETY: `w` pins a live `Thread`, and the test is single-threaded.
    assert_eq!(unsafe { Thread::vruntime(w.as_ptr()) }, base - TICK_NS);
    let ahead = inert_ref(3);
    // SAFETY: `ahead` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_vruntime(ahead.as_ptr(), 3 * TICK_NS) };
    seed_vruntime(&st, ahead.as_ptr(), 0, false);
    // SAFETY: `ahead` pins a live `Thread`, and the test is single-threaded.
    assert_eq!(unsafe { Thread::vruntime(ahead.as_ptr()) }, 3 * TICK_NS);
}

/// Bring `n` CPUs online (0..n) with reserved ready queues, for placement tests.
fn online_n(st: &mut SchedState, n: usize) {
    for c in 0..n {
        st.cpu_online[c] = true;
        st.ready[c] = thread_list().unwrap();
    }
}

/// The online set a placement test passes in: `n` CPUs up, none parked. A value rather than
/// the `ONLINE_MASK` global — see [`cpu_accepts_work`].
fn all_up(n: usize) -> u64 {
    (1u64 << n) - 1
}

/// A CPU that has parked forever must not be given work — and its queue must still be
/// drainable, which is how the threads it already holds get rescued.
///
/// `leave_online` cannot clear `cpu_online[]`, so after a ring-0 fault parks an AP the two
/// disagree: placing on it hangs the thread, while draining its queue is the rescue.
/// **The steal half asserts against an emptied peer set on purpose**: were a thread left on
/// CPU 1, `steal_available` would be satisfied by *that* queue and the assertion would pass
/// even with the victim side wrongly gated. `steal_one` is checked too rather than inferred
/// from `steal_available`.
#[test]
fn a_parked_cpu_takes_no_new_work_but_can_still_be_drained() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 4);
    let t = inert_ref(1);

    assert!(st.ready[0].push(inert_ref(10)).is_ok());
    assert!(st.ready[1].push(inert_ref(11)).is_ok());
    assert_eq!(pick_target_cpu(&st, t.as_ptr(), all_up(4)), 2);

    // CPU 2 parks: gone from the online set, still `true` in `cpu_online[]`.
    let parked = all_up(4) & !(1u64 << 2);
    assert!(st.cpu_online[2], "the park cannot clear this, which is why the bug existed");
    assert_eq!(pick_target_cpu(&st, t.as_ptr(), parked), 3, "placed on a CPU that will never run");

    // A waking thread whose home is the parked CPU goes elsewhere too.
    // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_last_cpu(t.as_ptr(), 2) };
    assert_ne!(pick_wake_cpu(&st, t.as_ptr(), parked), 2, "woken onto a parked home CPU");

    // Rescue half: empty every other queue so the parked CPU is the *only* steal source.
    drop(st.ready[0].pop().unwrap());
    drop(st.ready[1].pop().unwrap());
    for c in 0..MAX_CPUS {
        assert!(st.ready[c].is_empty(), "cpu {c} must start this half empty");
    }
    assert!(st.ready[2].push(inert_ref(12)).is_ok());

    // Gating either path on `cpu_accepts_work` would strand the thread on CPU 2 forever.
    assert!(steal_available(&st, 0), "a parked CPU's queue must stay drainable, or this fix strands what it holds");
    let stolen = steal_one(&mut st).expect("steal_one must reach a parked CPU's queue");
    // SAFETY: `stolen` pins a live `Thread`.
    assert_eq!(unsafe { &*(stolen.as_ptr() as *const Thread) }.tid(), 12);
    assert!(st.ready[2].is_empty(), "the parked CPU's queue was not drained");
}

/// When affinity permits no CPU that accepts work, placement must still land somewhere that
/// will run the thread — **not** on CPU 0 because it happened to be the fallback (the hang
/// `cpu_accepts_work` exists to prevent, reachable when a ring-0 fault parks the BSP).
/// Asserted as "the chosen CPU accepts work" rather than an index, so the test states the
/// invariant instead of the current tie-break.
#[test]
fn placement_falls_back_to_a_cpu_that_accepts_work_not_to_cpu_zero() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 4);
    // Pinned to CPU 3; CPUs 0 and 3 have parked, so nothing permitted accepts work.
    let t = inert_ref(1);
    // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_cpu_mask(t.as_ptr(), 1 << 3) };
    let parked = all_up(4) & !(1u64 << 0) & !(1u64 << 3);

    let chosen = pick_target_cpu(&st, t.as_ptr(), parked);
    assert!(
        cpu_accepts_work(&st, chosen, parked),
        "fallback picked cpu {chosen}, which does not accept work — the thread would \
             never be scheduled"
    );

    // **The fallback is least-loaded, not first-found**: a CPU whose queue is full is not
    // runnable either (`place_thread` returns `Err`, the wake callers `panic!`), so filling
    // CPU 1 must push the choice to CPU 2.
    for i in 0..MAX_THREADS {
        assert!(st.ready[1].push(inert_ref(100 + i as u32)).is_ok());
    }
    let chosen = pick_target_cpu(&st, t.as_ptr(), parked);
    assert!(cpu_accepts_work(&st, chosen, parked), "fallback picked cpu {chosen}, which does not accept work");
    assert!(
        st.ready[chosen].len() < st.ready[chosen].capacity(),
        "fallback picked cpu {chosen}, whose queue is at reserve — place_thread would \
             return Err and the wake callers panic"
    );

    // The wake path delegates to the same fallback. (The affinity mask rejects home 0 before
    // `cpu_accepts_work` is reached, so this asserts the delegation and nothing more.)
    // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_last_cpu(t.as_ptr(), 0) };
    let woken = pick_wake_cpu(&st, t.as_ptr(), parked);
    assert!(cpu_accepts_work(&st, woken, parked), "wake fallback picked cpu {woken}, which does not accept work");
}

#[test]
fn placement_least_loaded_and_respects_affinity() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 4);
    let a = inert_ref(1);
    assert_eq!(pick_target_cpu(&st, a.as_ptr(), all_up(4)), 0);
    assert!(st.ready[0].push(inert_ref(10)).is_ok());
    assert!(st.ready[1].push(inert_ref(11)).is_ok());
    assert_eq!(pick_target_cpu(&st, a.as_ptr(), all_up(4)), 2);
    // SAFETY: `a` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_cpu_mask(a.as_ptr(), 1 << 3) };
    assert_eq!(pick_target_cpu(&st, a.as_ptr(), all_up(4)), 3);
    // Pinned to an offline CPU (5) → falls **outside** the mask, to the least-loaded CPU that
    // accepts work; stated as properties, not an index.
    // SAFETY: `a` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_cpu_mask(a.as_ptr(), 1 << 5) };
    let chosen = pick_target_cpu(&st, a.as_ptr(), all_up(4));
    assert!(cpu_accepts_work(&st, chosen, all_up(4)), "fallback chose cpu {chosen}");
    assert_eq!(st.ready[chosen].len(), 0, "fallback chose loaded cpu {chosen} while an empty one accepted work");
}

#[test]
fn wake_placement_prefers_home_cpu() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 4);
    let t = inert_ref(1);
    // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_last_cpu(t.as_ptr(), 2) };
    assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 2);
    // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_cpu_mask(t.as_ptr(), 1 << 0) };
    assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 0);
}

#[test]
fn stealable_respects_affinity() {
    init_global_heap();
    let t = inert_ref(1); // kernel thread (no user_entry), default mask = all
    // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
    unsafe {
        assert!(stealable_to(t.as_ptr(), 0));
        assert!(stealable_to(t.as_ptr(), 3));
        Thread::set_cpu_mask(t.as_ptr(), 1 << 0);
        assert!(stealable_to(t.as_ptr(), 0));
        assert!(!stealable_to(t.as_ptr(), 1));
    }
}

#[test]
fn wake_placement_falls_back_when_home_queue_is_full() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 2);
    let t = inert_ref(1);
    // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_last_cpu(t.as_ptr(), 0) };
    // Fill CPU 0's queue to its reserve: the home pick must divert (not a fatal wake).
    for tid in 100..(100 + MAX_THREADS as u32) {
        assert!(st.ready[0].push(inert_ref(tid)).is_ok());
    }
    assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 1);
    drop(st.ready[0].pop().unwrap());
    assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 0);
}

#[test]
fn steal_one_skips_busiest_victim_with_no_stealable_thread() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 3);
    for tid in 10..12 {
        let t = inert_ref(tid);
        // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
        unsafe { Thread::set_cpu_mask(t.as_ptr(), 1 << 1) };
        assert!(st.ready[1].push(t).is_ok());
    }
    assert!(st.ready[2].push(inert_ref(20)).is_ok());
    // Host `this_cpu()` is 0. `steal_one` must not return None when `steal_available` holds
    // (searching only the busiest victim would), which panics the idle-steal paths.
    assert!(steal_available(&st, 0));
    let stolen = steal_one(&mut st).expect("steal_one must succeed when steal_available");
    // SAFETY: `stolen` pins a live `Thread`.
    let stolen_tid = unsafe { &*(stolen.as_ptr() as *const Thread) }.tid();
    assert_eq!(stolen_tid, 20);
    assert!(st.ready[2].is_empty());
}

#[test]
fn steal_one_prefers_the_busiest_stealable_victim() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 3);
    assert!(st.ready[1].push(inert_ref(10)).is_ok());
    for tid in 20..22 {
        assert!(st.ready[2].push(inert_ref(tid)).is_ok());
    }
    let stolen = steal_one(&mut st).unwrap();
    // SAFETY: `stolen` pins a live `Thread`.
    let stolen_tid = unsafe { &*(stolen.as_ptr() as *const Thread) }.tid();
    assert_eq!(stolen_tid, 20);
    assert_eq!(st.ready[2].len(), 1);
    assert_eq!(st.ready[1].len(), 1);
}

#[test]
fn drain_pending_drops_moves_all_and_preserves_capacity() {
    init_global_heap();
    let mut st = test_state();
    st.reap[0] = thread_list().unwrap();
    st.deferred_drops = Reserved::with_capacity(DEFERRED_DROP_RESERVE).unwrap();
    let reap_cap = st.reap[0].capacity();
    let dd_cap = st.deferred_drops.capacity();
    for tid in 1..=3 {
        assert!(st.reap[0].push(inert_ref(tid)).is_ok());
    }
    for tid in 4..=5 {
        assert!(st.deferred_drops.push(inert_ref(tid)).is_ok());
    }
    let mut buf: [Option<ObjectRef>; MAX_THREADS + DEFERRED_DROP_RESERVE] =
        [const { None }; MAX_THREADS + DEFERRED_DROP_RESERVE];
    let n = drain_pending_drops(&mut st, &mut buf);
    assert_eq!(n, 5);
    assert!(st.reap[0].is_empty());
    assert!(st.deferred_drops.is_empty());
    // The reserves survive the drain: `mem::take` would zero them and the next exit-path
    // push would allocate under `SCHED`.
    assert_eq!(st.reap[0].capacity(), reap_cap);
    assert_eq!(st.deferred_drops.capacity(), dd_cap);
    drop(buf); // the moved refs release their threads here (outside any lock)
}

// --- stats: the pure format step. The increment sites and `stats_snapshot` need a running
// scheduler (they read `this_cpu()`) and are validated under QEMU.

/// A snapshot with the given rows online (all other CPUs offline).
fn snap_with(rows: &[(usize, stats::CpuSnapshot)]) -> stats::Snapshot {
    let mut s = stats::Snapshot { cpus: [stats::CpuSnapshot::OFFLINE; MAX_CPUS], reaper_closed: 0, exit_closed: 0 };
    for &(c, row) in rows {
        s.cpus[c] = row;
    }
    s
}

#[test]
fn stats_format_renders_header_and_one_row_per_online_cpu() {
    init_global_heap();
    let snap = snap_with(&[
        (
            0,
            stats::CpuSnapshot {
                online: true,
                idle: false,
                ready: 1,
                counters: stats::Counters { switches: 1342, steals: 3, placed: 57, resched_ipis: 12, ticks: 4096 },
            },
        ),
        (
            1,
            stats::CpuSnapshot {
                online: true,
                idle: true,
                ready: 0,
                counters: stats::Counters { switches: 987, steals: 11, placed: 40, resched_ipis: 9, ticks: 4080 },
            },
        ),
    ]);
    assert_eq!(snap.cpus_online(), 2);
    let text = stats::format(&snap).unwrap();
    assert_eq!(
        text.as_str(),
        "cpus_online=2\n\
             reaper_closed=0\n\
             exit_closed=0\n\
             cpu=0 online=1 switches=1342 steals=3 placed=57 ipis=12 ticks=4096 ready=1 idle=0\n\
             cpu=1 online=1 switches=987 steals=11 placed=40 ipis=9 ticks=4080 ready=0 idle=1\n"
    );
}

#[test]
fn stats_format_omits_offline_cpus_preserving_indices() {
    init_global_heap();
    let row = stats::CpuSnapshot { online: true, idle: true, ready: 0, counters: stats::Counters::ZERO };
    let snap = snap_with(&[(0, row), (2, row)]);
    let text = stats::format(&snap).unwrap();
    assert_eq!(
        text.as_str(),
        "cpus_online=2\n\
             reaper_closed=0\n\
             exit_closed=0\n\
             cpu=0 online=1 switches=0 steals=0 placed=0 ipis=0 ticks=0 ready=0 idle=1\n\
             cpu=2 online=1 switches=0 steals=0 placed=0 ipis=0 ticks=0 ready=0 idle=1\n"
    );
}

// --- Deadline min-heap (pure) -------------------------------------

fn entry(deadline_ns: u64, target: usize) -> deadline::Entry {
    deadline::Entry { deadline_ns, target, kind: deadline::DeadlineKind::Timer, channel: 0 }
}

mod timers;

fn new_timer() -> ObjectRef {
    let obj = KBox::into_raw(Timer::try_new().unwrap()).as_ptr() as *mut ();
    // SAFETY: `obj` carries the single creation reference.
    unsafe { ObjectRef::from_raw(obj, KObjectType::Timer) }
}

#[test]
fn tick_quantum_decrements_then_reschedules_on_expiry() {
    assert_eq!(tick_quantum(3, true), (2, false));
    assert_eq!(tick_quantum(2, true), (1, false));
    assert_eq!(tick_quantum(1, true), (QUANTUM_TICKS, true));
    // Expiry with nothing ready → reset but do NOT reschedule (switching to idle is churn).
    assert_eq!(tick_quantum(1, false), (QUANTUM_TICKS, false));
}

#[test]
fn tick_quantum_one_tick_quantum_reschedules_every_tick_when_ready() {
    assert_eq!(tick_quantum(QUANTUM_TICKS, true).1, QUANTUM_TICKS == 1);
}

#[test]
fn queue_drop_releases_every_thread_exactly_once() {
    init_global_heap();
    test_probe::reset();
    {
        let mut st = test_state();
        st.ready[0] = thread_list().unwrap();
        for tid in 1..=4 {
            assert!(st.ready[0].push(inert_ref(tid)).is_ok());
        }
        *st.cur_slot() = Some(inert_ref(5));
        st.reap[0] = thread_list().unwrap();
        assert!(st.reap[0].push(inert_ref(6)).is_ok());
        assert!(st.reap[0].push(inert_ref(7)).is_ok());
        assert_eq!(test_probe::thread_destroys(), 0);
    } // st dropped here — every ObjectRef drops its one reference.
    assert_eq!(test_probe::thread_destroys(), 7, "each queued/current/reaped thread destroyed exactly once",);
}

/// Build an inert Thread tagged with `owner_pid = pid`, enough for the teardown tests'
/// `pid` scans (no process, no kernel stack).
fn inert_user_ref(tid: u32, pid: u32) -> ObjectRef {
    into_objref(Thread::try_new(tid, pid).unwrap())
}

#[test]
fn resume_suspended_moves_to_ready_and_sets_disposition() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0] = thread_list().unwrap();
    st.suspended = thread_list().unwrap();
    let th = inert_user_ref(1, 1);
    let obj = th.as_ptr();
    // SAFETY: pinned, single-threaded test; mimic a suspended thread.
    unsafe { Thread::set_state(obj, ThreadState::Suspended) };
    assert!(st.suspended.push(th).is_ok());

    // `resume_suspended` takes the global SCHED, so mirror its list ops to stay lock-free.
    let i = st.suspended.iter().position(|r| r.as_ptr() == obj).unwrap();
    let r = st.suspended.remove(i);
    // SAFETY: pinned, single-threaded.
    unsafe {
        Thread::set_disposition(obj, 2, 7);
        Thread::set_state(obj, ThreadState::Ready);
    }
    assert!(st.ready[0].push(r).is_ok());

    assert!(st.suspended.is_empty());
    assert_eq!(st.ready[0].len(), 1);
    // SAFETY: pinned.
    let (tag, code) = unsafe { Thread::take_disposition(obj) };
    assert_eq!((tag, code), (2, 7));
}

/// **A suspended thread's registers are a copy of its fault's**, taken under `SCHED`: the
/// thread's next trap reuses its stack, so a read of the frame there, after the lock, could take
/// a resumed thread's live state instead. A thread not suspended has none to read.
#[test]
fn a_suspended_threads_registers_are_its_faults_until_it_resumes() {
    init_global_heap();
    let th = inert_user_ref(1, 1);
    let obj = th.as_ptr();
    let regs = crate::arch::RegisterValues { rip: 0x40_1000, ..Default::default() };
    // SAFETY: pinned, single-threaded; as `suspend_with_fault` records them.
    unsafe {
        Thread::set_fault_registers(obj, regs);
        Thread::set_state(obj, ThreadState::Suspended);
    }
    // SAFETY: pinned by `th`.
    let read = unsafe { suspended_registers(obj) };
    assert_eq!(read, Some(regs));
    // SAFETY: pinned. The resume.
    unsafe { Thread::set_state(obj, ThreadState::Ready) };
    // SAFETY: pinned by `th`.
    assert_eq!(unsafe { suspended_registers(obj) }, None);
}

#[test]
fn has_live_siblings_scans_all_parked_lists() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0] = thread_list().unwrap();
    st.blocked = thread_list().unwrap();
    st.suspended = thread_list().unwrap();

    assert!(st.ready[0].push(inert_user_ref(1, 1)).is_ok());
    assert!(st.suspended.push(inert_user_ref(2, 1)).is_ok());
    assert!(st.blocked.push(inert_user_ref(3, 3)).is_ok());

    assert!(has_live_siblings(&st, 1));
    assert!(has_live_siblings(&st, 3));
    assert!(!has_live_siblings(&st, 2), "no pid-2 thread is parked");
}

#[test]
fn a_process_ending_marks_only_its_own_threads_and_frees_none() {
    init_global_heap();
    test_probe::reset();
    let mut st = test_state();
    st.ready[0] = thread_list().unwrap();
    st.reap[0] = thread_list().unwrap();
    for (tid, pid) in [(1, 1), (2, 2), (3, 1)] {
        assert!(st.ready[0].push(inert_user_ref(tid, pid)).is_ok());
    }

    end_siblings(&mut st, 1);
    // SAFETY: each is pinned by `ready`; single-threaded.
    let marked: std::vec::Vec<bool> = st.ready[0].iter().map(|r| unsafe { Thread::is_ending(r.as_ptr()) }).collect();
    assert_eq!(marked, [true, false, true]);
    assert!(st.reap[0].is_empty(), "each ends itself; none is parked for reaping here");
    assert_eq!(test_probe::thread_destroys(), 0);
}

#[test]
fn reaped_thread_destroyed_when_dropped() {
    init_global_heap();
    test_probe::reset();
    let r = inert_ref(1);
    assert_eq!(test_probe::thread_destroys(), 0);
    drop(r); // simulates reap_pending dropping the parked thread
    assert_eq!(test_probe::thread_destroys(), 1);
}

#[test]
fn into_objref_preserves_single_reference() {
    init_global_heap();
    test_probe::reset();
    let r = into_objref(Thread::try_new(9, 0).unwrap());
    // SAFETY: pinned, single-threaded.
    let (_entry, arg) = unsafe { Thread::entry_and_arg(r.as_ptr()) };
    assert_eq!(arg, 0);
    drop(r);
    assert_eq!(test_probe::thread_destroys(), 1);
    let _ = noop; // silence dead-code in case the demo path is cfg'd out
}

// --- the thread cap ------------------------------------------------------------

/// A local state whose online CPU 0 holds everything the reserves allow, as `init` reserves.
fn reserved_state() -> SchedState {
    let mut st = test_state();
    st.ready[0] = thread_list().unwrap();
    st.blocked = thread_list().unwrap();
    st.suspended = thread_list().unwrap();
    st.reap[0] = thread_list().unwrap();
    st.deadlines = Reserved::with_capacity(deadline::HEAP_RESERVE).unwrap();
    st.deadline_slots = [deadline::HEAP_RESERVE / 4; 3];
    st
}

/// A wake, a block, a fault and an exit cannot refuse: every thread the cap admits, pinned to
/// CPU 0, faults, blocks, wakes there and is reaped there, and a spawn past the cap is refused.
#[test]
fn every_thread_the_cap_admits_can_fault_block_wake_and_be_reaped_on_one_cpu() {
    init_global_heap();
    super::online_set::force(0b11);
    let mut st = reserved_state();
    let pinned = |tid: usize| {
        let r = inert_user_ref(tid as u32, 7);
        // SAFETY: `r` pins the thread; single-threaded test.
        unsafe { Thread::set_cpu_mask(r.as_ptr(), 1) };
        r
    };
    for tid in 1..=MAX_THREADS {
        assert!(place_thread(&mut st, pinned(tid), false).is_ok(), "thread {tid} refused");
    }
    st.cpu_online[1] = true;
    st.ready[1] = thread_list().unwrap();
    let elsewhere = inert_ref(0);
    // SAFETY: `elsewhere` pins the thread; single-threaded test.
    unsafe { Thread::set_cpu_mask(elsewhere.as_ptr(), 2) };
    assert!(place_thread(&mut st, elsewhere, false).is_err(), "a spawn past the cap, to a CPU with room");
    while let Some(r) = st.ready[0].pop() {
        push_reserved(&mut st.suspended, r, "suspended list");
    }
    while let Some(r) = st.suspended.pop() {
        // SAFETY: `r` pins the thread; single-threaded test.
        unsafe { Thread::set_state(r.as_ptr(), ThreadState::Blocked) };
        push_reserved(&mut st.blocked, r, "blocked list");
    }
    let blocked: std::vec::Vec<_> = st.blocked.iter().map(|r| r.as_ptr()).collect();
    for obj in blocked {
        assert!(make_runnable(&mut st, obj));
    }
    // Each parks itself for reaping on the CPU it ends on, as `finish_exit` does.
    while let Some(r) = st.ready[0].pop() {
        push_reserved(&mut st.reap[0], r, "reap list (self)");
    }
    assert_eq!(st.reap[0].len(), MAX_THREADS);
}

#[test]
fn a_thread_counts_from_its_first_placement_until_its_reap_is_drained() {
    init_global_heap();
    super::online_set::force(0b1);
    let mut st = reserved_state();
    place_thread(&mut st, inert_ref(1), false).unwrap();
    let woken = st.ready[0].pop().unwrap();
    place_thread(&mut st, woken, true).unwrap();
    assert_eq!(st.threads, 1, "a wake is not a new thread");
    let exited = st.ready[0].pop().unwrap();
    assert!(st.reap[0].push(exited).is_ok());
    let mut buf = [const { None }; 4];
    assert_eq!(drain_pending_drops(&mut st, &mut buf), 1);
    assert_eq!(st.threads, 0);
}

/// A blocked thread is woken to end itself, still registered on its objects and in the heap for its
/// own `wait_on` to undo; a suspended one resumes to terminate; the CPU running one is told.
#[test]
fn a_process_ending_wakes_its_blocked_thread_resumes_its_suspended_one_and_tells_the_cpu_running_one() {
    init_global_heap();
    super::online_set::force(0b11);
    let mut st = reserved_state();
    st.cpu_online[1] = true;
    st.ready[1] = thread_list().unwrap();
    let t1 = new_timer();
    let blocked = inert_user_ref(1, 7);
    let other = inert_user_ref(2, 8);
    for r in [&blocked, &other] {
        // SAFETY: every object here is pinned by the test; single-threaded.
        unsafe {
            Thread::wait_register(r.as_ptr(), &[t1.as_ptr() as usize], true);
            Timer::add_waiter(t1.as_ptr(), r.as_ptr()).unwrap();
            Thread::set_state(r.as_ptr(), ThreadState::Blocked);
        }
    }
    let (b, o) = (blocked.as_ptr(), other.as_ptr());
    let thread_deadline = deadline::Entry { kind: deadline::DeadlineKind::Thread, ..entry(5_000, b as usize) };
    deadline::push(&mut st.deadlines, thread_deadline).unwrap();
    assert!(st.blocked.push(blocked).is_ok());
    assert!(st.blocked.push(other).is_ok());
    let suspended = inert_user_ref(3, 7);
    let s3 = suspended.as_ptr();
    // SAFETY: pinned by the test; single-threaded.
    unsafe { Thread::set_state(s3, ThreadState::Suspended) };
    assert!(st.suspended.push(suspended).is_ok());
    let running = inert_user_ref(4, 7);
    let r4 = running.as_ptr();
    st.current[1] = Some(running);

    end_siblings(&mut st, 7);

    // SAFETY: each thread is pinned by a scheduler list; single-threaded.
    unsafe {
        assert!(Thread::is_ending(b) && Thread::is_ending(s3) && Thread::is_ending(r4));
        assert!(!Thread::is_ending(o), "another process's thread");
        assert_eq!(Thread::take_disposition(s3), (DISP_TERMINATE, 0));
    }
    assert_eq!(st.blocked.len(), 1);
    assert_eq!(st.blocked[0].as_ptr(), o, "the other process's thread stays blocked");
    assert!(st.suspended.is_empty());
    let ready: std::vec::Vec<_> = st.ready.iter().flat_map(|q| q.iter().map(|r| r.as_ptr())).collect();
    assert!(ready.contains(&b) && ready.contains(&s3), "woken and resumed to run");
    assert!(with_ending(1, |e| e.load(Ordering::Acquire)), "CPU 1 ends its running thread on the way out");
    assert!(st.deadlines.iter().any(|e| e.target == b as usize), "left for its own wait_on to remove");
    assert!(st.reap.iter().all(|q| q.is_empty()));
    // SAFETY: pinned by `t1`; single-threaded. What each thread's own `wait_on` would undo.
    assert_eq!(unsafe { Timer::take_waiters(t1.as_ptr()) }.len(), 2, "both still registered");
}

/// The last thread out of a process, however it left, hands the process's handles to the reaper
/// and wakes it, and its parent hears the status a `sys_process_exit` recorded, not its own.
#[test]
fn the_last_thread_out_of_a_process_wakes_the_reaper_with_the_status_its_exit_recorded() {
    use crate::object::thread::test_support::{user_process_ref, user_thread_no_stack};
    init_global_heap();
    super::online_set::force(0b1);
    let mut st = reserved_state();
    st.ended_pids = Reserved::with_capacity(4).unwrap();
    st.reaper = Some(inert_ref(99));
    let (process, _root) = user_process_ref(5);
    let chan = KBox::into_raw(NotificationChannel::try_new().unwrap()).as_ptr() as *mut ();
    // SAFETY: `into_raw` yielded the single creation reference; adopt it.
    let parent = unsafe { ObjectRef::from_raw(chan, KObjectType::NotificationChannel) };
    let recorded = ExitStatus { kind: ExitKind::Normal as u32, code: 3 };
    {
        let p = process.as_ptr() as *mut crate::object::Process;
        // SAFETY: the test's own Process, not yet shared with anything that reads it.
        unsafe { (*p).set_parent_notif(parent.clone()) };
        // SAFETY: as above.
        unsafe { &*p }.begin_ending(recorded);
    }
    let me = into_objref(user_thread_no_stack(1, process.clone(), 0x40_0000, 0x7fff_f000));
    let own = ExitStatus { kind: ExitKind::Crashed as u32, code: 14 };
    assert!(st.blocked.push(inert_user_ref(2, 5)).is_ok());

    assert!(!end_process_if_last(&mut st, me.as_ptr(), own), "a sibling is still live");
    assert!(st.ended_pids.is_empty() && st.reaper.is_some());

    let sibling = st.blocked.pop().unwrap();
    assert!(end_process_if_last(&mut st, me.as_ptr(), own));
    assert_eq!(st.ended_pids.iter().copied().collect::<std::vec::Vec<_>>(), [5]);
    assert!(st.reaper.is_none(), "the reaper was woken");
    // SAFETY: pinned by `me`; single-threaded.
    assert!(unsafe { Thread::process_ended(me.as_ptr()) });
    // SAFETY: the parent channel is pinned by `parent`; single-threaded.
    let heard = unsafe { NotificationChannel::try_recv(parent.as_ptr()) }.expect("ChildExited");
    assert_eq!(heard.as_bytes(), Notification::child_exited(5, recorded).as_bytes());
    drop((sibling, me));
}

/// A marked thread's `wait_on` answers `Ended` at once; a wait for I/O in flight heeds no mark.
#[test]
fn a_thread_its_process_marked_waits_for_nothing_but_its_io() {
    init_global_heap();
    // SAFETY: no objects, so nothing to pin.
    let (heeded, io) = with_current(an_ending_thread(), || unsafe { (wait_on(&[], 1, 0), wait_on_io(&[], 0, 0)) });
    assert!(matches!(heeded, WaitResult::Ended));
    assert!(matches!(io, WaitResult::TimedOut), "a poll for I/O answers as a poll");
}

/// A marked thread terminates at a fault instead of parking where nothing would resume it.
#[test]
fn a_thread_its_process_marked_terminates_at_a_fault_instead_of_suspending() {
    init_global_heap();
    let disposition = with_current(an_ending_thread(), || {
        suspend_with_fault(Default::default(), Notification::divide_by_zero(1, 0x10))
    });
    assert!(matches!(disposition, ResumeDisposition::Terminate(0)));
}

#[test]
fn a_thread_made_for_a_process_that_has_begun_to_exit_is_placed_marked() {
    use crate::object::thread::test_support::{user_process_ref, user_thread_no_stack};
    init_global_heap();
    super::online_set::force(0b1);
    let mut st = reserved_state();
    let (exiting, _) = user_process_ref(5);
    // SAFETY: `exiting` pins the test's own `Process`.
    unsafe { &*(exiting.as_ptr() as *const crate::object::Process) }
        .begin_ending(ExitStatus { kind: ExitKind::Normal as u32, code: 0 });
    let (running, _) = user_process_ref(6);
    for (process, marked) in [(exiting, true), (running, false)] {
        let thread = into_objref(user_thread_no_stack(1, process, 0x40_0000, 0x7fff_f000));
        let obj = thread.as_ptr();
        assert!(place_spawned(&mut st, thread).is_ok());
        // SAFETY: pinned by its run queue; single-threaded.
        assert_eq!(unsafe { Thread::is_ending(obj) }, marked);
    }
}

/// **The calling thread's identity is its pid, then its tid**; a kernel thread has none.
#[test]
fn the_current_identity_is_its_pid_then_its_tid() {
    use crate::object::thread::test_support::{user_process_ref, user_thread_no_stack};
    init_global_heap();
    let (process, _root) = user_process_ref(9);
    let me = into_objref(user_thread_no_stack(7, process, 0x40_0000, 0x7fff_f000));
    assert_eq!(with_current(me, current_pid_tid), Some((9, 7)));
    assert_eq!(with_current(inert_ref(3), current_pid_tid), None);
}

/// **A fault is reported unwaited only when its notification wakes no thread**: a supervisor
/// blocked on the channel is woken, wherever it then runs; a channel with no waiter, or no channel,
/// wakes nobody. The notification is queued either way, for whoever reads it.
#[test]
fn a_fault_wakes_the_thread_waiting_on_its_channel_or_none() {
    use crate::libkern::notification::FaultKind;
    init_global_heap();
    super::online_set::force(0b1);
    let mut st = reserved_state();
    let chan = KBox::into_raw(NotificationChannel::try_new().unwrap()).as_ptr() as *mut ();
    // SAFETY: `into_raw` yielded the single creation reference; adopt it.
    let _pinned = unsafe { ObjectRef::from_raw(chan, KObjectType::NotificationChannel) };
    let notif = Notification::seg_fault(7, 0xdead_0000, FaultKind::NotWritable);
    // SAFETY: the test's own live channel, its queue room reserved by `try_new`; single-threaded.
    unsafe {
        assert!(!deliver_fault(&mut st, None, notif), "no channel");
        assert!(!deliver_fault(&mut st, Some(chan), notif), "no thread waiting");
    }
    let supervisor = inert_user_ref(1, 7);
    let s = supervisor.as_ptr();
    // SAFETY: pinned by the test; single-threaded.
    unsafe {
        Thread::wait_register(s, &[chan as usize], true);
        NotificationChannel::add_waiter(chan, s).unwrap();
        Thread::set_state(s, ThreadState::Blocked);
    }
    assert!(st.blocked.push(supervisor).is_ok());
    // SAFETY: as above.
    assert!(unsafe { deliver_fault(&mut st, Some(chan), notif) }, "the supervisor was waiting");
    assert!(st.ready.iter().any(|q| q.iter().any(|r| r.as_ptr() == s)), "and is woken to run");
    for _ in 0..2 {
        // SAFETY: as above.
        assert!(unsafe { NotificationChannel::try_recv(chan) }.is_some(), "each fault was queued");
    }
}
