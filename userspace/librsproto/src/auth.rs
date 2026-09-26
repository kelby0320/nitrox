//! The `Auth` category (`op = 0x08xx`) bodies — credential validation, and the administration of
//! the accounts it validates against. See `docs/spec/rsproto-auth-ops.md`. `Authenticate`
//! (`0x0800`): `(username, password) → { AUTHENTICATED, principal, home } | DENIED`. A denied
//! credential is a normal reply (`result = DENIED`), not an `RsFlags::ERROR`. `List`, `Add`,
//! `Remove` and `SetPassword` (`0x0801`–`0x0804`, administration Part D.1) are asked on an admin
//! session, and a refusal there *is* an error reply.
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

// --- Administration (administration Part D.1) -------------------------------

/// `Auth::List` — every account's name and home. Asked on an admin session.
pub const OP_AUTH_LIST: u16 = 0x0801;
/// `Auth::Add` — a new account: a name and a password. Its home is `/home/<name>`.
pub const OP_AUTH_ADD: u16 = 0x0802;
/// `Auth::Remove` — an account, by name.
pub const OP_AUTH_REMOVE: u16 = 0x0803;
/// `Auth::SetPassword` — a new password for an account, by name.
pub const OP_AUTH_SET_PASSWORD: u16 = 0x0804;

/// Write an `Add` or `SetPassword` body: a name and a password, laid out as `Authenticate`'s
/// request.
pub fn build_account_request(out: &mut [u8], name: &[u8], password: &[u8]) -> Option<usize> {
    build_authenticate_request(out, name, password)
}

/// Parse an `Add` or `SetPassword` body. **Its lengths must account for it exactly**: a body with
/// bytes left over is malformed, as `Storage`'s are, where `Authenticate` has always let them pass.
pub fn parse_account_request(body: &[u8]) -> Option<AuthenticateRequest<'_>> {
    let r = parse_authenticate_request(body)?;
    (AUTH_REQUEST_PREFIX_LEN + r.username.len() + r.password.len() == body.len()).then_some(r)
}

/// Writes `List`'s reply: `count: u16`, then per account `name_len: u8`, the name, `home_len: u8`,
/// the home.
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

    /// Add an account. `None`, and nothing written, if a field is over 255 bytes or it will not
    /// fit.
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

/// Parse a `List` reply. `None` if an entry runs past the body, or the entries do not account for
/// it exactly.
pub fn parse_account_list(body: &[u8]) -> Option<AccountList<'_>> {
    if body.len() < 2 {
        return None;
    }
    let count = get_u16(body, 0);
    let list = AccountList { body, count };
    let mut end = 2;
    for _ in 0..count {
        end = list.entry_end(end)?;
    }
    (end == body.len()).then_some(list)
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
        let body = self.body;
        let mut at = 2;
        (0..self.count).map(move |_| {
            let nl = body[at] as usize;
            let name = &body[at + 1..at + 1 + nl];
            let h = at + 1 + nl;
            let hl = body[h] as usize;
            let home = &body[h + 1..h + 1 + hl];
            at = h + 1 + hl;
            (name, home)
        })
    }

    /// Where the entry starting at `at` ends, if it fits.
    fn entry_end(&self, at: usize) -> Option<usize> {
        let nl = *self.body.get(at)? as usize;
        let h = at.checked_add(1 + nl)?;
        let hl = *self.body.get(h)? as usize;
        let end = h.checked_add(1 + hl)?;
        (end <= self.body.len()).then_some(end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A list round-trips**, in order.
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

    /// **The reader, on bytes a correct writer never makes**: a count past the entries, an entry
    /// past the body, and bytes left over.
    #[test]
    fn an_account_list_that_does_not_add_up_is_refused() {
        assert!(parse_account_list(&[]).is_none());
        assert!(parse_account_list(&[1, 0]).is_none(), "one entry promised, none there");
        assert!(parse_account_list(&[1, 0, 5, b'a']).is_none(), "a name past the end");
        assert!(parse_account_list(&[1, 0, 1, b'a', 0]).is_some());
        assert!(parse_account_list(&[1, 0, 1, b'a', 0, 9]).is_none(), "a byte left over");
        assert!(parse_account_list(&[0, 0, 7]).is_none(), "bytes after an empty list");
    }

    /// **A list that will not fit writes nothing of the entry that did not fit.**
    #[test]
    fn an_account_list_writer_stops_at_its_buffer() {
        let mut buf = [0u8; 10];
        let mut w = AccountListWriter::new(&mut buf).unwrap();
        w.push(b"ab", b"/h").unwrap(); // 2 + 1 + 2 + 1 + 2 = 8
        assert!(w.push(b"c", b"/").is_none());
        let n = w.finish();
        assert_eq!(parse_account_list(&buf[..n]).unwrap().len(), 1);
    }

    /// **An account request must be exactly its fields**, where `Authenticate`'s parser lets trailing
    /// bytes pass.
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
