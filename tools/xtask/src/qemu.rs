//! QEMU's command line: TCG or KVM, the machine every boot shares, OVMF, and `cargo xtask qemu`.

use crate::*;

/// Append the machine / CPU / memory / UEFI-firmware flags shared by every QEMU
/// launch (`qemu`, `qemu-debug`, `test-qemu`) to `qemu`.
/// How the guest CPU is executed.
///
/// The kernel is **x2APIC-only** (decision log, 2026-06-26), which sets a hard floor on
/// the host side: QEMU's TCG only emulates x2APIC from **9.0**, so an older QEMU boots to
/// a kernel panic ("CPU lacks x2APIC"). KVM has no such limit — the in-kernel APIC has
/// supported x2APIC for years — so `--kvm` is both the fast path and the way to run on a
/// host whose QEMU is too old to emulate it.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Accel {
    /// Pure emulation. Requires QEMU ≥ 9.0 for x2APIC.
    Tcg,
    /// Hardware virtualisation (`-enable-kvm -cpu host`). Needs `/dev/kvm`.
    Kvm,
}

/// The host QEMU's `(major, minor)` version, or `None` if it could not be parsed.
fn qemu_version() -> Option<(u32, u32)> {
    let out = Command::new("qemu-system-x86_64").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "QEMU emulator version 9.2.0 (…)" — take the first dotted number.
    let ver = text.split_whitespace().find(|w| w.contains('.') && w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = ver.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor))
}

/// Fail *before* launching if the host cannot give the guest what the kernel requires, so
/// the operator sees an actionable message instead of a kernel panic from inside QEMU.
///
/// This check exists because the failure it replaces was genuinely confusing: a CI runner
/// with QEMU 8.2 booted to `*** KERNEL PANIC *** CPU lacks x2APIC`, which reads like a
/// kernel bug and is actually a host-tooling floor.
pub(crate) fn preflight_accel(accel: Accel) -> R<()> {
    match accel {
        Accel::Kvm => {
            if !Path::new("/dev/kvm").exists() {
                return Err("`--kvm` requested but /dev/kvm is absent — no hardware \
                     virtualisation available (nested virt off, or the kvm module is not \
                     loaded). Drop `--kvm` to use TCG, which needs QEMU >= 9.0."
                    .into());
            }
        }
        Accel::Tcg => {
            if let Some((major, minor)) = qemu_version()
                && major < 9
            {
                return Err(format!(
                    "QEMU {major}.{minor} is too old: TCG only emulates x2APIC from 9.0, \
                         and this kernel is x2APIC-only (decision log 2026-06-26), so the \
                         guest would panic with \"CPU lacks x2APIC\". Use `--kvm` (needs \
                         /dev/kvm) or install QEMU >= 9.0."
                )
                .into());
            }
        }
    }
    Ok(())
}

/// QEMU's machine, CPU, memory and firmware — and the screen, which every caller states:
/// `Some(size)` for a mode of that size, `None` for QEMU's own default (`test-qemu` and
/// `test-interactive`, so every CI run boots two sizes).
pub(crate) fn qemu_base_args(qemu: &mut Command, ovmf: &Firmware, accel: Accel, screen: Option<DisplaySize>) -> R<()> {
    if let Some(size) = screen {
        qemu.arg("-vga").arg("none").arg("-device").arg(format!("VGA,xres={},yres={}", size.w, size.h));
    }
    qemu.arg("-M")
        .arg("q35")
        // CPU model = QEMU's `max`: every feature the emulator can provide,
        // which is a strict superset of what the kernel requires. The kernel
        // enables only what it opts into (SMAP/SMEP in `init_protections`;
        // x87+SSE+AVX in `fpu_init_cpu`'s `XCR0`, never AVX-512 or LA57), so a
        // richer model changes nothing it doesn't ask for. `max` supplies, in
        // one word, everything the previous hand-rolled `qemu64,+…` string spelt
        // out — SMEP/SMAP (user-access protections), the on-chip + x2APIC local
        // controller, RDRAND/RDSEED (hardware CSPRNG seed), RDTSCP (`current_cpu`
        // reads `IA32_TSC_AUX`) — **plus** a properly-emulated XSAVE/AVX extended
        // state. That last point is why we moved off `qemu64`: splicing
        // `+xsave,+avx` onto the ancient `qemu64` model *hangs* TCG at the
        // `CR4.OSXSAVE` enable (a QEMU emulation fragility), whereas `max`
        // emulates the whole XSAVE path and boots clean. The real hardware path
        // is additionally proven under KVM (`-cpu host`); see the decision log
        // (2026-07-21 floating-point). x2APIC needs QEMU ≥ 9.0. SMP runs `-smp N`.
        .arg("-m")
        .arg("256M");
    // `max` (TCG) is every feature the emulator can provide; `host` (KVM) is every
    // feature the physical CPU has. Both are strict supersets of what the kernel asks
    // for, and both carry x2APIC — under TCG only from QEMU 9.0, which `preflight_accel`
    // checks before we get here.
    match accel {
        Accel::Tcg => {
            qemu.arg("-cpu").arg("max");
        }
        Accel::Kvm => {
            qemu.arg("-enable-kvm").arg("-cpu").arg("host");
        }
    }
    // UEFI firmware pflash drive(s) — split CODE+VARS on modern QEMU, or a
    // single combined image on legacy setups (see `locate_ovmf`).
    for a in firmware_pflash_args(ovmf)? {
        qemu.arg(a);
    }
    Ok(())
}

