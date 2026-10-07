//! The request→reply logic for the forwarded ops the kernel sends — the pure core
//! of the server loop, kept apart from [`crate::server`] so it is **host-testable**: it is
//! generic over [`Volume`], and each server's crate tests it through its own volume, against
//! an image its filesystem's tools built.
//!
//! [`serve`] dispatches by op: a `Namespace::Resolve` ([`serve_resolve`]) resolves a
//! path — lazily (reply `OBJECT_KIND_FILE_BLOCKS` + the file's map; the kernel builds the
//! page-cache object) or eagerly (reply a `MemoryObject` of the whole content) — and
//! a `File::ReadRange` ([`serve_read_range`]) reads one byte range of a file (the
//! page-cache fill; reply a `MemoryObject` of the range). The syscall plumbing
//! (materialising/transferring the `MemoryObject`, recv/send) lives in [`crate::server`];
//! this module touches no syscalls. An error reply carries the op of its request so
//! the kernel routes it to the right pending operation (lookup vs fill).

use crate::{BlockRun, FsError, Mapped, Refusal, Volume};
use libkern::KError;
use librsproto::error::{ERROR_BODY_LEN, error_body};
use librsproto::file::{READ_RANGE_REPLY_LEN, parse_read_range_request, read_range_reply};
use librsproto::namespace::{
    BLOCK_RUN_WIRE_LEN, FILE_BLOCKS_PREFIX_LEN, FILE_BLOCKS_READ_ONLY, OBJECT_KIND_MEMOBJ, RESOLVE_FILE_LAZY,
    RESOLVE_REPLY_LEN, file_blocks_prefix, parse_resolve_request, resolve_reply,
};
use librsproto::{OP_FILE_READ_RANGE, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

/// Largest suffix the server resolves (bounds the on-stack path buffer). A path
/// longer than this resolves to `TooLarge` — far beyond any real filesystem path.
pub const MAX_SUFFIX: usize = 1024;

/// **Largest file an eager resolve reads whole**, the slice-7 path's cap: what the server's
/// content buffer holds.
pub const MAX_FILE: usize = 64 * 1024;

/// What the caller ([`crate::server`]) should do with the reply a serve fn built.
pub enum Served {
    /// Success: `reply[..reply_len]` is the rsproto reply; the caller transfers a
    /// read-only `MemoryObject` of `content[..content_len]` in `IpcMsg.handles[0]`.
    /// Both an eager resolve and a `ReadRange` fill produce this.
    File { reply_len: usize, content_len: usize },
    /// A **Model A** lazy resolve: `reply[..reply_len]` carries the file size + block map;
    /// the caller transfers a `READ | TRANSFER` **duplicate of the block-device handle** in
    /// `IpcMsg.handles[0]` (the kernel does the file-data I/O). Keep the original device
    /// handle for metadata reads.
    LazyBlocks { reply_len: usize },
    /// An error reply (no handle transferred): `reply[..reply_len]`.
    Error { reply_len: usize },
}

/// Largest `BlockRun` map inlined in a Model A resolve reply. A file needing more runs is
/// too fragmented to inline (→ `TooLarge`); the standalone `MapRange` op covers it (deferred).
/// 64 runs = 1536 body bytes, comfortably inside the 4 KiB IPC payload.
pub const MAX_RUNS: usize = 64;

/// Serve one forwarded request: dispatch by op to [`serve_resolve`] (a
/// `Namespace::Resolve`) or [`serve_read_range`] (a `File::ReadRange`). An
/// undecodable request or an unknown op yields an error reply.
pub fn serve<V: Volume>(vol: &V, request: &[u8], content: &mut [u8], reply: &mut [u8]) -> Served {
    match decode(request) {
        Ok(m) if m.op == OP_NS_RESOLVE => serve_resolve(vol, request, content, reply),
        Ok(m) if m.op == OP_FILE_READ_RANGE => serve_read_range(vol, request, content, reply),
        // Known envelope, unknown op: reply Unsupported with that op.
        Ok(m) => error_reply(reply, m.request_id, KError::Unsupported, m.op),
        // Undecodable: no recoverable id/op; reply a Resolve-shaped error, id 0.
        Err(_) => error_reply(reply, 0, KError::InvalidArgument, OP_NS_RESOLVE),
    }
}

/// Serve one forwarded `Namespace::Resolve`: parse `request`, resolve `"/" + suffix`
/// (the mount is the filesystem root) to a regular file, and build the reply. With
/// `RESOLVE_FILE_LAZY` the reply is **lazy** — `OBJECT_KIND_FILE_BLOCKS`, the file size and its
/// map, no content read (the kernel builds the page-cache object; the caller transfers the
/// device). Otherwise it is **eager** — the whole content read into `content` and
/// named by a transferred `MemoryObject` (`OBJECT_KIND_MEMOBJ`, the slice-7 path,
/// capped at [`MAX_FILE`]). The `request_id` is echoed; a malformed/oversized
/// request or any [`FsError`] yields an error reply.
pub fn serve_resolve<V: Volume>(vol: &V, request: &[u8], content: &mut [u8], reply: &mut [u8]) -> Served {
    // Decode the envelope first — recover the `request_id` even if the rest is
    // unusable, so the error reply still correlates to the right lookup.
    let msg = match decode(request) {
        Ok(m) => m,
        // A request that doesn't even decode has no recoverable id; reply id 0.
        Err(_) => return error_reply(reply, 0, KError::InvalidArgument, OP_NS_RESOLVE),
    };
    let request_id = msg.request_id;
    if msg.op != OP_NS_RESOLVE {
        return error_reply(reply, request_id, KError::Unsupported, OP_NS_RESOLVE);
    }
    let req = match parse_resolve_request(msg.body) {
        Some(r) => r,
        None => return error_reply(reply, request_id, KError::InvalidArgument, OP_NS_RESOLVE),
    };
    if req.suffix.len() > MAX_SUFFIX {
        return error_reply(reply, request_id, KError::TooLarge, OP_NS_RESOLVE);
    }

    // Build the absolute path "/" + suffix (the binding is the filesystem root, so
    // the lookup suffix is the path under it; the kernel strips the leading '/').
    let mut path_buf = [0u8; MAX_SUFFIX + 1];
    path_buf[0] = b'/';
    path_buf[1..1 + req.suffix.len()].copy_from_slice(req.suffix);
    let path = &path_buf[..1 + req.suffix.len()];

    if req.flags & RESOLVE_FILE_LAZY != 0 {
        // Model A lazy resolve: map the file's blocks to device runs, reply the size +
        // block size + map (the kernel then does file-data I/O; the caller transfers the
        // device handle).
        let mut runs = [BlockRun::default(); MAX_RUNS];
        return match vol.map_file(path, &mut runs) {
            Ok(m) if m.size > u32::MAX as usize => error_reply(reply, request_id, KError::TooLarge, OP_NS_RESOLVE),
            Ok(m) => {
                // A read-only mount's files are marked so: the kernel installs them without
                // `MAP_WRITE` (administration Part C.3).
                let flags = if vol.read_only() { FILE_BLOCKS_READ_ONLY } else { 0 };
                match model_a_reply(reply, request_id, &m, &runs[..m.runs], flags) {
                    Some(reply_len) => Served::LazyBlocks { reply_len },
                    None => error_reply(reply, request_id, KError::KernelError, OP_NS_RESOLVE),
                }
            }
            Err(e) => error_reply(reply, request_id, kerror(e), OP_NS_RESOLVE),
        };
    }

    // Eager (slice-7): read the whole file and name a MemoryObject of it.
    match vol.read_file(path, content) {
        Ok(size) => match success_reply(reply, request_id, size) {
            Some(reply_len) => Served::File { reply_len, content_len: size },
            // The caller's reply buffer is the 4 KiB IPC payload — far larger than a
            // RESOLVE_REPLY — so this is unreachable; degrade to an error reply.
            None => error_reply(reply, request_id, KError::KernelError, OP_NS_RESOLVE),
        },
        Err(e) => error_reply(reply, request_id, kerror(e), OP_NS_RESOLVE),
    }
}

/// Serve one forwarded `File::ReadRange` (the page-cache fill): parse `request`,
/// read the requested byte range of `"/" + suffix` into `content`, and reply naming
/// a `MemoryObject` of the bytes read (the caller transfers it). The fill is
/// **stateless** — the file is re-identified by the suffix each call. `content_len`
/// in the reply is the bytes actually read (≤ requested; a short tail at EOF leaves
/// the kernel's zeroed frame as padding). An error reply carries the `ReadRange` op.
pub fn serve_read_range<V: Volume>(vol: &V, request: &[u8], content: &mut [u8], reply: &mut [u8]) -> Served {
    let msg = match decode(request) {
        Ok(m) => m,
        Err(_) => return error_reply(reply, 0, KError::InvalidArgument, OP_FILE_READ_RANGE),
    };
    let request_id = msg.request_id;
    if msg.op != OP_FILE_READ_RANGE {
        return error_reply(reply, request_id, KError::Unsupported, OP_FILE_READ_RANGE);
    }
    let req = match parse_read_range_request(msg.body) {
        Some(r) => r,
        None => return error_reply(reply, request_id, KError::InvalidArgument, OP_FILE_READ_RANGE),
    };
    if req.suffix.len() > MAX_SUFFIX {
        return error_reply(reply, request_id, KError::TooLarge, OP_FILE_READ_RANGE);
    }

    let mut path_buf = [0u8; MAX_SUFFIX + 1];
    path_buf[0] = b'/';
    path_buf[1..1 + req.suffix.len()].copy_from_slice(req.suffix);
    let path = &path_buf[..1 + req.suffix.len()];

    // The kernel asks at most one page; bound by `content` regardless.
    let len = (req.len as usize).min(content.len());
    match vol.read_file_range(path, req.offset, len, content) {
        Ok(n) => match range_reply(reply, request_id, n) {
            Some(reply_len) => Served::File { reply_len, content_len: n },
            None => error_reply(reply, request_id, KError::KernelError, OP_FILE_READ_RANGE),
        },
        Err(e) => error_reply(reply, request_id, kerror(e), OP_FILE_READ_RANGE),
    }
}

/// Build a success `ResolveReply` (object_kind `MEMOBJ`, the exact `content_len`)
/// into `reply`; `None` only if `reply` is too small.
fn success_reply(reply: &mut [u8], request_id: u64, content_len: usize) -> Option<usize> {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let body_len = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, content_len as u32)?;
    encode(reply, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body[..body_len], 1)
}

