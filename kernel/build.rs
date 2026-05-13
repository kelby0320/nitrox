//! Emits the absolute path to `linker.ld` as a `-T` link arg. Cargo runs
//! the linker from the target directory, so a relative path inside
//! `.cargo/config.toml` would not resolve.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let script = PathBuf::from(manifest_dir).join("linker.ld");
    println!("cargo::rerun-if-changed=linker.ld");
    println!("cargo::rustc-link-arg=-T{}", script.display());
}
