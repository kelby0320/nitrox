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
mod comments;
mod desktop;
mod diag;
mod docs;
mod files;
mod gates;
mod guest_input;
mod help;
mod host_test;
mod image;
mod live_image;
mod moves;
mod preview;
mod qemu;
mod qmp;
mod render;
mod rootfs;
mod screen;
mod session;
mod shot;
mod size;
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








/// `check-login`'s steps 6b to 6e: the window list, show desktop, the lifecycle rule, the
/// switcher, the overview and `/dev/desktop`.
fn login_window_list_and_overview(
    qmp: &mut Qmp,
    session: &mut Session,
    size: DisplaySize,
    st: &mut LoginState,
) -> R<()> {
    let list_click = st.list_click;
    let term_h = st.term_h;
    let term_w = st.term_w;
    let term_x = st.term_x;
    let term_y = st.term_y;
    let wallpaper_id = st.wallpaper_id;
    // 6b. **The window list, and the two things you can do to a window from it** (M8 Part C).
    //
    //     The launched terminal is a `normal` window, so it is listed — and it holds the
    //     keyboard, so the bar shows it focused. Clicking a focused entry puts the window away;
    //     clicking it again brings it back. This is the first thing in the shell that reflects
    //     compositor state continuously rather than at one moment, so the assertions are about
    //     what the list *says*, not only that a click was received.
    //
    //     The bar is the bottom `DisplaySize::BAR_H` rows of the screen, and the first task
    //     button spans x 48 to 234 — so `list_click`, at its middle and half the bar's height, is
    //     inside it at any size.

    // Close the menu first: it is a popup on top, and a press meant for the bar would land in
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
    // **The bottom bar's dead space**, between the last task button and the switcher. A panel
    // never takes focus, so a press there raises nothing and produces no focus
    // change *whatever else is on screen* — which is what makes it the honest test. Aiming at
    // bare desktop instead would depend on the terminal not being maximised at this point in the
    // gate, and it is.
    click_at(qmp, session, 600, size.bottom_bar_click_y())?;
    session.expect("desktop-shell: applications menu closed")?;
    // **That the compositor *said* so is checked against the whole transcript below**, not here.
    // The dismissal is logged while the press is being routed and the `press at` line is logged
    // when the routed record is delivered — so the dismissal comes *first*, and `click_at`'s own
    // position assertion has already scanned past it. Same rule this gate applies to every line
    // whose order is an implementation detail rather than a claim.

    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: minimized window ")?;
    // **The marker, not just a non-empty list.** The list still holds it — minimizing is not
    // closing, and a taskbar that dropped the entry would leave no way to get the window back —
    // and `_` is how the bar says so. Matching only `window list [` asserted the list existed
    // and nothing about what it shows (PR #242 review, optional 9).
    session.expect("desktop-shell: window list on ")?;
    session.expect(":_ nxterm")?;

    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;
    // Restored and focused. Focus arrives one iteration after the raise, so this is the second
    // list line the click produces, not the first.
    // **And it is named**, which is the title arm of the list doing something: `nxterm` sets a
    // title, the compositor reports it on `WindowTitle`, and the bar shows it instead of
    // `window 6`. Nothing in the tree sent a title before this part.
    session.expect(":> nxterm")?;

    // 6b2. **Show desktop, and back** (desktop refresh, Part C). The first press puts away every
    //      window this desktop's bar shows and lights the button; the second brings back exactly
    //      what the first put away. The restore set is the shell's, because minimising is a
    //      manager operation — which the plan's first draft put in the compositor (its review,
    //      finding 6). **One window here**, so the counts say the press put something away and
    //      brought it back, and no more: that it brings back *exactly* what it put away — not a
    //      window already minimised, not one past the bar's end — needs more windows than this
    //      step has, and is `desktop_shell::ShownDesktop`'s host tests (PR #314 review, optional
    //      5).
    let show = size.show_desktop_click();
    click_at(qmp, session, show.0, show.1)?;
    session.expect("desktop-shell: showing the desktop, minimised 1 window(s)")?;
    session.expect("desktop-shell: window list on ")?;
    session.expect(":_ nxterm")?;
    click_at(qmp, session, show.0, show.1)?;
    session.expect("desktop-shell: restored 1 window(s)")?;
    // Focus arrives one iteration after the raise, so this is the second list line, as above.
    session.expect(":> nxterm")?;
    println!("  ok: show-desktop put the window away and brought it back");

    // And the chord, which is the half a taskbar alone does not cover: putting a window away
    // without reaching for its entry. `Super+H`.
    chord(qmp, false, "h")?;
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
    // **Restore the terminal first: 6b left it minimized**, and a minimized window is not
    // focused — so `Super+Shift+N`, which moves *the focused window*, would correctly find
    // nothing to move and this block would assert against a gesture that did nothing. Clicking
    // its list entry restores and raises it, which is the gesture Part C added for exactly this.
    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;
    session.expect(":> nxterm")?;

    // Name it. The prompt is the same popup the launcher uses — a `panel` takes no keyboard
    // focus, so the bar itself could never read a typed name.
    chord(qmp, false, "r")?;
    session.expect("desktop-shell: naming this desktop")?;
    // **`Super+A` closes this too, and says which popup it closed.** One window serves both the
    // launcher and the name prompt, so a dismissal that read as the launcher's would send a
    // later gate looking for a line sitting in the transcript under the other name (PR #304
    // review, finding 1). Then reopen it and carry on naming.
    chord(qmp, false, "a")?;
    session.expect("desktop-shell: name prompt closed")?;
    chord(qmp, false, "r")?;
    session.expect("desktop-shell: naming this desktop")?;
    // **One character at a time, waiting for each**, the way `type_at_greeter` does. Injection
    // is relative and unacknowledged: a dropped batch ate the `r` and produced a desktop named
    // `wok`, which fails an assertion about naming while saying nothing about naming.
    let mut typed = String::new();
    for c in "work".chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        typed.push(c);
        session.expect(&format!("desktop-shell: name so far {typed}"))?;
    }
    press(qmp, "ret")?;
    session.expect("desktop-shell: named this desktop work")?;
    session.expect("desktop-shell: window list on work of 2")?;

    // 6c2. **The switcher** (desktop refresh, Part C): `›` goes to the next desktop, and a cell to
    //      its own. Two desktops here — `work` and the scratch one after it — so two cells.
    //      Aimed from the right edge, where the switcher is anchored; the name beside the arrows
    //      moves them, so `desktop_shell::panel`'s own test pins both aims with `work` and then
    //      `Desktop 2` as the name — the two states this step presses them in.
    const NEXT_FROM_RIGHT: i32 = 67;
    const FIRST_CELL_FROM_RIGHT: i32 = 148;
    let bar_y = size.bottom_bar_click_y();
    click_at(qmp, session, size.w as i32 - NEXT_FROM_RIGHT, bar_y)?;
    session.expect("desktop-shell: switched to Desktop 2")?;
    session.expect("desktop-shell: window list on Desktop 2 of 2 (empty)")?;
    click_at(qmp, session, size.w as i32 - FIRST_CELL_FROM_RIGHT, bar_y)?;
    session.expect("desktop-shell: switched to work")?;
    // **And the terminal has the keyboard again, because it is on the desktop the compositor is
    // showing.** The first runs of this step came back `[N:  nxterm]` — listed, unfocused, and not
    // on screen: the shell had been re-pointed off a removed desktop at step 6a3 without telling
    // the compositor, so "work" was one desktop to the shell and another to the compositor, and
    // nothing showed it until something switched away and back (`sync_current`). The move below
    // depends on this too: it moves the *focused* window.
    session.expect("desktop-shell: window list on work of 2 [")?;
    session.expect(":> nxterm")?;
    println!("  ok: the switcher's arrow and its cell each switched desktops, and back to the window");

    // Move the terminal to the second desktop. `work` is now empty — and **named**, so it
    // stays; the desktop that received the window is no longer the scratch slot, so a new one
    // is appended. Two facts in one line: the bar still says `work`, and there are three.
    chord(qmp, true, "2")?;
    session.expect("desktop-shell: moved window ")?;
    session.expect("desktop-shell: window list on work of 3 (empty)")?;

    // Follow it, to prove the list filters by desktop rather than merely being emptied.
    chord(qmp, false, "2")?;
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
    chord(qmp, true, "1")?;
    session.expect("desktop-shell: moved window ")?;
    session.expect("desktop-shell: window list on Desktop 2 of 2 (empty)")?;

    // 6d. **The overview** (M8 Part E): frozen thumbnails of this desktop, a sidebar of the
    //     others, and a window moved by dropping its thumbnail on one.
    //
    //     Opened from the desktop's name at the switcher's end — the indicator's place until the
    //     desktop refresh's Part C, which `desktop-shell.md` §7 always said opens the overview;
    //     M8 Part D made it advance to the next desktop only because there was no overview yet.
    //
    //     Bring the terminal back to this desktop first: `work` is empty after 6c, and an
    //     overview of nothing has no thumbnail to drag.
    chord(qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
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
    // **How many cards there are, read rather than assumed** (desktop refresh, Part E). The
    // block of cards is centred, so every card's position depends on the count — and the count
    // is a fact about this desktop's *state*, which the gate cannot compute. The line reads
    // `<id> showing <k> of <n> desktops`. Where each card then *sits* is the gate's own
    // arithmetic, kept here on purpose (M11 decision 2).
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;

    // Press inside the terminal's box in the **current** desktop's card, which is the first —
    // 6d switched to `work` with the chord above. Release over the second card, which is the
    // desktop the window is being moved to.
    //
    // **Aimed from the window's real geometry**, which the shell reported when it placed it:
    // a card draws every window where it actually is, so unlike the old fixed grid there is no
    // cell to aim at, and a hard-coded point would be a point that moved with the cascade.
    let thumb = size.window_in_card(0, cards, (term_x, term_y, term_w, term_h));
    // **The drag starts from a position already verified — by the click that opened this.** A
    // drag cannot check its own start: there is no press receipt until the button goes down, and
    // by then it has begun. The name's `click_at` above asserted where it landed and left the
    // pointer there, and opening the overview does not move it, so the walk to the thumbnail is
    // the same arithmetic every other step here does.
    //
    // **This used to be a second `click_at`, and there is nowhere left to aim one.** It was on
    // the thumbnail while a pick-up-and-abandon changed nothing, then on empty background while
    // that did nothing either. As of 2026-08-26 a click on a thumbnail raises its window and a
    // click on the background dismisses — which is the point of those changes, and leaves a
    // verifying click with no inert place to land.
    move_pointer_to(qmp, thumb.0, thumb.1)?;
    qmp.pointer = Some(thumb);
    qmp.send_button("left", true)?;
    // The press is the last event injected, so QEMU may hold it — see `expect_after_pointer`.
    expect_after_pointer(qmp, session, "desktop-shell: dragging window ")?;
    let (dx, dy) = size.card_click(1, cards);
    move_pointer_to(qmp, dx, dy)?;
    qmp.pointer = Some((dx, dy));
    qmp.send_button("left", false)?;
    expect_after_pointer(qmp, session, "desktop-shell: dropped window ")?;
    session.expect("desktop-shell: overview closed")?;

    // And it really moved: `work` is empty again, and the window is on the desktop it was
    // dropped on. **`(empty)` rather than the bare prefix**, which matched a list still holding
    // the window just as happily as one that had lost it (PR #244 review, optional 6).
    session.expect("desktop-shell: window list on work of 3 (empty)")?;

    // **A window clicked in a card that is not the current desktop's** (PR #323 review,
    // blocking 1). The state here is exactly the one that matters: `work` is current and empty,
    // and the terminal is on `cli` — so the terminal's box is in a card the user is not on.
    //
    // Before the fix this raised a window on a desktop nobody was looking at and closed the
    // overview, leaving the screen unchanged: `WindowStack::raise` reorders a stack, and
    // nothing had changed which desktop was being composited. **No step reached it**, because
    // the card click above aims at `card_click` — "where the placement cascade never reaches" —
    // and the raise click below only ever ran once its desktop was already current.
    //
    // The switch is asserted *before* the raise because that is the order the shell does it in:
    // a raise on a desktop that is not being composited is the bug, not the fix.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;
    let away = size.window_in_card(1, cards, (term_x, term_y, term_w, term_h));
    click_at(qmp, session, away.0, away.1)?;
    // **`Desktop 2`, not `cli`**: the second desktop is not named until 6f, so at this point its
    // label is the positional one `desktop_label` gives an unnamed desktop.
    session.expect("desktop-shell: switched to Desktop 2")?;
    session.expect("desktop-shell: overview raised window ")?;
    session.expect("desktop-shell: overview closed")?;
    println!("  ok: a window in another desktop's card went to that desktop and raised it");

    // Back to `work`, so the rest of this step runs from where it used to.
    chord(qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    chord(qmp, false, "2")?;
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
    click_at(qmp, session, 200, 200)?;
    type_at_terminal(qmp, "desktop")?;
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
    type_at_terminal(qmp, "desktop name 2 cli")?;
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
    // **The desktop's name opens the overview**, as the indicator in its place did since M8
    // Part E — what `desktop-shell.md` §7 always specified. Escape closes it again so the serial login below is not typing at a
    // popup that holds the keyboard.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
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
    press(qmp, "esc")?;
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
    chord(qmp, false, "3")?;
    session.expect("desktop-shell: switched to ")?;
    // **Empty is asserted, not assumed** — the discriminator only works where nothing else is
    // under the cursor, and which desktop holds what has moved several times by this point.
    session.expect("(empty)")?;
    // **`click_at` hands back the press line's tail**, which is the `win=` it ends with; an
    // `expect` for the whole line would scan past its own evidence. (It did, on this step's first
    // run.) Returned rather than read here, because `click_at` now also waits for the *release*
    // and that wait moves the cursor past the press line — a `rest_of_line` here would read the
    // release's `win=`, which is `none` by construction.
    let hit = click_at(qmp, session, size.centre().0, size.centre().1)?;
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
    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;

    // 6f. **The overview's clicks, which were dead until 2026-08-26.** `desktop-shell.md` §6
    //     says "you can switch desktops from inside it"; a press on a sidebar row set no drag
    //     and its release matched no arm, and a press-and-release on a thumbnail was discarded
    //     as an abandoned drag. So the two obvious gestures — go to that desktop, go to that
    //     window — did nothing, and only the *drag* the gate above exercises was ever wired up.
    //     Reported from a real session, which is the part worth keeping: the drag was gated and
    //     the click was not, and a gate that drives only the gesture it was written for cannot
    //     tell the difference between "unimplemented" and "untested".
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;

    // The chord path first: an overview left showing the desktop you just switched away from is
    // showing a card whose windows are no longer on it. It follows instead of closing,
    // which is what §6 means by "it fetches a different set of images".
    chord(qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    session.expect("desktop-shell: overview now showing 0 on work")?;

    // Then a click on a card, with no drag in flight. Card 1 is the second desktop — `cli`,
    // which is where the terminal is — so the refresh must find it again.
    //
    // **Aimed at the foot of the card**, clear of its border and of any window drawn in it:
    // this click means "go to that desktop", and one that landed on a window would mean "raise
    // that window" instead — which is the distinction the cards made possible and the reason
    // this point is not simply the card's middle.
    let (sx, sy) = size.card_click(1, cards);
    click_at(qmp, session, sx, sy)?;
    session.expect("desktop-shell: switched to cli")?;
    session.expect("desktop-shell: overview now showing 1 on cli")?;

    // **Clicking the row you are already on dismisses**, which is the way out of an overview on
    // a desktop with no windows — where clicking a window, the other way out, does not exist.
    // Reported as being stuck there with Escape the only escape, and Escape is not discoverable.
    click_at(qmp, session, sx, sy)?;
    session.expect("desktop-shell: overview closed")?;

    // **And a click on its background dismisses**, the way clicking outside a menu does. That
    // also makes the desktop's name a toggle: the overview covers the bar, so a second click
    // where the name is lands on background.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    click_at(qmp, session, 600, 700)?;
    session.expect("desktop-shell: overview closed")?;

    // And a click on a window in a card activates it, which is the third way out and the one
    // that takes you somewhere. `raise_window` is the same call the window list's entries make.
    //
    // **The terminal is on `cli` now**, which the clicks above switched to, so its box is in
    // whichever card that is — found by the same arithmetic as the drag, against the geometry
    // the shell reported when it placed the window.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;
    let on_term = size.window_in_card(1, cards, (term_x, term_y, term_w, term_h));
    click_at(qmp, session, on_term.0, on_term.1)?;
    session.expect("desktop-shell: overview raised window ")?;
    session.expect("desktop-shell: overview closed")?;

    Ok(())
}

/// `check-login`'s steps 6f to 6k and the second session: the overview's clicks, dragging,
/// resizing and snapping windows.
fn login_overview_drags(qmp: &mut Qmp, session: &mut Session, st: &mut LoginState) -> R<()> {
    let term_id = st.term_id;
    let term_w = st.term_w;
    let term_x = st.term_x;
    let term_y = st.term_y;
    let work = st.work;
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
    //     tracks correctly, which is exactly the defect the `scroll-grab` deferral described. Every
    //     other step here waits for the guest between injections; this one deliberately does not,
    //     because a stationary pointer across that round trip measures zero drift where a person
    //     sees forty pixels (PR #247 review, finding 4).
    const DRAG_STEPS: i32 = 4;
    const DRAG_DX: i32 = 10;
    const DRAG_DY: i32 = 5;
    // The title bar is the top of the window; x=100 is clear of the buttons at its right.
    let press_at = (100, term_y + chrome::TITLE_Y);
    move_pointer_to(qmp, press_at.0, press_at.1)?;
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
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "desktop-shell: window {term_id} geometry {},{} ",
            term_x + DRAG_STEPS * DRAG_DX,
            term_y + DRAG_STEPS * DRAG_DY
        ),
    )?;
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
    click_at(qmp, session, moved_x + term_w as i32 - chrome::MAXIMISE_X, moved_y + chrome::TITLE_Y)?;
    // **Two producers, one cause, so no order between them.** The shell logs the window's new
    // origin when the compositor's geometry event reaches it; the client logs the size it took
    // when the `Configure` reaches it. Both are downstream of the same apply and neither is
    // downstream of the other, so on four vCPUs either can reach the console first. Asserted as
    // a sequence this passed for two milestones and then failed in CI with both lines present
    // and the *first* expect having scanned past the second (2026-08-31).
    //
    // **And the request and the shell's answer to it in the same wait** (PR #353's CI): `nxterm`
    // logs the request after the compositor answers it, which is after the shell has it, so the
    // shell's maximise *and its move* can both come before the client's line. Waited for apart,
    // the first wait's cursor jumps past the move to the request, and the second takes the final
    // geometry for the move.
    //
    // The client's line says what it did with the size *and* what that came to in cells, which
    // is the difference between a window that grew and a terminal that can use the room: a grid
    // still 80x24 in a 1280x752 window would satisfy every other line here.
    let maximised = format!("desktop-shell: maximize window {term_id} to {},{} {}x{}", work.0, work.1, work.2, work.3);
    let moved = format!("desktop-shell: window {term_id} geometry {},{} ", work.0, work.1);
    let took = format!("nxterm: resized to {}x{}, grid ", work.2, work.3);
    session.expect_all(&["nxterm: asked the shell for window state 2", &maximised, &moved, &took])?;
    // And the committed geometry — the one the compositor reports and `/dev/draw/<id>/info`
    // answers with — is the work area exactly, not the work area rounded down to whole cells.
    session.expect(&format!("desktop-shell: window {term_id} geometry {},{} {}x{}", work.0, work.1, work.2, work.3))?;
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
    move_pointer_to(qmp, grip_at.0, grip_at.1)?;
    qmp.pointer = Some(grip_at);
    qmp.send_button("left", true)?;
    // `RESIZE_RIGHT | RESIZE_BOTTOM` is `2 | 8`.
    expect_after_pointer(qmp, session, &format!("compositor: interactive resize of window {term_id} edges 10"))?;
    for _ in 0..RESIZE_STEPS {
        qmp.send_motion(RESIZE_DX, RESIZE_DY)?;
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((grip_at.0 + RESIZE_STEPS * RESIZE_DX, grip_at.1 + RESIZE_STEPS * RESIZE_DY));
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "compositor: interactive resize of window {term_id} ended at {},{} {resized_w}x{resized_h}",
            work.0, work.1
        ),
    )?;
    // **The shell is what resizes the client**, which is the whole reason the gesture ends in an
    // event rather than in a `Configure` from the compositor: one path to a window's geometry
    // rather than two that can disagree. It says `drop` rather than `resize` because since
    // Part F one event ends both gestures, so the shell has one word for what it is answering —
    // the user let go, and this is the rectangle.
    session
        .expect(&format!("desktop-shell: drop window {term_id} to {},{} {resized_w}x{resized_h}", work.0, work.1))?;
    session.expect(&format!("nxterm: resized to {resized_w}x{resized_h}, grid "))?;
    // And the committed geometry, which is the assertion a release that commits nothing fails.
    session
        .expect(&format!("desktop-shell: window {term_id} geometry {},{} {resized_w}x{resized_h}", work.0, work.1))?;
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
    //     bar is at `work.1 + chrome::TITLE_Y` and clear of the buttons at `x = 100`.
    // **First the control, as a step of its own with a positive assertion**: a drag that passes
    // *through* a zone and is released outside it must snap nothing — and what it must do
    // instead is the ordinary move, so the assertion is the geometry that move produces. A
    // compositor that snapped on entry rather than on release would report the zone's target
    // here, and this line is what catches it. Asserting the absence of a snap would not: the
    // step after this one produces exactly that line, and an `expect` scans forward.
    let press_at = (100, work.1 + chrome::TITLE_Y);
    move_pointer_to(qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    expect_after_pointer(qmp, session, "compositor: interactive move of window ")?;
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
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "desktop-shell: window {term_id} geometry {},{} {}x{}",
            work.0 + walked.0,
            work.1 + walked.1,
            resized_w,
            resized_h
        ),
    )?;
    println!("  ok: a drag through the zone and out again moved the window and snapped nothing");

    // Then the drop itself, from the title bar where the window now is.
    //
    // **The press is asserted to land on *that* window**, which is what makes the step above a
    // control rather than a hope: a compositor that snapped on entry has moved the terminal to
    // the left half, so this press lands on nothing — and the failure says so here, one line
    // after the assertion it belongs to, instead of surfacing as a missing drag further down.
    let press_at = (passed_to.0 + 90, passed_to.1);
    move_pointer_to(qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    expect_after_pointer(
        qmp,
        session,
        &format!("compositor: press at x={} y={} win={term_id}", press_at.0, press_at.1),
    )?;
    // **On its title bar specifically**, which is the sharpest statement of where the window is:
    // a terminal that had snapped on the pass-through above sits at the work area's origin, so
    // this press lands on its *grid* — inside the window, and no drag at all. Without this line
    // the control's failure surfaces two steps later as a drag that never began.
    // In either order: a client's request and the compositor's receipt of it, as the file drag's
    // two lines are (Phase 6 Part B).
    session.expect_all(&["nxterm: dragging its own title bar", "compositor: interactive move of window "])?;
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
    expect_after_pointer(
        qmp,
        session,
        &format!("desktop-shell: drop window {term_id} to {},{} {}x{}", work.0, work.1, work.2 / 2, work.3),
    )?;
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
    burst_holds_its_position(qmp, session, &chord)?;

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
    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    click_at(qmp, session, work.0 + (work.2 / 2) as i32 - chrome::CLOSE_X, work.1 + chrome::TITLE_Y)?;
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

    Ok(())
}

