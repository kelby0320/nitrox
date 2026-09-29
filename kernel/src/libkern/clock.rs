//! ABI values for `sys_clock_read` and `sys_clock_set`: the clock selector, and
//! what a set says.
//!
//! [`ClockId`] is the `clock` argument to both. It is a boundary type the
//! kernel and userspace must agree on; its discriminants are the wire contract
//! (`docs/spec/syscall-abi.md`) and must not change. It lives here beside the
//! other ABI value types ([`MemFlags`](crate::libkern::memory),
//! [`Rights`](crate::libkern::handle::Rights)).
//!
//! `Monotonic` and `Realtime` are serviced — `Realtime` once the wall clock is
//! anchored, since 2026-07-24 — and `Realtime` alone can be set
//! (administration Part E.5). The per-CPU clocks reserve their slots until the
//! scheduler accounts CPU time (the `sys_clock_read` handler's TODO).

/// `sys_clock_set` succeeded, and the machine's hardware clock holds the time, so the next boot
/// anchors to it.
pub const CLOCK_SET_KEPT: u64 = 0;
/// `sys_clock_set` set the clock, but the hardware clock did not take the time — there is none,
/// or it did not read back — so it lasts until the next boot.
pub const CLOCK_SET_THIS_BOOT: u64 = 1;

/// The clock selected by `sys_clock_read`. `#[repr(u32)]`; the discriminants
/// are the stable ABI contract.
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ClockId {
    /// Nanoseconds since boot; never decreases, and cannot be set.
    Monotonic = 0,
    /// Wall-clock (Unix-epoch) nanoseconds, once anchored from the RTC or set. The one clock
    /// `sys_clock_set` sets, and so the one that can step.
    Realtime = 1,
    /// CPU time consumed by the calling process. Needs scheduler accounting.
    ProcessCpu = 2,
    /// CPU time consumed by the calling thread. Needs scheduler accounting.
    ThreadCpu = 3,
}

impl ClockId {
    /// Decode a raw `u32` selector. Returns `None` for an unknown value so the
    /// syscall layer can map it to `InvalidArgument`.
    pub const fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(ClockId::Monotonic),
            1 => Some(ClockId::Realtime),
            2 => Some(ClockId::ProcessCpu),
            3 => Some(ClockId::ThreadCpu),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_u32_round_trips_known_selectors() {
        assert_eq!(ClockId::from_u32(0), Some(ClockId::Monotonic));
        assert_eq!(ClockId::from_u32(1), Some(ClockId::Realtime));
        assert_eq!(ClockId::from_u32(2), Some(ClockId::ProcessCpu));
        assert_eq!(ClockId::from_u32(3), Some(ClockId::ThreadCpu));
    }

    #[test]
    fn from_u32_rejects_unknown_selectors() {
        assert_eq!(ClockId::from_u32(4), None);
        assert_eq!(ClockId::from_u32(u32::MAX), None);
    }

    #[test]
    fn discriminants_match_abi() {
        assert_eq!(ClockId::Monotonic as u32, 0);
        assert_eq!(ClockId::Realtime as u32, 1);
        assert_eq!(ClockId::ProcessCpu as u32, 2);
        assert_eq!(ClockId::ThreadCpu as u32, 3);
    }
}
