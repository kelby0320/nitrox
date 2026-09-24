//! `Namespace` category (`op = 0x01xx`) bodies. Slice 7 defines `Resolve` — the
//! kernel-forwarded path resolution. See `docs/spec/rsproto-namespace-ops.md`.

use crate::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64};

// --- Resolve flags ----------------------------------------------------------

/// `RESOLVE_FILE_AS_MEMOBJ` — resolve a regular file to a read-only `MemoryObject`
/// of its content, eagerly (slice 7; the whole file is read up front).
pub const RESOLVE_FILE_AS_MEMOBJ: u32 = 1 << 0;
/// `RESOLVE_FILE_LAZY` — resolve a regular file to a `File` resource
/// ([`OBJECT_KIND_FILE`]) whose pages are filled on demand via
/// [`File::ReadRange`](crate::file) (slice 8). The reply carries the file size,
/// not its bytes; no handle rides in `handles[0]` — the kernel builds the
/// page-cache object itself, pointed back at this server.
pub const RESOLVE_FILE_LAZY: u32 = 1 << 1;
/// `RESOLVE_GROW` — grow the file to the `new_size` appended after the suffix (a `u32`)
/// before replying its map. Combined with `RESOLVE_FILE_LAZY`. See
/// [`parse_resolve_grow_size`]. `docs/architecture/ext4-fs-server-rw.md`.
pub const RESOLVE_GROW: u32 = 1 << 2;
/// `RESOLVE_CREATE` — create the file (allocate an inode + insert a directory entry in
/// the parent) if it does not exist, before growing/mapping it. Combined with
/// `RESOLVE_FILE_LAZY | RESOLVE_GROW`; the `new_size` rides after the suffix as for
/// [`RESOLVE_GROW`]. `docs/architecture/ext4-fs-server-rw.md`.
pub const RESOLVE_CREATE: u32 = 1 << 3;
/// `RESOLVE_TRUNCATE` — **shrink** the file to the `new_size` appended after the suffix
/// (free the blocks past the new end) before replying its map. Combined with
/// [`RESOLVE_FILE_LAZY`]. For `sys_file_truncate`. The inverse of [`RESOLVE_GROW`], and a
/// separate flag rather than "grow to a smaller size" because the two do opposite things
/// to the block allocator and a caller must not get one when it asked for the other.
pub const RESOLVE_TRUNCATE: u32 = 1 << 4;
/// `RESOLVE_RENAME` — **rename** the resolved path to a second path carried in the same
/// request, and reply status-only ([`OBJECT_KIND_NONE`]) rather than an object.
///
/// A rename inherently names *two* directories, which a directory session — bound to one
/// inode, addressing entries by name — structurally cannot express; so it rides the
/// path-addressed resolve surface alongside `RESOLVE_CREATE`/`GROW`/`TRUNCATE`, with the
/// **namespace** as the confinement boundary (a process can only name what its supervisor
/// bound into it). The kernel additionally guarantees both paths resolve through the *same*
/// binding, so a server never sees a cross-filesystem rename. See the decision log
/// (2026-07-29).
pub const RESOLVE_RENAME: u32 = 1 << 5;

/// `RENAME_REPLACE` — the rename flag permitting an existing destination to be replaced.
/// Absent, a taken destination fails; the primitive does not guess.
pub const RENAME_REPLACE: u16 = 1 << 0;

// --- object_kind values (reply) ---------------------------------------------

/// The reply's `handles[0]` is a read-only `MemoryObject` of file content.
/// The request completed and produced **no object** — a status-only success, used by the
/// mutating resolves (`RESOLVE_RENAME`) that change the filesystem rather than resolve to
/// something. The reply carries no handle and the lookup completes with result `0`.
pub const OBJECT_KIND_NONE: u16 = 0;
pub const OBJECT_KIND_MEMOBJ: u16 = 1;
/// A directory resource. The fs-server does not use this kind on the wire — the kernel has
/// no "directory" reply kind, so an **open directory handle** is returned as an
/// [`OBJECT_KIND_CHANNEL`] (a session [`IpcChannel`] scoped to the resolved directory). This
/// value is reserved.
pub const OBJECT_KIND_DIRECTORY: u16 = 2;
/// A nested namespace (deferred).
pub const OBJECT_KIND_SUBNAMESPACE: u16 = 3;
/// A lazily-filled file: `content_len` is the **total file size**; the kernel
/// builds a page-cache object filled on demand via `File::ReadRange`. No handle
/// rides in `handles[0]`. Paired with [`RESOLVE_FILE_LAZY`].
pub const OBJECT_KIND_FILE: u16 = 4;
/// The reply's `handles[0]` is a live **`IpcChannel`** endpoint — a connection to the
/// resolving server, not a file. Used by connect-style servers (the logging service
/// resolves a log path to a per-principal write channel). `content_len` is unused.
pub const OBJECT_KIND_CHANNEL: u16 = 5;
/// A **Model A** (block-filesystem) lazy file: `content_len` is the file size, `handles[0]`
/// transfers the block device, and the reply body carries the filesystem block size + the
/// file's `BlockRun` map (see `docs/spec/rsproto-block-ops.md`). The kernel fills each page
/// zero-copy from the device.
pub const OBJECT_KIND_FILE_BLOCKS: u16 = 6;

