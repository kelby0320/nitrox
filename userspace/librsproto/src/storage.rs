//! `Storage` (`op = 0x10xx`) — mounting and unmounting, served by the storage service on an admin
//! session. See `docs/spec/rsproto-storage-ops.md`.
//!
//! **An admin session is a channel the service answers any resolve on its admin endpoint
//! with.** The endpoint is minted at `/svc/storage/admin-endpoint` and bound by the view broker
//! at `/dev/storage/admin` in a view with the `storage` grant, so the grant decides who
//! reaches it; the service answers every request on it.
//!
//! **A session has two channels of its own**, opened by resolving `info/media` and
//! `info/watch` on the session endpoint (`/dev/storage/media` and `/dev/storage/watch`): a
//! **media session**, carrying [`OP_STORAGE_EJECT`] and nothing that mounts; and a **watch**,
//! on which the service sends [`OP_STORAGE_CHANGED`] and nothing else.
//!
//! The filesystems and the table of what is on each disk are ordinary `Namespace` and `File`
//! operations under `/svc/storage/fs` and `/svc/storage/info`. A refusal is the standard
//! `ErrorBody`, and its reason says which step refused.

use crate::{get_u16, get_u32, put_u16, put_u32};

/// Client → service: **mount a device**. Body [`build_mount`]: the device's name as the tables
/// name it (`blk-<n>`), and a label, empty to let the service choose. **Always writable**: the
/// automatic mount of a live boot is the careful one, not an administrator's. Reply body: the label
/// it was mounted under, as bytes.
pub const OP_STORAGE_MOUNT: u16 = 0x1000;
/// Client → service: **unmount a filesystem**, by its label (body: the label's bytes). Reply
/// body: empty; `WouldBlock` if a file is still held (`rsproto-storage-ops.md` § `Unmount`).
pub const OP_STORAGE_UNMOUNT: u16 = 0x1001;
/// Client → service: **the devices in use**, which must not be granted raw: every mounted
/// filesystem's device, `init`'s included, and the disk that holds it. Body: empty. Reply body
/// [`build_in_use`]: their registry ids.
pub const OP_STORAGE_IN_USE: u16 = 0x1002;

/// Client → service, on a **media session**: **eject a stick**, by the name it is
/// mounted under (`/storage/<name>`, the table's `mounted` column). Body: the name's bytes.
/// Answered once it is safe to pull; **the drive goes whole**: every filesystem mounted on the
/// same disk is unmounted, or none is. Refused `NotFound`, `NoAccess` (an internal disk) or
/// `WouldBlock` (a file held). Reply body: their mount names, one per line ([`ejected_names`])
/// (`rsproto-storage-ops.md` § `Eject`).
pub const OP_STORAGE_EJECT: u16 = 0x1003;
/// Service → client, on a **watch**: **the set of mounts changed, or the set of devices** (a
/// partition a rescan published holding nothing changes no mount, and `disk --partition` waits
/// for its row); read the table again. Body: empty; nothing answers it. **A watch carries
/// nothing else**, so a full queue holds a ping already and a client reading one after many
/// changes has missed none.
pub const OP_STORAGE_CHANGED: u16 = 0x1004;
/// Client → service, on an admin session: **read a device again**, after `disk`
/// has written it, and mount what it holds as arrivals are mounted. Body: the device's name
/// (`blk-<n>`). A partition is probed again; a disk is first rescanned by the kernel. Refused
/// `NotFound`, `WouldBlock` (mounted), `NoAccess` (the boot disk) or `Unsupported` (a disk the
/// kernel cannot rescan). Reply body: the names it was mounted under, one per line, empty for
/// none (`rsproto-storage-ops.md` § `Reread`).
pub const OP_STORAGE_REREAD: u16 = 0x1005;

/// Whether a message on a watch is a ping: [`OP_STORAGE_CHANGED`] with no body; ignore others.
pub fn is_changed(op: u16, body: &[u8]) -> bool {
    op == OP_STORAGE_CHANGED && body.is_empty()
}

/// **The names an `Eject` reply says were unmounted**: one per line (a mount name has no newline).
pub fn ejected_names(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    body.split(|&b| b == b'\n').filter(|n| !n.is_empty())
}

/// Bytes before a `Mount` body's two strings: their lengths, `u16` each.
pub const MOUNT_PREFIX_LEN: usize = 4;

