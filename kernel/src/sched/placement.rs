use super::*;

/// `NICE_0_WEIGHT` and the Linux-style nice→weight table (nice `-20..=19`).
/// TimeShared vruntime accrues as `slice * NICE_0_WEIGHT / weight(nice)`, so a
/// lower (heavier-weighted) nice accrues *slower* and is picked more often.
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

/// The scheduler's pick key for a runnable thread — **lower sorts first**:
/// `(class_rank, rt_priority, vruntime)`. RealTime (rank 0, by priority) precedes
/// TimeShared (rank 1, by vruntime); Idle (rank 2) never appears in `ready`.
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

/// Pick the next thread by **scheduling-class precedence** and remove it from
/// `ready`, or `None` if empty. RealTime (lowest `rt_priority`, FIFO within a
/// priority) beats TimeShared (smallest `vruntime`). An O(n) scan — ample at our
/// thread counts; per-class heaps/buckets are a later optimization
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
    // Advance this CPU's monotonic TimeShared floor to the picked thread (the
    // smallest vruntime on this queue), so `min_vruntime[cpu]` tracks its leftmost.
    if best_key.0 == 1 {
        g.min_vruntime[cpu] = g.min_vruntime[cpu].max(best_key.2);
    }
    Some(g.ready[cpu].remove(best_i))
}

/// Whether `cpu` may be **given** work: the scheduler has it online *and* it has not
/// parked forever.
///
/// **Both, because the two disagree after a park.** [`leave_online`] clears the lock-free
/// mask but cannot clear `cpu_online[]` — that needs `SCHED`, which the parking CPU may
/// already hold — so a CPU that halted in `dump_and_halt` on a ring-0 fault stays `true`
/// there while the rest of the machine runs on. Placing on it is not a slowdown but a hang:
/// nothing on that queue will ever be picked, and the thread waits forever on a machine
/// that otherwise looks healthy.
///
/// **Deliberately not applied to the *victim* side of work-stealing.** `steal_one` and
/// `steal_available` ask which CPU to take work *from*, and a parked CPU is exactly the one
/// you want drained — gating them on this would strand the threads already queued there,
/// which is the opposite of the fix. The asymmetry is the point; do not "make it
/// consistent".
/// **`online` is passed in, not read here**, so the decision is a function of its inputs.
/// Reading the global made every placement test share process-wide mutable state with every
/// other, and `cargo test` runs them concurrently: on a 16-core host that was a 22 % failure
/// rate across `sched::tests`, which CI happened not to hit (PR #199 review, blocking 1).
/// Callers read [`online_mask`] once per placement decision, which also keeps a single scan
/// from seeing two different online sets.
pub(super) fn cpu_accepts_work(g: &SchedState, cpu: usize, online: u64) -> bool {
    g.cpu_online[cpu] && online & (1u64 << cpu) != 0
}

/// The least-loaded online CPU (fewest `ready` entries) that `obj`'s affinity
/// permits — the placement target for a new thread.
///
/// **When affinity permits no CPU that accepts work, it places on one that does anyway**,
/// outside the affinity mask; see the fallback for why that beats both the alternatives.
/// That case is reachable two ways, and an earlier version of this comment denied the
/// second: a permitted CPU can *park* after the fact, and `sys_thread_set_affinity` does
/// **not** reject an affinity naming no online CPU — it validates only that the mask is
/// non-empty within `MAX_CPUS`. (The kernel audit refuted the claim that it did — evidence
/// in the decision log, 2026-08-14.)
///
/// Caller holds `SCHED`.
pub(super) fn pick_target_cpu(g: &SchedState, obj: *mut (), online: u64) -> usize {
    // SAFETY: `obj` is a pinned Thread; `SCHED` held.
    let mask = unsafe { Thread::cpu_mask(obj) };
    let mut best = usize::MAX;
    let mut best_len = usize::MAX;
    // The same least-loaded scan, ignoring affinity — the fallback below wants a CPU that
    // can *run* the thread, and "least loaded" is as much a part of that as "accepts work":
    // a queue at `READY_RESERVE` makes `place_thread` return `Err`, which both wake callers
    // turn into a `panic!`. Tracked in the same pass so the fallback is not load-blind
    // (PR #201 review, finding 2); one loop rather than two, since every candidate is
    // already being visited.
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
    // *affinity-correct*: a thread placed where nothing will ever schedule it hangs
    // forever, while a thread running outside its affinity is a policy violation it
    // survives — and `dequeue_front` applies no affinity filter anyway, so that
    // distinction is already best-effort at the other end.
    //
    // **Falling back to CPU 0 unconditionally — what this did until 2026-08-14 —
    // reintroduced the exact hang `cpu_accepts_work` exists to prevent, one line below
    // it.** A ring-0 fault parks the BSP, a wake whose affinity excludes every remaining
    // CPU lands on it, and the thread never runs again. Found by the kernel audit and
    // written up in the decision log, 2026-08-14; the
    // commit that added the guard even documented this fallback as newly reachable and
    // then left it unguarded.
    //
    // Erroring instead is not available: both wake callers `panic!` on `Err`, so a
    // parked BSP would become a kernel panic rather than a misplaced thread.
    if spare != usize::MAX {
        return spare;
    }
    // Nothing at all accepts work. We are executing this code, so this CPU is the
    // least-wrong answer that exists — and if even that is false the machine is already
    // gone.
    SchedState::this_cpu()
}

