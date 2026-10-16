//! Nitrox build orchestrator.
//!
//! `cargo xtask help` lists its commands.
//!
//! **Host tooling, and the kernel's rules do not reach it.** No "stable Rust only" and no
//! "no external crates": both are about what runs on the target. What applies here is written
//! down in `tools/CLAUDE.md` — the bar a host dependency clears, and why the checked-in lockfile
//! is what pins it. This file said "avoids external crates" until M11 Part A took `png`, which
//! is the sort of stale rule a reader hits before they find the reasoning (PR #261 review,
//! finding 1).

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod bench;
mod build;
mod checks;
mod desktop;
mod diag;
mod files;
mod gates;
mod guest_input;
mod help;
mod host_test;
mod image;
mod live_image;
mod preview;
mod qemu;
mod qmp;
mod render;
mod rootfs;
mod screen;
mod session;
mod shot;
mod throughput;
mod usb;

use bench::*;
use build::*;
use checks::*;
use desktop::*;
use diag::*;
use files::read_text;
use gates::*;
use guest_input::*;
use host_test::*;
use image::*;
use live_image::*;
use preview::*;
use qemu::*;
use qmp::*;
use render::*;
use rootfs::*;
use screen::*;
use session::*;
use shot::*;
use throughput::*;
use usb::*;

/// The kernel console's glyphs and layout, compiled here by path so that `check-fbcon` reads a
/// screendump back into text with the code the kernel drew it with — the same face, palette and
/// geometry, not a copy of them. The pair names nothing outside itself: `text` reaches `glyphs`
/// as `super::glyphs`, which is this crate's root here and `crate::fbcon` in the kernel.
#[path = "../../../kernel/src/fbcon/glyphs.rs"]
#[allow(dead_code, reason = "the kernel's module, of which the gates use the screen-reading half")]
mod glyphs;
#[path = "../../../kernel/src/fbcon/text.rs"]
#[allow(dead_code, reason = "the kernel's module, of which the gates use the screen-reading half")]
mod text;

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

/// Smallest FAT32 this tooling builds: below about this many mebibytes of clusters, `mformat`
/// refuses the geometry and a smaller filesystem would have to be FAT16. The installable ESP is
/// sized to its contents or to this, whichever is larger (Phase 5 Part H.1).
const FAT32_MIN_MIB: u64 = 33;

