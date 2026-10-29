//! The request→reply logic for the forwarded ops the kernel sends — the pure core of the server
//! loop, **host-testable** because it touches no syscalls: generic over [`Volume`], each server's
//! crate tests it through its own volume. An error reply carries the op of its request so the
//! kernel routes it to the right pending operation (lookup vs fill).

use crate::{BlockRun, DirEntry, FsError, Mapped, RUN_UNWRITTEN, Refusal, Volume};
use libkern::KError;
use librsproto::error::{ERROR_BODY_LEN, error_body};
use librsproto::file::{
    DirReplyWriter, READ_RANGE_REPLY_LEN, parse_name_request, parse_read_dir_request, parse_read_range_request,
    parse_rename_request, read_range_reply,
};
use librsproto::namespace::{
    BLOCK_RUN_WIRE_LEN, FILE_BLOCKS_PREFIX_LEN, FILE_BLOCKS_READ_ONLY, OBJECT_KIND_MEMOBJ, RESOLVE_CREATE,
    RESOLVE_FILE_LAZY, RESOLVE_GROW, RESOLVE_REPLY_LEN, RESOLVE_TRUNCATE, file_blocks_prefix, parse_resolve_grow_size,
    parse_resolve_request, resolve_reply,
};
use librsproto::{OP_FILE_MKDIR, OP_FILE_READ_DIR, OP_FILE_RENAME, OP_FILE_RMDIR, OP_FILE_TOUCH, OP_FILE_UNLINK};
use librsproto::{OP_FILE_READ_RANGE, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

/// **Whether a session may name `name`** in its directory: one entry, never `.` or `..`, which
/// name the directory and its parent, a way out of a directory a session is confined to. How long
/// a name may be is each filesystem's: ext4's 255 bytes, FAT's 255 UTF-16 units, up to 765 bytes.
pub fn entry_name(name: &[u8]) -> bool {
    !name.is_empty() && !name.contains(&b'/') && name != b"." && name != b".."
}

/// Largest suffix the server resolves (bounds the on-stack path buffer). A longer path
/// resolves to `TooLarge`.
pub const MAX_SUFFIX: usize = 1024;

/// **Largest file an eager resolve reads whole**: the server's content buffer.
pub const MAX_FILE: usize = 64 * 1024;

/// What the caller ([`crate::server`]) should do with the reply a serve fn built.
pub enum Served {
    /// The caller transfers a read-only `MemoryObject` of `content[..content_len]` in
    /// `IpcMsg.handles[0]`: an eager resolve or a `ReadRange` fill.
    File { reply_len: usize, content_len: usize },
    /// A **Model A** lazy resolve: the caller transfers a `READ | TRANSFER` **duplicate of the
    /// block-device handle** in `IpcMsg.handles[0]`; the original stays for metadata reads.
    LazyBlocks { reply_len: usize },
    /// An error reply (no handle transferred): `reply[..reply_len]`.
    Error { reply_len: usize },
}

/// Largest `BlockRun` map inlined in a Model A resolve reply (1536 body bytes, inside the 4 KiB
/// IPC payload). A more fragmented file is `TooLarge` until `TODO(map-range)` lands.
pub const MAX_RUNS: usize = 64;

/// The `(request_id, directory)` of a resolve whose suffix names a directory, which opens a session.
/// **A suffix over [`MAX_SUFFIX`] opens none**, and [`serve`] refuses it `TooLarge`, rather than
/// opening what its first [`MAX_SUFFIX`] bytes name, an ancestor or a sibling cut short.
pub fn directory_open<V: Volume>(vol: &V, request: &[u8]) -> Option<(u64, u64)> {
    let m = decode(request).ok().filter(|m| m.op == OP_NS_RESOLVE)?;
    let r = parse_resolve_request(m.body).filter(|r| r.suffix.len() <= MAX_SUFFIX)?;
    Some((m.request_id, vol.resolve_dir(r.suffix).ok()?))
}

/// **The reply to one request on a directory session** bound to `dir`, encoded into `reply`: its
/// length, or `None` for a message that does not decode. Names are read where they arrived, at any
/// length the message holds. A file whose last name goes is handed to `release`; a directory
/// removed, to `freeing`.
#[allow(clippy::too_many_arguments, reason = "each is a part the server's one call plays")]
pub fn session<V: Volume>(
    vol: &V,
    dir: u64,
    request: &[u8],
    reply: &mut [u8],
    listing: &mut [u8],
    now: i64,
    mut release: impl FnMut(u64),
    freeing: impl FnOnce(u64),
) -> Option<usize> {
    let m = decode(request).ok()?;
    let (op, id) = (m.op, m.request_id);
    let r = match op {
        OP_FILE_READ_DIR => {
            let cursor = parse_read_dir_request(m.body).map(|r| r.cursor).unwrap_or(0);
            let listed = DirReplyWriter::new(listing).and_then(|mut w| {
                let next =
                    vol.read_dir(dir, cursor, |e: &DirEntry| w.push(e.id, e.kind, e.mode, e.size, e.mtime, e.name));
                Some(w.finish(next.ok()?))
            });
            // A listing too long to send is answered with an error: its client waits for exactly this.
            let encoded = listed.and_then(|n| encode(reply, op, id, RS_FLAG_REPLY, &listing[..n], 0));
            return Some(encoded.unwrap_or_else(|| encode_error(reply, id, KError::KernelError.as_i32(), op, b"")));
        }
        // Each names an entry in the bound directory, so a session can never change outside it.
        OP_FILE_MKDIR | OP_FILE_UNLINK | OP_FILE_RMDIR | OP_FILE_TOUCH => match parse_name_request(m.body) {
            Some(name) if entry_name(name) => match op {
                OP_FILE_MKDIR => vol.mkdir_at(dir, name, now),
                // The last name's removal frees the file only once the kernel has forgotten it, and
                // before the client hears it is gone.
                OP_FILE_UNLINK => vol.unlink_at(dir, name, now).map(|orphan| orphan.into_iter().for_each(&mut release)),
                OP_FILE_RMDIR => vol.rmdir_at(dir, name, now, freeing),
                _ => vol.touch_at(dir, name, now),
            },
            _ => Err(FsError::Unsupported),
        },
        OP_FILE_RENAME => match parse_rename_request(m.body) {
            Some((old, new)) if entry_name(old) && entry_name(new) => vol.rename_at(dir, old, new, now),
            _ => Err(FsError::Unsupported),
        },
        _ => return Some(encode_error(reply, id, KError::Unsupported.as_i32(), op, b"")),
    };
    match r {
        Ok(()) => encode(reply, op, id, RS_FLAG_REPLY, &[], 0),
        Err(e) => Some(encode_error(reply, id, kerror(e).as_i32(), op, reason(e))),
    }
}

/// Serve one forwarded request: dispatch by op to [`serve_resolve`] or [`serve_read_range`]. An
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

/// Serve one forwarded `Namespace::Resolve` of `"/" + suffix` (the mount is the filesystem root).
/// With `RESOLVE_FILE_LAZY` the reply is **lazy**: `OBJECT_KIND_FILE_BLOCKS`, the size and the
/// map, no content read. Otherwise it is **eager**: the whole content (capped at [`MAX_FILE`])
/// named by a transferred `MemoryObject`. A malformed or oversized request or any [`FsError`]
/// yields an error reply.
pub fn serve_resolve<V: Volume>(vol: &V, request: &[u8], content: &mut [u8], reply: &mut [u8]) -> Served {
    resolve(vol, request, content, reply, None)
}

/// What a resolve's grow or truncate changed, for the server to finish: the size a grow took the
/// file from, which [`serve_grown`] marks past, and the id a truncate ended, to forget and release.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Resized {
    /// The size a grow took the file from, if it grew it.
    pub grown_from: Option<u64>,
    /// The id a truncate ended.
    pub ended: Option<u64>,
}

