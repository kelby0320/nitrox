//! `libviews`: the view broker's client, below `with`, `account` and `desktop-shell`, which all
//! ask it. The wire codec is `librsproto::views` (`docs/spec/rsproto-views-ops.md`); this crate
//! asks no person anything, so a policy wanting a password is a refusal where no caller can prompt.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use libprompt::ipc;

use alloc::string::String;
use alloc::vec::Vec;
use libkern::Rights;
use libkern::abi::IPC_PAYLOAD_SIZE;
use libos::{CanLookup, Carried, Channel, Deadline, Handle, Namespace, NsMutable, Only, Ref};
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
pub const BROKER_PATH: &str = "/dev/views";

/// The request id [`request`] sends with. A password for the same request is sent with a later one.
pub const REQUEST_ID: u64 = 1;

/// A channel to the broker at `ns`'s [`BROKER_PATH`], tied to `ns`'s session; `None` if none.
pub fn broker<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>) -> Option<Handle<Channel, Only>> {
    ipc::lookup_in(ns, BROKER_PATH, Rights::SEND | Rights::RECV | Rights::WAIT)
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

/// The namespace and streams a request hands the program, each moved to the broker.
pub struct Handed {
    /// The namespace to build the view from. The broker copies it, and never uses it as it is.
    pub ns: Handle<Namespace, NsMutable>,
    /// The program's `stdin`.
    pub stdin: Option<Handle<Channel, Only>>,
    /// The program's `stdout`.
    pub stdout: Option<Handle<Channel, Only>>,
    /// The program's `stderr`.
    pub stderr: Option<Handle<Channel, Only>>,
    /// The terminal the program may prompt on.
    pub terminal: Option<Handle<Channel, Only>>,
    /// A `Tty::Token` ([`token`]); not a handle: the broker redeems it with the terminal server.
    pub token: Option<[u8; TTY_TOKEN_LEN]>,
}

impl Handed {
    /// `ns` alone: no streams, terminal or token.
    pub fn new(ns: Handle<Namespace, NsMutable>) -> Self {
        Handed { ns, stdin: None, stdout: None, stderr: None, terminal: None, token: None }
    }

    /// The handle bits, and the handles in the order the broker takes them: namespace, then
    /// streams.
    pub fn wire(self) -> (u8, Carried) {
        let mut bits = 0u8;
        let mut handles = Carried::new();
        // Five at most, inside a message's room, so no push is refused.
        let _ = handles.push(self.ns);
        for (bit, h) in [
            (REQ_STDIN, self.stdin),
            (REQ_STDOUT, self.stdout),
            (REQ_STDERR, self.stderr),
            (REQ_TERMINAL, self.terminal),
        ] {
            if let Some(h) = h {
                bits |= bit;
                let _ = handles.push(h);
            }
        }
        (bits, handles)
    }
}

/// Ask the broker on `ch` to run `program` in `view`; every handle in `handed` is gone on return
/// (sent, or closed here). [`Outcome::NeedPassword`] is answered by [`password`]. `env` is a
/// serialised record or empty; the broker sets `view` in it.
pub fn request<'a>(
    ch: impl Into<Ref<'a, Channel, Only>>,
    view: &str,
    program: &str,
    args: &[&str],
    env: &[u8],
    handed: Handed,
    deadline: Deadline,
) -> Result<(Outcome, String), Failed> {
    let ch = ch.into();
    let token = handed.token;
    let (bits, mut handles) = handed.wire();
    let arg_bytes: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
    let mut body = alloc::vec![0u8; IPC_PAYLOAD_SIZE - 64];
    let Some(n) = build_request(&mut body, bits, view.as_bytes(), program.as_bytes(), &arg_bytes, env, token.as_ref())
    else {
        return Err(Failed::TooLarge);
    };
    // What a refused send leaves in `handles` closes as it drops.
    if !ipc::transmit(ch, OP_VIEWS_REQUEST, REQUEST_ID, &body[..n], &mut handles) {
        return Err(Failed::NoAnswer);
    }
    match ipc::answer_on(ch, REQUEST_ID, deadline) {
        Some((false, body)) => Ok(outcome(&body)),
        Some((true, _)) => Err(Failed::Refused),
        None => Err(Failed::NoAnswer),
    }
}

/// The request id a [`token`] is asked as; a terminal answers by id, and this is never a read's.
const TOKEN_REQUEST_ID: u64 = 0x544f_4b45;

/// A one-time `Tty::Token` for `term`, so the broker learns the terminal from its server; `None`
/// if `term` does not answer by `deadline`, and the request then asks for a password.
pub fn token<'a>(term: impl Into<Ref<'a, Channel, Only>>, deadline: Deadline) -> Option<[u8; TTY_TOKEN_LEN]> {
    let term = term.into();
    if !ipc::transmit(term, OP_TTY_TOKEN, TOKEN_REQUEST_ID, &[], &mut Carried::new()) {
        return None;
    }
    match ipc::answer_on(term, TOKEN_REQUEST_ID, deadline)? {
        (false, body) => body.as_slice().try_into().ok(),
        (true, _) => None,
    }
}

