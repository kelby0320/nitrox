//! **A raw input node's reader side**, shared by every driver that publishes one (Phase 6 Part
//! B.1): the event ring, the one read parked on it, and the hand-off of a finished read from the
//! interrupt path to thread context.
//!
//! `docs/architecture/input-subsystem.md`. It was the PS/2 driver's until a second producer — USB
//! HID — needed the same thing. It moved rather than being copied because the hand-off is the
//! subtle part: an earlier version let another CPU drop a read the DPC was still using, a
//! use-after-free (PR #178 review, blocking 1). One copy of that reasoning is what keeps it fixed.
//!
//! ## The shape
//!
//! A driver keeps one [`Reader`] per node, under a lock of its own: the PS/2 driver one lock over
//! its two, since they share a controller. Everything here runs under that lock except
//! [`deliver`], which copies and completes, and the drop of a read [`Reader::take_owed`] hands
//! back — the two things that must not happen with a leaf lock held, or in a DPC at all.
//!
//! 1. **A read** ([`Reader::read_now`]) is satisfied from the ring at once, refused if another is
//!    parked (one reader per device), or parked ([`Reader::park`]) until events arrive.
//! 2. **The interrupt path** pushes events into the ring, and its DPC takes a parked read that now
//!    has something to deliver ([`Reader::take_ready`]). **Taken out, it is the DPC's alone**: no
//!    other CPU can reach it to drop it while its pointers are in use.
//! 3. The DPC delivers it ([`deliver`]) with no lock held, then hands it back ([`Reader::owe`]).
//!    **The DPC never drops it**: a last-reference drop reaches `SlabCache::free`, whose plain
//!    `SpinLock` is the same-CPU deadlock fixed for `io::block`.
//! 4. Thread context takes it ([`Reader::take_owed`]) and drops it, outside the lock: from
//!    `sched::reap_pending` on any CPU going idle, and before a new read parks.
//!
//! **And the key count**: how many key presses any keyboard has decoded since boot, which the
//! hardware report turns its pages on.

pub mod ring;

use core::sync::atomic::{AtomicU64, Ordering};

use crate::libkern::input::INPUT_EVENT_LEN;
use crate::mm::{PAGE_SIZE, heap};
use crate::object::{MemoryObject, ObjectRef};
use crate::syscall::error::KError;
use ring::EventRing;

/// Scratch for one drain. Sized to the ring, so a large read is satisfied in one pass.
pub const DRAIN_MAX: usize = ring::RING_EVENTS * INPUT_EVENT_LEN;

/// A parked `sys_io_submit(Read)` waiting for events on one node.
pub struct ParkedRead {
    po: ObjectRef,
    buffer: ObjectRef,
    buf_offset: u64,
    max_len: usize,
}

impl ParkedRead {
    /// A read of up to `max_len` bytes into `buffer` at `buf_offset`, completing `po`.
    pub fn new(po: &ObjectRef, buffer: &ObjectRef, buf_offset: u64, max_len: usize) -> ParkedRead {
        ParkedRead { po: po.clone(), buffer: buffer.clone(), buf_offset, max_len }
    }
}

/// What a read found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReadNow {
    /// This many bytes of records, drained into the caller's scratch.
    Drained(usize),
    /// Nothing in the ring, and no read parked: the caller parks this one.
    Empty,
    /// Another read is already parked. One reader per device.
    Busy,
}

/// **One node's reader side**: its ring, the read parked on it, and a finished read owed to
/// thread context. Its owner keeps it under a lock.
pub struct Reader {
    /// The node's events. The interrupt path pushes them; a read drains them.
    pub ring: EventRing,
    parked: Option<ParkedRead>,
    /// A read the DPC has finished with, awaiting a thread-context drop.
    to_drop: Option<ParkedRead>,
}

impl Reader {
    /// An empty reader.
    pub const fn new() -> Reader {
        Reader { ring: EventRing::new(), parked: None, to_drop: None }
    }

    /// **A read arriving**: drain up to `out`'s length of whole records into it if the ring has
    /// any, or say whether the caller may park.
    pub fn read_now(&mut self, out: &mut [u8], now_ns: u64) -> ReadNow {
        if !self.ring.is_empty() {
            ReadNow::Drained(self.ring.drain_into(out, now_ns))
        } else if self.parked.is_some() {
            ReadNow::Busy
        } else {
            ReadNow::Empty
        }
    }

    /// Park `read` until events arrive. The caller has had [`ReadNow::Empty`] under the same lock.
    pub fn park(&mut self, read: ParkedRead) {
        debug_assert!(self.parked.is_none(), "one reader per device");
        self.parked = Some(read);
    }

