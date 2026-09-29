//! `Meta::Unmount`, from `init`'s side (administration Part E.4): the request `init` sends each of
//! its filesystem servers at shutdown, and the answer it reads back
//! (`docs/spec/rsproto-wire-format.md` § Meta::Unmount).
//!
//! **Hand-built and hand-parsed**, as [`ready`](crate::ready) is: `userspace/init/CLAUDE.md` keeps
//! `librsproto` out of init's build. The tests check the request with `librsproto`'s own decoder,
//! and hand the parser what `fs-server-ext4` sends as well as what no correct server would.

use crate::ready::{ERROR_BODY_LEN, FLAG_ERROR, HEADER_LEN, RS_MAGIC};

/// The `Meta::Unmount` op.
pub const OP_UNMOUNT: u16 = 0x0005;
/// The envelope's version.
pub const RS_VERSION: u16 = 1;
/// `RS_FLAG_REPLY`: set on an answer.
pub const FLAG_REPLY: u32 = 1 << 0;

/// Write a `Meta::Unmount` request, numbered `request_id`, at the front of `out`: an envelope and
/// no body. Its length, or `None` if `out` is too short to hold it.
pub fn request(out: &mut [u8], request_id: u64) -> Option<usize> {
    let out = out.get_mut(..HEADER_LEN)?;
    out.fill(0);
    out[0..4].copy_from_slice(&RS_MAGIC.to_le_bytes());
    out[4..6].copy_from_slice(&RS_VERSION.to_le_bytes());
    out[6..8].copy_from_slice(&OP_UNMOUNT.to_le_bytes());
    out[8..16].copy_from_slice(&request_id.to_le_bytes());
    Some(HEADER_LEN)
}

/// What a server answered an unmount with.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer<'a> {
    /// The filesystem is recorded clean, or, for a read-only mount, left as it was found.
    Done,
    /// It could not be: here is why, empty if the server gave no reason that parses.
    Failed(&'a [u8]),
    /// Anything else: too short, the wrong magic or op, or not a reply.
    Unexpected,
}

/// Parse the rsproto message in an `IpcMsg`'s payload, already cut to the header's
/// `payload_len`.
pub fn answer(payload: &[u8]) -> Answer<'_> {
    let envelope = payload.len() >= HEADER_LEN && u32_at(payload, 0) == RS_MAGIC;
    if !envelope || u16_at(payload, 6) != OP_UNMOUNT {
        return Answer::Unexpected;
    }
    let flags = u32_at(payload, 16);
    if flags & FLAG_REPLY == 0 {
        return Answer::Unexpected;
    }
    if flags & FLAG_ERROR == 0 {
        return Answer::Done;
    }
    Answer::Failed(reason(payload).unwrap_or(&[]))
}

/// A failure's message, if its body holds a whole one.
fn reason(payload: &[u8]) -> Option<&[u8]> {
    let body_len = u32_at(payload, 20) as usize;
    let body = payload.get(HEADER_LEN..HEADER_LEN.checked_add(body_len)?)?;
    let msg_len = u16_at(body.get(..ERROR_BODY_LEN)?, 8) as usize;
    body.get(ERROR_BODY_LEN..ERROR_BODY_LEN + msg_len)
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::error::error_body;
    use librsproto::{RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

    /// **The request is what `fs-server-ext4` looks for**: `librsproto`'s own decoder reads an
    /// `OP_UNMOUNT` that is not a reply, with the id `init` gave it.
    #[test]
    fn the_request_decodes_as_an_unmount() {
        let mut buf = [0xAAu8; 64];
        let n = request(&mut buf, 7).unwrap();
        let m = decode(&buf[..n]).unwrap();
        assert_eq!((m.op, m.request_id, m.flags, m.body.len()), (librsproto::OP_UNMOUNT, 7, 0, 0));
        assert_eq!(OP_UNMOUNT, librsproto::OP_UNMOUNT);
        assert_eq!(RS_VERSION, librsproto::RS_VERSION);
        assert_eq!(FLAG_REPLY, RS_FLAG_REPLY);
        assert_eq!(request(&mut [0u8; HEADER_LEN - 1], 7), None, "too short to hold it");
    }

    /// **The answers `fs-server-ext4` sends**, encoded as it encodes them: an empty reply, and an
    /// error reply with its reason.
    #[test]
    fn a_servers_answers_parse() {
        let mut buf = [0u8; 256];
        let n = encode(&mut buf, librsproto::OP_UNMOUNT, 7, RS_FLAG_REPLY, &[], 0).unwrap();
        assert_eq!(answer(&buf[..n]), Answer::Done);

        let mut body = [0u8; 128];
        let why = b"the state could not be written";
        let len = error_body(&mut body, libkern::error::KError::IoError.as_i32(), 0, why).unwrap();
        let failed = RS_FLAG_REPLY | RS_FLAG_ERROR;
        let n = encode(&mut buf, librsproto::OP_UNMOUNT, 7, failed, &body[..len], 0).unwrap();
        assert_eq!(answer(&buf[..n]), Answer::Failed(why));
    }

    /// **What no correct server sends is not taken for an answer**: a request echoed back, the
    /// wrong op, a truncated envelope, and a failure whose body is cut short.
    #[test]
    fn anything_else_is_unexpected() {
        let mut buf = [0u8; 256];
        let n = request(&mut buf, 7).unwrap();
        assert_eq!(answer(&buf[..n]), Answer::Unexpected, "not a reply");
        let n = encode(&mut buf, librsproto::OP_READY, 7, RS_FLAG_REPLY, &[], 0).unwrap();
        assert_eq!(answer(&buf[..n]), Answer::Unexpected, "another op");
        assert_eq!(answer(&buf[..HEADER_LEN - 1]), Answer::Unexpected, "truncated");
        let failed = RS_FLAG_REPLY | RS_FLAG_ERROR;
        let n = encode(&mut buf, librsproto::OP_UNMOUNT, 7, failed, &[0; 4], 0).unwrap();
        assert_eq!(answer(&buf[..n]), Answer::Failed(b""), "a body too short for its reason");
    }
}
