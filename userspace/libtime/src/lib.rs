#![no_std]

//! Calendar arithmetic, and parsing a time or a duration — the pure half of `date` and `sleep`.

extern crate alloc;

use alloc::string::String;

/// A civil date and time, UTC. What [`civil_from_unix`] decomposes an instant into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    /// Astronomical year number; may be zero or negative.
    pub year: i64,
    /// 1-12.
    pub month: u32,
    /// 1-31.
    pub day: u32,
    /// 0-23.
    pub hour: u32,
    /// 0-59.
    pub minute: u32,
    /// 0-59; never 60, as the clock counts SI seconds.
    pub second: u32,
}

/// Days since 1970-01-01 → the civil date `(year, month, day)`, proleptic Gregorian; `days` may
/// be negative.
/// Howard Hinnant's `civil_from_days`, shifted to an era starting 0000-03-01 so the leap day
/// ends a year.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch from 1970-01-01 to 0000-03-01.
    let z = days + 719_468;
    // An era is 400 years = 146_097 days, the Gregorian repeat period.
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146_096]
    // Year of era, [0, 399]; the three correction terms are the 4/100/400 leap rules.
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    // Month of the shifted year, [0, 11], from the regular 153-day, 5-month pattern.
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (mp + if mp < 10 { 3 } else { -9 }) as u32; // [1, 12]
    // March-based year → January-based.
    (y + i64::from(m <= 2), m, d)
}

