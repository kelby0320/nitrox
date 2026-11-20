use super::*;
use crate::probe::Found;
use libkern::device::{DeviceKind, DeviceRecord};

fn devices() -> Vec<Device> {
    let disk = |served: u32| {
        let mut r = DeviceRecord::read(&[0u8; size_of::<DeviceRecord>()]).unwrap();
        (r.id, r.kind, r.served) = (10 + served, DeviceKind::Disk.as_u32(), served);
        Device { record: r, found: Found::Nothing }
    };
    std::vec![disk(0), disk(1)]
}

/// **A resolve probes at most once an interval from the last probe's end**, over a moving clock.
#[test]
fn a_probe_waits_out_the_interval_from_the_last_ones_end() {
    let (ds, mut r) = (devices(), Rescan::default());
    let due = |r: &Rescan, now: u64| r.due(&ds, "all", now, &[10]);
    assert!(due(&r, 5), "the first");
    r.probed(1_000, std::vec![10]);
    for now in [1_000, 1_001, 1_000 + INTERVAL_NS - 1] {
        assert!(!due(&r, now), "{now}: within the interval");
    }
    assert!(due(&r, 1_000 + INTERVAL_NS), "its end");
    assert!(due(&r, u64::MAX), "long after");
    assert!(!due(&r, 0), "a clock behind the last probe waits for it");
}

/// **A change to what is mounted probes at once, and a name that names no device never does.**
#[test]
fn a_change_of_mounts_probes_and_an_unknown_name_never_does() {
    let (ds, mut r) = (devices(), Rescan::default());
    r.probed(1_000, std::vec![10]);
    assert!(r.due(&ds, "all", 1_001, &[]), "unmounted since");
    assert!(r.due(&ds, "all", 1_001, &[10, 11]), "mounted since");
    assert!(r.due(&ds, "blk-1", 1_000 + INTERVAL_NS, &[10]), "a device's own table");
    assert!(!r.due(&ds, "blk-1", 1_001, &[10]), "and within the interval, not");
    let fresh = Rescan::default();
    for name in ["blk-2", "", "all.tsm", "ALL"] {
        assert!(!fresh.due(&ds, name, 1_001, &[]), "{name:?}");
    }
}
