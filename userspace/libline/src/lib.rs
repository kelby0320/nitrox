//! `libline` — the line discipline: what a byte typed does to a line, and what to write so the
//! screen keeps up. `tty-server` drives it in canonical mode for a terminal's readers; `nxsh` reads
//! raw and drives it to edit its own prompt. One implementation, deployed twice.

#![cfg_attr(not(test), no_std)]

extern crate alloc;
