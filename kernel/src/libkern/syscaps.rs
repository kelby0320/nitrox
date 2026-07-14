//! [`SysCaps`] — the process-level capability bitmask.
//!
//! The **second axis of authority** (see `docs/architecture/syscaps.md`): ambient
//! *per-process* capabilities for privileged classes of operation, distinct from
//! per-handle [`Rights`](super::handle::Rights) (per-object authority). A process
//! holds a `SysCaps` set on its [`Process`](crate::object::Process); it is granted at
//! spawn (`child = parent & args.syscaps`, never amplified), immutable thereafter, and
//! checked at the syscall boundary (`require_syscap`).
//!
//! Hand-rolled bitflags in the [`Rights`](super::handle::Rights) style (the kernel
//! forbids the `bitflags` crate). The bit positions are normative and mirrored in
//! `userspace/libkern/src/syscaps.rs`; the set is the v5.1-committed six
//! (`docs/history/os-design-v5.1.md` § "System Capability Bitmask").

use core::ops::{BitAnd, BitOr, BitOrAssign};

/// A process's ambient system-capability set.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct SysCaps(u64);

impl SysCaps {
    /// Load/unload a Tier-2 loadable kernel module.
    pub const LOAD_MODULE: SysCaps = SysCaps(1 << 0);
    /// Bind names into namespaces (`sys_ns_bind`) — construct namespaces at all.
    /// Concentrated in supervisors (init, service-mgr, session-mgr); never held by an
    /// ordinary resource server.
    pub const BIND_NAMESPACE: SysCaps = SysCaps(1 << 1);
    /// Map arbitrary physical memory (boot-path/recovery only).
    pub const PHYSICAL_MEMORY: SysCaps = SysCaps(1 << 2);
    /// Request the `RealTime` scheduling class.
    pub const REAL_TIME: SysCaps = SysCaps(1 << 3);
    /// Set the realtime-clock offset (time-sync service).
    pub const SYSTEM_CLOCK: SysCaps = SysCaps(1 << 4);
    /// Manage the audit subsystem.
    pub const AUDIT_CONTROL: SysCaps = SysCaps(1 << 5);

    /// The bitmask of all defined capabilities — the set the kernel grants init at
    /// boot. Update alongside the constants above.
    const ALL_BITS: u64 = (1 << 6) - 1;

    /// The empty set (no authority).
    pub const fn empty() -> Self {
        SysCaps(0)
    }

    /// The full set (every defined capability) — init's boot grant.
    pub const fn all() -> Self {
        SysCaps(Self::ALL_BITS)
    }

    /// Wrap a raw bit pattern, **dropping unknown bits**. A `SpawnArgs.syscaps` word
    /// from userspace may set reserved bits; they carry no authority (nothing matches
    /// them) and the ⊆-parent intersection at spawn bounds what takes effect anyway.
    pub const fn from_bits_truncate(bits: u64) -> Self {
        SysCaps(bits & Self::ALL_BITS)
    }

    /// The raw bit pattern, for the `SpawnArgs` ABI crossing and tests.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// `true` if every capability in `other` is also held by `self`.
    pub const fn contains(self, other: SysCaps) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Bitwise union.
    pub const fn union(self, other: SysCaps) -> Self {
        SysCaps(self.0 | other.0)
    }

    /// Bitwise intersection — the spawn attenuation operator
    /// (`child = parent & args.syscaps`).
    pub const fn intersection(self, other: SysCaps) -> Self {
        SysCaps(self.0 & other.0)
    }

    /// `true` iff `self ⊆ other`.
    pub const fn is_subset_of(self, other: SysCaps) -> bool {
        (self.0 & other.0) == self.0
    }
}

impl BitOr for SysCaps {
    type Output = SysCaps;
    fn bitor(self, rhs: SysCaps) -> SysCaps {
        self.union(rhs)
    }
}

impl BitAnd for SysCaps {
    type Output = SysCaps;
    fn bitand(self, rhs: SysCaps) -> SysCaps {
        self.intersection(rhs)
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
    fn empty_and_all() {
        assert_eq!(SysCaps::empty().bits(), 0);
        assert_eq!(SysCaps::all().bits(), 0b11_1111); // six caps
        assert!(SysCaps::all().contains(SysCaps::BIND_NAMESPACE));
        assert!(!SysCaps::empty().contains(SysCaps::BIND_NAMESPACE));
    }

    #[test]
    fn distinct_bits() {
        let caps = [
            SysCaps::LOAD_MODULE,
            SysCaps::BIND_NAMESPACE,
            SysCaps::PHYSICAL_MEMORY,
            SysCaps::REAL_TIME,
            SysCaps::SYSTEM_CLOCK,
            SysCaps::AUDIT_CONTROL,
        ];
        // all six are subsets of `all`, pairwise distinct, and non-empty.
        let mut seen = 0u64;
        for c in caps {
            assert!(c.is_subset_of(SysCaps::all()));
            assert_ne!(c.bits(), 0);
            assert_eq!(seen & c.bits(), 0, "overlapping bit");
            seen |= c.bits();
        }
        assert_eq!(seen, SysCaps::all().bits());
    }

    #[test]
    fn intersection_is_spawn_attenuation() {
        // A parent without REAL_TIME cannot grant it: child = parent & requested.
        let parent = SysCaps::BIND_NAMESPACE | SysCaps::LOAD_MODULE;
        let requested = SysCaps::BIND_NAMESPACE | SysCaps::REAL_TIME;
        let child = parent & requested;
        assert!(child.contains(SysCaps::BIND_NAMESPACE));
        assert!(!child.contains(SysCaps::REAL_TIME), "cannot amplify beyond parent");
        assert!(child.is_subset_of(parent));
    }

    #[test]
    fn from_bits_truncate_drops_unknown() {
        // A reserved/high bit carries no authority — it truncates away.
        let c = SysCaps::from_bits_truncate(0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(c, SysCaps::all());
        assert_eq!(SysCaps::from_bits_truncate(1 << 40).bits(), 0);
    }
}