/// The CPU to place a **waking** thread on: its home CPU (`last_cpu`) when that CPU
/// is online, affinity-permitted, **and has queue room** (cache-warm, and avoids
/// needless migration), otherwise the least-loaded permitted CPU. The room check
/// (F6, decision log 2026-07-21) keeps a full home queue from being a fatal wake:
/// the fallback's least-loaded pick has room unless *every* permitted queue is full.
///
/// **That is one of three ways [`place_thread`] refuses.** This comment claimed it was the
/// only one until 2026-08-14 (kernel audit C.3(c)), and the correction that day named two;
/// `pick_target_cpu` has three exits and all three reach `Err` (PR #204 review, finding 3):
///
/// 1. `best` — the least-loaded CPU that is both affinity-permitted and accepting work, when
///    every such queue is at reserve. The case this paragraph describes.
/// 2. `spare` — reached when affinity permits *no* CPU that accepts work, so placement leaves
///    the mask: a thread pinned to a CPU that has parked, while every CPU still accepting
///    work is full. Note what the panic must **not** say here — "every affinity-permitted
///    queue is at reserve" is false, since the only permitted queue may be empty and merely
///    parked.
/// 3. `this_cpu()` — the last resort, when *nothing* accepts work, returned without checking
///    its queue. Deliberately not load-checked: there is no queue worth preferring when
///    nothing will run, and this CPU is the only one known to be executing.
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

/// Enqueue a runnable thread `r` on a chosen CPU's run queue, seeding its vruntime
/// against that CPU's floor. `wake` selects the placement policy (home CPU vs
/// least-loaded) and the seeding (latency boost vs plain floor). Returns `Err(r)` —
/// handing the ref back so the caller can drop it **outside** the lock — if the
/// chosen queue is at its reserved capacity. Caller holds `SCHED`.
pub(super) fn place_thread(g: &mut SchedState, r: ObjectRef, wake: bool) -> Result<(), ObjectRef> {
    let obj = r.as_ptr();
    // Read once for this whole placement decision, rather than per candidate CPU.
    let online = online_mask();
    let cpu = if wake {
        // A wake re-homes to the thread's home CPU (`last_cpu`) when possible —
        // cache-warm, and it avoids a needless migration.
        pick_wake_cpu(g, obj, online)
    } else {
        // A newly spawned thread — **user or kernel** — is placed on the
        // least-loaded permitted CPU, so userspace uses the APs from the start.
        // Migrating an already-running user thread is now safe: its CR3 and
        // per-CPU kernel-stack arming (TSS.RSP0 / syscall stack / `KERNEL_GS_BASE`)
        // are re-established on every switch-in (`switch_into` → `resolve_root` +
        // `arm_kernel_stack_for`), the syscall MSRs are re-armed at each ring-3
        // descent, dense CPU indices are unique by construction, the shared
        // kernel-vmap is kept coherent by TLB shootdown, and the switch-out race
        // is closed by the `on_cpu` guard. See the decision log (2026-07-01).
        pick_target_cpu(g, obj, online)
    };
    if g.ready[cpu].len() >= g.ready[cpu].capacity() {
        return Err(r);
    }
    seed_vruntime(g, obj, cpu, wake);
    push_reserved(&mut g.ready[cpu], r, "run queue (placement)");
    g.stats[cpu].placed += 1;
    // If the thread landed on a *different* CPU, poke that CPU with a reschedule
    // IPI so it runs the newcomer promptly (resuming it if idle) instead of waiting
    // for its next periodic tick — the timer is not a dependable wake for a halted
    // CPU, so cross-CPU wake delivery must be explicit. Same-CPU placement needs no
    // IPI: whichever path made this thread runnable reaches a scheduling point before
    // returning to userspace — the timer tail (`on_timer_tick`), the syscall return,
    // or, for a wake from a device-completion DPC, the device-IRQ tail's
    // `resched_if_idle` (added 2026-07-23; without it a same-CPU I/O-completion wake on
    // an idle CPU stalled until the next tick — the fs-server "I/O hang").
    if cpu != SchedState::this_cpu() {
        crate::arch::send_reschedule_ipi(cpu);
    }
    Ok(())
}

