//! The request→reply logic for the forwarded ops the kernel sends — the pure core
//! of the server loop, kept out of `main.rs` so it is **host-testable** against the
//! `mke2fs` fixture (it is generic over [`BlockReader`], exactly like the parser).
//!
//! [`serve`] dispatches by op: a `Namespace::Resolve` ([`serve_resolve`]) resolves a
//! path — lazily (reply `OBJECT_KIND_FILE` + the file size; the kernel builds the
//! page-cache object) or eagerly (reply a `MemoryObject` of the whole content) — and
//! a `File::ReadRange` ([`serve_read_range`]) reads one byte range of a file (the
//! page-cache fill; reply a `MemoryObject` of the range). The syscall plumbing
//! (materialising/transferring the `MemoryObject`, recv/send) lives in `main.rs`;
//! this module touches no syscalls. An error reply carries the op of its request so
//! the kernel routes it to the right pending operation (lookup vs fill).

use crate::{BlockReader, FsError, ext4};
use libkern::KError;
use librsproto::file::{READ_RANGE_REPLY_LEN, parse_read_range_request, read_range_reply};
use librsproto::namespace::{
    OBJECT_KIND_FILE, OBJECT_KIND_MEMOBJ, RESOLVE_FILE_LAZY, RESOLVE_REPLY_LEN,
    parse_resolve_request, resolve_reply,
};
use librsproto::{
    OP_FILE_READ_RANGE, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode,
};
use librsproto::error::{ERROR_BODY_LEN, error_body};

/// Largest suffix the server resolves (bounds the on-stack path buffer). A path
/// longer than this resolves to `TooLarge` — far beyond any real filesystem path.
pub const MAX_SUFFIX: usize = 1024;

/// What the caller (`main.rs`) should do with the reply a serve fn built.
pub enum Served {
    /// Success: `reply[..reply_len]` is the rsproto reply; the caller transfers a
    /// read-only `MemoryObject` of `content[..content_len]` in `IpcMsg.handles[0]`.
    /// Both an eager resolve and a `ReadRange` fill produce this.
    File { reply_len: usize, content_len: usize },
    /// Success with **no** transferred handle: `reply[..reply_len]`. A lazy resolve
    /// (the reply carries the file size; the kernel builds the object itself).
    Lazy { reply_len: usize },
    /// An error reply (no handle transferred): `reply[..reply_len]`.
    Error { reply_len: usize },
}

/// Serve one forwarded request: dispatch by op to [`serve_resolve`] (a
/// `Namespace::Resolve`) or [`serve_read_range`] (a `File::ReadRange`). An
/// undecodable request or an unknown op yields an error reply.
pub fn serve<R: BlockReader>(
    reader: &R,
    request: &[u8],
    content: &mut [u8],
    reply: &mut [u8],
) -> Served {
    match decode(request) {
        Ok(m) if m.op == OP_NS_RESOLVE => serve_resolve(reader, request, content, reply),
        Ok(m) if m.op == OP_FILE_READ_RANGE => serve_read_range(reader, request, content, reply),
        // Known envelope, unknown op: reply Unsupported with that op.
        Ok(m) => error_reply(reply, m.request_id, KError::Unsupported, m.op),
        // Undecodable: no recoverable id/op; reply a Resolve-shaped error, id 0.
        Err(_) => error_reply(reply, 0, KError::InvalidArgument, OP_NS_RESOLVE),
    }
}

