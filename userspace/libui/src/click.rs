//! Counting a run of clicks: first, second, third.
//!
//! **A double click is a toolkit concept and `route` has no clock** (M14 decision 5). Every list
//! wants one, so the counting lives here rather than in each application — but nothing in `libui`
//! may make a syscall, so this is *given* a position and a time and answers with a number. Pure,
//! testable, and no clock in a module that forbids one.
//!
//! ## Where the time comes from, and what that costs
//!
//! The application reads the clock when the press is *delivered*, because a `PointerEvent` carries
//! no timestamp — `libinput::Logical` drops the `time_ns` the kernel puts on every `InputEvent`,
//! so the press time is not available anywhere above the compositor's input thread.
//!
//! The difference matters in one direction only: a client stalled between two *deliberate* single
//! clicks receives them closer together than they were made, and can read them as a double. It
//! cannot turn a real double click into two singles, because delivery cannot pull events further
//! apart than the stall that bunched them. `TODO(press-time)` carries the fix — a timestamp on the
//! wire, which is what X11 and Wayland both learned to do — and its trigger.

use libdraw::geom::Point;

/// How long after a press a second one still belongs to the same run, in milliseconds.
///
/// **The interval every desktop uses**, near enough: fast enough that two deliberate clicks are
/// two, slow enough that a double click is not a dexterity test.
pub const RUN_MS: u64 = 400;

/// How far the pointer may move between two presses of one run, in pixels.
///
/// **Not zero**, because a mouse moves a pixel or two under a finger pressing a button, and a
/// double click that demanded an identical position would fail for anyone whose hand is not
/// perfectly steady. Not large either: two clicks a centimetre apart are two clicks on two things.
pub const SLOP: i32 = 4;

/// Counts a run of presses.
///
/// **Compared against the *previous* press rather than the run's first**, which is what lets a
/// triple click drift a couple of pixels across its three presses without the third being
/// disowned by the first.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Clicks {
    /// Where and when the last press was, and what number it was in its run.
    last: Option<(Point, u64, u32)>,
}

impl Clicks {
    /// A tracker that has seen nothing.
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Record a press, and say what number it is in its run: `1`, `2`, `3`, …
    ///
    /// A press near enough to the last one in time and space continues that run; anything else
    /// starts a new one. **The count keeps going up** rather than wrapping at two, because
    /// `nxterm` wants a triple click to mean a line — a tracker that only answered "double?" would
    /// be a second one for the third click.
    pub fn press(&mut self, at: Point, at_ms: u64) -> u32 {
        let n = match self.last {
            Some((was, then, n)) if continues(was, then, at, at_ms) => n.saturating_add(1),
            _ => 1,
        };
        self.last = Some((at, at_ms, n));
        n
    }

    /// Forget the run, so the next press is a first one.
    ///
    /// **What a gesture that is not a click calls**, and the reason it is public: a press that
    /// turns into a drag, or one the widget under it acted on by moving, should not leave a run
    /// open for whatever the pointer lands on next.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Whether a press at `at`/`at_ms` continues the run whose last press was `was`/`then`.
///
/// **`saturating_sub` on the time**, so a clock that appears to go backwards — which
/// `CLOCK_MONOTONIC` will not do, but a caller feeding something else might — reads as zero
/// elapsed and continues the run, rather than wrapping to an enormous interval that silently ends
/// every run for ever.
fn continues(was: Point, then: u64, at: Point, at_ms: u64) -> bool {
    at_ms.saturating_sub(then) <= RUN_MS
        && (at.x - was.x).abs() <= SLOP
        && (at.y - was.y).abs() <= SLOP
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: Point = Point::new(100, 40);

    #[test]
    fn presses_close_in_time_and_place_are_one_run() {
        let mut c = Clicks::new();
        assert_eq!(c.press(AT, 1_000), 1);
        assert_eq!(c.press(AT, 1_100), 2);
        assert_eq!(c.press(AT, 1_200), 3, "a run keeps counting past two");
    }

    /// The two ways a run ends, each checked on its own.
    ///
    /// **Separately, because either alone would pass a tracker that ignored the other** — a
    /// version testing only "far apart and long ago" is satisfied by a comparison that looks at
    /// one of them.
    #[test]
    fn time_and_distance_each_end_a_run_by_themselves() {
        let mut c = Clicks::new();
        c.press(AT, 1_000);
        assert_eq!(
            c.press(AT, 1_000 + RUN_MS + 1),
            1,
            "same place, too late — the interval alone must end the run"
        );

        let mut c = Clicks::new();
        c.press(AT, 1_000);
        let far = Point::new(AT.x + SLOP + 1, AT.y);
        assert_eq!(c.press(far, 1_010), 1, "same moment, too far — distance alone must end it");

        // …and the boundaries themselves are inside the run, not outside it.
        let mut c = Clicks::new();
        c.press(AT, 1_000);
        assert_eq!(c.press(Point::new(AT.x + SLOP, AT.y + SLOP), 1_000 + RUN_MS), 2);
    }

    /// A run that ended starts a new one rather than resuming the old.
    #[test]
    fn a_broken_run_starts_again_from_one() {
        let mut c = Clicks::new();
        c.press(AT, 1_000);
        c.press(AT, 1_100);
        assert_eq!(c.press(AT, 9_000), 1, "far too late");
        assert_eq!(c.press(AT, 9_100), 2, "and the new run counts from there");
    }

    #[test]
    fn a_reset_makes_the_next_press_a_first_one() {
        let mut c = Clicks::new();
        c.press(AT, 1_000);
        c.reset();
        assert_eq!(c.press(AT, 1_050), 1, "close enough in time, but the run was abandoned");
    }

    /// A clock that goes backwards continues the run instead of ending every run for ever.
    ///
    /// **The failure this prevents is silent and permanent**: an unsigned subtraction that wrapped
    /// would give an interval of billions, so every press after one bad timestamp would count as a
    /// first — a desktop where double click stopped working and nothing said why.
    #[test]
    fn a_backwards_clock_does_not_wrap_the_interval() {
        let mut c = Clicks::new();
        c.press(AT, 5_000);
        assert_eq!(c.press(AT, 4_900), 2);
    }
}
