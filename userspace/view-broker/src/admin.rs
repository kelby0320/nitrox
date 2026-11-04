//! The broker's dealings with `auth-service` and the policy file: the accounts it lists, adds,
//! removes and sets a password for, and the policy it shows, judges and installs.

use crate::wire::{
    Ch, Root, lookup, now_ns, recv, recv_full, reply_error, reply_outcome, send, send_request, wait_until,
};
use crate::{Broker, Endpoint, POLICY_PATH};
use alloc::string::String;
use alloc::vec::Vec;
use libkern::{KError, Rights, scrub};
use librsproto::auth::{
    OP_AUTH_ADD, OP_AUTH_LIST, OP_AUTH_REMOVE, OP_AUTH_SET_PASSWORD, build_account_request, parse_account_list,
    parse_account_request,
};
use librsproto::views::*;
use librsproto::{RS_FLAG_ERROR, RS_FLAG_REPLY};
use view_broker::accounts;
use view_broker::policy;

/// How long the broker waits for `auth-service`'s account list.
const ACCOUNTS_WAIT_NS: u64 = 5_000_000_000;
/// Where a new policy is written before it is renamed over the old one.
const POLICY_NEW: &str = "/system/views.toml.new";

/// The reason an error reply gives in its `ErrorBody`, or its `KError` when it gives none.
fn refusal(body: &[u8]) -> String {
    match librsproto::error::parse_error(body) {
        Some(e) if !e.msg.is_empty() => String::from_utf8_lossy(e.msg).into_owned(),
        Some(e) => alloc::format!("{:?}", KError::from_i32(e.kerror)),
        None => String::from("an answer that does not read"),
    }
}

/// What a refused name is told: the rule, never the name back — it has not been checked, so it
/// goes into no log line and no path.
const NAME_RULE: &str =
    "a name is 1 to 32 bytes: a lowercase letter or `_`, then lowercase letters, digits, `_` or `-`";

impl Broker {
    /// **The accounts that exist**, from `auth-service`'s `List`. `None` if it cannot be asked (the
    /// session dropped, for the next ask to reopen); judging a policy then fails closed.
    pub(crate) fn accounts(&mut self) -> Option<Vec<(String, String)>> {
        let body = self.ask_auth(OP_AUTH_LIST, &[]).ok()?;
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        Some(parse_account_list(&body)?.iter().map(|(n, h)| (text(n), text(h))).collect())
    }

    /// **Ask `auth-service`'s admin session**, opened on first need: send `op` with `body`, and
    /// wait at most `ACCOUNTS_WAIT_NS` for the answer — its body, or the reason it refused. A
    /// session that does not answer is dropped, so the next ask opens a new one.
    ///
    /// **The body may carry a password**, so the send buffer is zeroed as soon as the kernel has
    /// the message; the caller zeroes its own copy.
    pub(crate) fn ask_auth(&mut self, op: u16, body: &[u8]) -> Result<Vec<u8>, String> {
        if self.auth_admin.is_none() {
            self.auth_admin = lookup(&self.root_ns, "/svc/auth/admin", Rights::SEND | Rights::RECV | Rights::WAIT);
        }
        let Some(admin) = &self.auth_admin else {
            return Err(String::from("auth-service could not be reached"));
        };
        let asked = send_request(admin, op, 1, body);
        let deadline = now_ns().saturating_add(ACCOUNTS_WAIT_NS);
        let answer = asked
            .then(|| {
                loop {
                    match recv_full(admin) {
                        Ok(Some((o, 1, flags, body, _))) if o == op => break Some((flags & RS_FLAG_ERROR != 0, body)),
                        Ok(Some(_)) => {}
                        Ok(None) if wait_until(admin, deadline) => {}
                        _ => break None,
                    }
                }
            })
            .flatten();
        match answer {
            Some((false, body)) => Ok(body),
            Some((true, body)) => Err(refusal(&body)),
            None => {
                self.auth_admin = None;
                Err(String::from("auth-service did not answer"))
            }
        }
    }

    /// Judge a policy's text as `Check` and `Install` do (`policy::check`): the reason, if not.
    pub(crate) fn judge(&mut self, text: &[u8]) -> Result<policy::Policy, String> {
        let text = core::str::from_utf8(text).map_err(|_| String::from("the file is not text"))?;
        let accounts = self
            .accounts()
            .ok_or_else(|| String::from("the accounts could not be listed, so the policy cannot be judged"))?;
        let names: Vec<&str> = accounts.iter().map(|(n, _)| n.as_str()).collect();
        policy::check(text, &names).map_err(|e| alloc::format!("{e}"))
    }

