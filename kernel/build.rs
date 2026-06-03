//! Emits the absolute path to `linker.ld` as a `-T` link arg when
//! building for the bare-metal kernel target. Cargo runs the linker from
//! the target directory, so a relative path inside `.cargo/config.toml`
//! would not resolve.
//!
//! The linker script is only valid for the `x86_64-unknown-none` target.
//! Host builds (e.g. `cargo test --target x86_64-unknown-linux-gnu`)
//! must skip it; injecting a freestanding ELF layout into a std-linked
//! test binary fails with `STT_TLS without PT_TLS` errors.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=linker.ld");
    // The bin embeds the first userspace program via `include_bytes!`
    // (kernel/src/main.rs). `include_bytes!` already tracks the file as a
    // rustc input, but declare it here too so a rebuilt `hello` reliably
    // re-embeds. xtask builds `hello` before the kernel.
    println!("cargo::rerun-if-changed=../userspace/target/x86_64-unknown-none/release/hello");
    let target = env::var("TARGET").expect("TARGET");
    if target == "x86_64-unknown-none" {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let script = PathBuf::from(manifest_dir).join("linker.ld");
        println!("cargo::rustc-link-arg=-T{}", script.display());
    }
}
