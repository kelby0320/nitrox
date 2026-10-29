//! Exit, fault suspension and resumption, and the reaper.

use super::*;

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

/// Deliver a `ChildExited { pid, status }` to `me_obj`'s process's parent channel, if it has
/// one, and wake its waiters. Caller holds `SCHED`. Borrows the channel pointer (no owned
/// `ObjectRef`, so no destructor under `SCHED`); enqueue is allocation-free.
fn deliver_child_exited(g: &mut SchedState, me_obj: *mut (), status: ExitStatus) {
    let info: Option<(*mut (), u32)> = {
        // SAFETY: `me_obj` is the running thread, pinned, lock held.
        let th = unsafe { &*(me_obj as *const Thread) };
        th.process_ref().and_then(|p| {
            // SAFETY: `p` pins a live Process; read its parent channel + pid.
            let proc = unsafe { &*(p.as_ptr() as *const crate::object::Process) };
            proc.parent_notif_ptr().map(|chan| (chan, proc.pid_u32()))
            // `p` (a cloned ObjectRef, never the last) drops here: a plain atomic
            // decrement, no destructor, safe under SCHED.
        })
    };
    if let Some((chan, child_pid)) = info {
        let notif = Notification::child_exited(child_pid, status);
        // SAFETY: `chan` is the parent's channel, kept alive by this process's
        // `parent_notif` reference (still held — `me` is not yet reaped); SCHED
        // held; no allocation (queue pre-reserved).
        let _edge = unsafe { NotificationChannel::enqueue(chan, notif) };
        signal_channel(g, chan);
    }
}

