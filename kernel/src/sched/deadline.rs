use super::{MAX_CPUS, MAX_THREADS, SCHED, SchedState, push_reserved};
use crate::libkern::{AllocError, KVec};
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

/// The heap capacity reserved at [`init`](super::init), every slot promised to a thread: a
/// thread waits on one deadline at most, and there are `MAX_THREADS` placed threads at most and a
/// boot thread per CPU.
pub(super) const HEAP_RESERVE: usize = MAX_THREADS + MAX_CPUS;

/// What brings its own heap slots, from creation to destruction: a [`Timer`] one, and an IPC
/// endpoint one per send it can hold.
///
/// [`Timer`]: crate::object::Timer
#[derive(Clone, Copy)]
pub enum Slots {
    Timer,
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

/// Promise `slots` their place in the heap, growing it outside `SCHED` first if every slot is
/// promised, so that no push finds it full and a process holding many timers pays at creation.
pub fn reserve_slots(slots: Slots) -> Result<(), AllocError> {
    loop {
        let want = {
            let mut g = SCHED.lock();
            let promised = g.deadline_slots.iter().sum::<usize>() + slots.count();
            if promised <= g.deadlines.capacity() {
                g.deadline_slots[slots.kind()] += slots.count();
                return Ok(());
            }
            promised.max(2 * g.deadlines.capacity())
        };
        let mut grown = KVec::new();
        grown.try_reserve(want)?;
        let spare = regrow(&mut SCHED.lock().deadlines, grown);
        drop(spare);
    }
}

/// Give back what [`reserve_slots`] promised `slots`. Caller holds `SCHED`.
pub(super) fn release_slots(g: &mut SchedState, slots: Slots) {
    let held = &mut g.deadline_slots[slots.kind()];
    debug_assert!(*held >= slots.count(), "slots given back that were never promised");
    *held = held.saturating_sub(slots.count());
}

/// [`release_slots`], for a constructor that fails after reserving.
pub fn give_back_slots(slots: Slots) {
    release_slots(&mut SCHED.lock(), slots);
}

/// Leave the larger of `h` and `grown` in `h`, its entries in their order, and return the other
/// for dropping outside `SCHED`.
pub(super) fn regrow(h: &mut KVec<Entry>, mut grown: KVec<Entry>) -> KVec<Entry> {
    if grown.capacity() <= h.capacity() {
        return grown;
    }
    for &e in h.iter() {
        push_reserved(&mut grown, e, "deadline heap (grown)");
    }
    core::mem::replace(h, grown)
}

/// [`push`] an entry its owner holds a slot for. A kind with more entries than slots is a
/// reservation missing somewhere, which a debug build stops at.
pub(super) fn push_promised(g: &mut SchedState, e: Entry) -> Result<(), AllocError> {
    debug_assert!(
        g.deadlines.iter().filter(|x| x.kind == e.kind).count() < g.deadline_slots[e.kind as usize],
        "a {:?} deadline with no slot promised",
        e.kind
    );
    push(&mut g.deadlines, e)
}

/// The earliest entry, or `None` if empty.
pub(super) fn peek(h: &KVec<Entry>) -> Option<Entry> {
    h.first().copied()
}

/// Insert `e`, sifting up by `deadline_ns`. `Err` if at reserve (the caller
/// maps it to an out-of-memory failure; never grows under the lock).
pub(super) fn push(h: &mut KVec<Entry>, e: Entry) -> Result<(), AllocError> {
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
pub(super) fn pop_min(h: &mut KVec<Entry>) -> Option<Entry> {
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
pub(super) fn remove(h: &mut KVec<Entry>, target: usize, kind: DeadlineKind) -> bool {
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

fn sift_up(h: &mut KVec<Entry>, mut i: usize) {
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

fn sift_down(h: &mut KVec<Entry>, mut i: usize) {
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
