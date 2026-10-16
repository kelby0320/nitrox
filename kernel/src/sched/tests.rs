// --- leave_online's decision half ------------------------------------------
//
// **Three tests, not one.** They were a single `#[test]` because `ONLINE_MASK` was a
// process-global and separate tests run concurrently, so splitting them would have made
// them race — the old comment said exactly that. The set is now per-thread under
// `cfg(test)` (see `online_set`), so each case establishes its own starting set and fails
// on its own name.
//
// Measured rather than assumed, because the interesting part is how *weak* the natural
// signal is: split against the shared global these three passed **40 runs out of 40**.
// They would have been isolated by being fast, which is not isolation. Forcing the
// interleaving with sleeps — one test asserting 300 ms after its own write, another
// writing 100 ms in — fails `a_park_clears_its_own_bit_and_only_its_own` deterministically
// on the shared backing and passes on this one. Same shape, same result, as the mock `IF`
// flag in `libkern::spinlock`.

// --- the two preemption guards ------------------------------------------------
//
// Both are `debug_assert`s added by PR #203 (audit B.5c, B.5e), and the 2026-08-14 entry
// recorded that **neither had a host test**, giving the reason: `PREEMPT_OFF` was a
// process-global that concurrent host tests already touched through `SpinLock::lock`, so
// raising the counter to test a guard would have raised it under every other test running
// at that moment. That is fixed above — the counter is per-thread under `cfg(test)` — and
// these are the tests it was blocking.
//
// Each pair is positive-and-negative on purpose: the `should_panic` half alone would pass
// against a guard that fires unconditionally.

/// Raise the depth on *this thread's* counter and put it back, whatever the test does.
///
/// RAII rather than a bare increment-and-reset, because two of these tests panic while it
/// is alive: cleanup written *after* the assert would simply not run. Test builds unwind
/// (the `panic = "abort"` assertion in `sched.rs` is `cfg(not(test))`), so `Drop` runs
/// during the unwind and the restore is unconditional.
///
/// **Nothing depends on that today**, and an earlier version of this comment claimed
/// otherwise — that it is what keeps `--test-threads=1` correct, "where the whole suite
/// shares one thread". It does not: libtest spawns a **fresh OS thread per test at every
/// concurrency level**, so a `thread_local!` cannot carry from one test to the next and the
/// leak this is said to prevent cannot happen. Measured on rustc 1.95.0 — at
/// `--test-threads=1` two tests report `ThreadId(2)` and `ThreadId(3)` — and confirmed the
/// other way: delete this `Drop` body and the suite is still 640 green at
/// `--test-threads=1`. The fixture is defensive against a future harness or `catch_unwind`
/// wrapper that *does* share a thread, which is a fine reason to keep it and not a reason
/// to claim it is load-bearing.
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

/// B.5(c)'s ring-3 half: a kernel path that leaked a `preempt_disable` is caught at the
/// next syscall entry rather than silently stopping this CPU preempting.
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

/// B.5(c) proper: the involuntary paths latch into `RESCHED_PENDING`; nothing checked the
/// voluntary ones until PR #203, and nothing tested that check until this one.
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
/// This is the case that matters. `current_cpu()` reports `0` for a CPU that never
/// established an identity (x86's `IA32_TSC_AUX` reset default), so an unguarded park
/// clears the *running BSP's* bit — dropping it from the TLB-shootdown set, where the
/// slow path's self-IPI is the only thing that invalidates its own TLB. A freed frame
/// stays reachable through a stale translation. Note the range check does not catch it:
/// `0` is perfectly in range (PR #198 review, blocking 1).
#[test]
fn a_park_with_no_identity_clears_nobodys_bit() {
    super::online_set::force(0b1011);
    super::clear_online_bit(false, 0);
    assert_eq!(super::online_mask(), 0b1011, "an unbound CPU cleared someone else's bit");
}

