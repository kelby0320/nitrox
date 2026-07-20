//! `auth-service` — the credential-oracle logic (host-testable).
//!
//! Pure, `#![no_std]`, no-`alloc` credential validation: parse the user database
//! and verify a `(username, password)` against a stored PBKDF2 verifier, answering
//! the `Auth` rsproto category (`docs/spec/rsproto-auth-ops.md`). No syscalls —
//! the bare-target server (`src/main.rs`) supplies the DB bytes + the request/reply
//! buffers; this crate is the policy. Under `cargo test` it builds as host `std`.
//!
//! **User DB format** — a `passwd`-style line file (`docs/architecture/session-and-auth.md`):
//!
//! ```text
//! # comment / blank lines ignored
//! name:salt_hex:iterations:verifier_hex:home
//! ```
//!
//! The stored `verifier` is `PBKDF2-HMAC-SHA256(password, salt, iterations)` — a
//! one-way value; the password is never stored. See `userspace/auth-service/CLAUDE.md`.

#![cfg_attr(not(test), no_std)]

use librsproto::auth::{
    AUTH_RESULT_AUTHENTICATED, build_authenticate_reply, build_denied_reply,
    parse_authenticate_request,
};

/// Max salt length in bytes we decode from a record (generous — salts are ~8–16 B).
pub const SALT_MAX: usize = 32;
/// The PBKDF2 verifier length (one SHA-256 block), matching `libcrypto`.
pub const VERIFIER_LEN: usize = libcrypto::password::VERIFIER_LEN;

/// The outcome of an authentication attempt. On success the `principal` / `home`
/// borrow from the matched DB record.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AuthOutcome<'a> {
    Authenticated { principal: &'a [u8], home: &'a [u8] },
    Denied,
}

/// A fixed dummy salt + verifier used to run an equivalent PBKDF2 for an **unknown**
/// user, so "no such user" and "wrong password" take the same time and reveal the
/// same `Denied` — no user-enumeration / timing oracle (`rsproto-auth-ops.md`).
const DUMMY_SALT: [u8; 8] = [0xa5; 8];
const DUMMY_VERIFIER: [u8; VERIFIER_LEN] = [0u8; VERIFIER_LEN];

/// Validate `(username, password)` against the user DB `db`. A missing user still
/// runs a dummy verify (constant work) and returns [`AuthOutcome::Denied`].
pub fn authenticate<'a>(db: &'a [u8], username: &[u8], password: &[u8]) -> AuthOutcome<'a> {
    for line in db.split(|&b| b == b'\n') {
        let line = trim(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let Some(rec) = Record::parse(line) else {
            continue; // a malformed record is skipped, not fatal
        };
        if rec.name != username {
            continue;
        }
        // Decode the record's salt + verifier, then verify.
        let mut salt = [0u8; SALT_MAX];
        let Some(salt_len) = hex_decode(rec.salt_hex, &mut salt) else {
            return AuthOutcome::Denied;
        };
        let mut verifier = [0u8; VERIFIER_LEN];
        let Some(vlen) = hex_decode(rec.verifier_hex, &mut verifier) else {
            return AuthOutcome::Denied;
        };
        if vlen != VERIFIER_LEN {
            return AuthOutcome::Denied;
        }
        if libcrypto::password::verify(password, &salt[..salt_len], rec.iterations, &verifier) {
            return AuthOutcome::Authenticated { principal: rec.name, home: rec.home };
        }
        return AuthOutcome::Denied;
    }
    // Unknown user: run an equivalent derivation so timing does not distinguish it.
    let _ = libcrypto::password::verify(
        password,
        &DUMMY_SALT,
        libcrypto::password::DEFAULT_ITERATIONS,
        &DUMMY_VERIFIER,
    );
    AuthOutcome::Denied
}

/// Serve one `Authenticate` request: parse the request body, authenticate against
/// `db`, and write the reply body into `reply_out`, returning its length. Returns
/// `None` on a **malformed** request (a truncated body) — the caller answers with an
/// rsproto `ERROR` reply; a *denied* credential is a normal `Some(_)` reply.
pub fn serve_authenticate(request_body: &[u8], db: &[u8], reply_out: &mut [u8]) -> Option<usize> {
    let req = parse_authenticate_request(request_body)?;
    match authenticate(db, req.username, req.password) {
        AuthOutcome::Authenticated { principal, home } => {
            build_authenticate_reply(reply_out, AUTH_RESULT_AUTHENTICATED, principal, home)
        }
        AuthOutcome::Denied => build_denied_reply(reply_out),
    }
}

/// One parsed DB record (fields borrow from the line).
struct Record<'a> {
    name: &'a [u8],
    salt_hex: &'a [u8],
    iterations: u32,
    verifier_hex: &'a [u8],
    home: &'a [u8],
}

impl<'a> Record<'a> {
    /// Parse `name:salt_hex:iterations:verifier_hex:home`. `None` if a field is
    /// missing or `iterations` is not a decimal number.
    fn parse(line: &'a [u8]) -> Option<Record<'a>> {
        let mut it = line.splitn(5, |&b| b == b':');
        let name = it.next()?;
        let salt_hex = it.next()?;
        let iterations = parse_u32(it.next()?)?;
        let verifier_hex = it.next()?;
        let home = it.next()?;
        if name.is_empty() || home.is_empty() {
            return None;
        }
        Some(Record { name, salt_hex, iterations, verifier_hex, home })
    }
}

/// Trim leading/trailing ASCII whitespace (spaces, tabs, CR) from a line.
fn trim(s: &[u8]) -> &[u8] {
    let is_ws = |c: u8| c == b' ' || c == b'\t' || c == b'\r';
    let mut a = 0;
    let mut b = s.len();
    while a < b && is_ws(s[a]) {
        a += 1;
    }
    while b > a && is_ws(s[b - 1]) {
        b -= 1;
    }
    &s[a..b]
}

/// Parse an ASCII decimal `u32`; `None` if empty or non-digit / overflow.
fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut n: u32 = 0;
    for &c in s {
        let d = c.checked_sub(b'0').filter(|&d| d < 10)?;
        n = n.checked_mul(10)?.checked_add(d as u32)?;
    }
    Some(n)
}

