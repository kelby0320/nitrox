//! Exit, fault suspension and resumption, and the reaper.

use super::*;
use crate::arch::percpu::cpu_local;
use crate::libkern::IrqsOff;
use crate::libkern::SpinLock;
use crate::libkern::lockrank::LockRank;

/// The action a supervisor's `sys_exception_resume` requests for a thread suspended on a
/// fault. Only the two terminal dispositions exist; `ResumeSkip`/`ModifyAndResume` and the
/// debugger priority chain are deferred (`docs/spec/syscall-abi.md`,
/// `docs/architecture/notifications.md`) and the auto-terminate timeout is
/// `TODO(unsupervised-fault)` in `docs/rationale/deferred-decisions.md`.
pub enum ResumeDisposition {
    /// Re-enter the faulting instruction (tag `0`) with the frame unmodified, for a
    /// supervisor that has repaired the fault's cause; otherwise it simply re-faults.
    Resume,
    /// Terminate the thread with `code` (tag `2`) via [`exit_thread`] with a crashed status.
    Terminate(i32),
}

/// Deliver a `ChildExited { pid, status }` to `me`'s process's parent channel, if it has one,
/// and wake its waiters. Caller holds `SCHED`. Borrows the channel (no owned reference, so no
/// destructor under `SCHED`); enqueue is allocation-free.
fn deliver_child_exited(g: &mut SchedState, me: &Thread, status: ExitStatus) {
    // The parent's channel, kept alive by this process's `parent_notif` reference (still held:
    // `me` is not yet reaped).
    let info = me.process().and_then(|p| p.parent_notif().map(|chan| (chan, p.pid_u32())));
    if let Some((chan, child_pid)) = info {
        // No allocation: the queue is pre-reserved.
        let _edge = chan.enqueue(&mut g.tok, Notification::child_exited(child_pid, status));
        signal_channel(g, chan);
    }
}

/// `true` if any thread of process `my_pid` is still live anywhere the scheduler tracks:
/// `ready`, `blocked`, `suspended`, **or as another CPU's `current`**. The exiting thread is
/// already out of its own CPU's `current`; without the `current[]` scan a running sibling
/// would make this `false` and the process be torn down from under it. Caller holds `SCHED`.
pub(super) fn has_live_siblings(g: &SchedState, my_pid: u32) -> bool {
    let same = |r: &Ref<Thread>| r.owner_pid() == my_pid;
    g.ready.iter().any(|q| q.iter().any(same))
        || g.blocked.iter().any(same)
        || g.suspended.iter().any(same)
        || g.current.iter().flatten().any(same)
}

/// Whether process `pid` still has a thread the scheduler holds live. Once it has none its
/// handles are swept, or about to be, so a handle installed for it after the sweep is closed by
/// nothing: whoever installs one for another process checks this after allocating.
pub fn pid_has_threads(pid: u32) -> bool {
    has_live_siblings(&SCHED.lock(), pid)
}

cpu_local! {
    /// Per-CPU copy of the running thread's [`Thread::is_ending`], written under `SCHED` and read on
    /// the way to ring 3 without it, by its own CPU with interrupts masked.
    pub(super) static ENDING: AtomicBool = AtomicBool::new(false);
}

/// End the running thread if its process is exiting: called on every return to ring 3, with its
/// kernel stack unwound and nothing held.
pub fn end_if_ending() {
    let ending = {
        let off = IrqsOff::observed();
        ENDING.with(&off.pin(), |e| e.load(Ordering::Acquire))
    };
    if ending {
        // What makes freeing sound at an interrupt's tail (`lock-ordering.md`).
        crate::libkern::lockrank::assert_user_entry_safe();
        assert_user_entry_preempt_safe();
        // The status is the process's, recorded by the exit that marked the thread.
        exit_thread(ExitStatus { kind: ExitKind::Normal as u32, code: 0 });
    }
}

