//! [`SysCaps`] — the userspace mirror of the kernel's process-capability bitmask.
//!
//! Ambient *per-process* authority, distinct from per-handle [`Rights`](crate::Rights).
//! Userspace constructs a `SysCaps` value into [`SpawnArgs`](crate::abi::SpawnArgs) to
//! grant a child a (necessarily ⊆-its-own) capability set. The bit positions are
//! normative and must match `kernel/src/libkern/syscaps.rs`. See
//! `docs/architecture/syscaps.md`.

use core::ops::{BitAnd, BitOr, BitOrAssign};

/// A process's ambient system-capability set.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct SysCaps(u64);

impl SysCaps {
    /// Load/unload a Tier-2 loadable kernel module.
    pub const LOAD_MODULE: SysCaps = SysCaps(1 << 0);
    /// Bind names into namespaces — construct namespaces at all (supervisor-only).
    pub const BIND_NAMESPACE: SysCaps = SysCaps(1 << 1);
    /// Map arbitrary physical memory (boot-path/recovery only).
    pub const PHYSICAL_MEMORY: SysCaps = SysCaps(1 << 2);
    /// Request the `RealTime` scheduling class.
    pub const REAL_TIME: SysCaps = SysCaps(1 << 3);
    /// Set the realtime-clock offset.
    pub const SYSTEM_CLOCK: SysCaps = SysCaps(1 << 4);
    /// Manage the audit subsystem.
    pub const AUDIT_CONTROL: SysCaps = SysCaps(1 << 5);

    const ALL_BITS: u64 = (1 << 6) - 1;

    /// The empty set (no authority).
    pub const fn empty() -> Self {
        SysCaps(0)
    }

    /// The full set (every defined capability).
    pub const fn all() -> Self {
        SysCaps(Self::ALL_BITS)
    }

    /// Wrap a raw bit pattern, dropping unknown bits.
    pub const fn from_bits_truncate(bits: u64) -> Self {
        SysCaps(bits & Self::ALL_BITS)
    }

    /// The raw bit pattern (for the `SpawnArgs` crossing).
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// `true` if every capability in `other` is also held by `self`.
    pub const fn contains(self, other: SysCaps) -> bool {
        (self.0 & other.0) == other.0
    }

    /// `true` iff `self ⊆ other`.
    pub const fn is_subset_of(self, other: SysCaps) -> bool {
        (self.0 & other.0) == self.0
    }
}

/// `u64` alias for raw call sites (mirrors the `RIGHT_*` aliases).
pub const SYSCAP_BIND_NAMESPACE: u64 = SysCaps::BIND_NAMESPACE.bits();
/// `u64` alias for `REAL_TIME`.
pub const SYSCAP_REAL_TIME: u64 = SysCaps::REAL_TIME.bits();

impl BitOr for SysCaps {
    type Output = SysCaps;
    fn bitor(self, rhs: SysCaps) -> SysCaps {
        SysCaps(self.0 | rhs.0)
    }
}

impl BitAnd for SysCaps {
    type Output = SysCaps;
    fn bitand(self, rhs: SysCaps) -> SysCaps {
        SysCaps(self.0 & rhs.0)
    }
}

impl BitOrAssign for SysCaps {
    fn bitor_assign(&mut self, rhs: SysCaps) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_values_match_the_kernel() {
        // Pinned to kernel/src/libkern/syscaps.rs.
        assert_eq!(SysCaps::LOAD_MODULE.bits(), 1 << 0);
        assert_eq!(SysCaps::BIND_NAMESPACE.bits(), 1 << 1);
        assert_eq!(SysCaps::PHYSICAL_MEMORY.bits(), 1 << 2);
        assert_eq!(SysCaps::REAL_TIME.bits(), 1 << 3);
        assert_eq!(SysCaps::SYSTEM_CLOCK.bits(), 1 << 4);
        assert_eq!(SysCaps::AUDIT_CONTROL.bits(), 1 << 5);
        assert_eq!(SysCaps::all().bits(), 0b11_1111);
    }
}
