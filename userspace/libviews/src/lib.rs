//! `libviews`: the view broker's client, below `with`, `account` and `desktop-shell`, which all
//! ask it. The wire codec is `librsproto::views` (`docs/spec/rsproto-views-ops.md`); this crate
//! asks no person anything, so a policy wanting a password is a refusal where no caller can prompt.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use libprompt::ipc;

use alloc::string::String;
use alloc::vec::Vec;
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::{RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT};
pub use librsproto::views::Outcome;
use librsproto::views::{
    OP_VIEWS_DECIDE, OP_VIEWS_FORGET, OP_VIEWS_LIST, OP_VIEWS_PASSWORD, OP_VIEWS_REQUEST, REQ_STDERR, REQ_STDIN,
    REQ_STDOUT, REQ_TERMINAL, build_decide, build_request, parse_rows,
};
use librsproto::{OP_TTY_TOKEN, TTY_TOKEN_LEN};

/// The broker's answer as outcome and reason; one that does not read is a denial.
pub fn outcome(body: &[u8]) -> (Outcome, String) {
    match librsproto::views::parse_outcome(body) {
        Some((o, why)) => (o, String::from_utf8_lossy(why).into_owned()),
        None => (Outcome::Denied { retry: false }, String::from("the broker's answer did not read")),
    }
}

/// Where a session's namespace binds the broker.
pub const BROKER_PATH: &[u8] = b"/dev/views";

/// The request id [`request`] sends with. A password for the same request is sent with a later one.
pub const REQUEST_ID: u64 = 1;

/// A channel to the broker at `ns`'s [`BROKER_PATH`], tied to `ns`'s session; `0` if none.
pub fn broker(ns: u64) -> u64 {
    ipc::lookup(ns, BROKER_PATH, RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT)
}

/// Why the broker gave no answer to act on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Failed {
    /// What was asked does not fit one message.
    TooLarge,
    /// It could not be sent, or the broker went away or ran past the deadline before answering.
    NoAnswer,
    /// An error answer: for a listing, an unreadable policy; else the broker lacks the op.
    Refused,
    /// The answer did not read.
    Garbled,
}

impl Failed {
    /// What to tell a person.
    pub fn why(self) -> &'static str {
        match self {
            Failed::TooLarge => "the request is too large",
            Failed::NoAnswer => "the view broker did not answer",
            Failed::Refused => "the view broker answered with an error",
            Failed::Garbled => "the view broker's answer did not read",
        }
    }
}

/// The namespace and streams a request hands the program; each handle present moves to the broker.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Handed {
    /// The namespace to build the view from. The broker copies it, and never uses it as it is.
    pub ns: u64,
    /// The program's `stdin`.
    pub stdin: Option<u64>,
    /// The program's `stdout`.
    pub stdout: Option<u64>,
    /// The program's `stderr`.
    pub stderr: Option<u64>,
    /// The terminal the program may prompt on.
    pub terminal: Option<u64>,
    /// A `Tty::Token` ([`token`]); not a handle: the broker redeems it with the terminal server.
    pub token: Option<[u8; TTY_TOKEN_LEN]>,
}

impl Handed {
    /// The handle bits and handles in the order the broker takes them: namespace, then streams.
    pub fn wire(&self) -> (u8, Vec<u64>) {
        let mut bits = 0u8;
        let mut handles = alloc::vec![self.ns];
        for (bit, h) in [
            (REQ_STDIN, self.stdin),
            (REQ_STDOUT, self.stdout),
            (REQ_STDERR, self.stderr),
            (REQ_TERMINAL, self.terminal),
        ] {
            if let Some(h) = h {
                bits |= bit;
                handles.push(h);
            }
        }
        (bits, handles)
    }
}

/// Ask the broker on `ch` to run `program` in `view`; every handle in `handed` is gone on return
/// (sent, or closed here). [`Outcome::NeedPassword`] is answered by [`password`]. `env` is a
/// serialised record or empty; the broker sets `view` in it.
pub fn request(
    ch: u64,
    view: &str,
    program: &str,
    args: &[&str],
    env: &[u8],
    handed: Handed,
    deadline: u64,
) -> Result<(Outcome, String), Failed> {
    let (bits, handles) = handed.wire();
    let arg_bytes: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
    let mut body = alloc::vec![0u8; IPC_PAYLOAD_SIZE - 64];
    let token = handed.token.as_ref();
    let Some(n) = build_request(&mut body, bits, view.as_bytes(), program.as_bytes(), &arg_bytes, env, token) else {
        for h in handles {
            ipc::close(h);
        }
        return Err(Failed::TooLarge);
    };
    if !ipc::send(ch, OP_VIEWS_REQUEST, REQUEST_ID, &body[..n], &handles) {
        for h in handles {
            ipc::close(h);
        }
        return Err(Failed::NoAnswer);
    }
    match ipc::answer(ch, REQUEST_ID, deadline) {
        Some((false, body)) => Ok(outcome(&body)),
        Some((true, _)) => Err(Failed::Refused),
        None => Err(Failed::NoAnswer),
    }
}

/// The request id a [`token`] is asked as; a terminal answers by id, and this is never a read's.
const TOKEN_REQUEST_ID: u64 = 0x544f_4b45;

