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


/// The file `check-storage` writes, at the root of the SATA disk's `nitrox-root`.
const STORAGE_PATTERN_FILE: &str = "check-storage.bin";
/// The pattern `test-pattern` writes, **written down a second time here** so the gate does not
/// take its aim from the program under test: `userspace/test-harness/src/pattern.rs`'s `LEN` and
/// `byte` must match these.
const STORAGE_PATTERN_LEN: usize = 3 * 4096 + 1234;

/// Byte `i` of [`STORAGE_PATTERN_LEN`]'s pattern.
fn storage_pattern_byte(i: usize) -> u8 {
    ((i >> 12) as u8).wrapping_mul(0x5B) ^ (i as u8) ^ 0xA5
}

/// `cargo xtask check-storage` — **the storage chain, with the host holding the result**
/// (administration Part C.8).
///
/// The test live image boots as a USB stick beside **a copy of the release disk** on the AHCI
/// controller: the laptop with Nitrox installed and a stick in it. That is the one topology with a
/// second disk the host can read afterwards, since the only other second disk a boot can have is a
/// RAM disk, whose writes never reach a host file.
///
/// The copy's root is first marked **not cleanly unmounted**, as an installed machine's is until
/// Part E's `shutdown`. On the serial console:
/// - the storage service reports it so and auto-mounts it read-only, it being a live boot, and a
///   write there is refused;
/// - the table's `clean` column says no, and still says no after the read-only unmount, which
///   wrote nothing;
/// - `with admin disk` mounts it writable, and `test-pattern` writes a pattern through a mapping
///   and **exits without a sync**. The host sees the file on the disk without the pattern;
/// - `test-pattern --check` reads it back through `/storage`, and `with admin disk --unmount`
///   runs the chain, after which the table says clean.
///
/// Then the machine is stopped, and the host carves `nitrox-root` out of the disk: `e2fsck -fn`
/// clean, the superblock marked clean, and the file holding the pattern — read with `debugfs`,
/// not with the library that wrote it.
fn cmd_check_storage(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    require_tool("e2fsck")?;
    require_tool("debugfs")?;
    require_tool("fsck.fat")?;
    // **A copy of the release disk, made fresh**, so a run cannot pass on what an earlier one
    // wrote. The release image is built first: building the test stick rebuilds the programs
    // for its own mode.
    cmd_image(BuildMode::Normal)?;
    let work = build_cache().join("check-storage");
    fs::create_dir_all(&work)?;
    let disk = work.join("disk.img");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    mark_root_not_clean(&disk)?;
    add_internal_fat(&disk)?;
    cmd_image_live_for(BuildMode::Selftest)?;
    // **Two sticks to plug in** (Phase 6 Part D): an MBR one holding an ext4 partition, which is
    // ejected and read on the host, and a whole-disk ext4, which is pulled while mounted.
    let stick_mbr = work.join("stick-mbr.img");
    mbr_ext4_stick(&stick_mbr, 48, [STICK_MBR_LABEL, STICK_MBR_LABEL_2])?;
    let stick_whole = work.join("stick-whole.img");
    whole_ext4_stick(&stick_whole, 16, STICK_WHOLE_LABEL)?;
    // **And a FAT stick** (Phase 6 Part E.6), for `fs-server-fat`.
    let stick_fat = work.join("stick-fat.img");
    fat32_stick(&stick_fat, &work)?;
    // **And a blank one** (Phase 6 Part G), 2 GiB and sparse, partitioned and formatted in the guest.
    let stick_blank = work.join(BLANK_STICK);
    let _ = fs::remove_file(&stick_blank);
    fs::File::create(&stick_blank).and_then(|f| f.set_len(2 << 30))?;
    let qmp_sock = work.join("qmp.sock");
    let _ = fs::remove_file(&qmp_sock);

    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", qmp_sock.display()))
        .arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=stick,format=raw,file={}", live_test_image_path().display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick")
        .arg("-drive")
        .arg(format!("if=none,id=disk,format=raw,file={}", disk.display()))
        .arg("-device")
        .arg("ide-hd,drive=disk,bus=ide.0")
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
    println!("xtask: storage gate — booting the test live image beside a copy of the release disk…\n");
    let mut session = Session::spawn(cmd, "check-storage")?;
    let result = run_storage_steps(&mut session, &disk, &work).and_then(|()| {
        let mut qmp = Qmp::connect(&qmp_sock)?;
        run_stick_steps(&mut session, &mut qmp)?;
        run_format_steps(&mut session, &mut qmp, &work)
    });
    let transcript = session.finish();
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    println!("\nxtask: the machine is stopped; the disk, on the host:");
    check_storage_disk(&disk, &work)?;
    println!("\nxtask: and the sticks:");
    check_sticks(&stick_mbr, &work.join("stick-whole-pulled.img"), &work)?;
    check_fat_stick(&stick_fat, &work)?;
    println!("\nxtask: and the stick partitioned and formatted in the guest:");
    check_formatted_stick(&work.join(BLANK_STICK_EXT4), &stick_blank, &work)?;
    println!(
        "\nxtask: a file written through a mapping and never synced reached the disk through an \
         unmount, and the filesystem was left clean ✓"
    );
    Ok(())
}

/// The labels of `check-storage`'s two sticks (Phase 6 Part D).
const STICK_MBR_LABEL: &str = "nxstick";
/// The MBR stick's second partition's label (PR #367 review): an eject takes the whole stick.
const STICK_MBR_LABEL_2: &str = "nxstick2";
/// The MBR stick's two partitions, as sectors: the first 31 MiB from 1 MiB, the second 15 MiB from
/// 32 MiB, on a 48 MiB stick.
const STICK_MBR_PARTS: [(u32, u32); 2] = [(2048, 63_488), (65_536, 30_720)];
const STICK_WHOLE_LABEL: &str = "nxwhole";

/// `mke2fs`'s features for a filesystem `fs-server-ext4` serves: the live image's root's.
const SERVED_EXT4_FEATURES: &str = "^has_journal,^64bit,^metadata_csum,^resize_inode";

/// **An ext4 of `blocks` 4 KiB blocks labelled `label`, made in the file `path`.**
fn served_ext4(path: &Path, blocks: u64, label: &str) -> R<()> {
    require_tool("mke2fs")?;
    run(Command::new("mke2fs")
        .arg("-q")
        .arg("-F")
        .arg("-t")
        .arg("ext4")
        .arg("-O")
        .arg(SERVED_EXT4_FEATURES)
        .arg("-b")
        .arg("4096")
        .arg("-L")
        .arg(label)
        .arg(path)
        .arg(blocks.to_string()))
}

/// **A stick with an MBR naming one Linux partition**, `mib` MiB, the partition from 1 MiB to the
/// end holding an ext4 labelled `label` (Phase 6 Part D).
///
/// **Two partitions since PR #367's review**, an ext4 in each of [`STICK_MBR_PARTS`], so an eject
/// named for one is seen to take the other.
fn mbr_ext4_stick(path: &Path, mib: u64, labels: [&str; 2]) -> R<()> {
    let f = fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    f.set_len(mib * 1024 * 1024)?;
    drop(f);
    let parts = STICK_MBR_PARTS.map(|(first, count)| (first, count, 0x83));
    write_mbr_parts(path, &parts)?;
    for ((first, count), label) in STICK_MBR_PARTS.into_iter().zip(labels) {
        let fs_img = path.with_extension(format!("{label}.ext4"));
        let _ = fs::remove_file(&fs_img);
        served_ext4(&fs_img, count as u64 * 512 / 4096, label)?;
        splice_into(path, first as u64 * 512, &fs_img)?;
    }
    Ok(())
}

/// **A stick that is a filesystem**: `mib` MiB of ext4 labelled `label`, with no table at all.
fn whole_ext4_stick(path: &Path, mib: u64, label: &str) -> R<()> {
    let _ = fs::remove_file(path);
    served_ext4(path, mib * 256, label)
}

/// The FAT stick's label, and the names the host puts on it (Phase 6 Part E.6): a long name, a
/// Unicode one, a file two directories down, and one the guest removes. Beside them, the pattern
/// at [`STORAGE_PATTERN_FILE`], which the guest reads back.
const STICK_FAT_LABEL: &str = "NXFAT";
const FAT_LONG_NAME: &str = "a long file name from the host.txt";
const FAT_UNICODE_NAME: &str = "naïve résumé 名前.txt";
const FAT_NESTED_DIR: &str = "nested/deeper";
const FAT_NESTED_NAME: &str = "inner file.txt";
const FAT_DOOMED: &str = "REMOVE.ME";
/// What the guest makes on the FAT stick: a directory, a copy of the Unicode-named file into it
/// under a longer Unicode name built from its own, the long-named file renamed into it, and a file
/// written through a mapping. **The directory's name has no space**, so a path through it is one
/// bareword: a parenthesised path straight after a command's name reads as a call to it
/// (`userspace/nxsh/src/parse.rs`), so `copy` and `rename` lead with `--force` when theirs must be
/// computed.
const FAT_MADE_DIR: &str = "made-by-nitrox";
const FAT_RENAMED: &str = "renamed from the host.txt";
const FAT_WRITTEN: &str = "written-by-nitrox.bin";
/// The internal FAT: the third partition's name and its filesystem's label.
const INTERNAL_FAT_PARTLABEL: &str = "internal-fat";
const INTERNAL_FAT_LABEL: &str = "NXINTERNAL";

/// The host's bytes for each file it puts on the FAT stick: told apart by name.
fn fat_host_bytes(name: &str) -> Vec<u8> {
    match name {
        FAT_LONG_NAME => (0..9_000u32).map(|i| (i % 251) as u8).collect(),
        FAT_UNICODE_NAME => b"unicode, from the host\n".to_vec(),
        FAT_NESTED_NAME => b"two directories down\n".to_vec(),
        STORAGE_PATTERN_FILE => (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect(),
        _ => b"the guest removes this\n".to_vec(),
    }
}

/// An mtools command, with names in UTF-8 and no complaint about a geometry mtools did not choose.
fn mtools_cmd(tool: &str) -> Command {
    let mut c = Command::new(tool);
    c.env("MTOOLS_SKIP_CHECK", "1").env("LC_ALL", "C.UTF-8");
    c
}

/// **A FAT stick for `fs-server-fat`** (Phase 6 Part E.6): 300 MiB, sparse, an MBR naming one FAT32
/// partition from 1 MiB, formatted `mformat -F -c 8` — **FAT32 by its cluster count, with 4 KiB
/// clusters, and its data region off a 4 KiB boundary**, which is the case maps in sectors exist
/// for and is asserted here from the boot sector, so a formatter that aligned it would fail the
/// gate before the boot. Then the host's files: [`fat_host_bytes`]'s, by `mcopy`.
fn fat32_stick(path: &Path, work: &Path) -> R<()> {
    use std::io::{Read, Seek, SeekFrom};
    for tool in ["mformat", "mcopy", "mmd"] {
        require_tool(tool)?;
    }
    const MIB: u64 = 1024 * 1024;
    let _ = fs::remove_file(path);
    fs::File::create(path)?.set_len(300 * MIB)?;
    let count = (300 * 2048 - 2048) as u32;
    write_mbr(path, 2048, count, 0x0C)?;
    let at = format!("{}@@{MIB}", path.display());
    run(mtools_cmd("mformat")
        .arg("-i")
        .arg(&at)
        .arg("-F")
        .arg("-c")
        .arg("8")
        .arg("-T")
        .arg(count.to_string())
        .arg("-v")
        .arg(STICK_FAT_LABEL)
        .arg("::"))?;
    let mut boot = [0u8; 512];
    let mut f = fs::File::open(path)?;
    f.seek(SeekFrom::Start(MIB))?;
    f.read_exact(&mut boot)?;
    let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u64;
    let fat_sectors = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]) as u64;
    let data = 2048 + reserved + boot[16] as u64 * fat_sectors;
    let clusters = (count as u64 + 2048 - data) / boot[13] as u64;
    if data.is_multiple_of(8) || boot[13] != 8 || clusters < 65_525 {
        return Err(format!(
            "the FAT stick's data region is at sector {data} with {} sectors a cluster and {clusters} \
             clusters: the gate needs FAT32 by count, 4 KiB clusters, and data off a 4 KiB boundary",
            boot[13]
        )
        .into());
    }
    let src = work.join("fat-src");
    let _ = fs::remove_dir_all(&src);
    fs::create_dir_all(&src)?;
    run(mtools_cmd("mmd").arg("-i").arg(&at).arg("::nested").arg(format!("::{FAT_NESTED_DIR}")))?;
    for (name, dest) in [
        (FAT_LONG_NAME, FAT_LONG_NAME.to_string()),
        (FAT_UNICODE_NAME, FAT_UNICODE_NAME.to_string()),
        (FAT_NESTED_NAME, format!("{FAT_NESTED_DIR}/{FAT_NESTED_NAME}")),
        (STORAGE_PATTERN_FILE, STORAGE_PATTERN_FILE.to_string()),
        (FAT_DOOMED, FAT_DOOMED.to_string()),
    ] {
        let file = src.join(name);
        fs::write(&file, fat_host_bytes(name))?;
        run(mtools_cmd("mcopy").arg("-i").arg(&at).arg(&file).arg(format!("::{dest}")))?;
    }
    println!(
        "xtask: the FAT stick: FAT32 of {clusters} 4 KiB clusters, its data from sector {data}, off \
         a 4 KiB boundary ✓"
    );
    Ok(())
}

/// **An internal disk's FAT** (Phase 6 Part E.6): a third partition on the copy of the release
/// disk, grown for it — 64 MiB of FAT32 with 4 KiB clusters, which `fs-server-fat` would serve,
/// on a disk that is not removable, so nothing mounts it by itself. On the same disk because the
/// AHCI driver binds the first SATA disk it finds.
fn add_internal_fat(disk: &Path) -> R<()> {
    require_tool("sgdisk")?;
    require_tool("mformat")?;
    const MIB: u64 = 1024 * 1024;
    let len = fs::metadata(disk)?.len();
    fs::OpenOptions::new().write(true).open(disk)?.set_len(len + 65 * MIB)?;
    // The backup GPT to the new end, then the partition in the space between.
    run(Command::new("sgdisk").arg("-e").arg(disk))?;
    run(Command::new("sgdisk")
        .arg("-n")
        .arg("3:0:0")
        .arg("-t")
        .arg("3:0700")
        .arg("-c")
        .arg(format!("3:{INTERNAL_FAT_PARTLABEL}"))
        .arg(disk))?;
    let (lba, sectors) = partition_extent(disk, 3)?;
    run(mtools_cmd("mformat")
        .arg("-i")
        .arg(format!("{}@@{}", disk.display(), lba * 512))
        .arg("-F")
        .arg("-c")
        .arg("8")
        .arg("-T")
        .arg(sectors.to_string())
        .arg("-v")
        .arg(INTERNAL_FAT_LABEL)
        .arg("::"))
}

/// Mark the disk image's `nitrox-root` **not cleanly unmounted**, as an installed machine's is:
/// nothing unmounts `init`'s root until Part E's `shutdown`, so a live boot on the laptop finds its
/// disk this way. It is also what makes the table's `clean` column mean something here: after the
/// read-only unmount it must still say no, and only the writable one may make it say yes
/// (PR #336 review, finding 1).
fn mark_root_not_clean(disk: &Path) -> R<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let (lba, _) = partition_extent(disk, 2)?;
    let at = lba * 512 + 1024 + 58;
    let mut f = fs::OpenOptions::new().read(true).write(true).open(disk)?;
    let mut b = [0u8; 2];
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(&mut b)?;
    f.seek(SeekFrom::Start(at))?;
    f.write_all(&(u16::from_le_bytes(b) & !EXT4_VALID_FS).to_le_bytes())?;
    Ok(())
}

/// The SATA disk's `nitrox-root`, as it stands in the disk image `disk`, carved into `work`.
fn storage_root_fs(disk: &Path, work: &Path) -> R<PathBuf> {
    let fs_img = work.join("nitrox-root.ext4");
    carve_partition(disk, 2, &fs_img)?;
    Ok(fs_img)
}

/// `path`'s bytes in the ext4 image `fs_img`, read with `debugfs`; `None` if it is not there.
fn debugfs_cat(fs_img: &Path, path: &str) -> R<Option<Vec<u8>>> {
    let out = Command::new("debugfs")
        .arg("-R")
        .arg(format!("cat {path}"))
        .arg(fs_img)
        .output()
        .map_err(|e| format!("run debugfs: {e}"))?;
    if String::from_utf8_lossy(&out.stderr).contains("not found") {
        return Ok(None);
    }
    if !out.status.success() {
        return Err(format!("debugfs could not read {path} in {}", fs_img.display()).into());
    }
    Ok(Some(out.stdout))
}

/// The superblock's `s_state` in the ext4 image `fs_img`: bytes 58–59 of the superblock at 1024.
/// Read here rather than through `fs_server_ext4::ext4::was_left_clean`, which is the code under
/// test.
fn ext4_s_state(fs_img: &Path) -> R<u16> {
    let bytes = fs::read(fs_img)?;
    let at = 1024 + 58;
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| format!("{} has no superblock", fs_img.display()).into())
}

/// `s_state`'s "cleanly unmounted" bit (`EXT4_VALID_FS`) and its error bit (`EXT4_ERROR_FS`).
const EXT4_VALID_FS: u16 = 0x0001;
const EXT4_ERROR_FS: u16 = 0x0002;