/// The civil date → days since 1970-01-01, proleptic Gregorian: [`civil_from_days`]'s inverse.
/// Hinnant's `days_from_civil`. `month` must be 1-12 and `day` a day of it; [`parse_utc`]
/// checks that by converting back.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    // January and February belong to the previous March-based year.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from((month + 9) % 12); // March = 0
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146_096]
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` exactly (`2026-09-28T14:30:00Z`) into seconds since the Unix epoch.
/// No offset but `Z` (no timezone database), no fraction, no leap second. A date that does not
/// exist, such as 30 February, is refused rather than rolled over, by converting back and
/// comparing.
pub fn parse_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let punctuation = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':'), (19, b'Z')];
    if b.len() != 20 || punctuation.iter().any(|&(i, c)| b[i] != c) {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<u32> {
        let digit = |n: u32, c: &u8| c.is_ascii_digit().then(|| n * 10 + u32::from(c - b'0'));
        b[from..to].iter().try_fold(0u32, digit)
    };
    let (year, month, day) = (i64::from(num(0, 4)?), num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let in_range = (1..=12).contains(&month) && (1..=31).contains(&day);
    if !in_range || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != (year, month, day) {
        return None;
    }
    Some(days * 86_400 + i64::from(hour * 3600 + minute * 60 + second))
}

/// Decompose nanoseconds since the Unix epoch into a UTC civil date and time.
pub fn civil_from_unix(unix_nanos: u64) -> Civil {
    let secs = (unix_nanos / 1_000_000_000) as i64;
    // Floor-divide, so a pre-epoch instant lands on its own day rather than truncating toward zero.
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400); // second of day, [0, 86_399]
    let (year, month, day) = civil_from_days(days);
    Civil { year, month, day, hour: (sod / 3600) as u32, minute: ((sod % 3600) / 60) as u32, second: (sod % 60) as u32 }
}

/// `YYYY-MM-DD HH:MM:SS` (UTC), the display form `date` prints without a stream.
pub fn format_civil(c: &Civil) -> String {
    let mut s = String::new();
    push_pad(&mut s, c.year.unsigned_abs(), 4);
    s.push('-');
    push_pad(&mut s, u64::from(c.month), 2);
    s.push('-');
    push_pad(&mut s, u64::from(c.day), 2);
    s.push(' ');
    push_pad(&mut s, u64::from(c.hour), 2);
    s.push(':');
    push_pad(&mut s, u64::from(c.minute), 2);
    s.push(':');
    push_pad(&mut s, u64::from(c.second), 2);
    s
}

/// Append `v` zero-padded to at least `width` digits.
fn push_pad(out: &mut String, v: u64, width: usize) {
    let mut digits = [0u8; 20];
    let mut n = 0;
    let mut v = v;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
        if v == 0 {
            break;
        }
    }
    for _ in n..width {
        out.push('0');
    }
    for i in (0..n).rev() {
        out.push(digits[i] as char);
    }
}

/// Parse a duration into **nanoseconds**: a decimal number with an optional unit suffix (bare is
/// seconds; `ns`, `us`, `ms`, `s`, `m`, `h`). Fractions are scaled as integers, so `0.1s` is exact.
/// Returns `None` for anything malformed or that overflows.
pub fn parse_duration(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let mut i = 0;

    let start = i;
    let mut int_part: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        int_part = int_part.checked_mul(10)?.checked_add(u64::from(b[i] - b'0'))?;
        i += 1;
    }
    let had_int = i > start;

    // Keep 9 digits; a nanosecond clock cannot express more, and that is not an error.
    let mut frac: u64 = 0;
    let mut frac_digits = 0u32;
    let mut had_frac = false;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            had_frac = true;
            if frac_digits < 9 {
                frac = frac * 10 + u64::from(b[i] - b'0');
                frac_digits += 1;
            }
            i += 1;
        }
    }
    if !had_int && !had_frac {
        return None;
    }

    let unit_ns: u64 = match &b[i..] {
        b"" | b"s" => 1_000_000_000,
        b"ns" => 1,
        b"us" => 1_000,
        b"ms" => 1_000_000,
        b"m" => 60 * 1_000_000_000,
        b"h" => 3_600 * 1_000_000_000,
        _ => return None,
    };

    let whole = int_part.checked_mul(unit_ns)?;
    if frac_digits == 0 {
        return Some(whole);
    }
    let mut scale = 1u64;
    for _ in 0..frac_digits {
        scale = scale.checked_mul(10)?;
    }
    let frac_ns = frac.checked_mul(unit_ns)? / scale;
    whole.checked_add(frac_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_handles_the_century_rules() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11016), (2000, 2, 29));
        // 2100 is not a leap year (divisible by 100, not 400): the case hand-written rules get wrong.
        assert_eq!(civil_from_days(47540), (2100, 2, 28));
        assert_eq!(civil_from_days(47541), (2100, 3, 1));
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
        // Before the epoch: the era arithmetic must floor, not truncate.
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        for days in -200 * 365..300 * 365 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 2, 29), 11016);
        assert_eq!(days_from_civil(2100, 3, 1), 47541);
    }

    #[test]
    fn parse_utc_reads_the_documented_form() {
        assert_eq!(parse_utc("2026-09-28T14:30:00Z"), Some(1_790_605_800));
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2000-02-29T23:59:59Z"), Some(951_868_799));
        assert_eq!(parse_utc("2031-01-02T00:00:00Z"), Some(1_925_078_400));
        assert_eq!(parse_utc("2099-12-31T23:59:59Z"), Some(4_102_444_799));
        let c = civil_from_unix(1_790_605_800 * 1_000_000_000);
        assert_eq!(format_civil(&c), "2026-09-28 14:30:00");
    }

    /// Anything else is refused, including dates that do not exist.
    #[test]
    fn parse_utc_refuses_everything_else() {
        for bad in [
            "",
            "2026-09-28",
            "2026-09-28 14:30:00Z", // a space for the `T`
            "2026-09-28T14:30:00",  // no `Z`
            "2026-09-28T14:30:00+00:00",
            "2026-09-28t14:30:00z",
            "2026-09-28T14:30:00.5Z", // a fraction
            "2026-9-28T14:30:00Z",
            "+026-09-28T14:30:00Z",
            "2026-13-28T14:30:00Z",
            "2026-00-28T14:30:00Z",
            "2026-09-00T14:30:00Z",
            "2026-09-31T14:30:00Z", // September has 30
            "2026-02-29T14:30:00Z", // 2026 is common
            "2100-02-29T14:30:00Z", // and so is 2100
            "2026-09-28T24:00:00Z",
            "2026-09-28T14:60:00Z",
            "2026-09-28T14:30:60Z", // no leap second
        ] {
            assert_eq!(parse_utc(bad), None, "{bad:?}");
        }
        assert!(parse_utc("2026-09-30T23:59:59Z").is_some());
        assert!(parse_utc("2024-02-29T00:00:00Z").is_some());
        assert!(parse_utc("2000-02-29T00:00:00Z").is_some());
    }

    #[test]
    fn civil_from_unix_splits_the_time_of_day() {
        let c = civil_from_unix(0);
        assert_eq!((c.year, c.month, c.day), (1970, 1, 1));
        assert_eq!((c.hour, c.minute, c.second), (0, 0, 0));

        let c = civil_from_unix(1_709_214_307 * 1_000_000_000);
        assert_eq!((c.year, c.month, c.day), (2024, 2, 29));
        assert_eq!((c.hour, c.minute, c.second), (13, 45, 7));

        let c = civil_from_unix(1_709_214_307 * 1_000_000_000 + 999_999_999);
        assert_eq!(c.second, 7);
    }

    #[test]
    fn format_civil_zero_pads_every_field() {
        let c = civil_from_unix(1_709_214_307 * 1_000_000_000);
        assert_eq!(format_civil(&c), "2024-02-29 13:45:07");
        let c = civil_from_unix(1_000_000_000); // 1970-01-01T00:00:01Z
        assert_eq!(format_civil(&c), "1970-01-01 00:00:01");
    }

    #[test]
    fn parse_duration_accepts_the_documented_forms() {
        assert_eq!(parse_duration("5"), Some(5_000_000_000));
        assert_eq!(parse_duration("5s"), Some(5_000_000_000));
        assert_eq!(parse_duration("200ms"), Some(200_000_000));
        assert_eq!(parse_duration("1500us"), Some(1_500_000));
        assert_eq!(parse_duration("42ns"), Some(42));
        assert_eq!(parse_duration("2m"), Some(120_000_000_000));
        assert_eq!(parse_duration("1h"), Some(3_600_000_000_000));
    }

    #[test]
    fn parse_duration_fractions_are_exact() {
        assert_eq!(parse_duration("1.5s"), Some(1_500_000_000));
        assert_eq!(parse_duration("0.1s"), Some(100_000_000));
        assert_eq!(parse_duration("0.5m"), Some(30_000_000_000));
        assert_eq!(parse_duration(".25s"), Some(250_000_000));
        assert_eq!(parse_duration("0.1234567891s"), Some(123_456_789));
    }

    #[test]
    fn parse_duration_rejects_malformed_input() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("."), None);
        assert_eq!(parse_duration("s"), None);
        assert_eq!(parse_duration("5x"), None);
        assert_eq!(parse_duration("5 s"), None);
        assert_eq!(parse_duration("-5"), None);
        assert_eq!(parse_duration("99999999999999999999"), None);
        assert_eq!(parse_duration("99999999999h"), None);
    }
}
