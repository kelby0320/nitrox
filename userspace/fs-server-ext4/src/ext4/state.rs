//! `s_state` and the label: how the filesystem was left, and recording it mounted and unmounted.

use super::*;

/// Byte offset of `s_state`: the superblock at 1024, `s_state` at 58 within it.
const S_STATE_AT: u64 = 1024 + 58;
/// Where the superblock keeps `s_volume_name`: 16 bytes, NUL-padded.
const S_VOLUME_NAME_AT: u64 = 1024 + 120;
/// `s_state`'s "cleanly unmounted" bit (`EXT4_VALID_FS`).
const STATE_CLEAN: u16 = 0x0001;

/// **Whether the filesystem was left clean**: `s_state`'s "cleanly unmounted" bit. A writable
/// mount clears it ([`mark_mounted`]) and an unmount sets it again as its last write
/// ([`mark_clean`]). `Err` if the device holds no ext4 filesystem.
pub fn was_left_clean<R: BlockReader>(r: &R) -> Result<bool, FsError> {
    read_superblock(r)?;
    let mut s = [0u8; 2];
    r.read_at(S_STATE_AT, &mut s)?;
    Ok(u16::from_le_bytes(s) & STATE_CLEAN != 0)
}

/// **The filesystem's own label**, `s_volume_name`, written into `out`: returns how many bytes are
/// meaningful (sixteen, NUL-padded; no terminator when all are used). `Err` if not ext4.
pub fn volume_label<R: BlockReader>(r: &R, out: &mut [u8; 16]) -> Result<usize, FsError> {
    read_superblock(r)?;
    r.read_at(S_VOLUME_NAME_AT, out)?;
    Ok(out.iter().position(|&b| b == 0).unwrap_or(out.len()))
}

/// Record that the filesystem is **mounted writable**: clear the "cleanly unmounted" bit, leaving
/// the rest of `s_state`. Done before `Meta::Ready`, so it never looks clean while it can change.
pub fn mark_mounted<RW: BlockReader + BlockWriter>(rw: &RW) -> Result<(), FsError> {
    set_state(rw, |s| s & !STATE_CLEAN)
}

/// Record that the filesystem was **cleanly unmounted**: set the bit, as the last write.
pub fn mark_clean<RW: BlockReader + BlockWriter>(rw: &RW) -> Result<(), FsError> {
    set_state(rw, |s| s | STATE_CLEAN)
}

fn set_state<RW: BlockReader + BlockWriter>(rw: &RW, f: impl Fn(u16) -> u16) -> Result<(), FsError> {
    read_superblock(rw)?;
    let mut s = [0u8; 2];
    rw.read_meta(S_STATE_AT, &mut s)?;
    rw.write_at(S_STATE_AT, &f(u16::from_le_bytes(s)).to_le_bytes())
}