/// Build a `Mount` body: `device_len: u16`, `label_len: u16`, device, label; `None` if too long.
pub fn build_mount(out: &mut [u8], device: &[u8], label: &[u8]) -> Option<usize> {
    let total = MOUNT_PREFIX_LEN.checked_add(device.len())?.checked_add(label.len())?;
    if out.len() < total || device.len() > u16::MAX as usize || label.len() > u16::MAX as usize {
        return None;
    }
    put_u16(out, 0, device.len() as u16);
    put_u16(out, 2, label.len() as u16);
    out[MOUNT_PREFIX_LEN..MOUNT_PREFIX_LEN + device.len()].copy_from_slice(device);
    out[MOUNT_PREFIX_LEN + device.len()..total].copy_from_slice(label);
    Some(total)
}

/// A `Mount` body's device name and label; `None` unless the lengths account for the body exactly.
pub fn parse_mount(body: &[u8]) -> Option<(&[u8], &[u8])> {
    if body.len() < MOUNT_PREFIX_LEN {
        return None;
    }
    let device_len = get_u16(body, 0) as usize;
    let label_len = get_u16(body, 2) as usize;
    if body.len() != MOUNT_PREFIX_LEN + device_len + label_len {
        return None;
    }
    let device = &body[MOUNT_PREFIX_LEN..MOUNT_PREFIX_LEN + device_len];
    Some((device, &body[MOUNT_PREFIX_LEN + device_len..]))
}

/// Bytes before a `Mount` reply's label: its mode, a reserved `0`, and the label's length.
pub const MOUNTED_PREFIX_LEN: usize = 4;

/// Build a `Mount` reply: `mode` (`0` writable, `1` read-only), a reserved `0`, `label_len: u16`,
/// the label, then `read_only`'s reason, nothing when writable; `None` if `out` is short.
pub fn build_mounted(out: &mut [u8], label: &[u8], read_only: Option<&[u8]>) -> Option<usize> {
    let why = read_only.unwrap_or_default();
    let total = MOUNTED_PREFIX_LEN.checked_add(label.len())?.checked_add(why.len())?;
    if out.len() < total || label.len() > u16::MAX as usize {
        return None;
    }
    out[0] = read_only.is_some() as u8;
    out[1] = 0;
    put_u16(out, 2, label.len() as u16);
    out[MOUNTED_PREFIX_LEN..MOUNTED_PREFIX_LEN + label.len()].copy_from_slice(label);
    out[MOUNTED_PREFIX_LEN + label.len()..total].copy_from_slice(why);
    Some(total)
}

/// A `Mount` reply's label, and why it is read-only if it is; `None` for one its mode and lengths do
/// not account for, a writable one carrying a reason among them.
pub fn parse_mounted(body: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    if body.len() < MOUNTED_PREFIX_LEN || body[1] != 0 {
        return None;
    }
    let end = MOUNTED_PREFIX_LEN + get_u16(body, 2) as usize;
    let (label, why) = (body.get(MOUNTED_PREFIX_LEN..end)?, &body[end..]);
    match body[0] {
        0 if why.is_empty() => Some((label, None)),
        1 => Some((label, Some(why))),
        _ => None,
    }
}

/// Build an `InUse` reply: `count: u32`, then `count` `u32` registry ids; `None` if `out` is short.
pub fn build_in_use(out: &mut [u8], ids: &[u32]) -> Option<usize> {
    let total = 4usize.checked_add(ids.len().checked_mul(4)?)?;
    if out.len() < total || ids.len() > u32::MAX as usize {
        return None;
    }
    put_u32(out, 0, ids.len() as u32);
    for (i, &id) in ids.iter().enumerate() {
        put_u32(out, 4 + 4 * i, id);
    }
    Some(total)
}

