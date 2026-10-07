//! **FAT's dates and times** (Phase 6 Part E.2), which carry no zone: read and written as UTC, as
//! the system keeps its clock. A date is days from 1980 to 2107; a time has two-second steps.

/// 1980-01-01 00:00:00 UTC, the first moment a FAT date can hold.
const FAT_EPOCH: i64 = 315_532_800;
/// 2107-12-31 23:59:58 UTC, the last.
const FAT_END: i64 = 4_354_819_198;

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// **Seconds since the epoch as a FAT `(date, time)`**, clamped to what a FAT can hold. `0` — a
/// machine with no clock — is FAT's first moment, as Linux writes it.
pub fn to_fat(secs: i64) -> (u16, u16) {
    let t = secs.clamp(FAT_EPOCH, FAT_END);
    let (days, rem) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let date = (((y - 1980) as u16) << 9) | ((m as u16) << 5) | d as u16;
    let time = (((rem / 3600) as u16) << 11) | ((((rem / 60) % 60) as u16) << 5) | ((rem % 60) / 2) as u16;
    (date, time)
}

/// **A FAT `(date, time)` as seconds since the epoch**, or `0` — unknown — for a date of zero, which
/// a FAT that keeps none writes.
pub fn from_fat(date: u16, time: u16) -> i64 {
    if date == 0 {
        return 0;
    }
    let (y, m, d) = (1980 + (date >> 9) as i64, ((date >> 5) & 0xF) as i64, (date & 0x1F) as i64);
    if !(1..=12).contains(&m) || d == 0 {
        return 0;
    }
    let (h, mi, s) = ((time >> 11) as i64, ((time >> 5) & 0x3F) as i64, ((time & 0x1F) * 2) as i64);
    days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-06 22:54:30 UTC, and its FAT fields read back.
    #[test]
    fn a_moment_round_trips_to_two_seconds() {
        let t = 1_791_327_270;
        let (date, time) = to_fat(t);
        assert_eq!((date >> 9, (date >> 5) & 0xF, date & 0x1F), (46, 10, 6));
        assert_eq!((time >> 11, (time >> 5) & 0x3F, (time & 0x1F) * 2), (22, 54, 30));
        assert_eq!(from_fat(date, time), t);
        assert_eq!(from_fat(to_fat(t + 1).0, to_fat(t + 1).1), t, "two-second steps");
    }

    #[test]
    fn the_ends_clamp_and_no_date_is_unknown() {
        assert_eq!(to_fat(0), to_fat(FAT_EPOCH));
        assert_eq!(from_fat(to_fat(0).0, to_fat(0).1), FAT_EPOCH);
        assert_eq!(from_fat(to_fat(i64::MAX).0, to_fat(i64::MAX).1), FAT_END);
        assert_eq!(from_fat(0, 0), 0);
    }
}
