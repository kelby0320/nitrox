//! The kernel's **wall-clock** time — `CLOCK_REALTIME`.
//!
//! Time-of-day is *derived*, not sampled: the hardware RTC is read **once** at
//! boot to establish an offset, and every later reading is
//! `monotonic + offset`. That shape is deliberate.
//!
//! - **It cannot jump backwards.** The monotonic counter is the only thing
//!   advancing, so timestamps taken in order are ordered, and code that
//!   subtracts two realtime readings never sees a negative interval — the
//!   classic hazard of sampling a RTC that a user or NTP can step.
//! - **The RTC is slow and racy to read** (port I/O plus an update-in-progress
//!   window; see `arch/rtc.rs`). Paying that once at boot rather than per
//!   timestamp matters: the filesystem server stamps an inode on every create,
//!   mkdir, and rename.
//! - **Setting the clock becomes a single atomic store** to the offset, which
//!   is what an NTP client or `date --set` will need. That path is deliberately
//!   *not* built here: adjusting time-of-day is real authority (it moves every
//!   future timestamp and, eventually, certificate validity), so it belongs
//!   behind a syscap rather than being ambient. Reading is ambient — it is
//!   information you cannot act on, and `CLOCK_MONOTONIC` already is.
//!   See the decision log, 2026-07-24.
//!
//! If the RTC cannot be read (no such device, or it reports an implausible
//! date), the clock stays **unset** and `CLOCK_REALTIME` keeps returning
//! `Unsupported` rather than inventing an epoch — a filesystem stamping 1970 on
//! every file is at least honestly wrong, where a fabricated "plausible" time
//! is silently wrong.

use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crate::arch::Timer;
use crate::arch::timer::ArchTimer;

/// `realtime_ns = monotonic_ns + OFFSET_NS`. Meaningful only while [`IS_SET`].
static OFFSET_NS: AtomicI64 = AtomicI64::new(0);
/// Whether [`init`] found a usable wall clock.
static IS_SET: AtomicBool = AtomicBool::new(false);

/// Nanoseconds per second.
const NS_PER_SEC: i64 = 1_000_000_000;

/// Anchor the wall clock from the hardware RTC. Called once during boot, after
/// the monotonic timer is up. Returns the epoch seconds it anchored to, or
/// `None` if no usable clock was found (see the module docs).
pub fn init() -> Option<i64> {
    let epoch_secs = crate::arch::wall_clock_seconds()?;
    let offset = epoch_secs.checked_mul(NS_PER_SEC)? - Timer::read_ns() as i64;
    OFFSET_NS.store(offset, Ordering::Relaxed);
    // `Release` pairs with the `Acquire` in `realtime_ns`, so a reader that sees
    // the clock as set also sees the offset it was set with.
    IS_SET.store(true, Ordering::Release);
    Some(epoch_secs)
}

/// Current wall-clock time in nanoseconds since the Unix epoch, or `None` if
/// the clock was never anchored.
pub fn realtime_ns() -> Option<i64> {
    if !IS_SET.load(Ordering::Acquire) {
        return None;
    }
    Some(Timer::read_ns() as i64 + OFFSET_NS.load(Ordering::Relaxed))
}

/// Current wall-clock time in whole seconds since the Unix epoch, or `None`.
/// The form filesystem timestamps want.
pub fn realtime_secs() -> Option<i64> {
    realtime_ns().map(|ns| ns.div_euclid(NS_PER_SEC))
}

/// Whether the wall clock is anchored.
pub fn is_set() -> bool {
    IS_SET.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offset arithmetic, independent of the hardware: anchoring at an
    /// epoch while the monotonic counter reads `mono` must make a later reading
    /// advance by exactly the monotonic delta.
    fn offset_for(epoch_secs: i64, mono_ns: i64) -> i64 {
        epoch_secs * NS_PER_SEC - mono_ns
    }

    #[test]
    fn realtime_tracks_monotonic_exactly() {
        // Anchor at 2026-07-24 13:45:30 UTC with 5 s on the monotonic clock.
        let off = offset_for(1_784_900_730, 5 * NS_PER_SEC);
        // Immediately after anchoring, realtime is the epoch we anchored to.
        assert_eq!(5 * NS_PER_SEC + off, 1_784_900_730 * NS_PER_SEC);
        // 90 s of monotonic later, realtime has advanced by exactly 90 s — no
        // drift, no re-reading of the RTC.
        assert_eq!(
            (95 * NS_PER_SEC + off) - (5 * NS_PER_SEC + off),
            90 * NS_PER_SEC
        );
    }

    #[test]
    fn seconds_truncate_toward_negative_infinity() {
        // `div_euclid`, not `/`: a pre-epoch instant must floor rather than
        // truncate toward zero, or timestamps just before 1970 land a second in
        // the future.
        assert_eq!((-1i64).div_euclid(NS_PER_SEC), -1);
        assert_eq!((NS_PER_SEC - 1).div_euclid(NS_PER_SEC), 0);
        assert_eq!((-NS_PER_SEC).div_euclid(NS_PER_SEC), -1);
    }

    #[test]
    fn unset_clock_reports_nothing() {
        // The global starts unset in a fresh test process; `realtime_ns` must
        // report `None` rather than an offset-of-zero epoch (i.e. 1970).
        assert!(!is_set());
        assert_eq!(realtime_ns(), None);
        assert_eq!(realtime_secs(), None);
    }
}
