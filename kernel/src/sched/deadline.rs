use super::{MAX_CPUS, MAX_THREADS, SCHED, SchedState, push_reserved};
use crate::libkern::{AllocError, Reserved};
use crate::mm::PAGE_SIZE;
use crate::mm::buddy::MAX_ORDER;
use crate::object::IpcChannel;

/// What a deadline-heap entry fires when its `deadline_ns` elapses.
///
/// [`Timer`]: crate::object::Timer
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DeadlineKind {
    /// A `sys_wait` thread-deadline: `target` is the waiting Thread object,
    /// woken directly (its wait slots stay un-signaled → it sees a timeout).
    Thread,
    /// A [`Timer`] fire: `target` is the Timer object address.
    Timer,
    /// A `BlockBounded` IPC send's delivery deadline: `target` is the
    /// [`PendingOperation`](crate::object::PendingOperation), `channel` the
    /// receiving endpoint holding the (undelivered) send.
    PendingSend,
}

/// One pending deadline, keyed by `(target, kind)` for removal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Entry {
    pub deadline_ns: u64,
    pub target: usize,
    pub kind: DeadlineKind,
    /// The receiving endpoint, for [`DeadlineKind::PendingSend`]; `0` else.
    pub channel: usize,
}

/// The slots [`init`](super::init) promises the threads: one each, for `MAX_THREADS` placed threads
/// and a boot thread per CPU.
pub(super) const HEAP_RESERVE: usize = MAX_THREADS + MAX_CPUS;

/// What brings its own heap slots, held by a [`SlotHold`] from its creation to its destruction.
#[derive(Clone, Copy)]
pub enum Slots {
    /// A [`Timer`](crate::object::Timer): one, for its one entry.
    Timer,
    /// An IPC endpoint: one per send it can hold, each of which may carry a deadline.
    Endpoint,
}

impl Slots {
    fn kind(self) -> usize {
        match self {
            Slots::Timer => DeadlineKind::Timer as usize,
            Slots::Endpoint => DeadlineKind::PendingSend as usize,
        }
    }

    fn count(self) -> usize {
        match self {
            Slots::Timer => 1,
            Slots::Endpoint => IpcChannel::MAX_PENDING_SENDS,
        }
    }
}

/// Slots promised to the object this is a field of, given back once, when that is dropped.
pub struct SlotHold(Slots);

impl SlotHold {
    /// Promise `slots`, growing the heap outside `SCHED` first if every slot it has is promised.
    pub fn reserve(slots: Slots) -> Result<SlotHold, AllocError> {
        reserve_slots(slots)?;
        Ok(SlotHold(slots))
    }
}

impl Drop for SlotHold {
    fn drop(&mut self) {
        release_slots(&mut SCHED.lock(), self.0);
    }
}

/// The most slots one heap holds: one buddy block, less a page for the allocator's header.
pub(super) const MOST_SLOTS: usize = ((PAGE_SIZE << MAX_ORDER) - PAGE_SIZE) / core::mem::size_of::<Entry>();

/// Twice `capacity`, or `promised` if more, never past [`MOST_SLOTS`]; `None` past it.
pub(super) fn grown_capacity(promised: usize, capacity: usize) -> Option<usize> {
    (promised <= MOST_SLOTS).then(|| promised.max(2 * capacity).min(MOST_SLOTS))
}

pub(super) fn reserve_slots(slots: Slots) -> Result<(), AllocError> {
    // TODO(deadline-slot-quota): one process can take every slot.
    loop {
        let want = {
            let mut g = SCHED.lock();
            let promised = g.deadline_slots.iter().sum::<usize>() + slots.count();
            if promised <= g.deadlines.capacity() {
                g.deadline_slots[slots.kind()] += slots.count();
                #[cfg(test)]
                HELD_HERE.with(|h| h.set(h.get() + slots.count() as isize));
                return Ok(());
            }
            grown_capacity(promised, g.deadlines.capacity()).ok_or(AllocError)?
        };
        let grown = Reserved::with_capacity(want)?;
        let spare = regrow(&mut SCHED.lock().deadlines, grown);
        drop(spare);
    }
}

/// Give back what [`reserve_slots`] promised `slots`. Caller holds `SCHED`.
pub(super) fn release_slots(g: &mut SchedState, slots: Slots) {
    let held = &mut g.deadline_slots[slots.kind()];
    debug_assert!(*held >= slots.count(), "slots given back that were never promised");
    *held = held.saturating_sub(slots.count());
    #[cfg(test)]
    HELD_HERE.with(|h| h.set(h.get() - slots.count() as isize));
}

