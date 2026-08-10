//! `input-server` — the merge, and nothing that needs a kernel.
//!
//! The half of the input server with all the behaviour and none of the syscalls: taking two
//! devices' event streams and producing one ordered stream, and tracking what a consumer
//! missed. `main.rs` is the part that cannot be host-tested — reading the raw nodes, serving
//! `/dev/input/new`, and sending on channels.
//!
//! See `docs/spec/rsproto-input-ops.md` for the contract this implements and
//! `docs/design/input-subsystem.md` for why the server exists at all.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

use libkern::abi::{EV_SYN, InputEvent, SYN_DROPPED, SYN_REPORT};

/// Events buffered from one device per wakeup.
///
/// A PS/2 mouse reports at 100 Hz and a key repeat at ~30 Hz, so a wakeup that has fallen a
/// whole scheduler quantum behind still has far fewer than this waiting.
pub const PER_DEVICE: usize = 32;

/// Events in one merged batch — both devices' worth.
pub const BATCH_MAX: usize = PER_DEVICE * 2 + 1;

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

/// What a consumer is owed: the events to send, and whether a loss must be announced first.
///
/// The server discards a batch it cannot deliver and records the fact here; the next batch
/// that does go out is preceded by `SYN_DROPPED`. That is the same contract the kernel's
/// per-device ring uses, and it is why input needs no back-pressure design — a consumer that
/// falls behind degrades to one that resynchronises.
#[derive(Clone, Copy, Debug, Default)]
pub struct Consumer {
    /// Batches discarded since the last successful send.
    lost: u32,
}

impl Consumer {
    /// A consumer that is up to date.
    pub const fn new() -> Self {
        Self { lost: 0 }
    }

    /// Record that a batch could not be delivered.
    pub fn record_loss(&mut self) {
        self.lost = self.lost.saturating_add(1);
    }

    /// Whether a loss is waiting to be announced.
    pub fn owes_announcement(&self) -> bool {
        self.lost > 0
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
        let extra = usize::from(self.lost > 0);
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
        c.record_loss();
        c.record_loss();
        assert!(c.owes_announcement());

        let batch = [key(30, 10), syn(10)];
        let mut out = [InputEvent::default(); BATCH_MAX];
        let n = c.frame(&batch, 99, &mut out).expect("fits");
        assert_eq!(n, 3);
        assert_eq!(out[0].kind, EV_SYN);
        assert_eq!(out[0].code, SYN_DROPPED, "the announcement leads, before any survivor");
        assert_eq!(out[0].value, 2, "and carries how many batches went");
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
        c.record_loss();
        let batch = [key(30, 10), syn(10)];
        let mut out = [InputEvent::default(); 2]; // room for the batch but not the marker
        assert_eq!(c.frame(&batch, 99, &mut out), None);
        assert!(c.owes_announcement(), "and the loss is still owed afterwards");
    }
}
