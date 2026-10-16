//! The current thread, its process and their IDs.

use super::*;

/// `true` if this CPU's current thread is its idle thread (its object address equals
/// this CPU's `idle_addr`). Caller holds `SCHED`.
pub(super) fn current_is_idle(g: &SchedState) -> bool {
    g.cur_ref().as_ref().is_some_and(|c| c.as_ptr() as usize == g.idle_addr())
}

/// Read the current thread's entry point and argument. Used by
/// [`thread_enter`] when a freshly scheduled thread first runs.
pub(super) fn current_entry() -> (ThreadEntry, usize) {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive and we hold the lock.
    unsafe { Thread::entry_and_arg(cur.as_ptr()) }
}

/// The pid of the process that owns the currently running thread.
///
/// Valid during a syscall: the current thread is the calling user thread, so
/// this is the `caller_pid` the handle table's `lookup`/`close`/`restrict`/
/// `stat`/`duplicate` need. Takes only the rank-1 run-queue lock and releases
/// it before returning — handle syscalls call this **first**, then take the
/// rank-3 handle-table lock, never nesting the two.
pub fn current_owner_pid() -> u32 {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive (it holds a refcount on the `Thread`)
    // and we hold the run-queue lock, which — the one global `SCHED` lock — serialises all
    // access to the `Thread`. Forming a shared `&Thread` to read `owner_pid`
    // is sound; no `&mut` is taken anywhere under this lock.
    unsafe { &*(cur.as_ptr() as *const crate::object::Thread) }.owner_pid()
}

/// The calling thread's numeric identity `(pid, tid)`, read under one `SCHED`
/// hold — the capture step of the `/proc/self/status` snapshot (see
/// `object::kernel_server` and `docs/architecture/scheduler.md` § "The stats
/// surface"). `None` when no thread is current yet, or when the current thread
/// is a kernel/boot thread with no owning process (mirroring
/// [`current_process`]'s `None` arm, which the other `/proc/self` leaves map to
/// *not found*).
pub fn current_pid_tid() -> Option<(u32, u32)> {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref()?;
    // SAFETY: `current` is pinned alive and the run-queue lock serialises
    // Thread access; shared `&Thread` reads of the identity fields are sound.
    // `has_process` touches no refcount, so no `ObjectRef` clone/drop happens
    // under the rank-1 lock.
    let t = unsafe { &*(cur.as_ptr() as *const crate::object::Thread) };
    if !t.has_process() {
        return None;
    }
    Some((t.owner_pid(), t.tid()))
}

/// The tid of the currently running thread. Used by the exception path to fill
/// the `thread` field of a fault notification. Takes only the rank-1 lock.
pub fn current_tid() -> u32 {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive and the run-queue lock serialises
    // Thread access; a shared `&Thread` read of `tid` is sound.
    unsafe { &*(cur.as_ptr() as *const crate::object::Thread) }.tid()
}

/// Pop one notification from `channel` under `SCHED` (or synthesize a dropped
/// notice). The lock-guarded core of `sys_notif_recv` and the in-kernel
/// supervisor; the caller copies the result out **after** this returns.
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

/// The [`Process`](crate::object::Process) owning the currently running
/// thread, if it has one (`None` for kernel/boot threads). Valid during a
/// syscall: the current thread is the calling user thread, so this is the
/// process whose address space and handles the call operates on. Takes only
/// the rank-1 run-queue lock; the returned [`ObjectRef`](crate::object::ObjectRef)
/// is cloned under the lock and outlives it. Handle/memory syscalls call this
/// first, then take lower-rank locks (handle-table rank-3, AS rank-4), never
/// nesting upward.
pub fn current_process() -> Option<ObjectRef> {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
    // SAFETY: as `current_owner_pid` — `current` is pinned and the run-queue
    // lock serialises Thread access; `process_ref` clones the stored
    // `ObjectRef` (bumping the process refcount) under the lock.
    unsafe { &*(cur.as_ptr() as *const crate::object::Thread) }.process_ref()
}

/// The currently running [`Thread`](crate::object::Thread) object, cloned as an
/// [`ObjectRef`](crate::object::ObjectRef) that outlives the lock. During a
/// syscall this is the calling user thread — the `/proc/self/thread` kernel
/// server uses it to hand the caller a handle to itself. Takes only the rank-1
/// run-queue lock. `None` only before the first thread runs (never during a
/// syscall). Unlike [`current_process`], `current` *is* the Thread `ObjectRef`,
/// so it is cloned directly.
pub fn current_thread() -> Option<ObjectRef> {
    let g = SCHED.lock();
    // SAFETY-equivalent note: `ObjectRef::clone` is an atomic refcount bump (no
    // `&mut`, no drop), sound to perform under the run-queue lock.
    g.cur_ref().clone()
}
