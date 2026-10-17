//! Pass the absolute path to `user.ld` as a `-T` link arg, as `kernel/build.rs` does; a relative
//! path would not resolve from the target directory. `rustc-link-arg-bins` keeps the
//! fixed-address script off the lib's host test link, which it corrupts.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=user.ld");
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let script = PathBuf::from(dir).join("user.ld");
    println!("cargo::rustc-link-arg-bins=-T{}", script.display());
}