/// An out-of-range index is ignored — and this is asserted at **64**, not `MAX_CPUS`.
///
/// `MAX_CPUS` is 8, so deleting the range check leaves `0b1011 & !(1 << 8)` = `0b1011`
/// and an assertion at `MAX_CPUS` passes for the broken implementation too. A shift only
/// misbehaves at the width of the type: `1u64 << 64` panics in debug and wraps to
/// `1 << 0` in release — clearing **CPU 0's** bit, the same silent corruption the
/// identity guard exists to prevent. Caught by a reviewer's negative control.
#[test]
fn a_park_with_an_out_of_range_index_clears_nobodys_bit() {
    super::online_set::force(0b1011);
    super::clear_online_bit(true, 64);
    assert_eq!(super::online_mask(), 0b1011, "an out-of-range index cleared a bit");
}

use super::*;
use crate::mm::test_support::init_global_heap;
use crate::object::header::test_probe;

// These tests exercise the run-queue bookkeeping and refcount handling
// at the data-structure level. They do NOT call `context_switch`,
// `thread_trampoline`, or run a thread body — those manipulate real
// registers/stacks and are validated only under QEMU (the `xtask qemu`
// serial trace) until `xtask test-qemu` exists.

extern "C" fn noop(_arg: usize) {}

/// Build an inert Thread ObjectRef without a kernel stack (so no real
/// paging is needed on the host).
fn inert_ref(tid: u32) -> ObjectRef {
    into_objref(Thread::try_new(tid, 0).unwrap())
}

/// A fresh `SchedState` for data-structure tests (no idle thread; no real
/// paging needed since the refs are inert).
fn test_state() -> SchedState {
    SchedState {
        ready: [const { KVec::new() }; MAX_CPUS],
        cpu_online: {
            let mut o = [false; MAX_CPUS];
            o[0] = true;
            o
        },
        current: [const { None }; MAX_CPUS],
        reap: [const { KVec::new() }; MAX_CPUS],
        suspended: KVec::new(),
        next_tid: 1,
        next_pid: 2,
        quantum: [QUANTUM_TICKS; MAX_CPUS],
        min_vruntime: [0; MAX_CPUS],
        idle: [const { None }; MAX_CPUS],
        idle_addr: [0; MAX_CPUS],
        blocked: KVec::new(),
        deadlines: KVec::new(),
        stats: [stats::Counters::ZERO; MAX_CPUS],
        deferred_drops: KVec::new(),
        ended_pids: KVec::new(),
        reaper: None,
    }
}

#[test]
fn dequeue_front_is_round_robin() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0].try_reserve(READY_RESERVE).unwrap();
    for tid in 1..=3 {
        st.ready[0].try_push(inert_ref(tid)).unwrap();
    }
    // Dequeue front, re-enqueue at back: classic round-robin rotation.
    let a = dequeue_front(&mut st).unwrap();
    // SAFETY: pinned, single-threaded test.
    let a_tid = unsafe { &*(a.as_ptr() as *const Thread) }.tid();
    assert_eq!(a_tid, 1);
    st.ready[0].try_push(a).unwrap();
    let b = dequeue_front(&mut st).unwrap();
    // SAFETY: `b` pins a live `Thread`.
    let b_tid = unsafe { &*(b.as_ptr() as *const Thread) }.tid();
    assert_eq!(b_tid, 2, "round-robin must pick the next, not repeat");
    st.ready[0].try_push(b).unwrap();
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
    // Out-of-range nice clamps to the table ends.
    assert_eq!(nice_weight(-128), nice_weight(-20));
    assert_eq!(nice_weight(127), nice_weight(19));
    // nice 0 accrues exactly one slice per tick; lower nice accrues slower.
    assert_eq!(vruntime_delta(0), TICK_NS);
    assert!(vruntime_delta(-20) < vruntime_delta(0));
    assert!(vruntime_delta(19) > vruntime_delta(0));
}

