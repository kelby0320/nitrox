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