/// `cargo xtask qemu` — boot the image in a window, for a person rather than a gate.
///
/// ## `--grab`, and why an interactive session needs it
///
/// **The guest has a relative pointing device and no absolute one.** A PS/2 mouse reports
/// *movement*, not position, and there is no USB or virtio input driver here for a tablet
/// device to talk to — so nothing ever tells the guest where the host's pointer is. The two
/// cursors are independent: the compositor's starts at the centre of the screen, the host's
/// starts whereever it was, and the offset between them is permanent. Worse, it cannot be
/// corrected by pushing into a corner, because the host pointer leaves the window — and stops
/// generating motion — before the guest's cursor reaches the edge. A person sees "I cannot
/// reach the left side of the screen", and a scaled window makes it arbitrarily worse.
///
/// **And the host desktop keeps `Super` for itself.** Every chord this system binds is
/// `Super`-something (`Super+A`, `Super+H`, `Super+1`, `Super+Shift+1`, `Super+R`), and GNOME,
/// KDE and COSMIC all bind `Super` at the compositor. Ungrabbed, those keystrokes are the *host's*,
/// and the guest is never told they happened.
///
/// A grab fixes both: the pointer is confined to the window, so movement keeps arriving and
/// any edge is reachable, and the keyboard goes to the guest including its modifiers. QEMU's
/// own binding is `Ctrl-Alt-G`; this flag asks for the grab up front instead.
///
/// **On Wayland it also asks GTK for an X11 backend.** A grab is an X server operation, and
/// under a Wayland session QEMU's GTK window cannot take one — it can only ask the compositor
/// to inhibit shortcuts, which the compositor may decline. Running the window through XWayland
/// (`GDK_BACKEND=x11`) gives it the real thing. Nothing else about the guest changes.
pub(crate) fn cmd_qemu(
    debug: bool,
    mode: BuildMode,
    accel: Accel,
    grab: bool,
    size: Option<DisplaySize>,
    extra_args: &[String],
) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(mode)?;
    let ovmf = locate_ovmf()?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut qemu, &ovmf, accel, size)?;
    qemu.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-serial")
        .arg("stdio")
        .arg("-no-reboot")
        .arg("-no-shutdown");
    if grab {
        qemu.arg("-display").arg("gtk,grab-on-hover=on");
        // Only on a Wayland session, and only if XWayland is actually there to talk to: forcing
        // an X11 backend with no `DISPLAY` would fail to open a window at all.
        let wayland = env::var("XDG_SESSION_TYPE").map(|s| s == "wayland").unwrap_or(false);
        if wayland && env::var("DISPLAY").is_ok() {
            qemu.env("GDK_BACKEND", "x11");
            println!(
                "xtask: Wayland session — running the QEMU window through XWayland so it can \
                 take a real grab"
            );
        }
        println!(
            "xtask: input grabbed on hover — the pointer is confined to the window and `Super` \
             reaches the guest. Ctrl-Alt-G releases it."
        );
    } else {
        println!(
            "xtask: the pointer is NOT grabbed. The guest has a relative mouse and no absolute \
             one, so its cursor and yours are two independent cursors — press Ctrl-Alt-G in the \
             window (or start with `--grab`) before expecting them to line up, or before \
             pressing a `Super` chord, which your desktop otherwise keeps."
        );
    }
    if debug {
        qemu.arg("-S").arg("-s");
        println!("xtask: QEMU paused on entry; attach gdb to localhost:1234");
    }
    for a in extra_args {
        qemu.arg(a);
    }
    run(&mut qemu)
}