    /// A policy or accounts channel. Each is reached only through its grant's binding, so
    /// holding it is the authority.
    pub(crate) fn serve_endpoint(&mut self, i: usize) {
        let (op, request_id, mut body, _sent) = match recv(&self.endpoints[i].0) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                self.endpoints.remove(i);
                return;
            }
        };
        let who = self.principal(self.endpoints[i].1).unwrap_or_default();
        match self.endpoints[i].2 {
            Endpoint::Policy => self.serve_policy(i, op, request_id, &body, &who),
            Endpoint::Accounts => self.serve_accounts(i, op, request_id, &body, &who),
        }
        scrub(&mut body);
    }

    /// The channel of endpoint `i`.
    fn channel(&self, i: usize) -> &Ch {
        &self.endpoints[i].0
    }

    pub(crate) fn serve_policy(&mut self, i: usize, op: u16, request_id: u64, body: &[u8], who: &str) {
        match op {
            OP_VIEWS_SHOW => match libfs::read(&self.root_ns, POLICY_PATH) {
                Ok(text) if text.len() <= POLICY_MAX => {
                    let _ = send(self.channel(i), op, request_id, RS_FLAG_REPLY, &text);
                }
                Ok(_) => reply_error(self.channel(i), op, request_id, KError::TooLarge),
                Err(_) => reply_error(self.channel(i), op, request_id, KError::NotFound),
            },
            OP_VIEWS_INSTALL => {
                let installed = if body.len() > POLICY_MAX {
                    Err(alloc::format!("a policy is at most {POLICY_MAX} bytes"))
                } else {
                    self.judge(body).and_then(|_| {
                        libfs::write(&self.root_ns, POLICY_NEW, body)
                            .and_then(|()| libfs::rename_path(&self.root_ns, POLICY_NEW, POLICY_PATH, true))
                            .map_err(|_| String::from("the policy could not be written"))
                    })
                };
                match installed {
                    Ok(()) => {
                        self.audit(&alloc::format!("policy: {who} installed a new policy"));
                        reply_outcome(self.channel(i), op, request_id, Outcome::Started, "the policy is installed");
                    }
                    Err(why) => {
                        self.audit(&alloc::format!("policy: {who}'s policy was refused: {why}"));
                        reply_outcome(self.channel(i), op, request_id, Outcome::Denied { retry: false }, &why);
                    }
                }
            }
            _ => reply_error(self.channel(i), op, request_id, KError::Unsupported),
        }
    }

    /// **`Accounts`**: anyone's to read, as a Unix `passwd` file is.
    pub(crate) fn list_accounts(&mut self, i: usize, request_id: u64) {
        let Some(listed) = self.accounts() else {
            return self.refuse(i, OP_VIEWS_ACCOUNTS, request_id, KError::IoError);
        };
        let policy = self.read_policy().ok();
        let mut out = [0u8; POLICY_MAX];
        let mut at = 2;
        for a in accounts::shown(&listed, &self.sessions, policy.as_ref()) {
            let row = AccountRow {
                name: a.name.as_bytes(),
                home: a.home.as_bytes(),
                sessions: a.sessions.min(u16::MAX as usize) as u16,
                administers: a.administers,
            };
            if push_account(&mut out, &mut at, &row).is_none() {
                return self.refuse(i, OP_VIEWS_ACCOUNTS, request_id, KError::TooLarge);
            }
        }
        self.answer(i, OP_VIEWS_ACCOUNTS, request_id, &out[..at]);
    }

    /// **An accounts channel**: `AddAccount`, `RemoveAccount` and `SetPassword`, and nothing else.
    /// Every answer is audited as the session's principal, and a refusal with the guard that
    /// refused it.
    pub(crate) fn serve_accounts(&mut self, i: usize, op: u16, request_id: u64, body: &[u8], who: &str) {
        let (asked, answer) = match op {
            OP_VIEWS_ADD_ACCOUNT => ("add", self.add_account(body)),
            OP_VIEWS_REMOVE_ACCOUNT => ("removal", self.remove_account(body)),
            OP_VIEWS_SET_PASSWORD => ("password setting", self.set_password(body)),
            _ => return reply_error(self.channel(i), op, request_id, KError::Unsupported),
        };
        match answer {
            Ok(done) => {
                self.audit(&alloc::format!("accounts: {who} {done}"));
                reply_outcome(self.channel(i), op, request_id, Outcome::Started, &done);
            }
            Err(why) => {
                self.audit(&alloc::format!("accounts: {who}'s {asked} was refused: {why}"));
                reply_outcome(self.channel(i), op, request_id, Outcome::Denied { retry: false }, &why);
            }
        }
    }

    /// **`AddAccount`**: the name and password checked, the home and its folders made — or an
    /// existing home adopted — and then the record, by `auth-service`. A home this made is taken
    /// away again **only when the record is known not to be there** ([`accounts::settled`]), so a
    /// refusal leaves nothing behind and a slow add never leaves an account without its home.
    fn add_account(&mut self, body: &[u8]) -> Result<String, String> {
        let r = parse_account_request(body).ok_or_else(|| String::from("the request does not read"))?;
        if !libusers::valid_name(r.username) {
            return Err(String::from(NAME_RULE));
        }
        if !libusers::valid_password(r.password) {
            return Err(String::from("a password is 1 to 128 bytes"));
        }
        // Checked, so ASCII: fit for a path and a log line.
        let name = String::from_utf8_lossy(r.username).into_owned();
        let listed = self.accounts().ok_or_else(|| String::from("the accounts could not be listed"))?;
        if listed.iter().any(|(n, _)| *n == name) {
            return Err(alloc::format!("an account named {name} already exists"));
        }
        let home = alloc::format!("/home/{name}");
        let made = match libfs::make_dir(&self.root_ns, &home) {
            Ok(()) => true,
            Err(_) if libfs::is_directory(&self.root_ns, &home) => false,
            Err(_) => return Err(alloc::format!("{home} could not be made")),
        };
        let undo = |root: &Root| {
            if made {
                let _ = libfs::remove_recursive(root, &home, &mut |_, _| {});
            }
        };
        // The folders `nxfiles`' sidebar offers; the installer makes them for the first account.
        for folder in libfs::HOME_FOLDERS {
            let dir = libfs::join(home.as_bytes(), folder.as_bytes());
            if libfs::make_dir(&self.root_ns, &dir).is_err() && !libfs::is_directory(&self.root_ns, &dir) {
                undo(&self.root_ns);
                return Err(alloc::format!("{dir} could not be made"));
            }
        }
        let mut req = [0u8; 256];
        let asked = match build_account_request(&mut req, r.username, r.password) {
            Some(n) => self.ask_auth(OP_AUTH_ADD, &req[..n]).map(|_| ()),
            None => Err(String::from("the request does not fit")),
        };
        scrub(&mut req);
        let (settled, why) = match asked {
            Ok(()) => (accounts::Settled::Done, String::new()),
            Err(why) => (accounts::settled(self.accounts().as_deref(), &name, true), why),
        };
        match settled {
            accounts::Settled::Done if made => Ok(alloc::format!("added {name}, with a new home at {home}")),
            accounts::Settled::Done => Ok(alloc::format!("added {name}, adopting {home}, which was already there")),
            accounts::Settled::NotDone => {
                undo(&self.root_ns);
                Err(why)
            }
            accounts::Settled::Unknown => {
                Err(alloc::format!("{why}; whether {name} was added cannot be said, so {home} is kept"))
            }
        }
    }

    /// **`RemoveAccount`**: guarded by `accounts::refuse_removal`; then the record, by
    /// `auth-service`, and the home only if asked.
    fn remove_account(&mut self, body: &[u8]) -> Result<String, String> {
        let (name, home_too) = parse_remove_account(body).ok_or_else(|| String::from("the request does not read"))?;
        if !libusers::valid_name(name) {
            return Err(String::from(NAME_RULE));
        }
        let name = String::from_utf8_lossy(name).into_owned();
        let listed = self.accounts().ok_or_else(|| String::from("the accounts could not be listed"))?;
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        let policy = self.read_policy();
        let policy = policy.as_ref().map_err(String::as_str);
        if let Some(why) = accounts::refuse_removal(&name, &names, &self.sessions, policy) {
            return Err(why);
        }
        let home = alloc::format!("/home/{name}");
        // Settled the same way as an add: a removal whose answer never came may have happened.
        if let Err(why) = self.ask_auth(OP_AUTH_REMOVE, name.as_bytes()) {
            match accounts::settled(self.accounts().as_deref(), &name, false) {
                accounts::Settled::Done => {}
                accounts::Settled::NotDone => return Err(why),
                accounts::Settled::Unknown => {
                    return Err(alloc::format!("{why}; whether {name} was removed cannot be said, so {home} is kept"));
                }
            }
        }
        if !home_too {
            return Ok(alloc::format!("removed {name}, keeping {home}"));
        }
        Ok(match libfs::remove_recursive(&self.root_ns, &home, &mut |_, _| {}) {
            Ok(()) => alloc::format!("removed {name}, and {home} with it"),
            Err(_) if !libfs::is_directory(&self.root_ns, &home) => {
                alloc::format!("removed {name}; there was no {home}")
            }
            Err(_) => alloc::format!("removed {name}, but {home} could not be removed"),
        })
    }

    /// **`SetPassword`**: an administrator setting someone's password, with no current one to
    /// prove — the grant is the proof.
    fn set_password(&mut self, body: &[u8]) -> Result<String, String> {
        let r = parse_account_request(body).ok_or_else(|| String::from("the request does not read"))?;
        if !libusers::valid_name(r.username) {
            return Err(String::from(NAME_RULE));
        }
        if !libusers::valid_password(r.password) {
            return Err(String::from("a password is 1 to 128 bytes"));
        }
        let name = String::from_utf8_lossy(r.username).into_owned();
        let mut req = [0u8; 256];
        let asked = match build_account_request(&mut req, r.username, r.password) {
            Some(n) => self.ask_auth(OP_AUTH_SET_PASSWORD, &req[..n]).map(|_| ()),
            None => Err(String::from("the request does not fit")),
        };
        scrub(&mut req);
        asked.map(|()| alloc::format!("set a new password for {name}"))
    }
}
