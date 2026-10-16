//! `test`: every workspace's host tests.

use crate::*;

pub(crate) fn cmd_test() -> R<()> {
    // Tools workspace tests (xtask itself, image-builder helpers, etc.).
    let tools_manifest = repo_root().join("tools").join("Cargo.toml");
    run(Command::new("cargo").arg("test").arg("--manifest-path").arg(&tools_manifest))?;

    // Kernel host tests. The kernel's `.cargo/config.toml` pins the
    // build target to `x86_64-unknown-none`, which can't link the
    // standard test harness, so we force the host triple here. `--lib`
    // skips the `[[bin]]` (it's `#![no_main]`, unbuildable on host).
    let host = host_triple()?;
    let kernel_dir = repo_root().join("kernel");
    run(Command::new("cargo").arg("test").arg("--lib").arg("--target").arg(&host).current_dir(&kernel_dir))?;

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
    // init's library tests (the first message a resource server sends; the `manifest` and
    // `toml_lite` parsers moved to `libinittoml`, tested below). `--lib` skips the
    // `#![no_main]` bin, which can't build for the host.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("init")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libgpt` host tests: the partition tables the installer writes and reads (Phase 5 Part
    // H.1). Two of them hold the crate to **`sgdisk`** — it verifies what we write and we read
    // what it wrote — because a partition table is a format other people's firmware consumes, and
    // agreeing only with ourselves would prove nothing. `gdisk` is a project dependency alongside
    // `e2fsprogs`, and the tests panic with that message if it is missing.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libgpt")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `nxinstall`'s **layout** tests (Phase 5 Part H.1): where the two partitions go on a
    // target, and every reason to refuse one. `--lib` skips the `#![no_main]` bin. The
    // arithmetic is here rather than in the program because its mistakes destroy a disk and are
    // invisible in a boot that succeeds — a root partition one block into the backup array
    // installs a machine that works until something rewrites the table. Since PR #348's review
    // they also copy a pristine root into an empty filesystem in memory and write the first
    // account over it, since `check-install`, the only boot that installs, is not in CI.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("nxinstall")
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
    // view-broker's library tests (the policy reader, rule evaluation, the last-administrator
    // guard, per-session pacing, session ids, and pairing exits with their codes). `--lib` skips
    // the `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("view-broker")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // device-mgr's library tests (names, classes and one owner each, the replay, the tables and
    // the suffixes). `--lib` skips the `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("device-mgr")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // storage-service's library tests (what a device holds, against real `mformat` and GPT
    // sectors and an ext4 `mkfs` made; `init.toml`'s mounts matched to devices; the live-boot rule;
    // the tables). `--lib` skips the `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("storage-service")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libusers` — the user database's format and its edits, shared by `auth-service`, `account`
    // and this build since administration Part D.1.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libusers")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libinittoml` — the `init.toml` parser, shared by `init` and the storage service since
    // administration Part C.5, and its tests with it.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libinittoml")
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
    // `libfsserver`'s own tests (Phase 6 Part E.1). Its resolve core is generic over a volume,
    // so the bulk of what tests it runs above, through `fs-server-ext4`'s.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libfsserver")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `fs-server-fat`'s library tests (Phase 6 Part E), against images mtools and `mkfs.fat`
    // build, and — for what it writes — `fsck.fat -n`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("fs-server-fat")
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
    // `libprompt` — asking a person on a terminal (administration Part G.2): the new password typed
    // twice, held to `libusers`' rules. It was `coreutils::prompt`, whose tests ran under
    // `-p coreutils`; moved, they would stop running unless named here.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libprompt")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libviews` — the view broker's client (administration Part F.3): a request's handles in the
    // order the broker takes them, and reading a listing for the answer a request would get. The
    // agreement of that reading with the broker's own rule is `view-broker`'s test, above.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libviews")
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

    // `desktop-shell`'s library — the desktop-entry parser, the Applications menu's filter, and since Phase 5
    // Part E the layout arithmetic on the screen's size (the window list's placement, and since the
    // desktop refresh's Part C both bars as element trees, the task capacity beside the switcher
    // and this gate's aims pinned as literals). **Its own doc said this ran long before it did**:
    // the three tests it had were compiled by nothing, so a capacity bug that painted an entry
    // under the indicator would have passed CI.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("desktop-shell")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;

    // `desktop-session-mgr`'s greeter — its state, the keys it acts on itself, and the window it
    // draws, all of which are functions of values (desktop refresh, Part D). **The size this file
    // writes down as `GREETER` is measured there**, so the gate's copy and the greeter's own
    // cannot drift without a host test failing first.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("desktop-session-mgr")
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

    // `libsession`'s one pure decision: which namespace bindings are block devices, for the
    // rebinding that reads a source's own bindings where it has no registry (administration Part
    // B.5). The rest of the crate is syscalls, which the gates boot.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libsession")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;

    // `input-server`'s merge — the devices' streams into one ordered run, sent as batches that
    // end on group boundaries — what a slow consumer is owed, and the set of devices the device
    // manager hands over. Same split and the same reason: all the behaviour, no syscalls.
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