// Test-only fixture credential for the auth + session-mgr login-path demo. **Not a
// secret**: the shipped image stores only the one-way PBKDF2 verifier of
// `DEMO_PASSWORD`; the password is a build input for the emulator demo user. init's
// login selftest (auth Part E) must use these same literals. A fixed salt keeps the
// image build reproducible (a single demo user makes salt-uniqueness moot). See
// `docs/architecture/session-and-auth.md`.
/// **A framed window's chrome, as the gates aim at it** — `libui::widget`'s geometry, copied on
/// purpose (M11 decision 2): a gate that read the toolkit to know where to click could agree with a
/// toolkit that had stopped drawing where it says. `libui`'s own tests pin the other side, as
/// literals pressed on built trees: `the_title_buttons_land_where_the_gates_aim` for the title bar
/// and `dialog_buttons_land_where_the_constants_say` for a dialog. (This doc named
/// `title_button_centre` as the pin; it is a helper that moves with the constants, and the literal
/// test is the PR #313 review's optional 6.)
///
/// **One table since the desktop refresh's Part B**, which moved every one of these: the title bar
/// grew from 26 to 31, its buttons went from contiguous 26-pixel slots to the design's 23 with 9
/// between, and a window's content lost its 3-pixel frame. They were a dozen inline numbers —
/// `+ 13`, `- 39`, `- 65`, `+ 4` — and a change to the chrome was a search for all of them.
///
/// **The browser's own metrics are [`browser`] below**, a separate table because they come from a
/// separate crate and are checked a different way: `xtask` links `libui` and parses `nxfiles`.
mod chrome {
    /// A title bar's height, its bottom rule included (`libui::widget::TITLE_BAR_H`).
    pub const TITLE_BAR_H: i32 = 31;
    /// How far below a window's top edge its title bar is aimed at: the middle of the bar's face,
    /// below the one-pixel border.
    pub const TITLE_Y: i32 = 16;
    /// How far left of a window's right edge the **close** button's centre is: the border, five
    /// of padding, and half of a 23-pixel button.
    pub const CLOSE_X: i32 = 18;
    /// The **maximise** button's, one button and one 9-pixel gap further left.
    pub const MAXIMISE_X: i32 = 50;
    /// The **minimise** button's, one more further.
    pub const MINIMISE_X: i32 = 82;
    /// How far right of a window's left edge its content starts: the border, and no frame.
    pub const CONTENT_X: i32 = 1;
    /// A tab strip's height (`libui::widget::TAB_STRIP_H`) — 30 since the refresh's Part H, where
    /// it was 24 and a tab filled it.
    ///
    /// **Kept though no aim reads it today**: the browser stopped drawing a strip for a single
    /// tab in Part I, and the two aims that counted one are the reason that part's CI run
    /// failed. It stays because `the_gates_chrome_table_is_the_toolkits` compares it, so the
    /// next aim that needs a strip's height starts from a number something checks.
    #[allow(dead_code, reason = "read only by the test that holds it to the toolkit's")]
    pub const TAB_STRIP_H: i32 = 30;
    /// One tab's height inside that strip (`TAB_H`), and [`TAB_TOP`] the strip's padding above it.
    pub const TAB_H: i32 = 24;
    /// See [`TAB_H`].
    pub const TAB_TOP: i32 = 5;
    /// One tab's width (`TAB_W`). **Fixed rather than shared out**, which is what lets a gate aim
    /// at tab `n`: a tab does not move when another opens.
    pub const TAB_W: i32 = 120;
    /// The strip's padding before its first tab (`TAB_SIDE`), new in Part H.
    pub const TAB_SIDE: i32 = 6;
    /// From one tab's left edge to the next's (`TAB_PITCH`): a tab and the pixel between them.
    pub const TAB_PITCH: i32 = 121;
    /// **A `libui` dialog's size and its two buttons' centres** (`DIALOG_W`, `DIALOG_H`,
    /// `DIALOG_LEFT_CX`, `DIALOG_RIGHT_CX`, `DIALOG_BUTTON_CY`): the editor's confirmation, which
    /// `check-login` answers, and the shell's waiting dialog, which `check-logout` does
    /// (administration Part F.2). Here since F.2, when a second gate aimed at them; they were
    /// `check-login`'s locals.
    pub const DIALOG_W: i32 = 340;
    /// See [`DIALOG_W`].
    pub const DIALOG_H: i32 = 132;
    /// See [`DIALOG_W`]: the left button's centre — discard, End anyway.
    pub const DIALOG_LEFT_CX: i32 = 89;
    /// See [`DIALOG_W`]: the right button's centre — keep editing, Cancel.
    pub const DIALOG_RIGHT_CX: i32 = 250;
    /// See [`DIALOG_W`]: both buttons' vertical centre.
    pub const DIALOG_BUTTON_CY: i32 = 106;
    /// **How far in from the screen's right edge the power button is aimed at**: half its width,
    /// `desktop_shell::panel::POWER_W` (administration Part F.2), whose own test presses here.
    pub const POWER_FROM_RIGHT: i32 = 17;
}

/// **The file browser's own chrome, as the gates aim at it** — `nxfiles`'s metrics, copied on
/// purpose for [`chrome`]'s reason (M11 decision 2). `xtask` cannot link that crate: it is a
/// bare-target program, and its library half pulls `libsurface` with it, so
/// `the_gates_browser_table_is_the_browsers` reads them out of its source and compares — the way
/// the greeter's pair is kept honest (PR #318 review, finding 1).
mod browser {
    /// The sidebar's width (`nxfiles::SIDEBAR_W`): the design's 132 since the refresh's Part I.
    pub const SIDEBAR_W: i32 = 132;
    /// The margin around it (`nxfiles::SIDEBAR_PAD`) — nought since Part I, where it was 6: the
    /// design's sidebar runs to the window's edge.
    pub const SIDEBAR_PAD: i32 = 0;
    /// The path strip's height (`nxfiles::PATH_H`).
    pub const PATH_H: i32 = 24;
    /// A listing row's height (`nxfiles::ROW_H`): the design's 25 since Part I, where it was 20.
    pub const ROW_H: i32 = 25;
    /// The column header above the rows (`nxfiles::HEADER_H`), new in Part I.
    pub const HEADER_H: i32 = 25;
    /// The menu bar above the path strip (`nxfiles::MENU_BAR_H`).
    pub const MENU_BAR_H: i32 = 24;
    /// The Drives heading below the places (`nxfiles::DRIVES_HEADING_H`, a row's height): Phase 6
    /// Part F.
    pub const DRIVES_HEADING_H: i32 = ROW_H;
}

