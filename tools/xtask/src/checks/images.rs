//! `check-images`: a test image and a release image differ only where allowed.

use crate::*;

/// `check-images`' live half: the live image differs from the image of its mode **only in data** —
/// the release stick from the release image, and the test stick (`image --live --selftest`) from a
/// `--selftest` image.
///
/// Three claims, each checked against what the builds produced rather than against the functions
/// that produced them:
///
/// 1. the live initramfs carries the ordinary initramfs's files, byte-identical except
///    `etc/init.toml`, which must differ — it names `nitrox-live`;
/// 2. the ext4 inside the built stick's `root.img` holds the same entries as the ordinary
///    image's root partition — names, kinds, sizes and contents — read back with `debugfs`;
/// 3. for the release stick, the FAT inside its `install-esp.img` — the ESP the installer writes
///    to a disk — holds the same entries as the release image's own ESP. The first two are about
///    the stick booting; this one is about the machine booting once the stick is gone. Nobody
///    installs from a test stick, so it is not asked of that one.
fn check_live_image(dir: &Path, release_cpio: &Path, mode: BuildMode) -> R<()> {
    let tag = if mode.stages_test_data() { "live-test" } else { "live" };
    let what = if mode.stages_test_data() { "the test live" } else { "the live" };
    let base = if mode.stages_test_data() { "a --selftest" } else { "the release" };
    let live_cpio = dir.join(format!("{tag}.cpio"));
    build_initramfs_for(&live_cpio, mode, RootDevice::Live)?;
    let r = cpio_entries(&fs::read(release_cpio)?);
    let l = cpio_entries(&fs::read(&live_cpio)?);
    let mut names: Vec<&String> = r.keys().chain(l.keys()).collect();
    names.sort();
    names.dedup();
    let differ: Vec<&String> = names.into_iter().filter(|k| r.get(*k) != l.get(*k)).collect();
    if differ != ["etc/init.toml"] {
        return Err(format!(
            "{what} initramfs must differ from {base} one in `etc/init.toml` alone — the root's \
             partition label — and it differs in {differ:?}. A live image is the image of its \
             mode with its root in RAM; a program or a declaration that differs is a live-only \
             build, which Part C's discipline rules out."
        )
        .into());
    }
    println!("check-images: {what} initramfs is {base} one but for etc/init.toml ({} files) ✓", r.len());

    require_tool("debugfs")?;
    cmd_image(mode)?;
    cmd_image_live_for(mode)?;
    let stick = if mode.stages_test_data() { live_test_image_path() } else { live_image_path() };
    let release_fs = dir.join(format!("{tag}-base-root.ext4"));
    carve_partition(&image_path(), 2, &release_fs)?;
    let esp = dir.join(format!("{tag}-esp.img"));
    carve_partition(&stick, 1, &esp)?;
    let root_img = dir.join(format!("{tag}-root.img"));
    let _ = fs::remove_file(&root_img);
    run(Command::new("mcopy").arg("-i").arg(&esp).arg("::/boot/root.img").arg(&root_img))?;
    let live_fs = dir.join(format!("{tag}-root.ext4"));
    carve_partition(&root_img, 1, &live_fs)?;

    let release_tree = ext4_tree(&release_fs, &dir.join(format!("{tag}-base-root")))?;
    let live_tree = ext4_tree(&live_fs, &dir.join(format!("{tag}-root")))?;
    let problems = tree_problems(&release_tree, &live_tree, &format!("{what} root"));
    if !problems.is_empty() {
        return Err(format!(
            "the filesystem inside {what} image's root.img is not {base} image's root: \
             {problems:?}. The live root is built from `stage_rootfs` for the image's mode and \
             nothing else."
        )
        .into());
    }
    println!("check-images: {what} root.img holds {base} root's {} entries, byte for byte ✓", release_tree.len());

    // **The pristine root is the root too.** `install-root.img` is what
    // the installer copies onto a disk, so it is held to the same tree as `root.img` — and its one
    // partition is found, as `nxinstall` finds it, by `libgpt`'s reader and the shared name.
    let install_root = dir.join(format!("{tag}-install-root.img"));
    let _ = fs::remove_file(&install_root);
    run(Command::new("mcopy").arg("-i").arg(&esp).arg("::/boot/install-root.img").arg(&install_root))?;
    let front = fs::read(&install_root)?;
    let table = front
        .get(..libgpt::table::FRONT_BYTES)
        .and_then(|f| libgpt::table::read(f).ok())
        .ok_or_else(|| format!("{what} image's install-root.img holds no partition table libgpt reads"))?;
    let part = table.by_name(libgpt::INSTALL_SOURCE_LABEL.as_bytes()).ok_or_else(|| {
        format!(
            "{what} image's install-root.img has no partition named {}: the installer would find \
             no root to copy, and the storage service would not know to leave it unmounted",
            libgpt::INSTALL_SOURCE_LABEL
        )
    })?;
    let source_fs = dir.join(format!("{tag}-install-root.ext4"));
    let start = (part.first_lba * 512) as usize;
    let len = (part.blocks() * 512) as usize;
    fs::write(&source_fs, front.get(start..start + len).ok_or("install-root.img is shorter than its partition")?)?;
    let source_tree = ext4_tree(&source_fs, &dir.join(format!("{tag}-install-root")))?;
    let problems = tree_problems(&release_tree, &source_tree, &format!("{what} install root"));
    if !problems.is_empty() {
        return Err(format!(
            "the filesystem inside {what} image's install-root.img is not {base} image's root: \
             {problems:?}. It is what the installer copies, built from the same `stage_rootfs` \
             tree as root.img."
        )
        .into());
    }
    println!(
        "check-images: {what} install-root.img's {} holds {base} root's {} entries ✓",
        libgpt::INSTALL_SOURCE_LABEL,
        release_tree.len()
    );
    if mode.stages_test_data() {
        return Ok(());
    }

    // **The installable ESP is the release ESP.** The claims above compare the parts of the live
    // image that boot the *stick*; this module boots the machine **afterwards**, and nothing
    // above would notice it built from the live `limine.conf`: the installed machine boots
    // while the stick is plugged in and fails the first time it is not, looking for
    // `gpt-partlabel:nitrox-live`. It is compared file for file against what a release image
    // ships, which also makes `build_esp`'s own arguments the thing under test.
    let install_esp = dir.join("install-esp.img");
    let _ = fs::remove_file(&install_esp);
    run(Command::new("mcopy").arg("-i").arg(&esp).arg("::/boot/install-esp.img").arg(&install_esp))?;
    let release_esp = dir.join("release-esp.img");
    carve_partition(&image_path(), 1, &release_esp)?;
    let release_esp_tree = fat_tree(&release_esp, &dir.join("release-esp"))?;
    let install_esp_tree = fat_tree(&install_esp, &dir.join("install-esp"))?;
    let problems = tree_problems(&release_esp_tree, &install_esp_tree, "the installable ESP");
    if !problems.is_empty() {
        return Err(format!(
            "the live image's `install-esp.img` module is not the release ESP: {problems:?}. It is \
             what the installer copies onto a disk, so it must carry the release menu — no module \
             lines — and the release initramfs, which names `gpt-partlabel:{ROOT_PARTLABEL}`. \
             Built from the live image's own initramfs or `limine.conf`, it installs a system that \
             mounts the stick it came from."
        )
        .into());
    }
    println!("check-images: the installable ESP module is the release ESP's {} entries ✓", release_esp_tree.len());
    Ok(())
}

