//! Spawning threads and processes, and bringing up each CPU's scheduler.

use super::*;

/// Pre-reserved capacity for [`SchedState::ended_pids`]. Exit pushes into it under
/// `SCHED`, so it must never allocate while held; the reaper drains it promptly, and the
/// existing per-CPU sites drain it too, so the depth only has to cover a burst of exits
/// between two scheduler entries.
const ENDED_PIDS_RESERVE: usize = 64;

/// Thread id of the reaper — like [`IDLE_TID`], an identity marker rather than a real tid.
const REAPER_TID: u32 = u32::MAX - 1;

/// Initialise the scheduler, adopting the running boot context as the first
/// (current) thread so the first [`context_switch`] has a valid slot to
/// save into. Must run once, after the allocators and paging are up and
/// before any [`spawn`]/[`yield_now`].
pub fn init() -> Result<(), AllocError> {
    // Cache the kernel/boot page-table root for the CR3 hook (see
    // `resolve_root`). `active_root` reads CR3 — a ring-0 op only reached at
    // real boot, never in host tests (which never call `init`).
    BOOT_ROOT.store(Paging::active_root().as_u64(), Ordering::Relaxed);

    // Build the pre-reserved run queue + blocked list + deadline heap OUTSIDE
    // the lock (the only growth). Blocking and timer arming stay within these
    // reserves, never allocating under the rank-1 lock.
    let mut ready: KVec<ObjectRef> = KVec::new();
    ready.try_reserve(READY_RESERVE)?;
    let mut blocked: KVec<ObjectRef> = KVec::new();
    blocked.try_reserve(BLOCKED_RESERVE)?;
    let mut suspended: KVec<ObjectRef> = KVec::new();
    suspended.try_reserve(BLOCKED_RESERVE)?;
    let mut reap: KVec<ObjectRef> = KVec::new();
    reap.try_reserve(REAP_RESERVE)?;
    let mut deadlines: KVec<deadline::Entry> = KVec::new();
    deadlines.try_reserve(deadline::HEAP_RESERVE)?;
    let mut deferred_drops: KVec<ObjectRef> = KVec::new();
    deferred_drops.try_reserve(DEFERRED_DROP_RESERVE)?;
    let boot = Thread::try_new_boot(0, 0)?;
    let boot_ref = into_objref(boot);

    // The idle thread: a runnable kernel thread with its own stack that just
    // halts. Built outside the lock (it allocates a kernel stack). It is never
    // enqueued or reaped — its body loops forever.
    let idle = Thread::try_new_runnable(IDLE_TID, 0, idle_body, 0)?;
    let idle_ref = into_objref(idle);
    let idle_addr = idle_ref.as_ptr() as usize;

    // The reaper: **one** for the machine, not one per CPU. It closes handle tables, which
    // are keyed on pid and touch no thread stack, so any CPU may do it. (The per-CPU
    // `reap` list is a different job with a different rule — see `reaper_body`.)
    // Allocated outside the lock, like the idle thread; it starts runnable and parks
    // itself the first time it finds nothing to do.
    let reaper = Thread::try_new_runnable(REAPER_TID, 0, reaper_body, 0)?;
    let reaper_ref = into_objref(reaper);
    let mut ended_pids: KVec<u32> = KVec::new();
    ended_pids.try_reserve(ENDED_PIDS_RESERVE)?;

    let mut g = SCHED.lock();
    g.ready[0] = ready; // BSP is logical CPU 0; APs reserve their own in `ap_init`.
    g.cpu_online[0] = true; // the BSP; each AP sets its own bit in `ap_init`.
    online_set::add(0); // lock-free mirror (BSP = bit 0)
    g.blocked = blocked;
    g.suspended = suspended;
    g.reap[0] = reap;
    g.deadlines = deadlines;
    g.deferred_drops = deferred_drops;
    g.ended_pids = ended_pids;
    push_reserved(&mut g.ready[0], reaper_ref, "run queue (reaper install)");
    *g.cur_slot() = Some(boot_ref);
    *g.idle_slot() = Some(idle_ref);
    g.set_idle_addr(idle_addr);
    g.quantum = [QUANTUM_TICKS; MAX_CPUS];
    Ok(())
}

/// Bring an application processor into the scheduler. Run on the AP itself after
/// its architecture bring-up ([`crate::arch::ap_cpu_init`]) and timer arming.
/// Creates this CPU's **boot thread** (adopting the running Limine context) and
/// its **idle thread**, installs them in this CPU's `current`/`idle` slots, enables
/// preemption, and retires the boot thread — leaving the AP running idle and
/// pulling runnable threads from the **shared** `ready` queue. Diverges.
///
/// The global run/blocked/reap/deadline structures are *not* (re)built here —
/// [`init`] did that once on the BSP; an AP only adds its own per-CPU slots.
/// Retire this AP's boot thread into the scheduler. Diverges.
///
/// **[`ap_init`] must have succeeded first.** The two were one function until 2026-08-19;
/// splitting them is what lets `ap_entry` report the CPU online only *after* its scheduler
/// context exists. While the init was in here, `AP_ONLINE` had already been incremented by
/// the time it ran, so a failure left the BSP counting a CPU that never joined — and outside
/// a `test-harness` build, where the panic hits `debug_exit`, the machine simply booted on
/// with one fewer core. That is the state this change exists to remove.
pub fn ap_run() -> ! {
    // SAFETY: this CPU's `current` (boot) and `idle` slots are set, so a timer
    // tick can schedule. Arm preemption, then retire the boot thread into the
    // run loop (the AP continues as its idle thread until `ready` has work).
    unsafe { Cpu::interrupts_enable() };
    exit_thread(ExitStatus { kind: ExitKind::Normal as u32, code: 0 });
}

