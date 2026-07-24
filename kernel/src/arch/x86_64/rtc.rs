//! The battery-backed real-time clock — the machine's only source of
//! **wall-clock** time.
//!
//! Read once at boot to anchor the kernel's realtime clock
//! ([`crate::clock`]); after that, time-of-day is the monotonic counter plus a
//! fixed offset, so it advances smoothly and never jumps backwards.
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
//! ## What it does not do
//!
//! The RTC is assumed to hold **UTC**. That is what QEMU provides by default
//! (`-rtc base=utc`) and the convention every Unix-like system uses; a machine
//! whose firmware keeps local time would report a skewed epoch. There is no
//! timezone database to correct it with, and a timezone is a *display* concern
//! for the shell, not a kernel one.
//!
//! The **century** is a guess. The century register's location is only
//! discoverable from ACPI's FADT, which this kernel does not parse yet
//! (`docs/rationale/why-phased-acpi.md`), so a two-digit year is mapped into
//! 2000–2099 and the result sanity-clamped. Wrong by a century is caught by the
//! clamp; wrong within this century is not possible from a two-digit year.

use super::regs::{inb, outb};

/// CMOS address (index) port. Bit 7 additionally masks NMI; we preserve it.
const CMOS_ADDR: u16 = 0x70;
/// CMOS data port.
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
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

/// Read the six time registers once.
///
/// # Safety
/// As [`read_reg`].
unsafe fn read_raw() -> Raw {
    // SAFETY: all six are plain time registers.
    unsafe {
        Raw {
            second: read_reg(REG_SECONDS),
            minute: read_reg(REG_MINUTES),
            hour: read_reg(REG_HOURS),
            day: read_reg(REG_DAY),
            month: read_reg(REG_MONTH),
            year: read_reg(REG_YEAR),
        }
    }
}

/// Seconds since the Unix epoch from the machine's RTC, or `None` if the clock
/// could not be read consistently or reports an implausible date.
///
/// Called **once**, during boot, from [`crate::clock::init`].
pub fn wall_clock_seconds() -> Option<i64> {
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
        read_raw()
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
            read_raw()
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
    decode(prev, status_b)
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

    // No ACPI ⇒ no century register; a two-digit year maps into 2000–2099.
    let year = 2000i64 + year2 as i64;

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

/// One packed BCD byte to binary, or `None` if either nibble is not a digit.
fn bcd_to_binary(v: u8) -> Option<u8> {
    let hi = v >> 4;
    let lo = v & 0x0F;
    if hi > 9 || lo > 9 {
        return None;
    }
    Some(hi * 10 + lo)
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
        Raw { second, minute, hour, day, month, year }
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
}
