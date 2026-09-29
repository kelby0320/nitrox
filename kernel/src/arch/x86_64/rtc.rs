//! The battery-backed real-time clock — the machine's only source of
//! **wall-clock** time.
//!
//! Read once at boot to anchor the kernel's realtime clock
//! ([`crate::clock`]); after that, time-of-day is the monotonic counter plus an
//! offset, so it advances smoothly. **Written when the clock is set**
//! (`sys_clock_set`, administration Part E.5), so the next boot anchors to the
//! time that was set rather than to what the chip had before.
//!
//! On a PC that clock is the MC146818-compatible CMOS RTC behind the index/data
//! port pair `0x70`/`0x71`. The equivalent on another architecture is a
//! memory-mapped RTC (`PL031` on many aarch64 boards), which is why the neutral
//! name this is exported under is `wall_clock_seconds` — the *concept* is
//! portable, the CMOS ports are not.
//!
//! ## Reading it correctly
//!
//! Three hazards, all handled below:
//!
//! - **Mid-update tearing.** The chip updates its registers roughly once a
//!   second and sets `UIP` (update-in-progress) in status register A while it
//!   does. Reading through that window can catch a half-rolled-over time
//!   (`23:59:60`-shaped garbage). We wait for `UIP` to clear, then read the
//!   whole set, then read it again and require the two to agree — the standard
//!   double-read, because the update can begin between our `UIP` check and our
//!   last register read.
//! - **BCD vs binary.** Bit 2 of status register B says which. Most firmware
//!   leaves it in BCD, where `0x59` means 59.
//! - **12-hour mode.** Bit 1 of status register B. In 12-hour mode bit 7 of the
//!   hours register is the PM flag, and 12 AM is stored as 12, not 0.
//!
//! ## Writing it
//!
//! Setting `SET` (bit 7 of status B) stops the chip's updates, so the registers
//! are written without an update landing half way through; clearing it starts
//! them again. The time is written **in the chip's own encoding** — the BCD or
//! binary, 12- or 24-hour form status B already says it keeps — since firmware
//! reads the chip that way too. Then it is read back, and a write the chip did
//! not keep is reported rather than assumed: a machine with no RTC reads
//! `0xFF` everywhere.
//!
//! ## What it does not do
//!
//! The RTC is assumed to hold **UTC**. That is what QEMU provides by default
//! (`-rtc base=utc`) and the convention every Unix-like system uses; a machine
//! whose firmware keeps local time would report a skewed epoch. There is no
//! timezone database to correct it with, and a timezone is a *display* concern
//! for the shell, not a kernel one.
//!
//! The **century** comes from the century register when ACPI's FADT names one
//! (administration Part E.3), in the chip's own BCD or binary. Without one — no
//! FADT, or one that names none, or a register that reads outside 19–21 — a
//! two-digit year is mapped into 2000–2099, which is what it was before the FADT
//! was parsed.

use super::regs::{inb, outb};

/// CMOS address (index) port. Bit 7 additionally masks NMI; we preserve it.
const CMOS_ADDR: u16 = 0x70;
/// CMOS data port.
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
/// Day of the week, 1 (Sunday) to 7. Never read — the date says it — but written, for firmware
/// that shows it.
const REG_WEEKDAY: u8 = 0x06;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

/// Status A: an update is in progress; the time registers may be mid-roll.
const STATUS_A_UIP: u8 = 1 << 7;
/// Status B: time registers are binary rather than BCD.
const STATUS_B_BINARY: u8 = 1 << 2;
/// Status B: hours are 12-hour with a PM flag rather than 24-hour.
const STATUS_B_24_HOUR: u8 = 1 << 1;
/// Status B: the chip stops updating the time registers while this is set, so
/// a write cannot be torn by an update.
const STATUS_B_SET: u8 = 1 << 7;

/// Bound on the `UIP` spin. The flag is set for well under a millisecond per
/// second, so this is orders of magnitude of headroom; it exists so a machine
/// with no RTC (or a stuck one) fails the read instead of hanging the boot.
const UIP_SPIN_LIMIT: u32 = 1_000_000;