/// Per-CPU scheduler-context setup for an AP: a boot thread (adopting the running
/// context) as `current`, plus this CPU's idle thread. Mirrors the per-CPU half of
/// [`init`] without the one-time global reserves.
pub fn ap_init() -> Result<(), AllocError> {
    // A fresh tid for this AP's (transient) boot thread; the idle thread reuses
    // `IDLE_TID` (idle identity is its object address, via `idle_addr`).
    let boot_tid = {
        let mut g = SCHED.lock();
        let t = g.next_tid;
        g.next_tid = g.next_tid.wrapping_add(1);
        t
    };
    let boot = Thread::try_new_boot(boot_tid, 0)?;
    let boot_ref = into_objref(boot);
    let idle = Thread::try_new_runnable(IDLE_TID, 0, idle_body, 0)?;
    let idle_ref = into_objref(idle);
    let idle_addr = idle_ref.as_ptr() as usize;
    // This CPU's own reap list + ready queue — reserved outside the lock so neither
    // `finish_exit`'s reap push nor a placement onto this CPU allocates under `SCHED`.
    let mut reap: KVec<ObjectRef> = KVec::new();
    reap.try_reserve(REAP_RESERVE)?;
    let mut ready: KVec<ObjectRef> = KVec::new();
    ready.try_reserve(READY_RESERVE)?;

    let mut g = SCHED.lock();
    *g.cur_slot() = Some(boot_ref);
    *g.idle_slot() = Some(idle_ref);
    g.set_idle_addr(idle_addr);
    *g.reap_slot() = reap;
    *g.ready_slot() = ready;
    // This AP is now schedulable — mark *its* bit (set in the same critical section
    // that reserved its queues, so no placement targets it before it is reserved).
    let cpu = SchedState::this_cpu();
    g.cpu_online[cpu] = true;
    online_set::add(cpu); // lock-free mirror
    Ok(())
}

/// Create a runnable kernel thread that will run `entry(arg)` in the default
/// **TimeShared** class and enqueue it. Returns the new thread id.
pub fn spawn(entry: ThreadEntry, arg: usize) -> Result<u32, AllocError> {
    spawn_with_class(entry, arg, SchedClass::TimeShared, 0, 0)
}

/// Set a thread's CPU **affinity** mask (`sys_thread_set_affinity`). Takes effect at
/// the thread's next placement / steal / wake; a thread already running on a now-
/// excluded CPU finishes its slice there and moves on its next reschedule (affinity is
/// advisory for the in-flight slice). The caller pins `thread` via a held handle ref.
///
/// # Safety
///
/// `thread` points at a live `Thread`, pinned for the call by a reference the caller holds.
pub unsafe fn set_thread_affinity(thread: *mut (), mask: u8) {
    let _g = SCHED.lock();
    // SAFETY: `thread` pins a live Thread (caller holds a handle ref); `SCHED` held.
    unsafe { Thread::set_cpu_mask(thread, mask) };
}

/// As [`spawn`] but pinned to the CPUs in `cpu_mask` (bit `c` ⇒ may run on CPU `c`).
/// Kernel-internal helper for affinity demos/tests; user threads use
/// `sys_thread_set_affinity`.
pub fn spawn_with_affinity(entry: ThreadEntry, arg: usize, cpu_mask: u8) -> Result<u32, AllocError> {
    spawn_inner(entry, arg, SchedClass::TimeShared, 0, 0, cpu_mask)
}

/// As [`spawn`], but in scheduling `class` with `rt_priority` (RealTime) / `nice`
/// (TimeShared). Kernel-internal: trusted callers set the class directly (the
/// `REAL_TIME` syscap gate for *user* threads lands with the capability system).
/// The stack allocation + frame fabrication happen before the (brief) enqueue lock.
pub fn spawn_with_class(
    entry: ThreadEntry,
    arg: usize,
    class: SchedClass,
    rt_priority: u8,
    nice: i8,
) -> Result<u32, AllocError> {
    spawn_inner(entry, arg, class, rt_priority, nice, u8::MAX)
}

