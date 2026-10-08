//! `coreutils` — shared machinery for the Nitrox coreutils.
//!
//! The programs themselves (`list`, `copy`, …) are the crate's bins; this library is what
//! they have in common:
//!
//! - [`stage`] — the Tier-0/Tier-1 startup prologue: streams, `argv`, `stderr`, exits.
//! - [`args`] — GNU-style flag parsing (`--long`, `-f`, `--`, `--help`/`--version`).
//! - [`entropy`] — random bytes from the kernel: `account`'s salts, `disk`'s IDs (Phase 6 Part G.4).
//! - [`format`] — what `disk --partition` and `disk --format` write, decided before they write it
//!   (Phase 6 Part G.4).
//!
//! **Asking a person on a terminal** is [`libprompt`], and the view broker's client [`libviews`] —
//! each moved out when a program that is not a coreutil needed it (administration Parts F.3 and
//! G.2).
//!
//! The **filesystem** half is [`libfs`], not something in here — it moved out in M10 Part A
//! when a graphical file browser needed it and did not need any of the above. The directory
//! client they all use is [`librsproto::session::Dir`], likewise not here: it belongs beside
//! the protocol it speaks.
//!
//! Every program is an ordinary process speaking **TSM1 on stdio** — not a resource
//! server. It may be a *client* of one (as each of these is a client of the fs-server),
//! but implementing `librsproto`'s server side is not what it takes to join a pipeline
//! (design §3).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod args;
pub mod entropy;
pub mod format;
pub mod stage;
/// Calendar arithmetic and duration parsing.
///
/// **Moved out to [`libtime`] in M11 Part E batch 9** and re-exported here, because it grew a
/// second consumer: `desktop-shell` formats the same instant for the clock on its top bar.
/// `userspace/CLAUDE.md`'s rule is what moved it — a helper with one consumer belongs to that
/// consumer, a helper with two belongs below both — and the shell reaching into this crate for it
/// is exactly the shape that rule exists to catch.
pub use libtime as time;

pub use args::{ArgError, Args, Flag, parse};
pub use stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