/// **Apply a resolve's `RESOLVE_CREATE`, then its `RESOLVE_GROW` or `RESOLVE_TRUNCATE`**, before it
/// is served; one with neither changes nothing. **A create's or a grow's failure fails the
/// resolve**: each gives back what it took, and served anyway, the file at its old size was
/// installed as the size asked for. **A truncate's is served**, the file as it now is: it may have
/// freed blocks the kernel's map still names, and only the new map lets them go. Its read-only
/// refusal, which changed nothing, fails it.
pub fn apply_size<V: Volume>(vol: &V, request: &[u8], now: i64) -> Result<Resized, FsError> {
    let none = Ok(Resized::default());
    let Ok(m) = decode(request) else { return none };
    let Some(r) = parse_resolve_request(m.body).filter(|_| m.op == OP_NS_RESOLVE) else { return none };
    if r.flags & (RESOLVE_GROW | RESOLVE_TRUNCATE) == 0 || r.suffix.len() > MAX_SUFFIX {
        return none;
    }
    let Some(new_size) = parse_resolve_grow_size(m.body) else { return none };
    let mut buf = [0u8; MAX_SUFFIX + 1];
    buf[0] = b'/';
    buf[1..1 + r.suffix.len()].copy_from_slice(r.suffix);
    let path = &buf[..1 + r.suffix.len()];
    if r.flags & RESOLVE_CREATE != 0
        && let Some(slash) = path.iter().rposition(|&b| b == b'/')
    {
        let parent = if slash == 0 { &b"/"[..] } else { &path[..slash] };
        vol.create_file(parent, &path[slash + 1..], now)?;
    }
    if r.flags & RESOLVE_TRUNCATE != 0 {
        return match vol.truncate_file(path, new_size as usize, now) {
            Ok(ended) => Ok(Resized { grown_from: None, ended }),
            Err(FsError::ReadOnly) => Err(FsError::ReadOnly),
            Err(_) => Ok(Resized::default()),
        };
    }
    let was = vol.grow_file(path, new_size as usize, now)?;
    Ok(Resized { grown_from: (was < new_size as u64).then_some(was), ended: None })
}

