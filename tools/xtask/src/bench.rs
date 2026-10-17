//! `bench-compose`: what composing a drag costs, and where.

use crate::*;

/// `cargo xtask bench-compose` — what composing a drag costs, and where.
///
/// A tool, not a gate: CI timing is noise. It records a *before* distribution (flicker is a tail
/// phenomenon). Run it under both accelerators: TCG measures instruction count, so the shadow
/// arm should be slower by about one copy (the control); KVM holds the real answer.
pub(crate) fn cmd_bench_compose(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Bench)?;
    let ovmf = locate_ovmf()?;

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        .arg("-display")
        .arg("none")
        .arg("-chardev")
        .arg("stdio,id=hostserial,signal=off")
        .arg("-serial")
        .arg("chardev:hostserial")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let how = if matches!(accel, Accel::Kvm) { "KVM" } else { "TCG" };
    println!("xtask: benchmarking compose under QEMU ({how})…\n");
    let mut session = Session::spawn(cmd, "bench-compose")?;
    // Slow on purpose: a timeout mid-distribution would report percentiles of a truncated tail.
    let done = session.expect_within("compose-bench: done", std::time::Duration::from_secs(300))?;
    let transcript = session.finish();
    let path = build_cache().join("guest-transcript-bench-compose.log");
    let _ = fs::write(&path, &transcript);
    if !done {
        return Err(format!("the bench did not finish; transcript at {}", path.display()).into());
    }
    report_bench(&transcript)
}

/// Turn the guest's per-sample lines into the distributions the question is about.
fn report_bench(transcript: &str) -> R<()> {
    let mut aperture: Vec<u64> = Vec::new();
    let mut anonymous: Vec<u64> = Vec::new();
    let mut inplace: Vec<u64> = Vec::new();
    let mut shadow: Vec<u64> = Vec::new();
    let mut exposed: Vec<u64> = Vec::new();
    let mut both: Vec<u64> = Vec::new();
    let mut area: Vec<u64> = Vec::new();

    for line in transcript.lines() {
        let Some(rest) = line.split_once("compose-bench: ").map(|(_, r)| r) else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        match f.as_slice() {
            ["e1", "aperture", n] => push_num(&mut aperture, n),
            ["e1", "anonymous", n] => push_num(&mut anonymous, n),
            ["e2", a, "inplace", w, "shadow", x, "exposed", y, "both", z] => {
                push_num(&mut area, a);
                push_num(&mut inplace, w);
                push_num(&mut shadow, x);
                push_num(&mut exposed, y);
                push_num(&mut both, z);
            }
            _ => {}
        }
    }
    if inplace.is_empty() {
        return Err("no `compose-bench: e2` samples in the transcript".into());
    }

    println!("\n--- E1: is the aperture behaving like RAM? ---");
    let (ap, an) = (median(&mut aperture), median(&mut anonymous));
    println!("  one row, median ns:  aperture {ap}   anonymous {an}");
    match (ap, an) {
        (0, _) | (_, 0) => println!("  (no samples — E1 was skipped)"),
        _ => {
            let pct = (ap as f64 / an as f64) * 100.0;
            println!("  aperture is {pct:.0}% of anonymous");
            // Print the reading: a bare ratio invites the one already expected.
            if pct < 150.0 {
                println!(
                    "  → the aperture is behaving as cached RAM. \"off MMIO into cached RAM\"\n\
                     \x20   is not a mechanism on this system; any win must come from elsewhere."
                );
            } else {
                println!(
                    "  → the aperture is materially slower than RAM, so the plan's stated\n\
                     \x20   mechanism is live after all. Worth re-reading the page-table path."
                );
            }
        }
    }

    println!("\n--- E2: in place vs shadow, one-pixel window drag ---");
    println!("  {} frames per arm, median damage {} px", inplace.len(), median(&mut area));
    let base = percentile(&mut inplace.clone(), 50);
    for (name, v) in [
        ("in place        ", &mut inplace),
        ("shadow          ", &mut shadow),
        ("exposed         ", &mut exposed),
        ("exposed + shadow", &mut both),
    ] {
        if v.is_empty() {
            continue;
        }
        let (p50, p90, p99) = (percentile(v, 50), percentile(v, 90), percentile(v, 99));
        let rel = if base > 0 { pct_of(p50, base) } else { String::from("    -") };
        println!("  {name}  p50 {p50:>9}  p90 {p90:>9}  p99 {p99:>9} ns   {rel} of in place");
    }
    println!(
        "\n  **The flicker fix is an ordering property** — no background-only frame is ever on\n\
         \x20 screen — and no number here can show it. What these decide is what it costs, and\n\
         \x20 whether skipping the background fill pays for it."
    );
    Ok(())
}

/// `v` as a percentage of `base`, right-aligned for a column.
fn pct_of(v: u64, base: u64) -> String {
    format!("{:>4.0}%", (v as f64 / base as f64) * 100.0)
}

/// Parse one decimal sample, ignoring a line that does not carry one.
fn push_num(into: &mut Vec<u64>, s: &str) {
    if let Ok(n) = s.parse::<u64>() {
        into.push(n);
    }
}

/// The median, or 0 for no samples.
fn median(v: &mut [u64]) -> u64 {
    percentile(v, 50)
}

/// The `p`th percentile by nearest rank (an interpolation is a number no frame took), or 0.
fn percentile(v: &mut [u64], p: usize) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let idx = (v.len() * p).div_ceil(100).saturating_sub(1).min(v.len() - 1);
    v[idx]
}