/// **The editor's chooser, as the gates aim at it** — `nxedit::CHOOSER_KEY`, the key its dialog's
/// elements are numbered from. `the_gates_editor_table_is_the_editors` keeps the copy honest; the
/// layout itself is `libui`'s, which this crate links, so the gate lays the chooser out as the
/// editor does rather than writing down where its controls are.
mod editor {
    /// The chooser's key base (`nxedit::CHOOSER_KEY`).
    pub const CHOOSER_KEY: u64 = 200;
    /// Its Up button's key, as `libui::chooser::view` numbers it.
    pub const CHOOSER_UP_KEY: u64 = CHOOSER_KEY + 7;
}

const DEMO_USER: &str = "alice";
const DEMO_PASSWORD: &str = "correct horse battery staple";
const DEMO_HOME: &str = "/home/alice";
/// A second account `test-interactive` adds, logs in as, and removes (administration Part D.4):
/// a fixture like `DEMO_PASSWORD`, for an account that exists only for the length of one step.
const BOB_PASSWORD: &str = "bob's first password";
/// What bob changes his password to.
const BOB_NEW_PASSWORD: &str = "bob's second password";
/// A second copy that does not match the first, typed once to be refused.
const BOB_MISTYPED_PASSWORD: &str = "bob's second pasword";
/// The wrong password `test-interactive` types at the login prompt, before the right one.
const LOGIN_WRONG_PASSWORD: &str = "wrong-password";
/// The wrong password it types at `with`'s prompt, in step 20b.
const WITH_WRONG_PASSWORD: &str = "not-the-password-4271";
/// **Every password `test-interactive` types**, none of which may appear in what the guest prints:
/// each is typed at a prompt with echo off, and a copy on the console is a copy in every log.
/// Step 20b checks its two early, so a failure there names the step; this is the whole run's.
const TYPED_PASSWORDS: &[&str] =
    &[DEMO_PASSWORD, LOGIN_WRONG_PASSWORD, WITH_WRONG_PASSWORD, BOB_PASSWORD, BOB_NEW_PASSWORD, BOB_MISTYPED_PASSWORD];

/// The greeter's window, `GREETER_W`×`GREETER_H` — **this file's own copy** of
/// `desktop-session-mgr`'s pair, as every chrome metric here is a copy (M11 decision 2).
///
/// **What keeps a copy honest is a test, not a comment.** Part D first claimed that the greeter's
/// own host test — which measures the card against *its* `GREETER_H` — made this copy safe, and it
/// did nothing of the kind: the two constants never met, so moving the card and its constant
/// together left this one behind and the first thing to notice would have been a boot
/// (PR #318 review, finding 1). `the_gates_greeter_size_is_the_greeters_own` reads them out of
/// that crate's source and compares, the way `abi-sync-check` keeps the kernel's constants and
/// `libkern`'s equal. So the loop is closed by two host tests: the card is as tall as the
/// greeter's constant, and the greeter's constant is this one.
const GREETER: (u32, u32) = (340, 141);

/// The `font_px` the staged `theme.toml` carries — **deliberately not the built-in 13**.
///
/// A gate asserting the default proves nothing: a client that never received the theme reports
/// the same number, so the assertion passes with the wire cut. This one can only have come from
/// the file, through the shell, onto the setup record and into a window (PR #263 review,
/// blocking 2). It is inside what the fixed chrome holds, which is what `MAX_FONT_PX` bounds.
///
/// **12 since the desktop refresh's Part G**, which chose it by photographing the desktop at 13
/// and 12 beside the design: DejaVu at 12 reads like the page's Plex at 11.5. It was 14, with the
/// built-in 16 — the two moved together, and stay one step apart for the reason above.
const THEME_FONT_PX: u8 = 12;

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
/// **The drawn half of the shell's line also tells fit from fill, but only because the gate's
/// screen is not this picture's shape** (Phase 5 Part E). 16:10 filled into 1360x768 draws
/// `1360x850 at 0,-41`, cropped top and bottom, where fit would leave bars down the sides — so a
/// theme staged back to `fit` fails `check-login` at that line. At 1280x800, the size the gates
/// booted before, the two rules draw the same `1280x800 at 0,0`, and there only the decoded half
/// carries weight; `check-resolutions` runs `check-login` at that size too.
const WALLPAPER_W: u32 = 1920;
/// See [`WALLPAPER_W`]. 16:10, which the gate's 1360x768 is not.
const WALLPAPER_H: u32 = 1200;