/// Steal one runnable thread for this CPU: from the **busiest** other online CPU
/// that actually holds a thread this CPU may run (affinity permits `me`, not
/// mid-switch-out) — so an otherwise-idle CPU picks up slack. Victims are ranked
/// by queue length, but a busier victim with **no** stealable thread does not
/// shadow a smaller one that has one: `steal_one` succeeds exactly when
/// [`steal_available`] is true. (The idle-steal paths are gated on
/// `steal_available` and fall back to taking the idle slot on a `None` — with
/// idle already current that slot is empty, so a mismatch was a panic: F4,
/// decision log 2026-07-21.) The thread is removed from the victim's queue and
/// returned. `None` if no other CPU has a thread this CPU may run. Caller holds
/// `SCHED`.
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

/// `true` if some other online CPU holds a thread this CPU (`me`) may run — the cheap
/// precondition the idle-tick path checks before triggering a steal. Caller holds `SCHED`.
pub(super) fn steal_available(g: &SchedState, me: usize) -> bool {
    (0..MAX_CPUS).any(|c| {
        c != me
            && g.cpu_online[c]
            // SAFETY: each entry pins a live Thread; `SCHED` held.
            && g.ready[c].iter().any(|r| unsafe { stealable_to(r.as_ptr(), me) })
    })
}

/// `true` if the thread `obj` may be **stolen** to CPU `me`: its affinity includes
/// `me` and it is not still mid-switch-out. **User and kernel threads alike** are
/// stealable — migrating an already-running user thread is safe now that its CR3 and
/// per-CPU kernel-stack state are re-armed on every switch-in and the switch-out race
/// is closed (see [`place_thread`] and the 3b fixes).
///
/// # Safety
/// `obj` is a live, pinned `Thread`; the caller holds `SCHED`.
pub(super) unsafe fn stealable_to(obj: *mut (), me: usize) -> bool {
    // Skip a thread still mid-switch-out on another CPU (`on_cpu` set): its
    // parked `saved_sp` is not yet committed, so resuming it here would load a
    // stale/garbage frame and double-run it. It stays on the victim's queue and
    // becomes stealable on the next round once the switch completes (the SMP
    // `on_cpu` invariant; see `Thread::on_cpu` / `switch_into`).
    // SAFETY: this function's contract: `obj` is a live, pinned `Thread`, and `SCHED` is held.
    unsafe { !Thread::is_on_cpu(obj) && Thread::cpu_mask(obj) & (1 << me) != 0 }
}

/// Pick the next thread to run on this CPU: its own queue first, else **steal** from
/// the busiest peer. A stolen thread is re-seeded against this CPU's vruntime floor
/// (its vruntime referenced the victim's) so it is neither favored nor starved. `None`
/// only when no CPU has runnable work. Caller holds `SCHED`.
pub(super) fn pick_next(g: &mut SchedState) -> Option<ObjectRef> {
    if let Some(t) = dequeue_front(g) {
        return Some(t);
    }
    let stolen = steal_one(g)?;
    seed_vruntime(g, stolen.as_ptr(), SchedState::this_cpu(), true);
    Some(stolen)
}

/// Charge the running thread (on this CPU) one tick of virtual runtime, scaled by
/// its nice weight — but only if it is **TimeShared** (RealTime is fixed-priority,
/// not fair-scheduled; Idle never accrues). Caller holds `SCHED`.
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

/// Seed a freshly-runnable TimeShared thread's vruntime so it joins fairly:
/// `new` (spawned) → the current floor; `woken` → `min_vruntime - slice` (a small
/// latency boost) but never below its own accrued vruntime. No-op for non-TS or a
/// thread already at/above the floor. Caller holds `SCHED`.
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