/// `true` if any thread of process `my_pid` is still live anywhere the scheduler tracks:
/// `ready`, `blocked`, `suspended`, **or as another CPU's `current`**. The exiting thread is
/// already out of its own CPU's `current`; without the `current[]` scan a running sibling
/// would make this `false` and the process be torn down from under it. Caller holds `SCHED`.
pub(super) fn has_live_siblings(g: &SchedState, my_pid: u32) -> bool {
    // SAFETY: each entry pins a live Thread; `SCHED` held — a shared read of
    // `owner_pid` is sound (no `&mut` taken).
    let same = |r: &ObjectRef| unsafe { &*(r.as_ptr() as *const Thread) }.owner_pid() == my_pid;
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

/// Per-CPU copy of the running thread's [`Thread::is_ending`], written under `SCHED` and read on
/// the way to ring 3 without it, by its own CPU with interrupts masked.
#[cfg(not(test))]
static ENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

#[cfg(test)]
std::thread_local! {
    /// Host-test backing for [`ENDING`], one set per test thread, as `preempt`'s flags are.
    static ENDING_TLS: [AtomicBool; MAX_CPUS] = const { [const { AtomicBool::new(false) }; MAX_CPUS] };
}

/// CPU `cpu`'s copy of its running thread's mark. See [`ENDING`].
#[cfg(not(test))]
#[inline(always)]
pub(super) fn with_ending<R>(cpu: usize, f: impl FnOnce(&AtomicBool) -> R) -> R {
    f(&ENDING[cpu])
}

#[cfg(test)]
pub(super) fn with_ending<R>(cpu: usize, f: impl FnOnce(&AtomicBool) -> R) -> R {
    ENDING_TLS.with(|a| f(&a[cpu]))
}

/// End the running thread if its process is exiting: called on every return to ring 3, with its
/// kernel stack unwound and nothing held.
pub fn end_if_ending() {
    if with_ending(SchedState::this_cpu(), |e| e.load(Ordering::Acquire)) {
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
    // SAFETY: each entry pins a live Thread; `SCHED` held.
    let same = |r: &ObjectRef| unsafe { &*(r.as_ptr() as *const Thread) }.owner_pid() == pid;
    let me = SchedState::this_cpu();
    for c in 0..MAX_CPUS {
        for r in g.ready[c].iter().filter(|r| same(r)) {
            // SAFETY: `r` pins the thread; `SCHED` held.
            unsafe { Thread::set_ending(r.as_ptr()) };
        }
        if let Some(r) = g.current[c].as_ref().filter(|r| same(r)) {
            // SAFETY: as above.
            unsafe { Thread::set_ending(r.as_ptr()) };
            with_ending(c, |e| e.store(true, Ordering::Release));
            if c != me {
                crate::arch::send_reschedule_ipi(c);
            }
        }
    }
    // SAFETY: as `same`.
    let unmarked = |r: &ObjectRef| same(r) && !unsafe { Thread::is_ending(r.as_ptr()) };
    while let Some(i) = g.blocked.iter().position(unmarked) {
        let th = g.blocked[i].as_ptr();
        // SAFETY: `blocked` pins the thread, blocked in `wait_on`; `SCHED` held.
        unsafe {
            Thread::set_ending(th);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
            }
        }
    }
    while let Some(i) = g.suspended.iter().position(same) {
        let r = g.suspended.remove(i);
        // SAFETY: `r` pins the thread; `SCHED` held.
        unsafe { Thread::set_ending(r.as_ptr()) };
        resume_parked(g, r, DISP_TERMINATE, 0);
    }
}

/// If no other thread of the exiting thread's process is live, end the process: its handles go
/// to the reaper, woken at once rather than at this CPU's next idle, and its parent hears
/// `ChildExited` with the status a `sys_process_exit` recorded, else `status`.
pub(super) fn end_process_if_last(g: &mut SchedState, me_obj: *mut (), status: ExitStatus) -> bool {
    // SAFETY: `me_obj` is the exiting thread, pinned by its caller; `SCHED` held.
    let th = unsafe { &*(me_obj as *const Thread) };
    let Some(process) = th.process_ref() else {
        return false;
    };
    if has_live_siblings(g, th.owner_pid()) {
        return false;
    }
    // SAFETY: `process` pins the live `Process`. Dropping this clone is never the last
    // reference (the thread holds one), so it frees nothing under `SCHED`.
    let status = unsafe { &*(process.as_ptr() as *const crate::object::Process) }.ending().unwrap_or(status);
    // SAFETY: as above.
    unsafe { Thread::set_process_ended(me_obj) };
    // A full queue leaves the sweep to this CPU's `reap_pending`, which `process_ended` asks for.
    if g.ended_pids.push(th.owner_pid()).is_ok() {
        wake_reaper(g);
    }
    deliver_child_exited(g, me_obj, status);
    true
}

/// Mark `me` (the current thread, already out of `current`) `Exited`, park it in `reap`, and
/// switch away forever. Its stack cannot be freed here — this code is running on it — so the
/// next scheduler entry reaps it. Shared tail of [`exit_thread`] and [`exit_process`].
fn finish_exit(mut g: IrqSpinLockGuard<'_, SchedState>, me: ObjectRef) -> ! {
    let me_obj = me.as_ptr();
    // SAFETY: `me` is the running thread, pinned, lock held. (The idle thread
    // never exits, so `me` is never the idle thread.)
    unsafe { Thread::set_state(me_obj, ThreadState::Exited) };
    // SAFETY: as above.
    let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
    // Park self for deferred reclamation.
    push_reserved(g.reap_slot(), me, "reap list (self)");

    // Switch to the next ready thread, else the idle thread (parked here, since `me` was
    // current, not idle).
    let next = match pick_next(&mut g) {
        Some(n) => n,
        None => g.idle_slot().take().expect("idle thread exists after init"),
    };
    let next_obj = next.as_ptr();
    // SAFETY: next is pinned, becoming current; lock held.
    unsafe { Thread::set_state(next_obj, ThreadState::Running) };
    *g.cur_slot() = Some(next);

    // Switch away forever. `switch_into` loads the incoming root before the stack swap, so
    // when the last user thread exits CR3 is off its root before this thread is reaped (its
    // `AddressSpace::Drop` frees the PML4).
    // SAFETY: `me_slot` is our own (now-`Exited`, pinned-in-`reap`) saved-SP
    // slot — written by the switch and never read again; `me_obj`/`next_obj` is the
    // pinned incoming thread. We never resume, so the restore inside
    // `switch_into` is never reached.
    unsafe { switch_into(g, me_slot, me_obj, next_obj) };
    unreachable!("switched away from an exited thread");
}

/// Terminate the **current thread** with exit `status`; the last of a process out ends it. Used by
/// `sys_thread_exit`, the resume-`Terminate` path, [`end_if_ending`] and kernel thread bodies.
pub fn exit_thread(status: ExitStatus) -> ! {
    // Reclaim any earlier exited thread first, so `reap` has room for us.
    reap_pending();

    let mut g = SCHED.lock();
    let me = g.cur_slot().take().expect("current set");
    end_process_if_last(&mut g, me.as_ptr(), status);
    finish_exit(g, me);
}

/// Terminate the **whole process** of the current thread with `status`: record it, set every
/// sibling to end itself ([`end_siblings`]) and exit. Used by `sys_process_exit`.
pub fn exit_process(status: ExitStatus) -> ! {
    // Reclaim any earlier exited thread first, so `reap` has room.
    reap_pending();

    let mut g = SCHED.lock();
    let me = g.cur_slot().take().expect("current set");
    // SAFETY: `me` is the running thread, pinned, lock held.
    let th = unsafe { &*(me.as_ptr() as *const Thread) };
    if let Some(process) = th.process_ref() {
        // SAFETY: `process` pins the live `Process`; dropping this clone frees nothing (the
        // thread holds a reference), so it may drop under `SCHED`.
        unsafe { &*(process.as_ptr() as *const crate::object::Process) }.begin_ending(status);
        end_siblings(&mut g, th.owner_pid());
    }
    end_process_if_last(&mut g, me.as_ptr(), status);
    finish_exit(g, me);
}

/// Emit a last-ditch kernel diagnostic for a ring-3 fault that **stranded the scheduler**:
/// no runnable thread is left to receive the fault notification and call
/// `sys_exception_resume`, so the system would idle forever (notably an init crash). Called
/// only from the no-runnable-thread branch of [`suspend_with_fault`].
///
/// Uses the unsynchronized emergency serial writer, lock-free like `dump_and_halt`. Sound
/// here: a userspace fault never holds `SERIAL`, and the caller holds `SCHED` so interrupts
/// are masked. Reads only neutral data, staying out of the arch-private `ExceptionFrame`.
fn report_stranded_fault(me_obj: *mut (), notif: &Notification) {
    use crate::libkern::notification::{KIND_DIVIDE_BY_ZERO, KIND_ILLEGAL_INSN, KIND_SEG_FAULT, KIND_STACK_OVERFLOW};
    use core::fmt::Write;
    // SAFETY: `me_obj` is the running (faulting) thread, pinned, `SCHED` held.
    let th = unsafe { &*(me_obj as *const Thread) };
    let kind = match notif.kind() {
        KIND_SEG_FAULT => "segfault",
        KIND_ILLEGAL_INSN => "illegal-instruction",
        KIND_DIVIDE_BY_ZERO => "divide-by-zero",
        KIND_STACK_OVERFLOW => "stack-overflow",
        _ => "fault",
    };
    let mut w = crate::arch::serial::emergency_writer();
    let _ = writeln!(
        w,
        "\n*** unhandled ring-3 fault (no thread left to resume it): pid {} tid {} {} @ {:#018x} ***",
        th.owner_pid(),
        th.tid(),
        kind,
        notif.fault_addr(),
    );
}

/// Suspend the **current thread** after a ring-3 fault: deliver `notif` to its process's
/// notification channel (waking the supervisor), record the `ExceptionFrame` at `frame_ptr`
/// on this thread's kernel stack, park in `suspended`, and switch away. Returns the
/// [`ResumeDisposition`] the supervisor chose once the thread is runnable again.
pub fn suspend_with_fault(frame_ptr: usize, notif: Notification) -> ResumeDisposition {
    // Reclaim earlier exited threads first, off the rank-1 lock.
    reap_pending();

    let me_obj;
    {
        let mut g = SCHED.lock();
        let cur = g.cur_ref().as_ref().expect("current set").as_ptr();
        // SAFETY: the running thread, pinned by `current`; `SCHED` held.
        if unsafe { Thread::is_ending(cur) } {
            // Its process is exiting, and resumes nothing: it terminates instead of parking.
            return ResumeDisposition::Terminate(0);
        }
        let me = g.cur_slot().take().expect("current set");
        me_obj = me.as_ptr();

        // Deliver the fault notification (borrowed pointer — no destructor under `SCHED`).
        let chan: Option<*mut ()> = {
            // SAFETY: `me` is the running thread, pinned, lock held.
            let th = unsafe { &*(me_obj as *const Thread) };
            th.process_ref().and_then(|p| {
                // SAFETY: `p` pins a live Process; read its channel pointer.
                let proc = unsafe { &*(p.as_ptr() as *const crate::object::Process) };
                proc.notification_channel_ptr()
            })
        };
        if let Some(c) = chan {
            // SAFETY: `c` is the channel the current Process owns a ref on (alive
            // at least until this thread is reaped); SCHED held; queue
            // pre-reserved.
            let _edge = unsafe { NotificationChannel::enqueue(c, notif) };
            signal_channel(&mut g, c);
        }

        // Record the frame, mark Suspended, park off the run queue; the `ExceptionFrame`
        // stays on this frozen kernel stack until resume.
        // TODO(unsupervised-fault): nothing supervises an ordinary program, so with
        // the thirty-second auto-terminate deferred its thread stays here for the
        // boot — first reached by a pulled stick's failed fill.
        // SAFETY: `me` is the running thread, pinned, lock held.
        unsafe {
            Thread::set_exception_frame(me_obj, frame_ptr);
            Thread::set_state(me_obj, ThreadState::Suspended);
        }
        // SAFETY: as above.
        let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
        push_reserved(&mut g.suspended, me, "suspended list");

        // Switch to the next runnable thread. If none, this fault **stranded the scheduler**
        // (see `report_stranded_fault`). A serviced fault — the supervisor's waiter was just
        // woken by `signal_channel` above — takes the `Some` branch and stays quiet.
        let next = match pick_next(&mut g) {
            Some(n) => n,
            None => {
                report_stranded_fault(me_obj, &notif);
                g.idle_slot().take().expect("idle thread exists after init")
            }
        };
        let next_obj = next.as_ptr();
        // SAFETY: next is pinned, becoming current; lock held.
        unsafe { Thread::set_state(next_obj, ThreadState::Running) };
        *g.cur_slot() = Some(next);

        // We resume here when `sys_exception_resume` moves us `suspended`→`ready`.
        // SAFETY: `me_slot` is our own (now-`Suspended`, pinned-in-`suspended`)
        // saved-SP slot; `me_obj`/`next_obj` are the pinned outgoing / incoming
        // threads.
        unsafe { switch_into(g, me_slot, me_obj, next_obj) };
    }

    // Read (and clear) the disposition the resume stored on us.
    let (tag, code) = {
        let _g = SCHED.lock();
        // SAFETY: we run on `me_obj` again (back in `current`), `SCHED` held.
        unsafe { Thread::take_disposition(me_obj) }
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
///
/// # Safety
///
/// `thread` points at a live `Thread`, pinned for the call by a reference the caller holds.
pub unsafe fn resume_suspended(thread: *mut (), disp_tag: u8, disp_code: i32) -> bool {
    let mut g = SCHED.lock();
    let Some(i) = g.suspended.iter().position(|r| r.as_ptr() == thread) else {
        return false;
    };
    let r = g.suspended.remove(i);
    resume_parked(&mut g, r, disp_tag, disp_code);
    true
}

/// Record a disposition on `r`, just taken out of `suspended`, and place it to run.
fn resume_parked(g: &mut SchedState, r: ObjectRef, disp_tag: u8, disp_code: i32) {
    let thread = r.as_ptr();
    // SAFETY: `r` pins `thread`; `SCHED` held. The disposition is read by
    // `suspend_with_fault` when this thread next runs.
    unsafe {
        Thread::set_disposition(thread, disp_tag, disp_code);
        Thread::set_state(thread, ThreadState::Ready);
    }
    // Re-home on its CPU (a wake; a full home falls back to a permitted queue with room).
    // Fails only when every permitted queue is at reserve.
    if let Err(returned) = place_thread(g, r, true) {
        // **Do not let `returned` drop here**: `SCHED` is held and an `ObjectRef` drop can
        // reach the allocator's plain spinlock. Leaked deliberately (`panic = "abort"`); see
        // `make_runnable` for the full argument.
        core::mem::forget(returned);
        panic!(
            "resume placement: no ready queue would take the thread — every affinity-permitted \
             queue is at reserve; or affinity permits no CPU that accepts work and every CPU \
             that does is at reserve; or no CPU accepts work and this one is at reserve"
        );
    }
}

/// The kernel-stack address of `thread`'s captured `ExceptionFrame`, or `None` if it is not
/// `Suspended`. Used by `sys_thread_get_registers`; a suspended thread stays parked, so the
/// frame is stable after this returns (the caller drops `SCHED` before the user copy-out).
///
/// # Safety
///
/// `thread` points at a live `Thread`, pinned for the call by a reference the caller holds.
pub unsafe fn thread_exception_frame(thread: *mut ()) -> Option<usize> {
    let _g = SCHED.lock();
    // SAFETY: the caller pins `thread` via its Thread handle; `SCHED` held.
    if unsafe { Thread::state_now(thread) } != ThreadState::Suspended {
        return None;
    }
    // SAFETY: same.
    unsafe { Thread::exception_frame(thread) }
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
        buf[n] = Some(r);
        n += 1;
    }
    while n < buf.len() {
        let Some(r) = g.deferred_drops.pop() else { break };
        buf[n] = Some(r);
        n += 1;
    }
    n
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
    crate::drivers::console::reclaim_completed();
    crate::drivers::ps2::reclaim_completed();
    crate::drivers::xhci::hid::reclaim_completed();
    loop {
        // A batch a lock hold, on this stack; the loop goes round for the rest.
        let mut buf: [Option<ObjectRef>; 64] = [const { None }; 64];
        let n = {
            let mut g = SCHED.lock();
            drain_pending_drops(&mut g, &mut buf)
        };
        // Close the handles of any process whose last thread is in this batch **before** the
        // drops below, so its pipes close (waking peers) as promptly as the reap runs. Thread
        // context, no `SCHED`; the exiting thread could not do it — it holds `SCHED` and never
        // returns from `finish_exit`.
        for slot in buf[..n].iter() {
            let Some(r) = slot else { continue };
            if r.object_type() != KObjectType::Thread {
                continue;
            }
            // SAFETY: the `ObjectRef` pins this `Thread`; nothing else mutates a
            // reaped thread's fields.
            let (ended, pid) = unsafe {
                let t = &*(r.as_ptr() as *const Thread);
                (Thread::process_ended(r.as_ptr()), t.owner_pid())
            };
            // `pid == 0` is a kernel/idle thread, which owns no handles and whose pid must
            // never be swept.
            if ended && pid != 0 {
                crate::handle::global::close_all_owned_by(pid);
            }
        }

        // Drop with preemption disabled: the drops take the allocator locks and (for a kernel
        // stack) run a TLB shootdown — plain locks other CPUs may spin on. Being descheduled
        // while holding one starves the spinners for a round — or forever when the reaper is
        // the **idle** thread, never re-picked while the spinners keep every CPU busy.
        // Interrupts stay enabled; only the switch is deferred.
        preempt_disable();
        for slot in buf[..n].iter_mut() {
            drop(slot.take());
        }
        preempt_enable();
        if n < buf.len() {
            return; // drained everything that was parked when we looked
        }
        // Filled the batch: go around for the rest.
    }
}

/// The reaper thread body: close the handle tables of processes that have exited, then
/// park until there are more.
///
/// **Why a thread at all.** If reclamation ran only at `exit_*`/`yield_now`/
/// `suspend_with_fault` and in the idle loop, a system where nothing exits or yields and no
/// CPU reaches idle (one busy-looping service is enough) would never reclaim an exited
/// process, and every pipe it held would stay open: a peer blocked on a channel waits for a
/// `PeerClosed` only reclamation can deliver. A schedulable thread removes the dependency
/// on scheduling luck.
///
/// It closes **handle tables only**. Kernel stacks stay with the per-CPU [`reap`](SchedState::reap)
/// path: a thread parks itself there and then switches off its own stack, so freeing it
/// from another CPU is a use-after-free, and the reaper runs wherever it is scheduled.
pub(super) extern "C" fn reaper_body(_arg: usize) {
    loop {
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

/// Park the reaper until [`wake_reaper`] runs. Re-checks the queue **under `SCHED`** before
/// parking, which closes the lost-wakeup window: a pid pushed between the reaper's last
/// drain and this park would otherwise wake nothing.
fn park_reaper() {
    let mut g = SCHED.lock();
    if !g.ended_pids.is_empty() {
        return; // work arrived while we were draining — go round again
    }
    let me = g.cur_slot().take().expect("current set");
    let me_obj = me.as_ptr();
    let next = match pick_next(&mut g) {
        Some(n) => n,
        None => g.idle_slot().take().expect("idle thread exists after init"),
    };
    let next_obj = next.as_ptr();
    // SAFETY: both pinned (self parked in the reaper slot, next becoming current);
    // `SCHED` held — the Thread accessor contract.
    unsafe {
        Thread::set_state(me_obj, ThreadState::Blocked);
        Thread::set_state(next_obj, ThreadState::Running);
    }
    // SAFETY: as above.
    let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
    // Park in the reaper slot — not `blocked`, so no generic waker can find it, and not
    // `ready`, so it does not spin. `wake_reaper` is the only way back.
    debug_assert!(g.reaper.is_none());
    g.reaper = Some(me);
    *g.cur_slot() = Some(next);
    // Resumes here, lock-free, when `wake_reaper` re-queues us.
    // SAFETY: `me_slot` is our own (now-parked) saved-SP slot; both threads pinned.
    unsafe { switch_into(g, me_slot, me_obj, next_obj) };
}

/// Make the reaper runnable, if it is parked. Caller holds `SCHED`. A no-op when it is
/// already running or queued — the wake is level-triggered off [`SchedState::ended_pids`],
/// so a missed edge costs nothing.
///
/// **The only placement site that bypasses the policy.** It pushes onto `ready[this_cpu()]`
/// directly: no affinity check and no [`cpu_accepts_work`]. Sound today because a CPU
/// executing this code is by definition not parked, so the CPU it chooses always accepts
/// work — but if placement policy grows a rule, this is the site that will not have it.
fn wake_reaper(g: &mut SchedState) {
    let Some(r) = g.reaper.take() else {
        return;
    };
    // SAFETY: `r` is the parked reaper, pinned by the slot's reference; `SCHED` held.
    unsafe { Thread::set_state(r.as_ptr(), ThreadState::Ready) };
    let cpu = SchedState::this_cpu();
    push_reserved(&mut g.ready[cpu], r, "run queue (reaper wake)");
}
