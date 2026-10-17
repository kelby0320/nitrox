//! **A raw input node's reader side**, shared by every driver that publishes one: the event ring,
//! the one read parked on it, and the hand-off of a finished read from the interrupt path to thread
//! context (`docs/architecture/input-subsystem.md`). One copy, because the hand-off is the subtle
//! part: an earlier version let another CPU drop a read the DPC was still using.
//!
//! A driver keeps one [`Reader`] per node, under a lock of its own (PS/2: one over its two).
//! Everything here runs under that lock except [`deliver`] and the drop of a read
//! [`Reader::take_owed`] hands back, which must not happen with a leaf lock held, or in a DPC at
//! all.
//!
//! 1. **A read** ([`Reader::read_now`]) is satisfied from the ring at once, refused if another is
//!    parked, or parked ([`Reader::park`]) until events arrive, never beside an unreclaimed read
//!    ([`read_or_park`]).
//! 2. **The interrupt path** pushes events, and its DPC takes a parked read that has something to
//!    deliver ([`Reader::take_ready`]). **Taken out, it is the DPC's alone.**
//! 3. The DPC delivers it ([`deliver`]) with no lock held, then hands it back ([`Reader::owe`]).
//!    **The DPC never drops it**: a last-reference drop reaches `SlabCache::free`, whose plain
//!    `SpinLock` is the same-CPU deadlock fixed for `io::block`.
//! 4. Thread context takes it ([`Reader::take_owed`]) and drops it outside the lock: from
//!    `sched::reap_pending` and before a new read parks.
//!
//! It also holds the key count the hardware report turns its pages on.

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
    /// **The device has gone and its ring is drained**: the read is refused at submission with
    /// `PeerClosed`, so a reader cannot spin on reads that each complete with an error.
    Gone,
    /// The read before is still being delivered, or is owed: reclaim and ask again.
    Retry,
}

/// What a read's submission came to: [`ReadNow`] with its `Retry` waited out by [`read_or_park`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// This many bytes drained into the caller's scratch.
    Drained(usize),
    /// The read is parked until events arrive.
    Parked,
    /// Another read is already parked.
    Busy,
    /// The device has gone and its ring is drained.
    Gone,
}

/// **One node's reader side**: its ring, the read parked on it, and a finished read owed.
pub struct Reader {
    /// The node's events: the interrupt path pushes them, a read drains them.
    pub ring: EventRing,
    parked: Option<ParkedRead>,
    to_drop: Option<ParkedRead>,
    delivering: bool,
    /// The device has gone: what the ring holds is still read, and then nothing.
    retired: bool,
}

impl Reader {
    /// An empty reader.
    pub const fn new() -> Reader {
        Reader { ring: EventRing::new(), parked: None, to_drop: None, delivering: false, retired: false }
    }

    /// **A read arriving**: drain whole records if the ring has any, else say whether the caller
    /// may park.
    pub fn read_now(&mut self, out: &mut [u8], now_ns: u64) -> ReadNow {
        if !self.ring.is_empty() {
            ReadNow::Drained(self.ring.drain_into(out, now_ns))
        } else if self.retired {
            ReadNow::Gone
        } else if self.parked.is_some() {
            ReadNow::Busy
        } else if self.delivering || self.to_drop.is_some() {
            ReadNow::Retry
        } else {
            ReadNow::Empty
        }
    }

    /// **Events from the interrupt path**, pushed as one group, or dropped once the device has
    /// gone: a retired node's ring ends with the releases its departure pushed. Whether they
    /// were pushed.
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
        let taken = self.take_parked(scratch, now_ns);
        self.delivering |= taken.is_some();
        taken
    }

    fn take_parked(&mut self, scratch: &mut [u8], now_ns: u64) -> Option<(ParkedRead, usize)> {
        if self.parked.is_none() || self.ring.is_empty() {
            return None;
        }
        let read = self.parked.take()?;
        let len = read.max_len.min(scratch.len());
        let n = self.ring.drain_into(&mut scratch[..len], now_ns);
        Some((read, n))
    }

    /// Hand a delivered read back for thread context to drop; done last, after use.
    pub fn owe(&mut self, read: ParkedRead) {
        debug_assert!(
            self.to_drop.is_none(),
            "a second completion before a reclaim: a read parks only when none is owed or in flight"
        );
        self.to_drop = Some(read);
        self.delivering = false;
    }

    /// The read owed to thread context, if any. **Drop it outside the lock**: it frees memory.
    pub fn take_owed(&mut self) -> Option<ParkedRead> {
        self.to_drop.take()
    }

    /// **The device has gone.** What the ring holds is still read, and a read after that is
    /// [`ReadNow::Gone`]. The parked read is handed back, not owed: with what the ring answers it,
    /// for the caller to [`deliver`], or to [`refuse`] on an empty ring.
    pub fn retire(&mut self, scratch: &mut [u8], now_ns: u64) -> (Option<(ParkedRead, usize)>, Option<ParkedRead>) {
        let ready = self.take_parked(scratch, now_ns);
        self.retired = true;
        let refused = if self.ring.is_empty() { self.parked.take() } else { None };
        (ready, refused)
    }
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

