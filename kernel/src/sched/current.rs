//! The current thread, its process and their IDs.

use super::*;

/// `true` if this CPU's current thread is its idle thread (its object address equals
/// this CPU's `idle_addr`). Caller holds `SCHED`.
pub(super) fn current_is_idle(g: &SchedState) -> bool {
    g.cur_ref().as_ref().is_some_and(|c| c.as_ptr() as usize == g.idle_addr())
}

/// Read the current thread's entry point and argument, for [`thread_enter`].
pub(super) fn current_entry() -> (ThreadEntry, usize) {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive and we hold the lock.
    unsafe { Thread::entry_and_arg(cur.as_ptr()) }
}

/// The pid of the process that owns the currently running thread.
///
/// Valid during a syscall, where it is the `caller_pid` the handle table's calls need.
/// Takes only the rank-1 lock and releases it before returning: handle syscalls call this
/// **first**, then take the rank-3 handle-table lock, never nesting the two.
pub fn current_owner_pid() -> u32 {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive (it holds a refcount on the `Thread`); `SCHED` held.
    unsafe { Thread::owner_pid_of(cur.as_ptr()) }
}

/// The calling thread's `(pid, tid)` under one `SCHED` hold — the capture step of the
/// `/proc/self/status` snapshot (`docs/architecture/scheduler.md` § "The stats surface").
/// `None` when no thread is current or the current thread is a kernel/boot thread with no
/// owning process (matching [`current_process`]'s `None` arm).
pub fn current_pid_tid() -> Option<(u32, u32)> {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref()?;
    let t = cur.as_ptr();
    // SAFETY: `current` is pinned alive; `SCHED` held. None of these touches a refcount.
    unsafe { Thread::process_of(t).map(|_| (Thread::owner_pid_of(t), Thread::tid_of(t))) }
}

/// The tid of the currently running thread, for the `thread` field of a fault
/// notification. Takes only the rank-1 lock.
pub fn current_tid() -> u32 {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive; `SCHED` held.
    unsafe { Thread::tid_of(cur.as_ptr()) }
}

/// Pop one notification from `channel` under `SCHED` (or synthesize a dropped notice). The
/// lock-guarded core of `sys_notif_recv` and the in-kernel supervisor; the caller copies
/// the result out **after** this returns.
///
/// # Safety
///
/// `channel` points at a live `NotificationChannel`, pinned for the call by a reference the caller
/// holds.
pub unsafe fn notif_try_recv(channel: *mut ()) -> Option<Notification> {
    let _g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning the channel; SCHED held.
    unsafe { NotificationChannel::try_recv(channel) }
}

/// The [`Process`](crate::object::Process) owning the currently running thread (`None` for
/// kernel/boot threads). Takes only the rank-1 lock; the returned
/// [`ObjectRef`](crate::object::ObjectRef) is cloned under it and outlives it. Callers then
/// take lower-rank locks (handle-table rank-3, AS rank-4), never nesting upward.
pub fn current_process() -> Option<ObjectRef> {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
    // SAFETY: as `current_owner_pid`; the clone bumps the process's refcount under the lock.
    unsafe { Thread::process_ref_of(cur.as_ptr()) }
}

/// The currently running [`Thread`](crate::object::Thread), cloned as an
/// [`ObjectRef`](crate::object::ObjectRef) that outlives the lock. Takes only the rank-1
/// lock. `None` only before the first thread runs.
pub fn current_thread() -> Option<ObjectRef> {
    let g = SCHED.lock();
    // SAFETY-equivalent note: `ObjectRef::clone` is an atomic refcount bump (no
    // `&mut`, no drop), sound to perform under the run-queue lock.
    g.cur_ref().clone()
}

/// Run `f` with `thread` as the global scheduler's current thread, which no other host test reads:
/// one at a time, and restored even if `f` panics.
#[cfg(test)]
pub(crate) fn with_current<R>(thread: ObjectRef, f: impl FnOnce() -> R) -> R {
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    struct Restore(Option<ObjectRef>);
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
pub(crate) fn an_ending_thread() -> ObjectRef {
    let t = into_objref(Thread::try_new(0, 0).unwrap());
    // SAFETY: the test's own thread, which nothing else reads yet.
    unsafe { Thread::set_ending(t.as_ptr()) };
    t
}