#[test]
fn dequeue_prefers_realtime_by_priority_then_timeshared() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0].try_reserve(READY_RESERVE).unwrap();
    let ts = inert_ref(1); // TimeShared (default), vruntime 0
    let rt_lo = inert_ref(2); // RealTime, priority 20
    let rt_hi = inert_ref(3); // RealTime, priority 5 (higher)
    // SAFETY: pinned, single-threaded test.
    unsafe {
        Thread::set_sched(rt_lo.as_ptr(), SchedClass::RealTime, 20, 0);
        Thread::set_sched(rt_hi.as_ptr(), SchedClass::RealTime, 5, 0);
    }
    st.ready[0].try_push(ts).unwrap();
    st.ready[0].try_push(rt_lo).unwrap();
    st.ready[0].try_push(rt_hi).unwrap();
    // RealTime beats TimeShared; lower rt_priority value wins within RealTime.
    assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 3, "RealTime prio 5 first");
    assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 2, "RealTime prio 20 next");
    assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 1, "TimeShared last");
}

#[test]
fn dequeue_timeshared_picks_min_vruntime_and_advances_floor() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0].try_reserve(READY_RESERVE).unwrap();
    let a = inert_ref(1);
    let b = inert_ref(2);
    let c = inert_ref(3);
    // SAFETY: pinned, single-threaded test.
    unsafe {
        Thread::set_vruntime(a.as_ptr(), 500);
        Thread::set_vruntime(b.as_ptr(), 100); // smallest
        Thread::set_vruntime(c.as_ptr(), 300);
    }
    st.ready[0].try_push(a).unwrap();
    st.ready[0].try_push(b).unwrap();
    st.ready[0].try_push(c).unwrap();
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
    // A new TimeShared thread (vruntime 0) jumps up to the floor.
    let n = inert_ref(1);
    seed_vruntime(&st, n.as_ptr(), 0, false);
    // SAFETY: pinned, single-threaded test.
    assert_eq!(unsafe { Thread::vruntime(n.as_ptr()) }, base);
    // A waking thread is boosted to `floor - slice` (latency), not the bare floor.
    let w = inert_ref(2);
    seed_vruntime(&st, w.as_ptr(), 0, true);
    // SAFETY: `w` pins a live `Thread`, and the test is single-threaded.
    assert_eq!(unsafe { Thread::vruntime(w.as_ptr()) }, base - TICK_NS);
    // A thread already ahead of the floor keeps its (larger) vruntime.
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
        st.ready[c].try_reserve(READY_RESERVE).unwrap();
    }
}

/// The online set a placement test passes in: `n` CPUs up, none parked.
///
/// A value rather than the `ONLINE_MASK` global, deliberately — see
/// [`cpu_accepts_work`]. Writing the global here made these tests race each other.
fn all_up(n: usize) -> u64 {
    (1u64 << n) - 1
}