/// One raw reading of the time registers.
#[derive(Copy, Clone, PartialEq, Eq)]
struct Raw {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    /// The century register, when the FADT names one.
    century: Option<u8>,
}

/// Read one CMOS register.
///
/// # Safety
/// Port I/O. Reads a status/time register only; never writes CMOS state.
unsafe fn read_reg(reg: u8) -> u8 {
    // Preserve the NMI-disable bit (bit 7) as the firmware left it rather than
    // clearing it as a side effect of every read.
    // SAFETY: reading the current index, then selecting `reg` and reading data.
    unsafe {
        let nmi = inb(CMOS_ADDR) & 0x80;
        outb(CMOS_ADDR, nmi | (reg & 0x7F));
        inb(CMOS_DATA)
    }
}

/// Write one CMOS register.
///
/// # Safety
/// Port I/O. `reg` must be a time register, status B, or the FADT's century
/// register, and the caller must serialize writers ([`crate::clock`] does).
unsafe fn write_reg(reg: u8, value: u8) {
    // SAFETY: selecting `reg`, preserving the NMI-disable bit as `read_reg` does, and writing it.
    unsafe {
        let nmi = inb(CMOS_ADDR) & 0x80;
        outb(CMOS_ADDR, nmi | (reg & 0x7F));
        outb(CMOS_DATA, value);
    }
}

/// Read the six time registers once.
///
/// # Safety
/// As [`read_reg`].
unsafe fn read_raw(century: Option<u8>) -> Raw {
    // SAFETY: all six are plain time registers, and the century register's index is the FADT's.
    unsafe {
        Raw {
            second: read_reg(REG_SECONDS),
            minute: read_reg(REG_MINUTES),
            hour: read_reg(REG_HOURS),
            day: read_reg(REG_DAY),
            month: read_reg(REG_MONTH),
            year: read_reg(REG_YEAR),
            century: century.map(|index| read_reg(index)),
        }
    }
}

/// Seconds since the Unix epoch from the machine's RTC, or `None` if the clock
/// could not be read consistently or reports an implausible date.
///
/// Called **once**, during boot, from [`crate::clock::init`].
pub fn wall_clock_seconds() -> Option<i64> {
    let century = super::acpi::fadt().and_then(|f| f.century);
    let (raw, status_b) = read_stable(century)?;
    decode(raw, status_b)
}

/// **Write `secs` to the RTC**, and say whether it holds it: read back, within
/// two seconds of what was written, since the chip goes on counting.
///
/// Called only from [`crate::clock::set`], which serializes callers. `false`
/// for a time the chip cannot hold — before 2000 or from 2100 without a
/// century register — and for a chip that did not keep the write.
pub fn set_wall_clock_seconds(secs: i64) -> bool {
    let century = super::acpi::fadt().and_then(|f| f.century);
    // SAFETY: status B is a plain configuration register.
    let status_b = unsafe { read_reg(REG_STATUS_B) } & !STATUS_B_SET;
    let Some((raw, weekday)) = encode(secs, status_b, century.is_some()) else {
        return false;
    };
    // SAFETY: port I/O on the time registers, status B and the FADT's century register, with
    // updates stopped for the write; `crate::clock` holds its lock around this call.
    unsafe {
        write_reg(REG_STATUS_B, status_b | STATUS_B_SET);
        write_reg(REG_SECONDS, raw.second);
        write_reg(REG_MINUTES, raw.minute);
        write_reg(REG_HOURS, raw.hour);
        write_reg(REG_WEEKDAY, weekday);
        write_reg(REG_DAY, raw.day);
        write_reg(REG_MONTH, raw.month);
        write_reg(REG_YEAR, raw.year);
        if let (Some(index), Some(value)) = (century, raw.century) {
            write_reg(index, value);
        }
        write_reg(REG_STATUS_B, status_b);
    }
    read_stable(century)
        .and_then(|(raw, status_b)| decode(raw, status_b))
        .is_some_and(|read| (read - secs).abs() <= 2)
}