fn run_storage_steps(s: &mut Session, disk: &Path, work: &Path) -> R<()> {
    let at = format!("/storage/{ROOT_PARTLABEL}/{STORAGE_PATTERN_FILE}");
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();

    // 0. The menu's countdown boots its first entry, the ordinary one.
    s.expect(", cmdline \"\"")?;

    // 1. **The disk's root is auto-mounted read-only**, the boot being a live one. Its name is
    //    what the storage service calls it, read off the line that reports it.
    let reported = format!("(partition {ROOT_PARTLABEL}): ext4");
    s.expect(&reported)?;
    let line = s.matched_line()?;
    let name = line
        .split_whitespace()
        .find(|w| w.starts_with("blk-"))
        .ok_or_else(|| format!("no `blk-<n>` in {line:?}"))?
        .to_string();
    let index = name.trim_start_matches("blk-").to_string();
    if !line.contains(&format!("ext4, not left clean; mounted at /storage/{ROOT_PARTLABEL} (ro)")) {
        return Err(format!(
            "the disk's {ROOT_PARTLABEL} is not reported not clean and auto-mounted read-only: \
             {line:?}. The gate cleared its clean bit, and a live boot mounts the machine's own \
             disks read-only"
        )
        .into());
    }
    s.expect("storage-service: a live boot")?;
    println!(
        "  ok: {name}, the disk's {ROOT_PARTLABEL}, not left clean and auto-mounted read-only on a \
         live boot"
    );

    // 1b. **`boot-probe` has finished, before anything is typed** (Phase 6 Part D). It starts once
    //     the test harness's chain has, and the sticks made this gate long enough to meet it: a
    //     `--kvm` run had it starting while a stick was pulled. Its later tests open view-broker
    //     clients until the broker refuses one, and install a policy, either of which a `with
    //     admin` here could meet halfway. **Its verdict is not this gate's**: on this machine it
    //     takes the SATA disk's `nitrox-root` for the root it wrote, and finds no scratch disk, so
    //     it fails. It writes no disk directly, so the one the host checks is still the gate's.
    //     Waited for as `check-shutdown` waits, whatever its code.
    if !s.expect_within("service-mgr: 'boot-probe' exited code=", std::time::Duration::from_secs(300))? {
        return Err("`boot-probe` did not exit within five minutes of the storage service's report".into());
    }
    println!("  ok: boot-probe has finished, so nothing of it overlaps what is typed");

    // 2. A serial login. The prompt is searched for in the whole transcript, as `check-live`
    //    does: it and the greeter come up together.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !s.transcript().contains("nitrox login:") {
        if std::time::Instant::now() > deadline {
            return Err("no `nitrox login:` prompt on the serial column".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    // **The table's `clean` column, as a session reads it** — a token the typed command does not
    // contain, so the match is the answer and not the echo. The row is found by the device's name:
    // `label` is the filesystem's own, and this one has none.
    let clean_now = |s: &mut Session, expected: bool| -> R<()> {
        s.send(&format!(
            "disk --list | filter name == \"{name}\" | map {{ |r| format(\"clean-now={{}}\", r.clean) }}"
        ))?;
        s.expect(&format!("clean-now={expected}"))?;
        s.expect("/home>")?;
        Ok(())
    };
    clean_now(s, false)?;

    // 3. **A write there is refused**, by the read-only mount: named, rather than any failure.
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: {at} refused: NoAccess (-2)"))?;
    s.expect("/home>")?;
    println!("  ok: a write to it is refused NoAccess");
    //    **And a session's eject of it is refused** (Phase 6 Part F): an internal disk is the
    //    `storage` grant's to unmount, and the refusal says how — with no password asked.
    let from = s.transcript().len();
    s.send(&format!("disk --eject {ROOT_PARTLABEL}"))?;
    s.expect(&format!(
        "disk: {ROOT_PARTLABEL} not ejected: it is not removable: an internal disk is unmounted with the storage \
         grant (with admin disk --unmount)"
    ))?;
    s.expect("/home>")?;
    if s.transcript()[from..].contains("password:") {
        return Err("`disk --eject` asked for a password".into());
    }
    println!("  ok: `disk --eject {ROOT_PARTLABEL}` is refused, naming `with admin disk --unmount`");

    // 4. Unmounted, and mounted writable, through the `storage` grant.
    let admin = |s: &mut Session, command: &str| -> R<()> { with_admin_asked(s, command) };
    admin(s, &format!("disk --unmount {ROOT_PARTLABEL}"))?;
    s.expect("fs-server: unmounted; a read-only mount wrote nothing")?;
    s.expect(&format!("storage-service: unmounted {ROOT_PARTLABEL}, not left clean (read-only, so as it was found)"))?;
    s.expect(&format!("disk: unmounted {ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    // **Still not clean**, and the table says so: it reads the device, not the unmount's word.
    clean_now(s, false)?;
    admin(s, &format!("disk --mount /dev/blk/{index}"))?;
    s.expect(&format!("storage-service: mounted {ROOT_PARTLABEL} (rw), as asked"))?;
    s.expect(&format!("disk: mounted {name} at /storage/{ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    println!("  ok: `with admin disk` unmounted it and mounted it writable");

    // 5. **Written through a mapping, and not synced.**
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!(
        "test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not \
         sync"
    ))?;
    s.expect("/home>")?;

    // 6. **On the host, the file is on the disk and the pattern is not** — so what the last step
    //    finds, the unmount put there. Read while the guest runs: QEMU writes the image file as
    //    the guest writes the disk, and nothing on this filesystem is moving between steps. The
    //    file's being there, at its size, is what makes the pattern's absence mean something:
    //    this is the live filesystem the host is reading, and its data has not been written yet.
    //    And the superblock says it is mounted writable, so a clean one at the end is the
    //    unmount's doing.
    let fs_img = storage_root_fs(disk, work)?;
    let path = format!("/{STORAGE_PATTERN_FILE}");
    match debugfs_cat(&fs_img, &path)? {
        None => {
            return Err(format!(
                "{path} is not on the disk at all after `test-pattern --write`: the server's \
                 create did not reach it, so the absence of the pattern would prove nothing"
            )
            .into());
        }
        Some(b) if b.len() != STORAGE_PATTERN_LEN => {
            return Err(format!(
                "{path} is on the disk at {} bytes, not the {STORAGE_PATTERN_LEN} it was created \
                 with",
                b.len()
            )
            .into());
        }
        Some(b) if b == pattern => {
            return Err(format!(
                "{path} already holds the pattern before any sync or unmount — something wrote it \
                 back, so this gate cannot show that the unmount does"
            )
            .into());
        }
        Some(_) => {}
    }
    let state = ext4_s_state(&fs_img)?;
    if state & EXT4_VALID_FS != 0 {
        return Err(format!(
            "the superblock says clean (s_state {state:#06x}) while the filesystem is mounted \
             writable, so a clean one at the end would not be the unmount's doing"
        )
        .into());
    }
    println!(
        "  ok: on the host meanwhile: the file is there at {STORAGE_PATTERN_LEN} bytes without the \
         pattern, and the superblock says mounted"
    );

    // 7. Read back through `/storage` in the guest: from the kernel's cache, the one object.
    s.send(&format!("test-pattern --check {at}"))?;
    s.expect(&format!("test-pattern: {at} holds the pattern, {STORAGE_PATTERN_LEN} bytes ok"))?;
    s.expect("/home>")?;
    println!("  ok: test-pattern reads it back through /storage");

    // 8. **The unmount**: written back, marked clean, the drive flushed.
    admin(s, &format!("disk --unmount {ROOT_PARTLABEL}"))?;
    s.expect("fs-server: unmounted, and the filesystem recorded clean")?;
    s.expect(&format!("storage-service: unmounted {ROOT_PARTLABEL}, left clean"))?;
    s.expect(&format!("disk: unmounted {ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    // And the table, which said no after the read-only unmount, now says yes.
    clean_now(s, true)?;
    println!("  ok: `with admin disk --unmount` ran the chain, and the table says clean");
    Ok(())
}

/// **Sticks that come and go** (Phase 6 Part D), after the disk's steps, plugged and pulled over
/// QMP. Each wait reads from the moment of its action, since the kernel, the device manager and the
/// storage service each answer one and their lines land in any order.
///
/// 1. **The boot stick is passed over**: the kernel flags it, and the service says so.
/// 2. **An MBR stick plugged in**: its disk and partition read in the kernel, the partition's ext4
///    auto-mounted **writable**, on a live boot too (Part F), and written through a mapping without
///    a sync. **An eject while a file on it is held is refused**, and `disk --eject` then ejects it
///    with no password. Pulled then, its records depart and the service drops it.
/// 3. **A whole-disk stick pulled while mounted** writable, with a file left dirty: the teardown's
///    write-back and the server's marking each come back at once, the dirty file let go, the server
///    gone, the label gone, and a command after it runs.
/// 4. **The same stick plugged in again** is a new disk at a new index, mounted again.
fn run_stick_steps(s: &mut Session, qmp: &mut Qmp) -> R<()> {
    let secs = std::time::Duration::from_secs;
    let any = |_: &str| true;
    // The whole line holding `pat` after `from`, waited for.
    let line_of = |s: &mut Session, from: usize, pat: &str| -> R<String> {
        s.line_since(from, pat, |_: &str| true, std::time::Duration::from_secs(60))?;
        let t = s.transcript();
        t[from.min(t.len())..]
            .lines()
            .find(|l| l.contains(pat))
            .map(|l| l.trim().to_string())
            .ok_or_else(|| format!("no line holding {pat:?}").into())
    };
    let blk_of = |line: &str| -> R<String> {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix("blk-"))
            .map(|n| n.trim_end_matches(|c: char| !c.is_ascii_digit()).to_string())
            .ok_or_else(|| format!("no `blk-<n>` in {line:?}").into())
    };
    let plug = |qmp: &mut Qmp, id: &str, node: &str| -> R<()> {
        qmp.execute(&format!(
            r#"{{"execute":"device_add","arguments":{{"driver":"usb-storage","id":"{id}","bus":"xhci.0","drive":"{node}"}}}}"#
        ))?;
        Ok(())
    };
    let pull = |qmp: &mut Qmp, id: &str| -> R<()> {
        qmp.execute(&format!(r#"{{"execute":"device_del","arguments":{{"id":"{id}"}}}}"#))?;
        Ok(())
    };
    let add_node = |qmp: &mut Qmp, node: &str, file: &Path| -> R<()> {
        qmp.execute(&format!(
            r#"{{"execute":"blockdev-add","arguments":{{"driver":"raw","node-name":"{node}","file":{{"driver":"file","filename":"{}"}}}}}}"#,
            file.display()
        ))?;
        Ok(())
    };

    // 1. The boot stick: flagged by the kernel, passed over by the service.
    let boot = s.transcript();
    if !boot.contains("gpt: the disk the machine started from") {
        return Err("the kernel did not flag the live stick as the disk the machine started from".into());
    }
    if !boot.lines().any(|l| {
        l.contains("storage-service: blk-") && l.contains("; on the disk the machine started from, passed over")
    }) {
        return Err("the storage service did not say it passed the boot stick over".into());
    }
    println!("  ok: the boot stick is flagged, and passed over");

    // 1b. **The internal FAT is not mounted, and the release disk's ESP not served** (Phase 6 Part
    //     E.6): the first servable and on a disk that is not removable, the second refused for its
    //     512-byte clusters. Read from the boot's report, as the boot stick's line is.
    let internal = format!(
        "(partition {INTERNAL_FAT_PARTLABEL}): fat '{INTERNAL_FAT_LABEL}', left clean; not removable, so not mounted"
    );
    if !boot.lines().any(|l| l.contains("storage-service: blk-") && l.contains(&internal)) {
        return Err(format!("no report of the internal FAT left unmounted: {internal:?}").into());
    }
    let esp =
        "(partition NITROX_ESP): fat 'NITROX_ESP', left clean; not served: 512-byte clusters, smaller than a page";
    if !boot.lines().any(|l| l.contains("storage-service: blk-") && l.contains(esp)) {
        return Err(format!("no report of the release disk's ESP refused for its clusters: {esp:?}").into());
    }
    println!(
        "  ok: the internal FAT is servable and not mounted, not being removable; the ESP is refused for its clusters"
    );

    // 2. The MBR stick.
    let mbr_img = build_cache().join("check-storage").join("stick-mbr.img");
    add_node(qmp, "stickmbr", &mbr_img)?;
    let from = s.transcript().len();
    plug(qmp, "stickmbr", "stickmbr")?;
    let disk_line = line_of(s, from, ": LUN 0: disk \"QEMU QEMU HARDDISK (")?;
    let port = disk_line
        .strip_prefix("usb: port ")
        .and_then(|r| r.split(':').next())
        .ok_or_else(|| format!("no port in {disk_line:?}"))?
        .to_string();
    s.line_since(from, "mbr:  partition 1 lba 2048..", any, secs(60))?;
    let mounted = line_of(
        s,
        from,
        &format!(
            "(partition partition 1 (unlabelled)): ext4 '{STICK_MBR_LABEL}'; mounted at /storage/{STICK_MBR_LABEL} (rw)"
        ),
    )?;
    let index = blk_of(&mounted)?;
    line_of(
        s,
        from,
        &format!(
            "(partition partition 2 (unlabelled)): ext4 '{STICK_MBR_LABEL_2}'; mounted at /storage/{STICK_MBR_LABEL_2} (rw)"
        ),
    )?;
    println!("  ok: an MBR stick plugged in: its two partitions read, each ext4 mounted writable on a live boot");
    // A pattern on each, written through a mapping and not synced, for the eject to write back.
    let ats = [STICK_MBR_LABEL, STICK_MBR_LABEL_2].map(|l| format!("/storage/{l}/{STORAGE_PATTERN_FILE}"));
    for at in &ats {
        s.send(&format!("test-pattern --write {at}"))?;
        s.expect(&format!(
            "test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"
        ))?;
        s.expect("/home>")?;
    }
    // **Refused while a file on the stick is held** — on its *other* partition (PR #367 review):
    // a session's eject takes the whole stick, so a file held on either refuses it, and both stay
    // mounted. `test-pattern` holds the file and asks, since a shell with no background jobs
    // cannot hold one while it runs `disk`.
    s.send(&format!("test-pattern --eject-held {} {STICK_MBR_LABEL}", ats[1]))?;
    s.expect(&format!("storage-service: {STICK_MBR_LABEL_2} is in use: 1 file(s) still open or mapped"))?;
    s.expect(&format!(
        "test-pattern: eject of {STICK_MBR_LABEL} with {} held: refused, a file on it is still open or mapped ok",
        ats[1]
    ))?;
    s.expect("/home>")?;
    // **And nothing was ejected**: the partition named is still mounted, not unmounted before the
    // other one refused. A token the typed line does not hold, so the match is the answer.
    s.send(&format!("format(\"still-mounted={{}}\", (list /storage | filter name == \"{STICK_MBR_LABEL}\" | count))"))?;
    s.expect("still-mounted=1")?;
    s.expect("/home>")?;
    println!("  ok: an eject of one partition while a file on the other is held is refused, and leaves both mounted");
    eject_stick(s, &[STICK_MBR_LABEL, STICK_MBR_LABEL_2])?;
    println!("  ok: written without a sync, and both partitions ejected by one `disk --eject`, no password asked");
    let from = s.transcript().len();
    pull(qmp, "stickmbr")?;
    s.line_since(from, &format!("usb: port {port}: disconnected; slot "), any, secs(30))?;
    s.line_since(from, &format!("storage-service: blk-{index} departed"), any, secs(30))?;
    println!("  ok: pulled, its records departed, and the service let it go");

    // 3. The whole-disk stick, pulled while mounted.
    let whole_img = build_cache().join("check-storage").join("stick-whole.img");
    add_node(qmp, "stickwhole", &whole_img)?;
    let from = s.transcript().len();
    plug(qmp, "stickwhole", "stickwhole")?;
    let disk_line = line_of(s, from, ": LUN 0: disk \"QEMU QEMU HARDDISK (")?;
    let port = disk_line
        .strip_prefix("usb: port ")
        .and_then(|r| r.split(':').next())
        .ok_or_else(|| format!("no port in {disk_line:?}"))?
        .to_string();
    s.line_since(from, "partitions: no table: neither a GPT nor an MBR", any, secs(60))?;
    let mounted =
        line_of(s, from, &format!("): ext4 '{STICK_WHOLE_LABEL}'; mounted at /storage/{STICK_WHOLE_LABEL} (rw)"))?;
    let whole = blk_of(&mounted)?;
    println!("  ok: a whole-disk stick plugged in, read as no table, and its ext4 mounted writable as the disk");
    let at = format!("/storage/{STICK_WHOLE_LABEL}/{STORAGE_PATTERN_FILE}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    let from = s.transcript().len();
    pull(qmp, "stickwhole")?;
    s.line_since(from, &format!("usb: port {port}: disconnected; slot "), any, secs(30))?;
    // **The teardown's I/O to a disk that has gone comes back at once**: the write-back of the
    // file left dirty — whose refusal lets it go — and the server's attempt to record the
    // filesystem clean.
    s.line_since(from, "file: its device has gone; a dirty file and its ", any, secs(30))?;
    s.line_since(from, "fs-server: unmounted, but the filesystem could not be recorded clean", any, secs(30))?;
    s.line_since(
        from,
        &format!("storage-service: {STICK_WHOLE_LABEL} left while mounted; what had not been written back is gone"),
        any,
        secs(30),
    )?;
    s.line_since(from, &format!("storage-service: blk-{whole} departed"), any, secs(30))?;
    // **The stick as the pull left it, copied now**, for the host to find still marked in use: the
    // same stick plugged in again below is mounted writable, which marks it in use whatever the
    // pull did (Phase 6 Part F; read-only until then, which wrote nothing).
    fs::copy(&whole_img, whole_img.with_file_name("stick-whole-pulled.img"))?;
    let from = s.transcript().len();
    s.send("list /storage")?;
    s.expect("/home>")?;
    if s.transcript()[from..].lines().skip(1).any(|l| l.contains(STICK_WHOLE_LABEL)) {
        return Err(format!("/storage still lists {STICK_WHOLE_LABEL} after its stick was pulled").into());
    }
    println!(
        "  ok: pulled while mounted: the dirty file let go, the server gone, the label gone, the session going on"
    );

    // 4. Plugged in again: a new disk, a new index, mounted again.
    let from = s.transcript().len();
    plug(qmp, "stickwhole2", "stickwhole")?;
    let mounted =
        line_of(s, from, &format!("): ext4 '{STICK_WHOLE_LABEL}'; mounted at /storage/{STICK_WHOLE_LABEL} (rw)"))?;
    let again = blk_of(&mounted)?;
    if again.parse::<u32>().ok() <= whole.parse::<u32>().ok() {
        return Err(format!("plugged in again, the stick took blk-{again}, not an index past blk-{whole}").into());
    }
    println!("  ok: plugged in again: blk-{again}, a new index, mounted again");

    // 5. **A FAT stick** (Phase 6 Part E.6), served by `fs-server-fat`: auto-mounted writable, on a
    //    live boot too (Part F), and what the host put there listed and read.
    let fat_img = build_cache().join("check-storage").join("stick-fat.img");
    add_node(qmp, "stickfat", &fat_img)?;
    let from = s.transcript().len();
    plug(qmp, "stickfat", "stickfat")?;
    let mounted = line_of(
        s,
        from,
        &format!(
            "(partition partition 1 (unlabelled)): fat '{STICK_FAT_LABEL}'; mounted at /storage/{STICK_FAT_LABEL} (rw)"
        ),
    )?;
    let fat = blk_of(&mounted)?;
    s.line_since(from, "fs-server: ready (fat, read-write)", any, secs(30))?;
    println!("  ok: a FAT stick plugged in, its FAT32 mounted writable by fs-server-fat at /storage/{STICK_FAT_LABEL}");
    let root = format!("/storage/{STICK_FAT_LABEL}");
    // **What a command printed**: everything up to the prompt after it, its echo and the prompt
    // aside.
    let output = |s: &mut Session, command: &str| -> R<String> {
        let from = s.transcript().len();
        s.send(command)?;
        s.expect("/home>")?;
        let t = s.transcript();
        Ok(t[from.min(t.len())..].lines().skip(1).filter(|l| !l.contains("/home>")).collect::<Vec<_>>().join("\n"))
    };
    let listing = |s: &mut Session, dir: &str| -> R<String> {
        output(s, &format!("list {dir} | map {{ |r| format(\"entry={{}}|{{}}|{{}}\", r.name, r.kind, r.size) }}"))
    };
    let names = listing(s, &root)?;
    for (name, kind, size) in [
        (FAT_LONG_NAME, "file", fat_host_bytes(FAT_LONG_NAME).len()),
        (FAT_UNICODE_NAME, "file", fat_host_bytes(FAT_UNICODE_NAME).len()),
        ("nested", "dir", 0),
        (STORAGE_PATTERN_FILE, "file", STORAGE_PATTERN_LEN),
        (FAT_DOOMED, "file", fat_host_bytes(FAT_DOOMED).len()),
    ] {
        if !names.contains(&format!("entry={name}|{kind}|{size}")) {
            return Err(format!("`list {root}` does not show {name:?}, a {kind} of {size} bytes:\n{names}").into());
        }
    }
    let nested = listing(s, &format!("{root}/{FAT_NESTED_DIR}"))?;
    let want = format!("entry={FAT_NESTED_NAME}|file|{}", fat_host_bytes(FAT_NESTED_NAME).len());
    if !nested.contains(&want) {
        return Err(format!("`list {root}/{FAT_NESTED_DIR}` does not show {want:?}:\n{nested}").into());
    }
    let at = format!("{root}/{STORAGE_PATTERN_FILE}");
    s.send(&format!("test-pattern --check {at}"))?;
    s.expect(&format!("test-pattern: {at} holds the pattern, {STORAGE_PATTERN_LEN} bytes ok"))?;
    s.expect("/home>")?;
    println!(
        "  ok: the host's names listed — long, Unicode, nested — with their sizes, and its pattern read through a mapping"
    );

    // 6. **Written**, with no remount: a directory made, the Unicode-named file copied into it
    //    under a longer name built from its own — the serial line carries ASCII alone, so the name
    //    comes from the listing — the long-named one renamed into it, one removed, and a file
    //    written through a mapping without a sync.
    let made = format!("{root}/{FAT_MADE_DIR}");
    for command in [
        format!("mkdir {made}"),
        format!(
            "for r in (list {root} | filter {{ |r| r.name ~= /^na/ }}) {{ copy --force (format(\"{root}/{{}}\", r.name)) (format(\"{made}/a copy of {{}}\", r.name)) }}"
        ),
        format!("rename --force (\"{root}/{FAT_LONG_NAME}\") (\"{made}/{FAT_RENAMED}\")"),
        format!("remove {root}/{FAT_DOOMED}"),
    ] {
        // A refusal is a line naming the program, or the shell's own; what each prints on success
        // (`mkdir`'s table of what it created, say) is the host's to judge, after the eject.
        let said = output(s, &command)?;
        let program =
            command.split_whitespace().find(|w| ["mkdir", "copy", "rename", "remove"].contains(w)).unwrap_or("nxsh");
        if said.lines().any(|l| l.contains(&format!("{program}: ")) || l.contains("nxsh: ")) {
            return Err(format!("`{command}` was refused:\n{said}").into());
        }
    }
    let at = format!("{root}/{FAT_WRITTEN}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    let made_names = listing(s, &made)?;
    let copied = format!("entry=a copy of {FAT_UNICODE_NAME}|file|{}", fat_host_bytes(FAT_UNICODE_NAME).len());
    if !made_names.contains(&copied) || !made_names.contains(&format!("entry={FAT_RENAMED}|file|")) {
        return Err(format!("`list {made}` is not the copy and the rename:\n{made_names}").into());
    }
    println!(
        "  ok: a directory, a copy to a long Unicode name, a rename and a removal; a file written through a mapping"
    );

    // 7. **Ejected and pulled**: the eject writes back and records the filesystem clean.
    eject_stick(s, &[STICK_FAT_LABEL])?;
    let from = s.transcript().len();
    pull(qmp, "stickfat")?;
    s.line_since(from, &format!("storage-service: blk-{fat} departed"), any, secs(30))?;
    println!("  ok: ejected, recorded clean, and pulled");
    Ok(())
}

/// **The blank stick `check-storage` partitions and formats** (Phase 6 Part G), and the copy of it
/// taken once its ext4 was ejected, before an MBR replaced the GPT.
const BLANK_STICK: &str = "stick-blank.img";
const BLANK_STICK_EXT4: &str = "stick-blank-ext4.img";

/// **The whole line holding `pat`** after `from`, waited for.
fn line_after(s: &mut Session, from: usize, pat: &str) -> R<String> {
    s.line_since(from, pat, |_: &str| true, std::time::Duration::from_secs(120))?;
    let t = s.transcript();
    t[from.min(t.len())..]
        .lines()
        .find(|l| l.contains(pat))
        .map(|l| l.trim().to_string())
        .ok_or_else(|| format!("no line holding {pat:?}").into())
}

/// The `<n>` of the first `blk-<n>` in `line`.
fn blk_in(line: &str) -> R<String> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix("blk-"))
        .map(|n| n.trim_end_matches(|c: char| !c.is_ascii_digit()).to_string())
        .ok_or_else(|| format!("no `blk-<n>` in {line:?}").into())
}

/// **Partitioning and formatting, through `with admin`** (Phase 6 Part G), after the sticks:
///
/// 4. **`disk --list`'s `note`** names the internal FAT's reason, `not removable, so not mounted`.
/// 5. **`disk --format` of a partition of the boot stick** is refused before anything is written,
///    naming the disk the machine started from.
/// 1. **A blank 2 GiB stick plugged in, then `disk --format DISK ext4`**: a GPT — the default for
///    ext4 — with one partition, which the kernel's rescan publishes, an ext4 in it, mounted
///    writable, written and ejected. Copied then, for the host to read the GPT and the ext4.
/// 2. **`disk --partition DISK`**: an MBR, the GPT's partition departed and a new one arrived,
///    holding nothing — the old ext4 at 1 MiB wiped with the table.
/// 3. **`disk --format PART fat`**: a FAT32, read again and mounted writable, written; the same
///    command refused while it is mounted; ejected and pulled. The host checks it after the boot.
///
/// The note and the boot stick come first, since they need no stick of their own.
fn run_format_steps(s: &mut Session, qmp: &mut Qmp, work: &Path) -> R<()> {
    let secs = std::time::Duration::from_secs;
    let any = |_: &str| true;
    let boot = s.transcript();
    let report =
        |part: &str| boot.lines().find(|l| l.contains("storage-service: blk-") && l.contains(part)).map(str::to_string);
    let internal = report(&format!("(partition {INTERNAL_FAT_PARTLABEL}):")).ok_or("no report of the internal FAT")?;
    let internal = blk_in(&internal)?;
    // **A partition of the boot stick**, by its parent in `/dev/devices`: the report names the disk
    // as passed over, and its partition by what is in it — the live stick's ESP, by its clusters.
    let boot_disk = boot
        .lines()
        .find(|l| {
            l.contains("storage-service: blk-")
                && l.contains("(disk ")
                && l.contains("; on the disk the machine started from, passed over")
        })
        .ok_or("no report of the boot stick, passed over")?;
    let boot_disk = blk_in(boot_disk)?;
    s.send(&format!(
        "open /dev/devices/all.tsm | filter parent == \"blk-{boot_disk}\" | map {{ |r| format(\"boot-part=<{{}}>\", r.name) }}"
    ))?;
    s.expect("boot-part=<blk-")?;
    // Read once the prompt is back, so the line is whole.
    s.expect("/home>")?;
    let on_stick = {
        let t = s.transcript();
        let at = t.rfind("boot-part=<blk-").ok_or("no boot-part line")?;
        t[at + "boot-part=<blk-".len()..].split('>').next().unwrap_or("").to_string()
    };

    // 4. The note.
    s.send(&format!(
        "disk --list | filter name == \"blk-{internal}\" | map {{ |r| format(\"note-is={{}}\", r.note) }}"
    ))?;
    s.expect("note-is=not removable, so not mounted")?;
    s.expect("/home>")?;
    println!("  ok: `disk --list`'s note says why the internal FAT is not mounted");

    // 5. The boot stick, refused before anything is written.
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --format blk-{on_stick} fat"))?;
    s.expect(&format!(
        "disk: blk-{on_stick} not formatted: it is on the disk the machine started from; nothing was written"
    ))?;
    s.expect("/home>")?;
    if s.transcript()[from..].contains("disk: wrote ") {
        return Err(format!("`disk --format blk-{on_stick}` wrote to the boot stick").into());
    }
    println!(
        "  ok: a partition of the boot stick is refused before anything is written, naming the disk the machine started from"
    );

    // 1. The blank stick, formatted ext4 whole.
    let blank = work.join(BLANK_STICK);
    qmp.execute(&format!(
        r#"{{"execute":"blockdev-add","arguments":{{"driver":"raw","node-name":"stickblank","file":{{"driver":"file","filename":"{}"}}}}}}"#,
        blank.display()
    ))?;
    let from = s.transcript().len();
    qmp.execute(r#"{"execute":"device_add","arguments":{"driver":"usb-storage","id":"stickblank","bus":"xhci.0","drive":"stickblank"}}"#)?;
    let disk_line = line_after(s, from, "\", 4194304 blocks of 512 bytes, record ")?;
    let port = disk_line
        .strip_prefix("usb: port ")
        .and_then(|r| r.split(':').next())
        .ok_or_else(|| format!("no port in {disk_line:?}"))?
        .to_string();
    let disk = blk_in(&line_after(s, from, "): no filesystem")?)?;
    println!("  ok: a blank 2 GiB stick plugged in: blk-{disk}, holding nothing");
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --format blk-{disk} ext4"))?;
    s.line_since(from, &format!("disk: wrote blk-{disk} ext4 (gpt)"), any, secs(300))?;
    s.line_since(
        from,
        &format!("usb: port {port}: LUN 0: rescanned: 0 partition(s) departed, 1 published"),
        any,
        secs(60),
    )?;
    let mounted = line_after(s, from, "(partition nitrox): ext4 'nitrox'; mounted at /storage/nitrox (rw)")?;
    let gpt_part = blk_in(&mounted)?;
    s.line_since(
        from,
        &format!("disk: blk-{disk} formatted ext4 (gpt): blk-{gpt_part}, mounted at /storage/nitrox"),
        any,
        secs(60),
    )?;
    s.expect("/home>")?;
    println!(
        "  ok: `disk --format blk-{disk} ext4`: a GPT, its partition published by a rescan, an ext4 mounted writable"
    );
    let at = format!("/storage/nitrox/{STORAGE_PATTERN_FILE}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    eject_stick(s, &["nitrox"])?;
    fs::copy(&blank, work.join(BLANK_STICK_EXT4))?;
    println!("  ok: written without a sync, and ejected");

    // 2. Partitioned again, MBR: the GPT's partition goes, and the new one holds nothing.
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --partition blk-{disk}"))?;
    s.line_since(from, &format!("disk: wrote blk-{disk} (mbr)"), any, secs(120))?;
    s.line_since(
        from,
        &format!("usb: port {port}: LUN 0: rescanned: 1 partition(s) departed, 1 published"),
        any,
        secs(60),
    )?;
    s.line_since(from, &format!("storage-service: blk-{gpt_part} departed"), any, secs(60))?;
    let arrived = line_after(s, from, "(partition partition 1 (unlabelled)): no filesystem")?;
    let part = blk_in(&arrived)?;
    s.line_since(from, &format!("disk: blk-{disk} partitioned (mbr): blk-{part}, holding nothing"), any, secs(60))?;
    s.expect("/home>")?;
    println!(
        "  ok: `disk --partition blk-{disk}`: an MBR, the GPT's blk-{gpt_part} departed, blk-{part} arrived holding nothing"
    );

    // 3. The partition formatted FAT, written, refused while mounted, ejected, pulled.
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --format blk-{part} fat"))?;
    s.line_since(from, &format!("disk: wrote blk-{part} fat"), any, secs(300))?;
    line_after(
        s,
        from,
        &format!("blk-{part} (partition partition 1 (unlabelled)): fat 'NITROX'; mounted at /storage/NITROX (rw)"),
    )?;
    s.line_since(from, &format!("disk: blk-{part} formatted fat: mounted at /storage/NITROX"), any, secs(60))?;
    s.expect("/home>")?;
    println!("  ok: `disk --format blk-{part} fat`: read again and mounted writable by fs-server-fat");
    let at = format!("/storage/NITROX/{STORAGE_PATTERN_FILE}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --format blk-{part} fat"))?;
    s.expect(&format!(
        "disk: blk-{part} not formatted: it is mounted, as NITROX: unmount it first (with admin disk --unmount NAME); nothing was written"
    ))?;
    s.expect("/home>")?;
    if s.transcript()[from..].contains("disk: wrote ") {
        return Err(format!("`disk --format blk-{part}` wrote while it was mounted").into());
    }
    println!("  ok: the same command, while it is mounted, is refused before anything is written");
    eject_stick(s, &["NITROX"])?;
    let from = s.transcript().len();
    qmp.execute(r#"{"execute":"device_del","arguments":{"id":"stickblank"}}"#)?;
    s.line_since(from, &format!("storage-service: blk-{part} departed"), any, secs(30))?;
    println!("  ok: written, ejected and pulled");
    Ok(())
}

/// **`len` bytes of `from` at `at`, copied into `to`** — sparsely, since most of a stick is never
/// written.
fn carve_sparse(from: &Path, at: u64, len: u64, to: &Path) -> R<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    const MIB: u64 = 1024 * 1024;
    let mut src = fs::File::open(from)?;
    src.seek(SeekFrom::Start(at))?;
    let mut dst = fs::File::create(to)?;
    let mut buf = vec![0u8; MIB as usize];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(MIB) as usize;
        src.read_exact(&mut buf[..n])?;
        if buf[..n].iter().any(|&b| b != 0) {
            dst.write_all(&buf[..n])?;
        } else {
            dst.seek(SeekFrom::Current(n as i64))?;
        }
        done += n as u64;
    }
    dst.set_len(len)?;
    Ok(())
}

/// **The stick partitioned and formatted in the guest, on the host** (Phase 6 Part G):
/// - **as its ext4 was ejected** (`ext4_copy`): a GPT whose one partition is a Linux filesystem from
///   1 MiB to the last usable sector, holding an ext4 `e2fsck -fn` finds clean, with the pattern
///   `debugfs` reads;
/// - **as it was pulled** (`stick`): an MBR naming one FAT32 partition from 1 MiB to the end, **no
///   GPT left** — neither header, front nor back — and the FAT `fsck.fat -n` finds clean, its
///   pattern read by `mtype`.
fn check_formatted_stick(ext4_copy: &Path, stick: &Path, work: &Path) -> R<()> {
    use std::os::unix::fs::FileExt;
    let sectors = fs::metadata(stick)?.len() / 512;
    let sector = |path: &Path, lba: u64| -> R<[u8; 512]> {
        let mut b = [0u8; 512];
        fs::File::open(path)?.read_exact_at(&mut b, lba * 512)?;
        Ok(b)
    };
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();

    let header = sector(ext4_copy, 1)?;
    if &header[..8] != b"EFI PART" {
        return Err("the stick formatted ext4 has no GPT at LBA 1".into());
    }
    let entry = sector(ext4_copy, 2)?;
    let linux = [0xAFu8, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4];
    let first = u64::from_le_bytes(entry[32..40].try_into()?);
    let last = u64::from_le_bytes(entry[40..48].try_into()?);
    if entry[..16] != linux || first != 2048 || last != sectors - 34 {
        return Err(format!(
            "the GPT's first entry is not a Linux filesystem from 2048 to {}: {first}..{last}",
            sectors - 34
        )
        .into());
    }
    // **Every other entry unused** (PR #369 review): entries are 128 bytes, four to a sector, so the
    // second is at LBA 2 + 128 — sixteen zero bytes at LBA 3 are the fifth's type.
    let mut array = vec![0u8; 128 * 128];
    fs::File::open(ext4_copy)?.read_exact_at(&mut array, 2 * 512)?;
    if let Some(n) = (1..128).find(|&n| array[n * 128..n * 128 + 16] != [0u8; 16]) {
        return Err(format!("the GPT has a partition in entry {n}, beside the one `disk --format` wrote").into());
    }
    println!(
        "  ok: the ext4 format's table is a GPT, one Linux filesystem partition from 1 MiB to the last usable sector"
    );
    let part = work.join("stick-blank-ext4-part.img");
    carve_sparse(ext4_copy, first * 512, (last - first + 1) * 512, &part)?;
    check_ext4_clean(&part, "the stick's ext4")?;
    match debugfs_cat(&part, &format!("/{STORAGE_PATTERN_FILE}"))? {
        Some(b) if b == pattern => println!("  ok: its ext4 holds the pattern, read with debugfs"),
        _ => return Err("the stick's ext4 does not hold the pattern: the eject did not write it back".into()),
    }

    let mbr = sector(stick, 0)?;
    let e = &mbr[0x1BE..0x1CE];
    let (kind, first, count) =
        (e[4], u32::from_le_bytes(e[8..12].try_into()?) as u64, u32::from_le_bytes(e[12..16].try_into()?) as u64);
    if mbr[510..512] != [0x55, 0xAA] || kind != 0x0C || first != 2048 || count != sectors - 2048 {
        return Err(format!(
            "the stick's MBR is not one FAT32 partition from 2048 to the end: type {kind:#04x}, {first}+{count}"
        )
        .into());
    }
    if mbr[0x1CE..0x1FE].iter().any(|&b| b != 0) {
        return Err("the stick's MBR names more than one partition".into());
    }
    if &sector(stick, 1)?[..8] == b"EFI PART" || &sector(stick, sectors - 1)?[..8] == b"EFI PART" {
        return Err("a GPT header outlived the MBR: the kernel could read the stale table".into());
    }
    println!(
        "  ok: the stick's table is an MBR, one FAT32 partition from 1 MiB, and no GPT header is left at either end"
    );
    let fat = work.join("stick-blank-fat-part.img");
    carve_fat_clean(stick, &fat, "the stick's FAT")?;
    let out = mtools_cmd("mtype").arg("-i").arg(&fat).arg(format!("::{STORAGE_PATTERN_FILE}")).output()?;
    if out.stdout != pattern {
        return Err("mtype does not read the pattern off the stick's FAT: the eject did not write it back".into());
    }
    let info = mtools_cmd("minfo").arg("-i").arg(&fat).arg("::").output()?;
    let info = String::from_utf8_lossy(&info.stdout);
    if !info.contains("FAT32") {
        return Err(format!("the stick's FAT is not a FAT32:\n{info}").into());
    }
    println!("  ok: its FAT32 holds the pattern, read with mtype");
    Ok(())
}

/// **`disk --eject NAME`, as a session runs it** (Phase 6 Part F): no `with`, and no password asked.
/// The chain writes back and records the filesystem clean before the service says the stick can
/// be pulled, and `disk` says so after.
///
/// **The drive goes whole** (PR #367 review): `names` are every filesystem on the stick, the first
/// the one named, and each is unmounted, in the order mounted, before the service and `disk` name
/// them all.
fn eject_stick(s: &mut Session, names: &[&str]) -> R<()> {
    let from = s.transcript().len();
    s.send(&format!("disk --eject {}", names[0]))?;
    for name in names {
        s.expect("fs-server: unmounted, and the filesystem recorded clean")?;
        s.expect(&format!("storage-service: unmounted {name}, left clean"))?;
    }
    let all = names.join(", ");
    s.expect(&format!("storage-service: ejected {all}, safe to remove"))?;
    s.expect(&format!("disk: ejected {all}"))?;
    s.expect("/home>")?;
    if s.transcript()[from..].contains("password:") {
        return Err(format!("`disk --eject {}` asked for a password", names[0]).into());
    }
    Ok(())
}

/// The sticks, once the machine has stopped (Phase 6 Part D): the ejected one's partition carved
/// out by its MBR, `e2fsck -fn` clean, `s_state` clean and the pattern read with `debugfs`; the
/// pulled one — `whole`, copied as the pull left it — still marked in use, which is what an unplug
/// without an eject leaves.
fn check_sticks(mbr: &Path, whole: &Path, work: &Path) -> R<()> {
    use std::io::{Read, Seek, SeekFrom};
    // **Each partition, carved by its MBR entry**: one eject took both (PR #367 review).
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    for ((first, count), label) in STICK_MBR_PARTS.into_iter().zip([STICK_MBR_LABEL, STICK_MBR_LABEL_2]) {
        let part = work.join(format!("stick-mbr-{label}.ext4"));
        let mut f = fs::File::open(mbr)?;
        f.seek(SeekFrom::Start(first as u64 * 512))?;
        let mut buf = vec![0u8; count as usize * 512];
        f.read_exact(&mut buf)?;
        fs::write(&part, buf)?;
        check_ext4_clean(&part, &format!("the ejected stick's {label}"))?;
        match debugfs_cat(&part, &format!("/{STORAGE_PATTERN_FILE}"))? {
            Some(b) if b == pattern => {
                println!("  ok: the ejected stick's {label} holds the pattern, read with debugfs")
            }
            Some(b) => {
                return Err(format!("the ejected stick's {label} file is {} bytes and not the pattern", b.len()).into());
            }
            None => {
                return Err(format!(
                    "the ejected stick's {label} has no pattern file: the eject did not write it back"
                )
                .into());
            }
        }
    }
    let state = ext4_s_state(whole)?;
    if state & EXT4_VALID_FS != 0 {
        return Err(format!(
            "the stick pulled while mounted writable records itself clean (s_state {state:#06x}): \
             nothing should have been able to mark it"
        )
        .into());
    }
    println!("  ok: the stick pulled while mounted is still marked in use (s_state {state:#06x})");
    Ok(())
}

/// **A stick's FAT partition carved out** — sparsely, at the MBR's 1 MiB — to `part`, and required
/// clean: `fsck.fat -n` finds nothing, and the dirty bit is clear.
fn carve_fat_clean(stick: &Path, part: &Path, what: &str) -> R<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    const MIB: u64 = 1024 * 1024;
    // Carved sparsely: nearly all of it never written.
    let mut from = fs::File::open(stick)?;
    let len = from.metadata()?.len() - MIB;
    from.seek(SeekFrom::Start(MIB))?;
    let mut to = fs::File::create(part)?;
    let mut buf = vec![0u8; MIB as usize];
    for _ in 0..len / MIB {
        from.read_exact(&mut buf)?;
        if buf.iter().any(|&b| b != 0) {
            to.write_all(&buf)?;
        } else {
            to.seek(SeekFrom::Current(MIB as i64))?;
        }
    }
    to.set_len(len)?;
    drop(to);
    let out = Command::new("fsck.fat").arg("-n").arg(part).output()?;
    if !out.status.success() {
        return Err(format!(
            "fsck.fat -n does not find {what} clean:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let mut boot = [0u8; 512];
    fs::File::open(part)?.read_exact(&mut boot)?;
    if boot[0x41] & 1 != 0 {
        return Err(format!("{what}'s dirty bit is set after its unmount").into());
    }
    println!("  ok: fsck.fat -n finds {what} clean, its dirty bit clear");
    Ok(())
}

/// **The FAT stick, once the machine has stopped** (Phase 6 Part E.6): its partition carved out,
/// `fsck.fat -n` clean — the dirty bit among what it checks — and mtools reading what the guest
/// did: the names it made, long and Unicode, the host's file under its new name, the one removed
/// gone, the copy's bytes, and the pattern it wrote through a mapping.
fn check_fat_stick(stick: &Path, work: &Path) -> R<()> {
    let part = work.join("stick-fat-part.img");
    carve_fat_clean(stick, &part, "the FAT stick")?;
    let at = format!("{}", part.display());
    let listed = mtools_cmd("mdir").arg("-i").arg(&at).arg("-/").arg("-b").arg("::").output()?;
    let listed: std::collections::BTreeSet<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("::/"))
        .map(str::to_string)
        .collect();
    let want: std::collections::BTreeSet<String> = [
        FAT_UNICODE_NAME.to_string(),
        "nested/".to_string(),
        format!("{FAT_NESTED_DIR}/"),
        format!("{FAT_NESTED_DIR}/{FAT_NESTED_NAME}"),
        STORAGE_PATTERN_FILE.to_string(),
        format!("{FAT_MADE_DIR}/"),
        format!("{FAT_MADE_DIR}/a copy of {FAT_UNICODE_NAME}"),
        format!("{FAT_MADE_DIR}/{FAT_RENAMED}"),
        FAT_WRITTEN.to_string(),
    ]
    .into_iter()
    .collect();
    if listed != want {
        return Err(format!("mdir lists the FAT stick as\n{listed:#?}\nwhere the guest left\n{want:#?}").into());
    }
    let read = |path: &str| -> R<Vec<u8>> {
        let out = mtools_cmd("mtype").arg("-i").arg(&at).arg(format!("::{path}")).output()?;
        if !out.status.success() {
            return Err(format!("mtype could not read {path:?} off the FAT stick").into());
        }
        Ok(out.stdout)
    };
    if read(&format!("{FAT_MADE_DIR}/a copy of {FAT_UNICODE_NAME}"))? != fat_host_bytes(FAT_UNICODE_NAME) {
        return Err("the guest's copy of the Unicode-named file does not hold its bytes".into());
    }
    if read(&format!("{FAT_MADE_DIR}/{FAT_RENAMED}"))? != fat_host_bytes(FAT_LONG_NAME) {
        return Err("the renamed file does not hold the long-named file's bytes".into());
    }
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    if read(FAT_WRITTEN)? != pattern {
        return Err(format!(
            "{FAT_WRITTEN} on the FAT stick does not hold the pattern: the eject did not write it back"
        )
        .into());
    }
    println!("  ok: mdir lists the guest's names, long and Unicode; mtype reads the copy, the rename and the pattern");
    Ok(())
}

/// The disk, once the machine has stopped: `e2fsck -fn` clean, the superblock marked clean, and
/// the file holding the pattern.
fn check_storage_disk(disk: &Path, work: &Path) -> R<()> {
    let fs_img = storage_root_fs(disk, work)?;
    check_left_clean(&fs_img)?;
    let path = format!("/{STORAGE_PATTERN_FILE}");
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    match debugfs_cat(&fs_img, &path)? {
        Some(b) if b == pattern => {}
        Some(b) => {
            let first = (0..STORAGE_PATTERN_LEN).find(|&i| b.get(i) != Some(&pattern[i]));
            return Err(format!(
                "{path} on the disk is {} bytes and is not the pattern (first difference at \
                 byte {first:?}): the unmount did not write the mapped pages back",
                b.len()
            )
            .into());
        }
        None => return Err(format!("{path} is not on the disk").into()),
    }
    println!("  ok: {path} holds the pattern, {STORAGE_PATTERN_LEN} bytes, read by debugfs");
    Ok(())
}

/// The FAT stick `check-media` plugs in: its label, and so the name it is mounted under.
const MEDIA_LABEL: &str = "NXMEDIA";
/// What `check-media` types into the editor, and what the host then reads off the stick.
const MEDIA_TEXT: &str = "saved onto a stick from nxedit";
/// The name it is saved under, at the stick's root.
const MEDIA_FILE: &str = "from-nitrox.txt";

/// `cargo xtask check-media` — **removable media in a session** (Phase 6 Part F), on the desktop a
/// person uses: the **release** live image as the boot stick, beside a copy of the release disk,
/// logged in at the graphical greeter.
///
/// 1. **Files at Home**: Drives lists the internal disk's `nitrox-root`, mounted read-only on a
///    live boot, with **no eject button**.
/// 2. **A FAT stick plugged in** over QMP, and **nothing typed or moved after it**: auto-mounted
///    writable, and its row with an eject button appears in Files' sidebar — read off screendumps
///    until it is drawn. Any input would wake Files on its compositor channel and draw the row
///    whether the watch had woken it or not (PR #366 review).
/// 3. **Saved from the editor**: text typed, Save As, the chooser's Up to `/` — aimed at where
///    `libui` lays the button out for the theme the image stages — and `storage/<stick>/<file>`
///    named in its field, which is joined to `/`.
/// 4. **Ejected from Files**: a click on the stick's eject button. The chain records the
///    filesystem clean with no password asked, the row goes, and the stick is pulled.
/// 5. **On the host**: `fsck.fat -n` clean, and `mtype` reads back what was typed.
///
/// **It asserts effects**, not Files' own account of itself: the row on the screen at step 2,
/// and at step 4 an eject that happens only if the button was drawn where the gate aims.
fn cmd_check_media(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    require_tool("fsck.fat")?;
    for tool in ["mformat", "mtype"] {
        require_tool(tool)?;
    }
    cmd_image(BuildMode::Normal)?;
    cmd_image_live()?;
    let work = build_cache().join("check-media");
    fs::create_dir_all(&work)?;
    let disk = work.join("disk.img");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    let stick = work.join("stick.img");
    media_stick(&stick)?;
    let qmp_sock = work.join("qmp.sock");
    let _ = fs::remove_file(&qmp_sock);
    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", qmp_sock.display()))
        .arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=boot,format=raw,file={}", live_image_path().display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=boot")
        .arg("-drive")
        .arg(format!("if=none,id=disk,format=raw,file={}", disk.display()))
        .arg("-device")
        .arg("ide-hd,drive=disk,bus=ide.0")
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
    println!("xtask: media gate — booting the live image beside a copy of the release disk…\n");
    let mut session = Session::spawn(cmd, "check-media")?;
    let result = Qmp::connect(&qmp_sock).and_then(|mut qmp| {
        qmp.screen = Some(size);
        run_media_steps(&mut session, &mut qmp, &stick, &work, size)
    });
    let transcript = session.finish();
    let _ = fs::remove_file(&qmp_sock);
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    if transcript.contains(DEMO_PASSWORD) {
        return Err("the password reached the console".into());
    }
    println!("\nxtask: the machine is stopped; the stick, on the host:");
    let part = work.join("stick-part.img");
    carve_fat_clean(&stick, &part, "the ejected stick")?;
    let out = mtools_cmd("mtype").arg("-i").arg(&part).arg(format!("::{MEDIA_FILE}")).output()?;
    let want = format!("{MEDIA_TEXT}\n");
    if !out.status.success() || out.stdout != want.as_bytes() {
        return Err(format!(
            "mtype reads {MEDIA_FILE} off the stick as {:?}, not the {want:?} typed into the editor",
            String::from_utf8_lossy(&out.stdout)
        )
        .into());
    }
    println!("  ok: mtype reads back what was typed into the editor");
    println!(
        "\nxtask: a stick plugged into the live desktop appeared in Files, took a file from the editor, and ejected clean ✓"
    );
    Ok(())
}

/// **The stick `check-media` plugs in**: a FAT32 partition behind an MBR, as one is sold, labelled
/// [`MEDIA_LABEL`] and empty. 300 MiB is what FAT32 by count needs at 4 KiB clusters, the smallest
/// `fs-server-fat` serves; the file is sparse.
fn media_stick(path: &Path) -> R<()> {
    const MIB: u64 = 1024 * 1024;
    let _ = fs::remove_file(path);
    fs::File::create(path)?.set_len(300 * MIB)?;
    let count = (300 * 2048 - 2048) as u32;
    write_mbr(path, 2048, count, 0x0C)?;
    let at = format!("{}@@{MIB}", path.display());
    run(mtools_cmd("mformat")
        .arg("-i")
        .arg(&at)
        .arg("-F")
        .arg("-c")
        .arg("8")
        .arg("-T")
        .arg(count.to_string())
        .arg("-v")
        .arg(MEDIA_LABEL)
        .arg("::"))?;
    Ok(())
}

/// **How much is drawn in a rectangle of a screendump**: the pixels differing from its top-left
/// one by more than the faintest edge of anti-aliased text. Glyph ink against a flat ground.
fn ink_in(dump: &(u32, u32, Vec<u8>), x: i32, y: i32, w: i32, h: i32) -> usize {
    let (dw, dh, rgb) = dump;
    let px = |x: i32, y: i32| {
        let i = (y as usize * *dw as usize + x as usize) * 3;
        (rgb[i], rgb[i + 1], rgb[i + 2])
    };
    let (x0, y0) = (x.clamp(0, *dw as i32 - 1), y.clamp(0, *dh as i32 - 1));
    let ground = px(x0, y0);
    let mut n = 0;
    for yy in y0..(y + h).min(*dh as i32) {
        for xx in x0..(x + w).min(*dw as i32) {
            let c = px(xx, yy);
            let d = c.0.abs_diff(ground.0) as u32 + c.1.abs_diff(ground.1) as u32 + c.2.abs_diff(ground.2) as u32;
            if d > 96 {
                n += 1;
            }
        }
    }
    n
}

/// The steps; see [`cmd_check_media`].
fn run_media_steps(s: &mut Session, qmp: &mut Qmp, stick: &Path, work: &Path, size: DisplaySize) -> R<()> {
    let secs = std::time::Duration::from_secs;
    let any = |_: &str| true;
    const APPS_CLICK: (i32, i32) = (60, 12);
    let dump_at = work.join("screen.ppm");
    let look = |qmp: &mut Qmp| -> R<(u32, u32, Vec<u8>)> {
        qmp.screendump(&dump_at)?;
        parse_ppm(&fs::read(&dump_at)?)
    };
    let launch = |qmp: &mut Qmp, s: &mut Session, program: &str| -> R<(u32, i32, i32)> {
        click_at(qmp, s, APPS_CLICK.0, APPS_CLICK.1)?;
        s.expect("desktop-shell: applications menu open")?;
        type_into_menu(qmp, s, program)?;
        press(qmp, "ret")?;
        s.expect(&format!("desktop-shell: launched {program} into its own namespace"))?;
        s.expect("desktop-shell: placed window ")?;
        let placed = s.rest_of_line()?;
        parse_placement(&placed).ok_or_else(|| format!("could not read {program}'s placement from {placed:?}").into())
    };

    // 1. **Log in, and Files at Home**, with the internal disk in Drives and no eject button on it.
    s.expect("desktop-session-mgr: greeter presented")?;
    logout_gate_login(qmp, s, size)?;
    let (files_id, fx, fy) = launch(qmp, s, "nxfiles")?;
    let side_x = fx + chrome::CONTENT_X;
    let content_top = fy + chrome::TITLE_BAR_H + browser::MENU_BAR_H + browser::PATH_H;
    let places = HOME_FOLDERS.len() as i32 + 2;
    let drive_top = |i: i32| content_top + (places + i) * browser::ROW_H + browser::DRIVES_HEADING_H;
    let button_w = libui::widget::ROW_BUTTON_W as i32;
    let label_ink = |d: &(u32, u32, Vec<u8>), i: i32| {
        ink_in(d, side_x, drive_top(i), browser::SIDEBAR_W - button_w, browser::ROW_H)
    };
    let button_ink = |d: &(u32, u32, Vec<u8>), i: i32| {
        ink_in(d, side_x + browser::SIDEBAR_W - button_w, drive_top(i), button_w, browser::ROW_H)
    };
    let settled = parse_ppm(&settle_and_capture(qmp, &dump_at)?)?;
    let (root_label, root_button) = (label_ink(&settled, 0), button_ink(&settled, 0));
    if root_label < 30 || root_button != 0 || label_ink(&settled, 1) != 0 {
        return Err(format!(
            "Files' first drive row has {root_label} px of label and {root_button} of button, and its second {} px: \
             the internal disk alone, with no eject button, was expected (dump {})",
            label_ink(&settled, 1),
            dump_at.display()
        )
        .into());
    }
    println!("  ok: Files lists the internal disk in Drives, with no eject button");

    // 2. **The stick, plugged in with nothing typed after it**: its row read off the screen.
    qmp.execute(&format!(
        r#"{{"execute":"blockdev-add","arguments":{{"driver":"raw","node-name":"media","file":{{"driver":"file","filename":"{}"}}}}}}"#,
        stick.display()
    ))?;
    let from = s.transcript().len();
    qmp.execute(
        r#"{"execute":"device_add","arguments":{"driver":"usb-storage","id":"media","bus":"xhci.0","drive":"media"}}"#,
    )?;
    // **No "left clean" on a writable mount's line**, which says nothing of how it was found: the
    // mount has marked it in use (`storage.md` §9).
    s.line_since(from, &format!("fat '{MEDIA_LABEL}'; mounted at /storage/{MEDIA_LABEL} (rw)"), any, secs(60))?;
    let deadline = std::time::Instant::now() + secs(30);
    loop {
        let d = look(qmp)?;
        if label_ink(&d, 1) >= 30 && button_ink(&d, 1) >= 10 {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "the stick mounted, and 30 s later Files' second drive row has {} px of label and {} of eject \
                 button: the watch did not wake it (dump {})",
                label_ink(&d, 1),
                button_ink(&d, 1),
                dump_at.display()
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    println!("  ok: the stick mounted writable, and its row and eject button appeared with no input");

    // 3. **Saved from the editor**, through Save As.
    launch(qmp, s, "nxedit")?;
    let _ = settle_and_capture(qmp, &dump_at)?;
    for (i, c) in MEDIA_TEXT.chars().enumerate() {
        let (qcode, shift) = qcode_for(c)?;
        if shift {
            qmp.send_key("shift", true)?;
        }
        press(qmp, &qcode)?;
        if shift {
            qmp.send_key("shift", false)?;
        }
        s.expect(&format!("nxedit: buffer rev {}", i + 1))?;
    }
    let from = s.transcript().len();
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(qmp, "s")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    s.expect("nxedit: choosing a file in /home - ")?;
    s.expect("nxedit: the chooser has the keyboard")?;
    s.line_since(from, "desktop-shell: placed dialog ", any, secs(30))?;
    let placed = s.transcript()[from..]
        .lines()
        .find_map(|l| l.split_once("desktop-shell: placed dialog ").map(|(_, r)| r.to_string()))
        .ok_or("no dialog placement")?;
    let (_, _, dx, dy, dw, dh) = parse_dialog_placement(&placed)
        .ok_or_else(|| format!("could not read the chooser's placement from {placed:?}"))?;
    let up = chooser_up_at(dw, dh)?;
    click_at(qmp, s, dx + up.0, dy + up.1)?;
    s.expect("nxedit: the chooser moved to / - ")?;
    press(qmp, "backspace")?;
    s.expect("nxedit: chooser name so far ")?;
    let left: usize = s
        .rest_of_line()?
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or("could not read how much of the name was left")?;
    for n in (0..left).rev() {
        press(qmp, "backspace")?;
        s.expect(&format!("nxedit: chooser name so far {n} chars"))?;
    }
    let name = format!("storage/{MEDIA_LABEL}/{MEDIA_FILE}");
    for (i, c) in name.chars().enumerate() {
        let (qcode, shift) = qcode_for(c)?;
        if shift {
            qmp.send_key("shift", true)?;
        }
        press(qmp, &qcode)?;
        if shift {
            qmp.send_key("shift", false)?;
        }
        s.expect(&format!("nxedit: chooser name so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    s.expect(&format!("nxedit: saved /storage/{MEDIA_LABEL}/{MEDIA_FILE} - "))?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "q")?;
    qmp.send_key("ctrl", false)?;
    s.expect("nxedit: quitting")?;
    s.expect("nxedit: closing")?;
    let _ = settle_and_capture(qmp, &dump_at)?;
    println!("  ok: the editor saved onto the stick through Save As: Up to /, then storage/{MEDIA_LABEL}");

    // 4. **Ejected from Files**, by its button: the chain, and no password.
    let from = s.transcript().len();
    let pressed = click_at(qmp, s, side_x + browser::SIDEBAR_W - button_w / 2, drive_top(1) + browser::ROW_H / 2)?;
    if !pressed.contains(&format!("win={files_id}")) {
        return Err(format!("the click on the eject button reached {pressed:?}, not Files' window {files_id}").into());
    }
    s.expect("fs-server: unmounted, and the filesystem recorded clean")?;
    s.expect(&format!("storage-service: unmounted {MEDIA_LABEL}, left clean"))?;
    s.expect(&format!("storage-service: ejected {MEDIA_LABEL}, safe to remove"))?;
    s.expect(&format!("nxfiles: {MEDIA_LABEL} ejected"))?;
    if s.transcript()[from..].contains("password") {
        return Err("ejecting from Files asked for a password".into());
    }
    // **The row's label, not its button**: the pointer rests where the button was, and the cursor
    // is drawn into the screen this reads.
    let deadline = std::time::Instant::now() + secs(30);
    while label_ink(&look(qmp)?, 1) != 0 {
        if std::time::Instant::now() > deadline {
            return Err(format!("the ejected stick's row is still drawn (dump {})", dump_at.display()).into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let from = s.transcript().len();
    qmp.execute(r#"{"execute":"device_del","arguments":{"id":"media"}}"#)?;
    s.line_since(from, "storage-service: blk-", |rest: &str| rest.ends_with(" departed"), secs(30))?;
    println!("  ok: ejected from Files with no password, recorded clean, its row gone, and pulled");
    Ok(())
}

/// **Where the chooser's Up button is**, inside a chooser window `w`×`h`: laid out by `libui`, as the
/// editor lays it out, with the theme the image stages — whose `font_px` sizes the title above it.
fn chooser_up_at(w: u32, h: u32) -> R<(i32, i32)> {
    let (theme, _) = staged_theme()?;
    if (w, h) != (libui::chooser::CHOOSER_W, libui::chooser::CHOOSER_H) {
        let (tw, th) = (libui::chooser::CHOOSER_W, libui::chooser::CHOOSER_H);
        return Err(format!("the chooser was placed {w}x{h}, not the toolkit's {tw}x{th}").into());
    }
    let font = host_font(theme.font_ui.as_str())?;
    let mut state = libui::chooser::ChooserState::saving("untitled");
    let view: libui::element::Element<u64> = libui::chooser::view(
        libui::chooser::Mode::Save,
        "/home",
        &[],
        &mut state,
        editor::CHOOSER_KEY,
        None,
        |k| k,
        0,
        0,
        0,
        true,
        &theme,
    );
    let l = libui::layout::layout(
        &view,
        libdraw::geom::Rect::new(0, 0, w, h),
        &libui::paint::FontMetrics::new(&font, theme.font_px),
    );
    let r = libui::layout::locate(&view, &l, editor::CHOOSER_UP_KEY).ok_or("the chooser has no Up button")?;
    Ok((r.origin.x + r.size.w as i32 / 2, r.origin.y + r.size.h as i32 / 2))
}

/// The file `check-shutdown` writes, under the serial session's `/home` — `alice`'s home on the
/// root filesystem.
const SHUTDOWN_PATTERN_FILE: &str = "check-shutdown.bin";

/// Where it lands on the root filesystem: the session's `/home` is `alice`'s home.
fn shutdown_pattern_path() -> String {
    format!("/home/{DEMO_USER}/{SHUTDOWN_PATTERN_FILE}")
}

/// The test image's services that do not honour a stop, passed over at a shutdown after their
/// bound: its graphical clients and `restart-probe`. Any other name "still running" is a failure.
const SHUTDOWN_UNHONOURED: &[&str] = &["nxterm", "ui-testclient", "input-testclient", "restart-probe"];

/// `cargo xtask check-shutdown` — **a shutdown, with the host holding the result** (administration
/// Part E.4d).
///
/// A copy of a `--selftest` disk image, the release root with the test packages, booted twice and
/// driven on serial as a person at the prompt would drive it.
///
/// **The halt.** Logged in as `alice`, `test-pattern` writes a pattern under `/home` through a
/// mapping and **exits without a sync**; the host reads the disk while the guest runs and finds the
/// file without the pattern, and the superblock saying mounted. Then `with power shutdown`, with no
/// password, as the seeded policy gives it to everyone. The gate asserts the sequence on COM1 — the
/// sessions ended when asked rather than after their bound, every service but the test image's
/// known clients stopped, `init` unmounted `/` clean — and **reads the message off the screen**
/// with `check-fbcon`'s decoder as well as off COM1, since the laptop has no serial port. With the
/// machine stopped, the host carves the root partition out: `e2fsck -fn` clean, the superblock
/// recording a clean unmount, and the file holding the pattern, read with `debugfs`.
///
/// **The reboot.** A fresh copy of the disk, since `boot-probe` is not one to run twice on the same
/// filesystem; `with admin date --set` to 2031, then `with power shutdown --reboot`, a reset
/// through the FADT's register, and QEMU is watched through a second boot — which must **anchor its
/// clock to the time set**, read back from the RTC the set wrote (administration Part E.5), and
/// whose `init` must mount the root **without** finding it not cleanly unmounted, which is the
/// reboot's own shutdown leaving it clean.
///
/// **Only after `boot-probe`'s verdict.** The power endpoint is one, and `boot-probe` takes and
/// lets go of it to test its refusals; a shutdown asked in the meantime would find the broker
/// unable to grant it.
fn cmd_check_shutdown(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    require_tool("e2fsck")?;
    require_tool("debugfs")?;
    cmd_image(BuildMode::Selftest)?;
    let work = build_cache().join("check-shutdown");
    fs::create_dir_all(&work)?;
    let disk = work.join("disk.img");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    let ovmf = locate_ovmf()?;
    let qmp_sock = work.join("qmp.sock");
    let dump = work.join("screen.ppm");
    let boot = |reboot: bool| -> R<Command> {
        let _ = fs::remove_file(&qmp_sock);
        let mut cmd = Command::new("qemu-system-x86_64");
        qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
        cmd.arg("-drive")
            .arg(format!("if=none,id=disk,format=raw,file={}", disk.display()))
            .arg("-device")
            .arg("ide-hd,drive=disk,bus=ide.0")
            .arg("-display")
            .arg("none")
            .arg("-qmp")
            .arg(format!("unix:{},server=on,wait=off", qmp_sock.display()))
            .arg("-chardev")
            .arg("stdio,id=hostserial,signal=off")
            .arg("-serial")
            .arg("chardev:hostserial")
            .arg("-smp")
            .arg("4")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // The halt must not be taken for a reset; the reboot must be allowed to happen.
        if !reboot {
            cmd.arg("-no-reboot");
        }
        Ok(cmd)
    };

    println!("xtask: shutdown gate — booting a copy of the test disk, to halt…\n");
    let mut session = Session::spawn(boot(false)?, "check-shutdown")?;
    let result = run_shutdown_halt(&mut session, &disk, &work, &qmp_sock, &dump);
    let transcript = session.finish();
    let _ = fs::write(build_cache().join("guest-transcript-check-shutdown.log"), &transcript);
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    check_shutdown_sequence(&transcript)?;
    println!("\nxtask: the machine is stopped; the disk, on the host:");
    let fs_img = work.join("nitrox-root.ext4");
    carve_partition(&disk, 2, &fs_img)?;
    check_left_clean(&fs_img)?;
    let path = shutdown_pattern_path();
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    match debugfs_cat(&fs_img, &path)? {
        Some(b) if b == pattern => {}
        Some(b) => {
            let first = (0..STORAGE_PATTERN_LEN).find(|&i| b.get(i) != Some(&pattern[i]));
            return Err(format!(
                "{path} on the disk is {} bytes and is not the pattern (first difference at byte \
                 {first:?}): the shutdown did not write the mapped pages back",
                b.len()
            )
            .into());
        }
        None => return Err(format!("{path} is not on the disk").into()),
    }
    println!("  ok: {path} holds the pattern, {STORAGE_PATTERN_LEN} bytes, read by debugfs");

    println!("\nxtask: a fresh copy of the disk, to reboot…\n");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    let mut session = Session::spawn(boot(true)?, "check-shutdown")?;
    let result = run_shutdown_reboot(&mut session);
    let transcript = session.finish();
    let _ = fs::write(build_cache().join("guest-transcript-check-shutdown-reboot.log"), &transcript);
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    println!(
        "\nxtask: a shutdown wrote back a file nothing synced, left the root clean and said so on the \
         screen, and a reboot came back ✓"
    );
    Ok(())
}

/// Wait for `boot-probe`'s verdict, then log in on serial as `DEMO_USER`.
fn shutdown_login(s: &mut Session) -> R<()> {
    s.expect("boot-probe: test-harness verdict")?;
    // **The whole line, waited for** (PR #356's CI): the prefix can arrive before the word after
    // it, and on KVM this read `… verdict P` from the transcript and refused a passing image.
    if !s.matched_line()?.contains("boot-probe: test-harness verdict PASS") {
        return Err("boot-probe's verdict was not PASS, so the image is not one to shut down".into());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !s.transcript().contains("nitrox login:") {
        if std::time::Instant::now() > deadline {
            return Err("no `nitrox login:` prompt on the serial column".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    Ok(())
}

/// The halt: a write through a mapping with no sync, seen on the host without its pattern, then
/// `with power shutdown`, and the message on COM1 and on the screen.
fn run_shutdown_halt(s: &mut Session, disk: &Path, work: &Path, qmp_sock: &Path, dump: &Path) -> R<()> {
    shutdown_login(s)?;
    let at = format!("/home/{SHUTDOWN_PATTERN_FILE}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    // **On the host, the file is on the disk and the pattern is not**, as in `check-storage`: so
    // what the host finds after the shutdown, the shutdown put there.
    let fs_img = work.join("nitrox-root-midrun.ext4");
    carve_partition(disk, 2, &fs_img)?;
    let path = shutdown_pattern_path();
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    match debugfs_cat(&fs_img, &path)? {
        None => {
            return Err(format!(
                "{path} is not on the disk at all after `test-pattern --write`, so the absence of \
                 the pattern would prove nothing"
            )
            .into());
        }
        Some(b) if b == pattern => {
            return Err(format!(
                "{path} already holds the pattern before any shutdown — something wrote it back, so \
                 this gate cannot show that the shutdown does"
            )
            .into());
        }
        Some(_) => {}
    }
    let state = ext4_s_state(&fs_img)?;
    if state & EXT4_VALID_FS != 0 {
        return Err(format!(
            "the superblock says clean (s_state {state:#06x}) while the root is mounted writable, \
             so a clean one at the end would not be the shutdown's doing"
        )
        .into());
    }
    println!("  ok: on the host meanwhile: the file is there without the pattern, and the root mounted");

    // **The shutdown, as anyone may ask it**: the seeded policy's `power` view, with no password.
    s.send("with power shutdown")?;
    s.expect("shutdown: shutting down")?;
    s.expect("It is now safe to turn off your computer.")?;
    println!("  ok: `with power shutdown` was taken, and COM1 says it is safe to turn off");

    // **And the screen says so**, read with the kernel console's own glyphs: the laptop has no
    // COM1. The machine is halted, so the frame is still.
    let mut qmp = Qmp::connect(qmp_sock)?;
    expect_safe_on_screen(&mut qmp, dump)?;
    println!("  ok: the screen, read back as text, ends in the message");
    Ok(())
}

/// **A halted machine's screen ends in "It is now safe to turn off your computer."**, read back
/// as text with the kernel console's own glyphs and layout (`check-fbcon`'s decoder) — what a
/// person sees on a machine with no serial port. `check-shutdown` and `check-logout` read it.
fn expect_safe_on_screen(qmp: &mut Qmp, dump: &Path) -> R<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        qmp.screendump(dump)?;
        let (w, h, rgb) = parse_ppm(&fs::read(dump)?)?;
        let frame = Frame::read(w, h, &rgb);
        let last = frame.lines().iter().rev().find(|l| !l.is_empty()).map(|l| l.to_string());
        if frame.is_console() && last.as_deref() == Some("It is now safe to turn off your computer.") {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "the screen does not end in the message: {} (last line {last:?}, dump at {})",
                if frame.is_console() { "it is console text" } else { "it is not the console" },
                dump.display()
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// **What a shutdown's COM1 must show**: the sessions ended when asked, every service stopped but
/// the test image's known clients, and `init` left `/` clean before `sys_power` flushed the disk.
fn check_shutdown_sequence(transcript: &str) -> R<()> {
    for line in [
        "service-mgr: shutting down, as asked",
        "nxsh: asked to stop, exiting",
        "session-mgr: asked to stop, exiting",
        "desktop-session-mgr: asked to stop, exiting",
        "service-mgr: the sessions have ended",
        "storage-service: asked to stop: everything it mounted is unmounted, exiting",
        "logging-service: asked to stop, exiting",
        "auth-service: asked to stop, exiting",
        "service-mgr: every service has been asked to stop; telling init to finish",
        "init: / unmounted, and its server left the filesystem clean",
        "power: stopping every processor, to halt",
    ] {
        if !transcript.contains(line) {
            return Err(format!("`{line}` is not in the transcript").into());
        }
    }
    if transcript.contains("still running after the sessions' bound") {
        return Err("a login supervisor ran out its bound rather than ending its session when asked".into());
    }
    for l in transcript.lines().filter(|l| l.contains("is still running after it was asked to stop")) {
        if !SHUTDOWN_UNHONOURED.iter().any(|n| l.contains(&format!("'{n}'"))) {
            return Err(format!("a service did not stop when asked: {l:?}").into());
        }
    }
    println!("  ok: the sessions ended when asked, every service stopped, and init left / clean");
    Ok(())
}

/// The time the reboot half sets before it restarts: 2031-01-02T00:00:00Z, years from any host's
/// clock, so a second boot that anchored to the host's time rather than the one written is plain.
const SHUTDOWN_SET_CLOCK: (&str, i64) = ("2031-01-02T00:00:00Z", 1_925_078_400);

/// How far past [`SHUTDOWN_SET_CLOCK`] the second boot may anchor: the seconds the reboot took,
/// since QEMU's RTC goes on counting, with room for a slow TCG host. A clock that did not survive
/// is five years out.
const SHUTDOWN_CLOCK_SLACK_SECS: i64 = 600;

/// The reboot: the clock set with `with admin date --set`, then `with power shutdown --reboot`
/// resets through the FADT's register. The second boot anchors its clock to the time that was set
/// — the RTC's write-back, across a reset (administration Part E.5) — and its `init` mounts the
/// root without finding it not cleanly unmounted.
fn run_shutdown_reboot(s: &mut Session) -> R<()> {
    s.expect("init: up (pid 1)")?;
    shutdown_login(s)?;
    let (time, epoch) = SHUTDOWN_SET_CLOCK;
    s.send(&format!("with admin date --set {time} --unix"))?;
    s.expect("[with admin] password (1 of 3): ")?;
    s.send(DEMO_PASSWORD)?;
    s.expect(&format!("wall clock: set to {epoch} (Unix epoch seconds, UTC); the RTC holds it"))?;
    s.expect("/home>")?;
    s.send("with power shutdown --reboot")?;
    s.expect("shutdown: restarting")?;
    s.expect("power: resetting through the FADT's reset register")?;
    let reset_at = s.transcript().len();
    s.expect("wall clock: anchored at ")?;
    let anchored = s.rest_of_line()?;
    let secs: i64 = anchored
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("the second boot's clock line did not read: `{anchored}`"))?;
    if !(epoch..=epoch + SHUTDOWN_CLOCK_SLACK_SECS).contains(&secs) {
        let slack = SHUTDOWN_CLOCK_SLACK_SECS;
        return Err(format!(
            "the second boot anchored its clock at {secs}, not within {slack} s after the {epoch} \
             set before the reboot: the RTC did not keep the time written to it"
        )
        .into());
    }
    println!("  ok: the clock set before the reboot is the one the second boot anchored to: {secs}");
    s.expect("init: up (pid 1)")?;
    s.expect("init: mounted fs-server-ext4 at /")?;
    let after = s.transcript()[reset_at..].to_string();
    if after.contains("the filesystem was not cleanly unmounted last time") {
        return Err("the second boot found the root not cleanly unmounted: the reboot's shutdown did not \
             leave it clean"
            .into());
    }
    println!(
        "  ok: `with power shutdown --reboot` reset the machine, and the second boot mounted the \
         root clean"
    );
    Ok(())
}

/// **What an unmount through the storage service must leave**, read on the host from the carved
/// filesystem `fs_img`: `e2fsck -fn` clean, and the superblock's `s_state` recording a clean
/// unmount with no error — `check-storage`'s and `check-recovery`'s.
fn check_left_clean(fs_img: &Path) -> R<()> {
    check_ext4_clean(fs_img, &format!("the disk's {ROOT_PARTLABEL}"))
}

/// `e2fsck -fn` clean and `s_state` recorded clean, for the ext4 image `fs_img`, which is `what`.
fn check_ext4_clean(fs_img: &Path, what: &str) -> R<()> {
    // `e2fsck -fn` exits 0 while reporting problems, so its output is what is read — as
    // `check_installed_root` reads it.
    let out = Command::new("e2fsck")
        .args(["-fn", &fs_img.display().to_string()])
        .output()
        .map_err(|e| format!("run e2fsck: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if text.contains("? no") || !text.contains(" files (") {
        return Err(format!("e2fsck is not happy with {what}:\n{text}").into());
    }
    println!("  ok: e2fsck -fn finds it clean");
    let state = ext4_s_state(fs_img)?;
    if state & EXT4_VALID_FS == 0 || state & EXT4_ERROR_FS != 0 {
        return Err(format!(
            "the superblock's s_state is {state:#06x}: not recorded clean, or with its error bit \
             set. The unmount's `Meta::Unmount` is what records it clean"
        )
        .into());
    }
    println!("  ok: the superblock records it cleanly unmounted (s_state {state:#06x})");
    Ok(())
}

/// The password `check-recovery` resets `alice`'s to, on the installed disk — a fixture like
/// `DEMO_PASSWORD`, for a disk the gate made and throws away.
const RECOVERED_PASSWORD: &str = "alice after recovery";

/// `cargo xtask check-recovery` — **a forgotten password, reset from the live image**
/// (administration Part D.5), on demand like `check-install`: two boots and a disk image.
///
/// Recovery is the one account path nobody exercises until they need it, and then there is no
/// administrator to ask: the person has forgotten the password `with admin` wants. So the gate is
/// the path a person takes on the laptop, with no view and no service writing the file.
///
/// **The first boot** is the live image as a USB stick beside **a copy of the release disk**,
/// its root marked not cleanly unmounted as an installed machine's is. On the serial console,
/// logged in as the live image's own `alice`:
/// - the disk's `nitrox-root` is auto-mounted read-only; `with admin disk` unmounts it and mounts
///   it writable — the live image's `alice` is its administrator, not the installed one's;
/// - `account --password alice --users /storage/nitrox-root/system/users` sets a new password in
///   **that disk's** file, typed twice;
/// - `with admin disk --unmount` leaves it clean;
/// - and the live system's own `alice` still logs in with the old password: the file edited was
///   the disk's, not the running system's, which no session can reach.
///
/// **Between the boots, on the host**: the filesystem carved out of the disk is clean, and its
/// `/system/users` differs from what the release disk shipped **in `alice`'s line alone**.
///
/// **The second boot** is that disk alone, with no stick: the old password is refused at the
/// login, and the new one taken. And neither boot printed either password.
fn cmd_check_recovery(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    require_tool("e2fsck")?;
    require_tool("debugfs")?;
    // **A copy of the release disk, made fresh**, so a run cannot pass on what an earlier one
    // wrote.
    cmd_image(BuildMode::Normal)?;
    let work = build_cache().join("check-recovery");
    fs::create_dir_all(&work)?;
    let disk = work.join("disk.img");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    mark_root_not_clean(&disk)?;
    let shipped = debugfs_cat(&storage_root_fs(&disk, &work)?, "/system/users")?
        .ok_or("the release disk has no /system/users")?;
    cmd_image_live()?;
    let ovmf = locate_ovmf()?;
    let secrets = [DEMO_PASSWORD, RECOVERED_PASSWORD];
    let leaked = |transcript: &str| secrets.iter().position(|p| transcript.contains(p));

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=stick,format=raw,file={}", live_image_path().display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick")
        .arg("-drive")
        .arg(format!("if=none,id=disk,format=raw,file={}", disk.display()))
        .arg("-device")
        .arg("ide-hd,drive=disk,bus=ide.0")
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
    println!("xtask: recovery gate — booting the live image beside a copy of the release disk…\n");
    let mut session = Session::spawn(cmd, "check-recovery")?;
    let result = run_recovery_steps(&mut session);
    let transcript = session.finish();
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    if let Some(p) = leaked(&transcript) {
        return Err(format!("password {p} reached the console on the live boot: a prompt echoed it").into());
    }

    println!("\nxtask: the machine is stopped; the disk, on the host:");
    let fs_img = storage_root_fs(&disk, &work)?;
    check_left_clean(&fs_img)?;
    let users = debugfs_cat(&fs_img, "/system/users")?.ok_or("the disk has no /system/users")?;
    check_recovered_users(&shipped, &users)?;

    println!("\nxtask: booting the disk alone…\n");
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", disk.display()))
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
    let mut session = Session::spawn(cmd, "check-recovery-boot")?;
    let result = run_recovered_boot_steps(&mut session);
    let transcript = session.finish();
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    if let Some(p) = leaked(&transcript) {
        return Err(format!("password {p} reached the console on the disk's boot: a prompt echoed it").into());
    }
    println!("\nxtask: a password reset from the live image logs in on the installed disk ✓");
    Ok(())
}

/// Wait for the serial login prompt, searched for in the whole transcript as `check-live` does:
/// it and the greeter come up together, so an `expect` could scan past it.
fn await_serial_login(s: &mut Session) -> R<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !s.transcript().contains("nitrox login:") {
        if std::time::Instant::now() > deadline {
            return Err("no `nitrox login:` prompt on the serial column".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

/// The live boot: the disk's root made writable, `alice`'s password set in its file, and unmounted.
fn run_recovery_steps(s: &mut Session) -> R<()> {
    let users = format!("/storage/{ROOT_PARTLABEL}/system/users");

    // 1. **The disk's root, auto-mounted read-only**, the boot being a live one; its name is read
    //    off the line the storage service reports it on.
    let reported = format!("(partition {ROOT_PARTLABEL}): ext4");
    s.expect(&reported)?;
    let line = s.matched_line()?;
    let name = line
        .split_whitespace()
        .find(|w| w.starts_with("blk-"))
        .ok_or_else(|| format!("no `blk-<n>` in {line:?}"))?
        .to_string();
    let index = name.trim_start_matches("blk-").to_string();
    if !line.contains(&format!("mounted at /storage/{ROOT_PARTLABEL} (ro)")) {
        return Err(format!("the disk's {ROOT_PARTLABEL} is not auto-mounted read-only: {line:?}").into());
    }
    println!("  ok: {name}, the disk's {ROOT_PARTLABEL}, auto-mounted read-only on a live boot");

    // 2. The live image's `alice`, on the serial column.
    await_serial_login(s)?;
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;

    // 3. **Writable**, through the live image's own administrator.
    let admin = |s: &mut Session, command: &str| -> R<()> { with_admin_asked(s, command) };
    admin(s, &format!("disk --unmount {ROOT_PARTLABEL}"))?;
    s.expect(&format!("disk: unmounted {ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    admin(s, &format!("disk --mount /dev/blk/{index}"))?;
    s.expect(&format!("storage-service: mounted {ROOT_PARTLABEL} (rw), as asked"))?;
    s.expect(&format!("disk: mounted {name} at /storage/{ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    println!("  ok: `with admin disk` unmounted it and mounted it writable");

    // 4. **The reset**: no view and no service — `libusers` on the disk's file.
    s.send(&format!("account --password alice --users {users}"))?;
    s.expect("new password for alice: ")?;
    s.send(RECOVERED_PASSWORD)?;
    s.expect("again: ")?;
    s.send(RECOVERED_PASSWORD)?;
    s.expect(&format!("account: set a new password for alice in {users}"))?;
    s.expect("/home>")?;
    println!("  ok: account --password alice --users set it in the disk's file");

    // 5. Unmounted, and left clean.
    admin(s, &format!("disk --unmount {ROOT_PARTLABEL}"))?;
    s.expect(&format!("storage-service: unmounted {ROOT_PARTLABEL}, left clean"))?;
    s.expect(&format!("disk: unmounted {ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;

    // 6. **The running system's own file is untouched**: its `alice` logs in as before.
    s.send("exit")?;
    s.expect("nitrox login:")?;
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    println!("  ok: unmounted clean, and the live system's own alice logs in as before");
    Ok(())
}

/// `alice`'s line changed, **and nothing else**: every other line of `shipped` is in `now` byte for
/// byte, and `alice`'s new record takes the recovered password and refuses the old one.
fn check_recovered_users(shipped: &[u8], now: &[u8]) -> R<()> {
    let lines = |b: &[u8]| -> Vec<Vec<u8>> { b.split(|&c| c == b'\n').map(|l| l.to_vec()).collect() };
    let (before, after) = (lines(shipped), lines(now));
    let is_alice = |l: &[u8]| l.starts_with(format!("{DEMO_USER}:").as_bytes());
    if before.len() != after.len() {
        return Err(format!("/system/users went from {} lines to {}", before.len(), after.len()).into());
    }
    for (b, a) in before.iter().zip(&after) {
        if is_alice(b) != is_alice(a) || (!is_alice(b) && b != a) {
            return Err(format!(
                "a line other than alice's changed in /system/users: {:?} became {:?}",
                String::from_utf8_lossy(b),
                String::from_utf8_lossy(a)
            )
            .into());
        }
        if is_alice(b) && b == a {
            return Err("alice's line in the disk's /system/users is as the release disk shipped it".into());
        }
    }
    let alice = libusers::find(now, DEMO_USER.as_bytes()).ok_or("the disk's /system/users has no alice")?;
    if !alice.verifies(RECOVERED_PASSWORD.as_bytes()) || alice.verifies(DEMO_PASSWORD.as_bytes()) {
        return Err("alice's new record does not take the recovered password, or still takes the old one".into());
    }
    // **Under a fresh salt**: a whole one, from the entropy source, and not the build's.
    let salt_of = |file: &[u8]| {
        let mut out = [0u8; libusers::SALT_MAX];
        let n = libusers::find(file, DEMO_USER.as_bytes()).and_then(|r| r.salt(&mut out)).unwrap_or(0);
        out[..n].to_vec()
    };
    let (old, new) = (salt_of(shipped), salt_of(now));
    if new.len() != libusers::SALT_LEN || new.iter().all(|&b| b == 0) || new == old {
        return Err(format!(
            "alice's new salt is {} bytes {:02x?}: not a fresh {}-byte one from the entropy source",
            new.len(),
            new,
            libusers::SALT_LEN
        )
        .into());
    }
    println!("  ok: /system/users changed in alice's line alone, to the recovered password, under a fresh salt");
    Ok(())
}

/// The disk alone: the old password refused at the login, and the new one taken.
fn run_recovered_boot_steps(s: &mut Session) -> R<()> {
    s.expect(&format!("init:   /: fs-server-ext4 on gpt-partlabel:{ROOT_PARTLABEL} (rw)"))?;
    await_serial_login(s)?;
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("login incorrect")?;
    s.expect("nitrox login:")?;
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(RECOVERED_PASSWORD)?;
    s.expect("/home>")?;
    println!("  ok: on the disk alone, alice's old password is refused and the recovered one logs in");
    Ok(())
}

/// The facts `check-report`'s boot adds to [`EMULATED_MACHINE_FACTS`]: the laptop's shape — a USB
/// stick, an **empty** AHCI controller that is therefore declined, and no UART — and the command
/// line the menu entry passed. **And the stick itself, enumerated** (Phase 6 Part A): the line a
/// person photographs off the laptop's report to say what its controller found.
const REPORT_FACTS: &[&[&str]] = &[
    &["boot: HHDM ", ", cmdline \"hwreport\""],
    &["drivers: 00:1f.2 declined by ahci: no SATA disk on any implemented port"],
    &["usb: port 1: 46f4:0001 class 08/06/50, SuperSpeed, \"QEMU USB HARDDRIVE (", "\": mass storage, bulk-only"],
    // **Its disk, beside it** (Phase 6 Part D), and the disk the machine started from.
    &["usb: port 1: LUN 0: disk \"QEMU QEMU HARDDISK (", " blocks of 512 bytes, record "],
    &["gpt: the disk the machine started from: its GUID is the one Limine loaded from"],
    &["usb: first round: 1 device(s) in "],
    &["ramdisk: module 1 (/boot/root.img)"],
    &["label \"nitrox-live\" -> block node"],
    &["console: no UART at COM1"],
];

/// `cargo xtask check-report` — **the hardware report, read the way it will be read on the
/// laptop** (Phase 5 Part D.4).
///
/// Boots the live image as a USB stick with the AHCI controller empty and **`-serial none`**, so
/// nothing comes from COM1: the machine this is for has none. Then, off the screen alone:
///
/// 1. **The menu.** Finds Limine's menu with a detector of its own — the kernel console's decoder
///    reads only the console's cells — presses Down, sees the highlight move, and presses Enter.
/// 2. **The pages.** Reads each report page with the console's decoder, whole off one frame, by
///    its prompt row, and presses a key for the next; the last key ends the report.
/// 3. **The facts.** The pages together carry [`EMULATED_MACHINE_FACTS`] and [`REPORT_FACTS`]: a
///    declined AHCI controller, the module disk and its partition, no COM1.
/// 4. **The boot goes on**: the console hands the screen to the compositor.
///
/// Between them, this and `test-qemu` cover a claimed and a declined function, and COM1 both ways.
fn cmd_check_report(accel: Accel, usb: bool, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image_live()?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-report.sock");
    let dump = work.join("report.ppm");
    let _ = fs::remove_file(&qmp_sock);
    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=stick,format=raw,file={}", live_image_path().display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick")
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
        // No UART at all: the report has to say so, and nothing here could read one.
        .arg("-serial")
        .arg("none")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // **`--usb`: the same machine with no i8042** (Phase 6 Part B), so every page turns on a USB
    // key press: the report counts any keyboard's, and holds its pages for one.
    if usb {
        usb_input_args(&mut cmd, false);
    }
    println!("xtask: hardware report gate — choosing the live image's report entry, with no serial port…\n");
    let _session = Session::spawn(cmd, "check-report")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;
    let look = |qmp: &mut Qmp| -> R<(u32, u32, Vec<u8>)> {
        qmp.screendump(&dump)?;
        parse_ppm(&fs::read(&dump)?)
    };

    // 1. The menu, and its second entry chosen.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let first = loop {
        let (w, h, rgb) = look(&mut qmp)?;
        if let Some(highlight) = limine_menu(w, h, &rgb) {
            break highlight;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("no Limine menu on the screen within 120s (last dump {})", dump.display()).into());
        }
    };
    press(&mut qmp, "down")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (w, h, rgb) = look(&mut qmp)?;
        match limine_menu(w, h, &rgb) {
            Some(highlight) if highlight > first => break,
            _ if std::time::Instant::now() > deadline => {
                return Err(format!(
                    "Down did not move the menu's highlight below the first entry's (y {first}) \
                     within 10s — the countdown may have booted it (last dump {})",
                    dump.display()
                )
                .into());
            }
            _ => {}
        }
    }
    press(&mut qmp, "ret")?;
    println!("  ok: found Limine's menu, moved to its second entry, and chose it");

    // 2. The pages, each read whole off one frame by its prompt, and a key for the next.
    let mut pages: Vec<Vec<String>> = Vec::new();
    let mut total: Option<usize> = None;
    let mut console_seen = false;
    let mut deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while total.is_none_or(|n| pages.len() < n) {
        let (w, h, rgb) = look(&mut qmp)?;
        let frame = Frame::read(w, h, &rgb);
        if frame.is_console() {
            console_seen = true;
            // The menu detector's control, on every console frame read here: the kernel's screen
            // is never taken for Limine's.
            if limine_menu(w, h, &rgb).is_some() {
                return Err(format!("the menu detector matched a kernel console frame ({})", dump.display()).into());
            }
            if let Some((index, of)) = frame.rows.last().and_then(|r| r.as_deref()).and_then(report_prompt) {
                let expected = *total.get_or_insert(of);
                if of != expected {
                    return Err(format!("page {index} says there are {of} pages; the first said {expected}").into());
                }
                if index == pages.len() + 1 {
                    let lines = frame.lines();
                    pages.push(lines[..lines.len() - 1].iter().map(|l| l.to_string()).collect());
                    press(&mut qmp, "spc")?;
                    deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                } else if index > pages.len() + 1 {
                    return Err(
                        format!("page {index}/{of} came up after page {} — one went by unread", pages.len()).into()
                    );
                }
            }
        } else if console_seen && frame.text_cells == 0 {
            return Err(format!(
                "the boot went on to the desktop after {} of {} report page(s) — the report did \
                 not hold (last dump {})",
                pages.len(),
                total.map_or("?".to_string(), |n| n.to_string()),
                dump.display()
            )
            .into());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "no report page {} on the screen in time — {} read so far{} (last dump {})",
                pages.len() + 1,
                pages.len(),
                if console_seen { "" } else { ", and never a whole console frame" },
                dump.display()
            )
            .into());
        }
    }
    let n = pages.len();
    println!("  ok: read {n} report page(s) off the screen, pressing a key for each");

    // 3. The facts, from the pages alone — the framebuffer's at the size this gate booted, with no
    //    padding, which is all QEMU can give (`DisplaySize`).
    let lines: Vec<&str> = pages.iter().flatten().map(String::as_str).collect();
    let framebuffer = format!("framebuffer: {}x{} pitch {} padding 0 bpp 32", size.w, size.h, size.w * 4);
    let framebuffer: &[&str] = &[&framebuffer];
    // Under `--usb` the machine has no i8042 and a keyboard and mouse beside the stick, so the
    // FADT's fact and the first round's count are the other ones.
    let replaced =
        |f: &&&[&str]| !usb || !f.iter().any(|frag| frag.contains("8042 present") || frag.contains("first round: 1 "));
    let usb_facts: &[&[&str]] = if usb { USB_INPUT_FACTS } else { &[] };
    let missing = missing_facts(
        &lines,
        EMULATED_MACHINE_FACTS
            .iter()
            .chain(REPORT_FACTS)
            .filter(replaced)
            .chain(usb_facts)
            .copied()
            .chain([framebuffer]),
    );
    if !missing.is_empty() {
        let read: Vec<String> =
            pages.iter().enumerate().map(|(i, p)| format!("--- page {} ---\n{}", i + 1, p.join("\n"))).collect();
        return Err(format!(
            "the report's {n} page(s) do not say: {missing:?}. The pages as read:\n{}",
            read.join("\n")
        )
        .into());
    }
    println!(
        "  ok: the pages say what this machine is — a declined AHCI controller, the module disk, the stick \
         enumerated, no COM1"
    );
    // **And the first page says which build** (the laptop polish's Part D): the line a person
    // checks first on a machine that misbehaves, before anything else on the report.
    let built_from = built_from_line();
    if !pages.first().is_some_and(|p| p.iter().any(|l| l.contains(&built_from))) {
        return Err(format!("the report's first page does not say `{built_from}`").into());
    }
    println!("  ok: the first page names the build — {built_from}");

    // 4. The boot goes on, and the console hands the screen over.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let (w, h, rgb) = look(&mut qmp)?;
        let frame = Frame::read(w, h, &rgb);
        if !frame.is_console() && frame.text_cells == 0 {
            break;
        }
        if let Some((index, of)) = frame.rows.last().and_then(|r| r.as_deref()).and_then(report_prompt)
            && index > n
        {
            return Err(format!("page {index}/{of} came up after the last page's key").into());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "the console did not hand the screen over within 120s of the last page (last dump {})",
                dump.display()
            )
            .into());
        }
    }
    println!("  ok: after the last page the boot went on, and the console handed the screen over");

    let _ = fs::remove_file(&qmp_sock);
    println!(
        "\nxtask: the hardware report is chosen from the menu, held a page at a time, and read with no serial port ✓"
    );
    Ok(())
}

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
fn cmd_check_resolutions(accel: Accel) -> R<()> {
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

/// `(page, of)` from a report page's prompt row, `— page 2/3 — any key —` (`kernel/src/report.rs`).
fn report_prompt(row: &str) -> Option<(usize, usize)> {
    let rest = row.strip_prefix("— page ")?.strip_suffix(" — any key —")?;
    let (index, of) = rest.split_once('/')?;
    let (index, of): (usize, usize) = (index.parse().ok()?, of.parse().ok()?);
    (index >= 1 && index <= of).then_some((index, of))
}

/// Limine's boot menu, as Limine 12.2.0 draws it on OVMF's framebuffer: `Some(y)` — the first pixel
/// row of the highlighted entry's bar — when `rgb` is that menu, `None` otherwise.
///
/// **A detector of its own**, because the console's decoder reads only the console's cells.
/// Calibrated on screendumps of the menu (2026-09-14): a black ground, grey entries and help text,
/// the title in cyan along the top, green key names and countdown, a bright-green countdown digit,
/// and the highlighted entry drawn black on a grey bar. All three of these must hold:
///
/// - **Every pixel is one of those five colours.** OVMF's screens carry its logo's colours and the
///   kernel console draws ink on slate, so neither passes this alone.
/// - **The title's cyan, and only along the top**: a line of text's worth, none of it below the
///   top tenth of the screen.
/// - **A highlight bar**: a row with a run of grey wider than any glyph stroke. Its `y` is how the
///   gate sees that Down moved the selection.
fn limine_menu(w: u32, h: u32, rgb: &[u8]) -> Option<usize> {
    const BLACK: [u8; 3] = [0, 0, 0];
    const GREY: [u8; 3] = [170, 170, 170];
    const GREEN: [u8; 3] = [0, 170, 0];
    const CYAN: [u8; 3] = [0, 170, 170];
    const BRIGHT_GREEN: [u8; 3] = [85, 255, 85];
    // Wider than any stroke of Limine's 8-pixel glyphs; the narrowest bar, behind `Nitrox`, is 72.
    const BAR_RUN: usize = 32;
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || rgb.len() < w * h * 3 {
        return None;
    }
    let (mut cyan, mut highlight) = (0usize, None);
    for y in 0..h {
        let mut run = 0usize;
        for x in 0..w {
            let i = (y * w + x) * 3;
            let px = [rgb[i], rgb[i + 1], rgb[i + 2]];
            if px == CYAN {
                if y > h / 10 {
                    return None;
                }
                cyan += 1;
            } else if px != BLACK && px != GREY && px != GREEN && px != BRIGHT_GREEN {
                return None;
            }
            run = if px == GREY { run + 1 } else { 0 };
            if run >= BAR_RUN && highlight.is_none() {
                highlight = Some(y);
            }
        }
    }
    if cyan >= 200 { highlight } else { None }
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
/// How many desktop entries the image stages — the number `check-login` asserts the shell read.
///
/// **Duplicated from `GRAPHICAL_APPLICATIONS`'s length on purpose.** Deriving it would make the
/// gate agree with the build by construction and assert nothing; a literal is a second statement
/// of the same fact, which is what an assertion is for.
const STAGED_APPLICATIONS: usize = 3;

/// `cargo xtask check-logout` — **ending a session from the desktop** (administration Part F.2),
/// in CI's QEMU job.
///
/// On the **release image**, as `check-login` is: only there does the greeter hold the keyboard,
/// and only after a real login does `desktop-shell` build a session a person can end. In one QEMU
/// run:
/// 1. log in at the greeter;
/// 2. open the editor from the Applications menu, and type a key, so it has something to lose;
/// 3. the power menu's **Log out**: the shell asks the editor's window to close, the editor asks
///    whether to discard, and half a second later **the shell's waiting dialog names it** —
///    **without taking the keyboard from the editor's question** (PR #346 review, finding 2):
///    Escape answers the question, *keep editing*, and the logout goes on waiting;
/// 4. **Cancel**, and the session goes on — the Applications menu still opens;
/// 5. Log out again, and **discard** in the editor's question, asked again: the editor
///    closes, the shell exits, and **the greeter comes back**;
/// 6. log in again, open a terminal, and Log out: **nothing to ask**, so no waiting dialog, and the
///    greeter comes back;
/// 7. log in again, open the editor with a key typed into it and a terminal, and type
///    `with power shutdown --reboot` there: **a shutdown started elsewhere asks the windows too** —
///    the editor asks, no dialog comes, and the shell goes after its own 3 s, **inside its
///    supervisor's 5 s**, so no leader is left "still running after it was asked to stop"; then the
///    machine resets, and **a second boot's greeter** comes up. No other gate reaches this path:
///    `check-shutdown` has no graphical session;
/// 8. log in, open the editor with a key typed into it, and choose **Restart** (administration Part
///    F.3): the editor asks, the waiting dialog names it, and **End anyway** destroys its window;
///    then the shell asks the view broker for `shutdown --reboot` in the `power` view, `service-mgr`'s
///    sequence asks the shell to stop, it goes at once with nothing left open, and **a third boot's
///    greeter** comes up;
/// 9. log in, and choose **Shut down**: nothing to ask, the broker starts `shutdown`, and the
///    machine halts — "It is now safe to turn off your computer." **read off the screen** with
///    `check-fbcon`'s decoder, as on the laptop, which has no serial port.
///
/// **Two resets and a halt in one QEMU run**, so it boots with a reset allowed to reboot — three
/// boots, and the order is forced: the halt must be last.
///
/// **Aimed, not measured**: the power button at `chrome::POWER_FROM_RIGHT` in from the right edge,
/// the menu's rows by the keyboard, and the dialogs' buttons at `chrome`'s published centres from
/// the origins the shell logs — the rule every screen gate here keeps (M11 decision 2).
fn cmd_check_logout(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;
    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-logout.sock");
    println!("xtask: logout gate — booting the release image…\n");
    let (mut session, mut qmp) = spawn_release_guest(accel, "check-logout", &qmp_sock, size, true, false)?;
    const APPS_CLICK: (i32, i32) = (60, 12);
    let power_click = (size.w as i32 - chrome::POWER_FROM_RIGHT, 12);

    // 1. Log in.
    session.expect("desktop-session-mgr: greeter presented")?;
    logout_gate_login(&mut qmp, &mut session, size)?;
    println!("  ok: logged in at the greeter");

    // 2. The editor, with a key typed into it.
    let editor = logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxedit")?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;
    println!("  ok: the editor is open (window {editor}) with something to lose");

    // 3. Log out: the editor asks, and the shell names it.
    logout_gate_choose(&mut qmp, &mut session, power_click, LOG_OUT)?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let placed = session.rest_of_line()?;
    let (placed_id, parent, _, _, _, _) = parse_dialog_placement(&placed)
        .ok_or_else(|| format!("could not read the editor's question's placement from {placed:?}"))?;
    if parent != editor {
        return Err(format!("the question placed is on window {parent}, not the editor's {editor}").into());
    }
    let (wx, wy) = logout_gate_waiting(&mut session, "Waiting for 1 window to close:")?;
    println!("  ok: Log out asked the editor, which asked about its buffer, and the shell named it");
    // **The keyboard is still the question's**: the dialog gave it back, so Escape reaches the
    // editor — *keep editing* — and not the dialog, where it would be Cancel. No receipt from the
    // editor to wait for: it announces only its first gain. None is needed: the shell logs after
    // the compositor has answered the raise, and a key is routed to whatever the stack says then.
    session.expect(&format!("desktop-shell: the keyboard back to window {placed_id}"))?;
    let from = session.transcript().len();
    press(&mut qmp, "esc")?;
    session.expect("nxedit: close cancelled, still editing")?;
    if session.transcript()[from..].contains("desktop-shell: ending the session cancelled") {
        return Err("Escape, meant for the editor's question, cancelled the logout".into());
    }
    println!("  ok: the waiting dialog left the keyboard with the editor's question");

    // 4. Cancel, and the session goes on.
    click_at(&mut qmp, &mut session, wx + chrome::DIALOG_RIGHT_CX, wy + chrome::DIALOG_BUTTON_CY)?;
    session.expect("desktop-shell: ending the session cancelled; it goes on")?;
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    press(&mut qmp, "esc")?;
    session.expect("desktop-shell: applications menu closed")?;
    println!("  ok: Cancel kept the session");

    // 5. Log out again, and discard in the question the editor asks again: the greeter comes back.
    logout_gate_choose(&mut qmp, &mut session, power_click, LOG_OUT)?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let placed = session.rest_of_line()?;
    let (_, _, qx, qy, _, _) = parse_dialog_placement(&placed)
        .ok_or_else(|| format!("could not read the editor's second question's placement from {placed:?}"))?;
    logout_gate_waiting(&mut session, "Waiting for 1 window to close:")?;
    click_at(&mut qmp, &mut session, qx + chrome::DIALOG_LEFT_CX, qy + chrome::DIALOG_BUTTON_CY)?;
    session.expect("nxedit: discarding the unsaved buffer")?;
    session.expect("desktop-shell: logging out, exiting")?;
    session.expect("desktop-session-mgr: session ended (leader exit 0)")?;
    session.expect("desktop-session-mgr: greeter presented again")?;
    println!("  ok: once the editor closed, the session ended and the greeter came back");

    // 6. Log in again; a terminal; Log out — nothing to ask.
    logout_gate_login(&mut qmp, &mut session, size)?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxterm")?;
    logout_gate_choose(&mut qmp, &mut session, power_click, LOG_OUT)?;
    let from = session.transcript().len();
    session.expect("desktop-shell: logging out, exiting")?;
    if session.transcript()[from..].contains("desktop-shell: waiting dialog open") {
        return Err("a terminal, which closes when asked, still brought up the waiting dialog".into());
    }
    session.expect("desktop-session-mgr: greeter presented again")?;
    println!("  ok: with nothing to ask, the session ended at once and the greeter came back");

    // 7. A restart typed at a terminal, with the editor holding something to lose.
    logout_gate_login(&mut qmp, &mut session, size)?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxedit")?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxterm")?;
    type_at_terminal(&mut qmp, "with power shutdown --reboot")?;
    session.expect("desktop-shell: asked to stop; asking every window to close first")?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    // **To the reset, then read back**: a shell that outlived its supervisor's bound never says it
    // is exiting before the machine goes, so waiting for that line would time out rather than say
    // why.
    let from = session.transcript().len();
    session.expect(RESET)?;
    let after = session.transcript()[from..].to_string();
    if after.contains("desktop-shell: waiting dialog open") {
        return Err("a shutdown started at a terminal brought up the waiting dialog".into());
    }
    logout_gate_stopped_in_time(&after)?;
    session.expect(BOOT_GREETER)?;
    println!("  ok: a restart from a terminal asked the windows, waited without a dialog, and a second boot came up");

    // 8. Restart from the desktop, with the editor holding something to lose: End anyway.
    logout_gate_login(&mut qmp, &mut session, size)?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxedit")?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;
    logout_gate_choose(&mut qmp, &mut session, power_click, RESTART)?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    let (wx, wy) = logout_gate_waiting(&mut session, "Waiting for 1 window to close:")?;
    // **Read back from the click**, not from the shell's line: `shutdown` runs as soon as the broker
    // has spawned it, and can say it is restarting before the shell has heard it was started.
    let from = session.transcript().len();
    click_at(&mut qmp, &mut session, wx + chrome::DIALOG_LEFT_CX, wy + chrome::DIALOG_BUTTON_CY)?;
    session.expect("desktop-shell: ending anyway, closing 1 window(s)")?;
    session.expect("desktop-shell: Restart: the broker started shutdown in the power view")?;
    session.expect(RESET)?;
    let after = session.transcript()[from..].to_string();
    for line in ["shutdown: restarting", "service-mgr: shutting down, to reboot, as asked"] {
        if !after.contains(line) {
            return Err(format!("the desktop's Restart reset the machine without `{line}`").into());
        }
    }
    logout_gate_stopped_in_time(&after)?;
    session.expect(BOOT_GREETER)?;
    println!("  ok: Restart waited for the editor, ended it anyway, asked the broker, and a third boot came up");

    // 9. Shut down from the desktop: the message, on the screen.
    logout_gate_login(&mut qmp, &mut session, size)?;
    let from = session.transcript().len();
    logout_gate_choose(&mut qmp, &mut session, power_click, SHUT_DOWN)?;
    session.expect("desktop-shell: Shut down: the broker started shutdown in the power view")?;
    session.expect("It is now safe to turn off your computer.")?;
    let after = session.transcript()[from..].to_string();
    for line in ["shutdown: shutting down", "service-mgr: shutting down, as asked"] {
        if !after.contains(line) {
            return Err(format!("the desktop's Shut down halted the machine without `{line}`").into());
        }
    }
    logout_gate_stopped_in_time(&after)?;
    expect_safe_on_screen(&mut qmp, &build_cache().join("check-logout-halt.ppm"))?;
    println!("  ok: Shut down asked the broker, and the screen, read back as text, says it is safe to turn off");
    println!(
        "\nxtask: logout gate PASSED — a logout waited for the editor's question, was cancelled, then \
         ended; one with nothing to ask ended at once; a restart from a terminal asked the windows; \
         and the desktop restarted the machine and shut it down ✓"
    );
    Ok(())
}

/// The power menu's rows, as `check-logout` chooses them: each by pressing Down that many times, so
/// their order is `desktop-shell`'s `panel` test's, which pins it (M11 decision 2).
const LOG_OUT: (&str, usize) = ("Log out", 1);
/// See [`LOG_OUT`].
const RESTART: (&str, usize) = ("Restart", 2);
/// See [`LOG_OUT`].
const SHUT_DOWN: (&str, usize) = ("Shut down", 3);

/// What a reset says last on COM1, before the next boot's first line.
const RESET: &str = "power: resetting through the FADT's reset register";

/// **The greeter of a boot**, and not the one presented again after a logout, whose line begins the
/// same way.
const BOOT_GREETER: &str = "desktop-session-mgr: greeter presented,";

/// **The shell went when asked, inside its supervisor's bound**, in the stretch of `transcript` a
/// shutdown or a restart covered.
fn logout_gate_stopped_in_time(transcript: &str) -> R<()> {
    if transcript.contains("desktop-shell is still running after it was asked to stop")
        || !transcript.contains("desktop-shell: asked to stop, exiting")
    {
        return Err("the shell outlived its supervisor's bound: its own wait for the windows must end first".into());
    }
    Ok(())
}

/// Log in at the greeter as the demo account, and wait for the shell's bars.
fn logout_gate_login(qmp: &mut Qmp, session: &mut Session, size: DisplaySize) -> R<()> {
    type_at_greeter(qmp, session, DEMO_USER)?;
    press(qmp, "tab")?;
    type_at_greeter(qmp, session, DEMO_PASSWORD)?;
    press(qmp, "ret")?;
    session.expect("desktop-session-mgr: login ok -> home=/home/alice")?;
    session.expect("desktop-shell: up (graphical session leader)")?;
    session.expect(&format!("desktop-shell: bottom bar placed at 0,{}", size.bottom_bar_y()))?;
    Ok(())
}

/// Launch `program` from the Applications menu, and return its window's id once the shell has
/// placed it and the screen has settled — so what is typed next reaches it.
fn logout_gate_launch(qmp: &mut Qmp, session: &mut Session, apps: (i32, i32), program: &str) -> R<u32> {
    click_at(qmp, session, apps.0, apps.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, program)?;
    press(qmp, "ret")?;
    session.expect(&format!("desktop-shell: launched {program} into its own namespace"))?;
    session.expect("desktop-shell: placed window ")?;
    let placed = session.rest_of_line()?;
    let (id, _, _) =
        parse_placement(&placed).ok_or_else(|| format!("could not read {program}'s placement from {placed:?}"))?;
    let _ = settle_and_capture(qmp, &build_cache().join("check-logout.ppm"))?;
    Ok(id)
}

/// Open the power menu at `power` and choose `row` — one of [`LOG_OUT`], [`RESTART`] and
/// [`SHUT_DOWN`] — by the keyboard.
fn logout_gate_choose(qmp: &mut Qmp, session: &mut Session, power: (i32, i32), row: (&str, usize)) -> R<()> {
    click_at(qmp, session, power.0, power.1)?;
    session.expect("desktop-shell: power menu open")?;
    for _ in 0..row.1 {
        press(qmp, "down")?;
    }
    press(qmp, "ret")?;
    session.expect(&format!("desktop-shell: ending the session: {}", row.0))?;
    Ok(())
}

/// Wait for the shell's waiting dialog, check what it says, and return its origin.
fn logout_gate_waiting(session: &mut Session, says: &str) -> R<(i32, i32)> {
    session.expect("desktop-shell: waiting dialog open, window ")?;
    let line = session.rest_of_line()?;
    // `<id> at <x>,<y>: <what it says>`
    let (head, said) = line.split_once(": ").ok_or_else(|| format!("no text in {line:?}"))?;
    if said.trim() != says {
        return Err(format!("the waiting dialog says {:?}, not {says:?}", said.trim()).into());
    }
    let at = head.split_once(" at ").map(|(_, a)| a).ok_or_else(|| format!("no origin in {line:?}"))?;
    let (x, y) = at
        .split_once(',')
        .and_then(|(x, y)| Some((x.trim().parse::<i32>().ok()?, y.trim().parse::<i32>().ok()?)))
        .ok_or_else(|| format!("could not read the dialog's origin from {line:?}"))?;
    Ok((x, y))
}

/// `cargo xtask check-fbcon` — **the boot is legible with no serial port at all** (Phase 5
/// Part B).
///
/// Boots the release userspace with `-serial none` and reads the screen back into text with the
/// kernel console's own glyphs, palette and geometry (`glyphs` and `text`, compiled here by path).
/// Nothing is learned from COM1, because on the machine this is for there is no COM1. Three claims:
///
/// 1. **The boot is on the screen**: the kernel's first and last lines, and userspace's lines up to
///    the compositor — the last thing printed before a client takes the screen.
/// 2. **The console lets go**: once the compositor has drawn, no console text reappears over it,
///    although every service goes on printing.
/// 3. **A stop takes the screen back**: a panic after the desktop is up repaints the console with
///    the diagnosis at the bottom, and nothing draws over it afterwards.
///
/// All three rest on the `fbcon-gate` kernel feature, which is why this gate builds
/// [`BuildMode::FbconGate`] rather than booting `check-login`'s image.
///
/// **Held frames, not lucky samples.** A dump costs 4–18 ms depending on the host, and a
/// scrolling console does not wait for it: the first version of this gate required
/// `compositor: up` to be *seen*, and that line is on screen only from its own paint to the
/// compositor's first frame — one dump in 1640 on a fast host, and missed 3 runs in 5 with a
/// dump slowed to CI's rate (PR #296 review). So the kernel holds the screen still for a second
/// after the timer is calibrated and again at the handout, and each claim below is read off one
/// of those frames — **all of its lines together, in a single frame**, which a hold guarantees
/// and no run of transient sightings can imitate.
fn cmd_check_fbcon(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::FbconGate)?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-fbcon.sock");
    let dump = work.join("fbcon.ppm");
    let _ = fs::remove_file(&qmp_sock);
    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
        // **The point of the gate.** No UART is created, so nothing the guest writes to COM1
        // goes anywhere, and the port reads back as a floating bus.
        .arg("-serial")
        .arg("none")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    println!("xtask: console gate — booting with no serial port…\n");
    let _session = Session::spawn(cmd, "check-fbcon")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    let look = |qmp: &mut Qmp, seen: &mut std::collections::BTreeSet<String>| -> R<Frame> {
        qmp.screendump(&dump)?;
        let (w, h, rgb) = parse_ppm(&fs::read(&dump)?)?;
        let frame = Frame::read(w, h, &rgb);
        for row in frame.lines() {
            if !row.is_empty() {
                seen.insert(row.to_string());
            }
        }
        Ok(frame)
    };

    // 1. The boot, read off the two held frames. Each group must appear whole in one frame.
    //
    // **The handout group's first line is the input server's bind, not the kernel's last line**
    // (administration Part B.4, moved again by Part E.1). It was the kernel's last line until the
    // boot grew past what one frame shows: Part B's device manager put 41 lines between it and
    // `compositor: up`, against the 36 a held frame guarantees. Then it was `init`'s auth-service
    // bind, until `service-mgr` took the servers over and logs two lines for each between. The
    // input server is bound just before the compositor starts. The kernel's own lines are the
    // early group's claim; this group's is that `sys_kprint` lines reach the screen up to the
    // handout, which any userspace line proves.
    const EARLY: &[&str] = &["Nitrox kernel — diagnostics online", "allocators up"];
    const HANDOUT: &[&str] = &["service-mgr: input-server bound at /dev/input/new", "compositor: up"];
    let (mut early, mut handout, mut console_seen) = (false, false, false);
    let mut rows = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let frame = look(&mut qmp, &mut seen)?;
        if frame.is_console() {
            console_seen = true;
            rows = frame.rows.len();
        }
        early |= frame.has_all(EARLY);
        handout |= frame.has_all(HANDOUT);
        // The handover: a frame that is not the console and has no console glyph anywhere on it,
        // after the console has been seen. Not "after a given line", which is what made the
        // first version of this loop wait on a sighting.
        if console_seen && frame.text_cells == 0 && !frame.is_console() {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "no console handover within 120s — {} distinct row(s) read off the screen, {}",
                seen.len(),
                if console_seen { "and the console never left it" } else { "and never a whole console frame" }
            )
            .into());
        }
    }
    if !early {
        return Err(format!(
            "no single frame showed {EARLY:?} — the kernel's first line (with its em dash, from the \
             face's table) and a line from before PCI. The `fbcon-gate` hold after the timer keeps \
             them up for a second. Read {} distinct row(s); the last dump is {}",
            seen.len(),
            dump.display()
        )
        .into());
    }
    println!("  ok: held after the timer, with no serial port — the kernel's first line, em dash and all, before PCI");
    if !handout {
        return Err(format!(
            "no single frame showed {HANDOUT:?} together — a line `init` printed as it brought its \
             services up, and the last before the handout. The hold at the handout keeps that frame \
             up for a second, and it shows at least the last {} lines ({rows} rows less a \
             quarter-screen jump), so a boot that now prints more than that after the first needs a \
             nearer one here. Read {} distinct row(s)",
            rows - rows / 4,
            seen.len()
        )
        .into());
    }
    println!(
        "  ok: held at the handout — a line init printed bringing its services up, and \"compositor: up\", on one screen"
    );

    // 2. The console let go. Every service keeps printing through all of this — the greeter's
    //    redraws, the heartbeat — so a console still drawing would put text over the desktop.
    const SETTLE: std::time::Duration = std::time::Duration::from_secs(6);
    let until = std::time::Instant::now() + SETTLE;
    let mut dumps = 0;
    while std::time::Instant::now() < until {
        let frame = look(&mut qmp, &mut seen)?;
        dumps += 1;
        if frame.text_cells > 0 {
            return Err(format!(
                "console text reappeared over the desktop after the handover: {} cell(s) read as \
                 ink-on-paper glyphs, e.g. {:?} (dump at {})",
                frame.text_cells,
                frame.cells_sample(),
                dump.display()
            )
            .into());
        }
    }
    println!(
        "  ok: the compositor has the screen, and {dumps} dumps over {}s found no console text on it",
        SETTLE.as_secs()
    );

    // 3. A stop takes it back.
    qmp.send_key("f10", true)?;
    qmp.send_key("f10", false)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let panic_frame = loop {
        let frame = look(&mut qmp, &mut seen)?;
        if frame.is_console() && frame.lines().iter().any(|l| l.contains("*** KERNEL PANIC ***")) {
            break frame;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "no panic on the screen within 30s of F10 — the last dump ({}) {}",
                dump.display(),
                if frame.is_console() { "is console text without a panic in it" } else { "is not the console" }
            )
            .into());
        }
    };
    let message = "fbcon-gate: F10 pressed, and this kernel was built to stop on it";
    if !panic_frame.lines().iter().any(|l| l.contains(message)) {
        return Err(format!("the panic reached the screen without its message {message:?}").into());
    }
    println!("  ok: the panic took the screen back from the desktop, message and all");
    // Held, not merely drawn: the other CPUs were told to halt before the repaint, so nothing —
    // least of all the compositor — draws over it. **The pointer is moved to give something a
    // reason to.** An idle greeter draws nothing for seconds on end, so without the motion this
    // check passed with the stop's NMIs deleted and the compositor still running (measured
    // 2026-09-14). A running machine answers motion with a cursor, drawn over the text; a stopped
    // one does not answer — the PS/2 bytes sit in the controller, since every CPU that would
    // drain them, by interrupt or by its tick, is halted.
    for _ in 0..6 {
        qmp.send_motion(60, 40)?;
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    let later = look(&mut qmp, &mut seen)?;
    if later.lines() != panic_frame.lines() || !later.is_console() {
        return Err(format!(
            "something drew over the panic within 2s of it reaching the screen (dump at {})",
            dump.display()
        )
        .into());
    }
    println!("  ok: and it is still there 2s later, untouched");

    let _ = fs::remove_file(&qmp_sock);
    println!("\nxtask: the boot, the handover and a stop are all legible with no serial port ✓");
    Ok(())
}

/// **The shell's last whole window list** in `transcript`: what follows `window list on cli of ` on
/// the last line holding it **that has ended**. The transcript is read as it arrives, so the shell's
/// next list may be half there — a line cut after its second window, counted, said two, and once
/// after its first said one: `check-login --usb`'s two failures in Phase 6 Part B.7's rounds, where
/// the whole line before it held all three. A match is not a line (PR #352 review).
fn last_window_list(transcript: &str) -> Option<&str> {
    let whole = &transcript[..transcript.rfind('\n').map_or(0, |i| i + 1)];
    let (i, m) = whole.rmatch_indices("window list on cli of ").next()?;
    whole[i + m.len()..].lines().next()
}

/// **Paced on the greeter's redraw counter**, because a greeter has no echo. What was typed is
/// on screen and nowhere else, and this window repaints 420×200 per keystroke — the cost
/// `check-terminal` types one character at a time to stay behind. Waiting for the redraw is
/// the same discipline against a different receipt.
fn cmd_check_login(accel: Accel, usb: bool, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-login.sock");

    println!("xtask: graphical login gate — booting the release image…\n");
    // **`--usb`: a machine with no i8042** (Phase 6 Part B), so every key at the greeter is typed on
    // a USB keyboard: the Definition of Done's "types at the greeter".
    let (mut session, mut qmp) = spawn_release_guest(accel, "check-login", &qmp_sock, size, false, usb)?;

    // 0. **The AHCI controller took the MSI path in a *release* image.** `test-qemu` already
    //    adjudicates this (`check_ahci_msi_path`), but only for the selftest build — and the
    //    image that will be carried to the laptop is this one. It costs one expectation
    //    because the driver binds long before anything draws.
    //
    //    Ordered **first**, before the greeter lines, because `expect` consumes what it scans
    //    past: asserting it after them would find nothing and hang. It is also why this reads
    //    the path token rather than the vector — the vector differs between this image and the
    //    selftest one (0x30 here, 0x31 there), since the selftest build's `IrqRouter::self_test`
    //    takes a device vector for the PIT and never releases it.
    session.expect("ahci: irq via MSI")?;

    // 0b. **The greeter is centred on the screen this gate booted** (Phase 5 Part E). It centred
    //     on a written-down 1280×800 until then, which on the laptop put it 43 px left and 16 px
    //     low of centre. Its size is `desktop-session-mgr`'s `GREETER_W`×`GREETER_H`, written down
    //     here a second time for the reason every chrome metric in this file is (M11 decision 2).
    //     [`GREETER`] says what keeps this copy honest.
    //     **Before the first redraw**, because the greeter reads the screen before it opens its
    //     window, and `expect` consumes what it scans past.
    session.expect(&format!(
        "desktop-session-mgr: greeter centred at {},{} on a {} screen",
        (size.w - GREETER.0) / 2,
        (size.h - GREETER.1) / 2,
        size
    ))?;
    println!("  ok: the greeter centred itself on the {size} screen");

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
    // **And the view broker's `/dev/views`, at the session's base** (administration Part A.4).
    // Printed before the leader is spawned, so it is ordered against the next line, not racing it.
    session.expect("desktop-session-mgr: session has /dev/views")?;
    // **And the device manager's tables, at the base `/info`** (administration Part B.4) — printed
    // right after, from what was bound. The application namespaces the shell builds carry them too,
    // which the shell checks by resolving the binding and says on its `grants` line below.
    session.expect("desktop-session-mgr: session has /dev/devices")?;
    // **And `/storage`** (administration Part C.6): the storage service's session endpoint, which
    // the supervisor resolved itself and bound at `/storage` and `/dev/storage`.
    session.expect("desktop-session-mgr: session has /storage")?;
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
    // deliberately not the built-in 13.
    //
    // **The drawn size tells fit from fill again, at the gates' screen** (Phase 5 Part E). On the
    // 1280×800 screen every gate booted until then, 16:10 into 16:10 is `1280x800 at 0,0` under
    // either rule; on 1360×768 a fill is `1360x850 at 0,-41` and a fit `1228x768 at 66,0`. The
    // expected line is computed here with `libdraw::scale::fill` — the staged theme's mode — on
    // the size this gate booted, so the arithmetic has one source and its own tests pin it.
    //
    // A picture is pixels a release-image boot has no reference for, which is what the plan says
    // about this gate; the dimensions are what can be asserted, and they pin the whole chain —
    // the theme naming a file and a mode, `libfs` reading it, `libdraw::png` decoding it, and
    // `libdraw::scale::fill` placing it.
    // **And the line is printed after the window is committed**, which the first version of
    // this step was not: it asserted a line emitted as soon as the picture had been placed, so
    // it passed while `CreateWindow` failed and the desktop showed its bare ground colour. The
    // `window N` on the end is what makes this an assertion about a picture on screen rather
    // than about arithmetic.
    let drawn = libdraw::scale::fill(
        libdraw::geom::Size::new(WALLPAPER_W, WALLPAPER_H),
        libdraw::geom::Size::new(size.w, size.h),
    );
    session.expect(&format!(
        "desktop-shell: wallpaper {WALLPAPER_W}x{WALLPAPER_H} drawn {}x{} at {},{} window ",
        drawn.size.w, drawn.size.h, drawn.origin.x, drawn.origin.y
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
    // **And `/session/user`, since administration Part F.1**: `desktop-shell` builds an
    // application's namespace with `libsession`, as the supervisors build a session's, and the
    // one piece its own builder had never bound was who the session is for. `whoami` in a desktop
    // terminal said "no session identity" until then. The shell resolves it in the namespace it
    // built, and names it here only if it answered.
    session.expect("desktop-shell: serving /dev/desktop")?;
    session.expect("desktop-shell: application /dev/desktop bound")?;
    session.expect("desktop-shell: application namespace grants new + /home + /session/user + /dev/devices + /storage, withholds manage")?;
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
    // **The shell's `SetScheme` reached the compositor and was accepted** (desktop refresh, Part
    // A) — the one process that never reads the theme file, and the one that draws every window's
    // shadow. The shell sends it whatever the scheme, so the line appears on a light boot too.
    //
    // **What this proves on this image is the path, not the value.** The staged scheme is light
    // and the compositor starts light, so a compositor that accepted the message and applied
    // nothing would log the same line and pass. The value's mapping is pinned by the manager's
    // unit test, `a_scheme_repaints_everything_once_and_an_unknown_one_is_refused`. The expected
    // name is computed from the staged theme rather than written as `light` so that the day the
    // staged scheme changes, this starts checking the value without anyone remembering to
    // (PR #312 review, optional 7, which caught the comment claiming more).
    let scheme = staged_theme()?.0.scheme;
    session.expect(&format!("compositor: scheme {}", scheme.as_str()))?;

    // **The work area, and that it is not the screen** (M9 Part B). The shell asks the
    // compositor rather than subtracting its own bars, because any `panel`-role client declares
    // a strut and only the compositor sees them all. If this ever equals the screen, the struts
    // are not being counted and every maximised window will sit under the bars.
    session.expect("desktop-shell: work area ")?;
    let work_line = session.rest_of_line()?;
    let work = parse_work_area(&work_line).ok_or_else(|| format!("could not read the work area from {work_line:?}"))?;
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
    // window-list click landing on nothing — which says the bar is not *there* rather
    // than where it is (PR #242 review, optional 9).
    session.expect(&format!("desktop-shell: bottom bar placed at 0,{}", size.bottom_bar_y()))?;
    // **The count, not just the prefix** (PR #279 review, finding 7). This is the one line that
    // distinguishes desktop entries from the `/bin` listing they replaced: a regression to
    // listing every program would still open the menu, still match `nxterm`, and still launch it,
    // so every other expectation in this gate holds under both behaviours.
    session.expect(&format!("desktop-shell: /applications lists {STAGED_APPLICATIONS} application(s)"))?;
    // After it, because the chord is registered on the loop's first pass and the entries
    // are read before the loop. `expect` scans forward, so asserting these out of order times
    // out on a line already behind the cursor.
    session.expect("Super+R names, Super+A opens applications")?;
    session.expect("desktop-shell: Super+H minimizes the focused window")?;

    let mut st = LoginState { qmp_sock, shell_px, wallpaper_id, work, ..LoginState::default() };
    login_menus_and_buttons(&mut qmp, &mut session, size, &mut st)?;
    login_window_list_and_overview(&mut qmp, &mut session, size, &mut st)?;
    login_overview_drags(&mut qmp, &mut session, &mut st)?;
    login_file_browser(&mut qmp, &mut session, &mut st)?;
    login_editor(&mut qmp, &mut session, size, &mut st)?;
    login_buffers_and_clipboard(&mut qmp, &mut session, &mut st)?;
    login_dialogs_and_places(qmp, session, size, &mut st)
}

// **A receipt per character, and it is a count rather than an echo.** An editor's echo is
// its own window, and this gate boots a release image — there are no rendered glyphs to
// read on serial. Injection is relative and unacknowledged, so without a per-key wait a
// dropped PS/2 batch would show up as a *wrong file* two steps later.
const TYPED: &str = "nitrox";

//    The bar spans the screen at y=0 and `Applications` is its first word, 105 pixels wide
//    at the staged 12 px (111 at the built-in 13), so a press at (60, 12) lands inside it —
//    which `desktop_shell::panel`'s own test pins against this literal at both sizes
//    (desktop refresh, Part C).
const APPS_CLICK: (i32, i32) = (60, 12);

const TITLE_BAR_H: i32 = chrome::TITLE_BAR_H;

// **And the menu bar above the path strip** (M12 Part B), which moved every row down by its
// height. `nxfiles::list_top` is the browser's own version of this sum; a gate that had
// missed the change would press one row high and drag the wrong file.
const MENU_BAR_H: i32 = 24;

// **Where a listing row is.** Hoisted here from the drag step below, which is the other thing
// that aims at one — two copies of this sum is how a gate comes to press one row high after
// a strip changes height. `nxfiles::list_top` is the browser's own version of it.
// **The sidebar takes the left of the content** (M14 Part D), so every aim at a *listing* row
// starts past it while the strips above still span the full width. `nxfiles::SIDEBAR_W` is
// the browser's own version of this number.
const SIDEBAR_W: i32 = browser::SIDEBAR_W;

const ROW_H: i32 = browser::ROW_H;

const PATH_H: i32 = browser::PATH_H;

// Every `Super`-chord this gate presses goes through here, so the four `send_key` calls a
// chord is have one spelling rather than one per step.
/// Press `code` with `Super`, and `Shift` too when `shift`, then let them all go.
fn chord(qmp: &mut Qmp, shift: bool, code: &str) -> R<()> {
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
}

/// What `check-login`'s steps hand on to the steps after them.
#[derive(Default)]
struct LoginState {
    edit_id: u32,
    files_win: (u32, i32, i32, u32, u32),
    first_term_id: u32,
    fx: i32,
    fy: i32,
    list_click: (i32, i32),
    qmp_sock: PathBuf,
    shell_px: u32,
    term_h: u32,
    term_id: u32,
    term_w: u32,
    term_x: i32,
    term_y: i32,
    wallpaper_id: u32,
    work: (i32, i32, u32, u32, u32, u32),
}

/// `check-login`'s steps 4 to 6: the Applications menu, a terminal launched from it, its title
/// bar's buttons and the taskbar's.
fn login_menus_and_buttons(qmp: &mut Qmp, session: &mut Session, size: DisplaySize, st: &mut LoginState) -> R<()> {
    let work = st.work;
    // 4. **The Applications menu, by both of its triggers** — `desktop-shell.md` §4's word on
    //    the top bar and its chord. The chord is `Super+A` since Phase 5, because the word is on
    //    a `panel` and a panel can only be clicked: on the laptop Phase 5 targets there is no
    //    pointing device until USB (Phase 6), so without a chord a session could be logged into
    //    and never used.
    //
    //    Asserted through the compositor's own `press at` line first, for the reason
    //    `check-terminal` gives: it separates "the pointer was not there" from "the pointer was
    //    there and nothing happened".
    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;

    //    **And the chord, closing what the word opened, then opening it again.** Closing first
    //    is deliberate: it is the half the word cannot do — a press on it dismisses the open menu
    //    and the click then reopens it, as on every window's menu bar — and it leaves the launch
    //    below driven entirely by the keyboard, which is the path the laptop has. A chord that
    //    only ever opened would pass this gate while leaving a person no way out of the menu but
    //    Escape.
    chord(qmp, false, "a")?;
    session.expect("desktop-shell: applications menu closed")?;
    chord(qmp, false, "a")?;
    session.expect("desktop-shell: applications menu open")?;

    // 5. **Type to filter, then launch.** The menu is a `popup`, so it holds the keyboard —
    //    the property `check-terminal` relies on when it says an open menu "is a topmost popup
    //    and takes the keyboard". The top bar could not receive these keys at all.
    //
    //    **`nxterm`, which is what makes the milestone visible**: a person types into the
    //    Applications menu and a terminal opens. Part E launched a coreutil because the
    //    mechanism was the deliverable; Part F is the thing launched being worth looking at.
    type_into_menu(qmp, session, "nxterm")?;
    press(qmp, "ret")?;
    // Each line is a distinct claim: the namespace was built and **checked** before anything
    // ran in it, and only then was the program spawned into it.
    session.expect("desktop-shell: application namespace grants new + /home + /session/user + /dev/devices + /storage, withholds manage")?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    // **Only the shell's own lines are ordered here.** `nxterm` starts concurrently with the
    // shell closing the menu, so an `expect` between the two is a race between processes —
    // the flake PR #227's review caught and PR #236's avoided. What `nxterm` did is checked
    // against the whole transcript below, where order does not matter.
    session.expect("desktop-shell: applications menu closed")?;
    // **And the shell placed it**, which is the manager half actually doing something rather
    // than merely holding a channel. Every window created while a manager is attached is
    // announced to it, and a `normal` one's first `Configure` is *held* until the manager acts
    // — so a shell that received `WindowCreated` and did nothing would leave launched
    // applications invisible, a failure that looks like they never started.
    //
    // **Asserted against the launched terminal, and after the close.** It sat after the menu
    // *opened* until M8 Part C, where the shell stopped placing its own windows — a menu is a
    // `popup`, placed by its creator and never held for anyone, so the line it matched was a
    // placement that could not have been load-bearing. The terminal's placement arrives one
    // loop iteration after the close, when its `WindowCreated` is drained.
    session.expect("desktop-shell: placed window ")?;
    // **Remembered, because the drag below needs to know where it started.** The line reads
    // `<id> at 0,<y>`; nothing moves this window between here and there — a desktop change and a
    // minimise leave the origin alone — so this is its origin at that point.
    let placed = session.rest_of_line()?;
    let (term_id, term_x, term_y) =
        parse_placement(&placed).ok_or_else(|| format!("could not read the terminal's placement from {placed:?}"))?;
    // Its width, which the title bar's buttons are measured from — **before the window-list
    // line**, which follows it: an `expect` scans forward, so asserting the list first would
    // consume past the geometry this needs.
    session.expect(&format!("desktop-shell: window {term_id} geometry "))?;
    let geom_line = session.rest_of_line()?;
    let (term_w, term_h) = parse_geometry(&geom_line)
        .ok_or_else(|| format!("could not read the terminal's geometry from {geom_line:?}"))?;

    // And it is listed, focused, because it has the keyboard.
    session.expect("desktop-shell: window list on ")?;

    // Where a window-list entry sits: the first task button on the bottom bar, whose height comes
    // from the screen this gate booted. `DisplaySize::task_click` is hardcoded like every other
    // chrome metric this gate aims at, and for the same reason: a gate that read the shell's layout
    // to know where to click could agree with a shell that had stopped drawing where it says (M11
    // decision 2).
    let list_click: (i32, i32) = size.task_click(0);

    // 6a2. **The title bar's buttons ask, and the shell disposes** (M9 Part B). A client cannot
    //      minimise or maximise itself — both are manager operations — so the button sends
    //      `Surface::RequestState`, the compositor forwards it, and the shell decides.
    //
    // **The buttons are measured from the right edge in layout order** — minimise, maximise,
    // close; see `chrome`. Part C added close, which moved the other two a slot left; the first
    // run after that clicked *close* where it meant maximise, which is what a coordinate constant
    // hides and a changing layout exposes.
    let button_y = term_y + chrome::TITLE_Y;
    // Measured from the window's **right edge**, which is its origin plus its width — not from
    // its width, which is the same number only while windows are placed at x=0.
    let right = term_x + term_w as i32;
    let maximise_at = (right - chrome::MAXIMISE_X, button_y);
    let minimise_at = (right - chrome::MINIMISE_X, button_y);

    // **Maximise here is asserted as far as the shell's answer** — that the ask reached it and
    // that it answered with the *work area* rather than the screen. What the client does with
    // the size is 6g's, which is where the whole round trip is read back.
    click_at(qmp, session, maximise_at.0, maximise_at.1)?;
    // **In any order**: `nxterm` logs its request after the compositor answers, and the compositor
    // answers once it has forwarded it, so the shell's line can come first (PR #353's CI).
    let maximised = format!("desktop-shell: maximize window {term_id} to {},{} {}x{}", work.0, work.1, work.2, work.3);
    session.expect_all(&["nxterm: asked the shell for window state 2", &maximised])?;
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
    session.expect(&format!("desktop-shell: window {term_id} geometry {},{} {}x{}", work.0, work.1, work.2, work.3))?;
    click_at(qmp, session, work.0 + work.2 as i32 - chrome::MAXIMISE_X, work.1 + chrome::TITLE_Y)?;
    // **In any order**: `nxterm` logs its request after the compositor answers, and the compositor
    // answers once it has forwarded it, so the shell's line can come first (PR #353's CI).
    session.expect_all(&[
        "nxterm: asked the shell for window state 0",
        &format!("desktop-shell: restore window {term_id} to "),
    ])?;
    // The size is pinned by the client's own line rather than by the shell's: the shell prints
    // what it asked for, and what this step is about is the window going back to the shape it
    // had — which only the client can say it did.
    session.expect(&format!("nxterm: resized to {term_w}x{term_h}, grid "))?;
    println!("  ok: and the same button restored it to where it came from");

    // **And minimise, end to end**, because nothing in it depends on a client honouring
    // anything: the window leaves the screen and the bar marks it with `_`.
    click_at(qmp, session, minimise_at.0, minimise_at.1)?;
    // **In any order** (PR #353's CI, the same class one step down): `nxterm` logs its request
    // after the compositor answers it, and the compositor answers once it has forwarded it, so the
    // shell can be first. The list's `_` exists only after the minimise, so no earlier line
    // answers for it. `_` is the bar's mark for a minimized window. The desktop count is not
    // asserted: the lifecycle rule appends an empty desktop as soon as one has a window, so it is
    // 2 here and says nothing about minimising.
    session.expect_all(&[
        "nxterm: asked the shell for window state 1",
        &format!("desktop-shell: client asked to minimize window {term_id}"),
        &format!("desktop-shell: window list on Desktop 1 of 2 [{term_id}:_ "),
    ])?;

    // Restore it from the list, which is where a minimized window comes back from, so the steps
    // below have a window to work with.
    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;

    // **And minimise it a second time**, which is the step that was missing. The compositor drops
    // a request for the state a window last *asked* to be in, to keep a looping client off a
    // bounded queue — and the taskbar restore above is a `SetMinimized` from the *manager*, which
    // does not go through that path. With the shadow left stale, this second press was dropped as
    // a repeat, the client was told it had succeeded, and the button stayed dead until some other
    // state was asked for first (PR #249 review, blocking 1).
    click_at(qmp, session, minimise_at.0, minimise_at.1)?;
    // **In any order**: `nxterm` logs its request after the compositor answers, and the compositor
    // answers once it has forwarded it, so the shell's line can come first (PR #353's CI).
    session.expect_all(&[
        "nxterm: asked the shell for window state 1",
        &format!("desktop-shell: client asked to minimize window {term_id}"),
    ])?;
    println!("  ok: the minimise button still works after the taskbar restored the window");
    click_at(qmp, session, list_click.0, list_click.1)?;
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
    middle_click_at(qmp, session, list_click.0, list_click.1)?;
    // **Only the ordered half is an `expect`.** The compositor logs before it replies, so it
    // leads; the client then wakes and answers. The *shell's* own line comes after its request
    // returns, which is a race against the client it just woke — observed on both sides of the
    // client's two lines — so it is checked against the whole transcript below, where order does
    // not matter. Same rule as `nxterm`'s output in PR #227.
    session.expect(&format!("compositor: asked window {term_id} to close"))?;
    session.expect("nxterm: asked to close, exiting")?;
    session.expect("nxterm: closing")?;
    // **Its desktop went with it, and the compositor was told.** Desktop 1 was emptied and
    // unnamed, so the lifecycle rule removed it and the shell landed on the one after — which is
    // `Desktop 1` now, by position. Until the desktop refresh's Part C this move reached only the
    // shell; step 6c2 is where the difference showed.
    session.expect("desktop-shell: the compositor follows to Desktop 1")?;
    // The compositor tore the windows down with the session, and the list lost the entry.
    session.expect("desktop-shell: window list on Desktop 1 of 1 (empty)")?;
    println!("  ok: the taskbar asked, and the client closed itself");
    let first_term_id = term_id;

    // Launch another, because everything below needs a terminal — and **by clicking the row this
    // time**, which is the second launch path and did not exist until M11 Part E batch 4. The
    // shell read pointer events for the overview, the applications button and the taskbar, and
    // never for the modal's own window: its rows could not be clicked at all, and nothing under
    // the cursor reacted. Everything after this step depends on the terminal, so a click that
    // silently does nothing fails the rest of the gate rather than passing quietly.
    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    // Filtered to one row first, so the row being clicked is known without the gate having to
    // work out where `nxterm` sorts in the contents of `/bin`.
    type_into_menu(qmp, session, "nxterm")?;
    // **The menu hangs 8 pixels in from its word and 2 below the bar**, at (8, 32), with the
    // filter field above its rows (desktop refresh, Part C) — so the one row left sits a field's
    // height below that, at y 69 to 93 in the staged 12 px (71 to 97 at the built-in 13).
    // `desktop_shell::panel`'s own test pins this literal against a layout of the menu at both.
    // `click_at` asserts the press position before anything downstream is checked, which is what
    // separates "the pointer was not over the row" from "it was, and the click did nothing".
    const ROW1: (i32, i32) = (60, 82);
    click_at(qmp, session, ROW1.0, ROW1.1)?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    session.expect("desktop-shell: placed window ")?;
    let replaced = session.rest_of_line()?;
    let (term_id, term_x, term_y) = parse_placement(&replaced)
        .ok_or_else(|| format!("could not read the replacement terminal's placement from {replaced:?}"))?;
    session.expect(&format!("desktop-shell: window {term_id} geometry "))?;
    let regeom = session.rest_of_line()?;
    let (term_w, _term_h) =
        parse_geometry(&regeom).ok_or_else(|| format!("could not read the replacement's geometry from {regeom:?}"))?;

    // 6. **And the top bar still works.** The modal used to be opened once and never closed,
    //    so it stayed on top of whatever was launched and the bar's click handler — gated on
    //    there being no modal — was inert for the rest of the session: no second launch, no
    //    way back. Clicking again is the direct test; asserting the close alone would be a
    //    proxy for it (PR #237 review, finding 6).
    // **Escape first, so this step's precondition is stated rather than assumed.** `click_at`
    // retries a press that did not land where it was aimed, and an abandoned attempt still
    // *pressed* somewhere — if that somewhere is the Applications word (a mis-walked pointer
    // parks at x=0) the menu is already open, and the aimed click then dismisses it and opens it
    // again: the line below matches either way, and would be asserting less than it says. Escape
    // closes a menu left open, and with none open it reaches the focused terminal and does
    // nothing.
    press(qmp, "esc")?;
    session.skip_to_end()?;
    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;

    st.first_term_id = first_term_id;
    st.list_click = list_click;
    st.term_h = term_h;
    st.term_id = term_id;
    st.term_w = term_w;
    st.term_x = term_x;
    st.term_y = term_y;
    Ok(())
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

/// Whether the file browser started at `path` and **then** listed it.
///
/// **Line by line, and the listing only after the start.** One browser prints both, in that
/// order, so the order is the claim; a listing of the same directory from before the start — which
/// `check-login` has, from the sidebar step — says nothing about this browser. Exact on the start
/// line, so `/home/Documents/sub` is not `/home/Documents`. Another browser listing the same
/// directory in between would satisfy it; none is driven while step 13 runs.
fn started_and_listed(transcript: &str, path: &str) -> Result<(), String> {
    let start = format!("nxfiles: starting at {path}");
    let listed = format!("nxfiles: listed {path} - ");
    let mut lines = transcript.lines().map(|l| l.trim_end_matches('\r'));
    if !lines.any(|l| l == start) {
        return Err(format!("the transcript has no \"{start}\""));
    }
    if !lines.any(|l| l.starts_with(&listed)) {
        return Err(format!("nothing after \"{start}\" says \"{listed}…\""));
    }
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
    // typed in the Applications menu and got a terminal. It found a font, a terminal and
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
    // "lists 0 programs", and "menu open" says nothing about its contents — so a session
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
    if transcript.contains("desktop-shell: /applications lists 0 application(s)") {
        return Err("the Applications menu is empty: no desktop entries reached the session \
             namespace. Either the `/applications` bind failed (the session log says so), the \
             directory would not open, or every entry was malformed. The menu opening is not \
             evidence it has anything in it"
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

/// Move the browser's selection from a fresh `/home` listing down to `papers`.
///
/// **Because row 0 stopped being `papers`.** M14 Part D stages `Documents`, `Downloads` and
/// `Pictures` into the demo home for the sidebar to point at, and directories lead the listing —
/// so a fresh listing selects `Documents` and the two steps that pressed Enter on it descended
/// into the wrong directory. The old comment predicted precisely this and said it would fail
/// loudly, which it did.
///
/// Each arrow is acknowledged by the browser's `selected` receipt, which is both a per-keystroke
/// pace and the assertion that the arrow moved where this thinks it did.
fn select_papers(qmp: &mut Qmp, session: &mut Session) -> R<()> {
    for name in ["Downloads", "Pictures", "papers"] {
        press(qmp, "down")?;
        session.expect(&format!("nxfiles: selected {name}"))?;
    }
    Ok(())
}

/// Wait for `pat` after injecting pointer input, **flushing QEMU's mouse queue while waiting**.
///
/// **QEMU holds a packet it cannot fit rather than dropping it.** Its PS/2 queue is sixteen bytes,
/// and `ps2_mouse_send_packet` returns without queuing when a packet will not fit, leaving the
/// buttons and motion in the device's state (`hw/input/ps2.c`, the same in 8.2 and 11.0). They go
/// out at the device's next sync — and only an injected event causes one. So a gate that injects a
/// release last and then waits can wait for ever: the guest drains the queue, but nothing asks QEMU
/// to send what it held. That was the whole cause of the `lost-release` deferral, reproduced by holding the
/// guest's drain for a walk and a click (2026-09-23): ten releases held out of ten, each delivered
/// by one event that moved nothing.
///
/// So each second without `pat` sends [`Qmp::flush`], which delivers only what was already
/// injected — and a later flush delivers what an earlier one had no room for. Forty-five seconds
/// in all, as [`Session::expect`].
///
/// **A press held together with walk motion still lands short**: QEMU packs both into one packet
/// and the guest's decoder emits the button before the motion. What this recovers is a button
/// held on its own, which is the case a gate's last event is (PR #331 review, optional 2).
fn expect_after_pointer(qmp: &mut Qmp, session: &mut Session, pat: &str) -> R<()> {
    const SLICE: std::time::Duration = std::time::Duration::from_secs(1);
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
    let start = std::time::Instant::now();
    while start.elapsed() < TIMEOUT {
        if session.expect_within(pat, SLICE)? {
            return Ok(());
        }
        qmp.flush()?;
    }
    Err(session.timeout_report(&format!("{pat:?}"), TIMEOUT)?.into())
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