/// Build a **Model A** lazy `ResolveReply` into `reply`: `OBJECT_KIND_FILE_BLOCKS` +
/// `content_len` = file size, then the block size, the file's id, and the
/// `BlockRun` map, with `handle_count = 1` (the caller transfers the device handle). Body
/// layout matches `rsproto-namespace-ops.md` § *The `FILE_BLOCKS` body* and the kernel's
/// `file_blocks_reply_header`/`file_blocks_run`. `None` only if `reply` is too small.
fn model_a_reply(reply: &mut [u8], request_id: u64, m: &Mapped, runs: &[BlockRun], flags: u32) -> Option<usize> {
    let mut body = [0u8; FILE_BLOCKS_PREFIX_LEN + MAX_RUNS * BLOCK_RUN_WIRE_LEN];
    let mut off = file_blocks_prefix(&mut body, m.size as u32, m.block_size, runs.len() as u32, m.id, flags)?;
    for r in runs {
        body[off..off + 8].copy_from_slice(&r.file_block.to_le_bytes());
        body[off + 8..off + 16].copy_from_slice(&r.device_lba.to_le_bytes());
        body[off + 16..off + 20].copy_from_slice(&r.length.to_le_bytes());
        body[off + 20..off + 24].copy_from_slice(&r.flags.to_le_bytes());
        off += BLOCK_RUN_WIRE_LEN;
    }
    encode(reply, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body[..off], 1)
}

