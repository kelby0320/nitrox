//! `Storage` (`op = 0x10xx`) — mounting and unmounting, served by the storage service on an admin
//! session. See `docs/spec/rsproto-storage-ops.md` and `docs/planning/administration.md` § Part C.
//!
//! **An admin session is a channel the service answers any resolve on its admin endpoint with.**
//! The endpoint is minted at `/svc/storage/admin-endpoint`, and the view broker binds it at
//! `/dev/storage/admin` in a view with the `storage` grant (C.6). What reaches it is therefore
//! the grant's to decide; the service answers every request on it.
//!
//! The filesystems themselves, and the table of what is on each disk, are ordinary `Namespace` and
//! `File` operations under `/svc/storage/fs` and `/svc/storage/info`, not these.
//!
//! Bodies are little-endian and byte-serialised into a caller buffer, like every category here.
//! A refusal is the standard `ErrorBody`, and its reason says which step refused.

use crate::{get_u16, get_u32, put_u16, put_u32};

/// Client → service: **mount a device**. Body [`build_mount`]: the device's name as the tables
/// name it (`blk-<n>`), and a label, empty to let the service choose. **Always writable**: the
/// automatic mount of a live boot is the careful one, not an administrator's. Reply body: the label
/// it was mounted under, as bytes.
pub const OP_STORAGE_MOUNT: u16 = 0x1000;
/// Client → service: **unmount a filesystem**, by its label. Body: the label's bytes. The service
/// writes back every dirty file, refuses with `WouldBlock` if a file is still held, has the server
/// record the filesystem clean and exit, and flushes the drive. Reply body: empty.
pub const OP_STORAGE_UNMOUNT: u16 = 0x1001;
/// Client → service: **the devices in use**, which must not be granted raw: every mounted
/// filesystem's device, `init`'s included, and the disk that holds it. Body: empty. Reply body
/// [`build_in_use`]: their registry ids.
pub const OP_STORAGE_IN_USE: u16 = 0x1002;

/// Bytes before a `Mount` body's two strings: their lengths, `u16` each.
pub const MOUNT_PREFIX_LEN: usize = 4;

/// Build a `Mount` body: `device_len: u16`, `label_len: u16`, the device's name, then the label.
/// `None` if either is too long for its length or `out` is too small.
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

/// A `Mount` body's device name and label. `None` unless the body is exactly the prefix and the
/// two strings its lengths say — a body with bytes after them is malformed, not padded.
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

/// Build an `InUse` reply: `count: u32`, then `count` registry ids, `u32` each. `None` if `out` is
/// too small.
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

/// Call `each` with every id an `InUse` reply carries, in order. `false` unless the body is exactly
/// the count and that many ids.
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

    #[test]
    fn a_mount_carries_a_device_and_a_label() {
        let mut out = [0u8; 64];
        let n = build_mount(&mut out, b"blk-1", b"nitrox-scratch").unwrap();
        assert_eq!(parse_mount(&out[..n]), Some((&b"blk-1"[..], &b"nitrox-scratch"[..])));
        let n = build_mount(&mut out, b"blk-3", b"").unwrap();
        assert_eq!(parse_mount(&out[..n]), Some((&b"blk-3"[..], &b""[..])), "no label: the service chooses");
        assert_eq!(build_mount(&mut out[..8], b"blk-1", b"x"), None, "no room");
    }

    /// **The lengths must account for the body exactly**: what a correct client never sends — a
    /// body cut short, or with bytes after its strings — is refused, not read around.
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
