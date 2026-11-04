use super::*;
use crate::testing::{entries, lookup_only};
use libkern::KError;
use libkern::RawHandle;
use libkern::device::{DEPARTED, DeviceKind, DeviceRecord, REGISTRY_MAGIC, REGISTRY_VERSION};
use libos::{Handle, MapReadWrite, NsMutable, mock};
use std::string::String;
use std::vec::Vec;

type Ns = Handle<Namespace, NsMutable>;

const DEVICE: Rights = Rights::from_bits(
    Rights::READ.bits()
        | Rights::WRITE.bits()
        | Rights::DUPLICATE.bits()
        | Rights::INSPECT.bits()
        | Rights::TRANSFER.bits(),
);

fn record(id: u32, kind: DeviceKind, served: u32, flags: u8) -> DeviceRecord {
    let mut r = DeviceRecord::read(&[0u8; 144]).unwrap();
    (r.id, r.kind, r.served, r.flags) = (id, kind.as_u32(), served, flags);
    r
}

fn snapshot(records: &[DeviceRecord]) -> Vec<u8> {
    let mut out = std::vec![0u8; 24];
    out[0..4].copy_from_slice(&REGISTRY_MAGIC.to_le_bytes());
    out[4..8].copy_from_slice(&REGISTRY_VERSION.to_le_bytes());
    out[8..12].copy_from_slice(&(records.len() as u32).to_le_bytes());
    out[12..16].copy_from_slice(&144u32.to_le_bytes());
    for r in records {
        out.extend_from_slice(r.as_bytes());
    }
    out
}

fn memory(bytes: &[u8]) -> Handle<libos::Memory, MapReadWrite> {
    let mem = Handle::<libos::Memory, MapReadWrite>::create(4096).unwrap();
    mem.map(4096).unwrap().write_from(0, bytes).unwrap();
    mem
}

fn bind_device(ns: &Ns, path: &str) {
    let raw = mock::with_rights(mock::device(), DEVICE);
    // SAFETY: a handle the mock has just made, which nothing else holds.
    let dev = unsafe { Handle::<Resource, ReadWrite>::from_raw(RawHandle(raw), Rights::empty()) };
    ns.bind(path, &dev).unwrap();
}

/// A source whose registry lists four block devices (ids 10, 11, 13, 14 at `/dev/blk/0`, `1`, `2`
/// and `3`) and a keyboard, of which `0`, `1` and `3` are bound, `0` with its `info` leaf, and `3`
/// has departed.
fn source() -> Ns {
    let ns = Ns::create().unwrap();
    let records = [
        record(10, DeviceKind::Disk, 0, 0),
        record(11, DeviceKind::Partition, 1, 0),
        record(12, DeviceKind::Keyboard, 0, 0),
        record(13, DeviceKind::RamDisk, 2, 0),
        record(14, DeviceKind::Disk, 3, DEPARTED),
    ];
    ns.bind("/dev/registry", &memory(&snapshot(&records))).unwrap();
    for n in [0, 1, 3] {
        bind_device(&ns, &alloc::format!("/dev/blk/{n}"));
    }
    ns.bind("/dev/blk/0/info", &memory(b"info")).unwrap();
    ns
}

fn paths(es: &[(String, u64)]) -> Vec<&str> {
    es.iter().map(|(p, _)| p.as_str()).collect()
}

/// Each listed device is handed on with its snapshot, its rights as asked, a gap skipped, a departed
/// device left, and nothing left open.
#[test]
fn a_device_is_handed_on_with_its_info_and_its_rights() {
    mock::reset();
    let (from, to) = (source(), Ns::create().unwrap());
    let before = (mock::open_handles(), mock::mappings());
    assert_eq!(share_block_devices(lookup_only(&from), &to, &[]), 2);
    let es = entries(to.as_ref());
    assert_eq!(paths(&es), ["/dev/blk/0", "/dev/blk/0/info", "/dev/blk/1"]);
    assert_eq!(es[0].1, DEVICE.bits());
    assert_eq!(es[1].1, (Rights::MAP_READ | Rights::TRANSFER).bits());
    assert_eq!(es[2].1, DEVICE.bits());
    assert_eq!((mock::open_handles(), mock::mappings()), before, "the handles and the mapping are given back");
    assert!(mock::logged().ends_with(b"libsession: 2 block device(s) handed to a namespace\n"));
    assert_eq!(mock::stray_closes(), 0);
}

