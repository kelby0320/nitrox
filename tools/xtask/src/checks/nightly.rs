//! `check-nightly`: no crate uses a nightly `#![feature]`.

use crate::*;

/// Enforce the **narrowed** stable-Rust rule: userspace pins a nightly toolchain (it must, to
/// `-Z build-std` a custom hard-float target), but no crate anywhere may use a nightly
/// *language or library* feature. Without this the pin would quietly turn into a licence to
/// reach for `#![feature(...)]`.
pub(crate) fn cmd_check_nightly() -> R<()> {
    let mut violations: Vec<String> = Vec::new();
    // The two workspaces that ship *in* Nitrox. `tools/` is host tooling, outside the stable-Rust
    // rule. `target/` is pruned because build-std rebuilds `core` from a vendored checkout that
    // legitimately uses nightly features.
    for ws in ["kernel/src", "userspace"] {
        let src_root = repo_root().join(ws);
        visit_rs_files_skipping(&src_root, &["target"], &mut |path| {
            let text = fs::read_to_string(path)?;
            for (i, line) in text.lines().enumerate() {
                // Only real code counts; prose may legitimately discuss the attribute.
                let code = line.split("//").next().unwrap_or("");
                if code.contains("#![feature(") {
                    violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                }
            }
            Ok(())
        })?;
    }

    if violations.is_empty() {
        println!("check-nightly: no `#![feature(...)]` — nightly is used only for build-std ✓");
        Ok(())
    } else {
        let mut msg = String::from(
            "nightly language/library features are not permitted — the nightly toolchain \
             exists solely to `-Z build-std` the custom userspace target:\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}
