use crate::*;

/// Stage the root filesystem's tree for `mode` into `staging` — everything `mke2fs -d` puts on
/// `nitrox-root`: `/system`, the store, the demo home, the fonts.
///
/// **One function for every image that carries a root**, extracted from `assemble_image` in
/// Phase 5 Part C so the live image's `root.img` is built from the release root's staging rather
/// than a copy of its steps. `check-images` does not trust that and compares the built filesystems
/// anyway — see `check_live_image`.
pub(crate) fn stage_rootfs(staging: &Path, mode: BuildMode) -> R<()> {
    fs::create_dir_all(staging.join("system"))?;
    // `/scratch` — the backing directory for the **second writable mount**. The kernel
    // calls a rename cross-filesystem when the two paths resolve to a different (server,
    // subtree base) pair, so binding this one server a second time with base `/scratch`
    // yields a destination that is genuinely cross-mount to `/system` while staying
    // writable. That combination did not exist before (2026-07-30): `/initramfs` is
    // cross-mount but read-only, so only the *detection* half of `move`'s fallback could
    // ever run. Empty here; a test image's `init.toml` binds it (`TEST_BINDS_TOML`) and the harness
    // populates it.
    fs::create_dir_all(staging.join("scratch"))?;
    fs::write(staging.join("system").join("current-generation"), b"nitrox-rootfs generation 1\n")?;
    // **The declarations and the profile manifest** (administration Part E.1c): read by
    // `service-mgr` and the profile server after `init` has mounted this filesystem. The only two
    // files, beside the test package, in which a test root may differ from a release one
    // (`ROOT_DIVERGENCE_ALLOWED`), and on the root because they can be edited here — the
    // initramfs they came from is a boot archive nothing writes.
    fs::write(staging.join("system").join("services.toml"), services_toml(mode))?;
    fs::create_dir_all(staging.join("system").join("profiles"))?;
    fs::write(staging.join("system").join("profiles").join("system.toml"), system_profile(mode)?)?;
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
    // PBKDF2 of the fixture password. The fixture credential is a build input for the
    // emulator demo user, not a secret. See docs/architecture/session-and-auth.md.
    //
    // **Written by `libusers::write_record`** (administration Part D.1), the function
    // `auth-service` rewrites the file with and `account`'s offline mode edits it with — so the
    // seeded line is one the service reads by construction, where it was formatted by hand here
    // until then.
    {
        let mut users =
            Vec::from(&b"# Nitrox user database (auth-service).\n# name:salt_hex:iterations:verifier_hex:home\n"[..]);
        let mut line = [0u8; libusers::MAX_FILE];
        let n = libusers::write_record(
            &mut line,
            DEMO_USER.as_bytes(),
            DEMO_HOME.as_bytes(),
            DEMO_PASSWORD.as_bytes(),
            &DEMO_SALT,
        )
        .map_err(|r| format!("seed the demo account: {}", String::from_utf8_lossy(r.why())))?;
        users.extend_from_slice(&line[..n]);
        fs::write(staging.join("system").join("users"), &users)?;
    }
    // `/system/views.toml` — the view broker's policy (administration Part A.6). The demo account
    // administers the machine, as it is the only account the build makes; `install` is a narrower
    // view that lets it run one program, so a gate can see a request refused by policy rather than
    // by the absence of any rule. An installed system gets its own from the installer (Part G).
    fs::write(staging.join("system").join("views.toml"), seeded_views_toml().as_bytes())?;
    // The demo user's home directory — the writable session root a login constructs
    // (auth Part E). The user shell writes into it, and since M11 Part C it arrives holding
    // the session's theme.
    fs::create_dir_all(staging.join(DEMO_HOME.trim_start_matches('/')))?;
    // The theme, shown in full and live only where it differs — see `staged_theme`.
    {
        let (_, text) = staged_theme()?;
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
        println!("xtask: seeded {WALLPAPER_PATH} ({WALLPAPER_W}x{WALLPAPER_H}, {n} bytes)");
    }
    // **The folders the browser's sidebar offers** (M14 Part D). Staged here for the demo home.
    // Whoever makes a home makes them: the view broker for every account an administrator adds
    // (administration Part D.3), and the installer for an installed machine's first (Part G.2).
    //
    // **Spelled twice, and checked by a boot rather than by the compiler.** `libfs` names the
    // same three in `HOME_FOLDERS` and this crate does not link it — that is the same reason the
    // gate spells a menu row as a number. What keeps them in step is `check-login`: it presses a
    // sidebar row and demands a *listing*, and a folder staged under another name would answer
    // "no such directory" instead.
    for name in HOME_FOLDERS {
        let path = staging.join(DEMO_HOME.trim_start_matches('/')).join(name);
        fs::create_dir_all(&path).map_err(|e| format!("stage {}: {e}", path.display()))?;
    }
    println!("xtask: seeded {DEMO_HOME}/{{{}}}", HOME_FOLDERS.join(","));
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
    println!("xtask: store package {cu_store}/bin/ ({} programs)", programs.len());

    // **The desktop entries** — one per graphical application, in the same package as the
    // binaries they name, projected at `/applications` the way `bin/` is projected at `/bin`
    // (M14 Part H). This is what stops the Applications menu listing every program on the
    // system: `/bin` holds services, servers and CLI tools too, and *"is this graphical?"* is
    // not a property that can be read off a binary — it is a claim somebody has to make, and
    // this is where it is made.
    //
    // **Beside the binaries rather than in a file of their own**, so that a package carries its
    // own applications: adding one here is adding a file to a package, not editing a list that
    // has to be kept in step with three others.
    // **An entry naming a program the package does not ship is a build error**, not a menu row
    // that launches nothing (PR #279 review, finding 5). `profile_programs` makes the same
    // guarantee for its own three consumers; this is the fourth list, so it needs the same guard.
    for (exec, name) in GRAPHICAL_APPLICATIONS {
        if !programs.contains(&exec) {
            return Err(format!(
                "the desktop entry for {name:?} names {exec:?}, which `profile_programs()` does \
                 not ship — the menu would list it and launching it would open nothing"
            )
            .into());
        }
    }
    let apps_dir = staging.join(cu_store.trim_start_matches('/')).join("applications");
    fs::create_dir_all(&apps_dir)?;
    for (exec, name) in GRAPHICAL_APPLICATIONS {
        fs::write(
            apps_dir.join(format!("{exec}.toml")),
            format!("# Desktop entry — see docs/spec/desktop-entry.md\nname = \"{name}\"\nexec = \"{exec}\"\n"),
        )
        .map_err(|e| format!("stage the {exec} desktop entry: {e}"))?;
    }
    println!("xtask: store package {cu_store}/applications/ ({} entries)", GRAPHICAL_APPLICATIONS.len());

    // The `system` package: the services. Everything init and service-mgr spawn after the
    // root is mounted now resolves through `/bin` like any other program.
    let sys_store = store_path_for_all(SYSTEM_SERVICES, "system", "0.1.0")?;
    let sys_bin = staging.join(sys_store.trim_start_matches('/')).join("bin");
    fs::create_dir_all(&sys_bin)?;
    for prog in SYSTEM_SERVICES {
        fs::copy(userspace_bin_path(prog), sys_bin.join(prog))
            .map_err(|e| format!("stage {prog} into the store: {e}"))?;
    }
    println!("xtask: store package {sys_store}/bin/ ({} services)", SYSTEM_SERVICES.len());

    // The `test` package: the guest-side gates and the programs they drive. Absent from a
    // release image — `cmd_build` does not even build them outside selftest modes.
    if mode.stages_test_data() {
        let programs = test_package();
        let test_store = store_path_for_all(&programs, "test", "0.1.0")?;
        let test_bin = staging.join(test_store.trim_start_matches('/')).join("bin");
        fs::create_dir_all(&test_bin)?;
        for prog in &programs {
            fs::copy(userspace_bin_path(prog), test_bin.join(prog))
                .map_err(|e| format!("stage {prog} into the store: {e}"))?;
        }
        println!("xtask: store package {test_store}/bin/ ({} test programs)", programs.len());
    }

    // `/system/fonts` — the faces the desktop draws with, and their licence beside them.
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
    // the previews and the display gate use. The proportional face's bold rides with it
    // (desktop refresh, Part G): it is not a theme key but the companion `load_ui` looks for
    // beside the built-in face, so it is named by `libdraw` rather than by the theme.
    //
    // The licence ships with them because the fonts are redistributed: DejaVu's terms are
    // permissive but require the notice to travel with the files. One notice covers all three —
    // its `Files: *` stanza is the DejaVu family, which is also why the second and third faces
    // cost no new licence question.
    {
        let fonts = staging.join("system").join("fonts");
        fs::create_dir_all(&fonts)?;
        let theme = libdraw::theme::Theme::light();
        // **Distinct paths, however the theme names them.** A theme whose two faces are one file
        // stages it once. This used to be `dedup`, which only removes *adjacent* equals and so
        // stopped covering that case the moment the bold face was inserted between the two
        // (PR #316 review, optional 4).
        let mut faces: Vec<PathBuf> = Vec::with_capacity(4);
        for face in [
            font_asset(theme.font_ui.as_str())?,
            font_asset(libdraw::text::UI_BOLD_FONT_PATH)?,
            font_asset(theme.font_mono.as_str())?,
        ] {
            if !faces.contains(&face) {
                faces.push(face);
            }
        }
        faces.push(repo_root().join("assets/fonts").join("LICENSE-DejaVu.txt"));
        let mut total = 0u64;
        for from in &faces {
            let name = from.file_name().ok_or("a font asset with no file name")?;
            total += fs::copy(from, fonts.join(name)).map_err(|e| format!("stage {}: {e}", from.display()))?;
        }
        println!("xtask: seeded /system/fonts ({} files, {total} bytes)", faces.len());
    }
    Ok(())
}

/// Parse `sgdisk -i <n> <disk>` for partition `n`'s first LBA and sector count.
pub(crate) fn partition_extent(disk: &Path, n: u32) -> R<(u64, u64)> {
    let out = Command::new("sgdisk")
        .arg("-i")
        .arg(n.to_string())
        .arg(disk)
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
pub(crate) fn splice_into(dst: &Path, offset: u64, src: &Path) -> R<()> {
    use std::io::{Seek, SeekFrom, Write};
    let data = fs::read(src)?;
    let mut f = fs::OpenOptions::new().write(true).open(dst)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&data)?;
    Ok(())
}