/// Set every other thread of process `pid` to end itself on its next return to ring 3: each is
/// marked, then woken, resumed or sent a reschedule IPI as it is blocked, suspended or running.
/// **None is freed here**: only its own unwinding lets go of what its kernel stack holds.
pub(super) fn end_siblings(g: &mut SchedState, pid: u32) {
    let same = |r: &Ref<Thread>| r.owner_pid() == pid;
    let me = g.me();
    for c in 0..MAX_CPUS {
        for r in g.ready[c].iter().filter(|r| same(r)) {
            r.set_ending(&mut g.tok);
        }
        if let Some(r) = g.current[c].as_ref().filter(|r| same(r)) {
            r.set_ending(&mut g.tok);
            ENDING.remote(c, |e| e.store(true, Ordering::Release));
            if c != me {
                crate::arch::send_reschedule_ipi(c);
            }
        }
    }
    while let Some(i) = g.blocked.iter().position(|r| same(r) && !r.is_ending(&g.tok)) {
        let th = &g.blocked[i];
        th.set_ending(&mut g.tok);
        if th.wait_try_wake() {
            let th: *const Thread = &**th;
            make_runnable(g, th);
        }
    }
    while let Some(i) = g.suspended.iter().position(same) {
        let r = g.suspended.remove(i);
        r.set_ending(&mut g.tok);
        resume_parked(g, r, DISP_TERMINATE, 0);
    }
}

/// If no other thread of the exiting thread's process is live, end the process: its handles go
/// to the reaper, woken at once rather than at this CPU's next idle, and its parent hears
/// `ChildExited` with the status a `sys_process_exit` recorded, else `status`.
pub(super) fn end_process_if_last(g: &mut SchedState, me: &Thread, status: ExitStatus) -> bool {
    let Some(process) = me.process() else { return false };
    let pid = me.owner_pid();
    if has_live_siblings(g, pid) {
        return false;
    }
    let status = process.ending().unwrap_or(status);
    me.set_process_ended();
    // A full queue leaves the sweep to this CPU's `reap_pending`, which `process_ended` asks for.
    if g.ended_pids.push(pid).is_ok() {
        wake_reaper(g);
    }
    deliver_child_exited(g, me, status);
    true
}

/// Park `me`, out of `current`, in this CPU's reap list and switch away forever. Its stack
/// cannot be freed here — this code is running on it — so the next scheduler entry on this CPU
/// reaps it; the switch loads the incoming root first, so CR3 is off a last thread's root before
/// its `AddressSpace::Drop` frees it. Shared tail of [`exit_thread`] and [`exit_process`].
fn finish_exit(g: IrqSpinLockGuard<'_, SchedState>, me: Outgoing) -> ! {
    switch_away(g, me, Park::Exited);
    unreachable!("switched away from an exited thread");
}

/// Terminate the **current thread** with exit `status`; the last of a process out ends it. Used by
/// `sys_thread_exit`, the resume-`Terminate` path, [`end_if_ending`] and kernel thread bodies.
pub fn exit_thread(status: ExitStatus) -> ! {
    // Reclaim any earlier exited thread first, so `reap` has room for us.
    reap_pending();

    let mut g = SCHED.lock();
    let me = take_current(&mut g);
    end_process_if_last(&mut g, &me, status);
    finish_exit(g, me);
}

/// Terminate the **whole process** of the current thread with `status`: record it, set every
/// sibling to end itself ([`end_siblings`]) and exit. Used by `sys_process_exit`.
pub fn exit_process(status: ExitStatus) -> ! {
    // Reclaim any earlier exited thread first, so `reap` has room.
    reap_pending();

    let mut g = SCHED.lock();
    let me = take_current(&mut g);
    if let Some(process) = me.process() {
        process.begin_ending(status);
        end_siblings(&mut g, me.owner_pid());
    }
    end_process_if_last(&mut g, &me, status);
    finish_exit(g, me);
}

/// Queue a fault's notification on its process's channel, if it has one: whether that woke a
/// thread waiting to hear it, wherever the thread then runs. Caller holds `SCHED`.
pub(super) fn deliver_fault(g: &mut SchedState, chan: Option<&NotificationChannel>, notif: Notification) -> bool {
    let Some(c) = chan else { return false };
    let _edge = c.enqueue(&mut g.tok, notif);
    signal_channel(g, c)
}