    /// Whether a read is parked: what an interrupt checks before queueing its DPC.
    pub fn has_parked(&self) -> bool {
        self.parked.is_some()
    }

    /// **The DPC's half**: take the parked read if the ring has something for it, and drain into
    /// `scratch` what it asked for. The read is the caller's alone from here until [`Reader::owe`].
    pub fn take_ready(&mut self, scratch: &mut [u8], now_ns: u64) -> Option<(ParkedRead, usize)> {
        if self.parked.is_none() || self.ring.is_empty() {
            return None;
        }
        let read = self.parked.take()?;
        let len = read.max_len.min(scratch.len());
        let n = self.ring.drain_into(&mut scratch[..len], now_ns);
        Some((read, n))
    }

    /// Hand a delivered read back, for thread context to drop. Last, so it is never reachable
    /// while its pointers are still in use.
    pub fn owe(&mut self, read: ParkedRead) {
        debug_assert!(
            self.to_drop.is_none(),
            "a second completion before a reclaim: a read drains what is owed first, so a new \
             read cannot be parked while one is owed"
        );
        self.to_drop = Some(read);
    }

    /// The read owed to thread context, if any. **Drop it outside the lock**: a drop reaches the
    /// allocator.
    pub fn take_owed(&mut self) -> Option<ParkedRead> {
        self.to_drop.take()
    }
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

/// The bytes a read of `max_len` may take: **floored to whole records** and to [`DRAIN_MAX`]. A
/// reader asking for 24 bytes gets one 16-byte event, not one and a half — the record has no sync
/// word, so a partial tail would misalign everything after it (`input-subsystem.md` §3a). Too
/// small for even one record is refused: completing with zero would look like end-of-stream to a
/// reader that simply passed a short buffer.
pub fn read_len(max_len: u64) -> Result<usize, KError> {
    let len = ((max_len as usize) / INPUT_EVENT_LEN * INPUT_EVENT_LEN).min(DRAIN_MAX);
    if len == 0 { Err(KError::InvalidArgument) } else { Ok(len) }
}

/// **Copy `bytes` into a read's buffer and complete its operation.** No lock held: completing takes
/// the scheduler's.
pub fn deliver(read: &ParkedRead, bytes: &[u8]) {
    // SAFETY: `read` owns the `ObjectRef` pinning this `MemoryObject`, and the caller keeps `read`
    // for the duration.
    unsafe { copy_into_memobj(read.buffer.as_ptr(), read.buf_offset, bytes) };
    crate::sched::complete_pending_op(read.po.as_ptr(), 0, bytes.len() as u64);
}

/// Copy `bytes` into `buffer` at `buf_offset` and complete `po`: a read satisfied at once, from
/// thread context, with the caller's references.
pub fn deliver_now(buffer: &ObjectRef, po: &ObjectRef, buf_offset: u64, bytes: &[u8]) {
    // SAFETY: `buffer` is the caller's live `MemoryObject` reference, held across this.
    unsafe { copy_into_memobj(buffer.as_ptr(), buf_offset, bytes) };
    crate::sched::complete_pending_op(po.as_ptr(), 0, bytes.len() as u64);
}

/// Copy `src` into `buffer`'s frames starting at byte `buf_offset`, via the HHDM. The
/// caller has bounds-checked the range (`sys_io_submit` does).
///
/// # Safety
///
/// `buffer` must point at a live `MemoryObject` — held alive by an `ObjectRef` the caller
/// keeps for the duration. The DPC passes a *borrowed* pointer precisely so it never owns a
/// reference it would then have to drop.
unsafe fn copy_into_memobj(buffer: *const (), buf_offset: u64, src: &[u8]) {
    // SAFETY: the caller guarantees `buffer` pins a live `MemoryObject`.
    let mo: &MemoryObject = unsafe { &*(buffer as *const MemoryObject) };
    let frames = mo.frames();
    let hhdm = heap::hhdm_offset();
    let mut pos = buf_offset as usize;
    for &b in src {
        let page = pos / PAGE_SIZE;
        let intra = pos % PAGE_SIZE;
        if page >= frames.len() {
            break;
        }
        let dst = (frames[page].as_u64() + hhdm) as *mut u8;
        // SAFETY: within an owned, HHDM-mapped buffer frame (bounds pre-checked).
        unsafe { *dst.add(intra) = b };
        pos += 1;
    }
}

/// Key presses decoded since boot, by every keyboard.
static KEY_PRESSES: AtomicU64 = AtomicU64::new(0);

/// A keyboard decoded a press. A held key's typematic repeats count, as the keyboard sends each
/// as a press. What the hardware report waits on (Phase 5 Part D.3): it needs to know *that* a key
/// went down and nothing about which, so no keystroke is kept where a log could show it.
pub fn note_key_press() {
    KEY_PRESSES.fetch_add(1, Ordering::Relaxed);
}

/// Key presses decoded since boot. Compare two readings to learn whether a key went down between
/// them; the count says nothing about which key.
pub fn key_presses() -> u64 {
    KEY_PRESSES.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::KBox;
    use crate::libkern::handle::KObjectType;
    use crate::libkern::input::{InputEvent, KEY_PRESS};
    use crate::mm::test_support::init_global_heap;
    use crate::object::PendingOperation;
    use crate::object::header::test_probe;

    fn adopt<T>(b: KBox<T>, ty: KObjectType) -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference of a `ty` object.
        unsafe { ObjectRef::from_raw(KBox::into_raw(b).as_ptr() as *mut (), ty) }
    }

