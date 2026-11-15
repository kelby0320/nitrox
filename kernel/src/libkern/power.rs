//! The power node's operations, the one byte a write to it carries — the kernel's copy.
//! `userspace/libkern` carries the matching one, and `cargo xtask abi-sync-check` pairs the two
//! by name. Not a version-hash input: a payload, as [`ClockId`](super::ClockId) is an argument.

/// Flush every disk, stop every processor, and say it is safe to turn the machine off.
pub const POWER_HALT: u64 = 0;
/// Flush every disk, stop every processor, and reset the machine.
pub const POWER_REBOOT: u64 = 1;

/// What a write to the power node asks for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PowerOp {
    /// [`POWER_HALT`].
    Halt,
    /// [`POWER_REBOOT`].
    Reboot,
}

impl PowerOp {
    /// Decode a write's byte, or `None` for a value that names no operation.
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
