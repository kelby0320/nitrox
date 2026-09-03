//! Nitrox build orchestrator.
//!
//! Subcommands:
//!   build           build the kernel ELF
//!   image           build kernel + assemble a UEFI-bootable GPT/FAT32 image
//!   qemu            build + launch QEMU with OVMF
//!   qemu-debug      build + launch QEMU paused for GDB on :1234
//!   test            host-side unit tests (kernel lib + tools workspace)
//!   test-qemu       boot a headless self-test image; adjudicate via isa-debug-exit
//!   check-deferrals fail if a `TODO(<tag>)` has no deferred-decisions.md entry
//!   check-docs      fail if a doc links to, or cites, a path that does not exist
//!   check-images    fail if a test image and a release image differ by anything new
//!   preview         render the toolkit on the host to a PNG; no boot
//!   check-display   boot + screendump; compare the screen against a libdraw render
//!   check-terminal  boot + type into the GUI terminal; assert the shell answered
//!   check-login     boot the release image + drive the graphical greeter to a session
//!   check-irq-scope fail if an interrupt entry stub skips the lock-ordering scope
//!   abi-sync-check  fail if userspace/libkern has drifted from the kernel ABI
//!   fetch-limine    download the pinned limine-binary tarball into the cache
//!   clean           remove all build outputs and caches
//!
//! **Host tooling, and the kernel's rules do not reach it.** No "stable Rust only" and no
//! "no external crates": both are about what runs on the target. What applies here is written
//! down in `tools/CLAUDE.md` — the bar a host dependency clears, and why the checked-in lockfile
//! is what pins it. This file said "avoids external crates" until M11 Part A took `png`, which
//! is the sort of stale rule a reader hits before they find the reasoning (PR #261 review,
//! finding 1).

use std::env;
use std::error::Error;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Limine version we build against. Bump this together with any changes
/// to `kernel/src/limine.rs`.
const LIMINE_VERSION: &str = "v12.2.0";

/// Total boot-disk size, in MiB. Holds two GPT partitions: the EFI System Partition
/// (FAT32, [`ESP_SIZE_MIB`]) and the ext4 `nitrox-root` filesystem (the rest).
const IMAGE_SIZE_MIB: u64 = 128;
/// The EFI System Partition size. Comfortably above the FAT32 minimum so
/// `mformat -F` (forced FAT32) is always valid; the rest of the disk is the ext4
/// `nitrox-root` partition.
const ESP_SIZE_MIB: u64 = 48;

// Test-only fixture credential for the auth + session-mgr login-path demo. **Not a
// secret**: the shipped image stores only the one-way PBKDF2 verifier of
// `DEMO_PASSWORD`; the password is a build input for the emulator demo user. init's
// login selftest (auth Part E) must use these same literals. A fixed salt keeps the
// image build reproducible (a single demo user makes salt-uniqueness moot). See
// `docs/architecture/session-and-auth.md`.
const DEMO_USER: &str = "alice";
const DEMO_PASSWORD: &str = "correct horse battery staple";
const DEMO_HOME: &str = "/home/alice";

/// The `font_px` the staged `theme.toml` carries — **deliberately not the built-in 16**.
///
/// A gate asserting the default proves nothing: a client that never received the theme reports
/// the same number, so the assertion passes with the wire cut. This one can only have come from
/// the file, through the shell, onto the setup record and into a window (PR #263 review,
/// blocking 2). It is inside what the fixed chrome holds, which is what `MAX_FONT_PX` bounds.
const THEME_FONT_PX: u8 = 14;

/// Where the staged wallpaper goes, **as the session sees it**.
///
/// `libsession` binds the user's home at `/home`, so a file staged at `/home/alice/wallpaper.png`
/// is this path inside the session — the same relationship `THEME_PATH` has to `theme.toml`.
const WALLPAPER_PATH: &str = "/home/wallpaper.png";

/// The size the staged wallpaper is cropped to, and **deliberately not the screen's**.
///
/// The same argument [`THEME_FONT_PX`] rests on: a picture the size of the screen would let a
/// shell that never decoded anything report the right numbers, so the gate would pass with the
/// decoder gone. 1920x1200 can only have come from an `IHDR` that was actually read.
///
/// **What it no longer distinguishes is fit from fill**, and that is worth stating rather than
/// leaving for somebody to notice: 16:10 into a 16:10 screen draws at `1280x800 at 0,0` under
/// either rule, so the *drawn* half of the shell's line stopped carrying weight when the shipped
/// picture became the screen's shape. The decoded half still does, and what pins fit-versus-fill
/// is `libdraw::scale`'s own control — the one that stretches to fill and takes six tests down
/// with it. A gate cannot have both here: a picture whose drawn size discriminates is a picture
/// with bars down the side, which is not what the desktop should look like.
const WALLPAPER_W: u32 = 1920;
/// See [`WALLPAPER_W`]. 16:10, which is the screen's shape.
const WALLPAPER_H: u32 = 1200;

/// The photograph the wallpaper is cropped from — see `assets/wallpapers/README.md`.
const WALLPAPER_ASSET: &str = "assets/wallpapers/scuba-divers.png";
const DEMO_SALT: [u8; 8] = [0x9e, 0x3f, 0xa2, 0x5c, 0x71, 0x0b, 0xd4, 0x86];

type R<T> = Result<T, Box<dyn Error>>;

/// What to compile into the kernel + `init`. Selects the cargo feature the two
/// crates are built with; the other userspace binaries are always feature-less.
#[derive(Clone, Copy, PartialEq)]
enum BuildMode {
    /// Production boot: straight to userspace, no demos.
    Normal,
    /// `--selftest`: compile + run the boot self-tests / demos, then drop to eshell.
    Selftest,
    /// `test-qemu`: everything `Selftest` runs, plus the `isa-debug-exit` verdict path
    /// so the run self-adjudicates headlessly (`test-harness` feature).
    TestHarness,
    /// `check-input --no-ps2-irq`: [`TestHarness`](Self::TestHarness) plus the kernel's
    /// `no-ps2-irq`, which leaves the i8042's interrupt generation off so the tick-driven
    /// `ps2::poll` sweep is the only path from a keystroke to the guest.
    TestHarnessNoPs2Irq,
    /// `bench-compose`: [`TestHarness`](Self::TestHarness) with `compose-bench` declared
    /// **instead of** `boot-probe`.
    ///
    /// The substitution is the whole of the mode. A measurement needs the screen to itself —
    /// another client drawing mid-frame is noise attributed to whichever arm was running — and
    /// it needs to be the thing that ends the boot, because `boot-probe` fires the verdict and
    /// a bench declared after it would never run.
    Bench,
}

impl BuildMode {
    /// The cargo `--features` value for the **userspace** build of `init` (`None` = no flag).
    ///
    /// `init` is the only userspace crate that takes it. `session-mgr` did until the retrofit
    /// moved the boot verdict out of it (Part B); `nxterm` takes `test-harness` alone, which
    /// is a different value and is passed separately.
    fn features(self) -> Option<&'static str> {
        match self {
            BuildMode::Normal => None,
            BuildMode::Selftest => Some("selftest"),
            BuildMode::TestHarness | BuildMode::TestHarnessNoPs2Irq | BuildMode::Bench => {
                Some("test-harness")
            }
        }
    }

    /// Whether this is a harness build — the images that carry the guest-side gates.
    ///
    /// A predicate rather than a `matches!` at each site, because a new variant otherwise has
    /// to be remembered in every one of them, and the first time that happened it was missed:
    /// `TestHarnessNoPs2Irq` was added to [`features`](Self::features) and not to the arm that
    /// decides whether `nxterm` is built with its instrumentation, so `--no-ps2-irq` quietly
    /// shipped an image missing the terminal's harness lines. Harmless then — nothing that
    /// gate asserts comes from `nxterm` — but it made "the same image" false at the point the
    /// comments claimed it.
    fn is_test_harness(self) -> bool {
        matches!(
            self,
            BuildMode::TestHarness | BuildMode::TestHarnessNoPs2Irq | BuildMode::Bench
        )
    }

    /// The same, for the **kernel**, which has one feature userspace does not.
    ///
    /// Split rather than appended to [`features`](Self::features) because that value also
    /// reaches `init`, and `no-ps2-irq` is a statement about the i8042 that has no meaning in
    /// a userspace crate. Declaring it there as a no-op would make `--features` valid at the
    /// cost of putting a hardware setting in a crate that cannot act on it — the same trade
    /// `session-mgr` used to make with `selftest`, and the reason its features are gone.
    fn kernel_features(self) -> Option<&'static str> {
        match self {
            BuildMode::TestHarnessNoPs2Irq => Some("test-harness,no-ps2-irq"),
            other => other.features(),
        }
    }
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let cmd = args.next();
    let rest: Vec<String> = args.collect();

    // `--selftest` (anywhere in the args) compiles + runs the boot self-tests / demos
    // (kernel `boot_selftest` + init's demo chain); without it the build boots straight
    // to userspace. Strip it out before forwarding the rest to QEMU.
    let selftest = rest.iter().any(|a| a == "--selftest");
    // `--kvm` runs the guest under hardware virtualisation instead of TCG. Two reasons to
    // reach for it: speed (a boot loop runs in a fraction of the time), and hosts whose
    // QEMU predates 9.0 and therefore cannot emulate the x2APIC this kernel requires —
    // KVM has no such limit. Stripped before the rest is forwarded to QEMU.
    // `--no-ps2-irq` (check-input only) boots a kernel whose i8042 never asserts its IRQs, so
    // the tick-driven recovery sweep is the only path input can take. See `cmd_check_input`.
    let no_ps2_irq = rest.iter().any(|a| a == "--no-ps2-irq");
    // `--grab` (interactive `qemu` only) makes the QEMU window take the host's pointer and
    // keyboard. See `cmd_qemu`: without a grab the guest cursor and the host pointer are two
    // different cursors, and the host desktop keeps `Super` for itself.
    let grab = rest.iter().any(|a| a == "--grab");
    // **Rejected before dispatch, not in a match arm.** A flag that exists to make an
    // invisible path visible must not be silently ignored: someone reproducing a sweep bug
    // interactively would otherwise get a boot with the i8042's IRQs *on* and nothing said
    // about it. (`--selftest` is tolerated on commands that do not read it — a pre-existing
    // looseness that costs less, because it cannot make a boot quietly unlike the one asked
    // for.) Checked here because a guard arm placed after the per-command arms never runs.
    if grab && !matches!(cmd.as_deref(), Some("qemu") | Some("qemu-debug")) {
        eprintln!(
            "xtask: `--grab` is only meaningful for `qemu`/`qemu-debug` — it is about a person \
             using the window, and every other command drives the guest over QMP"
        );
        return ExitCode::FAILURE;
    }
    if no_ps2_irq && cmd.as_deref() != Some("check-input") {
        eprintln!(
            "xtask: `--no-ps2-irq` is only meaningful for `check-input` — it boots a kernel \
             whose i8042 never asserts its IRQs, so the tick-driven recovery sweep is the \
             only path input can take"
        );
        return ExitCode::FAILURE;
    }
    let accel = if rest.iter().any(|a| a == "--kvm") {
        Accel::Kvm
    } else {
        Accel::Tcg
    };
    let qargs: Vec<String> = rest
        .iter()
        .filter(|a| {
            *a != "--selftest" && *a != "--kvm" && *a != "--no-ps2-irq" && *a != "--grab"
        })
        .cloned()
        .collect();
    let mode = if selftest {
        BuildMode::Selftest
    } else {
        BuildMode::Normal
    };

    let result = match cmd.as_deref() {
        Some("build") => cmd_build(mode),
        Some("image") => cmd_image(mode),
        Some("qemu") => cmd_qemu(false, mode, accel, grab, &qargs),
        Some("qemu-debug") => cmd_qemu(true, mode, accel, grab, &qargs),
        Some("test") => cmd_test(),
        Some("test-qemu") => cmd_test_qemu(accel),
        Some("test-interactive") => cmd_test_interactive(accel),
        Some("check-arch") => cmd_check_arch(),
        Some("check-nightly") => cmd_check_nightly(),
        Some("check-deferrals") => cmd_check_deferrals(),
        Some("check-docs") => cmd_check_docs(),
        Some("check-images") => cmd_check_images(),
        // **The first *positional* argument, not the first argument.** `qargs` has the global
        // flags stripped, and anything else beginning with `-` is a flag this command does not
        // have — landing one in the name slot reports "no preview called `--offline`", which
        // names the wrong problem (PR #261 review, optional 3).
        Some("tune") => cmd_tune(&qargs),
        Some("preview") => cmd_preview(
            qargs.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("all"),
        ),
        // Same positional-argument rule as `preview`, and for the same reason: `--kvm` in the
        // name slot would be reported as "no shot called `--kvm`".
        Some("shot") => cmd_shot(
            qargs.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("all"),
            accel,
        ),
        Some("check-display") => cmd_check_display(accel),
        Some("check-terminal") => cmd_check_terminal(accel),
        Some("check-login") => cmd_check_login(accel),
        Some("bench-compose") => cmd_bench_compose(accel),
        Some("check-input") => cmd_check_input(accel, no_ps2_irq),
        Some("check-irq-scope") => cmd_check_irq_scope(),
        Some("abi-sync-check") => cmd_abi_sync_check(),
        Some("fetch-limine") => cmd_fetch_limine().map(|_| ()),
        Some("clean") => cmd_clean(),
        Some("help") | Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!("unknown subcommand: {other}").into()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "Nitrox build orchestrator.\n\
         \n\
         Usage: cargo xtask <command> [args]\n\
         \n\
         Commands:\n  \
           build         build the kernel ELF\n  \
           image         build + assemble a UEFI-bootable disk image\n  \
           qemu          build + launch QEMU with OVMF\n  \
           qemu-debug    build + launch QEMU paused for GDB on :1234\n  \
           test          host-side unit tests (kernel lib + tools)\n  \
           test-qemu     boot a headless self-test image; pass/fail via isa-debug-exit\n  \
           test-interactive  boot the release image and drive a real login + shell\n  \
           check-terminal    click into nxterm, type, and check the shell's answer renders\n  \
           check-login       type a wrong then a right password at the graphical greeter\n  \
           check-input       inject a key and a click; check both reach a userspace client\n  \
           \x20                `--no-ps2-irq` boots with the i8042's IRQs off, so the\n  \
           \x20                tick-driven recovery sweep is the only path input takes\n  \
           check-display     boot + screendump; compare the screen to a libdraw render\n  \
           bench-compose     what composing a drag costs, and where (M13 Part A)\n  \
           preview           render the toolkit here and write a PNG; `preview ui|term|all`\n  \
           shot              boot the release image and photograph the desktop;\n  \
           \x20                `shot all|greeter|desktop|apps|windows|overview`\n  \
           check-arch    fail if kernel code outside arch/ uses arch internals\n  \
           check-nightly fail if any crate uses a nightly `#![feature(...)]`\n  \
           check-deferrals fail if a `TODO(<tag>)` has no deferred-decisions.md entry\n  \
           check-docs      fail if a doc links to, or cites, a path that does not exist\n  \
           check-images    fail if a test image and a release image differ by anything new\n  \
           check-irq-scope fail if an interrupt entry stub skips the lock-ordering scope\n  \
           abi-sync-check  fail if userspace/libkern has drifted from the kernel ABI\n  \
           fetch-limine  download the pinned Limine binary tarball\n  \
           clean         remove build outputs and caches\n  \
           help          show this message\n\
         \n\
         `--selftest` (build/image/qemu) compiles + runs the boot self-tests / demos;\n         \
         without it the build boots straight to userspace.\n         \
         `--grab` (qemu/qemu-debug) hands the window the pointer and keyboard. The\n         \
         guest has a relative mouse and no absolute one, so ungrabbed its cursor and\n         \
         yours are two independent cursors that never line up, and every `Super`\n         \
         chord belongs to your desktop instead. Ctrl-Alt-G releases the grab.\n         \
         `--kvm` (any command that boots a guest) runs under hardware virtualisation\n         \
         instead of TCG — faster, and required on a host whose QEMU predates 9.0 (TCG\n         \
         emulates x2APIC only from 9.0, and this kernel is x2APIC-only).\n         \
         Other args after `qemu` / `qemu-debug` are forwarded to QEMU.\n"
    );
}

// --- Paths --------------------------------------------------------------

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `tools/xtask`; the repo root is two up.
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(&manifest);
    match path.parent().and_then(Path::parent) {
        Some(p) => p.to_path_buf(),
        None => panic!("cannot derive repo root from {manifest}"),
    }
}

fn build_cache() -> PathBuf {
    repo_root().join("tools").join("build-cache")
}

fn limine_dir() -> PathBuf {
    build_cache().join("limine")
}

fn kernel_elf() -> PathBuf {
    repo_root()
        .join("kernel")
        .join("target")
        .join("x86_64-unknown-none")
        .join("debug")
        .join("nitrox-kernel")
}

fn image_path() -> PathBuf {
    build_cache().join("nitrox.hdd")
}

fn limine_conf() -> PathBuf {
    repo_root().join("boot").join("limine.conf")
}

// --- Subcommands --------------------------------------------------------

/// The coreutils: one crate, a bin per program. Built, packed into the initramfs, and
/// packaged into the store from this single list — see [`profile_programs`].
const COREUTILS: &[&str] = &[
    "list", "copy", "mkdir", "remove", "rename", "move", "touch", "date", "sleep", "whoami",
    // The graphical session's desktops, and `/dev/desktop`'s first consumer — see M8 Part F.
    "desktop",
    // The clipboard, either side of a pipe — M12 decision 4, which is what makes the kill ring
    // reachable by something other than a window.
    "clip",
];

/// The system services, packaged into the store like any other program.
///
/// **`fs-server-ext4` is here *and* in the initramfs**, and the two copies serve different
/// purposes. The store copy makes it versioned and content-addressed like everything else,
/// and is what a **non-root** fs-server (a second mount) would be spawned from, since root
/// is already up by then. The initramfs copy is the only one that can ever mount the root
/// — and the only one that could ever *re*-mount it, because `/store` is unreadable without
/// the very server being restarted. See `TODO(fs-server-restart)`.
const SYSTEM_SERVICES: &[&str] = &[
    "service-mgr",
    "session-mgr",
    "desktop-session-mgr",
    "desktop-shell",
    "auth-service",
    "logging-service",
    "heartbeat",
    "fs-server-ext4",
    "tty-server",
    // The display arm's two servers. They were initramfs-resident until 2026-08-11 for one
    // reason — they predate `/bin` — and neither has a bootstrap role: a compositor cannot
    // run before there is a root to read a font from, let alone before there is one to spawn
    // it from. Moving them here cost a reordering in `init`, which now brings the display up
    // after the profile server rather than before it.
    "compositor",
    "input-server",
    // The kill ring (M12 Part E). A store package like the rest: nothing about a clipboard is
    // needed to reach a mounted root, and its only client runs long after one.
    "clipboard-server",
];

/// The test programs, packaged into a store package of their own in selftest/test-harness
/// builds and absent from a release image entirely.
///
/// **A separate package from `system`**, so the system package keeps meaning "the services
/// this OS is made of" rather than "those, plus whatever the test build needed". It also
/// keeps the system package's content hash identical between a release and a test image,
/// which is the property a content-addressed path is supposed to have.
const TEST_PROGRAMS: &[&str] = &[
    "test-harness",
    "test-stage",
    "display-selftest",
    "ui-testclient",
    "input-testclient",
    "boot-probe",
    // The M13 Part A measurement. In the same package as the rest: it is a test program, and a
    // package per binary would claim an independence it does not have.
    "compose-bench",
];

fn cmd_build(mode: BuildMode) -> R<()> {
    // Build the userspace programs BEFORE the kernel: the kernel embeds their
    // ELFs via `include_bytes!`, so the artifacts must exist at kernel compile
    // time. Only `init` (and the kernel) carry the selftest / test-harness feature.
    cmd_build_hello()?;
    // The integration smoke-test harness (bins `test-harness`, `test-stage` and
    // `display-selftest`) is built
    // + embedded ONLY in selftest/test-harness builds — absent from release images.
    if mode.features().is_some() {
        build_userspace_crate("test-harness", TEST_PROGRAMS, None)?;
    }
    build_userspace_bin("init", mode.features())?;
    build_userspace_bin("fs-server-ext4", None)?;
    build_userspace_bin("eshell", None)?;
    build_userspace_bin("service-mgr", None)?;
    build_userspace_bin("heartbeat", None)?;
    // The coreutils (`list`, …) — real programs, present in release images. One crate,
    // a bin per program, so the crate directory is named separately from the bins.
    build_userspace_crate("coreutils", COREUTILS, None)?;
    // `nxsh` — the shell. Milestone 3 Part A ships the *language* (lexer + parser, tested
    // on the host); this builds its bin for the bare target so the language cannot
    // quietly stop being part of the OS while it is being written.
    build_userspace_bin("nxsh", None)?;
    build_userspace_bin("profile-server", None)?;
    build_userspace_bin("tty-server", None)?;
    // `input-server` — holds the raw device nodes and serves the merged stream. A
    // lib + bin split like `tty-server`: the merge is host-tested, this builds the bin.
    build_userspace_bin("input-server", None)?;
    build_userspace_bin("logging-service", None)?;
    build_userspace_bin("auth-service", None)?;
    // The clipboard (M12 Part E). A lib + bin split like `auth-service`: the ring is
    // host-tested, this builds the bare-target server.
    build_userspace_bin("clipboard-server", None)?;
    // **`None`, and that is the point.** `session-mgr` took `mode.features()` because it
    // fired the self-test verdict; the retrofit moved the verdict to `boot-probe` and left
    // the crate with no reader for either feature. Passing one anyway would make the next
    // test-only branch compile on the first try, which is how the thing Part B removed comes
    // back — the zero should be a wall, not a count.
    build_userspace_bin("session-mgr", None)?;
    build_userspace_bin("desktop-session-mgr", None)?;
    build_userspace_bin("desktop-shell", None)?;
    build_userspace_bin("compositor", None)?;
    // The GUI terminal (M5 Part B). A lib/bin split like `tty-server`: the state, the view and
    // the update are host-tested, the bin is the window and the event pump.
    // **`test-harness` only**, not `mode.features()` — which is what `init` takes, and would
    // hand this `selftest` as well. The feature makes the terminal report each
    // completed grid line on the debug console for `check-terminal` to assert on; a real build
    // must not have it, because a terminal narrating itself to the kernel log undoes the point
    // of the tty server owning output. A first version passed it unconditionally, so every
    // image shipped the instrumentation (PR #194 review, finding 3).
    build_userspace_bin(
        "nxterm",
        mode.is_test_harness().then_some("test-harness"),
    )?;
    // The file browser (M10 Part B). A lib/bin split like `nxterm`: the listing, the view and
    // the update are host-tested, the bin is the window and the event pump. No `test-harness`
    // feature — what its gate asserts is the line it prints when it lists a directory, which a
    // release image prints too, so there is nothing to compile in conditionally.
    build_userspace_bin("nxfiles", None)?;
    // The text editor (M10 Part D), split the same way and for the same reason. No
    // `test-harness` feature either: what its gate asserts is what it prints when it opens and
    // saves a file, which a release image prints too.
    build_userspace_bin("nxedit", None)?;
    // A library with no consumer yet — see `check_userspace_lib`. `compositor` no longer
    // needs one: its own bin compiles it for the target.
    check_userspace_lib("libdraw")?;
    check_userspace_lib("libsurface")?;
    check_userspace_lib("libinput")?;
    check_userspace_lib("libui")?;
    check_userspace_lib("libterm")?;

    let kernel_dir = repo_root().join("kernel");
    let mut k = Command::new("cargo");
    k.arg("build");
    if let Some(f) = mode.kernel_features() {
        k.arg("--features").arg(f);
    }
    run(k.current_dir(&kernel_dir))?;
    let elf = kernel_elf();
    if !elf.exists() {
        return Err(format!("kernel ELF missing after build: {}", elf.display()).into());
    }
    println!("xtask: built kernel ELF at {}", elf.display());
    Ok(())
}

/// The userspace target's name — the stem of `userspace/x86_64-unknown-nitrox.json`,
/// which is also the directory cargo puts its artifacts under.
///
/// A **custom** spec rather than a built-in target because userspace needs a hard-float
/// ABI and stable rustc ships no freestanding x86_64 target that has one (see the
/// decision log, 2026-07-21). The kernel keeps the built-in `x86_64-unknown-none`.
const USERSPACE_TARGET: &str = "x86_64-unknown-nitrox";

/// A `cargo` command for the **userspace** workspace, resolved against
/// `userspace/rust-toolchain.toml`.
///
/// xtask is itself launched by `cargo run`, which exports `RUSTUP_TOOLCHAIN` into our
/// environment — and that variable *overrides* rustup's directory-based lookup, so a
/// child `cargo` would silently stay on the outer (stable) toolchain and reject `-Z`.
/// Clearing it lets `userspace/rust-toolchain.toml` govern, which keeps the nightly pin
/// in exactly one place instead of duplicating the version string here. `RUSTC`/`RUSTDOC`
/// go too: cargo points them at the outer toolchain's binaries, which would defeat the
/// re-resolution.
fn userspace_cargo() -> Command {
    let mut c = Command::new("cargo");
    c.env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTC")
        .env_remove("RUSTDOC");
    c
}

/// Point a bare-target `cargo` invocation at the Nitrox userspace target.
///
/// A custom spec has no precompiled sysroot, so `core`/`alloc`/`compiler_builtins` are
/// rebuilt from source with `-Z build-std` — the reason `userspace/rust-toolchain.toml`
/// pins a nightly. `compiler-builtins-mem` is deliberately **not** requested: `libkern`
/// exports its own `memcpy`/`memmove`/`memset`/`memcmp`, and enabling the feature would
/// define them twice.
///
/// `--target` is passed as the JSON path relative to the crate directory (which is the
/// cwd for these builds), matching the crate's own `.cargo/config.toml`.
fn arg_userspace_target(c: &mut Command) {
    c.arg("--target")
        .arg(format!("../{USERSPACE_TARGET}.json"))
        .arg("-Z")
        .arg("build-std=core,alloc,compiler_builtins");
}

/// Directory holding built userspace release artifacts.
fn userspace_release_dir() -> PathBuf {
    repo_root()
        .join("userspace/target")
        .join(USERSPACE_TARGET)
        .join("release")
}

/// Path to the built `hello` userspace ELF (release; the kernel embeds this).
fn hello_elf() -> PathBuf {
    userspace_release_dir().join("hello")
}

/// Build the `hello` userspace program as a static `ET_EXEC` for the bare
/// target. Run from `userspace/hello` so that crate's `.cargo/config.toml`
/// (target + non-PIE/static rustflags) applies without affecting the other
/// userspace members.
fn cmd_build_hello() -> R<()> {
    let hello_dir = repo_root().join("userspace").join("hello");
    let mut c = userspace_cargo();
    c.arg("build").arg("--release");
    arg_userspace_target(&mut c);
    run(c.current_dir(&hello_dir))?;
    let elf = hello_elf();
    if !elf.exists() {
        return Err(format!("hello ELF missing after build: {}", elf.display()).into());
    }
    println!("xtask: built hello ELF at {}", elf.display());
    Ok(())
}

/// Build the userspace program `name` as a static `ET_EXEC` for the bare
/// target (run from its own crate dir so its `.cargo/config.toml` applies). The
/// kernel embeds the result via `include_bytes!`. Generalises `cmd_build_hello`
/// for the spawn-demo binaries (`parent`, `child`).
fn build_userspace_bin(name: &str, features: Option<&str>) -> R<()> {
    build_userspace_crate(name, &[name], features)
}

/// Build a userspace **library** crate for the bare target, purely to prove it still
/// compiles there.
///
/// Libraries are normally built as a side effect of the bins that depend on them, so they
/// need no entry here. `libdraw` is the exception: it lands (plan M1 Part A) before the
/// compositor that will consume it, so nothing would compile it for
/// `x86_64-unknown-nitrox` at all, and a `no_std` regression — an accidental `std` path,
/// an `alloc` item behind the wrong cfg — would sit undetected until Part B went to build
/// on it. Its host tests pass on the host target and prove nothing about that.
///
/// Delete the entry once a bin depends on the crate; the bin's build covers it then.
fn check_userspace_lib(dir: &str) -> R<()> {
    let crate_dir = repo_root().join("userspace").join(dir);
    let mut c = userspace_cargo();
    c.arg("build").arg("--release").arg("-p").arg(dir);
    arg_userspace_target(&mut c);
    run(c.current_dir(&crate_dir))?;
    println!("xtask: {dir} compiles for {USERSPACE_TARGET} ✓");
    Ok(())
}

/// Build the userspace crate in `userspace/<dir>` for the bare target, then verify each of
/// `bins` produced an ELF. Most crates are one directory per program, so
/// [`build_userspace_bin`] covers them; a crate holding several programs (`coreutils`,
/// `test-harness`) names them here, since the directory no longer matches the bin name.
///
/// The build must run with `cwd` = the crate directory: its `.cargo/config.toml` is what
/// selects the custom target and the static/non-PIE link flags.
fn build_userspace_crate(dir: &str, bins: &[&str], features: Option<&str>) -> R<()> {
    let crate_dir = repo_root().join("userspace").join(dir);
    let mut c = userspace_cargo();
    c.arg("build").arg("--release");
    arg_userspace_target(&mut c);
    if let Some(f) = features {
        c.arg("--features").arg(f);
    }
    run(c.current_dir(&crate_dir))?;
    for name in bins {
        let elf = userspace_release_dir().join(name);
        if !elf.exists() {
            return Err(format!("{name} ELF missing after build: {}", elf.display()).into());
        }
        println!("xtask: built {name} ELF at {}", elf.display());
    }
    Ok(())
}

fn cmd_image(mode: BuildMode) -> R<()> {
    cmd_build(mode)?;
    let limine_root = cmd_fetch_limine()?;
    let bootx64 = find_bootx64(&limine_root)?;
    let initramfs = initramfs_path();
    build_initramfs(&initramfs, mode)?;
    assemble_image(
        &bootx64,
        &kernel_elf(),
        &limine_conf(),
        &initramfs,
        &image_path(),
        mode,
    )?;
    println!("xtask: image at {}", image_path().display());
    Ok(())
}

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
enum Accel {
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
fn preflight_accel(accel: Accel) -> R<()> {
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
            if let Some((major, minor)) = qemu_version() {
                if major < 9 {
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
    }
    Ok(())
}

fn qemu_base_args(qemu: &mut Command, ovmf: &Firmware, accel: Accel) -> R<()> {
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
/// `Super`-something (`Super+H`, `Super+1`, `Super+Shift+1`, `Super+R`), and GNOME, KDE and
/// COSMIC all bind `Super` at the compositor. Ungrabbed, those keystrokes are the *host's*,
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
fn cmd_qemu(
    debug: bool,
    mode: BuildMode,
    accel: Accel,
    grab: bool,
    extra_args: &[String],
) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(mode)?;
    let ovmf = locate_ovmf()?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut qemu, &ovmf, accel)?;
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

/// **Interactive-session tests: drive the real login and shell over the serial console.**
///
/// This boots `BuildMode::Normal` — the **release image**, which nothing else ever boots.
///
/// **Why it exists**, in the past tense since 2026-08-21: `session-mgr` used to auto-log-in
/// and run a fixed script under `test-harness`, so the `login:` prompt, a typed password, a
/// real shell prompt and `exit` were all `#[cfg(not(feature = "test-harness"))]` code that CI
/// compiled and never executed. Every interactive bug this project has had lived exactly
/// there — the console read using the wrong rights, a `cd` guard refusing a builtin that
/// existed, a login that could not be repeated, a password prompt landing on the username's
/// line. Retrofit Part B deleted that substitution: `session-mgr` now has one `login()` in
/// every build, and this gate is what exercises it.
///
/// It still boots the only release image any gate boots, and that is still the point — the
/// `test-harness` image differs by a service declaration and by `init`'s remaining cfgs
/// (retrofit Part C), so "the same code" is a claim this gate is the only one able to test.
///
/// **Expect-driven, not sleep-driven.** Each step waits for the text that says the guest is
/// ready for it, so the run is paced by the guest rather than by guessed delays. That is
/// the difference between a test and a flake.
///
/// One boot serves every scenario: the shell returns to `login:`, so the session sequence
/// continues rather than paying ~15 s of boot per case.
fn cmd_test_interactive(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;
    let ovmf = locate_ovmf()?;

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel)?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        // **`signal=off` is what lets the guest see `Ctrl-C`.** QEMU's `stdio` chardev
        // defaults to `signal=on`, where `0x03` on stdin is QEMU's own interrupt and never
        // reaches the guest — so the harness could type every key except the one §11h is
        // about. Spelled as an explicit chardev because `-serial stdio` has no way to say
        // it.
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

    println!("xtask: interactive session tests (release image, expect-driven)…\n");
    let mut session = Session::spawn(cmd, "test-interactive")?;
    let result = run_interactive_scenarios(&mut session);
    let transcript = session.finish();

    match result {
        Ok(n) => {
            println!("\nxtask: interactive tests PASSED ({n} steps)");
            Ok(())
        }
        Err(e) => {
            // The transcript is the diagnosis — without it a failure says only "expected X".
            println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
            Err(e)
        }
    }
}

/// The scenarios, in one boot. Returns the number of steps that passed.
fn run_interactive_scenarios(s: &mut Session) -> R<usize> {
    let mut steps = 0usize;

    // 1. The machine reaches a login prompt at all — the release image's first claim.
    s.expect("nitrox login:")?;
    steps += 1;

    // 2. A wrong password is refused and the prompt comes back. Tested before the good
    //    one so a broken denial cannot hide behind a successful login.
    s.send("alice")?;
    s.expect("password:")?;
    s.send("wrong-password")?;
    s.expect("login incorrect")?;
    s.expect("nitrox login:")?;
    steps += 1;

    // 3. The password prompt begins its own line. `read_line` used to return on CR/LF
    //    without echoing it, so the cursor never left the username's line and the prompt
    //    rendered as `alicepassword:`.
    s.send("alice")?;
    s.expect("\npassword:")?;
    steps += 1;

    // 4. A correct password reaches a shell prompt.
    s.send(DEMO_PASSWORD)?;
    // **Through the shared session core**, which is what makes this a check on M7 Part B
    // rather than only on the login. `session-mgr` no longer spawns the shell itself: it
    // calls `libsession::spawn_leader`, and this line is that function's. Without it the
    // refactor could be inert — a crate that compiles, is linked, and is not on the path —
    // which is exactly the shape of the title cap PR #233 shipped. The line names the
    // program, so a supervisor that went back to spawning `nxsh` directly would be silent
    // here rather than passing.
    s.expect("libsession: nxsh spawned into the session namespace")?;
    s.expect("/home>")?;
    steps += 1;

    // 5. A program from the profile runs — `/bin` is bound in the session namespace and
    //    the shell can spawn through it.
    s.send("whoami")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 5a. **The session's environment is what `session-mgr` built** — the shell starts in
    //     the principal's home, and `$env.HOME` names it.
    //
    //     Steps 5a–5c are the login proof, moved here from a script `session-mgr` ran under
    //     `test-harness` after auto-logging-in. It asserted the same three things in a build
    //     where the typed login above did **not exist** — `login()`, `tty_open` and the whole
    //     `tty_*` layer were `#[cfg(not(feature = "test-harness"))]`. Proving them against the
    //     release image is the point of `docs/planning/test-path-retrofit.md`; they land here
    //     *before* the script is deleted, so no coverage is lost in between.
    s.send("format(\"athome={}\", ($env.PWD == $env.HOME))")?;
    s.expect("athome=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 5b. **Home is writable, and what was written reads back.** The fs endpoint is bound
    //     subtree-scoped to the principal's home, so this is the sandbox working rather than
    //     a filesystem working: the same shell cannot reach anything above it.
    s.send("[1, 2] | save ./nx-login.txt")?;
    s.expect("/home>")?;
    s.send("format(\"rows={}\", (open ./nx-login.txt | count))")?;
    s.expect("rows=2")?;
    s.expect("/home>")?;
    steps += 1;

    // 5c. **A directory read of the same bind finds it.** The script asserted `list . | count`
    //     was at least one, which a count cannot express here; naming the file is stronger
    //     anyway — a count of one passes on a home holding some *other* file.
    s.send("list .")?;
    s.expect("nx-login.txt")?;
    s.expect("/home>")?;
    steps += 1;

    // 6. A *failing* stage reports and returns to the prompt rather than hanging. This is
    //    the shape that hung the shell for an afternoon: a stage that exits without
    //    writing, whose pipe only closes once its process is reclaimed.
    s.send("list /nope")?;
    s.expect("filesystem unreachable")?;
    s.expect("/home>")?;
    steps += 1;

    // 7. The interpreter keeps state across lines — a `def` typed at the prompt is
    //    callable afterwards.
    //
    //    **The answer is wrapped, for the reason steps 12 and 13 give.** It was `add(2, 3)`
    //    asserted with a bare `expect("5")`, which is the weakest pattern in the gate: a
    //    single character, in a stream that also carries heartbeat and service-manager
    //    logging. A transcript of one passing run contains 22 occurrences of `5`, fourteen
    //    of them before this step even runs — so the step passed on a `seq=` or `uptime_ns=`
    //    digit as readily as on the shell's answer, and was observed doing exactly that.
    //
    //    The call is bound with `let` and formatted on the next line rather than nested as
    //    `format("add={}", add(2, 3))`, which nxsh rejects — a user-function call inside an
    //    argument list is a parse error ("expected , or ) in an argument list"). Both
    //    constructs used here are already exercised by steps 12 and 14.
    s.send("def add(a, b) { a + b }")?;
    s.expect("/home>")?;
    s.send("let sum = add(2, 3)")?;
    s.expect("/home>")?;
    s.send("format(\"add={}\", sum)")?;
    s.expect("add=5")?;
    steps += 1;

    // 8. **History.** `\x1b[A` is Up; `send` appends the Enter. Recalling `whoami` and
    //    running it proves the *buffer* was replaced, which is the part that matters —
    //    the erase-and-redraw is bytes on a wire that only a real terminal renders, so
    //    asserting on appearance would assert on the capture rather than the shell.
    s.send("whoami")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    s.send("\x1b[A")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 9. Two Ups walk further back: with `date` most recent, the second Up reaches
    //    `whoami` again. This is what distinguishes a working cursor from a history that
    //    only ever returns its newest entry.
    s.send("date")?;
    s.expect("unix")?;
    s.expect("/home>")?;
    s.send("\x1b[A\x1b[A")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 10. **Reverse-search.** `\x12` is Ctrl-R; the query narrows to `whoami`, and Enter
    //     runs it. Asserting on the command's *output* again, not on the search prompt —
    //     the redraw is erase-and-rewrite, which only a real terminal renders.
    s.send("\x12whoa")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 11. **Cancelling a search restores what was being typed.** Type `date`, search for
    //     something else, abandon it with Ctrl-G, press Enter — `date` must run. This is
    //     the property that makes Ctrl-R safe to press by accident.
    s.send("date\x12who\x07")?;
    s.expect("unix")?;
    s.expect("/home>")?;
    steps += 1;

    // 12. **`break` at a real prompt** — and the multi-line path with it. The body holds
    //     two statements, which are newline-separated (§9a), so the loop spans two lines
    //     and the REPL's unclosed-brace continuation carries it (§11b). The `mut` survives
    //     from the line before, which is what makes the count observable at all. Host
    //     tests cover the semantics; this covers the path they cannot — every expensive
    //     bug this shell has had was interactive-only.
    //
    //
    //     Two things here were got wrong first and are worth keeping right. **The
    //     assertion is `n=3`, not `3`**: `expect` scans forward until it matches, so a
    //     bare digit finds the echo of the line that was just typed. And **the increment
    //     comes before the test**, so that "the loop stopped" and "the rest of the body
    //     was skipped" give different answers — with the increment last, a `break` that
    //     merely abandoned the iteration would leave `n` at 3 as well, and the step would
    //     pass against a broken implementation. Both were found by breaking `break` and
    //     watching this step keep passing.
    s.send("mut n = 0")?;
    s.expect("/home>")?;
    s.send("for x in 0..9 { n = n + 1\n if n == 3 { break } }")?;
    s.expect("/home>")?;
    s.send("format(\"n={}\", n)")?;
    s.expect("n=3")?;
    s.expect("/home>")?;
    steps += 1;

    // 13. **`parse` at a real prompt**, both directions of §6's contract: text becomes a
    //     number and takes part in arithmetic, and text that is not a number fails loud
    //     rather than becoming zero. `format` wraps the answer for the same reason as the
    //     step above — `42` on its own would match the echo of what was typed, `sum=42`
    //     cannot.
    s.send("format(\"sum={}\", (\"40\" | parse Int) + 2)")?;
    s.expect("sum=42")?;
    s.expect("/home>")?;
    s.send("\"abc\" | parse Int")?;
    s.expect("cannot parse")?;
    s.expect("/home>")?;
    steps += 1;

    // 14. **Errors are constructible and recoverable, at a real prompt.** `fail` raises
    //     with a message a script chose, `catch` binds it as an ordinary record, and `try`
    //     in value position supplies a fallback — the three halves of §2 that did not
    //     exist before Part C. `n=8080` rather than `8080` for the reason step 12 gives.
    //
    //     **The catch block formats its answer** so the assertion cannot be satisfied by the
    //     terminal's echo of the command. `expect("boom")` matched the echoed line — the
    //     word is in the text being typed — and so passed with the `try`/`catch` removed
    //     from the command entirely. `caught=boom` appears only if the block ran.
    s.send("try { fail \"boom\" } catch (e) { format(\"caught={}\", e.message) }")?;
    s.expect("caught=boom")?;
    s.expect("/home>")?;
    s.send("let n = try { \"x\" | parse Int } catch { 8080 }")?;
    s.expect("/home>")?;
    s.send("format(\"n={}\", n)")?;
    s.expect("n=8080")?;
    s.expect("/home>")?;
    steps += 1;

    // 15. **Sequences and reduction** at a real prompt: a String is a sequence of its
    //     characters (so `count` is its length), and a Range is a sequence of its values
    //     (so it can be summed without first being written out as a list). Both were
    //     "expected a Table or a List" before Part D.
    s.send("format(\"len={}\", (\"hello\" | count))")?;
    s.expect("len=5")?;
    s.expect("/home>")?;
    s.send("format(\"sum={}\", (1..=10 | sum))")?;
    s.expect("sum=55")?;
    s.expect("/home>")?;
    // …and Part E's breadth: text taken apart and put back, and membership.
    s.send("\"a,b,c\" | split \",\" | join \"-\"")?;
    s.expect("a-b-c")?;
    s.expect("/home>")?;
    s.send("format(\"hit={}\", (2 in [1, 2]))")?;
    s.expect("hit=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 16. **`capture` at a real prompt**, the shape §10b writes: match, take a group,
    //     convert it. Text that matched could only be tested before, never taken apart.
    //     The second line pins the alternation fix — `~=` with a second branch that has to
    //     win was false for the entire life of the engine.
    s.send("let g = \"port 8080\" | capture /(\\d+)/")?;
    s.expect("/home>")?;
    s.send("format(\"port={}\", (g[1] | parse Int))")?;
    s.expect("port=8080")?;
    s.expect("/home>")?;
    s.send("format(\"alt={}\", (\"b\" ~= /a|b/))")?;
    s.expect("alt=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 17. **`Ctrl-C` interrupts a running evaluation** (§11h) — the hazard that section
    //     exists for. `while true { }` has no exit and, on a system with no SIGINT, no way
    //     to be stopped: it meant a reboot.
    //
    //     The loop and the interrupt go in **one write** deliberately. Sent separately it
    //     is a race the test loses: an empty loop reaches the ten-million-iteration
    //     backstop in well under a second, so the run would end on its own and the step
    //     would pass without the interrupt ever arriving. One write puts the byte in the
    //     guest before evaluation starts, and the tty holds it until the shell's next
    //     checkpoint asks.
    s.send_raw("while true { }\n\x03")?;
    s.expect("interrupted")?;
    s.expect("/home>")?;
    // …and the shell is still usable afterwards, which is the actual claim.
    s.send("format(\"alive={}\", 1 + 1)")?;
    s.expect("alive=2")?;
    s.expect("/home>")?;
    steps += 1;

    // 18. **`Ctrl-C` at a prompt discards the line** rather than leaving it half-typed —
    //     the same event, and the shell decides what it means from what it was doing.
    s.send_raw("garbage-that-should-vanish\x03")?;
    s.expect("^C")?;
    s.expect("/home>")?;
    s.send("format(\"clean={}\", 2 + 2)")?;
    s.expect("clean=4")?;
    s.expect("/home>")?;
    steps += 1;

    // 19. **`Ctrl-C` stops a running *stage*** (§11h, G2).
    //
    //     The ordering is made deterministic rather than assumed, and that took two goes.
    //     Sending `sleep 60\n\x03` in one write does **not** test this: the interrupt is
    //     already queued when the line is read, so the shell's statement checkpoint fires
    //     before the pipeline ever starts and nothing is ever spawned — it passes without
    //     a stage being involved. Sending the interrupt separately is a race the test can
    //     only lose silently.
    //
    //     So the guest supplies the anchor. Two lines go in one write; `started=1` is
    //     printed by the first, which proves the second is *now running*, and only then is
    //     the interrupt sent. Sixty seconds is far past the 45s expect timeout, so this can
    //     only pass by `sleep` actually being cut short.
    s.send_raw("format(\"started={}\", 1)\nsleep 60\n")?;
    s.expect("started=1")?;
    s.send_raw("\x03")?;
    s.expect("interrupted")?;
    s.expect("/home>")?;
    steps += 1;

    // 19. **`list [x]` returns a prompt instead of locking the shell.** The word-mode
    //     scanner produced an empty token at `[` without advancing, so the argument loop
    //     bumped it forever — a user-reachable hang on a path that predates every part of
    //     Milestone 4, and one with no escape until Ctrl-C exists (§11h). It is asserted
    //     in guest because "the shell is still there" is the whole claim.
    s.send("list [x]")?;
    s.expect("cannot begin a bareword")?;
    s.expect("/home>")?;
    steps += 1;

    // 19c. **The clipboard, from a pipeline** — M12 Part E and decision 4, whose whole point is
    //      that this resource is not graphical-only. Four claims in order: a session's namespace
    //      carries `/dev/clipboard`; a pipeline can push into the ring; a paste comes back out
    //      as text; and the ring is a *ring*, so what was copied first is still reachable at
    //      index 1 after a second copy.
    //
    //      **Asserted from the serial column deliberately.** The graphical gate proves copy in
    //      one application and paste in another; this proves the same server answers a process
    //      with no window at all, which is the half a windowed test cannot see.
    s.send("\"clip-one\" | clip --copy")?;
    s.expect("/home>")?;
    s.send("clip")?;
    s.expect("clip-one")?;
    s.expect("/home>")?;
    s.send("\"clip-two\" | clip --copy")?;
    s.expect("/home>")?;
    s.send("clip")?;
    s.expect("clip-two")?;
    s.expect("/home>")?;
    // Index 1 is the one before it — the property that makes this a kill ring rather than a
    // slot, and the one a single-slot implementation would fail while passing everything above.
    s.send("clip 1")?;
    s.expect("clip-one")?;
    s.expect("/home>")?;
    // …and the listing says how much is there without saying what it is.
    s.send("clip --list | count")?;
    s.expect("2")?;
    s.expect("/home>")?;
    steps += 1;

    // 20. **`exit N` sets the status**, which `session-mgr` logs — so the argument form is
    //     observable rather than merely "the shell left". Before Part C the driver matched
    //     the literal line `exit`, so `exit 3` missed it entirely and came back as
    //     "`exit` is handled by the shell's driver".
    s.send("exit 3")?;
    s.expect("shell exit 3")?;
    s.expect("nitrox login:")?;
    s.send("alice")?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    steps += 1;

    // 21. A bare `exit` still returns to the login prompt, and logging in again works. A
    //     login that cannot be repeated is not a login.
    s.send("exit")?;
    s.expect("nitrox login:")?;
    s.send("alice")?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    steps += 1;

    Ok(steps)
}

/// `cargo xtask check-input` — inject a keystroke and a click, and check they arrive.
///
/// The counterpart to `check-display`, and the answer to the same class of problem. A
/// display gate exists because the guest staying consistent with itself proves nothing about
/// the device binding; an input gate exists because a driver that reports `armed` proves
/// nothing about whether an interrupt ever produced an event. Both are cases where every
/// self-contained check passes and the thing still does not work.
///
/// Since M3 Part B this runs through the **whole path**: i8042 → driver ring → raw node →
/// `input-server` → merge → consumer channel → guest. The client consumes `/dev/input/new`
/// rather than the raw nodes, which it can no longer open — the driver is single-reader per
/// device and the server holds both, which is the exclusivity the keylogging boundary rests
/// on, demonstrated rather than asserted.
///
/// The guest resolves `/dev/input/new`, prints `listening`, and only then does this inject:
/// an event produced before the consumer channel exists is one the server has nowhere to
/// send, so injecting on a timer would make this flaky in exactly the way a test of a rare
/// path must not be.
fn cmd_check_input(accel: Accel, no_ps2_irq: bool) -> R<()> {
    preflight_accel(accel)?;
    // **`--no-ps2-irq` boots the same image with the i8042's interrupt generation left off**,
    // so every byte has to be recovered by the tick-driven `ps2::poll` sweep rather than
    // arriving on IRQ 1 / 12. The assertions below are unchanged — that is the point. A
    // keystroke and a click still have to reach a userspace client; only the road they take
    // is different.
    //
    // It exists because **no pass count can catch deleting that sweep.** On healthy hardware
    // the interrupt path carries every byte and the sweep never fires, so it is pure
    // redundancy: remove it and every gate in the tree still passes. This flag makes the
    // redundancy load-bearing, which is the only way its absence becomes observable. The
    // sweep itself was added 2026-08-13 for the i8042's one-byte output buffer losing edges —
    // a bug that made every input gate intermittently flaky and was written off as flakiness
    // for weeks.
    cmd_image(if no_ps2_irq {
        BuildMode::TestHarnessNoPs2Irq
    } else {
        BuildMode::TestHarness
    })?;
    let ovmf = locate_ovmf()?;
    let qmp_sock = build_cache().join("check-input.qmp");
    fs::create_dir_all(build_cache())?;
    let _ = fs::remove_file(&qmp_sock);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel)?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
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

    println!("xtask: input gate — booting and injecting…\n");
    let mut session = Session::spawn(cmd, "check-input")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;

    // The compositor is a consumer of the same merged stream — it resolves `/dev/input/new`
    // during startup, which is why init binds the input server first. Asserted *before* the
    // test client's `listening`, which is the ordering init fixes: the compositor is spawned
    // and answers `Meta::Ready` well before the selftest client runs.
    //
    // This proves the compositor is **attached**, not that a key reached a window: with no
    // client holding a window at injection time there is nothing to route to. That end of
    // the chain is Part D's gate, which brings an echo client that stays alive.
    session.expect("compositor: input connected")?;

    session.expect("input-testclient: listening")?;

    // **Wait for `ui-testclient` to finish before injecting anything.** Its leak probe churns
    // 128 windows, and every one of them is a `Normal` window that becomes, for its brief
    // life, the topmost window that takes focus — so a keystroke injected while it runs is
    // routed to a window that is about to be destroyed. That is the compositor behaving
    // correctly; it is the *gate* that was assuming a quiet stack. Diagnosed from the
    // compositor's own routing log: `key win=9` while the client had announced `id=8`.
    //
    // Safe to wait for here rather than later: `listening` is printed milliseconds after
    // spawn, the churn takes seconds, so this line always follows it.
    session.expect("ui-testclient: PASSED")?;

    // ---- Motion survives a consumer that stops reading ----
    //
    // **The guard on the property PR #246 exists to keep**, and the accelerator cannot change
    // what it means. A relative delta *is* the pointer's position: a batch `input-server` cannot
    // deliver is movement no consumer can re-derive, unlike a key or a button, whose state
    // `SYN_DROPPED` asks it to resynchronise. Discarding one leaves every consumer permanently
    // offset from the device.
    //
    // **The overrun is caused by the client, not by the clock.** The first version of this guard
    // lived in `check-login` and outran the compositor's repaint — which works under TCG and not
    // under KVM, where the repaint is fast enough that the ring never fills, and `--kvm` is the
    // only configuration CI runs (PR #246 review, blocking 1). Here the client announces that it
    // has stopped reading and sleeps; the ring fills at any speed.
    //
    // **First, before anything else is injected**, because a sum cannot be delimited: with
    // `--no-ps2-irq` input arrives on a 10 ms sweep and an earlier phase's flood is still
    // trickling in when this one starts. Bracketing it with a button press and release was the
    // first repair and failed for the right reason — a button in an overflowing ring is exactly
    // what deferral cannot recover, so the delimiter was eaten by the overrun it measured.
    //
    // The assertion is arithmetic: relative deltas are additive, so whatever the batching,
    // coalescing and deferral in between do, the consumer must end up with the total injected.
    // **Started by a keystroke, not by the client's own clock.** This step sits between two
    // independent clients' output and `ui-testclient` finishes on its own schedule, so a client
    // that stalled at boot was racing whatever the harness was waiting for — and this step's
    // expects then consumed the `PASSED` line the wait above needed (CI, 2026-08-27). `F2` is
    // unbound: `ui-testclient` registers `Super+F1`.
    press(&mut qmp, "f2")?;
    session.expect("input-testclient: input stalled")?;
    // **Thirty, not sixty.** The ring is sixteen messages and the client is asleep, so thirty
    // overruns it by fourteen — the property is "it overran", not "by how much".
    const BURST: i32 = 30;
    const BDX: i32 = 7;
    const BDY: i32 = 3;
    // **Paced, and the pacing is not a workaround for the guest.** The overrun under test comes
    // from a consumer that has stopped reading, not from how fast the harness injects — so
    // slowing the injection costs the measurement nothing. What it avoids is *host*-side loss:
    // with the i8042's interrupts off (`--no-ps2-irq`) nothing reads the controller until the
    // 10 ms recovery sweep, and QEMU's own PS/2 queue is sixteen bytes — barely five packets —
    // so a burst injected as fast as QMP accepts it overflows *that* and the deltas are gone
    // before the guest ever sees them. Measured: one to five packets missing per run, with the
    // guest announcing no loss at all, because nothing in the guest lost anything.
    for _ in 0..BURST {
        qmp.send_motion(BDX, BDY)?;
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    // The sum, and the evidence that it was worth summing. `REL_Y` comes back with the sign the
    // harness injected — the PS/2 wire reports positive-Y as up and the driver negates, which
    // `check-input`'s earlier motion assertion pins.
    session.expect("input-testclient: motion sum ")?;
    let line = session.rest_of_line()?;
    let field = |name: &str| -> Option<i64> {
        line.split_whitespace()
            .filter_map(|f| f.split_once('='))
            .find(|(k, _)| *k == name)
            .and_then(|(_, v)| v.parse().ok())
    };
    let (dx, dy) = (field("dx"), field("dy"));
    let (announced, widest) = (field("announced"), field("widest"));
    let want = (Some((BURST * BDX) as i64), Some((BURST * BDY) as i64));
    if (dx, dy) != want {
        let _ = session.child.kill();
        return Err(format!(
            "input gate FAILED: {BURST} motions of ({BDX}, {BDY}) injected while the consumer \
             was not reading summed to {dx:?}, {dy:?} rather than {:?}, {:?}. A relative delta \
             that does not arrive cannot be recovered — `input-server` must carry the motion of \
             an undeliverable batch forward and re-emit it, not count it as a `SYN_DROPPED` gap. \
             The line was: motion sum {line}",
            want.0, want.1
        )
        .into());
    }
    if announced != Some(0) {
        let _ = session.child.kill();
        return Err(format!(
            "input gate FAILED: a motion-only burst announced {announced:?} lost records. \
             Nothing here is unrecoverable, so nothing should be announced: motion sum {line}"
        )
        .into());
    }
    // **The precondition, checked rather than assumed.** Recovery is invisible by design — the
    // stream still adds up — so a run where the ring never filled satisfies the sum above while
    // testing nothing. A folded group carries the total of the batches it stands for, so it is
    // wider than any single injected delta; equal to it means no deferral happened and this gate
    // proved nothing. That is not a pass.
    if widest.is_none_or(|w| w <= BDX as i64) {
        let _ = session.child.kill();
        return Err(format!(
            "input gate FAILED: the consumer's widest REL_X was {widest:?}, no more than the \
             {BDX} injected — so no batch was ever deferred and this gate did not exercise the \
             path it exists for. Raise BURST until the {}-slot consumer ring overruns, or check \
             that the client really stopped reading: motion sum {line}",
            "16"
        )
        .into());
    }
    println!("  ok: {BURST} motions across a stalled consumer arrived in full (widest fold {widest:?})");



    // A key down and up. `a` is scancode 0x1E in set 1, so keycode 30 — chosen because it
    // is in the identity range the decoder relies on, and because a wrong `E0` or release
    // bit shows up as a different code rather than as silence.
    qmp.send_key("a", true)?;
    session.expect("input-testclient: ev kind=1 code=30 value=1")?;
    qmp.send_key("a", false)?;
    session.expect("input-testclient: ev kind=1 code=30 value=0")?;

    // Motion, then a click. The mouse reports positive-Y as up and `REL_Y` is positive-down,
    // so a downward injection must come back **negated** — that inversion is a one-line
    // mistake no host test can catch, because the host has no PS/2 wire to be wrong about.
    qmp.send_motion(5, 3)?;
    session.expect("input-testclient: ev kind=2 code=0 value=5")?;
    // `REL_Y` must come back **+3**, matching the injected downward motion. The PS/2 wire
    // reports positive-Y as *up*, so this only holds because the driver negates — and a
    // missing negation is a one-line error no host test can catch, because the host has no
    // PS/2 wire to be wrong about. This assertion is the only thing standing between an
    // inverted mouse and a green build.
    session.expect("input-testclient: ev kind=2 code=1 value=3")?;
    qmp.send_button("left", true)?;
    session.expect("input-testclient: ev kind=1 code=272 value=1")?;
    // Release it: phase 2 asserts on the button mask, and a press left held from here would
    // make `buttons` already non-zero before its own click.
    qmp.send_button("left", false)?;

    // ---- Through a window ----
    //
    // Everything above proves the *device* stream reached userspace. This proves the
    // Surface path: the compositor routed it to a window and `libsurface` delivered it into that
    // window's queue. The client creates the window only now, so the two phases cannot
    // contend for its four-message session ring.
    session.expect("input-testclient: window ready")?;

    // **A deliberate flood, and a weak one.** Twelve cursor movements are what broke this
    // gate before M3 Part D3, when each became a `PointerEvent` sent `NOBLOCK` at a
    // four-message ring and the keystroke behind them went on the floor.
    //
    // It proves less than it looks: the client drains in `wait_event` between injections, so
    // thirteen messages never queue against a sixteen-slot ring and **no send is ever
    // refused**. It covers neither coalescing nor retry — the phase-3 stall below is what
    // covers those. Kept because it is still the historical regression, cheap, and the shape
    // a real cursor movement takes.
    for _ in 0..12 {
        qmp.send_motion(-120, -120)?;
    }

    // **The window was told it has the keyboard.** The second half of the two-focus rule:
    // widget focus is the toolkit's, window focus is the compositor's, and a client needs
    // both to know whether a caret should blink. Announced on the change, so this arrives
    // once when the window becomes the topmost focusable one.
    // **Ordering, not just arrival.** Focus is announced on the create itself, so it
    // precedes any input this window could be routed. Asserting only that a focus event
    // *arrived* passes against a compositor that announces it late — verified by
    // reintroducing that bug and watching the gate stay green (PR #184 review, finding 2).
    session.expect("input-testclient: first win event=focus")?;
    session.expect("input-testclient: win focus has=1")?;

    // `b` is keycode 48. It goes to the *focused* window — this client's, being the topmost
    // that takes focus — not to whatever the cursor happens to be over.
    qmp.send_key("b", true)?;
    session.expect("input-testclient: win key code=48 down=1")?;
    // **Through the toolkit**, which is Part B's actual deliverable: the same keystroke came
    // out of `libui`'s router after `element -> layout -> diff -> route`, addressed to a
    // widget rather than a window. Everything in that chain is unit-tested; this is the only
    // thing that says the pieces are wired to each other.
    session.expect("input-testclient: widget key code=48 down=1")?;
    // **Held, not released**: the compositor repeats it. Asserted before the release,
    // because a repeat that only arrived after the key came up would be a bug that a test
    // ordering these the other way round could not see.
    session.expect("input-testclient: win repeat code=48")?;

    qmp.send_key("b", false)?;
    session.expect("input-testclient: win key code=48 down=0")?;

    // ---- A registered chord is consumed (M8 Part B) ----
    //
    // **Here, not at the end of the gate.** The first version ran this after
    // `input-testclient: PASSED`, where the client has stopped logging window events — so a
    // compositor that delivered the chord produced the same silence as one that consumed it,
    // and the control (fire *and* deliver) passed. It has to run while the window that holds
    // focus is still reporting what it receives.
    //
    // The compositor's own tests pin consumption over `route`, the function the binary routes
    // with. What only a boot can show is that a chord injected on a real PS/2 wire, into a stack
    // where a real client holds the keyboard, does not also type into it.
    //
    // **Twice, so the screen ends as it started.** `ui-testclient` toggles the desktop on each
    // chord, and everything after this needs its window back on screen.
    for want in ["desktop 2", "desktop 1"] {
        qmp.send_key("meta_l", true)?;
        qmp.send_key("f1", true)?;
        // **Held past the repeat delay, which is the whole of what the first version missed.**
        // Repeat is armed from the *physical* transition, so a consumed chord used to arm one
        // anyway and deliver its key to the focused window 400 ms later, bypassing the router
        // entirely. Injecting down-and-up back to back never reached that timer, so the gate was
        // blind to it — `input-testclient` logs repeats like any other key, so holding the chord
        // is all that was needed (PR #241 review, blocking 1). `REPEAT_DELAY_NS` is 400 ms.
        std::thread::sleep(std::time::Duration::from_millis(700));
        qmp.send_key("f1", false)?;
        qmp.send_key("meta_l", false)?;
        // **The positive half, and it is what makes the absence at the end mean anything.**
        // Without it a chord that was never injected — a wrong qcode, a dropped QMP command —
        // would produce the same silence as one correctly consumed.
        session.expect(&format!("ui-testclient: hotkey fired -> {want}"))?;
    }
    // **And a chord whose action moves nothing, held past the repeat delay.** The two above
    // switch desktops, which empties the current one — so `focus_candidate` becomes `None` and
    // `fire_repeat` cancels itself, making this gate immune to a wrongly-armed repeat by
    // coincidence of what the client does with the chord. `Super+Space` only prints, so focus
    // stays where it is and a repeat that should not exist lands in the focused window's log.
    qmp.send_key("meta_l", true)?;
    qmp.send_key("spc", true)?;
    std::thread::sleep(std::time::Duration::from_millis(700));
    qmp.send_key("spc", false)?;
    qmp.send_key("meta_l", false)?;
    session.expect("ui-testclient: quiet chord fired")?;

    // No wait for focus here: the compositor announces it back **between** the two chords, so
    // an expect placed after them scans forward past a line already emitted and times out. The
    // click assertions below are what depend on the window being back, and they say so.

    // And a click, which is routed by hit-testing instead of focus. `buttons=1` is the mask
    // the record carries on every kind — the field that used to read zero here.
    qmp.send_button("left", true)?;
    session.expect("input-testclient: win ptr kind=1 btn=272 buttons=1")?;
    // Kind 1 is `POINTER_BUTTON`, and the coordinates are **widget-local**: the grid fills
    // the window and sits at its origin, so they match — which is exactly why the host tests
    // place a widget away from the origin as well.
    session.expect("input-testclient: widget ptr kind=1")?;
    qmp.send_button("left", false)?;

    // ---- Park-and-retry ----
    //
    // The client stops draining. The flood then overruns its 16-slot ring: everything after
    // that parks in the compositor's per-session outbox, where motion coalesces to a single
    // record and the key queues behind it. On waking, the key must still arrive.
    //
    // This is the only assertion covering park-and-retry. The earlier flood covers neither
    // it nor coalescing — with the client draining between injections a send is never
    // refused at all, which is finding 3 of the PR #181 review.
    session.expect("input-testclient: stalling")?;

    // Comfortably past the 16-slot ring, so the outbox is genuinely holding messages.
    for _ in 0..40 {
        qmp.send_motion(3, 3)?;
    }
    // `c` is keycode 46. Injected *after* the ring is full, so it can only arrive if the
    // compositor parked it and re-sent it — and only if it wakes itself to do so, since a
    // client draining its own ring signals nothing to the compositor.
    qmp.send_key("c", true)?;

    session.expect("input-testclient: late key code=46")?;

    session.expect("input-testclient: PASSED")?;

    let transcript = session.finish();
    let _ = fs::remove_file(&qmp_sock);

    // `f1` is keycode 59. The window that had the keyboard must have seen neither the press nor
    // its release — `win key` is the compositor's delivery and `widget key` is `libui`'s router
    // one layer above it, so checking both says the chord stopped at the compositor rather than
    // being dropped later by luck.
    // 59 is F1, 57 is Space — the two chords this client registers.
    for line in ["win key code=59", "widget key code=59", "win key code=57"] {
        if transcript.contains(line) {
            return Err(format!(
                "input gate FAILED: a registered chord reached the focused window — the \
                 transcript contains \"{line}\". `ui-testclient` registers `Super+F1` (59) and \
                 `Super+Space` (57), so the compositor must consume each chord's press, its \
                 release, and any repeat armed from it; delivering any of them makes every \
                 hotkey also type into whatever has the keyboard"
            )
            .into());
        }
    }
    println!("  ok: Super+F1 fired the manager's chord and reached no window");

    println!("\nxtask: input gate PASSED — an injected key and click reached userspace ✓");
    Ok(())
}

/// Read `<id> at <x>,<y> <w>x<h>` — the tail of `nxfiles`' menu-popup line.
fn parse_menu_popup(tail: &str) -> Option<(u32, i32, i32, u32, u32)> {
    let (id, rest) = tail.trim().split_once(" at ")?;
    let (origin, size) = rest.split_once(' ')?;
    let (x, y) = origin.split_once(',')?;
    let (w, h) = size.trim().split_once('x')?;
    Some((id.parse().ok()?, x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?))
}

/// Read `<id> of window <parent> at <x>,<y> <w>x<h>` — the tail of the shell's dialog placement.
///
/// A line of its own rather than the `placed window` one every other placement uses, because a
/// dialog is placed by a different rule: centred on its parent rather than stepped along the
/// cascade, so the line names the parent it was centred on.
fn parse_dialog_placement(tail: &str) -> Option<(u32, u32, i32, i32, u32, u32)> {
    let (id, rest) = tail.trim().split_once(" of window ")?;
    let (parent, rest) = rest.split_once(" at ")?;
    let (origin, size) = rest.split_once(' ')?;
    let (x, y) = origin.split_once(',')?;
    let (w, h) = size.trim().split_once('x')?;
    Some((
        id.parse().ok()?,
        parent.parse().ok()?,
        x.parse().ok()?,
        y.parse().ok()?,
        w.parse().ok()?,
        h.parse().ok()?,
    ))
}

/// Which slot of the taskbar `id` occupies, from the shell's own window-list line.
///
/// **Read rather than counted.** The bar shows the windows on the *current* desktop, in the
/// order the shell holds them, and a gate that kept its own tally would drift the first time a
/// window was closed or moved between desktops — which by this point in `check-login` has
/// happened several times. The line is `… [20:nxfiles] [21:notes.txt*]`, so the slot is the
/// position of the group whose id matches.
fn taskbar_slot(list: &str, id: u32) -> Option<usize> {
    list.split('[')
        .skip(1)
        .position(|group| group.split_once(':').is_some_and(|(n, _)| n.trim() == id.to_string()))
}

/// Type `text` into the applications modal, one character at a time, waiting for each.
///
/// **A receipt per character, because injection is relative and unacknowledged.** The modal's
/// filter had no line of its own until M12 Part A — the shell's source said the receipt was
/// "limited to renaming so the launcher's typing stays quiet" — so a burst of six keys followed
/// immediately by a click on a row was six chances to lose a keystroke and no way to tell.
/// The list would then be showing something else, the click would land on nothing, and the gate
/// would fail three steps later at whatever depended on the launch. It did, intermittently, in
/// CI and locally (PR #267).
///
/// The count is read and not asserted: what `/bin` holds is the image's business, and a gate
/// that pinned the number of matches would fail the day a program is added. What is asserted is
/// that the keystroke arrived at all.
fn type_into_modal(qmp: &mut Qmp, session: &mut Session, text: &str) -> R<()> {
    for c in text.chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect("desktop-shell: applications modal listing ")?;
    }
    Ok(())
}

/// Middle-click at `(x, y)`, having first walked the pointer there and checked it arrived.
///
/// **The position is verified with a left click before the middle one**, because the compositor
/// logs where a press landed and nothing else here can say where the pointer is. On a
/// window-list entry that left click is a *gesture in its own right* — it raises the window, or
/// minimises it if it already had the focus — so a caller must be able to live with either. The
/// close request that follows does not care which happened.
fn middle_click_at(qmp: &mut Qmp, session: &mut Session, x: i32, y: i32) -> R<()> {
    click_at(qmp, session, x, y)?;
    qmp.send_button("middle", true)?;
    qmp.send_button("middle", false)?;
    Ok(())
}

/// Every `lines N->M, E evicted` `nxterm` reported, one per resize it accepted.
fn transcript_reflows(transcript: &str) -> Vec<(u32, u32, u32)> {
    transcript
        .lines()
        .filter_map(|l| l.split(", lines ").nth(1))
        .filter_map(|tail| {
            let (counts, rest) = tail.trim().split_once(", ")?;
            let (a, b) = counts.split_once("->")?;
            let e: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            Some((a.parse().ok()?, b.parse().ok()?, e.parse().ok()?))
        })
        .collect()
}

/// The largest grid `nxterm` reported reaching, in cells, from `nxterm: resized to WxH, grid CxR`.
///
/// **The largest rather than the last**, because a terminal is resized more than once in a run
/// and the interesting one is the maximise. `None` if it never reported a resize at all, which
/// is what a client that declines every `Configure` looks like.
fn transcript_grid(transcript: &str) -> Option<(u32, u32)> {
    transcript
        .lines()
        .filter_map(|l| l.split(", grid ").nth(1))
        .filter_map(|tail| {
            let t: String = tail.trim().chars().take_while(|c| c.is_ascii_digit() || *c == 'x').collect();
            let (c, r) = t.split_once('x')?;
            Some((c.parse().ok()?, r.parse().ok()?))
        })
        .max()
}

/// Read `<x>,<y> <w>x<h>` — the tail of the shell's window-geometry line. Returns `(w, h)`.
fn parse_geometry(rest: &str) -> Option<(u32, u32)> {
    let mut it = rest.split_whitespace();
    let _origin = it.next()?;
    let (w, h) = it.next()?.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Read `<x>,<y> <w>x<h> of <sw>x<sh>` — the tail of the shell's work-area line.
///
/// Returns `(x, y, w, h, screen_w, screen_h)`.
fn parse_work_area(rest: &str) -> Option<(i32, i32, u32, u32, u32, u32)> {
    let mut it = rest.split_whitespace();
    let (x, y) = it.next()?.split_once(',')?;
    let (w, h) = it.next()?.split_once('x')?;
    if it.next()? != "of" {
        return None;
    }
    let (sw, sh) = it.next()?.split_once('x')?;
    Some((
        x.parse().ok()?,
        y.parse().ok()?,
        w.parse().ok()?,
        h.parse().ok()?,
        sw.parse().ok()?,
        sh.parse().ok()?,
    ))
}

/// Read `<id> at 0,<y>` — the tail of the shell's placement line. Returns `(id, y)`.
fn parse_placement(rest: &str) -> Option<(u32, i32, i32)> {
    let mut it = rest.split_whitespace();
    let id = it.next()?.parse().ok()?;
    if it.next()? != "at" {
        return None;
    }
    // **`x` as well as `y`, since M11 Part E batch 4.** It was discarded because the cascade
    // always started at the left edge, so every button this gate aims at could be measured from
    // the window's width alone. Insetting the cascade moved every one of them, and the gate went
    // on clicking as though the window began at zero — landing on minimise where it meant
    // maximise. A gate that assumes an origin is a gate that stops working when something moves.
    let (x, y) = it.next()?.split_once(',')?;
    Some((id, x.parse().ok()?, y.parse().ok()?))
}

/// Read `<id> at <x>,<y> <w>x<h>` — the tail of `nxterm`'s menu-popup line.
///
/// Hand-parsed for the same reason the QMP reply is: xtask carries no regex dependency, and the
/// shape here is fixed by the one line that emits it.
fn parse_popup_line(rest: &str) -> Option<(u32, i32, i32, u32, u32)> {
    let mut it = rest.split_whitespace();
    let id = it.next()?.parse().ok()?;
    if it.next()? != "at" {
        return None;
    }
    let (x, y) = it.next()?.split_once(',')?;
    let (w, h) = it.next()?.split_once('x')?;
    Some((id, x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?))
}

/// Read `desktop-shell: window N geometry X,Y WxH` — the shell's own report of where a window is.
///
/// **Asked rather than computed**, because the alternative is re-implementing the shell's
/// placement cascade in the harness and having it drift the first time the shell's policy
/// changes. The gate needs real coordinates to press on a row and to grab a title bar, and the
/// shell prints them for every window it places.
fn parse_geometry_line(rest: &str) -> Option<(u32, i32, i32, u32, u32)> {
    let mut it = rest.split_whitespace();
    let id = it.next()?.parse().ok()?;
    if it.next()? != "geometry" {
        return None;
    }
    let (x, y) = it.next()?.split_once(',')?;
    let (w, h) = it.next()?.split_once('x')?;
    Some((id, x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?))
}

/// Wait for the next window-geometry line and read it.
fn next_geometry(session: &mut Session) -> R<(u32, i32, i32, u32, u32)> {
    session.expect("desktop-shell: window ")?;
    let line = session.rest_of_line()?;
    parse_geometry_line(&line)
        .ok_or_else(|| format!("could not read a window geometry from {line:?}").into())
}

/// Pin the pointer to the bottom-right corner, then walk it to `(x, y)`.
///
/// **Relative injection, so the pointer must be somewhere known first.** A PS/2 packet carries a
/// 9-bit signed delta, so one huge motion is a different movement rather than a big one — the
/// steps are bounded, and the corner is reached by over-driving into the clamp.
fn move_pointer_to(qmp: &mut Qmp, x: i32, y: i32) -> R<()> {
    // **Pin only when the position is unknown.** The pin is twenty over-driven motions, and
    // repeating it before every click is what floods the guest's input ring.
    let from = match qmp.pointer {
        Some(p) => p,
        None => {
            for _ in 0..20 {
                qmp.send_motion(100, 100)?; // pin to (1279, 799)
            }
            // **Let the pin drain before walking, because the two are not equally forgiving.**
            // The pin is over-driven — twenty motions to cross thirteen hundred pixels — so a
            // packet it loses changes nothing. The walk is exact, and a packet it loses is a
            // permanent offset. Injected back to back they are one burst, and QEMU's PS/2 queue
            // is sixteen bytes that drops a *whole packet* which will not fit rather than
            // truncating it — so the burst arrives full and the walk is the half that pays.
            //
            // Seen in CI on 2026-08-31: `check-terminal` aimed at (397, 295) and the press
            // landed at (495, 351), exactly one step of (-98, -56) short. The same drain, for
            // the same reason, as the one `burst_holds_its_position` takes after its own pin.
            std::thread::sleep(std::time::Duration::from_millis(500));
            (1279, 799)
        }
    };
    let (mut dx, mut dy) = (x - from.0, y - from.1);
    while dx != 0 || dy != 0 {
        let sx = dx.clamp(-100, 100);
        let sy = dy.clamp(-100, 100);
        qmp.send_motion(sx, sy)?;
        dx -= sx;
        dy -= sy;
    }
    Ok(())
}

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
fn cmd_bench_compose(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Bench)?;
    let ovmf = locate_ovmf()?;

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel)?;
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
    let done = session
        .expect_within("compose-bench: done", std::time::Duration::from_secs(300))?;
    let transcript = session.finish();
    let path = build_cache().join("guest-transcript-bench-compose.log");
    let _ = fs::write(&path, &transcript);
    if !done {
        return Err(format!(
            "the bench did not finish; transcript at {}",
            path.display()
        )
        .into());
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
fn median(v: &mut Vec<u64>) -> u64 {
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

/// `cargo xtask check-login` — the **graphical login gate**: a wrong password, then a right
/// one, then a session.
///
/// The sequence `test-interactive` runs on serial, driven the way `check-input` and
/// `check-terminal` drive input, and adjudicated on the host. Built in Part D rather than at
/// the end of the milestone so Parts E and F land against a gate that already exists.
///
/// **The release image, not the test one, and that is not a preference.** In the test image
/// the greeter is bottom-most — `service-mgr` brings the login chain up before declared
/// services, which is what keeps `check-display`'s reference windows undisturbed — so it does
/// not hold the keyboard and nothing typed would reach it. In a release image it is the only
/// window. That also makes this the gate that proves the graphical arm exists for a person:
/// every other display gate boots `--selftest`.
///
/// **Paced on the greeter's redraw counter**, because a greeter has no echo. What was typed is
/// on screen and nowhere else, and this window repaints 420×200 per keystroke — the cost
/// `check-terminal` types one character at a time to stay behind. Waiting for the redraw is
/// the same discipline against a different receipt.
fn cmd_check_login(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-login.sock");

    println!("xtask: graphical login gate — booting the release image…\n");
    let (mut session, mut qmp) = spawn_release_guest(accel, "check-login", &qmp_sock)?;

    // 1. The greeter is up before anyone has authenticated. That is the claim Part D's second
    //    box makes, and in a release image nothing else has drawn anything.
    // The redraw is logged inside `present`, so it precedes the line that announces the
    // window — asserting them the other way round consumes the transcript past it.
    session.expect("desktop-session-mgr: greeter redraw 1")?;
    session.expect("desktop-session-mgr: greeter presented")?;

    // 2. **A wrong password is refused**, tested before the right one so a broken denial
    //    cannot hide behind a successful login — the same ordering `test-interactive` uses.
    type_at_greeter(&mut qmp, &mut session, DEMO_USER)?;
    press(&mut qmp, "tab")?;
    type_at_greeter(&mut qmp, &mut session, "wrong-password")?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-session-mgr: login denied")?;

    // 3. The right one reaches a session. Each line is a distinct claim: the oracle answered,
    //    the namespace was built **without a console**, and an unprivileged leader started in
    //    it.
    type_at_greeter(&mut qmp, &mut session, DEMO_USER)?;
    press(&mut qmp, "tab")?;
    type_at_greeter(&mut qmp, &mut session, DEMO_PASSWORD)?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-session-mgr: login ok -> home=/home/alice")?;
    session.expect("desktop-session-mgr: session namespace built (no /dev/console)")?;
    // **The leader's own line, and only it.** `libsession` logs "spawned … with its
    // environment" from the *parent* after the setup message goes out, while the child logs
    // this from its first instruction — so their order is a race between two processes, and
    // asserting one would be a flake that passes until it does not (PR #227 review). This is
    // the stronger claim anyway: the parent saying it spawned something is not evidence the
    // something ran. `test-interactive` already pins the `libsession` line for the serial
    // column, where it is ordered against a prompt rather than against another process.
    session.expect("desktop-shell: up (graphical session leader)")?;
    // **The theme, read before anything is drawn** — which is also where this assertion has to
    // sit: the shell reads it in `_start`, before its first bar exists, so an expectation placed
    // beside the *topic* rather than beside its position in the stream scans past it (M11
    // Part C). It comes from the user's own subtree rather than `/etc`, because a session
    // namespace binds `/home` and has no `/etc` — no new authority, and the file is somewhere a
    // person can actually delete.
    // **One line whichever way it went**, which is what lets the "delete the file" control run
    // against this gate rather than needing this step edited out (PR #263 review, finding 4).
    // What it takes from the line is the size the shell *resolved*; the client's own line is
    // compared against it below.
    session.expect("desktop-shell: theme ")?;
    let theme_line = session.rest_of_line()?;
    let shell_px = theme_line
        .rsplit_once("font_px ")
        .and_then(|(_, n)| n.trim().parse::<u32>().ok())
        .ok_or_else(|| format!("no font_px in the shell's theme line: {theme_line:?}"))?;
    println!("  ok: the shell resolved a theme ({})", theme_line.trim());

    // **The wallpaper, decoded in the guest** (M12 Part F). Immediately after the theme, because
    // that is where it is in the stream: the shell reads the theme, opens the picture it names,
    // and only then creates its first bar. An expectation placed beside the *topic* rather than
    // beside its position scans past output that was there — the same rule the theme's own line
    // above states, and the one M11 Part C learned the expensive way.
    //
    // **The decoded size is the half that discriminates.** `1920x1200` can only have come from
    // an `IHDR` the guest actually read — the staged picture is deliberately not the screen's
    // size, so a shell that decoded nothing cannot report it, exactly as `THEME_FONT_PX` is
    // deliberately not the built-in 16.
    //
    // **The drawn size no longer tells fit from fill**, and that changed when the shipped
    // wallpaper became a real photograph cropped to the screen's shape: 16:10 into 16:10 is
    // `1280x800 at 0,0` under either rule. It was `1280x720 at 0,40` while the fixture was a
    // 16:9 gradient, which discriminated — and cost 107 pixels of bare desktop down each side of
    // a real picture, which is not what a desktop should look like. What pins fit-versus-fill is
    // `libdraw::scale`'s own stretch control, where it belongs: it fails six tests in a second
    // rather than one gate in three minutes. See [`WALLPAPER_W`].
    //
    // A picture is pixels a release-image boot has no reference for, which is what the plan says
    // about this gate; the dimensions are what can be asserted, and they pin the whole chain —
    // the theme naming a file, `libfs` reading it, `libdraw::png` decoding it, and
    // `libdraw::scale::fit` placing it.
    // **And the line is printed after the window is committed**, which the first version of
    // this step was not: it asserted a line emitted as soon as the picture had been placed, so
    // it passed while `CreateWindow` failed and the desktop showed its bare ground colour. The
    // `window N` on the end is what makes this an assertion about a picture on screen rather
    // than about arithmetic.
    session.expect(&format!(
        "desktop-shell: wallpaper {WALLPAPER_W}x{WALLPAPER_H} drawn 1280x800 at 0,0 window "
    ))?;
    // **Kept, because stickiness is asserted against this id** further down: a press on an empty
    // desktop has to name *this* window, and `win=none` is what a wallpaper stamped with
    // desktop 1 gives.
    let wallpaper_line = session.rest_of_line()?;
    let wallpaper_id: u32 = wallpaper_line
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("no window id in the wallpaper line: {wallpaper_line:?}"))?;
    println!("  ok: the guest decoded a {WALLPAPER_W}x{WALLPAPER_H} PNG and put it on screen");
    // **The narrow bind, which is what closed the `manage-ungated` deferral.** The shell builds
    // an application namespace and checks it *before* launching into it: `/dev/draw/new`
    // resolves, `/dev/draw/manage` does not. Asserting the shell's own verdict rather than
    // re-deriving it here keeps the check where the refusal is — a shell that found the gate
    // open declines to launch, which is behaviour rather than a test.
    // **The endpoint is bound into application namespaces, not into the session's.** The shell
    // cannot check this by resolving it — a resolve is forwarded to whoever serves the path, so
    // asking the kernel for its own endpoint would block it waiting for its own answer. The
    // bind's success is what it can report; the `desktop` command below is what proves the
    // binding is reachable (M8 Part F).
    // **The clock read the wall clock** (M11 Part E batch 9), which is the one thing about it a
    // gate can see: its value changes every minute, and this boot has no reference render of a
    // bar to compare pixels against. What the line distinguishes is a clock absent because the
    // RTC was unreadable from a bar that failed to draw one — the *formatting* is a host test in
    // `libtime`, where it belongs.
    //
    // Immediately after the theme, because that is where it is: the shell reads both before it
    // draws anything, and an expectation placed beside its topic rather than its position in the
    // stream scans past output that was there.
    session.expect("desktop-shell: clock ")?;
    session.expect("desktop-shell: serving /dev/desktop")?;
    session.expect("desktop-shell: application /dev/desktop bound")?;
    session.expect("desktop-shell: application namespace grants new + /home, withholds manage")?;
    // **And it draws.** M7 Part E makes the shell a real compositor client: it resolves
    // `/dev/draw` from the namespace `desktop-session-mgr` built — not from a root one, which
    // it does not have — and presents a `panel` top bar. Asserting the window rather than only
    // the process is what distinguishes a session that runs from one that is merely spawned.
    session.expect("desktop-shell: top bar presented")?;
    // **The compositor's first real manager.** Everything M6 built for one -- placement,
    // restacking, the initial-configure hold -- has been exercised by a test client until now.
    // Holding the channel is also the other half of what closed `manage-ungated`: the shell's
    // session namespace binds the `/dev/draw` subtree and reaches `manage`, an application's
    // binds `/dev/draw/new` alone and does not.
    // **The bottom bar is created here, before the manager attaches** (M8 Part C). It has to be
    // *placed*, and only a manager can place — but a `panel` is held for the manager exactly
    // like a `normal` window, and `create` blocks until the first `Configure`. Creating it after
    // `manage()` parked the shell inside `create`, unable to drain the channel and so unable to
    // send the `Place` that would release it; only the 200 ms configure deadline broke the tie.
    session.expect("desktop-shell: bottom bar presented")?;
    session.expect("desktop-shell: manager channel held")?;

    // **The work area, and that it is not the screen** (M9 Part B). The shell asks the
    // compositor rather than subtracting its own bars, because any `panel`-role client declares
    // a strut and only the compositor sees them all. If this ever equals the screen, the struts
    // are not being counted and every maximised window will sit under the bars.
    session.expect("desktop-shell: work area ")?;
    let work_line = session.rest_of_line()?;
    let work = parse_work_area(&work_line)
        .ok_or_else(|| format!("could not read the work area from {work_line:?}"))?;
    if work.3 >= work.5 {
        return Err(format!(
            "the work area is as tall as the screen ({work_line}) — the shell's bars declare \
             struts, so a work area that ignores them would put every maximised window under \
             them"
        )
        .into());
    }
    println!("  ok: the work area is {}x{} of a {}x{} screen", work.2, work.3, work.4, work.5);

    // **And the position, asserted rather than inferred.** A dock edge reserves space; it does
    // not move the window. Without this the bar's placement was only covered by proxy — the
    // list click at (90, 788) landing on nothing — which says the bar is not *there* rather
    // than where it is (PR #242 review, optional 9).
    session.expect("desktop-shell: bottom bar placed at 0,776")?;
    session.expect("desktop-shell: /bin lists ")?;
    // After `/bin`, because the chord is registered on the loop's first pass and the programs
    // are read before the loop. `expect` scans forward, so asserting these out of order times
    // out on a line already behind the cursor.
    session.expect("desktop-shell: Super+H minimizes the focused window")?;

    // 4. **The applications modal.** `desktop-shell.md` §4 gives it two triggers — this button
    //    and the Super key — and only the button can exist yet: the Super key is a *global
    //    hotkey*, which §8 makes a capability the compositor does not have, and a `panel` takes
    //    no keyboard focus so a key would never reach the shell at all.
    //
    //    The bar spans the screen at y=0 and the button is its left 120px, so a press at
    //    (60, 12) lands inside it. Asserted through the compositor's own `press at` line
    //    first, for the reason `check-terminal` gives: it separates "the pointer was not
    //    there" from "the pointer was there and nothing happened".
    const APPS_CLICK: (i32, i32) = (60, 12);
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications modal open")?;

    // 5. **Type to filter, then launch.** The modal is a `popup`, so it holds the keyboard —
    //    the property `check-terminal` relies on when it says an open menu "is a topmost popup
    //    and takes the keyboard". The top bar could not receive these keys at all.
    //
    //    **`nxterm`, which is what makes the milestone visible**: a person types into the
    //    applications modal and a terminal opens. Part E launched a coreutil because the
    //    mechanism was the deliverable; Part F is the thing launched being worth looking at.
    type_into_modal(&mut qmp, &mut session, "nxterm")?;
    press(&mut qmp, "ret")?;
    // Each line is a distinct claim: the namespace was built and **checked** before anything
    // ran in it, and only then was the program spawned into it.
    session.expect("desktop-shell: application namespace grants new + /home, withholds manage")?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    // **Only the shell's own lines are ordered here.** `nxterm` starts concurrently with the
    // shell closing the modal, so an `expect` between the two is a race between processes —
    // the flake PR #227's review caught and PR #236's avoided. What `nxterm` did is checked
    // against the whole transcript below, where order does not matter.
    session.expect("desktop-shell: applications modal closed")?;
    // **And the shell placed it**, which is the manager half actually doing something rather
    // than merely holding a channel. Every window created while a manager is attached is
    // announced to it, and a `normal` one's first `Configure` is *held* until the manager acts
    // — so a shell that received `WindowCreated` and did nothing would leave launched
    // applications invisible, a failure that looks like they never started.
    //
    // **Asserted against the launched terminal, and after the close.** It sat after the modal
    // *opened* until M8 Part C, where the shell stopped placing its own windows — a modal is a
    // `popup`, placed by its creator and never held for anyone, so the line it matched was a
    // placement that could not have been load-bearing. The terminal's placement arrives one
    // loop iteration after the close, when its `WindowCreated` is drained.
    session.expect("desktop-shell: placed window ")?;
    // **Remembered, because the drag below needs to know where it started.** The line reads
    // `<id> at 0,<y>`; nothing moves this window between here and there — a desktop change and a
    // minimise leave the origin alone — so this is its origin at that point.
    let placed = session.rest_of_line()?;
    let (term_id, term_x, term_y) = parse_placement(&placed)
        .ok_or_else(|| format!("could not read the terminal's placement from {placed:?}"))?;
    // Its width, which the title bar's buttons are measured from — **before the window-list
    // line**, which follows it: an `expect` scans forward, so asserting the list first would
    // consume past the geometry this needs.
    session.expect(&format!("desktop-shell: window {term_id} geometry "))?;
    let geom_line = session.rest_of_line()?;
    let (term_w, term_h) = parse_geometry(&geom_line)
        .ok_or_else(|| format!("could not read the terminal's geometry from {geom_line:?}"))?;

    // And it is listed, focused, because it has the keyboard.
    session.expect("desktop-shell: window list on ")?;

    // Where a window-list entry sits: the first slot on the bottom bar.
    const LIST_CLICK: (i32, i32) = (90, 788);
    // How wide one is — `desktop-shell::ENTRY_W`, so slot `i`'s centre is `ENTRY_W * i + 90`.
    // Hardcoded like every other chrome metric this gate aims at, and for the same reason: a
    // gate that read the shell's layout to know where to click could agree with a shell that
    // had stopped drawing where it says (M11 decision 2).
    const ENTRY_W: i32 = 180;

    // 6a2. **The title bar's buttons ask, and the shell disposes** (M9 Part B). A client cannot
    //      minimise or maximise itself — both are manager operations — so the button sends
    //      `Surface::RequestState`, the compositor forwards it, and the shell decides.
    //
    // **The buttons are measured from the right edge in layout order** — minimise, maximise,
    // close, each `TITLE_BUTTON_W` (26) wide. Part C added close, which moved the other two a
    // slot left; the first run after that clicked *close* where it meant maximise, which is what
    // a coordinate constant hides and a changing layout exposes.
    let button_y = term_y + 13;
    // Measured from the window's **right edge**, which is its origin plus its width — not from
    // its width, which is the same number only while windows are placed at x=0.
    let right = term_x + term_w as i32;
    let maximise_at = (right - 39, button_y);
    let minimise_at = (right - 65, button_y);

    // **Maximise here is asserted as far as the shell's answer** — that the ask reached it and
    // that it answered with the *work area* rather than the screen. What the client does with
    // the size is 6g's, which is where the whole round trip is read back.
    click_at(&mut qmp, &mut session, maximise_at.0, maximise_at.1)?;
    session.expect("nxterm: asked the shell for window state 2")?;
    session.expect(&format!(
        "desktop-shell: maximize window {term_id} to {},{} {}x{}",
        work.0, work.1, work.2, work.3
    ))?;
    println!("  ok: maximise asked for the work area, not the screen");

    // **And put it back, which is a gesture in its own right and the only one that sends
    // `WINDOW_STATE_NORMAL`** (M9 Part D). The shell has had a restore path since Part B and
    // nothing could reach it: the button was one-way, which was invisible while the client
    // declined every `Configure` and is a window you cannot get back the moment it does not.
    //
    // **The button has moved**, because the window has: it is the work area now, so its
    // top-right corner is the work area's. A gate that clicked the old coordinates would land
    // on the title bar and start a drag — which is exactly what the first run of this did.
    session.expect(&format!("nxterm: resized to {}x{}, grid ", work.2, work.3))?;
    // **The committed geometry, not the client's report of what it was asked.** The resize line
    // is printed when the `Configure` arrives; the window is still its old size until the frame
    // after that is drawn and committed. Clicking on the strength of the first line lands
    // outside the window — `win=none` — which is what the first run of this step did.
    session.expect(&format!(
        "desktop-shell: window {term_id} geometry {},{} {}x{}",
        work.0, work.1, work.2, work.3
    ))?;
    click_at(&mut qmp, &mut session, work.0 + work.2 as i32 - 39, work.1 + 13)?;
    session.expect("nxterm: asked the shell for window state 0")?;
    session.expect(&format!("desktop-shell: restore window {term_id} to "))?;
    // The size is pinned by the client's own line rather than by the shell's: the shell prints
    // what it asked for, and what this step is about is the window going back to the shape it
    // had — which only the client can say it did.
    session.expect(&format!("nxterm: resized to {term_w}x{term_h}, grid "))?;
    println!("  ok: and the same button restored it to where it came from");

    // **And minimise, end to end**, because nothing in it depends on a client honouring
    // anything: the window leaves the screen and the bar marks it with `_`.
    click_at(&mut qmp, &mut session, minimise_at.0, minimise_at.1)?;
    session.expect("nxterm: asked the shell for window state 1")?;
    session.expect(&format!("desktop-shell: client asked to minimize window {term_id}"))?;
    // `_` is the bar's mark for a minimized window. The desktop count is not asserted: the
    // lifecycle rule appends an empty desktop as soon as one has a window, so it is 2 here and
    // says nothing about minimising.
    session.expect(&format!("desktop-shell: window list on desktop 1 of 2 [{term_id}:_ "))?;

    // Restore it from the list, which is where a minimized window comes back from, so the steps
    // below have a window to work with.
    click_at(&mut qmp, &mut session, LIST_CLICK.0, LIST_CLICK.1)?;
    session.expect("desktop-shell: raised window ")?;

    // **And minimise it a second time**, which is the step that was missing. The compositor drops
    // a request for the state a window last *asked* to be in, to keep a looping client off a
    // bounded queue — and the taskbar restore above is a `SetMinimized` from the *manager*, which
    // does not go through that path. With the shadow left stale, this second press was dropped as
    // a repeat, the client was told it had succeeded, and the button stayed dead until some other
    // state was asked for first (PR #249 review, blocking 1).
    click_at(&mut qmp, &mut session, minimise_at.0, minimise_at.1)?;
    session.expect("nxterm: asked the shell for window state 1")?;
    session.expect(&format!("desktop-shell: client asked to minimize window {term_id}"))?;
    println!("  ok: the minimise button still works after the taskbar restored the window");
    click_at(&mut qmp, &mut session, LIST_CLICK.0, LIST_CLICK.1)?;
    session.expect("desktop-shell: raised window ")?;

    // 6a3. **The taskbar asks, and the client is what closes** (M9 Part C). Middle-click is the
    //      gesture — the same one every taskbar this borrows from uses, and it needs no room in
    //      a layout that is one fixed slot per window. A window holds a process's work, so the
    //      shell *asks*: `Manage::Close` exists for a client that will not answer, and reaching
    //      for it first would destroy windows out from under processes that were fine.
    //
    //      **The control is the live client, and it is these two assertions rather than a
    //      separate run**: `nxterm` says it was asked and says it is closing, so the window went
    //      away by its own hand. A shell that destroyed it instead would produce neither line —
    //      and the window would be gone all the same, which is why the client's side is the only
    //      place the difference is visible.
    middle_click_at(&mut qmp, &mut session, LIST_CLICK.0, LIST_CLICK.1)?;
    // **Only the ordered half is an `expect`.** The compositor logs before it replies, so it
    // leads; the client then wakes and answers. The *shell's* own line comes after its request
    // returns, which is a race against the client it just woke — observed on both sides of the
    // client's two lines — so it is checked against the whole transcript below, where order does
    // not matter. Same rule as `nxterm`'s output in PR #227.
    session.expect(&format!("compositor: asked window {term_id} to close"))?;
    session.expect("nxterm: asked to close, exiting")?;
    session.expect("nxterm: closing")?;
    // The compositor tore the windows down with the session, and the list lost the entry.
    session.expect("desktop-shell: window list on desktop 1 of 1 (empty)")?;
    println!("  ok: the taskbar asked, and the client closed itself");
    let first_term_id = term_id;

    // Launch another, because everything below needs a terminal — and **by clicking the row this
    // time**, which is the second launch path and did not exist until M11 Part E batch 4. The
    // shell read pointer events for the overview, the applications button and the taskbar, and
    // never for the modal's own window: its rows could not be clicked at all, and nothing under
    // the cursor reacted. Everything after this step depends on the terminal, so a click that
    // silently does nothing fails the rest of the gate rather than passing quietly.
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications modal open")?;
    // Filtered to one row first, so the row being clicked is known without the gate having to
    // work out where `nxterm` sorts in the contents of `/bin`.
    type_into_modal(&mut qmp, &mut session, "nxterm")?;
    // **The modal hangs from the button now**, at (0, `BAR_H`), rather than covering the bar it
    // drops from — so the first row sits a field's height below the bar. `click_at` asserts the
    // press position before anything downstream is checked, which is what separates "the pointer
    // was not over the row" from "it was, and the click did nothing".
    const ROW1: (i32, i32) = (60, 64);
    click_at(&mut qmp, &mut session, ROW1.0, ROW1.1)?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    session.expect("desktop-shell: placed window ")?;
    let replaced = session.rest_of_line()?;
    let (term_id, term_x, term_y) = parse_placement(&replaced).ok_or_else(|| {
        format!("could not read the replacement terminal's placement from {replaced:?}")
    })?;
    session.expect(&format!("desktop-shell: window {term_id} geometry "))?;
    let regeom = session.rest_of_line()?;
    let (term_w, _term_h) = parse_geometry(&regeom)
        .ok_or_else(|| format!("could not read the replacement's geometry from {regeom:?}"))?;

    // 6. **And the top bar still works.** The modal used to be opened once and never closed,
    //    so it stayed on top of whatever was launched and the bar's click handler — gated on
    //    there being no modal — was inert for the rest of the session: no second launch, no
    //    way back. Clicking again is the direct test; asserting the close alone would be a
    //    proxy for it (PR #237 review, finding 6).
    // **Escape first, so this step's precondition is stated rather than assumed.** `click_at`
    // retries a press that did not land where it was aimed, and an abandoned attempt still
    // *pressed* somewhere — if that somewhere is the applications button (x < 120, and a
    // mis-walked pointer parks at x=0) the modal is already open, the shell ignores the aimed
    // click because it opens no second modal, and the assertion below waits for a line that
    // will never come. Escape with no modal open reaches the focused terminal and does nothing.
    press(&mut qmp, "esc")?;
    session.skip_to_end()?;
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications modal open")?;

    // 6b. **The window list, and the two things you can do to a window from it** (M8 Part C).
    //
    //     The launched terminal is a `normal` window, so it is listed — and it holds the
    //     keyboard, so the bar shows it focused. Clicking a focused entry puts the window away;
    //     clicking it again brings it back. This is the first thing in the shell that reflects
    //     compositor state continuously rather than at one moment, so the assertions are about
    //     what the list *says*, not only that a click was received.
    //
    //     The bar is the bottom 24 rows of an 800-high screen, and entries are 180px wide from
    //     the left — so (90, 788) is inside the first one.

    // Close the modal first: it is a popup on top, and a press meant for the bar would land in
    // it. **By clicking outside it rather than by pressing Escape** (M11 Part E batch 4) —
    // Escape is covered by the launch step above, and dismissal-on-outside-click is what did not
    // exist: this process never sees a press aimed at another window, so the modal stayed open
    // over whatever was clicked. The compositor's focus event is the one signal that says the
    // person went elsewhere, and it is what closes it now.
    //
    // **Onto bare desktop, which is the case that was broken.** Clicking another *window* raises
    // it, and a raise is a focus change the popup hears about — so the first version of this step
    // clicked into the terminal and passed while the reported bug survived: a press on the
    // desktop or on a panel raises nothing, changes no focus, and left the modal open over it.
    // The compositor sends `Surface::Dismissed` for that press now, which is the half a client
    // cannot see for itself.
    //
    // **The bottom bar's dead space**, between the last window-list entry and the desktop
    // indicator. A panel never takes focus, so a press there raises nothing and produces no focus
    // change *whatever else is on screen* — which is what makes it the honest test. Aiming at
    // bare desktop instead would depend on the terminal not being maximised at this point in the
    // gate, and it is.
    click_at(&mut qmp, &mut session, 600, 788)?;
    session.expect("desktop-shell: applications modal closed")?;
    // **That the compositor *said* so is checked against the whole transcript below**, not here.
    // The dismissal is logged while the press is being routed and the `press at` line is logged
    // when the routed record is delivered — so the dismissal comes *first*, and `click_at`'s own
    // position assertion has already scanned past it. Same rule this gate applies to every line
    // whose order is an implementation detail rather than a claim.


    click_at(&mut qmp, &mut session, LIST_CLICK.0, LIST_CLICK.1)?;
    session.expect("desktop-shell: minimized window ")?;
    // **The marker, not just a non-empty list.** The list still holds it — minimizing is not
    // closing, and a taskbar that dropped the entry would leave no way to get the window back —
    // and `_` is how the bar says so. Matching only `window list [` asserted the list existed
    // and nothing about what it shows (PR #242 review, optional 9).
    session.expect("desktop-shell: window list on ")?;
    session.expect(":_ nxterm")?;

    click_at(&mut qmp, &mut session, LIST_CLICK.0, LIST_CLICK.1)?;
    session.expect("desktop-shell: raised window ")?;
    // Restored and focused. Focus arrives one iteration after the raise, so this is the second
    // list line the click produces, not the first.
    // **And it is named**, which is the title arm of the list doing something: `nxterm` sets a
    // title, the compositor reports it on `WindowTitle`, and the bar shows it instead of
    // `window 6`. Nothing in the tree sent a title before this part.
    session.expect(":> nxterm")?;

    // And the chord, which is the half a taskbar alone does not cover: putting a window away
    // without reaching for its entry. `Super+H`.
    qmp.send_key("meta_l", true)?;
    qmp.send_key("h", true)?;
    qmp.send_key("h", false)?;
    qmp.send_key("meta_l", false)?;
    session.expect("desktop-shell: Super+H minimized window ")?;

    // 6c. **The lifecycle rule, which is Part D's whole claim** (M8 Part D).
    //
    //     Governing decision 3: an **unnamed** empty desktop is removed, a **named** one is
    //     kept, and the list always ends with one empty unnamed desktop to create into. The box
    //     asked for this by *closing* a window — which no gate can do inside a session, since
    //     the only way to close the launched terminal is through its shell and that draws into
    //     the grid, which renders under `test-harness` only (PR #242 review, optional 7). A
    //     desktop also empties when its last window is **moved away**, which is a gesture this
    //     part builds, so the rule is exercised that way instead.
    //
    //     Sequence: name this desktop, move the terminal off it, and show the desktop survived
    //     *because* it is named; then move the terminal back and show the desktop it vacated —
    //     unnamed — is gone.
    let chord = |qmp: &mut Qmp, shift: bool, code: &str| -> R<()> {
        qmp.send_key("meta_l", true)?;
        if shift {
            qmp.send_key("shift", true)?;
        }
        qmp.send_key(code, true)?;
        qmp.send_key(code, false)?;
        if shift {
            qmp.send_key("shift", false)?;
        }
        qmp.send_key("meta_l", false)?;
        Ok(())
    };

    // **Restore the terminal first: 6b left it minimized**, and a minimized window is not
    // focused — so `Super+Shift+N`, which moves *the focused window*, would correctly find
    // nothing to move and this block would assert against a gesture that did nothing. Clicking
    // its list entry restores and raises it, which is the gesture Part C added for exactly this.
    click_at(&mut qmp, &mut session, LIST_CLICK.0, LIST_CLICK.1)?;
    session.expect("desktop-shell: raised window ")?;
    session.expect(":> nxterm")?;

    // Name it. The prompt is the same popup the launcher uses — a `panel` takes no keyboard
    // focus, so the bar itself could never read a typed name.
    chord(&mut qmp, false, "r")?;
    session.expect("desktop-shell: naming this desktop")?;
    // **One character at a time, waiting for each**, the way `type_at_greeter` does. Injection
    // is relative and unacknowledged: a dropped batch ate the `r` and produced a desktop named
    // `wok`, which fails an assertion about naming while saying nothing about naming.
    let mut typed = String::new();
    for c in "work".chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(&mut qmp, &qcode)?;
        typed.push(c);
        session.expect(&format!("desktop-shell: name so far {typed}"))?;
    }
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: named this desktop work")?;
    session.expect("desktop-shell: window list on work of 2")?;

    // Move the terminal to the second desktop. `work` is now empty — and **named**, so it
    // stays; the desktop that received the window is no longer the scratch slot, so a new one
    // is appended. Two facts in one line: the bar still says `work`, and there are three.
    chord(&mut qmp, true, "2")?;
    session.expect("desktop-shell: moved window ")?;
    session.expect("desktop-shell: window list on work of 3 (empty)")?;

    // Follow it, to prove the list filters by desktop rather than merely being emptied.
    chord(&mut qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    session.expect(":> nxterm")?;

    // And back to `work`. The desktop just vacated is **unnamed** and empty, so it goes: the
    // count drops from three to two, which is the removal half of the rule.
    //
    // **The bar does not follow the window, and that is the point of reading the count.** A
    // move changes where a window is, not where you are — so the shell stays on the desktop it
    // was on, which has just been emptied. That desktop is now the trailing scratch slot, which
    // the rule keeps, so the reading is "desktop 2 of 2, empty": one desktop fewer than before,
    // and the one that went was the unnamed one.
    chord(&mut qmp, true, "1")?;
    session.expect("desktop-shell: moved window ")?;
    session.expect("desktop-shell: window list on desktop 2 of 2 (empty)")?;

    // 6d. **The overview** (M8 Part E): frozen thumbnails of this desktop, a sidebar of the
    //     others, and a window moved by dropping its thumbnail on one.
    //
    //     Opened from the indicator, which `desktop-shell.md` §7 always said it does — Part D
    //     made it advance to the next desktop only because there was no overview to open.
    //
    //     Bring the terminal back to this desktop first: `work` is empty after 6c, and an
    //     overview of nothing has no thumbnail to drag.
    chord(&mut qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    click_at(&mut qmp, &mut session, 1200, 788)?;
    // **The compositor's own line first, and it comes first in the guest too**: the shell
    // captures every visible window *before* it creates the overview to show them in. Asserted
    // rather than inferred, because an overview that opened with no thumbnails would satisfy
    // every assertion below about windows moving — the drag is by coordinate.
    session.expect("compositor: captured window ")?;
    // **And it wrote pixels.** The compositor logs a successful capture whether or not the
    // scale put anything in the buffer, and a black thumbnail is indistinguishable from a dark
    // window on a serial console — so the shell checks the buffer it owns and says what it
    // found. Without this the gate passed against a compositor that answered `Ok` and wrote
    // nothing at all.
    session.expect("desktop-shell: thumbnail of window ")?;
    session.expect("desktop-shell: overview open, window ")?;

    // The first thumbnail sits at (16, 40) and is 240x150 — see `thumb_rect`. Press inside it,
    // release over the second sidebar row, which is desktop 2.
    const THUMB: (i32, i32) = (100, 100);
    // `SIDE_ROW_H` is 72 since M11 Part E batch 10 — a miniature of the desktop plus its
    // padding — and this is the second place that number lives. Half a row down, so the aim is
    // clear of both edges.
    let side_row = |i: i32| (1180, 24 + i * 72 + 36);
    // **The drag starts from a position already verified — by the click that opened this.** A
    // drag cannot check its own start: there is no press receipt until the button goes down, and
    // by then it has begun. `click_at(1200, 788)` above asserted where it landed and left the
    // pointer there, and opening the overview does not move it, so the walk to the thumbnail is
    // the same arithmetic every other step here does.
    //
    // **This used to be a second `click_at`, and there is nowhere left to aim one.** It was on
    // the thumbnail while a pick-up-and-abandon changed nothing, then on empty background while
    // that did nothing either. As of 2026-08-26 a click on a thumbnail raises its window and a
    // click on the background dismisses — which is the point of those changes, and leaves a
    // verifying click with no inert place to land.
    move_pointer_to(&mut qmp, THUMB.0, THUMB.1)?;
    qmp.pointer = Some(THUMB);
    qmp.send_button("left", true)?;
    session.expect("desktop-shell: dragging window ")?;
    let (dx, dy) = side_row(1);
    move_pointer_to(&mut qmp, dx, dy)?;
    qmp.pointer = Some((dx, dy));
    qmp.send_button("left", false)?;
    session.expect("desktop-shell: dropped window ")?;
    session.expect("desktop-shell: overview closed")?;

    // And it really moved: `work` is empty again, and the window is on the desktop it was
    // dropped on. **`(empty)` rather than the bare prefix**, which matched a list still holding
    // the window just as happily as one that had lost it (PR #244 review, optional 6).
    session.expect("desktop-shell: window list on work of 3 (empty)")?;
    chord(&mut qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    session.expect(":> nxterm")?;

    // 6e. **`/dev/desktop` and its first consumer** (M8 Part F).
    //
    //     the `desktop-endpoint` deferral refused to bind an endpoint nothing resolved, because this
    //     milestone had three times shipped a capability that was specified, tested in isolation
    //     and unreachable on the path a caller uses. So the binding and something that reaches
    //     it land together — and *this* is the assertion that says so: a `/bin` command, run by
    //     the shell that `nxterm` spawned, resolving a path bound into a namespace
    //     `desktop-shell` constructed.
    //
    //     Typed at the terminal, which is the only way a program in this session is started.
    //     Its stdout goes into the terminal's grid, which renders under `test-harness` only —
    //     so the command also says what it did on the debug console, which is where every gate
    //     reads a release image.
    //
    //     The terminal has the keyboard: it was raised and focused above, and the overview is
    //     closed.
    // **Click inside the terminal first, which is the two-focus rule.** Raising it from the bar
    // gives the *window* the keyboard; the grid *widget* inside it is focused by `libui`'s
    // router, and that happens on a press. Without this the keys arrive at `nxterm` — verified,
    // 28 of them — and its router has nowhere to send them, so nothing reaches the shell and
    // the command simply never runs. `check-terminal` has always clicked into the terminal
    // before typing; this gate had not needed to until now.
    click_at(&mut qmp, &mut session, 200, 200)?;
    type_at_terminal(&mut qmp, "desktop")?;
    // **The shell answering, and the command reporting** — both halves, because either alone
    // is satisfiable without the other. A shell that served a list nobody received would print
    // the first; a command that invented an answer would print the second.
    // **Only the shell's own lines are ordered here.** The command runs in its own process, so
    // whether its output lands before or after the shell's next line is a race between two
    // processes — one run put `desktop: named` before the bar's redraw and the next put it
    // after. What the command said is checked against the whole transcript below, where order
    // does not matter; this is the same rule PR #227's review established for `nxterm`.
    session.expect("desktop-shell: served List of ")?;

    // And a mutation, which is the half a read-only op cannot prove: the command changes the
    // shell's model, and the shell's own bar says so.
    // **The desktop showing, which is the second one** — the drop moved the terminal there and
    // `Super+2` followed it. Naming the *first* would change a desktop the bar is not showing,
    // and the assertion below reads the bar.
    type_at_terminal(&mut qmp, "desktop name 2 cli")?;
    session.expect("desktop-shell: served Name cli")?;
    // The bar is the shell's own readout, so this is the model changing rather than a reply.
    session.expect("desktop-shell: window list on cli of ")?;

    // **And the chrome is still there.** Both bars are `panel`s created at startup, so the
    // compositor stamped them with the desktop that was current then — and `visible_on` is the
    // single predicate behind compositing, focus *and* hit-testing, so from the first switch
    // they were neither drawn nor clickable. The only way back to the applications button was a
    // chord (PR #243 review, blocking 1). The shell marks them sticky; this is what says so.
    //
    // Asserted by the compositor naming the window a press landed on: on another desktop a
    // non-sticky bar gives `win=none`, and no shell line follows because nothing was reached.
    //
    // **The indicator opens the overview since Part E**, which is what `desktop-shell.md` §7
    // always specified — Part D made it advance to the next desktop only because there was no
    // overview to open. Escape closes it again so the serial login below is not typing at a
    // popup that holds the keyboard.
    click_at(&mut qmp, &mut session, 1200, 788)?;
    // **Before the "overview open" line, because that is where it is** — `open_overview` reports
    // its ground and the caller announces the window afterwards. An expectation placed beside
    // its *topic* rather than beside its position in the stream scans past output that was
    // there, which is the rule this file states about the theme and re-learns here.
    //
    // **The overview keeps the desktop's picture** (reported from a real session, 2026-09-02).
    // It is a full-screen *opaque* window — a translucent one has been possible since M13 Part
    // B, but the overview is not one yet (Part C) — so it
    // does not sit over the desktop, it replaces it, and painting a flat colour made the
    // wallpaper disappear whenever you looked at the desktops. It is the wallpaper dimmed now,
    // and each sidebar miniature is the wallpaper scaled, because a preview showing flat blue
    // while the desktop behind it shows a photograph is a preview of something that does not
    // exist.
    //
    // **The line, not the pixels.** A release-image boot has no reference render to compare a
    // photograph against; what this pins is that the picture was kept, passed down and used.
    // `cargo xtask shot` is what shows the result, which is what that tool is for.
    session.expect("desktop-shell: overview ground is the wallpaper")?;
    println!("  ok: the overview kept the desktop's picture rather than covering it");
    session.expect("desktop-shell: overview open, window ")?;
    press(&mut qmp, "esc")?;
    session.expect("desktop-shell: overview closed")?;

    // **And so is the wallpaper, which it was not** (M12 Part F; PR #272 review, blocking 1).
    // It is created at startup like the bars, so the compositor stamped it with desktop 1 and
    // every other desktop showed the bare ground colour — visible as flicker rather than as a
    // missing feature, because switching back restored it.
    //
    // **Asserted on an empty desktop**, which is what makes the press unambiguous: `Super+3`
    // has nothing on it, so every point that is not one of the two bars is the wallpaper or is
    // nothing at all. A press in the middle names the wallpaper's window when it is sticky and
    // gives `win=none` when it is not — the same discriminator the bars use above, and the same
    // reason: the shell's own "is sticky" line says it *asked*, and the compositor's says it
    // took.
    chord(&mut qmp, false, "3")?;
    session.expect("desktop-shell: switched to ")?;
    // **Empty is asserted, not assumed** — the discriminator only works where nothing else is
    // under the cursor, and which desktop holds what has moved several times by this point.
    session.expect("(empty)")?;
    // **`click_at` already consumed the press line**, up to the coordinates — so what is left
    // to read is the `win=` it ends with, and an `expect` for the whole line would scan past
    // its own evidence. (It did, on this step's first run.)
    click_at(&mut qmp, &mut session, 640, 400)?;
    let hit = session.rest_of_line()?;
    if !hit.contains(&format!("win={wallpaper_id}")) {
        return Err(format!(
            "a press on an empty desktop reported '{}' — the wallpaper (window {wallpaper_id}) \
             is not sticky, so every desktop but the first shows the bare ground colour",
            hit.trim()
        )
        .into());
    }
    println!("  ok: the wallpaper is sticky — still there on an empty desktop");
    // Back to the one the rest of this gate is written against.
    chord(&mut qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;

    // 6f. **The overview's clicks, which were dead until 2026-08-26.** `desktop-shell.md` §6
    //     says "you can switch desktops from inside it"; a press on a sidebar row set no drag
    //     and its release matched no arm, and a press-and-release on a thumbnail was discarded
    //     as an abandoned drag. So the two obvious gestures — go to that desktop, go to that
    //     window — did nothing, and only the *drag* the gate above exercises was ever wired up.
    //     Reported from a real session, which is the part worth keeping: the drag was gated and
    //     the click was not, and a gate that drives only the gesture it was written for cannot
    //     tell the difference between "unimplemented" and "untested".
    click_at(&mut qmp, &mut session, 1200, 788)?;
    session.expect("desktop-shell: overview open, window ")?;

    // The chord path first: an overview left showing the desktop you just switched away from is
    // showing thumbnails of windows that are no longer there. It follows instead of closing,
    // which is what §6 means by "it fetches a different set of images".
    chord(&mut qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    session.expect("desktop-shell: overview now showing 0 on work")?;

    // Then the sidebar click, with no drag in flight. Row 1 is the second desktop — `cli`,
    // which is where the terminal is — so the refresh must find it again.
    // `SIDE_ROW_H` is 72 since M11 Part E batch 10 — a miniature of the desktop plus its
    // padding — and this is the second place that number lives. Half a row down, so the aim is
    // clear of both edges.
    let side_row = |i: i32| (1180, 24 + i * 72 + 36);
    let (sx, sy) = side_row(1);
    click_at(&mut qmp, &mut session, sx, sy)?;
    session.expect("desktop-shell: switched to cli")?;
    session.expect("desktop-shell: overview now showing 1 on cli")?;

    // **Clicking the row you are already on dismisses**, which is the way out of an overview on
    // a desktop with no windows — where clicking a window, the other way out, does not exist.
    // Reported as being stuck there with Escape the only escape, and Escape is not discoverable.
    click_at(&mut qmp, &mut session, sx, sy)?;
    session.expect("desktop-shell: overview closed")?;

    // **And a click on its background dismisses**, the way clicking outside a menu does. That
    // also makes the indicator a toggle: the overview covers the bar, so a second click where
    // the indicator is lands on background.
    click_at(&mut qmp, &mut session, 1200, 788)?;
    session.expect("desktop-shell: overview open, window ")?;
    click_at(&mut qmp, &mut session, 600, 700)?;
    session.expect("desktop-shell: overview closed")?;

    // And a click on a thumbnail activates its window, which is the third way out and the one
    // that takes you somewhere. `raise_window` is the same call the window list's entries make.
    click_at(&mut qmp, &mut session, 1200, 788)?;
    session.expect("desktop-shell: overview open, window ")?;
    click_at(&mut qmp, &mut session, 100, 100)?;
    session.expect("desktop-shell: overview raised window ")?;
    session.expect("desktop-shell: overview closed")?;

    // 6g. **A window is dragged by its own title bar** (M9 Part A). Client-side decorations mean
    //     the bar is pixels `nxterm` committed; what crosses the wire is one `StartMove`, and the
    //     compositor — which is holding the grab the press opened — moves the window from there.
    //
    //     **The motion is injected before the request can arrive — probabilistically, and worth
    //     saying so.** The two motions are *sent* before the wait below, but nothing proves the
    //     compositor processed them before the client's request: they travel QMP → PS/2 →
    //     `input-server` while the request travels compositor → client → compositor, and the
    //     ordering is a race this gate wins by a wide margin rather than by construction. If
    //     that ever stops being true the step still passes for the correct implementation and
    //     stops distinguishing the late-measurement one — so the host test
    //     `an_interactive_move_offsets_by_where_the_press_landed…` is the guard that cannot
    //     drift, and this is the one that proves the whole path (PR #248 review, finding 9).
    //
    //     **The motion is injected before the request can arrive, and that is the assertion.**
    //     `StartMove` is a full round trip after the press: the compositor delivers it, `libui`
    //     routes it, `nxterm` decides it landed on the bar, and only then does the request go
    //     out. A compositor that took its drag offset from the pointer *at the request* would
    //     lose whatever the pointer did in between — the window jumps by that much and then
    //     tracks correctly, which is exactly the defect `TODO(scroll-grab)` describes. Every
    //     other step here waits for the guest between injections; this one deliberately does not,
    //     because a stationary pointer across that round trip measures zero drift where a person
    //     sees forty pixels (PR #247 review, finding 4).
    const DRAG_STEPS: i32 = 4;
    const DRAG_DX: i32 = 10;
    const DRAG_DY: i32 = 5;
    // The title bar is the top 26 px of the window; x=100 is clear of the buttons at its right.
    let press_at = (100, term_y + 13);
    move_pointer_to(&mut qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    // **Half the motion before the request can possibly arrive**, which is the half a
    // compositor reading the pointer at `StartMove` would lose.
    for _ in 0..DRAG_STEPS / 2 {
        qmp.send_motion(DRAG_DX, DRAG_DY)?;
    }
    // **And the button stays down until the drag is accepted.** A person holds it for a
    // fraction of a second; this harness can press and release faster than the round trip to
    // the client and back, and a release that lands first takes the grab away — the compositor
    // then refuses a move for a window nobody is holding, which is correct and is not what this
    // step is testing (found the expensive way: the first version released immediately).
    session.expect("compositor: interactive move of window ")?;
    for _ in 0..DRAG_STEPS / 2 {
        qmp.send_motion(DRAG_DX, DRAG_DY)?;
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((press_at.0 + DRAG_STEPS * DRAG_DX, press_at.1 + DRAG_STEPS * DRAG_DY));
    // The window ended up offset by exactly what was injected. One line for the gesture:
    // the compositor reports no geometry change per motion, deliberately.
    // **From where it started, not from zero.** A drag moves a window by what was injected, and
    // the destination is its origin plus that — which was the same number only while the cascade
    // placed every window at x=0 (M11 Part E batch 4).
    session.expect(&format!(
        "desktop-shell: window {term_id} geometry {},{} ",
        term_x + DRAG_STEPS * DRAG_DX,
        term_y + DRAG_STEPS * DRAG_DY
    ))?;
    println!("  ok: the terminal moved with its own title bar, offset by where it was grabbed");

    // **And maximising it now *moves* it, which is the half a `Configure` was not applying.**
    // The window is at (40, …) after the drag, so a maximise that only carried a size would
    // resize it in place and leave it hanging off the screen. The compositor applies the origin
    // as `Place` would (PR #249 review, blocking 2).
    //
    // **The size is asserted here too, as of M9 Part D** — this is where Part B's box was left
    // open, deliberately, because the size stays the client's to decline and `nxterm` declined
    // every `Configure` for three milestones. Now it accepts, and the assertion is the whole
    // round trip: the button asks, the shell decides, the compositor forwards, the client
    // reallocates and *commits* at the new size, and the geometry the shell reads back is the
    // committed one. A client that still declined would produce every line but the last.
    //
    // The button is at the window's top-right, and the window has moved: its origin is where it
    // was placed plus what the drag injected.
    let moved_x = term_x + DRAG_STEPS * DRAG_DX;
    let moved_y = term_y + DRAG_STEPS * DRAG_DY;
    click_at(&mut qmp, &mut session, moved_x + term_w as i32 - 39, moved_y + 13)?;
    session.expect("nxterm: asked the shell for window state 2")?;
    session.expect(&format!(
        "desktop-shell: maximize window {term_id} to {},{} {}x{}",
        work.0, work.1, work.2, work.3
    ))?;
    // **Two producers, one cause, so no order between them.** The shell logs the window's new
    // origin when the compositor's geometry event reaches it; the client logs the size it took
    // when the `Configure` reaches it. Both are downstream of the same apply and neither is
    // downstream of the other, so on four vCPUs either can reach the console first. Asserted as
    // a sequence this passed for two milestones and then failed in CI with both lines present
    // and the *first* expect having scanned past the second (2026-08-31).
    //
    // The client's line says what it did with the size *and* what that came to in cells, which
    // is the difference between a window that grew and a terminal that can use the room: a grid
    // still 80x24 in a 1280x752 window would satisfy every other line here.
    let moved = format!("desktop-shell: window {term_id} geometry {},{} ", work.0, work.1);
    let took = format!("nxterm: resized to {}x{}, grid ", work.2, work.3);
    session.expect_all(&[&moved, &took])?;
    // And the committed geometry — the one the compositor reports and `/dev/draw/<id>/info`
    // answers with — is the work area exactly, not the work area rounded down to whole cells.
    session.expect(&format!(
        "desktop-shell: window {term_id} geometry {},{} {}x{}",
        work.0, work.1, work.2, work.3
    ))?;
    println!("  ok: maximise moved the window and the client committed the work area exactly");


    // 6j. **The window is dragged smaller by its own corner** (M9 Part E). The grip is pixels
    //     `nxterm` committed, like the title bar; what crosses the wire is one `StartResize`,
    //     and then nothing at all until the button comes up. The compositor moves an outline —
    //     its own drawing, over the composed stack, reaching no client — and hands the shell one
    //     rectangle at the release.
    //
    //     **The window is maximised here, so its corner is the work area's**, and dragging
    //     inward is what keeps every injected motion inside the screen: a drag that ran into the
    //     clamp would assert against a rectangle the pointer never reached.
    //
    //     **Four assertions, in the order the mechanism goes**: the compositor took the gesture
    //     and says which edges; it reports one rectangle when the button comes up; the *shell*
    //     turns that into a `Configure`, which is the half that proves the compositor did not
    //     resize anything itself; and the client commits it. The last is the one the plan calls
    //     for — the control is an outline that tracks and a release that commits nothing, and
    //     every line before the last holds for that implementation too.
    const RESIZE_STEPS: i32 = 4;
    const RESIZE_DX: i32 = -50;
    const RESIZE_DY: i32 = -25;
    let resized_w = (work.2 as i32 + RESIZE_STEPS * RESIZE_DX) as u32;
    let resized_h = (work.3 as i32 + RESIZE_STEPS * RESIZE_DY) as u32;
    // The grip is a 16-pixel square over the window's bottom-right corner; its middle is eight
    // pixels in from each edge.
    let grip_at = (work.0 + work.2 as i32 - 8, work.1 + work.3 as i32 - 8);
    move_pointer_to(&mut qmp, grip_at.0, grip_at.1)?;
    qmp.pointer = Some(grip_at);
    qmp.send_button("left", true)?;
    // `RESIZE_RIGHT | RESIZE_BOTTOM` is `2 | 8`.
    session.expect(&format!("compositor: interactive resize of window {term_id} edges 10"))?;
    for _ in 0..RESIZE_STEPS {
        qmp.send_motion(RESIZE_DX, RESIZE_DY)?;
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((
        grip_at.0 + RESIZE_STEPS * RESIZE_DX,
        grip_at.1 + RESIZE_STEPS * RESIZE_DY,
    ));
    session.expect(&format!(
        "compositor: interactive resize of window {term_id} ended at {},{} {resized_w}x{resized_h}",
        work.0, work.1
    ))?;
    // **The shell is what resizes the client**, which is the whole reason the gesture ends in an
    // event rather than in a `Configure` from the compositor: one path to a window's geometry
    // rather than two that can disagree. It says `drop` rather than `resize` because since
    // Part F one event ends both gestures, so the shell has one word for what it is answering —
    // the user let go, and this is the rectangle.
    session.expect(&format!(
        "desktop-shell: drop window {term_id} to {},{} {resized_w}x{resized_h}",
        work.0, work.1
    ))?;
    session.expect(&format!("nxterm: resized to {resized_w}x{resized_h}, grid "))?;
    // And the committed geometry, which is the assertion a release that commits nothing fails.
    session.expect(&format!(
        "desktop-shell: window {term_id} geometry {},{} {resized_w}x{resized_h}",
        work.0, work.1
    ))?;
    println!("  ok: the terminal was dragged smaller by its corner, and committed it");

    // 6k. **A window thrown at the left edge snaps to half the work area** (M9 Part F). The
    //     shell registered eight zones — four edges, four corners — computed from the work area,
    //     and the compositor matches the pointer against that table during a move. It knows
    //     nothing about halves: what it shows and what it asks for is the *target* rectangle the
    //     table gave it, which is why the policy can be wrong only in the shell.
    //
    //     **The assertion is against the work area, not the screen.** A zone table computed from
    //     `screen_h` would put the window under the bars, and the committed geometry is the only
    //     line that says which of the two the shell used.
    //
    //     The window is `resized_w x resized_h` at the work area's origin after 6j, so its title
    //     bar is at `work.1 + 13` and clear of the buttons at `x = 100`.
    // **First the control, as a step of its own with a positive assertion**: a drag that passes
    // *through* a zone and is released outside it must snap nothing — and what it must do
    // instead is the ordinary move, so the assertion is the geometry that move produces. A
    // compositor that snapped on entry rather than on release would report the zone's target
    // here, and this line is what catches it. Asserting the absence of a snap would not: the
    // step after this one produces exactly that line, and an `expect` scans forward.
    let press_at = (100, work.1 + 13);
    move_pointer_to(&mut qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    session.expect("compositor: interactive move of window ")?;
    // **The expectation is summed from what is injected**, not written out beside it: the first
    // version divided each leg into three and wrote the leg's total, which is two pixels away
    // from three times a truncated third.
    let mut walked = (0, 0);
    for (dx, dy) in [(-30, 60), (-30, 60), (-30, 60), (66, 0), (66, 0), (66, 0)] {
        qmp.send_motion(dx, dy)?;
        walked = (walked.0 + dx, walked.1 + dy);
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    qmp.send_button("left", false)?;
    let passed_to = (press_at.0 + walked.0, press_at.1 + walked.1);
    qmp.pointer = Some(passed_to);
    session.expect(&format!(
        "desktop-shell: window {term_id} geometry {},{} {}x{}",
        work.0 + walked.0,
        work.1 + walked.1,
        resized_w,
        resized_h
    ))?;
    println!("  ok: a drag through the zone and out again moved the window and snapped nothing");

    // Then the drop itself, from the title bar where the window now is.
    //
    // **The press is asserted to land on *that* window**, which is what makes the step above a
    // control rather than a hope: a compositor that snapped on entry has moved the terminal to
    // the left half, so this press lands on nothing — and the failure says so here, one line
    // after the assertion it belongs to, instead of surfacing as a missing drag further down.
    let press_at = (passed_to.0 + 90, passed_to.1);
    move_pointer_to(&mut qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    session.expect(&format!(
        "compositor: press at x={} y={} win={term_id}",
        press_at.0, press_at.1
    ))?;
    // **On its title bar specifically**, which is the sharpest statement of where the window is:
    // a terminal that had snapped on the pass-through above sits at the work area's origin, so
    // this press lands on its *grid* — inside the window, and no drag at all. Without this line
    // the control's failure surfaces two steps later as a drag that never began.
    session.expect("nxterm: dragging its own title bar")?;
    session.expect("compositor: interactive move of window ")?;
    // **Paced, and the reason is worth keeping.** The first version injected thirty-six motions
    // as fast as QMP would take them; the consumer ring overran, `input-server` announced the
    // gap, and `libinput` turned it into a `Logical::Dropped` — which ends a gesture *without*
    // asking for anything, exactly as Part E's review required. So the gate lost its drag to
    // the machinery working correctly. A person dragging a window produces nothing like this
    // rate; a harness does, and it has to slow down rather than be accommodated.
    // **Down as well as left**, because the *pointer* picks the zone and the title bar sits
    // inside the top-left **corner**'s band — the window is at the work area's origin, so a
    // straight drag left snaps a quarter, correctly, and would say nothing about edges. The
    // corners are registered first precisely so the more specific zone wins, which is the
    // manager's ordering to get right; this step is about the edge, so it aims at one.
    let step = -(press_at.0 - 10) / 6;
    for _ in 0..6 {
        qmp.send_motion(step, 30)?;
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((press_at.0 + step * 6, press_at.1 + 180));
    // The compositor asks for the zone's target; the shell answers with the `Configure`; the
    // client commits it. Half the **work area**, at its origin.
    session.expect(&format!(
        "desktop-shell: drop window {term_id} to {},{} {}x{}",
        work.0,
        work.1,
        work.2 / 2,
        work.3
    ))?;
    session.expect(&format!("nxterm: resized to {}x{}, grid ", work.2 / 2, work.3))?;
    session.expect(&format!(
        "desktop-shell: window {term_id} geometry {},{} {}x{}",
        work.0,
        work.1,
        work.2 / 2,
        work.3
    ))?;
    println!("  ok: dropped at the left edge, the terminal took half the work area");

    // 6h. **Movement is not lost while the compositor is busy** — the one property the whole
    //     input path exists to keep, and the one nothing here checked.
    //
    //     A relative pointer's deltas *are* its position: a batch the input server cannot
    //     deliver is movement no consumer can ever re-derive, unlike a key or a button, whose
    //     state `SYN_DROPPED` asks the consumer to resynchronise. Dropping one therefore leaves
    //     the guest's cursor permanently offset from the host's pointer — which is what a
    //     person sees as "I cannot reach the left edge any more", because the host pointer
    //     leaves the window before the guest's cursor arrives (2026-08-26).
    //
    //     Injected **against a desktop switch**, which is the one repaint that legitimately
    //     costs the whole screen — ~100 ms under TCG, during which the compositor reads no input
    //     and the consumer's ring fills.
    //
    //     **Under TCG only, and that is why this is not the guard.** The overrun here depends on
    //     out-injecting a repaint, and under KVM the repaint is fast enough that 120 QMP-paced
    //     motions never fill the ring — so this step passes whether or not motion is deferred,
    //     which is what it did when it *was* the guard (PR #246 review, blocking 1). CI runs
    //     `--kvm` everywhere. The property is gated in `check-input`, where the stall is a
    //     consumer that stops reading and therefore means the same thing at any speed; what
    //     this step still buys is the whole path — a real manager, a real desktop switch, a
    //     real cursor — on the accelerator a person actually runs locally.
    burst_holds_its_position(&mut qmp, &mut session, &chord)?;

    // 6i. **The close button is the client's own, and sends nothing** (M9 Part C). Everything
    //      else on this bar asks the shell; this one exits. The window **is** the work area
    //      after 6g — origin *and* size, since M9 Part D — so its top-right corner is the work
    //      area's, and that is what the button is measured from.
    //
    //      **After 6h, not before it**, which costs a desktop switch back: 6h's stall is a
    //      whole-screen recompose, and a recompose with no window left in it is a weaker one
    //      than the step's own rationale describes. Closing the terminal first made the local
    //      diagnostic quieter for nothing (PR #251 review).
    //
    //      **Asserted by what is absent as well as what is present.** `nxterm: closing` says it
    //      went; the transcript check below says no `RequestClose` was sent for *this* window,
    //      which is what makes it the button rather than the taskbar. The two terminals have
    //      different ids precisely so that check can name one of them.
    //
    //      **Measured from the snapped width**: 6j dragged the window smaller by its corner and
    //      6k then dropped it at the left edge, so its right edge is the work area's midpoint.
    chord(&mut qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    click_at(&mut qmp, &mut session, work.0 + (work.2 / 2) as i32 - 13, work.1 + 13)?;
    session.expect("nxterm: closing")?;
    session.expect("desktop-shell: window list on ")?;
    println!("  ok: the close button closed the terminal with no request to the shell");

    // 4. **Two independent sessions**, which is Part D's fourth box and the one most easily
    //    asserted rather than tested. The graphical session is running *now* — its leader
    //    blocks, so it does not end on its own — and the serial column's prompt has been live
    //    on this same boot the whole time. Logging in there proves the two supervisors do not
    //    contend: neither arbitrates, there is no registry, and serial stays the recovery
    //    path by construction rather than by care.
    //
    //    The same user, twice, with two namespaces. That cost is named and accepted in
    //    `graphical-session.md` §6.2 — it matches Linux, where `getty` and `gdm` do not
    //    coordinate either.
    session.send(DEMO_USER)?;
    session.expect("password:")?;
    session.send(DEMO_PASSWORD)?;
    session.expect("/home>")?;

    // 7. **The file browser, and the two sessions used as one fact** (M10 Part B). The serial
    //    shell just logged in above, and it sees the *same* `/home` subtree the graphical
    //    session does — `libsession::build_namespace` and `desktop-shell::build_app_namespace`
    //    bind it identically, which is the assumption M10's decision 1 rests on. So the serial
    //    side can *make* the thing the graphical side is then asserted to see, which is a
    //    stronger gate than a fixture: nothing here is arranged by the harness.
    session.send("mkdir ./papers")?;
    session.expect("/home>")?;

    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications modal open")?;
    type_into_modal(&mut qmp, &mut session, "nxfiles")?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: launched nxfiles into its own namespace")?;
    // **The theme reached the application** (M11 Part C): a value that travelled from a file on
    // disk, through one reader in the shell, onto the setup record every launch already carries,
    // and into a window. It is asserted here because it is the first thing the client says — it
    // reads what the session told it before it reads a directory.
    // **The same number at both ends**, which is what says the theme crossed the wire rather than
    // each end reaching for its own default.
    session.expect(&format!("nxfiles: theme font_px {shell_px}"))?;
    // **And that it is the staged one**, which is what gates the *file*. `THEME_FONT_PX` is not
    // the built-in size, so a shell that stopped putting the theme on the setup record — or a
    // client that stopped reading it — reports 16 and fails here. **This is the one line the
    // "delete the theme file" control removes**, because that control is about the run where no
    // file exists.
    if shell_px != u32::from(THEME_FONT_PX) {
        return Err(format!(
            "the session resolved font_px {shell_px}, but the image staged {THEME_FONT_PX} — \
             the shell did not read the file the build wrote"
        )
        .into());
    }
    // **It starts at `HOME`**, which is the binding the shell gave it and not a path compiled
    // in. The count is read rather than asserted: what is in a user's home is the image's
    // business, and a gate that pinned it would fail the first time anything else wrote there.
    session.expect("nxfiles: listed /home - ")?;
    let listed = session.rest_of_line()?;
    println!("  ok: nxfiles started at HOME and listed it ({})", listed.trim());


    // **Where the shell put it**, read after the listing rather than before it: a client lists
    // its directory *then* creates its window, so the shell's geometry line comes second — and
    // asking for it first consumed the listing this step asserts. The gate presses on a row
    // later, and a row's position is the window's origin plus chrome; re-deriving the shell's
    // placement cascade here would be a second copy of a policy that is the shell's to change.
    let files_win = next_geometry(&mut session)?;

    // **Enter descends into the selected row**, which is the directory the serial side just
    // made: directories sort before files and a fresh listing selects row 0, so this is the
    // keyboard reaching the same message a row press produces. If `/home` ever holds a
    // directory sorting before `papers`, this fails naming the path it did open — which is a
    // loud failure rather than a quiet one.
    press(&mut qmp, "ret")?;
    session.expect("nxfiles: listed /home/papers - 0 entries")?;
    println!("  ok: Enter descended, and an empty directory lists nothing");

    // And Backspace leaves it again. Two navigations, both from the keyboard, because a
    // listing a person cannot drive without aiming at it is a listing they have to aim at.
    press(&mut qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;
    println!("  ok: Backspace went back up");

    // 8. **The editor, opened by the browser, and read back by the shell** (M10 Part D). The
    //    same two-session fact as step 7, used the other way round: there the serial side made
    //    something the graphical side had to see; here the graphical side writes something the
    //    serial side has to read. Nothing in between is arranged by the harness.
    //
    //    **The file is made empty and inside `papers`**, which is what makes the listing
    //    deterministic: `/home` holds whatever the image put there, but `papers` was created by
    //    step 7 and holds exactly this.
    session.send("touch ./papers/notes.txt")?;
    session.expect("/home>")?;

    press(&mut qmp, "ret")?;
    session.expect("nxfiles: listed /home/papers - 1 entries")?;

    // **Enter on a file row asks the shell to open it**, which is Part B's box, moved here
    // because it needed something to launch. The browser holds no authority to spawn anything —
    // it names a path over `/dev/desktop` and the shell decides what opens it.
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: launched nxedit into its own namespace")?;
    // **Two processes reacting to one launch, so no order between them**: the browser learns
    // its request was taken when the shell's reply arrives, and the editor reads the file when
    // the kernel gets round to running it.
    let asked = String::from("nxfiles: asked to open /home/papers/notes.txt");
    let opened = String::from("nxedit: opened /home/papers/notes.txt - 0 bytes");
    session.expect_all(&[&asked, &opened])?;
    // After the open, for the reason the browser's is read after its listing: the editor reads
    // its file before it has a window to place.
    let edit_win = next_geometry(&mut session)?;
    println!("  ok: a file row launched the editor on the file it names");

    // **A receipt per character, and it is a count rather than an echo.** An editor's echo is
    // its own window, and this gate boots a release image — there are no rendered glyphs to
    // read on serial. Injection is relative and unacknowledged, so without a per-key wait a
    // dropped PS/2 batch would show up as a *wrong file* two steps later.
    const TYPED: &str = "nitrox";
    for (i, c) in TYPED.chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(&mut qmp, &qcode)?;
        session.expect(&format!("nxedit: buffer rev {}", i + 1))?;
    }
    println!("  ok: {} keystrokes reached the editor's buffer", TYPED.len());

    // Ctrl+S, and the editor says what it wrote. Seven bytes: six typed and the newline the
    // editor adds, because a last line without one reads as one line shorter than it looks.
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 7 bytes")?;

    // **And the assertion is made from outside the editor.** Asking the editor what it saved
    // would be asking the accused: it would answer from the buffer it holds, which is the thing
    // in doubt. The serial session reads the file with `nxsh`'s `open` — there is no `cat`; a
    // path is a stream — and what the shell prints is what is on disk.
    session.send("open ./papers/notes.txt")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: the shell read back what the editor saved, from outside it");

    // 8b. **Undo, redo, and find** (M12 Part C). The grouping is the decision this part makes,
    //     and the gate asserts it the only way that cannot be argued with: **by byte count, from
    //     outside**. Two characters typed together are one group, so one undo takes both — seven
    //     bytes on disk rather than eight — and a redo brings both back to nine. An
    //     implementation that grouped per *keystroke* would leave **eight** here; nine is what an
    //     undo that took nothing leaves; and one that grouped per *save* would leave seven after
    //     the redo as well. (This paragraph said nine for the per-keystroke case, twice, two
    //     lines after getting it right — PR #269 review, worth fixing 4.)
    for c in "ab".chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(&mut qmp, &qcode)?;
        session.expect("nxedit: buffer rev ")?;
    }

    // Ctrl+Z. The revision moves on an undo, which is what makes an undone buffer read as
    // modified — the editor has to ask before closing something that is no longer the file.
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "z")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    // Seven: `nitrox` and the newline the editor adds. Eight would mean the undo took one
    // character rather than the group; nine would mean it took nothing.
    session.expect("nxedit: saved /home/papers/notes.txt - 7 bytes")?;
    session.send("open ./papers/notes.txt")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: one undo took the whole group, and the shell read back the shorter file");

    // Ctrl+Y brings it back, and the file grows by exactly what the undo removed.
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "y")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 9 bytes")?;
    session.send("open ./papers/notes.txt")?;
    session.expect(&format!("{TYPED}ab"))?;
    session.expect("/home>")?;
    println!("  ok: and redo put it back, byte for byte");

    // **Find is the second thing that wants the keys** — the shape the save-as field
    // established, which is why it is the same field. What is typed goes to it and not to the
    // buffer, and the receipt is where the match landed rather than what was looked for.
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "f")?;
    qmp.send_key("ctrl", false)?;
    for (i, c) in "tro".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(&mut qmp, &qcode)?;
        session.expect(&format!("nxedit: find so far {} chars", i + 1))?;
    }
    press(&mut qmp, "ret")?;
    session.expect("nxedit: find hit at line 0")?;
    // Escape ends the mode. **Not asserted here, and this comment used to say it was** — nothing
    // types into this editor again: step 9 drags its title bar with the pointer, and the click
    // after that hands the keyboard to the browser, so a find field left open would change
    // nothing any later step can see (PR #269 review, worth fixing 3). What covers it is
    // `nxedit`'s own `ctrl_f_opens_a_find_field_and_enter_walks_the_matches`, which fails when
    // `Esc` stops closing a find field. The press is kept because leaving the editor in a mode
    // the rest of the run does not expect is worse than a keystroke nobody checks.
    press(&mut qmp, "esc")?;
    println!("  ok: find took the keys, matched, and gave them back");

    // 9. **A file dragged from the browser onto the editor** (M10 Part E) — the part every
    //    application above exists to make honest: two real windows, one payload, and no test
    //    client on either end.
    //
    //    **The editor is snapped to the right half first**, because the shell cascades new
    //    windows and the editor is sitting on top of the browser it was launched from. Dropping
    //    a window at the screen's right edge is M9 Part F's own gesture, so the geometry the
    //    drag then uses is the work area's half rather than a number this gate invented.
    let (edit_id, ex, ey, ew, _eh) = edit_win;
    let title_grab = (ex + ew as i32 / 2, ey + 13);
    move_pointer_to(&mut qmp, title_grab.0, title_grab.1)?;
    qmp.pointer = Some(title_grab);
    qmp.send_button("left", true)?;
    for _ in 0..6 {
        qmp.send_motion(120, 0)?;
    }
    session.expect("compositor: interactive move of window ")?;
    // To the right edge, which is where the shell registered a snap zone.
    for _ in 0..6 {
        qmp.send_motion(120, 0)?;
    }
    qmp.send_button("left", false)?;
    qmp.pointer = None;
    session.expect(&format!(
        "desktop-shell: drop window {edit_id} to {},{} {}x{}",
        work.0 + (work.2 / 2) as i32,
        work.1,
        work.2 / 2,
        work.3
    ))?;
    session.expect(&format!("nxedit: resized to {}x{}", work.2 / 2, work.3))?;
    println!("  ok: the editor snapped to the right half, clear of the browser");

    // A second file, so the drag carries something the editor is not already showing — dropping
    // the open file is deliberately a no-op, and a gate that did it would assert nothing.
    session.send("touch ./papers/other.txt")?;
    session.expect("/home>")?;

    // **Click the browser to give it the keyboard**, then walk it out and back in so it lists
    // the file the serial side just made. A listing is read when something navigates; nothing
    // polls, deliberately.
    let (_, fx, fy, _fw, _fh) = files_win;
    // **On the path strip, not the title bar.** A press on a title bar is a `StartMove` — the
    // click would work and the window would not move, but the gate would be asserting against a
    // gesture it did not mean to make. The path text carries no handler and still raises the
    // window, because click-to-focus is the compositor's and not the toolkit's.
    // **On the path strip**, which is now three strips down: a title bar, the menus, and the
    // tabs. `fy + 38` used to be the path and is the *menu bar* since M12 Parts B and D — it
    // still raised the window, because click-to-focus is the compositor's and the bar's own
    // background carries no handler, so this went on passing while meaning something else.
    click_at(
        &mut qmp,
        &mut session,
        fx + 120,
        fy + 1 + TITLE_BAR_H + MENU_BAR_H + TAB_STRIP_H + PATH_H / 2,
    )?;
    press(&mut qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;
    press(&mut qmp, "ret")?;
    session.expect("nxfiles: listed /home/papers - 2 entries")?;

    // Row 1 is `other.txt`: the listing sorts directories first and then by name, and `notes`
    // sorts before `other`. The row's y is the window's origin plus its chrome — the title bar
    // and the path strip — plus half a row.
    const TITLE_BAR_H: i32 = 26;
    const PATH_H: i32 = 24;
    const ROW_H: i32 = 20;
    // **And the menu bar above the path strip** (M12 Part B), which moved every row down by its
    // height. `nxfiles::list_top` is the browser's own version of this sum; a gate that had
    // missed the change would press one row high and drag the wrong file.
    const MENU_BAR_H: i32 = 24;
    let row1 =
        (fx + 120, fy + TITLE_BAR_H + MENU_BAR_H + TAB_STRIP_H + PATH_H + ROW_H + ROW_H / 2);
    move_pointer_to(&mut qmp, row1.0, row1.1)?;
    qmp.pointer = Some(row1);
    qmp.send_button("left", true)?;
    // **Past the slop, then across.** The browser turns a press into a drag once it has
    // travelled, which is what keeps a click a click.
    //
    // **The slop counts toward where the pointer is**, and it did not until M11 Part E batch 2b
    // found out. Injection is relative: these six motions move the guest's pointer 600 across
    // and 240 down, and the walk below started its arithmetic from `row1` as though they had
    // not happened — so every step was 600 too far right, and the pointer ended clamped against
    // the screen's right edge instead of at `onto`.
    //
    // It passed for a month because the editor's text area reached the window's last pixel
    // column, so a drop at the extreme edge landed on it anyway. Giving the window a frame moved
    // the content in by four pixels and the drop started landing on the frame — a real gate bug,
    // surfaced by a change that had nothing to do with it.
    let mut at = row1;
    for _ in 0..6 {
        qmp.send_motion(100, 40)?;
        at = (at.0 + 100, at.1 + 40);
    }
    session.expect("nxfiles: dragging other.txt")?;
    session.expect("compositor: drag from window ")?;
    // Into the editor's document area — below its title bar and status strip, and well inside
    // the half of the screen it now occupies.
    let onto = (work.0 + work.2 as i32 * 3 / 4, work.1 + work.3 as i32 / 2);
    while at != onto {
        let step = (
            (onto.0 - at.0).clamp(-100, 100),
            (onto.1 - at.1).clamp(-100, 100),
        );
        qmp.send_motion(step.0, step.1)?;
        at = (at.0 + step.0, at.1 + step.1);
    }
    // **Letting go is what delivers it.** The highlight the compositor draws while the pointer
    // is over a window that takes the payload is pixels rather than a line — its logic is
    // host-tested in `compositor::input`, including the two cases that must show *nothing*.
    qmp.send_button("left", false)?;
    qmp.pointer = Some(onto);
    session.expect(&format!("compositor: drop win={edit_id} on=document"))?;
    session.expect("nxedit: drop of other.txt on the document")?;
    session.expect("nxedit: opened /home/papers/other.txt - 0 bytes")?;
    println!("  ok: a file dragged from the browser opened in the editor");

    // 9c. **Two buffers in one window, switched, and the right one saved** (M12 Part D). The drop
    //     above opened a *tab* rather than replacing the buffer — which is what removed the old
    //     refusal, since there is nothing to lose by taking a file when it arrives beside the one
    //     already open. So the editor is now showing `other.txt` with `notes.txt` a tab away.
    session.expect("nxedit: tab ")?;
    let showing = session.rest_of_line()?;
    if !showing.contains("other.txt") {
        return Err(format!("the drop should have made other.txt current; the editor says {showing:?}").into());
    }

    // **The tabs are where the fixed width says they are**, which is what a fixed width buys: a
    // tab does not move when another opens. `libui::widget::TAB_W` and `TAB_STRIP_H` are the
    // source, pinned to a real tree by `a_tab_selects_where_it_is_and_its_close_box_does_not_
    // select_it` — this gate cannot link the crate, so it hardcodes them as it does every other
    // chrome metric.
    const TAB_W: i32 = 120;
    // **A tab strip in both applications** (M12 Part D): above the editor's text and below the
    // browser's menus, which moved every one of the browser's rows down again.
    // `nxfiles::list_top` is that application's own version of the same sum.
    const TAB_STRIP_H: i32 = 24;
    // The editor is the work area's right half after the snap above.
    let ed = (work.0 + (work.2 / 2) as i32, work.1);
    // Tab `i`'s label area: 4 is `WINDOW_CONTENT_X`, `TAB_W` per tab, and 40 into the label —
    // clear of the close box, whose centre is at `TAB_CLOSE_CX` (110).
    let tab = |i: i32| (ed.0 + 4 + TAB_W * i + 40, ed.1 + 1 + TITLE_BAR_H + TAB_STRIP_H / 2);
    let tab0 = tab(0);
    click_at(&mut qmp, &mut session, tab0.0, tab0.1)?;
    session.expect("nxedit: tab ")?;
    let switched = session.rest_of_line()?;
    if !switched.contains("notes.txt") {
        return Err(format!("clicking the first tab should show notes.txt; got {switched:?}").into());
    }
    println!("  ok: the editor holds two buffers, and a tab click switched between them");

    // **`End` first, and that is not tidying.** Step 8b's search left its match *selected* —
    // which is what a find is supposed to do — and typing replaces a selection, so a bare
    // keystroke here would have edited the middle of the file rather than appending to it. It
    // did, on the first run: `tro` became `c` and the save wrote seven bytes. The behaviour is
    // right and the gate's assumption was not.
    press(&mut qmp, "end")?;
    // **And the save goes to the tab that is current, not the one opened last.** That is the
    // whole risk of tabs in an editor, and the assertion is made from outside: the serial shell
    // reads the file back.
    press(&mut qmp, "c")?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 10 bytes")?;
    session.send("open ./papers/notes.txt")?;
    session.expect(&format!("{TYPED}abc"))?;
    session.expect("/home>")?;
    // The other tab's file is untouched: a save that went to the wrong buffer would have
    // written nine bytes here instead of leaving it empty.
    session.send("list ./papers")?;
    session.expect("/home>")?;
    println!("  ok: and the save reached the current tab's file, read back by the shell");

    // 9d. **Copy in the editor, paste in the terminal, and the shell reads what was copied**
    //     (M12 Part E) — one gesture crossing two applications and a server, which is the whole
    //     point of making the clipboard a resource rather than a slot in a widget.
    //
    //     **Asserted through the filesystem, not through a log line.** `nxterm` in a *release*
    //     image does not report its grid — deliberately, since a terminal narrating itself to
    //     the kernel log undoes the point of the tty server owning output — so "the shell
    //     printed it" cannot be read off the transcript. What can be is the file the pasted text
    //     names: the serial column lists a directory and the name is there or it is not. That is
    //     the same two-session trick steps 7, 8 and 9b use, and it is stronger than a log line,
    //     because a paste that delivered the wrong bytes produces a differently-named file
    //     rather than a matching count.
    //
    //     The copy is a **find**, which selects its match — so this needs no keystroke that
    //     changes the buffer, and steps 11 and 12 find the editor exactly as step 9c left it.
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "f")?;
    qmp.send_key("ctrl", false)?;
    for (i, c) in TYPED.chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(&mut qmp, &qcode)?;
        session.expect(&format!("nxedit: find so far {} chars", i + 1))?;
    }
    press(&mut qmp, "ret")?;
    session.expect("nxedit: find hit at line 0")?;
    // **Escape first, and that is the rule Part D's review settled rather than tidying.** A
    // chord that acts on the *buffer* stays the field's while a field is open, and a copy is
    // one — so `Ctrl+C` here would have gone to the find field and done nothing. (It did, on
    // this step's first run.) Escape closes the field and leaves the match selected: the
    // selection is the buffer's, not the field's.
    press(&mut qmp, "esc")?;
    // Ctrl+C. **A count, never the text** — an editor's buffer is a person's document, and the
    // serial console is a log file. The count is exactly `TYPED`'s length, which is what says
    // the *selection* was copied rather than the line or the buffer.
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "c")?;
    qmp.send_key("ctrl", false)?;
    session.expect(&format!("nxedit: copied {} bytes", TYPED.len()))?;
    println!("  ok: the editor copied its selection onto the ring");

    // A terminal to paste into. **Launched here rather than reusing step 6's**, which was
    // closed long before the editor existed — and closed again at the end of this step, so the
    // windows steps 11 and 12 drive are the ones they expect to find.
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications modal open")?;
    type_into_modal(&mut qmp, &mut session, "nxterm")?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    session.expect("desktop-shell: applications modal closed")?;

    // Click into the new terminal's grid to give it the keyboard. **Its origin comes off the
    // shell's own placement line** — the cascade moves, and a gate that assumed an origin is
    // the bug M11 Part E batch 4 fixed. Its *size* cannot: the shell logs at most
    // `MAX_LOGGED_GEOMETRY` geometry lines per session and this run passed that long ago, which
    // is why step 11 hardcodes the editor's size too. A point a little inside the top-left is
    // in the grid whatever the size, and needs no second number.
    session.expect("desktop-shell: placed window ")?;
    let placed = session.rest_of_line()?;
    let (_, tx, ty) = parse_placement(&placed)
        .ok_or_else(|| format!("could not read the terminal's placement from {placed:?}"))?;
    // Below the title bar and the terminal's own menu bar, and well inside the frame.
    click_at(&mut qmp, &mut session, tx + 100, ty + TITLE_BAR_H + MENU_BAR_H + 40)?;

    // `touch ./` — typed — then the paste, then a suffix. **The suffix is what makes the
    // assertion discriminating**: `nitrox.clip` can only exist if the paste delivered exactly
    // `nitrox` *and* landed at the cursor with typing continuing after it. A paste that
    // delivered nothing leaves `.clip`; one that delivered the whole line leaves something
    // else again.
    // QMP's own key names, not the characters: `.` is `dot` and `/` is `slash`.
    for qcode in ["t", "o", "u", "c", "h", "spc", "dot", "slash"] {
        press(&mut qmp, qcode)?;
    }
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(&mut qmp, "v")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    session.expect(&format!("nxterm: pasted {} bytes", TYPED.len()))?;
    for qcode in ["dot", "c", "l", "i", "p"] {
        press(&mut qmp, qcode)?;
    }
    press(&mut qmp, "ret")?;

    // **The serial column is what says it happened.** Asking the terminal would be asking the
    // accused: its grid is where the pasted text was drawn, and the thing in doubt is whether
    // the bytes were real.
    //
    // **Retried, because the two sides are genuinely concurrent.** Nothing orders the terminal's
    // `touch` against the serial `list` — two shells in two sessions, and the only thing between
    // them is the filesystem. The first version sent one `list` and passed by luck; M12 Part F's
    // wallpaper added a PNG decode to the shell's startup, the timing moved, and it started
    // failing. This is `expect_within`'s stated case — absence that is retryable rather than a
    // verdict — and it is still a verdict at the end of the loop: a `touch` that never ran fails
    // the gate after thirty seconds rather than passing.
    let mut made = false;
    for _ in 0..10 {
        session.send("list .")?;
        if session
            .expect_within(&format!("{TYPED}.clip"), std::time::Duration::from_secs(3))?
        {
            made = true;
            break;
        }
        // Discard the failed listing, or the next attempt matches this one's output.
        session.skip_to_end()?;
    }
    if !made {
        return Err(format!(
            "the terminal pasted {} bytes but no {TYPED}.clip appeared in /home: the shell in \
             the terminal did not run the command the paste completed",
            TYPED.len()
        )
        .into());
    }
    session.expect("/home>")?;
    println!("  ok: the paste reached the shell in the terminal, and it made {TYPED}.clip");

    // Close the terminal from inside, so steps 11 and 12 drive the windows step 9 left.
    for qcode in ["e", "x", "i", "t"] {
        press(&mut qmp, qcode)?;
    }
    press(&mut qmp, "ret")?;
    session.expect("nxterm: the terminal ended")?;

    // 9b. **The browser renames a file, and the shell reads the new name back** (M12 Part B).
    //     The same two-session fact steps 7 and 8 use, a third time: the graphical side does
    //     something to the filesystem and the *serial* side is what says it happened. Asking the
    //     browser to re-list would be asking the accused — it would answer from the listing it
    //     holds, which is the thing in doubt.
    //
    //     The browser is showing `/home/papers` and a fresh listing selects row 0, which is
    //     `notes.txt` — directories sort first and then by name, and this directory has none.
    //     Nothing has moved the selection since: a *press* on a row starts a drag and only a
    //     click activates one, which is what step 9 relied on too.
    //
    //     **Clicking the bar item, not a keyboard route**, because a menu that cannot be opened
    //     by pointing at it is a menu nobody will find. The item sits at the content's left
    //     edge, one bar below the title.
    let file_menu = (fx + 20, fy + 1 + TITLE_BAR_H + MENU_BAR_H / 2);
    click_at(&mut qmp, &mut session, file_menu.0, file_menu.1)?;
    session.expect("nxfiles: menu popup ")?;
    let popup = session.rest_of_line()?;
    let (_, px, py, _pw, ph) = parse_menu_popup(&popup)
        .ok_or_else(|| format!("could not read the menu's placement from {popup:?}"))?;

    // **The row height is divided out of the popup rather than derived from the theme.** A menu
    // row is text plus padding, so its height follows `font_px` — which a theme file sets, and
    // which M11's decision 2 keeps *out* of the metrics a gate is allowed to assume. Four rows,
    // and the frame is a one-pixel border with two of padding on each side.
    const MENU_FRAME: i32 = 3;
    const FILE_MENU_ROWS: i32 = 4;
    let row_h = (ph as i32 - 2 * MENU_FRAME) / FILE_MENU_ROWS;
    // `rename` is the third row: new file, new folder, rename, delete.
    let rename_at = (px + 20, py + MENU_FRAME + row_h * 2 + row_h / 2);
    click_at(&mut qmp, &mut session, rename_at.0, rename_at.1)?;

    // **A receipt per character**, the discipline every typed sequence in this gate follows —
    // and `nxfiles` grew the line to make it possible, for the reason the launcher's filter did
    // one part earlier: an unacknowledged burst is a dropped keystroke discovered as a wrong
    // filename several steps later.
    for (i, c) in "renamed".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(&mut qmp, &qcode)?;
        session.expect(&format!("nxfiles: name so far {} chars", i + 1))?;
    }
    press(&mut qmp, "ret")?;
    session.expect("nxfiles: renamed /home/papers/renamed")?;
    session.expect("nxfiles: listed /home/papers - 2 entries")?;
    println!("  ok: the browser's File menu renamed a file");

    // **And the serial session reads it back**, which is what makes this a rename rather than a
    // browser that redrew a label. The content is step 8's, so a rename that had copied or
    // truncated shows up here as well.
    session.send("open ./papers/renamed")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: and the shell found the new name, with the old contents");

    // 10. **An editor launched from the menu, which is a launch with no file** (M11 Part E
    //     batch 7). `nxedit` required `argv[1]` and the applications modal passes none, so it
    //     printed "no file to edit" and exited — reported as "nxedit doesn't launch from the
    //     menu", and true in the most literal way. It opens untitled now and asks for a name when
    //     there is something to save.
    press(&mut qmp, "esc")?;
    session.skip_to_end()?;
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications modal open")?;
    type_into_modal(&mut qmp, &mut session, "nxedit")?;
    // **Enter, not a click on the row.** Clicking a row is proved above, where the terminal every
    // later step depends on is launched that way; what this step is about is what happens *after*
    // a launch that carries no file, so it takes the shortest route to one.
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: launched nxedit into its own namespace")?;
    // **The placement is the proof**, not the launch: the shell said the same thing before this
    // change, and the editor then exited before it ever created a window. A window that gets
    // placed is a window that exists.
    session.expect("desktop-shell: placed window ")?;
    let untitled = session.rest_of_line()?;
    let (untitled_id, untitled_x, untitled_y) = parse_placement(&untitled)
        .ok_or_else(|| format!("could not read the untitled editor's placement from {untitled:?}"))?;
    println!("  ok: the editor launched from the menu and stayed up (window {untitled_id})");

    // **Wait for it to reach the screen before typing at it.** "Placed" says the shell put the
    // window somewhere, not that the compositor has moved the keyboard to it — the same race that
    // sent a launch's keystrokes into the wrong program in batch 4. Settling the screen is the
    // property that actually has to hold.
    let _ = settle_and_capture(&mut qmp, &build_cache().join("check-login.ppm"))?;

    // Ctrl+S with nothing named asks for a name rather than writing one nobody chose.
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    // Then the name, one key at a time — and **waiting for each**, which the comment here
    // claimed and the code did not do (M12 Part A). `nxedit` had no receipt for this field:
    // naming a buffer is not editing it, so `buffer rev` never moves, and seven unacknowledged
    // keystrokes were seven chances to end up asserting against a file called `scrath`.
    for (i, c) in "scratch".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(&mut qmp, &qcode)?;
        session.expect(&format!("nxedit: name so far {} chars", i + 1))?;
    }
    press(&mut qmp, "ret")?;
    // **`/home`, which *is* the user's subtree from inside the session.** A session namespace
    // binds the user's directory at `/home` — that is what scopes it — so an untitled buffer
    // named here lands in the place the session owns, and a path mentioning `alice` would be
    // this gate asserting the host's view of a name the guest cannot see. An empty buffer is
    // zero bytes: the newline the editor adds is per line, and there are none.
    session.expect("nxedit: saved /home/scratch - 0 bytes")?;
    println!("  ok: an untitled buffer was named and written into the session's home");

    // 11. **A confirmation dialog, driven to both answers** (M12 Part A). `Role::Dialog` has
    //     existed since M2 Part A and no program a person runs had ever created one: the editor
    //     answered every `CloseRequested` by exiting, and its own source said so — "an editor
    //     with somewhere to put a question would ask it, and this one has no dialog to ask in".
    //
    //     **The dialog's geometry is hardcoded here**, the way this gate already hardcodes a
    //     title bar's height and a list's row height, because it cannot link the crate that
    //     defines them. `libui::widget::DIALOG_LEFT_CX` and its siblings are the source — they
    //     were `nxedit`'s until M12 Part B moved them down beside `dialog_frame`, when a second
    //     confirmation would otherwise have given this gate two tables to keep in step — and
    //     `libui::widget::tests::dialog_buttons_land_where_the_constants_say` is the host test
    //     that pins these numbers to a tree that is actually built, so a change to `DIALOG_PAD`
    //     fails there, beside the change, rather than here after a three-minute boot.
    //
    //     (This paragraph named two symbols that same part deleted — PR #268 review, worth
    //     fixing 2 — which is the drift the move was made to prevent, in the one place a
    //     compiler cannot see.)
    const CONFIRM_W: i32 = 340;
    const CONFIRM_H: i32 = 132;
    const CONFIRM_DISCARD_CX: i32 = 91;
    const CONFIRM_KEEP_CX: i32 = 249;
    const CONFIRM_BUTTON_CY: i32 = 103;
    // **And the editor's own size, `nxedit::START_SIZE`, because the geometry line cannot be
    // read this late.** The shell logs at most `MAX_LOGGED_GEOMETRY` of them per session and
    // this run passed that long ago — a bound that exists because the event is client-driven,
    // and a gate is not a reason to raise it. This window was launched from the menu and nothing
    // has resized it, so its size is the one the editor starts at.
    const EDITOR_W: i32 = 560;
    const EDITOR_H: i32 = 420;

    // One keystroke is all it takes to have something to lose. The editor was saved a moment
    // ago, so this is the difference between the buffer and the file and nothing else.
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;

    // **Its own close button, which used to be the end of the process.** The rightmost of the
    // three title-bar controls, measured from the window's right edge — the same arithmetic 6a2
    // uses, and the same one 6i used on `nxterm`, whose close button really does just exit.
    let untitled_right = untitled_x + EDITOR_W;
    let close_at = (untitled_right - 13, untitled_y + 13);
    click_at(&mut qmp, &mut session, close_at.0, close_at.1)?;
    // **The editor's line comes first, and it is ordered by construction rather than by luck.**
    // It is printed *before* the dialog is asked for, because a dialog's first `Configure` is
    // held for the manager — so a line printed after `Child::open` returned would be downstream
    // of the shell, and the two processes would be racing to the console. They were, and the
    // winner depended on the accelerator: TCG gave the shell both lines and KVM gave the client
    // the second one, which is how this passed twice locally and failed in CI (PR #267).
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let placed_dialog = session.rest_of_line()?;
    let (_, parent_id, dx, dy, dw, dh) = parse_dialog_placement(&placed_dialog)
        .ok_or_else(|| format!("could not read a dialog placement from {placed_dialog:?}"))?;
    if parent_id != untitled_id {
        return Err(format!(
            "the editor asked about window {untitled_id}, and the shell placed a dialog on \
             window {parent_id}"
        )
        .into());
    }
    if (dw as i32, dh as i32) != (CONFIRM_W, CONFIRM_H) {
        return Err(format!(
            "the dialog is {dw}x{dh}, and this gate aims at a {CONFIRM_W}x{CONFIRM_H} one \
             — the four constants above came from `nxedit`'s published geometry and have \
             drifted from it"
        )
        .into());
    }
    // **The centring is re-derived here, and that is the assertion rather than a second copy of
    // a policy.** M10 Part E's rule — read the shell's geometry lines, do not recompute the
    // cascade — is about placements this gate has no opinion on. This one it does: "centred on
    // its parent, kept inside the work area" is the claim M12 Part A makes, and reading the
    // number back without checking it would assert only that some number was printed.
    let want_x = (untitled_x + (EDITOR_W - CONFIRM_W) / 2)
        .clamp(work.0, work.0 + work.2 as i32 - CONFIRM_W);
    let want_y = (untitled_y + (EDITOR_H - CONFIRM_H) / 2)
        .clamp(work.1, work.1 + work.3 as i32 - CONFIRM_H);
    if (dx, dy) != (want_x, want_y) {
        return Err(format!(
            "the dialog landed at {dx},{dy}; centred on window {untitled_id} at \
             {untitled_x},{untitled_y} {EDITOR_W}x{EDITOR_H} and clamped to the work area \
             it belongs at {want_x},{want_y}"
        )
        .into());
    }
    println!("  ok: an unsaved buffer asked instead of exiting, in a dialog centred on it");

    // **The first answer**, and the one a person reaches for by accident: keep editing. The
    // editor is still there afterwards, which is the whole point of asking.
    click_at(&mut qmp, &mut session, dx + CONFIRM_KEEP_CX, dy + CONFIRM_BUTTON_CY)?;
    session.expect("nxedit: close cancelled, still editing")?;
    println!("  ok: `keep editing` dismissed the question and kept the editor");

    // **And the second answer.** Asked again — the same button, the same window, because a
    // question answered *no* must be askable again or the editor can never be closed at all.
    click_at(&mut qmp, &mut session, close_at.0, close_at.1)?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let placed_again = session.rest_of_line()?;
    let (_, _, dx2, dy2, _, _) = parse_dialog_placement(&placed_again)
        .ok_or_else(|| {
            format!("could not read the second dialog placement from {placed_again:?}")
        })?;
    click_at(&mut qmp, &mut session, dx2 + CONFIRM_DISCARD_CX, dy2 + CONFIRM_BUTTON_CY)?;
    session.expect("nxedit: discarding the unsaved buffer")?;
    session.expect("nxedit: closing")?;
    // **The list the destroy produces**, read here because it is the one line whose position in
    // the stream is known: the editor exits, the compositor destroys its windows, and the shell
    // redraws the bar. Step 12 needs a slot out of it, and asking for a list line later would
    // wait for a change that nothing is going to make.
    // **Read until the list reflects the destroy, not merely the next time it changes.** The
    // bar is redrawn for several reasons and two of them fire here in order: the dialog goes
    // first, which hands the keyboard back to the editor and marks the list dirty while the
    // editor is still in it, and only then does the process exit and take its window. Four
    // attempts is a bound rather than a guess — nothing produces that many — and it fails
    // naming the last line it saw.
    let mut after_close = String::new();
    for _ in 0..4 {
        session.expect("desktop-shell: window list on ")?;
        after_close = session.rest_of_line()?;
        if taskbar_slot(&after_close, untitled_id).is_none() {
            break;
        }
    }
    if taskbar_slot(&after_close, untitled_id).is_some() {
        return Err(format!(
            "window {untitled_id} is still in the taskbar after the editor said it was closing: \
             {after_close:?}"
        )
        .into());
    }
    let slot = taskbar_slot(&after_close, edit_id).ok_or_else(|| {
        format!(
            "window {edit_id} is not in the taskbar list {after_close:?}, so it cannot be clicked"
        )
    })?;
    println!("  ok: `discard` was the only thing that ended the run");

    // 12. **Insisting is a second click, not a clock** (M12 Part A). M9 Part C left this
    //     ungated on the stated grounds that "the release image has no client that can be made
    //     to ignore a request", and named the trigger: *the first application that can be
    //     wedged on purpose*. An editor holding an unanswered question is exactly that — from
    //     the shell's side it is indistinguishable from a client that has stopped listening.
    //
    //     **Which is why the two-second grace period had to go.** Against this client a timer
    //     destroys the window, and the buffer with it, two seconds after one click and with no
    //     way to intervene. The shell cannot tell "wedged" from "asking"; the person looking at
    //     the dialog can, so the second middle-click is what says "I meant it".
    //
    //     Driven on the editor from step 9, which is snapped to the right half and holds
    //     `other.txt`. Clicking its document area both raises it and gives it the keyboard.
    let doc = (work.0 + work.2 as i32 * 3 / 4, work.1 + work.3 as i32 / 2);
    click_at(&mut qmp, &mut session, doc.0, doc.1)?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;

    // Its taskbar slot came off the shell's own list above: a slot is a *position*, and windows
    // have been closed since the last time anything here counted, so `id * ENTRY_W` would land
    // on somebody else's entry.
    let entry = (ENTRY_W * slot as i32 + ENTRY_W / 2, LIST_CLICK.1);

    // **`middle_click_at` verifies the pointer with a left click first**, which on a taskbar
    // entry is a gesture in its own right — it raises the window, or minimises it if it already
    // had the keyboard. Neither is asserted here, and neither matters: what the middle click
    // does is the same either way, and a gate that pinned it would be asserting against which
    // window happened to be focused three clicks earlier.
    middle_click_at(&mut qmp, &mut session, entry.0, entry.1)?;
    // **The compositor's line is the ordered one, not the shell's** — the same rule 6a3 states:
    // the compositor logs before it replies, so it leads, and the shell's own line comes after
    // its request returns, which is a race against the client it just woke. The shell's is
    // checked against the whole transcript below, where order does not matter.
    session.expect(&format!("compositor: asked window {edit_id} to close"))?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let asked_dialog = session.rest_of_line()?;
    let (_, _, ax, ay, _, _) = parse_dialog_placement(&asked_dialog)
        .ok_or_else(|| format!("could not read the ask's dialog placement from {asked_dialog:?}"))?;
    println!("  ok: the taskbar's ask reached a client that declined to answer it");

    // **The person answers it, and the arming must not outlive the answer** (PR #267 review,
    // blocking 1). The first version of this policy armed on the ask and disarmed only when the
    // window went away, so *keep editing* left the entry armed for the life of the window and
    // the next middle-click — at any distance in time — destroyed it with no question at all.
    // That is the unbounded version of the very outcome this change replaced a two-second timer
    // to avoid.
    click_at(&mut qmp, &mut session, ax + CONFIRM_KEEP_CX, ay + CONFIRM_BUTTON_CY)?;
    session.expect("nxedit: close cancelled, still editing")?;

    // **A real wait, in a gate that is otherwise expect-driven.** There is no output that says
    // an arming expired — expiry is the *absence* of a state — so the only way to observe it is
    // to let the clock pass it. `INSIST_WINDOW_NS` is five seconds; six is past it with room,
    // and it is the one sleep in this file that is waiting for a rule rather than for a message.
    std::thread::sleep(std::time::Duration::from_secs(6));
    middle_click_at(&mut qmp, &mut session, entry.0, entry.1)?;
    // **Asked, not destroyed** — and this assertion *is* the control. Under the version the
    // review caught, this click produced "did not answer; closed it" and the buffer went with
    // the window; here the question is put again, which is what a click made long after the
    // last one means.
    session.expect(&format!("compositor: asked window {edit_id} to close"))?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    println!("  ok: the arming expired, so a later click asked again rather than destroying");

    // **And a click while the ask is still in hand insists.** The window goes, the dialog goes
    // with it — a dialog is destroyed with its parent — and the shell says which path it took.
    middle_click_at(&mut qmp, &mut session, entry.0, entry.1)?;
    session.expect(&format!("desktop-shell: window {edit_id} did not answer; closed it"))?;
    println!("  ok: a second middle-click destroyed the window the first one only asked about");

    let transcript = session.finish();
    let _ = fs::remove_file(&qmp_sock);
    // **Written on the way past, whichever way this ends.** The checks below each wrote it on
    // *their* failure, so a passing run left whatever an earlier failing one had put there —
    // and a stale transcript is worse than none, because it reads as evidence. It cost an
    // afternoon in M12 Part F: a wallpaper line missing from a file three hours old was
    // diagnosed as a broken decoder for twenty minutes before anybody checked the timestamp.
    // The same argument the checks below already make about their own failures, applied to the
    // case nobody thought needed it.
    let _ = fs::write(build_cache().join("guest-transcript-check-login.log"), &transcript);

    // **The grid grew, in cells.** Read off the client's own line rather than computed here:
    // the cell size is the font's, and re-deriving it in the gate would be a second
    // implementation of `Metrics` that could agree with nothing. Every other assertion in 6g is
    // about *pixels*, and a grid still 80x24 in a 1280x752 window satisfies all of them — a
    // large window with a small terminal in the corner.
    // **The reflow, asserted on a release image without reading anybody's terminal.** A rewrap
    // re-breaks lines at a new width; it does not create or destroy them, so `nxterm` reports
    // the count either side of every resize and they must agree. The control the plan asks for
    // — "two deliberately short adjacent lines are still two rows" — is this same property
    // stated over the whole history: an implementation that ignored the soft-wrap flag and
    // joined every adjacent row collapses it to one line, in one number, whatever the content
    // happened to be. The row-level version of the control, with the long line and the short
    // pair spelled out, is `libterm`'s own test.
    let reflows = transcript_reflows(&transcript);
    if reflows.is_empty() {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err("no \"nxterm: resized to …, lines N->M\" line in the transcript: the \
                    terminal never accepted a `Configure`"
            .into());
    }
    for &(before, after, evicted) in &reflows {
        // **The eviction is subtracted, not tolerated.** Narrowing makes more rows out of the
        // same text, so a history near `SCROLLBACK` loses its oldest lines to the ring — and a
        // gate that asserted equality unconditionally would one day blame `Line::wrapped` for
        // the bound, sending the first person to hit it to the wrong file. Nothing in this
        // session comes near the cap, so `evicted` is 0 here; it is subtracted because the
        // claim is about the rewrap and this is what makes it exactly that (PR #252 review,
        // finding 2).
        if after + evicted != before {
            let path = build_cache().join("guest-transcript-check-login.log");
            let _ = fs::write(&path, &transcript);
            return Err(format!(
                "a resize turned {before} logical lines into {after}, with {evicted} evicted by \
                 the bounded scrollback — so {} went missing. Re-wrapping moves where the \
                 breaks are and must not join lines that were never one: `Line::wrapped` is \
                 what separates a soft wrap from a line that ended, and a rewrap that ignores \
                 it merges paragraphs the first time a window widens",
                before as i64 - after as i64 - evicted as i64
            )
            .into());
        }
    }
    println!("  ok: {} reflow(s) kept every line a line", reflows.len());

    match transcript_grid(&transcript) {
        Some((c, r)) if c > 80 && r > 24 => {
            println!("  ok: the maximised terminal's grid grew to {c}x{r} cells")
        }
        found => {
            let path = build_cache().join("guest-transcript-check-login.log");
            let _ = fs::write(&path, &transcript);
            return Err(format!(
                "the maximised terminal's grid is {found:?}, not bigger than the 80x24 it starts \
                 at. `nxterm` must refit the grid to the window it accepted — the window growing \
                 is Part D's easy half, and a terminal that cannot use the room is the point of \
                 the reflow"
            )
            .into());
        }
    }

    // **And that it asked before it insisted** (M12 Part A) — order-independent for the same
    // reason, and it is what makes step 12 a *policy* assertion rather than a coincidence: the
    // shell reached `Manage::Close` on the second click, and it reached `RequestClose` on the
    // first. A shell that destroyed the window straight away would produce the "did not answer"
    // line the ordered half already matched, and none of this.
    if !transcript.contains(&format!("desktop-shell: asked window {edit_id} to close")) {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the shell destroyed window {edit_id} without ever reporting that it asked first: \
             insisting must be the *second* middle-click, and the first must be a request the \
             client is free to answer with a dialog"
        )
        .into());
    }

    // The shell said it asked — order-independent, because that line races the client it woke.
    if !transcript.contains(&format!("desktop-shell: asked window {first_term_id} to close")) {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the shell never reported asking window {first_term_id} to close, so the taskbar's \
             middle-click did not reach `Manage::RequestClose`"
        )
        .into());
    }

    // **The shell the terminal spawned went with it.** `nxterm` holds the pty master and hands
    // the far end to its `nxsh`; closing the window therefore has to end that shell too, or
    // every close leaks a process. Nothing observed the child until the Part C review asked —
    // the machinery was there (`libstream` documents `PeerClosed` as "stop producing, exit")
    // and the gate now says so out loud. Order-independent: the child notices its master go at
    // whatever moment the tty-server gets there.
    // **The modal was dismissed by the compositor, not by a focus change** (M11 Part E batch 5).
    // The distinction is the whole of the fix: clicking another *window* raises it, and a raise
    // is a focus change the popup hears about — which is why the first version of this step
    // passed while the reported bug survived. A press on a panel raises nothing.
    if !transcript.contains("compositor: dismissed win=") {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the applications modal closed, but the compositor never sent a dismissal — so it \
             closed on a focus change, which is the half of this that already worked. A press on \
             a panel raises no window and changes no focus, so nothing but `Surface::Dismissed` \
             can have closed it.\n\nthe transcript is at {}",
            path.display()
        )
        .into());
    }
    if !transcript.contains("nxsh: terminal closed") {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err("the terminal closed and its `nxsh` did not: no \"nxsh: terminal closed\" in \
                    the transcript. A shell whose terminal is gone has nobody to read from and \
                    nobody to print to — it must exit rather than be orphaned per window closed"
            .into());
    }

    // **The close button sent nothing.** Present-and-absent together: the click above closed the
    // window, and no `RequestClose` was ever sent for it — the id is the second terminal's, so
    // the first one's genuine request cannot satisfy this.
    if transcript.contains(&format!("asked window {term_id} to close")) {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the close button asked the shell: the transcript contains \"asked window \
             {term_id} to close\". `nxterm`'s close button is its own — it exits, and the \
             compositor tears its windows down with its session. A client that routed its own \
             close through the manager would be asking somebody else for permission to stop"
        )
        .into());
    }
    if let Err(e) = check_two_sessions(&transcript) {
        let path = build_cache().join("guest-transcript-check-login.log");
        let saved = fs::write(&path, &transcript).is_ok();
        if saved {
            println!("\nthe full transcript is at {}", path.display());
        }
        return Err(e);
    }
    println!(
        "\nxtask: graphical login gate PASSED — refused a wrong password, ran a session, and a \
         serial login ran beside it ✓"
    );
    Ok(())
}

/// Assert the graphical session was still running when the serial one started.
///
/// **An absence, which `expect` cannot express.** The two logins succeeding in sequence does
/// not by itself prove they overlapped: a graphical session that ended the moment the serial
/// one began would produce the same ordered lines. What distinguishes them is that
/// `desktop-session-mgr` never reported a session ending — its leader blocks, so the only way
/// that line appears is if the session came down.
fn check_two_sessions(transcript: &str) -> R<()> {
    // **The terminal opened and hosted a shell**, which is what makes M7 visible: a person
    // typed in the applications modal and got a terminal. It found a font, a terminal and
    // `/bin` in the namespace the shell built for it — none of which an application namespace
    // had before Part F.
    //
    // Checked against the transcript rather than with an `expect`, because `nxterm` runs
    // concurrently with the shell and ordering the two would be a race. **Not the grid line**:
    // `nxterm::report_row` is `test-harness`-only and this gate boots a release image, so
    // asserting `"nxterm: grid> …"` here could never have passed — which is how the first
    // version of this check failed.
    // **And its shell got an environment — asserted from the shell, not from its parent.**
    // M7 Part F's second claim: `nxterm` took no setup message and handed `nxsh` a
    // `Record::default()`, so a terminal launched into a constructed namespace would give its
    // shell no `$env.HOME` while every serial login's has one.
    //
    // **This reads `nxsh`'s own line, and the first version did not.** It read `nxterm:
    // hosting a shell (env: N)`, which `nxterm` logs from the environment it *received* — so
    // breaking the forward to `nxsh` and leaving the receipt intact kept the gate green. A
    // parent cannot testify to what its child was given; only the child can. Demonstrated in
    // review by doing exactly that (PR #238 review, finding 1).
    //
    // Two shells must report, because this gate runs two sessions: the terminal
    // `desktop-shell` launched, and the serial login beside it. Counting them is what stops an
    // absence-only check from passing when no shell started at all.
    let shells = transcript.matches("nxsh: up (env: ").count();
    if transcript.contains("nxsh: up (env: 0 fields)") {
        return Err("a shell started with an empty environment, so its `$env.HOME` is unset. \
             Somewhere between `desktop-session-mgr` and `nxsh` a setup message was not sent \
             or not forwarded"
            .into());
    }
    if shells < 2 {
        return Err(format!(
            "expected two shells to report an environment (the launched terminal's and the \
             serial login's); saw {shells}. A shell that never started cannot have an empty \
             environment, which is why absence alone is not the check."
        )
        .into());
    }
    if !transcript.contains("nxterm: hosting a shell") {
        return Err("the launched terminal never hosted a shell. `nxterm: no shell` means it \
             could not spawn one — most likely `/bin` is missing from the application \
             namespace; no `nxterm:` line at all means it never started"
            .into());
    }
    // **An empty applications list would satisfy every expect above.** "`/bin` lists " matches
    // "lists 0 programs", and "modal open" says nothing about its contents — so a session
    // whose `/bin` failed to open would pass the whole gate. Asserted as an absence for the
    // same reason the concurrency check is: `expect` cannot say "a number greater than zero".
    // **What the `desktop` command itself reported** — order-independent, because it is a
    // different process from the shell whose lines are asserted above. Both halves matter:
    // without the shell's lines a command could have invented these, and without these a shell
    // could have served a reply nobody received.
    // **`(table)` rather than a bare "listed"**: the listing is the one data product here, and
    // it goes to stdout as TSM1 like every other coreutil's. It went to *stderr* until PR #245's
    // review — where `desktop | sort name` produced nothing and the rows interleaved with every
    // stage's diagnostics on the shared sink — and a gate reading a release image can see the
    // bytes nowhere, so the command names the branch it took. Match the name, not just the count.
    for line in ["desktop: running", "desktop: listed 3 desktops (table)", "desktop: named 2 cli"] {
        if !transcript.contains(line) {
            return Err(format!(
                "the `desktop` command did not report \"{line}\". It resolves /dev/desktop from \
                 the namespace `desktop-shell` built for the terminal that ran it, so a missing \
                 line means either the bind or the resolve — check for \"nxsh: could not \
                 resolve\", which is what a command that never started looks like"
            )
            .into());
        }
    }

    // **The configure deadline must not fire, and this is what would have caught the deadlock.**
    // `no manager answer for window N; showing it` exists to name a wedged or absent manager —
    // "a wedged or slow shell must delay a window, never lose it". Part C briefly created the
    // bottom bar *after* taking the manager channel, so the shell parked inside `create` waiting
    // for a `Configure` only it could release, and this line fired on every healthy boot. A
    // signal that fires when nothing is wrong is not a signal (PR #242 review, blocking 1).
    if transcript.contains("compositor: no manager answer for window") {
        return Err("the compositor's configure deadline fired during a healthy session start: \
             some window was held for the manager until the 200ms timeout. That timer exists for \
             a wedged or absent manager, so a session that needs it has a shell blocking on \
             something only the shell could answer — check what was created after \
             `/dev/draw/manage` was taken"
            .into());
    }
    if transcript.contains("desktop-shell: /bin lists 0 programs") {
        return Err("the applications modal is empty: /bin projected no programs into the \
             session namespace. The modal opening is not evidence it has anything in it"
            .into());
    }
    if !transcript.contains("desktop-shell: up (graphical session leader)") {
        return Err("the graphical session never started, so nothing was concurrent".into());
    }
    if transcript.contains("desktop-session-mgr: session ended") {
        return Err("the graphical session ended before the serial login finished, so the two \
             were sequential rather than concurrent. Its leader blocks and should outlive this \
             gate — check the transcript for why it exited"
            .into());
    }
    println!("xtask: a graphical and a serial session ran at the same time ✓");
    Ok(())
}

/// Position the pointer, click, and **confirm where the press landed** — retrying if it did
/// not land there.
///
/// **The press is the receipt, and it already existed.** `move_pointer_to` fires ~33
/// unacknowledged relative motions, and a dropped one leaves a permanent offset; the
/// compositor already reports every press's position, so the retry needs no guest change at
/// all. PR #232 tried reporting position per *motion* instead and it cost too much in the
/// compositor's input path — this is the cheap half of that idea, using a receipt the protocol
/// was already emitting.
///
/// Observed on the first full run of this gate: `input batch DROPPED (SYN_DROPPED)` and a
/// press at (321, 312) with `win=none`.
///
/// **A retry is not inert, and since M8 Part C it can undo the thing it is retrying.** The
/// bar's entries are *toggles* — a press that misses the aimed pixel by less than an entry
/// width lands on the same entry and performs the gesture, and the retry then performs its
/// inverse, so the gate waits forever for a transition that happened twice. That is not fixed
/// here; what is fixed is the cause of the misses. Every click used to re-pin the pointer to a
/// corner with twenty over-driven motions before walking, and that burst is what overran the
/// guest's input ring. A confirmed press is a position report, so `Qmp::pointer` remembers it
/// and consecutive clicks walk from there — two motions rather than thirty-four
/// (PR #243 review, finding 3).
fn click_at(qmp: &mut Qmp, session: &mut Session, x: i32, y: i32) -> R<()> {
    const ATTEMPTS: u32 = 3;
    const PER_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(10);
    for attempt in 1..=ATTEMPTS {
        // Unknown position means pin first; known means walk the difference.
        move_pointer_to(qmp, x, y)?;
        qmp.send_button("left", true)?;
        qmp.send_button("left", false)?;
        if session.expect_within(&format!("compositor: press at x={x} y={y}"), PER_ATTEMPT)? {
            qmp.pointer = Some((x, y));
            return Ok(());
        }
        // It did not land where it was aimed, so where it *is* is no longer known.
        qmp.pointer = None;
        println!("  note: the press did not land at ({x}, {y}) on attempt {attempt}/{ATTEMPTS}");
        // The abandoned attempt's press must not satisfy the next one's wait.
        session.skip_to_end()?;
    }
    Err(format!(
        "the pointer never reached ({x}, {y}) in {ATTEMPTS} attempts. Injection is relative and \
         unacknowledged, so anything that eats a delta leaves a permanent offset. Since \
         2026-08-26 `input-server` carries the motion of an undeliverable batch forward instead \
         of discarding it, and `burst_holds_its_position` gates that — so a failure here is \
         more likely a mis-aimed click than lost movement. Check that gate's verdict first, \
         then look for `input batch DROPPED (SYN_DROPPED)` in the transcript. Loss on the \
         *host* side leaves no such line at all: QEMU's PS/2 queue drops a whole packet it \
         cannot fit, which is why the pin is drained before the walk and why three attempts \
         failing means something other than a dropped motion"
    )
    .into())
}

/// Inject a burst of motion across a full-screen repaint and check that none of it was lost.
///
/// **The assertion is arithmetic, not a screenshot.** Every injected delta is known, the start
/// is pinned by over-driving into a corner, and the compositor reports where a press landed — so
/// the expected position is exact and any difference is movement that did not arrive. There is
/// no rounding, no scaling and no acknowledgement anywhere on this path: a PS/2 delta is applied
/// as an integer and clamped only at the screen edge, which this route stays well inside.
///
/// **The burst must be faster than the guest can drain**, or it proves nothing: paced injection
/// is what every other step here does, and a consumer that is never overrun is a consumer whose
/// loss path is untested. It is sent as fast as QMP accepts it, immediately after a desktop
/// switch, which is the compositor's one legitimately whole-screen recompose.
fn burst_holds_its_position(
    qmp: &mut Qmp,
    session: &mut Session,
    chord: &dyn Fn(&mut Qmp, bool, &str) -> R<()>,
) -> R<()> {
    // Start pinned, and **without a click to confirm it**: over-driving into a corner is the one
    // position injection can establish with no acknowledgement, because the clamp is what makes
    // it certain — 2000 px of travel into an edge lands on the edge whether or not some of it
    // arrives. A confirming click would be worse than redundant here: the bottom-right corner is
    // the desktop indicator's hit region, so it opens the overview, which then takes the chord
    // below instead of the shell.
    for _ in 0..20 {
        qmp.send_motion(100, 100)?;
    }
    qmp.pointer = Some((1279, 799));
    // Let that drain before anything else is injected. **The chord below is a key, and a key is
    // exactly what this fix does not recover**: `SYN_DROPPED` asks a consumer to resynchronise
    // state it can re-derive, and motion is the half it cannot. Flooding the ring and then
    // relying on a keystroke to survive it would be a gate that failed for the one reason the
    // system is entitled to.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Up and to the left, staying inside the screen the whole way: a burst that ran into a
    // clamp would hide exactly the loss this is looking for.
    const K: i32 = 120;
    const DX: i32 = -6;
    const DY: i32 = -3;
    let want = (1279 + K * DX, 799 + K * DY);

    // **To desktop 1, because the session is on desktop 2 by now** and switching to the desktop
    // already showing is a no-op the shell correctly says nothing about — the first version of
    // this waited 45 s for an acknowledgement that was never coming.
    //
    // **The motion goes in immediately after the chord, not after the acknowledgement.** The
    // shell logs the switch once the compositor has answered, and the compositor answers when
    // the recompose is done — so waiting for that line puts the burst entirely *after* the stall
    // it is supposed to land in. Verified the expensive way: with the burst placed after the
    // acknowledgement, this gate passed against an `input-server` that discards motion, which is
    // the bug it exists to catch.
    //
    // Injecting behind the chord is safe for the chord: its keys are already queued ahead of
    // this motion, and an overflowing ring refuses what arrives last.
    chord(qmp, false, "1")?;
    for _ in 0..K {
        qmp.send_motion(DX, DY)?;
    }
    session.expect("desktop-shell: switched to ")?;

    // Settle before reading: deferred motion is re-sent on the next wakeup or within a few
    // milliseconds, and the click that reads the position is a *button* — droppable like any
    // other, so it is injected once the guest is idle rather than into the same stall.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Where it actually ended up. Not `click_at`, which re-pins and retries — the retry exists
    // to survive precisely the loss under test, and using it here would report a pass after
    // correcting for the bug.
    qmp.send_button("left", true)?;
    qmp.send_button("left", false)?;
    let want_line = format!("compositor: press at x={} y={}", want.0, want.1);
    if session.expect_within(&want_line, std::time::Duration::from_secs(20))? {
        qmp.pointer = Some(want);
        println!("  ok: {K} motions across a full-screen repaint arrived to the pixel");
        return Ok(());
    }
    qmp.pointer = None;
    Err(format!(
        "input gate FAILED: {K} injected motions of ({DX}, {DY}) from (1279, 799) did not put \
         the cursor at ({}, {}). A relative delta that does not arrive cannot be recovered — \
         `input-server` must carry the motion of an undeliverable batch forward and re-emit it, \
         rather than counting it as a `SYN_DROPPED` gap. Look for `input batch DROPPED` in the \
         transcript: a gap here means the compositor was busy and the deferral did not cover it",
        want.0, want.1
    )
    .into())
}

/// Type a line into a windowed terminal, then Enter.
///
/// **Paced by a delay rather than by a receipt, which is the exception here.** Every other
/// typing loop in these gates waits for something the guest says — the greeter's redraw, the
/// shell's `name so far`, a prompt on serial. A terminal's echo goes into its *grid*, and the
/// grid renders under `test-harness` only, so a gate booting a release image has nothing to
/// wait on per character. Injection is relative and unacknowledged, and a dropped PS/2 batch
/// eats a keystroke — which here means the command line reads `desktp` and nothing runs.
///
/// The delay is the smallest thing that removes the drops rather than a guess at a safe number:
/// the whole line is a handful of characters and the guest's input ring is drained per event.
fn type_at_terminal(qmp: &mut Qmp, line: &str) -> R<()> {
    const PER_KEY: std::time::Duration = std::time::Duration::from_millis(40);
    // **A bare Enter first, because this gate has pressed Escape at a terminal.** `ESC` is the
    // **meta prefix**: the discipline consumes the byte after a bare one, exactly as readline
    // does — `ESC d` is M-d, and an unbound pair is discarded rather than inserted. This gate
    // presses Escape to dismiss the modal and the overview, and when no popup is open that
    // Escape reaches whatever holds the keyboard, which by then is the terminal. So the next
    // character typed is eaten: `desktop` becomes `esktop`.
    //
    // Measured rather than assumed, after it was briefly taken for a bug in `nxsh`: typing one
    // command three times in a session loses one character, and injecting an Escape between two
    // of them loses a second — the loss follows the Escape. An empty line is the cheapest thing
    // to feed a prefix that is going to take one.
    std::thread::sleep(PER_KEY);
    press(qmp, "ret")?;
    // **The wait comes *before* each key, including the first**, and that is not cosmetic.
    // Typing here follows a click that raises the window, focuses a widget and repaints; the
    // first character injected straight after it was swallowed, and the shell read `esktop`,
    // which resolves to nothing. Sleeping only *between* keys leaves exactly that one gap.
    for c in line.chars() {
        std::thread::sleep(PER_KEY);
        match c {
            ' ' => press(qmp, "spc")?,
            _ => {
                let mut qcode = String::new();
                qcode.push(c);
                press(qmp, &qcode)?;
            }
        }
    }
    std::thread::sleep(PER_KEY);
    press(qmp, "ret")?;
    Ok(())
}

/// Inject one key by qcode, press and release.
fn press(qmp: &mut Qmp, qcode: &str) -> R<()> {
    qmp.send_key(qcode, true)?;
    qmp.send_key(qcode, false)?;
    Ok(())
}

/// Type `text` at the greeter, one character at a time, waiting for each redraw.
///
/// **One at a time and waited for**, for the reason `check-terminal`'s typing loop gives: a
/// word injected as fast as QMP can send it outruns a client that repaints between keystrokes,
/// and the tail lands on a client that is not looking. A human types slower than this loop; a
/// harness does not.
fn type_at_greeter(qmp: &mut Qmp, session: &mut Session, text: &str) -> R<()> {
    for c in text.chars() {
        let qcode = match c {
            'a'..='z' => {
                let mut s = String::new();
                s.push(c);
                s
            }
            '0'..='9' => c.to_string(),
            ' ' => "spc".to_string(),
            '-' => "minus".to_string(),
            other => {
                return Err(format!(
                    "check-login cannot type {other:?}: add its qcode to `type_at_greeter`.                      The demo credentials are lowercase, digits, space and hyphen; a new one                      outside that set needs a mapping rather than a silent skip"
                )
                .into());
            }
        };
        press(qmp, &qcode)?;
        // The redraw is the receipt, and its number is not asserted — only that one happened.
        //
        // **One receipt is not one keystroke.** The greeter drains every queued event and
        // presents once, so two keys that arrive in the same pump produce one redraw and this
        // loop runs a receipt ahead until the next key supplies it. Measured on a passing
        // run: 20 keystrokes produced 20 redraws in one phase and 34 produced 33 in another.
        // That is fine and self-correcting — nothing is lost, and the drift stays far under
        // the outbox's depth — but it is pacing, not a per-character acknowledgement, and the
        // comment said otherwise (PR #236 review, finding 5).
        session.expect("desktop-session-mgr: greeter redraw ")?;
    }
    Ok(())
}

///
/// **The whole loop, in one assertion**: i8042 → `input-server` → compositor → `nxterm` →
/// `tty-server` → `nxsh` → back out → the terminal's grid. Every piece of it has its own test;
/// none of those can tell you the pieces are joined.
///
/// **Asserted on the grid's contents, not on pixels.** What a shell prints is not fixed by this
/// milestone, so comparing pixels would pin it — and the display gate already compares a
/// *fixed* terminal render, which is the part that must not drift. Under `test-harness`,
/// `nxterm` reports each completed grid line on the debug console; that is what this reads.
///
/// **It clicks before it types**, which is not ceremony. `nxterm` is created first so it sits at
/// the bottom of the stack (windows stack at the origin in creation order and it is the
/// largest — see `init`), and keys follow the *topmost focusable* window. Click-to-focus raises
/// it, which is both how a user would do it and the only mechanism available: there is no op to
/// raise a window, and there will not be until Milestone 6.
/// `cargo xtask check-terminal` — prove a keystroke reaches a shell and its answer comes back.
///
/// **Status 2026-08-13 (superseding an earlier status the same day): the flake was real, and it
/// was not in the harness.** This gate spent two milestones failing, and the standing diagnosis
/// here blamed "driving a GUI from QMP" and suspected an `input-server` bug where a consumer
/// disconnecting cost the others their stream. **Both were wrong**, and the second sent at least
/// one investigation after a bug that does not exist.
///
/// The cause was a kernel deadlock: the boot self-test's verdict syscall parked a CPU that the
/// scheduler still counted online, so the next TLB shootdown — any large `free` in any process —
/// waited forever for an acknowledgement it could never get. `nxterm` froze mid-repaint while
/// freeing a glyph outline, which is why it looked like a terminal that stops on a particular
/// keystroke. Fixed 2026-08-13; see the decision log.
///
/// One piece of the old diagnosis survives and is still load-bearing below: **a keystroke is not
/// cheap.** The terminal repaints, waits for a free buffer and copies a window of pixels before
/// it looks at input again, so keys injected as fast as QMP sends them outrun it. Pacing on the
/// echo is the fix, and it is what the typing loop does.
///
/// **Wired into CI** since 2026-08-18 (`.github/workflows/ci.yml`, the QEMU integration job,
/// unconditional), on 64 consecutive passes. The bar was never the count: the audit logged one
/// unreproduced failure at the click step, and what made promotion defensible is that this gate
/// now asserts *where* the press landed before asserting that `nxterm` received it, so a
/// recurrence reports coordinates rather than a bare timeout. That failure is still
/// unexplained. The deferral entry that used to hold this record has moved to the resolved
/// table; the live record is now `docs/decision-log.md`, 2026-08-18.
fn cmd_check_terminal(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    let work = repo_root().join("tools/build-cache");
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-terminal.sock");
    let _ = fs::remove_file(&qmp_sock);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel)?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
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

    println!("xtask: terminal gate — booting and typing…\n");
    let mut session = Session::spawn(cmd, "check-terminal")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;

    // **The grid's cell, recomputed on the host from the same file** (M11 Part D). Since the
    // desktop's font became proportional, `nxterm` is the one program that loads two — and
    // handing the wrong one to `libterm` does not fail: `Metrics` takes a cell's width from a
    // single glyph's advance, so a proportional face yields a plausible number and then draws
    // every column at the wrong x. Everything else this gate checks would still pass, because
    // the grid the harness reports is cells and not pixels.
    //
    // So the guest says which face it measured with and what it got, and the host measures the
    // same file at the same size and compares. The size is taken from the line rather than
    // assumed: the image stages a theme with a deliberately non-default `font_px`, and pinning
    // the number here would be pinning the theme file to this gate.
    {
        session.expect("nxterm: grid font ")?;
        let line = session.rest_of_line()?;
        let (path, rest) = line.trim().split_once(", cell ").ok_or_else(|| {
            format!("nxterm's grid line is not in the form this gate reads: {line:?}")
        })?;
        let (dims, px) = rest.split_once(" at ").ok_or_else(|| {
            format!("nxterm's grid line has no size: {line:?}")
        })?;
        let (w, h) = dims.split_once('x').ok_or_else(|| format!("no cell size: {line:?}"))?;
        let (w, h): (u32, u32) = (w.parse()?, h.parse()?);
        let px: f32 = px.trim_end_matches("px").parse()?;
        let want = libterm::render::Metrics::new(&host_font(path)?, px);
        if (want.cell_w, want.cell_h) != (w, h) {
            return Err(format!(
                "the guest measured a {w}x{h} cell from {path} at {px}px; the host makes it \
                 {}x{} from the same file. Either the guest loaded a different font from the \
                 one it named — the proportional face is the live mistake here — or the two \
                 sides disagree about rasterisation, which no other gate would notice",
                want.cell_w, want.cell_h
            )
            .into());
        }
        println!("  ok: the grid measures a {w}x{h} cell from {path}, as the host does");
    }

    // The shell is up in the window: its banner reached the grid, which means the whole
    // output direction already works before a key is injected.
    session.expect("nxterm: grid> nxsh: interactive shell")?;

    // Wait for `ui-testclient` to finish churning windows before touching focus — a click
    // landing mid-churn raises whatever exists at that instant. Same reason `check-input`
    // waits for it.
    session.expect("ui-testclient: PASSED")?;

    // **And wait for `input-testclient` to leave.** It creates a 2048×2048 window — larger
    // than the screen — as its second phase, and while that exists it is the topmost window
    // everywhere and takes every click and keystroke. Undriven, it gives up after an idle
    // deadline and says so. Waiting is not optional: a click sent before this *is* its first
    // phase's sentinel, so it wakes the client, which then creates that window and swallows
    // everything typed after the first character.
    // **The phase-1 message specifically.** The client has two idle exits and they used to
    // print the same line: this one before the window phase, where nothing has been created
    // and acting immediately is safe, and one *after* the 2048×2048 window exists, where the
    // line is an announcement and the window outlives it through `exit`, the kernel's
    // teardown and the compositor noticing the peer closed. Waiting on the ambiguous string
    // meant this gate could resume in either state; the click that follows lands on the
    // terminal in one and on a screen-sized window in the other.
    session.expect("input-testclient: idle before the window phase")?;

    // Click inside `nxterm`, past the right edge of the reference windows stacked above its
    // top-left corner — they are 320 wide, and the aim is clear of them in x whatever their
    // heights, which is what keeps this point stable as the reference picture grows.
    //
    // **Through `click_at`, which this gate hand-rolled the sequence for until 2026-08-31.**
    // The pin and the walk are the same arithmetic it does — injection is relative, so the
    // pointer is over-driven into a corner first and everything after that is subtraction; the
    // first version of this gate clicked at (0, 0), inside the 64×32 scene window, and the
    // version after that sent one motion of (4000, 4000), which a 9-bit PS/2 delta turns into a
    // different, meaningless movement. What the helper adds is the retry, and the retry is what
    // was missing: a dropped packet is not a lost pixel but a permanent offset, so asserting the
    // position once turns one lost motion into a failed build.
    //
    // **Which is what happened, and the split assertion is why it can be said.** CI failed here
    // with the press at (495, 351) — exactly one step of (-98, -56) short of its aim, the whole
    // packet gone rather than a truncated one. The comment this replaces called the position
    // "bit-identical over 40 loaded runs" and left the gate's one prior failure unexplained;
    // both readings came from having no diagnosis to hang on it. It has one now: injection at
    // this rate loses a packet occasionally, the guest cannot recover a relative delta it never
    // received, and the drain in `move_pointer_to` is the half of the fix that stops it
    // happening. The assertion is unchanged and still exact — a press that lands anywhere else
    // is still a failure, and a systematic misplacement still fails all three attempts.
    click_at(&mut qmp, &mut session, 397, 295)?;

    // **Wait for the click to land before typing.** Click-to-focus is what gives `nxterm` the
    // keyboard — it is created first and therefore bottom-most — and the raise is not
    // instantaneous from the host's point of view. Injected back to back, the first keystroke
    // races it and lands on whatever was on top; the failure is intermittent, which is worse
    // than consistent. A focus *change* is not usable as the signal here, because the window
    // may already have had focus and no event is sent for that.
    session.expect("nxterm: clicked")?;


    // **One character at a time, each waited for.** Two reasons, and the second is the one
    // that cost an afternoon.
    //
    // Keys go to the topmost focusable window, and the click above is what raises `nxterm` —
    // created first, so bottom-most until then. A word injected back to back races that raise.
    //
    // And a keystroke here is not cheap: the terminal repaints, waits for a free buffer and
    // copies a window's worth of pixels before it looks at input again. Six keys injected as
    // fast as QMP can send them outrun that, and the tail of the word lands on a client that
    // is not looking. A human types slower than this loop; a harness does not.
    //
    // Waiting on the echo is not a workaround for the pacing — it *is* the assertion. Each
    // character has gone `nxterm` → `tty-server` → `nxsh` → back before the next is sent.
    let mut typed = String::from("/> ");
    for k in ["w", "h", "o", "a", "m", "i"] {
        qmp.send_key(k, true)?;
        qmp.send_key(k, false)?;
        typed.push_str(k);
        session.expect(&format!("nxterm: grid> {typed}"))?;
    }

    // **The echo is the loop.** Every character above travelled to the shell and came back
    // as output before it reached the grid — `nxterm` does not echo locally, and a terminal
    // that did would pass this while talking to nobody.
    qmp.send_key("ret", true)?;
    qmp.send_key("ret", false)?;
    session.expect("nxterm: grid> /> whoami")?;

    // And the shell answered. **Its text is not asserted**, because a shell in a window has
    // no session: `nxterm` inherits `init`'s root namespace, so `/session/user` — which is
    // what `whoami` reads — is not bound, and it says so. That is the same gap as
    // `TODO(gui-dev-tty)` seen from the other side, and Milestone 7 closes both when
    // `desktop-shell` constructs a namespace per application.
    session.expect("nxterm: grid> ")?;

    // **The menu is a window now (M6 C3).** It was a `Stack` layer over the terminal, which
    // worked only because it happened to fit inside it; as a `popup` it is parented to the
    // terminal, placed by `nxterm` at the anchor its own layout gives, and clipped by the
    // screen rather than by its parent.
    //
    // **F1, not a click on the bar.** `nxterm` is created before `ui-testclient`'s windows, so
    // the bar button that normally opens the menu is underneath them and cannot be clicked. A
    // key can be injected, and doing it *here* — after everything typed at the shell has been
    // asserted — matters: an open menu is a topmost popup and takes the keyboard, so opening it
    // earlier would swallow the typing this gate exists to check.
    qmp.send_key("f1", true)?;
    qmp.send_key("f1", false)?;
    session.expect("nxterm: menu popup ")?;
    // `<id> at <x>,<y> <w>x<h>`, in screen coordinates — the compositor resolved the offset
    // against the terminal's origin, so this is where a click has to be aimed.
    let line = session.rest_of_line()?;
    let Some((_, px, py, pw, ph)) = parse_popup_line(&line) else {
        let _ = session.child.kill();
        return Err(format!("could not read the popup's geometry from {line:?}").into());
    };
    println!("  ok: the menu is a window: {line}");
    if pw == 0 || ph == 0 {
        let _ = session.child.kill();
        return Err(format!("the menu popup has no extent: {line:?}").into());
    }

    // **Click the menu.** This is the whole of C3 in one step: the record names the popup,
    // `libsurface` routes it to the popup's own tree rather than the terminal's, and the item
    // it lands on produces a message. Before part 1 the record could not say which window it
    // was for; before part 2 the client could not hold both windows at once.
    //
    // The upper quarter, so it lands on the first item rather than near the boundary between
    // the two. The press position is asserted before the effect, for the reason the click above
    // states: it separates "the pointer was not there" from "the pointer was there and nothing
    // happened".
    let (cx, cy) = (px + pw as i32 / 2, py + ph as i32 / 4);
    // The pointer is already at a *confirmed* position from the click above, so this walks a
    // few motions rather than re-pinning — and a retry here is cheap for the same reason.
    // Nothing dismisses this popup but choosing from it, so an attempt that lands elsewhere
    // leaves it open for the next one.
    // **Hover before the click, and it has to be in that order** (M11 Part E batch 3). Hover is
    // the first thing in this system that reacts to the pointer without a button held, and it is
    // invisible to a gate: the highlight is pixels, and this boot has no reference render of a
    // menu to compare against. So the client says which item it is over — `MENU_CLEAR_KEY`, the
    // one the click then activates.
    //
    // Moving and clicking as one step does not show it: choosing `Clear` closes the menu, and
    // the popup is destroyed at the top of the next iteration *before* it would have painted
    // itself hovered. So the pointer arrives first and the receipt is waited for.
    //
    // **It is a claim about the path, not the widget.** `menu_item` painting a highlight when
    // told to is a host test; that `Router::inside` is fed by real PS/2 motion, through the
    // compositor, into a popup's own router, and reaches the view, is only observable here — and
    // it had never happened before this batch, because nothing asked the router.
    move_pointer_to(&mut qmp, cx, cy)?;
    session.expect("nxterm: menu hover 2")?;
    // **The receipt is also the position proof**, which is why the tracked position is set here
    // rather than assumed. `move_pointer_to` deliberately does not record where it went — only a
    // *confirmed* press does, because injection is relative and an unacknowledged move leaves
    // the host believing something it cannot check. Here the guest has just said it is over the
    // item, so the position is known by evidence rather than by assertion.
    //
    // Skipping this cost an afternoon: the click below then walked its delta from the position
    // the *previous* click had confirmed, doubling the movement, landing at the corner, and
    // dismissing the menu — after which the retry pressed on the terminal underneath.
    qmp.pointer = Some((cx, cy));
    click_at(&mut qmp, &mut session, cx, cy)?;
    session.expect("nxterm: menu chose Clear")?;

    let _ = session.child.kill();
    let _ = fs::remove_file(&qmp_sock);

    // Everything above is ordered, so it is an `expect` chain. This last check is not:
    // `boot-probe` exits somewhere between `ui-testclient: PASSED` and
    // `input-testclient: idle`, and which side it lands on is timing. An `expect` placed
    // between two lines whose order is not guaranteed is a flake, so this reads the
    // finished transcript instead.
    check_service_attribution(&session.finish().into_bytes())?;

    println!(
        "\nxtask: terminal gate PASSED — a keystroke reached the shell and its answer \
         reached the grid ✓"
    );
    Ok(())
}

/// Assert that `service-mgr` told its two children's exits apart.
///
/// `KIND_CHILD_EXITED` names a child by pid and nothing maps a process handle to a pid, so
/// the discriminator is which control endpoint closed (`TODO(child-exit-attribution)`).
///
/// **This lives on `check-terminal` rather than `test-qemu`, and the reason is the verdict.**
/// `test-qemu` asserted it until retrofit Part B moved `SYS_TEST_EXIT` into `boot-probe`:
/// there, the probe's last act terminates the machine, so `service-mgr` never sees it exit
/// and there is nothing to attribute. `check-terminal` boots the *same image* without the
/// `isa-debug-exit` device, so the verdict write is ignored, the probe carries on to
/// `exit(0)`, and its exit is attributed like any other service's.
///
/// `code=0` and not a bare "exited": a supervisor that reads a *closed control channel* as
/// an exit reports the death before the child has run and prints `code=unknown`
/// (PR #226 review, findings 1 and 3).
fn check_service_attribution(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("service-mgr: 'boot-probe' exited code=0") {
        // Three different faults reach this line, so the message names all three rather
        // than the one that prompted the assertion.
        return Err("service-mgr did not attribute boot-probe's exit with its status \
             (expected \"service-mgr: 'boot-probe' exited code=0\"). Check the transcript for \
             which it is: a \"boot-probe: … FAIL\" line means a gate failed and the probe \
             exited 1 — that is the real fault and this assertion is only how it surfaced; \
             \"code=unknown\" means the death was detected without a matching notification, \
             which is what an early control-handle close looks like; and no \"boot-probe\" \
             line at all means it never ran."
            .into());
    }
    // And nothing else was blamed for it. `heartbeat` is `policy = always`, so a
    // misattributed exit shows up as a restart of a service that never stopped. Its
    // *requested* shutdown is a different line and is expected — this boot runs long
    // enough to reach it, which `test-qemu` never did.
    if text.contains("service-mgr: restarting 'heartbeat'") {
        return Err("service-mgr restarted 'heartbeat', which never exited — \
             boot-probe's exit was misattributed to it"
            .into());
    }
    println!("xtask: service-mgr attributed boot-probe's exit to boot-probe ✓");
    Ok(())
}

/// How many screendumps the display gate takes waiting for two in a row to match, and how
/// long it waits between them. ~3s of budget, against a repaint measured in tens of ms.
const SCREEN_SETTLE_TRIES: usize = 12;
/// See [`SCREEN_SETTLE_TRIES`].
const SCREEN_SETTLE_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Screendump until two consecutive captures are identical, and return the settled bytes.
///
/// **Two identical dumps, not a sleep.** A fixed delay is a guess that either wastes time or is
/// too short on a loaded machine, and silently becomes too short again the next time the scene
/// grows. This waits for the property a comparison actually needs — and it cannot pass a screen
/// that is wrong but stable, because the comparison still runs afterwards.
fn settle_and_capture(qmp: &mut Qmp, shot: &Path) -> R<Vec<u8>> {
    let mut prev: Option<Vec<u8>> = None;
    for _ in 0..SCREEN_SETTLE_TRIES {
        qmp.screendump(shot)?;
        let bytes =
            fs::read(shot).map_err(|e| format!("read screendump {}: {e}", shot.display()))?;
        if prev.as_deref() == Some(bytes.as_slice()) {
            return Ok(bytes);
        }
        prev = Some(bytes);
        std::thread::sleep(SCREEN_SETTLE_WAIT);
    }
    Err(format!(
        "the guest's screen never stopped changing over {SCREEN_SETTLE_TRIES} captures \
         {SCREEN_SETTLE_WAIT:?} apart — a compositor repainting continuously is itself the bug"
    )
    .into())
}

/// `cargo xtask check-display` — prove the pixels actually reach the screen.
///
/// **What a self-hash structurally cannot answer** (`docs/design/display-substrate.md`
/// §8c): a compositor can hash its own buffer correctly while writing to the wrong base
/// address, the wrong stride, or with the channels swapped. Nothing inside the guest can
/// detect that, because the guest stays perfectly consistent with itself. So the picture
/// has to be read from outside, which is what QEMU's `screendump` is for.
///
/// The division worth remembering: **`test-qemu` tests the compositor, this tests the
/// framebuffer binding.**
///
/// **No golden image.** The expected picture is rendered here by the same `libdraw` the
/// guest uses, so there is no binary artefact in the repo to regenerate, and no
/// brittle-image maintenance problem. That is sound precisely because what is under test
/// is the *binding* — base address, stride, channel order — not the compositing, which
/// §8b already covers.
///
/// `cargo xtask preview [ui|term|all]` — render the toolkit on the host and write it as a PNG.
///
/// **The whole point is that a judgement about how something looks should cost a glance rather
/// than a boot** (M11 Part A). Polish is a hundred small decisions, and a decision that costs
/// three minutes of QEMU is a decision not made — so this puts the same renders `check-display`
/// adjudicates against into a file anyone can open.
///
/// **The same renderer, deliberately.** `xtask` already links `libui`, `libdraw` and `libterm`
/// because the display gate renders the expected picture here rather than checking in a golden
/// file. This is a second entry point onto that, not a second renderer: a preview that could
/// differ from what the gate demands of the guest would be a picture of nothing in particular.
///
/// **What it cannot show**, said plainly so nobody reads more into it: anything the *compositor*
/// draws — the cursor, the drag outline, the background between windows — and the arrangement of
/// real windows on a real screen. Those are composed in the guest by clients that have to run.
/// What this covers is the toolkit's own surfaces, which is where most of the polish lives.
fn cmd_preview(what: &str) -> R<()> {
    let faces = host_faces()?;
    let dir = build_cache();
    fs::create_dir_all(&dir).ok();
    let frames = preview_frames(&faces);
    let names: Vec<&str> = frames.iter().map(|(n, _)| *n).collect();
    if what != "all" && !names.contains(&what) {
        // Checked before anything is drawn: the first version reported this by rendering both
        // references a second time purely to list their names.
        return Err(format!(
            "no preview called {what:?} — try `all` or one of: {}",
            names.join(", ")
        )
        .into());
    }
    for (name, frame) in &frames {
        if what != "all" && what != *name {
            continue;
        }
        let (w, h, rgb) = rgb_of(frame);
        let path = dir.join(format!("preview-{name}.png"));
        write_png(&path, w, h, &rgb)?;
        println!("xtask: {} ({w}x{h})", path.display());
    }
    Ok(())
}

/// `cargo xtask tune [--ground N] [--side N] [--radius N] [--strength N] [--drop N]` — try the
/// overview's opacity and a window's shadow **without booting**.
///
/// **The loop `preview` exists for, pointed at the two things `preview` could not show** (M13
/// Part C). `preview` renders the toolkit's own surfaces; the overview's translucency and a
/// window's shadow are *compositing*, which happened only in the guest — so judging either cost a
/// three-minute boot, and picking a number by eye needs half a dozen looks.
///
/// **The desktop underneath is the real one**, read back from `shot-windows.png` when a
/// `cargo xtask shot` has left one there, and the wallpaper otherwise. That is what makes the
/// answer trustworthy: the question is how much of the *actual* desktop should show through, and
/// a mock of it would be a picture of my guess rather than of the screen. The overview's own
/// content is stood in for — rectangles where thumbnails go — because the thing being judged is
/// the ground behind them, not the labels on them.
///
/// It writes `tune-shadow.png` and `tune-overview.png`, and prints the values it used so a good
/// one can be copied into `libdraw::theme::WINDOW_SHADOW` and `desktop-shell`'s constants.
fn cmd_tune(args: &[String]) -> R<()> {
    use libdraw::compose::{Shadow, SurfaceRef, compose_exposed};
    use libdraw::format::{PixelFormat, Rgb};
    use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
    use libdraw::geom::{Point, Rect};

    let num = |name: &str, default: u32| -> R<u32> {
        match args.iter().position(|a| a == name) {
            Some(i) => args
                .get(i + 1)
                .ok_or_else(|| format!("{name} wants a number"))?
                .parse::<u32>()
                .map_err(|e| format!("{name}: {e}").into()),
            None => Ok(default),
        }
    };
    let theme_shadow = libdraw::theme::WINDOW_SHADOW;
    let shadow = Shadow {
        radius: num("--radius", theme_shadow.radius)?,
        offset: Point::new(0, num("--drop", theme_shadow.offset.y as u32)? as i32),
        colour: theme_shadow.colour,
        strength: num("--strength", theme_shadow.strength as u32)?.min(255) as u8,
    };
    let ground_alpha = num("--ground", 210)?.min(255) as u8;
    let side_alpha = num("--side", 150)?.min(255) as u8;

    let (sw, sh) = (1280u32, 800u32);
    let g = Geometry::packed(sw, sh, PixelFormat::XRGB8888);
    let dir = build_cache();
    fs::create_dir_all(&dir).ok();

    // --- the desktop underneath: the real screen if one has been photographed ---
    let mut desktop = MemFramebuffer::new(g);
    let shot = dir.join("shot-windows.png");
    let from_shot = read_rgb_png(&shot).ok().filter(|(w, h, _)| *w == sw && *h == sh);
    match &from_shot {
        Some((_, _, rgb)) => {
            for y in 0..sh {
                for x in 0..sw {
                    let i = (y as usize * sw as usize + x as usize) * 3;
                    desktop.put_pixel(x, y, Rgb::new(rgb[i], rgb[i + 1], rgb[i + 2]));
                }
            }
        }
        None => {
            let (px, wg) = wallpaper_for_screen(sw, sh)?;
            for y in 0..sh {
                for x in 0..sw {
                    let off = wg.offset_of(x, y).unwrap_or(0);
                    let word =
                        u32::from_le_bytes([px[off], px[off + 1], px[off + 2], px[off + 3]]);
                    desktop.put_pixel(x, y, wg.format.decode(word));
                }
            }
        }
    }

    // --- 1. the shadow, over a wallpaper with two mock windows ---
    //
    // Drawn fresh rather than over the screendump, because the screendump already *has* the
    // shipped shadow baked into it: comparing a new one against it would be comparing two
    // shadows and calling the sum an answer.
    let (wp, wg) = wallpaper_for_screen(sw, sh)?;
    let mut shot_fb = MemFramebuffer::new(g);
    let faces = host_faces()?;
    let ui = reference_frame(&faces, "ui")?;
    let (uw, uh) = {
        let ug = Framebuffer::geometry(&ui);
        (ug.width, ug.height)
    };
    let panel = MemFramebuffer::filled(Geometry::packed(sw, 24, PixelFormat::XRGB8888), Rgb::new(0xEC, 0xEC, 0xEC));
    let wall = SurfaceRef::new(wg, Point::new(0, 0), &wp);
    let bar = SurfaceRef::new(panel.geometry(), Point::new(0, 0), panel.bytes());
    let a = SurfaceRef::new(Framebuffer::geometry(&ui), Point::new(120, 140), ui.bytes())
        .with_shadow(shadow);
    let b = SurfaceRef::new(Framebuffer::geometry(&ui), Point::new(120 + uw as i32 / 2, 140 + uh as i32 / 2), ui.bytes())
        .with_shadow(shadow);
    compose_exposed(&mut shot_fb, Rgb::new(0x2A, 0x55, 0x70), &[wall, bar, a, b], &[g.bounds()]);
    let (w1, h1, rgb1) = rgb_of(&shot_fb);
    let p1 = dir.join("tune-shadow.png");
    write_png(&p1, w1, h1, &rgb1)?;

    // --- 2. the overview's ground and sidebar, over the real desktop ---
    let mut over = desktop;
    let side_w = 200u32;
    // **The bars stay uncovered**, which is what the guest does — the shell's two panels sit above
    // the overview popup, and a mock that dimmed them would show a picture the screen never has.
    // Compare `shot-overview.png`: its top bar and taskbar are at full brightness.
    let bar_h = 24u32;
    let body = Rect::new(0, bar_h as i32, sw, sh - bar_h * 2);
    let side = Rect::new((sw - side_w) as i32, bar_h as i32, side_w, sh - bar_h * 2);
    // The ground first, then the sidebar over it, then opaque stand-ins for the thumbnails —
    // the same order `render_overview` uses, so the layering is the shell's rather than a
    // rearrangement of it.
    blend_rect(&mut over, body, Rgb::new(0, 0, 0), ground_alpha);
    blend_rect(&mut over, side, Rgb::new(0x1B, 0x3A, 0x4E), side_alpha);
    for (i, r) in [Rect::new(60, 90, 420, 300), Rect::new(540, 210, 400, 280)].iter().enumerate() {
        let tint = if i == 0 { Rgb::new(0xF2, 0xF2, 0xF2) } else { Rgb::new(0x10, 0x14, 0x18) };
        over.fill_rect(*r, tint);
    }
    for row in 0..2u32 {
        over.fill_rect(
            Rect::new(side.origin.x + 12, bar_h as i32 + 16 + row as i32 * 90, side_w - 90, 70),
            Rgb::new(0x9F, 0xB8, 0xC8),
        );
    }
    let (w2, h2, rgb2) = rgb_of(&over);
    let p2 = dir.join("tune-overview.png");
    write_png(&p2, w2, h2, &rgb2)?;

    println!(
        "xtask: shadow radius {} drop {} strength {}  ->  {}",
        shadow.radius,
        shadow.offset.y,
        shadow.strength,
        p1.display()
    );
    println!(
        "xtask: overview ground {ground_alpha} sidebar {side_alpha}  ->  {}{}",
        p2.display(),
        if from_shot.is_some() { "  (over the real screendump)" } else { "  (over the wallpaper — run `xtask shot` for the real desktop)" }
    );
    Ok(())
}

/// The staged wallpaper, cropped and scaled to a `sw`x`sh` screen, as XRGB8888 pixels.
///
/// The same crop the image build stages and the same downscale the shell performs, so the ground
/// under a preview is the ground the guest would show.
fn wallpaper_for_screen(
    sw: u32,
    sh: u32,
) -> R<(Vec<u8>, libdraw::framebuffer::Geometry)> {
    use libdraw::format::{PixelFormat, Rgb};
    use libdraw::framebuffer::Geometry;
    let path = repo_root().join(WALLPAPER_ASSET);
    let (iw, ih, rgb) = read_rgb_png(&path)?;
    let top = ((ih.saturating_sub(WALLPAPER_H)) / 2) as usize;
    let src_g = Geometry::packed(iw, WALLPAPER_H.min(ih), PixelFormat::XRGB8888);
    let mut src = vec![0u8; src_g.byte_len()];
    for y in 0..src_g.height {
        for x in 0..iw {
            let i = ((top + y as usize) * iw as usize + x as usize) * 3;
            let c = Rgb::new(rgb[i], rgb[i + 1], rgb[i + 2]);
            let off = src_g.offset_of(x, y).unwrap_or(0);
            src[off..off + 4].copy_from_slice(&src_g.format.encode(c).to_le_bytes());
        }
    }
    let dst_g = Geometry::packed(sw, sh, PixelFormat::XRGB8888);
    let mut dst = vec![0u8; dst_g.byte_len()];
    if !libdraw::scale::box_downscale(&src, src_g, &mut dst, dst_g) {
        return Err("the wallpaper would not scale to the preview screen".into());
    }
    Ok((dst, dst_g))
}

/// Fill `rect` by blending `colour` into what is already there — the host's stand-in for a
/// translucent surface the compositor would blend at scanout.
fn blend_rect(
    fb: &mut libdraw::framebuffer::MemFramebuffer,
    rect: libdraw::geom::Rect,
    colour: libdraw::format::Rgb,
    alpha: u8,
) {
    use libdraw::framebuffer::Framebuffer;
    let bounds = Framebuffer::geometry(fb).bounds();
    let Some(area) = rect.intersect(&bounds) else { return };
    for y in area.origin.y..area.bottom() as i32 {
        for x in area.origin.x..area.right() as i32 {
            fb.blend_pixel(x as u32, y as u32, colour, alpha);
        }
    }
}

/// Read an 8-bit RGB PNG back as `(width, height, rgb)`.
fn read_rgb_png(path: &std::path::Path) -> R<(u32, u32, Vec<u8>)> {
    let file = std::io::BufReader::new(fs::File::open(path)?);
    let mut reader = png::Decoder::new(file).read_info()?;
    let info = reader.info().clone();
    if info.color_type != png::ColorType::Rgb || info.bit_depth != png::BitDepth::Eight {
        return Err(format!("{}: expected 8-bit RGB", path.display()).into());
    }
    let size = reader.output_buffer_size().ok_or("image too large")?;
    let mut buf = vec![0u8; size];
    let frame = reader.next_frame(&mut buf)?;
    buf.truncate(frame.buffer_size());
    Ok((info.width, info.height, buf))
}

/// `cargo xtask shot [all|greeter|desktop|apps|windows|overview]` — photograph the running
/// desktop.
///
/// **The other half of `preview`, and the half it said it could not be.** Part A's command
/// renders the toolkit's own surfaces on the host in about a second, and its doc names what that
/// structurally cannot show: anything the *compositor* draws — the cursor, the drag outline, the
/// ground between windows — and the arrangement of real windows on a real screen. Those are
/// composed in the guest by clients that have to run, so the only honest way to look at them is
/// to boot and take the picture.
///
/// **A photograph, not a render**, which is what keeps this out of the "two sources for one
/// answer" trap that `preview_frames` exists to avoid: nothing here draws anything. It boots the
/// **release** image — the one a person would use, with no `--selftest` clients on the screen —
/// drives it to each moment worth looking at, and writes what QEMU says is on the display.
///
/// **Several moments per boot**, because the boot is the cost. One run gives the greeter, the
/// bare desktop, the applications modal, a screen with real windows on it, and the overview — a
/// polish list is written against all five. The overview is there because it is the one surface
/// with no other way to be looked at: it covers the screen and closes when anything else is
/// clicked.
///
/// It is a tool and not a gate: it asserts only enough to know the picture is of a working
/// desktop rather than of a blank screen, which is the one failure that would otherwise be
/// mistaken for a design opinion.
fn cmd_shot(what: &str, accel: Accel) -> R<()> {
    const MOMENTS: [&str; 5] = ["greeter", "desktop", "apps", "windows", "overview"];
    if what != "all" && !MOMENTS.contains(&what) {
        return Err(format!(
            "no shot called {what:?} — try `all` or one of: {}",
            MOMENTS.join(", ")
        )
        .into());
    }
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let dump = work.join("shot.ppm");
    let qmp_sock = work.join("qmp-shot.sock");
    let (mut session, mut qmp) = spawn_release_guest(accel, "shot", &qmp_sock)?;

    // A closure would borrow both halves for the rest of the function, so the capture is a
    // statement each time — four lines, and no plumbing to read past.
    macro_rules! capture {
        ($name:expr) => {
            if what == "all" || what == $name {
                let ppm = settle_and_capture(&mut qmp, &dump)?;
                let (w, h, rgb) = parse_ppm(&ppm)?;
                let path = work.join(concat!("shot-", $name, ".png"));
                write_png(&path, w, h, &rgb)?;
                println!("  shot: {} ({w}x{h})", path.display());
            }
        };
    }

    println!("xtask: booting the release image to photograph it…\n");

    // 1. **The greeter**, which in a release image is the only window there is.
    session.expect("desktop-session-mgr: greeter presented")?;
    // The pointer somewhere a person would leave it, so the cursor is in the picture. Without
    // this it sits whereever QEMU starts it, which is the top-left corner and under the window.
    move_pointer_to(&mut qmp, 640, 400)?;
    capture!("greeter");

    // 2. **A session.** No wrong password here — that is `check-login`'s claim to make, and a
    //    tool that took twice as long to produce the same pictures would be a tool used less.
    type_at_greeter(&mut qmp, &mut session, DEMO_USER)?;
    press(&mut qmp, "tab")?;
    type_at_greeter(&mut qmp, &mut session, DEMO_PASSWORD)?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: up (graphical session leader)")?;
    // Both bars, because a desktop missing one is exactly the picture that would be mistaken
    // for a design decision rather than a broken boot. In the order the shell prints them —
    // `expect` scans forward, so a pair asserted by topic rather than by position in the stream
    // times out on output that was there.
    // **The clock read the wall clock**, which is the one thing about it a gate can see: its
    // value changes every minute and the bar is pixels this boot has no reference render of. The
    // line distinguishes a clock that is absent because the RTC was unreadable from a bar that
    // failed to draw one — and the *formatting* is a host test in `libtime`, where it belongs.
    session.expect("desktop-shell: clock ")?;
    session.expect("desktop-shell: top bar presented, window ")?;
    session.expect("desktop-shell: bottom bar placed at 0,776")?;
    capture!("desktop");

    // 3. **The applications modal**, the one piece of chrome with no window of its own: a popup
    //    over the desktop, which is where the toolkit's list rows are seen at their real size.
    click_at(&mut qmp, &mut session, 60, 12)?;
    session.expect("desktop-shell: applications modal open")?;
    capture!("apps");

    // 4. **Real windows.** Two applications rather than one, because half of what a desktop
    //    looks like is how two windows sit next to each other — and one of each kind: a
    //    proportional-font application and the terminal, which is the only grid on the screen.
    launch_from_modal(&mut qmp, &mut session, "nxfiles")?;
    // **Escape before aiming at the button again**, the precondition `check-login` states for
    // the same click: `click_at` retries a press that did not land, and an abandoned attempt
    // still pressed *somewhere* — if that somewhere was the applications button the modal is
    // already open, the aimed click opens no second one, and the wait below never ends.
    press(&mut qmp, "esc")?;
    session.skip_to_end()?;
    click_at(&mut qmp, &mut session, 60, 12)?;
    session.expect("desktop-shell: applications modal open")?;
    // And drawn, before a keystroke is aimed at it.
    let _ = settle_and_capture(&mut qmp, &dump)?;
    launch_from_modal(&mut qmp, &mut session, "nxterm")?;
    // The shell cascades what it places, so the two land offset rather than stacked.
    //
    // **No wait here**: `launch_from_modal` already waited for the shell to place the window,
    // which is the stronger receipt — and the terminal's own startup lines come *before* that, so
    // an `expect` for one of them scans past output that was already there.
    move_pointer_to(&mut qmp, 900, 500)?;
    capture!("windows");

    // 5. **The overview**, which is the one surface with no other way to be looked at: it is
    //    opened from the desktop indicator, it covers the screen, and it is where the sidebar's
    //    desktop miniatures live (M11 Part E batch 10).
    //    **Pressed by hand rather than through `click_at`**, because the shell logs the open
    //    while it routes the press and the compositor logs the press when it *delivers* the
    //    routed record — so the open comes first, and `click_at`'s own position assertion scans
    //    past it. Nothing is lost: a press that misses simply does not open the overview, and the
    //    wait below fails.
    const OVERVIEW_AT: (i32, i32) = (1200, 788);
    move_pointer_to(&mut qmp, OVERVIEW_AT.0, OVERVIEW_AT.1)?;
    qmp.send_button("left", true)?;
    qmp.send_button("left", false)?;
    session.expect("desktop-shell: overview open, window ")?;
    qmp.pointer = Some(OVERVIEW_AT);
    capture!("overview");

    let _ = fs::remove_file(&qmp_sock);
    println!("\nxtask: shots written to {}", work.display());
    Ok(())
}

/// Type a program's name into the open applications modal and launch it.
///
/// The modal is a `popup`, so it holds the keyboard — the same property `check-login` relies on.
fn launch_from_modal(qmp: &mut Qmp, session: &mut Session, program: &str) -> R<()> {
    for c in program.chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
    }
    press(qmp, "ret")?;
    session.expect(&format!("desktop-shell: launched {program} into its own namespace"))?;
    session.expect("desktop-shell: applications modal closed")?;
    // **And wait until its window is on screen**, which is not the same claim and is the one the
    // next step needs. A launch returns when the *shell* has spawned the program; the program
    // then starts, creates a window, and the compositor focuses it — after the modal for the
    // *next* launch has already opened. Typing into that modal put the second program's name
    // into the first program, and in a file browser Enter means "open the selected row", so the
    // shot ended up with an editor on `theme.toml` instead of a terminal.
    session.expect("desktop-shell: placed window ")?;
    Ok(())
}

/// Boot the **release** image headless with a QMP socket and the serial on stdio.
///
/// Shared by `check-login` and `shot`, which are the two things that boot the image a person
/// would actually use — every other gate boots `--selftest`. The three that do build this
/// command themselves and differ from each other in the image mode and the flags; these two are
/// identical, and two identical copies of a boot are how the second one quietly stops matching.
fn spawn_release_guest(accel: Accel, gate: &'static str, qmp_sock: &Path) -> R<(Session, Qmp)> {
    let ovmf = locate_ovmf()?;
    // Removed rather than reused: a socket left by a killed run is a file `Qmp::connect` will
    // open and never get an answer from. The *caller* owns the path, because it is also what
    // gets cleaned up at the end of a gate.
    let _ = fs::remove_file(qmp_sock);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel)?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
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

    let session = Session::spawn(cmd, gate)?;
    let qmp = Qmp::connect(qmp_sock)?;
    Ok((session, qmp))
}

/// The repository file the image build stages at `guest_path`.
///
/// **The one place a guest font path becomes a host file**, used by the staging *and* by every
/// host-side render. That is what makes the gate's central claim true rather than asserted: the
/// picture drawn here is drawn with the same bytes the guest read off ext4, because both come
/// from this function and both paths come from `Theme::light()`. Point the built-in theme at a
/// different face and the image, the previews and the gate all follow it — or, if nothing stages
/// that face, all three fail together and say so.
fn font_asset(guest_path: &str) -> R<PathBuf> {
    let name = guest_path.strip_prefix(FONT_DIR).filter(|n| !n.contains('/')).ok_or_else(|| {
        // **Without saying where the path came from**, because two callers supply it: the
        // built-in theme, for the staging and the reference renders, and `check-terminal`, whose
        // path arrives off the guest's serial line and can therefore be a user's `theme.toml`.
        // Naming the wrong one would send a reader to `Theme::light()` for a value that is in a
        // file on the disk (PR #264 review, finding 1).
        format!(
            "{guest_path:?} is not a file directly under {FONT_DIR} — the image build stages \
             that directory and nothing else, so the host has no way to render with it"
        )
    })?;
    Ok(repo_root().join("assets/fonts").join(name))
}

/// Where the image build binds the fonts, and therefore where a themeable path starts.
const FONT_DIR: &str = "/system/fonts/";

/// One host-side face, loaded from the path a theme names.
fn host_font(guest_path: &str) -> R<libdraw::text::Font> {
    let path = font_asset(guest_path)?;
    libdraw::text::Font::from_bytes(
        fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?,
    )
    .ok_or_else(|| format!("{} did not parse on the host", path.display()).into())
}

/// Both faces the built-in theme names, in the order [`preview_frames`] wants them.
///
/// **The built-in theme rather than the staged file**, and the difference matters: the file
/// carries a deliberately non-default `font_px` for `check-login` to read back, while the guest
/// client whose pictures this gate compares — `ui-testclient` — gets no setup record at all and
/// draws with `Theme::light()`. Rendering the host's reference from the file would compare two
/// different themes and call the difference a display bug.
fn host_faces() -> R<(libdraw::text::Font, libdraw::text::Font)> {
    let t = libdraw::theme::Theme::light();
    Ok((host_font(t.font_ui.as_str())?, host_font(t.font_mono.as_str())?))
}

/// One reference render by name, for the gate that compares it against a guest.
///
/// **The gate reads this rather than calling the renderer itself**, which is what makes
/// `preview_frames` the single source it claims to be: adding a region to `check-display` and
/// adding a preview are one change, and a preview that stopped being the gate's picture fails
/// against the guest instead of quietly becoming a picture of nothing (PR #261 review, finding 2).
fn reference_frame(
    faces: &(libdraw::text::Font, libdraw::text::Font),
    name: &str,
) -> R<libdraw::framebuffer::MemFramebuffer> {
    preview_frames(faces)
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, f)| f)
        .ok_or_else(|| format!("no reference render called {name:?}").into())
}

/// Every arrangement the host can render, by name.
///
/// **The one place either the preview or the display gate builds a reference picture.** Both used
/// to construct their own; the comment claiming they were the same renders was true and unenforced
/// until it was tested, and a solid rectangle at the right size passed everything.
/// **Each arrangement takes the face its guest counterpart loads** (M11 Part D): the toolkit's
/// window is the desktop's proportional font and the terminal's is the fixed-advance one. They
/// took one font between them until Part D, because the system had only one.
fn preview_frames(
    (ui, mono): &(libdraw::text::Font, libdraw::text::Font),
) -> Vec<(&'static str, libdraw::framebuffer::MemFramebuffer)> {
    vec![
        ("ui", libui::reference::render(ui)),
        ("term", libterm::render::reference::render_with(mono)),
    ]
}

/// A framebuffer's visible pixels as RGB triples, row-major.
///
/// **Read through `Framebuffer::get_pixel` rather than off the bytes**, which is not laziness:
/// the toolkit reference's pitch is 1292 for a 1280-byte row, and `XRGB8888` stores
/// little-endian — so the bytes run blue, green, red, pad. Walking the buffer directly is two
/// chances to be wrong (a stride and a channel order) in code whose only job is to be a faithful
/// copy, and `libdraw` already answers both questions correctly for its own compositing.
fn rgb_of(fb: &libdraw::framebuffer::MemFramebuffer) -> (u32, u32, Vec<u8>) {
    use libdraw::framebuffer::Framebuffer;
    let g = fb.geometry();
    let mut out = Vec::with_capacity((g.width as usize) * (g.height as usize) * 3);
    for y in 0..g.height {
        for x in 0..g.width {
            let p = Framebuffer::get_pixel(fb, x, y).unwrap_or_default();
            out.extend_from_slice(&[p.r, p.g, p.b]);
        }
    }
    (g.width, g.height, out)
}

/// Write `rgb` — `w * h` triples — to `path` as an 8-bit RGB PNG.
fn write_png(path: &std::path::Path, w: u32, h: u32, rgb: &[u8]) -> R<()> {
    fs::write(path, encode_png(w, h, rgb)?).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// The PNG's bytes, for a caller that wants them without a file — which is what lets the test
/// below decode what was encoded rather than trusting that it round-trips.
fn encode_png(w: u32, h: u32, rgb: &[u8]) -> R<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(|e| format!("png header: {e}"))?;
        writer.write_image_data(rgb).map_err(|e| format!("png data: {e}"))?;
    }
    Ok(out)
}

/// The staged wallpaper's bytes: [`WALLPAPER_ASSET`], centre-cropped to 16:10.
///
/// **The repository keeps the photograph as supplied and the crop happens here.** It is 4:3 and
/// the screen is 16:10, so fitting it whole leaves 107 pixels of desktop colour down each side —
/// which is what it looked like, and not what a wallpaper should. Cropping in the build rather
/// than committing a pre-cropped file means nothing is lost and the decision is a dozen lines
/// somebody can read rather than something baked into a binary nobody can review.
///
/// **Centre, and only vertically.** The crop takes the full width and drops equal numbers of
/// rows from the top and bottom, which for this picture is empty water; a horizontal crop would
/// have to decide which diver to lose.
///
/// This replaced a generated gradient, which existed so that no unreviewable binary was
/// load-bearing for a gate. That argument is weaker than it sounds now the asset is *content* a
/// person chose rather than a fixture: what the gate needs from it is its size, and that is
/// asserted against [`WALLPAPER_W`] and [`WALLPAPER_H`] here, from the same crop that produced
/// it.
fn wallpaper_png() -> R<Vec<u8>> {
    let path = repo_root().join(WALLPAPER_ASSET);
    // `BufRead`, which the decoder wants: it reads chunk headers a few bytes at a time.
    let file = std::io::BufReader::new(
        fs::File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?,
    );
    let mut reader = png::Decoder::new(file)
        .read_info()
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let info = reader.info().clone();
    if info.color_type != png::ColorType::Rgb || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "{}: the staged wallpaper must be 8-bit RGB, not {:?}/{:?}",
            path.display(),
            info.color_type,
            info.bit_depth
        )
        .into());
    }
    // **An overflow guard, not an interlace check.** `png`'s own doc: "Returns `None` if the
    // output buffer does not fit into the memory space of the machine." An interlaced RGB8 file
    // returns `Some` and de-interlaces into the same layout, so this would pass it straight
    // through — which is fine here, since the guest's decoder is the one that refuses
    // interlacing and this is a host-side crop. The first version of this comment said the
    // opposite (PR #273 review, optional 4).
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| format!("{}: the decoder cannot size this image", path.display()))?;
    let mut src = vec![0u8; size];
    let frame = reader.next_frame(&mut src).map_err(|e| format!("decode: {e}"))?;
    let (sw, sh) = (frame.width, frame.height);
    if sw != WALLPAPER_W || sh < WALLPAPER_H {
        return Err(format!(
            "{}: expected {WALLPAPER_W} wide and at least {WALLPAPER_H} tall, got {sw}x{sh}",
            path.display()
        )
        .into());
    }
    let top = ((sh - WALLPAPER_H) / 2) as usize;
    let row = sw as usize * 3;
    let mut rgb = Vec::with_capacity(row * WALLPAPER_H as usize);
    for y in 0..WALLPAPER_H as usize {
        let o = (top + y) * row;
        rgb.extend_from_slice(&src[o..o + row]);
    }
    encode_png(WALLPAPER_W, WALLPAPER_H, &rgb)
}

/// A **smoke gate, not a per-commit one**: it boots a full image and compares an image,
/// so the plan runs it once per display-arm change.
fn cmd_check_display(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    let work = repo_root().join("tools/build-cache");
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp.sock");
    let shot = work.join("screendump.ppm");
    let _ = fs::remove_file(&qmp_sock);
    let _ = fs::remove_file(&shot);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel)?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        // The machine protocol channel: `screendump` here, `input-send-event` in M3.
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
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

    println!("xtask: display gate — booting and capturing the screen…\n");
    let mut session = Session::spawn(cmd, "check-display")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;

    // The guest paces this: capture only once it says the scene is on the screen.
    // Screendumping on a timer would race the boot and produce a blank or half-drawn
    // frame that looks like a real mismatch.
    //
    // Since M2 Part D the scene arrives through the **whole Surface protocol** — a real
    // client sharing memory with the compositor — rather than being written straight to
    // the aperture. The client emits this only after a `Release` acknowledges its final
    // commit, so the frame is composited by the time we capture.
    // **Which two faces the guest read, in the order it reads them** (M11 Part D).
    //
    // The pixel comparison below cannot make this claim on its own, and the reason is worth
    // stating because it is the trap this whole gate is shaped around: it checks that the host
    // and the guest *agree*, so a swap that happened on both sides at once would still be green.
    // Rendering the toolkit's window in the terminal's font is exactly that kind of change —
    // one constant, two call sites — and it is what the system did before this part.
    //
    // These lines pin the guest's half. The host's is pinned by `host_faces`, which takes both
    // paths from `Theme::light()`, and by `libdraw`'s own test that the two are different files.
    //
    // **First, because `expect` consumes forward.** Both are printed before `ui-testclient`
    // opens a window at all, so asserting them after any line further down the boot scans past
    // them and times out on output that was there — the failure mode PR #258's review named.
    {
        let t = libdraw::theme::Theme::light();
        session.expect(&format!("ui-testclient: font loaded {}", t.font_ui.as_str()))?;
        session.expect(&format!("ui-testclient: font loaded {}", t.font_mono.as_str()))?;
        println!("  ok: the guest loaded the desktop's face and the terminal's, in that order");
    }

    // **The deadline that keeps a held configure from becoming a lost window (M6 B4).**
    //
    // Asserted from the compositor's own log because the client cannot see *why* it was
    // released — a configure is a configure. The trigger is deterministic rather than lucky:
    // the B3 probe below calls `Window::new` while holding the manager channel, so it blocks
    // waiting for an answer that only it could give and provably will not. That is exactly the
    // wedged-shell case, produced on purpose.
    //
    // Without this line the deadline is still load-bearing — remove it and the probe hangs —
    // but the gate would report a 45-second timeout rather than naming what broke.
    session.expect("compositor: no manager answer for window")?;

    // **The other half of the seam (M6 B3): the manager is *told*, not just obeyed.** B1
    // proved a manager can act on the compositor; nothing proved the compositor reports back.
    // The client watches one window's whole life on the manager channel — created, focus,
    // geometry after a move, destroyed — and `fail()`s naming whichever record did not
    // arrive, so a broken event path reports itself here rather than as a screen that happens
    // to still match. The probe window is destroyed before the scene is captured, so it
    // changes no pixel this gate compares, and it runs *first* — creating and destroying a
    // window is the noisiest thing this client does, so it happens before the placements
    // rather than leaving a full-screen repaint racing the capture.
    session.expect("ui-testclient: manager saw created, focus, geometry and destroyed")?;

    // **Placement before first paint (M6 B4): the ordering rule, not just the event.** The
    // client creates a window through the raw transport, checks that no configure has arrived,
    // answers as the manager with an origin nothing else produces, and only then reads the
    // configure — asserting it carries that origin. A compositor that sent the configure at
    // once still passes every other gate here: the window would simply appear at the default
    // origin and jump when placed, which no screen comparison taken afterwards can see.
    session.expect("ui-testclient: the first configure carried the manager's placement")?;

    // **A popup is placed by its creator (M6 C1).** The client creates a parent, has the manager
    // move it somewhere the default never puts a window, then creates a popup at a negative-x
    // offset and reads the geometry back through `/dev/draw/<id>/info`. An offset resolved
    // against the origin instead of against the parent is a different answer, and a popup held
    // for a manager that by design never places popups would still be waiting.
    session.expect("ui-testclient: a popup was placed by its creator, without the manager")?;

    // **And the other parented role goes the other way.** A `dialog` names a parent but a
    // manager places it, so it is held like a `normal` and its creator's offset is ignored. The
    // hold lives in the compositor *binary*, which no host test builds — merge the two roles
    // back into one arm and every other gate here stays green.
    session.expect("ui-testclient: a dialog was held for the manager and placed by it")?;

    // **The manager seam, asserted rather than attempted (M6 B1).** `ui-testclient` places its
    // reference windows through `/dev/draw/manage` and falls back to the compositor's default
    // placement if the resolve fails — deliberately, since it is a test fixture and not a real
    // session's manager. That fallback is silent by design, which means without these two lines
    // the whole manager path could break and every gate would still pass: the windows land at
    // the origin either way, because the origin *is* the default.
    //
    // The second line is the contract B1 exists to state: one manager at a time. The compositor
    // refuses a second resolve rather than deposing the first, and until the client asked twice
    // that branch was unreachable, so the rule was pinned by nothing at all.
    // **M8 Part A: the desktop requests, driven the way a client drives them.** The filtering
    // itself is pinned by `compositor`'s own tests, over the same functions the binary calls.
    // What those cannot reach is the wire — that a body encoded by a client, sent down
    // `/dev/draw/manage`, lands in the right arm of `dispatch` and is answered — which is the
    // gap PR #233's title cap fell into. The client sets a desktop and a minimized flag, reads
    // each back through `/dev/draw/<id>/info`, checks a current desktop of 0 is refused, and
    // restores everything, so this changes no pixel the comparison below walks.
    session.expect("ui-testclient: desktop and minimized requests round-tripped through info")?;
    session.expect("ui-testclient: reference windows placed via /dev/draw/manage")?;
    session.expect("ui-testclient: a second /dev/draw/manage was refused")?;

    session.expect("ui-testclient: scene presented via /dev/draw")?;
    // **Capture a screen that has stopped changing, rather than the first one offered.**
    //
    // The client's line says *it* is done, not that the compositor is: a repaint is work the
    // compositor does after the request that caused it was answered, and recompositing
    // 1280x800 is not instant. Capturing immediately caught a torn frame — background filled,
    // windows not yet composited, no cursor — about half the time once `ui-testclient` began
    // creating and destroying a window near the end of its run (M6 B3).
    //
    // Two identical consecutive dumps, not a sleep: a fixed delay is a guess that either
    // wastes time or is too short on a loaded machine, and silently becomes too short again
    // the next time the scene grows. This waits for the property the gate actually needs, and
    // it cannot pass a screen that is wrong but stable — the comparison below still runs.
    let captured = match settle_and_capture(&mut qmp, &shot) {
        Ok(b) => b,
        Err(e) => {
            let _ = session.child.kill();
            return Err(e);
        }
    };
    let (w, h, pixels) = parse_ppm(&captured)?;
    println!("  captured {w}x{h} from the guest's display");

    // The expected image, rendered here rather than stored.
    use libdraw::framebuffer::Framebuffer;
    let expected = libdraw::scene::render_reference();
    let (sw, sh) = (libdraw::scene::SCREEN_WIDTH, libdraw::scene::SCREEN_HEIGHT);
    if w < sw || h < sh {
        let _ = session.child.kill();
        return Err(format!("guest display is {w}x{h}, smaller than the {sw}x{sh} scene").into());
    }

    // **The cursor is on screen**, which nothing else here checks: the comparison below walks
    // only the scene's region at the top-left, and the pointer starts at the screen's centre.
    // Without this the sprite could fail to draw at all and every gate would stay green.
    let (cx, cy) = (w / 2, h / 2);
    let body = (0xFFu8, 0xFFu8, 0xFFu8);
    let mut cursor_px = 0usize;
    for y in cy..(cy + 16).min(h) {
        for x in cx..(cx + 12).min(w) {
            let i = (y as usize * w as usize + x as usize) * 3;
            if (pixels[i], pixels[i + 1], pixels[i + 2]) == body {
                cursor_px += 1;
            }
        }
    }
    if cursor_px == 0 {
        let _ = session.child.kill();
        return Err(format!(
            "no cursor at the screen centre ({cx},{cy}): the pointer sprite is not being drawn"
        )
        .into());
    }
    println!("  ok: cursor visible at ({cx},{cy}) — {cursor_px} body pixels");

    let mut mismatches = 0usize;
    let mut first: Option<(u32, u32, (u8, u8, u8), (u8, u8, u8))> = None;
    for y in 0..sh {
        for x in 0..sw {
            let want = Framebuffer::get_pixel(&expected, x, y).unwrap_or_default();
            let i = (y as usize * w as usize + x as usize) * 3;
            let got = (pixels[i], pixels[i + 1], pixels[i + 2]);
            if got != (want.r, want.g, want.b) {
                mismatches += 1;
                if first.is_none() {
                    first = Some((x, y, got, (want.r, want.g, want.b)));
                }
            }
        }
    }

    // The font both reference renders are drawn with here — the same file the image build
    // stages at `/system/fonts/`, against a guest render made from the bytes it read off the
    // disk. This is the only check anywhere that a font loads on the target at all.
    let faces = host_faces()?;

    // **The terminal's picture**, between the two. Same construction and the same argument as
    // the toolkit's: `libterm` on the host renders the fixed reference stream, the guest renders
    // it from the font it read off ext4, and the two are compared where the smaller windows
    // above do not cover. This is the only place a terminal render is checked against pixels
    // that actually reached a screen — every other check on it is the guest agreeing with
    // itself.
    // **Taken from `preview_frames`, which is what makes "a second entry point, not a second
    // renderer" true rather than claimed.** Both were built here independently until PR #261's
    // review demonstrated the gap: a preview replaced by a solid rectangle *at the gate's
    // dimensions* left every host test passing, because nothing tied the two together but a
    // comment. One source means the drift is not possible, and a wrong source fails here —
    // against a real guest, which is the only thing that can tell a render from a picture.
    let (uw, uh) = (libui::reference::WIDTH, libui::reference::HEIGHT);
    let term = reference_frame(&faces, "term")?;
    // The terminal's size comes from the frame rather than from `reference::size`, for the same
    // reason: two answers to one question are two answers that can differ. The host test still
    // compares them, so the agreement is asserted somewhere.
    let (tw, th) = {
        let g = libdraw::framebuffer::Framebuffer::geometry(&term);
        (g.width, g.height)
    };
    // The stacking the exclusions below assume, stated rather than trusted: each window must sit
    // wholly inside the one beneath it, or a region the gate believes it is comparing is covered
    // by something it is not comparing against — a hole that would be silent.
    if !(sw <= tw && sh <= th && tw <= uw && th <= uh) {
        return Err(format!(
            "the reference windows are no longer nested: scene {sw}x{sh}, terminal {tw}x{th}, \
             toolkit {uw}x{uh}. `ui-testclient` creates them largest-first because windows stack \
             at the origin in creation order; fix the sizes or the order before the exclusions \
             below can mean anything"
        )
        .into());
    }
    // **The window above casts a shadow onto this one** (M13 Part C), so the reference has to
    // carry it or the gate would be comparing against a picture the compositor never draws. Applied
    // through `libdraw`'s own `draw_shadow` with `libdraw`'s own constant — the gate computes its
    // expected answer, and a shadow that stopped reaching the screen now fails here rather than
    // going unnoticed.
    let mut term = term;
    libdraw::compose::draw_shadow(
        &mut term,
        libdraw::geom::Rect::new(0, 0, sw, sh),
        &libdraw::theme::WINDOW_SHADOW,
        &libdraw::geom::Rect::new(0, 0, tw, th),
    );
    let term = term;

    let mut term_mismatches = 0usize;
    let mut term_first: Option<(u32, u32, (u8, u8, u8), (u8, u8, u8))> = None;
    let mut term_compared = 0usize;
    if w >= tw && h >= th {
        for y in 0..th {
            for x in 0..tw {
                if x < sw && y < sh {
                    continue; // the scene's window is on top here
                }
                term_compared += 1;
                let want = Framebuffer::get_pixel(&term, x, y).unwrap_or_default();
                let i = (y as usize * w as usize + x as usize) * 3;
                let got = (pixels[i], pixels[i + 1], pixels[i + 2]);
                if got != (want.r, want.g, want.b) {
                    term_mismatches += 1;
                    if term_first.is_none() {
                        term_first = Some((x, y, got, (want.r, want.g, want.b)));
                    }
                }
            }
        }
    }

    // **The toolkit's window**, at the bottom of the three, and the only check that puts
    // `libui` on a screen.
    //
    // Compared everywhere *except* the rectangles of the windows above it. That exclusion is
    // not a weakening: a compositor that stacked them the other way would fail the comparisons
    // above, so the ordering is still covered.
    let ui = reference_frame(&faces, "ui")?;
    // The terminal's window shadows the toolkit's, for the same reason and from the same source.
    // The scene's shadow reaches at most `radius` past the terminal, which is inside the terminal's
    // own rectangle and therefore inside the region excluded below — so one shadow, not two.
    let mut ui = ui;
    libdraw::compose::draw_shadow(
        &mut ui,
        libdraw::geom::Rect::new(0, 0, tw, th),
        &libdraw::theme::WINDOW_SHADOW,
        &libdraw::geom::Rect::new(0, 0, uw, uh),
    );
    let ui = ui;
    let mut ui_mismatches = 0usize;
    let mut ui_first: Option<(u32, u32, (u8, u8, u8), (u8, u8, u8))> = None;
    let mut ui_compared = 0usize;
    if w >= uw && h >= uh {
        for y in 0..uh {
            for x in 0..uw {
                if x < tw && y < th {
                    // The terminal's window is on top here, and the scene's above that. One
                    // exclusion rather than two, because the nesting was asserted above.
                    continue;
                }
                ui_compared += 1;
                let want = Framebuffer::get_pixel(&ui, x, y).unwrap_or_default();
                let i = (y as usize * w as usize + x as usize) * 3;
                let got = (pixels[i], pixels[i + 1], pixels[i + 2]);
                if got != (want.r, want.g, want.b) {
                    ui_mismatches += 1;
                    if ui_first.is_none() {
                        ui_first = Some((x, y, got, (want.r, want.g, want.b)));
                    }
                }
            }
        }
    }

    // **M8 Part B: the screendump Part A could not take.** Part A's gate box asked for a
    // switched screen compared against a `libdraw` render, and could not have it: the guest had
    // no way to be *told* to switch, because this client parked on `sys_wait` rather than
    // reading anything. A registered chord is that channel — the host injects it over QMP, the
    // guest acts on it, and the screen captured is one the host asked for.
    //
    // **Only when the static comparison already passed.** A switched screen means nothing if
    // the unswitched one was wrong, and running it anyway would report the second failure while
    // hiding the first.
    if mismatches == 0 && term_mismatches == 0 && ui_mismatches == 0 {
        if let Err(e) = desktop_round_trip(&mut qmp, &mut session, &work, w, sw, sh) {
            let _ = session.child.kill();
            let _ = fs::remove_file(&qmp_sock);
            return Err(e);
        }
    }

    let _ = session.child.kill();
    let _ = fs::remove_file(&qmp_sock);

    if mismatches > 0 {
        let (x, y, got, want) = first.expect("a mismatch was counted");
        return Err(format!(
            "display gate FAILED: {mismatches} of {} scene pixels differ.\n  \
             first at ({x},{y}): screen {got:?}, expected {want:?}\n  \
             the capture is at {} — a whole-image shift suggests a base-address or stride \
             error, and swapped components suggest a channel-order one",
            sw as usize * sh as usize,
            shot.display()
        )
        .into());
    }
    if w < tw || h < th {
        return Err(
            format!("guest display is {w}x{h}, smaller than the {tw}x{th} reference terminal")
                .into(),
        );
    }
    if term_mismatches > 0 {
        let (x, y, got, want) = term_first.expect("a mismatch was counted");
        return Err(format!(
            "display gate FAILED: {term_mismatches} of {term_compared} terminal pixels differ.\n  \
             first at ({x},{y}): screen {got:?}, expected {want:?}\n  \
             the capture is at {} — if the whole region is one colour the guest never presented \
             the reference terminal; if only some cells differ, suspect the attribute or cursor \
             render rather than the binding",
            shot.display()
        )
        .into());
    }
    if w < uw || h < uh {
        return Err(format!(
            "guest display is {w}x{h}, smaller than the {uw}x{uh} reference UI"
        )
        .into());
    }
    if ui_mismatches > 0 {
        let (x, y, got, want) = ui_first.expect("a mismatch was counted");
        return Err(format!(
            "display gate FAILED: {ui_mismatches} of {ui_compared} toolkit pixels differ.\n  \
             first at ({x},{y}): screen {got:?}, expected {want:?}\n  \
             the capture is at {} — if the whole region is the background colour the guest \
             never loaded the UI font or never presented its window; if \
             only the glyphs differ, the target rasterised differently from the host",
            shot.display()
        )
        .into());
    }
    println!(
        "\nxtask: display gate PASSED — the {sw}x{sh} scene, {term_compared} pixels of the \
         {tw}x{th} terminal and {ui_compared} pixels of the {uw}x{uh} toolkit window match \
         libdraw, libterm and libui pixel for pixel ✓"
    );
    Ok(())
}

/// Inject the test client's chord, capture the switched screen, and switch back.
///
/// **What this proves that a host test cannot.** `compositor`'s own tests pin the desktop filter
/// over `compose_into`, which is the function the binary composites with — so the *logic* is
/// covered there. What only a screendump can show is the guest being consistent with itself and
/// wrong: a filter that runs but composites into the wrong place, or a switch that leaves the
/// previous frame on screen because nothing repainted. That is the class `check-display` exists
/// for, and until a chord existed there was no way to ask the guest to enter the state.
///
/// The round trip is deliberately both ways. A one-way check passes for a compositor that
/// filtered a window out permanently, or dropped its buffer on the way — the scene coming *back*
/// with no further commit is what says a desktop switch is a filter changing its mind.
fn desktop_round_trip(
    qmp: &mut Qmp,
    session: &mut Session,
    work: &Path,
    screen_w: u32,
    sw: u32,
    sh: u32,
) -> R<()> {
    let shot = work.join("screendump-desktop.ppm");

    // The client says when it is listening. Injecting before that is a race the guest would
    // lose silently — the chord would be delivered to the focused window as an ordinary key.
    session.expect("ui-testclient: hotkey registered, waiting")?;

    // `Super+F1`. The modifier goes down first and comes up last, which is what a person does
    // and what `libinput`'s modifier tracking expects.
    let chord = |qmp: &mut Qmp| -> R<()> {
        qmp.send_key("meta_l", true)?;
        qmp.send_key("f1", true)?;
        qmp.send_key("f1", false)?;
        qmp.send_key("meta_l", false)?;
        Ok(())
    };

    chord(qmp)?;
    session.expect("ui-testclient: hotkey fired -> desktop 2")?;
    let switched = settle_and_capture(qmp, &shot)?;
    let (w2, _, px2) = parse_ppm(&switched)?;
    if w2 != screen_w {
        return Err(format!("the switched capture is {w2} wide, the first was {screen_w}").into());
    }

    // **Every pixel of the scene region is the background.** The reference windows are all on
    // desktop 1, so desktop 2 shows nothing — and this region is where they were, so a filter
    // that did not run leaves their content exactly here.
    // The compositor clears to `libdraw::scene::BACKGROUND` — the same constant the reference
    // render fills with, so host and guest cannot disagree about what "empty" looks like.
    let bgc = libdraw::scene::BACKGROUND;
    let bg = (bgc.r, bgc.g, bgc.b);
    let mut lit = 0usize;
    let mut first_lit = None;
    for y in 0..sh {
        for x in 0..sw {
            let i = (y as usize * w2 as usize + x as usize) * 3;
            let got = (px2[i], px2[i + 1], px2[i + 2]);
            if got != bg {
                lit += 1;
                if first_lit.is_none() {
                    first_lit = Some((x, y, got));
                }
            }
        }
    }
    if lit > 0 {
        let (x, y, got) = first_lit.expect("a lit pixel was counted");
        return Err(format!(
            "display gate FAILED: {lit} of {} pixels are still drawn after switching to an \
             empty desktop.\n  first at ({x},{y}): screen {got:?}, expected the background \
             {bg:?}\n  the capture is at {} — the compositor is compositing windows that are \
             not on the current desktop, or switching did not repaint",
            sw as usize * sh as usize,
            shot.display()
        )
        .into());
    }
    println!("  ok: desktop 2 is empty — {} scene pixels are background", sw as usize * sh as usize);

    // And back. No client commits anything in between, so the scene returning is the filter
    // changing its mind rather than a redraw.
    chord(qmp)?;
    session.expect("ui-testclient: hotkey fired -> desktop 1")?;
    let restored = settle_and_capture(qmp, &shot)?;
    let (w3, _, px3) = parse_ppm(&restored)?;
    use libdraw::framebuffer::Framebuffer;
    let expected = libdraw::scene::render_reference();
    let mut bad = 0usize;
    let mut first_bad = None;
    for y in 0..sh {
        for x in 0..sw {
            let want = Framebuffer::get_pixel(&expected, x, y).unwrap_or_default();
            let i = (y as usize * w3 as usize + x as usize) * 3;
            let got = (px3[i], px3[i + 1], px3[i + 2]);
            if got != (want.r, want.g, want.b) {
                bad += 1;
                if first_bad.is_none() {
                    first_bad = Some((x, y, got, (want.r, want.g, want.b)));
                }
            }
        }
    }
    if bad > 0 {
        let (x, y, got, want) = first_bad.expect("a mismatch was counted");
        return Err(format!(
            "display gate FAILED: {bad} scene pixels differ after switching back.\n  \
             first at ({x},{y}): screen {got:?}, expected {want:?}\n  the capture is at {} — \
             the window came back changed, so a desktop switch is not the pure filter it is \
             specified to be",
            shot.display()
        )
        .into());
    }
    println!("  ok: switching back restored the scene pixel for pixel, with no client commit");
    Ok(())
}

/// Parse a binary PPM (P6) into `(width, height, rgb_bytes)`.
///
/// QEMU writes exactly this format, and it is what `libdraw::ppm` emits, so the two ends
/// of the gate speak the same thing. Hand-rolled because it is a header and raw bytes.
fn parse_ppm(data: &[u8]) -> R<(u32, u32, Vec<u8>)> {
    // Header fields are whitespace-separated ASCII: "P6", width, height, maxval, then a
    // single whitespace byte and the body.
    let mut fields: Vec<String> = Vec::new();
    let mut i = 0usize;
    while fields.len() < 4 && i < data.len() {
        while i < data.len() && data[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < data.len() && data[i] == b'#' {
            while i < data.len() && data[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < data.len() && !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..i]).into_owned());
    }
    if fields.len() != 4 || fields[0] != "P6" {
        return Err(format!("not a binary PPM (fields: {fields:?})").into());
    }
    let w: u32 = fields[1].parse().map_err(|_| "bad PPM width")?;
    let h: u32 = fields[2].parse().map_err(|_| "bad PPM height")?;
    if fields[3] != "255" {
        return Err(format!("unsupported PPM maxval {}", fields[3]).into());
    }
    i += 1; // the single whitespace byte after maxval
    let need = w as usize * h as usize * 3;
    if data.len() < i + need {
        return Err(format!("PPM body short: {} < {need}", data.len() - i).into());
    }
    Ok((w, h, data[i..i + need].to_vec()))
}

/// A minimal QMP client over a Unix socket.
///
/// QEMU's machine protocol is line-delimited JSON. This speaks just enough of it for the
/// display gate — a handshake and one command — without a JSON library: the messages
/// sent are fixed strings, and the only thing read back is whether a reply line carries
/// `"return"` or `"error"`.
///
/// QMP rather than the human monitor because it is the supported interface and because
/// Milestone 3 needs `input-send-event` from the same channel; adding HMP now would mean
/// replacing it then.
struct Qmp {
    stream: std::os::unix::net::UnixStream,
    buf: Vec<u8>,
    /// Where the guest's pointer is, when a press receipt has confirmed it.
    ///
    /// **The pointer is relative and unacknowledged, so this is the only way to know.** Without
    /// it every click re-pins to a corner with twenty over-driven motions before walking, and
    /// that burst is what overruns the guest's input ring: `input batch DROPPED (SYN_DROPPED)`,
    /// then a press tens of pixels from where it was aimed. A confirmed press *is* a position
    /// report, so consecutive clicks cost two motions instead of thirty-four
    /// (PR #243 review, finding 3). Cleared whenever an attempt does not land, because then it
    /// is not known any more.
    pointer: Option<(i32, i32)>,
}

impl Qmp {
    /// Connect and complete the capabilities handshake.
    ///
    /// QEMU creates the socket as it starts, so this retries briefly rather than
    /// requiring the caller to guess how long that takes.
    fn connect(path: &Path) -> R<Qmp> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let stream = loop {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(format!("connect QMP socket {}: {e}", path.display()).into()),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .map_err(|e| format!("qmp read timeout: {e}"))?;
        let mut q = Qmp { stream, buf: Vec::new(), pointer: None };
        // The greeting arrives unsolicited; then capabilities must be negotiated before
        // any other command is accepted.
        let greeting = q.read_line()?;
        if !greeting.contains("QMP") {
            return Err(format!("unexpected QMP greeting: {greeting}").into());
        }
        q.execute(r#"{"execute":"qmp_capabilities"}"#)?;
        Ok(q)
    }

    /// Read one newline-delimited message.
    fn read_line(&mut self) -> R<String> {
        use std::io::Read as _;
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&self.buf[..i]).into_owned();
                self.buf.drain(..=i);
                return Ok(line);
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).map_err(|e| format!("qmp read: {e}"))?;
            if n == 0 {
                return Err("qmp socket closed".into());
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Send one command and wait for its reply, skipping asynchronous events.
    ///
    /// QEMU interleaves `{"event": ...}` messages with replies, so a reader that took the
    /// next line as the answer would eventually read an event and misreport success.
    fn execute(&mut self, json: &str) -> R<String> {
        use std::io::Write as _;
        self.stream
            .write_all(format!("{json}\n").as_bytes())
            .map_err(|e| format!("qmp write: {e}"))?;
        self.stream.flush().map_err(|e| format!("qmp flush: {e}"))?;
        loop {
            let line = self.read_line()?;
            if line.contains("\"event\"") {
                continue; // asynchronous event, not our reply
            }
            if line.contains("\"error\"") {
                return Err(format!("qmp command failed: {json} -> {line}").into());
            }
            if line.contains("\"return\"") {
                return Ok(line);
            }
            // Anything else is unexpected; surface it rather than looping forever.
            return Err(format!("unexpected qmp reply: {line}").into());
        }
    }

    /// Run a **human monitor** command through QMP and return its text output.
    ///
    /// The monitor is where the interesting diagnostics live — `info registers`, `info
    /// cpus` — and QMP has no structured equivalent for most of them.
    fn hmp(&mut self, cmd: &str) -> R<String> {
        let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
        let reply = self.execute(&format!(
            r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"{escaped}"}}}}"#
        ))?;
        Ok(unescape_json_return(&reply))
    }

    /// Inject a key press or release by QEMU `qcode` name (`"a"`, `"shift"`, `"esc"`).
    ///
    /// Injection is what makes an input driver testable at all: without it nothing can type
    /// at the guest, so the ISR, the event ring and the parked read are exercised only by a
    /// human (`display-substrate.md` §8d).
    fn send_key(&mut self, qcode: &str, down: bool) -> R<()> {
        self.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[{{"type":"key",
               "data":{{"down":{down},"key":{{"type":"qcode","data":"{qcode}"}}}}}}]}}}}"#
        ))?;
        Ok(())
    }

    /// Inject a pointer button press or release (`"left"`, `"right"`, `"middle"`).
    fn send_button(&mut self, button: &str, down: bool) -> R<()> {
        self.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[{{"type":"btn",
               "data":{{"down":{down},"button":"{button}"}}}}]}}}}"#
        ))?;
        Ok(())
    }

    /// Inject relative pointer motion.
    fn send_motion(&mut self, dx: i32, dy: i32) -> R<()> {
        self.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[
               {{"type":"rel","data":{{"axis":"x","value":{dx}}}}},
               {{"type":"rel","data":{{"axis":"y","value":{dy}}}}}]}}}}"#
        ))?;
        Ok(())
    }

    /// Capture the guest's display to `path` as a binary PPM.
    fn screendump(&mut self, path: &Path) -> R<()> {
        let p = path.to_str().ok_or("screendump path is not UTF-8")?;
        self.execute(&format!(r#"{{"execute":"screendump","arguments":{{"filename":"{p}"}}}}"#))?;
        Ok(())
    }
}


/// Pull the string value of a QMP `"return"` field out of a reply line and undo the JSON
/// escaping. Hand-rolled because xtask carries no JSON dependency, and the shape here is
/// fixed: `{"return": "…"}`.
fn unescape_json_return(line: &str) -> String {
    let Some(start) = line.find(r#""return""#) else { return String::new() };
    let rest = &line[start + 8..];
    let Some(open) = rest.find('"') else { return String::new() };
    let body = &rest[open + 1..];
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('u') => {
                    // \uXXXX — consume the four digits; the monitor emits none in practice.
                    for _ in 0..4 {
                        let _ = chars.next();
                    }
                    out.push('?');
                }
                Some(other) => out.push(other),
                None => break,
            },
            _ => out.push(c),
        }
    }
    out
}

/// A function symbol from an ELF's symbol table.
struct ElfSym {
    addr: u64,
    size: u64,
    name: String,
}

/// Read the `STT_FUNC` symbols out of an ELF64 file, sorted by address.
///
/// Hand-rolled rather than shelling out to `nm`/`addr2line`: those are another tool
/// requirement on every machine that runs the tests, for sixty lines of well-specified
/// header walking.
fn elf_function_symbols(path: &Path) -> R<Vec<ElfSym>> {
    let b = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let u16at = |o: usize| -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) };
    let u32at = |o: usize| -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) };
    let u64at = |o: usize| -> u64 {
        let mut v = [0u8; 8];
        v.copy_from_slice(&b[o..o + 8]);
        u64::from_le_bytes(v)
    };
    if b.len() < 64 || &b[..4] != b"\x7fELF" || b[4] != 2 {
        return Err(format!("{} is not an ELF64", path.display()).into());
    }
    let shoff = u64at(0x28) as usize;
    let shentsize = u16at(0x3A) as usize;
    let shnum = u16at(0x3C) as usize;
    let mut syms = Vec::new();
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if sh + shentsize > b.len() {
            break;
        }
        const SHT_SYMTAB: u32 = 2;
        if u32at(sh + 4) != SHT_SYMTAB {
            continue;
        }
        let off = u64at(sh + 0x18) as usize;
        let size = u64at(sh + 0x20) as usize;
        let link = u32at(sh + 0x28) as usize; // the associated string table
        let strsh = shoff + link * shentsize;
        let stroff = u64at(strsh + 0x18) as usize;
        const SYM_SIZE: usize = 24;
        for j in 0..size / SYM_SIZE {
            let e = off + j * SYM_SIZE;
            if e + SYM_SIZE > b.len() {
                break;
            }
            const STT_FUNC: u8 = 2;
            if b[e + 4] & 0xF != STT_FUNC {
                continue;
            }
            let addr = u64at(e + 8);
            let sz = u64at(e + 16);
            if addr == 0 {
                continue;
            }
            let nameoff = stroff + u32at(e) as usize;
            let end = b[nameoff..].iter().position(|&c| c == 0).unwrap_or(0) + nameoff;
            let name = String::from_utf8_lossy(&b[nameoff..end]).into_owned();
            syms.push(ElfSym { addr, size: sz, name });
        }
    }
    syms.sort_by_key(|s| s.addr);
    Ok(syms)
}

/// Undo rustc's legacy `_ZN…E` name mangling, enough to read a backtrace.
///
/// `_ZN13nitrox_kernel5sched9idle_body17h08d8…E` → `nitrox_kernel::sched::idle_body`. Not a
/// general Itanium demangler — just the length-prefixed component form rustc emits, with the
/// trailing hash and any LLVM suffix dropped. Unrecognised names come back unchanged, which
/// is strictly better than the alternative of not printing them.
fn demangle(name: &str) -> String {
    let Some(rest) = name.strip_prefix("_ZN") else { return name.to_string() };
    let rest = rest.split(".llvm.").next().unwrap_or(rest);
    let mut parts: Vec<String> = Vec::new();
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'E' {
            break;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return name.to_string(); // not the shape we expect
        }
        let Ok(len) = rest[start..i].parse::<usize>() else { return name.to_string() };
        if i + len > bytes.len() {
            return name.to_string();
        }
        let part = &rest[i..i + len];
        i += len;
        // The final `17h<16 hex>` component is the disambiguating hash, not a path element.
        if !(part.len() == 17 && part.starts_with('h') && part[1..].bytes().all(|c| c.is_ascii_hexdigit())) {
            parts.push(part.to_string());
        }
    }
    if parts.is_empty() { name.to_string() } else { parts.join("::") }
}

/// Name the function containing `addr`, if any.
///
/// Several symbols can share one address — aliased generic instantiations, say — and this
/// returns an arbitrary one of the group. Read a resolved name as "this address is in this
/// function's code", not as a unique identification.
fn resolve_symbol(syms: &[ElfSym], addr: u64) -> Option<String> {
    let i = syms.partition_point(|s| s.addr <= addr).checked_sub(1)?;
    let s = &syms[i];
    // A zero-size symbol still names its entry point; otherwise require containment so a
    // wildly wrong address is reported as unknown rather than attributed to the last
    // function before it.
    if s.size == 0 || addr < s.addr + s.size {
        Some(format!("{}+{:#x}", demangle(&s.name), addr - s.addr))
    } else {
        None
    }
}

/// Walk a stack, printing every word that resolves to a kernel function.
///
/// A real unwinder needs frame pointers or DWARF; this is the cheap approximation that
/// works on an optimised kernel: read raw stack words and keep the ones that land inside a
/// known function. It over-reports (stale return addresses linger below the stack pointer)
/// and that is fine — the question it answers is *"is this CPU inside this function
/// twice?"*, which a few spurious frames cannot fake.
fn stack_trace(qmp: &mut Qmp, syms: &[ElfSym], cpu: usize, rsp: u64, depth: usize) {
    if syms.is_empty() || rsp == 0 {
        return;
    }
    if qmp.hmp(&format!("cpu {cpu}")).is_err() {
        return;
    }
    let Ok(text) = qmp.hmp(&format!("x/{depth}gx {rsp:#x}")) else { return };
    let mut shown = 0usize;
    for line in text.lines() {
        // `addr: 0xword 0xword`
        let Some((_, words)) = line.split_once(':') else { continue };
        for w in words.split_whitespace() {
            let Some(hex) = w.strip_prefix("0x") else { continue };
            let Ok(v) = u64::from_str_radix(hex, 16) else { continue };
            if let Some(sym) = resolve_symbol(syms, v) {
                println!("      ↳ {sym}");
                shown += 1;
                if shown >= 12 {
                    return;
                }
            }
        }
    }
}

/// Dump what the guest was doing when it stopped answering.
///
/// The discriminator this exists for: **are all vCPUs halted, or is one spinning?** Every
/// CPU parked in the idle loop means nothing is runnable — a lost wake, where somebody is
/// blocked on a completion that was signalled to no one. A CPU spinning in a lock is the
/// opposite problem. A bare 90-second timeout cannot tell those apart, and the whole cost
/// of chasing the `test-qemu` flake so far has been that ambiguity.
fn dump_guest_state(qmp: &mut Qmp) {
    println!("\n─── guest state at timeout ───");
    // **Stop the guest first.** Without this the vCPUs keep executing between queries, so
    // the RIP summary, the stack walk and the full register block are three snapshots of
    // three different moments — and they visibly disagreed when this was first written.
    // The run is over either way; a stopped guest is the only coherent one.
    if let Err(e) = qmp.execute(r#"{"execute":"stop"}"#) {
        println!("  (could not pause the guest, readings may be inconsistent: {e})");
    }
    let syms = match elf_function_symbols(&kernel_elf()) {
        Ok(s) => s,
        Err(e) => {
            println!("  (kernel symbols unavailable: {e})");
            Vec::new()
        }
    };
    match qmp.hmp("info registers -a") {
        Ok(text) => {
            let mut cpu = 0usize;
            let mut last_rbx = 0u64;
            let mut last_rsp = 0u64;
            let mut cpu_state: Vec<(usize, u64, u64, u64)> = Vec::new();
            let grab = |line: &str, key: &str| -> Option<u64> {
                let pos = line.find(key)?;
                let hex: String = line[pos + key.len()..]
                    .chars()
                    .skip_while(|c| c.is_whitespace() || *c == '=')
                    .take_while(|c| c.is_ascii_hexdigit())
                    .collect();
                u64::from_str_radix(&hex, 16).ok()
            };
            for line in text.lines() {
                if let Some(v) = grab(line, "RBX=") {
                    last_rbx = v;
                }
                if let Some(v) = grab(line, "RSP=") {
                    last_rsp = v;
                }
                if let Some(rest) = line.trim().strip_prefix("CPU#") {
                    cpu = rest.trim().parse().unwrap_or(cpu);
                }
                // The monitor prints `RIP=<hex>` on the register dump's second line.
                if let Some(pos) = line.find("RIP=") {
                    let hex: String =
                        line[pos + 4..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                    if let Ok(rip) = u64::from_str_radix(&hex, 16) {
                        let where_ = resolve_symbol(&syms, rip)
                            .unwrap_or_else(|| {
                                if rip >= 0xFFFF_8000_0000_0000 {
                                    "<kernel, no symbol>".to_string()
                                } else {
                                    "<userspace>".to_string()
                                }
                            });
                        println!("  CPU#{cpu}  RIP={rip:#018x}  {where_}");
                        cpu_state.push((cpu, rip, last_rbx, last_rsp));
                    }
                }
            }
            // The two questions the summary above cannot answer: are the spinning CPUs
            // waiting on the *same* lock (RBX carries the `&SlabCache` receiver), and is
            // either of them already inside that function further down its own stack —
            // which is what re-entrancy looks like.
            for &(cpu, _rip, rbx, rsp) in &cpu_state {
                println!("\n  CPU#{cpu}  RBX={rbx:#018x}  RSP={rsp:#018x}  stack:");
                stack_trace(qmp, &syms, cpu, rsp, 48);
            }
        }
        Err(e) => println!("  (info registers failed: {e})"),
    }
    match qmp.hmp("info cpus") {
        Ok(text) => {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                println!("  {}", line.trim());
            }
        }
        Err(e) => println!("  (info cpus failed: {e})"),
    }
    // The full register block too, verbose but only on a failure. The summary above says
    // *where* each CPU is; this says *what it is working on* — and for a spin inside a
    // lock acquire, the lock's address is in a register, which is the only way to tell one
    // CPU waiting on another from two CPUs waiting on nothing.
    if let Ok(text) = qmp.hmp("info registers -a") {
        println!("\n  --- full registers ---");
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            println!("  {}", line.trim_end());
        }
    }
    println!("─────────────────────────────\n");
}

/// Which of `sent` — the lines typed since the last match — would satisfy `pat` by echo alone.
///
/// **A free function so it can be table-tested.** Inline in [`Session::expect`] the only thing
/// proving it worked would be a manual mutation of the gate, which is the situation this guard
/// exists to argue against; the same reasoning is written out at
/// [`scope_binding`]. The live risk is not the predicate's logic but its inputs: `sent` is
/// appended to in two places, and a later `send_line`-style wrapper that forgets to do so
/// disarms the guard for those steps while every gate stays green.
fn echo_source<'a>(sent: &'a [String], pat: &str) -> Option<&'a str> {
    if pat.is_empty() {
        return None;
    }
    sent.iter().find(|s| s.contains(pat)).map(|s| s.as_str())
}

/// A driven QEMU serial session: write lines, wait for text.
struct Session {
    child: std::process::Child,
    /// Which gate this is, so a failing run's transcript does not overwrite another gate's.
    /// A batch of runs is usually what you are comparing, and a single fixed filename means
    /// every failure destroys the evidence from the one before it — which happened to a
    /// reviewer mid-review (PR #197).
    gate: &'static str,
    /// Everything the guest has printed, accumulated by a reader thread.
    out: std::sync::Arc<std::sync::Mutex<String>>,
    /// How far `expect` has already matched, so each step scans only new output and a
    /// pattern cannot be satisfied by an earlier occurrence of itself.
    cursor: usize,
    /// Every line typed at the guest since the last successful [`expect`](Self::expect), so
    /// that call can refuse a pattern the guest's own echo would satisfy. Empty for gates
    /// that type over QMP rather than the serial line, which is every gate but
    /// `test-interactive`.
    ///
    /// **A list, not the most recent line.** The unconsumed-echo window is not one send: two
    /// sends before an `expect` leave *both* echoes ahead of the cursor, and a guard that
    /// remembered only the second would wave the first one's text straight through — the one
    /// shape that walks past a guard whose whole purpose is preventing recurrence. Cleared on
    /// a successful match because the cursor has then advanced past those echoes, so they can
    /// no longer satisfy anything.
    sent_since_match: Vec<String>,
}

impl Drop for Session {
    /// Kill the guest when the session goes out of scope.
    ///
    /// Every `?` between `spawn` and an explicit `kill` used to leak a QEMU, and a leaked
    /// QEMU holds the write lock on `tools/build-cache/nitrox.hdd` — so the *next* run boots
    /// a guest that cannot open its disk and fails with a bare timeout and no serial output,
    /// which looks nothing like the original error. A reviewer lost a pass to a 20-hour-old
    /// orphan this way (PR #175 review). Killing an already-dead child is harmless, so the
    /// explicit kills elsewhere stay valid.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    fn spawn(mut cmd: Command, gate: &'static str) -> R<Session> {
        let mut child = cmd.spawn().map_err(|e| format!("spawn qemu: {e}"))?;
        let stdout = child.stdout.take().ok_or("qemu stdout")?;
        let out = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = out.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut r = stdout;
            let mut buf = [0u8; 1024];
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut g) = sink.lock() {
                    g.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
        Ok(Session { child, out, cursor: 0, gate, sent_since_match: Vec::new() })
    }

    /// The rest of the line the last [`expect`](Self::expect) matched on.
    ///
    /// For the few assertions that need a *value* the guest computed rather than a string it
    /// was always going to print — a window's id and geometry, say, which the host cannot know
    /// and must not guess. Read after `expect` has moved the cursor past the pattern.
    fn rest_of_line(&self) -> R<String> {
        // **Wait for the newline.** `expect` returns the instant the *prefix* appears, and the
        // guest's line reaches the host serial in whatever chunks `read` happens to return — so
        // taking what is there can take half a line. Reading `"… popup 138 at 0,"` and failing
        // to parse it would be a rare, confusing gate failure with nothing wrong in the guest
        // (PR #223 review, finding 6).
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                let tail = &g[self.cursor..];
                if let Some(end) = tail.find('\n') {
                    return Ok(tail[..end].trim().to_string());
                }
            }
            if std::time::Instant::now() > deadline {
                return Err("the guest never finished the line after the matched prefix".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Wait up to `timeout` for `pat`, answering **whether it arrived** rather than failing.
    ///
    /// For the one case where absence is retryable rather than a verdict: a click whose
    /// positioning a dropped PS/2 packet left short. [`expect`](Self::expect) is right
    /// everywhere else — a gate that treats a missing guest line as "try again" is a gate that
    /// can pass while the guest is broken.
    fn expect_within(&mut self, pat: &str, timeout: std::time::Duration) -> R<bool> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                if let Some(i) = g[self.cursor..].find(pat) {
                    self.cursor += i + pat.len();
                    self.sent_since_match.clear();
                    println!("  ok: saw {pat:?}");
                    return Ok(true);
                }
            }
            if std::time::Instant::now() > deadline {
                return Ok(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Drop everything said so far without matching it — for discarding an abandoned attempt's
    /// output, so a retry cannot match a line the previous try emitted.
    fn skip_to_end(&mut self) -> R<()> {
        let g = self.out.lock().map_err(|_| "transcript lock")?;
        self.cursor = g.len();
        Ok(())
    }

    /// Wait for `pat` in output not yet consumed. The guest paces the test.
    fn expect(&mut self, pat: &str) -> R<()> {
        // **An `expect` the guest's echo can satisfy is not an assertion.** The terminal
        // echoes what the harness types and `expect` takes the first match, which is always
        // the echo — emitted as the line is sent, long before anything evaluates it. Such a
        // step passes against a guest with the feature removed entirely.
        //
        // The convention is to wrap the answer so the expected text cannot appear in the
        // command (`format("n={}", n)` → `n=3`), and it is stated twice in
        // `run_interactive_scenarios`, at steps 12 and 13, along with the history of how it
        // was learned. It was still violated twice in that same function — a comment cannot
        // fail a build, so this does.
        //
        // **What this does not cover**, so nobody reads it as more than it is: it knows only
        // what the harness typed. A line the guest *re-emits* from its own history (Up-arrow,
        // Ctrl-R — steps 8-11) was never sent by us and is invisible here, which is why those
        // steps assert on command output instead. Neither does it cover the other half of the
        // problem — a pattern satisfied by a background service's logging rather than by the
        // shell — because nothing can tell the harness what a service might print. See the
        // 2026-08-18 decision-log entry.
        if let Some(echoed) = echo_source(&self.sent_since_match, pat) {
            return Err(format!(
                "expect({:?}) is satisfied by the guest's echo of the text just typed \
                 ({:?}), which it emits as the line is sent — so this step would pass \
                 against a guest that never evaluated the command. Wrap the answer so \
                 the pattern cannot occur in what was typed: \
                 `format(\"label={{}}\", …)` then expect(\"label=…\").",
                pat, echoed
            )
            .into());
        }
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                if let Some(i) = g[self.cursor..].find(pat) {
                    self.cursor += i + pat.len();
                    // The cursor is now past those echoes; they cannot satisfy anything else.
                    self.sent_since_match.clear();
                    println!("  ok: saw {pat:?}");
                    return Ok(());
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(self.timeout_report(&format!("{pat:?}"), TIMEOUT)?.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Wait for **all** of `pats`, in whatever order the guest emits them.
    ///
    /// **For lines whose order is not the guest's to guarantee.** [`expect`](Self::expect)
    /// consumes forward — a match moves the cursor past it — so a chain of them asserts a
    /// *sequence*, and that is the right shape when one line causes the next. It is the wrong
    /// shape when two processes react to the same event: they are scheduled independently on
    /// four vCPUs and the order they reach the serial console in is not a property of the
    /// system. Asserted as a sequence, the pair passes until the day the other one wins, and
    /// then fails by scanning *past* the line it will ask for next — so the timeout names a
    /// line that is sitting in the transcript, which reads like a lost message and is not one.
    ///
    /// Observed on 2026-08-31: maximising the terminal makes `desktop-shell` log the window's
    /// new geometry and `nxterm` log the size it took, both downstream of one compositor apply
    /// and neither downstream of the other.
    ///
    /// **This is not a licence to unorder an assertion that is merely inconvenient.** A pair
    /// belongs here only when nothing in the system orders it — where one line's producer sends
    /// the message that causes the other's, the sequence is the assertion. `configure_window`
    /// logging before it sends is what makes every `… window N to …` → `nxterm: resized …` pair
    /// in this file a genuine chain.
    fn expect_all(&mut self, pats: &[&str]) -> R<()> {
        for pat in pats {
            if let Some(echoed) = echo_source(&self.sent_since_match, pat) {
                return Err(format!(
                    "expect_all({pat:?}) is satisfied by the guest's echo of the text just \
                     typed ({echoed:?}) — see `expect`, which explains the whole trap."
                )
                .into());
            }
        }
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                let found: Vec<(usize, &str)> = pats
                    .iter()
                    .filter_map(|p| g[self.cursor..].find(p).map(|i| (i + p.len(), *p)))
                    .collect();
                if found.len() == pats.len() {
                    // **Past the last of them, not the first.** The cursor is what stops a
                    // later step matching a line this one already accounted for, and the whole
                    // point here is that which of these is last is not known in advance.
                    let mut found = found;
                    found.sort_by_key(|(end, _)| *end);
                    for (_, p) in &found {
                        println!("  ok: saw {p:?}");
                    }
                    self.cursor += found.last().map_or(0, |(end, _)| *end);
                    self.sent_since_match.clear();
                    return Ok(());
                }
            }
            if std::time::Instant::now() > deadline {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                let missing: Vec<&str> =
                    pats.iter().copied().filter(|p| !g[self.cursor..].contains(p)).collect();
                drop(g);
                // Naming the ones that *did* arrive is the point: an unordered wait that times
                // out with one of two lines present is a different fault from one with neither.
                let what = format!("all of {pats:?}, still missing {missing:?}");
                return Err(self.timeout_report(&what, TIMEOUT)?.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// What a timed-out wait reports, and the transcript it saves on the way out.
    ///
    /// Shared so that every wait fails the same way, and **distinguishes "the guest said
    /// nothing" from "the guest stopped early".** They are the same line in the log and
    /// completely different faults: no output at all usually means QEMU never got as far as the
    /// guest — a disk held open by an orphaned instance, a missing image, bad firmware — while
    /// partial output is a real hang, and the tail says where.
    ///
    /// **The whole transcript goes to a file, not just the tail.** The tail is what you read
    /// first and it is almost never enough: these gates fail *between* two things the guest
    /// said, so the interesting part is the hundred lines before the end. The transcript was
    /// already accumulated in full and only ever printed truncated, which cost three separate
    /// investigations in M5 and M6 — each one reduced to guessing at a guest whose log existed
    /// and was thrown away.
    fn timeout_report(&self, waiting_for: &str, timeout: std::time::Duration) -> R<String> {
        let g = self.out.lock().map_err(|_| "transcript lock")?;
        if g.trim().is_empty() {
            return Ok(format!(
                "timed out after {timeout:?} waiting for {waiting_for}, and the guest \
                 produced NO output at all — this is usually QEMU failing to start \
                 rather than a hang (a stale qemu-system-x86_64 holding \
                 build-cache/nitrox.hdd will do it; check with `pgrep -a qemu`)"
            ));
        }
        let tail: String =
            g.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
        let path = build_cache().join(format!("guest-transcript-{}.log", self.gate));
        let saved = fs::write(&path, g.as_str()).is_ok();
        let where_ = if saved {
            format!("\n\nthe full transcript is at {}", path.display())
        } else {
            String::new()
        };
        Ok(format!("timed out after {timeout:?} waiting for {waiting_for}; last output was:\n{tail}{where_}"))
    }

    /// Send bytes with **no trailing newline** — for keys that are not a line.
    ///
    /// `Ctrl-C` is the case that needs it: appending `\n` would submit the line as well as
    /// interrupt, and the two behaviours are exactly what the step is trying to tell apart.
    fn send_raw(&mut self, bytes: &str) -> R<()> {
        use std::io::Write as _;
        self.sent_since_match.push(bytes.to_string());
        let stdin = self.child.stdin.as_mut().ok_or("qemu stdin")?;
        stdin.write_all(bytes.as_bytes()).map_err(|e| format!("write to guest: {e}"))?;
        stdin.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    /// Type a line, Enter included.
    fn send(&mut self, line: &str) -> R<()> {
        use std::io::Write as _;
        self.sent_since_match.push(line.to_string());
        let stdin = self.child.stdin.as_mut().ok_or("qemu stdin")?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|e| format!("write to guest: {e}"))?;
        stdin.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    /// Kill the guest and hand back the transcript.
    fn finish(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.out.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Integration-test runner: build the `test-harness` image, boot it headless, and
/// adjudicate the run from QEMU's exit code. The guest ends the run by writing a
/// verdict to the `isa-debug-exit` device (init on success/failure, or the kernel
/// panic handler); QEMU then exits `(verdict << 1) | 1`. A hung boot is caught by a
/// wall-clock timeout. See `docs/conventions/qemu-integration-tests.md`.
fn cmd_test_qemu(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    // Wall-clock ceiling: a hung boot must fail the run, not block CI forever. The
    // healthy self-test boot completes in a few seconds under TCG; 90 s is generous
    // (the demand-paging demo does many emulated-AHCI faults).
    const TIMEOUT_SECS: u32 = 90;
    // isa-debug-exit maps a guest port write `v` to host exit `(v << 1) | 1`: init's
    // PASS verdict (0x10) → 33; FAIL (0x11) → 35; the `timeout` tool uses 124.
    const PASS_EXIT: i32 = (0x10 << 1) | 1; // 33

    // **Spawned directly, not under `timeout(1)`.** The wrapper killed QEMU before we
    // could ask it anything, so a hang produced a bare "no verdict" line and nothing else —
    // which is exactly the ambiguity that made the `test-qemu` flake expensive to chase.
    // Owning the deadline means the guest is still alive at the moment it misses it.
    let qmp_sock = build_cache().join("test-qemu.qmp");
    fs::create_dir_all(build_cache())?;
    let _ = fs::remove_file(&qmp_sock);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel)?;
    cmd.arg("-qmp").arg(format!("unix:{},server,nowait", qmp_sock.display()));
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        // The guest ends the run by writing its verdict to this port.
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        // Headless: serial to our captured stdout, no display; `-smp 4` so the SMP
        // distribution/affinity self-tests are meaningful; never reboot on triple-fault.
        .arg("-display")
        .arg("none")
        .arg("-serial")
        .arg("stdio")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    println!("xtask: running integration tests under QEMU (timeout {TIMEOUT_SECS}s)…\n");
    let mut child = cmd.spawn().map_err(|e| format!("spawn qemu: {e}"))?;

    // Drain stdout on a thread: a guest that fills the pipe buffer would otherwise block
    // on its own serial writes and look exactly like the hang we are trying to diagnose.
    let stdout = child.stdout.take().ok_or("qemu stdout")?;
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = captured.clone();
    let reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut r = stdout;
        let mut buf = [0u8; 4096];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Ok(mut g) = sink.lock() {
                g.extend_from_slice(&buf[..n]);
            }
        }
    });

    // **stderr gets its own drainer.** Piping it and never reading it loses exactly the
    // messages you need when a run fails for a *host* reason — an unavailable accelerator, a
    // bad image path, an option this QEMU does not know — leaving only
    // "FAILED (qemu exit 1)". Worse, an undrained pipe blocks QEMU once it fills, which
    // presents as a guest hang at the deadline: the precise misdiagnosis the rest of this
    // change exists to prevent (PR #177 review, finding 1).
    let stderr = child.stderr.take().ok_or("qemu stderr")?;
    let errs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let esink = errs.clone();
    let ereader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut r = stderr;
        let mut buf = [0u8; 4096];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Ok(mut g) = esink.lock() {
                g.extend_from_slice(&buf[..n]);
            }
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS as u64);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait qemu: {e}"))? {
            Some(st) => break st,
            None => {
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    // **Interrogate before killing.** This is the whole point of owning the
                    // deadline; once QEMU is dead the evidence is gone.
                    match Qmp::connect(&qmp_sock) {
                        Ok(mut qmp) => dump_guest_state(&mut qmp),
                        Err(e) => println!("\nxtask: could not reach QMP to dump state: {e}"),
                    }
                    let _ = child.kill();
                    break child.wait().map_err(|e| format!("reap qemu: {e}"))?;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };
    let _ = reader.join();
    let _ = ereader.join();
    let _ = fs::remove_file(&qmp_sock);

    // Echo the captured serial log so the operator sees the boot + self-test output.
    if let Ok(g) = captured.lock() {
        std::io::stdout().write_all(&g)?;
    }
    if let Ok(g) = errs.lock()
        && !g.is_empty()
    {
        std::io::stderr().write_all(&g)?;
    }

    if timed_out {
        return Err(format!(
            "integration tests TIMED OUT after {TIMEOUT_SECS}s — no verdict (likely a hang); \
             see the guest state dumped above"
        )
        .into());
    }

    match status.code() {
        Some(code) if code == PASS_EXIT => {
            let transcript = captured.lock().map(|g| g.clone()).unwrap_or_default();
            check_login_chain(&transcript)?;
            check_demo_chain(&transcript)?;
            check_display_selftest(&transcript)?;
            check_every_service_started(&transcript)?;
            println!("\nxtask: integration tests PASSED (qemu exit {code})");
            Ok(())
        }
        Some(code) => {
            Err(format!("integration tests FAILED (qemu exit {code}; expected {PASS_EXIT})").into())
        }
        None => Err("qemu terminated by a signal with no exit code".into()),
    }
}

/// Assert that the display self-test **passed**.
///
/// **The same hole `check_demo_chain` closes, one declaration over.** `init` adjudicated this
/// exit code and fired `test_exit(false)` on it; retrofit Part C2 made the self-test a
/// declaration with `policy = "never"`, so `service-mgr` logs the code and does nothing with
/// it, and `display-selftest` never calls `SYS_TEST_EXIT` itself. Forcing its hash comparison
/// to fail produces `display-selftest: FAILED`, `exited code=1`, and `integration tests
/// PASSED` (PR #229 review, finding 1).
///
/// Exit code **2** is the one the deleted comment singled out: it means the framebuffer
/// binding was missing, and folding that into success "is how the entire display arm could go
/// missing with `test-qemu` still green, which is exactly what an earlier version of this code
/// did". Requiring the `PASSED` line rejects 1 and 2 alike, and rejects the self-test not
/// running at all.
///
/// `check-display` is not a substitute: it is a separate, path-filtered workflow, and it does
/// not read this exit code.
fn check_display_selftest(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("display-selftest: PASSED") {
        return Err("the display self-test did not pass — expected \
             \"display-selftest: PASSED\" in the transcript. \"FAILED\" is a hash \
             mismatch; no line at all (exit code 2) means it could not bind the framebuffer, \
             which is the display arm going missing rather than a rendering bug."
            .into());
    }
    println!("xtask: the display self-test passed ✓");
    Ok(())
}

/// Assert that **every declared service actually started**.
///
/// `init` used to fire `test_exit(false)` when a spawn failed; `spawn_service` logs and carries
/// on, so a `ui-testclient` that never starts reaches PASS. This is deliberately a check on the
/// *failure* lines rather than a list of expected services: a new declaration is covered the
/// day it is added, and there is nothing to keep in step (PR #229 review, finding 1).
fn check_every_service_started(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    for pat in ["service-mgr: image not found", "service-mgr: spawn FAIL"] {
        if let Some(i) = text.find(pat) {
            let line: String = text[i..].lines().next().unwrap_or(pat).into();
            return Err(format!(
                "a declared service failed to start, and nothing in the guest fails the run \
                 for it: {line:?}"
            )
            .into());
        }
    }
    println!("xtask: every declared service started ✓");
    Ok(())
}

/// Assert that the demo chain **finished**, not merely that it ran.
///
/// **This replaces a check `init` used to make.** `init::supervise` ran the chain
/// synchronously and failed the run on a non-zero exit; retrofit Part C2 made it a service
/// declaration, so nothing in the guest reads its exit code any more — `policy = "never"`
/// means service-mgr starts it once and does not react.
///
/// Without this, a chain that dies partway reaches PASS: the checks that follow it are
/// `boot-probe`'s, and `boot-probe` waits for the chain to *exit*, not to succeed. That is not
/// hypothetical — it happened on the first C2 boot. The chain stopped at
/// `test-harness: session user bind FAIL`, because `init` had spawned it with
/// `BIND_NAMESPACE` and the declaration could not yet say so, and `test-qemu` passed anyway
/// with half the spawns missing.
fn check_demo_chain(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("test-harness: all smoke tests passed") {
        return Err("the demo chain did not finish — expected \
             \"test-harness: all smoke tests passed\" in the transcript. The last \
             \"test-harness:\" line names the stage it stopped at; a `FAIL` there is the \
             real fault. `boot-probe` waits for the chain to exit, not to succeed, so the \
             run reaches PASS either way and only this says otherwise."
            .into());
    }
    println!("xtask: the demo chain finished (all smoke tests passed) ✓");
    Ok(())
}

/// Assert that the login chain came up — `session-mgr` holding the endpoints it needs to
/// build a session.
///
/// **This replaces a verdict `session-mgr` used to fire itself.** It called `verdict(false)`
/// when its endpoint handoff failed, which is a session supervisor adjudicating a test run;
/// removing that (retrofit Part B) would otherwise have let a broken login chain reach PASS,
/// because nothing else in `test-qemu` reads it. The transcript does.
///
/// It does **not** assert that anyone logged in: nothing types a password here, and after
/// Part B nothing auto-logs-in either. Whether a real login works is
/// `cargo xtask test-interactive`'s question, on the release image.
///
/// **It depends on an ordering that is not causal, which is worth naming rather than
/// discovering.** `service-mgr` queues session-mgr's four handoffs before `supervise` starts
/// any declared service, so the *send* is ordered — but session-mgr still has to be scheduled
/// to print this line before `boot-probe`, spawned afterwards, fires PASS and terminates the
/// machine. If that ever inverts, a healthy boot fails here, which is the expensive kind of
/// red.
///
/// The margin is large and was measured rather than assumed: session-mgr's line lands **six
/// lines and one ELF materialisation** before `service-mgr: starting service 'heartbeat'`, and
/// `boot-probe` starts after that — session-mgr has four queued receives to do while
/// `service-mgr` resolves and spawns two programs. If this ever goes red on a boot that looks
/// healthy, check that ordering first, and consider asserting on
/// `service-mgr: login chain up` instead: that line *is* causally before `boot-probe`, at the
/// cost of proving only that the handoffs were sent, not that they arrived.
fn check_login_chain(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("session-mgr: received fs + profile endpoints; auth resolved from /svc/auth") {
        return Err("the login chain did not come up — session-mgr never reported its \
             endpoints (look for \"session-mgr: endpoint handoff FAIL\", or for \
             service-mgr failing to spawn it at all)"
            .into());
    }
    println!("xtask: the login chain came up (session-mgr holds its endpoints) ✓");
    Ok(())
}

fn cmd_fetch_limine() -> R<PathBuf> {
    let dir = limine_dir();
    let marker = dir.join(".version");
    if marker.exists() {
        if let Ok(v) = fs::read_to_string(&marker) {
            if v.trim() == LIMINE_VERSION {
                return Ok(dir);
            }
        }
    }

    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;

    let url = format!(
        "https://github.com/limine-bootloader/limine/releases/download/{LIMINE_VERSION}/limine-binary.tar.gz"
    );
    let tarball = build_cache().join("limine-binary.tar.gz");
    fs::create_dir_all(build_cache())?;
    println!("xtask: fetching {url}");
    run(Command::new("curl")
        .arg("-fL")
        .arg("--retry")
        .arg("3")
        .arg("-o")
        .arg(&tarball)
        .arg(&url))?;

    run(Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&dir)
        .arg("--strip-components=1"))?;

    fs::remove_file(&tarball).ok();
    fs::write(&marker, LIMINE_VERSION)?;
    Ok(dir)
}

fn cmd_clean() -> R<()> {
    let kernel_dir = repo_root().join("kernel");
    run(Command::new("cargo").arg("clean").current_dir(&kernel_dir))?;
    let userspace_dir = repo_root().join("userspace");
    run(Command::new("cargo").arg("clean").current_dir(&userspace_dir))?;
    let cache = build_cache();
    if cache.exists() {
        fs::remove_dir_all(&cache)?;
        println!("xtask: removed {}", cache.display());
    }
    Ok(())
}

fn cmd_test() -> R<()> {
    // Tools workspace tests (xtask itself, image-builder helpers, etc.).
    let tools_manifest = repo_root().join("tools").join("Cargo.toml");
    run(Command::new("cargo")
        .arg("test")
        .arg("--manifest-path")
        .arg(&tools_manifest))?;

    // Kernel host tests. The kernel's `.cargo/config.toml` pins the
    // build target to `x86_64-unknown-none`, which can't link the
    // standard test harness, so we force the host triple here. `--lib`
    // skips the `[[bin]]` (it's `#![no_main]`, unbuildable on host).
    let host = host_triple()?;
    let kernel_dir = repo_root().join("kernel");
    run(Command::new("cargo")
        .arg("test")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&kernel_dir))?;

    // Userspace `libkern` host tests. From the userspace workspace dir libkern
    // builds for the host (it has no per-crate `.cargo/config.toml` pinning the
    // bare target, unlike the demo bins); `-p libkern` skips those bins and the
    // explicit host `--target` mirrors the kernel approach.
    let userspace_dir = repo_root().join("userspace");
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libkern")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libheap` host tests (the freeing allocator engine, exercised through a
    // `std`-backed arena source). A plain lib (no bare-target bin), host-tested like
    // `libkern`; the target `SyscallSource` is `cfg`'d out under `test`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libheap")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libos` host tests (the async core — the `Op` future + `block_on` + error
    // mapping, against a mock syscall seam). A plain lib; the target syscall path is
    // `cfg`'d out under `test`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libos")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libstream` host tests (the TSM1 wire codec — type tags, header/schema/value
    // round-trips, truncation/EOF handling). Pure `core + alloc`, no deps, so it
    // host-tests unchanged like `libcrypto`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libstream")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // init's library tests (the `manifest` + `toml_lite` parsers). `--lib` skips the
    // `#![no_main]` bin, which can't build for the host.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("init")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `librsproto` host tests (the resource-server protocol wire codec). A plain
    // lib (no bare-target bin), host-tested like `libkern`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("librsproto")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libdraw` host tests — geometry, pixel formats, framebuffers, compositing, and
    // the reference scene's hash. This is the display arm's gate (plan M1 Part A,
    // governing decision 1: "No compositing code merges without the `Framebuffer` trait
    // and host tests behind it"). Pure `core + alloc`, no deps, so compositing is
    // asserted pixel-exactly in milliseconds rather than through a boot.
    // `--features io` so the framebuffer-acquisition arithmetic is covered too:
    // `geometry_from` is where a channel-order or stride mistake would live, and it is
    // pure logic that needs no display. The feature is off by default so the crate's
    // core stays dependency-free.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libdraw")
        .arg("--features")
        .arg("io")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `compositor` host tests — the window model and request dispatch: roles, struts, the
    // buffer lifecycle, stacking, compositing, and the per-connection ownership boundary.
    // `--lib` skips the `#![no_main]` server bin, which cannot build for the host, exactly
    // as for `init` and `service-mgr`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("compositor")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libsurface` host tests — the client-side buffer lifecycle behind a mock transport:
    // which buffer may be drawn into, why single buffering cannot work, and that a release
    // for another window frees nothing. The messages themselves are `librsproto`'s.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libsurface")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libui` host tests — the toolkit's pure half: the element tree and the two-pass
    // layout. Every function under test is a function of values, which is the whole reason
    // the declarative model was chosen (`docs/architecture/widget-toolkit.md` §2).
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libui")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libterm` host tests — terminal semantics, all of which are functions of values: the cell
    // vocabulary, the escape-sequence parser, the grid with its scrollback, the render, and the
    // key encoder. The reason the crate is not part of `libdraw` (`display-arm-plan.md` M5 A2).
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libterm")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `nxterm` host tests — the terminal's state, view and update, which are functions of
    // values. The first real application built on `libui`'s declarative model, and the reason
    // that model was chosen (`widget-toolkit.md` §2).
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("nxterm")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libcrypto` host tests (SHA-256 / HMAC / PBKDF2 against published vectors). A
    // plain `core`-only lib (no bare-target bin), host-tested like `librsproto`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libcrypto")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libtime`'s calendar arithmetic and duration parsing — the same six tests, moved here with
    // the module when it grew a second consumer (M11 Part E batch 9).
    //
    // **Adding a crate to the workspace does not add it to this list**, and that is the whole
    // lesson: these tests ran as part of `-p coreutils --lib` while `time` was a module of it,
    // and moving the module out silently stopped running them. Nothing failed — a moved test that
    // nobody runs passes by not existing (PR #265 review, blocking 1). The century rules in
    // `civil_from_days` are what was left unguarded, and they now feed a clock on the top bar as
    // well as `date`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libtime")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // service-mgr's library tests (the service-declaration parser). `--lib` skips the
    // `#![no_main]` supervisor bin, which can't build for the host.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("service-mgr")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // profile-server's library tests (the profile-manifest parser). `--lib` skips the
    // `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("profile-server")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // auth-service's library tests (the credential logic: user-DB parse + PBKDF2
    // verify + the Auth serve path). `--lib` skips the `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("auth-service")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // clipboard-server's library tests (the kill ring: which entry index 0 names, what a
    // wrap does, when a cycle is refused). `--lib` skips the `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("clipboard-server")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // logging-service's library tests (the log-path classifier). `--lib` skips the
    // `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("logging-service")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `fs-server-ext4` reader-library tests (the ext4 parser, against an `mke2fs`
    // fixture). `--lib` skips the bare-target server `[[bin]]` (added in Part 4).
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("fs-server-ext4")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libfs` — whole-file and path helpers, host-tested where they do not touch a
    // namespace. Was `coreutils::fs` until M10 Part A; it moved down a layer when a
    // graphical file browser needed it and needed none of the shell-program machinery
    // around it.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libfs")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `coreutils` shared-library tests (argument parsing). `--lib` skips the
    // bare-target program `[[bin]]`s.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("coreutils")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `nxfiles` — what a listing *is*: sorted, marked, navigable. Every rule is a decision
    // about a `Vec`, which is why the browser's half that reads a disk is the binary's and this
    // one runs in milliseconds. `--lib` skips the bare-target `[[bin]]`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("nxfiles")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `nxedit` — what an editor *is*: a buffer, a modified marker, and what a failed save does
    // to both. The rule that matters most here is the one a host test can pin and a gate cannot
    // reach without a broken filesystem: a save that fails keeps the buffer.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("nxedit")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `nxsh` language tests — lexer and parser (Milestone 3 Part A). The whole language
    // is a library with no syscalls in it precisely so it can be tested here, in a
    // second, rather than through a 90-second boot. `--lib` skips the bare-target bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("nxsh")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;

    // `tty-server`'s line discipline — the part with all the behaviour and none of the
    // syscalls. Line editing existed three times before this server and the copies
    // disagreed (the `alicepassword:` prompt bug), so the one implementation is tested
    // where testing is cheap.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("tty-server")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;

    // `libinput` — the `SYN` state machine, modifier tracking and the keymap. Pure, and the
    // one place both ends of the protocol interpret input, so it is tested once.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libinput")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;

    // `input-server`'s merge — two devices' streams into one ordered batch, and what a slow
    // consumer is owed. Same split and the same reason: all the behaviour, no syscalls.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("input-server")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    Ok(())
}

/// Enforce the architecture-abstraction boundary: kernel code outside
/// `kernel/src/arch/` must reach the arch layer only through the neutral
/// `crate::arch` interface, never `arch::x86_64::…` internals. The private
/// `mod x86_64` already makes such a path a compile error; this lint is the
/// regression net for comments, doc-links, and future re-export slips that
/// the compiler can't catch. See `docs/conventions/arch-boundary.md`.
/// Enforce the **narrowed** stable-Rust rule: userspace pins a nightly toolchain (it
/// must, to `-Z build-std` a custom hard-float target), but no crate anywhere may use a
/// nightly *language or library* feature.
///
/// Without this check the pin would quietly turn into a licence to reach for
/// `#![feature(...)]`, and the project would drift onto nightly for real. See
/// `userspace/rust-toolchain.toml` and the decision log (2026-07-21 floating-point).
fn cmd_check_nightly() -> R<()> {
    let mut violations: Vec<String> = Vec::new();
    // The two workspaces that ship *in* Nitrox. `tools/` is deliberately excluded:
    // xtask is host tooling, explicitly outside the stable-Rust rule (see this file's
    // module docs). `target/` is pruned because build-std rebuilds `core` from a
    // vendored checkout that legitimately uses nightly features.
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

/// The placeholder is spelt `TODO(<tag>)` throughout this file — angle brackets included — and
/// that is load-bearing: this gate scans `tools/xtask/src`, so the bare form in its own prose
/// was being counted as a real marker named `tag`, inflating the tally and, worse, making the
/// gate depend on `deferred-decisions.md` continuing to contain the same placeholder. The
/// angle brackets fail the plain-word check, so [`markers_in_line`] skips them.
///
/// This sentence used to spell the bare form out to explain itself, which was invisible only
/// because the code scan stopped at the first marker on a line and that one was the bracketed
/// version. Sharing [`markers_in_line`] between both directions surfaced it immediately — the
/// third self-reference in this file, and the one the previous bug was hiding.
///
/// Every `TODO(<tag>)` in the shipping source must have a matching entry in
/// `docs/rationale/deferred-decisions.md`.
///
/// This exists because of a specific, repeated failure: the three deferrals that cost the
/// most to rediscover (exit-time handle reclamation, the wall clock, file truncate) were
/// each recorded *somewhere other than* the canonical list — one in an architecture doc,
/// one as a `TODO` tag in the kernel's syscall table, one in a crate `CLAUDE.md` — so none was
/// ever reviewed, and each surfaced only when a consumer tripped over it (audit,
/// 2026-07-24). A `TODO` is a deferral; this makes the code half of that mechanical.
///
/// The document must name the tag **literally** — `TODO(msi)`, not just the word "msi" —
/// because a bare short tag (`mm`) matches half the prose in any technical document, which
/// would make the check pass without recording anything. Naming it also makes the entry
/// searchable from the code and vice versa.
/// One family of ABI constants mirrored between the kernel and `userspace/libkern`.
///
/// `pattern` is matched per line on both sides; capture group semantics are handled by
/// [`extract_consts`] (name, then value). The two sides use the *same* shape within a
/// family — `pub const SYS_X: u64 = N;` on both, `Name = -N,` enum variants on both — which
/// is what makes a line-based comparison sound here rather than needing a Rust parser.
struct AbiFamily {
    /// Human name for the report.
    what: &'static str,
    kernel_file: &'static str,
    user_file: &'static str,
    /// Which line shape to extract. See [`extract_consts`].
    shape: AbiShape,
    /// Names legitimately present on **one** side only, each with the reason. An
    /// unexplained one-sided name is a finding; a listed one is a documented asymmetry.
    /// Keeping this explicit is what stops the check from being noisy enough to disable.
    one_sided: &'static [(&'static str, &'static str)],
}

#[derive(Copy, Clone, PartialEq)]
enum AbiShape {
    /// `pub const NAME: u64 = <int>;`
    U64Const,
    /// `    Name = <int>,` — an enum variant with an explicit discriminant.
    EnumVariant,
    /// `pub const NAME: Rights = Rights(1 << k);`
    RightsBit,
    /// `pub const NAME: u16 = <int>;` — the input event classes and codes.
    U16Const,
}

/// The ABI surfaces `userspace/libkern` mirrors by hand, and therefore the ones that can
/// silently drift. Layout of `#[repr(C)]` types is deliberately **not** here: both sides
/// already carry `offset_of!`/`size_of` compile-time asserts, which is a stronger check than
/// text comparison and fails at build time.
const ABI_FAMILIES: &[AbiFamily] = &[
    AbiFamily {
        what: "syscall numbers",
        kernel_file: "kernel/src/syscall/table.rs",
        user_file: "userspace/libkern/src/syscall.rs",
        shape: AbiShape::U64Const,
        one_sided: &[
            ("SYS_TEST_EXIT", "kernel test-harness builds only; userspace mirrors it for the harness"),
        ],
    },
    AbiFamily {
        what: "KError discriminants",
        kernel_file: "kernel/src/syscall/error.rs",
        user_file: "userspace/libkern/src/error.rs",
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
    AbiFamily {
        what: "Rights bits",
        kernel_file: "kernel/src/libkern/handle.rs",
        user_file: "userspace/libkern/src/handle.rs",
        shape: AbiShape::RightsBit,
        one_sided: &[],
    },
    AbiFamily {
        what: "input event classes and codes",
        kernel_file: "kernel/src/libkern/input.rs",
        user_file: "userspace/libkern/src/abi.rs",
        shape: AbiShape::U16Const,
        one_sided: &[],
    },
    AbiFamily {
        what: "KObjectType discriminants",
        kernel_file: "kernel/src/libkern/handle.rs",
        user_file: "userspace/libkern/src/handle.rs",
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
];

/// Individually-named constants that mirror across the boundary under *different* names or
/// in unrelated files, so a family sweep cannot pair them. Each is `(kernel file, kernel
/// name, userspace file, userspace name)`.
///
/// These are the ones that bit in practice: `MAX_WAIT_HANDLES` and the IPC limits are
/// hand-copied values with no compile-time tie between the two sides at all.
const ABI_PAIRS: &[(&str, &str, &str, &str)] = &[
    (
        "kernel/src/object/thread.rs",
        "MAX_WAIT_HANDLES",
        "userspace/libkern/src/abi.rs",
        "MAX_WAIT_HANDLES",
    ),
    (
        "kernel/src/libkern/ipc.rs",
        "IPC_HANDLE_MAX",
        "userspace/libkern/src/abi.rs",
        "IPC_HANDLE_MAX",
    ),
];

/// Pull `name -> value` pairs of one `shape` out of a source file.
fn extract_consts(text: &str, shape: AbiShape) -> BTreeMap<String, i128> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        match shape {
            AbiShape::U64Const => {
                // pub const NAME: u64 = <int>;
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                let Some((ty, val)) = tail.split_once('=') else { continue };
                if ty.trim() != "u64" {
                    continue;
                }
                if let Some(v) = parse_int(val) {
                    out.insert(name.trim().to_string(), v);
                }
            }
            AbiShape::U16Const => {
                // pub const NAME: u16 = <int>;  (and the i32 `KEY_*` values alongside them)
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                let Some((ty, val)) = tail.split_once('=') else { continue };
                if ty.trim() != "u16" && ty.trim() != "i32" {
                    continue;
                }
                if let Some(v) = parse_int(val) {
                    out.insert(name.trim().to_string(), v);
                }
            }
            AbiShape::EnumVariant => {
                // Name = <int>,
                let Some(body) = t.strip_suffix(',') else { continue };
                let Some((name, val)) = body.split_once('=') else { continue };
                let name = name.trim();
                if name.is_empty()
                    || !name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    continue;
                }
                if let Some(v) = parse_int(val) {
                    out.insert(name.to_string(), v);
                }
            }
            AbiShape::RightsBit => {
                // pub const NAME: Rights = Rights(1 << k);
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                if !tail.contains("Rights(") {
                    continue;
                }
                let Some((_, shifted)) = tail.split_once("1 <<") else { continue };
                let digits: String =
                    shifted.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(k) = digits.parse::<u32>() {
                    out.insert(name.trim().to_string(), 1i128 << k);
                }
            }
        }
    }
    out
}

/// Parse a trailing integer literal (decimal or `0x`), tolerating `_` and a `;`/`,` tail.
fn parse_int(s: &str) -> Option<i128> {
    let t = s.trim().trim_end_matches([';', ',']).trim().replace('_', "");
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i128::from_str_radix(hex, 16).ok();
    }
    t.parse::<i128>().ok()
}

/// Find `pub const NAME: <ty> = <int>;` for one specific name, whatever the type.
fn extract_named(text: &str, want: &str) -> Option<i128> {
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("pub const ") else { continue };
        let Some((name, tail)) = rest.split_once(':') else { continue };
        if name.trim() != want {
            continue;
        }
        let Some((_, val)) = tail.split_once('=') else { continue };
        return parse_int(val);
    }
    None
}

/// `cargo xtask abi-sync-check` — verify `userspace/libkern` still mirrors the kernel ABI.
///
/// `libkern` is a **hand-maintained** copy of the kernel's syscall numbers, error and object
/// discriminants, rights bits, and shared limits. Nothing in the build ties the two together,
/// so an edit to one side is invisible until something misbehaves at runtime. This compares
/// them and fails on a mismatch, a name the kernel has and userspace lacks, or a one-sided
/// name that is not documented as such.
///
/// Deliberately **not** checked here: `#[repr(C)]` layouts. Both sides already assert their
/// own field offsets and sizes at compile time, which is stronger than text comparison and
/// fails earlier.
fn cmd_abi_sync_check() -> R<()> {
    let root = repo_root();
    let mut problems: Vec<String> = Vec::new();
    let mut compared = 0usize;

    for fam in ABI_FAMILIES {
        let kt = fs::read_to_string(root.join(fam.kernel_file))
            .map_err(|e| format!("read {}: {e}", fam.kernel_file))?;
        let ut = fs::read_to_string(root.join(fam.user_file))
            .map_err(|e| format!("read {}: {e}", fam.user_file))?;
        let k = extract_consts(&kt, fam.shape);
        let u = extract_consts(&ut, fam.shape);
        if k.is_empty() {
            problems.push(format!(
                "{}: extracted nothing from {} — the checker's pattern has gone stale, \
                 which silently disables this family",
                fam.what, fam.kernel_file
            ));
            continue;
        }
        for (name, kv) in &k {
            match u.get(name) {
                Some(uv) if uv == kv => compared += 1,
                Some(uv) => problems.push(format!(
                    "{}: {} is {} in {} but {} in {}",
                    fam.what, name, kv, fam.kernel_file, uv, fam.user_file
                )),
                None => {
                    if !fam.one_sided.iter().any(|(n, _)| n == name) {
                        problems.push(format!(
                            "{}: {} exists in {} but not in {} — mirror it, or record it in \
                             the checker's `one_sided` list with a reason",
                            fam.what, name, fam.kernel_file, fam.user_file
                        ));
                    }
                }
            }
        }
    }

    match check_kerror_decode_table(&root) {
        Ok(n) => compared += n,
        Err(mut found) => problems.append(&mut found),
    }

    for (kf, kn, uf, un) in ABI_PAIRS {
        let kt = fs::read_to_string(root.join(kf)).map_err(|e| format!("read {kf}: {e}"))?;
        let ut = fs::read_to_string(root.join(uf)).map_err(|e| format!("read {uf}: {e}"))?;
        match (extract_named(&kt, kn), extract_named(&ut, un)) {
            (Some(a), Some(b)) if a == b => compared += 1,
            (Some(a), Some(b)) => problems.push(format!(
                "shared limit: {kn} is {a} in {kf} but {un} is {b} in {uf}"
            )),
            (a, _) => problems.push(format!(
                "shared limit: could not read {} — the checker's pattern has gone stale",
                if a.is_none() { format!("{kn} in {kf}") } else { format!("{un} in {uf}") }
            )),
        }
    }

    if !problems.is_empty() {
        let mut msg = String::from(
            "userspace/libkern must mirror the kernel ABI exactly (docs/spec/syscall-abi.md); \
             these disagree:\n",
        );
        for p in &problems {
            msg.push_str("  ");
            msg.push_str(p);
            msg.push('\n');
        }
        return Err(msg.into());
    }
    println!("abi-sync-check: {compared} ABI value(s) agree between the kernel and libkern ✓");
    Ok(())
}

/// Verify `libkern::KError::from_i32` decodes **every** kernel `KError` variant.
///
/// Mirroring the enum is not enough. `from_i32` is a second, hand-written copy of the
/// same table, and its `_ => KernelError` catch-all — deliberate forward-compat, so a
/// newer kernel's error does not panic an older `libkern` — means a *missing* arm is
/// indistinguishable from an unknown code at runtime. `IoError` sat in both enums with
/// matching discriminants and no arm here from 2026-06 to 2026-07-30: every device error
/// silently decoded as `KernelError`, `abi-sync-check` passed, and the round-trip test in
/// `error.rs` missed it because that test enumerates variants by hand too.
///
/// So this derives the expected set from the **kernel's** enum. A variant added to the
/// kernel and mirrored into the userspace enum but forgotten here is now a guard failure,
/// which is the only place in the chain that does not depend on someone remembering.
fn check_kerror_decode_table(root: &Path) -> Result<usize, Vec<String>> {
    const KERNEL: &str = "kernel/src/syscall/error.rs";
    const USER: &str = "userspace/libkern/src/error.rs";

    let read = |p: &str| {
        fs::read_to_string(root.join(p)).map_err(|e| vec![format!("KError decode: read {p}: {e}")])
    };
    let kt = read(KERNEL)?;
    let ut = read(USER)?;

    let kernel = extract_consts(&kt, AbiShape::EnumVariant);
    if kernel.is_empty() {
        return Err(vec![format!(
            "KError decode: extracted no variants from {KERNEL} — the checker's pattern has \
             gone stale, which silently disables this check"
        )]);
    }

    // `    -40 => KError::IoError,` → (-40, "IoError"). The wildcard arm and every other
    // line shape fall out on the integer parse.
    let mut arms: BTreeMap<i128, String> = BTreeMap::new();
    // The variant the `_` arm yields. It needs no explicit arm of its own: an unlisted
    // code already lands on it, including its own. Every *other* variant does.
    let mut catch_all: Option<String> = None;
    for line in ut.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        let Some((lhs, rhs)) = t.split_once("=>") else { continue };
        let Some(name) = rhs.trim().trim_end_matches(',').strip_prefix("KError::") else {
            continue;
        };
        match lhs.trim() {
            "_" => catch_all = Some(name.to_string()),
            n => {
                let Ok(code) = n.parse::<i128>() else { continue };
                arms.insert(code, name.to_string());
            }
        }
    }
    if arms.is_empty() {
        return Err(vec![format!(
            "KError decode: found no `<int> => KError::Name` arms in {USER} — either \
             `from_i32` was rewritten in another shape, or the checker's pattern has gone \
             stale. Either way this check is no longer checking anything"
        )]);
    }

    let mut problems = Vec::new();
    for (name, value) in &kernel {
        if catch_all.as_deref() == Some(name.as_str()) {
            continue;
        }
        match arms.get(value) {
            Some(mapped) if mapped == name => {}
            Some(mapped) => problems.push(format!(
                "KError decode: {USER} decodes {value} as {mapped}, but {KERNEL} defines \
                 {value} as {name}"
            )),
            None => problems.push(format!(
                "KError decode: {KERNEL} defines {name} = {value}, but {USER}'s `from_i32` \
                 has no arm for it — it would decode as KernelError, silently"
            )),
        }
    }
    if problems.is_empty() {
        Ok(kernel.len())
    } else {
        Err(problems)
    }
}

fn cmd_check_deferrals() -> R<()> {
    let doc_path = repo_root()
        .join("docs")
        .join("rationale")
        .join("deferred-decisions.md");
    let doc = fs::read_to_string(&doc_path)
        .map_err(|e| format!("read {}: {e}", doc_path.display()))?
        .to_lowercase();

    let mut violations: Vec<String> = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    for ws in ["kernel/src", "userspace", "tools/xtask/src"] {
        let src_root = repo_root().join(ws);
        visit_rs_files_skipping(&src_root, &["target"], &mut |path| {
            let text = fs::read_to_string(path)?;
            for (i, line) in text.lines().enumerate() {
                for tag in markers_in_line(line) {
                    if !tags.iter().any(|t| t == tag) {
                        tags.push(tag.to_string());
                    }
                    if !doc.contains(&format!("todo({})", tag.to_lowercase())) {
                        violations.push(format!(
                            "{}:{}: TODO({tag}) has no entry in deferred-decisions.md",
                            path.display(),
                            i + 1
                        ));
                    }
                }
            }
            Ok(())
        })?;
    }

    // **The other direction**, which the gate did not ask until 2026-08-18. Enforcing only
    // code-to-doc leaves the failure that actually happens unchecked: an entry whose marker was
    // deleted or never written is one nobody trips over while editing the code, which is the
    // whole reason the markers exist. Audit D.5(b) measured 9 of 28 open entries binding to
    // nothing, four of them appearing nowhere else in the repository.
    let raw = fs::read_to_string(&doc_path)
        .map_err(|e| format!("read {}: {e}", doc_path.display()))?;
    let open_tags = open_section_tags(&raw);
    let exempt_count = open_tags.iter().filter(|(_, e)| *e).count();
    for (tag, exempt) in &open_tags {
        if *exempt || tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
            continue;
        }
        violations.push(format!(
            "deferred-decisions.md: TODO({tag}) is an open entry with no TODO({tag}) marker \
             anywhere in kernel/src, userspace or tools/xtask/src — put one where someone \
             changing that code will trip over it, or mark the entry `{NO_CODE_SITE}` if the \
             feature genuinely has no code site yet"
        ));
    }

    if violations.is_empty() {
        println!(
            "check-deferrals: {} tag(s) in code, all recorded; {} open entr(ies), all backed \
             by a marker ({} exempt) ✓",
            tags.len(),
            open_tags.len(),
            exempt_count
        );
        Ok(())
    } else {
        let mut msg = String::from(
            "`TODO(<tag>)` and deferred-decisions.md must agree in **both** directions — a \
             deferral only exists if it is in the canonical list, and one nobody can trip \
             over while editing the code is one nobody will act on (see that document's \
             closing section):\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}

/// Every `TODO(<name>)` marker on one line, in order, skipping anything that is not a plain
/// searchable word.
///
/// **One helper because the two directions disagreed.** The code scan used a single
/// `split_once`, so it saw only the *first* marker on a line and skipped the rest of the line
/// entirely when that one failed the plain-word check; the doc scan looped. While only the
/// code-to-doc direction existed that asymmetry cost silent under-enforcement. Once the
/// reverse direction depends on the code scan being complete it becomes a false failure: two
/// markers written on one line, and the gate reports the second as missing while pointing at
/// the line it is on.
fn markers_in_line(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some((_, after)) = rest.split_once("TODO(") {
        let Some((tag, tail)) = after.split_once(')') else {
            break;
        };
        rest = tail;
        // A tag has to be a plain word to be searchable; anything else is prose that happens
        // to contain the marker — including this module's own `TODO(<tag>)` placeholder.
        if !tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            out.push(tag);
        }
    }
    out
}

/// Marker exempting one deferral entry from the doc-to-code direction.
const NO_CODE_SITE: &str = "<!-- check-deferrals: no-code-site -->";

/// Every `TODO(<tag>)` in the document's **open** section, paired with whether its line
/// exempts it from needing a code marker.
///
/// **Open section only, and that boundary is the whole subtlety.** A resolved entry has no
/// code marker *because closing it deleted the marker* — that is the lifecycle working. A
/// first pass at this measurement scraped the whole file, counted `shell-cwd` (named
/// narratively inside a Resolved row, describing the deletion of its own markers) as rot, and
/// would have sent the follow-up work to reinstate the very marker `check-deferrals` had
/// caught. The same cut excludes the prose `TODO(<tag>)` in the closing "How to use this
/// document" section, which sits below `## Resolved`.
fn open_section_tags(doc: &str) -> Vec<(String, bool)> {
    let open = match doc.find("\n## Resolved") {
        Some(i) => &doc[..i],
        None => doc,
    };
    let mut out: Vec<(String, bool)> = Vec::new();
    for line in open.lines() {
        let exempt = line.contains(NO_CODE_SITE);
        for tag in markers_in_line(line) {
            // **Exemption ORs across occurrences, it does not take the first.** A tag is
            // routinely named on an earlier line than its own entry — `tty-server`'s entry
            // cross-references `TODO(session-metadata-server)` 170 lines above that entry —
            // and first-occurrence-wins would then discard a marker written on the entry
            // itself, failing the gate while telling you to do the thing you just did.
            match out.iter_mut().find(|(t, _)| t == tag) {
                Some(e) => e.1 |= exempt,
                None => out.push((tag.to_string(), exempt)),
            }
        }
    }
    out
}

/// The initramfs files a **test** image is allowed to differ from a **release** image in.
///
/// Two are data — the extra service declarations, and the profile manifest that lists the
/// test-harness store package. One is code: `sbin/init`, for the single `#[cfg(feature =
/// "selftest")]` that makes the `/subtreetest` binding, which cannot be expressed as manifest
/// data until `init.toml` grows a bind-mount concept (`test-path-retrofit.md` Part C).
///
/// **Everything else must be byte-identical**, and that is the whole claim of the retrofit:
/// the software under test is the software that ships. Five of the eight entries are, today.
const IMAGE_DIVERGENCE_ALLOWED: &[&str] =
    &["etc/profiles/system.toml", "etc/services.toml", "sbin/init"];

/// `cargo xtask check-images` — a test image may differ from a release image only in **data**,
/// plus the one code difference still on the books.
///
/// Builds both initramfs archives and compares them file by file. A new divergence fails,
/// which is what makes the retrofit's result a wall rather than a measurement.
///
/// **What it actually catches**, stated precisely because the first version of this comment
/// overstated it (PR #230 review, finding 1): wiring `mode.features()` into a build that does
/// not take one — the shape Part B removed from `session-mgr` — makes that program's ELF differ
/// here. A bare `#[cfg(feature = "test-harness")]` does **not**, because `eshell`,
/// `fs-server-ext4` and `profile-server` declare no features, so the cfg is inert in both modes.
/// The invariant is therefore "no build-mode-varying input reaches these programs", which is the
/// useful one — a cfg that nothing can turn on is not a divergence.
///
/// A consequence worth knowing: most of the byte-identity is cargo not rebuilding a crate whose
/// feature set did not change. That is the invariant holding, not an independent measurement of
/// it.
///
/// It is deliberately a check on the **set** of differing files rather than on a byte count:
/// the allowed three change size whenever a declaration is added, and pinning sizes would make
/// this fail for the right reason at the wrong time.
///
/// **It is one-directional.** A file that *stops* differing — `sbin/init`, when the
/// `/subtreetest` binding finally becomes manifest data — leaves the allow-list stale with
/// nothing to say so. That is the harmless direction, and tightening it would mean failing a
/// build for getting *better*; the cost is that the list needs pruning by hand when a box in
/// `test-path-retrofit.md` closes.
fn cmd_check_images() -> R<()> {
    let dir = build_cache().join("check-images");
    fs::create_dir_all(&dir)?;
    let release = dir.join("release.cpio");
    let test = dir.join("test.cpio");

    // `cmd_build` first: `build_initramfs` packs ELFs that must already exist, and the two
    // modes produce different `init` bytes.
    cmd_build(BuildMode::Normal)?;
    build_initramfs(&release, BuildMode::Normal)?;
    cmd_build(BuildMode::TestHarness)?;
    build_initramfs(&test, BuildMode::TestHarness)?;

    let r = cpio_entries(&fs::read(&release)?);
    let t = cpio_entries(&fs::read(&test)?);

    let only_release: Vec<&String> = r.keys().filter(|k| !t.contains_key(*k)).collect();
    let only_test: Vec<&String> = t.keys().filter(|k| !r.contains_key(*k)).collect();
    if !only_release.is_empty() || !only_test.is_empty() {
        return Err(format!(
            "the two images no longer carry the same set of files — release-only {only_release:?}, \
             test-only {only_test:?}. The initramfs program list is deliberately identical in \
             every build mode (see `build_initramfs`)."
        )
        .into());
    }

    let mut differ: Vec<&String> = r.iter().filter(|(k, v)| t.get(*k) != Some(v)).map(|(k, _)| k).collect();
    differ.sort();
    let unexpected: Vec<&&String> =
        differ.iter().filter(|k| !IMAGE_DIVERGENCE_ALLOWED.contains(&k.as_str())).collect();
    if !unexpected.is_empty() {
        return Err(format!(
            "a test image and a release image now differ in {unexpected:?}, which is not on the \
             allowed list. If that is a new `#[cfg(feature = \"test-harness\")]` or \
             `#[cfg(feature = \"selftest\")]`, it is the divergence \
             `docs/planning/test-path-retrofit.md` exists to remove — prefer a service \
             declaration or a host-side assertion. If it is deliberate, add it to \
             `IMAGE_DIVERGENCE_ALLOWED` with the reason."
        )
        .into());
    }
    println!(
        "check-images: {} initramfs files, {} byte-identical between a test and a release \
         image; {:?} differ, all allowed ✓",
        r.len(),
        r.len() - differ.len(),
        differ
    );
    Ok(())
}

/// Parse a CPIO `newc` archive into `name -> contents`. Mirrors `cpio_entry`'s writer.
///
/// **`TRAILER!!!` is skipped.** It is the zero-byte end-of-archive sentinel, not a file, and
/// counting it inflated every number this gate prints by one — "8 entries, 5 byte-identical"
/// for what is 7 files and 4 (PR #230 review, finding 6).
fn cpio_entries(blob: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut i = 0usize;
    let hex = |b: &[u8]| usize::from_str_radix(std::str::from_utf8(b).unwrap_or("0"), 16).unwrap_or(0);
    while i + 110 <= blob.len() {
        if &blob[i..i + 6] != b"070701" {
            break;
        }
        let fs = hex(&blob[i + 54..i + 62]);
        let ns = hex(&blob[i + 94..i + 102]);
        let name = String::from_utf8_lossy(&blob[i + 110..i + 110 + ns.saturating_sub(1)]).into_owned();
        i = (i + 110 + ns).div_ceil(4) * 4;
        let end = (i + fs).min(blob.len());
        if name != "TRAILER!!!" {
            out.insert(name, blob[i..end].to_vec());
        }
        i = (i + fs).div_ceil(4) * 4;
    }
    out
}

/// `cargo xtask check-docs` — the documentation cannot point at things that do not exist.
///
/// Three cheap, mechanical checks. None of them can tell whether a doc's *prose* is true;
/// they exist because the 2026-08-05 consistency pass found that a large share of the
/// drift was not subtle at all. `docs/architecture/overview.md` — the file the root
/// `CLAUDE.md` tells you to read first — linked to five documents that had never been
/// written, and a spec cited a `librsproto` source file that does not exist. A reader who
/// follows a dead reference goes to the source and builds a private model, which is one of
/// the ways the docs and the code came apart in the first place.
///
/// 1. **Link integrity.** Every relative `[text](path.md)` link resolves.
/// 2. **Cited source paths.** A backticked `kernel/…`, `userspace/…` or `tools/…` path in a
///    doc that describes *current behaviour* must exist.
/// 3. **Status lines.** Every `docs/architecture/*.md` carries one, because `CLAUDE.md`
///    promises it and tells readers to trust it over the body's tense.
///
/// **Check 2 is deliberately scoped, and the escape hatch is deliberate too.** It is an
/// *allowlist*, not a denylist: only `docs/spec/`, `docs/reference/`, `docs/architecture/`
/// and `docs/conventions/` are scanned for cited source paths (see `describes_current`).
/// Everything else — `design/`, `archive/`, `planning/`, `rationale/`, the top-level
/// `docs/*.md`, and the `CLAUDE.md` files — is exempt, because those either do not describe
/// current behaviour or record what *was* true: a ticked box reading "Retire
/// `kernel/src/embedded_images.rs` entirely" cites a path absent *because the work
/// succeeded*, and rewriting it would corrupt the record.
///
/// The remaining false positive is the honest forward reference — `user-memory-access.md`
/// says "*When* aarch64 is implemented its primitives live in …" — so a line carrying
/// `<!-- check-docs: allow-missing -->` is exempt anywhere. Both shapes were real findings,
/// not hypotheticals.
fn cmd_check_docs() -> R<()> {
    const ALLOW: &str = "<!-- check-docs: allow-missing -->";
    let root = repo_root();

    // Docs that describe how the system behaves today. `design/` (subsystems not yet
    // built), `archive/` (superseded artifacts) and `planning/` (intent, with checkboxes)
    // are excluded from the source-path check for the reason in the doc comment above.
    let describes_current = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        ["/docs/spec/", "/docs/reference/", "/docs/architecture/", "/docs/conventions/"]
            .iter()
            .any(|d| s.contains(d))
    };

    // Append-only records. Their backticked path mentions describe the world as it was —
    // an entry explaining that `docs/history/decision-log.md` moved has to name the old
    // path — so the cited-path checks are not applied to them, and the alternative was a
    // `check-docs: allow-missing` marker on every future entry that describes a rename.
    // Their markdown *links* are still checked: prose about an old path is a record, but a
    // link is navigation, and a reader will click it.
    let is_record = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        s.ends_with("/docs/decision-log.md") || s.contains("/docs/archive/")
    };

    let mut violations: Vec<String> = Vec::new();
    let (mut links, mut paths) = (0usize, 0usize);
    // `(citing file, line, target file, anchor)`, resolved after the walk — a link into
    // *another* document's heading needs that document read, which the per-file pass below
    // cannot do without re-reading it once per link.
    let mut anchor_refs: Vec<(PathBuf, usize, PathBuf, String)> = Vec::new();

    // Everything that documents this project, not just `docs/`. The `CLAUDE.md` files are
    // instructions Claude Code loads directly, and `.claude/skills/` defines the review
    // procedure — both cite doc paths, and both were missed by an earlier version of this
    // gate that walked `docs/` alone. `docs/history/` was dissolved while `SKILL.md` still
    // told a reviewer to file decisions there, and nothing caught it.
    let mut roots: Vec<PathBuf> = ["docs", ".claude", "kernel", "userspace", "tools"]
        .iter()
        .map(|d| root.join(d))
        .collect();
    roots.retain(|p| p.exists());

    // Every `*.md` basename in the repo, so a bare backticked filename can be resolved.
    let mut known_md: Vec<String> = Vec::new();
    for r in &roots {
        visit_md_files_skipping(r, &["target", "build-cache"], &mut |p| {
            if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                known_md.push(n.to_string());
            }
            Ok(())
        })?;
    }
    known_md.push("CLAUDE.md".into());

    let mut check = |path: &Path| -> R<()> {
        let text = fs::read_to_string(path)?;
        let dir = path.parent().unwrap_or(&root).to_path_buf();
        for (i, line) in text.lines().enumerate() {
            let at = |m: &str| format!("{}:{}: {m}", path.display(), i + 1);

            // 1. Relative markdown links to .md files.
            //
            // **Inline code is not markdown.** A document that *quotes* link syntax — this
            // gate's own entry in `decision-log.md` writes ``](#anchor)`` — is talking about
            // links, not making one. Blanking the code spans first is what lets prose describe
            // the syntax without the checker chasing it.
            let masked = mask_code_spans(line);
            let mut rest = masked.as_str();
            while let Some(open) = rest.find("](") {
                let after = &rest[open + 2..];
                let Some(close) = after.find(')') else { break };
                let target = &after[..close];
                rest = &after[close..];
                let file = target.split('#').next().unwrap_or("");
                if target.contains("://") {
                    continue;
                }
                // 1b. `](#anchor)` and `](other.md#anchor)`. A heading link that resolves to
                // nothing is silent in every renderer — it scrolls nowhere — and this gate
                // skipped it for years because it split on `#` and dropped what followed. A
                // Part C link to `#close-0x0925` (the heading is `RequestClose … and Close …`,
                // which slugs to something else entirely) is what found it; four more were
                // already broken, one of them since the section was renamed.
                if let Some((_, anchor)) = target.split_once('#')
                    && !anchor.is_empty()
                    // `#L36` is a GitHub line link, not a heading — it resolves on the web
                    // and nowhere else, and it is deliberate where it appears.
                    && !(anchor.starts_with('L')
                        && anchor[1..].chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'L'))
                {
                    let target_file =
                        if file.is_empty() { path.to_path_buf() } else { dir.join(file) };
                    if target_file.extension().map_or(false, |e| e == "md") {
                        anchor_refs.push((
                            path.to_path_buf(),
                            i + 1,
                            target_file,
                            anchor.to_string(),
                        ));
                    }
                }
                if file.is_empty() || !file.ends_with(".md") {
                    continue;
                }
                links += 1;
                if !dir.join(file).exists() {
                    violations.push(at(&format!("link target does not exist: {file}")));
                }
            }

            // 2a. Backticked `docs/…​.md` references, in every doc. These are prose, not
            // markdown links, so check 1 never sees them — which is exactly how
            // `docs/history/design-doc-v5.1.md` survived in `overview.md` for months
            // while the file was really named `os-design-v5.1.md`.
            if !line.contains(ALLOW) && !is_record(path) {
                for piece in line.split('`').skip(1).step_by(2) {
                    // `docs/spec/rsproto-*.md` names a family, not a file.
                    if !piece.ends_with(".md") || piece.contains('*') || piece.contains(' ') {
                        continue;
                    }
                    if piece.starts_with("docs/") {
                        paths += 1;
                        if !root.join(piece).exists() {
                            violations.push(at(&format!("cited doc does not exist: {piece}")));
                        }
                    } else if !piece.contains('/') {
                        // A bare filename, e.g. ``see `desktop-shell.md` ``. Renaming a doc
                        // leaves these behind — the `docs/…`-prefixed form gets swept and
                        // the bare one does not. Dissolving `docs/history/` left 11 of them.
                        paths += 1;
                        if !known_md.iter().any(|k| k == piece) {
                            violations.push(at(&format!(
                                "cited doc does not exist anywhere in the repo: {piece}"
                            )));
                        }
                    }
                }
            }

            // 2. Backticked repo-relative source paths, in current-behaviour docs only.
            if describes_current(path) && !line.contains(ALLOW) {
                for piece in line.split('`').skip(1).step_by(2) {
                    let ok_prefix = ["kernel/", "userspace/", "tools/"]
                        .iter()
                        .any(|p| piece.starts_with(p));
                    let ok_suffix = piece.ends_with(".rs") || piece.ends_with(".toml");
                    // `kernel/src/arch/<arch>/registers.rs` is a template, not a path.
                    let placeholder = piece.contains('<') || piece.contains('>');
                    if !ok_prefix || !ok_suffix || piece.contains(' ') || placeholder {
                        continue;
                    }
                    paths += 1;
                    if !root.join(piece).exists() {
                        violations.push(at(&format!(
                            "cited source path does not exist: {piece} \
                             (if the reference is deliberate, mark the line `{ALLOW}`)"
                        )));
                    }
                }
            }
        }
        Ok(())
    };

    for r in &roots {
        visit_md_files_skipping(r, &["target"], &mut check)?;
    }

    // 1b (continued). Resolve the collected heading links, reading each target once.
    let mut anchor_cache: std::collections::BTreeMap<PathBuf, Vec<String>> =
        std::collections::BTreeMap::new();
    let anchors = anchor_refs.len();
    for (src, line, target, anchor) in anchor_refs {
        if !target.exists() {
            continue; // already reported by check 1 as a missing link target
        }
        let known = match anchor_cache.get(&target) {
            Some(a) => a,
            None => {
                let a = heading_anchors(&fs::read_to_string(&target)?);
                anchor_cache.entry(target.clone()).or_insert(a)
            }
        };
        if !known.iter().any(|k| k == &anchor) {
            let near = known
                .iter()
                .find(|k| k.starts_with(anchor.split('-').next().unwrap_or(&anchor)))
                .map(|k| format!(" (did you mean #{k}?)"))
                .unwrap_or_default();
            violations.push(format!(
                "{}:{line}: link to a heading that does not exist: #{anchor}{near}",
                src.display()
            ));
        }
    }

    // 3. Every architecture and design doc states what is actually built. `design/` needs
    //    it most: those documents describe subsystems with no code behind them.
    let mut arch_docs = 0usize;
    for sub in ["architecture", "design"] {
        let dir = root.join("docs").join(sub);
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().map_or(false, |e| e == "md") {
                arch_docs += 1;
                let text = fs::read_to_string(&path)?;
                let has_status = text.lines().take(20).any(|l| {
                    let t = l
                        .trim_start_matches('#')
                        .trim_start_matches("> ")
                        .trim_start_matches('*')
                        .trim_start()
                        .to_lowercase();
                    t.starts_with("status")
                });
                if !has_status {
                    violations.push(format!(
                        "{}: no Status line in the first 20 lines — every architecture and \
                         design doc must state what is actually built (root CLAUDE.md \
                         promises this)",
                        path.display()
                    ));
                }
            }
        }
    }

    // 4. The two hand-maintained numeric tables must agree with the kernel.
    //
    // `abi-sync-check` ties the kernel to `libkern`; nothing tied either to the docs, so
    // `syscall-abi.md` and `error-codes.md` could drift from both and no gate would notice.
    // They happen to agree today (37 syscalls, 16 error codes, checked 2026-08-05) — this
    // keeps it that way, for the same reason `abi-sync-check` exists.
    let mut table_stats: Vec<String> = Vec::new();
    for (what, src_rel, doc_rel) in [
        ("syscall", "kernel/src/syscall/table.rs", "docs/spec/syscall-abi.md"),
        ("error code", "kernel/src/syscall/error.rs", "docs/reference/error-codes.md"),
    ] {
        let src_text = fs::read_to_string(root.join(src_rel))?;
        let doc_text = fs::read_to_string(root.join(doc_rel))?;

        // `12`, or `0xFFFF_0000` for the debug-only range.
        let parse_num = |t: &str| -> Option<i64> {
            let t = t.trim().trim_end_matches([';', ',']).trim();
            match t.strip_prefix("0x") {
                Some(hex) => i64::from_str_radix(&hex.replace('_', ""), 16).ok(),
                None => t.parse::<i64>().ok(),
            }
        };

        let mut from_src: Vec<(i64, String)> = Vec::new();
        let mut declared = 0usize;
        if what == "syscall" {
            // `pub const SYS_FOO_BAR: u64 = 12;` → (12, "sys_foo_bar")
            for line in src_text.lines() {
                let Some(rest) = line.trim().strip_prefix("pub const SYS_") else { continue };
                declared += 1;
                let Some((name, tail)) = rest.split_once(": u64 = ") else { continue };
                let Some(num) = parse_num(tail) else { continue };
                from_src.push((num, format!("sys_{}", name.to_ascii_lowercase())));
            }
            // Every declared constant must have parsed. Without this, a number the parser
            // cannot read is dropped *silently* — and if the doc omits it too, the two
            // omissions cancel and the check reports agreement it never tested. That is
            // how `0xFFFF_0000` slipped through: 39 constants, 37 compared, green.
            if declared != from_src.len() {
                violations.push(format!(
                    "{src_rel}: {declared} `pub const SYS_` declarations but only {} parsed — \
                     the rest are dropped silently, so the comparison is incomplete. Teach \
                     cmd_check_docs the number format it could not read.",
                    from_src.len()
                ));
            }
        } else {
            // The `KError` enum body only — a stray `= -1` elsewhere must not match.
            let body = match src_text.find("enum KError") {
                Some(a) => {
                    let tail = &src_text[a..];
                    &tail[..tail.find("\n}").unwrap_or(tail.len())]
                }
                None => "",
            };
            for line in body.lines() {
                let Some((name, tail)) = line.trim().split_once(" = ") else { continue };
                if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.is_empty() {
                    continue;
                }
                let Some(num) = tail.trim_end_matches(',').trim().parse::<i64>().ok() else {
                    continue;
                };
                from_src.push((num, name.to_string()));
            }
        }

        // Markdown rows: `| `<number>` | `<name>` | …`
        let mut from_doc: Vec<(i64, String)> = Vec::new();
        for line in doc_text.lines() {
            let line = line.trim();

            // The debug-only syscalls are documented as prose bullets, not table rows:
            // ``- `sys_debug_kprint(…) -> isize` (`0xFFFF_0000`) — …``. Read those too,
            // or they are invisible to a table-only parser.
            if what == "syscall" && line.starts_with("- ") {
                let cells: Vec<&str> = line.split('`').skip(1).step_by(2).collect();
                let name = cells.iter().find_map(|c| {
                    c.strip_prefix("sys_")
                        .and_then(|r| r.split_once('('))
                        .map(|(n, _)| format!("sys_{n}"))
                });
                let num = cells.iter().find(|c| c.starts_with("0x")).and_then(|c| parse_num(c));
                if let (Some(name), Some(num)) = (name, num) {
                    from_doc.push((num, name));
                    continue;
                }
            }

            if !line.starts_with('|') {
                continue;
            }
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            let unquote = |c: &str| c.trim_matches('`').trim().to_string();
            if cells.len() < 3 {
                continue;
            }
            let (a, b) = (unquote(cells[1]), unquote(cells[2]));
            let Ok(num) = a.parse::<i64>() else { continue };
            if b.is_empty() || !b.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            from_doc.push((num, b));
        }

        from_src.sort();
        from_doc.sort();

        // A checker that parses nothing passes silently. That is a vacuous check, and this
        // project has shipped four vacuous tests; refuse to be the fifth.
        if from_src.is_empty() || from_doc.is_empty() {
            violations.push(format!(
                "{what} table: parsed {} entries from {src_rel} and {} from {doc_rel} — \
                 one side parsed empty, so the comparison proves nothing. The format \
                 changed; update cmd_check_docs.",
                from_src.len(),
                from_doc.len()
            ));
            continue;
        }

        for (n, name) in &from_src {
            match from_doc.iter().find(|(dn, _)| dn == n) {
                None => violations.push(format!(
                    "{doc_rel}: {what} {n} (`{name}`) is defined in {src_rel} but missing \
                     from the table"
                )),
                Some((_, dname)) if dname != name => violations.push(format!(
                    "{doc_rel}: {what} {n} is `{name}` in {src_rel} but `{dname}` in the table"
                )),
                Some(_) => {}
            }
        }
        for (n, name) in &from_doc {
            if !from_src.iter().any(|(sn, _)| sn == n) {
                violations.push(format!(
                    "{doc_rel}: {what} {n} (`{name}`) is in the table but not defined in \
                     {src_rel}"
                ));
            }
        }
        table_stats.push(format!("{} {what}s", from_src.len()));
    }

    if violations.is_empty() {
        println!(
            "check-docs: {links} link(s), {anchors} heading link(s), \
             {paths} cited source path(s), \
             {arch_docs} architecture doc(s) with a Status line, {} — all agree ✓",
            table_stats.join(" + ")
        );
        Ok(())
    } else {
        let mut msg = String::from(
            "documentation must not point at things that do not exist \
             (see cmd_check_docs in tools/xtask/src/main.rs for why each check exists):\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}

/// How `irq_dispatcher!` binds the guard [`enter_interrupt`] returns.
///
/// The distinction is the whole of rule 2. `enter_interrupt` opens the scope and its guard
/// closes it on drop, so *when the guard drops* decides whether the handler body runs inside
/// the scope or after it — and the three forms below are indistinguishable to a check that
/// only asks whether the text `enter_interrupt` appears.
#[derive(Debug, PartialEq, Eq)]
enum ScopeBinding {
    /// `let name = …enter_interrupt();` — lives to the end of the block. Correct.
    Held,
    /// `let _ = …enter_interrupt();` — dropped immediately. `#[must_use]` does not fire,
    /// because `let _ =` is the sanctioned way to silence it.
    Discarded,
    /// `…enter_interrupt();` — a temporary, dropped at the end of the statement.
    Temporary,
}

/// Classify the line in `irq_dispatcher!` that calls `enter_interrupt`.
fn scope_binding(line: &str) -> ScopeBinding {
    let t = line.trim();
    let Some(rest) = t.strip_prefix("let ") else {
        return ScopeBinding::Temporary;
    };
    let Some((binding, _)) = rest.split_once('=') else {
        return ScopeBinding::Temporary;
    };
    // `let mut g: Guard = …` → `g`.
    let binding = binding.trim().trim_start_matches("mut ").trim();
    let binding = binding.split(':').next().unwrap_or("").trim();
    if binding == "_" { ScopeBinding::Discarded } else { ScopeBinding::Held }
}

/// How many naked entry stubs `check-irq-scope` expects to find.
///
/// A count, because the check keys on a textual convention and a stub can leave its view
/// without leaving the kernel — by being renamed, or moved out of `kernel/src/arch`. The
/// emptiness guard below only fires at *zero*; this is what notices one going missing.
/// Today: 6 in `idt.rs` (exception, page-fault, timer, TLB shootdown, reschedule IPI,
/// device IRQ) + `syscall_entry`.
const EXPECTED_ENTRY_STUBS: usize = 7;

/// `<name> = sym TARGET` operands that are deliberately **not** interrupt dispatchers.
///
/// Keyed on the *target*, not the operand name: allowlisting by operand name would let a
/// rename buy the exemption this exists to deny.
const SYM_OPERAND_ALLOWLIST: &[&str] = &[
    // The thread trampoline: reached by a context switch, not by an interrupt, so the
    // rank order continues rather than restarting.
    "crate::sched::thread_enter",
];

/// `cargo xtask check-irq-scope` — every interrupt entry opens a lock-ordering scope.
///
/// The kernel's lock-rank tracker (`kernel/src/libkern/lockrank.rs`) models interrupt
/// context by having each handler start from an empty view of the held-rank stack, because
/// the acquisition order genuinely restarts at an interrupt boundary — a tick that lands on
/// a thread holding an allocator lock and takes `SCHED` is correct, and reads as an
/// inversion otherwise. That is not a refinement: it is what makes the tracker sound, and
/// getting it wrong is what got the first attempt withdrawn (decision log 2026-07-29).
///
/// A dispatcher that forgets the scope does not fail loudly. It just starts reporting
/// phantom inversions from its own vector, days later, in whoever's boot log. So the scope
/// is not left to the author of the next dispatcher:
///
/// 1. every `dispatch = sym NAME` in a naked entry stub must name a function defined by the
///    `irq_dispatcher!` macro, which opens the scope itself; and
/// 2. that macro must in fact still call `enter_interrupt` — otherwise rule 1 checks that
///    everyone went through a door that no longer leads anywhere.
///
/// Arch-generic: it walks `kernel/src/arch`, so an aarch64 entry path is covered the day it
/// exists rather than the day someone remembers this check.
///
/// ## What it cannot see (the ESCAPES boundary)
///
/// This is a textual scan, so its coverage is a convention rather than a proof. Three
/// escapes are closed by construction — a guard bound to `_` or left as a temporary
/// ([`ScopeBinding`]), a stub whose operand is renamed ([`SYM_OPERAND_ALLOWLIST`]), and a
/// stub that leaves the scan's view entirely ([`EXPECTED_ENTRY_STUBS`]). Two are known and
/// **not** closed, verified during the PR #202 review:
///
/// - A macro that puts the handler body *above* the `let` binding classifies as held and
///   passes. The classifier reads the binding form, not the ordering.
/// - The count notices a stub going missing, not a new one arriving in a form the scan does
///   not parse — a positional `sym` operand, say. Adding one is invisible to all three
///   guards.
///
/// Both need someone writing an entry stub in a deliberately unusual shape, which is a
/// different threat from the accident this gate exists to catch. Stated so the next reader
/// knows where the line is rather than assuming there isn't one.
fn cmd_check_irq_scope() -> R<()> {
    let arch_dir = repo_root().join("kernel").join("src").join("arch");
    // Dispatchers named by a naked stub, as (file:line, file, name).
    let mut called: Vec<(String, String, String)> = Vec::new();
    // Dispatchers the macro generated, as (file, name).
    //
    // **Per file, for the same reason as `user_entry` below.** Matched by name alone, a
    // second architecture reusing an obvious name — `timer_dispatch`, `exception_dispatch` —
    // would have its *unscoped* dispatcher exempted because x86_64 happens to generate one
    // with that name. Demonstrated in the PR #202 review with a probe file, which passed
    // green. The check advertises itself as arch-generic, so that is the live half of the
    // escape rather than a hypothetical.
    let mut generated: Vec<(String, String)> = Vec::new();
    // Dispatchers that are not interrupts and take the ring-3 entry discipline instead
    // (see `assert_user_entry_safe`): the order begins there rather than restarting.
    let mut user_entry: Vec<(String, String)> = Vec::new();
    // `<name> = sym …` operands that are not the `dispatch` this check keys on. The operand
    // name is a local choice in `naked_asm!`, so without this a stub leaves the check's view
    // by being renamed — see § What it cannot see on `cmd_check_irq_scope`.
    let mut stray_sym: Vec<(String, String)> = Vec::new();
    // Files that define the macro, and whether the definition still opens a scope.
    let mut macro_defs: Vec<(String, Option<ScopeBinding>)> = Vec::new();

    visit_rs_files(&arch_dir, &mut |path| {
        let text = fs::read_to_string(path)?;
        let lines: Vec<&str> = text.lines().collect();
        // `irq_dispatcher! {` … `fn NAME(` — the docs sit between the two.
        let mut pending_macro_body = false;
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();

            if let Some(rest) = line.split("dispatch = sym").nth(1) {
                let name: String = rest
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    called.push((
                        format!("{}:{}", path.display(), i + 1),
                        path.display().to_string(),
                        name,
                    ));
                }
            }

            // Any *other* `<ident> = sym` operand. Allowlisted by target, not by operand
            // name, so renaming an operand cannot buy an exemption.
            // Comment lines are skipped, as in the `assert_user_entry_safe` scan below and
            // for the same reason: a doc comment quoting an operand is prose, not code.
            // Whitespace is normalised first so `dispatch=sym` cannot slip past the
            // convention by omitting the spaces.
            let normalised = line.replace("=sym", "= sym").replace("= sym", "= sym");
            if !trimmed.starts_with("//")
                && !trimmed.starts_with("*")
                && let Some(before) = normalised.split("= sym").next()
                && normalised.contains("= sym")
                && !normalised.contains("dispatch = sym")
            {
                let operand: String = before
                    .trim_end()
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let target: String = normalised
                    .split("= sym")
                    .nth(1)
                    .unwrap_or("")
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
                    .collect();
                if !operand.is_empty()
                    && !target.is_empty()
                    && !SYM_OPERAND_ALLOWLIST.contains(&target.as_str())
                {
                    stray_sym.push((
                        format!("{}:{}", path.display(), i + 1),
                        format!("{operand} = sym {target}"),
                    ));
                }
            }

            if trimmed.starts_with("irq_dispatcher!") {
                pending_macro_body = true;
            } else if pending_macro_body && trimmed.starts_with("fn ") {
                let name: String = trimmed[3..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                generated.push((path.display().to_string(), name));
                pending_macro_body = false;
            }

            // A plain `fn NAME(` whose body asserts the ring-3 entry invariant. Scanned to
            // the next column-0 `}`, which ends a top-level function.
            if let Some(after) = trimmed.strip_prefix("fn ").or_else(|| {
                trimmed
                    .strip_prefix("unsafe extern \"C\" fn ")
                    .or_else(|| trimmed.strip_prefix("extern \"C\" fn "))
            }) {
                let name: String = after
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                let body_end = lines[i..]
                    .iter()
                    .position(|l| *l == "}")
                    .map(|o| i + o)
                    .unwrap_or(lines.len());
                // **Code only, and this file only.** Matched textually anywhere in the
                // function it would be satisfied by a doc comment that merely mentions the
                // assertion; pooled across files it would exempt a dispatcher because some
                // *other* file has a same-named function that asserts.
                if lines[i..body_end].iter().any(|l| {
                    let t = l.trim();
                    !t.starts_with("//") && !t.starts_with("*") && l.contains("assert_user_entry_safe")
                }) {
                    user_entry.push((path.display().to_string(), name));
                }
            }

            if trimmed.starts_with("macro_rules! irq_dispatcher") {
                // The expansion is short; the scope call is within a few lines.
                let end = (i + 12).min(lines.len());
                let scope_line = lines[i..end].iter().find(|l| l.contains("enter_interrupt"));
                macro_defs.push((
                    format!("{}:{}", path.display(), i + 1),
                    scope_line.map(|l| scope_binding(l)),
                ));
            }
        }
        Ok(())
    })?;

    let mut violations: Vec<String> = Vec::new();

    // A check that silently finds nothing to check is worse than no check — it reads as a
    // pass. (The `abi-sync-check` guard, for the same reason.)
    if called.is_empty() {
        violations.push(
            "found no `dispatch = sym …` entry stubs under kernel/src/arch — either the \
             entry path was restructured (update this check) or the scan is broken"
                .to_string(),
        );
    }
    if macro_defs.is_empty() {
        violations.push(
            "found no `macro_rules! irq_dispatcher` definition — the macro every dispatcher \
             is required to go through does not exist"
                .to_string(),
        );
    }
    for (site, binding) in &macro_defs {
        match binding {
            None => violations.push(format!(
                "{site}: `irq_dispatcher!` no longer calls `enter_interrupt` — dispatchers \
                 go through it precisely so they get a lock-ordering scope"
            )),
            Some(ScopeBinding::Discarded) => violations.push(format!(
                "{site}: `irq_dispatcher!` binds the interrupt scope to `_`, which drops it \
                 *before* the handler body runs — the body is then checked against the \
                 interrupted context, which is the flat-tracker behaviour that got the \
                 first tracker withdrawn (decision log 2026-07-29). Bind it to a name"
            )),
            Some(ScopeBinding::Temporary) => violations.push(format!(
                "{site}: `irq_dispatcher!` calls `enter_interrupt()` without binding the \
                 guard, so it is dropped at the end of that statement and the handler body \
                 runs unscoped. Bind it to a name"
            )),
            Some(ScopeBinding::Held) => {}
        }
    }
    if called.len() != EXPECTED_ENTRY_STUBS {
        violations.push(format!(
            "found {} entry stub(s), expected {EXPECTED_ENTRY_STUBS}. A stub that leaves \
             this check's view does so silently — the count is the only thing that notices. \
             If the entry path genuinely gained or lost one, update EXPECTED_ENTRY_STUBS \
             deliberately",
            called.len()
        ));
    }
    for (site, operand) in &stray_sym {
        violations.push(format!(
            "{site}: `{operand}` — an entry stub names its dispatcher `dispatch = sym`, and \
             this check keys on that literal. A differently-named operand is invisible to \
             it, so either rename the operand to `dispatch` or add the target to \
             SYM_OPERAND_ALLOWLIST with a reason"
        ));
    }
    for (site, file, name) in &called {
        if !generated.iter().any(|(f, g)| f == file && g == name)
            && !user_entry.iter().any(|(f, u)| f == file && u == name)
        {
            violations.push(format!(
                "{site}: entry stub dispatches to `{name}`, which is neither defined by \
                 `irq_dispatcher!` nor asserts `assert_user_entry_safe()` — it would run \
                 without a lock-ordering scope"
            ));
        }
    }

    if violations.is_empty() {
        println!(
            "check-irq-scope: {} entry stub(s) → {} scoped dispatcher(s) + {} ring-3 \
             entry point(s) ✓",
            called.len(),
            generated.len(),
            user_entry.len()
        );
        Ok(())
    } else {
        let mut msg = String::from(
            "every interrupt entry must open a lock-ordering scope — define the dispatcher \
             with the `irq_dispatcher!` macro (see kernel/src/libkern/lockrank.rs \
             § Interrupt scopes):\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}

fn cmd_check_arch() -> R<()> {
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

/// Walk every `*.md` under `dir`, calling `f` on each, pruning any directory whose name
/// is in `skip`. The markdown counterpart of [`visit_rs_files_skipping`].
/// `line` with each **closed** inline-code span replaced by a character that cannot appear in a
/// link, so a scan for `](` cannot find one inside backticks.
///
/// **Only closed pairs, and only within this line.** A code span may wrap across lines —
/// ``Priority::RealTime`` does, in `thread-args.md` — which leaves a line holding one unmatched
/// backtick and everything after it looking like code. Masking from an unmatched backtick to the
/// end of the line would swallow the real links that follow it (two of them, both found the
/// moment this was written the other way). An unmatched backtick therefore masks nothing, which
/// errs toward checking a link that was only being quoted rather than skipping one that was not.
fn mask_code_spans(line: &str) -> String {
    let mut out: Vec<char> = line.chars().collect();
    let mut open: Option<usize> = None;
    for i in 0..out.len() {
        if out[i] != '`' {
            continue;
        }
        match open {
            None => open = Some(i),
            Some(start) => {
                for c in out.iter_mut().take(i + 1).skip(start) {
                    *c = '\0';
                }
                open = None;
            }
        }
    }
    out.into_iter().collect()
}

/// Every anchor a markdown document offers: one per heading, plus explicit `<a id="…">`.
///
/// **GitHub's slug rules, because that is where these links are read**: lowercase, keep
/// alphanumerics, underscores and hyphens, turn spaces into hyphens, drop everything else — so
/// `## Transport — no bespoke protocol` is `transport--no-bespoke-protocol`, with *two* hyphens,
/// the em dash having left its two spaces behind. Repeats of one slug are suffixed `-1`, `-2`,
/// as `github-slugger` does. Headings inside fenced code are not headings.
fn heading_anchors(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        // An explicit anchor is the escape hatch for a target that is not a heading at all —
        // `syscall-abi.md` uses one to name a bullet in its type list.
        let mut rest = line;
        while let Some(at) = rest.find("<a ") {
            rest = &rest[at + 3..];
            let Some(open) = rest.find(['"']) else { break };
            let key = rest[..open].trim();
            let after = &rest[open + 1..];
            let Some(close) = after.find('"') else { break };
            if key.ends_with("id=") || key.ends_with("name=") {
                out.push(after[..close].to_string());
            }
            rest = &after[close..];
        }

        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !t.starts_with('#') {
            continue;
        }
        let title = t.trim_start_matches('#').trim();
        if title.is_empty() {
            continue;
        }
        let mut slug = String::new();
        for c in title.chars() {
            if c.is_alphanumeric() {
                slug.extend(c.to_lowercase());
            } else if c == ' ' || c == '-' {
                slug.push('-');
            } else if c == '_' {
                slug.push('_');
            }
        }
        let n = match counts.iter_mut().find(|(s, _)| *s == slug) {
            Some((_, n)) => {
                *n += 1;
                *n
            }
            None => {
                counts.push((slug.clone(), 0));
                0
            }
        };
        out.push(if n == 0 { slug } else { format!("{slug}-{n}") });
    }
    out
}

fn visit_md_files_skipping(
    dir: &Path,
    skip: &[&str],
    f: &mut dyn FnMut(&Path) -> R<()>,
) -> R<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if skip.contains(&name) {
                continue;
            }
            visit_md_files_skipping(&path, skip, f)?;
        } else if path.extension().map_or(false, |e| e == "md") {
            f(&path)?;
        }
    }
    Ok(())
}

/// Recursively visit every `.rs` file under `dir`, calling `f` on each. Like
/// [`visit_rs_files`], but pruning any directory whose name is in `skip` (e.g. build
/// output trees, which can be enormous and are not project source).
fn visit_rs_files_skipping(
    dir: &Path,
    skip: &[&str],
    f: &mut dyn FnMut(&Path) -> R<()>,
) -> R<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if skip.contains(&name) {
                continue;
            }
            visit_rs_files_skipping(&path, skip, f)?;
        } else if path.extension().map_or(false, |e| e == "rs") {
            f(&path)?;
        }
    }
    Ok(())
}

fn visit_rs_files(dir: &Path, f: &mut dyn FnMut(&Path) -> R<()>) -> R<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            visit_rs_files(&path, f)?;
        } else if path.extension().map_or(false, |e| e == "rs") {
            f(&path)?;
        }
    }
    Ok(())
}

/// Return the host's target triple by parsing `rustc -vV` output.
fn host_triple() -> R<String> {
    let out = Command::new("rustc").arg("-vV").output()?;
    if !out.status.success() {
        return Err(format!("rustc -vV exited {}", out.status).into());
    }
    let text = String::from_utf8(out.stdout)?;
    parse_host_from_rustc_vv(&text)
        .ok_or_else(|| "rustc -vV did not contain a `host:` line".into())
}

/// Find the `host:` line in `rustc -vV` output and return the triple.
fn parse_host_from_rustc_vv(s: &str) -> Option<String> {
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("host:") {
            let triple = rest.trim();
            if !triple.is_empty() {
                return Some(triple.to_string());
            }
        }
    }
    None
}

// --- Image assembly -----------------------------------------------------

fn find_bootx64(limine_root: &Path) -> R<PathBuf> {
    // The tarball layout has varied between versions; search a small set of
    // known locations rather than hard-coding one.
    let candidates = [
        limine_root.join("BOOTX64.EFI"),
        limine_root.join("limine-binary").join("BOOTX64.EFI"),
        limine_root.join("efi").join("x86_64").join("BOOTX64.EFI"),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    // Fall back to a recursive scan.
    if let Some(found) = walk_for(limine_root, "BOOTX64.EFI")? {
        return Ok(found);
    }
    Err(format!(
        "BOOTX64.EFI not found under {}; tarball layout may have changed",
        limine_root.display()
    )
    .into())
}

fn walk_for(root: &Path, name: &str) -> R<Option<PathBuf>> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let p = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            if let Some(found) = walk_for(&p, name)? {
                return Ok(Some(found));
            }
        } else if ft.is_file() && p.file_name().is_some_and(|n| n == name) {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// The initramfs payload. Slice 4 ships a placeholder `etc/init.toml` (a single
/// critical-path mount, processed once an fs-server exists in slice 5+);
/// spawnable images move into the initramfs with the spawn-ABI work (slice 7).
const INIT_TOML: &str = "\
# Nitrox init manifest (Phase 2 slice 4 placeholder).\n\
[[mount]]\n\
fs_server = \"fs-server-ext4\"\n\
device = \"gpt-partlabel:nitrox-root\"\n\
mount_point = \"/\"\n\
mode = \"rw\"\n\
required_for = \"boot\"\n";

/// The service declarations, read by `service-mgr` from `/initramfs/etc/services.toml`.
///
/// **One file, many `[service.<name>]` tables** — the 2026-08-21 change to
/// `docs/spec/service-toml-schema.md`. It previously said each file declares one service
/// and the manager scans the directory, and nothing in this system can enumerate a
/// directory of `.toml` files.
///
/// `executable` is a path per the schema, resolved through service-mgr's namespace:
/// `/bin/heartbeat` is projected from the content-addressed store by the profile server
/// (the real userspace path), not the initramfs `/sbin` staging.
const SERVICES_TOML: &str = "\
# Nitrox service declarations.\n\
[service.heartbeat]\n\
executable = \"/bin/heartbeat\"\n\
description = \"Demo supervised service (slice A)\"\n\
\n\
[service.heartbeat.restart]\n\
policy = \"always\"\n\
max_attempts = 3\n\
backoff = \"exponential\"\n\
backoff_initial = \"200ms\"\n\
backoff_max = \"2s\"\n";

/// The `boot-probe` declaration, **appended to [`SERVICES_TOML`] in selftest and
/// test-harness images and absent from a release image**.
///
/// This is the retrofit's governing decision 3 made concrete: the test image differs from
/// the release image *by data*. `init` and `service-mgr` are byte-identical in both; one
/// of them reads a file with an extra table in it. See
/// `docs/planning/test-path-retrofit.md`.
///
/// `policy = "never"` because the probe runs once and exits — a restart would re-run
/// checks that have already reported. `/bin/boot-probe` comes from the `test-harness`
/// store package, which is itself absent from a release image, so the declaration and the
/// executable appear and disappear together.
const BOOT_PROBE_TOML: &str = "\
\n\
# The graphical self-tests and demo clients. `init` spawned these under `selftest` until\n\
# retrofit Part C2; they are data now, so `init` is byte-identical in both images.\n\
#\n\
# **Order is file order.** `nxterm` must precede `ui-testclient` because windows stack in\n\
# creation order at the origin and the display gate compares the top-left, so the largest\n\
# window has to be at the bottom.\n\
[service.display-selftest]\n\
executable = \"/bin/display-selftest\"\n\
description = \"Framebuffer + compositor self-test (one-shot)\"\n\
\n\
[service.display-selftest.restart]\n\
policy = \"never\"\n\
\n\
[service.nxterm]\n\
executable = \"/bin/nxterm\"\n\
description = \"The GUI terminal\"\n\
after = [\"display-selftest\"]\n\
\n\
[service.nxterm.restart]\n\
policy = \"never\"\n\
\n\
[service.ui-testclient]\n\
executable = \"/bin/ui-testclient\"\n\
description = \"Toolkit + window-management test client\"\n\
\n\
[service.ui-testclient.restart]\n\
policy = \"never\"\n\
\n\
[service.input-testclient]\n\
executable = \"/bin/input-testclient\"\n\
description = \"Injected key + click test client\"\n\
\n\
[service.input-testclient.restart]\n\
policy = \"never\"\n\
\n\
[service.test-harness]\n\
executable = \"/bin/test-harness\"\n\
description = \"The Phase 1-3 demo chain (one-shot)\"\n\
# It creates a namespace and binds `/session/user` into it. `init` granted this directly\n\
# when it spawned the chain; a declaration has to say so.\n\
syscaps = [\"BIND_NAMESPACE\"]\n\
\n\
[service.test-harness.restart]\n\
policy = \"never\"\n\
\n\
# **`after` is the ordering the boot verdict rests on.** The gates below run immediately\n\
# before the only `SYS_TEST_EXIT(PASS)` call, so everything the run adjudicates has to have\n\
# happened first — `fp_gate` was moved out of the demo chain precisely because whoever owns\n\
# the verdict races it, and completed in 2 of 15 KVM runs there. `init::supervise` enforced\n\
# this by running the chain synchronously; now the declaration says it.\n\
[service.boot-probe]\n\
executable = \"/bin/boot-probe\"\n\
description = \"In-guest substrate checks and the boot verdict\"\n\
after = [\"test-harness\"]\n\
\n\
[service.boot-probe.restart]\n\
policy = \"never\"\n";

/// The `compose-bench` declaration, replacing [`BOOT_PROBE_TOML`] in a [`Bench`] image.
///
/// **`after` every display client in the image**, which is the point: `display-selftest`,
/// `nxterm` and the two test clients all draw at startup, and a frame timed while one of them is
/// compositing is noise attributed to whichever arm happened to be running. Ordering is the only
/// tool available — there is no "the screen is quiet now" signal — so the bench goes last and
/// ends the boot itself.
///
/// [`Bench`]: BuildMode::Bench
const BENCH_TOML: &str = "\
\n\
# The M13 Part A measurement. Declared **instead of** `boot-probe` (see `BOOT_PROBE_TOML`): it owns\n\
# the screen for the length of a run and fires the boot verdict itself.\n\
[service.compose-bench]\n\
executable = \"/bin/compose-bench\"\n\
description = \"What composing a drag costs, and where (M13 Part A)\"\n\
after = [\"test-harness\"]\n\
\n\
[service.compose-bench.restart]\n\
policy = \"never\"\n";

/// Build path for the packed initramfs CPIO archive.
fn initramfs_path() -> PathBuf {
    build_cache().join("initramfs.cpio")
}

/// The programs in the boot image, each with **the reason it cannot come from the filesystem**.
///
/// A pair rather than a bare name, because the reason is the whole rule and a list of names
/// does not carry it. This has now drifted twice — services accumulated here until
/// 2026-08-03, and the display arm's two servers went back in over the following week — both
/// times because adding a name to a list of names is a one-word change that looks like every
/// other one. Adding an entry here means writing down why the program is special, and if you
/// cannot, it belongs in the store like everything else.
///
/// The bar is "required to get from the bootloader to a mounted root, plus what cannot live
/// on the filesystem it depends on". Everything else is projected into `/bin` by the profile
/// server and spawned from there.
const INITRAMFS_PROGRAMS: &[(&str, &str)] = &[
    ("init", "the kernel boot-loads /sbin/init from here; nothing else is running yet"),
    ("fs-server-ext4", "it *is* the root mount, and is the only possible restart image for it"),
    ("eshell", "the recovery path *for a failed mount*, so it cannot live on that filesystem"),
    (
        "profile-server",
        "/bin does not exist until it runs. The alternative — teach init to read the manifest \
         and spawn it by store path — puts TOML parsing in the one process that must not fail",
    ),
];

/// The largest the packed initramfs may be, in bytes.
///
/// Not a memory limit: it is a **tripwire on the rule above**. Nothing releases the initramfs
/// (`sys_release_initramfs` is referenced in the docs and does not exist), so every byte here
/// is held for the machine's uptime — but the reason for the ceiling is that the list drifts
/// silently and a size is the one symptom that shows up without anyone looking. The four
/// programs come to ~231 KB; this leaves room for them to grow by half again before anyone
/// has to think about it, and fails immediately if a fifth is added.
///
/// A failure is a question, not a wall: either the new program has a bootstrap reason — in
/// which case add it to [`INITRAMFS_PROGRAMS`] with that reason and raise this — or it does
/// not, and it belongs in a store package.
const INITRAMFS_MAX_BYTES: usize = 384 * 1024;

/// Append one CPIO `newc` entry (header + NUL-terminated name + data, each region
/// NUL-padded to a 4-byte boundary) to `out`. Matches `kernel/src/initramfs.rs`.
fn cpio_entry(out: &mut Vec<u8>, ino: u32, name: &str, data: &[u8]) {
    let namesize = name.len() + 1; // includes the trailing NUL
    out.extend_from_slice(b"070701");
    // 13 eight-hex fields: ino, mode, uid, gid, nlink, mtime, filesize,
    // devmajor, devminor, rdevmajor, rdevminor, namesize, check.
    for f in [
        ino, 0o100644, 0, 0, 1, 0, data.len() as u32, 0, 0, 0, 0, namesize as u32, 0,
    ] {
        out.extend_from_slice(format!("{f:08x}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// The release path of a built userspace binary (bare target).
fn userspace_bin_path(name: &str) -> PathBuf {
    userspace_release_dir().join(name)
}

/// FNV-1a content hash of `bytes` as the store path's opaque `<hash>` (12 hex chars).
/// A non-cryptographic content hash — sufficient as a unique, deterministic directory
/// name for now; the Nix-style build-input hash arrives with the build system. See
/// `docs/architecture/content-addressed-store.md`.
fn store_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

/// A store path for a package containing several programs.
///
/// The hash covers **every** ELF in the package, concatenated in the given order, so
/// changing any one of them moves the whole package — which is the property that makes a
/// content-addressed path mean anything. Hashing only the first would let nine of the ten
/// coreutils change under a path that claims they did not.
fn store_path_for_all(bins: &[&str], name: &str, version: &str) -> R<String> {
    let mut bytes = Vec::new();
    for b in bins {
        bytes.extend_from_slice(
            &fs::read(userspace_bin_path(b))
                .map_err(|e| format!("read {b} ELF for store hash: {e}"))?,
        );
    }
    Ok(format!("/store/{}-{}-{}", store_hash(&bytes), name, version))
}

/// The programs a session gets through its profile: the coreutils, plus `nxsh`.
///
/// `nxsh` is here as well as being the login leaf — a user should be able to run a nested
/// shell, and one that cannot invoke itself is a strange thing to hand someone.
///
/// One list, three consumers (the build, the initramfs, the store package), so a new
/// coreutil cannot end up built-but-unreachable or packaged-but-unbuilt.
fn profile_programs() -> Vec<&'static str> {
    let mut v = COREUTILS.to_vec();
    v.push("nxsh");
    // The terminal is a program a person runs, not a service the system runs — the same class
    // as the shell beside it. Part C's `session-mgr` will spawn it from `/bin` like any other.
    v.push("nxterm");
    // The file browser, likewise a program a person runs. In `/bin`, so the applications modal
    // lists it without anything being told about it (M10 Part B).
    v.push("nxfiles");
    // The editor (M10 Part D). In `/bin` for two reasons rather than one: the modal lists it,
    // *and* `desktop-shell` resolves `/bin/nxedit` when a client asks it to open a path — so a
    // build that packaged the browser and not the editor would present a file row that opens
    // nothing, which is the failure this list exists to make impossible.
    v.push("nxedit");
    v
}

/// Pack the initramfs CPIO `newc` archive at `out`: the config manifests, the `init`
/// ELF (the kernel boot-loads `/sbin/init` from here — retiring the embedded copy),
/// and the mandatory `TRAILER!!!`. Built by `cmd_build` before this runs.
fn build_initramfs(out: &Path, mode: BuildMode) -> R<()> {
    let mut buf = Vec::new();
    cpio_entry(&mut buf, 1, "etc/init.toml", INIT_TOML.as_bytes());
    // The declarations file. Its **content** is what differs between a test image and a
    // release image — see `BOOT_PROBE_TOML`. The programs below do not differ.
    let mut services = String::from(SERVICES_TOML);
    if mode.features().is_some() {
        services.push_str(BOOT_PROBE_TOML);
    }
    // **`compose-bench` instead of `boot-probe`, not beside it.** `boot-probe` fires the boot
    // verdict, so anything declared after it never runs; and a measurement wants the screen to
    // itself, which is why this mode exists at all rather than the bench being one more service
    // in the harness image.
    if matches!(mode, BuildMode::Bench) {
        services = services.replace(BOOT_PROBE_TOML, BENCH_TOML);
    }
    cpio_entry(&mut buf, 2, "etc/services.toml", services.as_bytes());
    // Pack every program ELF at `sbin/<name>`: the kernel boot-loads `/sbin/init`, and
    // the spawners resolve their children by path (`/initramfs/sbin/<name>`), retiring
    // the kernel-embedded `ImageId` images. Built by `cmd_build` before this runs.
    //
    // **The list is the same in every build mode.** The initramfs a test boots is therefore
    // the initramfs a release boots, so the boot path under test is the boot path that ships.
    // Until 2026-08-11 a test image's was 680 KB against a release's 323 KB, and both carried
    // programs with no bootstrap role at all.
    let mut ino = 3u32;
    for (name, _why) in INITRAMFS_PROGRAMS {
        let elf = userspace_bin_path(name);
        let bytes =
            fs::read(&elf).map_err(|e| format!("read {name} ELF {}: {e}", elf.display()))?;
        cpio_entry(&mut buf, ino, &format!("sbin/{name}"), &bytes);
        ino += 1;
    }
    // The system profile manifest — the profile server reads it and projects the listed
    // packages' `bin/` into `/bin`. Generated (not a static const) because it references
    // the store path, whose hash is content-derived at build time (must match the ext4
    // store dir). See `docs/architecture/profiles-and-namespace-projection.md`.
    let sys_store = store_path_for_all(SYSTEM_SERVICES, "system", "0.1.0")?;
    let cu_store = store_path_for_all(&profile_programs(), "coreutils", "0.1.0")?;
    let mut system_profile = format!(
        "# System profile manifest (generation 1).\n\
         [profile]\n\
         name = \"system\"\n\
         generation = 1\n\
         \n\
         [[package]]\n\
         name = \"system\"\n\
         version = \"0.1.0\"\n\
         path = \"{sys_store}\"\n\
         \n\
         [[package]]\n\
         name = \"coreutils\"\n\
         version = \"0.1.0\"\n\
         path = \"{cu_store}\"\n"
    );
    // The test package, in selftest/test-harness builds only. Projected into `/bin` like any
    // other package, so `init` spawns `/bin/ui-testclient` by exactly the path it spawns
    // `/bin/logging-service` by — one mechanism, not a test-only one.
    if mode.features().is_some() {
        let test_store = store_path_for_all(TEST_PROGRAMS, "test", "0.1.0")?;
        system_profile.push_str(&format!(
            "\n[[package]]\n\
             name = \"test\"\n\
             version = \"0.1.0\"\n\
             path = \"{test_store}\"\n"
        ));
    }
    cpio_entry(&mut buf, ino, "etc/profiles/system.toml", system_profile.as_bytes());
    cpio_entry(&mut buf, 0, "TRAILER!!!", b"");
    // The tripwire. Checked before the write so a build that trips it does not leave an image
    // behind that boots and looks fine.
    if buf.len() > INITRAMFS_MAX_BYTES {
        return Err(format!(
            "the initramfs is {} bytes, over the {INITRAMFS_MAX_BYTES}-byte ceiling.\n  \
             It carries only what cannot come from the filesystem — see INITRAMFS_PROGRAMS. \
             If you added a program: does it have a bootstrap reason? If so, add it there \
             *with that reason* and raise the ceiling. If not, put it in a store package and \
             spawn it from /bin.",
            buf.len()
        )
        .into());
    }
    fs::write(out, &buf)?;
    println!(
        "xtask: built initramfs ({} of {INITRAMFS_MAX_BYTES} bytes) at {}",
        buf.len(),
        out.display()
    );
    Ok(())
}

fn assemble_image(
    bootx64: &Path,
    kernel: &Path,
    conf: &Path,
    initramfs: &Path,
    out: &Path,
    mode: BuildMode,
) -> R<()> {
    require_tool("sgdisk")?;
    require_tool("mformat")?;
    require_tool("mcopy")?;
    require_tool("mmd")?;
    require_tool("mke2fs")?;

    if out.exists() {
        fs::remove_file(out)?;
    }

    // 1. Allocate the raw disk.
    {
        let f = fs::File::create(out)?;
        f.set_len(IMAGE_SIZE_MIB * 1024 * 1024)?;
    }

    // 2. GPT: an EFI System Partition (FAT32, ESP_SIZE_MIB starting at 1 MiB) and
    //    the ext4 `nitrox-root` filesystem filling the rest. The slice-6 GPT driver
    //    enumerates every non-empty entry (no type-GUID filter) and binds
    //    `/dev/disk/by-partlabel/nitrox-root` at boot — so the second partition
    //    rides the existing boot disk; no separate QEMU drive is needed.
    run(Command::new("sgdisk")
        .arg("--clear")
        .arg("-n").arg(format!("1:2048:+{ESP_SIZE_MIB}M")) // ESP: LBA 2048 (1 MiB), bounded
        .arg("-t").arg("1:ef00")                            // EFI System
        .arg("-c").arg("1:NITROX_ESP")
        .arg("-n").arg("2:0:0")                             // nitrox-root: next aligned → end
        .arg("-t").arg("2:8300")                            // Linux filesystem
        .arg("-c").arg("2:nitrox-root")
        .arg(out))?;

    // Query each partition's on-disk extent (robust to GPT's end-of-disk reserve).
    let (esp_lba, esp_sectors) = partition_extent(out, 1)?;
    let (root_lba, root_sectors) = partition_extent(out, 2)?;

    // A scratch dir for the per-partition images + the ext4 staging tree.
    let work = out.with_extension("partbuild");
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir_all(&work)?;

    // 3. Build the ESP as a separate, exactly-partition-sized FAT32 image (so the
    //    FAT is bounded to the partition), then splice it in. mformat on a plain
    //    file formats the whole file; no `@@offset` games.
    let esp = work.join("esp.img");
    {
        let f = fs::File::create(&esp)?;
        f.set_len(esp_sectors * 512)?;
    }
    let espf = esp.display().to_string();
    run(Command::new("mformat").arg("-i").arg(&espf).arg("-F").arg("-v").arg("NITROX_ESP"))?;
    run(Command::new("mmd")
        .arg("-i").arg(&espf)
        .arg("::/EFI").arg("::/EFI/BOOT").arg("::/boot").arg("::/boot/limine"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(bootx64).arg("::/EFI/BOOT/BOOTX64.EFI"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(conf).arg("::/boot/limine/limine.conf"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(kernel).arg("::/boot/kernel"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(initramfs).arg("::/boot/initramfs"))?;
    splice_into(out, esp_lba * 512, &esp)?;

    // 4. Build the ext4 `nitrox-root` filesystem as a separate, partition-sized
    //    image populated at creation (`mke2fs -d`, no root/mount), then splice it
    //    in. The feature set matches the fs-server-ext4 reader's support (the
    //    Part-2 fixture uses the same flags). The staging tree holds the milestone
    //    file the Part-6 init loop reads.
    let staging = work.join("rootfs");
    fs::create_dir_all(staging.join("system"))?;
    // `/scratch` — the backing directory for the **second writable mount**. The kernel
    // calls a rename cross-filesystem when the two paths resolve to a different (server,
    // subtree base) pair, so binding this one server a second time with base `/scratch`
    // yields a destination that is genuinely cross-mount to `/system` while staying
    // writable. That combination did not exist before (2026-07-30): `/initramfs` is
    // cross-mount but read-only, so only the *detection* half of `move`'s fallback could
    // ever run. Empty here; init binds it under `selftest` and the harness populates it.
    fs::create_dir_all(staging.join("scratch"))?;
    fs::write(
        staging.join("system").join("current-generation"),
        b"nitrox-rootfs generation 1\n",
    )?;
    // `system/large.bin` — the slice-8 Part-5 large-file milestone fixture: a file
    // past the old 64 KiB eager cap, spanning several pages, with **position-
    // sensitive** content so init's verifier catches a mis-faulted page. Each byte
    // `i` is `((i >> 12) ^ i) as u8` (the page index in the high part XOR the low
    // offset byte). This generator MUST match init's `fill_byte` /
    // `LARGE_FILE_BYTES` (`userspace/init/src/main.rs`).
    //
    // Sized at 8 pages (was 64): each demand-fault round-trips through the
    // *stateless* fs-server fill (full path/extent re-resolve per page), which
    // costs ~325 ms/page under QEMU's emulated AHCI — 64 pages made boot a ~20 s
    // silent wait. 8 pages still proves multi-page demand-faulting; the per-page
    // cost (kernel read-ahead + an fs-server open-file cookie) is a Phase-3 item.
    // See docs/rationale/deferred-decisions.md.
    const LARGE_FILE_BYTES: usize = 32 * 1024; // 8 pages
    let mut large = vec![0u8; LARGE_FILE_BYTES];
    for (i, b) in large.iter_mut().enumerate() {
        *b = (((i >> 12) ^ i) & 0xFF) as u8;
    }
    fs::write(staging.join("system").join("large.bin"), &large)?;
    // `system/rwtest` — a one-block (4 KiB) writable fixture for the Model A overwrite
    // test (fs-server-rw Part C). Initial content is `byte[i] = i & 0xFF`; init's selftest
    // maps it `MAP_WRITE`, overwrites a marker, `sys_file_sync`s, then re-resolves + reads
    // to confirm the write reached disk.
    let mut rwtest = vec![0u8; 4096];
    for (i, b) in rwtest.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }
    fs::write(staging.join("system").join("rwtest"), &rwtest)?;
    // `/system/users` — the auth-service credential DB (passwd-style:
    // `name:salt_hex:iterations:verifier_hex:home`). Seeded here so NO plaintext or
    // verifier is committed to the source tree: the stored value is the one-way
    // PBKDF2 of the fixture password, computed with the *same* libcrypto the
    // on-target auth-service verifies with (no drift). The fixture credential is a
    // build input for the emulator demo user, not a secret; init's login selftest
    // (auth Part E) uses the same literals. See docs/architecture/session-and-auth.md.
    {
        use std::fmt::Write as _;
        let iters = libcrypto::password::DEFAULT_ITERATIONS;
        let verifier = libcrypto::password::derive(DEMO_PASSWORD.as_bytes(), &DEMO_SALT, iters);
        let mut users = String::new();
        users.push_str("# Nitrox user database (auth-service).\n");
        users.push_str("# name:salt_hex:iterations:verifier_hex:home\n");
        write!(users, "{DEMO_USER}:").unwrap();
        for b in &DEMO_SALT {
            write!(users, "{b:02x}").unwrap();
        }
        write!(users, ":{iters}:").unwrap();
        for b in &verifier {
            write!(users, "{b:02x}").unwrap();
        }
        writeln!(users, ":{DEMO_HOME}").unwrap();
        fs::write(staging.join("system").join("users"), users.as_bytes())?;
    }
    // The demo user's home directory — the writable session root a login constructs
    // (auth Part E). The user shell writes into it, and since M11 Part C it arrives holding
    // the session's theme.
    fs::create_dir_all(staging.join(DEMO_HOME.trim_start_matches('/')))?;
    // **The theme, shipped with every field written out** (M11 Part C). It could ship empty or
    // not at all — a missing file is the built-in theme, which is what the host tests pin — and
    // a file naming every value is what makes the thing *discoverable*: a person who wants to
    // change a colour opens it and sees which colours there are. It is written from
    // `Theme::light()` rather than typed out, so the file and the constants cannot drift.
    {
        let mut text = String::from("# The session's theme. Delete this file for the built-in one.\n");
        text.push_str("# Colours are \"#RRGGBB\"; font_px is a size in pixels per em.\n\n");
        // Written from `Theme::light()` so the file and the constants cannot drift — except for
        // the one field the gate reads back, which is deliberately not the default.
        let mut shipped = libdraw::theme::Theme::light();
        shipped.font_px = f32::from(THEME_FONT_PX);
        // **The theme names the wallpaper** (M12 Part F). It is the *file* that decides the
        // desktop has a picture behind it — the built-in theme names none, deliberately, because
        // a wallpaper is a file a person supplies and a default would make the desktop's ground
        // depend on whatever the build happened to stage.
        shipped.wallpaper = Some(
            libdraw::theme::ThemePath::parse(WALLPAPER_PATH)
                .ok_or("the staged wallpaper path does not fit a ThemePath")?,
        );
        text.push_str(&shipped.to_config());
        let path = staging.join(DEMO_HOME.trim_start_matches('/')).join("theme.toml");
        fs::write(&path, text.as_bytes()).map_err(|e| format!("stage {}: {e}", path.display()))?;
    }
    // The wallpaper itself. **The maintainer's photograph, committed as taken and cropped here**
    // — see `wallpaper_png` for why the crop is a build step rather than a pre-cropped file, and
    // `assets/wallpapers/README.md` for the picture's provenance.
    //
    // This block used to say "generated rather than checked in", which was the argument for the
    // gradient that came before: no unreviewable binary should be load-bearing for a gate. The
    // asset is *content* a person chose now, and the comment outlived it by one commit — a
    // reader who starts here would have got the superseded reasoning and never reached the new
    // one four thousand lines away (PR #273 review, worth fixing 2).
    {
        let path = staging.join(DEMO_HOME.trim_start_matches('/')).join("wallpaper.png");
        let bytes = wallpaper_png()?;
        let n = bytes.len();
        fs::write(&path, &bytes).map_err(|e| format!("stage {}: {e}", path.display()))?;
        println!(
            "xtask: seeded {WALLPAPER_PATH} ({WALLPAPER_W}x{WALLPAPER_H}, {n} bytes)"
        );
    }
    println!("xtask: seeded /system/users + {DEMO_HOME} (with a theme)");
    // The content-addressed store, pre-built read-only into the ext4 root. Each package
    // lives at /store/<hash>-<name>-<version>/bin/<prog> — a demand-paged file the profile
    // server projects into /bin. heartbeat is the first package. The store path (hash) is
    // derived from the ELF, matching the initramfs profile manifest. See
    // `docs/architecture/content-addressed-store.md`.
    // The `coreutils` package: the programs a shell can actually run. One package rather
    // than one per binary — they version and ship together, and ten manifest entries would
    // claim an independence they do not have.
    //
    // Until this existed, the coreutils lived *only* in the initramfs, so a session could
    // not reach them at all without being handed the whole boot image. Since 2026-08-03
    // the boot image no longer carries them at all.
    let programs = profile_programs();
    let cu_store = store_path_for_all(&programs, "coreutils", "0.1.0")?;
    let cu_bin = staging.join(cu_store.trim_start_matches('/')).join("bin");
    fs::create_dir_all(&cu_bin)?;
    for prog in &programs {
        fs::copy(userspace_bin_path(prog), cu_bin.join(prog))
            .map_err(|e| format!("stage {prog} into the store: {e}"))?;
    }
    println!(
        "xtask: store package {cu_store}/bin/ ({} programs)",
        programs.len()
    );

    // The `system` package: the services. Everything init and service-mgr spawn after the
    // root is mounted now resolves through `/bin` like any other program.
    let sys_store = store_path_for_all(SYSTEM_SERVICES, "system", "0.1.0")?;
    let sys_bin = staging.join(sys_store.trim_start_matches('/')).join("bin");
    fs::create_dir_all(&sys_bin)?;
    for prog in SYSTEM_SERVICES {
        fs::copy(userspace_bin_path(prog), sys_bin.join(prog))
            .map_err(|e| format!("stage {prog} into the store: {e}"))?;
    }
    println!(
        "xtask: store package {sys_store}/bin/ ({} services)",
        SYSTEM_SERVICES.len()
    );

    // The `test` package: the guest-side gates and the programs they drive. Absent from a
    // release image — `cmd_build` does not even build them outside selftest modes.
    if mode.features().is_some() {
        let test_store = store_path_for_all(TEST_PROGRAMS, "test", "0.1.0")?;
        let test_bin = staging.join(test_store.trim_start_matches('/')).join("bin");
        fs::create_dir_all(&test_bin)?;
        for prog in TEST_PROGRAMS {
            fs::copy(userspace_bin_path(prog), test_bin.join(prog))
                .map_err(|e| format!("stage {prog} into the store: {e}"))?;
        }
        println!(
            "xtask: store package {test_store}/bin/ ({} test programs)",
            TEST_PROGRAMS.len()
        );
    }

    // `/system/fonts` — the two faces the desktop draws with, and their licence beside them.
    //
    // **Here and not in the initramfs**, which the plan settled before the code was written:
    // nothing that draws text runs before the root is mounted, and at 343 KiB the smaller file
    // is larger than every program in the boot image put together. A client resolves the path
    // its theme names and demand-pages the file in.
    //
    // **Which files, from the theme rather than from a list here** (M11 Part D). `Theme::light()`
    // names a proportional face for the desktop and a fixed-advance one for the grid; staging
    // exactly those is what makes "the guest reads the font the host rendered with" a property
    // of the build instead of two lists somebody keeps equal. `font_asset` is the same mapping
    // the previews and the display gate use.
    //
    // The licence ships with them because the fonts are redistributed: DejaVu's terms are
    // permissive but require the notice to travel with the files. One notice covers both — its
    // `Files: *` stanza is the DejaVu family, which is also why the second face cost no new
    // licence question.
    {
        let fonts = staging.join("system").join("fonts");
        fs::create_dir_all(&fonts)?;
        let theme = libdraw::theme::Theme::light();
        let mut faces: Vec<PathBuf> =
            vec![font_asset(theme.font_ui.as_str())?, font_asset(theme.font_mono.as_str())?];
        faces.dedup();
        faces.push(repo_root().join("assets/fonts").join("LICENSE-DejaVu.txt"));
        let mut total = 0u64;
        for from in &faces {
            let name = from.file_name().ok_or("a font asset with no file name")?;
            total += fs::copy(from, fonts.join(name))
                .map_err(|e| format!("stage {}: {e}", from.display()))?;
        }
        println!("xtask: seeded /system/fonts ({} files, {total} bytes)", faces.len());
    }

    let rootfs = work.join("rootfs.ext4");
    let blocks = (root_sectors * 512) / 4096; // 4 KiB block count
    run(Command::new("mke2fs")
        .arg("-q").arg("-F").arg("-t").arg("ext4")
        .arg("-O").arg("^has_journal,^64bit,^metadata_csum,^resize_inode")
        .arg("-b").arg("4096")
        .arg("-d").arg(&staging)
        .arg(&rootfs)
        .arg(blocks.to_string()))?;
    splice_into(out, root_lba * 512, &rootfs)?;

    // Leave `work/` in place for inspection; `cmd_image` rebuilds it each run.
    Ok(())
}

/// Parse `sgdisk -i <n> <disk>` for partition `n`'s first LBA and sector count.
fn partition_extent(disk: &Path, n: u32) -> R<(u64, u64)> {
    let out = Command::new("sgdisk")
        .arg("-i").arg(n.to_string()).arg(disk)
        .output()
        .map_err(|e| format!("failed to run sgdisk -i {n}: {e}"))?;
    if !out.status.success() {
        return Err(format!("sgdisk -i {n} {} failed", disk.display()).into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut first = None;
    let mut last = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("First sector:") {
            first = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok());
        } else if let Some(rest) = line.strip_prefix("Last sector:") {
            last = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok());
        }
    }
    let first = first.ok_or("sgdisk: missing 'First sector'")?;
    let last = last.ok_or("sgdisk: missing 'Last sector'")?;
    Ok((first, last - first + 1))
}

/// Overwrite `dst` (in place, no truncation) with `src`'s bytes starting at byte
/// `offset` — splice a partition image into the GPT disk.
fn splice_into(dst: &Path, offset: u64, src: &Path) -> R<()> {
    use std::io::{Seek, SeekFrom, Write};
    let data = fs::read(src)?;
    let mut f = fs::OpenOptions::new().write(true).open(dst)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&data)?;
    Ok(())
}

/// UEFI firmware for the QEMU pflash. Modern QEMU ships **split** firmware — a
/// read-only CODE image plus a writable VARS (NVRAM) store — and a CODE-only
/// pflash will not boot (the firmware needs its variable region). Older setups
/// shipped a single combined image used as one read-only pflash.
enum Firmware {
    /// Legacy single combined image (e.g. `OVMF.fd`): one read-only pflash.
    Single(PathBuf),
    /// Split firmware: a read-only CODE image plus a VARS *template* that is
    /// copied to a writable per-run store before boot.
    Split { code: PathBuf, vars_template: PathBuf },
}

/// Locate UEFI firmware, preferring the modern split (CODE+VARS) layout that
/// QEMU bundles under its data dir. `NITROX_OVMF` overrides the CODE/combined
/// image; pair it with `NITROX_OVMF_VARS` to force the split layout.
fn locate_ovmf() -> R<Firmware> {
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
        (
            "/usr/local/share/qemu/edk2-x86_64-code.fd",
            "/usr/local/share/qemu/edk2-i386-vars.fd",
        ),
        (
            "/usr/share/qemu/edk2-x86_64-code.fd",
            "/usr/share/qemu/edk2-i386-vars.fd",
        ),
        // Debian/Ubuntu's `ovmf` package has shipped the 4 MB build under these names
        // since 22.04, and on current releases they are the *only* ones present — the
        // unsuffixed pair below is older layouts (and the `/usr/share/ovmf/OVMF.fd`
        // single image at the end is older still). Ordered newest-first so a machine
        // with both prefers the split pair, which gives a writable VARS store.
        (
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/OVMF/OVMF_VARS_4M.fd",
        ),
        (
            "/usr/share/OVMF/OVMF_CODE.fd",
            "/usr/share/OVMF/OVMF_VARS.fd",
        ),
        (
            "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
            "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd",
        ),
    ];
    for (code, vars) in split {
        if Path::new(code).exists() && Path::new(vars).exists() {
            return Ok(Firmware::Split {
                code: PathBuf::from(code),
                vars_template: PathBuf::from(vars),
            });
        }
    }
    // Legacy single combined image.
    let single = [
        "/usr/share/ovmf/OVMF.fd",
        "/usr/share/qemu/OVMF.fd",
        "/usr/share/edk2-ovmf/x64/OVMF.fd",
    ];
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
        Firmware::Single(code) => Ok(vec![
            "-drive".into(),
            format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
        ]),
        Firmware::Split { code, vars_template } => {
            let vars = build_cache().join("ovmf-vars.fd");
            fs::copy(vars_template, &vars).map_err(|e| {
                format!(
                    "copy OVMF vars {} -> {}: {e}",
                    vars_template.display(),
                    vars.display()
                )
            })?;
            Ok(vec![
                "-drive".into(),
                format!("if=pflash,unit=0,format=raw,readonly=on,file={}", code.display()),
                "-drive".into(),
                format!("if=pflash,unit=1,format=raw,file={}", vars.display()),
            ])
        }
    }
}

// --- Helpers ------------------------------------------------------------

fn require_tool(name: &str) -> R<()> {
    let status = Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(format!(
            "required host tool `{name}` is missing — install it and retry"
        )
        .into()),
    }
}

fn run(cmd: &mut Command) -> R<()> {
    let pretty = format_cmd(cmd);
    let status = cmd.status().map_err(|e| format!("failed to spawn {pretty}: {e}"))?;
    if !status.success() {
        return Err(format!("command failed ({status}): {pretty}").into());
    }
    Ok(())
}

fn format_cmd(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        s.push(' ');
        s.push_str(&arg.to_string_lossy());
    }
    s
}

// --- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A preview is the same picture the display gate demands of the guest.
    ///
    /// **What this pins, and what it deliberately no longer claims to.** *Sharing* is structural
    /// now: `cmd_check_display` reads its expected frames from `preview_frames`, so a preview
    /// that is not the gate's picture fails against a real guest rather than passing here. This
    /// test said the size assertion would catch "a different size **or theme**" and it would not —
    /// a solid rectangle at 320×300 passed it (PR #261 review, finding 2).
    ///
    /// What it does pin is worth pinning. The sizes are the gate's own constants and
    /// `libterm`'s own `size()`, which is the one place those agree with the frames the gate
    /// measures. And the PNG is **decoded back to the framebuffer's pixels**, which covers the
    /// conversion: the toolkit reference's pitch is 1292 for a 1280-byte row and `XRGB8888` is
    /// little-endian, so a direct copy is wrong twice over — a stride and a channel order.
    ///
    /// Decoded rather than compared against what went in, because a round trip through one
    /// library's encoder and back tests the encoder; what is under test here is the pixels.
    #[test]
    fn a_preview_is_the_picture_the_display_gate_demands() {
        use libdraw::framebuffer::Framebuffer;
        let faces = host_faces().expect("the vendored fonts");
        let frames = preview_frames(&faces);

        let sizes: Vec<(&str, u32, u32)> = frames
            .iter()
            .map(|(n, f)| (*n, f.geometry().width, f.geometry().height))
            .collect();
        let term = libterm::render::reference::size(&faces.1);
        assert_eq!(
            sizes,
            [
                ("ui", libui::reference::WIDTH, libui::reference::HEIGHT),
                ("term", term.w, term.h),
            ],
            "the previews are the gate's arrangements, at the gate's sizes"
        );

        for (name, frame) in &frames {
            let (w, h, rgb) = rgb_of(frame);
            let png = encode_png(w, h, &rgb).expect("encode");
            let decoder = png::Decoder::new(std::io::Cursor::new(png));
            let mut reader = decoder.read_info().expect("a readable PNG");
            let info = reader.info().clone();
            assert_eq!((info.width, info.height), (w, h), "{name}: size on the way out");
            assert_eq!(info.color_type, png::ColorType::Rgb, "{name}: eight-bit RGB");

            let mut buf = vec![0u8; reader.output_buffer_size().expect("a bounded image")];
            let out = reader.next_frame(&mut buf).expect("one frame");
            let decoded = &buf[..out.buffer_size()];

            for y in 0..h {
                for x in 0..w {
                    let want = Framebuffer::get_pixel(frame, x, y).unwrap_or_default();
                    let i = ((y as usize) * (w as usize) + x as usize) * 3;
                    assert_eq!(
                        (decoded[i], decoded[i + 1], decoded[i + 2]),
                        (want.r, want.g, want.b),
                        "{name}: pixel {x},{y} came back different"
                    );
                }
            }
        }
    }

    /// The line scanner both directions share.
    ///
    /// It exists because they did not share one: the code side stopped at the first marker
    /// per line, so two markers on one line meant the second was reported missing while the
    /// error pointed at the line carrying it. The bracketed-placeholder case is not
    /// hypothetical either — this file's own prose contains one, and the first-marker-only
    /// behaviour was hiding a *bare* marker written after it on the same line.
    #[test]
    fn markers_in_line_finds_every_plain_word_marker_and_no_others() {
        use super::markers_in_line;

        // Assembled, for the reason the neighbouring test gives.
        fn m(name: &str) -> String {
            format!("TODO{}{}{}", '(', name, ')')
        }

        assert_eq!(markers_in_line("nothing here"), Vec::<&str>::new());
        assert_eq!(markers_in_line(&format!("// {}", m("alpha"))), ["alpha"]);

        // Two on one line — the case that produced a false failure.
        assert_eq!(
            markers_in_line(&format!("// {} and {}: see the doc.", m("alpha"), m("beta"))),
            ["alpha", "beta"]
        );

        // A bracketed placeholder is skipped and does not stop the scan: a real marker
        // after it on the same line is still found.
        assert_eq!(
            markers_in_line(&format!("// spelt {} not {}", m("<tag>"), m("tag"))),
            ["tag"]
        );

        // Not markers.
        assert_eq!(markers_in_line(&format!("// {}", m("has space"))), Vec::<&str>::new());
        assert_eq!(markers_in_line(&format!("// {}", m(""))), Vec::<&str>::new());
        assert_eq!(markers_in_line("// TODO(unclosed"), Vec::<&str>::new());
    }

    /// The doc-to-code direction's extractor, on the shapes that decide what it enforces.
    ///
    /// The Resolved cases are the reason this is a table test and not a live-document check:
    /// a first pass at this measurement scraped the whole file and counted a tag named inside
    /// a Resolved row — narrating the *deletion* of its own markers — as an unbacked entry.
    /// Acting on that would have reinstated the stale marker that closing it removed.
    ///
    /// The exemption case has no user in the document today, so this is the only thing
    /// exercising it. An escape hatch nobody has opened is exactly the sort of thing that
    /// turns out not to work the first time someone needs it.
    ///
    /// **The fixture assembles its markers rather than spelling them.** `check-deferrals`
    /// scans `tools/xtask/src`, so a literal `TODO` + `(name)` in this file *is* a marker as
    /// far as the gate is concerned: writing the fixture out longhand made the gate fail on
    /// its own test data, reporting seven deferrals that do not exist. Same trap as the
    /// `TODO(<tag>)` placeholder in this module's prose, one level further in.
    #[test]
    fn open_section_tags_counts_open_entries_and_nothing_else() {
        use super::open_section_tags;

        // Assembled at runtime; see the note above.
        fn m(name: &str) -> String {
            format!("TODO{}{}{}", '(', name, ')')
        }

        let doc = format!(
            "# Deferred\n\
             **Thing one — `{alpha}`.** Words.\n\
             **Thing two — `{beta}`.** More words.\n\
             **Thing three — `{gamma}`.** No code site yet. {ncs}\n\
             **Two on one line — `{delta}` and `{epsilon}`.**\n\
             **Repeat — `{alpha}` again.**\n\
             **Cross-reference — also the trigger for `{theta}`.**\n\
             **The real one — `{theta}`.** No code site. {ncs}\n\
             **Not a tag — `{spaced}` and `{empty}`.**\n\
             \n## Resolved (kept for the record)\n\
             | Thing four (`{zeta}`) | closing it deleted its markers |\n\
             \n## How to use this document\n\
             Every `{eta}` must appear here.\n",
            alpha = m("alpha"),
            beta = m("beta"),
            gamma = m("gamma"),
            delta = m("delta"),
            epsilon = m("epsilon"),
            theta = m("theta"),
            spaced = m("has space"),
            empty = m(""),
            zeta = m("zeta"),
            eta = m("eta"),
            ncs = NO_CODE_SITE,
        );

        let got = open_section_tags(&doc);
        let names: Vec<&str> = got.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "gamma", "delta", "epsilon", "theta"]);

        // Only the marked line is exempt.
        let exempt: Vec<&str> =
            got.iter().filter(|(_, e)| *e).map(|(t, _)| t.as_str()).collect();
        // `theta` is named on an earlier line as a cross-reference and exempted on its own
        // line below — the geometry `tty-server` -> `session-metadata-server` already has in
        // the real document. First-occurrence-wins drops the exemption and fails the gate
        // while telling you to write the marker you just wrote.
        assert_eq!(
            exempt,
            ["gamma", "theta"],
            "the no-code-site marker must be honoured wherever in the entry it appears"
        );

        // The three that must NOT appear, and the reason each is excluded.
        assert!(!names.contains(&"zeta"), "a Resolved row's tag is not an open entry");
        assert!(!names.contains(&"eta"), "the closing section's prose is not an entry");
        assert!(!names.contains(&"has space"), "a tag must be a plain word");

        // A document with no Resolved heading is open all the way down.
        let all_open = open_section_tags(&format!("**One — `{}`.**\n", m("solo")));
        assert_eq!(all_open, vec![(String::from("solo"), false)]);
    }

    /// The echo guard's predicate, on the shapes it has to tell apart.
    ///
    /// The flagship pair is the last one: the exact command `test-interactive` step 14 sends,
    /// against the pattern that shipped for months (`boom` — satisfied by the echo, so the
    /// step passed with `try`/`catch` deleted) and the one that replaced it (`caught=boom` —
    /// produced only by evaluating the catch block). A predicate that cannot separate those
    /// two is decoration.
    #[test]
    fn echo_source_finds_a_pattern_the_guest_would_echo_back() {
        use super::echo_source;

        let none: [String; 0] = [];
        // Gates that type over QMP send nothing, so the guard must stay inert for them.
        assert_eq!(echo_source(&none, "anything"), None);

        let one = [String::from("format(\"add={}\", sum)")];
        assert_eq!(echo_source(&one, "add=5"), None, "the answer is not in the command");
        assert_eq!(echo_source(&one, "sum"), Some("format(\"add={}\", sum)"));

        // An empty pattern is contained in every string; it must not trip the guard.
        assert_eq!(echo_source(&one, ""), None);

        // **Two sends before one expect.** Both echoes are still ahead of the cursor, so the
        // earlier one has to be caught too — a predicate holding only the latest send would
        // return None here and wave the assertion through.
        let two = [String::from("echo hello"), String::from("whoami")];
        assert_eq!(echo_source(&two, "hello"), Some("echo hello"));
        assert_eq!(echo_source(&two, "whoami"), Some("whoami"));
        assert_eq!(echo_source(&two, "alice"), None, "the answer is in neither command");

        // The regression, both directions.
        let step14 = [String::from(
            "try { fail \"boom\" } catch (e) { format(\"caught={}\", e.message) }",
        )];
        assert!(echo_source(&step14, "boom").is_some(), "the old pattern must be refused");
        assert_eq!(echo_source(&step14, "caught=boom"), None, "the new one must be allowed");
    }

    /// The rule-2 classifier, on every form it has to tell apart.
    ///
    /// Table-tested because it *is* the fix: before this, the only thing proving the
    /// classifier worked was a manual mutation of `idt.rs`, which is exactly the situation
    /// the gate argues against. `Discarded` is the case that shipped green for months —
    /// `let _ =` keeps `#[must_use]` quiet and closes the scope before the handler body.
    #[test]
    fn scope_binding_tells_a_held_guard_from_one_dropped_where_it_is_made() {
        use super::{ScopeBinding, scope_binding};

        // Held: any real binding, however it is spelt.
        for line in [
            "            let _lock_scope = crate::libkern::lockrank::enter_interrupt();",
            "let g = enter_interrupt();",
            "let mut g = enter_interrupt();",
            "let g: IrqScope = enter_interrupt();",
            "let mut g: IrqScope = enter_interrupt();",
        ] {
            assert_eq!(scope_binding(line), ScopeBinding::Held, "{line}");
        }

        // Discarded: the underscore *pattern*, not a name that begins with one.
        assert_eq!(scope_binding("let _ = enter_interrupt();"), ScopeBinding::Discarded);
        assert_eq!(scope_binding("  let _   =   enter_interrupt();"), ScopeBinding::Discarded);
        assert_eq!(scope_binding("let _x = enter_interrupt();"), ScopeBinding::Held);

        // Temporary: no binding at all, so it drops at the end of the statement.
        assert_eq!(scope_binding("enter_interrupt();"), ScopeBinding::Temporary);
        assert_eq!(scope_binding("        enter_interrupt();"), ScopeBinding::Temporary);
        // A comment mentioning it is not a call site either.
        assert_eq!(scope_binding("/// calls enter_interrupt()"), ScopeBinding::Temporary);
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Per-test unique tmp dir. We avoid `tempfile` to keep xtask
    /// dependency-free, so we have to clean up manually.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "nitrox-xtask-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            if path.exists() {
                fs::remove_dir_all(&path).expect("clear stale tmp");
            }
            fs::create_dir_all(&path).expect("create tmp");
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir -p");
        }
        fs::write(p, b"").expect("touch");
    }

    #[test]
    fn walk_for_finds_nested_file() {
        let tmp = TmpDir::new("walk-nested");
        touch(&tmp.path().join("a/b/c/target.bin"));
        let found = walk_for(tmp.path(), "target.bin").unwrap();
        let found = found.expect("walk_for should locate target.bin");
        assert_eq!(found.file_name().unwrap(), "target.bin");
    }

    #[test]
    fn walk_for_returns_none_when_missing() {
        let tmp = TmpDir::new("walk-missing");
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        assert!(walk_for(tmp.path(), "nope.efi").unwrap().is_none());
    }

    #[test]
    fn find_bootx64_uses_known_location() {
        let tmp = TmpDir::new("bootx64-known");
        // Limine v12 layout: efi/x86_64/BOOTX64.EFI
        let expected = tmp.path().join("efi/x86_64/BOOTX64.EFI");
        touch(&expected);
        // Decoy that should be ignored because the known location wins.
        touch(&tmp.path().join("somewhere/else/BOOTX64.EFI"));
        let found = find_bootx64(tmp.path()).unwrap();
        assert_eq!(found, expected);
    }

    #[test]
    fn find_bootx64_falls_back_to_recursive_scan() {
        let tmp = TmpDir::new("bootx64-fallback");
        let weird = tmp.path().join("unexpected/depth/BOOTX64.EFI");
        touch(&weird);
        let found = find_bootx64(tmp.path()).unwrap();
        assert!(found.ends_with("BOOTX64.EFI"));
    }

    #[test]
    fn find_bootx64_errors_when_absent() {
        let tmp = TmpDir::new("bootx64-absent");
        fs::create_dir_all(tmp.path().join("efi")).unwrap();
        assert!(find_bootx64(tmp.path()).is_err());
    }

    #[test]
    fn format_cmd_includes_program_and_args() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello").arg("world");
        assert_eq!(format_cmd(&cmd), "echo hello world");
    }

    #[test]
    fn format_cmd_handles_no_args() {
        let cmd = Command::new("true");
        assert_eq!(format_cmd(&cmd), "true");
    }

    #[test]
    fn parse_host_extracts_linux_triple() {
        let sample = "\
rustc 1.95.0 (59807616e 2026-04-14)
binary: rustc
commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860
commit-date: 2026-04-14
host: x86_64-unknown-linux-gnu
release: 1.95.0
LLVM version: 22.1.2
";
        assert_eq!(
            parse_host_from_rustc_vv(sample).as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
    }

    #[test]
    fn parse_host_extracts_macos_triple() {
        let sample = "rustc 1.95.0\nhost: aarch64-apple-darwin\n";
        assert_eq!(
            parse_host_from_rustc_vv(sample).as_deref(),
            Some("aarch64-apple-darwin")
        );
    }

    #[test]
    fn parse_host_returns_none_when_absent() {
        let sample = "rustc 1.95.0\nrelease: 1.95.0\n";
        assert!(parse_host_from_rustc_vv(sample).is_none());
    }
}

#[cfg(test)]
mod diag_tests {
    use super::*;

    #[test]
    fn a_qmp_return_string_is_unescaped() {
        assert_eq!(unescape_json_return(r#"{"return": "a\nb"}"#), "a\nb");
        assert_eq!(unescape_json_return(r#"{"return": "he said \"hi\""}"#), "he said \"hi\"");
        assert_eq!(unescape_json_return(r#"{"return": "C:\\x"}"#), "C:\\x");
        assert_eq!(unescape_json_return(r#"{"return": ""}"#), "", "an empty monitor reply");
    }

    #[test]
    fn a_reply_without_a_return_field_yields_nothing_rather_than_garbage() {
        assert_eq!(unescape_json_return(r#"{"error": {"class": "GenericError"}}"#), "");
        assert_eq!(unescape_json_return("not json at all"), "");
    }

    #[test]
    fn demangling_recovers_a_rust_path() {
        assert_eq!(
            demangle("_ZN13nitrox_kernel5sched9idle_body17h08d8be4e35d6ac8aE"),
            "nitrox_kernel::sched::idle_body"
        );
    }

    #[test]
    fn demangling_drops_the_llvm_suffix_and_the_hash() {
        assert_eq!(
            demangle("_ZN13nitrox_kernel7drivers4ahci12issue_locked17h0ddde3632e042bdcE.llvm.29"),
            "nitrox_kernel::drivers::ahci::issue_locked"
        );
    }

    #[test]
    fn an_unmangled_or_v0_name_comes_back_unchanged() {
        // The failure this guards: a future rustc switches the kernel to v0 mangling and
        // `demangle` silently passes everything through, leaving the next hang dump full of
        // `_RNvNt…`. Passing through is the right behaviour — printing *something* beats
        // printing nothing — but it must be a decision, not an accident.
        assert_eq!(demangle("memcpy"), "memcpy");
        assert_eq!(demangle("_RNvNtCs1234_4core3fmt5write"), "_RNvNtCs1234_4core3fmt5write");
        assert_eq!(demangle("_ZN"), "_ZN", "truncated: not the shape we expect");
        assert_eq!(demangle("_ZNxyz"), "_ZNxyz", "no length prefix");
    }

    #[test]
    fn a_symbol_is_resolved_only_inside_its_extent() {
        let syms = vec![
            ElfSym { addr: 0x1000, size: 0x40, name: "a".into() },
            ElfSym { addr: 0x2000, size: 0, name: "b".into() },
        ];
        assert_eq!(resolve_symbol(&syms, 0x1000).as_deref(), Some("a+0x0"));
        assert_eq!(resolve_symbol(&syms, 0x1020).as_deref(), Some("a+0x20"));
        // Past `a`'s extent but before `b`: unknown, rather than attributed to `a`. A dump
        // that names the wrong function is worse than one that admits it does not know.
        assert_eq!(resolve_symbol(&syms, 0x1040), None);
        assert_eq!(resolve_symbol(&syms, 0x0), None, "below everything");
        // A zero-size symbol still names its entry point.
        assert_eq!(resolve_symbol(&syms, 0x2000).as_deref(), Some("b+0x0"));
        assert_eq!(resolve_symbol(&syms, 0x9999).as_deref(), Some("b+0x7999"));
    }

    /// A minimal ELF64 with one section header table, one `SHT_SYMTAB` and its strtab,
    /// carrying two `STT_FUNC` symbols and one non-func that must be ignored.
    fn tiny_elf() -> Vec<u8> {
        const SHOFF: usize = 0x100;
        const SHENT: usize = 64;
        let strtab: &[u8] = b"\0alpha\0beta\0notafunc\0";
        let stroff = 0x300usize;
        let symoff = 0x200usize;
        let mut b = vec![0u8; 0x400];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // ELF64
        b[0x28..0x30].copy_from_slice(&(SHOFF as u64).to_le_bytes());
        b[0x3A..0x3C].copy_from_slice(&(SHENT as u16).to_le_bytes());
        b[0x3C..0x3E].copy_from_slice(&2u16.to_le_bytes()); // two section headers

        // Section 0: the symtab. Section 1: its string table.
        let sh = SHOFF;
        b[sh + 4..sh + 8].copy_from_slice(&2u32.to_le_bytes()); // SHT_SYMTAB
        b[sh + 0x18..sh + 0x20].copy_from_slice(&(symoff as u64).to_le_bytes());
        b[sh + 0x20..sh + 0x28].copy_from_slice(&(24u64 * 3).to_le_bytes());
        b[sh + 0x28..sh + 0x2C].copy_from_slice(&1u32.to_le_bytes()); // sh_link -> strtab
        let sh1 = SHOFF + SHENT;
        b[sh1 + 4..sh1 + 8].copy_from_slice(&3u32.to_le_bytes()); // SHT_STRTAB
        b[sh1 + 0x18..sh1 + 0x20].copy_from_slice(&(stroff as u64).to_le_bytes());

        b[stroff..stroff + strtab.len()].copy_from_slice(strtab);

        let mut put_sym = |i: usize, nameoff: u32, info: u8, addr: u64, size: u64| {
            let e = symoff + i * 24;
            b[e..e + 4].copy_from_slice(&nameoff.to_le_bytes());
            b[e + 4] = info;
            b[e + 8..e + 16].copy_from_slice(&addr.to_le_bytes());
            b[e + 16..e + 24].copy_from_slice(&size.to_le_bytes());
        };
        put_sym(0, 1, 2, 0x2000, 0x10); // "alpha", STT_FUNC — deliberately out of order
        put_sym(1, 7, 2, 0x1000, 0x20); // "beta",  STT_FUNC
        put_sym(2, 12, 1, 0x3000, 0x8); // "notafunc", STT_OBJECT — must be skipped
        b
    }

    #[test]
    fn the_elf_walker_reads_func_symbols_sorted_and_skips_the_rest() {
        let dir = std::env::temp_dir().join("nitrox-xtask-elf-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("tiny.elf");
        fs::write(&path, tiny_elf()).unwrap();
        let syms = elf_function_symbols(&path).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["beta", "alpha"], "sorted by address, and only STT_FUNC");
        assert_eq!(syms[0].addr, 0x1000);
        assert_eq!(syms[1].size, 0x10);
        // And the resolver agrees with the walker's ordering.
        assert_eq!(resolve_symbol(&syms, 0x2008).as_deref(), Some("alpha+0x8"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_non_elf_is_refused_rather_than_misparsed() {
        let dir = std::env::temp_dir().join("nitrox-xtask-elf-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("not.elf");
        fs::write(&path, b"#!/bin/sh\necho hi\n").unwrap();
        assert!(elf_function_symbols(&path).is_err());
        let _ = fs::remove_file(&path);
    }

    /// The terminal's ground is its own, and no cell colour is invisible on it.
    ///
    /// **What this used to claim, and why each version was replaced.** M11 Part B first asserted
    /// that no ANSI colour equals a *chrome* colour — an attempt to encode "a theme must not
    /// retint what programs print" as an inequality. It is the wrong encoding, and the palette
    /// contained the counterexample: `ansi[0]` and `title_inactive` were both `#1C222A`, chosen
    /// independently as the darkest tone in one scheme. It passed only because `title_inactive`
    /// was the one theme colour missing from its list (PR #262 review, blocking 1). **Provenance
    /// is not equality**, and a test comparing values cannot see provenance.
    ///
    /// Its replacement asserted that the grid's two defaults *followed* the theme, which was
    /// structural from Part B until Part E cut the tie. What is asserted now is the decision that
    /// replaced it, and the evidence for it:
    ///
    /// - **The grid's ground is not the desktop's**, which fails if somebody re-ties them.
    /// - **No cell colour is the ground it is drawn on.** A cell painted in the background colour
    ///   is text that cannot be read at all — the one legibility property with a sharp edge.
    ///   (Nearly-equal is a judgement; ANSI black on a dark ground is dim everywhere, which is
    ///   the convention rather than a bug.)
    /// - **And the reason the tie was cut, as a live check**: the brightest of the sixteen is
    ///   within a hair of the desktop's white. Following the theme would put invisible text on
    ///   screen, and avoiding that would mean retuning the sixteen — the one thing the rule above
    ///   forbids. If somebody ever does retune them for a light ground, this fails and says so,
    ///   which is the moment to revisit the decision rather than to delete the assertion.
    #[test]
    fn the_terminals_ground_is_its_own_and_no_cell_is_invisible_on_it() {
        let theme = libui::paint::Theme::default();
        let palette = libterm::cell::Palette::default();

        assert_ne!(
            palette.background, theme.background,
            "the grid's ground was re-tied to the desktop's (M11 Part E); see the assertion below"
        );

        for (i, c) in palette.ansi.iter().enumerate() {
            assert_ne!(
                *c, palette.background,
                "ANSI colour {i} is the terminal's own background — text in it is invisible"
            );
        }

        let gap = |a: libdraw::format::Rgb, b: libdraw::format::Rgb| {
            (a.r.abs_diff(b.r)).max(a.g.abs_diff(b.g)).max(a.b.abs_diff(b.b))
        };
        let brightest = palette
            .ansi
            .iter()
            .copied()
            .max_by_key(|c| u32::from(c.r) + u32::from(c.g) + u32::from(c.b))
            .expect("sixteen colours");
        assert!(
            gap(brightest, theme.background) <= 32,
            "the brightest ANSI colour is {brightest:?}, no longer close to the desktop's ground \
             {:?} — the sixteen may have been retuned for a light ground, which is the trigger \
             for revisiting whether the grid should follow the theme after all",
            theme.background
        );
    }

    #[test]
    fn a_dialog_placement_reads_back_as_six_numbers() {
        // The line `check-login` aims a click from. It carries the parent as well as the origin,
        // because the assertion beside it is that the dialog was centred *on that window* — a
        // parser that dropped the parent would leave the gate checking arithmetic against a
        // window it had not identified.
        assert_eq!(
            parse_dialog_placement(" 21 of window 20 at 490,214 340x132"),
            Some((21, 20, 490, 214, 340, 132))
        );
        // A negative origin is legal: the clamp keeps a dialog inside the work area, and the
        // work area's own origin is not the screen's.
        assert_eq!(
            parse_dialog_placement("3 of window 2 at -4,24 100x50"),
            Some((3, 2, -4, 24, 100, 50))
        );
        // And the shapes that must not silently parse as something else.
        assert_eq!(parse_dialog_placement("21 at 490,214 340x132"), None, "no parent");
        assert_eq!(parse_dialog_placement("21 of window 20 at 490 340x132"), None, "no comma");
        assert_eq!(parse_dialog_placement(""), None);
    }

    #[test]
    fn a_taskbar_slot_is_the_position_of_the_id_not_the_id() {
        // **The distinction the gate depends on.** Ids are not slots — a window closed earlier
        // in the run leaves the ones after it at lower positions — and clicking `id * ENTRY_W`
        // would land on somebody else's entry as soon as anything had ever been closed.
        let list = "desktop 1 of 2 [20:nxfiles] [24:notes.txt*] [31:untitled]";
        assert_eq!(taskbar_slot(list, 20), Some(0));
        assert_eq!(taskbar_slot(list, 24), Some(1));
        assert_eq!(taskbar_slot(list, 31), Some(2));
        assert_eq!(taskbar_slot(list, 21), None, "a window on another desktop has no slot");
        // A title that contains a digit must not be mistaken for an id: the match is on the
        // group's own `id:` prefix, not on the group containing the number anywhere.
        assert_eq!(taskbar_slot("desktop 1 of 1 [7:file20.txt]", 20), None);
        assert_eq!(taskbar_slot("desktop 1 of 1 (empty)", 20), None);
    }
}