/// Decode lowercase/uppercase hex `hex` into `out`, returning the byte count.
/// `None` on an odd length, a non-hex digit, or `out` too small.
fn hex_decode(hex: &[u8], out: &mut [u8]) -> Option<usize> {
    if hex.len() % 2 != 0 || hex.len() / 2 > out.len() {
        return None;
    }
    for (i, pair) in hex.chunks_exact(2).enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(hex.len() / 2)
}

/// One hex digit → its 0–15 value; `None` if not a hex digit.
fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a one-line DB for `user`/`password` with the given `home`, using the
    /// real KDF (so the test exercises the same path the seeder + verifier do).
    fn db_line(user: &str, password: &str, salt: &[u8], home: &str) -> std::string::String {
        use std::fmt::Write;
        let iters = libcrypto::password::DEFAULT_ITERATIONS;
        let v = libcrypto::password::derive(password.as_bytes(), salt, iters);
        let mut s = std::string::String::new();
        write!(s, "{user}:").unwrap();
        for b in salt {
            write!(s, "{b:02x}").unwrap();
        }
        write!(s, ":{iters}:").unwrap();
        for b in &v {
            write!(s, "{b:02x}").unwrap();
        }
        write!(s, ":{home}").unwrap();
        s
    }

    #[test]
    fn correct_password_authenticates() {
        let db = db_line("alice", "correct horse", b"\x01\x02\x03\x04", "/home/alice");
        assert_eq!(
            authenticate(db.as_bytes(), b"alice", b"correct horse"),
            AuthOutcome::Authenticated { principal: b"alice", home: b"/home/alice" }
        );
    }

    #[test]
    fn wrong_password_denied() {
        let db = db_line("alice", "correct horse", b"\x01\x02\x03\x04", "/home/alice");
        assert_eq!(authenticate(db.as_bytes(), b"alice", b"wrong"), AuthOutcome::Denied);
    }

    #[test]
    fn unknown_user_denied() {
        let db = db_line("alice", "pw", b"salt1234", "/home/alice");
        assert_eq!(authenticate(db.as_bytes(), b"bob", b"pw"), AuthOutcome::Denied);
    }

    #[test]
    fn comments_and_blanks_ignored_multiuser() {
        let mut db = std::string::String::new();
        db.push_str("# the user database\n\n");
        db.push_str(&db_line("alice", "apw", b"aaaa1111", "/home/alice"));
        db.push('\n');
        db.push_str(&db_line("bob", "bpw", b"bbbb2222", "/home/bob"));
        db.push_str("\n# trailing comment\n");
        assert!(matches!(
            authenticate(db.as_bytes(), b"bob", b"bpw"),
            AuthOutcome::Authenticated { home, .. } if home == b"/home/bob"
        ));
        assert_eq!(authenticate(db.as_bytes(), b"alice", b"bpw"), AuthOutcome::Denied);
    }

    #[test]
    fn serve_builds_authenticated_then_denied_replies() {
        use librsproto::auth::{
            build_authenticate_request, parse_authenticate_reply, AUTH_RESULT_DENIED,
        };
        let db = db_line("alice", "hunter2", b"\x10\x20\x30\x40", "/home/alice");

        let mut req = [0u8; 128];
        let rn = build_authenticate_request(&mut req, b"alice", b"hunter2").unwrap();
        let mut reply = [0u8; 128];
        let n = serve_authenticate(&req[..rn], db.as_bytes(), &mut reply).unwrap();
        let r = parse_authenticate_reply(&reply[..n]).unwrap();
        assert!(r.is_authenticated());
        assert_eq!(r.home, b"/home/alice");

        let rn = build_authenticate_request(&mut req, b"alice", b"nope").unwrap();
        let n = serve_authenticate(&req[..rn], db.as_bytes(), &mut reply).unwrap();
        let r = parse_authenticate_reply(&reply[..n]).unwrap();
        assert!(!r.is_authenticated());
        assert_eq!(r.result, AUTH_RESULT_DENIED);
    }

    #[test]
    fn serve_rejects_malformed_request() {
        let db = db_line("alice", "pw", b"salt5678", "/home/alice");
        let mut reply = [0u8; 64];
        // A body too short to hold the request prefix → None (caller sends ERROR).
        assert!(serve_authenticate(&[0u8; 2], db.as_bytes(), &mut reply).is_none());
    }

    #[test]
    fn hex_and_int_parsers() {
        let mut out = [0u8; 4];
        assert_eq!(hex_decode(b"deadBEEF", &mut out), Some(4));
        assert_eq!(out, [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(hex_decode(b"abc", &mut out), None); // odd length
        assert_eq!(hex_decode(b"xy", &mut out), None); // non-hex
        assert_eq!(parse_u32(b"4096"), Some(4096));
        assert_eq!(parse_u32(b""), None);
        assert_eq!(parse_u32(b"12a"), None);
    }
}
