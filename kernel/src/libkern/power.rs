//! `sys_power`'s operations (administration Part E.3) — the kernel's copy. `userspace/libkern`
//! carries the matching one, and `cargo xtask abi-sync-check` pairs the two by name. Not a
//! version-hash input: a syscall argument, like [`ClockId`](super::ClockId).

/// Flush every disk, stop every processor, and say it is safe to turn the machine off.
pub const POWER_HALT: u64 = 0;
/// Flush every disk, stop every processor, and reset the machine.
pub const POWER_REBOOT: u64 = 1;

/// What `sys_power` was asked to do.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PowerOp {
    /// [`POWER_HALT`].
    Halt,
    /// [`POWER_REBOOT`].
    Reboot,
}

impl PowerOp {
    /// Decode `sys_power`'s argument, or `None` for a value that names no operation.
    pub const fn from_u64(v: u64) -> Option<Self> {
        match v {
            POWER_HALT => Some(Self::Halt),
            POWER_REBOOT => Some(Self::Reboot),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_op_decodes_and_nothing_else_does() {
        assert_eq!(PowerOp::from_u64(POWER_HALT), Some(PowerOp::Halt));
        assert_eq!(PowerOp::from_u64(POWER_REBOOT), Some(PowerOp::Reboot));
        assert_eq!(PowerOp::from_u64(2), None);
        assert_eq!(PowerOp::from_u64(u64::MAX), None);
    }
}