/// Forget every password, on every terminal, that the session on `ch` has remembered.
pub fn forget<'a>(ch: impl Into<Ref<'a, Channel, Only>>, deadline: Deadline) -> Result<(), Failed> {
    let ch = ch.into();
    if !ipc::transmit(ch, OP_VIEWS_FORGET, REQUEST_ID, &[], &mut Carried::new()) {
        return Err(Failed::NoAnswer);
    }
    match ipc::answer_on(ch, REQUEST_ID, deadline) {
        Some((false, _)) => Ok(()),
        Some((true, _)) => Err(Failed::Refused),
        None => Err(Failed::NoAnswer),
    }
}

/// A password for the request waiting on `ch`: the broker's answer, after the session's delay.
pub fn password<'a>(
    ch: impl Into<Ref<'a, Channel, Only>>,
    request_id: u64,
    pw: &[u8],
) -> Result<(Outcome, String), Failed> {
    match ipc::call_on(ch, OP_VIEWS_PASSWORD, request_id, pw, &mut Carried::new()) {
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
pub fn list<'a>(ch: impl Into<Ref<'a, Channel, Only>>, deadline: Deadline) -> Result<Vec<Row>, Failed> {
    let ch = ch.into();
    if !ipc::transmit(ch, OP_VIEWS_LIST, REQUEST_ID, &[], &mut Carried::new()) {
        return Err(Failed::NoAnswer);
    }
    let body = match ipc::answer_on(ch, REQUEST_ID, deadline) {
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
pub fn decide<'a>(
    ch: impl Into<Ref<'a, Channel, Only>>,
    view: &str,
    program: &str,
    deadline: Deadline,
) -> Result<(Outcome, String), Failed> {
    let ch = ch.into();
    let mut body = [0u8; 512];
    let Some(n) = build_decide(&mut body, view.as_bytes(), program.as_bytes()) else {
        return Err(Failed::TooLarge);
    };
    if !ipc::transmit(ch, OP_VIEWS_DECIDE, REQUEST_ID, &body[..n], &mut Carried::new()) {
        return Err(Failed::NoAnswer);
    }
    match ipc::answer_on(ch, REQUEST_ID, deadline) {
        Some((false, body)) => Ok(outcome(&body)),
        Some((true, _)) => Err(Failed::Refused),
        None => Err(Failed::NoAnswer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libos::mock;
    use librsproto::views::parse_request;

    fn channel() -> Handle<Channel, Only> {
        Handle::<Channel, Only>::pair(1).unwrap().0
    }

    /// The handles in order, as the values the kernel knows them by.
    fn raws(mut handles: Carried) -> Vec<u64> {
        (0..handles.len()).filter_map(|i| handles.take::<Channel, Only>(i)).map(|h| h.raw().0).collect()
    }

    #[test]
    fn a_requests_handles_are_in_the_order_the_broker_takes_them() {
        mock::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let (n, i, o, e, t) = (ns.raw().0, channel(), channel(), channel(), channel());
        let want = [n, i.raw().0, o.raw().0, e.raw().0, t.raw().0];
        let all = Handed { ns, stdin: Some(i), stdout: Some(o), stderr: Some(e), terminal: Some(t), token: None };
        let (bits, handles) = all.wire();
        assert_eq!(bits, REQ_STDIN | REQ_STDOUT | REQ_STDERR | REQ_TERMINAL);
        assert_eq!(raws(handles), want);
        // A gap is closed up: the broker counts the bits, not the slots.
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let (o, t) = (channel(), channel());
        let want = [ns.raw().0, o.raw().0, t.raw().0];
        let (bits, handles) = Handed { stdout: Some(o), terminal: Some(t), ..Handed::new(ns) }.wire();
        assert_eq!((bits, raws(handles)), (REQ_STDOUT | REQ_TERMINAL, want.to_vec()));
    }

    #[test]
    fn a_request_the_wire_builds_is_one_the_broker_reads_with_as_many_handles() {
        mock::reset();
        let h = Handed { stderr: Some(channel()), ..Handed::new(Handle::<Namespace, NsMutable>::create().unwrap()) };
        let (bits, handles) = h.wire();
        let mut body = alloc::vec![0u8; IPC_PAYLOAD_SIZE - 64];
        let n = build_request(&mut body, bits, b"power", b"shutdown", &[b"--reboot"], &[], None).unwrap();
        let r = parse_request(&body[..n]).unwrap();
        assert_eq!(r.handle_count(), handles.len());
        assert_eq!((r.view, r.program), (&b"power"[..], &b"shutdown"[..]));
    }

    #[test]
    fn a_request_too_large_closes_every_handle_it_was_given() {
        mock::reset();
        let (ch, _broker) = Handle::<Channel, Only>::pair(1).unwrap();
        let open = mock::open_handles();
        let h = Handed { stdout: Some(channel()), ..Handed::new(Handle::<Namespace, NsMutable>::create().unwrap()) };
        let huge = "a".repeat(IPC_PAYLOAD_SIZE);
        let r = request(&ch, "admin", &huge, &[], &[], h, Deadline::POLL);
        assert_eq!(r, Err(Failed::TooLarge));
        assert_eq!(mock::open_handles(), open, "the namespace and the stream closed, neither sent");
    }
}
