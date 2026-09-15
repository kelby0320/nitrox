//! The first message a resource server sends on its control channel, parsed: a `Meta::Ready`
//! carrying the endpoint to bind, or a refusal saying why there is none
//! (`docs/spec/rsproto-wire-format.md` § Meta::Ready).
//!
//! **Hand-parsed**, as init has always read the Ready — `userspace/init/CLAUDE.md` keeps
//! `librsproto` out of init's build — and host-tested here, where the tests build messages with
//! `librsproto`'s own encoder and then hand the parser what a correct encoder never would.
//!
//! The refusal is Phase 5's: `fs-server-ext4` used to say Ready over a device holding no
//! filesystem, and the first sign was a program that would not load off the mount. It now
//! refuses, and init prints the reason beside the device and mount point only it knows.

/// `"RSMG"`, the envelope's first four bytes.
pub const RS_MAGIC: u32 = 0x5253_4D47;
/// The `Meta::Ready` op.
pub const OP_READY: u16 = 0x0004;
/// `RS_FLAG_ERROR`: set on a refusal.
pub const FLAG_ERROR: u32 = 1 << 1;
/// The envelope before the body: magic, version, op, request id, flags, body length, handle
/// count, reserved.
pub const HEADER_LEN: usize = 28;
/// An `ErrorBody` before its message: `kerror`, `server_code`, `msg_len`, reserved.
pub const ERROR_BODY_LEN: usize = 12;
/// The longest reason init prints. A longer one is cut: it is a line on a console that may be
/// 170 columns wide, and the server is trusted with a device, not with init's log.
pub const MAX_REASON: usize = 192;

/// What a server's first control message said.
#[derive(Debug, PartialEq, Eq)]
pub enum First<'a> {
    /// A `Meta::Ready` that transferred a handle: the endpoint to bind.
    Ready,
    /// A `Meta::Ready` with the error flag: the server cannot serve, and here is its reason —
    /// empty if it gave none, or gave one that does not parse.
    Refused(&'a [u8]),
    /// Anything else: too short, the wrong magic or op, or a Ready with no handle to bind.
    Unexpected,
}

/// Parse the rsproto message in an `IpcMsg`'s payload (`payload`, already cut to the header's
/// `payload_len`) that arrived with `handles` transferred handles.
pub fn parse(payload: &[u8], handles: usize) -> First<'_> {
    if payload.len() < HEADER_LEN
        || u32_at(payload, 0) != RS_MAGIC
        || u16_at(payload, 6) != OP_READY
    {
        return First::Unexpected;
    }
    if u32_at(payload, 16) & FLAG_ERROR == 0 {
        return if handles >= 1 { First::Ready } else { First::Unexpected };
    }
    First::Refused(reason(payload).unwrap_or(&[]))
}

/// A refusal's message, if its body holds a whole one.
fn reason(payload: &[u8]) -> Option<&[u8]> {
    let body_len = u32_at(payload, 20) as usize;
    let body = payload.get(HEADER_LEN..HEADER_LEN.checked_add(body_len)?)?;
    let msg_len = u16_at(body.get(..ERROR_BODY_LEN)?, 8) as usize;
    body.get(ERROR_BODY_LEN..ERROR_BODY_LEN + msg_len)
}

