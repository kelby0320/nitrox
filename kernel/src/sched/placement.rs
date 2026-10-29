//! Where a thread runs: weights, vruntime, the CPU chosen and work stolen.

use super::*;

/// `NICE_0_WEIGHT` and the Linux-style nice→weight table (nice `-20..=19`): vruntime accrues
/// as `slice * NICE_0_WEIGHT / weight(nice)`, so a heavier (lower) nice accrues *slower*.
pub(super) const NICE_0_WEIGHT: u64 = 1024;

#[rustfmt::skip]
const NICE_WEIGHTS: [u64; 40] = [
    /* -20..-16 */ 88761, 71755, 56483, 46273, 36291,
    /* -15..-11 */ 29154, 23254, 18705, 14949, 11916,
    /* -10..-6  */  9548,  7620,  6100,  4904,  3906,
    /*  -5..-1  */  3121,  2501,  1991,  1586,  1277,
    /*   0..4   */  1024,   820,   655,   526,   423,
    /*   5..9   */   335,   272,   215,   172,   137,
    /*  10..14  */   110,    87,    70,    56,    45,
    /*  15..19  */    36,    29,    23,    18,    15,
];

/// The nice→weight table entry for `nice` (clamped to `-20..=19`).
#[inline]
pub(super) fn nice_weight(nice: i8) -> u64 {
    NICE_WEIGHTS[(nice as i32 + 20).clamp(0, 39) as usize]
}

/// The per-tick virtual-runtime increment for a TimeShared thread with `nice`.
#[inline]
pub(super) fn vruntime_delta(nice: i8) -> u64 {
    TICK_NS * NICE_0_WEIGHT / nice_weight(nice)
}

/// The pick key for a runnable thread, **lower first**: `(class_rank, rt_priority,
/// vruntime)`. RealTime (rank 0) precedes TimeShared (rank 1); Idle never appears in `ready`.
///
/// # Safety
/// `obj` is a live, pinned `Thread`; the caller holds `SCHED`.
unsafe fn sched_key(obj: *mut ()) -> (u8, u8, u64) {
    // SAFETY: this function's contract: `obj` is a live, pinned `Thread`, and `SCHED` is held.
    unsafe {
        match Thread::sched_class(obj) {
            SchedClass::RealTime => (0, Thread::rt_priority(obj), 0),
            SchedClass::TimeShared => (1, 0, Thread::vruntime(obj)),
            SchedClass::Idle => (2, 0, u64::MAX),
        }
    }
}

/// Pick the next thread by **class precedence** and remove it from `ready`: RealTime (lowest
/// `rt_priority`, FIFO within one) beats TimeShared (smallest `vruntime`). An O(n) scan
/// (`docs/architecture/scheduler.md`). Caller holds the run-queue lock.
pub(super) fn dequeue_front(g: &mut SchedState) -> Option<ObjectRef> {
    let cpu = SchedState::this_cpu();
    let n = g.ready[cpu].len();
    if n == 0 {
        return None;
    }
    let mut best_i = 0usize;
    // SAFETY: each `ready` entry pins a live Thread; `SCHED` held.
    let mut best_key = unsafe { sched_key(g.ready[cpu][0].as_ptr()) };
    for i in 1..n {
        // SAFETY: as above. `<` (strict) keeps the earliest entry on ties (FIFO).
        let key = unsafe { sched_key(g.ready[cpu][i].as_ptr()) };
        if key < best_key {
            best_i = i;
            best_key = key;
        }
    }
    // Advance this CPU's monotonic TimeShared floor to the picked vruntime.
    if best_key.0 == 1 {
        g.min_vruntime[cpu] = g.min_vruntime[cpu].max(best_key.2);
    }
    Some(g.ready[cpu].remove(best_i))
}

