//! `input-server` — the merge, and nothing that needs a kernel.
//!
//! The half of the input server with all the behaviour and none of the syscalls: taking the
//! devices' event streams and producing one ordered stream, tracking what a consumer missed, and
//! keeping the set of devices the device manager hands over ([`devices`]). `main.rs` is the part
//! that cannot be host-tested — subscribing, reading the device nodes, serving `/dev/input/new`,
//! and sending on channels.
//!
//! See `docs/spec/rsproto-input-ops.md` for the contract this implements and
//! `docs/architecture/input-subsystem.md` for why the server exists at all.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

use libkern::abi::{EV_REL, EV_SYN, InputEvent, REL_WHEEL, REL_X, REL_Y, SYN_DROPPED, SYN_REPORT};

/// Events buffered from one device per wakeup.
///
/// A PS/2 mouse reports at 100 Hz and a key repeat at ~30 Hz, so a wakeup that has fallen a
/// whole scheduler quantum behind still has far fewer than this waiting.
pub const PER_DEVICE: usize = 32;

/// Devices read at once: a keyboard and a mouse today, with room for Phase 6's.
pub const MAX_DEVICES: usize = 8;

/// Events one wakeup can harvest — every device's read — and so what [`merge`] must hold.
pub const MERGE_MAX: usize = PER_DEVICE * MAX_DEVICES;

/// Events in one `Events` message's batch.
///
/// **Two devices' worth**, so a keyboard and a mouse — today's machine — always fit one message.
/// A wakeup that harvested more is sent as consecutive batches, each ending on a group boundary
/// ([`batches`]): one message cannot hold every device's worth, since eight devices' reads are
/// more records than a message's payload.
pub const BATCH_MAX: usize = PER_DEVICE * 2 + 1;

// A group is at most one device's read, so every group fits in a batch.
const _: () = assert!(PER_DEVICE < BATCH_MAX);

/// The relative axes a deferred batch carries forward, in the order they are re-emitted.
///
/// **Relative axes are the state, so they cannot be resynchronised.** A key or a button has a
/// current value the consumer can be told to re-derive, which is what `SYN_DROPPED` asks for; a
/// `REL_X` of −7 *is* the movement, and a consumer that misses it has no way to learn what it
/// missed. Summing the deltas of a batch that could not be sent and re-emitting them later is
/// therefore lossless, not approximate: addition is what the consumer was going to do with them
/// anyway (2026-08-26, the cursor that could not reach the left edge).
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

