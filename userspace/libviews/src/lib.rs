//! `libviews` — **the view broker's client**: asking the broker to run a program in a view, and the
//! channel plumbing that carries the asking (`docs/spec/rsproto-views-ops.md`).
//!
//! **Below `with` because the desktop asks too** (administration Part F.3). The request was
//! `with`'s own until the power menu's Restart and Shut down needed to make it, from `desktop-shell`,
//! which cannot reach into a coreutil — `userspace/CLAUDE.md`'s rule for a helper with a second
//! consumer. `with`, `account` and `desktop-shell` use it; `coreutils::ipc` came with it as
//! [`ipc`].
//!
//! **What stays with each caller is how it asks a person.** `with` prompts on its terminal for a
//! password. The desktop has no graphical prompt yet (`docs/design/graphical-prompt.md`), so a
//! policy asking for one is a refusal there, and [`decide`] lets it find that out before it closes
//! a window.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod ipc;

use alloc::string::String;
use alloc::vec::Vec;
use libkern::abi::IPC_PAYLOAD_SIZE;
use libkern::{RIGHT_RECV, RIGHT_SEND, RIGHT_WAIT};
pub use librsproto::views::Outcome;
use librsproto::views::{
    OP_VIEWS_DECIDE, OP_VIEWS_LIST, OP_VIEWS_PASSWORD, OP_VIEWS_REQUEST, REQ_STDERR, REQ_STDIN, REQ_STDOUT,
    REQ_TERMINAL, build_decide, build_request, parse_rows,
};

/// Where a session's namespace binds the broker.
pub const BROKER_PATH: &[u8] = b"/dev/views";

/// The request id [`request`] sends with. A password for the same request is sent with a later one.
pub const REQUEST_ID: u64 = 1;

/// **The broker, as `ns` reaches it**: a channel of its own at `ns`'s [`BROKER_PATH`], which the
/// broker ties to the session whose namespace that is. `0` if there is none.
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
    /// The broker answered with an error rather than an answer: for a listing, a policy that does
    /// not read; for anything else, a broker that does not know the op.
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

/// **What a request hands the program**: the namespace the broker builds the view from, and the
/// program's streams. Each handle present moves to the broker with the request.
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
}

impl Handed {
    /// The request's handle bits and its handles **in the order the broker takes them**: the
    /// namespace, then each stream present, in the order of its bit.
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

/// **Ask the broker on `ch` to run `program` with `args` in `view`**, handing it `handed`: the
/// outcome and its reason, or why there was none.
///
/// `env` is a serialised record for the program's environment, or empty for none; the broker sets
/// `view` in it either way. The request goes as [`REQUEST_ID`]. **Every handle in `handed` is gone
/// when this returns**: sent, the broker's; not sent, closed here. An outcome of
/// [`Outcome::NeedPassword`] is answered with [`password`], on the same channel.
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
    let Some(n) = build_request(&mut body, bits, view.as_bytes(), program.as_bytes(), &arg_bytes, env) else {
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
        Some((false, body)) => Ok(ipc::outcome(&body)),
        Some((true, _)) => Err(Failed::Refused),
        None => Err(Failed::NoAnswer),
    }
}

/// **A password for the request waiting on `ch`**, sent as `request_id`: the broker's answer, once
/// it has checked it — after the session's delay, when the last one was wrong.
pub fn password(ch: u64, request_id: u64, pw: &[u8]) -> Result<(Outcome, String), Failed> {
    match ipc::call(ch, OP_VIEWS_PASSWORD, request_id, pw, &[]) {
        Some((false, body)) => Ok(ipc::outcome(&body)),
        Some((true, _)) => Err(Failed::Refused),
        None => Err(Failed::NoAnswer),
    }
}

/// One view a person may use, as the broker lists it: one per view a rule names, in the policy's
/// order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The view.
    pub view: String,
    /// The programs the rule lets them run in it: `*`, or names separated by spaces.
    pub run: String,
    /// Whether the rule asks for their password.
    pub password: bool,
}

/// **The views the person `ch`'s session belongs to may use.**
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

/// **What the broker would answer a request for `program` in `view`** — the outcome and its
/// reason — asked with `Decide`, which runs nothing.
///
/// **The broker's own rule, not a reading of `List`.** This was a client applying the rule to the
/// listing's rows until the PR #346 review found the broker cuts a listing at 2 KiB: a row past the
/// cut read as "no rule", refusing what the policy allows. A policy changed between this and the
/// request is the request's to answer: this is a way to know before asking, not a way to ask.
pub fn decide(ch: u64, view: &str, program: &str, deadline: u64) -> Result<(Outcome, String), Failed> {
    let mut body = [0u8; 512];
    let Some(n) = build_decide(&mut body, view.as_bytes(), program.as_bytes()) else {
        return Err(Failed::TooLarge);
    };
    if !ipc::send(ch, OP_VIEWS_DECIDE, REQUEST_ID, &body[..n], &[]) {
        return Err(Failed::NoAnswer);
    }
    match ipc::answer(ch, REQUEST_ID, deadline) {
        Some((false, body)) => Ok(ipc::outcome(&body)),
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
        let all = Handed { ns: 1, stdin: Some(2), stdout: Some(3), stderr: Some(4), terminal: Some(5) };
        assert_eq!(all.wire(), (REQ_STDIN | REQ_STDOUT | REQ_STDERR | REQ_TERMINAL, alloc::vec![1, 2, 3, 4, 5]));
        // A gap is closed up: the broker counts the bits, not the slots.
        let some = Handed { ns: 1, stdout: Some(3), terminal: Some(5), ..Handed::default() };
        assert_eq!(some.wire(), (REQ_STDOUT | REQ_TERMINAL, alloc::vec![1, 3, 5]));
        // The desktop's: the namespace alone.
        assert_eq!(Handed { ns: 7, ..Handed::default() }.wire(), (0, alloc::vec![7]));
    }

    #[test]
    fn a_request_the_wire_builds_is_one_the_broker_reads_with_as_many_handles() {
        let h = Handed { ns: 1, stderr: Some(4), ..Handed::default() };
        let (bits, handles) = h.wire();
        let mut body = alloc::vec![0u8; IPC_PAYLOAD_SIZE - 64];
        let n = build_request(&mut body, bits, b"power", b"shutdown", &[b"--reboot"], &[]).unwrap();
        let r = parse_request(&body[..n]).unwrap();
        assert_eq!(r.handle_count(), handles.len());
        assert_eq!((r.view, r.program), (&b"power"[..], &b"shutdown"[..]));
    }
}
