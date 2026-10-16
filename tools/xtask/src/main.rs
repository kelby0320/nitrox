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