/// Say that a ring-3 fault **woke no waiting thread**: unless one reads the notification later
/// and calls `sys_exception_resume`, the faulting thread stays suspended (notably an init crash).
///
/// Written under `SERIAL`, taken after `SCHED` as their ranks order, so it never interleaves
/// with a line another CPU is printing. Reads only neutral data, not the `ExceptionFrame`.
fn report_unwaited_fault(me: &Thread, notif: &Notification) {
    use crate::libkern::notification::{KIND_DIVIDE_BY_ZERO, KIND_ILLEGAL_INSN, KIND_SEG_FAULT, KIND_STACK_OVERFLOW};
    let (pid, tid) = (me.owner_pid(), me.tid());
    let kind = match notif.kind() {
        KIND_SEG_FAULT => "segfault",
        KIND_ILLEGAL_INSN => "illegal-instruction",
        KIND_DIVIDE_BY_ZERO => "divide-by-zero",
        KIND_STACK_OVERFLOW => "stack-overflow",
        _ => "fault",
    };
    crate::kprintln!(
        "\n*** unhandled ring-3 fault (no thread waiting to resume it): pid {} tid {} {} @ {:#018x} ***",
        pid,
        tid,
        kind,
        notif.fault_addr(),
    );
}

/// Suspend the **current thread** after a ring-3 fault: deliver `notif` to its process's
/// notification channel (waking the supervisor), record the user registers `regs` it faulted
/// with, park in `suspended`, and switch away. Returns the [`ResumeDisposition`] the supervisor
/// chose once the thread is runnable again.
pub fn suspend_with_fault(regs: crate::arch::RegisterValues, notif: Notification) -> ResumeDisposition {
    // Reclaim earlier exited threads first, off the rank-1 lock.
    reap_pending();

    {
        let mut g = SCHED.lock();
        let (cur, tok) = g.running();
        if cur.is_ending(tok) {
            // Its process is exiting, and resumes nothing: it terminates instead of parking.
            return ResumeDisposition::Terminate(0);
        }
        let me = take_current(&mut g);

        // Deliver the fault notification (borrowed — no destructor under `SCHED`) on the channel
        // the current Process owns a ref on, alive at least until this thread is reaped.
        let chan = me.process().and_then(|p| p.notification_channel());
        if !deliver_fault(&mut g, chan, notif) {
            report_unwaited_fault(&me, &notif);
        }

        // Record the registers and park off the run queue; the `ExceptionFrame` stays on this
        // frozen kernel stack until resume.
        // TODO(unsupervised-fault): nothing supervises an ordinary program, so with
        // the thirty-second auto-terminate deferred its thread stays here for the
        // boot — first reached by a pulled stick's failed fill.
        me.set_fault_registers(&mut g.tok, regs);
        // Resumes here when `sys_exception_resume` moves us `suspended`→`ready`.
        switch_away(g, me, Park::Suspended);
    }

    // Read the disposition the resume stored on us, back in this CPU's `current`.
    let (tag, code) = {
        let mut g = SCHED.lock();
        let (me, tok) = g.running();
        me.take_disposition(tok)
    };
    match tag {
        DISP_TERMINATE => ResumeDisposition::Terminate(code),
        _ => ResumeDisposition::Resume,
    }
}

/// The disposition tag that terminates a resumed thread ([`ResumeDisposition::Terminate`]).
pub(super) const DISP_TERMINATE: u8 = 2;

/// Resume a thread suspended on a fault: record the supervisor's disposition on it and move
/// it `suspended`→`ready`. Returns `false` (no-op) if `thread` is not `Suspended` — the
/// backstop `sys_exception_resume` maps to `InvalidArgument`. Takes `SCHED` itself.
pub fn resume_suspended(thread: &Ref<Thread>, disp_tag: u8, disp_code: i32) -> bool {
    let mut g = SCHED.lock();
    let thread = thread.as_ptr();
    let Some(i) = g.suspended.iter().position(|r| r.as_ptr() == thread) else {
        return false;
    };
    let r = g.suspended.remove(i);
    resume_parked(&mut g, r, disp_tag, disp_code);
    true
}

/// Record a disposition on `r`, just taken out of `suspended`, and place it to run.
fn resume_parked(g: &mut SchedState, r: Ref<Thread>, disp_tag: u8, disp_code: i32) {
    // Read by `suspend_with_fault` when this thread next runs.
    r.set_disposition(&mut g.tok, disp_tag, disp_code);
    r.set_state(&mut g.tok, ThreadState::Ready);
    // Re-home on its CPU (a wake; a full home falls back to a permitted queue with room).
    place_woken(g, r, "resume placement (every queue the resumed thread may go to)");
}