#[cfg(test)]
std::thread_local! {
    /// This test thread's slots not given back, so a test can count its own.
    static HELD_HERE: core::cell::Cell<isize> = const { core::cell::Cell::new(0) };
}

/// See [`HELD_HERE`].
#[cfg(test)]
pub(crate) fn slots_held_here() -> isize {
    HELD_HERE.with(|h| h.get())
}

/// Leave the larger of `h` and `grown` in `h`, entries in order; return the other, to drop later.
pub(super) fn regrow(h: &mut Reserved<Entry>, mut grown: Reserved<Entry>) -> Reserved<Entry> {
    if grown.capacity() <= h.capacity() {
        return grown;
    }
    for &e in h.iter() {
        push_reserved(&mut grown, e, "deadline heap (grown)");
    }
    core::mem::replace(h, grown)
}

/// [`push`] an entry its owner holds a slot for; a debug build stops a kind past its slots.
pub(super) fn push_promised(g: &mut SchedState, e: Entry) -> Result<(), AllocError> {
    debug_assert!(
        g.deadlines.iter().filter(|x| x.kind == e.kind).count() < g.deadline_slots[e.kind as usize],
        "a {:?} deadline with no slot promised",
        e.kind
    );
    push(&mut g.deadlines, e)
}

/// The earliest entry, or `None` if empty.
pub(super) fn peek(h: &Reserved<Entry>) -> Option<Entry> {
    h.first().copied()
}

/// Insert `e`, sifting up by `deadline_ns`. `Err` if at reserve (the caller
/// maps it to an out-of-memory failure; never grows under the lock).
pub(super) fn push(h: &mut Reserved<Entry>, e: Entry) -> Result<(), AllocError> {
    if h.len() >= h.capacity() {
        return Err(AllocError);
    }
    super::push_reserved(h, e, "deadline heap");
    let mut i = h.len() - 1;
    while i > 0 {
        let parent = (i - 1) / 2;
        if h[i].deadline_ns < h[parent].deadline_ns {
            h.swap(i, parent);
            i = parent;
        } else {
            break;
        }
    }
    Ok(())
}

/// Remove and return the earliest entry, sifting the moved-up last element
/// down. `None` if empty.
pub(super) fn pop_min(h: &mut Reserved<Entry>) -> Option<Entry> {
    let n = h.len();
    if n == 0 {
        return None;
    }
    h.swap(0, n - 1);
    let min = h.pop();
    sift_down(h, 0);
    min
}

/// Remove the first entry matching `(target, kind)`. Returns `true` if one
/// was removed. Used when a `sys_wait` resumes (drop its deadline), a timer
/// is re-armed (drop its stale entry), or a `BlockBounded` send is delivered
/// early / its endpoint closes (drop its pending-send deadline).
pub(super) fn remove(h: &mut Reserved<Entry>, target: usize, kind: DeadlineKind) -> bool {
    // TODO(deadline-heap-scan): a scan, under `SCHED`, of a heap one process can fill.
    let Some(i) = h.iter().position(|e| e.target == target && e.kind == kind) else {
        return false;
    };
    let n = h.len();
    h.swap(i, n - 1);
    h.pop();
    // The element now at `i` may be too small (sift up) or too large (down).
    if i < h.len() {
        if i > 0 && h[i].deadline_ns < h[(i - 1) / 2].deadline_ns {
            sift_up(h, i);
        } else {
            sift_down(h, i);
        }
    }
    true
}

fn sift_up(h: &mut Reserved<Entry>, mut i: usize) {
    while i > 0 {
        let parent = (i - 1) / 2;
        if h[i].deadline_ns < h[parent].deadline_ns {
            h.swap(i, parent);
            i = parent;
        } else {
            break;
        }
    }
}

fn sift_down(h: &mut Reserved<Entry>, mut i: usize) {
    let n = h.len();
    loop {
        let l = 2 * i + 1;
        let r = 2 * i + 2;
        let mut smallest = i;
        if l < n && h[l].deadline_ns < h[smallest].deadline_ns {
            smallest = l;
        }
        if r < n && h[r].deadline_ns < h[smallest].deadline_ns {
            smallest = r;
        }
        if smallest == i {
            break;
        }
        h.swap(i, smallest);
        i = smallest;
    }
}
