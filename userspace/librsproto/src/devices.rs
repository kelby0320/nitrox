//! `Devices` (`op = 0x0Fxx`) — the device manager, served at `/svc/devices`. See
//! `docs/spec/rsproto-devices-ops.md` and `docs/planning/administration.md` § Part B.
//!
//! **A class owner subscribes by resolving `/svc/devices/<class>`.** The resolve's answer is a
//! channel, and the manager sends on it, unsolicited: an [`OP_DEVICES_ARRIVED`] for every present
//! device of the class, each carrying the device's handle, then one [`OP_DEVICES_SETTLED`]. That
//! replay is coldplug. Later arrivals — Phase 6's — and [`OP_DEVICES_DEPARTED`] follow on the same
//! channel. **A class has one owner at a time**: a second resolve is refused with `AlreadyExists`
//! while the first channel is open, because a raw device has one reader in the kernel and a second
//! would drain the first one's events.
//!
//! The information side — `/svc/devices/info`, a directory of TSM1 tables — speaks the ordinary
//! `File` and `Namespace` operations, not these.
//!
//! Bodies are little-endian and byte-serialised into a caller buffer, like every category here.

use crate::{get_u32, put_u32};

/// Manager → owner, unsolicited, `request_id` 0: a device of the owner's class is present. Body:
/// the device's registry record, [`RECORD_LEN`] bytes, as `/dev/registry` serves it
/// (`libkern::device::DeviceRecord`). `handles[0]`: the device node — the owner's own duplicate.
pub const OP_DEVICES_ARRIVED: u16 = 0x0F00;
/// Manager → owner, unsolicited: every device present when the owner subscribed has arrived.
/// Body [`build_settled`]: how many did. An owner that serves only once it has its devices waits
/// for this, not for a count it cannot know.
pub const OP_DEVICES_SETTLED: u16 = 0x0F01;
/// Manager → owner, unsolicited: a device has gone. Body [`build_departed`]: its registry id.
/// **Nothing sends one until Phase 6** gives the kernel an event source; it is specified now so an
/// owner is written against it from the start.
pub const OP_DEVICES_DEPARTED: u16 = 0x0F02;

/// Bytes of a registry record — `libkern::device::DeviceRecord`'s size, which both sides assert.
pub const RECORD_LEN: usize = 144;
/// Bytes of a `Settled` or `Departed` body.
pub const WORD_LEN: usize = 4;

/// Build an `Arrived` body: the record's bytes. `None` if `record` is not [`RECORD_LEN`] bytes or
/// `out` is too small.
pub fn build_arrived(out: &mut [u8], record: &[u8]) -> Option<usize> {
    if record.len() != RECORD_LEN || out.len() < RECORD_LEN {
        return None;
    }
    out[..RECORD_LEN].copy_from_slice(record);
    Some(RECORD_LEN)
}

/// The record an `Arrived` body carries. `None` unless the body is exactly [`RECORD_LEN`] bytes.
pub fn parse_arrived(body: &[u8]) -> Option<&[u8]> {
    (body.len() == RECORD_LEN).then_some(body)
}

/// Build a `Settled` body: how many devices the replay sent.
pub fn build_settled(out: &mut [u8], count: u32) -> Option<usize> {
    build_word(out, count)
}

/// How many devices a `Settled` says arrived. `None` unless the body is exactly [`WORD_LEN`].
pub fn parse_settled(body: &[u8]) -> Option<u32> {
    parse_word(body)
}

/// Build a `Departed` body: the departed device's registry id.
pub fn build_departed(out: &mut [u8], id: u32) -> Option<usize> {
    build_word(out, id)
}

/// The id a `Departed` names. `None` unless the body is exactly [`WORD_LEN`].
pub fn parse_departed(body: &[u8]) -> Option<u32> {
    parse_word(body)
}

fn build_word(out: &mut [u8], v: u32) -> Option<usize> {
    if out.len() < WORD_LEN {
        return None;
    }
    put_u32(out, 0, v);
    Some(WORD_LEN)
}

fn parse_word(body: &[u8]) -> Option<u32> {
    (body.len() == WORD_LEN).then(|| get_u32(body, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_arrival_carries_exactly_one_record() {
        let record = [7u8; RECORD_LEN];
        let mut out = [0u8; 256];
        let n = build_arrived(&mut out, &record).unwrap();
        assert_eq!(parse_arrived(&out[..n]), Some(&record[..]));
        // What a correct manager never sends: a record cut short, or with bytes after it.
        assert_eq!(parse_arrived(&out[..n - 1]), None);
        assert_eq!(parse_arrived(&out[..n + 1]), None);
        assert_eq!(build_arrived(&mut out, &record[..RECORD_LEN - 1]), None, "not a record");
        assert_eq!(build_arrived(&mut out[..RECORD_LEN - 1], &record), None, "no room");
    }

    #[test]
    fn settled_and_departed_are_one_word() {
        let mut out = [0u8; 8];
        let n = build_settled(&mut out, 3).unwrap();
        assert_eq!(parse_settled(&out[..n]), Some(3));
        let n = build_departed(&mut out, 9).unwrap();
        assert_eq!(parse_departed(&out[..n]), Some(9));
        assert_eq!(parse_settled(&[0, 0, 0]), None);
        assert_eq!(parse_departed(&[0, 0, 0, 0, 0]), None);
    }
}