/// The user registers `thread` faulted with, or `None` if it is not `Suspended`: a copy, taken
/// under `SCHED`, so a resume that races the caller's copy-out cannot change it.
pub fn suspended_registers(thread: &Ref<Thread>) -> Option<crate::arch::RegisterValues> {
    let g = SCHED.lock();
    (thread.state(&g.tok) == ThreadState::Suspended).then(|| thread.fault_registers(&g.tok))
}

/// Pop up to `buf.len()` parked refs — this CPU's `reap` list first, then the global
/// [`SchedState::deferred_drops`] — into `buf`. **Moves only**: no drop, no allocation, and
/// the lists keep their reserved buffers. The caller drops the refs after releasing `SCHED`, in
/// thread context.
pub(super) fn drain_pending_drops(g: &mut SchedState, buf: &mut [Option<ObjectRef>]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        let Some(r) = g.reap_slot().pop() else { break };
        g.threads = g.threads.saturating_sub(1);
        buf[n] = Some(r.erase());
        n += 1;
    }
    while n < buf.len() {
        let Some(r) = g.deferred_drops.pop() else { break };
        buf[n] = Some(r);
        n += 1;
    }
    n
}

/// **The drivers' reclaims** of what their DPCs gave back, run by [`reap_pending`]: each joins once,
/// at its driver's start.
static RECLAIMS: Reclaims = Reclaims::new();

/// A driver's reclaim.
type Reclaim = fn();

/// A table of reclaims, four places.
pub(super) struct Reclaims(SpinLock<[Option<Reclaim>; 4]>);

impl Reclaims {
    pub(super) const fn new() -> Reclaims {
        Reclaims(SpinLock::new(LockRank::Leaf, [None; 4]))
    }

    /// `reclaim` in the first free place; `false` if every place is taken.
    pub(super) fn register(&self, reclaim: Reclaim) -> bool {
        let mut r = self.0.lock();
        let Some(slot) = r.iter_mut().find(|s| s.is_none()) else { return false };
        *slot = Some(reclaim);
        true
    }

    /// Run every one registered, the table let go of first: a reclaim frees.
    pub(super) fn run(&self) {
        let reclaims = *self.0.lock();
        for reclaim in reclaims.into_iter().flatten() {
            reclaim();
        }
    }
}

/// **Have [`reap_pending`] run `reclaim`**, in thread context; `false` if every place is taken.
pub fn register_reclaim(reclaim: Reclaim) -> bool {
    RECLAIMS.register(reclaim)
}

/// Drop every pending reaped thread and deferred-drop ref **outside** the run-queue lock, in
/// thread context (a reaped thread's `KernelStack` `Drop` takes rank-6 allocator locks and
/// starts a TLB shootdown). Idempotent; called by `yield_now`, `exit_thread`,
/// `exit_process`, `suspend_with_fault`, the idle loop and the boot drainer. Only this CPU's
/// reap list is drained — a thread is reclaimed by the CPU it died on, after its
/// `switch_into` completed — plus the CPU-agnostic [`SchedState::deferred_drops`].
pub fn reap_pending() {
    // Completed block IRPs, whose boxes their DPC deliberately did not free: a free reaches
    // the plain-spinlock allocator, which must not happen in IRQ or DPC context. Done first
    // so the frees happen even with nothing else to reap.
    crate::io::block::reclaim_completed();
    // The char drivers park their completed reads for the same reason.
    RECLAIMS.run();
    loop {
        // A batch a lock hold, on this stack; the loop goes round for the rest.
        let mut buf: [Option<ObjectRef>; 64] = [const { None }; 64];
        let n = {
            let mut g = SCHED.lock();
            drain_pending_drops(&mut g, &mut buf)
        };
        // **Before** the drops below, so an ended process's pipes close (waking peers) as
        // promptly as the reap runs.
        close_ended_processes(&buf[..n]);

        // Drop with preemption disabled: the drops take the allocator locks and (for a kernel
        // stack) run a TLB shootdown — plain locks other CPUs may spin on. Being descheduled
        // while holding one starves the spinners for a round — or forever when the reaper is
        // the **idle** thread, never re-picked while the spinners keep every CPU busy.
        // Interrupts stay enabled; only the switch is deferred.
        let off = preempt_disable();
        for slot in buf[..n].iter_mut() {
            drop(slot.take());
        }
        drop(off);
        if n < buf.len() {
            return; // drained everything that was parked when we looked
        }
        // Filled the batch: go around for the rest.
    }
}

