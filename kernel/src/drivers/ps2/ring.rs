//! The per-device event ring: **whole records in, whole records out, losses announced**.
//!
//! `docs/architecture/input-subsystem.md` §3a. This is the design's one deliberate departure from
//! reusing the console's mechanism unchanged, and it exists because the two obvious choices
//! do not compose:
//!
//! - [`InputEvent`](crate::libkern::input::InputEvent) is **fixed-size**.
//! - The console's ring is **byte**-granular and drops a single byte when full.
//!
//! One byte lost at a non-record boundary misaligns the stream *permanently*: `kind` starts
//! reading the high half of the previous `time_ns`, and nothing recovers, because the record
//! has no sync word and `EV_SYN` is a value *inside* a record rather than a framing marker
//! anyone can scan for. Splitting a read across a record boundary is harmless — the reader
//! buffers the remainder — but dropping part of one is not.
//!
//! So: the ring stores `InputEvent`s, not bytes; overflow discards the **oldest whole
//! record**; and the next drain is preceded by
//! [`SYN_DROPPED`](crate::libkern::input::SYN_DROPPED) so the consumer discards accumulated
//! state rather than carrying a phantom held key for the rest of the session.
//!
//! **The oldest is discarded, not the newest.** Input is a stream whose value decays: the
//! most recent events are the ones a user is waiting to see happen. Dropping the newest
//! would also make the eventual `SYN_DROPPED` describe a gap that had already been
//! interpreted.

use crate::libkern::input::{INPUT_EVENT_LEN, InputEvent};

/// Events held per device.
///
/// Sized against a burst, not a backlog: 128 events is roughly a second of vigorous mouse
/// movement (a PS/2 mouse reports at 100 Hz, up to three events per report) or far more
/// typing than anyone produces between two scheduler quanta. A consumer that falls further
/// behind than this is not slow, it is broken, and `SYN_DROPPED` is how it finds out.
pub const RING_EVENTS: usize = 128;

/// A fixed-capacity ring of whole [`InputEvent`]s with loss announcement.
pub struct EventRing {
    buf: [InputEvent; RING_EVENTS],
    head: usize,
    len: usize,
    /// Whole records discarded since the last announcement, `0` when nothing is owed.
    lost: u32,
}

impl EventRing {
    /// An empty ring.
    pub const fn new() -> Self {
        Self {
            buf: [InputEvent { kind: 0, code: 0, value: 0, time_ns: 0 }; RING_EVENTS],
            head: 0,
            len: 0,
            lost: 0,
        }
    }

    /// Events currently buffered.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the ring holds nothing to deliver.
    ///
    /// **Deliberately just `len == 0`.** An earlier version also tested `lost == 0`, on the
    /// reasoning that a ring owing a `SYN_DROPPED` still has something to tell the consumer.
    /// That state cannot arise: `lost` is only incremented by [`push`](Self::push) when the
    /// ring is *full*, and that same push immediately refills the slot it freed, so
    /// `lost > 0` implies `len == RING_EVENTS`. `len` falls only through
    /// [`drain_into`](Self::drain_into), which emits the announcement and clears `lost`
    /// before it removes anything. So `len == 0` already implies nothing is owed.
    ///
    /// It is gone rather than kept as defence because it could not be tested: the PR #178
    /// review showed the original assertion held for the broken implementation too, and two
    /// further attempts to construct the state were equally vacuous — which is the argument
    /// for deleting dead code rather than for writing a third contrivance around it.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push one event, discarding the oldest whole record if the ring is full.
    pub fn push(&mut self, e: InputEvent) {
        if self.len == RING_EVENTS {
            self.head = (self.head + 1) % RING_EVENTS;
            self.len -= 1;
            self.lost = self.lost.saturating_add(1);
        }
        let tail = (self.head + self.len) % RING_EVENTS;
        self.buf[tail] = e;
        self.len += 1;
        // Why `is_empty` need not consider `lost`: a loss is only recorded by dropping from
        // a full ring, and this push refills the slot, so an owed announcement always has
        // records travelling with it.
        debug_assert!(self.lost == 0 || self.len == RING_EVENTS, "a loss implies a full ring");
    }

