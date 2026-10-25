//! Spawning threads and processes, and bringing up each CPU's scheduler.

use super::*;

/// Pre-reserved capacity for [`SchedState::ended_pids`]. Exit pushes into it under `SCHED`,
/// so it must never allocate; the depth only has to cover a burst of exits between two
/// scheduler entries.
const ENDED_PIDS_RESERVE: usize = 64;

/// Thread id of the reaper — like [`IDLE_TID`], an identity marker, not a real tid.
const REAPER_TID: u32 = u32::MAX - 1;

/// A list a thread can be on — a run queue, the blocked or suspended list, a reap list — reserved
/// to hold [`MAX_THREADS`]: every list is made here, so the boot's reserves are the tests'.
pub(super) fn thread_list() -> Result<KVec<ObjectRef>, AllocError> {
    let mut list = KVec::new();
    list.try_reserve(MAX_THREADS)?;
    Ok(list)
}

/// Initialise the scheduler, adopting the running boot context as the first (current)
/// thread so the first [`context_switch`] has a valid slot to save into. Must run once,
/// after the allocators and paging are up and before any [`spawn`]/[`yield_now`].
pub fn init() -> Result<(), AllocError> {
    // Cache the kernel/boot page-table root (see `resolve_root`); `active_root` reads CR3,
    // never reached in host tests.
    BOOT_ROOT.store(Paging::active_root().as_u64(), Ordering::Relaxed);

    // Build the pre-reserved queues and heap OUTSIDE the lock (the only growth); blocking and
    // timer arming stay within these reserves.
    let (ready, blocked, suspended, reap) = (thread_list()?, thread_list()?, thread_list()?, thread_list()?);
    let mut deadlines: KVec<deadline::Entry> = KVec::new();
    deadlines.try_reserve(deadline::HEAP_RESERVE)?;
    let mut deferred_drops: KVec<ObjectRef> = KVec::new();
    deferred_drops.try_reserve(DEFERRED_DROP_RESERVE)?;
    let boot = Thread::try_new_boot(0, 0)?;
    let boot_ref = into_objref(boot);

    // The idle thread: a kernel thread with its own stack that just halts. Built outside the
    // lock (it allocates a stack). Never enqueued or reaped.
    let idle = Thread::try_new_runnable(IDLE_TID, 0, idle_body, 0)?;
    let idle_ref = into_objref(idle);
    let idle_addr = idle_ref.as_ptr() as usize;

    // The reaper: **one** for the machine. It closes handle tables, which are keyed on pid
    // and touch no thread stack, so any CPU may do it (the per-CPU `reap` list is a
    // different job — see `reaper_body`). Allocated outside the lock; it starts runnable and
    // parks itself when it finds nothing to do.
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
    g.deadline_slots[deadline::DeadlineKind::Thread as usize] = deadline::HEAP_RESERVE;
    g.deferred_drops = deferred_drops;
    g.ended_pids = ended_pids;
    push_reserved(&mut g.ready[0], reaper_ref, "run queue (reaper install)");
    *g.cur_slot() = Some(boot_ref);
    g.threads = 2; // the boot thread and the reaper
    *g.idle_slot() = Some(idle_ref);
    g.set_idle_addr(idle_addr);
    g.quantum = [QUANTUM_TICKS; MAX_CPUS];
    Ok(())
}

/// Retire this AP's boot thread into the scheduler. Diverges.
///
/// **[`ap_init`] must have succeeded first.** They are separate so `ap_entry` reports the
/// CPU online only *after* its scheduler context exists: otherwise a failed init would leave
/// the BSP counting a CPU that never joined, and the machine would boot on with one fewer
/// core.
pub fn ap_run() -> ! {
    // SAFETY: this CPU's `current` (boot) and `idle` slots are set, so a timer
    // tick can schedule. Arm preemption, then retire the boot thread into the
    // run loop (the AP continues as its idle thread until `ready` has work).
    unsafe { Cpu::interrupts_enable() };
    exit_thread(ExitStatus { kind: ExitKind::Normal as u32, code: 0 });
}

/// Per-CPU scheduler-context setup for an AP: a boot thread (adopting the running context)
/// as `current`, plus this CPU's idle thread. The one-time global reserves were built by
/// [`init`] on the BSP.
pub fn ap_init() -> Result<(), AllocError> {
    // A fresh tid for this AP's (transient) boot thread; the idle thread reuses `IDLE_TID`.
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
    // Reserved outside the lock so neither `finish_exit`'s reap push nor a placement onto
    // this CPU allocates under `SCHED`.
    let (reap, ready) = (thread_list()?, thread_list()?);

    let mut g = SCHED.lock();
    *g.cur_slot() = Some(boot_ref);
    g.threads += 1; // this AP's boot thread, which exits into the reap list
    *g.idle_slot() = Some(idle_ref);
    g.set_idle_addr(idle_addr);
    *g.reap_slot() = reap;
    *g.ready_slot() = ready;
    // Mark *this* CPU's bit in the same critical section that reserved its queues, so no
    // placement targets it before they exist.
    let cpu = SchedState::this_cpu();
    g.cpu_online[cpu] = true;
    online_set::add(cpu); // lock-free mirror
    Ok(())
}