/// `check-login`'s step 7: the file browser, and the two sessions used as one fact.
fn login_file_browser(qmp: &mut Qmp, session: &mut Session, st: &mut LoginState) -> R<()> {
    let shell_px = st.shell_px;
    // 7. **The file browser, and the two sessions used as one fact** (M10 Part B). The serial
    //    shell just logged in above, and it sees the *same* `/home` subtree the graphical
    //    session does — `libsession::build_namespace` and `desktop-shell::build_app_namespace`
    //    bind it identically, which is the assumption M10's decision 1 rests on. So the serial
    //    side can *make* the thing the graphical side is then asserted to see, which is a
    //    stronger gate than a fixture: nothing here is arranged by the harness.
    session.send("mkdir ./papers")?;
    session.expect("/home>")?;

    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, "nxfiles")?;
    press(qmp, "ret")?;
    // **The theme reached the application** (M11 Part C): a value that travelled from a file on
    // disk, through one reader in the shell, onto the setup record every launch already carries,
    // and into a window. It is asserted here because it is the first thing the client says — it
    // reads what the session told it before it reads a directory.
    // **The same number at both ends**, which is what says the theme crossed the wire rather than
    // each end reaching for its own default.
    //
    // **Unordered against the shell's own line**, for the reason the `nxedit` launch below
    // already carries and this one did not: the shell logs *after* `send_setup_env` returns, so
    // the application is already running and can reach the serial console first. Held as a
    // sequence, this passed for two milestones and then failed in CI on 2026-09-03 — timing out
    // on `nxfiles: theme font_px 14` while that exact line sat in the transcript it printed,
    // which is the symptom `expect_all`'s own doc predicts.
    //
    // **The race was always there; what changed is only how often it is lost.** M13 Part C grew
    // every window's damage by a shadow's radius, so the shell has more repainting to do around
    // the launch — a plausible nudge and not a proven one. Worth stating that way round, because
    // "the change caused it" would suggest reverting something, and what was actually wrong is
    // an assertion that asked the system to guarantee an order it never had.
    let launched = String::from("desktop-shell: launched nxfiles into its own namespace");
    let themed = format!("nxfiles: theme font_px {shell_px}");
    session.expect_all(&[&launched, &themed])?;
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
    let files_win = next_geometry(session)?;

    // **Enter descends into the selected row**, which is the keyboard reaching the same message a
    // row press produces.
    //
    // **Arrowed to rather than assumed.** This used to press Enter on row 0 and its comment said
    // that would fail loudly "if `/home` ever holds a directory sorting before `papers`" — M14
    // Part D staged three, so it does. Each arrow is acknowledged by the browser's `selected`
    // receipt, which is both a per-keystroke acknowledgement and the assertion that the arrow
    // moved where this thinks it did.
    select_papers(qmp, session)?;
    press(qmp, "ret")?;
    session.expect("nxfiles: listed /home/papers - 0 entries")?;
    println!("  ok: Enter descended, and an empty directory lists nothing");

    // And Backspace leaves it again. Two navigations, both from the keyboard, because a
    // listing a person cannot drive without aiming at it is a listing they have to aim at.
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;
    println!("  ok: Backspace went back up");

    // **The location bar** (M14 Part D). `Ctrl+L` opens it on where the tab is, the path is
    // retyped, and `Enter` goes there — which is the whole chain: the chord reached the browser,
    // the keys went to the *field* rather than to the listing behind it, and what was typed became
    // a navigation.
    //
    // **A receipt per character**, the discipline every typed sequence in this gate follows.
    qmp.send_key("ctrl", true)?;
    press(qmp, "l")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: location so far 5 chars")?;
    println!("  ok: Ctrl+L opened the bar seeded with /home");

    // Clear the five it was seeded with. **Backspace here must not go up a directory**, which is
    // the property the emptying itself proves: a browser whose listing still had the keyboard
    // would navigate rather than edit, and the counts below would never arrive.
    for left in (0..5).rev() {
        press(qmp, "backspace")?;
        session.expect(&format!("nxfiles: location so far {left} chars"))?;
    }
    for (i, c) in "/home/papers".chars().enumerate() {
        let qcode = match c {
            '/' => String::from("slash"),
            other => other.to_string(),
        };
        press(qmp, &qcode)?;
        session.expect(&format!("nxfiles: location so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    // **The bar closes before the listing arrives, and the order is not a choice.** The receipt
    // is emitted from the render pass, which runs a full turn of the loop before the pending
    // navigation is taken — so `closed` is on the wire first. `Session::expect` consumes what it
    // scans past, so waiting for the listing first eats the close and then hangs on it; the first
    // version of this step did exactly that.
    session.expect("nxfiles: location bar closed")?;
    session.expect("nxfiles: listed /home/papers - ")?;
    println!("  ok: and what was typed into it became a navigation");

    // Back to `/home`, which is where the steps below expect to be.
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    // **Properties** (M14 Part D). `Ctrl+I` on the selected row, then `Esc`.
    //
    // **What only a boot can say is that it *drew*.** A dialog whose tree is undiffable opens,
    // reports its size and never paints a frame — the failure M14 Part C spent a boot on — and
    // from outside that is indistinguishable from one that opened and closed. So both ends are
    // asserted, and the slice between them is checked for the browser's own complaint.
    let before_props = session.transcript().len();
    qmp.send_key("ctrl", true)?;
    press(qmp, "i")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: showing properties")?;
    // **Pressed once the dialog has the keyboard, not once it was asked for.** `showing` is
    // printed before the window exists, and while the shell is still placing it a key goes to
    // the browser — an `Esc` there does nothing, and the dialog never closes. A probe that held
    // the shell's placement for half a second failed this step exactly that way.
    session.expect("nxfiles: properties has the keyboard")?;
    press(qmp, "esc")?;
    session.expect("nxfiles: properties closed")?;
    let props_window = session.transcript()[before_props..].to_string();
    if props_window.contains("could not be drawn") || props_window.contains("could not open") {
        let _ = session.child.kill();
        return Err(format!("the properties dialog opened and complained: {props_window:?}").into());
    }
    println!("  ok: Ctrl+I showed the properties dialog, and Esc closed it");

    // **And the margin around it** (M15 Part E), which moved every sidebar row down by its height
    // without moving a listing row at all. The design's sidebar runs to the window's edge, so
    // since the refresh's Part I that margin is nought.
    const SIDEBAR_PAD: i32 = browser::SIDEBAR_PAD;
    // **No tab strip while there is one tab** (desktop refresh, Part I): the browser leaves it
    // out rather than drawing an empty one, so every row moved up by its height — and **a column
    // header sits above the rows now**, so they moved back down by its. `nxfiles::list_top` is
    // the browser's own version of this sum.
    // Where the window's content begins, below the chrome that spans its whole width.
    let content_top = files_win.2 + TITLE_BAR_H + MENU_BAR_H + PATH_H;
    let row_y = |row: i32| content_top + browser::HEADER_H + row * ROW_H + ROW_H / 2;
    // **A sidebar row is not a listing row's neighbour any more** (desktop refresh, Part I): the
    // column header sits above the *listing* and the sidebar starts at the content's top, so the
    // two are a header apart. This was `row_y(row) + SIDEBAR_PAD`, which after the header landed
    // a whole row low — the gate pressed `Downloads` while asking for `Documents`, and said so.
    // `SIDEBAR_PAD` is nought since the same part, the design's sidebar running to the edge.
    let side_y = |row: i32| content_top + SIDEBAR_PAD + row * ROW_H + ROW_H / 2;

    // **The sidebar** (M14 Part D). One press on *Documents* goes there — one, not two: a sidebar
    // row has nothing to select and no second verb, so the listing's double click would be a rule
    // copied past its reason.
    //
    // **This is also what keeps two spellings of the folder names in step.** `libfs` names them
    // in `HOME_FOLDERS` and the image build stages them from this crate's `HOME_FOLDERS`, and this
    // crate does not link a guest library to compare them — so a folder staged under another name
    // answers "no such directory" here rather than differing silently.
    //
    // Sidebar rows are Home, the three folders, then Root; they sit `SIDEBAR_PAD` below the
    // listing rows beside them, because the panel is inset from the content it shares an edge
    // with while the strips above span the full width.
    click_at(qmp, session, files_win.1 + 40, side_y(1))?;
    session.expect("nxfiles: listed /home/Documents - ")?;
    println!("  ok: one press on a sidebar row went there");
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    // **Cut, copy and paste of files** (M14 Part D), which is the file-clipboard deferral built: a
    // path crosses `/dev/clipboard` between two directories and the **serial** side reads the
    // result back. Asserted through the filesystem rather than through a log line, for step 9d's
    // reason — a paste that acted on the wrong path makes a differently-named file rather than a
    // matching count.
    //
    // Row 4 is `theme.toml`, the first file after the three directories.
    click_at(qmp, session, files_win.1 + SIDEBAR_W + 120, row_y(4))?;
    session.expect("nxfiles: selected theme.toml")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "c")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: clipboard now holds 1 path(s) to copy")?;
    println!("  ok: Ctrl+C put a path on the clipboard");

    // Into an empty directory, by the sidebar, and paste there.
    click_at(qmp, session, files_win.1 + 40, side_y(1))?;
    session.expect("nxfiles: listed /home/Documents - 0 entries")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "v")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: copied /home/Documents/theme.toml")?;
    session.expect("nxfiles: listed /home/Documents - 1 entries")?;
    println!("  ok: and Ctrl+V copied it into another directory");

    // **The serial column is what says it happened**, because asking the browser would be asking
    // the program that claims to have done it.
    session.send("list ./Documents")?;
    session.expect("theme.toml")?;
    session.expect("/home>")?;
    println!("  ok: and the shell sees the file the paste made");

    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    // **A single click selects, a double click opens** (M14 Part D, decision 5).
    //
    // **This is the only place the interaction exists.** The gate navigates with `Enter` and drags
    // with a press-and-move, so neither touches what a *click* means; the host tests pin the
    // counting and the select-versus-open split, and what they cannot say is that a real pointer's
    // two presses arrive close enough together in the guest to be one run.
    //
    // **The single click aims at a *file*, and that is the assertion.** `/home` lists `papers`
    // first because directories lead, then `theme.toml` and `wallpaper.png` — so row 1 is a file,
    // and before this part clicking it asked the shell to open it. The receipt is change-driven,
    // which is why the row cannot be row 0: a fresh listing selects that one already, so a click
    // on it changes nothing and reports nothing.
    // Rows: `Documents`, `Downloads`, `Pictures`, `papers`, then `theme.toml` and
    // `wallpaper.png` — directories lead, so row 4 is the first file.
    let before = session.transcript().len();
    let file_row = (files_win.1 + SIDEBAR_W + 120, row_y(4));
    click_at(qmp, session, file_row.0, file_row.1)?;
    session.expect("nxfiles: selected theme.toml")?;
    println!("  ok: one click on a file selected it");

    // **And did not open it**, which `expect` cannot say on its own: it scans forward, so the very
    // line that would prove failure is one it would skip. The transcript since before the click is
    // checked for the request instead.
    let window = session.transcript()[before..].to_string();
    if window.contains("nxfiles: asked to open") {
        let _ = session.child.kill();
        return Err(format!(
            "a single click on a file must select it, not open it — the transcript since the \
             click holds an open request. {window:?}"
        )
        .into());
    }
    println!("  ok: and did not ask the shell to open it");

    // Two clicks on the directory row descend into it.
    let dir_row = (files_win.1 + SIDEBAR_W + 120, row_y(3));
    click_at(qmp, session, dir_row.0, dir_row.1)?;
    session.expect("nxfiles: selected papers")?;
    click_at(qmp, session, dir_row.0, dir_row.1)?;
    session.expect("nxfiles: listed /home/papers - ")?;
    println!("  ok: and a second click on a directory descended");

    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    st.files_win = files_win;
    Ok(())
}

/// `check-login`'s steps 8 to 9: the editor opened by the browser, undo and find, and a file
/// dropped onto it.
fn login_editor(qmp: &mut Qmp, session: &mut Session, size: DisplaySize, st: &mut LoginState) -> R<()> {
    let files_win = st.files_win;
    let work = st.work;
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

    // **The selection is on `Documents` again**, because the `Backspace` above re-listed `/home`
    // and a fresh listing selects row 0.
    select_papers(qmp, session)?;
    press(qmp, "ret")?;
    session.expect("nxfiles: listed /home/papers - 1 entries")?;

    // **Enter on a file row asks the shell to open it**, which is Part B's box, moved here
    // because it needed something to launch. The browser holds no authority to spawn anything —
    // it names a path over `/dev/desktop` and the shell decides what opens it.
    press(qmp, "ret")?;
    session.expect("desktop-shell: launched nxedit into its own namespace")?;
    // **Two processes reacting to one launch, so no order between them**: the browser learns
    // its request was taken when the shell's reply arrives, and the editor reads the file when
    // the kernel gets round to running it.
    let asked = String::from("nxfiles: asked to open /home/papers/notes.txt");
    // **The language is on this line since M14 Part G**, and asserting it is what says the
    // shipped binary ran `syntax::for_name` at all. `.txt` is plain text, which is a supported
    // answer rather than a gap — the four languages this system contains are host-tested, and
    // what only a boot can add is that the dispatch happens in the binary that ships.
    let opened = String::from("nxedit: opened /home/papers/notes.txt - 0 bytes, highlighted as plain");
    session.expect_all(&[&asked, &opened])?;
    // After the open, for the reason the browser's is read after its listing: the editor reads
    // its file before it has a window to place.
    let edit_win = next_geometry(session)?;
    println!("  ok: a file row launched the editor on the file it names");

    for (i, c) in TYPED.chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxedit: buffer rev {}", i + 1))?;
    }
    println!("  ok: {} keystrokes reached the editor's buffer", TYPED.len());

    // Ctrl+S, and the editor says what it wrote. Seven bytes: six typed and the newline the
    // editor adds, because a last line without one reads as one line shorter than it looks.
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
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
        press(qmp, &qcode)?;
        session.expect("nxedit: buffer rev ")?;
    }

    // Ctrl+Z. The revision moves on an undo, which is what makes an undone buffer read as
    // modified — the editor has to ask before closing something that is no longer the file.
    qmp.send_key("ctrl", true)?;
    press(qmp, "z")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
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
    press(qmp, "y")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
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
    press(qmp, "f")?;
    qmp.send_key("ctrl", false)?;
    for (i, c) in "tro".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxedit: find so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    session.expect("nxedit: find hit at line 0")?;
    // Escape ends the mode. **Not asserted here, and this comment used to say it was** — nothing
    // types into this editor again: step 9 drags its title bar with the pointer, and the click
    // after that hands the keyboard to the browser, so a find field left open would change
    // nothing any later step can see (PR #269 review, worth fixing 3). What covers it is
    // `nxedit`'s own `ctrl_f_opens_a_find_field_and_enter_walks_the_matches`, which fails when
    // `Esc` stops closing a find field. The press is kept because leaving the editor in a mode
    // the rest of the run does not expect is worse than a keystroke nobody checks.
    press(qmp, "esc")?;
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
    let title_grab = (ex + ew as i32 / 2, ey + chrome::TITLE_Y);
    move_pointer_to(qmp, title_grab.0, title_grab.1)?;
    qmp.pointer = Some(title_grab);
    qmp.send_button("left", true)?;
    for _ in 0..6 {
        qmp.send_motion(120, 0)?;
    }
    session.expect("compositor: interactive move of window ")?;
    // To the right edge, which is where the shell registered a snap zone — **a screen's width of
    // travel, into the clamp, and paced**. Six more motions of 120 reached it from here on 1280 and
    // 1360 and stopped 176 px short on 1920, where the drop snapped nowhere; twenty-three unpaced
    // ones overran the input ring on 2560, and the dropped batch ended the gesture before the
    // release — the pacing the left-edge drag above already explains (Phase 5 Part E.4).
    for _ in 0..(size.w / 240 + 2) {
        qmp.send_motion(240, 0)?;
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    qmp.send_button("left", false)?;
    qmp.pointer = None;
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "desktop-shell: drop window {edit_id} to {},{} {}x{}",
            work.0 + (work.2 / 2) as i32,
            work.1,
            work.2 / 2,
            work.3
        ),
    )?;
    session.expect(&format!("nxedit: resized to {}x{}", work.2 / 2, work.3))?;
    println!("  ok: the editor snapped to the right half, clear of the browser");

    // A second file, so the drag carries something the editor is not already showing — dropping
    // the open file is deliberately a no-op, and a gate that did it would assert nothing.
    session.send("touch ./papers/other.txt")?;
    session.expect("/home>")?;
    // **A third file the browser must not show** (M14 Part D). A dot is a convention meaning
    // "not part of what this directory is for", and the assertion below is that `/home/papers`
    // holds three entries while the browser lists two — which is the only way to tell hiding
    // from a directory that happens to have two files in it.
    session.send("touch ./papers/.quiet.txt")?;
    session.expect("/home>")?;

    // **Click the browser to give it the keyboard**, then walk it out and back in so it lists
    // the file the serial side just made. A listing is read when something navigates; nothing
    // polls, deliberately.
    let (_, fx, fy, _fw, _fh) = files_win;
    // **On the path strip, not the title bar.** A press on a title bar is a `StartMove` — the
    // click would work and the window would not move, but the gate would be asserting against a
    // gesture it did not mean to make. The path text carries no handler and still raises the
    // window, because click-to-focus is the compositor's and not the toolkit's.
    // **On the path strip**, which is two strips down: a title bar and the menus. It was three
    // until the refresh's Part I, when the browser stopped drawing a tab strip for a single tab
    // — and an aim that still counted one landed on the *sidebar's* first place, so the "inert"
    // click navigated to `/home` and the `Backspace` after it went up to `/`. **This gate passed
    // locally anyway**, because that Backspace was lost on the way; CI's landed, and the step
    // walked into the wrong directory. `fy + 38` used to be the path and is the *menu bar* since
    // M12 Parts B and D — it still raised the window, because click-to-focus is the compositor's
    // and the bar's own background carries no handler, so this went on passing while meaning
    // something else. The same shape of mistake twice, which is why the sum lives in `browser`.
    // **And clear of the sidebar**, which is what turned a slip into a navigation: over the
    // places, a press one strip out goes somewhere; over the listing, the worst a slip can do is
    // select a row, because descending takes a second click. An inert click should be inert
    // under a mistake as well as under a correct sum.
    click_at(qmp, session, fx + SIDEBAR_W + 60, fy + 1 + TITLE_BAR_H + MENU_BAR_H + PATH_H / 2)?;
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;
    // **The third step to depend on which row a fresh listing selects**, and the one that never
    // said so: it walks out of `papers` and straight back in, which worked only while `papers`
    // was row 0. The two before it at least named the row.
    select_papers(qmp, session)?;
    press(qmp, "ret")?;
    // **The receipt says both numbers, and that is the assertion.** "2 entries" alone cannot tell
    // a filtered listing from a directory that happens to hold two; "(1 hidden)" is the browser
    // stating that it read three and is showing two.
    session.expect("nxfiles: listed /home/papers - 2 entries (1 hidden)")?;
    println!("  ok: three files in the directory, two of them listed");

    // **`Ctrl+H` shows them, and again hides them** (M14 Part D). The count is the assertion and
    // the *guest's* count is the only one that can fail here: the host tests already pin that a
    // dot is filtered, that the toggle asks for a listing and that the View menu's mark tracks
    // the flag — what none of them can say is that a real directory read through a real server
    // comes back with the entry in it.
    //
    // **Both directions, and the second is not tidying.** Hiding drops entries rather than
    // skipping them, so showing them again is a *listing* rather than a flag; a toggle that
    // worked once and not back would leave every later count in this gate one too high, which is
    // how it would be discovered — several hundred lines away, as a rename that seemed to add a
    // file.
    qmp.send_key("ctrl", true)?;
    press(qmp, "h")?;
    qmp.send_key("ctrl", false)?;
    // No suffix here: with nothing hidden, what was read and what is shown are the same number,
    // and the receipt says so by not mentioning a difference.
    session.expect("nxfiles: listed /home/papers - 3 entries")?;
    println!("  ok: Ctrl+H showed the hidden one");
    qmp.send_key("ctrl", true)?;
    press(qmp, "h")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: listed /home/papers - 2 entries (1 hidden)")?;
    println!("  ok: and hid it again, leaving the directory as the rest of this gate expects");

    // Row 1 is `other.txt`: the listing sorts directories first and then by name, and `notes`
    // sorts before `other`. The row's y is the window's origin plus its chrome — the title bar,
    // the menus, the path strip and the column header — plus a row and a half. **No tab strip**,
    // which the browser leaves out for a single tab (desktop refresh, Part I), and **a header**,
    // which it gained in the same part.
    let row1 = (fx + SIDEBAR_W + 120, fy + TITLE_BAR_H + MENU_BAR_H + PATH_H + browser::HEADER_H + ROW_H + ROW_H / 2);
    move_pointer_to(qmp, row1.0, row1.1)?;
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
    // **Asserted directly, not retried.** Like the menu click below, this was flaky for exactly
    // one reason — a repaint between the press and the release re-identified the row under the
    // pointer and `libui` dropped the gesture — and the retry that hid it is gone with the bug.
    // **In either order** (Phase 6 Part B's `check-login --usb`, under TCG): `nxfiles` says it is
    // dragging after it asks the compositor to start the drag, and the compositor says so when the
    // request arrives — two processes answering one gesture, ordered by nothing. Expected in turn,
    // the compositor's line came first once and the first `expect` scanned past it.
    session.expect_all(&["nxfiles: dragging other.txt", "compositor: drag from window "])?;
    // Into the editor's document area — below its title bar and status strip, and well inside
    // the half of the screen it now occupies.
    let onto = (work.0 + work.2 as i32 * 3 / 4, work.1 + work.3 as i32 / 2);
    while at != onto {
        let step = ((onto.0 - at.0).clamp(-100, 100), (onto.1 - at.1).clamp(-100, 100));
        qmp.send_motion(step.0, step.1)?;
        at = (at.0 + step.0, at.1 + step.1);
    }
    // **Letting go is what delivers it.** The highlight the compositor draws while the pointer
    // is over a window that takes the payload is pixels rather than a line — its logic is
    // host-tested in `compositor::input`, including the two cases that must show *nothing*.
    qmp.send_button("left", false)?;
    qmp.pointer = Some(onto);
    expect_after_pointer(qmp, session, &format!("compositor: drop win={edit_id} on=document"))?;
    session.expect("nxedit: drop of other.txt on the document")?;
    session.expect("nxedit: opened /home/papers/other.txt - 0 bytes")?;
    println!("  ok: a file dragged from the browser opened in the editor");

    st.edit_id = edit_id;
    st.fx = fx;
    st.fy = fy;
    Ok(())
}

