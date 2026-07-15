//! Pass the absolute path to `user.ld` as a `-T` link arg. Cargo runs the linker
//! from the target directory, so a relative path in `.cargo/config.toml` would not
//! resolve — this mirrors `kernel/build.rs`.
//!
//! Like init, service-mgr is also a **library** with host unit tests
//! (`cargo test -p service-mgr --lib`). The fixed-address bare-target script must
//! NOT reach that host link (it corrupts it — the linker errors), so we use
//! `rustc-link-arg-bins`, which applies only to the `[[bin]]`, never to the lib
//! test binary.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=user.ld");
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let script = PathBuf::from(dir).join("user.ld");
    println!("cargo::rustc-link-arg-bins=-T{}", script.display());
}
