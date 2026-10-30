//! The kernel's **wall-clock** time: `CLOCK_REALTIME`.
//!
//! Time-of-day is *derived*, not sampled: the hardware RTC is read **once** at boot to establish
//! an offset, and every later reading is `monotonic + offset`.
//!
//! - **It moves only with the monotonic counter, until someone sets it.** Between sets, timestamps
//!   taken in order are ordered. **A set steps it**, backwards as readily as forwards: code that
//!   must never see a negative interval subtracts `CLOCK_MONOTONIC` readings, which nothing can
//!   step.
//! - **The RTC is slow and racy to read** (port I/O plus an update-in-progress window; see
//!   `arch/x86_64/rtc.rs`), so it is paid once at boot rather than per timestamp: the filesystem
//!   server stamps an inode on every create, mkdir and rename.
//! - **Setting the clock is a single atomic store** to the offset ([`set`], `sys_clock_set`), then
//!   a write of the RTC so the next boot anchors to what was set. Adjusting time-of-day is real
//!   authority (it moves every future timestamp), so it needs `SYSTEM_CLOCK`; reading is ambient,
//!   as `CLOCK_MONOTONIC` already is. See the decision log, 2026-07-24.
//!
//! If the RTC cannot be read (no such device, or an implausible date), the clock stays **unset**
//! until something sets it, and `CLOCK_REALTIME` keeps returning `Unsupported` rather than
//! inventing an epoch: a filesystem stamping 1970 on every file is at least honestly wrong.

use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crate::arch::timer::ArchTimer;
use crate::arch::{Rtc, Timer};
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
/// 2000-2099 on a machine whose FADT names no century register, so a later time would read back as
/// another century at the next boot.
const SETTABLE_UNTIL_SECS: i64 = 4_102_444_800;

/// The RTC, held across [`init`]'s read and [`set`], so two sets cannot leave clock and chip apart.
static RTC: SpinLock<Rtc> = SpinLock::new(
    LockRank::Leaf,
    // SAFETY: the one `Rtc`.
    unsafe { Rtc::claim() },
);

/// Anchor the wall clock from the hardware RTC, once during boot after the monotonic timer is up.
/// Returns the epoch seconds it anchored to, or `None` if no usable clock was found.
pub fn init() -> Option<i64> {
    let epoch_secs = RTC.lock().wall_clock_seconds()?;
    let offset = epoch_secs.checked_mul(NS_PER_SEC)? - Timer::read_ns() as i64;
    OFFSET_NS.store(offset, Ordering::Relaxed);
    // `Release` pairs with the `Acquire` in `realtime_ns`: a reader that sees the clock set also
    // sees its offset.
    IS_SET.store(true, Ordering::Release);
    Some(epoch_secs)
}

/// **Whether `ns` is a time the clock may be set to**, as whole seconds since the epoch: 2000-01-01
/// to the end of 2099, which is what the RTC holds on every machine. Pure, so `sys_clock_set`'s
/// refusal is host-tested.
pub fn settable(ns: u64) -> Option<i64> {
    let secs = i64::try_from(ns / NS_PER_SEC as u64).ok()?;
    (SETTABLE_FROM_SECS..SETTABLE_UNTIL_SECS).contains(&secs).then_some(secs)
}

/// **Set the wall clock** to `ns` since the epoch, and write it to the RTC. Returns whether the RTC
/// holds it: `false` means the clock is set until the next boot, which anchors to whatever the chip
/// still says. The caller has checked [`settable`] and `SYSTEM_CLOCK`.
///
/// **It steps the clock**, backwards as readily as forwards, and anchors a clock the boot could not
/// read.
pub fn set(ns: u64) -> bool {
    let mut rtc = RTC.lock();
    // The offset first, from the counter as it reads now: the RTC write and read-back take a
    // millisecond or more and must not be charged to the time that was asked for.
    OFFSET_NS.store(ns as i64 - Timer::read_ns() as i64, Ordering::Relaxed);
    IS_SET.store(true, Ordering::Release);
    rtc.set_wall_clock_seconds((ns / NS_PER_SEC as u64) as i64)
}

/// Current wall-clock time in nanoseconds since the Unix epoch, or `None` if never anchored.
pub fn realtime_ns() -> Option<i64> {
    if !IS_SET.load(Ordering::Acquire) {
        return None;
    }
    Some(Timer::read_ns() as i64 + OFFSET_NS.load(Ordering::Relaxed))
}

/// Current wall-clock time in whole seconds since the Unix epoch, or `None`.
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

    /// The offset arithmetic, independent of the hardware.
    fn offset_for(epoch_secs: i64, mono_ns: i64) -> i64 {
        epoch_secs * NS_PER_SEC - mono_ns
    }

    #[test]
    fn realtime_tracks_monotonic_exactly() {
        let off = offset_for(1_784_900_730, 5 * NS_PER_SEC);
        assert_eq!(5 * NS_PER_SEC + off, 1_784_900_730 * NS_PER_SEC);
        assert_eq!((95 * NS_PER_SEC + off) - (5 * NS_PER_SEC + off), 90 * NS_PER_SEC);
    }

    #[test]
    fn seconds_truncate_toward_negative_infinity() {
        // `div_euclid`, not `/`: a pre-epoch instant must floor, not truncate toward zero.
        assert_eq!((-1i64).div_euclid(NS_PER_SEC), -1);
        assert_eq!((NS_PER_SEC - 1).div_euclid(NS_PER_SEC), 0);
        assert_eq!((-NS_PER_SEC).div_euclid(NS_PER_SEC), -1);
    }

    /// **The times a set may ask for**, each bound tested at its neighbour.
    #[test]
    fn settable_is_the_rtcs_century() {
        let ns = |secs: i64| secs as u64 * NS_PER_SEC as u64;
        assert_eq!(settable(ns(SETTABLE_FROM_SECS)), Some(946_684_800));
        assert_eq!(settable(ns(SETTABLE_FROM_SECS) - 1), None);
        assert_eq!(settable(ns(SETTABLE_UNTIL_SECS) - 1), Some(4_102_444_799));
        assert_eq!(settable(ns(SETTABLE_UNTIL_SECS)), None);
        assert_eq!(settable(ns(1_790_605_800) + 500_000_000), Some(1_790_605_800));
        assert_eq!(settable(0), None);
        assert_eq!(settable(u64::MAX), None);
    }

    #[test]
    fn unset_clock_reports_nothing() {
        assert!(!is_set());
        assert_eq!(realtime_ns(), None);
        assert_eq!(realtime_secs(), None);
    }
}