/// One stable reading of the time registers, and status B: wait out an update,
/// then read until two readings agree. `None` for a chip that never settles.
fn read_stable(century: Option<u8>) -> Option<(Raw, u8)> {
    // Wait out any in-progress update, then double-read: the update can start
    // between the `UIP` check and the last register read, so two identical
    // readings are what actually proves the value is stable.
    let mut spins = 0u32;
    // SAFETY: port I/O against the CMOS index/data pair; reads only.
    let mut prev = unsafe {
        while read_reg(REG_STATUS_A) & STATUS_A_UIP != 0 {
            spins += 1;
            if spins > UIP_SPIN_LIMIT {
                return None; // no RTC, or one stuck mid-update
            }
        }
        read_raw(century)
    };
    let mut tries = 0u32;
    loop {
        // SAFETY: as above.
        let next = unsafe {
            while read_reg(REG_STATUS_A) & STATUS_A_UIP != 0 {
                spins += 1;
                if spins > UIP_SPIN_LIMIT {
                    return None;
                }
            }
            read_raw(century)
        };
        if next == prev {
            break;
        }
        prev = next;
        tries += 1;
        if tries > 8 {
            return None; // never settled — treat as unreadable
        }
    }

    // SAFETY: status B is a plain configuration register.
    let status_b = unsafe { read_reg(REG_STATUS_B) };
    Some((prev, status_b))
}

/// Convert a stable register reading into Unix epoch seconds, honouring the
/// BCD/binary and 12/24-hour encodings status register B selects.
///
/// Split out from the port I/O so the fiddly part is a pure function with host
/// tests — the encodings are exactly where an RTC read goes quietly wrong.
fn decode(raw: Raw, status_b: u8) -> Option<i64> {
    let binary = status_b & STATUS_B_BINARY != 0;
    let conv = |v: u8| if binary { Some(v) } else { bcd_to_binary(v) };

    // The PM flag rides in bit 7 of the hours register in 12-hour mode, so mask
    // it off *before* converting — 0x92 is not valid BCD.
    let hour_raw = raw.hour;
    let pm = status_b & STATUS_B_24_HOUR == 0 && hour_raw & 0x80 != 0;
    let mut hour = conv(hour_raw & 0x7F)?;
    if status_b & STATUS_B_24_HOUR == 0 {
        // 12-hour: 12 AM is stored as 12 and means 0; 12 PM stays 12.
        if hour == 12 {
            hour = 0;
        }
        if pm {
            hour += 12;
        }
    }

    let second = conv(raw.second)?;
    let minute = conv(raw.minute)?;
    let day = conv(raw.day)?;
    let month = conv(raw.month)?;
    let year2 = conv(raw.year)?;

    // The century register when the FADT names one and it reads as one; otherwise a two-digit
    // year maps into 2000–2099.
    let century = raw.century.and_then(conv).filter(|c| (19..=21).contains(c)).unwrap_or(20);
    let year = century as i64 * 100 + year2 as i64;

    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let days = days_from_civil(year, month as u32, day as u32);
    Some(days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64)
}

