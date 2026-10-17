//! The disk image assembled: the initramfs and its programs, the store, the ESP, a scratch disk.

use crate::*;

pub(crate) fn find_bootx64(limine_root: &Path) -> R<PathBuf> {
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
    if let Some(found) = walk_for(limine_root, "BOOTX64.EFI")? {
        return Ok(found);
    }
    Err(format!("BOOTX64.EFI not found under {}; tarball layout may have changed", limine_root.display()).into())
}

pub(crate) fn walk_for(root: &Path, name: &str) -> R<Option<PathBuf>> {
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

/// The initramfs payload: a placeholder `etc/init.toml` with a single critical-path mount.
const INIT_TOML: &str = "\
# Nitrox init manifest (Phase 2 slice 4 placeholder).\n\
[[mount]]\n\
fs_server = \"fs-server-ext4\"\n\
device = \"gpt-partlabel:nitrox-root\"\n\
mount_point = \"/\"\n\
mode = \"rw\"\n\
required_for = \"boot\"\n";

/// The binds a **test** image's `init.toml` adds: `/subtreetest`, scoped to `/system`, proves
/// `move` refuses to recurse through a mount; `/scratch`, scoped to `/scratch`, is a second
/// writable mount (another subtree base is another mount to the kernel) for `move`'s cross-mount
/// half.
const TEST_BINDS_TOML: &str = "\
\n\
[[bind]]\n\
path = \"/subtreetest\"\n\
source = \"/\"\n\
subtree = \"/system\"\n\
\n\
[[bind]]\n\
path = \"/scratch\"\n\
source = \"/\"\n\
subtree = \"/scratch\"\n";

/// The service declarations every image carries, read by `service-mgr` from
/// `/system/services.toml`; [`services_toml`] adds a test image's. One file of `[service.<name>]`
/// tables, since nothing here can enumerate a directory.
pub(crate) const SERVICES_TOML: &str = "\
# Nitrox service declarations.\n\
#\n\
# **The servers first, in the order `init` started them** (administration Part E.1): each\n\
# `endpoint` is waited on for its `Meta::Ready` before the next starts, which keeps the orders\n\
# `init`'s comments called load-bearing. `never` restarts them, as `init` never did.\n\
#\n\
# **`essential` on every one but the clipboard** (PR #341 review): each of the others' absence\n\
# locks the administrator out or loses state. The terminal server, the input server and the\n\
# compositor are among them because their clients do not reconnect: on a machine with no serial\n\
# port, stopping one leaves nothing to type at until power-off.\n\
[service.auth-service]\n\
executable = \"/bin/auth-service\"\n\
description = \"The credential oracle, and the user database's writer\"\n\
endpoint = \"/svc/auth\"\n\
critical = true\n\
essential = true\n\
\n\
[service.auth-service.restart]\n\
policy = \"never\"\n\
\n\
[service.logging-service]\n\
executable = \"/bin/logging-service\"\n\
description = \"The service log, and the audit\"\n\
endpoint = \"/log\"\n\
critical = true\n\
essential = true\n\
\n\
[service.logging-service.restart]\n\
policy = \"never\"\n\
\n\
[service.tty-server]\n\
executable = \"/bin/tty-server\"\n\
description = \"Terminals, over the console\"\n\
endpoint = \"/dev/tty\"\n\
essential = true\n\
\n\
[service.tty-server.restart]\n\
policy = \"never\"\n\
\n\
[service.clipboard-server]\n\
executable = \"/bin/clipboard-server\"\n\
description = \"The clipboard\"\n\
endpoint = \"/dev/clipboard\"\n\
\n\
[service.clipboard-server.restart]\n\
policy = \"never\"\n\
\n\
[service.view-broker]\n\
executable = \"/bin/view-broker\"\n\
description = \"Views, and the policy and accounts it fronts\"\n\
endpoint = \"/svc/views\"\n\
essential = true\n\
# `SYSTEM_CLOCK` only to give it: the `clock` grant is that capability at a program's spawn, and\n\
# a parent passes on only what it holds (administration Part E.5).\n\
syscaps = [\"BIND_NAMESPACE\", \"SYSTEM_CLOCK\"]\n\
\n\
[service.view-broker.restart]\n\
policy = \"never\"\n\
\n\
[service.device-mgr]\n\
executable = \"/bin/device-mgr\"\n\
description = \"The device manager\"\n\
endpoint = \"/svc/devices\"\n\
essential = true\n\
\n\
[service.device-mgr.restart]\n\
policy = \"never\"\n\
\n\
[service.storage-service]\n\
executable = \"/bin/storage-service\"\n\
description = \"Disks, and the filesystems on them\"\n\
endpoint = \"/svc/storage\"\n\
essential = true\n\
syscaps = [\"BIND_NAMESPACE\"]\n\
# Its stop writes back and unmounts every filesystem it mounted, which takes as long as what is\n\
# dirty takes to write (PR #343 review): not `service-mgr`'s 3 s.\n\
stop_timeout = \"60s\"\n\
\n\
[service.storage-service.restart]\n\
policy = \"never\"\n\
\n\
[service.input-server]\n\
executable = \"/bin/input-server\"\n\
description = \"Keyboards and pointers, merged\"\n\
endpoint = \"/dev/input/new\"\n\
essential = true\n\
\n\
[service.input-server.restart]\n\
policy = \"never\"\n\
\n\
[service.compositor]\n\
executable = \"/bin/compositor\"\n\
description = \"The display\"\n\
endpoint = \"/dev/draw\"\n\
essential = true\n\
\n\
[service.compositor.restart]\n\
policy = \"never\"\n";

/// **`heartbeat`, in a test image only**: it runs for the whole boot under `always`, which
/// `check_service_attribution` leans on. Declared before [`BOOT_PROBE_TOML`].
const HEARTBEAT_TOML: &str = "\
\n\
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

/// The declarations file for `mode`, staged at `/system/services.toml` on the root. Its **content**
/// is what differs between a test image and a release image; the programs do not.
pub(crate) fn services_toml(mode: BuildMode) -> String {
    let mut services = String::from(SERVICES_TOML);
    if mode.stages_test_data() {
        services.push_str(HEARTBEAT_TOML);
        services.push_str(BOOT_PROBE_TOML);
    }
    // `compose-bench` replaces `boot-probe`: `boot-probe` fires the verdict, so anything after it
    // never runs.
    if matches!(mode, BuildMode::Bench) {
        services = services.replace(BOOT_PROBE_TOML, BENCH_TOML);
    }
    services
}

/// The system profile manifest for `mode`, staged at `/system/profiles/system.toml`. Generated
/// because it names content-derived store paths.
pub(crate) fn system_profile(mode: BuildMode) -> R<String> {
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
    if mode.stages_test_data() {
        let test_store = store_path_for_all(&test_package(), "test", "0.1.0")?;
        system_profile.push_str(&format!(
            "\n[[package]]\n\
             name = \"test\"\n\
             version = \"0.1.0\"\n\
             path = \"{test_store}\"\n"
        ));
    }
    Ok(system_profile)
}

/// The `boot-probe` declaration, **appended in test images and absent from a release image**: the
/// two differ by data. `policy = "never"` because the probe runs once.
const BOOT_PROBE_TOML: &str = "\
\n\
# **A server that exits when asked** (administration Part E.1): `boot-probe` resolves it, has it\n\
# exit, and resolves the same path until the restarted instance answers. Declared **first** of\n\
# the test services, since it is a server: the login chain starts after the last server, and must\n\
# still start before the clients below.\n\
[service.restart-probe]\n\
executable = \"/bin/restart-probe\"\n\
description = \"A server that exits when asked\"\n\
endpoint = \"/svc/restart-probe\"\n\
\n\
[service.restart-probe.restart]\n\
policy = \"always\"\n\
max_attempts = 5\n\
backoff = \"linear\"\n\
backoff_initial = \"100ms\"\n\
\n\
# The graphical self-tests and demo clients. `init` spawned these under `selftest` until\n\
# retrofit Part C2; they are data now, so `init` is byte-identical in both images.\n\
#\n\
# **Order is file order.** `nxterm` precedes `ui-testclient` so that its window exists before\n\
# `ui-testclient` raises its reference windows over it: the display gate compares the\n\
# top-left, where all four sit, so the largest has to be at the bottom. The raise is what\n\
# stacks them. Creation order did until administration C.1, and it was a race.\n\
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
# To bind a channel of its own into a namespace it made, and answer its own lookups\n\
# with `SUBNAMESPACE` (administration Part C.4).\n\
syscaps = [\"BIND_NAMESPACE\"]\n\
\n\
[service.boot-probe.restart]\n\
policy = \"never\"\n";

/// The `compose-bench` declaration, replacing [`BOOT_PROBE_TOML`] in a bench image. It is `after`
/// every display client, since a frame timed while one composes is noise, and ends the boot itself.
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

pub(crate) fn initramfs_path() -> PathBuf {
    build_cache().join("initramfs.cpio")
}

/// The live image: a separate file, so a live build never clobbers every other gate's image.
pub(crate) fn live_image_path() -> PathBuf {
    build_cache().join("nitrox-live.img")
}

pub(crate) fn live_initramfs_path() -> PathBuf {
    build_cache().join("initramfs-live.cpio")
}

/// The **test** live image, built with `--selftest` for `check-storage`: a file of its own, so it
/// never stands in for the release stick the other live gates boot.
pub(crate) fn live_test_image_path() -> PathBuf {
    build_cache().join("nitrox-live-test.img")
}

pub(crate) fn live_test_initramfs_path() -> PathBuf {
    build_cache().join("initramfs-live-test.cpio")
}

pub(crate) const ROOT_PARTLABEL: &str = "nitrox-root";

/// The root partition's label inside `root.img`: **distinct on purpose**, so an installed machine
/// and a stick never answer to one name.
pub(crate) const LIVE_ROOT_PARTLABEL: &str = "nitrox-live";

/// Room the live root gets beyond what is staged on it, for what a session writes (gone at
/// power-off).
pub(crate) const LIVE_ROOT_SLACK_MIB: u64 = 16;

/// Room the pristine root, `install-root.img`, gets: **for `mke2fs`'s own tables, not for
/// writing**; every MiB saved is one the firmware does not read.
pub(crate) const INSTALL_ROOT_MARGIN_MIB: u64 = 4;

/// Ceiling on `root.img`. **Its reason is the firmware**: UEFI reads the whole file off a USB stick
/// before the kernel runs, so a large image should be a decision. The release root stages ~8 MiB.
pub(crate) const LIVE_ROOT_MAX_MIB: u64 = 64;

/// The programs in the boot image, each with **the reason it cannot come from the filesystem**. A
/// pair because the reason is the whole rule: a bare list drifted twice. The bar is "required to
/// get from the bootloader to a mounted root, plus what cannot live on the filesystem it depends
/// on".
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

/// The largest the packed initramfs may be, in bytes: a **tripwire on the rule above**, since
/// nothing releases the initramfs and the list drifts silently. The four programs come to ~231 KB,
/// room to grow by half, and a fifth trips it.
/// A failure is a question: add the program to [`INITRAMFS_PROGRAMS`] with its bootstrap reason and
/// raise this, or put it in a store package.
const INITRAMFS_MAX_BYTES: usize = 384 * 1024;

/// Append one CPIO `newc` entry (NUL-padded to 4 bytes throughout) to `out`; matches
/// `kernel/src/initramfs.rs`.
fn cpio_entry(out: &mut Vec<u8>, ino: u32, name: &str, data: &[u8]) {
    let namesize = name.len() + 1; // includes the trailing NUL
    out.extend_from_slice(b"070701");
    for f in [ino, 0o100644, 0, 0, 1, 0, data.len() as u32, 0, 0, 0, 0, namesize as u32, 0] {
        out.extend_from_slice(format!("{f:08x}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

pub(crate) fn userspace_bin_path(name: &str) -> PathBuf {
    userspace_release_dir().join(name)
}

/// FNV-1a content hash of `bytes` as the store path's opaque 12-hex-char `<hash>`: unique and
/// deterministic until the build system's input hash (`content-addressed-store.md`).
fn store_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

/// A store path for a package containing several programs. The hash covers **every** ELF, in the
/// given order, so changing any one moves the whole package.
pub(crate) fn store_path_for_all(bins: &[&str], name: &str, version: &str) -> R<String> {
    let mut bytes = Vec::new();
    // **The desktop entries are part of the package's contents**, so part of its hash: otherwise a
    // changed display name yields different contents at an identical store path.
    for (exec, display) in GRAPHICAL_APPLICATIONS {
        if bins.contains(&exec) {
            bytes.extend_from_slice(exec.as_bytes());
            bytes.extend_from_slice(display.as_bytes());
        }
    }
    for b in bins {
        bytes.extend_from_slice(
            &fs::read(userspace_bin_path(b)).map_err(|e| format!("read {b} ELF for store hash: {e}"))?,
        );
    }
    Ok(format!("/store/{}-{}-{}", store_hash(&bytes), name, version))
}

/// The policy the build seeds at `/system/views.toml`: `view_broker::policy::seed(DEMO_USER)`, as
/// the installer seeds its first account, so the two cannot drift.
pub(crate) fn seeded_views_toml() -> String {
    view_broker::policy::seed(DEMO_USER)
}

/// The programs a session gets through its profile: the coreutils, `nxsh` (also the login leaf, so
/// a user can run a nested shell) and the programs a person runs. One list, three consumers, so
/// nothing is built-but-unreachable or packaged-but-unbuilt.
pub(crate) fn profile_programs() -> Vec<&'static str> {
    let mut v = COREUTILS.to_vec();
    v.push("nxsh");
    v.push("nxterm");
    v.push("nxfiles");
    // `desktop-shell` resolves `/bin/nxedit` to open a path, so a build without it offers a file
    // row that opens nothing.
    v.push("nxedit");
    // The installer is in `/bin`, not the Applications menu; it ships in every image as its
    // authority is the view's.
    v.push("nxinstall");
    v
}

/// The applications a person launches, and what to call them: the display name and the filter come
/// from one entry. **Every `exec` must be a program [`profile_programs`] ships**, asserted in
/// `assemble_image`: an entry naming a program the package lacks builds clean, lists in the menu,
/// and opens no window.
pub(crate) const GRAPHICAL_APPLICATIONS: [(&str, &str); 3] =
    [("nxterm", "Terminal"), ("nxfiles", "Files"), ("nxedit", "Text Editor")];

/// Pack the initramfs CPIO `newc` archive at `out`: the config manifests and the `init` ELF.
pub(crate) fn build_initramfs(out: &Path, mode: BuildMode) -> R<()> {
    build_initramfs_for(out, mode, RootDevice::Disk)
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RootDevice {
    Disk,
    /// The `nitrox-live` partition inside `root.img`, a Limine module the kernel publishes as a RAM
    /// disk.
    Live,
}

/// [`build_initramfs`], for either kind of root. The live image's initramfs is the release one
/// with a single manifest line changed — `check-images` holds it to exactly that.
pub(crate) fn build_initramfs_for(out: &Path, mode: BuildMode, root: RootDevice) -> R<()> {
    let mut buf = Vec::new();
    let mut init_toml = String::from(INIT_TOML);
    if root == RootDevice::Live {
        let disk = format!("device = \"gpt-partlabel:{ROOT_PARTLABEL}\"");
        if init_toml.matches(&disk).count() != 1 {
            return Err(format!("INIT_TOML no longer names its root as {disk:?} exactly once").into());
        }
        init_toml = init_toml.replace(&disk, &format!("device = \"gpt-partlabel:{LIVE_ROOT_PARTLABEL}\""));
    }
    if mode.stages_test_data() {
        init_toml.push_str(TEST_BINDS_TOML);
    }
    cpio_entry(&mut buf, 1, "etc/init.toml", init_toml.as_bytes());
    // The declarations and the profile manifest are on the root, as this archive on the FAT ESP is
    // never written. Every program ELF is packed at `sbin/<name>`. **The list is the same in every
    // build mode**, so the boot path under test is the one that ships.
    for (ino, (name, _why)) in (2u32..).zip(INITRAMFS_PROGRAMS.iter()) {
        let elf = userspace_bin_path(name);
        let bytes = fs::read(&elf).map_err(|e| format!("read {name} ELF {}: {e}", elf.display()))?;
        cpio_entry(&mut buf, ino, &format!("sbin/{name}"), &bytes);
    }
    cpio_entry(&mut buf, 0, "TRAILER!!!", b"");
    // The tripwire is checked before the write so a tripped build leaves no image behind.
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
    println!("xtask: built initramfs ({} of {INITRAMFS_MAX_BYTES} bytes) at {}", buf.len(), out.display());
    Ok(())
}

pub(crate) fn assemble_image(
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

    {
        let f = fs::File::create(out)?;
        f.set_len(IMAGE_SIZE_MIB * 1024 * 1024)?;
    }

    // 2. GPT: an ESP (FAT32, `ESP_SIZE_MIB` from 1 MiB) and the ext4 `nitrox-root` filling the
    // rest. The GPT driver binds every non-empty entry (no type-GUID filter) as
    // `/dev/disk/by-partlabel/nitrox-root`.
    run(Command::new("sgdisk")
        .arg("--clear")
        .arg("-n")
        .arg(format!("1:2048:+{ESP_SIZE_MIB}M")) // ESP: LBA 2048 (1 MiB), bounded
        .arg("-t")
        .arg("1:ef00") // EFI System
        .arg("-c")
        .arg("1:NITROX_ESP")
        .arg("-n")
        .arg("2:0:0") // nitrox-root: next aligned → end
        .arg("-t")
        .arg("2:8300") // Linux filesystem
        .arg("-c")
        .arg("2:nitrox-root")
        .arg(out))?;

    let (esp_lba, esp_sectors) = partition_extent(out, 1)?;
    let (root_lba, root_sectors) = partition_extent(out, 2)?;

    let work = out.with_extension("partbuild");
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir_all(&work)?;

    // 3. The ESP is a separate, exactly-sized FAT32 image, so the FAT is bounded, spliced in.
    let esp = work.join("esp.img");
    // **A test image's second filesystem**: a small ext4 labelled `nitrox-scratch`, one more Limine
    // module published as a RAM disk, which the storage service auto-mounts so `boot-probe` can
    // exercise `/svc/storage` on every CI run. A **machine** difference, not a software one.
    let scratch = work.join("scratch.img");
    let test_conf = work.join("limine.conf");
    let scratch_module = [(scratch.as_path(), SCRATCH_ESP_PATH)];
    let (conf, extra): (&Path, &[(&Path, &str)]) = if mode.stages_test_data() {
        build_scratch_fs(&scratch)?;
        fs::write(&test_conf, test_limine_conf(&fs::read_to_string(conf)?)?)?;
        (test_conf.as_path(), &scratch_module[..])
    } else {
        (conf, &[])
    };
    build_esp(&esp, esp_sectors, bootx64, conf, kernel, initramfs, extra)?;
    splice_into(out, esp_lba * 512, &esp)?;

    // 4. The ext4 root is a separate, partition-sized image populated by `mke2fs -d`, then spliced
    // in, with the feature set `fs-server-ext4` reads.
    let staging = work.join("rootfs");
    stage_rootfs(&staging, mode)?;

    let rootfs = work.join("rootfs.ext4");
    let blocks = (root_sectors * 512) / 4096; // 4 KiB block count
    run(Command::new("mke2fs")
        .arg("-q")
        .arg("-F")
        .arg("-t")
        .arg("ext4")
        .arg("-O")
        .arg("^has_journal,^64bit,^metadata_csum,^resize_inode")
        .arg("-b")
        .arg("4096")
        .arg("-d")
        .arg(&staging)
        .arg(&rootfs)
        .arg(blocks.to_string()))?;
    splice_into(out, root_lba * 512, &rootfs)?;
    // `work/` stays for inspection; each run rebuilds it.
    Ok(())
}

/// Build an ESP image of `sectors` sectors: Limine, its configuration, the kernel and its font
/// licence, the initramfs and any `extra` files. Shared by both image builders so neither forgets a
/// file.
pub(crate) fn build_esp(
    esp: &Path,
    sectors: u64,
    bootx64: &Path,
    conf: &Path,
    kernel: &Path,
    initramfs: &Path,
    extra: &[(&Path, &str)],
) -> R<()> {
    {
        let f = fs::File::create(esp)?;
        f.set_len(sectors * 512)?;
    }
    let espf = esp.display().to_string();
    run(Command::new("mformat").arg("-i").arg(&espf).arg("-F").arg("-v").arg("NITROX_ESP"))?;
    run(Command::new("mmd")
        .arg("-i")
        .arg(&espf)
        .arg("::/EFI")
        .arg("::/EFI/BOOT")
        .arg("::/boot")
        .arg("::/boot/limine"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(bootx64).arg("::/EFI/BOOT/BOOTX64.EFI"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(conf).arg("::/boot/limine/limine.conf"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(kernel).arg("::/boot/kernel"))?;
    // **The kernel carries a font, so its licence travels with it**: the SIL OFL requires each copy
    // of Terminus Font to contain its notice.
    run(Command::new("mcopy")
        .arg("-i")
        .arg(&espf)
        .arg(repo_root().join("assets/fonts/LICENSE-Terminus.txt"))
        .arg("::/boot/LICENSE-Terminus.txt"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(initramfs).arg("::/boot/initramfs"))?;
    for (from, to) in extra {
        run(Command::new("mcopy").arg("-i").arg(&espf).arg(from).arg(format!("::{to}")))?;
    }
    Ok(())
}

const SCRATCH_LABEL: &str = "nitrox-scratch";

const SCRATCH_ESP_PATH: &str = "/boot/scratch.img";

/// What the scratch filesystem's `README` says, which `boot-probe` holds to its own copy.
const SCRATCH_README: &str = "nitrox-scratch: a test image's second filesystem, mounted by the storage service\n";

/// Its size: room for the probe's writes, and little to load.
const SCRATCH_MIB: u64 = 8;

/// **A file of [`SCRATCH_AHEAD_PAGES`] pages**, byte `i` being `i % 251`, which `boot-probe` reads
/// through a mapping to see fills read ahead; nothing opens it first.
const SCRATCH_AHEAD: &str = "ahead";

/// Its length in pages: two fills' worth at 32 pages a fill.
const SCRATCH_AHEAD_PAGES: usize = 64;

/// Build the test image's scratch filesystem at `out`: a bare ext4 labelled [`SCRATCH_LABEL`], so
/// its label is its own, holding a `README` and [`SCRATCH_AHEAD`], in the feature set
/// `fs-server-ext4` reads.
fn build_scratch_fs(out: &Path) -> R<()> {
    let staging = out.with_extension("staging");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    fs::write(staging.join("README"), SCRATCH_README)?;
    fs::write(
        staging.join(SCRATCH_AHEAD),
        (0..SCRATCH_AHEAD_PAGES * 4096).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
    )?;
    let _ = fs::remove_file(out);
    fs::File::create(out)?.set_len(SCRATCH_MIB * 1024 * 1024)?;
    run(Command::new("mke2fs")
        .arg("-q")
        .arg("-F")
        .arg("-t")
        .arg("ext4")
        .arg("-O")
        .arg("^has_journal,^64bit,^metadata_csum,^resize_inode")
        .arg("-b")
        .arg("4096")
        .arg("-L")
        .arg(SCRATCH_LABEL)
        .arg("-d")
        .arg(&staging)
        .arg(out)
        .arg((SCRATCH_MIB * 1024 * 1024 / 4096).to_string()))?;
    Ok(())
}

/// A test image's `limine.conf`, from the release one: the scratch filesystem as a module after the
/// initramfs. `base` must hold one entry and its initramfs line once, so the module cannot land
/// in an entry nobody boots.
pub(crate) fn test_limine_conf(base: &str) -> R<String> {
    let initramfs_line = "    module_path: boot():/boot/initramfs\n";
    if base.matches(initramfs_line).count() != 1 || base.matches("\n/").count() + base.starts_with('/') as usize != 1 {
        return Err("boot/limine.conf no longer has one entry with its initramfs line once".into());
    }
    Ok(base.replacen(initramfs_line, &format!("{initramfs_line}    module_path: boot():{SCRATCH_ESP_PATH}\n"), 1))
}
