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

// --- object_kind values (reply) ---------------------------------------------

/// The reply's `handles[0]` is a read-only `MemoryObject` of file content.
pub const OBJECT_KIND_MEMOBJ: u16 = 1;
/// A directory resource (deferred).
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