/// **The registers for `secs`**, in the encoding status register B selects, and
/// the day of the week — the inverse of [`decode`], and host-tested against it.
///
/// `century` says whether the FADT names a century register. Without one the
/// chip holds 2000–2099 alone, since that is how [`decode`] reads two digits;
/// with one, the centuries [`decode`] believes, 19 to 21. Anything else is
/// `None`, rather than a write the next boot reads as another year.
fn encode(secs: i64, status_b: u8, century: bool) -> Option<(Raw, u8)> {
    let binary = status_b & STATUS_B_BINARY != 0;
    let conv = |v: u32| {
        let v = v as u8;
        if binary { v } else { (v / 10) << 4 | v % 10 }
    };
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let (hundreds, year2) = (year.div_euclid(100), year.rem_euclid(100) as u32);
    let representable = if century { (19..=21).contains(&hundreds) } else { hundreds == 20 };
    if !representable {
        return None;
    }
    let hour24 = sod / 3600;
    let hour = if status_b & STATUS_B_24_HOUR != 0 {
        conv(hour24)
    } else {
        // 12-hour: midnight is 12 AM and noon 12 PM, with the PM flag in bit 7.
        let twelve = if hour24 % 12 == 0 { 12 } else { hour24 % 12 };
        conv(twelve) | if hour24 >= 12 { 0x80 } else { 0 }
    };
    let raw = Raw {
        second: conv(sod % 60),
        minute: conv(sod / 60 % 60),
        hour,
        day: conv(day),
        month: conv(month),
        year: conv(year2),
        century: century.then(|| conv(hundreds as u32)),
    };
    // 1970-01-01 was a Thursday, day 5 counting Sunday as 1.
    let weekday = (days + 4).rem_euclid(7) as u8 + 1;
    Some((raw, weekday))
}

/// One packed BCD byte to binary, or `None` if either nibble is not a digit.
fn bcd_to_binary(v: u8) -> Option<u8> {
    let hi = v >> 4;
    let lo = v & 0x0F;
    if hi > 9 || lo > 9 {
        return None;
    }
    Some(hi * 10 + lo)
}