/// Whether `cpu` may be **given** work: the scheduler has it online *and* it has not parked
/// forever.
///
/// **Both, because the two disagree after a park.** [`leave_online`] clears the lock-free
/// mask but cannot clear `cpu_online[]` (that needs `SCHED`, which the parking CPU may
/// already hold), so a CPU halted in `dump_and_halt` on a ring-0 fault stays `true` there
/// while the machine runs on. Placing on it is not a slowdown but a hang: nothing on that
/// queue will ever be picked.
///
/// **Deliberately not applied to the *victim* side of work-stealing.** A parked CPU is
/// exactly the one to drain; gating `steal_one`/`steal_available` on this would strand the
/// threads already queued there. Do not "make it consistent".
///
/// **`online` is passed in, not read here**, so the decision is a function of its inputs:
/// reading the global made placement tests share process-wide state under concurrent
/// `cargo test` (a 22 % failure rate). Callers read [`online_mask`] once per decision, which
/// also keeps one scan from seeing two different online sets.
pub(super) fn cpu_accepts_work(g: &SchedState, cpu: usize, online: u64) -> bool {
    g.cpu_online[cpu] && online & (1u64 << cpu) != 0
}

/// The least-loaded online CPU (fewest `ready` entries) that `obj`'s affinity permits — the
/// placement target for a new thread.
///
/// **When affinity permits no CPU that accepts work, it places on one that does anyway**,
/// outside the mask (see the fallback). Reachable because a permitted CPU can *park* after
/// the fact, and `sys_thread_set_affinity` validates only that the mask is non-empty within
/// `MAX_CPUS`, not that it names an online CPU. Caller holds `SCHED`.
pub(super) fn pick_target_cpu(g: &SchedState, obj: *mut (), online: u64) -> usize {
    // SAFETY: `obj` is a pinned Thread; `SCHED` held.
    let mask = unsafe { Thread::cpu_mask(obj) };
    let mut best = usize::MAX;
    let mut best_len = usize::MAX;
    // The same scan ignoring affinity, for the fallback below: a full queue
    // makes `place_thread` return `Err`, which both wake callers turn into a `panic!`, so the
    // fallback must be least-loaded too.
    let mut spare = usize::MAX;
    let mut spare_len = usize::MAX;
    for c in 0..MAX_CPUS {
        if !cpu_accepts_work(g, c, online) {
            continue;
        }
        let len = g.ready[c].len();
        if len < spare_len {
            spare_len = len;
            spare = c;
        }
        if mask & (1 << c) == 0 {
            continue;
        }
        if len < best_len {
            best_len = len;
            best = c;
        }
    }
    if best != usize::MAX {
        return best;
    }
    // **Affinity permits no CPU that accepts work.** Prefer *runnable* over
    // *affinity-correct*: a thread placed where nothing will schedule it hangs forever,
    // while one running outside its affinity merely violates policy.
    //
    // **Falling back to CPU 0 unconditionally would reintroduce the hang
    // `cpu_accepts_work` exists to prevent**: a ring-0 fault parks the BSP, a wake whose
    // affinity excludes every remaining CPU lands on it, and the thread never runs again.
    // Erroring instead is not available: both wake callers `panic!` on `Err`.
    if spare != usize::MAX {
        return spare;
    }
    // Nothing accepts work. We are executing, so this CPU is the least-wrong answer.
    SchedState::this_cpu()
}

/// The CPU to place a **waking** thread on: its home CPU (`last_cpu`) when that CPU is
/// online, affinity-permitted, **and has queue room** (cache-warm, no needless migration),
/// otherwise the least-loaded permitted CPU. The room check keeps a full home queue from
/// being a fatal wake.
///
/// **That is one of three ways [`place_thread`] refuses**; `pick_target_cpu` has three exits
/// and all reach `Err`:
///
/// 1. `best` — every affinity-permitted CPU that accepts work is at reserve.
/// 2. `spare` — affinity permits *no* CPU that accepts work, so placement leaves the mask:
///    a thread pinned to a parked CPU while every CPU still accepting work is full. A panic
///    here must **not** say "every affinity-permitted queue is at reserve" — the only
///    permitted queue may be empty and merely parked.
/// 3. `this_cpu()` — the last resort when *nothing* accepts work, returned without checking
///    its queue: no queue is worth preferring when nothing will run, and this CPU is the
///    only one known to be executing.
///
/// Caller holds `SCHED`.
pub(super) fn pick_wake_cpu(g: &SchedState, obj: *mut (), online: u64) -> usize {
    // SAFETY: `obj` is a pinned Thread; `SCHED` held.
    let home = unsafe { Thread::last_cpu(obj) } as usize;
    // SAFETY: as above.
    let mask = unsafe { Thread::cpu_mask(obj) };
    if home < MAX_CPUS
        && cpu_accepts_work(g, home, online)
        && mask & (1 << home) != 0
        && g.ready[home].len() < g.ready[home].capacity()
    {
        return home;
    }
    pick_target_cpu(g, obj, online)
}