/// Build a success `ReadRangeReply` (the `content_len` bytes ride in `handles[0]`)
/// into `reply`; `None` only if `reply` is too small.
fn range_reply(reply: &mut [u8], request_id: u64, content_len: usize) -> Option<usize> {
    let mut body = [0u8; READ_RANGE_REPLY_LEN];
    let body_len = read_range_reply(&mut body, content_len as u32)?;
    encode(reply, OP_FILE_READ_RANGE, request_id, RS_FLAG_REPLY, &body[..body_len], 1)
}

/// Build an error reply (`REPLY | ERROR`, an `ErrorBody` carrying `err`) for `op`
/// into `reply`. The body has no message (replies stay minimal).
fn error_reply(reply: &mut [u8], request_id: u64, err: KError, op: u16) -> Served {
    Served::Error { reply_len: encode_error(reply, request_id, err.as_i32(), op, b"") }
}

/// Encode a standalone error reply (`REPLY | ERROR`) for `request_id` / `op`
/// carrying the `kerror` discriminant — and `reason`, up to 64 bytes, for a person reading the
/// error — into `reply`, returning its length. The `op`
/// must match the request's so the kernel routes the error to the right pending
/// operation (a lookup vs a fill). Exposed for the server loop's fallback (e.g. if
/// it cannot materialise an object it already resolved).
pub fn encode_error(reply: &mut [u8], request_id: u64, kerror: i32, op: u16, reason: &[u8]) -> usize {
    let mut body = [0u8; ERROR_BODY_LEN + 64];
    let reason = &reason[..reason.len().min(64)];
    let body_len = error_body(&mut body, kerror, 0, reason).unwrap_or(0);
    encode(reply, op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &body[..body_len], 0).unwrap_or(0)
}