/// Call `each` with every id of an `InUse` reply; `false` unless the body is exactly count + ids.
pub fn parse_in_use(body: &[u8], mut each: impl FnMut(u32)) -> bool {
    if body.len() < 4 {
        return false;
    }
    let count = get_u32(body, 0) as usize;
    if count.checked_mul(4).and_then(|n| n.checked_add(4)) != Some(body.len()) {
        return false;
    }
    (0..count).for_each(|i| each(get_u32(body, 4 + 4 * i)));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A Mount's reply says how it was mounted**, and why when read-only; one whose mode or
    /// lengths do not account for it, a writable one with a reason among them, reads as nothing.
    #[test]
    fn a_mount_reply_says_how_it_was_mounted() {
        let mut out = [0u8; 64];
        let n = build_mounted(&mut out, b"data", None).unwrap();
        assert_eq!(parse_mounted(&out[..n]), Some((&b"data"[..], None)));
        let n = build_mounted(&mut out, b"csum", Some(b"it can only be read here")).unwrap();
        assert_eq!(parse_mounted(&out[..n]), Some((&b"csum"[..], Some(&b"it can only be read here"[..]))));
        let bad: [&[u8]; 5] =
            [b"\x00\x00\x04\x00dat", b"\x02\x00\x01\x00d", b"\x00\x01\x01\x00d", b"\x00\x00\x01\x00dx", b"\x01\x00"];
        for body in bad {
            assert_eq!(parse_mounted(body), None, "{body:?}");
        }
    }

    #[test]
    fn a_mount_carries_a_device_and_a_label() {
        let mut out = [0u8; 64];
        let n = build_mount(&mut out, b"blk-1", b"nitrox-scratch").unwrap();
        assert_eq!(parse_mount(&out[..n]), Some((&b"blk-1"[..], &b"nitrox-scratch"[..])));
        let n = build_mount(&mut out, b"blk-3", b"").unwrap();
        assert_eq!(parse_mount(&out[..n]), Some((&b"blk-3"[..], &b""[..])), "no label: the service chooses");
        assert_eq!(build_mount(&mut out[..8], b"blk-1", b"x"), None, "no room");
    }

    #[test]
    fn a_mount_whose_lengths_disagree_with_its_body_is_refused() {
        let mut out = [0u8; 64];
        let n = build_mount(&mut out, b"blk-1", b"label").unwrap();
        assert_eq!(parse_mount(&out[..n - 1]), None, "cut short");
        assert_eq!(parse_mount(&out[..n + 1]), None, "a byte after");
        assert_eq!(parse_mount(&[0, 0, 0]), None, "no prefix");
        assert_eq!(parse_mount(&[0xFF, 0xFF, 0, 0, b'x']), None, "a length past the end");
    }

    #[test]
    fn an_eject_reply_names_each_mount_on_a_line() {
        fn names(b: &[u8]) -> std::vec::Vec<&[u8]> {
            ejected_names(b).collect()
        }
        assert_eq!(names(b"ONE\nTWO"), [&b"ONE"[..], b"TWO"]);
        assert_eq!(names(b"partition 1\n"), [&b"partition 1"[..]]);
        assert!(names(b"").is_empty());
    }

    #[test]
    fn a_ping_is_changed_with_no_body() {
        assert!(is_changed(OP_STORAGE_CHANGED, &[]));
        assert!(!is_changed(OP_STORAGE_CHANGED, &[0]), "a body");
        assert!(!is_changed(OP_STORAGE_EJECT, &[]), "another op");
        let ops = [OP_STORAGE_MOUNT, OP_STORAGE_UNMOUNT, OP_STORAGE_IN_USE, OP_STORAGE_EJECT, OP_STORAGE_CHANGED];
        for (i, a) in ops.iter().enumerate() {
            assert!(ops[i + 1..].iter().all(|b| b != a), "{a:#06x} used twice");
        }
    }

    #[test]
    fn in_use_is_a_counted_list_of_ids() {
        let mut out = [0u8; 64];
        let n = build_in_use(&mut out, &[3, 7, 12]).unwrap();
        let mut got = [0u32; 3];
        let mut i = 0;
        assert!(parse_in_use(&out[..n], |id| {
            got[i] = id;
            i += 1;
        }));
        assert_eq!(got, [3, 7, 12]);
        let n = build_in_use(&mut out, &[]).unwrap();
        assert!(parse_in_use(&out[..n], |_| panic!("no ids")));
        assert!(!parse_in_use(&[1, 0, 0, 0], |_| {}), "a count with no id after it");
        assert!(!parse_in_use(&[0, 0, 0, 0, 9], |_| {}), "a byte after the list");
        assert!(!parse_in_use(&[0xFF, 0xFF, 0xFF, 0xFF], |_| {}), "a count that overflows");
    }
}
