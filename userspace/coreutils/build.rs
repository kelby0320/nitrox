//! Pass the absolute path to `user.ld` as a `-T` link arg: Cargo runs the linker from the target
//! directory, so a relative path in `.cargo/config.toml` would not resolve.
//!
//! The fixed-address script must NOT reach the host link of the library's tests, so this uses
//! `rustc-link-arg-bins`, which applies only to the `[[bin]]`s. Same reason as `init`'s build.rs.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=user.ld");
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let script = PathBuf::from(dir).join("user.ld");
    println!("cargo::rustc-link-arg-bins=-T{}", script.display());
}