/// The folders staged under the demo home, which `libfs::HOME_FOLDERS` names too.
///
/// Kept in step by `check-login` rather than by the compiler — see where they are staged.
const HOME_FOLDERS: &[&str] = &["Documents", "Downloads", "Pictures"];

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
    /// `check-fbcon`: [`Normal`](Self::Normal) — the release userspace, unchanged — over a kernel
    /// built with `fbcon-gate`, which holds the screen still at two moments and panics when F10
    /// is pressed. A sampling gate needs frames that hold, and a machine that stops *after*
    /// userspace has drawn; a working kernel offers neither.
    FbconGate,
}

impl BuildMode {
    /// The mode's test feature (`None` = none), as the **kernel** takes it — see
    /// [`kernel_features`](Self::kernel_features).
    ///
    /// **No userspace crate takes it any more.** `init` was the last, until Phase 5 Part C.1
    /// made its `/subtreetest` binding manifest data; `session-mgr` stopped at retrofit Part B.
    /// `nxterm` takes `test-harness` alone, which is a different value and passed separately.
    fn features(self) -> Option<&'static str> {
        match self {
            BuildMode::Normal | BuildMode::FbconGate => None,
            BuildMode::Selftest => Some("selftest"),
            BuildMode::TestHarness | BuildMode::TestHarnessNoPs2Irq | BuildMode::Bench => Some("test-harness"),
        }
    }

    /// Whether the image carries **test data**: the `test` store package, the boot probe's
    /// declaration, and the test manifest's binds. True of every mode with a test feature —
    /// `--selftest` as much as the harness — and the one question the image build asks, now
    /// that no userspace program is built differently for it.
    fn stages_test_data(self) -> bool {
        self.features().is_some()
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
        matches!(self, BuildMode::TestHarness | BuildMode::TestHarnessNoPs2Irq | BuildMode::Bench)
    }

    /// The same, for the **kernel**, which has one feature userspace does not.
    ///
    /// Split rather than appended to [`features`](Self::features) because that value once also
    /// reached `init`, and `no-ps2-irq` is a statement about the i8042 that has no meaning in a
    /// userspace crate. **No userspace crate takes `features()` since Phase 5 Part C.1** — wiring it
    /// back into one would make that program differ between images, which `check-images` fails. The
    /// split stays because it is still the right shape: the kernel's feature set is a superset of
    /// the test mode's, and `stages_test_data` reads the test mode's.
    fn kernel_features(self) -> Option<&'static str> {
        match self {
            BuildMode::TestHarnessNoPs2Irq => Some("test-harness,no-ps2-irq"),
            BuildMode::FbconGate => Some("fbcon-gate"),
            other => other.features(),
        }
    }
}

/// **The commit this tree was built from**, as the running system shows it (the laptop polish's
/// Part D): `git rev-parse --short=12 HEAD`, with `-dirty` when a tracked file differs from it, or
/// `unknown` outside a repository. On 2026-10-01 a stick that had never been rewritten looked for an
/// afternoon like a bug in the installer; a line on the screen naming the build would have said so
/// at once.
fn build_commit() -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(repo_root())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let Some(head) = git(&["rev-parse", "--short=12", "HEAD"]).filter(|h| !h.is_empty()) else {
        return String::from("unknown");
    };
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    if dirty { format!("{head}-dirty") } else { head }
}

/// The line the kernel logs first on every boot, naming the commit this process built it from.
fn built_from_line() -> String {
    format!("nitrox: built from {}", env::var(COMMIT_ENV).unwrap_or_else(|_| String::from("unknown")))
}

/// The environment variable [`build_commit`]'s answer reaches every build through: the kernel's
/// and `nxsh`'s `option_env!`, which rustc records as a dependency, so a new commit rebuilds the
/// two crates that show it and nothing else.
const COMMIT_ENV: &str = "NITROX_COMMIT";