/// `check-login`'s steps 9a2 to 9d: two buffers in one window, copy and paste across programs,
/// `with`, and a rename.
fn login_buffers_and_clipboard(qmp: &mut Qmp, session: &mut Session, st: &mut LoginState) -> R<()> {
    let fx = st.fx;
    let fy = st.fy;
    let work = st.work;
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
    // tab does not move when another opens. `libui::widget`'s constants are the source, pinned to
    // a real tree by `a_tab_selects_where_it_is_and_its_close_box_does_not_select_it`; this gate
    // aims from `chrome`'s copies rather than from the toolkit (M11 decision 2), and
    // `the_gates_chrome_table_is_the_toolkits` is what keeps those copies equal to it.
    //
    // **A tab strip in both applications** (M12 Part D): above the editor's text and below the
    // browser's menus, which moved every one of the browser's rows down again.
    // `nxfiles::list_top` is that application's own version of the same sum.
    //
    // The editor is the work area's right half after the snap above.
    let ed = (work.0 + (work.2 / 2) as i32, work.1);
    // Tab `i`'s label area: the strip's own left padding, `TAB_PITCH` per tab, and a third of a
    // tab into the label — clear of the close box, whose centre is at `TAB_CLOSE_CX` (110).
    //
    // **Down the tab rather than down the strip** since Part H: the strip is 30 and a tab is the
    // 24 below its 5 of padding, so half the strip is still inside a tab but half a tab is the
    // honest aim.
    //
    // **Below the editor's menu bar since M14 Part A**, which put one above the strip to match
    // the browser. The strip did not move relative to anything it contains; the whole of it moved
    // down by a bar, and a gate that aimed at the old y clicked the menus instead — which is how
    // this failed, silently, as a tab that never became current.
    let tab = |i: i32| {
        (
            ed.0 + chrome::CONTENT_X + chrome::TAB_SIDE + chrome::TAB_PITCH * i + chrome::TAB_W / 3,
            ed.1 + 1 + TITLE_BAR_H + MENU_BAR_H + chrome::TAB_TOP + chrome::TAB_H / 2,
        )
    };
    let tab0 = tab(0);
    click_at(qmp, session, tab0.0, tab0.1)?;
    session.expect("nxedit: tab ")?;
    let switched = session.rest_of_line()?;
    if !switched.contains("notes.txt") {
        return Err(format!("clicking the first tab should show notes.txt; got {switched:?}").into());
    }
    println!("  ok: the editor holds two buffers, and a tab click switched between them");

    // **One process, two top-level windows** (M14 Part B) — the thing the protocol has always
    // allowed and nothing had done. The shell's window list is where it shows, which is why the
    // assertion is made there rather than on a receipt the editor prints about itself: a client
    // can say anything, and what matters is that the *manager* sees two windows it can name.
    //
    // `Ctrl+Shift+N`, with Shift because `Ctrl+N` is "new" in the singular everywhere and an
    // editor's singular is a buffer.
    //
    // The shell logs the placement only after the compositor answers it, by which time the window
    // is configured and the editor may already have said so: the two lines come in either order.
    let from = session.transcript().len();
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(qmp, "n")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    let any = |_: &str| true;
    let wait = std::time::Duration::from_secs(45);
    let placed = session.line_since(from, "desktop-shell: placed window ", any, wait)?;
    session.line_since(from, "nxedit: opened another window", any, wait)?;
    println!("  ok: a second top-level window from one process: {}", placed.trim());

    // **And it is a window in its own right**, not a decoration: the taskbar lists it, which is
    // the shell's own account of what exists rather than the editor's.
    let listing = session.transcript();
    let last = last_window_list(&listing).unwrap_or_default().to_string();
    // **The entries, not the number.** The number after `of ` is `desktops.len()`, which this
    // gate switched to 2 several steps ago and which is satisfied whatever the taskbar holds — so
    // reading it made this assertion decoration that could not fail (PR #283 review, blocking 3).
    // The windows are the bracketed `[id:label]` items.
    let count = last.matches('[').count();
    if count < 3 {
        // **The transcript, kept**: the list says what the shell showed and not why, and this
        // failed once in seventeen runs under KVM with nothing kept to read (Phase 6 Part B.7's
        // gate round, 2026-10-13).
        let _ = fs::write(build_cache().join("guest-transcript-check-login.log"), session.transcript());
        let _ = session.child.kill();
        return Err(format!(
            "after New Window the shell lists {count} window(s) on this desktop — the browser, \
             the editor and the editor's second window make three. The list read {last:?}"
        )
        .into());
    }
    println!("  ok: the shell's window list counts it too ({count} windows)");

    // **Closing one window must not take the others with it**, which is the whole of why the
    // conversion had to be uniform — and was broken anyway until PR #283's review: an `exit(0)`
    // left inside the per-window loop ran against *each* window's `App`, so closing either one
    // ended the process and discarded whatever the other held. Nothing caught it because this
    // gate had never closed a second window.
    //
    // `Ctrl+W` on the new window: it has one tab, and closing the last tab closes the window.
    //
    // **Read from where the key was pressed, in either order.** The editor's receipt and the
    // shell's list are two processes answering one event, and nothing orders them: this waited
    // for the receipt and then for a list line, and when the shell spoke first the list was
    // consumed on the way to the receipt and the wait ran out (PR #353's CI). A bare
    // `expect_all` would not do either — the list line from opening the window is still unread,
    // and would answer for the close.
    let new_id = placed.split_whitespace().next().unwrap_or("").to_string();
    let from = session.transcript().len();
    qmp.send_key("ctrl", true)?;
    press(qmp, "w")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: closed a window")?;
    // …and the editor is still running, which is the half that would have been lost: **a list
    // made after the key, without the closed window and still with the first.**
    let gone = format!("[{new_id}:");
    session.line_since(
        from,
        "desktop-shell: window list on cli of ",
        |l| !l.contains(&gone) && l.contains("notes.txt"),
        std::time::Duration::from_secs(30),
    )?;
    println!("  ok: closing one window left the other open");

    // **Give the first window the keyboard back**, because the new one took it — which is right,
    // and is exactly what makes the rest of this gate's typing land in the wrong place otherwise.
    // The editor was snapped to the right half earlier and the new window was placed at the
    // top-left, so they do not overlap and a click in the text area is unambiguous.
    let back = (ed.0 + 300, ed.1 + 400);
    click_at(qmp, session, back.0, back.1)?;
    println!("  ok: and clicking the first window took the keyboard back");

    // **`End` first, and that is not tidying.** Step 8b's search left its match *selected* —
    // which is what a find is supposed to do — and typing replaces a selection, so a bare
    // keystroke here would have edited the middle of the file rather than appending to it. It
    // did, on the first run: `tro` became `c` and the save wrote seven bytes. The behaviour is
    // right and the gate's assumption was not.
    press(qmp, "end")?;
    // **And the save goes to the tab that is current, not the one opened last.** That is the
    // whole risk of tabs in an editor, and the assertion is made from outside: the serial shell
    // reads the file back.
    press(qmp, "c")?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
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

    // **The file chooser** (M14 Part C). `Ctrl+Shift+S` on a buffer that has a name, then a new
    // name typed and accepted — and the assertion is that the *save* went to the new path, not
    // that a dialog appeared.
    //
    // **After the ordinary save above, deliberately.** Save As changes what the buffer *is*, so
    // every later `Ctrl+S` follows it — which is the feature, and which broke the step above when
    // this ran before it. A gate that had to be reordered by the change is the change being
    // real. "Save As is not Save with a prompt" is the part's whole content:
    // a chooser that only picked a destination would write these bytes to `notes.txt` and leave
    // the tab, the marker and the next `Ctrl+S` all pointing at the old file.
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(qmp, "s")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    // **The receipt counts the rows**, which is the half a "a dialog opened" expectation cannot
    // see: an unreadable directory opens the chooser over an *empty* list deliberately, so a gate
    // that only waited for the window would pass for the case decision 3 is about — the
    // application listing and the widget rendering what it was handed. Two entries here:
    // `notes.txt` and the `other.txt` the serial side made at step 8.
    session.expect("nxedit: choosing a file in /home/papers - 2 entries")?;
    // **And then the keyboard.** The line above is said before the chooser's window is asked
    // for, so a backspace sent on it could reach the editor while the shell was still placing
    // the dialog — and delete a character of the *document*. That was this gate's flake:
    // `buffer rev 12` where `chooser name so far 8 chars` should have been.
    session.expect("nxedit: the chooser has the keyboard")?;
    // **A receipt per character**, which is the discipline every typed sequence here follows and
    // the reason `nxedit` grew the line: an unacknowledged burst is a dropped keystroke found as a
    // wrong filename several steps later, and under KVM the injected keys arrive bunched enough
    // for it to happen. The field is seeded with the buffer's own name — `notes.txt`, nine
    // characters — so this empties it rather than appending to it, and the count runs down.
    for left in (0..9).rev() {
        press(qmp, "backspace")?;
        session.expect(&format!("nxedit: chooser name so far {left} chars"))?;
    }
    for (i, c) in "asked".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxedit: chooser name so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    session.expect("nxedit: saved /home/papers/asked - ")?;
    println!("  ok: Save As wrote to the name it was given, not to the file it was opened as");

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
    press(qmp, "f")?;
    qmp.send_key("ctrl", false)?;
    for (i, c) in TYPED.chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxedit: find so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    session.expect("nxedit: find hit at line 0")?;
    // **Escape first, and that is the rule Part D's review settled rather than tidying.** A
    // chord that acts on the *buffer* stays the field's while a field is open, and a copy is
    // one — so `Ctrl+C` here would have gone to the find field and done nothing. (It did, on
    // this step's first run.) Escape closes the field and leaves the match selected: the
    // selection is the buffer's, not the field's.
    press(qmp, "esc")?;
    // Ctrl+C. **A count, never the text** — an editor's buffer is a person's document, and the
    // serial console is a log file. The count is exactly `TYPED`'s length, which is what says
    // the *selection* was copied rather than the line or the buffer.
    qmp.send_key("ctrl", true)?;
    press(qmp, "c")?;
    qmp.send_key("ctrl", false)?;
    session.expect(&format!("nxedit: copied {} bytes", TYPED.len()))?;
    println!("  ok: the editor copied its selection onto the ring");

    // A terminal to paste into. **Launched here rather than reusing step 6's**, which was
    // closed long before the editor existed — and closed again at the end of this step, so the
    // windows steps 11 and 12 drive are the ones they expect to find.
    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, "nxterm")?;
    // The terminal's lines and the shell's placement of its window come in either order.
    let launched = session.transcript().len();
    press(qmp, "ret")?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    session.expect("desktop-shell: applications menu closed")?;

    // Click into the new terminal's grid to give it the keyboard. **Its origin comes off the
    // shell's own placement line** — the cascade moves, and a gate that assumed an origin is
    // the bug M11 Part E batch 4 fixed. Its *size* cannot: the shell logs at most
    // `MAX_LOGGED_GEOMETRY` geometry lines per session and this run passed that long ago, which
    // is why step 11 hardcodes the editor's size too. A point a little inside the top-left is
    // in the grid whatever the size, and needs no second number.
    let any = |_: &str| true;
    let wait = std::time::Duration::from_secs(45);
    let placed = session.line_since(launched, "desktop-shell: placed window ", any, wait)?;
    let (_, tx, ty) =
        parse_placement(&placed).ok_or_else(|| format!("could not read the terminal's placement from {placed:?}"))?;
    // Below the title bar and the terminal's own menu bar, and well inside the frame.
    // **Wait for the shell before clicking, not after**, and both halves of that are load-bearing.
    //
    // *Wait*, because this step used to type into a terminal that was still starting: the press
    // receipt is the *compositor's* and says nothing about the client, so the eight characters of
    // `touch ./` and the chord behind them went out while the guest was opening a tty, attaching a
    // backend and drawing a window. The batch carrying them was dropped —
    // `compositor: input batch DROPPED (SYN_DROPPED)` — and the paste simply never happened. It
    // was a race this step had been winning: M14 Part C added receipts to the editor step above,
    // the phase of everything after it moved, and it began failing every run. Same shape as the
    // drag hand-off in Part B, which the tab strip exposed and which had been called a flake twice.
    //
    // *Before the click*, because `Session::expect` consumes what it scans past and the two event
    // sources interleave freely: the first attempt put these two lines after `click_at`, and on
    // the very next run the shell came up *before* the press receipt, so the click's own wait ate
    // them and the gate hung on a line already in the transcript. The window has been placed by
    // here, which is all a click needs.
    session.line_since(launched, "nxterm: hosting a shell", any, wait)?;
    session.line_since(launched, "nxsh: up", any, wait)?;
    click_at(qmp, session, tx + 100, ty + TITLE_BAR_H + MENU_BAR_H + 40)?;

    // `touch ./` — typed — then the paste, then a suffix. **The suffix is what makes the
    // assertion discriminating**: `nitrox.clip` can only exist if the paste delivered exactly
    // `nitrox` *and* landed at the cursor with typing continuing after it. A paste that
    // delivered nothing leaves `.clip`; one that delivered the whole line leaves something
    // else again.
    // QMP's own key names, not the characters: `.` is `dot` and `/` is `slash`.
    //
    // **Paced, before every key including the first** — `type_at_terminal`'s rule, for its reason:
    // a release image renders no grid line to wait on per character, and a batch injected while
    // the compositor recomposes is dropped whole (`input batch DROPPED`). Waiting for `nxsh: up`
    // above closed the race at 1280×800 and 1360×768; `check-resolutions` lost the whole of
    // `touch ./` to it at 1024×768, where the window list's redraw lands in the same moment
    // (Phase 5 Part E.4).
    const PER_KEY: std::time::Duration = std::time::Duration::from_millis(40);
    for qcode in ["t", "o", "u", "c", "h", "spc", "dot", "slash"] {
        std::thread::sleep(PER_KEY);
        press(qmp, qcode)?;
    }
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(qmp, "v")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    session.expect(&format!("nxterm: pasted {} bytes", TYPED.len()))?;
    for qcode in ["dot", "c", "l", "i", "p"] {
        std::thread::sleep(PER_KEY);
        press(qmp, qcode)?;
    }
    press(qmp, "ret")?;

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
        if session.expect_within(&format!("{TYPED}.clip"), std::time::Duration::from_secs(3))? {
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

    // 9a2. **`with`, from a terminal the Applications menu opened** (administration Part A). The
    //      whole chain the serial gate cannot see: `desktop-shell` bound `/dev/views` into this
    //      terminal's namespace at the graphical session's base; `nxsh` handed `with` a sibling of
    //      the window's terminal; the password is typed into *this window* and read there. A
    //      release image does not narrate the grid, so what is asserted is the broker's audit on
    //      the console — started, and `nxinstall` exiting 0 because it saw the granted disks.
    type_at_terminal(qmp, "with admin nxinstall")?;
    session.expect("view: alice admin nxinstall — allowed, asking for a password")?;
    // No leading Enter here, unlike `type_at_terminal`: at a password prompt one would be an empty
    // password.
    type_answer(qmp, DEMO_PASSWORD)?;
    session.expect("view: alice admin nxinstall — started")?;
    session.expect("view: alice admin nxinstall — exited, code 0")?;
    println!("  ok: `with admin nxinstall` in a desktop terminal saw the granted disks");
    //      **And not the one in use** (administration Part C.6). Code 0 above says the grant bound
    //      something, which on a release boot is the ESP alone. The disk holding `init`'s root is
    //      `/dev/blk/0`, and naming it makes `nxinstall` log its refusal on the console, where a
    //      release image's gate can read it: the disk is not in the session at all. Until C.6
    //      this step passed on that disk too, which was the weaker claim.
    //      **Not asked, in the same terminal, inside five minutes** (the laptop polish's Part A):
    //      the password above opened a window for this window's terminal and `admin`, so the
    //      broker starts this without a prompt, and nothing is typed. `boot-probe` holds the
    //      window to its terminal and view, and the forgery control, on a test image.
    type_at_terminal(qmp, "with admin nxinstall /dev/blk/0")?;
    // **Refused as the running system** (administration Part G.2), which the installer now knows:
    // the tables say `init` mounted its root partition. Unordered with the audit line: that one
    // reaches the console through the logging service, the refusal straight from `nxinstall`.
    session.expect_all(&[
        "view: alice admin nxinstall — allowed, within the grace period",
        "nxinstall: refused /dev/blk/0: it holds the running system",
    ])?;
    session.expect("view: alice admin nxinstall — exited, code 1")?;
    println!("  ok: and it did not see /dev/blk/0, the disk holding init's root");

    // Close the terminal from inside, so steps 11 and 12 drive the windows step 9 left.
    for qcode in ["e", "x", "i", "t"] {
        press(qmp, qcode)?;
    }
    press(qmp, "ret")?;
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
    // **One click, asserted directly.** This was retried for a fortnight of CI runs while a
    // delivered click intermittently produced nothing; the cause was `libui` losing the gesture
    // when a repaint between the press and the release re-identified the widget under it, and it
    // is fixed. The retry is gone deliberately: it would hide the regression it was written for.
    click_at(qmp, session, file_menu.0, file_menu.1)?;
    session.expect("nxfiles: menu popup ")?;
    let popup = session.rest_of_line()?;
    let (_, px, py, _pw, ph) =
        parse_menu_popup(&popup).ok_or_else(|| format!("could not read the menu's placement from {popup:?}"))?;

    // **The row is found rather than computed** (M14 Part A). This used to divide the popup's
    // height by a row count, which was already careful about not deriving a row height from the
    // theme — `font_px` is a metric M11's decision 2 keeps out of a gate's assumptions. The File
    // menu now holds a **separator**, which occupies height without being a row, so no division
    // names the right one. The client says which row the pointer is over, so the gate walks down
    // the popup and stops when the guest agrees it is over `Rename`.
    // The popup's border and the space above its first row: one and five since the desktop
    // refresh's Part B (the design's `padding: 5px 0`), one and two before.
    const MENU_FRAME: i32 = 6;
    // `Rename` is `MENU_ROW_KEY + 8`: New Tab, Close Tab, a rule, New Window, Quit, a rule, New
    // File, New Folder, Rename. **It has moved twice** — once when the menu gained separators and
    // again when M14 Part B added the window rows — and each time the gate failed as a *prompt*
    // that never opened, several steps from the menu it was really about. The walk below at least
    // says which row it could not find.
    const RENAME_ROW: u64 = 109;
    let rx = px + 20;
    let mut ry = py + MENU_FRAME;
    let mut rename_at = None;
    // **Each step records where it went**, which is the exception the terminal gate's own walk
    // states: `move_pointer_to` walks a delta from the tracked position and does not update it,
    // so a loop that left it alone would re-walk the whole delta every time and run the pointer
    // off the popup. The receipt is what confirms the position; this is only what keeps the
    // steps four pixels apart.
    while ry < py + ph as i32 - MENU_FRAME {
        move_pointer_to(qmp, rx, ry)?;
        qmp.pointer = Some((rx, ry));
        // **The row the pointer settled on, not the first one it crossed** — see
        // `Session::settled_line`. The receipt lands on the frame after the motion; a step that
        // arrived nowhere new says nothing, and the next step is taken.
        let over = session.settled_line(
            "nxfiles: menu hover ",
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(400),
        )?;
        if over.as_deref() == Some(RENAME_ROW.to_string().as_str()) {
            println!("  ok: the pointer settled over row {RENAME_ROW}");
            rename_at = Some((rx, ry));
            break;
        }
        ry += 4;
    }
    let Some(rename_at) = rename_at else {
        return Err(format!(
            "the pointer walked {}..{} down the File menu and the guest never reported row \
             {RENAME_ROW} under it",
            py + MENU_FRAME,
            py + ph as i32 - MENU_FRAME
        )
        .into());
    };
    // The guest has just said the pointer is over the row, so the position is known by evidence.
    // `move_pointer_to` records nothing itself — injection is relative, and an unacknowledged
    // move leaves the host believing something it cannot check.
    qmp.pointer = Some(rename_at);
    click_at(qmp, session, rename_at.0, rename_at.1)?;
    // **Wait for the menu popup to be gone before typing**, which the desktop-rename step at
    // step 6 already does in its own way and this one did not.
    //
    // The thing holding the keyboard is not the prompt — the prompt is a keyboard *mode* inside
    // the browser's own window. It is the **menu popup**: a `popup` takes focus and is topmost,
    // so until `nxfiles` closes it every key goes there and a letter yields no message. A
    // character injected before the close is dropped, and the gate then times out on "name so far
    // 1 chars" with "0 chars" sitting in the transcript. Seen under TCG and then under KVM on
    // 2026-09-03 (PR #278 review, blocking 1 — the first fix waited for a receipt that `nxfiles`
    // printed *before* closing the menu, which narrowed the window without closing it).
    //
    // `nxfiles` now emits this after the close rather than before, so it means the destroy has
    // been sent — **and since 2026-09-22 that is the whole guarantee**, because the compositor
    // serves every queued request before it routes input. Before that, a busy compositor could
    // still route the key first, and this step timed out now and then in CI with the destroy
    // served a millisecond after the key.
    session.expect("nxfiles: name so far 0 chars")?;

    // **A receipt per character**, the discipline every typed sequence in this gate follows —
    // and `nxfiles` grew the line to make it possible, for the reason the launcher's filter did
    // one part earlier: an unacknowledged burst is a dropped keystroke discovered as a wrong
    // filename several steps later.
    for (i, c) in "renamed".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxfiles: name so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    session.expect("nxfiles: renamed /home/papers/renamed")?;
    // **Three, because Save As made one.** The count is here to say a rename is not a copy, and
    // the number it compares against is the directory's — so the step above that wrote
    // `/home/papers/asked` moved it, and the earlier listing at step 8 still reads 2. A gate whose
    // constants are the filesystem's state has to change when something writes to it; that is the
    // coupling working, and it is what caught this on the first run.
    session.expect("nxfiles: listed /home/papers - 3 entries")?;
    println!("  ok: the browser's File menu renamed a file");

    // **And the serial session reads it back**, which is what makes this a rename rather than a
    // browser that redrew a label. The content is step 8's, so a rename that had copied or
    // truncated shows up here as well.
    session.send("open ./papers/renamed")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: and the shell found the new name, with the old contents");

    Ok(())
}

