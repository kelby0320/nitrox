//! **When a resolve of the table probes the devices nothing has mounted again**: a probe cannot be
//! cut short, so at most once an interval or a change of mounts (`storage.md` § 9).

use alloc::vec::Vec;

use crate::table::{self, Device};

/// The least time from the end of one probe to the next.
pub const INTERVAL_NS: u64 = 2_000_000_000;

/// When the last probe ended, and what was mounted then.
#[derive(Default)]
pub struct Rescan {
    last: Option<(u64, Vec<u32>)>,
}

impl Rescan {
    /// Whether a resolve of table file `name` at `now`, with `mounted` mounted, probes first.
    pub fn due(&self, devices: &[Device], name: &str, now: u64, mounted: &[u32]) -> bool {
        let names = name == "all" || devices.iter().any(|d| table::name(&d.record) == name);
        let stale = |(ended, then): &(u64, Vec<u32>)| then != mounted || now.saturating_sub(*ended) >= INTERVAL_NS;
        names && self.last.as_ref().is_none_or(stale)
    }

    /// A probe ended at `now`, with `mounted` the devices mounted.
    pub fn probed(&mut self, now: u64, mounted: Vec<u32>) {
        self.last = Some((now, mounted));
    }
}

#[cfg(test)]
mod tests;
