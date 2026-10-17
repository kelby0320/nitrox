//! `input-server` — the merge, and nothing that needs a kernel.
//!
//! The behaviour without the syscalls: merging the devices' event streams into one ordered
//! stream, tracking what a consumer missed, and the set of devices the manager hands over
//! ([`devices`]). Contract: `docs/spec/rsproto-input-ops.md`; why the server exists:
//! `docs/architecture/input-subsystem.md`.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

use libkern::abi::{
    EV_REL, EV_SYN, InputEvent, LIGHT_CAPS, LIGHT_NUM, LIGHT_SCROLL, REL_WHEEL, REL_X, REL_Y, SYN_DROPPED, SYN_REPORT,
};

/// Events buffered from one device per wakeup; a 100 Hz mouse is far below it.
pub const PER_DEVICE: usize = 32;

/// Devices read at once.
pub const MAX_DEVICES: usize = 8;

/// Events one wakeup can harvest — every device's read — and so what [`merge`] must hold.
pub const MERGE_MAX: usize = PER_DEVICE * MAX_DEVICES;

/// Events in one `Events` message: two devices' worth, so a keyboard and a mouse fit one; a
/// larger harvest goes as consecutive batches ([`batches`]), as eight devices exceed a payload.
pub const BATCH_MAX: usize = PER_DEVICE * 2 + 1;

// A group is at most one device's read, so every group fits in a batch.
const _: () = assert!(PER_DEVICE < BATCH_MAX);

/// The relative axes a deferred batch carries forward, in re-emit order.
///
/// A `REL_X` of −7 *is* the movement, so unlike a key it cannot be re-derived after
/// `SYN_DROPPED`; summing unsent deltas and re-emitting them is lossless.
pub const DEFERRED_AXES: [u16; 3] = [REL_X, REL_Y, REL_WHEEL];

/// Records one recovered group needs: one per axis that moved, plus its `SYN_REPORT`.
pub const DEFERRED_MAX: usize = DEFERRED_AXES.len() + 1;

/// Records [`Consumer::frame`] can write: the batch, an announcement, and a recovered group.
pub const FRAME_MAX: usize = BATCH_MAX + 1 + DEFERRED_MAX;

/// Which entry of [`DEFERRED_AXES`] this record accumulates into, if any.
fn axis_of(e: &InputEvent) -> Option<usize> {
    if e.kind != EV_REL {
        return None;
    }
    DEFERRED_AXES.iter().position(|&code| code == e.code)
}

/// Merge the devices' event streams into one run, ordered by `time_ns`; returns the count
/// written to `out`. `sources` is each device's harvest, by slot; an empty one takes no part.
///
/// Groups move whole, compared by their first record: sorting records could split a group,
/// which the spec promises never happens. Ordering is batch-scoped, since a global order would
/// hold every keystroke until the slowest device had spoken.
pub fn merge(sources: &[&[InputEvent]], out: &mut [InputEvent]) -> usize {
    let mut cursor = [0usize; MAX_DEVICES];
    let mut n = 0usize;
    loop {
        // The source whose next group starts earliest. Ties go to the lower slot: arbitrary but
        // deterministic, and coldplug order gives the keyboard the lower slot.
        let mut best: Option<(usize, (usize, usize))> = None;
        for (s, src) in sources.iter().enumerate().take(MAX_DEVICES) {
            let Some(span) = group_at(src, cursor[s]) else { continue };
            let earlier = match best {
                None => true,
                Some((b, bspan)) => src[span.0].time_ns < sources[b][bspan.0].time_ns,
            };
            if earlier {
                best = Some((s, span));
            }
        }
        let Some((s, span)) = best else { break };
        let src = sources[s];
        // All of the group or none of it: a partial group is undetectable to the caller, since
        // a short return looks like "that is all there was".
        let len = span.1 - span.0;
        if n + len > out.len() {
            return n;
        }
        out[n..n + len].copy_from_slice(&src[span.0..span.1]);
        n += len;
        cursor[s] = span.1;
    }
    n
}

/// Split `events` into consecutive runs of at most `max` records, each ending on a group
/// boundary. A group longer than `max` goes alone rather than split.
pub fn batches(events: &[InputEvent], max: usize) -> Batches<'_> {
    Batches { events, at: 0, max }
}

/// The runs [`batches`] yields.
pub struct Batches<'a> {
    events: &'a [InputEvent],
    at: usize,
    max: usize,
}

