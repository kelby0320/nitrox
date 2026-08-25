//! Pass the absolute path to `user.ld` as a `-T` link arg. Cargo runs the linker
//! from the target directory, so a relative path in `.cargo/config.toml` would not
//! resolve — this mirrors `kernel/build.rs`.
//!
//! `rustc-link-arg-bins` rather than `rustc-link-arg`: it applies only to the `[[bin]]`,
//! never to a lib test binary, so a crate that later grows host tests does not have the
//! fixed-address bare-target script corrupt their link.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=user.ld");
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let script = PathBuf::from(dir).join("user.ld");
    println!("cargo::rustc-link-arg-bins=-T{}", script.display());
}
