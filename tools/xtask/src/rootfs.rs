//! The root filesystem's tree staged for `mke2fs`, and a partition spliced into a GPT disk.

use crate::*;

/// Stage the root filesystem's tree for `mode` into `staging`: everything `mke2fs -d` puts on
/// `nitrox-root`, shared by every image that carries a root.
pub(crate) fn stage_rootfs(staging: &Path, mode: BuildMode) -> R<()> {
    fs::create_dir_all(staging.join("system"))?;
    // `/scratch` backs the **second writable mount**: a second bind with base `/scratch` is
    // cross-mount to `/system` yet writable, which a rename's cross-filesystem fallback needs
    // (`/initramfs` is read-only).
    fs::create_dir_all(staging.join("scratch"))?;
    fs::write(staging.join("system").join("current-generation"), b"nitrox-rootfs generation 1\n")?;
    // **The declarations and the profile manifest** are read after `init` mounts this filesystem;
    // with the test package they are the only files a test root may differ in
    // (`ROOT_DIVERGENCE_ALLOWED`).
    fs::write(staging.join("system").join("services.toml"), services_toml(mode))?;
    fs::create_dir_all(staging.join("system").join("profiles"))?;
    fs::write(staging.join("system").join("profiles").join("system.toml"), system_profile(mode)?)?;
    // `system/large.bin`: **position-sensitive** content, byte `i` being `((i >> 12) ^ i) as u8`,
    // so `boot-probe` catches a mis-faulted page; it MUST match `fill_byte` in
    // `bootprobe/files.rs`. 8 pages, as each demand-fault costs ~325 ms under emulated AHCI.
    const LARGE_FILE_BYTES: usize = 32 * 1024; // 8 pages
    let mut large = vec![0u8; LARGE_FILE_BYTES];
    for (i, b) in large.iter_mut().enumerate() {
        *b = (((i >> 12) ^ i) & 0xFF) as u8;
    }
    fs::write(staging.join("system").join("large.bin"), &large)?;
    // `system/rwtest`: one block, byte `i` being `i & 0xFF`, which `boot-probe` overwrites through
    // a mapping, syncs and reads back.
    let mut rwtest = vec![0u8; 4096];
    for (i, b) in rwtest.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }
    fs::write(staging.join("system").join("rwtest"), &rwtest)?;
    // `/system/users`, seeded as a one-way PBKDF2 so no verifier is committed, **written by
    // `libusers::write_record`** so the line is one the service reads by construction.
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
    // `/system/views.toml`: the demo account administers the machine, and `install` is a narrower
    // view, so a gate can see a request refused by policy.
    fs::write(staging.join("system").join("views.toml"), seeded_views_toml().as_bytes())?;
    fs::create_dir_all(staging.join(DEMO_HOME.trim_start_matches('/')))?;
    {
        let (_, text) = staged_theme()?;
        let path = staging.join(DEMO_HOME.trim_start_matches('/')).join("theme.toml");
        fs::write(&path, text.as_bytes()).map_err(|e| format!("stage {}: {e}", path.display()))?;
    }
    {
        let path = staging.join(DEMO_HOME.trim_start_matches('/')).join("wallpaper.png");
        let bytes = wallpaper_png()?;
        let n = bytes.len();
        fs::write(&path, &bytes).map_err(|e| format!("stage {}: {e}", path.display()))?;
        println!("xtask: seeded {WALLPAPER_PATH} ({WALLPAPER_W}x{WALLPAPER_H}, {n} bytes)");
    }
    // **The folders the browser's sidebar offers**, spelled twice: `libfs` names the same three in
    // `HOME_FOLDERS` and `check-login` demands a *listing* from a sidebar row.
    for name in HOME_FOLDERS {
        let path = staging.join(DEMO_HOME.trim_start_matches('/')).join(name);
        fs::create_dir_all(&path).map_err(|e| format!("stage {}: {e}", path.display()))?;
    }
    println!("xtask: seeded {DEMO_HOME}/{{{}}}", HOME_FOLDERS.join(","));
    println!("xtask: seeded /system/users + {DEMO_HOME} (with a theme)");
    // The content-addressed store is pre-built read-only into the root:
    // `/store/<hash>-<name>-<version>/bin/<prog>`, projected into /bin by the profile server.
    let programs = profile_programs();
    let cu_store = store_path_for_all(&programs, "coreutils", "0.1.0")?;
    let cu_bin = staging.join(cu_store.trim_start_matches('/')).join("bin");
    fs::create_dir_all(&cu_bin)?;
    for prog in &programs {
        fs::copy(userspace_bin_path(prog), cu_bin.join(prog))
            .map_err(|e| format!("stage {prog} into the store: {e}"))?;
    }
    println!("xtask: store package {cu_store}/bin/ ({} programs)", programs.len());

    // **The desktop entries**, one per graphical application, projected at `/applications`: *"is
    // this graphical?"* cannot be read off a binary. **An entry naming a program the package does
    // not ship is a build error.**
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

    let sys_store = store_path_for_all(SYSTEM_SERVICES, "system", "0.1.0")?;
    let sys_bin = staging.join(sys_store.trim_start_matches('/')).join("bin");
    fs::create_dir_all(&sys_bin)?;
    for prog in SYSTEM_SERVICES {
        fs::copy(userspace_bin_path(prog), sys_bin.join(prog))
            .map_err(|e| format!("stage {prog} into the store: {e}"))?;
    }
    println!("xtask: store package {sys_store}/bin/ ({} services)", SYSTEM_SERVICES.len());

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

    // `/system/fonts`: **here and not in the initramfs**, as nothing that draws text runs before
    // the root is mounted. **Which files come from the theme**, so "the guest reads the font the
    // host rendered with" is a property of the build. One DejaVu notice covers all three faces and
    // must travel with them.
    {
        let fonts = staging.join("system").join("fonts");
        fs::create_dir_all(&fonts)?;
        let theme = libdraw::theme::Theme::light();
        // **Distinct paths, however the theme names them** (`dedup` only removes adjacent equals).
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

/// Overwrite `dst` in place with `src`'s bytes from `offset`: splice a partition into a GPT disk.
pub(crate) fn splice_into(dst: &Path, offset: u64, src: &Path) -> R<()> {
    use std::io::{Seek, SeekFrom, Write};
    let data = fs::read(src)?;
    let mut f = fs::OpenOptions::new().write(true).open(dst)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&data)?;
    Ok(())
}