/// Close the handles of each process whose last thread is in `batch`, in thread context with no
/// `SCHED`: the exiting thread holds it and never returns from `finish_exit`.
pub(super) fn close_ended_processes(batch: &[Option<ObjectRef>]) {
    for th in batch.iter().flatten().filter_map(ObjectRef::downcast_ref::<Thread>) {
        let (ended, pid) = (th.process_ended(), th.owner_pid());
        // `pid == 0` is a kernel/idle thread, which owns no handles and whose pid must never be
        // swept.
        if ended && pid != 0 {
            crate::handle::global::close_all_owned_by(pid);
        }
    }
}

/// The reaper thread body: close the handle tables of processes that have exited, and drain the
/// completed-IRP list when no idle CPU has, then park until there is more.
///
/// **Why a thread at all.** If reclamation ran only at `exit_*`/`yield_now`/
/// `suspend_with_fault` and in the idle loop, a system where nothing exits or yields and no
/// CPU reaches idle (one busy-looping service is enough) would never reclaim an exited
/// process, and every pipe it held would stay open: a peer blocked on a channel waits for a
/// `PeerClosed` only reclamation can deliver. A schedulable thread removes the dependency
/// on scheduling luck.
///
/// Kernel stacks stay with the per-CPU [`reap`](SchedState::reap) path: a thread parks itself
/// there and then switches off its own stack, so freeing it from another CPU is a use-after-free,
/// and the reaper drains only the list of the CPU it runs on.
pub(super) extern "C" fn reaper_body(_arg: usize) {
    loop {
        if crate::io::block::reclaim_backlog() > 0 {
            reap_pending();
        }
        // Take one pid at a time: `close_all_owned_by` drops object references, which must
        // happen with `SCHED` released.
        let next = {
            let mut g = SCHED.lock();
            g.ended_pids.pop()
        };
        match next {
            Some(pid) => {
                crate::handle::global::close_all_owned_by(pid);
                REAPER_CLOSED.fetch_add(1, Ordering::Relaxed);
            }
            None => park_reaper(),
        }
    }
}

/// Park the reaper until [`wake_reaper`] runs. Re-checks its work **under `SCHED`** before
/// parking, which closes the lost-wakeup window: a pid pushed, or a completed IRP's box parked,
/// between the reaper's last drain and this park would otherwise wake nothing.
fn park_reaper() {
    let mut g = SCHED.lock();
    if !g.ended_pids.is_empty() || crate::io::block::reclaim_backlog() > 0 {
        return; // work arrived while we were draining — go round again
    }
    // Its own slot — not `blocked`, so no generic waker can find it, and not `ready`, so it does
    // not spin. Resumes here, lock-free, when `wake_reaper` re-queues it.
    let me = take_current(&mut g);
    switch_away(g, me, Park::Reaper);
}

/// Wake the reaper to drain the completed-IRP list: from a DPC, holding nothing.
pub fn wake_reaper_to_reclaim() {
    wake_reaper(&mut SCHED.lock());
}

/// Make the reaper runnable, if it is parked. Caller holds `SCHED`. A no-op when it is
/// already running or queued — the wake is level-triggered off [`SchedState::ended_pids`],
/// so a missed edge costs nothing.
///
/// **The only placement site that bypasses the policy.** It pushes onto `ready[me()]`
/// directly: no affinity check and no [`cpu_accepts_work`]. Sound today because a CPU
/// executing this code is by definition not parked, so the CPU it chooses always accepts
/// work — but if placement policy grows a rule, this is the site that will not have it.
fn wake_reaper(g: &mut SchedState) {
    let Some(r) = g.reaper.take() else {
        return;
    };
    r.set_state(&mut g.tok, ThreadState::Ready);
    let cpu = g.me();
    push_reserved(&mut g.ready[cpu], r, "run queue (reaper wake)");
}