/// The bytes a read of `max_len` may take: **floored to whole records** and to [`DRAIN_MAX`] — the
/// record has no sync word, so a partial tail would misalign everything after it
/// (`input-subsystem.md` §3a). Too small for one record is refused: zero would look like
/// end-of-stream.
pub fn read_len(max_len: u64) -> Result<usize, KError> {
    let len = ((max_len as usize) / INPUT_EVENT_LEN * INPUT_EVENT_LEN).min(DRAIN_MAX);
    if len == 0 { Err(KError::InvalidArgument) } else { Ok(len) }
}

/// **A read's submission**: `attempt` (which parks on [`ReadNow::Empty`]) until it is not
/// [`ReadNow::Retry`], with `reclaim` before each, so a read waits out the delivery before it
/// rather than parking beside it. Thread context.
pub fn read_or_park(mut reclaim: impl FnMut(), mut attempt: impl FnMut() -> ReadNow) -> Submitted {
    loop {
        reclaim();
        match attempt() {
            ReadNow::Drained(n) => return Submitted::Drained(n),
            ReadNow::Empty => return Submitted::Parked,
            ReadNow::Busy => return Submitted::Busy,
            ReadNow::Gone => return Submitted::Gone,
            ReadNow::Retry => core::hint::spin_loop(),
        }
    }
}

/// **Copy `bytes` into a read's buffer and complete it**; no lock held, as completing takes the
/// scheduler's.
pub fn deliver(read: &ParkedRead, bytes: &[u8]) {
    // SAFETY: `read` owns the `ObjectRef` pinning this `MemoryObject`, and the caller keeps `read`
    // for the duration.
    unsafe { copy_into_memobj(read.buffer.as_ptr(), read.buf_offset, bytes) };
    // SAFETY: `read` owns the reference pinning its `PendingOperation`.
    unsafe { crate::sched::complete_pending_op(read.po.as_ptr(), 0, bytes.len() as u64) };
}

/// **Complete a parked read with `err`** and no bytes, for a device that has gone. No lock held.
pub fn refuse(read: &ParkedRead, err: KError) {
    // SAFETY: `read` owns the reference pinning its `PendingOperation`.
    unsafe { crate::sched::complete_pending_op(read.po.as_ptr(), err as i32, 0) };
}

/// Copy `bytes` into `buffer` at `buf_offset` and complete `po`: a read satisfied at once.
pub fn deliver_now(buffer: &ObjectRef, po: &ObjectRef, buf_offset: u64, bytes: &[u8]) {
    // SAFETY: `buffer` is the caller's live `MemoryObject` reference, held across this.
    unsafe { copy_into_memobj(buffer.as_ptr(), buf_offset, bytes) };
    // SAFETY: `po` is the caller's `PendingOperation` reference.
    unsafe { crate::sched::complete_pending_op(po.as_ptr(), 0, bytes.len() as u64) };
}

/// Copy `src` into `buffer`'s frames at `buf_offset`, via the HHDM; the caller bounds-checks it.
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

/// **The lights a write to a keyboard's node carries**: exactly one byte at `buf_offset`, of
/// `LIGHT_NUM`, `LIGHT_CAPS` and `LIGHT_SCROLL` alone, else `InvalidArgument`. Thread context.
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

static KEY_PRESSES: AtomicU64 = AtomicU64::new(0);

/// A keyboard decoded a press (typematic repeats count). The hardware report needs *that* a key
/// went down, not which, so no keystroke is kept.
pub fn note_key_press() {
    KEY_PRESSES.fetch_add(1, Ordering::Relaxed);
}