pub(crate) fn carve_partition(disk: &Path, n: u32, out: &Path) -> R<()> {
    use std::io::{Read, Seek, SeekFrom};
    let (lba, sectors) = partition_extent(disk, n)?;
    let mut f = fs::File::open(disk)?;
    f.seek(SeekFrom::Start(lba * 512))?;
    let mut buf = vec![0u8; (sectors * 512) as usize];
    f.read_exact(&mut buf)?;
    fs::write(out, buf)?;
    Ok(())
}

#[derive(PartialEq, Debug, Clone)]
pub(crate) enum TreeEntry {
    Dir,
    File { size: u64, hash: u64 },
}

fn ext4_tree(fs_img: &Path, out: &Path) -> R<BTreeMap<String, TreeEntry>> {
    if out.exists() {
        fs::remove_dir_all(out)?;
    }
    fs::create_dir_all(out)?;
    // `rdump` complains that it cannot chown what it writes when not run as root, and extracts
    // everything anyway; the tree below is what is checked, and an empty one fails.
    let status = Command::new("debugfs")
        .arg("-R")
        .arg(format!("rdump / {}", out.display()))
        .arg(fs_img)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("run debugfs: {e}"))?;
    if !status.success() {
        return Err(format!("debugfs could not read {}", fs_img.display()).into());
    }
    let tree = host_tree(out)?;
    if tree.is_empty() {
        return Err(format!("debugfs extracted nothing from {}", fs_img.display()).into());
    }
    Ok(tree)
}