/// Serve one forwarded `Namespace::Resolve`: parse `request`, resolve `"/" + suffix`
/// (the mount is the filesystem root) to a regular file, and build the reply. With
/// `RESOLVE_FILE_LAZY` (the slice-8 kernel) the reply is **lazy** — `OBJECT_KIND_FILE`
/// + the file size, no content read or handle (the kernel builds the page-cache
/// object). Otherwise it is **eager** — the whole content read into `content` and
/// named by a transferred `MemoryObject` (`OBJECT_KIND_MEMOBJ`, the slice-7 path,
/// capped at [`ext4::MAX_FILE`]). The `request_id` is echoed; a malformed/oversized
/// request or any [`FsError`] yields an error reply.
pub fn serve_resolve<R: BlockReader>(
    reader: &R,
    request: &[u8],
    content: &mut [u8],
    reply: &mut [u8],
) -> Served {
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
        // Lazy: stat the file (no content read), reply the size + OBJECT_KIND_FILE.
        return match ext4::stat_file(reader, path) {
            Ok(size) if size > u32::MAX as usize => {
                error_reply(reply, request_id, KError::TooLarge, OP_NS_RESOLVE)
            }
            Ok(size) => match lazy_reply(reply, request_id, size) {
                Some(reply_len) => Served::Lazy { reply_len },
                None => error_reply(reply, request_id, KError::KernelError, OP_NS_RESOLVE),
            },
            Err(e) => error_reply(reply, request_id, fs_error_to_kerror(e), OP_NS_RESOLVE),
        };
    }

    // Eager (slice-7): read the whole file and name a MemoryObject of it.
    match ext4::read_file(reader, path, content) {
        Ok(size) => match success_reply(reply, request_id, size) {
            Some(reply_len) => Served::File { reply_len, content_len: size },
            // The caller's reply buffer is the 4 KiB IPC payload — far larger than a
            // RESOLVE_REPLY — so this is unreachable; degrade to an error reply.
            None => error_reply(reply, request_id, KError::KernelError, OP_NS_RESOLVE),
        },
        Err(e) => error_reply(reply, request_id, fs_error_to_kerror(e), OP_NS_RESOLVE),
    }
}

/// Serve one forwarded `File::ReadRange` (the page-cache fill): parse `request`,
/// read the requested byte range of `"/" + suffix` into `content`, and reply naming
/// a `MemoryObject` of the bytes read (the caller transfers it). The fill is
/// **stateless** — the file is re-identified by the suffix each call. `content_len`
/// in the reply is the bytes actually read (≤ requested; a short tail at EOF leaves
/// the kernel's zeroed frame as padding). An error reply carries the `ReadRange` op.
pub fn serve_read_range<R: BlockReader>(
    reader: &R,
    request: &[u8],
    content: &mut [u8],
    reply: &mut [u8],
) -> Served {
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
    match ext4::read_file_range(reader, path, req.offset, len, content) {
        Ok(n) => match range_reply(reply, request_id, n) {
            Some(reply_len) => Served::File { reply_len, content_len: n },
            None => error_reply(reply, request_id, KError::KernelError, OP_FILE_READ_RANGE),
        },
        Err(e) => error_reply(reply, request_id, fs_error_to_kerror(e), OP_FILE_READ_RANGE),
    }
}

/// Build a success `ResolveReply` (object_kind `MEMOBJ`, the exact `content_len`)
/// into `reply`; `None` only if `reply` is too small.
fn success_reply(reply: &mut [u8], request_id: u64, content_len: usize) -> Option<usize> {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let body_len = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, content_len as u32)?;
    encode(reply, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body[..body_len], 1)
}

/// Build a lazy `ResolveReply` (object_kind `FILE`, `content_len` = the file size)
/// into `reply` — **no** transferred handle (`handle_count = 0`); the kernel builds
/// the page-cache object. `None` only if `reply` is too small.
fn lazy_reply(reply: &mut [u8], request_id: u64, size: usize) -> Option<usize> {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let body_len = resolve_reply(&mut body, OBJECT_KIND_FILE, size as u32)?;
    encode(reply, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body[..body_len], 0)
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
    Served::Error { reply_len: encode_error(reply, request_id, err.as_i32(), op) }
}

/// Encode a standalone error reply (`REPLY | ERROR`) for `request_id` / `op`
/// carrying the `kerror` discriminant into `reply`, returning its length. The `op`
/// must match the request's so the kernel routes the error to the right pending
/// operation (a lookup vs a fill). Exposed for the server loop's fallback (e.g. if
/// it cannot materialise an object it already resolved).
pub fn encode_error(reply: &mut [u8], request_id: u64, kerror: i32, op: u16) -> usize {
    let mut body = [0u8; ERROR_BODY_LEN];
    let body_len = error_body(&mut body, kerror, 0, b"").unwrap_or(0);
    encode(
        reply,
        op,
        request_id,
        RS_FLAG_REPLY | RS_FLAG_ERROR,
        &body[..body_len],
        0,
    )
    .unwrap_or(0)
}

