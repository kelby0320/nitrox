//! The `Auth` category (`op = 0x08xx`) bodies — credential validation. See
//! `docs/spec/rsproto-auth-ops.md`. `Authenticate` (`0x0800`) is the only op:
//! `(username, password) → { AUTHENTICATED, principal, home } | DENIED`. A denied
//! credential is a normal reply (`result = DENIED`), not an `RsFlags::ERROR`.
//!
//! Bodies are little-endian, byte-serialised into a caller buffer (the `IpcMsg`
//! payload), like the other categories. The password crosses the channel in
//! cleartext (a kernel-mediated local IPC, no network); the server hashes it and
//! stores only a one-way verifier.

use crate::{get_u16, put_u16};

/// Fixed prefix of an `AuthenticateRequest` (before the username/password bytes).
pub const AUTH_REQUEST_PREFIX_LEN: usize = 4;
/// Fixed prefix of an `AuthenticateReply` (before the principal/home bytes).
pub const AUTH_REPLY_PREFIX_LEN: usize = 8;

/// `result` value: the credential is invalid (wrong password / unknown user /
/// malformed field). No reason is disclosed.
pub const AUTH_RESULT_DENIED: u16 = 0;
/// `result` value: the credential is valid; `principal` / `home` are populated.
pub const AUTH_RESULT_AUTHENTICATED: u16 = 1;

// --- Authenticate request ---------------------------------------------------

/// A parsed `AuthenticateRequest`.
#[derive(Copy, Clone, Debug)]
pub struct AuthenticateRequest<'a> {
    pub username: &'a [u8],
    pub password: &'a [u8],
}

/// Write an `AuthenticateRequest` body; returns its length. `None` if a field is
/// longer than `u16::MAX` or `out` is too small.
pub fn build_authenticate_request(
    out: &mut [u8],
    username: &[u8],
    password: &[u8],
) -> Option<usize> {
    if username.len() > u16::MAX as usize || password.len() > u16::MAX as usize {
        return None;
    }
    let total = AUTH_REQUEST_PREFIX_LEN + username.len() + password.len();
    if out.len() < total {
        return None;
    }
    put_u16(out, 0, username.len() as u16);
    put_u16(out, 2, password.len() as u16);
    let u_end = AUTH_REQUEST_PREFIX_LEN + username.len();
    out[AUTH_REQUEST_PREFIX_LEN..u_end].copy_from_slice(username);
    out[u_end..total].copy_from_slice(password);
    Some(total)
}

/// Parse an `AuthenticateRequest` body.
pub fn parse_authenticate_request(body: &[u8]) -> Option<AuthenticateRequest<'_>> {
    if body.len() < AUTH_REQUEST_PREFIX_LEN {
        return None;
    }
    let ulen = get_u16(body, 0) as usize;
    let plen = get_u16(body, 2) as usize;
    let u_end = AUTH_REQUEST_PREFIX_LEN.checked_add(ulen)?;
    let p_end = u_end.checked_add(plen)?;
    if body.len() < p_end {
        return None;
    }
    Some(AuthenticateRequest {
        username: &body[AUTH_REQUEST_PREFIX_LEN..u_end],
        password: &body[u_end..p_end],
    })
}

// --- Authenticate reply -----------------------------------------------------

/// A parsed `AuthenticateReply`.
#[derive(Copy, Clone, Debug)]
pub struct AuthenticateReply<'a> {
    pub result: u16,
    /// The canonical principal (empty on `DENIED`).
    pub principal: &'a [u8],
    /// The principal's home path (empty on `DENIED`).
    pub home: &'a [u8],
}

impl AuthenticateReply<'_> {
    /// `true` iff the credential was accepted.
    pub fn is_authenticated(&self) -> bool {
        self.result == AUTH_RESULT_AUTHENTICATED
    }
}

