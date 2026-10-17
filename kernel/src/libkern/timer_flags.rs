//! ABI-level value type for `sys_timer_create`.
//!
//! [`TimerFlags`] is the `flags` argument to `sys_timer_create`; like
//! [`MemFlags`](crate::libkern::memory) it is a hand-rolled bitflag (no `bitflags` crate, per
//! `kernel/CLAUDE.md`).
//!
//! No flags are defined yet; the type reserves the ABI slot so future flags can be added
//! without changing the syscall signature. The syscall layer parses with
//! [`TimerFlags::from_bits`] and rejects any unknown bit, so a program built against a newer
//! ABI gets a clean `InvalidArgument` rather than silently-ignored semantics.

/// Flags for `sys_timer_create`. `#[repr(transparent)]` boundary type.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct TimerFlags(u64);

impl TimerFlags {
    /// Every currently-defined flag bit. None yet.
    const KNOWN_BITS: u64 = 0;

    /// The empty set (no flags).
    pub const fn empty() -> Self {
        TimerFlags(0)
    }

    /// Parse a raw bit pattern, **rejecting** any bit outside [`KNOWN_BITS`](Self::KNOWN_BITS):
    /// `None` lets the syscall layer map it to `InvalidArgument`.
    pub const fn from_bits(bits: u64) -> Option<Self> {
        if bits & !Self::KNOWN_BITS != 0 { None } else { Some(TimerFlags(bits)) }
    }

    /// Parse a raw bit pattern, silently dropping unknown bits.
    pub const fn from_bits_truncate(bits: u64) -> Self {
        TimerFlags(bits & Self::KNOWN_BITS)
    }

    /// The raw bit pattern.
    pub const fn bits(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_bits() {
        assert_eq!(TimerFlags::empty().bits(), 0);
        assert_eq!(TimerFlags::default(), TimerFlags::empty());
    }

    #[test]
    fn from_bits_accepts_zero_rejects_unknown() {
        assert_eq!(TimerFlags::from_bits(0), Some(TimerFlags::empty()));
        assert_eq!(TimerFlags::from_bits(1), None);
        assert_eq!(TimerFlags::from_bits(0x8000_0000_0000_0000), None);
    }

    #[test]
    fn from_bits_truncate_drops_unknown() {
        assert_eq!(TimerFlags::from_bits_truncate(0xFFFF).bits(), 0);
    }
}
