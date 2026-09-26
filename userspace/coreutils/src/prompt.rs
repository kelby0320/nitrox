//! **Asking a person for a password**, on the terminal their shell handed the stage, echo off.
//!
//! `with` asks for the password a rule wants, and `account` for a new one and the current one
//! (administration Part D.4) — which is why this moved out of `with`. A stage that has no
//! terminal has nobody to ask, and must say so rather than read one from a stream.
//!
//! **An older reader on the same backend gets the line first**: input goes to the oldest terminal
//! with a read pending, so a program still reading one — an earlier stage of this pipeline, or one
//! a previous command left behind — receives what is typed at the prompt. `sudo` has the same
//! limit; `docs/planning/administration.md` records it.

use alloc::vec::Vec;
use libkern::scrub;
use librsproto::{OP_TTY_INTERRUPT, OP_TTY_READ_LINE, OP_TTY_SET_MODE, OP_TTY_WRITE, TTY_MODE_ECHO};

use crate::ipc::{recv, send, wait};

/// One exchange with the terminal, stepping over an `Interrupt` — which it records in
/// `interrupted`, since `Ctrl-C` at a password prompt means "never mind".
fn tty(term: u64, op: u16, body: &[u8], interrupted: &mut bool) -> Option<(bool, Vec<u8>)> {
    if !send(term, op, 1, body, &[]) {
        return None;
    }
    loop {
        wait(&[term])?;
        match recv(term) {
            Ok(Some((OP_TTY_INTERRUPT, 0, _, _))) => *interrupted = true,
            Ok(Some((_, 1, err, body))) => return Some((err, body)),
            Ok(_) => continue,
            Err(()) => return None,
        }
    }
}

/// Ask for a password on `term`, echo off. `None` if the person pressed `Ctrl-C` or `Ctrl-D`, or
/// the terminal failed.
pub fn ask_password(term: u64, prompt: &[u8]) -> Option<Vec<u8>> {
    let mut interrupted = false;
    let _ = tty(term, OP_TTY_SET_MODE, &[0], &mut interrupted);
    let _ = tty(term, OP_TTY_WRITE, prompt, &mut interrupted);
    let line = tty(term, OP_TTY_READ_LINE, &[], &mut interrupted);
    // **Echo back on before anything else**: this terminal goes to a program next, and an
    // elevated shell that inherited echo off would type blind.
    let _ = tty(term, OP_TTY_SET_MODE, &[TTY_MODE_ECHO], &mut interrupted);
    let _ = tty(term, OP_TTY_WRITE, b"\r\n", &mut interrupted);
    match line {
        Some((false, bytes)) if !interrupted => Some(bytes),
        Some((_, mut bytes)) => {
            scrub(&mut bytes);
            None
        }
        None => None,
    }
}

/// Why [`ask_new_password`] has no password to give.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NewPassword {
    /// `Ctrl-C` or `Ctrl-D`, or the terminal failed.
    Cancelled,
    /// The two did not match.
    Mismatch,
    /// Not 1 to 128 bytes (`libusers::valid_password`) — said here, before anything is sent.
    Invalid,
}

impl NewPassword {
    /// What to tell the person.
    pub fn why(self) -> &'static str {
        match self {
            NewPassword::Cancelled => "cancelled",
            NewPassword::Mismatch => "the two passwords did not match; nothing was changed",
            NewPassword::Invalid => "a password is 1 to 128 bytes; nothing was changed",
        }
    }
}

/// **A new password, typed twice** on `term`: `prompt` for the first, `again:` for the second, then
/// [`confirm`]ed.
pub fn ask_new_password(term: u64, prompt: &[u8]) -> Result<Vec<u8>, NewPassword> {
    let mut first = ask_password(term, prompt).ok_or(NewPassword::Cancelled)?;
    let Some(second) = ask_password(term, b"again: ") else {
        scrub(&mut first);
        return Err(NewPassword::Cancelled);
    };
    confirm(first, second)
}

/// **The password typed twice, if the two agree and it keeps the rules.** The second copy is
/// always zeroed; the first is kept only when it is the answer. Nothing is sent before this, so a
/// typing mistake changes nothing.
pub fn confirm(mut first: Vec<u8>, mut second: Vec<u8>) -> Result<Vec<u8>, NewPassword> {
    let same = first == second;
    scrub(&mut second);
    let refused = if !same {
        Some(NewPassword::Mismatch)
    } else if !libusers::valid_password(&first) {
        Some(NewPassword::Invalid)
    } else {
        None
    };
    match refused {
        Some(r) => {
            scrub(&mut first);
            Err(r)
        }
        None => Ok(first),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_password_is_the_one_typed_twice_within_the_rules() {
        let v = |s: &[u8]| s.to_vec();
        assert_eq!(confirm(v(b"hunter2"), v(b"hunter2")), Ok(v(b"hunter2")));
        assert_eq!(confirm(v(b"hunter2"), v(b"hunter3")), Err(NewPassword::Mismatch));
        assert_eq!(confirm(v(b"hunter2"), v(b"hunter2 ")), Err(NewPassword::Mismatch), "a trailing space");
        assert_eq!(confirm(v(b""), v(b"")), Err(NewPassword::Invalid), "empty, however often");
        let longest = [b'x'; libusers::PASSWORD_MAX];
        assert!(confirm(longest.to_vec(), longest.to_vec()).is_ok(), "the longest there may be");
        let over = [b'x'; libusers::PASSWORD_MAX + 1];
        assert_eq!(confirm(over.to_vec(), over.to_vec()), Err(NewPassword::Invalid), "one byte more");
    }
}