impl Iterator for Batches<'_> {
    type Item = core::ops::Range<usize>;

    fn next(&mut self) -> Option<core::ops::Range<usize>> {
        let start = self.at;
        let mut end = start;
        while let Some((_, next)) = group_at(self.events, end) {
            if next - start > self.max && end > start {
                break;
            }
            end = next;
        }
        if end == start {
            return None;
        }
        self.at = end;
        Some(start..end)
    }
}

/// The half-open range of the group starting at `from`, or `None` at the end.
///
/// A group runs through its `SYN_REPORT`; an unterminated trailing run is returned whole, so
/// an upstream bug loses ordering, not events.
fn group_at(events: &[InputEvent], from: usize) -> Option<(usize, usize)> {
    if from >= events.len() {
        return None;
    }
    let mut end = from;
    while end < events.len() {
        let e = events[end];
        end += 1;
        if e.kind == EV_SYN && e.code == SYN_REPORT {
            break;
        }
    }
    Some((from, end))
}

/// What a consumer is owed: motion it has not been given, and a loss to announce first.
///
/// A batch that cannot be delivered is deferred, and its halves are owed differently:
///
/// - **Relative motion is carried forward**, re-emitted as one group in front of the next
///   batch that goes out. Nothing is lost, so nothing is announced.
/// - **Everything else is announced**: the next batch is preceded by a `SYN_DROPPED` counting
///   whole records, like the kernel's per-device ring.
#[derive(Clone, Copy, Debug, Default)]
pub struct Consumer {
    /// Records (not batches) discarded since the last successful send.
    lost: u32,
    /// Relative movement summed from batches that could not be sent, by [`DEFERRED_AXES`] index.
    pending: [i32; DEFERRED_AXES.len()],
}

impl Consumer {
    /// A consumer that is up to date.
    pub const fn new() -> Self {
        Self { lost: 0, pending: [0; DEFERRED_AXES.len()] }
    }

    /// Take back records that could not be delivered.
    ///
    /// An upstream `SYN_DROPPED` contributes its own count, so a gap does not shrink each time
    /// it is re-deferred. Pass what was framed, not its batch: [`frame`](Self::frame) clears
    /// both debts as it writes them.
    pub fn defer(&mut self, records: &[InputEvent]) {
        let mut announce = 0u32;
        for e in records {
            match axis_of(e) {
                Some(i) => self.pending[i] = self.pending[i].saturating_add(e.value),
                None if e.kind == EV_SYN && e.code == SYN_REPORT => {}
                None if e.kind == EV_SYN && e.code == SYN_DROPPED => {
                    announce = announce.saturating_add(e.value.max(0) as u32);
                }
                None => announce = announce.saturating_add(1),
            }
        }
        self.lost = self.lost.saturating_add(announce);
    }

    /// Whether a loss is waiting to be announced.
    pub fn owes_announcement(&self) -> bool {
        self.lost > 0
    }

    /// Whether anything is owed. Deferred motion is sent without new events, or the cursor
    /// would stay short until the mouse moved again.
    pub fn owes_send(&self) -> bool {
        self.lost > 0 || self.pending.iter().any(|&v| v != 0)
    }

    /// Build the records to send for `batch`, prepending `SYN_DROPPED` if one is owed.
    ///
    /// Returns the count written, or `None` if `out` cannot hold all of it (never a partial
    /// group). Clears both debts as it writes them: a failed send hands the framed records to
    /// [`defer`](Self::defer).
    pub fn frame(&mut self, batch: &[InputEvent], now_ns: u64, out: &mut [InputEvent]) -> Option<usize> {
        let moved = self.pending.iter().filter(|&&v| v != 0).count();
        let extra = usize::from(self.lost > 0) + if moved > 0 { moved + 1 } else { 0 };
        if batch.len() + extra > out.len() {
            return None;
        }
        let mut n = 0;
        if self.lost > 0 {
            out[0] = InputEvent { kind: EV_SYN, code: SYN_DROPPED, value: self.lost as i32, time_ns: now_ns };
            n = 1;
            self.lost = 0;
        }
        // Recovered motion goes after the marker (`libinput` resets at `SYN_DROPPED`) and before
        // the batch (after it, it would be out of order). Stamped `now_ns`, the batch's oldest
        // time, so a consumer sorting by `time_ns` keeps that placement.
        if moved > 0 {
            for (i, &code) in DEFERRED_AXES.iter().enumerate() {
                if self.pending[i] != 0 {
                    out[n] = InputEvent { kind: EV_REL, code, value: self.pending[i], time_ns: now_ns };
                    n += 1;
                }
            }
            out[n] = InputEvent { kind: EV_SYN, code: SYN_REPORT, value: 0, time_ns: now_ns };
            n += 1;
            self.pending = [0; DEFERRED_AXES.len()];
        }
        out[n..n + batch.len()].copy_from_slice(batch);
        Some(n + batch.len())
    }
}

