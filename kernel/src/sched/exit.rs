use super::*;

/// The action a supervisor's `sys_exception_resume` requests for a thread
/// suspended on a fault, returned by [`suspend_with_fault`] when it resumes.
/// Phase 1 has the two terminal dispositions; `ResumeSkip`/`ModifyAndResume`,
/// the auto-terminate timeout, and the debugger priority chain are Phase 2.
pub enum ResumeDisposition {
    /// Re-enter the faulting instruction (tag `0`). The stub `iretq`s the
    /// unmodified frame — without fault-fixing it simply re-faults, so this is
    /// the mechanism for a supervisor that has repaired the fault's cause.
    Resume,
    /// Terminate the thread with `code` (tag `2`): the resume path calls
    /// [`exit_thread`] with a crashed status carrying `code`.
    Terminate(i32),
}

/// Deliver a `ChildExited { pid, status }` to `me_obj`'s process's parent
/// channel, if it has one, and wake that channel's waiters. Caller holds
/// `SCHED`. Borrows the channel pointer (never an owned `ObjectRef` — no
/// destructor under `SCHED`); enqueue is allocation-free. Shared by
/// [`exit_thread`] (last thread) and [`exit_process`].
fn deliver_child_exited(g: &mut SchedState, me_obj: *mut (), status: ExitStatus) {
    let info: Option<(*mut (), u32)> = {
        // SAFETY: `me_obj` is the running thread, pinned, lock held.
        let th = unsafe { &*(me_obj as *const Thread) };
        th.process_ref().and_then(|p| {
            // SAFETY: `p` pins a live Process; read its parent channel + pid.
            let proc = unsafe { &*(p.as_ptr() as *const crate::object::Process) };
            proc.parent_notif_ptr().map(|chan| (chan, proc.pid_u32()))
            // `p` (a cloned ObjectRef, never the last) drops here: a plain
            // atomic decrement, no destructor, safe under SCHED.
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

/// `true` if any thread belonging to process `my_pid` is still live anywhere the
/// scheduler tracks: a per-CPU `ready` queue, `blocked`, `suspended`, **or running
/// as another CPU's `current`**. Caller holds `SCHED`. The exiting thread has been
/// taken out of its own CPU's `current`; under SMP a sibling may be `current` on
/// another CPU, so the `current[]` scan is required (else a `false` here would
/// wrongly declare the caller the last live thread and tear the process down out
/// from under a running sibling).
pub(super) fn has_live_siblings(g: &SchedState, my_pid: u32) -> bool {
    // SAFETY: each entry pins a live Thread; `SCHED` held — a shared read of
    // `owner_pid` is sound (no `&mut` taken).
    let same = |r: &ObjectRef| unsafe { &*(r.as_ptr() as *const Thread) }.owner_pid() == my_pid;
    g.ready.iter().any(|q| q.iter().any(same))
        || g.blocked.iter().any(same)
        || g.suspended.iter().any(same)
        || g.current.iter().flatten().any(same)
}

/// Reap every thread of process `my_pid` parked in `list` (a `ready` or
/// `suspended` queue — neither registers on wait objects): mark each `Exited`
/// and move its `ObjectRef` into `reap`. Caller holds `SCHED`.
pub(super) fn reap_matching(list: &mut KVec<ObjectRef>, reap: &mut KVec<ObjectRef>, my_pid: u32) {
    // SAFETY (loop): each entry pins a live Thread; `SCHED` held.
    while let Some(i) = list.iter().position(|r| {
        // SAFETY: `r` pins a live Thread; `SCHED` held.
        let th = unsafe { &*(r.as_ptr() as *const Thread) };
        // Never reap a per-CPU **idle** thread: it is scheduler infrastructure, not
        // a sibling of the exiting process, and it may be *running right now* on
        // another CPU (or this one) — reclaiming it frees a live kernel stack (a
        // use-after-free `#DF`). Idle threads carry `owner_pid()==0`, so a `pid==0`
        // exit would otherwise sweep them all up.
        th.owner_pid() == my_pid && th.tid() != IDLE_TID
    }) {
        // Wait out a mid-switch-out guard before reclaiming: the sweep can see
        // a sibling parked on another CPU's queue whose switch-out has not yet
        // committed — that CPU is still executing on the sibling's kernel stack
        // for a few more instructions, and queueing it for the stack free here
        // would be a use-after-free (F5, decision log 2026-07-21). Bounded: the
        // owning CPU clears the guard from post-release straight-line code, no
        // lock needed.
        // SAFETY: the entry pins a live Thread; `SCHED` held.
        while unsafe { Thread::is_on_cpu(list[i].as_ptr()) } {
            core::hint::spin_loop();
        }
        let r = list.remove(i);
        // SAFETY: `r` pins the thread; `SCHED` held.
        unsafe { Thread::set_state(r.as_ptr(), ThreadState::Exited) };
        push_reserved(reap, r, "reap list");
    }
}

/// Reap every `blocked` thread of process `my_pid`, first unregistering it from
/// the wait objects + deadline heap it is parked on (so no waiter list or heap
/// entry is left dangling at a reaped thread). Caller holds `SCHED`.
fn reap_blocked_matching(
    blocked: &mut KVec<ObjectRef>,
    deadlines: &mut KVec<deadline::Entry>,
    reap: &mut KVec<ObjectRef>,
    my_pid: u32,
) {
    // SAFETY (loop): each entry pins a live Thread; `SCHED` held.
    while let Some(i) = blocked.iter().position(|r| {
        // SAFETY: `r` pins a live Thread; `SCHED` held.
        let th = unsafe { &*(r.as_ptr() as *const Thread) };
        // Never reap an idle thread (see `reap_matching`).
        th.owner_pid() == my_pid && th.tid() != IDLE_TID
    }) {
        // Wait out a mid-switch-out guard before reclaiming (see
        // [`reap_matching`] — a just-blocked sibling's switch-out may not have
        // committed; its CPU is still on the sibling's stack).
        // SAFETY: the entry pins a live Thread; `SCHED` held.
        while unsafe { Thread::is_on_cpu(blocked[i].as_ptr()) } {
            core::hint::spin_loop();
        }
        let r = blocked.remove(i);
        let obj = r.as_ptr();
        // Unregister from every object this thread was waiting on, mirroring
        // `wait_on`'s resume-side cleanup.
        let mut snap = [(0usize, false); MAX_WAIT_HANDLES];
        // SAFETY: live Thread, `SCHED` held.
        let n = unsafe { Thread::wait_snapshot(obj, &mut snap) };
        for &(o, _) in &snap[..n] {
            // SAFETY: the wait object is kept alive by this thread's `sys_wait`
            // `ObjectRef`s (released only when it unblocks); `SCHED` held.
            unsafe { obj_remove_waiter(o as *mut (), obj) };
        }
        // SAFETY: live Thread, `SCHED` held.
        if unsafe { Thread::wait_has_deadline(obj) } {
            deadline::remove(deadlines, obj as usize, deadline::DeadlineKind::Thread);
        }
        // SAFETY: live Thread, `SCHED` held.
        unsafe {
            Thread::wait_clear(obj);
            Thread::set_state(obj, ThreadState::Exited);
        }
        push_reserved(reap, r, "reap list");
    }
}

/// Mark `me` (the current thread, already taken out of `current`) `Exited`,
/// park it in `reap` for deferred stack reclamation, and switch away forever.
/// Shared tail of [`exit_thread`] and [`exit_process`]. The `Thread` and its
/// kernel stack cannot be freed here — this code is still running on that stack
/// — so the next scheduler entry reaps them.
fn finish_exit(mut g: IrqSpinLockGuard<'_, SchedState>, me: ObjectRef) -> ! {
    let me_obj = me.as_ptr();
    // SAFETY: `me` is the running thread, pinned, lock held. (The idle thread
    // never exits, so `me` is never the idle thread.)
    unsafe { Thread::set_state(me_obj, ThreadState::Exited) };
    // SAFETY: as above.
    let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
    // Park self for deferred reclamation.
    push_reserved(g.reap_slot(), me, "reap list (self)");

    // Switch to the next ready thread, else the idle thread (which always
    // exists post-init and is parked here, since `me` was current, not idle).
    let next = match pick_next(&mut g) {
        Some(n) => n,
        None => g.idle_slot().take().expect("idle thread exists after init"),
    };
    let next_obj = next.as_ptr();
    // SAFETY: next is pinned, becoming current; lock held.
    unsafe { Thread::set_state(next_obj, ThreadState::Running) };
    *g.cur_slot() = Some(next);

    // Switch away forever. `switch_into` loads the incoming root before the
    // stack swap, so when the last user thread exits CR3 is restored to the
    // boot root before this (parked-in-`reap`) thread is reaped — its
    // `AddressSpace::Drop` frees the PML4 CR3 would otherwise still reference.
    // SAFETY: `me_slot` is our own (now-`Exited`, pinned-in-`reap`) saved-SP
    // slot — written by the switch and never read again; `me_obj`/`next_obj` is the
    // pinned incoming thread. We never resume, so the restore inside
    // `switch_into` is never reached.
    unsafe { switch_into(g, me_slot, me_obj, next_obj) };
    unreachable!("switched away from an exited thread");
}

/// Terminate the **current thread** with exit `status` and switch away forever.
/// Used by `sys_thread_exit`, the resume-`Terminate` fault path, and kernel
/// thread bodies / the boot thread.
///
/// A `ChildExited { pid, status }` is delivered to the process's parent channel
/// **iff this is its last thread** (no live sibling remains) — a process with
/// other running threads has not exited, so no notification fires. Kernel/boot
/// threads have no process and produce none. Delivery happens here, before
/// parking, so a parent blocked in `sys_wait` wakes promptly.
pub fn exit_thread(status: ExitStatus) -> ! {
    // Reclaim any earlier exited thread first, so `reap` has room for us.
    reap_pending();

    let mut g = SCHED.lock();
    let me = g.cur_slot().take().expect("current set");
    let me_obj = me.as_ptr();

    // SAFETY: `me` is the running thread, pinned, lock held.
    let me_pid = unsafe { &*(me_obj as *const Thread) }.owner_pid();
    // SAFETY: same.
    let has_process = unsafe { &*(me_obj as *const Thread) }.process_ref().is_some();
    if has_process && !has_live_siblings(&g, me_pid) {
        // Last thread out: the process is ending, so its handles must be closed.
        // Deferred to the reaper for the same reason as in `exit_process`.
        // SAFETY: `me_obj` is the running thread, pinned, lock held.
        unsafe { Thread::set_process_ended(me_obj) };
        deliver_child_exited(&mut g, me_obj, status);
    }

    finish_exit(g, me);
}

/// Terminate the **whole process** of the current thread with exit `status`:
/// tear down every sibling thread (scan `ready`/`blocked`/`suspended` by
/// `owner_pid`, unregistering blocked siblings from their waits first), then
/// exit the current thread **with** a `ChildExited`. Used by `sys_process_exit`.
///
/// The per-process thread list that would let an external killer find these threads
/// without a scan lands later; the `owner_pid` scan reaps siblings parked on any CPU's
/// `ready` queue, `suspended`, or `blocked`. **SMP gap (deferred):** a sibling that is
/// *running* as another CPU's `current` is not reaped here — terminating it needs a
/// cross-CPU deschedule IPI (the same machinery as TLB shootdown, slice 3b). Not
/// triggered by today's workloads (multi-threaded processes don't `sys_process_exit`
/// with a sibling live on another CPU). A kernel/boot thread (no process) degrades to a
/// plain [`exit_thread`]-style exit (no siblings, no notification).
pub fn exit_process(status: ExitStatus) -> ! {
    // Reclaim any earlier exited thread first, so `reap` has room.
    reap_pending();

    let mut g = SCHED.lock();
    let me = g.cur_slot().take().expect("current set");
    let me_obj = me.as_ptr();

    // SAFETY: `me` is the running thread, pinned, lock held.
    let me_pid = unsafe { &*(me_obj as *const Thread) }.owner_pid();
    // SAFETY: same.
    let has_process = unsafe { &*(me_obj as *const Thread) }.process_ref().is_some();
    if has_process {
        // Reborrow the guard once as `&mut SchedState` so the field borrows
        // below are disjoint (through the guard's `Deref` each `&mut g.field`
        // would borrow the whole guard, conflicting in one call).
        let cpu = SchedState::this_cpu();
        let st: &mut SchedState = &mut g;
        // Torn-down siblings are parked (off-CPU) on *some* CPU's ready queue, in
        // `suspended`, or `blocked`; reaping them into this CPU's reap list is safe
        // (they are not on any CPU's stack). Sweep every per-CPU ready queue. The
        // `reap[cpu]` index is precomputed for disjoint field borrows.
        for c in 0..MAX_CPUS {
            reap_matching(&mut st.ready[c], &mut st.reap[cpu], me_pid);
        }
        reap_matching(&mut st.suspended, &mut st.reap[cpu], me_pid);
        reap_blocked_matching(&mut st.blocked, &mut st.deadlines, &mut st.reap[cpu], me_pid);
        // The process is ending: always deliver ChildExited (we are now its
        // last thread), and mark this thread so the reaper closes the process's
        // handles. The sweep cannot happen here — it drops object references
        // (destructors reach the rank-6 allocator, and an IPC endpoint's takes
        // rank-1 `SCHED` to wake blocked receivers), and we hold `SCHED` and never
        // return from `finish_exit`.
        // SAFETY: `me_obj` is the running thread, pinned, lock held.
        unsafe { Thread::set_process_ended(me_obj) };
        // Hand the handle table to the reaper. This is the half that peers are waiting
        // on: closing an IPC endpoint is what nulls its peer and wakes anyone blocked
        // there, so leaving it to "whenever some CPU next reaches idle" makes a blocked
        // reader's liveness depend on unrelated scheduling. `set_process_ended` above
        // still marks the thread, so the per-CPU sweep remains a correct fallback if the
        // queue is full.
        if st.ended_pids.len() < st.ended_pids.capacity() {
            let _ = st.ended_pids.push_within_capacity(me_pid);
            wake_reaper(st);
        }
        deliver_child_exited(st, me_obj, status);
    }

    finish_exit(g, me);
}

/// Suspend the **current thread** after a ring-3 fault: deliver `notif` to its
/// process's notification channel (waking the supervisor), record the
/// `ExceptionFrame` at `frame_ptr` (its address on this thread's kernel stack),
/// park the thread in `suspended`, and switch away — mirroring
/// [`block_current_and_switch`], but parked for `sys_exception_resume` rather
/// than a waker. Returns the [`ResumeDisposition`] the supervisor chose once the
/// thread is made runnable again. Called from the exception dispatchers.
/// Emit a last-ditch kernel diagnostic for a ring-3 fault that **stranded the
/// scheduler** — one that left no runnable thread to receive the fault notification
/// and call `sys_exception_resume`, so the system would idle forever. Without this
/// it is a silent hang (notably an init/pid-1 crash). Called only from the
/// no-runnable-thread branch of [`suspend_with_fault`], so a serviced fault (whose
/// supervisor waiter was just made runnable) never reaches here.
///
/// Uses the unsynchronized emergency serial writer — lock-free, mirroring the
/// ring-0 `dump_and_halt` path. Sound here: a userspace fault never holds `SERIAL`,
/// and the caller holds `SCHED` so interrupts are masked. Reads only neutral data
/// (pid/tid from the thread; kind/addr from the `Notification`), staying out of the
/// arch-private `ExceptionFrame`.
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

pub fn suspend_with_fault(frame_ptr: usize, notif: Notification) -> ResumeDisposition {
    // Reclaim any earlier exited thread first (we may be the only scheduler
    // entry for a while if we suspend), off the rank-1 lock.
    reap_pending();

    let me_obj;
    {
        let mut g = SCHED.lock();
        let me = g.cur_slot().take().expect("current set");
        me_obj = me.as_ptr();

        // Deliver the fault notification to the faulting process's channel
        // (borrowed pointer — no `ObjectRef` destructor under `SCHED`).
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

        // Record the frame, mark Suspended, park off the run queue. The
        // `ExceptionFrame` stays on this (now-frozen) kernel stack until resume.
        // TODO(unsupervised-fault): nothing supervises an ordinary program, so with
        // the thirty-second auto-terminate deferred its thread stays here for the
        // boot — first reached by a pulled stick's failed fill (Phase 6 Part D).
        // SAFETY: `me` is the running thread, pinned, lock held.
        unsafe {
            Thread::set_exception_frame(me_obj, frame_ptr);
            Thread::set_state(me_obj, ThreadState::Suspended);
        }
        // SAFETY: as above.
        let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
        push_reserved(&mut g.suspended, me, "suspended list");

        // Switch to the next runnable thread (mirrors block_current_and_switch).
        // If nothing is runnable, this fault **stranded the scheduler**: no thread
        // remained to receive the notification and `sys_exception_resume` us, so the
        // system would idle forever (a silent hang — notably an init/pid-1 crash).
        // Emit a last-ditch diagnostic. A serviced fault (the supervisor's waiter
        // was just woken by `signal_channel` above) takes the `Some` branch and
        // stays quiet — so this fires only for genuinely-unhandled faults.
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

        // Switch into `next`; we resume here when `sys_exception_resume` moves
        // us `suspended`→`ready` and the scheduler switches us in.
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
        2 => ResumeDisposition::Terminate(code),
        _ => ResumeDisposition::Resume,
    }
}

/// Resume a thread suspended on a fault: record the supervisor's disposition
/// (`disp_tag`/`disp_code`) on it and move it `suspended`→`ready`. Returns
/// `false` (no-op) if `thread` is not currently `Suspended` — the backstop
/// `sys_exception_resume` maps to `InvalidArgument`. Caller need not hold
/// `SCHED` (taken here).
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
    // SAFETY: `r` pins `thread`; `SCHED` held. The disposition is read by
    // `suspend_with_fault` when this thread next runs.
    unsafe {
        Thread::set_disposition(thread, disp_tag, disp_code);
        Thread::set_state(thread, ThreadState::Ready);
    }
    // Re-home on its CPU (a wake; a full home falls back to a permitted queue
    // with room — F6). Fails only when every permitted queue is at reserve.
    if let Err(returned) = place_thread(&mut g, r, true) {
        // **Do not let `returned` drop here.** `SCHED` is held, and an `ObjectRef` drop can
        // reach `dispatch_destroy` → `SlabCache::free` — a plain spinlock under the rank-1
        // lock, which is F2 (`kernel/docs/lock-ordering.md`). The two spawn callers avoid it
        // by carrying the ref out of the locked block with a `match`; here there is nowhere
        // to carry it *to*, because the next statement ends the machine.
        //
        // `if place_thread(..).is_err()` used to do exactly the forbidden thing: an `if`
        // condition is its own temporary scope, so the `Result` — and the `ObjectRef` inside
        // it — dropped at the end of the condition, *before* the panic, with `SCHED` still
        // held. An earlier version of this comment asserted the opposite and credited
        // `panic = "abort"` with saving it; both were wrong (PR #204 review, blocking 1).
        // The consequence was not theoretical: reserve exhaustion is the state this panic
        // exists for, and the drop would spin on the allocator lock while another CPU's tick
        // spins on `SCHED` with IRQs masked — a hang instead of the diagnostic.
        //
        // Leaked deliberately: `panic = "abort"` means nothing outlives this call, so a
        // `forget` costs nothing and is honest about it. `deferred_drops` would be the
        // documented alternative but is worse here — pushing to it can itself grow under
        // `SCHED` (audit C.1(c)) and nothing will ever drain it.
        core::mem::forget(returned);
        panic!(
            "resume placement: no ready queue would take the thread — every affinity-permitted \
             queue is at reserve; or affinity permits no CPU that accepts work and every CPU \
             that does is at reserve; or no CPU accepts work and this one is at reserve"
        );
    }
    true
}

/// The kernel-stack address of `thread`'s captured `ExceptionFrame`, or `None`
/// if it is not currently `Suspended`. Used by `sys_thread_get_registers`; the
/// thread stays parked while suspended, so the frame is stable to read after
/// this returns (the caller drops `SCHED` before the user copy-out).
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

/// Pop up to `buf.len()` parked refs — this CPU's `reap` list first, then the
/// global [`SchedState::deferred_drops`] — into `buf`. **Moves only**: no drop,
/// no allocation, and the lists keep their reserved buffers. (The previous
/// `mem::take` drain swapped in a zero-capacity `KVec`, so every later exit-path
/// push *allocated under `SCHED`* via `KVec::try_push` growth — the F11 deadlock
/// hazard, decision log 2026-07-21.) Returns the count. Caller holds `SCHED` and
/// drops the moved refs after releasing it, in thread context.
pub(super) fn drain_pending_drops(g: &mut SchedState, buf: &mut [Option<ObjectRef>]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        let Some(r) = g.reap_slot().pop() else { break };
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

/// Drop every pending reaped thread — and every deferred-drop ref — **outside**
/// the run-queue lock, in thread context (a reaped thread's `KernelStack` `Drop`
/// takes rank-6 allocator locks and initiates a TLB shootdown). Idempotent;
/// called at the top of [`yield_now`]/[`exit_thread`]/[`exit_process`]/
/// [`suspend_with_fault`], by the idle loop, and by the boot drainer. Only this
/// CPU's reap list is drained — a thread is reclaimed by the CPU it died on,
/// after its `switch_into` completed (so its stack is no longer in use) — plus
/// the CPU-agnostic [`SchedState::deferred_drops`]. Draining moves refs into a
/// fixed local buffer under a brief hold ([`drain_pending_drops`] — preserving
/// the lists' reserved capacity), then drops them with the lock released.
pub fn reap_pending() {
    // Completed block IRPs, whose boxes their DPC deliberately did not free. Same rule as
    // the `deferred_drops` below and the same reason: a free reaches the plain-spinlock
    // allocator, which must not happen in IRQ or DPC context. Done first so the frees
    // happen even if there is nothing else to reap.
    crate::io::block::reclaim_completed();
    // The char drivers park their completed reads for the same reason and need the same
    // thread-context drop (decision log, 2026-08-06).
    crate::drivers::console::reclaim_completed();
    crate::drivers::ps2::reclaim_completed();
    crate::drivers::xhci::hid::reclaim_completed();
    loop {
        let mut buf: [Option<ObjectRef>; REAP_RESERVE + DEFERRED_DROP_RESERVE] =
            [const { None }; REAP_RESERVE + DEFERRED_DROP_RESERVE];
        let n = {
            let mut g = SCHED.lock();
            drain_pending_drops(&mut g, &mut buf)
        };
        // Close the handles of any process whose last thread is in this batch —
        // **before** the drops below, so its pipes close (waking peers) as promptly
        // as the reap runs. This is the sweep's home: thread context, no `SCHED`
        // held, not IRQ context. The exiting thread could not do it itself; it
        // holds `SCHED` and never returns from `finish_exit`.
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
            // `pid == 0` is a kernel/idle thread, which owns no handles and whose
            // pid must never be swept.
            if ended && pid != 0 {
                crate::handle::global::close_all_owned_by(pid);
            }
        }

        // Drop with preemption disabled: the drops take the allocator locks and
        // (for a kernel stack) run a TLB shootdown — plain locks other CPUs may
        // spin on. Being descheduled while holding one starves the spinners for
        // a scheduling round — or forever, when the reaper is the **idle**
        // thread, which is never re-picked while the spinners keep every CPU
        // busy (F12, decision log 2026-07-21). Interrupts stay enabled; only
        // the switch is deferred.
        preempt_disable();
        for slot in buf[..n].iter_mut() {
            drop(slot.take());
        }
        preempt_enable();
        if n < buf.len() {
            return; // drained everything that was parked when we looked
        }
        // Filled the buffer (defensive; both lists stay within their reserves,
        // which the buffer covers exactly) — go around for any remainder.
    }
}

/// The idle thread body: reap any exited thread, then `hlt` until the next
/// interrupt. Runs with IF=1 (set by
/// [`thread_trampoline`](crate::arch::thread_trampoline)), so the periodic
/// timer wakes it; if a thread became ready, the tick reschedules to it,
/// otherwise control returns here and halts again. Never returns, so the idle
/// thread is never enqueued or reaped.
///
/// Reaping here (outside any IRQ context, holding no other lock) is the safe
/// home for draining the `reap` slot when the system would otherwise sit idle
/// — e.g. the boot thread parked in `reap` after it `exit`s at end of boot.
/// The reaper thread body: close the handle tables of processes that have exited, then
/// park until there are more.
///
/// **Why a thread at all.** Deferred reclamation used to run only at
/// `exit_*`/`yield_now`/`suspend_with_fault` and in the idle loop. On a system where no
/// thread exits or yields and no CPU reaches idle — one busy-looping service is enough —
/// nothing ran it, so no exited process was reclaimed and every pipe they held stayed
/// open. That is not a leak, it is a liveness bug: a peer blocked on a channel waits for a
/// `PeerClosed` that only reclamation can deliver. Twice in one day a spinning supervisor
/// hung an unrelated shell that way.
///
/// A normal schedulable thread removes the dependency on scheduling luck: a spinner shares
/// the CPU with the reaper instead of starving it. This is the shape Linux uses for the
/// same problem — the bulk of teardown happens in the dying task, and what must be
/// deferred is drained by dedicated kernel threads (`ksoftirqd`, `rcuo`, `kworker`) rather
/// than by whoever happens along.
///
/// It closes **handle tables only**. Reaped threads' kernel stacks stay with the per-CPU
/// [`reap`](SchedState::reap) path, because a thread parks itself there and then switches
/// off its own stack — freeing that from another CPU is a use-after-free, and the reaper
/// runs wherever it is scheduled.
pub(super) extern "C" fn reaper_body(_arg: usize) {
    loop {
        // Take one pid at a time: `close_all_owned_by` drops object references, which must
        // happen with `SCHED` released, so the lock cannot be held across the close.
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
/// parking, which is what closes the lost-wakeup window: an exit that pushes a pid between
/// the reaper's last drain and this park would otherwise wake nothing and leave the work
/// sitting until some unrelated scheduler entry noticed it.
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

/// Make the reaper runnable, if it is parked. Caller holds `SCHED`.
///
/// A no-op when it is already running or queued — the wake is level-triggered off
/// [`SchedState::ended_pids`], so a missed edge costs nothing.
///
/// **A fifth placement site, and the only one that bypasses the policy.** It pushes onto
/// `ready[this_cpu()]` directly: no affinity check and no [`cpu_accepts_work`]. Its capacity
/// check is [`push_reserved`]'s refusal, which is a real one — the `debug_assert!` that used
/// to sit here was not, since the growth beside it happened first and succeeded. Sound today for a reason worth writing down rather than
/// rediscovering — a CPU executing this code is by definition not parked, so the CPU it
/// chooses always accepts work — but it is a fifth opinion on placement, in the same shape
/// as the extra opinions on `online_mask` the audit found in § A. If placement policy grows
/// a rule, this is the site that will not have it (kernel audit C.3(e), 2026-08-14).
fn wake_reaper(g: &mut SchedState) {
    let Some(r) = g.reaper.take() else {
        return;
    };
    // SAFETY: `r` is the parked reaper, pinned by the slot's reference; `SCHED` held.
    unsafe { Thread::set_state(r.as_ptr(), ThreadState::Ready) };
    let cpu = SchedState::this_cpu();
    push_reserved(&mut g.ready[cpu], r, "run queue (reaper wake)");
}