/// Copy `reason` into `out` for printing: at most [`MAX_REASON`] bytes, and anything that is not
/// printable ASCII as `?`, so a reason cannot move the console's cursor or colour its text.
pub fn printable<'o>(reason: &[u8], out: &'o mut [u8; MAX_REASON]) -> &'o [u8] {
    let n = reason.len().min(MAX_REASON);
    for (o, &b) in out.iter_mut().zip(&reason[..n]) {
        *o = if (0x20..0x7F).contains(&b) { b } else { b'?' };
    }
    &out[..n]
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

    /// A message built by the encoder every server uses.
    fn encoded(flags: u32, body: &[u8], handles: u16) -> Vec<u8> {
        let mut out = vec![0u8; 512];
        let n = librsproto::encode(&mut out, librsproto::OP_READY, 0, flags, body, handles).unwrap();
        out.truncate(n);
        out
    }

    fn refusal(msg: &[u8]) -> Vec<u8> {
        let mut body = [0u8; 256];
        let n = error_body(&mut body, -5, 0, msg).unwrap();
        encoded(librsproto::RS_FLAG_ERROR, &body[..n], 0)
    }

    #[test]
    fn the_constants_are_librsprotos() {
        assert_eq!(RS_MAGIC, librsproto::RS_MAGIC);
        assert_eq!(OP_READY, librsproto::OP_READY);
        assert_eq!(FLAG_ERROR, librsproto::RS_FLAG_ERROR);
        assert_eq!(HEADER_LEN, librsproto::RS_HEADER_LEN);
        assert_eq!(ERROR_BODY_LEN, librsproto::error::ERROR_BODY_LEN);
    }

    #[test]
    fn a_ready_with_a_handle_is_ready_and_without_one_is_not() {
        let mut body = [0u8; 64];
        let n = librsproto::meta::ready(&mut body, b"fs-server-ext4").unwrap();
        let msg = encoded(0, &body[..n], 1);
        assert_eq!(parse(&msg, 1), First::Ready);
        assert_eq!(parse(&msg, 0), First::Unexpected, "nothing to bind");
    }

    #[test]
    fn a_refusal_carries_its_reason_and_needs_no_handle() {
        let msg = refusal(b"no ext4 filesystem: superblock magic 0x0000, not 0xef53");
        assert_eq!(parse(&msg, 0), First::Refused(b"no ext4 filesystem: superblock magic 0x0000, not 0xef53"));
        // A refusal is a refusal even if a handle came with it: init closes it and binds nothing.
        assert_eq!(parse(&msg, 1), First::Refused(b"no ext4 filesystem: superblock magic 0x0000, not 0xef53"));
    }

    /// **Bytes no correct encoder writes.** A reader tested only against its encoder's output
    /// learns nothing about the lengths it trusts.
    #[test]
    fn a_refusal_whose_lengths_lie_is_still_a_refusal_with_no_reason() {
        let good = refusal(b"why");

        // The body length says more than arrived.
        let mut long_body = good.clone();
        long_body[20..24].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(parse(&long_body, 0), First::Refused(b""));

        // The message length says more than the body holds.
        let mut long_msg = good.clone();
        long_msg[HEADER_LEN + 8..HEADER_LEN + 10].copy_from_slice(&200u16.to_le_bytes());
        assert_eq!(parse(&long_msg, 0), First::Refused(b""));

        // A body too short to be an ErrorBody at all.
        let mut short = good.clone();
        short[20..24].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(parse(&short, 0), First::Refused(b""));

        // A body length so large that adding the header to it would overflow.
        let mut huge = good.clone();
        huge[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&huge, 0), First::Refused(b""));
    }

    #[test]
    fn anything_but_a_ready_is_unexpected() {
        let good = refusal(b"why");
        assert_eq!(parse(&good[..HEADER_LEN - 1], 1), First::Unexpected, "too short for a header");
        let mut magic = good.clone();
        magic[0] ^= 0xFF;
        assert_eq!(parse(&magic, 1), First::Unexpected);
        let mut op = good.clone();
        op[6..8].copy_from_slice(&librsproto::OP_PING.to_le_bytes());
        assert_eq!(parse(&op, 1), First::Unexpected, "an error-flagged message that is not a Ready");
    }

    #[test]
    fn a_reason_is_printed_cut_and_with_control_bytes_replaced() {
        let mut out = [0u8; MAX_REASON];
        assert_eq!(printable(b"magic 0x0000\x1b[2J\n\xe2\x80\x94", &mut out), b"magic 0x0000?[2J????");
        let long = [b'x'; MAX_REASON + 10];
        assert_eq!(printable(&long, &mut out).len(), MAX_REASON);
    }
}
