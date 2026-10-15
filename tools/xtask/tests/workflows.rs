use std::path::Path;

const BUILD_INPUTS: [&str; 12] = [
    "rust-toolchain.toml",
    "userspace/rust-toolchain.toml",
    "userspace/x86_64-unknown-nitrox.json",
    ".cargo/config.toml",
    "kernel/.cargo/config.toml",
    "userspace/.cargo/config.toml",
    "kernel/Cargo.toml",
    "userspace/Cargo.toml",
    "tools/Cargo.toml",
    "kernel/Cargo.lock",
    "userspace/Cargo.lock",
    "tools/Cargo.lock",
];

/// The path-filtered gates run when what every crate is built with changes, so a toolchain bump or
/// a lockfile update is held by them in its own PR.
#[test]
fn the_filtered_gates_run_on_every_build_input() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for workflow in ["display.yml", "input.yml"] {
        let text = std::fs::read_to_string(root.join(".github/workflows").join(workflow)).unwrap();
        for input in BUILD_INPUTS {
            assert!(root.join(input).is_file(), "{input} no longer exists");
            assert!(text.contains(&format!("      - '{input}'\n")), "{workflow} does not run on {input}");
        }
    }
}
