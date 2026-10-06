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

use crate::libkern::input::{INPUT_EVENT_LEN, InputEvent};
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
    /// **The device has gone and its ring is drained** (Phase 6 Part C): the read is refused at
    /// submission, `PeerClosed`, with no operation to wait on — so a reader cannot spin on reads that
    /// each complete with an error.
    Gone,
}

/// **One node's reader side**: its ring, the read parked on it, and a finished read owed to
/// thread context. Its owner keeps it under a lock.
pub struct Reader {
    /// The node's events. The interrupt path pushes them; a read drains them.
    pub ring: EventRing,
    parked: Option<ParkedRead>,
    /// A read the DPC has finished with, awaiting a thread-context drop.
    to_drop: Option<ParkedRead>,
    /// The device has gone (Phase 6 Part C): what the ring holds is still read, and then nothing.
    retired: bool,
}

impl Reader {
    /// An empty reader.
    pub const fn new() -> Reader {
        Reader { ring: EventRing::new(), parked: None, to_drop: None, retired: false }
    }

    /// **A read arriving**: drain up to `out`'s length of whole records into it if the ring has
    /// any, or say whether the caller may park — never, once the device has gone.
    pub fn read_now(&mut self, out: &mut [u8], now_ns: u64) -> ReadNow {
        if !self.ring.is_empty() {
            ReadNow::Drained(self.ring.drain_into(out, now_ns))
        } else if self.retired {
            ReadNow::Gone
        } else if self.parked.is_some() {
            ReadNow::Busy
        } else {
            ReadNow::Empty
        }
    }

    /// **Events from the interrupt path**, pushed as one group, or dropped once the device has gone
    /// (PR #361 review): a retired node's ring ends with the releases its departure pushed, and an
    /// event after them would be a key pressed on a keyboard that is not there. Whether they were
    /// pushed.
    pub fn offer(&mut self, events: &[InputEvent]) -> bool {
        if !self.retired {
            self.ring.push_group(events);
        }
        !self.retired
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

    /// **The device has gone** (Phase 6 Part C). What the ring holds is still read, and a read after
    /// that is [`ReadNow::Gone`]. A read parked on an empty ring is handed back, for the caller to
    /// [`refuse`] outside the lock; the caller takes a read the ring can answer with
    /// [`take_ready`](Self::take_ready) first, so the releases a departure pushed reach it.
    pub fn retire(&mut self) -> Option<ParkedRead> {
        self.retired = true;
        if self.ring.is_empty() { self.parked.take() } else { None }
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

/// **Complete a parked read with `err`** and no bytes: a read waiting on a device that has gone
/// (Phase 6 Part C). No lock held.
pub fn refuse(read: &ParkedRead, err: KError) {
    crate::sched::complete_pending_op(read.po.as_ptr(), err as i32, 0);
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
    mo.copy_in(buf_offset as usize, src);
}

/// **The lights a write to a keyboard's node carries** (Phase 6 Part B.5): exactly one byte at
/// `buf_offset` in `buffer`, of `LIGHT_NUM`, `LIGHT_CAPS` and `LIGHT_SCROLL` alone. The byte, or
/// `InvalidArgument` for another length or another bit. Thread context, from `sys_io_submit`.
pub fn lights_from(buffer: &ObjectRef, buf_offset: u64, len: u64) -> Result<u8, KError> {
    use crate::libkern::input::{LIGHT_CAPS, LIGHT_NUM, LIGHT_SCROLL};
    if len != 1 {
        return Err(KError::InvalidArgument);
    }
    // SAFETY: `buffer` is the caller's live `MemoryObject` reference, held across this.
    let mo: &MemoryObject = unsafe { &*(buffer.as_ptr() as *const MemoryObject) };
    let (page, intra) = (buf_offset as usize / PAGE_SIZE, buf_offset as usize % PAGE_SIZE);
    let frame = *mo.frames().get(page).ok_or(KError::InvalidArgument)?;
    // SAFETY: within an owned, HHDM-mapped frame of the buffer (bounds checked by `sys_io_submit`
    // and by `get` above).
    let byte = unsafe { *((frame.as_u64() + heap::hhdm_offset()) as *const u8).add(intra) };
    if byte as u16 & !(LIGHT_NUM | LIGHT_CAPS | LIGHT_SCROLL) != 0 {
        return Err(KError::InvalidArgument);
    }
    Ok(byte)
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
    use crate::libkern::input::KEY_PRESS;
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

    /// **A retired reader is read until its ring is empty, and then refuses** (Phase 6 Part C): the
    /// releases a departure pushes still reach whoever reads next, and after them a read is `Gone`,
    /// never parked. A read parked on an empty ring when the device goes is handed back to refuse.
    #[test]
    fn a_retired_reader_drains_then_refuses() {
        init_global_heap();
        let mut r = Reader::new();
        let mut out = [0u8; DRAIN_MAX];
        r.ring.push(key(30));
        assert!(r.retire().is_none(), "nothing parked");
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Drained(INPUT_EVENT_LEN), "its events still read");
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Gone);
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Gone, "and stays gone");

        let mut parked = Reader::new();
        parked.park(read(INPUT_EVENT_LEN));
        assert!(parked.retire().is_some(), "a read waiting on an empty ring is handed back");
        assert!(!parked.has_parked());
        assert_eq!(parked.read_now(&mut out, 0), ReadNow::Gone);
    }

    /// **A retired reader takes no more events** (PR #361 review): its ring ends with what its
    /// departure pushed, so a report the DPC pushes late cannot follow the releases.
    #[test]
    fn a_retired_reader_takes_no_more_events() {
        init_global_heap();
        let mut r = Reader::new();
        let mut out = [0u8; DRAIN_MAX];
        assert!(r.offer(&[key(30)]), "taken while the device is here");
        r.ring.push(key(31));
        assert!(r.retire().is_none());
        assert!(!r.offer(&[key(32)]), "and not after it has gone");
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Drained(2 * INPUT_EVENT_LEN), "only what came before");
        assert_eq!(InputEvent::read(&out[INPUT_EVENT_LEN..]).map(|e| e.code), Some(31));
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Gone);
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

    /// **A lights write is one byte of the three lights** (Phase 6 Part B.5): another length or
    /// another bit is refused, before any driver sees it.
    #[test]
    fn a_lights_write_is_one_byte_of_three_bits() {
        init_global_heap();
        let mo = MemoryObject::try_new_filled(&[0x02, 0x08, 0x07]).unwrap();
        let buf = adopt(mo, KObjectType::MemoryObject);
        assert_eq!(lights_from(&buf, 0, 1), Ok(0x02), "Caps Lock");
        assert_eq!(lights_from(&buf, 2, 1), Ok(0x07), "all three");
        assert_eq!(lights_from(&buf, 1, 1), Err(KError::InvalidArgument), "bit 3 is no light");
        assert_eq!(lights_from(&buf, 0, 2), Err(KError::InvalidArgument), "two bytes");
        // **Held for the function's sake**: `sys_io_submit` answers a zero-length request itself,
        // before any driver, so no write reaches this with none (PR #359 review).
        assert_eq!(lights_from(&buf, 0, 0), Err(KError::InvalidArgument), "none");
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
