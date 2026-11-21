//! **When a resolve of the table probes the devices nothing has mounted again**: a probe cannot be
//! cut short, so it waits out an interval, or a change of mounts (`storage.md` § 9).

use alloc::vec::Vec;

use crate::table::{self, Device};

/// The least time from the end of one probe to the next.
pub const INTERVAL_NS: u64 = 2_000_000_000;

/// How many times its own length a probe waits: probes hold a fifth of the thread at most.
pub const SPACING: u64 = 4;

/// Whether `name` is a table file: `all`, or a device's.
pub fn names(devices: &[Device], name: &str) -> bool {
    name == "all" || devices.iter().any(|d| table::name(&d.record) == name)
}

/// When the last probe ended, how long it took, and what was mounted then.
#[derive(Default)]
pub struct Rescan {
    last: Option<(u64, u64, Vec<u32>)>,
}

impl Rescan {
    /// **Run `probe` if a resolve of a table file, `named` or not, is due**, `mounted` mounted:
    /// whether it probed. A clock that cannot be read probes, and records nothing.
    pub fn run(
        &mut self,
        named: bool,
        mounted: Vec<u32>,
        clock: impl Fn() -> Option<u64>,
        probe: impl FnOnce(),
    ) -> bool {
        let started = clock();
        let waited = |now: u64, (ended, took, then): &(u64, u64, Vec<u32>)| {
            *then != mounted || now.saturating_sub(*ended) >= INTERVAL_NS.max(took.saturating_mul(SPACING))
        };
        if !named || started.is_some_and(|now| self.last.as_ref().is_some_and(|l| !waited(now, l))) {
            return false;
        }
        probe();
        self.last = started.zip(clock()).map(|(s, e)| (e, e.saturating_sub(s), mounted));
        true
    }
}

#[cfg(test)]
mod tests;