/// Extract the FAT filesystem image `fat_img` into `out` with `mcopy`, and describe every entry
/// under it by path, as [`ext4_tree`] does. `mcopy` globs on the FAT side, so `::/*` is passed
/// through literally rather than by a shell.
fn fat_tree(fat_img: &Path, out: &Path) -> R<BTreeMap<String, TreeEntry>> {
    if out.exists() {
        fs::remove_dir_all(out)?;
    }
    fs::create_dir_all(out)?;
    run(Command::new("mcopy").arg("-i").arg(fat_img).arg("-s").arg("::/*").arg(out))?;
    let tree = host_tree(out)?;
    if tree.is_empty() {
        return Err(format!("mcopy extracted nothing from {}", fat_img.display()).into());
    }
    Ok(tree)
}

fn host_tree(root: &Path) -> R<BTreeMap<String, TreeEntry>> {
    fn walk(root: &Path, dir: &Path, tree: &mut BTreeMap<String, TreeEntry>) -> R<()> {
        use std::hash::{Hash, Hasher};
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(root)?.display().to_string();
            if entry.file_type()?.is_dir() {
                tree.insert(rel, TreeEntry::Dir);
                walk(root, &path, tree)?;
            } else {
                let bytes = fs::read(&path)?;
                let mut h = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut h);
                tree.insert(rel, TreeEntry::File { size: bytes.len() as u64, hash: h.finish() });
            }
        }
        Ok(())
    }
    let mut tree = BTreeMap::new();
    walk(root, root, &mut tree)?;
    Ok(tree)
}

fn tree_problems(
    expected: &BTreeMap<String, TreeEntry>,
    actual: &BTreeMap<String, TreeEntry>,
    actual_name: &str,
) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    for (path, entry) in expected {
        match actual.get(path) {
            None => problems.push(format!("{path}: missing from {actual_name}")),
            Some(e) if e != entry => problems.push(format!("{path}: differs")),
            Some(_) => {}
        }
    }
    for path in actual.keys().filter(|p| !expected.contains_key(*p)) {
        problems.push(format!("{path}: only in {actual_name}"));
    }
    problems
}

/// The initramfs files a **test** image is allowed to differ from a **release** image in.
///
/// `init.toml` alone, whose test copy adds two binds; the rest are on the root's list,
/// [`ROOT_DIVERGENCE_ALLOWED`]. **Everything else must be byte-identical**: the software under
/// test is the software that ships.
const IMAGE_DIVERGENCE_ALLOWED: &[&str] = &["etc/init.toml"];

/// The root-filesystem files a **test** root is allowed to differ from a **release** root in,
/// beside the test package's store directory, which only a test root has. Store paths are
/// compared with their hash taken out ([`unhashed`]):
/// - **the declarations**, with the test services in them;
/// - **the profile manifest**, which lists the test package — and the coreutils package by a
///   different hash, for the next reason;
/// - **`nxterm`**, the one program built differently: a test-harness image's reports each
///   completed grid row on the debug console for `check-terminal`, and a release one must not
///   narrate itself to the kernel log. The initramfs comparison never saw it, since it is not in
///   the initramfs.
///
/// **Every other file on the two roots must be byte-identical**, including the system and
/// coreutils packages: every program a release image runs.
const ROOT_DIVERGENCE_ALLOWED: &[&str] =
    &["system/services.toml", "system/profiles/system.toml", "store/coreutils-0.1.0/bin/nxterm"];

/// A root path with a store package's hash taken out: `store/<hash>-<name>-<version>/…` becomes
/// `store/<name>-<version>/…`. A package with one differing file then compares file by file.
pub(crate) fn unhashed(path: &str) -> String {
    match path.strip_prefix("store/").and_then(|rest| rest.split_once('-')) {
        Some((hash, tail)) if !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            format!("store/{tail}")
        }
        _ => path.to_string(),
    }
}

/// Every way a test root departs from a release root that [`ROOT_DIVERGENCE_ALLOWED`] and the test
/// package at `test_store` (relative, as the trees' paths are) do not account for, named by path
/// with store hashes taken out.
pub(crate) fn root_divergence(
    release: &BTreeMap<String, TreeEntry>,
    test: &BTreeMap<String, TreeEntry>,
    test_store: &str,
) -> Vec<String> {
    let unhash = |t: &BTreeMap<String, TreeEntry>| -> BTreeMap<String, TreeEntry> {
        t.iter().map(|(k, v)| (unhashed(k), v.clone())).collect()
    };
    let (release, test, test_store) = (unhash(release), unhash(test), unhashed(test_store));
    let in_test_package =
        |p: &str| p == test_store || p.strip_prefix(test_store.as_str()).is_some_and(|r| r.starts_with('/'));
    let mut problems = Vec::new();
    for (path, entry) in &release {
        if in_test_package(path) {
            problems.push(format!("{path}: the test package, on a release root"));
            continue;
        }
        match test.get(path) {
            None => problems.push(format!("{path}: only on the release root")),
            Some(e) if e != entry && !ROOT_DIVERGENCE_ALLOWED.contains(&path.as_str()) => {
                problems.push(format!("{path}: differs"))
            }
            Some(_) => {}
        }
    }
    for path in test.keys().filter(|p| !release.contains_key(*p) && !in_test_package(p)) {
        problems.push(format!("{path}: only on the test root"));
    }
    problems
}

