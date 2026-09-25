//! `auth-service` — the credential logic (host-testable).
//!
//! Pure, `#![no_std]`, no-`alloc`: verify a `(username, password)` against the user database, and
//! decide what an administrator's request does to it, answering the `Auth` rsproto category
//! (`docs/spec/rsproto-auth-ops.md`). No syscalls — the bare-target server (`src/main.rs`) supplies
//! the database's bytes, the buffers, and a salt from the kernel's entropy source, and does the
//! writing. Under `cargo test` it builds as host `std`.
//!
//! **The database's format is `libusers`'** (administration Part D.1): a `passwd`-style line file,
//! `name:salt_hex:iterations:verifier_hex:home`, whose `verifier` is
//! `PBKDF2-HMAC-SHA256(password, salt, iterations)` — one-way; the password is never stored. It was
//! parsed here until the build's seeder and `account`'s offline mode needed to write it too. See
//! `userspace/auth-service/CLAUDE.md`.

#![cfg_attr(not(test), no_std)]

use libkern::KError;
use librsproto::auth::{
    AUTH_RESULT_AUTHENTICATED, AccountListWriter, OP_AUTH_ADD, OP_AUTH_LIST, OP_AUTH_REMOVE,
    OP_AUTH_SET_PASSWORD, build_authenticate_reply, build_denied_reply, parse_account_request,
    parse_authenticate_request,
};

/// The PBKDF2 verifier length (one SHA-256 block), matching `libcrypto`.
pub const VERIFIER_LEN: usize = libusers::VERIFIER_LEN;

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
    match libusers::find(db, username) {
        Some(rec) if rec.verifies(password) => {
            AuthOutcome::Authenticated { principal: rec.name, home: rec.home }
        }
        Some(_) => AuthOutcome::Denied,
        None => {
            // Unknown user: run an equivalent derivation so timing does not distinguish it.
            let _ = libcrypto::password::verify(
                password,
                &DUMMY_SALT,
                libcrypto::password::DEFAULT_ITERATIONS,
                &DUMMY_VERIFIER,
            );
            AuthOutcome::Denied
        }
    }
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

// --- administration (Part D.1) ------------------------------------------------------------------

/// What an administrator's request comes to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Admin {
    /// Answer with the first `n` bytes of the reply buffer; nothing is written.
    Reply(usize),
    /// Install the first `n` bytes of the new-database buffer as the file, then answer with an
    /// empty body. **Nothing has changed until the install holds**: the caller keeps the old
    /// database if it does not.
    Write(usize),
    /// Refuse, with this error and reason.
    Refused(Refused),
}

/// Why an administrator's request was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The edit itself: a bad name or password, a name taken or missing, a file too large.
    Edit(libusers::Refusal),
    /// The body does not parse, or does not account for itself exactly.
    Malformed,
    /// Not one of the four administrative ops.
    NotAdmin,
    /// The account list would not fit in one message.
    ListTooLong,
}

impl Refused {
    /// The error a client is answered with.
    pub fn kerror(self) -> KError {
        use libusers::Refusal;
        match self {
            Refused::Edit(Refusal::BadName | Refusal::BadPassword) | Refused::Malformed => {
                KError::InvalidArgument
            }
            Refused::Edit(Refusal::Exists) => KError::AlreadyExists,
            Refused::Edit(Refusal::NoSuchAccount) => KError::NotFound,
            Refused::Edit(Refusal::TooLarge) | Refused::ListTooLong => KError::TooLarge,
            Refused::NotAdmin => KError::Unsupported,
        }
    }

    /// The reason, as the refusal says it.
    pub fn why(self) -> &'static [u8] {
        match self {
            Refused::Edit(r) => r.why(),
            Refused::Malformed => b"a malformed request",
            Refused::NotAdmin => b"not an administrative request",
            Refused::ListTooLong => b"the account list does not fit in one message",
        }
    }
}