/// Merge the devices' event streams into one run, ordered by `time_ns`. `sources` is each
/// device's harvest, by slot; an empty one takes no part, and past [`MAX_DEVICES`] are ignored.
///
/// **Groups move whole.** The merge advances a `SYN`-terminated group at a time rather than
/// a record at a time, comparing the *first* record of each source's next group. Sorting
/// records individually would be wrong twice over: records within a group share a timestamp,
/// so their relative order would depend on the sort's stability, and a group split across
/// the output is exactly what `rsproto-input-ops.md` promises never happens.
///
/// **Ordering is batch-scoped**, which is the guarantee the spec states and the strongest
/// one available: a global order would need every keystroke held until the slowest device
/// had spoken. Two events that happen close together are both buffered by the time the
/// server wakes, and those sort correctly — which is the shift-click case merging exists
/// for.
///
/// Returns the number of events written to `out`.
pub fn merge(sources: &[&[InputEvent]], out: &mut [InputEvent]) -> usize {
    let mut cursor = [0usize; MAX_DEVICES];
    let mut n = 0usize;
    loop {
        // The source whose next group starts earliest. **Ties go to the lower slot**: arbitrary,
        // but *deterministic*, which the display arm's determinism rule asks of anything a test
        // hashes or matches on. Coldplug arrives in registry order, so the keyboard holds the
        // lower slot and wins a tie, as it did when this merged exactly two.
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
        // **All of the group or none of it.** Copying record-by-record and stopping when
        // `out` fills delivers a partial group — the one thing the protocol promises never
        // to do — and the caller cannot tell, because a short return looks like "that is all
        // there was". Caught by `a_full_output_truncates_at_a_group_boundary_not_inside_one`.
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

/// Split `events` into consecutive runs of at most `max` records, **each ending on a group
/// boundary** — how a wakeup that harvested more than one message holds is sent, in order, as
/// several. Together the runs are `events`, whole.
///
/// A group longer than `max` goes alone rather than split. A device's read cannot produce one — a
/// group is at most one read, and a read at most [`PER_DEVICE`] — so this is the answer to a bug
/// upstream, and it keeps the promise that matters to a consumer: a group is never split.
pub fn batches(events: &[InputEvent], max: usize) -> Batches<'_> {
    Batches { events, at: 0, max }
}

/// The runs [`batches`] yields, as ranges of its `events`.
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
/// A group runs to and including its `SYN_REPORT`. A trailing run with no terminator — a
/// batch that ended mid-group, which the driver's ring is built never to produce — is
/// returned whole rather than dropped, so a bug upstream loses ordering rather than events.
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
/// A batch that cannot be delivered is **deferred, not discarded**, and the two halves of it
/// are owed differently:
///
/// - **Relative motion is carried forward.** Its deltas are summed into this consumer and
///   re-emitted as one group in front of the next batch that does go out. Nothing is lost, so
///   nothing about it is announced.
/// - **Everything else is announced.** Keys, buttons and an upstream `SYN_DROPPED`'s own count
///   go into `lost`, and the next batch is preceded by a `SYN_DROPPED` carrying it — the same
///   contract as the kernel's per-device ring, **including the unit** (whole records), which is
///   why a consumer never has to know which producer told it.
///
/// **The split is the correction of 2026-08-26.** This discarded whole batches, on the reasoning
/// that a consumer which falls behind "degrades to one that resynchronises". That is true of
/// state a consumer can re-derive and false of a relative axis, where the delta *is* the state:
/// the compositor's cursor ended up permanently offset from the host pointer, by exactly the
/// motion thrown away while it was busy repainting, and no `SYN_DROPPED` could tell it how far.
#[derive(Clone, Copy, Debug, Default)]
pub struct Consumer {
    /// **Records** discarded since the last successful send — not batches.
    ///
    /// The unit matters and was wrong here first. `SYN_DROPPED.value` means "how many whole
    /// records were discarded" wherever it comes from: the kernel's per-device ring counts
    /// records, the spec says records, and a consumer cannot tell which producer sent a given
    /// marker — so counting batches here would have made the same field mean two things and
    /// left a stalled consumer under-reporting by the batch size (PR #179 review, blocking 1).
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
    /// Relative motion is summed into `pending` and owed as movement; everything else is
    /// counted into `lost` and owed as an announcement. A `SYN_REPORT` is neither — it delimits
    /// a group, and the records it delimited are accounted for individually — and an upstream
    /// `SYN_DROPPED` contributes **its own count** rather than one record, so a gap does not
    /// shrink each time it is re-deferred.
    ///
    /// **Pass what was framed, not the batch it came from.** [`frame`](Self::frame) clears both
    /// debts as it writes them, so a send that then fails must hand the framed records back
    /// here; deferring the original batch instead would forget the marker and the motion that
    /// were prepended to it.
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

    /// Whether anything is owed — an announcement, deferred motion, or both.
    ///
    /// The server uses this to send to a consumer that has no new events: deferred motion is
    /// movement the user already made, and holding it until the next thing happens would leave
    /// the cursor short of where the mouse actually is until it is moved again.
    pub fn owes_send(&self) -> bool {
        self.lost > 0 || self.pending.iter().any(|&v| v != 0)
    }

    /// Build the records to send for `batch`, prepending `SYN_DROPPED` if one is owed.
    ///
    /// Returns the count written, or `None` if `out` cannot hold the whole thing — the
    /// caller must not send a partial batch, because a truncated group is exactly what the
    /// protocol promises never to deliver.
    ///
    /// **Clears both debts as it writes them**, so a caller whose send then fails must hand the
    /// framed records back to [`defer`](Self::defer) — not the batch they came from, which no
    /// longer carries the marker and the recovered motion prepended to it here.
    pub fn frame(&mut self, batch: &[InputEvent], now_ns: u64, out: &mut [InputEvent]) -> Option<usize> {
        let moved = self.pending.iter().filter(|&&v| v != 0).count();
        let extra = usize::from(self.lost > 0) + if moved > 0 { moved + 1 } else { 0 };
        if batch.len() + extra > out.len() {
            return None;
        }
        let mut n = 0;
        if self.lost > 0 {
            out[0] = InputEvent {
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: self.lost as i32,
                time_ns: now_ns,
            };
            n = 1;
            self.lost = 0;
        }
        // **The recovered motion goes after the marker and before the batch**, which is where it
        // happened. `libinput` resets what it has accumulated when it sees `SYN_DROPPED`, so
        // motion placed in front of the marker would be reset away; motion placed after the
        // batch would arrive out of order with movement that came later.
        //
        // Stamped with `now_ns` rather than with the time it was first seen: the batch it
        // belonged to is gone, and a timestamp older than records already delivered would break
        // the ordering the merge exists to provide. What it carries is the movement, not when it
        // happened.
        //
        // **The caller passes the *oldest* timestamp in the batch being framed**, not the
        // newest, so that these prepended records do not carry a time later than the records
        // they precede. `rsproto-input-ops.md` § Ordering invites a consumer to sort by
        // `time_ns`, and a sort that moved the recovered motion after the batch would undo the
        // placement this comment is about (PR #246 review, optional 8).
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

pub mod devices {
    //! **The devices this server reads are the device manager's to hand over** (administration
    //! Part B.3). The server subscribes to `/svc/devices/input`; the manager replays every
    //! keyboard and mouse as an `Arrived` carrying its node, then says `Settled`
    //! (`docs/spec/rsproto-devices-ops.md`). This is what the server decides with: what each
    //! message on that channel is, and which slot each device holds.
    //!
    //! **A slot is where a device lives in `main.rs`** — its node, its read buffer and its
    //! outstanding read — and its place in the merge, where the lower slot wins a tie.

    use super::MAX_DEVICES;
    use libkern::device::{DeviceKind, DeviceRecord};
    use librsproto::devices::{
        OP_DEVICES_ARRIVED, OP_DEVICES_DEPARTED, OP_DEVICES_SETTLED, parse_arrived, parse_departed,
        parse_settled,
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
        /// A device that is not input. The manager never sends one on this class; if it did,
        /// reading a disk as input events would be worse than refusing it, so its node is closed.
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
        /// Anything else: an op this category does not have, a body of the wrong size, or an
        /// arrival without exactly one node.
        Malformed,
    }

    /// Classify a message on the subscription: its `op`, its `body`, and how many handles came
    /// with it.
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
            OP_DEVICES_DEPARTED if handles == 0 => {
                parse_departed(body).map_or(Notice::Malformed, Notice::Departed)
            }
            _ => Notice::Malformed,
        }
    }

    /// What [`Table::arrive`] did with a device.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Arrival {
        /// It holds this slot now.
        Slot(usize),
        /// It already held one. A second node for the same device would be a second reader,
        /// which the kernel refuses, so the new node is closed.
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
        // The case merging exists for: a click and a keystroke that happened together must
        // arrive in the order they happened, not the order the reads completed.
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
        // The property the whole group-at-a-time walk exists for. A record-wise sort would
        // interleave these, because the mouse's motion sits between the keyboard's key and
        // its terminator in time.
        // Chosen so group-wise and record-wise walks disagree. The keyboard group starts
        // first (100 < 105) but *ends* last (110 > 106), so the mouse's records fall inside
        // the keyboard's span: sorting record-by-record would emit key@100, rel@105,
        // syn@106, syn@110 — both groups shredded. An earlier version of this test used data
        // where the two walks happened to agree, and a break that made `group_at` return
        // single records left it green.
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
        // Not arbitrary in the sense of "unspecified": a test that matches on output needs
        // the same answer every run.
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
        // The driver's ring never produces this. If it ever did, losing ordering beats
        // losing the events.
        let kbd = [key(30, 10)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        assert_eq!(merge(&[&kbd], &mut out), 1);
    }

    /// **More than two devices merge the same way**: by group start, whole groups, and a tie to
    /// the lower slot — here the third device's group ties the second's and follows it.
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

    /// **A slot with nothing harvested takes no part**, wherever it sits — a departed device's
    /// slot between two live ones is an empty source.
    #[test]
    fn an_empty_source_between_two_takes_no_part() {
        let kbd = [key(30, 10), syn(10)];
        let mouse = [rel(1, 5), syn(5)];
        let mut out = [InputEvent::default(); MERGE_MAX];
        let n = merge(&[&kbd, &[], &mouse], &mut out);
        assert_eq!(n, 4);
        assert_eq!((out[0].kind, out[2].kind), (EV_REL, EV_KEY));
    }

    /// **Every device's worth fits one merge**: `MERGE_MAX` is what `main.rs` sizes its buffer
    /// with, and a merge that ran out of room would drop the groups it did not reach. Held to the
    /// harvest, not to `MERGE_MAX` — a buffer sized too small would fill exactly and agree with
    /// itself.
    #[test]
    fn a_full_harvest_from_every_device_merges_whole() {
        let one: Vec<InputEvent> = (0..PER_DEVICE as u64 / 2).flat_map(|t| [rel(1, t), syn(t)]).collect();
        let sources: Vec<&[InputEvent]> = (0..MAX_DEVICES).map(|_| &one[..]).collect();
        let harvest: usize = sources.iter().map(|s| s.len()).sum();
        assert_eq!(harvest, PER_DEVICE * MAX_DEVICES, "every device read a full buffer");
        let mut out = [InputEvent::default(); MERGE_MAX];
        assert_eq!(merge(&sources, &mut out), harvest);
    }

    /// **A harvest is sent as batches that end on group boundaries**, in order, each at most a
    /// message's worth, and together the whole harvest.
    #[test]
    fn a_long_harvest_is_split_only_between_groups() {
        // Groups of three records: a batch of eight holds two, never two and two-thirds.
        let events: Vec<InputEvent> =
            (0..5u64).flat_map(|t| [rel(1, t), InputEvent { kind: EV_REL, code: REL_Y, value: 1, time_ns: t }, syn(t)]).collect();
        let runs: Vec<core::ops::Range<usize>> = batches(&events, 8).collect();
        assert_eq!(runs, vec![0..6, 6..12, 12..15]);
        for r in &runs {
            assert_eq!(events[r.end - 1].code, SYN_REPORT, "each ends a group");
        }
        assert_eq!(batches(&events, 15).collect::<Vec<_>>(), vec![0..15], "one batch when it fits");
        assert_eq!(batches(&[], 8).count(), 0, "nothing to send, no batch");
    }

    /// **A group longer than a batch goes alone rather than split** — which a device's read cannot
    /// produce, and which would otherwise be the one way to break the promise a consumer relies on.
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
            OP_DEVICES_ARRIVED, OP_DEVICES_DEPARTED, OP_DEVICES_SETTLED, build_arrived, build_departed,
            build_settled,
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

        /// **Each message is what it says, and nothing else is**: an arrival needs its node, a
        /// `Settled` or `Departed` carries none, and a body of another size is refused.
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

        /// **Arrivals in any order**: whichever comes first takes the lowest slot, and the set is
        /// the same.
        #[test]
        fn arrivals_in_any_order_each_take_the_lowest_free_slot() {
            let mut t = Table::new();
            assert_eq!(t.arrive(7), Arrival::Slot(0), "the mouse, first this time");
            assert_eq!(t.arrive(6), Arrival::Slot(1));
            assert_eq!((t.id(0), t.id(1), t.len()), (Some(7), Some(6), 2));
            assert_eq!(t.arrive(6), Arrival::Already, "a second node for one device is a second reader");
            assert_eq!(t.len(), 2);
        }

        /// **A keyboard alone is a table of one** — where the server used to exit for want of a
        /// mouse.
        #[test]
        fn a_keyboard_alone_is_served() {
            let mut t = Table::new();
            assert_eq!(t.arrive(6), Arrival::Slot(0));
            assert_eq!(t.len(), 1);
            assert!(Table::new().is_empty(), "and none is a table too");
        }

        /// **A departure mid-stream frees its slot for the next arrival**, and names nothing it
        /// does not hold.
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

        /// **Past the last slot a device is refused**, and the table is unchanged.
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
        // Two deferred batches of 20 and 6 unrecoverable records: the marker must say 26, not
        // 2. The unit is the kernel ring's — whole records — because a consumer cannot tell
        // which producer sent a `SYN_DROPPED` and must not have to.
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

        // Cleared: the next batch is clean.
        let n2 = c.frame(&batch, 100, &mut out).expect("fits");
        assert_eq!(n2, 2, "no second announcement for the same gap");
        assert!(!c.owes_announcement());
    }

    #[test]
    fn a_batch_that_does_not_fit_is_refused_rather_than_truncated() {
        // Sending half a batch would deliver a partial group, which is the one thing the
        // protocol promises never to do.
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
        // The whole point. A dropped motion batch used to become a `SYN_DROPPED` and nothing
        // else, and the consumer had no way to recover the pixels: the compositor's cursor
        // stayed offset from the host pointer by exactly this much, for the life of the
        // session.
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
        // The mixed case, and the reason the split is per record rather than per batch: the
        // key press is genuinely gone and must be announced, and the motion beside it is not.
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
        // `frame` clears as it writes, so a send that fails afterwards must return what was
        // framed. Deferring the *batch* instead would drop the marker and the recovered motion
        // that had just been prepended to it — the send path's one sharp edge.
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
        // A `SYN_DROPPED` from the kernel's ring counts what *it* lost. Counting it as one
        // record would shrink the gap every time the batch carrying it was re-deferred.
        let mut c = Consumer::new();
        c.defer(&[InputEvent { kind: EV_SYN, code: SYN_DROPPED, value: 7, time_ns: 1 }]);
        let mut out = [InputEvent::default(); FRAME_MAX];
        let n = c.frame(&[], 99, &mut out).expect("fits");
        assert_eq!(n, 1);
        assert_eq!(out[0].value, 7, "the gap it announced, not the one record carrying it");
    }

    #[test]
    fn a_consumer_owing_nothing_frames_an_empty_batch_as_nothing() {
        // The server sends only when this is non-zero: an empty message would wake every
        // consumer for no reason.
        let mut c = Consumer::new();
        let mut out = [InputEvent::default(); FRAME_MAX];
        assert_eq!(c.frame(&[], 99, &mut out), Some(0));
        assert!(!c.owes_send());
    }

    #[test]
    fn the_frame_buffer_is_big_enough_for_the_worst_case() {
        // `FRAME_MAX` is what `main.rs` sizes its buffer with, and a `frame` that does not fit
        // is deferred again — so an undersized buffer is not a truncation but a consumer that
        // never receives anything again.
        let mut c = Consumer::new();
        c.defer(&[
            key(30, 1),
            rel_code(REL_X, 1, 1),
            rel_code(REL_Y, 1, 1),
            rel_code(REL_WHEEL, 1, 1),
            syn(1),
        ]);
        let batch = [InputEvent::default(); BATCH_MAX];
        let mut out = [InputEvent::default(); FRAME_MAX];
        assert_eq!(c.frame(&batch, 99, &mut out), Some(FRAME_MAX));
    }
}