// --- Resolve request --------------------------------------------------------

/// Fixed prefix of a `ResolveRequest` (before the suffix bytes).
pub const RESOLVE_REQUEST_PREFIX_LEN: usize = 16;

/// A parsed `ResolveRequest`.
#[derive(Copy, Clone, Debug)]
pub struct ResolveRequest<'a> {
    pub requested_rights: u64,
    pub flags: u32,
    /// The path suffix (UTF-8, no leading `/`).
    pub suffix: &'a [u8],
}

/// Write a `ResolveRequest` body; returns its length.
pub fn resolve_request(
    out: &mut [u8],
    requested_rights: u64,
    flags: u32,
    suffix: &[u8],
) -> Option<usize> {
    if suffix.len() > u16::MAX as usize {
        return None;
    }
    let total = RESOLVE_REQUEST_PREFIX_LEN + suffix.len();
    if out.len() < total {
        return None;
    }
    put_u64(out, 0, requested_rights);
    put_u32(out, 8, flags);
    put_u16(out, 12, suffix.len() as u16);
    put_u16(out, 14, 0);
    out[RESOLVE_REQUEST_PREFIX_LEN..total].copy_from_slice(suffix);
    Some(total)
}

/// Parse a `ResolveRequest` body.
pub fn parse_resolve_request(body: &[u8]) -> Option<ResolveRequest<'_>> {
    if body.len() < RESOLVE_REQUEST_PREFIX_LEN {
        return None;
    }
    let suffix_len = get_u16(body, 12) as usize;
    let end = RESOLVE_REQUEST_PREFIX_LEN.checked_add(suffix_len)?;
    if body.len() < end {
        return None;
    }
    Some(ResolveRequest {
        requested_rights: get_u64(body, 0),
        flags: get_u32(body, 8),
        suffix: &body[RESOLVE_REQUEST_PREFIX_LEN..end],
    })
}

/// For a `RESOLVE_GROW` request, the target `new_size` (a `u32` appended after the suffix),
/// or `None` if the body is too short. See [`RESOLVE_GROW`].
pub fn parse_resolve_grow_size(body: &[u8]) -> Option<u32> {
    if body.len() < RESOLVE_REQUEST_PREFIX_LEN {
        return None;
    }
    let suffix_len = get_u16(body, 12) as usize;
    let off = RESOLVE_REQUEST_PREFIX_LEN.checked_add(suffix_len)?;
    if body.len() < off + 4 {
        return None;
    }
    Some(get_u32(body, off))
}

// --- Resolve reply (success) ------------------------------------------------

/// `ResolveReply` wire length (the resource handle rides in `IpcMsg.handles[0]`).
pub const RESOLVE_REPLY_LEN: usize = 8;

/// A parsed success `ResolveReply`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResolveReply {
    pub object_kind: u16,
    /// Exact resource length in bytes (a MemoryObject may zero-pad its tail).
    pub content_len: u32,
}

/// Write a success `ResolveReply` body; returns its length.
pub fn resolve_reply(out: &mut [u8], object_kind: u16, content_len: u32) -> Option<usize> {
    if out.len() < RESOLVE_REPLY_LEN {
        return None;
    }
    put_u16(out, 0, object_kind);
    put_u16(out, 2, 0);
    put_u32(out, 4, content_len);
    Some(RESOLVE_REPLY_LEN)
}

/// Bytes of a `FILE_BLOCKS` reply body before its runs: the `ResolveReply`, then `block_size`,
/// `run_count`, `file_id`, `flags` and four reserved bytes. The file id and flags arrived with
/// administration Part C.1, when the kernel began keeping one page-cache object per file.
pub const FILE_BLOCKS_PREFIX_LEN: usize = 32;
/// Bytes of one `BlockRun` in a `FILE_BLOCKS` reply: `file_block: u64`, `device_lba: u64`,
/// `length: u32`, `flags: u32`.
pub const BLOCK_RUN_WIRE_LEN: usize = 24;
/// `FILE_BLOCKS` flag: the file may not be written — a read-only mount's. The kernel installs
/// it without `MAP_WRITE`, whatever the lookup asked for (administration Part C.3).
pub const FILE_BLOCKS_READ_ONLY: u32 = 1 << 0;

