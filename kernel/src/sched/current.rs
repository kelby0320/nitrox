//! The current thread, its process and their IDs.

use super::*;

/// `true` if this CPU's current thread is its idle thread (its object address equals
/// this CPU's `idle_addr`). Caller holds `SCHED`.
pub(super) fn current_is_idle(g: &SchedState) -> bool {
    g.cur_ref().as_ref().is_some_and(|c| c.as_ptr() as usize == g.idle_addr())
}

/// Read the current thread's entry point and argument, for [`thread_enter`].
pub(super) fn current_entry() -> (ThreadEntry, usize) {
    SCHED.lock().cur_ref().as_deref().expect("current set when a thread runs").entry_and_arg()
}

/// The pid of the process that owns the currently running thread.
///
/// Valid during a syscall, where it is the `caller_pid` the handle table's calls need.
/// Takes only the rank-1 lock and releases it before returning: handle syscalls call this
/// **first**, then take the rank-3 handle-table lock, never nesting the two.
pub fn current_owner_pid() -> u32 {
    SCHED.lock().cur_ref().as_deref().expect("current set when a thread runs").owner_pid()
}

/// The calling thread's `(pid, tid)` under one `SCHED` hold — the capture step of the
/// `/proc/self/status` snapshot (`docs/architecture/scheduler.md` § "The stats surface").
/// `None` when no thread is current or the current thread is a kernel/boot thread with no
/// owning process (matching [`current_process`]'s `None` arm).
pub fn current_pid_tid() -> Option<(u32, u32)> {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_deref()?;
    cur.process().map(|_| (cur.owner_pid(), cur.tid()))
}

/// The tid of the currently running thread, for the `thread` field of a fault
/// notification. Takes only the rank-1 lock.
pub fn current_tid() -> u32 {
    SCHED.lock().cur_ref().as_deref().expect("current set when a thread runs").tid()
}

/// Pop one notification from `channel` under `SCHED` (or synthesize a dropped notice). The
/// lock-guarded core of `sys_notif_recv` and the in-kernel supervisor; the caller copies
/// the result out **after** this returns.
pub fn notif_try_recv(channel: &NotificationChannel) -> Option<Notification> {
    channel.try_recv(&mut SCHED.lock().tok)
}

/// The [`Process`](crate::object::Process) owning the currently running thread (`None` for
/// kernel/boot threads). Takes only the rank-1 lock; the returned
/// [`Ref`](crate::object::header::Ref) is cloned under it and outlives it. Callers then
/// take lower-rank locks (handle-table rank-3, AS rank-4), never nesting upward.
pub fn current_process() -> Option<Ref<Process>> {
    // The clone bumps the process's refcount under the lock.
    SCHED.lock().cur_ref().as_deref().expect("current set when a thread runs").process_ref()
}

/// The currently running [`Thread`](crate::object::Thread), cloned as an
/// [`ObjectRef`](crate::object::ObjectRef) that outlives the lock. Takes only the rank-1
/// lock. `None` only before the first thread runs.
pub fn current_thread() -> Option<ObjectRef> {
    // A clone is an atomic refcount bump (no drop), sound under the run-queue lock.
    SCHED.lock().cur_ref().clone().map(Ref::erase)
}

/// Run `f` with `thread` as the global scheduler's current thread, which no other host test reads:
/// one at a time, and restored even if `f` panics.
#[cfg(test)]
pub(crate) fn with_current<R>(thread: Ref<Thread>, f: impl FnOnce() -> R) -> R {
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    struct Restore(Option<Ref<Thread>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let mine = core::mem::replace(SCHED.lock().cur_slot(), self.0.take());
            drop(mine);
        }
    }
    let _one = ONE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _restore = Restore(SCHED.lock().cur_slot().replace(thread));
    f()
}

/// An inert thread whose process is exiting, for [`with_current`].
#[cfg(test)]
pub(crate) fn an_ending_thread() -> Ref<Thread> {
    let t = Ref::new(Thread::try_new(0, 0).unwrap());
    t.set_ending(&mut SCHED.lock().tok);
    t
}