    fn read(max_len: usize) -> ParkedRead {
        let po = adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
        let buf = adopt(MemoryObject::try_new(PAGE_SIZE).unwrap(), KObjectType::MemoryObject);
        ParkedRead::new(&po, &buf, 0, max_len)
    }

    fn key(n: u16) -> InputEvent {
        InputEvent::key(n, KEY_PRESS, 7)
    }

    /// **A read finds events, or room to park, or another reader** — one reader per device.
    #[test]
    fn a_read_drains_parks_or_is_refused() {
        init_global_heap();
        let mut r = Reader::new();
        let mut out = [0u8; DRAIN_MAX];
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Empty);
        r.park(read(INPUT_EVENT_LEN));
        assert!(r.has_parked());
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Busy, "a second reader while one is parked");
        r.ring.push(key(30));
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Drained(INPUT_EVENT_LEN), "events are taken at once");
    }

    /// **The DPC's half takes the parked read only when there is something for it**, drains no more
    /// than it asked for, and leaves the rest in the ring.
    #[test]
    fn a_parked_read_is_taken_once_with_what_it_asked_for() {
        init_global_heap();
        let mut r = Reader::new();
        let mut scratch = [0u8; DRAIN_MAX];
        r.park(read(INPUT_EVENT_LEN));
        assert!(r.take_ready(&mut scratch, 0).is_none(), "nothing in the ring, nothing taken");
        r.ring.push(key(30));
        r.ring.push(key(31));
        let (_read, n) = r.take_ready(&mut scratch, 0).expect("events and a reader");
        assert_eq!(n, INPUT_EVENT_LEN, "one record, as asked");
        assert!(!r.has_parked(), "taken out: the DPC's alone");
        assert!(!r.ring.is_empty(), "the second event waits for the next read");
        assert!(r.take_ready(&mut scratch, 0).is_none(), "and no second taker");
    }

    /// **A finished read is dropped only by thread context.** Handing it back destroys nothing;
    /// taking it is what lets it go, and that is the caller's, outside the lock. The DPC that
    /// dropped one was the same-CPU deadlock, and the one that let another CPU drop it early was a
    /// use-after-free.
    #[test]
    fn a_delivered_read_is_dropped_by_its_reclaimer_alone() {
        init_global_heap();
        test_probe::reset();
        let mut r = Reader::new();
        let mut scratch = [0u8; DRAIN_MAX];
        r.park(read(INPUT_EVENT_LEN));
        r.ring.push(key(30));
        let (done, _) = r.take_ready(&mut scratch, 0).unwrap();
        r.owe(done);
        assert_eq!(test_probe::pending_op_destroys(), 0, "owed, not dropped");
        let owed = r.take_owed().expect("owed to thread context");
        assert!(r.take_owed().is_none(), "once");
        drop(owed);
        assert_eq!(test_probe::pending_op_destroys(), 1, "dropped by the reclaimer");
    }

    /// **A read takes whole records**: floored to the record and to the scratch, and refused below
    /// one.
    #[test]
    fn a_read_length_is_whole_records() {
        assert_eq!(read_len(24), Ok(INPUT_EVENT_LEN));
        assert_eq!(read_len(INPUT_EVENT_LEN as u64), Ok(INPUT_EVENT_LEN));
        assert_eq!(read_len(INPUT_EVENT_LEN as u64 - 1), Err(KError::InvalidArgument));
        assert_eq!(read_len(u32::MAX as u64), Ok(DRAIN_MAX));
    }
}