/// `check-login`'s steps 10 to 13 and the transcript's last checks: dialogs, insisting on a
/// close, and the Places menu.
fn login_dialogs_and_places(mut qmp: Qmp, mut session: Session, size: DisplaySize, st: &mut LoginState) -> R<()> {
    let edit_id = st.edit_id;
    let first_term_id = st.first_term_id;
    let qmp_sock = st.qmp_sock.clone();
    let term_id = st.term_id;
    let work = st.work;
    // 10. **An editor launched from the menu, which is a launch with no file** (M11 Part E
    //     batch 7). `nxedit` required `argv[1]` and the Applications menu passes none, so it
    //     printed "no file to edit" and exited — reported as "nxedit doesn't launch from the
    //     menu", and true in the most literal way. It opens untitled now and asks for a name when
    //     there is something to save.
    press(&mut qmp, "esc")?;
    session.skip_to_end()?;
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(&mut qmp, &mut session, "nxedit")?;
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
    //     title bar's height and a list's row height — not because the crate cannot be linked
    //     (it is, for the display gate's reference frames) but because a gate that *aimed* from
    //     the toolkit could agree with a toolkit that had stopped drawing where it says (M11
    //     decision 2). A host test compares the two sets of numbers; a gate reads only its own. `libui::widget::DIALOG_LEFT_CX` and its siblings are the source — they
    //     were `nxedit`'s until M12 Part B moved them down beside `dialog_frame`, when a second
    //     confirmation would otherwise have given this gate two tables to keep in step — and
    //     `libui::widget::tests::dialog_buttons_land_where_the_constants_say` is the host test
    //     that pins these numbers to a tree that is actually built, so a change to `DIALOG_PAD`
    //     fails there, beside the change, rather than here after a three-minute boot.
    //
    //     (This paragraph named two symbols that same part deleted — PR #268 review, worth
    //     fixing 2 — which is the drift the move was made to prevent, in the one place a
    //     compiler cannot see.)
    const CONFIRM_W: i32 = chrome::DIALOG_W;
    const CONFIRM_H: i32 = chrome::DIALOG_H;
    // **Moved by the desktop refresh's Part B** from (91, 249, 103): the dialog's content runs
    // flush to its border now, three pixels wider each side and three lower. `libui`'s pin moved
    // with them, in the same change.
    const CONFIRM_DISCARD_CX: i32 = chrome::DIALOG_LEFT_CX;
    const CONFIRM_KEEP_CX: i32 = chrome::DIALOG_RIGHT_CX;
    const CONFIRM_BUTTON_CY: i32 = chrome::DIALOG_BUTTON_CY;
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
    let close_at = (untitled_right - chrome::CLOSE_X, untitled_y + chrome::TITLE_Y);
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
    let want_x = (untitled_x + (EDITOR_W - CONFIRM_W) / 2).clamp(work.0, work.0 + work.2 as i32 - CONFIRM_W);
    let want_y = (untitled_y + (EDITOR_H - CONFIRM_H) / 2).clamp(work.1, work.1 + work.3 as i32 - CONFIRM_H);
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
        .ok_or_else(|| format!("could not read the second dialog placement from {placed_again:?}"))?;
    let from = session.transcript().len();
    click_at(&mut qmp, &mut session, dx2 + CONFIRM_DISCARD_CX, dy2 + CONFIRM_BUTTON_CY)?;
    session.expect("nxedit: discarding the unsaved buffer")?;
    session.expect("nxedit: closing")?;
    // **The list the destroy produces.** Step 12 needs a slot out of it, and asking for a list line
    // later would wait for a change that nothing is going to make.
    //
    // **The list that reflects the destroy, not merely the next one.** The bar is redrawn for
    // several reasons and two of them fire here in order: the dialog goes first, which hands the
    // keyboard back to the editor and marks the list dirty while the editor is still in it, and
    // only then does the window go. **And read from the click, not from the receipt**: `nxedit`
    // destroys its window *before* it says it is closing, so the shell's list can come first, and
    // waiting for it after the receipt waited for a line already passed (PR #353's CI, where the
    // same order closed the editor's second window).
    let after_close = session.line_since(
        from,
        "desktop-shell: window list on ",
        |rest| taskbar_slot(rest, untitled_id).is_none(),
        std::time::Duration::from_secs(45),
    )?;
    let slot = taskbar_slot(&after_close, edit_id).ok_or_else(|| {
        format!("window {edit_id} is not in the taskbar list {after_close:?}, so it cannot be clicked")
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
    // have been closed since the last time anything here counted, so aiming by id would land on
    // somebody else's entry.
    let entry = size.task_click(slot);

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

    // 13. **The Places menu** (desktop refresh, Part C): the top bar's second word, and a place
    //     opened from it is the file browser started *there* — the argument `nxfiles` learned
    //     for this — rather than at its home.
    //
    //     **Last, because it leaves a browser on screen** that nothing after it has to account
    //     for. Both aims are pinned against the bar's and the menu's layout by
    //     `desktop_shell::panel`'s own test at both text sizes, as `APPS_CLICK` and `ROW1` are.
    //     In the staged 12 px the Places word is x 111 to 167 and the menu hangs at (119, 32)
    //     with `Documents` its second row, y 62 to 86; at the built-in 13 they are 116 to 175,
    //     (124, 32) and 64 to 90 — the word moves because `Large` widens `Applications` beside
    //     it. **The margin is asserted rather than described**: the word aim sits at least
    //     `AIM_MARGIN` inside the word at either size, which that test checks, because the
    //     figures written here went a size-step stale the moment the top bar changed and the
    //     sentence claiming a comfortable aim outlived them (PR #316 review, finding 1).
    const PLACES_CLICK: (i32, i32) = (141, 12);
    const PLACE_DOCUMENTS: (i32, i32) = (180, 75);
    click_at(&mut qmp, &mut session, PLACES_CLICK.0, PLACES_CLICK.1)?;
    session.expect("desktop-shell: places menu open")?;
    click_at(&mut qmp, &mut session, PLACE_DOCUMENTS.0, PLACE_DOCUMENTS.1)?;
    // **The shell's own lines, in the order it prints them**: the launch, then the menu going.
    // What the browser did is another process's output and is checked against the whole
    // transcript below — the rule 5 states for `nxterm`.
    session.expect("desktop-shell: opening place Documents")?;
    session.expect("desktop-shell: launched nxfiles into its own namespace")?;
    session.expect("desktop-shell: places menu closed")?;
    // And its window arrived — the receipt that the browser ran far enough to draw, which is
    // what makes the transcript checks below about a browser that started rather than one that
    // might yet.
    session.expect("desktop-shell: placed window ")?;
    println!("  ok: a place opened the file browser");

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
    // **The menu was dismissed by the compositor, not by a focus change** (M11 Part E batch 5).
    // The distinction is the whole of the fix: clicking another *window* raises it, and a raise
    // is a focus change the popup hears about — which is why the first version of this step
    // passed while the reported bug survived. A press on a panel raises nothing.
    if !transcript.contains("compositor: dismissed win=") {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the Applications menu closed, but the compositor never sent a dismissal — so it \
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
    // **The browser started in the place, and listed it** (step 13) — the listing searched for
    // only *after* the start, because this gate has the browser list `/home/Documents` from its
    // sidebar long before, and a whole-transcript search passed a browser that logged its argument
    // and then navigated home (PR #314 review, blocking 2). Neither line can see the sidebar, which
    // is why `nxfiles` keeps `HOME` as its home whatever it starts at, and says so where it decides.
    if let Err(why) = started_and_listed(&transcript, "/home/Documents") {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the Places menu launched the file browser, but {why}: `nxfiles` must start in the \
             place it is given as `argv[1]` and list it, not its home.\n\nthe transcript is at {}",
            path.display()
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
fn cmd_check_terminal(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    let work = repo_root().join("tools/build-cache");
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-terminal.sock");
    let _ = fs::remove_file(&qmp_sock);
    // QEMU's trace of the PS/2 keyboard's lights (Phase 6 Part B.5) — see [`ps2_lights`].
    let led_log = work.join("terminal-leds.log");
    let _ = fs::remove_file(&led_log);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
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
        .arg("-trace")
        .arg("ps2_set_ledstate")
        .arg("-trace")
        .arg("ps2_reset_keyboard")
        .arg("-D")
        .arg(&led_log)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    println!("xtask: terminal gate — booting and typing…\n");
    let mut session = Session::spawn(cmd, "check-terminal")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;
    qmp.screen = Some(size);

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
        let (path, rest) = line
            .trim()
            .split_once(", cell ")
            .ok_or_else(|| format!("nxterm's grid line is not in the form this gate reads: {line:?}"))?;
        let (dims, px) = rest.split_once(" at ").ok_or_else(|| format!("nxterm's grid line has no size: {line:?}"))?;
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

    // **Num Lock's light is on before anything is injected** (Phase 6 Part B.5): the compositor
    // sends the lights when it connects, and Num Lock is on from the start. Asserted here, ahead of
    // the first key and the first motion, because the compositor also sends after every input pass
    // whose locks differ from what it last sent — so a compositor that skipped the first send would
    // pass the same assertion made after any typing. The reset's own zero comes first.
    wait_ps2_lights(&led_log, &[0, 2], "the compositor's start")?;

    // **And the shell told the window where it is** (desktop refresh, Part K). `nxsh` writes
    // `OSC 7` beside every prompt, `libterm::parse` reads it and `nxterm` puts it in the title
    // bar as the design's subtitle. Three crates, each with host tests of its own that cannot
    // see the other two: the shell's test pins the bytes it writes, the parser's pins the bytes
    // it reads, and nothing on the host makes them the same bytes. This line is where they meet.
    session.expect("nxterm: shell is at /")?;
    println!("  ok: the shell announced its directory and the terminal read it");

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
    // was missing: a misplaced press is not a lost pixel but a permanent offset, so asserting the
    // position once turns one misplaced motion into a failed build.
    //
    // **Which is what happened, and the split assertion is why it can be said.** CI failed here
    // with the press at (495, 351) — exactly one step of (-98, -56) short of its aim. The
    // comment this replaces called the position "bit-identical over 40 loaded runs" and left the
    // gate's one prior failure unexplained. It was read on 2026-08-31 as a packet lost, and on
    // 2026-09-23, from QEMU's `ps2.c`, as a packet *held*: QEMU keeps what its sixteen-byte queue
    // cannot take and adds it to the next injected event, so the pin's over-drive rode on the
    // walk's first step and cancelled it. `move_pointer_to` drains the queue after its pin and
    // then flushes what QEMU held into the corner (`expect_after_pointer` has the source). The
    // assertion is unchanged and still exact — a press that lands anywhere else is still a
    // failure, and a systematic misplacement still fails all three attempts.
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

    // **A stage's *diagnostic* reaches the grid too** (Phase 5 Part H.1). Everything above
    // this is a program's `stdout` — a typed stream the shell captures and renders as the
    // pipeline's value. Diagnostics take the other path in design §1, and until now that
    // path ended nowhere a person could see: `Streams::stderr` was `None`, so a program fell
    // back to `kprint` — `SYS_DEBUG_KPRINT`, which reaches COM1 and nothing else. Under
    // QEMU a developer read them off the serial port and the hole was invisible; on the
    // laptop, which has no serial port, `nxinstall`'s entire first run printed nothing at
    // all (first install attempt, 2026-09-17).
    //
    // **This is the gate that can see it.** `check-install` drives a release image and
    // cannot read the grid; here `nxterm` narrates it, so the whole new path is asserted at
    // once: the stage sends on the shared `stderr`, the shell drains it while waiting, and
    // it lands in the grid as text. A failing `remove` is the cheapest diagnostic in the
    // tree, and a path that does not exist is a message no successful command would print.
    type_at_terminal(&mut qmp, "remove /nothing-here")?;
    session.expect("nxterm: grid> remove: ")?;
    println!("  ok: a stage's diagnostic rendered in the grid, not on the kernel log");

    // **And `nxsh --help` reaches the terminal** (the laptop polish's Part E). A shell running a
    // script is handed a terminal and set it aside, so its usage went to `kprint`: serial showed
    // it, and a person at a window saw nothing. The grid is narrated only from what reaches the
    // terminal, and the typed line does not hold the usage.
    type_at_terminal(&mut qmp, "nxsh --help")?;
    session.expect("nxterm: grid> usage: nxsh [SCRIPT.nx | -c SOURCE]")?;
    println!("  ok: `nxsh --help` rendered in the grid, not on the kernel log");

    // **And it follows the shell.** The line above proves the announcement arrives once; this
    // proves it is the shell's *current* answer rather than a value read at startup — which is
    // the whole point of putting it in a title bar, and which a terminal that announced only
    // its first prompt would pass the first assertion for. `cd` is a builtin, so nothing but
    // the shell's own state changes here.
    //
    // That the line is a *change* rather than one per prompt is
    // `a_repeated_announcement_is_not_a_change` in `nxterm`, on the host: `expect` scans
    // forward past whatever it does not match, so a gate cannot assert the absence of a line.
    type_at_terminal(&mut qmp, "cd /home")?;
    session.expect("nxterm: shell is at /home")?;
    println!("  ok: the terminal followed the shell into another directory");

    // **The locks, the keypad and the lights** (Phase 6 Part B.5). Nothing typed so far has
    // changed a lock, so nothing typed has set the lights again.
    wait_ps2_lights(&led_log, &[0, 2], "the typing so far")?;
    // One key at a time, each waited for as the loop above does, building the line it should show.
    let mut line = String::from("/home> ");
    let key = key_echoed;
    // **Caps Lock inverts Shift for letters, and for letters alone.** The quote is typed with it
    // on: a Caps Lock that inverted Shift for every key would type an apostrophe there. And a
    // shifted letter under it is lower case.
    press(&mut qmp, "caps_lock")?;
    wait_ps2_lights(&led_log, &[0, 2, 6], "Caps Lock on")?;
    key(&mut qmp, &mut session, &mut line, "apostrophe", true, "\"")?;
    key(&mut qmp, &mut session, &mut line, "a", false, "A")?;
    key(&mut qmp, &mut session, &mut line, "b", false, "B")?;
    key(&mut qmp, &mut session, &mut line, "a", true, "a")?;
    press(&mut qmp, "caps_lock")?;
    wait_ps2_lights(&led_log, &[0, 2, 6, 2], "Caps Lock off")?;
    key(&mut qmp, &mut session, &mut line, "c", false, "c")?;
    key(&mut qmp, &mut session, &mut line, "apostrophe", true, "\"")?;
    press(&mut qmp, "ret")?;
    // A string at the top level is shown bare, so this line is the shell's answer and cannot be
    // the typed line, which begins with the prompt.
    session.expect("nxterm: grid> ABac")?;
    println!("  ok: Caps Lock upper-cased the letters it was on for, and only the letters");
    // **The keypad, under Num Lock**: digits, an operator, and its own Enter — which the shell
    // takes as Enter, or there would be no answer.
    session.expect("nxterm: grid> /home>")?;
    line = String::from("/home> ");
    key(&mut qmp, &mut session, &mut line, "kp_1", false, "1")?;
    key(&mut qmp, &mut session, &mut line, "kp_2", false, "2")?;
    key(&mut qmp, &mut session, &mut line, "spc", false, " ")?;
    key(&mut qmp, &mut session, &mut line, "kp_add", false, "+")?;
    key(&mut qmp, &mut session, &mut line, "spc", false, " ")?;
    key(&mut qmp, &mut session, &mut line, "kp_3", false, "3")?;
    press(&mut qmp, "kp_enter")?;
    session.expect("nxterm: grid> 15")?;
    println!("  ok: the keypad typed digits and an operator, and its Enter ran the line");
    // **And with Num Lock off, the keypad is the keys printed under its digits**: 8 is Up, which
    // the line discipline answers by recalling the line before — under Num Lock it would be an 8.
    session.expect("nxterm: grid> /home>")?;
    press(&mut qmp, "num_lock")?;
    wait_ps2_lights(&led_log, &[0, 2, 6, 2, 0], "Num Lock off")?;
    press(&mut qmp, "kp_8")?;
    session.expect("nxterm: grid> /home> 12 + 3")?;
    println!("  ok: with Num Lock off, keypad 8 was Up and recalled the line before");
    press(&mut qmp, "num_lock")?;
    wait_ps2_lights(&led_log, &[0, 2, 6, 2, 0, 2], "Num Lock on again")?;
    press(&mut qmp, "kp_enter")?;
    session.expect("nxterm: grid> 15")?;

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
    // The press position is asserted before the effect, for the reason the click above states:
    // it separates "the pointer was not there" from "the pointer was there and nothing happened".
    //
    // **Hover before the click, and it has to be in that order** (M11 Part E batch 3). Hover is
    // the first thing in this system that reacts to the pointer without a button held, and it is
    // invisible to a gate: the highlight is pixels, and this boot has no reference render of a
    // menu to compare against. So the client says which row it is over, and the click activates
    // that one. Moving and clicking as one step does not show it: choosing closes the menu, and
    // the popup is destroyed before it would have painted itself hovered.
    //
    // **The row is found rather than computed** (M14 Part A). It used to be "the upper quarter",
    // which worked while the menu had two equal rows. It has five now — two of them with an
    // accelerator column, one of them a separator, and the first two greyed until something is
    // selected — so a fraction of the height names a different row depending on the theme's text
    // size. The hover receipt already says *which* row the pointer is over, so this walks down
    // the popup and stops when the guest says it is over the one it means to click.
    //
    // **`Clear`, which is `MENU_ROW_KEY + 6`.** Copy and Paste are above it — Copy is disabled
    // with nothing selected and would produce no message when clicked, which is correct behaviour
    // and useless as a proof that a click reached a row — and so are Select All, Find and Clear
    // Scrollback since M14 Part E, plus the separator.
    //
    // **It was `+ 3` and that row is now `Find`**, so the walk still found a row to hover and the
    // click chose the wrong one; the failure appeared as "never saw `menu chose Clear`" three
    // steps from the menu it was about. `nxterm::tests::the_gate_clicks_the_row_it_means` makes
    // this number wrong in a second instead of in a three-minute boot, which is the job the same
    // test has done twice for `nxfiles`.
    const CLEAR_ROW: u64 = 106;
    //
    // **Each step records where it went**, which is a deliberate exception to the rule that only
    // a *confirmed* press updates the tracked position. `move_pointer_to` walks a delta from that
    // position and does not update it, so a loop that left it alone would re-walk the whole delta
    // every iteration and run the pointer off the popup — which is what the first version of this
    // did. A dropped packet here costs one four-pixel step of drift, and the walk simply takes
    // one more; the receipt below is what actually confirms the position, so nothing downstream
    // trusts the estimate.
    let cx = px + pw as i32 / 2;
    let mut cy = py + 3;
    let mut found = None;
    while cy < py + ph as i32 - 3 {
        move_pointer_to(&mut qmp, cx, cy)?;
        qmp.pointer = Some((cx, cy));
        // **The row the pointer settled on, not the first one it crossed.** The first step here
        // comes from wherever the last click left the pointer, in 100-pixel packets, and the
        // guest reports every row it passes: after the desktop refresh's Part B moved this popup
        // four pixels, the approach's first packet landed on `Clear` itself, the walk took that
        // for arrival, and clicked the disabled `Copy` at the top. See `Session::settled_line`.
        let over = session.settled_line(
            "nxterm: menu hover ",
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(400),
        )?;
        if over.as_deref() == Some(CLEAR_ROW.to_string().as_str()) {
            println!("  ok: the pointer settled over row {CLEAR_ROW}");
            found = Some(cy);
            break;
        }
        cy += 4;
    }
    let Some(cy) = found else {
        // **Every line the popup said, because "never saw 103" alone does not distinguish the
        // ways this fails**: the pointer never reached the popup, it reached it and the row
        // numbering differs, or the popup could not draw at all. The first time this fired it
        // was the third — `nxterm: the menu popup could not be drawn`, twenty times over,
        // because the tree was not diffable — and a message that said only "never saw 103"
        // pointed at the walk rather than at the widget.
        let seen: Vec<String> =
            session.transcript().lines().filter(|l| l.contains("nxterm: menu")).map(str::to_string).collect();
        return Err(format!(
            "the pointer walked {}..{} down the menu popup and the guest never reported row \
             {CLEAR_ROW} under it; it reported: {}",
            py + 3,
            py + ph as i32 - 3,
            if seen.is_empty() { String::from("nothing") } else { seen.join(", ") }
        )
        .into());
    };
    // **The receipt is also the position proof**, which is why the tracked position is set here
    // rather than assumed. `move_pointer_to` deliberately does not record where it went — only a
    // *confirmed* press does, because injection is relative and an unacknowledged move leaves the
    // host believing something it cannot check. Here the guest has just said it is over the row,
    // so the position is known by evidence rather than by assertion.
    //
    // Skipping this cost an afternoon: the click below then walked its delta from the position
    // the *previous* click had confirmed, doubling the movement, landing at the corner, and
    // dismissing the menu — after which the retry pressed on the terminal underneath.
    qmp.pointer = Some((cx, cy));
    click_at(&mut qmp, &mut session, cx, cy)?;
    session.expect("nxterm: menu chose Clear")?;

    // **The keyboard half of an open menu**, which had no coverage until PR #280's review found
    // two blocking bugs living in it. Everything above this point is the pointer: a click opens,
    // a click chooses. Nothing had ever arrowed.
    //
    // The popup takes the keyboard while it is up, so these keys reach *it* and never the grid.
    //
    // **And the terminal has to have the keyboard back before F1 goes**, which is why this waits
    // for `focus=1`. Choosing closes the popup, and the compositor announces the terminal's focus
    // only once the popup has gone; F1 sent on `chose Clear` alone could arrive while the
    // closing popup still held the keyboard, and open nothing. It did, twice: once in the desktop
    // refresh's Part A under host load, and again in Part B without it, both transcripts ending
    // `chose Clear`, `focus=1`, and silence. The key's destination cannot be logged — console lines
    // never carry what was typed — so the evidence is the ordering: this is the transcript's own.
    session.expect("nxterm: focus=1")?;
    qmp.send_key("f1", true)?;
    qmp.send_key("f1", false)?;
    session.expect("nxterm: menu popup ")?;
    let reopened = session.rest_of_line()?;
    let Some((_, ex, _, _, eh)) = parse_popup_line(&reopened) else {
        let _ = session.child.kill();
        return Err(format!("could not read the reopened popup's geometry from {reopened:?}").into());
    };

    // **Left moves to the other menu, and the popup has to be rebuilt for it.** The window is
    // positioned under a bar word and sized for one menu's rows, both fixed at `Child::open` —
    // so a client that reconciled on "is a menu open" rather than "which one" would leave this
    // window where it is and draw the new menu's rows into the old menu's extent. The proof is a
    // *second* popup line naming a different word and a different height.
    qmp.send_key("left", true)?;
    qmp.send_key("left", false)?;
    session.expect("nxterm: menu popup ")?;
    let other = session.rest_of_line()?;
    let Some((_, fx2, _, _, fh)) = parse_popup_line(&other) else {
        let _ = session.child.kill();
        return Err(format!("could not read the File popup's geometry from {other:?}").into());
    };
    if fx2 == ex || fh == eh {
        let _ = session.child.kill();
        return Err(format!(
            "arrowing to the other menu did not rebuild the popup: Edit was {reopened:?}, \
             File is {other:?} — the window should move to File's word and shrink to its rows"
        )
        .into());
    }
    println!("  ok: arrowing rebuilt the popup under the other word: {other}");

    // Back to the menu with rows worth choosing, then down one and Enter.
    qmp.send_key("right", true)?;
    qmp.send_key("right", false)?;
    session.expect("nxterm: menu popup ")?;
    let _ = session.rest_of_line()?;
    // **One Down lands on Paste, not on Copy.** Copy is the first row and is greyed with nothing
    // selected, and arrowing skips what Enter could not act on — so this asserts the skip as well
    // as the movement. A gate that expected `Copy` here would be asserting the bug.
    qmp.send_key("down", true)?;
    qmp.send_key("down", false)?;
    qmp.send_key("ret", true)?;
    qmp.send_key("ret", false)?;
    session.expect("nxterm: menu chose Paste")?;
    println!("  ok: Down skipped the greyed row and Enter chose the next one");

    // **A second window of the terminal, with a shell of its own** (M14 Part B). The window is
    // the shell's proof as much as the tab's was: a second top-level that opened with nothing to
    // talk to would look identical from the outside, and this is the only place that difference
    // is visible.
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(&mut qmp, "n")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    // **The shell's line comes first, and structurally so**: `open_window` gives the new
    // window's first tab a tty and a shell before it returns, so the terminal cannot report a
    // window it has not finished building.
    // **The shell's line comes first, and structurally so**: `open_window` gives the new window's
    // first tab a tty and a shell before it returns, so the terminal cannot report a window it has
    // not finished building. **Nothing between the last two**, because the third comes from a
    // freshly spawned `nxsh` and its order against this process's own `kprint` is a race — the
    // rule this gate's own closing comment states (PR #283 review, optional 9).
    session.expect("nxterm: hosting a shell")?;
    session.expect("nxterm: opened another window")?;
    session.expect("nxsh: interactive shell")?;
    println!("  ok: Ctrl+Shift+N opened a second window with a shell of its own");

    // **A second tab is a second terminal**, which is the only part of M14 Part B a host test
    // cannot reach: the split of grids and outboxes is pinned in `nxterm`'s own tests, but that
    // a new tab is given its *own* tty and its *own* shell happens in the binary and needs a
    // boot. `Ctrl+Shift+T` — Shift because `Ctrl+T` belongs to whatever is running in the
    // terminal.
    //
    // The receipt is the shell's, not the tab's: a strip with two words in it would be drawn by
    // a terminal that opened a tab and gave it nothing to talk to, which is exactly the failure
    // worth catching.
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(&mut qmp, "t")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxterm: hosting a shell")?;
    session.expect("nxsh: interactive shell")?;
    println!("  ok: Ctrl+Shift+T opened a second tab with a shell of its own");

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
    // misattributed exit shows up as a restart of a service that never stopped. Nothing asks
    // it to stop any more: the 1.1 s demo stop sent `CTRL_OP_SHUTDOWN` to the *first*
    // declared service, which administration Part E.1a made `auth-service`, and it went.
    if text.contains("service-mgr: restarting 'heartbeat'") {
        return Err("service-mgr restarted 'heartbeat', which never exited — \
             boot-probe's exit was misattributed to it"
            .into());
    }
    println!("xtask: service-mgr attributed boot-probe's exit to boot-probe ✓");
    Ok(())
}


/// A **smoke gate, not a per-commit one**: it boots a full image and compares an image,
/// so the plan runs it once per display-arm change.
fn cmd_check_display(accel: Accel, size: DisplaySize) -> R<()> {
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
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
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
    // **Placed and then raised, in the order the comparison below composes them.** The stacking
    // used to be creation order, which raced `nxterm`'s window against these three; the client
    // now raises them itself, so `nxterm` only has to *exist* first.
    session.expect("ui-testclient: reference windows placed and stacked via /dev/draw/manage")?;
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
    // The compositor's `CURSOR_H` / `CURSOR_W`, which this crate cannot import — it depends on
    // `libdraw`, `libui` and `libterm`, not on `compositor`.
    for y in cy..(cy + 17).min(h) {
        for x in cx..(cx + 12).min(w) {
            let i = (y as usize * w as usize + x as usize) * 3;
            if (pixels[i], pixels[i + 1], pixels[i + 2]) == body {
                cursor_px += 1;
            }
        }
    }
    if cursor_px == 0 {
        let _ = session.child.kill();
        return Err(format!("no cursor at the screen centre ({cx},{cy}): the pointer sprite is not being drawn").into());
    }
    println!("  ok: cursor visible at ({cx},{cy}) — {cursor_px} body pixels");

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
    let ui = reference_frame(&faces, "ui")?;
    // The stacking the composition below assumes, stated rather than trusted: each window must sit
    // wholly inside the one beneath it, or a region the gate believes it is comparing is covered
    // by something it is not comparing against — a hole that would be silent.
    if !(sw <= tw && sh <= th && tw <= uw && th <= uh) {
        return Err(format!(
            "the reference windows are no longer nested: scene {sw}x{sh}, terminal {tw}x{th}, \
             toolkit {uw}x{uh}. `ui-testclient` creates them largest-first because windows stack \
             at the origin in creation order; fix the sizes or the order before the comparison \
             below can mean anything"
        )
        .into());
    }
    // **The expected screen is composed here, the way the guest composes it** (desktop refresh,
    // Part B). The three windows are rounded now, so each one's corners show what is beneath —
    // the scene's the terminal's, the terminal's the toolkit window's, and the toolkit window's
    // the desktop — and each casts its shadow over what is below and into its own corners. This
    // was built by hand until then: each render, the shadow of the window above painted onto it,
    // and the window above excluded. Rounding would have meant three more exclusions and a
    // hand-written blend per curve, so the pictures are stacked through `libdraw::compose`
    // instead, with the guest's own radius and shadow.
    //
    // **What is still computed independently is the part that matters**: the three pictures come
    // from `libdraw`'s scene, `libterm` and `libui`, and compose's rounding and shadowing are
    // held by their own unit tests. What this gate adds is that the guest applies them — a
    // compositor that forgot to round, or to shadow, fails here — and the binding: a wrong base,
    // stride or channel order.
    //
    // **The light scheme's shadow**, because nothing in a self-test boot says otherwise: there is
    // no session, so no shell sends `SetScheme`, and the compositor keeps the scheme it starts in.
    let shadow = libdraw::theme::window_shadow(libdraw::theme::Scheme::Light);
    let radius = libdraw::corner::WINDOW_RADIUS;
    let composed = {
        use libdraw::compose::SurfaceRef;
        let origin = libdraw::geom::Point::new(0, 0);
        // A function rather than a closure, so the borrow it returns can be named.
        fn at<'a>(
            fb: &'a libdraw::framebuffer::MemFramebuffer,
            origin: libdraw::geom::Point,
            shadow: libdraw::compose::Shadow,
            radius: u32,
        ) -> SurfaceRef<'a> {
            SurfaceRef::new(Framebuffer::geometry(fb), origin, fb.bytes()).with_shadow(shadow).with_corner(radius)
        }
        let mut out = libdraw::framebuffer::MemFramebuffer::new(libdraw::framebuffer::Geometry::packed(
            uw,
            uh,
            libdraw::format::PixelFormat::XRGB8888,
        ));
        // Bottom first, as `ui-testclient` created them — the nesting above is what makes this
        // the guest's order.
        let stack =
            [at(&ui, origin, shadow, radius), at(&term, origin, shadow, radius), at(&expected, origin, shadow, radius)];
        libdraw::compose::compose_full(&mut out, libdraw::scene::BACKGROUND, &stack);
        out
    };
    // **Except the bottom window's four corner squares**, and only those. The self-test image runs
    // `nxterm` as a service, and with no manager its window sits at the origin *beneath* these
    // three — invisible while they were square, because the toolkit window covered its rectangle
    // whole. Rounded, the toolkit window's corners show `nxterm`'s pixels and its shadow, which
    // this gate does not render. Everywhere else the toolkit window is opaque over whatever is
    // under it, so nothing else can reach the comparison; the scene's and the terminal's corners
    // show only windows this gate *does* render, and those are compared exactly — which is what
    // proves the guest rounds at all. The first run of the rounded gate failed on exactly these
    // pixels, first at the shared corner (0,0).
    let unknown_beneath = move |x: u32, y: u32| (x < radius || x >= uw - radius) && (y < radius || y >= uh - radius);
    // One pass, counted per window so a failure still says which picture it was in.
    let region = |x: u32, y: u32| {
        if x < sw && y < sh {
            0
        } else if x < tw && y < th {
            1
        } else {
            2
        }
    };
    let mut miss = [0usize; 3];
    let mut compared = [0usize; 3];
    type FirstMiss = (u32, u32, (u8, u8, u8), (u8, u8, u8));
    let mut firsts: [Option<FirstMiss>; 3] = [None; 3];
    if w >= uw && h >= uh {
        for y in 0..uh {
            for x in 0..uw {
                if unknown_beneath(x, y) {
                    continue;
                }
                let k = region(x, y);
                compared[k] += 1;
                let want = Framebuffer::get_pixel(&composed, x, y).unwrap_or_default();
                let i = (y as usize * w as usize + x as usize) * 3;
                let got = (pixels[i], pixels[i + 1], pixels[i + 2]);
                if got != (want.r, want.g, want.b) {
                    miss[k] += 1;
                    if firsts[k].is_none() {
                        firsts[k] = Some((x, y, got, (want.r, want.g, want.b)));
                    }
                }
            }
        }
    }
    let (mismatches, term_mismatches, ui_mismatches) = (miss[0], miss[1], miss[2]);
    let (first, term_first, ui_first) = (firsts[0], firsts[1], firsts[2]);
    let (term_compared, ui_compared) = (compared[1], compared[2]);

    // **M8 Part B: the screendump Part A could not take.** Part A's gate box asked for a
    // switched screen compared against a `libdraw` render, and could not have it: the guest had
    // no way to be *told* to switch, because this client parked on `sys_wait` rather than
    // reading anything. A registered chord is that channel — the host injects it over QMP, the
    // guest acts on it, and the screen captured is one the host asked for.
    //
    // **Only when the static comparison already passed.** A switched screen means nothing if
    // the unswitched one was wrong, and running it anyway would report the second failure while
    // hiding the first.
    if mismatches == 0
        && term_mismatches == 0
        && ui_mismatches == 0
        && let Err(e) = desktop_round_trip(&mut qmp, &mut session, &work, w, sw, sh, &composed, radius)
    {
        let _ = session.child.kill();
        let _ = fs::remove_file(&qmp_sock);
        return Err(e);
    }

    // **And `boot-probe` passed on this machine** (PR #358 review): read now, judged after the
    // screen, so a probe that failed cannot hide a mismatch.
    let probe = expect_probe_pass(&session);
    let _ = session.child.kill();
    let _ = fs::remove_file(&qmp_sock);

    if mismatches > 0 {
        let (x, y, got, want) = first.expect("a mismatch was counted");
        return Err(format!(
            "display gate FAILED: {mismatches} of {} scene pixels differ.\n  \
             first at ({x},{y}): screen {got:?}, expected {want:?}\n  \
             the capture is at {} — a whole-image shift suggests a base-address or stride \
             error, and swapped components suggest a channel-order one",
            compared[0],
            shot.display()
        )
        .into());
    }
    if w < tw || h < th {
        return Err(format!("guest display is {w}x{h}, smaller than the {tw}x{th} reference terminal").into());
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
        return Err(format!("guest display is {w}x{h}, smaller than the {uw}x{uh} reference UI").into());
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
    probe?;
    println!("  ok: boot-probe's verdict is PASS");
    println!(
        "\nxtask: display gate PASSED — {} pixels of the {sw}x{sh} scene, {term_compared} of the \
         {tw}x{th} terminal and {ui_compared} of the {uw}x{uh} toolkit window match libdraw, \
         libterm and libui pixel for pixel, rounded corners and shadows composed ✓",
        compared[0]
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
#[allow(
    clippy::too_many_arguments,
    reason = "the screen and scene geometry are separate scalars the caller already holds"
)]
fn desktop_round_trip(
    qmp: &mut Qmp,
    session: &mut Session,
    work: &Path,
    screen_w: u32,
    sw: u32,
    sh: u32,
    composed: &libdraw::framebuffer::MemFramebuffer,
    radius: u32,
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
    // **The composed screen, not the scene's own render** (desktop refresh, Part B): the scene's
    // window is rounded, so its corners show the terminal beneath it, and that is what coming
    // back unchanged has to reproduce.
    let expected: &libdraw::framebuffer::MemFramebuffer = composed;
    let mut bad = 0usize;
    let mut first_bad = None;
    for y in 0..sh {
        for x in 0..sw {
            // The shared corner at the origin shows `nxterm` beneath, which the comparison above
            // also leaves out — see `unknown_beneath` there.
            if x < radius && y < radius {
                continue;
            }
            let want = Framebuffer::get_pixel(expected, x, y).unwrap_or_default();
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

/// The last **complete** line of `fresh` containing `prefix` — what follows `prefix` on it,
/// trimmed — and how many bytes of `fresh` are whole lines. See [`Session::settled_line`].
///
/// **Whole lines only**: the reader appends as bytes arrive, and a half-written `hover 10` is not
/// row 10. The count lets the caller stop at the last complete line and read the rest next time.
fn last_receipt(fresh: &str, prefix: &str) -> (Option<String>, usize) {
    let whole = fresh.rfind('\n').map_or(0, |i| i + 1);
    let line = fresh[..whole].lines().rfind(|l| l.contains(prefix));
    let rest = line.map(|l| l[l.find(prefix).map_or(0, |i| i + prefix.len())..].trim().to_string());
    (rest, whole)
}


/// **The drive past 2 TiB `test-qemu` attaches** (Phase 6 Part G), in the build cache.
const PAST_TWO_TIB_IMAGE: &str = "test-qemu-usb-3tib.img";
/// The block `boot-probe` writes at: seven past the last a ten-byte command can name.
const PAST_TWO_TIB_BLOCK: u64 = (1 << 32) + 7;
/// What it writes there first; the rest of the sector is `(i * 31 + 7)`, as `boot-probe` makes it.
const PAST_TWO_TIB_MARK: &[u8; 32] = b"nitrox: past two tebibytes, G.5\n";

/// **The sector `boot-probe` wrote past 2 TiB, in the image** (Phase 6 Part G): where the guest
/// asked for it. A guest that read back what it wrote proves only that the write and the read
/// named the same block; a block number cut to 32 bits would name block 7 for both.
fn check_past_two_tib() -> R<()> {
    use std::os::unix::fs::FileExt;
    let big = build_cache().join(PAST_TWO_TIB_IMAGE);
    let mut got = [0u8; 512];
    let read = fs::File::open(&big).and_then(|f| f.read_exact_at(&mut got, PAST_TWO_TIB_BLOCK * 512));
    let low = fs::File::open(&big).and_then(|f| {
        let mut b = [0u8; 512];
        f.read_exact_at(&mut b, (PAST_TWO_TIB_BLOCK - (1 << 32)) * 512).map(|()| b)
    });
    let _ = fs::remove_file(&big);
    read.map_err(|e| format!("the 3 TiB image would not read: {e}"))?;
    let want: Vec<u8> =
        (0..512u32).map(|i| PAST_TWO_TIB_MARK.get(i as usize).copied().unwrap_or((i * 31 + 7) as u8)).collect();
    if got[..] != want[..] {
        let wrapped = low.is_ok_and(|b| b[..32] == PAST_TWO_TIB_MARK[..]);
        return Err(format!(
            "the sector boot-probe wrote at block {PAST_TWO_TIB_BLOCK} is not there in the image{}",
            if wrapped { ": it is at block 7, so the block number was cut to 32 bits" } else { "" }
        )
        .into());
    }
    println!("xtask: the sector boot-probe wrote past 2 TiB is in the 3 TiB image at block {PAST_TWO_TIB_BLOCK} ✓");
    Ok(())
}

/// **A stick as a shop sells one** (Phase 6 Part D): `mib` MiB, an MBR naming one FAT16 partition
/// from 1 MiB to the end, labelled `label`.
fn mbr_fat_stick(path: &Path, mib: u64, label: &str) -> R<()> {
    require_tool("mformat")?;
    let f = fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    f.set_len(mib * 1024 * 1024).map_err(|e| format!("size {}: {e}", path.display()))?;
    drop(f);
    let count = (mib * 2048 - 2048) as u32;
    write_mbr(path, 2048, count, 0x0E)?;
    run(Command::new("mformat")
        .arg("-i")
        .arg(format!("{}@@1048576", path.display()))
        .arg("-T")
        .arg(count.to_string())
        .arg("-c")
        .arg("1")
        .arg("-v")
        .arg(label)
        .arg("::"))?;
    Ok(())
}

/// **Write an MBR into `img`'s first block** (Phase 6 Part D): one entry, of MBR type `kind`, for
/// `count` blocks from `first`, and the signature. The other three entries are left empty.
fn write_mbr(img: &Path, first: u32, count: u32, kind: u8) -> R<()> {
    write_mbr_parts(img, &[(first, count, kind)])
}

/// An MBR naming each of `parts` — first sector, sector count, type — in its own entry, up to four.
fn write_mbr_parts(img: &Path, parts: &[(u32, u32, u8)]) -> R<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut mbr = [0u8; 512];
    for (i, &(first, count, kind)) in parts.iter().take(4).enumerate() {
        let e = &mut mbr[0x1BE + i * 16..0x1CE + i * 16];
        e[4] = kind;
        e[8..12].copy_from_slice(&first.to_le_bytes());
        e[12..16].copy_from_slice(&count.to_le_bytes());
    }
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    let mut f = fs::OpenOptions::new().write(true).open(img).map_err(|e| format!("open {}: {e}", img.display()))?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&mbr)?;
    Ok(())
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
    qemu_base_args(&mut cmd, &ovmf, accel, None)?;
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
    test_qemu_usb_args(&mut cmd)?;

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
    let mut hot = HotPlug::default();
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait qemu: {e}"))? {
            Some(st) => break st,
            None => {
                if let Ok(g) = captured.lock() {
                    hot.step(&g, &qmp_sock);
                }
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    // **Interrogate before killing.** This is the whole point of owning the
                    // deadline; once QEMU is dead the evidence is gone. The hot-plug's connection
                    // goes first: QEMU serves one QMP client at a time (PR #355 review).
                    hot.qmp = None;
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
            check_ahci_msi_path(&transcript)?;
            check_block_read_selftest(&transcript)?;
            check_oversize_refused(&transcript)?;
            check_every_service_started(&transcript)?;
            check_servers_are_service_mgrs(&transcript)?;
            check_system_control(&transcript)?;
            check_hardware_facts(&transcript)?;
            check_hot_plug(&transcript, &hot)?;
            check_usb_listed(&transcript)?;
            check_past_two_tib()?;
            println!("\nxtask: integration tests PASSED (qemu exit {code})");
            Ok(())
        }
        Some(code) => Err(format!("integration tests FAILED (qemu exit {code}; expected {PASS_EXIT})").into()),
        None => Err("qemu terminated by a signal with no exit code".into()),
    }
}

/// Facts about the machine QEMU emulates that every boot now logs (Phase 5 Part D.1), and that
/// both boots below know the answer to: q35's ECAM window and its four MADT CPUs and one IOAPIC.
/// The framebuffer is not among them since Phase 5 Part E: `test-qemu` boots QEMU's default mode
/// and `check-report` the gates' size, so each states its own. Each fact is fragments that must all appear in **one**
/// line.
///
/// **Written down here, not derived** — a second statement of each answer, which is what makes it
/// an assertion. On the laptop these are the lines to compare (the plan's § What to compare on
/// the day).
const EMULATED_MACHINE_FACTS: &[&[&str]] = &[
    &["acpi: ECAM @0xe0000000 seg 0 bus 0-255"],
    &["madt: lapic uid 0 apic 0 enabled"],
    &["madt: lapic uid 1 apic 1 enabled"],
    &["madt: lapic uid 2 apic 2 enabled"],
    &["madt: lapic uid 3 apic 3 enabled"],
    &["madt: ioapic 0 @0xfec00000 gsi 0"],
    &["acpi: 1 IOAPIC, 5 src-override, 4 CPU; 1 ECAM region"],
    // **q35's FADT** (administration Part E.3): the reset register a reboot writes first — the
    // chipset's reset control at I/O 0xcf9, `RESET_REG_SUP` set in the flags — the RTC's century
    // register, and an 8042.
    &["fadt: flags 0x84a5"],
    &["fadt: reset register I/O 0xcf9 width 8, value 0xf"],
    &["fadt: century register CMOS 0x32"],
    &["fadt: boot arch 0x2; 8042 present"],
];

/// The facts `test-qemu`'s boot adds to [`EMULATED_MACHINE_FACTS`]: its disk is on AHCI, so the
/// controller is **claimed**, it has a UART, so COM1 is **present**, and an xHCI with devices.
///
/// **The ACPI tables are pinned by signature and OEM, not address or length.** Read off the
/// first run (2026-09-14): QEMU's FACP, APIC, HPET, MCFG and WAET, each `BOCHS BXPC`, plus a BGRT
/// that is OVMF's (`INTEL EDK2`, the boot logo) and so not QEMU's to promise. The addresses are
/// wherever this OVMF build put them, and CI's OVMF is another build.
const TEST_QEMU_FACTS: &[&[&str]] = &[
    &["acpi: FACP @", "oem BOCHS BXPC"],
    &["acpi: APIC @", "oem BOCHS BXPC"],
    &["acpi: HPET @", "oem BOCHS BXPC"],
    &["acpi: MCFG @", "oem BOCHS BXPC"],
    &["acpi: WAET @", "oem BOCHS BXPC"],
    &["madt: lapic-nmi uid 0xff lint 1 flags 0x0"],
    &["pci 00:1f.2 8086:2922 class 01.06.01 pin 1 caps msi64"],
    &["drivers: 00:1f.2 claimed by ahci, MSI vec "],
    // **The xHCI, claimed** (Phase 6 Part A.1), configured as the laptop's is: MSI, no MSI-X
    // ([`XHCI_DEVICE`]). Its facts are QEMU's, read off the first boot: the USB 3 ports first, then
    // the USB 2 ones, as its Supported Protocol capabilities say. And the rings, the doorbell and
    // the event ring proved by a No Op before the outcome is claimed. Its PCI line is held to what
    // QEMU 8.2 and 11 both say — the identity and a 64-bit MSI capability — since CI runs the first
    // and a workstation may run the second.
    &["pci 00:03.0 1033:0194 class 0c.03.30 ", "caps msi64"],
    &["xhci: 00:03.0 up: xHCI 1.0, 16 ports (USB 2: 9-16, USB 3: 1-8), 64 slots, 32-byte contexts, 0 scratchpad(s)"],
    &["xhci: 00:03.0: the command ring answers: a No Op completed"],
    &["drivers: 00:03.0 claimed by xhci, MSI vec "],
    // **What the hub thread's first round found** (Part A.2): each device by its port, IDs, class,
    // speed, name and match, without its serial, which QEMU makes from the PCI path. The stick
    // takes a USB 3 port; the rest, USB 2 ones. The smart-card reader is the one whose default
    // endpoint is not its speed's, so its enumeration evaluates it.
    &["xhci: port 3 at start: connected, SuperSpeed, enabled"],
    &["xhci: port 6 at start: connected, SuperSpeed, enabled"],
    &["xhci: 6 of 16 ports connected at start"],
    &["usb: port 3: 46f4:0001 class 08/06/50, SuperSpeed, \"QEMU USB HARDDRIVE (", "\": mass storage, bulk-only"],
    &["usb: port 9: 0627:0001 class 03/01/01, high-speed, \"QEMU USB Keyboard (", "\": HID boot keyboard"],
    &["usb: port 10: 0627:0001 class 03/01/02, full-speed, \"QEMU USB Mouse (", "\": HID boot mouse"],
    &["usb: port 12: 0409:55aa class 09/00/00, full-speed, \"QEMU USB Hub (", "\": a hub, not supported"],
    &["usb: port 13: its default endpoint takes 64-byte packets, not 8; evaluated"],
    &["usb: port 13: 08e6:4433 class 0b/00/00, full-speed, \"QEMU USB CCID (", "\": nothing this kernel drives"],
    &["usb: port 6: 46f4:0001 class 08/06/50, SuperSpeed, \"QEMU USB HARDDRIVE (", "\": mass storage, bulk-only"],
    &["usb: first round: 6 device(s) in "],
    // **The stick bound as a disk, its MBR read** (Phase 6 Part D): INQUIRY's strings and the
    // capacity QEMU gives a 16 MiB image, and the one FAT16 partition the gate wrote.
    &["usb: port 3: LUN 0: disk \"QEMU QEMU HARDDISK (", "\", 32768 blocks of 512 bytes, record "],
    &["mbr:  partition 1 lba 2048..32767 (30720 sectors) type 0x0e -> block node"],
    &["mbr: 1 partition(s)"],
    &["boot-probe: registry: usb disk blk-", ", 32768 blocks of 512, under usb-"],
    &["boot-probe: registry: usb disk blk-", "'s partition blk-", ", by mbr"],
    // **The drive past 2 TiB** (Phase 6 Part G): `READ CAPACITY(10)` cannot say its size, so it is
    // asked again with `READ CAPACITY(16)` and read with the sixteen-byte commands; published with
    // its size; and written past 2 TiB through its node, which `check_past_two_tib` finds on the
    // host.
    &["usb: port 6: LUN 0: 2 TiB or more: 6442450944 blocks, read with the sixteen-byte commands"],
    &["usb: port 6: LUN 0: disk \"QEMU QEMU HARDDISK (", "\", 6442450944 blocks of 512 bytes, record "],
    &["boot-probe: registry: usb disk blk-", ", 6442450944 blocks of 512, under usb-"],
    &["boot-probe: past 2 TiB: blk-", ", 6442450944 blocks: a sector written and read back at block 4294967303 ok"],
    // **A disk read again** (Phase 6 Part G): `boot-probe`'s `Reread` of the stick, which the
    // kernel rescans — its one partition departed and published again — and which the watch is
    // pinged for though nothing is mounted on it.
    &["usb: port 3: LUN 0: rescanned: 1 partition(s) departed, 1 published"],
    &["storage-service: blk-", " rescanned, as asked: 1 partition(s)"],
    &[
        "boot-probe: storage admin: a Reread of blk-",
        " rescanned it: its partition replaced, the old one's node refused after, and the watch pinged with no mount changed ok",
    ],
    // **Reported, with its clusters, as not served** (Phase 6 Part E.6): 512-byte clusters, which
    // `fs-server-fat` refuses, so the boot's mounts — what `boot-probe` checks — are as they were.
    &[
        "storage-service: blk-",
        " (partition partition 1 (unlabelled)): fat 'NXSTICK', left clean; not served: 512-byte clusters, smaller than a page",
    ],
    // **The disk the machine started from**: named by Limine's record, flagged in the registry,
    // and the only one flagged — the SATA disk the test image boots from (Phase 6 Part D.1).
    &["boot: the modules came from partition 1 of the GPT disk "],
    &["gpt: the disk the machine started from: its GUID is the one Limine loaded from"],
    &["boot-probe: registry: the boot disk is blk-0, and nothing else is flagged"],
    // **And the table got each** (Part A.3): `boot-probe`'s line per `UsbDevice` record, read
    // through `/dev/registry` — under the controller's function, with the IDs, class, port, speed
    // (as xHCI numbers them: 1 full, 3 high, 4 SuperSpeed) and name the hub thread logged. The
    // lines above say a device was enumerated; only these say the registry has it.
    &["boot-probe: registry: usb port 3, 46f4:0001 class 08/06/50, speed 4, under 00:03.0, \"QEMU USB HARDDRIVE ("],
    &["boot-probe: registry: usb port 9, 0627:0001 class 03/01/01, speed 3, under 00:03.0, \"QEMU USB Keyboard ("],
    &["boot-probe: registry: usb port 10, 0627:0001 class 03/01/02, speed 1, under 00:03.0, \"QEMU USB Mouse ("],
    &["boot-probe: registry: usb port 12, 0409:55aa class 09/00/00, speed 1, under 00:03.0, \"QEMU USB Hub ("],
    &["boot-probe: registry: usb port 13, 08e6:4433 class 0b/00/00, speed 1, under 00:03.0, \"QEMU USB CCID ("],
    &["boot-probe: registry: usb port 6, 46f4:0001 class 08/06/50, speed 4, under 00:03.0, \"QEMU USB HARDDRIVE ("],
    // **The keyboard and mouse bound** (Part B.2), each at the index after the i8042's two, and in
    // the registry under its device. That `input-server` was given them is `check_usb_listed`'s:
    // how many it was given depends on where the hot-plug lands.
    &["usb: port 9: keyboard at /dev/input/raw/2"],
    &["usb: port 10: mouse at /dev/input/raw/3"],
    &["boot-probe: registry: usb keyboard at /dev/input/raw/2, under usb-"],
    &["boot-probe: registry: usb mouse at /dev/input/raw/3, under usb-"],
    &["console: RX loopback self-test OK"],
    &["cpu: requires +x2apic +rdtscp +nx +smep +smap;"],
    // QEMU's default mode, which `test-qemu` keeps so that every CI run boots two sizes.
    &["framebuffer: 1280x800 pitch 5120 padding 0 bpp 32"],
    // **What a write to the screen costs, and why** (Phase 5 Part G's measurement). The values
    // are the machine's and are not asserted — what is asserted is that the boot *says* them,
    // since a report that quietly stopped carrying them is how the laptop's next boot would
    // measure nothing. QEMU's own framebuffer is uncacheable here too, which is worth knowing:
    // the emulator disagrees with the hardware about the *cost*, not about the configuration.
    &["cache policy: ", "variable range(s), default "],
    &["cache policy: page attributes 0:"],
    // **The bootloader is held to the table the kernel programs** (Phase 5 Part G.1). The kernel
    // owns it either way; what this asserts is that nothing in flight — the console's mapping,
    // every `kvmap` MMIO mapping — chose its entry out of a different one. A bootloader that
    // changes its table fails here rather than silently changing what a live mapping means.
    &["cache policy: the kernel's table is installed; the bootloader's was the same"],
    // **What each block device says it is** (Phase 5 Part H.1). The installer refuses to write
    // anything that is not a `disk`, and this registry holds whole disks, the partitions found on
    // them and memory published as a disk — so a partition that called itself a disk would be a
    // partition table written over a filesystem. The capacity and the name are asserted with it:
    // they are what a person confirms a destructive operation by.
    // **By what it is, not where it lands**: a test image's scratch RAM disk (administration Part
    // C.5b) is published before any partition, so the ESP is `/dev/blk/2` there and would be
    // `/dev/blk/1` without it. The disk is first in any image.
    &["test-harness: /dev/blk/0 is a disk, 128 MiB, named QEMU HARDDISK"],
    &["test-harness: /dev/blk/", " is a partition, 48 MiB, named NITROX_ESP"],
    &["framebuffer: ", " is ", "memory, and a plain mapping of it is write-back"],
    // **What each mapping asks for**, which is what the timings are evidence of — and the half
    // QEMU agrees with the hardware about. Both ask for write-combining since Part G; before it,
    // the bootloader's did and a mapping this kernel made asked for write-back, which an
    // uncacheable range overrode.

    // **The two mappings agree since Part G.3**, which is the fix in one line: the aperture's
    // object says write-combining, so every mapping of it asks for that, the bootloader's included.
    &["framebuffer: the console's mapping asks for write-combining"],
    &["framebuffer: a mapping made as userspace's is asks for write-combining"],
    // **And what the client actually got.** The two lines above are the kernel measuring its own
    // mappings; this one is the object `/dev/framebuffer` mints, which is the only place the
    // aperture's answer reaches userspace and the only fact a compositor's speed depends on.
    &["framebuffer: handed out as write-combining memory"],
    &["framebuffer: a full-screen fill of ", "KiB took ", "through the console's mapping"],
    // The pair is the point: same loop, same pixels, two page-table entries. A boot that lost the
    // second line would leave the comparison looking like one number nobody could interpret.
    &["framebuffer: the same fill took ", "through a mapping made as userspace's is"],
];

/// The facts in `wanted` that no single line of `lines` carries every fragment of, each joined
/// for a message. Empty when every fact is there.
fn missing_facts<'a>(lines: &[&str], wanted: impl IntoIterator<Item = &'a [&'a str]>) -> Vec<String> {
    wanted
        .into_iter()
        .filter(|fact| !lines.iter().any(|line| fact.iter().all(|frag| line.contains(frag))))
        .map(|fact| fact.join(" … "))
        .collect()
}

/// Assert what the boot says about the machine (Phase 5 Part D.4): [`EMULATED_MACHINE_FACTS`],
/// [`TEST_QEMU_FACTS`], and the bootloader — this build's pinned Limine, on 64-bit UEFI.
fn check_hardware_facts(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    let lines: Vec<&str> = text.lines().collect();
    let limine = format!("boot: Limine {} on UEFI (64-bit), base revision 6", LIMINE_VERSION.trim_start_matches('v'));
    let bootloader: &[&str] = &[&limine];
    // **The build it is** (the laptop polish's Part D): the commit this process built from.
    let built_from = built_from_line();
    let commit: &[&str] = &[&built_from];
    let missing = missing_facts(
        &lines,
        EMULATED_MACHINE_FACTS.iter().chain(TEST_QEMU_FACTS).copied().chain([bootloader, commit]),
    );
    if !missing.is_empty() {
        return Err(format!(
            "the boot did not report what QEMU's machine is — no line says: {missing:?}. Each is \
             one line of the hardware report every boot logs (kernel_main's handoff, the ACPI \
             walk, PCI enumeration, the driver outcomes, the framebuffer record)"
        )
        .into());
    }
    println!(
        "xtask: the boot reported QEMU's machine as it is — tables, CPUs, IOAPIC, framebuffer, a claimed AHCI and xHCI, COM1 ✓"
    );
    Ok(())
}

/// Assert that the block driver **refused** a transfer larger than one command can describe.
///
/// **The same hole `check_display_selftest` documents, met again.** `drivers::self_test` prints
/// a verdict and does not call `SYS_TEST_EXIT`, so with the guard in `dispatch_block_irp`
/// deleted the guest printed `ahci: oversize self-test FAIL (249 frags accepted, table holds
/// 248)` and `test-qemu` exited **0**. A self-test whose failure nobody reads is not a test —
/// which is precisely what the comment on `check_display_selftest` had already worked out, one
/// declaration over.
///
/// Requiring the `OK` line rather than merely rejecting `FAIL` is deliberate: it also catches
/// the check not running at all, which is what a refactor that dropped the call would produce.
///
/// **The skip arm names every cause, because an earlier version named one and it was the
/// unlikeliest.** It said the check is skipped "only when the first block device publishes no
/// fragment limit" — but `oversize_refused` was then the *last* statement of
/// `drivers::self_test`, so a regression in the sector-0 read path skipped it too, and this
/// message would have sent the author to `max_frags` for a fault three functions away
/// (PR #293 review, 2). The call has moved above those returns, and
/// [`check_block_read_selftest`] adjudicates the read separately — so a read regression now
/// fails on its own check, with its own message, before this one is consulted.
fn check_oversize_refused(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if text.contains("ahci: oversize self-test OK") {
        println!("xtask: an oversized block transfer was refused ✓");
        return Ok(());
    }
    if text.contains("ahci: oversize self-test FAIL") {
        return Err("a block transfer needing more fragments than the device's command table \
             holds was NOT refused — the driver would write PRDT entries past the table it \
             owns, and the controller would DMA to whatever followed. See \
             `dispatch_block_irp`'s `max_frags` check and `TODO(block-transfer-split)`."
            .into());
    }
    Err("the oversize block self-test did not run — expected \
         \"ahci: oversize self-test OK\" in the transcript. It is skipped when no block device \
         was discovered at all, when the first one publishes no fragment limit (the ramdisk's \
         `u32::MAX`), or when the probe's own allocation failed. An AHCI disk that came up is \
         none of those."
        .into())
}

/// Assert that the AHCI driver acquired its interrupt over **MSI**, not by falling back to
/// INTx.
///
/// **A passing `test-qemu` proves nothing about this on its own**, which is the whole reason
/// the check exists. QEMU's firmware programs the PCI interrupt-line register to match the
/// IOAPIC, so the INTx fallback works perfectly here — the boot, the block read self-test and
/// the verdict are identical either way. Real UEFI frequently leaves that register
/// meaningless, so a silent fallback is a bug only the target machine would report, by not
/// booting.
///
/// **Matches the path token and never the vector**, deliberately. The vector is not stable
/// across builds: the selftest image's `IrqRouter::self_test` registers `pit_tick` and never
/// releases the slot, so AHCI lands on `0x31` there and `0x30` in a release image, and every
/// captured transcript in `tools/build-cache` splits on exactly that line. A matcher written
/// against the literal would pass this gate — `test-qemu` is a selftest image — and fail the
/// first time it were pointed at `check-login` or `test-interactive`. Part A's own switch
/// moved the number again.
///
/// The INTx arm is a **failure, not a skip**: every AHCI controller on either target machine
/// advertises MSI (capability at `0x80` on both), so taking the fallback under QEMU means the
/// capability walk, the message composition or the capability write went wrong — not that the
/// hardware lacked the feature.
fn check_ahci_msi_path(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if text.contains("ahci: irq via MSI") {
        println!("xtask: the AHCI controller took the MSI interrupt path ✓");
        return Ok(());
    }
    if text.contains("ahci: irq via INTx") {
        return Err("the AHCI driver fell back to INTx. QEMU's ICH9 AHCI advertises MSI at \
             capability 0x80, so the fallback firing here means the driver did not find it or \
             could not program it — `pci::find_capability`, `pci::read_msi`, \
             `ArchIrqInstall::install_msi` (which declines a destination wider than eight \
             bits), or `pci::Config::map`, whose failure logs its own line just above. The \
             boot still passes, because QEMU's firmware programs the interrupt-line register \
             and real UEFI often does not — which is the bug this gate exists to catch before \
             the laptop does."
            .into());
    }
    Err("the AHCI driver printed no interrupt-path line — expected \"ahci: irq via MSI\" or \
         \"ahci: irq via INTx\". Both arms print one, so its absence means the controller \
         never reached interrupt installation at all: no ABAR, no SATA disk on an implemented \
         port, a failed DMA allocation, or an HBA declined for lacking CAP.S64A. Each of those \
         logs its own line above this point."
        .into())
}

/// Assert that the block driver's **sector-0 read** self-test passed.
///
/// **The hole this branch is about, closed for its neighbour too.** `drivers::self_test` prints
/// a verdict and never calls `SYS_TEST_EXIT`, so before this every outcome of the real AHCI read
/// path — a dispatch error, a non-zero status, a short transfer — printed `ahci: read self-test
/// FAIL …` and `test-qemu` exited 0. `grep "read self-test" tools/xtask/src/main.rs` returned
/// nothing (PR #293 review, 2).
///
/// **Accepts either success line.** `read self-test OK` is the read plus a `0x55AA` boot
/// signature; `read self-test: sector 0 read OK … but sig` is the read succeeding on an image
/// whose sector 0 carries something else. Only the first appears today (the image has a
/// protective MBR), but requiring it would fail this gate for a change to the *image* rather
/// than to the driver, and what this checks is the driver.
fn check_block_read_selftest(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if text.contains("ahci: read self-test FAIL") {
        return Err("the AHCI sector-0 read self-test FAILED — the real block read path \
             (dispatch → controller DMA → completion) did not deliver. The transcript line \
             says which half: `(dispatch err …)` is a refusal to start, `(status … result …)` \
             is a completed request that errored or came up short."
            .into());
    }
    if text.contains("ahci: read self-test OK") || text.contains("sector 0 read OK") {
        println!("xtask: the block read self-test passed ✓");
        return Ok(());
    }
    Err("the AHCI read self-test did not run — expected an \"ahci: read self-test\" line in \
         the transcript. It is skipped when no block device was discovered, or when the probe \
         could not allocate its buffer."
        .into())
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
    // And, since `service-mgr` starts the servers (administration Part E.1), each way one fails to
    // come up: no `Ready`, a refusal, or a path it could not be bound at.
    for pat in [
        "service-mgr: image not found",
        "service-mgr: spawn FAIL",
        "did not come up",
        "sent no Ready within",
        "service-mgr: bind FAIL at",
        "registry bind FAIL",
        "service-mgr: no route for",
        "-- sessions will not reach it",
        // A declaration the parser could not take, a restart that could not spawn, and a file
        // that lost its critical servers (PR #340 review, findings 2 and 3).
        "' skipped: ",
        "' could not be restarted",
        "-- starting nothing",
    ] {
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

/// The servers `service-mgr` starts, and the root path each is bound at (administration Part
/// E.1). **Written down a second time here**, from `SERVICES_TOML`, so the gate does not take its
/// aim from the declarations it checks.
const SERVICE_MGR_SERVERS: &[(&str, &str)] = &[
    ("auth-service", "/svc/auth"),
    ("logging-service", "/log"),
    ("tty-server", "/dev/tty"),
    ("clipboard-server", "/dev/clipboard"),
    ("view-broker", "/svc/views"),
    ("device-mgr", "/svc/devices"),
    ("storage-service", "/svc/storage"),
    ("input-server", "/dev/input/new"),
    ("compositor", "/dev/draw"),
];

/// **`init` holds the system-control object** (administration Part E.3): the kernel made it and
/// handed it over in `rdx`, and `init` found a `SystemControl` with `WRITE` there. The other half,
/// that nothing else can use `sys_power`, is `boot-probe`'s, which gates the verdict itself.
fn check_system_control(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("init: holds the system-control object") {
        return Err("`init: holds the system-control object` is not in the transcript: the kernel's \
                    handoff in `rdx` did not reach init as a SystemControl with WRITE"
            .into());
    }
    println!("xtask: init holds the system-control object ✓");
    Ok(())
}

/// **`service-mgr` bound every server, and `init` bound none of them** (administration Part E.1).
/// `init` starts only its mounts, the profile server at `/bin`, and `service-mgr`; a line of
/// `init`'s naming one of these servers' paths would be the old arrangement back.
fn check_servers_are_service_mgrs(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    for (name, path) in SERVICE_MGR_SERVERS {
        let line = format!("service-mgr: {name} bound at {path}");
        if !text.contains(&line) {
            return Err(format!("`{line}` is not in the transcript: service-mgr did not bind {name}").into());
        }
        if text.lines().any(|l| l.starts_with("init: ") && l.contains(name)) {
            return Err(
                format!("init's log names {name}, which service-mgr starts since administration Part E.1").into()
            );
        }
    }
    // **And the endpoints two of them mint for sessions** (Part E.1b), each behind a route of its
    // own: what the login supervisors are handed for `/dev/devices` and `/storage`.
    for (name, through) in [("device-mgr", "info-endpoint"), ("storage-service", "session-endpoint")] {
        let line = format!("service-mgr: sessions reach {name} through its {through}");
        if !text.contains(&line) {
            return Err(format!("`{line}` is not in the transcript: no route to it for sessions").into());
        }
    }
    println!("xtask: service-mgr bound all {} servers, and init none of them ✓", SERVICE_MGR_SERVERS.len());
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
/// discovering.** `service-mgr` queues session-mgr's handoffs before it starts any declaration
/// after the last server, so the *send* is ordered — but session-mgr still has to be scheduled
/// to print this line before `boot-probe`, spawned afterwards, fires PASS and terminates the
/// machine. If that ever inverts, a healthy boot fails here, which is the expensive kind of
/// red.
///
/// The margin is large and was measured rather than assumed. It was six lines before
/// administration Part E.1a; on E.1a's first passing boot session-mgr's line landed **252
/// lines** before `service-mgr: starting service 'boot-probe'`, with the display self-test,
/// `nxterm` and the graphical test clients started between. If this ever goes red on a boot that looks
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
mod tests {
    use super::*;

    /// **The throughput stick's tree**: its three parts at the sizes the plan names, 384 MiB in all,
    /// and every name once — which is what lets a copy be checked by its name wherever it was put.
    #[test]
    fn the_throughput_tree_is_the_plans_and_its_names_are_unique() {
        let files = super::throughput_files();
        let in_dir = |d: &str| files.iter().filter(|f| f.0 == d || f.0.starts_with(&format!("{d}/"))).count();
        assert_eq!((in_dir("big"), in_dir("small"), in_dir("tree")), (32, 4096, 1024));
        assert_eq!(files.iter().map(|f| f.2).sum::<u64>(), 384 << 20);
        let names: std::collections::HashSet<&String> = files.iter().map(|f| &f.1).collect();
        assert_eq!(names.len(), files.len(), "a name twice could not be checked by its name");
        let short = |n: &str| {
            n.split_once('.').is_some_and(|(b, e)| b.len() <= 8 && e.len() == 3)
                && n.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'.')
        };
        assert!(files.iter().all(|f| short(&f.1)), "every name a FAT short name, as mcopy needs");
    }

    /// A pattern is its name's alone, the same each time, and its length to the byte.
    #[test]
    fn a_throughput_pattern_is_its_names_and_its_lengths() {
        let a = super::throughput_pattern("S0001.BIN", 16 * 1024);
        assert_eq!(a, super::throughput_pattern("S0001.BIN", 16 * 1024));
        assert_ne!(a, super::throughput_pattern("S0002.BIN", 16 * 1024));
        assert_eq!(super::throughput_pattern("x", 13).len(), 13);
        assert_eq!(&super::throughput_pattern("x", 13)[..], &super::throughput_pattern("x", 64)[..13]);
    }

    /// **The manager lists at least the first round's devices** (PR #356 review): the lines the
    /// three runs of fgb68 and before gave pass, and the reviewer's control — a manager that
    /// dropped every USB record, which `boot-probe` passed — fails, as does one short by one.
    #[test]
    fn device_mgr_must_list_the_first_rounds_usb_devices() {
        let boot_with = |listed: usize, since: usize, handed: usize| {
            format!(
                "usb: port 9: keyboard at /dev/input/raw/2\nusb: port 10: mouse at /dev/input/raw/3\n\
                 usb: first round: 5 device(s) in 287 ms\ninit: spawned init (pid 1)\n\
                 input-server: {handed} device(s) from the device manager\nboot-probe: devices: \
                 block held by the storage service, all.tsm has 20 rows, {listed} of them usb-<id>, and \
                 {since} USB records since ok\n"
            )
        };
        let boot = |listed: usize, since: usize| boot_with(listed, since, 4);
        for (listed, since) in [(5, 2), (6, 1), (7, 0)] {
            assert!(check_usb_listed(boot(listed, since).as_bytes()).is_ok(), "{listed} listed");
        }
        assert!(check_usb_listed(boot(0, 7).as_bytes()).is_err(), "none listed: the reviewer's control");
        assert!(check_usb_listed(boot(4, 3).as_bytes()).is_err(), "one short");
        let no_round = boot(5, 2).replace("usb: first round: 5 device(s)", "usb: no round");
        assert!(check_usb_listed(no_round.as_bytes()).is_err(), "no first round to hold it to");
        // **`input-server` handed the first round's keyboard and mouse** beside the i8042's two
        // (Part B.2): five when a hot-plugged keyboard was bound before the manager's read, and
        // three is one short.
        assert!(check_usb_listed(boot_with(5, 2, 5).as_bytes()).is_ok(), "a hot-plugged keyboard too");
        assert!(check_usb_listed(boot_with(5, 2, 3).as_bytes()).is_err(), "one input device short");
    }

    #[test]
    fn a_settled_receipt_is_the_last_whole_one() {
        // The approach crosses `Clear` and stops on `Copy`: the answer is where it stopped.
        let text = "nxterm: menu hover 106\nheartbeat\nnxterm: menu hover 100\n";
        assert_eq!(last_receipt(text, "nxterm: menu hover "), (Some("100".to_string()), text.len()));
        // A line still being written is not read, and not consumed.
        let partial = "nxterm: menu hover 100\nnxterm: menu hover 10";
        assert_eq!(
            last_receipt(partial, "nxterm: menu hover "),
            (Some("100".to_string()), "nxterm: menu hover 100\n".len())
        );
        // Nothing complete, nothing read.
        assert_eq!(last_receipt("nxterm: menu hov", "nxterm: menu hover "), (None, 0));
        assert_eq!(last_receipt("heartbeat\n", "nxterm: menu hover "), (None, 10));
    }

    /// **The lights after the last reset, read from a real trace** (Phase 6 Part B.5): the firmware's
    /// reset and its lights, then the kernel's reset, its zero and the compositor's Num Lock. The
    /// first version split at the word `ledstate`, which the event's name holds too, and read
    /// nothing from every line — the gate's first run said `[]` with this trace on disk.
    #[test]
    fn the_lights_are_read_after_the_last_reset() {
        let trace = "ps2_reset_keyboard 0x5fe490c46268\n\
                     ps2_set_ledstate 0x5fe490c46268 ledstate 0\n\
                     ps2_set_ledstate 0x5fe490c46268 ledstate 4\n\
                     ps2_reset_keyboard 0x5fe490c46268\n\
                     ps2_set_ledstate 0x5fe490c46268 ledstate 0\n\
                     ps2_set_ledstate 0x5fe490c46268 ledstate 2\n";
        assert_eq!(super::ps2_lights_in(trace), [0, 2]);
        assert_eq!(super::ps2_lights_in(""), [0u8; 0], "no trace, no lights");
    }

    #[test]
    fn the_staged_theme_reads_back_as_itself_and_only_its_overrides_are_live() {
        let (shipped, text) = super::staged_theme().unwrap();
        let (read, issues) = libdraw::theme::Theme::from_config(&text);
        assert!(issues.is_empty(), "{issues:?}\n{text}");
        assert_eq!(read, shipped);
        // **Live lines are exactly the deliberate ones**: the scheme, and the three fields a gate
        // reads back. Every colour is shown but commented, which is what lets the scheme line do
        // anything at all.
        let live: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split(" =").next())
            .collect();
        assert_eq!(live, ["scheme", "font_px", "wallpaper", "wallpaper_mode"], "{text}");
        assert!(text.contains("\n# accent = \"#2C7F92\"\n"), "the colours are shown:\n{text}");
        // And changing that one line changes the palette, keeping the overrides.
        let dark = text.replace("scheme = \"light\"", "scheme = \"dark\"");
        let (read, issues) = libdraw::theme::Theme::from_config(&dark);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(read.background, libdraw::theme::Theme::dark().background);
        assert_eq!((read.font_px, read.wallpaper), (shipped.font_px, shipped.wallpaper));
    }

    /// Every metric `chrome` copies from `libui::widget` still equals the toolkit's.
    ///
    /// **The generalisation of the greeter's pair** (PR #318 review, finding 1, applied to the
    /// rest): `chrome` exists because a gate that read the toolkit to know where to click could
    /// agree with a toolkit that had stopped drawing where it says (M11 decision 2) — and a
    /// deliberate copy still needs something that fails when it goes stale. `libui`'s own tests
    /// pin the other side, pressing these literals on trees that are actually built.
    ///
    /// **Imported rather than parsed.** This read the constants out of `widget.rs` with a little
    /// parser and a guard against the pattern going stale; `xtask` links `libui` already — it
    /// renders the display gate's reference frames with it — so the comparison is just a
    /// comparison (PR #319 review, optional 8). Nothing about M11 decision 2 is weakened: a
    /// *gate* still aims from `chrome`, and only this test sees both sides.
    ///
    /// **Only the direct copies.** `TITLE_Y`, `CLOSE_X`, `MAXIMISE_X`, `MINIMISE_X` and
    /// `CONTENT_X` are sums over the toolkit's constants rather than any one of them, and
    /// `the_title_buttons_land_where_the_gates_aim` is what pins those.
    #[test]
    fn the_gates_chrome_table_is_the_toolkits() {
        use libui::widget as w;
        for (what, gate, toolkit) in [
            ("TITLE_BAR_H", chrome::TITLE_BAR_H, w::TITLE_BAR_H as i32),
            ("TAB_STRIP_H", chrome::TAB_STRIP_H, w::TAB_STRIP_H as i32),
            ("TAB_H", chrome::TAB_H, w::TAB_H as i32),
            ("TAB_TOP", chrome::TAB_TOP, w::TAB_TOP as i32),
            ("TAB_W", chrome::TAB_W, w::TAB_W as i32),
            ("TAB_SIDE", chrome::TAB_SIDE, w::TAB_SIDE as i32),
            ("TAB_PITCH", chrome::TAB_PITCH, w::TAB_PITCH as i32),
            ("DIALOG_W", chrome::DIALOG_W, w::DIALOG_W as i32),
            ("DIALOG_H", chrome::DIALOG_H, w::DIALOG_H as i32),
            ("DIALOG_LEFT_CX", chrome::DIALOG_LEFT_CX, w::DIALOG_LEFT_CX),
            ("DIALOG_RIGHT_CX", chrome::DIALOG_RIGHT_CX, w::DIALOG_RIGHT_CX),
            ("DIALOG_BUTTON_CY", chrome::DIALOG_BUTTON_CY, w::DIALOG_BUTTON_CY),
        ] {
            assert_eq!(gate, toolkit, "`chrome::{what}` is {gate}, the toolkit's is {toolkit}");
        }
    }

    /// Every metric `browser` copies from `nxfiles` still equals the browser's.
    ///
    /// **Read out of the source, because this crate cannot link that one** — `nxfiles` is a
    /// bare-target program and its library half pulls `libsurface` with it, which is the same
    /// reason the greeter's pair is parsed rather than imported. The toolkit's own metrics are
    /// imported instead; see `the_gates_chrome_table_is_the_toolkits`.
    ///
    /// The extraction is guarded: a constant that stops being a literal fails here rather than
    /// quietly comparing nothing.
    #[test]
    fn the_gates_browser_table_is_the_browsers() {
        let path = repo_root().join("userspace/nxfiles/src/lib.rs");
        let text = fs::read_to_string(&path).expect("the browser's source is readable");
        let mut found: BTreeMap<String, i32> = BTreeMap::new();
        for line in text.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix("pub const ") else { continue };
            let Some((name, tail)) = rest.split_once(':') else { continue };
            let Some((ty, val)) = tail.split_once('=') else { continue };
            if ty.trim() != "u32" {
                continue;
            }
            if let Ok(v) = val.trim().trim_end_matches(';').parse::<i32>() {
                found.insert(name.trim().to_string(), v);
            }
        }
        let get = |what: &str| {
            *found.get(what).unwrap_or_else(|| {
                panic!(
                    "no `pub const {what}: u32` in {} — this checker's pattern has gone stale, \
                     which silently stops comparing anything",
                    path.display()
                )
            })
        };
        for (what, gate, app) in [
            ("SIDEBAR_W", browser::SIDEBAR_W, get("SIDEBAR_W")),
            ("SIDEBAR_PAD", browser::SIDEBAR_PAD, get("SIDEBAR_PAD")),
            ("PATH_H", browser::PATH_H, get("PATH_H")),
            ("ROW_H", browser::ROW_H, get("ROW_H")),
            ("MENU_BAR_H", browser::MENU_BAR_H, get("MENU_BAR_H")),
        ] {
            assert_eq!(gate, app, "`browser::{what}` is {gate}, the browser's is {app}");
        }
        // `HEADER_H` is the toolkit's, which the browser re-exports rather than spelling.
        assert_eq!(browser::HEADER_H, libui::widget::LIST_HEADER_H as i32);
        // **The Drives heading is a row's height** (Phase 6 Part F), spelled as one there.
        assert!(
            text.contains("pub const DRIVES_HEADING_H: u32 = ROW_H;") && browser::DRIVES_HEADING_H == browser::ROW_H,
            "the browser's Drives heading is no longer a row's height, and `check-media` aims below it"
        );
    }

    /// **The gate's copy of the editor's chooser key is the editor's**, and the toolkit puts the
    /// chooser's Up button at that key plus seven — found, in the window the toolkit sizes, in the
    /// strip below the title, at the theme the image stages (Phase 6 Part F).
    #[test]
    fn the_gates_editor_table_is_the_editors() {
        let path = repo_root().join("userspace/nxedit/src/lib.rs");
        let text = fs::read_to_string(&path).expect("the editor's source is readable");
        let line = format!("pub const CHOOSER_KEY: u64 = {};", editor::CHOOSER_KEY);
        assert!(text.contains(&line), "no `{line}` in {}", path.display());
        let (x, y) = chooser_up_at(libui::chooser::CHOOSER_W, libui::chooser::CHOOSER_H).expect("an Up button");
        assert!(x > 0 && x < 40 && y > libui::widget::TITLE_BAR_H as i32 && y < 90, "Up is at ({x}, {y})");
    }

    /// The gate's copy of the greeter's window is the greeter's own.
    ///
    /// **Two constants that must agree, kept by a test rather than by a comment** — the same
    /// arrangement `abi-sync-check` uses for the kernel's constants and `libkern`'s, and for the
    /// same reason: `xtask` writes chrome metrics down a second time on purpose (M11 decision 2),
    /// and a deliberate copy still needs something that fails when it goes stale. Part D claimed
    /// the greeter's own host test covered this; it measures the card against the *greeter's*
    /// constant and never reads this one (PR #318 review, finding 1).
    ///
    /// The extraction is guarded: finding neither constant is a failure, not a pass, because a
    /// pattern that has gone stale would otherwise silently agree with everything.
    #[test]
    fn the_gates_greeter_size_is_the_greeters_own() {
        let path = repo_root().join("userspace/desktop-session-mgr/src/lib.rs");
        let text = fs::read_to_string(&path).expect("the greeter's source is readable");
        let mut found: Vec<(String, u32)> = Vec::new();
        for line in text.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix("pub const GREETER_") else { continue };
            let Some((name, tail)) = rest.split_once(':') else { continue };
            let Some((ty, val)) = tail.split_once('=') else { continue };
            if ty.trim() != "u32" {
                continue;
            }
            if let Ok(v) = val.trim().trim_end_matches(';').parse::<u32>() {
                found.push((name.trim().to_string(), v));
            }
        }
        let get = |what: &str| {
            found.iter().find(|(n, _)| n == what).map(|(_, v)| *v).unwrap_or_else(|| {
                panic!(
                    "no `pub const GREETER_{what}: u32` in {} — this checker's pattern has gone \
                     stale, which silently stops comparing anything",
                    path.display()
                )
            })
        };
        assert_eq!((get("W"), get("H")), GREETER, "the greeter's window and this file's copy of it have drifted");
    }

    /// The gate's copy of the overview's card metrics is the shell's.
    ///
    /// **The copy is deliberate** (M11 decision 2): a gate that read the shell's layout to know
    /// where to aim could agree with a shell that had stopped drawing where it says. What a copy
    /// still needs is this — the greeter's `340x141` was `420x200` for a milestone, and the
    /// lesson Part D drew is that a copy without a comparison is a number nobody checks.
    ///
    /// **Parsed rather than imported**, because `desktop-shell` is a bare-target crate this one
    /// cannot link — the same reason `the_gates_browser_table_is_the_browsers` reads source.
    #[test]
    fn the_gates_card_metrics_are_the_shells() {
        let path = repo_root().join("userspace/desktop-shell/src/lib.rs");
        let text = fs::read_to_string(&path).expect("the shell's source is readable");
        let mut found: Vec<(String, i32)> = Vec::new();
        for line in text.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix("pub const ") else { continue };
            let Some((name, tail)) = rest.split_once(':') else { continue };
            let Some((ty, val)) = tail.split_once('=') else { continue };
            if ty.trim() != "u32" {
                continue;
            }
            if let Ok(v) = val.trim().trim_end_matches(';').parse::<i32>() {
                found.push((name.trim().to_string(), v));
            }
        }
        let get = |what: &str| {
            found.iter().find(|(n, _)| n == what).map(|(_, v)| *v).unwrap_or_else(|| {
                panic!(
                    "no `pub const {what}: u32` in {} — this checker's pattern has gone stale, \
                     which silently stops comparing anything",
                    path.display()
                )
            })
        };
        assert_eq!(
            (
                get("CARD_W_NUM"),
                get("CARD_W_DEN"),
                get("CARD_BORDER"),
                get("CARD_GAP"),
                get("CARD_CAPTION_GAP"),
                get("CARD_CAPTION_H"),
                get("CARD_SIDE_PAD"),
                get("BAR_H"),
            ),
            (
                DisplaySize::CARD_W_NUM,
                DisplaySize::CARD_W_DEN,
                DisplaySize::CARD_BORDER,
                DisplaySize::CARD_GAP,
                DisplaySize::CARD_CAPTION_GAP,
                DisplaySize::CARD_CAPTION_H,
                DisplaySize::CARD_SIDE_PAD,
                DisplaySize::BAR_H,
            ),
            "the overview's card metrics and this file's copy of them have drifted"
        );
    }

    /// The shipped UI face carries the glyph a marked menu row draws.
    ///
    /// **A missing glyph is a silent failure** — `.notdef`, drawn as a blank or a box, reported
    /// nowhere — so a decorative character picked in `libui` is a claim about a font file staged
    /// by this crate, and this is the only place both are in scope. The control is the second
    /// assertion: a private-use codepoint no face carries, which proves the check can fail.
    #[test]
    fn the_menu_mark_exists_in_the_shipped_face() {
        let (ui, _) = host_faces().expect("the built-in theme's faces load on the host");
        let mark = libui::menu::MARK.chars().next().expect("MARK is not empty");
        assert!(ui.has_glyph(mark), "the UI face has no glyph for {mark:?} — a marked menu row would draw a blank");
        assert!(!ui.has_glyph('\u{E000}'), "a private-use codepoint should be absent");
    }

    /// Every picture the display gate demands of the guest is a preview, at the gate's size — and
    /// the one preview it does not demand, `ui-dark`, is really the dark scheme.
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
    fn the_gates_pictures_are_previews_and_the_dark_one_is_dark() {
        use libdraw::framebuffer::Framebuffer;
        let faces = host_faces().expect("the vendored fonts");
        let frames = preview_frames(&faces);

        let sizes: Vec<(&str, u32, u32)> =
            frames.iter().map(|(n, f)| (*n, f.geometry().width, f.geometry().height)).collect();
        let term = libterm::render::reference::size(&faces.1);
        assert_eq!(
            sizes,
            [
                ("ui", libui::reference::WIDTH, libui::reference::HEIGHT),
                ("ui-dark", libui::reference::WIDTH, libui::reference::HEIGHT),
                ("term", term.w, term.h),
            ],
            "the previews are the gate's arrangements at the gate's sizes, and ui-dark"
        );
        // And the dark one really is dark. (60, 4) is inside the menu bar's highlighted `Edit`,
        // which is the selection — the accent washed over the scheme's background, `#D5E5E9` in
        // the light scheme — so the frames differ there and the dark one is the darker.
        let px = |n: &str| {
            let f = &frames.iter().find(|(name, _)| *name == n).expect("a frame").1;
            Framebuffer::get_pixel(f, 60, 4).expect("in bounds")
        };
        let (light, dark) = (px("ui"), px("ui-dark"));
        assert!(
            u32::from(dark.r) + u32::from(dark.g) + u32::from(dark.b)
                < u32::from(light.r) + u32::from(light.g) + u32::from(light.b),
            "the dark frame is not darker where the menu bar is: {dark:?} against {light:?}"
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
        assert_eq!(markers_in_line(&format!("// {} and {}: see the doc.", m("alpha"), m("beta"))), ["alpha", "beta"]);

        // A bracketed placeholder is skipped and does not stop the scan: a real marker
        // after it on the same line is still found.
        assert_eq!(markers_in_line(&format!("// spelt {} not {}", m("<tag>"), m("tag"))), ["tag"]);

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
        let exempt: Vec<&str> = got.iter().filter(|(_, e)| *e).map(|(t, _)| t.as_str()).collect();
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
        let step14 = [String::from("try { fail \"boom\" } catch (e) { format(\"caught={}\", e.message) }")];
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
            let path = env::temp_dir().join(format!("nitrox-xtask-{}-{}-{}", tag, std::process::id(), n));
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
        assert_eq!(parse_host_from_rustc_vv(sample).as_deref(), Some("x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn parse_host_extracts_macos_triple() {
        let sample = "rustc 1.95.0\nhost: aarch64-apple-darwin\n";
        assert_eq!(parse_host_from_rustc_vv(sample).as_deref(), Some("aarch64-apple-darwin"));
    }

    #[test]
    fn parse_host_returns_none_when_absent() {
        let sample = "rustc 1.95.0\nrelease: 1.95.0\n";
        assert!(parse_host_from_rustc_vv(sample).is_none());
    }

    /// **A test root may differ in the two data files and its own package, and nothing else**
    /// (administration Part E.1c) — including the control the plan names: a file added to one
    /// root, on either side.
    #[test]
    fn a_test_root_differs_from_a_release_one_only_where_allowed() {
        use std::collections::BTreeMap;
        let file = |n: u64| TreeEntry::File { size: n, hash: n };
        let pkg = "store/abc-test-0.1.0";
        let release: BTreeMap<String, TreeEntry> = [
            ("system", TreeEntry::Dir),
            ("system/services.toml", file(1)),
            ("system/profiles/system.toml", file(2)),
            ("store/def-system-0.1.0/bin/service-mgr", file(3)),
            ("store/0a1-coreutils-0.1.0/bin/list", file(8)),
            ("store/0a1-coreutils-0.1.0/bin/nxterm", file(9)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut test: BTreeMap<String, TreeEntry> = [
            ("system", TreeEntry::Dir),
            ("system/services.toml", file(10)),
            ("system/profiles/system.toml", file(20)),
            ("store/def-system-0.1.0/bin/service-mgr", file(3)),
            // The same package under another hash, since its `nxterm` is the test-harness build.
            ("store/fe2-coreutils-0.1.0/bin/list", file(8)),
            ("store/fe2-coreutils-0.1.0/bin/nxterm", file(90)),
            (pkg, TreeEntry::Dir),
            ("store/abc-test-0.1.0/bin/boot-probe", file(4)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(root_divergence(&release, &test, pkg), Vec::<String>::new());
        // A file added to the test root, outside its package.
        test.insert("system/extra".into(), file(5));
        assert_eq!(root_divergence(&release, &test, pkg), ["system/extra: only on the test root"]);
        test.remove("system/extra");
        // A program that differs.
        test.insert("store/def-system-0.1.0/bin/service-mgr".into(), file(6));
        assert_eq!(root_divergence(&release, &test, pkg), ["store/system-0.1.0/bin/service-mgr: differs"]);
        test.insert("store/def-system-0.1.0/bin/service-mgr".into(), file(3));
        // Another program in the package `nxterm` is in, differing too: only `nxterm` may.
        test.insert("store/fe2-coreutils-0.1.0/bin/list".into(), file(80));
        assert_eq!(root_divergence(&release, &test, pkg), ["store/coreutils-0.1.0/bin/list: differs"]);
        test.insert("store/fe2-coreutils-0.1.0/bin/list".into(), file(8));
        // A file added to the release root, and the test package on a release root.
        let mut release2 = release;
        release2.insert("system/extra".into(), file(5));
        release2.insert("store/abc-test-0.1.0/bin/boot-probe".into(), file(4));
        let mut got = root_divergence(&release2, &test, pkg);
        got.sort();
        assert_eq!(
            got,
            [
                "store/test-0.1.0/bin/boot-probe: the test package, on a release root",
                "system/extra: only on the release root",
            ]
        );
        assert_eq!(unhashed("store/0a1-coreutils-0.1.0/bin/x"), "store/coreutils-0.1.0/bin/x");
        assert_eq!(unhashed("store/not-hex-0.1.0"), "store/not-hex-0.1.0", "no hash to take out");
        assert_eq!(unhashed("system/users"), "system/users");
        // A name that only begins like the package is not in it.
        test.insert("store/abc-test-0.1.0x".into(), file(7));
        assert_eq!(root_divergence(&release2, &test, pkg).len(), 3);
    }

    /// The declarations' names in file order, each with whether it says `critical = true`.
    fn declared(text: &str) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = Vec::new();
        for line in text.lines().map(str::trim) {
            if let Some(name) = line.strip_prefix("[service.").and_then(|r| r.strip_suffix(']')) {
                if !name.contains('.') {
                    out.push((name.to_string(), false));
                }
            } else if line == "critical = true" {
                out.last_mut().expect("a key before any table").1 = true;
            }
        }
        out
    }

    /// **The emergency shell and the terminal server never hold the console together**
    /// (`console-and-tty.md` § *`eshell` is separate*). `service-mgr` asks `init` for the shell
    /// only when a `critical` server does not come up at boot, so the invariant is this file's
    /// order: every critical server is declared before `tty-server`, and so has failed, if it
    /// fails, before anything holds the console. Checked in every image's declarations.
    #[test]
    fn every_critical_server_is_declared_before_the_terminal_server() {
        // **What ships, from the one function that builds it** (PR #340 review, finding 7): a copy
        // assembled here missed `HEARTBEAT_TOML` the day E.1c put it between the two.
        let images = [
            ("release", services_toml(BuildMode::Normal)),
            ("test", services_toml(BuildMode::TestHarness)),
            ("bench", services_toml(BuildMode::Bench)),
        ];
        for (image, text) in &images {
            let decls = declared(text);
            let tty = decls.iter().position(|(n, _)| n == "tty-server").expect("tty-server is declared");
            let critical: Vec<usize> = (0..decls.len()).filter(|&i| decls[i].1).collect();
            assert!(!critical.is_empty(), "{image}: no critical server is declared at all");
            for i in critical {
                assert!(i < tty, "{image}: '{}' is critical and declared after tty-server", decls[i].0);
            }
        }
        // The scan reads what it is aimed at: a critical declaration moved past the terminal
        // server is found.
        let moved = format!("{SERVICES_TOML}[service.late]\nexecutable = \"/bin/late\"\ncritical = true\n");
        let decls = declared(&moved);
        assert_eq!(decls.last(), Some(&("late".to_string(), true)));
    }
}

#[cfg(test)]
mod diag_tests {
    use super::*;

    /// The listing counts only after the start — the check `check-login`'s step 13 makes, and the
    /// one a whole-transcript search could not fail (PR #314 review, blocking 2).
    #[test]
    fn a_browser_started_at_a_place_must_list_it_afterwards() {
        let p = "/home/Documents";
        // The sidebar step's listing, then a browser that started there and went home.
        let went_home = "nxfiles: listed /home/Documents - 2 entries\r\n\
                         nxfiles: starting at /home/Documents\r\n\
                         nxfiles: listed /home - 5 entries\r\n";
        assert!(started_and_listed(went_home, p).is_err(), "the earlier listing must not count");
        let honest = "nxfiles: listed /home/Documents - 2 entries\n\
                      nxfiles: starting at /home/Documents\n\
                      desktop-shell: placed window 20 at 24,54\n\
                      nxfiles: listed /home/Documents - 2 entries\n";
        assert_eq!(started_and_listed(honest, p), Ok(()));
        // A deeper directory is not this one, at either end.
        let deeper = "nxfiles: starting at /home/Documents/sub\nnxfiles: listed /home/Documents - 1 entries\n";
        assert!(started_and_listed(deeper, p).is_err());
        assert!(started_and_listed("nxfiles: listed /home/Documents - 1 entries\n", p).is_err());
    }

    #[test]
    fn the_derived_click_points_at_the_old_size_are_the_literals_they_replaced() {
        // The gates aimed at these by hand at 1280x800; deriving them from the size must give the
        // same answer there, or the move to 1360x768 changed what a gate clicks and not just where.
        //
        // **Except by the bar's height**, which the desktop refresh's Part C changed on purpose:
        // the bars are 30 pixels since, so every aim at the bottom bar is six higher than the
        // literal it replaced (776 and 788 were 24-pixel answers).
        let old = DisplaySize::parse("1280x800").unwrap();
        assert_eq!(old.corner(), (1279, 799));
        assert_eq!(old.centre(), (640, 400));
        assert_eq!((old.bottom_bar_y(), old.bottom_bar_click_y()), (770, 785));
        assert_eq!(old.desktop_name_click(), (1250, 785));
        assert_eq!(
            (old.task_click(0), old.task_click(1), old.show_desktop_click()),
            ((141, 785), (331, 785), (20, 785))
        );
        assert_eq!(old.card_w(), 293, "the design's 330 on 1440, at this width");
        assert_eq!(old.pin_motions(), 20, "the pin the gates always used at this size");
        let big = DisplaySize::parse("2560x1440").unwrap();
        assert!(big.pin_motions() * 100 > big.w as usize, "a pin that cannot cross the screen");
        let gate = DisplaySize::GATE;
        assert_eq!((gate.bottom_bar_y(), gate.desktop_name_click()), (738, (1330, 753)));
        assert_eq!((gate.card_w(), gate.card_cols()), (311, 3));
    }

    #[test]
    fn a_width_qemu_would_shear_is_refused_and_says_what_it_would_show() {
        // The number is computed, so it is asserted where it sits in the sentence: the message
        // also names 1360 in its closing clause, which a bare `contains("1360")` would match
        // whatever width was computed. 1370 is a second width whose answer appears nowhere else.
        for (text, scanned, drawn) in [("1366x768", 1360, 1366), ("1370x768", 1368, 1370)] {
            let e = DisplaySize::parse(text).unwrap_err().to_string();
            let says = format!("scan out {scanned} while the guest draws {drawn}");
            assert!(e.contains(&says), "the refusal of {text} names what QEMU would scan out: {e}");
        }
        assert!(DisplaySize::parse("1360x768").is_ok(), "control: a multiple of 8 is taken");
        for bad in ["1360", "x768", "1360x", "wide", "13 60x768", "320x200", "8192x768"] {
            assert!(DisplaySize::parse(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn the_live_menu_boots_the_release_entry_by_default_and_offers_the_report_and_the_installer() {
        let base = fs::read_to_string(limine_conf()).expect("boot/limine.conf is readable");
        let conf = live_limine_conf(&base).expect("the shipped limine.conf is one entry");
        assert_eq!(
            conf,
            "timeout: 5\n\
             \n\
             /Nitrox\n\
             \x20   protocol: limine\n\
             \x20   path: boot():/boot/kernel\n\
             \x20   module_path: boot():/boot/initramfs\n\
             \x20   module_path: boot():/boot/root.img\n\
             \n\
             /Nitrox — hardware report\n\
             \x20   protocol: limine\n\
             \x20   cmdline: hwreport\n\
             \x20   path: boot():/boot/kernel\n\
             \x20   module_path: boot():/boot/initramfs\n\
             \x20   module_path: boot():/boot/root.img\n\
             \n\
             /Nitrox — install to this machine\n\
             \x20   protocol: limine\n\
             \x20   path: boot():/boot/kernel\n\
             \x20   module_path: boot():/boot/initramfs\n\
             \x20   module_path: boot():/boot/root.img\n\
             \x20   module_path: boot():/boot/install-esp.img\n\
             \x20   module_path: boot():/boot/install-root.img\n"
        );
        // **No entry says `install`** (administration Part G.3): nothing reads it now, and a
        // session is ordinary whichever entry booted. The report entry's `hwreport` is the only
        // command line.
        let first = conf.split("\n\n").nth(1).expect("the default entry");
        assert!(!first.contains("cmdline:"), "the default entry must pass nothing: {first:?}");
        assert_eq!(conf.matches("cmdline:").count(), 1, "hwreport alone: {conf}");
        // **And carries no installable ESP**, which is what keeps it as cheap to boot as it was:
        // the module is 33 MiB of firmware reads and of permanently-held RAM, and an ordinary
        // session could not reach the device it becomes anyway.
        assert_eq!(
            conf.matches("install-esp.img").count(),
            1,
            "only the installer entry loads the installable ESP: {conf}"
        );
        // **Nor the pristine root** (administration Part G.1), for the same reason.
        assert_eq!(
            conf.matches("install-root.img").count(),
            1,
            "only the installer entry loads the pristine root: {conf}"
        );
    }

    #[test]
    fn the_live_menu_refuses_a_base_it_would_have_to_guess_about() {
        let one = "timeout: 0\n\n/Nitrox\n    protocol: limine\n    path: boot():/boot/kernel\n    module_path: boot():/boot/initramfs\n";
        assert!(live_limine_conf(one).is_ok(), "control: the one-entry shape is accepted");
        assert!(live_limine_conf(&one.replace("timeout: 0", "timeout: 3")).is_err());
        assert!(live_limine_conf(&format!("{one}\n/Other\n    protocol: limine\n")).is_err());
        assert!(live_limine_conf(&one.replace("    module_path: boot():/boot/initramfs\n", "")).is_err());
    }

    /// **The scratch module lands in the one entry, after the initramfs**, and the shipped conf is
    /// the shape that allows it. The kernel publishes every module after the first as a RAM disk,
    /// so the order is what keeps the initramfs the initramfs.
    #[test]
    fn a_test_conf_loads_the_scratch_disk_after_the_initramfs() {
        let base = fs::read_to_string(limine_conf()).expect("boot/limine.conf reads");
        let conf = test_limine_conf(&base).expect("the shipped limine.conf is one entry");
        let initramfs = conf.find("boot():/boot/initramfs").expect("the initramfs is loaded");
        let scratch = conf.find("boot():/boot/scratch.img").expect("the scratch disk is loaded");
        assert!(initramfs < scratch, "the initramfs is module 0: {conf}");
        assert_eq!(conf.matches("module_path:").count(), 2);
        let one = "timeout: 0\n\n/Nitrox\n    protocol: limine\n    path: boot():/boot/kernel\n    module_path: boot():/boot/initramfs\n";
        assert!(test_limine_conf(one).is_ok(), "control: the one-entry shape is accepted");
        assert!(test_limine_conf(&format!("{one}\n/Other\n    protocol: limine\n")).is_err());
        assert!(test_limine_conf(&one.replace("    module_path: boot():/boot/initramfs\n", "")).is_err());
    }

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
        assert_eq!(parse_dialog_placement(" 21 of window 20 at 490,214 340x132"), Some((21, 20, 490, 214, 340, 132)));
        // A negative origin is legal: the clamp keeps a dialog inside the work area, and the
        // work area's own origin is not the screen's.
        assert_eq!(parse_dialog_placement("3 of window 2 at -4,24 100x50"), Some((3, 2, -4, 24, 100, 50)));
        // And the shapes that must not silently parse as something else.
        assert_eq!(parse_dialog_placement("21 at 490,214 340x132"), None, "no parent");
        assert_eq!(parse_dialog_placement("21 of window 20 at 490 340x132"), None, "no comma");
        assert_eq!(parse_dialog_placement(""), None);
    }

    #[test]
    fn a_taskbar_slot_is_the_position_of_the_id_not_the_id() {
        // **The distinction the gate depends on.** Ids are not slots — a window closed earlier
        // in the run leaves the ones after it at lower positions — and aiming by id would land
        // on somebody else's entry as soon as anything had ever been closed.
        let list = "Desktop 1 of 2 [20:nxfiles] [24:notes.txt*] [31:untitled]";
        assert_eq!(taskbar_slot(list, 20), Some(0));
        assert_eq!(taskbar_slot(list, 24), Some(1));
        assert_eq!(taskbar_slot(list, 31), Some(2));
        assert_eq!(taskbar_slot(list, 21), None, "a window on another desktop has no slot");
        // A title that contains a digit must not be mistaken for an id: the match is on the
        // group's own `id:` prefix, not on the group containing the number anywhere.
        assert_eq!(taskbar_slot("Desktop 1 of 1 [7:file20.txt]", 20), None);
        assert_eq!(taskbar_slot("Desktop 1 of 1 (empty)", 20), None);
    }

    /// A QMP server stand-in on a fresh socket: it writes `opening`, then answers one command
    /// with an empty `return`, as QEMU answers `qmp_capabilities`.
    fn fake_qmp(opening: String) -> std::path::PathBuf {
        use std::io::{BufRead as _, Write as _};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("xtask-qmp-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(opening.as_bytes()).unwrap();
            let mut command = String::new();
            let _ = std::io::BufReader::new(s.try_clone().unwrap()).read_line(&mut command);
            let _ = s.write_all(b"{\"return\": {}}\n");
        });
        path
    }

    /// **An event before the greeting is skipped, not taken for it** — CI's Display gate on
    /// 2026-09-23, whose first line was a `RESUME` event. At the bound's neighbour: as many
    /// events as it allows still connect, and one more fails as a socket that never greets.
    #[test]
    fn qmp_connect_reads_past_events_to_the_greeting() {
        const GREETING: &str = "{\"QMP\": {\"version\": {}, \"capabilities\": []}}\n";
        const RESUME: &str = "{\"timestamp\": {\"seconds\": 1, \"microseconds\": 2}, \"event\": \"RESUME\"}\n";
        let connect = |events: usize| super::Qmp::connect(&fake_qmp(format!("{}{GREETING}", RESUME.repeat(events))));
        assert!(connect(0).is_ok(), "the ordinary order");
        assert!(connect(1).is_ok(), "an event first");
        assert!(connect(super::MAX_EVENTS_BEFORE_GREETING).is_ok(), "as many as allowed");
        let e = connect(super::MAX_EVENTS_BEFORE_GREETING + 1).err().expect("one too many");
        assert!(e.to_string().contains("unexpected QMP greeting"), "{e}");
    }

    /// **A character split across two reads arrives whole** — the failure that made
    /// `test-interactive` wait for `sleep — exited` with `sleep �� exited` in its transcript.
    #[test]
    fn a_character_split_across_reads_is_decoded_whole() {
        let line = "sleep — exited\n".as_bytes();
        let dash = line.iter().position(|&b| b == 0xE2).unwrap();
        let mut pending = Vec::new();
        pending.extend_from_slice(&line[..dash + 1]); // the read ends one byte into the dash
        let first = super::take_utf8(&mut pending);
        assert_eq!(first, "sleep ");
        assert_eq!(pending, [0xE2], "the dash's first byte waits for the rest");
        pending.extend_from_slice(&line[dash + 1..]);
        let second = super::take_utf8(&mut pending);
        assert_eq!(format!("{first}{second}"), "sleep — exited\n");
        assert!(pending.is_empty());
        // Bytes that are not UTF-8 at all still decode, lossily, rather than being held forever.
        let mut bad = vec![b'a', 0xFF, b'b'];
        assert_eq!(super::take_utf8(&mut bad), "a\u{FFFD}b");
        assert!(bad.is_empty());
    }
    /// **The last window list is a whole line's** (Phase 6 Part B.7): a list cut off as it arrived is
    /// passed over for the whole one before it — the transcript `check-login --usb` kept when it
    /// failed — and a transcript with no whole list has none.
    #[test]
    fn the_last_window_list_is_a_whole_lines() {
        let kept = "desktop-shell: placed window 24 at 24,150\n\
                    desktop-shell: window list on cli of 3 [21:  Files] [23:  notes.txt] [24:> window 24]\n\
                    nxedit: opened another window\n\
                    desktop-shell: window list on cli of 3 [21:  Files] [23:  notes.t";
        assert_eq!(last_window_list(kept), Some("3 [21:  Files] [23:  notes.txt] [24:> window 24]"));
        assert_eq!(last_window_list(kept).map(|l| l.matches('[').count()), Some(3));
        let ended = format!("{kept}xt] [24:  window 24]\n");
        assert_eq!(last_window_list(&ended), Some("3 [21:  Files] [23:  notes.txt] [24:  window 24]"));
        assert_eq!(last_window_list("desktop-shell: window list on cli of 3 [21:  Files]"), None);
    }
}
