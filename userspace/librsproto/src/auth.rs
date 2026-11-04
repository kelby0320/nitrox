//! The `Auth` category (`op = 0x08xx`) bodies — credential validation, and the administration of
//! the accounts it validates against. See `docs/spec/rsproto-auth-ops.md`. `Authenticate`
//! (`0x0800`): `(username, password) → { AUTHENTICATED, principal, home } | DENIED`. A denied
//! credential is a normal reply (`result = DENIED`), not an `RsFlags::ERROR`. `List`, `Add`,
//! `Remove` and `SetPassword` (`0x0801`–`0x0804`) are asked on an admin session, and a refusal
//! there *is* an error reply.
//!
//! The password crosses the channel in cleartext (a kernel-mediated local IPC, no network);
//! the server hashes it and stores only a one-way verifier.

use crate::{Reader, put_u16};

/// Fixed prefix of an `AuthenticateRequest` (before the username/password bytes).
pub const AUTH_REQUEST_PREFIX_LEN: usize = 4;
/// Fixed prefix of an `AuthenticateReply` (before the principal/home bytes).
pub const AUTH_REPLY_PREFIX_LEN: usize = 8;

/// `result`: the credential is invalid; no reason is disclosed.
pub const AUTH_RESULT_DENIED: u16 = 0;
/// `result` value: the credential is valid; `principal` / `home` are populated.
pub const AUTH_RESULT_AUTHENTICATED: u16 = 1;

/// A parsed `AuthenticateRequest`.
#[derive(Copy, Clone, Debug)]
pub struct AuthenticateRequest<'a> {
    /// The account name.
    pub username: &'a [u8],
    /// The cleartext password.
    pub password: &'a [u8],
}

/// Write an `AuthenticateRequest` body; `None` if a field is over `u16::MAX` or `out` is short.
pub fn build_authenticate_request(out: &mut [u8], username: &[u8], password: &[u8]) -> Option<usize> {
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
    let mut r = Reader::new(body);
    let ulen = r.u16()?;
    let plen = r.u16()?;
    Some(AuthenticateRequest { username: r.bytes(ulen as usize)?, password: r.bytes(plen as usize)? })
}

/// A parsed `AuthenticateReply`.
#[derive(Copy, Clone, Debug)]
pub struct AuthenticateReply<'a> {
    /// `AUTH_RESULT_DENIED` or `AUTH_RESULT_AUTHENTICATED`.
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

/// Write an `AuthenticateReply` body; `None` if a field is too long or `out` is too small.
pub fn build_authenticate_reply(out: &mut [u8], result: u16, principal: &[u8], home: &[u8]) -> Option<usize> {
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

/// Write a `DENIED` reply (no principal/home); see [`build_authenticate_reply`].
pub fn build_denied_reply(out: &mut [u8]) -> Option<usize> {
    build_authenticate_reply(out, AUTH_RESULT_DENIED, &[], &[])
}

/// Parse an `AuthenticateReply` body.
pub fn parse_authenticate_reply(body: &[u8]) -> Option<AuthenticateReply<'_>> {
    let mut r = Reader::new(body);
    let result = r.u16()?;
    let plen = r.u16()?;
    let hlen = r.u16()?;
    r.skip(2)?;
    Some(AuthenticateReply { result, principal: r.bytes(plen as usize)?, home: r.bytes(hlen as usize)? })
}

/// `Auth::List` — every account's name and home. Asked on an admin session.
pub const OP_AUTH_LIST: u16 = 0x0801;
/// `Auth::Add` — a new account: a name and a password. Its home is `/home/<name>`.
pub const OP_AUTH_ADD: u16 = 0x0802;
/// `Auth::Remove` — an account, by name.
pub const OP_AUTH_REMOVE: u16 = 0x0803;
/// `Auth::SetPassword` — a new password for an account, by name.
pub const OP_AUTH_SET_PASSWORD: u16 = 0x0804;

/// Write an `Add` or `SetPassword` body, laid out as `Authenticate`'s request.
pub fn build_account_request(out: &mut [u8], name: &[u8], password: &[u8]) -> Option<usize> {
    build_authenticate_request(out, name, password)
}

/// Parse an `Add` or `SetPassword` body. **Its lengths must account for it exactly**: a body with
/// bytes left over is malformed, as `Storage`'s are, where `Authenticate` has always let them pass.
pub fn parse_account_request(body: &[u8]) -> Option<AuthenticateRequest<'_>> {
    let r = parse_authenticate_request(body)?;
    (AUTH_REQUEST_PREFIX_LEN + r.username.len() + r.password.len() == body.len()).then_some(r)
}

/// `List`'s reply: `count: u16`, then per account `name_len: u8`, name, `home_len: u8`, home.
pub struct AccountListWriter<'a> {
    out: &'a mut [u8],
    at: usize,
    count: u16,
}

