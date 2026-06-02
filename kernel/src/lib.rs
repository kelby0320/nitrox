//! Nitrox kernel library crate.
//!
//! The kernel image proper is the `nitrox-kernel` binary in this same
//! Cargo package; this library exists so that algorithmic kernel code
//! (allocators, ABI codecs, namespace resolution, etc.) can be exercised
//! by `cargo test` on the host.
//!
//! Modules are `no_std` under normal builds and against the host's `std`
//! when compiled for tests — the `cfg_attr` below controls that switch.
//! The `_start` entry point and `#[panic_handler]` live in `main.rs`
//! because they must only exist in the bin.

#![cfg_attr(not(test), no_std)]

pub mod arch;
pub mod font;
pub mod framebuffer;
pub mod handle;
pub mod libkern;
pub mod limine;
pub mod mm;
pub mod object;
pub mod sched;