/// Enqueue a runnable thread `r` on a chosen CPU's run queue, seeding its vruntime against
/// that CPU's floor. `wake` selects the policy (home CPU vs least-loaded) and the seeding
/// (latency boost vs plain floor). Returns `Err(r)` — the ref handed back so the caller
/// drops it **outside** the lock — if the chosen queue is full. Caller holds `SCHED`.
pub(super) fn place_thread(g: &mut SchedState, r: ObjectRef, wake: bool) -> Result<(), ObjectRef> {
    let obj = r.as_ptr();
    // Read once for this whole placement decision, rather than per candidate CPU.
    let online = online_mask();
    let cpu = if wake {
        // A wake re-homes to the thread's home CPU (`last_cpu`) when possible (cache-warm).
        pick_wake_cpu(g, obj, online)
    } else {
        // A newly spawned thread — **user or kernel** — goes to the least-loaded permitted
        // CPU, so userspace uses the APs from the start. Migrating a running user thread is
        // safe: its CR3 and per-CPU kernel-stack arming (TSS.RSP0 / syscall stack /
        // `KERNEL_GS_BASE`) are re-established on every switch-in, the syscall MSRs are
        // re-armed at each ring-3 descent, dense CPU indices are unique, the kernel-vmap is
        // kept coherent by TLB shootdown, and the switch-out race is closed by `on_cpu`.
        pick_target_cpu(g, obj, online)
    };
    if (!wake && g.threads >= MAX_THREADS) || g.ready[cpu].len() >= g.ready[cpu].capacity() {
        return Err(r);
    }
    g.threads += usize::from(!wake);
    seed_vruntime(g, obj, cpu, wake);
    push_reserved(&mut g.ready[cpu], r, "run queue (placement)");
    g.stats[cpu].placed += 1;
    // A *different* CPU gets a reschedule IPI so it runs the newcomer promptly (resuming it
    // if idle) rather than at its next tick — the timer is no dependable wake for a halted
    // CPU. Same-CPU placement needs no IPI: whichever path made the thread runnable reaches
    // a scheduling point before returning to userspace — the timer tail, the syscall return,
    // or, for a device-completion DPC, the device-IRQ tail's `resched_if_idle`.
    if cpu != SchedState::this_cpu() {
        crate::arch::send_reschedule_ipi(cpu);
    }
    Ok(())
}

/// Steal one runnable thread for this CPU from the **busiest** other online CPU that holds
/// a thread this CPU may run (affinity permits `me`, not mid-switch-out). A busier victim
/// with **no** stealable thread does not shadow a smaller one that has one: `steal_one`
/// succeeds exactly when [`steal_available`] is true, since the idle-steal paths are gated
/// on it and fall back to the idle slot on `None` — which is empty when idle is already
/// current, so a mismatch would panic. Caller holds `SCHED`.
pub(super) fn steal_one(g: &mut SchedState) -> Option<ObjectRef> {
    let me = SchedState::this_cpu();
    let mut victim = usize::MAX;
    let mut victim_pos = 0usize;
    let mut victim_len = 0usize;
    for c in 0..MAX_CPUS {
        if c == me || !g.cpu_online[c] || g.ready[c].len() <= victim_len {
            continue;
        }
        // The first thread on this candidate that is stealable to this CPU.
        // SAFETY: each entry pins a live Thread; `SCHED` held.
        let stealable = (0..g.ready[c].len()).find(|&i| unsafe { stealable_to(g.ready[c][i].as_ptr(), me) });
        if let Some(pos) = stealable {
            victim = c;
            victim_pos = pos;
            victim_len = g.ready[c].len();
        }
    }
    if victim == usize::MAX {
        return None;
    }
    g.stats[me].steals += 1;
    Some(g.ready[victim].remove(victim_pos))
}