impl<'a> AccountListWriter<'a> {
    /// A writer over `out`; `None` if it cannot hold even the count.
    pub fn new(out: &'a mut [u8]) -> Option<AccountListWriter<'a>> {
        (out.len() >= 2).then_some(AccountListWriter { out, at: 2, count: 0 })
    }

    /// Add an account; `None`, writing nothing, if a field is over 255 bytes or it will not fit.
    pub fn push(&mut self, name: &[u8], home: &[u8]) -> Option<()> {
        if name.len() > 255 || home.len() > 255 || self.count == u16::MAX {
            return None;
        }
        let end = self.at + 2 + name.len() + home.len();
        if end > self.out.len() {
            return None;
        }
        let mut at = self.at;
        self.out[at] = name.len() as u8;
        at += 1;
        self.out[at..at + name.len()].copy_from_slice(name);
        at += name.len();
        self.out[at] = home.len() as u8;
        at += 1;
        self.out[at..at + home.len()].copy_from_slice(home);
        self.at = end;
        self.count += 1;
        Some(())
    }

    /// The body's length, with the count written.
    pub fn finish(self) -> usize {
        put_u16(self.out, 0, self.count);
        self.at
    }
}

/// A parsed `List` reply, every entry checked when it was parsed.
#[derive(Copy, Clone, Debug)]
pub struct AccountList<'a> {
    body: &'a [u8],
    count: u16,
}

/// Parse a `List` reply; `None` if an entry runs past the body or the entries do not fill it.
pub fn parse_account_list(body: &[u8]) -> Option<AccountList<'_>> {
    let mut r = Reader::new(body);
    let count = r.u16()?;
    for _ in 0..count {
        entry(&mut r)?;
    }
    r.finish()?;
    Some(AccountList { body, count })
}

/// One `List` entry: a `u8`-prefixed name, then a `u8`-prefixed home.
fn entry<'a>(r: &mut Reader<'a>) -> Option<(&'a [u8], &'a [u8])> {
    Some((r.lenpfx_u8()?, r.lenpfx_u8()?))
}

impl<'a> AccountList<'a> {
    /// How many accounts.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Each account's `(name, home)`, in the order the service gave them.
    pub fn iter(&self) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + '_ {
        let mut r = Reader::new(self.body);
        let _ = r.skip(2);
        (0..self.count).map_while(move |_| entry(&mut r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_list_round_trips() {
        let mut buf = [0u8; 128];
        let mut w = AccountListWriter::new(&mut buf).unwrap();
        w.push(b"alice", b"/home/alice").unwrap();
        w.push(b"bob", b"/home/bob").unwrap();
        let n = w.finish();
        let l = parse_account_list(&buf[..n]).unwrap();
        assert_eq!(l.len(), 2);
        let got: std::vec::Vec<_> = l.iter().collect();
        assert_eq!(got, [(&b"alice"[..], &b"/home/alice"[..]), (&b"bob"[..], &b"/home/bob"[..])]);
        let empty = AccountListWriter::new(&mut buf).unwrap().finish();
        assert!(parse_account_list(&buf[..empty]).unwrap().is_empty());
    }

    /// The reader, on bytes a correct writer never makes.
    #[test]
    fn an_account_list_that_does_not_add_up_is_refused() {
        assert!(parse_account_list(&[]).is_none());
        assert!(parse_account_list(&[1, 0]).is_none(), "one entry promised, none there");
        assert!(parse_account_list(&[1, 0, 5, b'a']).is_none(), "a name past the end");
        assert!(parse_account_list(&[1, 0, 1, b'a', 0]).is_some());
        assert!(parse_account_list(&[1, 0, 1, b'a', 0, 9]).is_none(), "a byte left over");
        assert!(parse_account_list(&[0, 0, 7]).is_none(), "bytes after an empty list");
    }

    #[test]
    fn an_account_list_writer_stops_at_its_buffer() {
        let mut buf = [0u8; 10];
        let mut w = AccountListWriter::new(&mut buf).unwrap();
        w.push(b"ab", b"/h").unwrap(); // 2 + 1 + 2 + 1 + 2 = 8
        assert!(w.push(b"c", b"/").is_none());
        let n = w.finish();
        assert_eq!(parse_account_list(&buf[..n]).unwrap().len(), 1);
    }

    #[test]
    fn an_account_request_is_exact() {
        let mut buf = [0u8; 32];
        let n = build_account_request(&mut buf, b"bob", b"pw").unwrap();
        let r = parse_account_request(&buf[..n]).unwrap();
        assert_eq!((r.username, r.password), (&b"bob"[..], &b"pw"[..]));
        assert!(parse_account_request(&buf[..n + 1]).is_none());
        assert!(parse_authenticate_request(&buf[..n + 1]).is_some(), "the old parser, as it was");
    }

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
        let n = build_authenticate_reply(&mut buf, AUTH_RESULT_AUTHENTICATED, b"alice", b"/home/alice").unwrap();
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

    #[test]
    fn parsers_survive_truncation_and_corruption() {
        use crate::fuzz::*;
        let mut b = [0u8; 64];
        let n = build_authenticate_request(&mut b, b"alice", b"pw").unwrap();
        refuses_every_prefix(&b[..n], |x| parse_authenticate_request(x).is_some());
        refuses_every_prefix(&b[..n], |x| parse_account_request(x).is_some());
        let n = build_authenticate_reply(&mut b, AUTH_RESULT_AUTHENTICATED, b"alice", b"/home/alice").unwrap();
        refuses_every_prefix(&b[..n], |x| parse_authenticate_reply(x).is_some());
        let mut w = AccountListWriter::new(&mut b).unwrap();
        w.push(b"alice", b"/home/alice").unwrap();
        w.push(b"bob", b"/").unwrap();
        let n = w.finish();
        refuses_every_prefix(&b[..n], |x| parse_account_list(x).is_some());
        survives_corruption(&b[..n], |x| parse_account_list(x).map(|l| l.iter().count()).is_some());
    }
}