/// The lights an `Input::Lights` asks for: its one byte, or `None` for another length or a bit
/// that is not a light.
pub fn lights_request(body: &[u8]) -> Option<u8> {
    match *body {
        [b] if b as u16 & !(LIGHT_NUM | LIGHT_CAPS | LIGHT_SCROLL) == 0 => Some(b),
        _ => None,
    }
}

pub mod leaving {
    //! A device leaving: its node answers a read `PeerClosed` once its releases have been read,
    //! and the manager sends `Departed`, in no fixed order. A slot is retired only when both
    //! have come, or a mouse unplugged mid-drag would lose its release still in the ring. A
    //! node that answered `PeerClosed` is not read again, so it cannot spin.

    /// Where a device is in leaving.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub enum Leaving {
        /// Present, as far as this server knows.
        #[default]
        Present,
        /// The manager said `Departed`; its node has not yet answered `PeerClosed`, so it is read.
        Departing,
        /// Its node answered `PeerClosed`; the manager's `Departed` has not come.
        Left,
    }

    /// What to do with the slot.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Act {
        /// Keep it.
        Keep,
        /// Retire it: both have come.
        Retire,
    }

    impl Leaving {
        /// Whether its node is still read: until it has answered `PeerClosed`.
        pub fn reading(self) -> bool {
            self != Leaving::Left
        }

        /// The manager's `Departed` came.
        pub fn departed(&mut self) -> Act {
            match *self {
                Leaving::Present | Leaving::Departing => {
                    *self = Leaving::Departing;
                    Act::Keep
                }
                Leaving::Left => Act::Retire,
            }
        }

        /// Its node answered `PeerClosed`: whether that is news (the one line to log), and what to do.
        pub fn peer_closed(&mut self) -> (bool, Act) {
            match *self {
                Leaving::Present => {
                    *self = Leaving::Left;
                    (true, Act::Keep)
                }
                Leaving::Departing => {
                    *self = Leaving::Left;
                    (true, Act::Retire)
                }
                Leaving::Left => (false, Act::Keep),
            }
        }
    }
}

pub mod devices {
    //! The devices the manager hands over (`docs/spec/rsproto-devices-ops.md`): what each
    //! message is, and which slot each device holds. A slot is where a device lives in `main.rs`
    //! and its place in the merge, where the lower slot wins a tie.

    use super::MAX_DEVICES;
    use libkern::device::{DeviceKind, DeviceRecord};
    use librsproto::devices::{
        OP_DEVICES_ARRIVED, OP_DEVICES_DEPARTED, OP_DEVICES_SETTLED, parse_arrived, parse_departed, parse_settled,
    };

