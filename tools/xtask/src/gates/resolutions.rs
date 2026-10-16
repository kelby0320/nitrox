use crate::*;

/// `cargo xtask check-resolutions` — **resolution independence, checked on demand** (Phase 5
/// Part E.4). Not in CI, by the maintainer's call: the system should not care what size its screen
/// is, CI boots two sizes on every run, and this is how the claim gets confirmed across more of them
/// from time to time without every PR paying for twenty boots.
///
/// Runs `check-display`, `check-terminal`, `check-login` and `check-fbcon` at each size — every
/// width a multiple of 8, for the reason `DisplaySize` gives — **one boot at a time**, each as its
/// own `cargo xtask <gate> --size WxH` child so a failure is exactly what that gate reports. Keeps
/// every run's output and transcript under `build-cache/resolutions/`, and prints a table.
///
/// **What it is allowed to find** besides a gate failure: whether a boot's demo chain finished —
/// the check `test-qemu` makes, which at 1024×768 failed its dead-log-source step in 2 runs of 3
/// on 2026-09-15 with nothing yet connecting size to it — and whether the compositor ran without
/// its shadow buffer, which is what a screen too large for the guest's memory looks like first.
pub(crate) fn cmd_check_resolutions(accel: Accel) -> R<()> {
    const SIZES: &[&str] = &["1024x768", "1280x800", "1360x768", "1920x1080", "2560x1440"];
    const GATES: &[&str] = &["check-display", "check-terminal", "check-login", "check-fbcon"];
    let dir = build_cache().join("resolutions");
    fs::create_dir_all(&dir)?;
    let exe = env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    println!(
        "xtask: {} gates at {} sizes, one boot at a time, under {} — output in {}\n",
        GATES.len(),
        SIZES.len(),
        if matches!(accel, Accel::Kvm) { "KVM" } else { "TCG" },
        dir.display()
    );
    let mut failed = Vec::new();
    for size in SIZES {
        for gate in GATES {
            let stem = format!("{gate}-{size}");
            let out_path = dir.join(format!("{stem}.log"));
            let transcript_path = dir.join(format!("{stem}.transcript.log"));
            let _ = fs::remove_file(&transcript_path);
            let out = fs::File::create(&out_path)?;
            let mut cmd = Command::new(&exe);
            cmd.arg(gate).arg("--size").arg(size);
            if matches!(accel, Accel::Kvm) {
                cmd.arg("--kvm");
            }
            cmd.env("NITROX_KEEP_TRANSCRIPT", &transcript_path).stdout(out.try_clone()?).stderr(out);
            let started = std::time::Instant::now();
            let passed = cmd.status().map_err(|e| format!("run {gate}: {e}"))?.success();
            let transcript = fs::read_to_string(&transcript_path).unwrap_or_default();
            let mut notes = Vec::new();
            if transcript.contains("test-harness:") {
                if transcript.contains("test-harness: all smoke tests passed") {
                    notes.push("demo chain finished".to_string());
                } else {
                    // **Unfinished, which is not failed**: `check-display` ends its boot once it
                    // has the screen, and on a large one that can be before the chain is done. A
                    // failure is one of these lines followed by nothing, in a boot that ran long
                    // enough — `check-terminal`'s does — so the last line is what to read.
                    let last = transcript.lines().rfind(|l| l.starts_with("test-harness:"));
                    notes.push(format!(
                        "demo chain unfinished when the boot ended — last: {}",
                        last.unwrap_or("?").trim()
                    ));
                }
            }
            if transcript.contains("compositor: no shadow buffer") {
                notes.push("compositor ran without its shadow buffer".to_string());
            }
            println!(
                "  {size:>9}  {gate:<15} {}  {:>4}s  {}",
                if passed { "pass" } else { "FAIL" },
                started.elapsed().as_secs(),
                notes.join("; ")
            );
            if !passed {
                failed.push(stem);
            }
        }
    }
    if failed.is_empty() {
        println!("\nxtask: every gate passed at every size ✓");
        Ok(())
    } else {
        Err(format!("{} run(s) failed: {} — see {}", failed.len(), failed.join(", "), dir.display()).into())
    }
}
