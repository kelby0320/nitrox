//! `libprompt` — asking a person for a line (echo on) or a password (echo off) on the terminal the
//! shell handed the program, and the channel plumbing ([`ipc`]) a password crosses afterwards.
//!
//! Which prompt to ask, and when, stays with each caller: this crate is how a question is put to a
//! terminal and how the answer is kept out of the stack. A stage with no terminal must say so
//! rather than read a password from a stream.
//!
//! Input goes to the oldest terminal with a read pending, so an earlier reader gets the line.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod ipc;

use alloc::vec::Vec;
use libkern::scrub;
use librsproto::{OP_TTY_INTERRUPT, OP_TTY_READ_LINE, OP_TTY_SET_MODE, OP_TTY_WRITE, TTY_MODE_ECHO};

use ipc::{recv, send, wait};

/// One exchange with the terminal, stepping over an `Interrupt` and recording it in `interrupted`:
/// `Ctrl-C` at a password prompt means "never mind".
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

/// Ask for a line on `term`, echo on. `None` on `Ctrl-C`, `Ctrl-D` or a terminal failure.
pub fn ask_line(term: u64, prompt: &[u8]) -> Option<Vec<u8>> {
    let mut interrupted = false;
    let _ = tty(term, OP_TTY_SET_MODE, &[TTY_MODE_ECHO], &mut interrupted);
    let _ = tty(term, OP_TTY_WRITE, prompt, &mut interrupted);
    match tty(term, OP_TTY_READ_LINE, &[], &mut interrupted) {
        Some((false, bytes)) if !interrupted => Some(bytes),
        _ => None,
    }
}

/// Ask for a password on `term`, echo off. `None` on `Ctrl-C`, `Ctrl-D` or a terminal failure.
pub fn ask_password(term: u64, prompt: &[u8]) -> Option<Vec<u8>> {
    ask_password_then(term, prompt, &mut || {})
}

/// [`ask_password`], calling `asked` once echo is off and before the read: a gate waiting on a
/// line the program logs there cannot type the password while echo is still on.
pub fn ask_password_then(term: u64, prompt: &[u8], asked: &mut dyn FnMut()) -> Option<Vec<u8>> {
    let mut interrupted = false;
    let _ = tty(term, OP_TTY_SET_MODE, &[0], &mut interrupted);
    let _ = tty(term, OP_TTY_WRITE, prompt, &mut interrupted);
    asked();
    let line = tty(term, OP_TTY_READ_LINE, &[], &mut interrupted);
    // Echo back on first: a shell that inherited echo off would type blind.
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

/// A new password typed twice on `term`, then [`confirm`]ed.
pub fn ask_new_password(term: u64, prompt: &[u8]) -> Result<Vec<u8>, NewPassword> {
    ask_new_password_then(term, prompt, &mut |_| {})
}

/// [`ask_new_password`], calling `asked` with `1` and then `2` as each read is ready, echo off.
/// Echo goes back on between the two reads, so one receipt for both would let the second copy be
/// typed while it was on.
pub fn ask_new_password_then(term: u64, prompt: &[u8], asked: &mut dyn FnMut(u8)) -> Result<Vec<u8>, NewPassword> {
    let mut first = ask_password_then(term, prompt, &mut || asked(1)).ok_or(NewPassword::Cancelled)?;
    let Some(second) = ask_password_then(term, b"again: ", &mut || asked(2)) else {
        scrub(&mut first);
        return Err(NewPassword::Cancelled);
    };
    confirm(first, second)
}

/// The password typed twice, if the two agree and it keeps the rules. The second copy is always
/// zeroed; the first is kept only when it is the answer. Nothing is sent before this, so a typing
/// mistake changes nothing.
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
