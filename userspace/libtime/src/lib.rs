#![no_std]

//! Calendar arithmetic and duration parsing — the pure half of `date` and `sleep`.
//!
//! Both utilities are mostly *not* about syscalls: `date` reads one clock and then does
//! calendar arithmetic, and `sleep` arms one timer and then does string parsing. Those
//! two pieces are where the bugs live, and they are the parts that need no kernel at all
//! — so they live here, in the library, and are tested on the host.
//!
//! There is no `std`, so the civil-from-days conversion is hand-rolled. It is Howard
//! Hinnant's algorithm (the one behind `<chrono>`), chosen because it is branch-free over
//! the leap rules rather than a chain of special cases — the century rules that make 1900
//! and 2100 common years while 2000 is a leap year fall out of the era arithmetic instead
//! of being written down and then got wrong.

extern crate alloc;

use alloc::string::String;

/// A civil date and time, UTC. What [`civil_from_unix`] decomposes an instant into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    /// 1-12.
    pub month: u32,
    /// 1-31.
    pub day: u32,
    /// 0-23.
    pub hour: u32,
    /// 0-59.
    pub minute: u32,
    /// 0-59. Leap seconds do not exist here: the clock is a count of SI seconds since
    /// the epoch, so this never reads 60.
    pub second: u32,
}

/// Days since 1970-01-01 → the civil date, proleptic Gregorian.
///
/// Hinnant's `civil_from_days`, shifted to an era beginning 0000-03-01 so that the leap
/// day lands at the *end* of a year and the month-length pattern becomes regular. `days`
/// may be negative (dates before the epoch).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch from 1970-01-01 to 0000-03-01.
    let z = days + 719_468;
    // An era is 400 years = 146_097 days, the Gregorian repeat period.
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146_096]
    // Year of era, [0, 399]. The three correction terms are the 4/100/400 leap rules.
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    // Month of the shifted year, [0, 11], from the regular 153-day 5-month pattern.
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (mp + if mp < 10 { 3 } else { -9 }) as u32; // [1, 12]
    // March-based year → January-based.
    (y + i64::from(m <= 2), m, d)
}

/// Decompose nanoseconds since the Unix epoch into a UTC civil date and time.
pub fn civil_from_unix(unix_nanos: u64) -> Civil {
    let secs = (unix_nanos / 1_000_000_000) as i64;
    // Floor-divide, so a pre-epoch instant lands on the right day rather than truncating
    // toward zero. (`secs` is non-negative for any clock this kernel can produce, but the
    // arithmetic should not quietly depend on that.)
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400); // second of day, [0, 86_399]
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: (sod / 3600) as u32,
        minute: ((sod % 3600) / 60) as u32,
        second: (sod % 60) as u32,
    }
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

/// Parse a duration into **nanoseconds**.
///
/// Accepts a decimal number with an optional unit suffix: bare (seconds), `ns`, `us`,
/// `ms`, `s`, `m`, `h`. A fractional part is exact — the digits are scaled as integers
/// rather than going through a float, so `0.1s` is exactly 100 ms and not a value that
/// depends on binary rounding.
///
/// Returns `None` for anything malformed, including an empty string, a lone `.`, an
/// unknown suffix, or a value that would overflow.
pub fn parse_duration(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let mut i = 0;

    // Integer part.
    let start = i;
    let mut int_part: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        int_part = int_part.checked_mul(10)?.checked_add(u64::from(b[i] - b'0'))?;
        i += 1;
    }
    let had_int = i > start;

    // Fractional part: keep 9 digits (nanosecond resolution), ignore the rest rather than
    // rejecting — trailing precision a nanosecond clock cannot express is not an error.
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
    // frac / 10^frac_digits of a unit, computed as integers.
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

    /// The dates worth pinning are the ones the leap rules disagree about.
    #[test]
    fn civil_from_days_handles_the_century_rules() {
        // The epoch itself.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000 is a leap year (divisible by 400), so 2000-02-29 exists.
        assert_eq!(civil_from_days(11016), (2000, 2, 29));
        // 2100 is **not** (divisible by 100, not by 400) — the case a hand-written
        // leap rule usually gets wrong. 2100-02-28 is followed by 2100-03-01.
        assert_eq!(civil_from_days(47540), (2100, 2, 28));
        assert_eq!(civil_from_days(47541), (2100, 3, 1));
        // An ordinary leap year.
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
        // Before the epoch: the era arithmetic must floor, not truncate.
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn civil_from_unix_splits_the_time_of_day() {
        let c = civil_from_unix(0);
        assert_eq!((c.year, c.month, c.day), (1970, 1, 1));
        assert_eq!((c.hour, c.minute, c.second), (0, 0, 0));

        // 2024-02-29T13:45:07Z
        let c = civil_from_unix(1_709_214_307 * 1_000_000_000);
        assert_eq!((c.year, c.month, c.day), (2024, 2, 29));
        assert_eq!((c.hour, c.minute, c.second), (13, 45, 7));

        // Sub-second input must not disturb the second.
        let c = civil_from_unix(1_709_214_307 * 1_000_000_000 + 999_999_999);
        assert_eq!(c.second, 7);
    }

    #[test]
    fn format_civil_zero_pads_every_field() {
        let c = civil_from_unix(1_709_214_307 * 1_000_000_000);
        assert_eq!(format_civil(&c), "2024-02-29 13:45:07");
        // Single-digit month/day/time must pad, which is the whole point of the helper.
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

    /// Fractions are scaled as integers, so these are exact rather than nearly-right.
    #[test]
    fn parse_duration_fractions_are_exact() {
        assert_eq!(parse_duration("1.5s"), Some(1_500_000_000));
        assert_eq!(parse_duration("0.1s"), Some(100_000_000));
        assert_eq!(parse_duration("0.5m"), Some(30_000_000_000));
        assert_eq!(parse_duration(".25s"), Some(250_000_000));
        // More precision than a nanosecond clock can hold is truncated, not rejected.
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
        // A unit that would overflow the multiply.
        assert_eq!(parse_duration("99999999999h"), None);
    }
}
