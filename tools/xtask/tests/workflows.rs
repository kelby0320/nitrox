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

/// A filtered file's split-off modules are filtered with it, whether the split made an `x/` beside
/// `x.rs` or siblings beside a crate root: a module the file declares out of line, not for tests,
/// whose own file opens with `use super::*;` is code that was the file's.
#[test]
fn a_filtered_files_split_off_modules_are_filtered() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for workflow in ["display.yml", "input.yml"] {
        let text = std::fs::read_to_string(root.join(".github/workflows").join(workflow)).unwrap();
        let filters: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("      - '")?.strip_suffix('\'')).collect();
        let covered = |path: &str| {
            filters.iter().any(|f| f.strip_suffix("/**").map_or(*f == path, |d| path.starts_with(&format!("{d}/"))))
        };
        for file in filters.iter().filter(|f| f.ends_with(".rs") && !f.contains('*')) {
            let source = std::fs::read_to_string(root.join(file)).unwrap();
            let lines: Vec<&str> = source.lines().collect();
            let dir = match Path::new(file).file_name().and_then(|n| n.to_str()) {
                Some("main.rs" | "lib.rs" | "mod.rs") => Path::new(file).parent().unwrap().to_path_buf(),
                _ => Path::new(file).with_extension(""),
            };
            for (i, line) in lines.iter().enumerate() {
                let Some(name) = line.strip_prefix("mod ").and_then(|l| l.strip_suffix(';')) else { continue };
                if i > 0 && lines[i - 1] == "#[cfg(test)]" {
                    continue;
                }
                let piece = [dir.join(format!("{name}.rs")), dir.join(name).join("mod.rs")]
                    .into_iter()
                    .find(|p| root.join(p).is_file());
                let Some(piece) = piece else { continue };
                let opens = std::fs::read_to_string(root.join(&piece)).unwrap();
                let first = opens.lines().find(|l| !l.is_empty() && !l.starts_with("//!"));
                let path = piece.to_str().unwrap();
                if first == Some("use super::*;") {
                    assert!(covered(path), "{workflow} runs on {file}, not on {path}, split off from it");
                }
            }
        }
    }
}

/// CI's QEMU job ties each gate to a runner by naming its group, and a name not in the matrix
/// runs the gate on no runner while every runner still passes. So each `cargo xtask` step of the
/// job names one of the matrix's groups, every group named anywhere is one of them, and each
/// group runs a gate.
#[test]
fn every_gate_of_the_qemu_job_runs_in_a_group_that_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    let mut job_lines = text.lines().skip_while(|l| *l != "  integration:").skip(1);
    let job: Vec<&str> = job_lines.by_ref().take_while(|l| !(l.starts_with("  ") && !l.starts_with("   "))).collect();
    let job = job.join("\n");
    let groups: Vec<&str> = job
        .lines()
        .find_map(|l| l.trim().strip_prefix("group: [")?.strip_suffix(']'))
        .expect("the matrix's groups")
        .split(", ")
        .collect();
    let named: Vec<&str> =
        job.split("matrix.group == '").skip(1).filter_map(|r| r.split_once('\'')).map(|(g, _)| g).collect();
    for g in &named {
        assert!(groups.contains(g), "`{g}` is named in the QEMU job and is not one of {groups:?}");
    }
    let lines: Vec<&str> = job.lines().collect();
    let mut run = Vec::new();
    for (i, line) in lines.iter().enumerate().filter(|(_, l)| l.trim().starts_with("run: cargo xtask")) {
        let guard = lines[i - 1].trim().strip_prefix("if: matrix.group == '").and_then(|g| g.split_once('\''));
        let (group, _) =
            guard.unwrap_or_else(|| panic!("`{}` names no group, so it runs on every runner", line.trim()));
        run.push(group);
    }
    for g in &groups {
        assert!(run.contains(g), "group `{g}` runs no gate");
    }
}