/// Map a reader [`FsError`] to the [`KError`] the lookup PO completes with.
fn fs_error_to_kerror(e: FsError) -> KError {
    match e {
        // A device read failure or a malformed on-disk structure is, to the client,
        // a medium-level I/O failure.
        FsError::Io | FsError::Corrupt => KError::IoError,
        FsError::Unsupported => KError::Unsupported,
        FsError::NotFound => KError::NotFound,
        FsError::TooLarge => KError::TooLarge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ImageReader, fixture};
    use librsproto::file::{parse_read_range_reply, read_range_request};
    use librsproto::namespace::{RESOLVE_FILE_AS_MEMOBJ, parse_resolve_reply, resolve_request};
    use librsproto::error::parse_error;

    /// Build a `Namespace::Resolve` request for `suffix` (the kernel's wire form).
    fn make_request(request_id: u64, suffix: &[u8]) -> ([u8; 512], usize) {
        let mut body = [0u8; 256];
        let body_len =
            resolve_request(&mut body, /*requested_rights*/ 0x8000, RESOLVE_FILE_AS_MEMOBJ, suffix)
                .unwrap();
        let mut buf = [0u8; 512];
        let n = encode(&mut buf, OP_NS_RESOLVE, request_id, /*flags*/ 0, &body[..body_len], 0)
            .unwrap();
        (buf, n)
    }

    #[test]
    fn resolves_a_file_to_a_memobj_reply() {
        let r = ImageReader(fixture(1024, b"nitrox-gen-0001\n"));
        let (req, req_len) = make_request(42, b"system/current-generation");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];

        match serve_resolve(&r, &req[..req_len], &mut content, &mut reply) {
            Served::File { reply_len, content_len } => {
                assert_eq!(&content[..content_len], b"nitrox-gen-0001\n");
                // The reply is a success ResolveReply echoing the request id.
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.op, OP_NS_RESOLVE);
                assert_eq!(m.request_id, 42);
                assert!(m.is_reply() && !m.is_error());
                assert_eq!(m.handle_count, 1);
                let rr = parse_resolve_reply(m.body).unwrap();
                assert_eq!(rr.object_kind, OBJECT_KIND_MEMOBJ);
                assert_eq!(rr.content_len as usize, content_len);
            }
            _ => panic!("expected a File reply"),
        }
    }

    #[test]
    fn missing_path_yields_a_not_found_error_reply() {
        let r = ImageReader(fixture(1024, b"x\n"));
        let (req, req_len) = make_request(7, b"system/nope");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];

        match serve_resolve(&r, &req[..req_len], &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.request_id, 7);
                assert!(m.is_reply() && m.is_error());
                assert_eq!(m.handle_count, 0);
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::NotFound.as_i32());
            }
            _ => panic!("expected an Error reply"),
        }
    }

    #[test]
    fn a_directory_is_not_a_regular_file() {
        let r = ImageReader(fixture(1024, b"x\n"));
        let (req, req_len) = make_request(1, b"system"); // a directory
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve_resolve(&r, &req[..req_len], &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::NotFound.as_i32());
            }
            _ => panic!("a directory must not resolve to a file"),
        }
    }

    #[test]
    fn a_non_resolve_op_is_unsupported() {
        let r = ImageReader(fixture(1024, b"x\n"));
        // A well-formed envelope with the wrong op (Ping).
        let mut buf = [0u8; 64];
        let n = encode(&mut buf, librsproto::OP_PING, 9, 0, &[], 0).unwrap();
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve_resolve(&r, &buf[..n], &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.request_id, 9);
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::Unsupported.as_i32());
            }
            _ => panic!("expected an Error reply"),
        }
    }

    #[test]
    fn a_garbage_request_replies_invalid_argument_id_zero() {
        let r = ImageReader(fixture(1024, b"x\n"));
        let garbage = [0u8; 8]; // too short / bad magic
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve_resolve(&r, &garbage, &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.request_id, 0); // unrecoverable id
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::InvalidArgument.as_i32());
            }
            _ => panic!("expected an Error reply"),
        }
    }

    /// Build a `RESOLVE_FILE_LAZY` resolve request (the slice-8 kernel's form).
    fn make_lazy_request(request_id: u64, suffix: &[u8]) -> ([u8; 512], usize) {
        let mut body = [0u8; 256];
        let body_len =
            resolve_request(&mut body, 0x8000, RESOLVE_FILE_LAZY, suffix).unwrap();
        let mut buf = [0u8; 512];
        let n = encode(&mut buf, OP_NS_RESOLVE, request_id, 0, &body[..body_len], 0).unwrap();
        (buf, n)
    }

    /// Build a `File::ReadRange` request (the page-cache fill's form).
    fn make_range_request(request_id: u64, offset: u64, len: u32, suffix: &[u8]) -> ([u8; 512], usize) {
        let mut body = [0u8; 256];
        let body_len = read_range_request(&mut body, offset, len, suffix).unwrap();
        let mut buf = [0u8; 512];
        let n = encode(&mut buf, OP_FILE_READ_RANGE, request_id, 0, &body[..body_len], 0).unwrap();
        (buf, n)
    }

    #[test]
    fn lazy_resolve_replies_file_kind_with_size_no_handle() {
        let r = ImageReader(fixture(1024, b"nitrox-gen-0001\n")); // 16 bytes
        let (req, req_len) = make_lazy_request(11, b"system/current-generation");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];

        match serve(&r, &req[..req_len], &mut content, &mut reply) {
            Served::Lazy { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.op, OP_NS_RESOLVE);
                assert_eq!(m.request_id, 11);
                assert!(m.is_reply() && !m.is_error());
                assert_eq!(m.handle_count, 0); // the kernel builds the object
                let rr = parse_resolve_reply(m.body).unwrap();
                assert_eq!(rr.object_kind, OBJECT_KIND_FILE);
                assert_eq!(rr.content_len, 16); // the file size
            }
            _ => panic!("expected a Lazy reply"),
        }
    }

    #[test]
    fn read_range_serves_a_byte_window() {
        let content_bytes = b"0123456789ABCDEF\n"; // 17 bytes
        let r = ImageReader(fixture(1024, content_bytes));
        let (req, req_len) = make_range_request(22, 4, 6, b"system/current-generation");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];

        match serve(&r, &req[..req_len], &mut content, &mut reply) {
            Served::File { reply_len, content_len } => {
                assert_eq!(content_len, 6);
                assert_eq!(&content[..content_len], b"456789");
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.op, OP_FILE_READ_RANGE);
                assert_eq!(m.request_id, 22);
                assert_eq!(m.handle_count, 1);
                let rr = parse_read_range_reply(m.body).unwrap();
                assert_eq!(rr.content_len, 6);
            }
            _ => panic!("expected a File reply"),
        }
    }

    #[test]
    fn read_range_tail_clamps_at_eof() {
        let r = ImageReader(fixture(1024, b"ABCDEFG\n")); // 8 bytes
        // Ask a full page from offset 4 → only 4 bytes remain.
        let (req, req_len) = make_range_request(23, 4, 4096, b"system/current-generation");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve(&r, &req[..req_len], &mut content, &mut reply) {
            Served::File { content_len, .. } => {
                assert_eq!(content_len, 4);
                assert_eq!(&content[..content_len], b"EFG\n");
            }
            _ => panic!("expected a File reply"),
        }
    }

    #[test]
    fn read_range_error_carries_the_read_range_op() {
        // A ReadRange for a missing file must reply with the ReadRange op so the
        // kernel routes the error to the pending fill (not a lookup) — else the
        // faulting thread would hang.
        let r = ImageReader(fixture(1024, b"x\n"));
        let (req, req_len) = make_range_request(24, 0, 4096, b"system/nope");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve(&r, &req[..req_len], &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.op, OP_FILE_READ_RANGE);
                assert_eq!(m.request_id, 24);
                assert!(m.is_error());
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::NotFound.as_i32());
            }
            _ => panic!("expected an Error reply"),
        }
    }
}