/// `true` if no online CPU has a thread to run: every ready queue is empty, and every other CPU
/// runs its idle thread. This CPU's current thread is the caller's to have taken. Caller holds
/// `SCHED`.
pub(super) fn nothing_to_run(g: &SchedState) -> bool {
    let me = SchedState::this_cpu();
    (0..MAX_CPUS).filter(|&c| g.cpu_online[c]).all(|c| {
        g.ready[c].is_empty()
            && (c == me || g.current[c].as_ref().is_none_or(|t| t.as_ptr() as usize == g.idle_addr[c]))
    })
}

/// `true` if some other online CPU holds a thread this CPU (`me`) may run — the cheap
/// precondition for the idle-tick steal. Caller holds `SCHED`.
pub(super) fn steal_available(g: &SchedState, me: usize) -> bool {
    (0..MAX_CPUS).any(|c| {
        c != me
            && g.cpu_online[c]
            // SAFETY: each entry pins a live Thread; `SCHED` held.
            && g.ready[c].iter().any(|r| unsafe { stealable_to(r.as_ptr(), me) })
    })
}

/// `true` if the thread `obj` may be **stolen** to CPU `me`: its affinity includes `me` and
/// it is not still mid-switch-out. **User and kernel threads alike** are stealable — see
/// [`place_thread`] for why migrating a running user thread is safe.
///
/// # Safety
/// `obj` is a live, pinned `Thread`; the caller holds `SCHED`.
pub(super) unsafe fn stealable_to(obj: *mut (), me: usize) -> bool {
    // Skip a thread still mid-switch-out on another CPU (`on_cpu` set): its `saved_sp` is
    // not yet committed, so resuming it here would load a stale frame and double-run it. It
    // becomes stealable once the switch completes (see `Thread::on_cpu` / `switch_into`).
    // SAFETY: this function's contract: `obj` is a live, pinned `Thread`, and `SCHED` is held.
    unsafe { !Thread::is_on_cpu(obj) && Thread::cpu_mask(obj) & (1 << me) != 0 }
}

/// Pick the next thread for this CPU: its own queue first, else **steal** from the busiest
/// peer. A stolen thread is re-seeded against this CPU's vruntime floor (its vruntime
/// referenced the victim's) so it is neither favored nor starved. Caller holds `SCHED`.
pub(super) fn pick_next(g: &mut SchedState) -> Option<ObjectRef> {
    if let Some(t) = dequeue_front(g) {
        return Some(t);
    }
    let stolen = steal_one(g)?;
    seed_vruntime(g, stolen.as_ptr(), SchedState::this_cpu(), true);
    Some(stolen)
}

/// Charge the running thread one tick of virtual runtime, scaled by nice weight — only if
/// **TimeShared** (RealTime is fixed-priority; Idle never accrues). Caller holds `SCHED`.
pub(super) fn accrue_vruntime(g: &mut SchedState) {
    let obj = match g.cur_ref().as_ref() {
        Some(c) => c.as_ptr(),
        None => return,
    };
    // SAFETY: `obj` is the running thread (pinned), `SCHED` held — the accessor
    // contract for these field reads/writes.
    unsafe {
        if Thread::sched_class(obj) == SchedClass::TimeShared {
            let v = Thread::vruntime(obj).saturating_add(vruntime_delta(Thread::nice(obj)));
            Thread::set_vruntime(obj, v);
        }
    }
}

/// Seed a freshly-runnable TimeShared thread's vruntime: `new` (spawned) → the current
/// floor; `woken` → `min_vruntime - slice` (a latency boost), never below its own accrued
/// vruntime. Caller holds `SCHED`.
pub(super) fn seed_vruntime(g: &SchedState, obj: *mut (), cpu: usize, woken: bool) {
    // SAFETY: `obj` is a pinned Thread; `SCHED` held.
    if unsafe { Thread::sched_class(obj) } != SchedClass::TimeShared {
        return;
    }
    let base = g.min_vruntime[cpu];
    let floor = if woken { base.saturating_sub(TICK_NS) } else { base };
    // SAFETY: as above. A spawned thread (vruntime 0) jumps to the floor; a waker
    // keeps its (higher) accrued vruntime if already ahead of the boosted floor.
    unsafe {
        if Thread::vruntime(obj) < floor {
            Thread::set_vruntime(obj, floor);
        }
    }
}
