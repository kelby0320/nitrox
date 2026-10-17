//! Counting a run of clicks: first, second, third.
//!
//! **A double click is a toolkit concept and `route` has no clock**: this is *given* a position and
//! a time (the record's own `time_ms`, the kernel's stamp) and answers with a number.

use libdraw::geom::Point;

/// How long after a press a second one still belongs to the same run, in milliseconds.
pub const RUN_MS: u64 = 400;

/// How many units of content one detent of the wheel moves; a *unit* is what the scrolled thing
/// counts in (lines, rows).
pub const WHEEL_UNITS: u16 = 3;

/// How far the pointer may move between two presses of one run, in pixels: **not zero**, as a mouse
/// moves a pixel or two under a finger.
pub const SLOP: i32 = 4;

/// Counts a run of presses, **compared against the *previous* press rather than the run's first**,
/// so a triple click may drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Clicks {
    last: Option<(Point, u64, u32)>,
}

impl Clicks {
    /// A tracker that has seen nothing.
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Record a press, and say what number it is in its run: `1`, `2`, `3`, … **The count keeps
    /// going up** (`nxterm` wants a triple click).
    pub fn press(&mut self, at: Point, at_ms: u64) -> u32 {
        let n = match self.last {
            Some((was, then, n)) if continues(was, then, at, at_ms) => n.saturating_add(1),
            _ => 1,
        };
        self.last = Some((at, at_ms, n));
        n
    }

    /// Forget the run, so the next press is a first one.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Whether a press at `at`/`at_ms` continues the run whose last press was `was`/`then`.
///
/// **`saturating_sub` on the time**, so a clock going backwards reads as zero elapsed.
fn continues(was: Point, then: u64, at: Point, at_ms: u64) -> bool {
    at_ms.saturating_sub(then) <= RUN_MS && (at.x - was.x).abs() <= SLOP && (at.y - was.y).abs() <= SLOP
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

    /// **Separately, because either alone would pass a tracker that ignored the other.**
    #[test]
    fn time_and_distance_each_end_a_run_by_themselves() {
        let mut c = Clicks::new();
        c.press(AT, 1_000);
        assert_eq!(c.press(AT, 1_000 + RUN_MS + 1), 1, "same place, too late — the interval alone must end the run");

        let mut c = Clicks::new();
        c.press(AT, 1_000);
        let far = Point::new(AT.x + SLOP + 1, AT.y);
        assert_eq!(c.press(far, 1_010), 1, "same moment, too far — distance alone must end it");

        let mut c = Clicks::new();
        c.press(AT, 1_000);
        assert_eq!(c.press(Point::new(AT.x + SLOP, AT.y + SLOP), 1_000 + RUN_MS), 2);
    }

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
    #[test]
    fn a_backwards_clock_does_not_wrap_the_interval() {
        let mut c = Clicks::new();
        c.press(AT, 5_000);
        assert_eq!(c.press(AT, 4_900), 2);
    }
}
