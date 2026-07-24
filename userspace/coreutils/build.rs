//! Pass the absolute path to `user.ld` as a `-T` link arg. Cargo runs the linker
//! from the target directory, so a relative path in `.cargo/config.toml` would not
//! resolve — this mirrors `kernel/build.rs`.
//!
//! `coreutils` is a **library** (the shared stage/args machinery, with host unit
//! tests: `cargo test -p coreutils --lib`) as well as a set of bare-target program
//! bins. The fixed-address bare-target script must NOT reach the host link — it
//! breaks it — so this uses `rustc-link-arg-bins`, which applies only to the
//! `[[bin]]`s, never to the lib test binary. Same reason as `init`'s build.rs.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=user.ld");
    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let script = PathBuf::from(dir).join("user.ld");
    println!("cargo::rustc-link-arg-bins=-T{}", script.display());
}