/// A one-time `Tty::Token` for `term`, so the broker learns the terminal from its server; `None`
/// if `term` does not answer by `deadline`, and the request then asks for a password.
pub fn token(term: u64, deadline: u64) -> Option<[u8; TTY_TOKEN_LEN]> {
    if !ipc::send(term, OP_TTY_TOKEN, TOKEN_REQUEST_ID, &[], &[]) {
        return None;
    }
    match ipc::answer(term, TOKEN_REQUEST_ID, deadline)? {
        (false, body) => body.as_slice().try_into().ok(),
        (true, _) => None,
    }
}

/// Forget every password, on every terminal, that the session on `ch` has remembered.
pub fn forget(ch: u64, deadline: u64) -> Result<(), Failed> {
    if !ipc::send(ch, OP_VIEWS_FORGET, REQUEST_ID, &[], &[]) {
        return Err(Failed::NoAnswer);
    }
    match ipc::answer(ch, REQUEST_ID, deadline) {
        Some((false, _)) => Ok(()),
        Some((true, _)) => Err(Failed::Refused),
        None => Err(Failed::NoAnswer),
    }
}

/// A password for the request waiting on `ch`: the broker's answer, after the session's delay.
pub fn password(ch: u64, request_id: u64, pw: &[u8]) -> Result<(Outcome, String), Failed> {
    match ipc::call(ch, OP_VIEWS_PASSWORD, request_id, pw, &[]) {
        Some((false, body)) => Ok(outcome(&body)),
        Some((true, _)) => Err(Failed::Refused),
        None => Err(Failed::NoAnswer),
    }
}

/// One view a person may use, one per view a rule names, in the policy's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The view's name.
    pub view: String,
    /// The programs the rule lets them run in it: `*`, or names separated by spaces.
    pub run: String,
    /// Whether the rule asks for their password.
    pub password: bool,
}

/// The views the person `ch`'s session belongs to may use.
pub fn list(ch: u64, deadline: u64) -> Result<Vec<Row>, Failed> {
    if !ipc::send(ch, OP_VIEWS_LIST, REQUEST_ID, &[], &[]) {
        return Err(Failed::NoAnswer);
    }
    let body = match ipc::answer(ch, REQUEST_ID, deadline) {
        Some((false, body)) => body,
        Some((true, _)) => return Err(Failed::Refused),
        None => return Err(Failed::NoAnswer),
    };
    let mut rows = Vec::new();
    let parsed = parse_rows(&body, |r| {
        rows.push(Row {
            view: String::from_utf8_lossy(r.view).into_owned(),
            run: String::from_utf8_lossy(r.run).into_owned(),
            password: r.password,
        })
    });
    match parsed {
        Some(_) => Ok(rows),
        None => Err(Failed::Garbled),
    }
}

/// What the broker would answer a request, via `Decide` (runs nothing); not `List`, cut at 2 KiB.
/// A policy changed between this and the request is the request's to answer: this is a way to
/// know before asking, not a way to ask.
pub fn decide(ch: u64, view: &str, program: &str, deadline: u64) -> Result<(Outcome, String), Failed> {
    let mut body = [0u8; 512];
    let Some(n) = build_decide(&mut body, view.as_bytes(), program.as_bytes()) else {
        return Err(Failed::TooLarge);
    };
    if !ipc::send(ch, OP_VIEWS_DECIDE, REQUEST_ID, &body[..n], &[]) {
        return Err(Failed::NoAnswer);
    }
    match ipc::answer(ch, REQUEST_ID, deadline) {
        Some((false, body)) => Ok(outcome(&body)),
        Some((true, _)) => Err(Failed::Refused),
        None => Err(Failed::NoAnswer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::views::parse_request;

    #[test]
    fn a_requests_handles_are_in_the_order_the_broker_takes_them() {
        let all = Handed { ns: 1, stdin: Some(2), stdout: Some(3), stderr: Some(4), terminal: Some(5), token: None };
        assert_eq!(all.wire(), (REQ_STDIN | REQ_STDOUT | REQ_STDERR | REQ_TERMINAL, alloc::vec![1, 2, 3, 4, 5]));
        // A gap is closed up: the broker counts the bits, not the slots.
        let some = Handed { ns: 1, stdout: Some(3), terminal: Some(5), ..Handed::default() };
        assert_eq!(some.wire(), (REQ_STDOUT | REQ_TERMINAL, alloc::vec![1, 3, 5]));
        assert_eq!(Handed { ns: 7, ..Handed::default() }.wire(), (0, alloc::vec![7]));
    }

    #[test]
    fn a_request_the_wire_builds_is_one_the_broker_reads_with_as_many_handles() {
        let h = Handed { ns: 1, stderr: Some(4), ..Handed::default() };
        let (bits, handles) = h.wire();
        let mut body = alloc::vec![0u8; IPC_PAYLOAD_SIZE - 64];
        let n = build_request(&mut body, bits, b"power", b"shutdown", &[b"--reboot"], &[], None).unwrap();
        let r = parse_request(&body[..n]).unwrap();
        assert_eq!(r.handle_count(), handles.len());
        assert_eq!((r.view, r.program), (&b"power"[..], &b"shutdown"[..]));
    }
}