/// The proleptic-Gregorian civil date `days` after 1970-01-01, as
/// `(year, month, day)` — the inverse of [`days_from_civil`], and Hinnant's
/// `civil_from_days`, as `libtime`'s is in userspace.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146_096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date.
///
/// Howard Hinnant's `days_from_civil`: shift the year to start in March so the
/// leap day lands at the end of the "year", which makes the day-of-year a closed
/// form and removes every leap-year special case from the arithmetic.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(second: u8, minute: u8, hour: u8, day: u8, month: u8, year: u8) -> Raw {
        Raw { second, minute, hour, day, month, year, century: None }
    }

    #[test]
    fn bcd_conversion_rejects_non_digits() {
        assert_eq!(bcd_to_binary(0x00), Some(0));
        assert_eq!(bcd_to_binary(0x59), Some(59));
        assert_eq!(bcd_to_binary(0x99), Some(99));
        // `0x1A` / `0xA1` are not valid BCD — a register misread as BCD when the
        // chip is in binary mode produces exactly these, so it must not silently
        // yield a plausible number.
        assert_eq!(bcd_to_binary(0x1A), None);
        assert_eq!(bcd_to_binary(0xA1), None);
    }

    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        // 2000 is a leap year (divisible by 400) but 1900 and 2100 are not —
        // the three cases a naive `year % 4` gets wrong.
        assert_eq!(days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28), 2);
        assert_eq!(days_from_civil(2100, 3, 1) - days_from_civil(2100, 2, 28), 1);
        // Cross-checked against `date -u -d '2026-07-24' +%s` / 86400.
        assert_eq!(days_from_civil(2026, 7, 24), 20658);
    }

    #[test]
    fn decodes_bcd_24_hour() {
        // 2026-07-24 13:45:30 UTC = 1774360530 (`date -u -d ... +%s`).
        let r = raw(0x30, 0x45, 0x13, 0x24, 0x07, 0x26);
        assert_eq!(decode(r, STATUS_B_24_HOUR), Some(1_784_900_730));
    }

    #[test]
    fn decodes_binary_24_hour() {
        // Same instant with the chip in binary mode.
        let r = raw(30, 45, 13, 24, 7, 26);
        assert_eq!(
            decode(r, STATUS_B_24_HOUR | STATUS_B_BINARY),
            Some(1_784_900_730)
        );
    }

    #[test]
    fn decodes_12_hour_pm_and_midnight() {
        // 1:45:30 PM in 12-hour BCD — the PM flag is bit 7 of the hours
        // register, which must be masked before BCD conversion (0x81 is valid
        // BCD but 0x93 would not be).
        let pm = raw(0x30, 0x45, 0x01 | 0x80, 0x24, 0x07, 0x26);
        assert_eq!(decode(pm, 0), Some(1_784_900_730));
        // 12 AM is stored as 12 and means hour 0 — the classic off-by-twelve.
        let midnight = raw(0x00, 0x00, 0x12, 0x24, 0x07, 0x26);
        assert_eq!(decode(midnight, 0), Some(1_784_851_200));
        // 12 PM stays 12, it does not become 24.
        let noon = raw(0x00, 0x00, 0x12 | 0x80, 0x24, 0x07, 0x26);
        assert_eq!(decode(noon, 0), Some(1_784_851_200 + 12 * 3600));
    }

    #[test]
    fn rejects_implausible_registers() {
        // Month 0 / day 0 / hour 25 are what a dead or absent RTC reports; a
        // bogus date must fail the read rather than anchor the system clock to
        // nonsense.
        assert_eq!(decode(raw(0, 0, 0, 1, 0, 0x26), STATUS_B_BINARY), None);
        assert_eq!(decode(raw(0, 0, 0, 0, 1, 0x26), STATUS_B_BINARY), None);
        assert_eq!(decode(raw(0, 0, 25, 1, 1, 0x26), STATUS_B_BINARY), None);
        assert_eq!(decode(raw(0, 61, 0, 1, 1, 0x26), STATUS_B_BINARY), None);
    }

    #[test]
    fn a_two_digit_year_lands_in_this_century() {
        // Year `00` is 2000, not 1900 — and certainly not 0.
        let r = raw(0, 0, 0, 1, 1, 0);
        assert_eq!(decode(r, STATUS_B_BINARY), Some(946_684_800));
    }

    /// **The century register, when the FADT names one** (administration Part E.3), in the chip's
    /// own encoding — and a reading outside 19–21 is not believed.
    #[test]
    fn a_century_register_sets_the_century() {
        // 1999-12-31 00:00 UTC is 946_598_400; with a century of 19, year 99 is 1999.
        let bcd = Raw { century: Some(0x19), ..raw(0, 0, 0, 0x31, 0x12, 0x99) };
        assert_eq!(decode(bcd, STATUS_B_24_HOUR), Some(946_598_400));
        let binary = Raw { century: Some(19), ..raw(0, 0, 0, 31, 12, 99) };
        assert_eq!(decode(binary, STATUS_B_BINARY | STATUS_B_24_HOUR), Some(946_598_400));
        // 20 reads as the guess did; 0x20 BCD is 20.
        let twenty = Raw { century: Some(0x20), ..raw(0, 0, 0, 1, 1, 0) };
        assert_eq!(decode(twenty, STATUS_B_24_HOUR), Some(946_684_800));
        // Nonsense falls back to 20, and a non-BCD byte in BCD mode does too.
        let wild = Raw { century: Some(55), ..raw(0, 0, 0, 1, 1, 0) };
        assert_eq!(decode(wild, STATUS_B_BINARY | STATUS_B_24_HOUR), Some(946_684_800));
        let not_bcd = Raw { century: Some(0x2a), ..raw(0, 0, 0, 1, 1, 0) };
        assert_eq!(decode(not_bcd, STATUS_B_24_HOUR), Some(946_684_800));
    }

    /// **What a set writes, byte for byte** (administration Part E.5), in each encoding — the
    /// instants `decodes_*` read, so a writer and a reader that agreed on a wrong encoding would
    /// still disagree with these.
    #[test]
    fn encodes_each_form_the_chip_keeps() {
        // 2026-07-24 13:45:30 UTC, a Friday: weekday 6, counting Sunday as 1.
        let t = 1_784_900_730;
        let bcd24 = encode(t, STATUS_B_24_HOUR, false).unwrap();
        assert!(bcd24 == (raw(0x30, 0x45, 0x13, 0x24, 0x07, 0x26), 6));
        let bin24 = encode(t, STATUS_B_24_HOUR | STATUS_B_BINARY, false).unwrap();
        assert!(bin24 == (raw(30, 45, 13, 24, 7, 26), 6));
        // 12-hour: 1 PM is 1 with the PM flag, in BCD and in binary.
        assert!(encode(t, 0, false).unwrap().0 == raw(0x30, 0x45, 0x01 | 0x80, 0x24, 0x07, 0x26));
        assert!(encode(t, STATUS_B_BINARY, false).unwrap().0 == raw(30, 45, 1 | 0x80, 24, 7, 26));
        // Midnight is 12 AM, and noon 12 PM — the off-by-twelve both ways.
        let midnight = 1_784_851_200;
        assert!(encode(midnight, 0, false).unwrap().0.hour == 0x12);
        assert!(encode(midnight + 12 * 3600, 0, false).unwrap().0.hour == 0x12 | 0x80);
        assert!(encode(midnight, STATUS_B_24_HOUR, false).unwrap().0.hour == 0x00);
        // A century register gets the century, in the same encoding.
        assert!(encode(t, STATUS_B_24_HOUR, true).unwrap().0.century == Some(0x20));
        assert!(encode(t, STATUS_B_24_HOUR | STATUS_B_BINARY, true).unwrap().0.century == Some(20));
        assert!(encode(t, STATUS_B_24_HOUR, false).unwrap().0.century.is_none());
        // The weekday: 1970-01-01 a Thursday (5), 1970-01-04 the Sunday after (1), and
        // 2000-01-01 a Saturday (7).
        assert_eq!(encode(0, STATUS_B_24_HOUR, true).unwrap().1, 5);
        assert_eq!(encode(3 * 86_400, STATUS_B_24_HOUR, true).unwrap().1, 1);
        assert_eq!(encode(946_684_800, STATUS_B_24_HOUR, false).unwrap().1, 7);
    }

    /// **Every encoding reads back as what was written**, across the day, the leap days and both
    /// ends of the century.
    #[test]
    fn what_is_written_reads_back() {
        let instants = [
            946_684_800i64, // 2000-01-01 00:00:00
            951_782_400,   // 2000-02-29 00:00:00, a leap day in a century year
            1_784_900_730, // 2026-07-24 13:45:30
            1_790_605_800, // 2026-09-28 14:30:00
            1_925_078_400, // 2031-01-02 00:00:00
            4_102_444_799, // 2099-12-31 23:59:59, the last second the chip holds without a century
        ];
        for &t in &instants {
            for hour_of_day in [0, 1, 11, 12, 13, 23] {
                let t = t - t.rem_euclid(86_400) + hour_of_day * 3600 + 59;
                let forms = [0, STATUS_B_BINARY, STATUS_B_24_HOUR, STATUS_B_24_HOUR | STATUS_B_BINARY];
                for status_b in forms {
                    for century in [false, true] {
                        let (r, _) = encode(t, status_b, century).unwrap();
                        let what = format!("{t} status B {status_b:#x} century {century}");
                        assert_eq!(decode(r, status_b), Some(t), "{what}");
                    }
                }
            }
        }
    }

    /// **A year the chip cannot hold is not written**: without a century register it holds
    /// 2000–2099, since that is how two digits read; with one, the centuries `decode` believes.
    #[test]
    fn a_year_the_chip_cannot_hold_is_refused() {
        let b = STATUS_B_24_HOUR;
        assert!(encode(946_684_799, b, false).is_none()); // 1999-12-31 23:59:59
        assert!(encode(4_102_444_800, b, false).is_none()); // 2100-01-01
        assert!(encode(946_684_799, b, true).unwrap().0.century == Some(0x19));
        assert!(encode(4_102_444_800, b, true).unwrap().0.century == Some(0x21));
        // 2200 is past what `decode` believes of a century register.
        assert!(encode(7_258_118_400, b, true).is_none());
    }

    /// `civil_from_days` is `days_from_civil`'s inverse, day by day over four centuries around the
    /// epoch — every leap rule, both ways.
    #[test]
    fn civil_from_days_inverts_days_from_civil() {
        for days in -200 * 365..300 * 365 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(47_541), (2100, 3, 1));
    }
}
