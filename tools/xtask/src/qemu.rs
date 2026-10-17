//! QEMU's command line: TCG or KVM, the machine every boot shares, OVMF, and `cargo xtask qemu`.

use crate::*;

/// How the guest CPU is executed.
///
/// The kernel is **x2APIC-only**, and QEMU's TCG emulates x2APIC only from **9.0**; KVM has no such
/// limit, so `--kvm` is the fast path and the way to run on an older QEMU (CI's is 8.2).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Accel {
    /// Pure emulation. Requires QEMU ≥ 9.0 for x2APIC.
    Tcg,
    /// Hardware virtualisation (`-enable-kvm -cpu host`). Needs `/dev/kvm`.
    Kvm,
}

fn qemu_version() -> Option<(u32, u32)> {
    let out = Command::new("qemu-system-x86_64").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ver = text.split_whitespace().find(|w| w.contains('.') && w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = ver.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor))
}

/// Fail *before* launching if the host cannot give the guest what the kernel requires: QEMU 8.2
/// otherwise boots to `CPU lacks x2APIC`, which reads like a kernel bug.
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

/// QEMU's machine, CPU, memory and firmware, and the screen: `Some(size)` for a mode of that size,
/// `None` for QEMU's default (`test-qemu` and `test-interactive`, so CI boots two sizes).
pub(crate) fn qemu_base_args(qemu: &mut Command, ovmf: &Firmware, accel: Accel, screen: Option<DisplaySize>) -> R<()> {
    if let Some(size) = screen {
        qemu.arg("-vga").arg("none").arg("-device").arg(format!("VGA,xres={},yres={}", size.w, size.h));
    }
    qemu.arg("-M")
        .arg("q35")
        // CPU model `max`: a superset of what the kernel requires, with a properly-emulated
        // XSAVE/AVX state. Splicing `+xsave,+avx` onto `qemu64` *hangs* TCG at the `CR4.OSXSAVE`
        // enable. KVM (`-cpu host`) proves the real hardware path.
        .arg("-m")
        .arg("256M");
    match accel {
        Accel::Tcg => {
            qemu.arg("-cpu").arg("max");
        }
        Accel::Kvm => {
            qemu.arg("-enable-kvm").arg("-cpu").arg("host");
        }
    }
    for a in firmware_pflash_args(ovmf)? {
        qemu.arg(a);
    }
    Ok(())
}

/// `cargo xtask qemu`: boot the image in a window, for a person rather than a gate. **`--grab`**:
/// the guest has a relative pointing device only, so the host and guest cursors are offset for good
/// and pushing into a corner cannot fix it; the host desktop also keeps `Super`. A grab confines
/// the pointer and sends the keyboard to the guest (QEMU's own binding is `Ctrl-Alt-G`). **On
/// Wayland it asks GTK for an X11 backend** (`GDK_BACKEND=x11`), as QEMU's GTK window cannot grab
/// there.
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
        // Only with XWayland there: an X11 backend with no `DISPLAY` opens no window at all.
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

/// UEFI firmware for the QEMU pflash: modern QEMU ships **split** CODE and VARS images (a CODE-only
/// pflash will not boot), older setups one combined image.
pub(crate) enum Firmware {
    Single(PathBuf),
    Split { code: PathBuf, vars_template: PathBuf },
}

/// Locate UEFI firmware, preferring the split layout. `NITROX_OVMF` overrides the CODE/combined
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
    // Newest layouts first, so a machine with both prefers a split pair and its writable VARS.
    let split = [
        ("/usr/local/share/qemu/edk2-x86_64-code.fd", "/usr/local/share/qemu/edk2-i386-vars.fd"),
        ("/usr/share/qemu/edk2-x86_64-code.fd", "/usr/share/qemu/edk2-i386-vars.fd"),
        ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
        ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
        ("/usr/share/edk2-ovmf/x64/OVMF_CODE.fd", "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"),
    ];
    for (code, vars) in split {
        if Path::new(code).exists() && Path::new(vars).exists() {
            return Ok(Firmware::Split { code: PathBuf::from(code), vars_template: PathBuf::from(vars) });
        }
    }
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

/// The `-drive if=pflash,…` argument(s) for `firmware`. UEFI needs a writable NVRAM region, so
/// split firmware's VARS template is copied to a fresh per-run store: `unit=0` CODE (ro), `unit=1`
/// VARS (rw).
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
