//! init's host-testable internals: the first message a resource server sends ([`ready`]), the
//! shutdown request ([`unmount`]) and, re-exported from `libinittoml`, the manifest parsers.
//! `src/main.rs` is the bare-target entry point; `std` under `cargo test`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod ready;
pub mod unmount;
/// The manifest parser, shared with the storage service and kept at its old path here.
pub use libinittoml::{manifest, toml_lite};

use libkern::{RIGHT_DUPLICATE, RIGHT_READ, RIGHT_TRANSFER, RIGHT_WRITE};

/// **What a mount's server holds its device with**: `WRITE` only for a read-write mount, so a
/// read-only one's cannot write it whatever `ReadOnly` does above it, nor have the kernel write a
/// file back to it. `TRANSFER` to hand it over, `DUPLICATE` for the kernel's copy.
pub fn device_rights(mode: manifest::Mode) -> u64 {
    let write = if mode == manifest::Mode::Rw { RIGHT_WRITE } else { 0 };
    RIGHT_READ | write | RIGHT_TRANSFER | RIGHT_DUPLICATE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_read_write_mounts_server_may_write_its_device() {
        let both = RIGHT_READ | RIGHT_TRANSFER | RIGHT_DUPLICATE;
        assert_eq!(device_rights(manifest::Mode::Ro), both);
        assert_eq!(device_rights(manifest::Mode::Rw), both | RIGHT_WRITE);
    }
}