/// [`serve_resolve`] for a resolve whose grow took the file from `grown_from` bytes: **what the
/// grow added is marked [`RUN_UNWRITTEN`]** ([`mark_grown`]), so the kernel fills it as zero
/// without reading it, as [`Volume::grow_file`] promises. Only this reply marks it; a later
/// resolve maps the same blocks unmarked.
pub fn serve_grown<V: Volume>(
    vol: &V,
    request: &[u8],
    content: &mut [u8],
    reply: &mut [u8],
    grown_from: u64,
) -> Served {
    resolve(vol, request, content, reply, Some(grown_from))
}

/// [`serve_resolve`] and [`serve_grown`]: `grown_from`, if a grow took the file from that size.
fn resolve<V: Volume>(
    vol: &V,
    request: &[u8],
    content: &mut [u8],
    reply: &mut [u8],
    grown_from: Option<u64>,
) -> Served {
    // Decode the envelope first, so even an unusable request's error reply correlates.
    let msg = match decode(request) {
        Ok(m) => m,
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

    // The binding is the filesystem root and the kernel strips the leading '/'.
    let mut path_buf = [0u8; MAX_SUFFIX + 1];
    path_buf[0] = b'/';
    path_buf[1..1 + req.suffix.len()].copy_from_slice(req.suffix);
    let path = &path_buf[..1 + req.suffix.len()];

    if req.flags & RESOLVE_FILE_LAZY != 0 {
        let mut runs = [BlockRun::default(); MAX_RUNS];
        return match vol.map_file(path, &mut runs) {
            Ok(m) if m.size > u32::MAX as usize => error_reply(reply, request_id, KError::TooLarge, OP_NS_RESOLVE),
            Ok(m) => {
                // A read-only mount's files are marked so, and a mount refusing changes since it
                // found the filesystem corrupt: the kernel installs them without `MAP_WRITE`.
                let flags = if vol.read_only() || vol.refusing_changes() { FILE_BLOCKS_READ_ONLY } else { 0 };
                let n = match grown_from {
                    Some(was) if m.block_size > 0 => mark_grown(&mut runs, m.runs, was.div_ceil(m.block_size as u64)),
                    _ => m.runs,
                };
                match model_a_reply(reply, request_id, &m, &runs[..n], flags) {
                    Some(reply_len) => Served::LazyBlocks { reply_len },
                    None => error_reply(reply, request_id, KError::KernelError, OP_NS_RESOLVE),
                }
            }
            Err(e) => error_reply(reply, request_id, kerror(e), OP_NS_RESOLVE),
        };
    }

    match vol.read_file(path, content) {
        Ok(size) => match success_reply(reply, request_id, size) {
            Some(reply_len) => Served::File { reply_len, content_len: size },
            // The reply buffer is the 4 KiB IPC payload, far larger than a RESOLVE_REPLY.
            None => error_reply(reply, request_id, KError::KernelError, OP_NS_RESOLVE),
        },
        Err(e) => error_reply(reply, request_id, kerror(e), OP_NS_RESOLVE),
    }
}

/// Serve one forwarded `File::ReadRange` (the page-cache fill) of `"/" + suffix`. It is
/// **stateless**: the file is re-identified by the suffix each call. `content_len` is the bytes
/// actually read; a short tail at EOF leaves the kernel's zeroed frame as padding.
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

/// Build a success `ResolveReply` (object_kind `MEMOBJ`).
fn success_reply(reply: &mut [u8], request_id: u64, content_len: usize) -> Option<usize> {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let body_len = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, content_len as u32)?;
    encode(reply, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body[..body_len], 1)
}

