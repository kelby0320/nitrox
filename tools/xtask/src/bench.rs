use crate::*;

/// `cargo xtask bench-compose` — what composing a drag costs, and where.
///
/// **Milestone 13 Part A opens with a measurement, and this runs it.** The plan's claim is that
/// composing into RAM and copying the finished damage rectangle to the aperture is *also faster*
/// than composing straight into the aperture, and records it as plausible and unproven. It is
/// probably wrong as stated: this system maps the aperture **write-back cached**
/// (`protection_to_page_flags` never sets `NO_CACHE`), so "the per-pixel work moving off MMIO
/// into cached RAM" describes nothing here. E1 checks that by measurement rather than by reading
/// page tables.
///
/// **A tool, not a gate**, like `shot` and `preview`. Timing under CI is noise and this tree has
/// no precedent for a performance gate; what this produces is a *before* number to compare a
/// later change against, and a distribution rather than a mean, because flicker is a tail
/// phenomenon.
///
/// **Run it under both accelerators.** TCG models neither caches nor QEMU's dirty-tracking, so it
/// measures instruction count — the shadow arm should be strictly slower there by about one copy,
/// which is the *control*. KVM is where the real answer is, and if the two agree this is not
/// measuring what it claims to.
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
    // **A generous deadline, because this is deliberately slow work.** Four hundred composed
    // frames under TCG is tens of seconds, and a run that timed out mid-distribution would report
    // a percentile over a truncated tail — which reads as a result rather than as a failure.
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
            // **The interpretation is printed, because a bare ratio invites the reading the
            // person already expected.** The plan's rationale needs the aperture to be *slower*;
            // parity means it is ordinary cached memory and that rationale describes nothing.
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

/// The `p`th percentile by nearest rank, or 0 for no samples.
///
/// **Nearest rank rather than interpolation**, because these are timings and an interpolated
/// value is a number no frame took.
fn percentile(v: &mut [u64], p: usize) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let idx = (v.len() * p).div_ceil(100).saturating_sub(1).min(v.len() - 1);
    v[idx]
}
