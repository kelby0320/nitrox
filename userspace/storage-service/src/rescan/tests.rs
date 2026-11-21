use super::*;
use crate::probe::Found;
use core::cell::Cell;
use libkern::device::{DeviceKind, DeviceRecord};

const S: u64 = 1_000_000_000;

fn devices() -> Vec<Device> {
    let disk = |served: u32| {
        let mut r = DeviceRecord::read(&[0u8; size_of::<DeviceRecord>()]).unwrap();
        (r.id, r.kind, r.served) = (10 + served, DeviceKind::Disk.as_u32(), served);
        Device { record: r, found: Found::Nothing }
    };
    std::vec![disk(0), disk(1)]
}

/// A resolve at `at` with `mounted` mounted, whose probe takes `took`: whether it probed.
fn at(r: &mut Rescan, at: u64, took: u64, mounted: &[u32]) -> bool {
    let now = Cell::new(at);
    r.run(true, mounted.to_vec(), || Some(now.get()), || now.set(at + took))
}

/// **A resolve probes at most once an interval from the last probe's end**, over a moving clock.
#[test]
fn a_probe_waits_out_the_interval_from_the_last_ones_end() {
    let mut r = Rescan::default();
    assert!(at(&mut r, 5, 0, &[10]), "the first");
    for now in [5, 6, 5 + INTERVAL_NS - 1] {
        assert!(!at(&mut r, now, 0, &[10]), "{now}: within the interval");
    }
    assert!(at(&mut r, 5 + INTERVAL_NS, 0, &[10]), "its end");
    assert!(!at(&mut r, 0, 0, &[10]), "a clock behind the last probe waits for it");
}

/// **A slow probe waits four times its length**, so probes hold a fifth of the thread at most.
#[test]
fn a_slow_probe_waits_four_times_its_length() {
    let mut r = Rescan::default();
    assert!(at(&mut r, 0, 3 * S, &[10]), "a three-second probe, ending at 3 s");
    assert!(!at(&mut r, 3 * S + 12 * S - 1, 0, &[10]), "within four times its length");
    assert!(at(&mut r, 15 * S, 0, &[10]), "after it");
}

/// **A change to what is mounted probes at once, and a name that names no device never does.**
#[test]
fn a_change_of_mounts_probes_and_an_unknown_name_never_does() {
    let mut r = Rescan::default();
    assert!(at(&mut r, 1_000, 0, &[10]));
    assert!(at(&mut r, 1_001, 0, &[]), "unmounted since");
    assert!(at(&mut r, 1_002, 0, &[10, 11]), "mounted since");
    assert!(!at(&mut r, 1_003, 0, &[10, 11]), "and within the interval, not");
    let ds = devices();
    assert!(names(&ds, "all") && names(&ds, "blk-1"));
    for name in ["blk-2", "", "all.tsm", "ALL"] {
        assert!(!names(&ds, name), "{name:?}");
    }
    let probed = Cell::new(false);
    assert!(!Rescan::default().run(false, std::vec![], || Some(1), || probed.set(true)) && !probed.get());
}

/// **A clock that cannot be read probes, and records nothing**: a record of it stopped them all.
#[test]
fn a_clock_that_cannot_be_read_probes_and_records_nothing() {
    let mut r = Rescan::default();
    for _ in 0..2 {
        assert!(r.run(true, std::vec![10], || None, || ()), "no clock");
    }
    let reads = Cell::new(0);
    let fails_last = || {
        reads.set(reads.get() + 1);
        (reads.get() == 1).then_some(S)
    };
    assert!(r.run(true, std::vec![10], fails_last, || ()), "a clock read before the probe");
    assert!(at(&mut r, S + 1, 0, &[10]), "and none after it: nothing recorded, so due again");
}
