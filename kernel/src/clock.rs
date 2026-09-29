//! The kernel's **wall-clock** time — `CLOCK_REALTIME`.
//!
//! Time-of-day is *derived*, not sampled: the hardware RTC is read **once** at
//! boot to establish an offset, and every later reading is
//! `monotonic + offset`. That shape is deliberate.
//!
//! - **It moves only with the monotonic counter, until someone sets it.**
//!   Between sets, timestamps taken in order are ordered. **A set steps it**,
//!   backwards as readily as forwards (administration Part E.5): code that must
//!   never see a negative interval subtracts `CLOCK_MONOTONIC` readings, which
//!   nothing can step. Until E.5 this said the realtime clock "cannot jump
//!   backwards", which was true only because nothing could set it.
//! - **The RTC is slow and racy to read** (port I/O plus an update-in-progress
//!   window; see `arch/rtc.rs`). Paying that once at boot rather than per
//!   timestamp matters: the filesystem server stamps an inode on every create,
//!   mkdir, and rename.
//! - **Setting the clock is a single atomic store** to the offset ([`set`],
//!   `sys_clock_set`), then a write of the RTC, so the next boot anchors to
//!   what was set. Adjusting time-of-day is real authority (it moves every
//!   future timestamp and, eventually, certificate validity), so it needs
//!   `SYSTEM_CLOCK` rather than being ambient. Reading is ambient — it is
//!   information you cannot act on, and `CLOCK_MONOTONIC` already is.
//!   See the decision log, 2026-07-24.
//!
//! If the RTC cannot be read (no such device, or it reports an implausible
//! date), the clock stays **unset** until something sets it, and
//! `CLOCK_REALTIME` keeps returning `Unsupported` rather than inventing an
//! epoch — a filesystem stamping 1970 on every file is at least honestly
//! wrong, where a fabricated "plausible" time is silently wrong.

use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crate::arch::Timer;
use crate::arch::timer::ArchTimer;
use crate::libkern::SpinLock;
use crate::libkern::lockrank::LockRank;

/// `realtime_ns = monotonic_ns + OFFSET_NS`. Meaningful only while [`IS_SET`].
static OFFSET_NS: AtomicI64 = AtomicI64::new(0);
/// Whether the wall clock is anchored: by [`init`], from the RTC, or by [`set`].
static IS_SET: AtomicBool = AtomicBool::new(false);

/// Nanoseconds per second.
const NS_PER_SEC: i64 = 1_000_000_000;

/// The first second the clock may be set to: 2000-01-01T00:00:00Z.
const SETTABLE_FROM_SECS: i64 = 946_684_800;
/// The first second it may not: 2100-01-01T00:00:00Z. The RTC holds a two-digit year, read as
/// 2000–2099 on a machine whose FADT names no century register, so a later time would read back
/// as another century at the next boot.
const SETTABLE_UNTIL_SECS: i64 = 4_102_444_800;

/// Serializes [`set`]: the offset and the RTC must be written by one caller at a time, or two sets
/// could leave the running clock at one and the chip at the other — and the RTC's index/data port
/// pair interleaved.
static SETTING: SpinLock<()> = SpinLock::new(LockRank::Leaf, ());

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

/// **Whether `ns` is a time the clock may be set to**, as whole seconds since the epoch: from
/// 2000-01-01 to the end of 2099, which is what the RTC holds on every machine (administration Part
/// E.5). A pure function, so `sys_clock_set`'s refusal is host-tested.
pub fn settable(ns: u64) -> Option<i64> {
    let secs = i64::try_from(ns / NS_PER_SEC as u64).ok()?;
    (SETTABLE_FROM_SECS..SETTABLE_UNTIL_SECS).contains(&secs).then_some(secs)
}

/// **Set the wall clock** to `ns` since the epoch, and write it to the RTC (administration Part
/// E.5). Returns whether the RTC holds it: `false` means the clock is set until the next boot,
/// which anchors to whatever the chip still says. The caller has checked [`settable`] and
/// `SYSTEM_CLOCK`.
///
/// **It steps the clock**, backwards as readily as forwards (see the module docs), and anchors
/// one that never was: a machine whose RTC could not be read at boot has a wall clock from here.
pub fn set(ns: u64) -> bool {
    let _setting = SETTING.lock();
    // The offset first, from the counter as it reads now: the RTC's write and read-back take a
    // millisecond or more, and should not be charged to the time that was asked for.
    OFFSET_NS.store(ns as i64 - Timer::read_ns() as i64, Ordering::Relaxed);
    IS_SET.store(true, Ordering::Release);
    crate::arch::set_wall_clock_seconds((ns / NS_PER_SEC as u64) as i64)
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

    /// **The times a set may ask for**: 2000-01-01T00:00:00Z to 2099-12-31T23:59:59Z, and a
    /// fraction of a second inside either end — each bound tested at its neighbour.
    #[test]
    fn settable_is_the_rtcs_century() {
        let ns = |secs: i64| secs as u64 * NS_PER_SEC as u64;
        assert_eq!(settable(ns(SETTABLE_FROM_SECS)), Some(946_684_800));
        assert_eq!(settable(ns(SETTABLE_FROM_SECS) - 1), None);
        assert_eq!(settable(ns(SETTABLE_UNTIL_SECS) - 1), Some(4_102_444_799));
        assert_eq!(settable(ns(SETTABLE_UNTIL_SECS)), None);
        // 2026-09-28T14:30:00.5Z is its whole second.
        assert_eq!(settable(ns(1_790_605_800) + 500_000_000), Some(1_790_605_800));
        assert_eq!(settable(0), None);
        assert_eq!(settable(u64::MAX), None);
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