    /// A message on the subscription, classified.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Notice {
        /// A keyboard or a mouse, whose node came with it.
        Arrived {
            /// Its registry id — what a `Departed` will name.
            id: u32,
            /// Keyboard or mouse.
            kind: DeviceKind,
        },
        /// A device that is not input; its node is closed rather than read as events.
        NotInput {
            /// Its registry id.
            id: u32,
            /// What it is instead.
            kind: DeviceKind,
        },
        /// The replay is over: this many arrived.
        Settled(u32),
        /// The device with this registry id has gone.
        Departed(u32),
        /// An unknown op, a body of the wrong size, or an arrival without exactly one node.
        Malformed,
    }

    /// Classify a message by its `op`, `body` and handle count.
    pub fn notice(op: u16, body: &[u8], handles: usize) -> Notice {
        match op {
            OP_DEVICES_ARRIVED if handles == 1 => match parse_arrived(body).and_then(DeviceRecord::read) {
                Some(r) if matches!(r.kind(), DeviceKind::Keyboard | DeviceKind::Mouse) => {
                    Notice::Arrived { id: r.id, kind: r.kind() }
                }
                Some(r) => Notice::NotInput { id: r.id, kind: r.kind() },
                None => Notice::Malformed,
            },
            OP_DEVICES_SETTLED if handles == 0 => parse_settled(body).map_or(Notice::Malformed, Notice::Settled),
            OP_DEVICES_DEPARTED if handles == 0 => parse_departed(body).map_or(Notice::Malformed, Notice::Departed),
            _ => Notice::Malformed,
        }
    }

    /// What [`Table::arrive`] did with a device.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Arrival {
        /// It holds this slot now.
        Slot(usize),
        /// It already held one; a second node would be a second reader, so it is closed.
        Already,
        /// Every slot is taken.
        Full,
    }

    /// Which registry id holds each slot.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Table {
        ids: [Option<u32>; MAX_DEVICES],
    }

    impl Table {
        /// No devices.
        pub const fn new() -> Table {
            Table { ids: [None; MAX_DEVICES] }
        }

        /// Give device `id` the lowest free slot.
        pub fn arrive(&mut self, id: u32) -> Arrival {
            if self.ids.contains(&Some(id)) {
                return Arrival::Already;
            }
            match self.ids.iter().position(Option::is_none) {
                Some(slot) => {
                    self.ids[slot] = Some(id);
                    Arrival::Slot(slot)
                }
                None => Arrival::Full,
            }
        }

        /// Free device `id`'s slot, and say which it was. `None` for a device it does not hold.
        pub fn depart(&mut self, id: u32) -> Option<usize> {
            let slot = self.ids.iter().position(|&s| s == Some(id))?;
            self.ids[slot] = None;
            Some(slot)
        }

        /// The slot device `id` holds, if any — left held, unlike [`depart`](Self::depart).
        pub fn slot(&self, id: u32) -> Option<usize> {
            (0..MAX_DEVICES).find(|&s| self.id(s) == Some(id))
        }

        /// The device in `slot`, if any.
        pub fn id(&self, slot: usize) -> Option<u32> {
            self.ids.get(slot).copied().flatten()
        }

        /// How many devices it holds.
        pub fn len(&self) -> usize {
            self.ids.iter().filter(|s| s.is_some()).count()
        }

        /// Whether it holds none.
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::abi::{BTN_LEFT, EV_KEY, EV_REL, KEY_PRESS, REL_X};

    fn key(code: u16, t: u64) -> InputEvent {
        InputEvent { kind: EV_KEY, code, value: KEY_PRESS, time_ns: t }
    }
    fn rel(v: i32, t: u64) -> InputEvent {
        InputEvent { kind: EV_REL, code: REL_X, value: v, time_ns: t }
    }
    fn syn(t: u64) -> InputEvent {
        InputEvent { kind: EV_SYN, code: SYN_REPORT, value: 0, time_ns: t }
    }

    #[test]
    fn an_empty_merge_produces_nothing() {
        let mut out = [InputEvent::default(); BATCH_MAX];
        assert_eq!(merge(&[], &mut out), 0);
    }

    #[test]
    fn one_device_passes_through_unchanged() {
        let kbd = [key(30, 10), syn(10)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        let n = merge(&[&kbd], &mut out);
        assert_eq!(&out[..n], &kbd);
    }

    #[test]
    fn the_older_group_comes_first_whichever_device_it_is_on() {
        let kbd = [key(30, 100), syn(100)];
        let mouse = [InputEvent { kind: EV_KEY, code: BTN_LEFT, value: 1, time_ns: 95 }, syn(95)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        let n = merge(&[&kbd, &mouse], &mut out);
        assert_eq!(n, 4);
        assert_eq!(out[0].code, BTN_LEFT, "the click was earlier, so it leads");
        assert_eq!(out[2].code, 30);
    }

    #[test]
    fn a_group_is_never_split_by_the_merge() {
        // The keyboard group starts first (100 < 105) but ends last (110 > 106), so a
        // record-wise sort would interleave the two groups.
        let kbd = [key(30, 100), syn(110)];
        let mouse = [rel(5, 105), syn(106)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        let n = merge(&[&kbd, &mouse], &mut out);
        assert_eq!(n, 4);
        assert_eq!(out[0].kind, EV_KEY, "the keyboard group starts first");
        assert_eq!(out[1].kind, EV_SYN, "and its terminator follows immediately");
        assert_eq!(out[1].time_ns, 110, "the keyboard's own SYN, not the mouse's");
        assert_eq!(out[2].kind, EV_REL);
        assert_eq!(out[3].kind, EV_SYN);
    }

    #[test]
    fn several_groups_interleave_by_group_start() {
        let kbd = [key(30, 10), syn(10), key(31, 30), syn(30)];
        let mouse = [rel(1, 20), syn(20), rel(2, 40), syn(40)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        let n = merge(&[&kbd, &mouse], &mut out);
        let times: Vec<u64> = out[..n].iter().map(|e| e.time_ns).collect();
        assert_eq!(times, vec![10, 10, 20, 20, 30, 30, 40, 40]);
    }

    #[test]
    fn a_tie_is_broken_deterministically() {
        let kbd = [key(30, 50), syn(50)];
        let mouse = [rel(1, 50), syn(50)];
        let mut a = [InputEvent::default(); BATCH_MAX];
        let mut b = [InputEvent::default(); BATCH_MAX];
        let n1 = merge(&[&kbd, &mouse], &mut a);
        let n2 = merge(&[&kbd, &mouse], &mut b);
        assert_eq!(&a[..n1], &b[..n2]);
        assert_eq!(a[0].kind, EV_KEY, "ties go to the keyboard");
    }

    #[test]
    fn a_full_output_truncates_at_a_group_boundary_not_inside_one() {
        let kbd = [key(30, 10), syn(10), key(31, 20), syn(20)];
        let mut out = [InputEvent::default(); 3];
        let n = merge(&[&kbd], &mut out);
        assert_eq!(n, 2, "the first group fits; the second does not, so it is not started");
    }

    #[test]
    fn a_trailing_group_with_no_terminator_is_still_delivered() {
        let kbd = [key(30, 10)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        assert_eq!(merge(&[&kbd], &mut out), 1);
    }

    #[test]
    fn three_devices_merge_by_group_start_with_ties_to_the_lower_slot() {
        let a = [key(30, 30), syn(30)];
        let b = [rel(1, 10), syn(10), rel(2, 20), syn(20)];
        let c = [rel(9, 20), syn(20)];
        let mut out = [InputEvent::default(); MERGE_MAX];
        let n = merge(&[&a, &b, &c], &mut out);
        let order: Vec<(u64, i32)> = out[..n].iter().step_by(2).map(|e| (e.time_ns, e.value)).collect();
        assert_eq!(order, vec![(10, 1), (20, 2), (20, 9), (30, KEY_PRESS)]);
    }

    #[test]
    fn an_empty_source_between_two_takes_no_part() {
        let kbd = [key(30, 10), syn(10)];
        let mouse = [rel(1, 5), syn(5)];
        let mut out = [InputEvent::default(); MERGE_MAX];
        let n = merge(&[&kbd, &[], &mouse], &mut out);
        assert_eq!(n, 4);
        assert_eq!((out[0].kind, out[2].kind), (EV_REL, EV_KEY));
    }

    // Held to the harvest, not to `MERGE_MAX`: a too-small buffer would fill exactly and agree
    // with itself.
    #[test]
    fn a_full_harvest_from_every_device_merges_whole() {
        let one: Vec<InputEvent> = (0..PER_DEVICE as u64 / 2).flat_map(|t| [rel(1, t), syn(t)]).collect();
        let sources: Vec<&[InputEvent]> = (0..MAX_DEVICES).map(|_| &one[..]).collect();
        let harvest: usize = sources.iter().map(|s| s.len()).sum();
        assert_eq!(harvest, PER_DEVICE * MAX_DEVICES, "every device read a full buffer");
        let mut out = [InputEvent::default(); MERGE_MAX];
        assert_eq!(merge(&sources, &mut out), harvest);
    }

    #[test]
    fn a_long_harvest_is_split_only_between_groups() {
        // Groups of three records: a batch of eight holds two, never two and two-thirds.
        let events: Vec<InputEvent> = (0..5u64)
            .flat_map(|t| [rel(1, t), InputEvent { kind: EV_REL, code: REL_Y, value: 1, time_ns: t }, syn(t)])
            .collect();
        let runs: Vec<core::ops::Range<usize>> = batches(&events, 8).collect();
        assert_eq!(runs, vec![0..6, 6..12, 12..15]);
        for r in &runs {
            assert_eq!(events[r.end - 1].code, SYN_REPORT, "each ends a group");
        }
        assert_eq!(batches(&events, 15).collect::<Vec<_>>(), vec![0..15], "one batch when it fits");
        assert_eq!(batches(&[], 8).count(), 0, "nothing to send, no batch");
    }

    #[test]
    fn a_group_longer_than_a_batch_goes_whole_and_alone() {
        let events = [rel(1, 1), rel(2, 1), rel(3, 1), syn(1), key(30, 2), syn(2)];
        let runs: Vec<core::ops::Range<usize>> = batches(&events, 2).collect();
        assert_eq!(runs, vec![0..4, 4..6]);
    }

    mod devices {
        use crate::MAX_DEVICES;
        use crate::devices::{Arrival, Notice, Table, notice};
        use libkern::device::{DeviceKind, DeviceRecord};
        use librsproto::devices::{
            OP_DEVICES_ARRIVED, OP_DEVICES_DEPARTED, OP_DEVICES_SETTLED, build_arrived, build_departed, build_settled,
        };

        fn arrived(id: u32, kind: DeviceKind) -> Vec<u8> {
            let mut raw = [0u8; 144];
            raw[0..4].copy_from_slice(&id.to_le_bytes());
            raw[8..12].copy_from_slice(&kind.as_u32().to_le_bytes());
            let r = DeviceRecord::read(&raw).unwrap();
            let mut out = [0u8; 144];
            let n = build_arrived(&mut out, r.as_bytes()).unwrap();
            out[..n].to_vec()
        }

        #[test]
        fn a_notice_is_classified_by_op_body_and_handles() {
            let kbd = arrived(6, DeviceKind::Keyboard);
            assert_eq!(notice(OP_DEVICES_ARRIVED, &kbd, 1), Notice::Arrived { id: 6, kind: DeviceKind::Keyboard });
            assert_eq!(notice(OP_DEVICES_ARRIVED, &kbd, 0), Notice::Malformed, "no node");
            assert_eq!(notice(OP_DEVICES_ARRIVED, &kbd[..143], 1), Notice::Malformed, "a record short");
            let disk = arrived(2, DeviceKind::Disk);
            assert_eq!(notice(OP_DEVICES_ARRIVED, &disk, 1), Notice::NotInput { id: 2, kind: DeviceKind::Disk });
            let mut w = [0u8; 4];
            let n = build_settled(&mut w, 2).unwrap();
            assert_eq!(notice(OP_DEVICES_SETTLED, &w[..n], 0), Notice::Settled(2));
            assert_eq!(notice(OP_DEVICES_SETTLED, &w[..n], 1), Notice::Malformed, "a handle it does not carry");
            let n = build_departed(&mut w, 7).unwrap();
            assert_eq!(notice(OP_DEVICES_DEPARTED, &w[..n], 0), Notice::Departed(7));
            assert_eq!(notice(OP_DEVICES_DEPARTED, &w[..3], 0), Notice::Malformed);
            assert_eq!(notice(0x0F7F, &w[..n], 0), Notice::Malformed, "an op the category lacks");
        }

        #[test]
        fn arrivals_in_any_order_each_take_the_lowest_free_slot() {
            let mut t = Table::new();
            assert_eq!(t.arrive(7), Arrival::Slot(0), "the mouse, first this time");
            assert_eq!(t.arrive(6), Arrival::Slot(1));
            assert_eq!((t.id(0), t.id(1), t.len()), (Some(7), Some(6), 2));
            assert_eq!(t.arrive(6), Arrival::Already, "a second node for one device is a second reader");
            assert_eq!(t.len(), 2);
        }

        #[test]
        fn a_keyboard_alone_is_served() {
            let mut t = Table::new();
            assert_eq!(t.arrive(6), Arrival::Slot(0));
            assert_eq!(t.len(), 1);
            assert!(Table::new().is_empty(), "and none is a table too");
        }

        #[test]
        fn a_departure_frees_its_slot_and_only_its_own() {
            let mut t = Table::new();
            t.arrive(6);
            t.arrive(7);
            t.arrive(9);
            assert_eq!(t.depart(7), Some(1));
            assert_eq!(t.depart(7), None, "already gone");
            assert_eq!(t.depart(42), None, "never held");
            assert_eq!((t.id(0), t.id(1), t.id(2), t.len()), (Some(6), None, Some(9), 2));
            assert_eq!(t.arrive(11), Arrival::Slot(1), "the gap is reused");
        }

        #[test]
        fn a_device_past_the_last_slot_is_refused() {
            let mut t = Table::new();
            for id in 0..MAX_DEVICES as u32 {
                assert_eq!(t.arrive(id), Arrival::Slot(id as usize));
            }
            assert_eq!(t.arrive(99), Arrival::Full);
            assert_eq!(t.len(), MAX_DEVICES);
        }
    }

    #[test]
    fn a_consumer_up_to_date_sends_exactly_the_batch() {
        let mut c = Consumer::new();
        let batch = [key(30, 10), syn(10)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        let n = c.frame(&batch, 99, &mut out).expect("fits");
        assert_eq!(n, 2);
        assert_eq!(&out[..n], &batch);
        assert!(!c.owes_announcement());
    }

    #[test]
    fn a_loss_is_announced_before_the_next_batch_and_only_once() {
        let mut c = Consumer::new();
        // Batches of 20 and 6 records: the marker says 26 (records), not 2 (batches).
        let first: Vec<InputEvent> = (0..20).map(|i| key(30, i)).collect();
        let second: Vec<InputEvent> = (0..6).map(|i| key(31, i)).collect();
        c.defer(&first);
        c.defer(&second);
        assert!(c.owes_announcement());

        let batch = [key(30, 10), syn(10)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        let n = c.frame(&batch, 99, &mut out).expect("fits");
        assert_eq!(n, 3);
        assert_eq!(out[0].kind, EV_SYN);
        assert_eq!(out[0].code, SYN_DROPPED, "the announcement leads, before any survivor");
        assert_eq!(out[0].value, 26, "records lost, not batches");
        assert_eq!(&out[1..n], &batch);

        let n2 = c.frame(&batch, 100, &mut out).expect("fits");
        assert_eq!(n2, 2, "no second announcement for the same gap");
        assert!(!c.owes_announcement());
    }

    #[test]
    fn a_batch_that_does_not_fit_is_refused_rather_than_truncated() {
        let mut c = Consumer::new();
        c.defer(&[key(30, 1), key(31, 1)]);
        let batch = [key(30, 10), syn(10)];
        let mut out = [InputEvent::default(); 2]; // room for the batch but not the marker
        assert_eq!(c.frame(&batch, 99, &mut out), None);
        assert!(c.owes_announcement(), "and the loss is still owed afterwards");
    }

    fn rel_code(code: u16, v: i32, t: u64) -> InputEvent {
        InputEvent { kind: EV_REL, code, value: v, time_ns: t }
    }

    #[test]
    fn motion_that_could_not_be_sent_is_carried_forward_rather_than_announced() {
        let mut c = Consumer::new();
        c.defer(&[rel_code(REL_X, -6, 10), rel_code(REL_Y, -3, 10), syn(10)]);
        assert!(c.owes_send(), "movement is owed");
        assert!(!c.owes_announcement(), "but nothing was lost, so nothing is announced");

        let batch = [rel_code(REL_X, -1, 20), syn(20)];
        let mut out = [InputEvent::default(); FRAME_MAX];
        let n = c.frame(&batch, 99, &mut out).expect("fits");
        assert_eq!(n, 5, "two axes, their SYN, and the batch");
        assert_eq!((out[0].kind, out[0].code, out[0].value), (EV_REL, REL_X, -6));
        assert_eq!((out[1].kind, out[1].code, out[1].value), (EV_REL, REL_Y, -3));
        assert_eq!((out[2].kind, out[2].code), (EV_SYN, SYN_REPORT), "a whole group");
        assert_eq!(&out[3..n], &batch, "and then what actually arrived");
        assert!(!c.owes_send(), "the debt is cleared by framing it");
    }

    #[test]
    fn deferred_motion_sums_across_batches_and_axes() {
        let mut c = Consumer::new();
        c.defer(&[rel_code(REL_X, -6, 10), rel_code(REL_Y, -3, 10), syn(10)]);
        c.defer(&[rel_code(REL_X, -4, 20), rel_code(REL_WHEEL, 1, 20), syn(20)]);
        let mut out = [InputEvent::default(); FRAME_MAX];
        let n = c.frame(&[], 99, &mut out).expect("fits");
        assert_eq!(n, 4, "three axes moved, plus the SYN");
        assert_eq!(out[0].value, -10, "the two X deltas add");
        assert_eq!((out[1].code, out[1].value), (REL_Y, -3));
        assert_eq!((out[2].code, out[2].value), (REL_WHEEL, 1), "the wheel is relative too");
    }

    #[test]
    fn a_key_in_a_deferred_batch_is_announced_while_its_motion_survives() {
        // The split is per record: the key is gone and announced, the motion beside it is not.
        let mut c = Consumer::new();
        c.defer(&[key(30, 10), rel_code(REL_X, -5, 10), syn(10)]);
        let mut out = [InputEvent::default(); FRAME_MAX];
        let n = c.frame(&[], 99, &mut out).expect("fits");
        assert_eq!(n, 3);
        assert_eq!(out[0].code, SYN_DROPPED, "the marker leads");
        assert_eq!(out[0].value, 1, "one record lost — the key, not the motion or its SYN");
        assert_eq!((out[1].code, out[1].value), (REL_X, -5), "after the marker, never before");
        assert_eq!(out[2].code, SYN_REPORT);
    }

    #[test]
    fn framed_records_handed_back_keep_both_debts() {
        let mut c = Consumer::new();
        c.defer(&[key(30, 5), rel_code(REL_X, -7, 5), syn(5)]);
        let batch = [rel_code(REL_Y, -2, 10), syn(10)];
        let mut out = [InputEvent::default(); FRAME_MAX];
        let n = c.frame(&batch, 99, &mut out).expect("fits");

        c.defer(&out[..n]); // the send failed
        let n2 = c.frame(&[], 100, &mut out).expect("fits");
        assert_eq!(out[0].code, SYN_DROPPED);
        assert_eq!(out[0].value, 1, "still one lost record, not zero and not two");
        let x = out[1..n2].iter().find(|e| e.code == REL_X && e.kind == EV_REL).expect("X");
        let y = out[1..n2].iter().find(|e| e.code == REL_Y && e.kind == EV_REL).expect("Y");
        assert_eq!((x.value, y.value), (-7, -2), "both the recovered and the batch's motion");
    }

    #[test]
    fn an_upstream_gap_keeps_its_own_count_when_deferred() {
        let mut c = Consumer::new();
        c.defer(&[InputEvent { kind: EV_SYN, code: SYN_DROPPED, value: 7, time_ns: 1 }]);
        let mut out = [InputEvent::default(); FRAME_MAX];
        let n = c.frame(&[], 99, &mut out).expect("fits");
        assert_eq!(n, 1);
        assert_eq!(out[0].value, 7, "the gap it announced, not the one record carrying it");
    }

    #[test]
    fn a_consumer_owing_nothing_frames_an_empty_batch_as_nothing() {
        let mut c = Consumer::new();
        let mut out = [InputEvent::default(); FRAME_MAX];
        assert_eq!(c.frame(&[], 99, &mut out), Some(0));
        assert!(!c.owes_send());
    }

    #[test]
    fn the_frame_buffer_is_big_enough_for_the_worst_case() {
        // An undersized `FRAME_MAX` would defer forever, not truncate.
        let mut c = Consumer::new();
        c.defer(&[key(30, 1), rel_code(REL_X, 1, 1), rel_code(REL_Y, 1, 1), rel_code(REL_WHEEL, 1, 1), syn(1)]);
        let batch = [InputEvent::default(); BATCH_MAX];
        let mut out = [InputEvent::default(); FRAME_MAX];
        assert_eq!(c.frame(&batch, 99, &mut out), Some(FRAME_MAX));
    }

    #[test]
    fn a_slot_retires_when_its_node_and_the_manager_have_both_said_so() {
        use super::leaving::{Act, Leaving};
        let mut d = Leaving::default();
        assert_eq!(d.departed(), Act::Keep, "Departed first: the node still has its releases");
        assert!(d.reading());
        assert_eq!(d.peer_closed(), (true, Act::Retire));

        let mut p = Leaving::default();
        assert_eq!(p.peer_closed(), (true, Act::Keep), "PeerClosed first");
        assert!(!p.reading(), "and not read again");
        assert_eq!(p.peer_closed(), (false, Act::Keep), "news once");
        assert_eq!(p.departed(), Act::Retire);
    }

    #[test]
    fn a_lights_request_is_one_byte_of_lights() {
        assert_eq!(lights_request(&[0]), Some(0));
        assert_eq!(lights_request(&[LIGHT_NUM as u8]), Some(0x01));
        assert_eq!(lights_request(&[LIGHT_CAPS as u8]), Some(0x02));
        assert_eq!(lights_request(&[0x07]), Some(0x07));
        assert_eq!(lights_request(&[0x08]), None, "a bit that is not a light");
        assert_eq!(lights_request(&[0x80]), None);
        assert_eq!(lights_request(&[]), None);
        assert_eq!(lights_request(&[0x01, 0x00]), None);
    }
}