/// The shared body of every kernel-thread spawn: fabricate the thread, set its
/// scheduling parameters + affinity, and place it on a CPU's run queue. `cpu_mask`
/// `u8::MAX` means no affinity restriction.
fn spawn_inner(
    entry: ThreadEntry,
    arg: usize,
    class: SchedClass,
    rt_priority: u8,
    nice: i8,
    cpu_mask: u8,
) -> Result<u32, AllocError> {
    let tid = {
        let mut g = SCHED.lock();
        let t = g.next_tid;
        g.next_tid = g.next_tid.wrapping_add(1);
        t
    };
    // Heavy work outside the lock.
    let thread = Thread::try_new_runnable(tid, 0, entry, arg)?;
    let r = into_objref(thread);
    let obj = r.as_ptr();
    // SAFETY: `obj` is the new thread, exclusively owned via `r` and not yet
    // enqueued — no other context can observe it, so setting its scheduling
    // parameters here (before the lock) is sound.
    unsafe {
        Thread::set_sched(obj, class, rt_priority, nice);
        Thread::set_cpu_mask(obj, cpu_mask);
    }

    let leftover = {
        let mut g = SCHED.lock();
        // Place on the least-loaded CPU's queue (seeding vruntime at its floor).
        // `place_thread` refuses rather than grow under the rank-1 lock, handing `r`
        // back so it drops below, lock-free (its `KernelStack` is rank-6 reclaimed).
        match place_thread(&mut g, r, false) {
            Ok(()) => return Ok(tid),
            Err(returned) => returned, // carry out of the locked block
        }
    };
    // Lock released: `leftover` drops here, releasing the thread's last reference and
    // freeing its kernel stack off the rank-1 lock.
    drop(leftover);
    Err(AllocError)
}

/// Create a **user** thread for `process` that descends to ring 3 at
/// `entry` with stack `user_sp`, and enqueue it. Returns a **cloned**
/// [`ObjectRef`] to the new thread (the enqueued `ready` entry holds its own
/// reference) so the caller can install a thread handle (`sys_thread_create`);
/// the thread id is `Thread::tid` of the returned object. The `process`
/// reference is moved into the thread (keeping its address space alive).
/// `boot_args` are the `[rdi, rsi, rdx, rcx]` register values seeded at the
/// thread's first ring-3 entry (the spawn hand-off — notification channel, root
/// namespace, first installed handle, `arg0`; `[0; 4]` for the boot/`hello`
/// path). The kernel stack + frame fabrication happen before the (brief) enqueue
/// lock.
pub fn spawn_user(process: ObjectRef, entry: u64, user_sp: u64, boot_args: [u64; 4]) -> Result<ObjectRef, AllocError> {
    // Default scheduling: TimeShared, nice 0, no affinity restriction — the behavior
    // every caller had before scheduling params were exposed via `ThreadArgs`.
    spawn_user_sched(process, entry, user_sp, boot_args, SchedClass::TimeShared, 0, 0, u8::MAX)
}

/// As [`spawn_user`], but with explicit scheduling parameters (from a user thread's
/// `ThreadArgs`). The `RealTime` class is `REAL_TIME`-gated by the caller
/// (`sys_thread_create`); this sets `class`/`rt_priority`/`nice`/`cpu_mask` on the
/// fresh thread **before** it is enqueued, so it never runs a scheduler tick under the
/// wrong class. `cpu_mask` `u8::MAX` ⇒ no affinity restriction.
#[allow(clippy::too_many_arguments, reason = "a thread's process, entry, stack and scheduling")]
pub fn spawn_user_sched(
    process: ObjectRef,
    entry: u64,
    user_sp: u64,
    boot_args: [u64; 4],
    class: SchedClass,
    rt_priority: u8,
    nice: i8,
    cpu_mask: u8,
) -> Result<ObjectRef, AllocError> {
    let tid = {
        let mut g = SCHED.lock();
        let t = g.next_tid;
        g.next_tid = g.next_tid.wrapping_add(1);
        t
    };
    // Heavy work outside the lock (consumes `process` on success).
    let thread = Thread::try_new_user(tid, process, entry, user_sp, boot_args)?;
    let r = into_objref(thread);
    let obj = r.as_ptr();
    // SAFETY: `obj` is the new thread, exclusively owned via `r` and not yet enqueued —
    // no other context can observe it, so setting its scheduling parameters here
    // (before the enqueue lock) is sound. Mirrors `spawn_inner`.
    unsafe {
        Thread::set_sched(obj, class, rt_priority, nice);
        Thread::set_cpu_mask(obj, cpu_mask);
    }
    // Clone the caller's handle before the enqueue moves `r` into `ready`.
    let handle = r.clone();

    let leftover = {
        let mut g = SCHED.lock();
        match place_thread(&mut g, r, false) {
            Ok(()) => return Ok(handle),
            Err(returned) => returned, // carry out of the locked block
        }
    };
    // Over capacity: lock released; `leftover` (the thread ref) and `handle` drop
    // here — releasing the thread's references, freeing its kernel stack, and
    // releasing the Process off the rank-1 lock.
    drop(leftover);
    Err(AllocError)
}

extern "C" fn idle_body(_arg: usize) {
    loop {
        reap_pending();
        // SAFETY: ring-0. `idle_halt` enables interrupts as it parks (`sti; hlt`),
        // so the CPU always sleeps with IF=1 — the periodic timer or a reschedule
        // IPI can then wake it, regardless of the inbound IF state.
        unsafe { Cpu::idle_halt() };
    }
}