/// Key presses decoded since boot; two readings tell whether a key went down between them.
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

    /// **A retired reader is read until its ring is empty, and then refuses.**
    #[test]
    fn a_retired_reader_drains_then_refuses() {
        init_global_heap();
        let mut r = Reader::new();
        let mut out = [0u8; DRAIN_MAX];
        r.ring.push(key(30));
        assert!(r.retire(&mut out, 0).1.is_none(), "nothing parked");
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Drained(INPUT_EVENT_LEN), "its events still read");
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Gone);
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Gone, "and stays gone");

        let mut parked = Reader::new();
        parked.park(read(INPUT_EVENT_LEN));
        assert!(parked.retire(&mut out, 0).1.is_some(), "a read waiting on an empty ring is handed back");
        assert!(!parked.has_parked());
        assert_eq!(parked.read_now(&mut out, 0), ReadNow::Gone);
    }

    #[test]
    fn a_retired_reader_takes_no_more_events() {
        init_global_heap();
        let mut r = Reader::new();
        let mut out = [0u8; DRAIN_MAX];
        assert!(r.offer(&[key(30)]), "taken while the device is here");
        r.ring.push(key(31));
        assert!(r.retire(&mut [0u8; DRAIN_MAX], 0).1.is_none());
        assert!(!r.offer(&[key(32)]), "and not after it has gone");
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Drained(2 * INPUT_EVENT_LEN), "only what came before");
        assert_eq!(InputEvent::read(&out[INPUT_EVENT_LEN..]).map(|e| e.code), Some(31));
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Gone);
    }

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

    /// The `check-logout` interleaving: a reader woken by a delivery must not park before reclaim.
    #[test]
    fn a_read_waits_out_the_delivery_before_it() {
        init_global_heap();
        let mut r = Reader::new();
        let mut scratch = [0u8; DRAIN_MAX];
        let mut out = [0u8; DRAIN_MAX];
        r.park(read(INPUT_EVENT_LEN));
        r.ring.push(key(30));
        let (first, _) = r.take_ready(&mut scratch, 0).unwrap();
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Retry, "in flight");
        r.owe(first);
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Retry, "owed");
        assert!(r.take_owed().is_some());
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Empty);
        r.park(read(INPUT_EVENT_LEN));
        r.ring.push(key(31));
        let (second, _) = r.take_ready(&mut scratch, 0).unwrap();
        r.owe(second);
        r.ring.push(key(32));
        assert_eq!(r.read_now(&mut out, 0), ReadNow::Drained(INPUT_EVENT_LEN), "events never wait");
    }

    #[test]
    fn read_or_park_reclaims_before_every_attempt() {
        let calls = core::cell::RefCell::new(std::vec::Vec::new());
        let mut answers = [ReadNow::Retry, ReadNow::Retry, ReadNow::Empty].into_iter();
        let found = read_or_park(
            || calls.borrow_mut().push("reclaim"),
            || {
                calls.borrow_mut().push("attempt");
                answers.next().unwrap()
            },
        );
        assert_eq!(found, Submitted::Parked);
        assert_eq!(*calls.borrow(), ["reclaim", "attempt", "reclaim", "attempt", "reclaim", "attempt"]);
    }

    #[test]
    fn retiring_leaves_no_delivery_in_flight() {
        init_global_heap();
        let mut r = Reader::new();
        let mut scratch = [0u8; DRAIN_MAX];
        r.park(read(INPUT_EVENT_LEN));
        r.ring.push(key(30));
        let (ready, refused) = r.retire(&mut scratch, 0);
        assert!(ready.is_some() && refused.is_none(), "the ring answers it");
        assert!(!r.delivering);
        r.to_drop = Some(read(INPUT_EVENT_LEN));
        assert_eq!(r.read_now(&mut scratch, 0), ReadNow::Gone);
    }

    /// **A finished read is dropped only by thread context.**
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

    #[test]
    fn a_lights_write_is_one_byte_of_three_bits() {
        init_global_heap();
        let mo = MemoryObject::try_new_filled(&[0x02, 0x08, 0x07]).unwrap();
        let buf = adopt(mo, KObjectType::MemoryObject);
        assert_eq!(lights_from(&buf, 0, 1), Ok(0x02), "Caps Lock");
        assert_eq!(lights_from(&buf, 2, 1), Ok(0x07), "all three");
        assert_eq!(lights_from(&buf, 1, 1), Err(KError::InvalidArgument), "bit 3 is no light");
        assert_eq!(lights_from(&buf, 0, 2), Err(KError::InvalidArgument), "two bytes");
        assert_eq!(lights_from(&buf, 0, 0), Err(KError::InvalidArgument), "none");
    }

    #[test]
    fn a_read_length_is_whole_records() {
        assert_eq!(read_len(24), Ok(INPUT_EVENT_LEN));
        assert_eq!(read_len(INPUT_EVENT_LEN as u64), Ok(INPUT_EVENT_LEN));
        assert_eq!(read_len(INPUT_EVENT_LEN as u64 - 1), Err(KError::InvalidArgument));
        assert_eq!(read_len(u32::MAX as u64), Ok(DRAIN_MAX));
    }
}

/// **Drives a driver's own read through the race**: a second read must not park beside the first.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::drivers::adopt;
    use crate::libkern::handle::KObjectType;
    use crate::mm::PAGE_SIZE;
    use crate::object::{MemoryObject, ObjectRef, PendingOperation};
    use crate::syscall::error::KError;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    pub(crate) fn a_read_waits_out_the_delivery_before_it<S: Send + Sync + 'static, T>(
        state: S,
        read: fn(&S, &ObjectRef, &ObjectRef) -> Result<(), KError>,
        take: fn(&S) -> T,
        owe: fn(&S, T),
        parked: fn(&S) -> bool,
    ) {
        crate::mm::test_support::init_global_heap();
        let state = Arc::new(state);
        let buf = adopt(MemoryObject::try_new(PAGE_SIZE).unwrap(), KObjectType::MemoryObject);
        let po = || adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
        assert_eq!(read(&state, &buf, &po()), Ok(()));
        let first = take(&state);
        let (done, answer) = mpsc::channel();
        let (second, buf2, po2) = (state.clone(), buf.clone(), po());
        std::thread::spawn(move || done.send(read(&second, &buf2, &po2)).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!parked(&state), "parked beside a delivery in flight");
        owe(&state, first);
        let r = answer.recv_timeout(Duration::from_secs(10)).expect("still waiting once the first was handed back");
        assert_eq!(r, Ok(()));
        assert!(parked(&state), "parked once the first was reclaimed");
    }
}