/// Build a `FILE_BLOCKS` reply body's prefix into `out`, returning [`FILE_BLOCKS_PREFIX_LEN`];
/// the caller writes the `run_count` runs from there. `file_id` is the file's identity on
/// its server — an inode number — and is what the kernel keeps one object per, so it must be
/// stable for the file's life and never `0` for a file that should be cached. `None` if
/// `out` is too small.
pub fn file_blocks_prefix(
    out: &mut [u8],
    content_len: u32,
    block_size: u32,
    run_count: u32,
    file_id: u64,
    flags: u32,
) -> Option<usize> {
    if out.len() < FILE_BLOCKS_PREFIX_LEN {
        return None;
    }
    resolve_reply(out, OBJECT_KIND_FILE_BLOCKS, content_len)?;
    put_u32(out, 8, block_size);
    put_u32(out, 12, run_count);
    put_u64(out, 16, file_id);
    put_u32(out, 24, flags);
    put_u32(out, 28, 0);
    Some(FILE_BLOCKS_PREFIX_LEN)
}

/// Parse a success `ResolveReply` body.
pub fn parse_resolve_reply(body: &[u8]) -> Option<ResolveReply> {
    if body.len() < RESOLVE_REPLY_LEN {
        return None;
    }
    Some(ResolveReply {
        object_kind: get_u16(body, 0),
        content_len: get_u32(body, 4),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_request_round_trips() {
        let mut buf = [0u8; 128];
        let n = resolve_request(&mut buf, 0x4, RESOLVE_FILE_AS_MEMOBJ, b"system/current-generation")
            .unwrap();
        assert_eq!(n, RESOLVE_REQUEST_PREFIX_LEN + 25);
        let r = parse_resolve_request(&buf[..n]).unwrap();
        assert_eq!(r.requested_rights, 0x4);
        assert_eq!(r.flags, RESOLVE_FILE_AS_MEMOBJ);
        assert_eq!(r.suffix, b"system/current-generation");
    }

    /// **The prefix lands where the kernel reads it** — checked byte by byte against the
    /// spec's offsets, not through a reader in this crate: the reader is the kernel's.
    #[test]
    fn a_file_blocks_prefix_is_laid_out_as_the_spec_draws_it() {
        let mut buf = [0xAAu8; 40];
        let n = file_blocks_prefix(&mut buf, 8192, 4096, 3, 0x0102_0304_0506_0708, FILE_BLOCKS_READ_ONLY)
            .unwrap();
        assert_eq!(n, 32);
        assert_eq!(&buf[0..2], &OBJECT_KIND_FILE_BLOCKS.to_le_bytes());
        assert_eq!(&buf[2..4], &[0, 0]);
        assert_eq!(&buf[4..8], &8192u32.to_le_bytes());
        assert_eq!(&buf[8..12], &4096u32.to_le_bytes());
        assert_eq!(&buf[12..16], &3u32.to_le_bytes());
        assert_eq!(&buf[16..24], &[8, 7, 6, 5, 4, 3, 2, 1], "the id, little-endian, at 16");
        assert_eq!(&buf[24..28], &[1, 0, 0, 0], "the flags at 24");
        assert_eq!(&buf[28..32], &[0, 0, 0, 0], "reserved, zeroed");
        assert_eq!(&buf[32..], &[0xAA; 8], "nothing written past the prefix");
        assert_eq!(file_blocks_prefix(&mut buf[..31], 0, 0, 0, 1, 0), None);
    }

    #[test]
    fn resolve_reply_round_trips() {
        let mut buf = [0u8; 16];
        let n = resolve_reply(&mut buf, OBJECT_KIND_MEMOBJ, 42).unwrap();
        assert_eq!(n, RESOLVE_REPLY_LEN);
        let r = parse_resolve_reply(&buf[..n]).unwrap();
        assert_eq!(r, ResolveReply { object_kind: OBJECT_KIND_MEMOBJ, content_len: 42 });
    }

    #[test]
    fn parse_rejects_truncated() {
        assert!(parse_resolve_request(&[0u8; 8]).is_none());
        // suffix_len claims 100 bytes but body is short
        let mut buf = [0u8; 32];
        super::put_u16(&mut buf, 12, 100);
        assert!(parse_resolve_request(&buf).is_none());
        assert!(parse_resolve_reply(&[0u8; 4]).is_none());
    }
}

/// The destination suffix + flags a [`RESOLVE_RENAME`] request carries after its primary
/// suffix: `dest_len: u16`, `flags: u16`, then `dest_len` bytes.
///
/// Laid out after the suffix for the same reason the size-changing resolves put `new_size`
/// there — the fixed prefix stays one shape for every resolve, and only the tail varies.
pub fn parse_resolve_rename(body: &[u8]) -> Option<(&[u8], u16)> {
    let r = parse_resolve_request(body)?;
    let tail = RESOLVE_REQUEST_PREFIX_LEN + r.suffix.len();
    if body.len() < tail + 4 {
        return None;
    }
    let dest_len = u16::from_le_bytes([body[tail], body[tail + 1]]) as usize;
    let flags = u16::from_le_bytes([body[tail + 2], body[tail + 3]]);
    if body.len() < tail + 4 + dest_len {
        return None;
    }
    Some((&body[tail + 4..tail + 4 + dest_len], flags))
}