fn main() -> ExitCode {
    // Run by an edit hook, outside `cargo`: before anything that needs `CARGO_MANIFEST_DIR`.
    if env::args().nth(1).as_deref() == Some("hook-comments") {
        return comments::hook_main();
    }
    // **Set once, for every child**: each cargo this process runs inherits it, so no build path —
    // kernel, userspace, host tests — can forget to pass it. Before any thread exists.
    // SAFETY: single-threaded at this point; nothing reads the environment concurrently.
    unsafe { env::set_var(COMMIT_ENV, build_commit()) };
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
    // `--usb` (Phase 6 Part B) boots a machine with no i8042 and a USB keyboard and mouse, so a key
    // that arrives came through USB or not at all. See `usb_input_args`.
    let usb = rest.iter().any(|a| a == "--usb");
    // `--grab` (interactive `qemu` only) makes the QEMU window take the host's pointer and
    // keyboard. See `cmd_qemu`: without a grab the guest cursor and the host pointer are two
    // different cursors, and the host desktop keeps `Super` for itself.
    let grab = rest.iter().any(|a| a == "--grab");
    // `--live` (`image` only): build the live image instead — see `cmd_image_live`.
    let live = rest.iter().any(|a| a == "--live");
    // `--size WxH` (the gates that boot a screen, and `qemu`): the screen QEMU gives the guest —
    // `DisplaySize::GATE` for a gate without it, QEMU's own default for `qemu`. See `DisplaySize`.
    let size_flag = rest.iter().position(|a| a == "--size");
    let size = match size_flag.map(|i| rest.get(i + 1)) {
        None => None,
        Some(None) => {
            eprintln!("xtask: `--size` wants a size, WxH");
            return ExitCode::FAILURE;
        }
        Some(Some(text)) => match DisplaySize::parse(text) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("xtask: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
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
    if live && cmd.as_deref() != Some("image") {
        eprintln!(
            "xtask: `--live` is only meaningful for `image` — `check-live` builds the live image \
             itself, and `check-images` compares it"
        );
        return ExitCode::FAILURE;
    }
    const SIZED: &[&str] = &[
        "check-display",
        "check-input",
        "check-terminal",
        "check-login",
        "check-logout",
        "check-fbcon",
        "check-live",
        "check-storage",
        "check-media",
        "check-shutdown",
        "check-report",
        "check-install",
        "check-recovery",
        "shot",
        "bench-compose",
        "qemu",
        "qemu-debug",
    ];
    if size.is_some() && !cmd.as_deref().is_some_and(|c| SIZED.contains(&c)) {
        eprintln!(
            "xtask: `--size` is only meaningful for a command that boots a screen ({}) — \
             `test-qemu` and `test-interactive` keep QEMU's default, so every CI run boots two sizes",
            SIZED.join(", ")
        );
        return ExitCode::FAILURE;
    }
    let gate_size = size.unwrap_or(DisplaySize::GATE);
    if no_ps2_irq && cmd.as_deref() != Some("check-input") {
        eprintln!(
            "xtask: `--no-ps2-irq` is only meaningful for `check-input` — it boots a kernel \
             whose i8042 never asserts its IRQs, so the tick-driven recovery sweep is the \
             only path input can take"
        );
        return ExitCode::FAILURE;
    }
    if usb && !matches!(cmd.as_deref(), Some("check-input") | Some("check-login") | Some("check-report")) {
        eprintln!(
            "xtask: `--usb` is only meaningful for `check-input`, `check-login` and `check-report` — \
             it boots a machine whose only keyboard and mouse are USB"
        );
        return ExitCode::FAILURE;
    }
    let accel = if rest.iter().any(|a| a == "--kvm") { Accel::Kvm } else { Accel::Tcg };
    let qargs: Vec<String> = rest
        .iter()
        .enumerate()
        .filter(|&(i, a)| {
            *a != "--selftest"
                && *a != "--kvm"
                && *a != "--no-ps2-irq"
                && *a != "--usb"
                && *a != "--grab"
                && *a != "--live"
                && Some(i) != size_flag
                && Some(i) != size_flag.map(|f| f + 1)
        })
        .map(|(_, a)| a.clone())
        .collect();
    let mode = if selftest { BuildMode::Selftest } else { BuildMode::Normal };

    let result = match cmd.as_deref() {
        Some("build") => cmd_build(mode),
        Some("image") if live => cmd_image_live_for(mode),
        Some("image") => cmd_image(mode),
        Some("qemu") => cmd_qemu(false, mode, accel, grab, size, &qargs),
        Some("qemu-debug") => cmd_qemu(true, mode, accel, grab, size, &qargs),
        Some("test") => cmd_test(),
        Some("test-qemu") => cmd_test_qemu(accel),
        Some("test-interactive") => cmd_test_interactive(accel),
        Some("check-arch") => cmd_check_arch(),
        Some("check-nightly") => cmd_check_nightly(),
        Some("check-deferrals") => cmd_check_deferrals(),
        Some("check-docs") => docs::cmd_check_docs(),
        Some("check-comments") => comments::cmd_check_comments(&qargs),
        Some("check-moves") => moves::cmd_check_moves(&qargs),
        Some("check-size") => size::cmd_check_size(&qargs),
        Some("check-images") => cmd_check_images(),
        // **The first *positional* argument, not the first argument.** `qargs` has the global
        // flags stripped, and anything else beginning with `-` is a flag this command does not
        // have — landing one in the name slot reports "no preview called `--offline`", which
        // names the wrong problem (PR #261 review, optional 3).
        Some("tune") => cmd_tune(&qargs),
        Some("preview") => cmd_preview(qargs.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("all")),
        // Same positional-argument rule as `preview`, and for the same reason: `--kvm` in the
        // name slot would be reported as "no shot called `--kvm`".
        Some("shot") => {
            cmd_shot(qargs.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("all"), accel, gate_size)
        }
        Some("check-display") => cmd_check_display(accel, gate_size),
        Some("check-terminal") => cmd_check_terminal(accel, gate_size),
        Some("check-login") => cmd_check_login(accel, usb, gate_size),
        Some("check-logout") => cmd_check_logout(accel, gate_size),
        Some("check-fbcon") => cmd_check_fbcon(accel, gate_size),
        Some("check-live") => cmd_check_live(accel, gate_size),
        Some("check-storage") => cmd_check_storage(accel, gate_size),
        Some("check-media") => cmd_check_media(accel, gate_size),
        Some("check-shutdown") => cmd_check_shutdown(accel, gate_size),
        Some("check-install") => cmd_check_install(accel, gate_size),
        Some("check-recovery") => cmd_check_recovery(accel, gate_size),
        Some("check-report") => cmd_check_report(accel, usb, gate_size),
        Some("check-resolutions") => cmd_check_resolutions(accel),
        Some("bench-compose") => cmd_bench_compose(accel, gate_size),
        Some("throughput-stick") => cmd_throughput_stick(&qargs),
        Some("check-input") => cmd_check_input(accel, no_ps2_irq, usb, gate_size),
        Some("check-irq-scope") => cmd_check_irq_scope(),
        Some("abi-sync-check") => cmd_abi_sync_check(),
        Some("fetch-limine") => cmd_fetch_limine().map(|_| ()),
        Some("clean") => cmd_clean(),
        Some("help") | Some("--help") | Some("-h") | None => {
            help::print_help();
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
    repo_root().join("kernel").join("target").join("x86_64-unknown-none").join("debug").join("nitrox-kernel")
}

fn image_path() -> PathBuf {
    build_cache().join("nitrox.hdd")
}

fn limine_conf() -> PathBuf {
    repo_root().join("boot").join("limine.conf")
}

/// Recursively visit every `.rs` file under `dir`, calling `f` on each. Like
/// [`visit_rs_files`], but pruning any directory whose name is in `skip` (e.g. build
/// output trees, which can be enormous and are not project source).
fn visit_rs_files_skipping(dir: &Path, skip: &[&str], f: &mut dyn FnMut(&Path) -> R<()>) -> R<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if skip.contains(&name) {
                continue;
            }
            visit_rs_files_skipping(&path, skip, f)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
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
        } else if path.extension().is_some_and(|e| e == "rs") {
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
    parse_host_from_rustc_vv(&text).ok_or_else(|| "rustc -vV did not contain a `host:` line".into())
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

// --- Helpers ------------------------------------------------------------

fn require_tool(name: &str) -> R<()> {
    let status = Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(format!("required host tool `{name}` is missing — install it and retry").into()),
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
mod tests;

#[cfg(test)]
mod diag_tests;
