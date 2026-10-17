//! `coreutils` — shared machinery for the Nitrox coreutils.
//!
//! The programs themselves (`list`, `copy`, …) are the crate's bins; this library is what
//! they have in common:
//!
//! - [`stage`] — the Tier-0/Tier-1 startup prologue: streams, `argv`, `stderr`, exits.
//! - [`args`] — GNU-style flag parsing (`--long`, `-f`, `--`, `--help`/`--version`).
//! - [`entropy`] — random bytes from the kernel: `account`'s salts, `disk`'s IDs.
//! - [`format`] — what `disk --partition` and `disk --format` write, decided before writing.
//!
//! **Asking a person on a terminal** is [`libprompt`], and the view broker's client [`libviews`].
//! The **filesystem** half is [`libfs`], and the directory client they all use is
//! [`librsproto::session::Dir`]; neither is in here.
//!
//! Every program is an ordinary process speaking **TSM1 on stdio**, not a resource server.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod args;
pub mod entropy;
pub mod format;
pub mod stage;
pub mod text;
/// Calendar arithmetic and duration parsing, re-exported from [`libtime`], which
/// `desktop-shell` also uses.
pub use libtime as time;

pub use args::{ArgError, Args, Flag, parse};
pub use stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
