//! ABI value type for `sys_clock_read`: the clock selector.
//!
//! [`ClockId`] is the `clock` argument to `sys_clock_read`. It is a boundary
//! type the kernel and userspace must agree on; its discriminants are the wire
//! contract (`docs/spec/syscall-abi.md`) and must not change. It lives here
//! beside the other ABI value types ([`MemFlags`](crate::libkern::memory),
//! [`Rights`](crate::libkern::handle::Rights)).
//!
//! Only [`ClockId::Monotonic`] is serviced this slice; the others reserve their
//! ABI slots (see the `sys_clock_read` handler's TODOs — `Realtime` needs a
//! wall-clock offset service, and the per-CPU clocks need scheduler CPU
//! accounting, neither of which exists yet).

/// The clock selected by `sys_clock_read`. `#[repr(u32)]`; the discriminants
/// are the stable ABI contract.
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ClockId {
    /// Nanoseconds since boot; never decreases. The only clock serviced now.
    Monotonic = 0,
    /// Wall-clock (Unix-epoch) nanoseconds. Needs a wall-clock offset service.
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