/// `cargo xtask check-images` — a test image may differ from a release image only in **data**.
///
/// Builds both initramfs archives and compares them file by file, and both root filesystems too
/// ([`root_divergence`]). A new divergence fails, which is what makes the result a wall rather
/// than a measurement.
///
/// **What it actually catches**: wiring `mode.features()` into a build that does not take one
/// makes that program's ELF differ here. A bare `#[cfg(feature = "test-harness")]` does **not**,
/// because `eshell`, `fs-server-ext4` and `profile-server` declare no features, so the cfg is
/// inert in both modes: the invariant is "no build-mode-varying input reaches these programs".
///
/// It checks the **set** of differing files, not a byte count, which changes whenever a
/// declaration is added. **It is one-directional**: a file that *stops* differing leaves the
/// allow-list stale with nothing to say so — the harmless direction, since tightening it would
/// fail a build for getting *better*; prune it by hand.
pub(crate) fn cmd_check_images() -> R<()> {
    let dir = build_cache().join("check-images");
    fs::create_dir_all(&dir)?;
    let release = dir.join("release.cpio");
    let test = dir.join("test.cpio");

    // `cmd_build` first: `build_initramfs` packs ELFs that must already exist.
    cmd_build(BuildMode::Normal)?;
    build_initramfs(&release, BuildMode::Normal)?;

    // **The third mode, the live image**: its initramfs may differ from the release one in the
    // root's label and nothing else, and its `root.img` filesystem must be the release root's.
    check_live_image(&dir, &release, BuildMode::Normal)?;

    // **And the test live image**, held to a `--selftest` image: `check-storage` boots it, and
    // what it proves holds for the release stick only while the two differ in data.
    let selftest = dir.join("selftest.cpio");
    cmd_build(BuildMode::Selftest)?;
    build_initramfs(&selftest, BuildMode::Selftest)?;
    check_live_image(&dir, &selftest, BuildMode::Selftest)?;

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
    let unexpected: Vec<&&String> = differ.iter().filter(|k| !IMAGE_DIVERGENCE_ALLOWED.contains(&k.as_str())).collect();
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

    // **And the roots**, where the declarations and the profile manifest live, and where every
    // program but four is — so this also holds the system and coreutils packages byte for byte.
    require_tool("debugfs")?;
    let mut trees = Vec::new();
    for (mode, name) in [(BuildMode::Normal, "release"), (BuildMode::TestHarness, "test")] {
        cmd_image(mode)?;
        let fs_img = dir.join(format!("{name}-root.ext4"));
        carve_partition(&image_path(), 2, &fs_img)?;
        trees.push(ext4_tree(&fs_img, &dir.join(format!("{name}-root")))?);
    }
    let test_store = store_path_for_all(&test_package(), "test", "0.1.0")?;
    let test_store = test_store.trim_start_matches('/');
    let problems = root_divergence(&trees[0], &trees[1], test_store);
    if !problems.is_empty() {
        return Err(format!(
            "a test root and a release root now differ in {problems:?}. A test root may differ in \
             {ROOT_DIVERGENCE_ALLOWED:?} and the test package ({test_store}), and nothing else: \
             a program that differs is the divergence `docs/planning/test-path-retrofit.md` \
             removed, and a new file is a test image carrying something a release one does not. \
             If it is deliberate data, add it to `ROOT_DIVERGENCE_ALLOWED` with the reason."
        )
        .into());
    }
    let tested = trees[1].keys().filter(|p| !p.starts_with(test_store)).count();
    println!(
        "check-images: {} root entries, identical between a test and a release root but for \
         {ROOT_DIVERGENCE_ALLOWED:?} and the test package ✓",
        tested
    );
    Ok(())
}

/// Parse a CPIO `newc` archive into `name -> contents`. Mirrors `cpio_entry`'s writer.
///
/// **`TRAILER!!!` is skipped.** It is the zero-byte end-of-archive sentinel, not a file, and
/// counting it inflated every number this gate prints by one — "8 entries, 5 byte-identical"
/// for what is 7 files and 4.
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
