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

/// The path-filtered gates run on a change to any of xtask's sources: a gate's code is spread over
/// its own module and the ones it shares, so a filter naming one file lets a change to another
/// skip the gate it breaks.
#[test]
fn the_filtered_gates_run_on_every_xtask_source() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut sources = Vec::new();
    rust_files(&root.join("tools/xtask/src"), &mut sources);
    assert!(sources.iter().any(|s| s.ends_with("gates/display.rs")), "{sources:?}");
    for workflow in ["display.yml", "input.yml"] {
        let text = std::fs::read_to_string(root.join(".github/workflows").join(workflow)).unwrap();
        let filters: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("      - '")?.strip_suffix('\'')).collect();
        for source in &sources {
            let path = source.strip_prefix(&root).unwrap().to_str().unwrap();
            let runs = filters.iter().any(|f| match f.strip_suffix("/**") {
                Some(dir) => path.starts_with(&format!("{dir}/")),
                None => *f == path,
            });
            assert!(runs, "{workflow} does not run on {path}");
        }
    }
}

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// A filtered file whose module was split keeps its modules filtered: an entry naming `x.rs` beside
/// a directory `x/` must have `x/**` beside it, or a change to the split-off code skips the gate.
#[test]
fn a_filtered_module_is_filtered_with_its_directory() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for workflow in ["display.yml", "input.yml"] {
        let text = std::fs::read_to_string(root.join(".github/workflows").join(workflow)).unwrap();
        let filters: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("      - '")?.strip_suffix('\'')).collect();
        for module in filters.iter().filter_map(|f| f.strip_suffix(".rs")) {
            if root.join(module).is_dir() {
                let all = format!("{module}/**");
                assert!(filters.contains(&all.as_str()), "{workflow} runs on {module}.rs, not {all}");
            }
        }
    }
}