/// UEFI firmware for the QEMU pflash. Modern QEMU ships **split** firmware — a
/// read-only CODE image plus a writable VARS (NVRAM) store — and a CODE-only
/// pflash will not boot (the firmware needs its variable region). Older setups
/// shipped a single combined image used as one read-only pflash.
pub(crate) enum Firmware {
    /// Legacy single combined image (e.g. `OVMF.fd`): one read-only pflash.
    Single(PathBuf),
    /// Split firmware: a read-only CODE image plus a VARS *template* that is
    /// copied to a writable per-run store before boot.
    Split { code: PathBuf, vars_template: PathBuf },
}

/// Locate UEFI firmware, preferring the modern split (CODE+VARS) layout that
/// QEMU bundles under its data dir. `NITROX_OVMF` overrides the CODE/combined
/// image; pair it with `NITROX_OVMF_VARS` to force the split layout.
pub(crate) fn locate_ovmf() -> R<Firmware> {
    if let Ok(code) = env::var("NITROX_OVMF") {
        let code = PathBuf::from(code);
        if code.exists() {
            if let Ok(vars) = env::var("NITROX_OVMF_VARS") {
                let vars = PathBuf::from(vars);
                if vars.exists() {
                    return Ok(Firmware::Split { code, vars_template: vars });
                }
            }
            return Ok(Firmware::Single(code));
        }
    }
    // Split CODE+VARS pairs. QEMU's x86_64 CODE pairs with the (historically
    // i386-named) VARS template; the `/usr/local` paths are a from-source/tarball
    // QEMU install's bundled edk2 firmware.
    let split = [
        ("/usr/local/share/qemu/edk2-x86_64-code.fd", "/usr/local/share/qemu/edk2-i386-vars.fd"),
        ("/usr/share/qemu/edk2-x86_64-code.fd", "/usr/share/qemu/edk2-i386-vars.fd"),
        // Debian/Ubuntu's `ovmf` package has shipped the 4 MB build under these names
        // since 22.04, and on current releases they are the *only* ones present — the
        // unsuffixed pair below is older layouts (and the `/usr/share/ovmf/OVMF.fd`
        // single image at the end is older still). Ordered newest-first so a machine
        // with both prefers the split pair, which gives a writable VARS store.
        ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
        ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
        ("/usr/share/edk2-ovmf/x64/OVMF_CODE.fd", "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"),
    ];
    for (code, vars) in split {
        if Path::new(code).exists() && Path::new(vars).exists() {
            return Ok(Firmware::Split { code: PathBuf::from(code), vars_template: PathBuf::from(vars) });
        }
    }
    // Legacy single combined image.
    let single = ["/usr/share/ovmf/OVMF.fd", "/usr/share/qemu/OVMF.fd", "/usr/share/edk2-ovmf/x64/OVMF.fd"];
    for c in single {
        if Path::new(c).exists() {
            return Ok(Firmware::Single(PathBuf::from(c)));
        }
    }
    Err("could not locate UEFI firmware; set NITROX_OVMF=/path/to/CODE.fd \
         (and NITROX_OVMF_VARS=/path/to/VARS.fd for split firmware)"
        .into())
}

/// The `-drive if=pflash,…` argument(s) for `firmware`. For split firmware the
/// read-only VARS template is copied to a fresh writable per-run store under
/// build-cache (UEFI needs a writable NVRAM region; the shared template is
/// read-only), emitted as `unit=0` CODE (ro) + `unit=1` VARS (rw).
fn firmware_pflash_args(firmware: &Firmware) -> R<Vec<String>> {
    match firmware {
        Firmware::Single(code) => {
            Ok(vec!["-drive".into(), format!("if=pflash,format=raw,readonly=on,file={}", code.display())])
        }
        Firmware::Split { code, vars_template } => {
            let vars = build_cache().join("ovmf-vars.fd");
            fs::copy(vars_template, &vars)
                .map_err(|e| format!("copy OVMF vars {} -> {}: {e}", vars_template.display(), vars.display()))?;
            Ok(vec![
                "-drive".into(),
                format!("if=pflash,unit=0,format=raw,readonly=on,file={}", code.display()),
                "-drive".into(),
                format!("if=pflash,unit=1,format=raw,file={}", vars.display()),
            ])
        }
    }
}