/// Create a runnable kernel thread that will run `entry(arg)` in the default **TimeShared**
/// class and enqueue it. Returns the new thread id.
pub fn spawn(entry: ThreadEntry, arg: usize) -> Result<u32, AllocError> {
    spawn_with_class(entry, arg, SchedClass::TimeShared, 0, 0)
}

/// Set a thread's CPU **affinity** mask (`sys_thread_set_affinity`). Takes effect at the
/// next placement / steal / wake; a thread running on a now-excluded CPU finishes its slice
/// there. The caller pins `thread` via a held handle ref.
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
pub fn spawn_with_affinity(entry: ThreadEntry, arg: usize, cpu_mask: u8) -> Result<u32, AllocError> {
    spawn_inner(entry, arg, SchedClass::TimeShared, 0, 0, cpu_mask)
}

/// As [`spawn`], but in scheduling `class` with `rt_priority` (RealTime) / `nice`
/// (TimeShared). Stack allocation and frame fabrication happen before the enqueue lock.
pub fn spawn_with_class(
    entry: ThreadEntry,
    arg: usize,
    class: SchedClass,
    rt_priority: u8,
    nice: i8,
) -> Result<u32, AllocError> {
    spawn_inner(entry, arg, class, rt_priority, nice, u8::MAX)
}

/// The shared body of every kernel-thread spawn: fabricate the thread, set its scheduling
/// parameters and affinity (`cpu_mask` `u8::MAX` = none), and place it on a run queue.
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
        // `place_thread` refuses rather than grow under the rank-1 lock, handing `r` back so
        // it drops below, lock-free.
        match place_thread(&mut g, r, false) {
            Ok(()) => return Ok(tid),
            Err(returned) => returned, // carry out of the locked block
        }
    };
    // Lock released: `leftover` drops here, freeing the kernel stack off the rank-1 lock.
    drop(leftover);
    Err(AllocError)
}

/// Create a **user** thread for `process` that descends to ring 3 at `entry` with stack
/// `user_sp`, and enqueue it. Returns a **cloned** [`ObjectRef`] (the `ready` entry holds its
/// own) so the caller can install a thread handle (`sys_thread_create`). The `process`
/// reference is moved into the thread, keeping its address space alive. `boot_args` are the
/// `[rdi, rsi, rdx, rcx]` seeded at first ring-3 entry (the spawn hand-off). Stack and frame
/// fabrication happen before the enqueue lock.
pub fn spawn_user(process: ObjectRef, entry: u64, user_sp: u64, boot_args: [u64; 4]) -> Result<ObjectRef, AllocError> {
    // Default scheduling: TimeShared, nice 0, no affinity restriction.
    spawn_user_sched(process, entry, user_sp, boot_args, SchedClass::TimeShared, 0, 0, u8::MAX)
}

/// As [`spawn_user`], but with explicit scheduling parameters (the `RealTime` class is
/// `REAL_TIME`-gated by the caller). They are set **before** the thread is enqueued, so it
/// never runs a tick under the wrong class.
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
        match place_spawned(&mut g, r) {
            Ok(()) => return Ok(handle),
            Err(returned) => returned, // carry out of the locked block
        }
    };
    // Over capacity: lock released; `leftover` and `handle` drop here, freeing the stack and
    // releasing the Process off the rank-1 lock.
    drop(leftover);
    Err(AllocError)
}

/// Place `r`, a thread just made for a process, marking it first if the process has begun to exit:
/// a thread placed after `exit_process` marked the others is marked here instead.
pub(super) fn place_spawned(g: &mut SchedState, r: ObjectRef) -> Result<(), ObjectRef> {
    // SAFETY: `r` pins the new thread; `SCHED` held.
    let th = unsafe { &*(r.as_ptr() as *const Thread) };
    // SAFETY: the clone pins the live `Process`; the thread holds another reference, so dropping
    // it frees nothing under `SCHED`.
    if th.process_ref().is_some_and(|p| unsafe { &*(p.as_ptr() as *const crate::object::Process) }.ending().is_some()) {
        // SAFETY: as above.
        unsafe { Thread::set_ending(r.as_ptr()) };
    }
    place_thread(g, r, false)
}

/// The idle thread body: reap any exited thread, then `hlt` until the next interrupt. Runs
/// with IF=1 (set by [`thread_trampoline`](crate::arch::thread_trampoline)), so the timer
/// wakes it and the tick reschedules to any ready thread. Never returns, so the idle thread
/// is never enqueued or reaped.
///
/// Reaping here (outside IRQ context, holding no other lock) is the safe home for draining
/// `reap` when the system would otherwise sit idle — e.g. the boot thread after it exits.
extern "C" fn idle_body(_arg: usize) {
    loop {
        reap_pending();
        // SAFETY: ring-0. `idle_halt` enables interrupts as it parks (`sti; hlt`),
        // so the CPU always sleeps with IF=1 — the periodic timer or a reschedule
        // IPI can then wake it, regardless of the inbound IF state.
        unsafe { Cpu::idle_halt() };
    }
}
