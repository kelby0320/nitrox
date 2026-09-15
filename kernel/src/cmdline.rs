//! The kernel command line: the boot entry's `cmdline:`, read once and parsed into flags
//! (Phase 5 Part D.2).
//!
//! **A command line never stops a boot.** A word this kernel does not know is reported and
//! ignored, and so is a value it cannot read: the line is typed at a boot menu, on a machine
//! that may have nothing else to say what went wrong, and a typo there should cost the flag,
//! not the boot.
//!
//! One flag exists: `hwreport[=<seconds>]`, which holds the kernel log on the screen before
//! userspace starts, a page at a time (see `main.rs`'s report mode). The seconds bound how long
//! a page waits for a key.

/// How long a hardware-report page waits for a key when `hwreport` gives no bound.
pub const HWREPORT_DEFAULT_SECS: u32 = 120;

/// What the command line asked for.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// `Some(seconds)` when a hardware report was requested: how long a page waits for a key
    /// before the report ends.
    pub hwreport: Option<u32>,
}

/// Something on the command line that was ignored, and why.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Ignored<'a> {
    /// A word naming no flag.
    Unknown(&'a [u8]),
    /// A known flag whose value could not be read; the flag took its default instead.
    BadValue(&'a [u8]),
}

/// Parse `line` into [`Flags`], reporting each word it ignores to `ignored`. Words are
/// separated by ASCII whitespace; a later occurrence of a flag replaces an earlier one.
pub fn parse<'a>(line: &'a [u8], mut ignored: impl FnMut(Ignored<'a>)) -> Flags {
    let mut flags = Flags::default();
    for word in line.split(|b| b.is_ascii_whitespace()).filter(|w| !w.is_empty()) {
        let (name, value) = match word.iter().position(|&b| b == b'=') {
            Some(eq) => (&word[..eq], Some(&word[eq + 1..])),
            None => (word, None),
        };
        match name {
            b"hwreport" => {
                flags.hwreport = Some(match value {
                    None => HWREPORT_DEFAULT_SECS,
                    Some(v) => match seconds(v) {
                        Some(s) => s,
                        None => {
                            ignored(Ignored::BadValue(word));
                            HWREPORT_DEFAULT_SECS
                        }
                    },
                });
            }
            _ => ignored(Ignored::Unknown(word)),
        }
    }
    flags
}

/// A whole number of seconds, in decimal digits only, that fits a `u32`.
fn seconds(v: &[u8]) -> Option<u32> {
    if v.is_empty() {
        return None;
    }
    let mut n: u32 = 0;
    for &b in v {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &[u8]) -> (Flags, Vec<Ignored<'_>>) {
        let mut ignored = Vec::new();
        let flags = parse(line, |i| ignored.push(i));
        (flags, ignored)
    }

    #[test]
    fn an_empty_line_asks_for_nothing() {
        assert_eq!(parsed(b""), (Flags { hwreport: None }, vec![]));
        assert_eq!(parsed(b"  \t "), (Flags { hwreport: None }, vec![]));
    }

    #[test]
    fn a_bare_hwreport_takes_the_default_bound_and_a_value_replaces_it() {
        assert_eq!(parsed(b"hwreport").0.hwreport, Some(HWREPORT_DEFAULT_SECS));
        assert_eq!(parsed(b"hwreport=5").0.hwreport, Some(5));
        assert_eq!(parsed(b" hwreport=0 ").0.hwreport, Some(0));
    }

    #[test]
    fn an_unknown_word_is_reported_and_does_not_stop_the_rest() {
        let (flags, ignored) = parsed(b"quiet hwreport=7 splash");
        assert_eq!(flags.hwreport, Some(7));
        assert_eq!(ignored, vec![Ignored::Unknown(b"quiet"), Ignored::Unknown(b"splash")]);
    }

    #[test]
    fn an_unreadable_value_is_reported_and_the_flag_keeps_its_default() {
        for bad in [&b"hwreport="[..], b"hwreport=abc", b"hwreport=-5", b"hwreport=99999999999"] {
            let (flags, ignored) = parsed(bad);
            assert_eq!(flags.hwreport, Some(HWREPORT_DEFAULT_SECS), "{:?}", bad);
            assert_eq!(ignored, vec![Ignored::BadValue(bad)]);
        }
    }

    #[test]
    fn a_word_that_only_starts_like_the_flag_is_not_the_flag() {
        let (flags, ignored) = parsed(b"hwreports hwreport_x");
        assert_eq!(flags.hwreport, None);
        assert_eq!(ignored.len(), 2);
    }
}