/// A CPU that has parked forever must not be given work — and its queue must still be
/// drainable, which is how the threads it already holds get rescued.
///
/// The two halves are the whole point. `leave_online` cannot clear `cpu_online[]` (that
/// needs `SCHED`, which the parking CPU may hold), so after a ring-0 fault parks an AP
/// the two disagree: placing on it hangs the thread forever, while draining its queue is
/// the rescue.
///
/// **The steal half asserts against an emptied peer set on purpose.** The first version
/// of this test left a thread on CPU 1, so `steal_available` was satisfied by *that*
/// queue and the assertion passed even with the victim side wrongly gated — the exact
/// "consistency cleanup" the doc comments warn against would have gone green
/// (PR #199 review, blocking 2). CPU 2 must be the only stealable queue for this to mean
/// anything, and `steal_one` is checked too rather than inferred from `steal_available`.
#[test]
fn a_parked_cpu_takes_no_new_work_but_can_still_be_drained() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 4);
    let t = inert_ref(1);

    // Load 0 and 1 so the least-loaded pick would otherwise land on CPU 2.
    st.ready[0].try_push(inert_ref(10)).unwrap();
    st.ready[1].try_push(inert_ref(11)).unwrap();
    assert_eq!(pick_target_cpu(&st, t.as_ptr(), all_up(4)), 2);

    // CPU 2 parks: gone from the online set, still `true` in `cpu_online[]`.
    let parked = all_up(4) & !(1u64 << 2);
    assert!(st.cpu_online[2], "the park cannot clear this, which is why the bug existed");
    assert_eq!(pick_target_cpu(&st, t.as_ptr(), parked), 3, "placed on a CPU that will never run");

    // A waking thread whose home is the parked CPU goes elsewhere too.
    // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_last_cpu(t.as_ptr(), 2) };
    assert_ne!(pick_wake_cpu(&st, t.as_ptr(), parked), 2, "woken onto a parked home CPU");

    // Now the rescue half. Empty every other queue so the parked CPU is the *only*
    // source a steal could come from — otherwise this passes without testing anything.
    drop(st.ready[0].pop().unwrap());
    drop(st.ready[1].pop().unwrap());
    for c in 0..MAX_CPUS {
        assert!(st.ready[c].is_empty(), "cpu {c} must start this half empty");
    }
    st.ready[2].try_push(inert_ref(12)).unwrap();

    // Host `this_cpu()` is 0. Both paths must reach a parked CPU's queue: gating either
    // on `cpu_accepts_work` strands the thread sitting on CPU 2 forever.
    assert!(steal_available(&st, 0), "a parked CPU's queue must stay drainable, or this fix strands what it holds");
    let stolen = steal_one(&mut st).expect("steal_one must reach a parked CPU's queue");
    // SAFETY: `stolen` pins a live `Thread`.
    assert_eq!(unsafe { &*(stolen.as_ptr() as *const Thread) }.tid(), 12);
    assert!(st.ready[2].is_empty(), "the parked CPU's queue was not drained");
}

