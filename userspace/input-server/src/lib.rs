//! `input-server` — the merge, and nothing that needs a kernel.
//!
//! The half of the input server with all the behaviour and none of the syscalls: taking two
//! devices' event streams and producing one ordered stream, and tracking what a consumer
//! missed. `main.rs` is the part that cannot be host-tested — reading the raw nodes, serving
//! `/dev/input/new`, and sending on channels.
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

/// Events in one merged batch — both devices' worth.
pub const BATCH_MAX: usize = PER_DEVICE * 2 + 1;

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

/// Merge two devices' event streams into one batch, ordered by `time_ns`.
///
/// **Groups move whole.** The merge advances a `SYN`-terminated group at a time rather than
/// a record at a time, comparing the *first* record of each side's next group. Sorting
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
pub fn merge(kbd: &[InputEvent], mouse: &[InputEvent], out: &mut [InputEvent]) -> usize {
    let (mut i, mut j, mut n) = (0usize, 0usize, 0usize);
    loop {
        let ka = group_at(kbd, i);
        let ma = group_at(mouse, j);
        let take_kbd = match (ka, ma) {
            (None, None) => break,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            // Ties go to the keyboard: arbitrary, but *deterministic*, which the display
            // arm's determinism rule asks of anything a test hashes or matches on.
            (Some(k), Some(m)) => kbd[k.0].time_ns <= mouse[m.0].time_ns,
        };
        let (src, span, cursor) = if take_kbd {
            (kbd, ka.expect("checked"), &mut i)
        } else {
            (mouse, ma.expect("checked"), &mut j)
        };
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
        *cursor = span.1;
    }
    n
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
    /// Clears the owed announcement, so the caller must treat a failed send as a fresh loss
    /// (that is what `record_loss` is for).
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
        // Stamped `now_ns` rather than with the time it was first seen: the batch it belongs to
        // is gone, and a timestamp older than records already delivered would break the ordering
        // the merge exists to provide. What it carries is the movement, not when it happened.
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
        assert_eq!(merge(&[], &[], &mut out), 0);
    }

    #[test]
    fn one_device_passes_through_unchanged() {
        let kbd = [key(30, 10), syn(10)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        let n = merge(&kbd, &[], &mut out);
        assert_eq!(&out[..n], &kbd);
    }

    #[test]
    fn the_older_group_comes_first_whichever_device_it_is_on() {
        // The case merging exists for: a click and a keystroke that happened together must
        // arrive in the order they happened, not the order the reads completed.
        let kbd = [key(30, 100), syn(100)];
        let mouse = [InputEvent { kind: EV_KEY, code: BTN_LEFT, value: 1, time_ns: 95 }, syn(95)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        let n = merge(&kbd, &mouse, &mut out);
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
        let n = merge(&kbd, &mouse, &mut out);
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
        let n = merge(&kbd, &mouse, &mut out);
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
        let n1 = merge(&kbd, &mouse, &mut a);
        let n2 = merge(&kbd, &mouse, &mut b);
        assert_eq!(&a[..n1], &b[..n2]);
        assert_eq!(a[0].kind, EV_KEY, "ties go to the keyboard");
    }

    #[test]
    fn a_full_output_truncates_at_a_group_boundary_not_inside_one() {
        let kbd = [key(30, 10), syn(10), key(31, 20), syn(20)];
        let mut out = [InputEvent::default(); 3];
        let n = merge(&kbd, &[], &mut out);
        assert_eq!(n, 2, "the first group fits; the second does not, so it is not started");
    }

    #[test]
    fn a_trailing_group_with_no_terminator_is_still_delivered() {
        // The driver's ring never produces this. If it ever did, losing ordering beats
        // losing the events.
        let kbd = [key(30, 10)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        assert_eq!(merge(&kbd, &[], &mut out), 1);
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
