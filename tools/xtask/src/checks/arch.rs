use crate::*;

pub(crate) fn cmd_check_arch() -> R<()> {
    let kernel_src = repo_root().join("kernel").join("src");
    let arch_dir = kernel_src.join("arch");
    let mut violations: Vec<String> = Vec::new();

    visit_rs_files(&kernel_src, &mut |path| {
        // The arch implementation legitimately names its own internals.
        if path.starts_with(&arch_dir) {
            return Ok(());
        }
        let text = fs::read_to_string(path)?;
        for (i, line) in text.lines().enumerate() {
            // Ignore comment/doc text — only real code is a boundary break.
            let code = line.split("//").next().unwrap_or("");
            if code.contains("arch::x86_64") || code.contains("arch::aarch64") {
                violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            }
        }
        Ok(())
    })?;

    if violations.is_empty() {
        println!("check-arch: no arch-internal references outside kernel/src/arch ✓");
        Ok(())
    } else {
        let mut msg = String::from(
            "arch boundary violated — use the neutral `crate::arch` interface, \
             not arch-internal modules:\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}