/// When affinity permits no CPU that accepts work, placement must still land somewhere
/// that will run the thread — **not** on CPU 0 because it happened to be the fallback.
///
/// This is the hang `cpu_accepts_work` exists to prevent, reintroduced one line below it
/// by `if best == usize::MAX { 0 }` (kernel audit; decision log 2026-08-14). Reachable
/// when a ring-0 fault parks
/// the BSP while APs keep running and a pinned thread wakes.
///
/// Asserted as "the chosen CPU accepts work" rather than "the chosen CPU is 1 or 2", so
/// the test states the invariant instead of the current tie-break.
#[test]
fn placement_falls_back_to_a_cpu_that_accepts_work_not_to_cpu_zero() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 4);
    // Pinned to CPU 3; CPUs 0 and 3 have both parked. Nothing permitted accepts work,
    // and the old fallback answered 0 — which is parked.
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

    // **And the fallback is least-loaded, not first-found.** A CPU that accepts work but
    // whose queue is at `READY_RESERVE` is not runnable either: `place_thread` returns
    // `Err` and both wake callers turn that into a `panic!`. Filling CPU 1 must push the
    // choice to CPU 2 rather than onto the full queue beside it (PR #201 review,
    // finding 2).
    for i in 0..READY_RESERVE {
        st.ready[1].try_push(inert_ref(100 + i as u32)).unwrap();
    }
    let chosen = pick_target_cpu(&st, t.as_ptr(), parked);
    assert!(cpu_accepts_work(&st, chosen, parked), "fallback picked cpu {chosen}, which does not accept work");
    assert!(
        st.ready[chosen].len() < st.ready[chosen].capacity(),
        "fallback picked cpu {chosen}, whose queue is at reserve — place_thread would \
             return Err and the wake callers panic"
    );

    // The wake path delegates to the same fallback. (Its own `cpu_accepts_work` check on
    // the home CPU is covered by `a_parked_cpu_takes_no_new_work_but_can_still_be_drained`
    // — here the affinity mask rejects home 0 before that check is reached, so this
    // asserts the delegation and nothing more.)
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
    // No affinity → least-loaded. With all queues empty, CPU 0 (first min) wins.
    let a = inert_ref(1);
    assert_eq!(pick_target_cpu(&st, a.as_ptr(), all_up(4)), 0);
    // Load CPU 0 and 1; the least-loaded becomes CPU 2.
    st.ready[0].try_push(inert_ref(10)).unwrap();
    st.ready[1].try_push(inert_ref(11)).unwrap();
    assert_eq!(pick_target_cpu(&st, a.as_ptr(), all_up(4)), 2);
    // Pinned to CPU 3 → must target CPU 3 regardless of load.
    // SAFETY: `a` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_cpu_mask(a.as_ptr(), 1 << 3) };
    assert_eq!(pick_target_cpu(&st, a.as_ptr(), all_up(4)), 3);
    // Pinned to a CPU that is not online (5) → falls back **outside** the mask, to the
    // least-loaded CPU that accepts work. This asserted CPU 0 unconditionally until
    // 2026-08-14, when that turned out to be a hang rather than a defensive default:
    // once a CPU can park, "CPU 0" is not necessarily a CPU that runs anything. Stated
    // as the two properties rather than an index, so a tie-break change does not read
    // as a contract change.
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
    // Home CPU 2 (last ran there) + affinity allows it → wake returns CPU 2.
    // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
    unsafe { Thread::set_last_cpu(t.as_ptr(), 2) };
    assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 2);
    // Affinity now excludes the home → falls back to least-loaded (CPU 0).
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
        // Pin to CPU 0 only: stealable to 0, not to 1.
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
    // Fill CPU 0's queue to its reserve: the home pick must divert (F6 — a
    // full home queue used to be a fatal wake).
    for tid in 100..(100 + READY_RESERVE as u32) {
        st.ready[0].try_push(inert_ref(tid)).unwrap();
    }
    assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 1);
    // With room at home again, the cache-warm home pick returns.
    drop(st.ready[0].pop().unwrap());
    assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 0);
}

#[test]
fn steal_one_skips_busiest_victim_with_no_stealable_thread() {
    init_global_heap();
    let mut st = test_state();
    online_n(&mut st, 3);
    // CPU 1 (busiest, 2 threads) — both pinned to CPU 1: not stealable to 0.
    for tid in 10..12 {
        let t = inert_ref(tid);
        // SAFETY: `t` pins a live `Thread`, and the test is single-threaded.
        unsafe { Thread::set_cpu_mask(t.as_ptr(), 1 << 1) };
        st.ready[1].try_push(t).unwrap();
    }
    // CPU 2 (smaller, 1 thread) — default all-CPU mask: stealable.
    st.ready[2].try_push(inert_ref(20)).unwrap();
    // Host `this_cpu()` is 0. `steal_available` says true; the F4 bug was
    // `steal_one` returning None here (it searched only the busiest victim),
    // which panics the idle-steal paths on the empty idle slot.
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
    st.ready[1].try_push(inert_ref(10)).unwrap();
    for tid in 20..22 {
        st.ready[2].try_push(inert_ref(tid)).unwrap();
    }
    // Both victims have stealable threads; CPU 2 is busier — steal from it.
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
    st.reap[0].try_reserve(REAP_RESERVE).unwrap();
    st.deferred_drops.try_reserve(DEFERRED_DROP_RESERVE).unwrap();
    let reap_cap = st.reap[0].capacity();
    let dd_cap = st.deferred_drops.capacity();
    for tid in 1..=3 {
        st.reap[0].try_push(inert_ref(tid)).unwrap();
    }
    for tid in 4..=5 {
        st.deferred_drops.try_push(inert_ref(tid)).unwrap();
    }
    let mut buf: [Option<ObjectRef>; REAP_RESERVE + DEFERRED_DROP_RESERVE] =
        [const { None }; REAP_RESERVE + DEFERRED_DROP_RESERVE];
    let n = drain_pending_drops(&mut st, &mut buf);
    assert_eq!(n, 5);
    assert!(st.reap[0].is_empty());
    assert!(st.deferred_drops.is_empty());
    // The reserves survive the drain — the F11 hazard was `mem::take` zeroing
    // them, making the next exit-path push allocate under `SCHED`.
    assert_eq!(st.reap[0].capacity(), reap_cap);
    assert_eq!(st.deferred_drops.capacity(), dd_cap);
    drop(buf); // the moved refs release their threads here (outside any lock)
}

