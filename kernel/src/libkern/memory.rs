//! ABI-level value types for the memory-object syscalls.
//!
//! [`MemFlags`] is the `flags` argument to `sys_memory_create`. It is a
//! boundary type the kernel and userspace must agree on; it lives here (not in
//! [`handle`](crate::libkern::handle)) because it is a distinct ABI surface
//! from the handle-system value types. Hand-rolled bitflags in the
//! [`Rights`](crate::libkern::handle::Rights) style — no `bitflags` crate, per
//! `kernel/CLAUDE.md`.
//!
//! No flags are defined yet; the type reserves the ABI slot so future flags
//! (cacheability, contiguity, …) can be added without changing the syscall
//! signature. The syscall layer parses with [`MemFlags::from_bits`] and
//! rejects any unknown bit, so a program built against a newer ABI that sets a
//! flag this kernel does not understand gets a clean `InvalidArgument` rather
//! than silently-ignored semantics.

/// Flags for `sys_memory_create`. `#[repr(transparent)]` boundary type.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct MemFlags(u64);

impl MemFlags {
    /// Every currently-defined flag bit. Empty in Phase 1.
    const KNOWN_BITS: u64 = 0;

    /// The empty set (no flags).
    pub const fn empty() -> Self {
        MemFlags(0)
    }

    /// Parse a raw bit pattern, **rejecting** any bit outside
    /// [`KNOWN_BITS`](Self::KNOWN_BITS). Returns `None` on an unknown bit so
    /// the syscall layer can map it to `InvalidArgument`.
    pub const fn from_bits(bits: u64) -> Option<Self> {
        if bits & !Self::KNOWN_BITS != 0 {
            None
        } else {
            Some(MemFlags(bits))
        }
    }

    /// Parse a raw bit pattern, silently dropping unknown bits. For callers
    /// that prefer lenient parsing; the syscall layer uses
    /// [`from_bits`](Self::from_bits) instead.
    pub const fn from_bits_truncate(bits: u64) -> Self {
        MemFlags(bits & Self::KNOWN_BITS)
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
        assert_eq!(MemFlags::empty().bits(), 0);
        assert_eq!(MemFlags::default(), MemFlags::empty());
    }

    #[test]
    fn from_bits_accepts_zero_rejects_unknown() {
        assert_eq!(MemFlags::from_bits(0), Some(MemFlags::empty()));
        // No flags are defined yet, so any set bit is unknown.
        assert_eq!(MemFlags::from_bits(1), None);
        assert_eq!(MemFlags::from_bits(0x8000_0000_0000_0000), None);
    }

    #[test]
    fn from_bits_truncate_drops_unknown() {
        assert_eq!(MemFlags::from_bits_truncate(0xFFFF).bits(), 0);
    }
}
