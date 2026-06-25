//! `ErrorBody` — the body of any error reply (`RS_FLAG_REPLY | RS_FLAG_ERROR`).
//! See `docs/spec/rsproto-wire-format.md` § "Error replies".

use crate::{get_u16, get_u32, put_u16, put_u32};

/// Fixed prefix of an [`ErrorBody`] (before the optional UTF-8 message).
pub const ERROR_BODY_LEN: usize = 12;

/// A parsed error body: a `KError` discriminant + an optional message.
#[derive(Copy, Clone, Debug)]
pub struct ErrorView<'a> {
    /// A `KError` discriminant (negative), e.g. `-10` for `NotFound`.
    pub kerror: i32,
    /// Server-specific finer code, `0` if not applicable.
    pub server_code: u32,
    /// Optional human-readable UTF-8 message (may be empty).
    pub msg: &'a [u8],
}

/// Serialise an error body (`kerror` + `server_code` + optional `msg`) into
/// `out`, returning its length, or `None` if `out` is too small or `msg` is
/// longer than `u16::MAX`.
pub fn error_body(out: &mut [u8], kerror: i32, server_code: u32, msg: &[u8]) -> Option<usize> {
    if msg.len() > u16::MAX as usize {
        return None;
    }
    let total = ERROR_BODY_LEN + msg.len();
    if out.len() < total {
        return None;
    }
    put_u32(out, 0, kerror as u32);
    put_u32(out, 4, server_code);
    put_u16(out, 8, msg.len() as u16);
    put_u16(out, 10, 0);
    out[ERROR_BODY_LEN..total].copy_from_slice(msg);
    Some(total)
}

/// Parse an error body. `None` if truncated.
pub fn parse_error(body: &[u8]) -> Option<ErrorView<'_>> {
    if body.len() < ERROR_BODY_LEN {
        return None;
    }
    let msg_len = get_u16(body, 8) as usize;
    let end = ERROR_BODY_LEN.checked_add(msg_len)?;
    if body.len() < end {
        return None;
    }
    Some(ErrorView {
        kerror: get_u32(body, 0) as i32,
        server_code: get_u32(body, 4),
        msg: &body[ERROR_BODY_LEN..end],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_round_trips() {
        let mut buf = [0u8; 64];
        let n = error_body(&mut buf, -10, 7, b"not found").unwrap();
        assert_eq!(n, ERROR_BODY_LEN + 9);
        let e = parse_error(&buf[..n]).unwrap();
        assert_eq!(e.kerror, -10);
        assert_eq!(e.server_code, 7);
        assert_eq!(e.msg, b"not found");
    }

    #[test]
    fn empty_message_ok() {
        let mut buf = [0u8; 16];
        let n = error_body(&mut buf, -30, 0, b"").unwrap();
        assert_eq!(n, ERROR_BODY_LEN);
        let e = parse_error(&buf[..n]).unwrap();
        assert_eq!(e.kerror, -30);
        assert!(e.msg.is_empty());
    }
}