    /// Push a group, all-or-nothing.
    ///
    /// A `SYN`-terminated group is one logical event, and delivering half of it is worse
    /// than delivering none: a consumer would act on a click with no coordinates, or motion
    /// with no terminator. If the group does not fit, the ring makes room by discarding
    /// oldest records — which is a loss it announces — rather than by truncating the group.
    pub fn push_group(&mut self, group: &[InputEvent]) {
        debug_assert!(group.len() <= RING_EVENTS, "a group larger than the ring cannot be atomic");
        for &e in group {
            self.push(e);
        }
    }

    /// Drain whole records into `dst`, which is truncated to a whole number of records.
    ///
    /// Returns bytes written. When a loss is owed, the **first** record written is
    /// `SYN_DROPPED` — before any surviving event, so the consumer resynchronises before
    /// interpreting anything on the far side of the gap.
    pub fn drain_into(&mut self, dst: &mut [u8], now_ns: u64) -> usize {
        let capacity = dst.len() / INPUT_EVENT_LEN;
        if capacity == 0 {
            return 0;
        }
        let mut written = 0usize;
        let mut put = |e: InputEvent, written: &mut usize| {
            let off = *written * INPUT_EVENT_LEN;
            let mut b = [0u8; INPUT_EVENT_LEN];
            e.write(&mut b);
            dst[off..off + INPUT_EVENT_LEN].copy_from_slice(&b);
            *written += 1;
        };

        if self.lost > 0 {
            put(InputEvent::dropped(self.lost as i32, now_ns), &mut written);
            self.lost = 0;
        }
        while written < capacity && self.len > 0 {
            let e = self.buf[self.head];
            self.head = (self.head + 1) % RING_EVENTS;
            self.len -= 1;
            put(e, &mut written);
        }
        written * INPUT_EVENT_LEN
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::input::*;

    /// Scratch large enough for a full ring plus the loss marker.
    const SCRATCH: usize = (RING_EVENTS + 4) * INPUT_EVENT_LEN;

    fn ev(n: u16) -> InputEvent {
        InputEvent::key(n, KEY_PRESS, n as u64)
    }

    /// Drain at most `records` records, returning them in `out` and how many there were.
    fn drain(r: &mut EventRing, records: usize, out: &mut [InputEvent]) -> usize {
        let mut buf = [0u8; SCRATCH];
        let cap = (records * INPUT_EVENT_LEN).min(SCRATCH);
        let n = r.drain_into(&mut buf[..cap], 999);
        let count = n / INPUT_EVENT_LEN;
        for (i, chunk) in buf[..n].chunks_exact(INPUT_EVENT_LEN).enumerate() {
            out[i] = InputEvent::read(chunk).expect("whole record");
        }
        count
    }

    #[test]
    fn events_come_out_in_order() {
        let mut r = EventRing::new();
        for i in 1..=3 {
            r.push(ev(i));
        }
        let mut out = [InputEvent::default(); 8];
        assert_eq!(drain(&mut r, 8, &mut out), 3);
        assert_eq!(&out[..3], &[ev(1), ev(2), ev(3)]);
        assert!(r.is_empty());
    }

    #[test]
    fn a_drain_never_writes_a_partial_record() {
        // The property the whole module exists for. A destination of 1.5 records must yield
        // exactly one, not one and a half.
        let mut r = EventRing::new();
        r.push(ev(1));
        r.push(ev(2));
        let mut buf = [0u8; INPUT_EVENT_LEN + INPUT_EVENT_LEN / 2];
        let n = r.drain_into(&mut buf, 0);
        assert_eq!(n, INPUT_EVENT_LEN, "one whole record, the half discarded from the count");
        assert_eq!(r.len(), 1, "and the second event is still buffered");
    }

    #[test]
    fn a_destination_too_small_for_one_record_takes_nothing() {
        let mut r = EventRing::new();
        r.push(ev(1));
        let mut buf = [0u8; INPUT_EVENT_LEN - 1];
        assert_eq!(r.drain_into(&mut buf, 0), 0);
        assert_eq!(r.len(), 1, "and nothing was consumed");
    }

    #[test]
    fn overflow_discards_the_oldest_whole_record() {
        let mut r = EventRing::new();
        for i in 0..RING_EVENTS as u16 + 3 {
            r.push(ev(i));
        }
        let mut out = [InputEvent::default(); RING_EVENTS + 4];
        let n = drain(&mut r, RING_EVENTS + 4, &mut out);
        assert_eq!(out[0], InputEvent::dropped(3, 999), "the loss is announced first");
        assert_eq!(out[1], ev(3), "the three oldest went, not the three newest");
        assert_eq!(out[n - 1], ev(RING_EVENTS as u16 + 2));
    }

    #[test]
    fn syn_dropped_precedes_the_survivors_not_follows_them() {
        // Ordering is the point: a consumer must resynchronise *before* interpreting
        // anything from the far side of the gap, or it applies post-gap events to pre-gap
        // state.
        let mut r = EventRing::new();
        for i in 0..RING_EVENTS as u16 + 1 {
            r.push(ev(i));
        }
        let mut out = [InputEvent::default(); 4];
        assert_eq!(drain(&mut r, 2, &mut out), 2);
        assert_eq!(out[0].kind, EV_SYN);
        assert_eq!(out[0].code, SYN_DROPPED);
        assert_eq!(out[1], ev(1), "then the oldest survivor");
    }

    #[test]
    fn a_loss_is_announced_once() {
        let mut r = EventRing::new();
        for i in 0..RING_EVENTS as u16 + 1 {
            r.push(ev(i));
        }
        let mut out = [InputEvent::default(); RING_EVENTS + 4];
        let n = drain(&mut r, RING_EVENTS + 1, &mut out);
        assert_eq!(out[0].code, SYN_DROPPED);
        assert!(n > 1);
        r.push(ev(0xFF));
        let n2 = drain(&mut r, 4, &mut out);
        assert_eq!(n2, 1, "no second announcement for the same gap");
        assert_eq!(out[0], ev(0xFF));
    }

    #[test]
    fn a_loss_is_only_ever_recorded_by_a_full_ring() {
        // The invariant that makes `is_empty` safe as `len == 0`: overflow is the only
        // source of `lost`, and the overflowing push refills the ring, so a loss never
        // outlives the records it arrived with. Asserted on the *strong* form
        // (`len == RING_EVENTS`), not the weak `len > 0` — a first attempt used the weak
        // form and still passed against a `push` that dropped without refilling.
        let mut r = EventRing::new();
        for i in 0..RING_EVENTS as u16 * 3 {
            r.push(ev(i));
            assert!(
                r.lost == 0 || r.len() == RING_EVENTS,
                "after {i} pushes: lost={} with len={}",
                r.lost,
                r.len()
            );
        }
        assert!(r.lost > 0, "the run overflowed, so the invariant was actually exercised");
    }

    #[test]
    fn a_drained_ring_is_empty_and_owes_nothing() {
        // The consumer-visible half: once everything has been taken, including any
        // announcement, the ring reports empty and a reader parks rather than spinning.
        let mut r = EventRing::new();
        for i in 0..RING_EVENTS as u16 + 5 {
            r.push(ev(i));
        }
        assert!(!r.is_empty(), "records buffered");
        let mut out = [InputEvent::default(); RING_EVENTS + 4];
        let n = drain(&mut r, RING_EVENTS + 4, &mut out);
        assert_eq!(out[0].code, SYN_DROPPED, "the announcement leads");
        assert_eq!(n, RING_EVENTS + 1, "announcement plus every survivor");
        assert_eq!(r.len(), 0);
        assert!(r.is_empty(), "and nothing is owed afterwards");
    }

    #[test]
    fn the_lost_count_is_carried() {
        let mut r = EventRing::new();
        for i in 0..RING_EVENTS as u16 * 2 {
            r.push(ev(i));
        }
        let mut out = [InputEvent::default(); 4];
        drain(&mut r, 1, &mut out);
        assert_eq!(out[0].value, RING_EVENTS as i32, "one full ring's worth was discarded");
    }

    #[test]
    fn a_group_arrives_whole() {
        let mut r = EventRing::new();
        let group = [InputEvent::rel(REL_X, 1, 5), InputEvent::rel(REL_Y, 2, 5), InputEvent::syn(5)];
        r.push_group(&group);
        let mut out = [InputEvent::default(); 8];
        assert_eq!(drain(&mut r, 8, &mut out), 3);
        assert_eq!(&out[..3], &group, "the SYN terminator is never separated from its group");
    }
}