/// Build a **Model A** lazy `ResolveReply`: `OBJECT_KIND_FILE_BLOCKS`, `content_len` = file size,
/// then the block size, the file's id and the `BlockRun` map, `handle_count = 1`. Body layout:
/// `rsproto-namespace-ops.md` § *The `FILE_BLOCKS` body*.
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

/// **Mark what a grow added**: of the first `n` runs, every one from file block
/// `fresh` on — the first block wholly past the file's old end — is flagged [`RUN_UNWRITTEN`], and
/// one across `fresh` is split in two, so the blocks before it go on reading from the device. A
/// hole stays a hole. With no room left to split, the run across is left as it is, and reads from
/// the device, which holds zeroes there: slower, never wrong. How many runs there are after.
pub(crate) fn mark_grown(runs: &mut [BlockRun], n: usize, fresh: u64) -> usize {
    let (mut n, mut i) = (n.min(runs.len()), 0);
    while i < n {
        let r = runs[i];
        let end = r.file_block + r.length as u64;
        if r.device_lba == 0 || end <= fresh {
            i += 1;
        } else if r.file_block >= fresh {
            runs[i].flags |= RUN_UNWRITTEN;
            i += 1;
        } else if n < runs.len() {
            let before = (fresh - r.file_block) as u32;
            runs.copy_within(i + 1..n, i + 2);
            runs[i].length = before;
            runs[i + 1] = BlockRun {
                file_block: fresh,
                device_lba: r.device_lba + before as u64,
                length: r.length - before,
                flags: r.flags | RUN_UNWRITTEN,
            };
            n += 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    n
}

/// Build a success `ReadRangeReply`; the bytes ride in `handles[0]`.
fn range_reply(reply: &mut [u8], request_id: u64, content_len: usize) -> Option<usize> {
    let mut body = [0u8; READ_RANGE_REPLY_LEN];
    let body_len = read_range_reply(&mut body, content_len as u32)?;
    encode(reply, OP_FILE_READ_RANGE, request_id, RS_FLAG_REPLY, &body[..body_len], 1)
}

fn error_reply(reply: &mut [u8], request_id: u64, err: KError, op: u16) -> Served {
    Served::Error { reply_len: encode_error(reply, request_id, err.as_i32(), op, b"") }
}

/// Encode an error reply (`REPLY | ERROR`) carrying the `kerror` discriminant and `reason`, up to
/// 64 bytes, into `reply`, returning its length. The `op` must match the request's so the kernel
/// routes the error to the right pending operation.
pub fn encode_error(reply: &mut [u8], request_id: u64, kerror: i32, op: u16, reason: &[u8]) -> usize {
    let mut body = [0u8; ERROR_BODY_LEN + 64];
    let reason = &reason[..reason.len().min(64)];
    let body_len = error_body(&mut body, kerror, 0, reason).unwrap_or(0);
    encode(reply, op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &body[..body_len], 0).unwrap_or(0)
}

/// Longest refusal reason; a longer one is cut rather than failing the refusal.
pub const MAX_REASON: usize = 192;

/// Encode what a server that cannot serve its device sends **in place of** `Meta::Ready`: the
/// `Ready` op with `RS_FLAG_ERROR`, an `ErrorBody` saying why, and no handle
/// (`docs/spec/rsproto-wire-format.md` § Meta::Ready). The supervisor prints the reason with the
/// device and mount point, so the server does not.
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

/// **The [`KError`] a reply carries for an [`FsError`]**, the one mapping for both servers. A
/// failing disk (`Io`) must not read as a bug in the server, nor `TooLarge` as the kernel out of
/// memory. `Corrupt` shares `IoError` with `Io`: the distinction is *server-specific*, which the
/// wire's `server_code` field is for (`docs/spec/rsproto-wire-format.md`), unpopulated until
/// something consumes it.
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
    fn a_grows_reply_marks_what_it_added_and_nothing_before_it() {
        let run = |file_block, device_lba, length| BlockRun { file_block, device_lba, length, flags: 0 };
        let fresh =
            |file_block, device_lba, length| BlockRun { flags: RUN_UNWRITTEN, ..run(file_block, device_lba, length) };
        let mut runs = [BlockRun::default(); 6];
        runs[..4].copy_from_slice(&[run(0, 100, 3), run(3, 200, 4), run(7, 0, 2), run(9, 300, 5)]);
        let n = mark_grown(&mut runs, 4, 5);
        assert_eq!(
            runs[..n],
            [run(0, 100, 3), run(3, 200, 2), fresh(5, 202, 2), run(7, 0, 2), fresh(9, 300, 5)],
            "split at block 5; the hole left alone"
        );

        let mut runs = [run(0, 100, 3), run(3, 200, 4), BlockRun::default()];
        assert_eq!(mark_grown(&mut runs, 2, 3), 2);
        assert_eq!(runs[..2], [run(0, 100, 3), fresh(3, 200, 4)]);

        let mut runs = [run(0, 100, 3), run(3, 200, 4)];
        assert_eq!(mark_grown(&mut runs, 2, 1), 2);
        assert_eq!(runs, [run(0, 100, 3), fresh(3, 200, 4)]);

        let mut runs = [run(0, 100, 3)];
        assert_eq!((mark_grown(&mut runs, 1, 0), runs[0]), (1, fresh(0, 100, 3)));
        let mut runs = [run(0, 100, 3)];
        assert_eq!((mark_grown(&mut runs, 1, 3), runs[0]), (1, run(0, 100, 3)));
    }

    #[test]
    fn a_reason_longer_than_the_limit_is_cut_rather_than_refused() {
        let mut t = Truncating { buf: [0; MAX_REASON], len: 0 };
        let long = "x".repeat(MAX_REASON + 40);
        assert!(core::fmt::write(&mut t, format_args!("{long}")).is_ok());
        assert_eq!(t.len, MAX_REASON);
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;