// --- stats: the pure format step of capture → format → synthesize. The
// increment sites and `stats_snapshot` need a running scheduler (they read
// `this_cpu()`) and are validated under QEMU (Part D's selftest gates the
// verdict on two-CPU activity).

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
    // CPUs 0 and 2 online, 1 offline: rows keep their dense indices.
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
    // Remove the middle target; ordering of the rest is preserved.
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
    // The same `target` can hold entries of different kinds — each removable
    // independently by `(target, kind)`.
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
    // Neither is a Timer.
    assert!(!deadline::remove(&mut h, 0xA, deadline::DeadlineKind::Timer));
    // Remove the Thread entry; the PendingSend one survives, channel intact.
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
fn tick_quantum_decrements_then_reschedules_on_expiry() {
    // A multi-tick quantum counts down and only reschedules at 0.
    assert_eq!(tick_quantum(3, true), (2, false));
    assert_eq!(tick_quantum(2, true), (1, false));
    // Expiry with a ready thread → reset + reschedule.
    assert_eq!(tick_quantum(1, true), (QUANTUM_TICKS, true));
    // Expiry with nothing ready → reset but do NOT reschedule (keep running
    // the current thread; switching to idle would be pointless churn).
    assert_eq!(tick_quantum(1, false), (QUANTUM_TICKS, false));
}

#[test]
fn tick_quantum_one_tick_quantum_reschedules_every_tick_when_ready() {
    // With QUANTUM_TICKS == 1 the live config reschedules each tick.
    assert_eq!(tick_quantum(QUANTUM_TICKS, true).1, QUANTUM_TICKS == 1);
}

#[test]
fn queue_drop_releases_every_thread_exactly_once() {
    init_global_heap();
    test_probe::reset();
    {
        let mut st = test_state();
        st.ready[0].try_reserve(READY_RESERVE).unwrap();
        for tid in 1..=4 {
            st.ready[0].try_push(inert_ref(tid)).unwrap();
        }
        *st.cur_slot() = Some(inert_ref(5));
        st.reap[0].try_reserve(REAP_RESERVE).unwrap();
        st.reap[0].try_push(inert_ref(6)).unwrap();
        st.reap[0].try_push(inert_ref(7)).unwrap();
        // No destroys while the refs are held.
        assert_eq!(test_probe::thread_destroys(), 0);
    } // st dropped here — every ObjectRef drops its one reference.
    assert_eq!(test_probe::thread_destroys(), 7, "each queued/current/reaped thread destroyed exactly once",);
}

/// Build an inert Thread tagged with `owner_pid = pid` (and no owning
/// Process / kernel stack, so no host paging is needed). The teardown tests
/// only read `owner_pid`, so this is enough to exercise the `pid` scans.
fn inert_user_ref(tid: u32, pid: u32) -> ObjectRef {
    into_objref(Thread::try_new(tid, pid).unwrap())
}

