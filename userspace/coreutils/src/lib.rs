//! `coreutils` — shared machinery for the Nitrox coreutils.
//!
//! The programs themselves (`list`, `copy`, …) are the crate's bins; this library is what
//! they have in common:
//!
//! - [`stage`] — the Tier-0/Tier-1 startup prologue: streams, `argv`, `stderr`, exits.
//! - [`args`] — GNU-style flag parsing (`--long`, `-f`, `--`, `--help`/`--version`).
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
pub mod stage;
pub mod time;

pub use args::{ArgError, Args, Flag, parse};
pub use stage::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Stage};
