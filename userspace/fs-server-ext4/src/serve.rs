//! The request→reply logic for a forwarded `Namespace::Resolve` — the pure core
//! of the server loop, kept out of `main.rs` so it is **host-testable** against the
//! `mke2fs` fixture (it is generic over [`BlockReader`], exactly like the parser).
//!
//! [`serve_resolve`] parses the kernel's rsproto request, reads the named file via
//! the reader, and builds the reply bytes — the success reply names a
//! `MemoryObject` of the file content (which the caller then materialises +
//! transfers), an error reply carries the `KError`. The syscall plumbing
//! (creating/filling/transferring the `MemoryObject`, recv/send) lives in
//! `main.rs`; this module touches no syscalls.

use crate::{BlockReader, FsError, ext4};
use libkern::KError;
use librsproto::namespace::{OBJECT_KIND_MEMOBJ, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use librsproto::error::{ERROR_BODY_LEN, error_body};

/// Largest suffix the server resolves (bounds the on-stack path buffer). A path
/// longer than this resolves to `TooLarge` — far beyond any real filesystem path.
pub const MAX_SUFFIX: usize = 1024;

/// What the caller (`main.rs`) should do with the reply [`serve_resolve`] built.
pub enum Served {
    /// Success: `reply[..reply_len]` is the rsproto reply; the caller transfers a
    /// read-only `MemoryObject` of `content[..content_len]` in `IpcMsg.handles[0]`.
    File { reply_len: usize, content_len: usize },
    /// An error reply (no handle transferred): `reply[..reply_len]`.
    Error { reply_len: usize },
}

/// Serve one forwarded `Namespace::Resolve`: parse `request` (the rsproto request
/// payload the kernel sent), resolve `"/" + suffix` (the mount is the filesystem
/// root) to a regular file, read it into `content`, and build the reply into
/// `reply`. The request's `request_id` is echoed so the kernel correlates the
/// reply to its lookup. A malformed/oversized request, an unexpected op, or any
/// [`FsError`] yields an **error reply** (mapped to a [`KError`]); the lookup's
/// `PendingOperation` then completes with that status.
///
/// `content` must be at least [`ext4::MAX_FILE`] bytes (the 64 KiB read-model cap)
/// and `reply` at least the largest reply (a `RESOLVE_REPLY` is tiny). The reader
/// is generic so this is host-tested against an in-memory image.
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
        Err(_) => return error_reply(reply, 0, KError::InvalidArgument),
    };
    let request_id = msg.request_id;
    if msg.op != OP_NS_RESOLVE {
        return error_reply(reply, request_id, KError::Unsupported);
    }
    let req = match parse_resolve_request(msg.body) {
        Some(r) => r,
        None => return error_reply(reply, request_id, KError::InvalidArgument),
    };
    if req.suffix.len() > MAX_SUFFIX {
        return error_reply(reply, request_id, KError::TooLarge);
    }

    // Build the absolute path "/" + suffix (the binding is the filesystem root, so
    // the lookup suffix is the path under it; the kernel strips the leading '/').
    let mut path_buf = [0u8; MAX_SUFFIX + 1];
    path_buf[0] = b'/';
    path_buf[1..1 + req.suffix.len()].copy_from_slice(req.suffix);
    let path = &path_buf[..1 + req.suffix.len()];

    match ext4::read_file(reader, path, content) {
        Ok(size) => match success_reply(reply, request_id, size) {
            Some(reply_len) => Served::File { reply_len, content_len: size },
            // The caller's reply buffer is the 4 KiB IPC payload — far larger than a
            // RESOLVE_REPLY — so this is unreachable; degrade to an error reply.
            None => error_reply(reply, request_id, KError::KernelError),
        },
        Err(e) => error_reply(reply, request_id, fs_error_to_kerror(e)),
    }
}

/// Build a success `ResolveReply` (object_kind `MEMOBJ`, the exact `content_len`)
/// into `reply`; `None` only if `reply` is too small.
fn success_reply(reply: &mut [u8], request_id: u64, content_len: usize) -> Option<usize> {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let body_len = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, content_len as u32)?;
    encode(reply, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body[..body_len], 1)
}

/// Build an error reply (`REPLY | ERROR`, an `ErrorBody` carrying `err`) into
/// `reply`. The body has no message (slice 7 keeps replies minimal).
fn error_reply(reply: &mut [u8], request_id: u64, err: KError) -> Served {
    Served::Error { reply_len: encode_error(reply, request_id, err.as_i32()) }
}

/// Encode a standalone error reply (`REPLY | ERROR`) for `request_id` carrying the
/// `kerror` discriminant into `reply`, returning its length. Exposed for the server
/// loop's fallback: if it cannot *materialise* the success object it already
/// resolved (e.g. the `MemoryObject` allocation fails), it turns the reply into an
/// error rather than transferring nothing.
pub fn encode_error(reply: &mut [u8], request_id: u64, kerror: i32) -> usize {
    let mut body = [0u8; ERROR_BODY_LEN];
    let body_len = error_body(&mut body, kerror, 0, b"").unwrap_or(0);
    encode(
        reply,
        OP_NS_RESOLVE,
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
            Served::Error { .. } => panic!("expected a File reply"),
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
            Served::File { .. } => panic!("expected an Error reply"),
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
            Served::File { .. } => panic!("a directory must not resolve to a file"),
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
            Served::File { .. } => panic!("expected an Error reply"),
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
            Served::File { .. } => panic!("expected an Error reply"),
        }
    }
}