/// Longest reason a refusal carries. Every reason a server's library gives fits with room to
/// spare; a longer one is cut here rather than failing the refusal.
pub const MAX_REASON: usize = 192;

/// Encode the message a server that cannot serve its device sends **in place of** `Meta::Ready`:
/// the `Ready` op with `RS_FLAG_ERROR`, an `ErrorBody` whose message says why, and no handle
/// (`docs/spec/rsproto-wire-format.md` § Meta::Ready). Returns its length, or `None` if `out`
/// cannot hold it.
///
/// The supervisor prints the reason with what only it knows — the device and the mount point —
/// so the server does not print it too.
pub fn encode_refusal(out: &mut [u8], why: &impl Refusal) -> Option<usize> {
    let mut reason = Truncating { buf: [0; MAX_REASON], len: 0 };
    // `Truncating` never fails a write, so neither does this.
    let _ = core::fmt::write(&mut reason, format_args!("{why}"));
    let mut body = [0u8; ERROR_BODY_LEN + MAX_REASON];
    let body_len = error_body(&mut body, kerror(why.fs_error()).as_i32(), 0, &reason.buf[..reason.len])?;
    encode(out, librsproto::OP_READY, 0, RS_FLAG_ERROR, &body[..body_len], 0)
}

/// A fixed buffer that keeps what fits and drops the rest.
struct Truncating {
    buf: [u8; MAX_REASON],
    len: usize,
}

impl core::fmt::Write for Truncating {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(MAX_REASON - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// **The [`KError`] a reply carries for an [`FsError`].**
///
/// Four of these arms were wrong or lossy until the 2026-07-30 ABI pass, and the
/// pattern in all four was the same: a `KError` that said what happened existed,
/// and the mapping reached for a vaguer one anyway. `Exists`/`NotEmpty` collapsed
/// into `InvalidArgument` (the documented `fs-error-granularity` deferral), but also
/// `TooLarge` reported `OutOfMemory` — the kernel is not out of memory, the file
/// exceeded the caller's buffer, and `KError::TooLarge` has meant exactly that
/// since the beginning — and `Io` reported `KernelError`, so a failing disk was
/// indistinguishable from a bug in the server.
///
/// `Corrupt` still shares `IoError` with `Io`: from a client's side both mean "the
/// medium did not yield what it should have", and neither changes what the client
/// can do. The distinction is real but it is *server-specific*, which is what the
/// wire's `server_code` field is for (`docs/spec/rsproto-wire-format.md`); it is
/// left unpopulated until something consumes it, rather than spending a kernel
/// discriminant on a difference only this server can observe.
///
/// **One mapping since Phase 6 Part E.1.** `fs-server-ext4` had two: this one in its loop, and one
/// in its resolve core still collapsing `Exists` and `NotEmpty` into `InvalidArgument`. No resolve
/// or range read produces either, so taking this one for both changed no reply.
pub fn kerror(e: FsError) -> KError {
    match e {
        FsError::NotFound => KError::NotFound,
        FsError::Unsupported => KError::Unsupported,
        FsError::TooLarge => KError::TooLarge,
        FsError::Exists => KError::AlreadyExists,
        FsError::NotEmpty => KError::NotEmpty,
        FsError::Corrupt | FsError::Io => KError::IoError,
        FsError::ReadOnly => KError::NoAccess,
        FsError::InvalidName => KError::InvalidArgument,
    }
}

/// What a person is told beside a refusal's `KError` — the reason, when it is not in the code.
pub fn reason(e: FsError) -> &'static [u8] {
    if e == FsError::ReadOnly { b"read-only mount" } else { b"" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_longer_than_the_limit_is_cut_rather_than_refused() {
        let mut t = Truncating { buf: [0; MAX_REASON], len: 0 };
        let long = "x".repeat(MAX_REASON + 40);
        assert!(core::fmt::write(&mut t, format_args!("{long}")).is_ok());
        assert_eq!(t.len, MAX_REASON);
    }
}