#[test]
fn resume_suspended_moves_to_ready_and_sets_disposition() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0].try_reserve(READY_RESERVE).unwrap();
    st.suspended.try_reserve(BLOCKED_RESERVE).unwrap();
    let th = inert_user_ref(1, 1);
    let obj = th.as_ptr();
    // SAFETY: pinned, single-threaded test; mimic a suspended thread.
    unsafe {
        Thread::set_state(obj, ThreadState::Suspended);
        Thread::set_exception_frame(obj, 0xdead_beef);
    }
    st.suspended.try_push(th).unwrap();

    // resume_suspended works against the global SCHED, so drive the list ops
    // directly here (mirroring its body) to keep the test lock-free.
    let i = st.suspended.iter().position(|r| r.as_ptr() == obj).unwrap();
    let r = st.suspended.remove(i);
    // SAFETY: pinned, single-threaded.
    unsafe {
        Thread::set_disposition(obj, 2, 7);
        Thread::set_state(obj, ThreadState::Ready);
    }
    st.ready[0].try_push(r).unwrap();

    assert!(st.suspended.is_empty());
    assert_eq!(st.ready[0].len(), 1);
    // SAFETY: pinned. take_disposition reads (tag, code) and clears the frame.
    let (tag, code) = unsafe { Thread::take_disposition(obj) };
    assert_eq!((tag, code), (2, 7));
    // SAFETY: pinned — the frame was cleared by take_disposition.
    assert!(unsafe { Thread::exception_frame(obj) }.is_none());
}

#[test]
fn has_live_siblings_scans_all_parked_lists() {
    init_global_heap();
    let mut st = test_state();
    st.ready[0].try_reserve(READY_RESERVE).unwrap();
    st.blocked.try_reserve(BLOCKED_RESERVE).unwrap();
    st.suspended.try_reserve(BLOCKED_RESERVE).unwrap();

    // pid 1 has a sibling parked in `suspended`; pid 2 has none anywhere.
    st.ready[0].try_push(inert_user_ref(1, 1)).unwrap();
    st.suspended.try_push(inert_user_ref(2, 1)).unwrap();
    st.blocked.try_push(inert_user_ref(3, 3)).unwrap();

    assert!(has_live_siblings(&st, 1));
    assert!(has_live_siblings(&st, 3));
    assert!(!has_live_siblings(&st, 2), "no pid-2 thread is parked");
}

#[test]
fn reap_matching_moves_only_same_pid_threads() {
    init_global_heap();
    test_probe::reset();
    {
        let mut st = test_state();
        st.ready[0].try_reserve(READY_RESERVE).unwrap();
        st.reap[0].try_reserve(REAP_RESERVE).unwrap();
        st.ready[0].try_push(inert_user_ref(1, 1)).unwrap();
        st.ready[0].try_push(inert_user_ref(2, 2)).unwrap();
        st.ready[0].try_push(inert_user_ref(3, 1)).unwrap();

        reap_matching(&mut st.ready[0], &mut st.reap[0], 1);
        assert_eq!(st.reap[0].len(), 2, "both pid-1 threads reaped");
        assert_eq!(st.ready[0].len(), 1, "the pid-2 thread stays");
        // SAFETY: pinned, single-threaded.
        let left = unsafe { &*(st.ready[0].iter().next().unwrap().as_ptr() as *const Thread) };
        assert_eq!(left.owner_pid(), 2);
        // No destroys yet — the refs are alive in `reap`.
        assert_eq!(test_probe::thread_destroys(), 0);
    } // st dropped — reap + ready release their refs.
    assert_eq!(test_probe::thread_destroys(), 3);
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
    // Spawn-style: create runnable would need paging, so use inert.
    let r = into_objref(Thread::try_new(9, 0).unwrap());
    // entry/arg accessor reads back the inert defaults.
    // SAFETY: pinned, single-threaded.
    let (_entry, arg) = unsafe { Thread::entry_and_arg(r.as_ptr()) };
    assert_eq!(arg, 0);
    drop(r);
    assert_eq!(test_probe::thread_destroys(), 1);
    let _ = noop; // silence dead-code in case the demo path is cfg'd out
}