/// **Serve one administrator's request** on `db`: `List`, `Add`, `Remove` or `SetPassword`. A new
/// password is derived under `salt`, which the caller draws fresh for each request. A write goes
/// into `new_db`, and a reply into `reply`.
pub fn serve_admin(
    op: u16,
    body: &[u8],
    db: &[u8],
    salt: &[u8; libusers::SALT_LEN],
    new_db: &mut [u8],
    reply: &mut [u8],
) -> Admin {
    let edit = |r: Result<usize, libusers::Refusal>| match r {
        Ok(n) => Admin::Write(n),
        Err(e) => Admin::Refused(Refused::Edit(e)),
    };
    match op {
        OP_AUTH_LIST => {
            if !body.is_empty() {
                return Admin::Refused(Refused::Malformed);
            }
            let Some(mut w) = AccountListWriter::new(reply) else {
                return Admin::Refused(Refused::ListTooLong);
            };
            for r in libusers::records(db) {
                if w.push(r.name, r.home).is_none() {
                    return Admin::Refused(Refused::ListTooLong);
                }
            }
            Admin::Reply(w.finish())
        }
        OP_AUTH_ADD => match parse_account_request(body) {
            Some(r) => edit(libusers::add(db, new_db, r.username, r.password, salt)),
            None => Admin::Refused(Refused::Malformed),
        },
        OP_AUTH_REMOVE => edit(libusers::remove(db, new_db, body)),
        OP_AUTH_SET_PASSWORD => match parse_account_request(body) {
            Some(r) => edit(libusers::set_password(db, new_db, r.username, r.password, salt)),
            None => Admin::Refused(Refused::Malformed),
        },
        _ => Admin::Refused(Refused::NotAdmin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::auth::{build_account_request, parse_account_list};

    /// Build a one-line DB for `user`/`password` with the given `home`, using the
    /// real KDF (so the test exercises the same path the seeder + verifier do).
    fn db_line(user: &str, password: &str, salt: &[u8], home: &str) -> std::string::String {
        let mut out = [0u8; libusers::MAX_FILE];
        let n = libusers::write_record(&mut out, user.as_bytes(), home.as_bytes(), password.as_bytes(), salt)
            .unwrap();
        std::string::String::from_utf8(out[..n].to_vec()).unwrap()
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
        db.push_str(&db_line("bob", "bpw", b"bbbb2222", "/home/bob"));
        db.push_str("# trailing comment\n");
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

    // --- administration ------------------------------------------------------------------------

    const SALT: [u8; libusers::SALT_LEN] = *b"0123456789abcdef";

    /// One admin request against `db`: what it came to, and the new file if it wrote one.
    fn admin(op: u16, body: &[u8], db: &str) -> (Admin, std::string::String) {
        let mut new_db = [0u8; libusers::MAX_FILE];
        let mut reply = [0u8; 4096];
        let a = serve_admin(op, body, db.as_bytes(), &SALT, &mut new_db, &mut reply);
        let file = match a {
            Admin::Write(n) => std::string::String::from_utf8(new_db[..n].to_vec()).unwrap(),
            _ => std::string::String::new(),
        };
        (a, file)
    }

    fn account(name: &str, password: &str) -> std::vec::Vec<u8> {
        let mut b = [0u8; 256];
        let n = build_account_request(&mut b, name.as_bytes(), password.as_bytes()).unwrap();
        b[..n].to_vec()
    }

    /// **An account added, its password set, then removed — each step authenticating as the file
    /// it wrote says.** The service installs what `Write` holds, so this is the whole of what an
    /// administrator's request does to the database.
    #[test]
    fn an_account_is_added_changed_and_removed() {
        let db = format!("# header\n{}", db_line("alice", "apw", b"s1", "/home/alice"));

        let (a, db) = admin(OP_AUTH_ADD, &account("bob", "first"), &db);
        assert!(matches!(a, Admin::Write(_)));
        assert_eq!(
            authenticate(db.as_bytes(), b"bob", b"first"),
            AuthOutcome::Authenticated { principal: b"bob", home: b"/home/bob" }
        );

        let (a, db) = admin(OP_AUTH_SET_PASSWORD, &account("bob", "second"), &db);
        assert!(matches!(a, Admin::Write(_)));
        assert_eq!(authenticate(db.as_bytes(), b"bob", b"first"), AuthOutcome::Denied);
        assert!(matches!(authenticate(db.as_bytes(), b"bob", b"second"), AuthOutcome::Authenticated { .. }));

        let (a, db) = admin(OP_AUTH_REMOVE, b"bob", &db);
        assert!(matches!(a, Admin::Write(_)));
        assert_eq!(authenticate(db.as_bytes(), b"bob", b"second"), AuthOutcome::Denied);
        assert!(
            matches!(authenticate(db.as_bytes(), b"alice", b"apw"), AuthOutcome::Authenticated { .. }),
            "and alice, untouched throughout"
        );
        assert!(db.starts_with("# header\n"));
    }

    /// **`List` names every account and its home**, and writes nothing.
    #[test]
    fn a_list_names_every_account() {
        let db = format!(
            "# header\n{}{}",
            db_line("alice", "a", b"s1", "/home/alice"),
            db_line("bob", "b", b"s2", "/home/bob")
        );
        let mut new_db = [0u8; libusers::MAX_FILE];
        let mut reply = [0u8; 4096];
        let Admin::Reply(n) = serve_admin(OP_AUTH_LIST, &[], db.as_bytes(), &SALT, &mut new_db, &mut reply) else {
            panic!("a list replies");
        };
        let l = parse_account_list(&reply[..n]).unwrap();
        let got: std::vec::Vec<_> = l.iter().collect();
        assert_eq!(got, [(&b"alice"[..], &b"/home/alice"[..]), (&b"bob"[..], &b"/home/bob"[..])]);
    }

    /// **Each refusal, with the error a client is answered with.**
    #[test]
    fn an_admin_request_is_refused_for_each_reason() {
        use libusers::Refusal;
        let db = db_line("alice", "a", b"s1", "/home/alice");
        let refused = |op, body: &[u8]| match admin(op, body, &db).0 {
            Admin::Refused(r) => Some((r, r.kerror())),
            _ => None,
        };
        let edit = |r, k| Some((Refused::Edit(r), k));
        assert_eq!(refused(OP_AUTH_ADD, &account("alice", "x")), edit(Refusal::Exists, KError::AlreadyExists));
        assert_eq!(refused(OP_AUTH_ADD, &account("Bob", "x")), edit(Refusal::BadName, KError::InvalidArgument));
        assert_eq!(refused(OP_AUTH_ADD, &account("bob", "")), edit(Refusal::BadPassword, KError::InvalidArgument));
        assert_eq!(refused(OP_AUTH_REMOVE, b"bob"), edit(Refusal::NoSuchAccount, KError::NotFound));
        assert_eq!(
            refused(OP_AUTH_SET_PASSWORD, &account("bob", "x")),
            Some((Refused::Edit(Refusal::NoSuchAccount), KError::NotFound))
        );
        let mut trailing = account("bob", "x");
        trailing.push(0);
        assert_eq!(refused(OP_AUTH_ADD, &trailing), Some((Refused::Malformed, KError::InvalidArgument)));
        assert_eq!(refused(OP_AUTH_LIST, b"x"), Some((Refused::Malformed, KError::InvalidArgument)));
        assert_eq!(
            refused(librsproto::OP_AUTHENTICATE, b""),
            Some((Refused::NotAdmin, KError::Unsupported)),
            "an admin session does not authenticate"
        );
    }

    /// **A list too long for the reply buffer is refused**, not cut short: a partial list would
    /// read as the whole of it.
    #[test]
    fn a_list_that_does_not_fit_is_refused() {
        let db = db_line("alice", "a", b"s1", "/home/alice");
        let mut new_db = [0u8; libusers::MAX_FILE];
        let mut reply = [0u8; 8];
        assert_eq!(
            serve_admin(OP_AUTH_LIST, &[], db.as_bytes(), &SALT, &mut new_db, &mut reply),
            Admin::Refused(Refused::ListTooLong)
        );
    }
}