/// A device whose registry id is withheld is not handed on, whatever its place.
#[test]
fn a_withheld_device_is_not_handed_on() {
    mock::reset();
    let (from, to) = (source(), Ns::create().unwrap());
    assert_eq!(share_block_devices(lookup_only(&from), &to, &[10, 99]), 1);
    assert_eq!(paths(&entries(to.as_ref())), ["/dev/blk/1"]);
    let all_withheld = Ns::create().unwrap();
    assert_eq!(share_block_devices(lookup_only(&from), &all_withheld, &[10, 11]), 0);
    assert!(entries(all_withheld.as_ref()).is_empty());
}

/// A source with no registry, or one that is no snapshot, hands nothing on.
#[test]
fn a_source_with_no_registry_hands_nothing_on() {
    mock::reset();
    let to = Ns::create().unwrap();
    let none = Ns::create().unwrap();
    bind_device(&none, "/dev/blk/0");
    assert_eq!(share_block_devices(lookup_only(&none), &to, &[]), 0);
    let garbage = Ns::create().unwrap();
    garbage.bind("/dev/registry", &memory(b"not a registry")).unwrap();
    bind_device(&garbage, "/dev/blk/0");
    assert_eq!(share_block_devices(lookup_only(&garbage), &to, &[]), 0);
    assert!(entries(to.as_ref()).is_empty());
    assert!(!mock::logged().ends_with(b"handed to a namespace\n"));
}

/// A device that will not bind is not counted and its `info` is not tried; an `info` that will not
/// bind is reported and the device still counts.
#[test]
fn a_bind_that_fails_is_not_counted() {
    mock::reset();
    let (from, to) = (source(), Ns::create().unwrap());
    mock::fail_next_bind(KError::NoAccess as i32);
    assert_eq!(share_block_devices(lookup_only(&from), &to, &[]), 1);
    assert_eq!(paths(&entries(to.as_ref())), ["/dev/blk/1"]);
    let to = Ns::create().unwrap();
    to.bind("/dev/blk/0/info", &memory(b"taken")).unwrap();
    assert_eq!(share_block_devices(lookup_only(&from), &to, &[11]), 1);
    assert!(mock::logged().ends_with(
        b"libsession: a device's info leaf would not bind\nlibsession: 1 block device(s) handed to a namespace\n"
    ));
}

/// What is taken back is read from the namespace: its devices and their leaves, and no lookalike.
#[test]
fn what_was_handed_on_is_taken_back_and_nothing_else() {
    mock::reset();
    let (from, to) = (source(), Ns::create().unwrap());
    share_block_devices(lookup_only(&from), &to, &[]);
    let other = memory(b"x");
    for path in ["/dev/blk/05", "/dev/blk/9x", "/home", "/dev/blkx/1"] {
        to.bind(path, &other).unwrap();
    }
    assert_eq!(withdraw_block_devices(&to), 2);
    assert_eq!(paths(&entries(to.as_ref())), ["/dev/blk/05", "/dev/blk/9x", "/home", "/dev/blkx/1"]);
    assert_eq!(withdraw_block_devices(&to), 0, "nothing is left to take");
    // A device at an index the source's registry never listed is still found, being read from the
    // namespace: a fixed range would miss it.
    bind_device(&to, "/dev/blk/4000");
    assert_eq!(withdraw_block_devices(&to), 1);
}

/// A binding is a device only at exactly `/dev/blk/<n>`; lookalikes must not be (un)bound.
#[test]
fn a_device_binding_is_exactly_dev_blk_and_an_index() {
    assert_eq!(blk_binding_index(b"/dev/blk/0"), Some(0));
    assert_eq!(blk_binding_index(b"/dev/blk/12"), Some(12));
    assert_eq!(blk_binding_index(b"/dev/blk/4294967295"), Some(u32::MAX));
    for not in [
        &b"/dev/blk/0/info"[..],
        b"/dev/blk",
        b"/dev/blk/",
        b"/dev/blkx/0",
        b"/dev/blk/01",
        b"/dev/blk/1a",
        b"/dev/blk/-1",
        b"/dev/blk/4294967296",
        b"/home/dev/blk/0",
    ] {
        assert_eq!(blk_binding_index(not), None, "{:?}", core::str::from_utf8(not));
    }
}