/// Write an `AuthenticateReply` body; returns its length. On `DENIED`, pass empty
/// `principal` / `home`. `None` if a field is too long or `out` is too small.
pub fn build_authenticate_reply(
    out: &mut [u8],
    result: u16,
    principal: &[u8],
    home: &[u8],
) -> Option<usize> {
    if principal.len() > u16::MAX as usize || home.len() > u16::MAX as usize {
        return None;
    }
    let total = AUTH_REPLY_PREFIX_LEN + principal.len() + home.len();
    if out.len() < total {
        return None;
    }
    put_u16(out, 0, result);
    put_u16(out, 2, principal.len() as u16);
    put_u16(out, 4, home.len() as u16);
    put_u16(out, 6, 0); // reserved
    let p_end = AUTH_REPLY_PREFIX_LEN + principal.len();
    out[AUTH_REPLY_PREFIX_LEN..p_end].copy_from_slice(principal);
    out[p_end..total].copy_from_slice(home);
    Some(total)
}

/// Write a `DENIED` reply (no principal/home). Convenience over
/// [`build_authenticate_reply`].
pub fn build_denied_reply(out: &mut [u8]) -> Option<usize> {
    build_authenticate_reply(out, AUTH_RESULT_DENIED, &[], &[])
}

/// Parse an `AuthenticateReply` body.
pub fn parse_authenticate_reply(body: &[u8]) -> Option<AuthenticateReply<'_>> {
    if body.len() < AUTH_REPLY_PREFIX_LEN {
        return None;
    }
    let result = get_u16(body, 0);
    let plen = get_u16(body, 2) as usize;
    let hlen = get_u16(body, 4) as usize;
    let p_end = AUTH_REPLY_PREFIX_LEN.checked_add(plen)?;
    let h_end = p_end.checked_add(hlen)?;
    if body.len() < h_end {
        return None;
    }
    Some(AuthenticateReply {
        result,
        principal: &body[AUTH_REPLY_PREFIX_LEN..p_end],
        home: &body[p_end..h_end],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let mut buf = [0u8; 64];
        let n = build_authenticate_request(&mut buf, b"alice", b"s3cret").unwrap();
        let r = parse_authenticate_request(&buf[..n]).unwrap();
        assert_eq!(r.username, b"alice");
        assert_eq!(r.password, b"s3cret");
    }

    #[test]
    fn request_empty_password() {
        let mut buf = [0u8; 32];
        let n = build_authenticate_request(&mut buf, b"bob", b"").unwrap();
        let r = parse_authenticate_request(&buf[..n]).unwrap();
        assert_eq!(r.username, b"bob");
        assert_eq!(r.password, b"");
    }

    #[test]
    fn reply_authenticated_round_trip() {
        let mut buf = [0u8; 64];
        let n =
            build_authenticate_reply(&mut buf, AUTH_RESULT_AUTHENTICATED, b"alice", b"/home/alice")
                .unwrap();
        let r = parse_authenticate_reply(&buf[..n]).unwrap();
        assert!(r.is_authenticated());
        assert_eq!(r.principal, b"alice");
        assert_eq!(r.home, b"/home/alice");
    }

    #[test]
    fn denied_reply_has_empty_fields() {
        let mut buf = [0u8; 16];
        let n = build_denied_reply(&mut buf).unwrap();
        let r = parse_authenticate_reply(&buf[..n]).unwrap();
        assert!(!r.is_authenticated());
        assert_eq!(r.result, AUTH_RESULT_DENIED);
        assert_eq!(r.principal, b"");
        assert_eq!(r.home, b"");
        assert_eq!(n, AUTH_REPLY_PREFIX_LEN);
    }

    #[test]
    fn truncated_bodies_rejected() {
        assert!(parse_authenticate_request(&[0u8; 2]).is_none());
        assert!(parse_authenticate_reply(&[0u8; 4]).is_none());
        // A length prefix that overruns the buffer.
        let mut buf = [0u8; 8];
        put_u16(&mut buf, 0, 100); // username_len = 100, but body is 8
        assert!(parse_authenticate_request(&buf).is_none());
    }

    #[test]
    fn build_rejects_small_out() {
        let mut tiny = [0u8; 3];
        assert!(build_authenticate_request(&mut tiny, b"x", b"y").is_none());
        assert!(build_authenticate_reply(&mut tiny, AUTH_RESULT_DENIED, b"", b"").is_none());
    }
}
