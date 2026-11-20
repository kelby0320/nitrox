//! `s_state` and the label: how the filesystem was left, and recording it mounted and unmounted.

use super::*;

/// Byte offset of `s_state`: the superblock at 1024, `s_state` at 58 within it.
const S_STATE_AT: u64 = 1024 + 58;
/// Where the superblock keeps `s_volume_name`: 16 bytes, NUL-padded.
const S_VOLUME_NAME_AT: u64 = 1024 + 120;
/// `s_state`'s "cleanly unmounted" bit (`EXT4_VALID_FS`).
const STATE_CLEAN: u16 = 0x0001;
/// `s_state`'s "errors found" bit (`EXT4_ERROR_FS`), which `e2fsck -p` checks a filesystem for.
const STATE_ERRORS: u16 = 0x0002;

fn clean(state: u16) -> bool {
    state & (STATE_CLEAN | STATE_ERRORS) == STATE_CLEAN
}

/// **Whether the filesystem was left clean**: `s_state` cleanly unmounted, with no errors found. A
/// writable mount clears the first ([`mark_mounted`]) and its unmount sets it again as its last
/// write ([`mark_unmounted`]). `Err` if the device holds no ext4 filesystem.
pub fn was_left_clean<R: BlockReader>(r: &R) -> Result<bool, FsError> {
    read_superblock(r)?;
    let mut s = [0u8; 2];
    r.read_at(S_STATE_AT, &mut s)?;
    Ok(clean(u16::from_le_bytes(s)))
}

/// **The filesystem's own label**, `s_volume_name`, written into `out`: returns how many bytes are
/// meaningful (sixteen, NUL-padded; no terminator when all are used). `Err` if not ext4.
pub fn volume_label<R: BlockReader>(r: &R, out: &mut [u8; 16]) -> Result<usize, FsError> {
    read_superblock(r)?;
    r.read_at(S_VOLUME_NAME_AT, out)?;
    Ok(out.iter().position(|&b| b == 0).unwrap_or(out.len()))
}

/// Record that the filesystem is **mounted writable**, before `Meta::Ready`, so it never looks
/// clean while it can change: clear the "cleanly unmounted" bit, and answer the state found.
pub fn mark_mounted<RW: BlockReader + BlockWriter>(rw: &RW) -> Result<u16, FsError> {
    let mut found = 0;
    set_state(rw, |s| {
        found = s;
        s & !STATE_CLEAN
    })?;
    Ok(found)
}

/// **Record an unmount, by the state its mount `found`**: clean only if found clean and no change
/// `failed`; else as found, error bit and all, or as the mount left it, for the next `e2fsck -p`.
pub fn mark_unmounted<RW: BlockReader + BlockWriter>(rw: &RW, found: u16, failed: bool) -> Result<(), FsError> {
    match (clean(found), failed) {
        (true, false) => mark_clean(rw),
        (true, true) => Ok(()),
        (false, _) => set_state(rw, |_| found),
    }
}

/// Record that the filesystem was **cleanly unmounted**: set the bit, as the last write.
pub fn mark_clean<RW: BlockReader + BlockWriter>(rw: &RW) -> Result<(), FsError> {
    set_state(rw, |s| s | STATE_CLEAN)
}

fn set_state<RW: BlockReader + BlockWriter>(rw: &RW, f: impl FnOnce(u16) -> u16) -> Result<(), FsError> {
    read_superblock(rw)?;
    let mut s = [0u8; 2];
    rw.read_meta(S_STATE_AT, &mut s)?;
    rw.write_at(S_STATE_AT, &f(u16::from_le_bytes(s)).to_le_bytes())
}
