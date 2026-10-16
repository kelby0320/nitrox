//! The disk image assembled: the initramfs and its programs, the store, the ESP, a scratch disk.

use crate::*;

pub(crate) fn find_bootx64(limine_root: &Path) -> R<PathBuf> {
    // The tarball layout has varied between versions; search a small set of
    // known locations rather than hard-coding one.
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
    // Fall back to a recursive scan.
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

/// The initramfs payload. Slice 4 ships a placeholder `etc/init.toml` (a single
/// critical-path mount, processed once an fs-server exists in slice 5+);
/// spawnable images move into the initramfs with the spawn-ABI work (slice 7).
const INIT_TOML: &str = "\
# Nitrox init manifest (Phase 2 slice 4 placeholder).\n\
[[mount]]\n\
fs_server = \"fs-server-ext4\"\n\
device = \"gpt-partlabel:nitrox-root\"\n\
mount_point = \"/\"\n\
mode = \"rw\"\n\
required_for = \"boot\"\n";

/// The binds a **test** image's `init.toml` adds: two more names for the root mount.
///
/// - `/subtreetest`, scoped to `/system`, which `boot-probe`'s `subtree_bind_test` resolves and
///   the demo harness's case 8 needs as a binding that is also an openable directory, to prove
///   `move` refuses to recurse through a mount.
/// - `/scratch`, scoped to `/scratch`: a second writable mount for the cross-mount half of
///   `move`. Same server, different subtree base, which is what the kernel classifies as
///   another mount.
///
/// Both were a `#[cfg(feature = "selftest")]` block in `init`'s `mount_one` — the last build-mode
/// branch in PID 1 — until `init.toml` grew `[[bind]]` (Phase 5 Part C.1).
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
/// `/system/services.toml` on the root — [`services_toml`] adds a test image's.
///
/// **One file, many `[service.<name>]` tables** — the 2026-08-21 change to
/// `docs/spec/service-toml-schema.md`. It previously said each file declares one service
/// and the manager scans the directory, and nothing in this system can enumerate a
/// directory of `.toml` files.
///
/// `executable` is a path per the schema, resolved through service-mgr's namespace:
/// `/bin/heartbeat` is projected from the content-addressed store by the profile server
/// (the real userspace path), not the initramfs `/sbin` staging.
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

/// **`heartbeat`, in a test image only** (administration Part E.1c). Phase 3's demo service, and
/// until then the one declaration a release image carried beyond the servers. A test image keeps
/// it as a service that runs for the whole boot under `always`, which is what
/// `check_service_attribution` leans on: a misattributed exit shows up as a restart of it.
/// Declared before [`BOOT_PROBE_TOML`] — and so before `restart-probe` and the login chain — as it
/// was when it sat at the end of [`SERVICES_TOML`].
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

/// The declarations file for `mode`, staged at `/system/services.toml` on the root
/// (administration Part E.1c; it was `/initramfs/etc/services.toml` until then). Its **content**
/// is what differs between a test image and a release image — see [`BOOT_PROBE_TOML`]. The
/// programs do not differ.
pub(crate) fn services_toml(mode: BuildMode) -> String {
    let mut services = String::from(SERVICES_TOML);
    if mode.stages_test_data() {
        services.push_str(HEARTBEAT_TOML);
        services.push_str(BOOT_PROBE_TOML);
    }
    // **`compose-bench` instead of `boot-probe`, not beside it.** `boot-probe` fires the boot
    // verdict, so anything declared after it never runs; and a measurement wants the screen to
    // itself, which is why this mode exists at all rather than the bench being one more service
    // in the harness image.
    if matches!(mode, BuildMode::Bench) {
        services = services.replace(BOOT_PROBE_TOML, BENCH_TOML);
    }
    services
}

/// The system profile manifest for `mode`, staged at `/system/profiles/system.toml` on the root
/// (administration Part E.1c; `/initramfs/etc/profiles/system.toml` until then). The profile
/// server reads it and projects the listed packages' `bin/` into `/bin`. Generated rather than a
/// constant because it names store paths, whose hashes are content-derived at build time and must
/// match the store directories on the same root. See
/// `docs/architecture/profiles-and-namespace-projection.md`.
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
    // The test package, in selftest/test-harness builds only. Projected into `/bin` like any
    // other package, so `service-mgr` spawns `/bin/ui-testclient` by exactly the path it spawns
    // `/bin/logging-service` by — one mechanism, not a test-only one.
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

/// The `boot-probe` declaration, **appended to [`SERVICES_TOML`] in selftest and
/// test-harness images and absent from a release image**.
///
/// This is the retrofit's governing decision 3 made concrete: the test image differs from
/// the release image *by data*. `init` and `service-mgr` are byte-identical in both; one
/// of them reads a file with an extra table in it. See
/// `docs/planning/test-path-retrofit.md`.
///
/// `policy = "never"` because the probe runs once and exits — a restart would re-run
/// checks that have already reported. `/bin/boot-probe` comes from the `test-harness`
/// store package, which is itself absent from a release image, so the declaration and the
/// executable appear and disappear together.
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

/// The `compose-bench` declaration, replacing [`BOOT_PROBE_TOML`] in a [`Bench`] image.
///
/// **`after` every display client in the image**, which is the point: `display-selftest`,
/// `nxterm` and the two test clients all draw at startup, and a frame timed while one of them is
/// compositing is noise attributed to whichever arm happened to be running. Ordering is the only
/// tool available — there is no "the screen is quiet now" signal — so the bench goes last and
/// ends the boot itself.
///
/// [`Bench`]: BuildMode::Bench
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

/// Build path for the packed initramfs CPIO archive.
pub(crate) fn initramfs_path() -> PathBuf {
    build_cache().join("initramfs.cpio")
}

/// The live image (Phase 5 Part C) — a separate file, so a live build never clobbers the image
/// every other gate boots.
pub(crate) fn live_image_path() -> PathBuf {
    build_cache().join("nitrox-live.img")
}

/// The live image's initramfs.
pub(crate) fn live_initramfs_path() -> PathBuf {
    build_cache().join("initramfs-live.cpio")
}

/// The **test** live image (administration Part C.8): the live image built with `--selftest`, for
/// `check-storage`. A file of its own, so a test stick never stands in for the release one that
/// `check-live`, `check-report` and `check-install` boot.
pub(crate) fn live_test_image_path() -> PathBuf {
    build_cache().join("nitrox-live-test.img")
}

/// The test live image's initramfs.
pub(crate) fn live_test_initramfs_path() -> PathBuf {
    build_cache().join("initramfs-live-test.cpio")
}

/// The root partition's label on a disk.
pub(crate) const ROOT_PARTLABEL: &str = "nitrox-root";

/// The root partition's label inside the live image's `root.img`. **Distinct on purpose**: a stick
/// booted on a machine with Nitrox installed must not find two partitions answering to one name.
pub(crate) const LIVE_ROOT_PARTLABEL: &str = "nitrox-live";

/// Room the live root filesystem gets beyond what is staged on it, for what a session writes.
/// The RAM disk takes writes; they are gone at power-off.
pub(crate) const LIVE_ROOT_SLACK_MIB: u64 = 16;

/// Room the pristine root, `install-root.img`, gets beyond what is staged on it (administration
/// Part G.1): **for `mke2fs`'s own tables, not for writing**. Nothing mounts the pristine copy and
/// nothing writes it, so it carries none of [`LIVE_ROOT_SLACK_MIB`], and every MiB saved is one the
/// firmware does not read off the stick on the install entry.
pub(crate) const INSTALL_ROOT_MARGIN_MIB: u64 = 4;

/// Ceiling on `root.img`. **Its reason is the firmware**: the whole file is read off a USB stick
/// by UEFI Boot Services before the kernel runs, on a laptop whose USB stack is the firmware's.
/// Today's release root stages about 8 MiB; an image near this size means something large got
/// staged, and it should be a decision rather than a slower boot nobody chose.
pub(crate) const LIVE_ROOT_MAX_MIB: u64 = 64;

/// The programs in the boot image, each with **the reason it cannot come from the filesystem**.
///
/// A pair rather than a bare name, because the reason is the whole rule and a list of names
/// does not carry it. This has now drifted twice — services accumulated here until
/// 2026-08-03, and the display arm's two servers went back in over the following week — both
/// times because adding a name to a list of names is a one-word change that looks like every
/// other one. Adding an entry here means writing down why the program is special, and if you
/// cannot, it belongs in the store like everything else.
///
/// The bar is "required to get from the bootloader to a mounted root, plus what cannot live
/// on the filesystem it depends on". Everything else is projected into `/bin` by the profile
/// server and spawned from there.
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

/// The largest the packed initramfs may be, in bytes.
///
/// Not a memory limit: it is a **tripwire on the rule above**. Nothing releases the initramfs
/// (`sys_release_initramfs` is referenced in the docs and does not exist), so every byte here
/// is held for the machine's uptime — but the reason for the ceiling is that the list drifts
/// silently and a size is the one symptom that shows up without anyone looking. The four
/// programs come to ~231 KB; this leaves room for them to grow by half again before anyone
/// has to think about it, and fails immediately if a fifth is added.
///
/// A failure is a question, not a wall: either the new program has a bootstrap reason — in
/// which case add it to [`INITRAMFS_PROGRAMS`] with that reason and raise this — or it does
/// not, and it belongs in a store package.
const INITRAMFS_MAX_BYTES: usize = 384 * 1024;

/// Append one CPIO `newc` entry (header + NUL-terminated name + data, each region
/// NUL-padded to a 4-byte boundary) to `out`. Matches `kernel/src/initramfs.rs`.
fn cpio_entry(out: &mut Vec<u8>, ino: u32, name: &str, data: &[u8]) {
    let namesize = name.len() + 1; // includes the trailing NUL
    out.extend_from_slice(b"070701");
    // 13 eight-hex fields: ino, mode, uid, gid, nlink, mtime, filesize,
    // devmajor, devminor, rdevmajor, rdevminor, namesize, check.
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

/// The release path of a built userspace binary (bare target).
pub(crate) fn userspace_bin_path(name: &str) -> PathBuf {
    userspace_release_dir().join(name)
}

/// FNV-1a content hash of `bytes` as the store path's opaque `<hash>` (12 hex chars).
/// A non-cryptographic content hash — sufficient as a unique, deterministic directory
/// name for now; the Nix-style build-input hash arrives with the build system. See
/// `docs/architecture/content-addressed-store.md`.
fn store_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

/// A store path for a package containing several programs.
///
/// The hash covers **every** ELF in the package, concatenated in the given order, so
/// changing any one of them moves the whole package — which is the property that makes a
/// content-addressed path mean anything. Hashing only the first would let nine of the ten
/// coreutils change under a path that claims they did not.
pub(crate) fn store_path_for_all(bins: &[&str], name: &str, version: &str) -> R<String> {
    let mut bytes = Vec::new();
    // **The desktop entries are part of the package's contents**, so they are part of its hash
    // (PR #279 review, finding 3). Without this, changing a display name produces different
    // package contents at a byte-identical store path — and `profile-server`'s caching rests on
    // "a store path is content-addressed, so it cannot change contents". Hashed for every package
    // rather than only the one that has them: an empty contribution costs nothing and a second
    // package growing entries then needs no second edit here.
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

/// The policy the build seeds at `/system/views.toml` — see where it is staged.
///
/// **`view_broker::policy::seed`, naming `DEMO_USER`** (administration Part G.2): the installer
/// writes the same policy onto an installed machine, naming its first account, so the two cannot
/// drift. A policy that named someone the build did not make would deny everything while reading
/// perfectly well.
pub(crate) fn seeded_views_toml() -> String {
    view_broker::policy::seed(DEMO_USER)
}

/// The programs a session gets through its profile: the coreutils, plus `nxsh`.
///
/// `nxsh` is here as well as being the login leaf — a user should be able to run a nested
/// shell, and one that cannot invoke itself is a strange thing to hand someone.
///
/// One list, three consumers (the build, the initramfs, the store package), so a new
/// coreutil cannot end up built-but-unreachable or packaged-but-unbuilt.
pub(crate) fn profile_programs() -> Vec<&'static str> {
    let mut v = COREUTILS.to_vec();
    v.push("nxsh");
    // The terminal is a program a person runs, not a service the system runs — the same class
    // as the shell beside it. Part C's `session-mgr` will spawn it from `/bin` like any other.
    v.push("nxterm");
    // The file browser, likewise a program a person runs. In `/bin`, so the Applications menu
    // lists it without anything being told about it (M10 Part B).
    v.push("nxfiles");
    // The editor (M10 Part D). In `/bin` for two reasons rather than one: the menu lists it,
    // *and* `desktop-shell` resolves `/bin/nxedit` when a client asks it to open a path — so a
    // build that packaged the browser and not the editor would present a file row that opens
    // nothing, which is the failure this list exists to make impossible.
    v.push("nxedit");
    // The installer (Phase 5 Part H.1). In `/bin` and **not** in the Applications menu: it is a
    // command-line tool like the coreutils, typed at a terminal as `with admin nxinstall`, and a
    // menu entry would offer a person a window that does not exist. It ships in every image
    // because its authority is the view's, not the program's (administration Part G.3): run
    // without one, it finds no disk.
    v.push("nxinstall");
    v
}

/// The applications a person launches, and what to call them.
///
/// **The display name is the point as much as the filter is.** A menu listing `nxfiles` is
/// naming a binary; one listing "Files" is naming an application. Both come from the same entry,
/// so they cannot disagree — and when an icon set exists, it goes here too rather than becoming a
/// fourth place that has to be kept in step.
///
/// **Every `exec` must be a program [`profile_programs`] ships**, asserted in `assemble_image`.
/// This is a fourth list beside that one's three consumers, and without the check an entry can
/// name a program the package does not contain — which builds clean, lists in the menu, logs a
/// launch, and opens no window (PR #279 review, finding 5).
pub(crate) const GRAPHICAL_APPLICATIONS: [(&str, &str); 3] =
    [("nxterm", "Terminal"), ("nxfiles", "Files"), ("nxedit", "Text Editor")];

/// Pack the initramfs CPIO `newc` archive at `out`: the config manifests, the `init`
/// ELF (the kernel boot-loads `/sbin/init` from here — retiring the embedded copy),
/// and the mandatory `TRAILER!!!`. Built by `cmd_build` before this runs.
pub(crate) fn build_initramfs(out: &Path, mode: BuildMode) -> R<()> {
    build_initramfs_for(out, mode, RootDevice::Disk)
}

/// Where an image's root filesystem is, as `init.toml` names it.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RootDevice {
    /// The `nitrox-root` partition on the boot disk.
    Disk,
    /// The `nitrox-live` partition inside `root.img`, a Limine module the kernel publishes as a RAM
    /// disk (Phase 5 Part C).
    Live,
}

/// [`build_initramfs`], for either kind of root. The live image's initramfs is the release one
/// with a single manifest line changed — `check-images` holds it to exactly that.
pub(crate) fn build_initramfs_for(out: &Path, mode: BuildMode, root: RootDevice) -> R<()> {
    let mut buf = Vec::new();
    // The mount manifest. A test image's also re-binds the root for the two fixtures that need
    // a second name on it — data, where it was `init`'s last build-mode `cfg` until Part C.1.
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
    // **The declarations and the profile manifest are not here** (administration Part E.1c): both
    // are on the root, at `/system/services.toml` and `/system/profiles/system.toml`, read after
    // `init` has mounted it. Neither has a bootstrap reason, and neither could be edited here —
    // this is a boot archive on the FAT ESP, which nothing writes.
    //
    // Pack every program ELF at `sbin/<name>`: the kernel boot-loads `/sbin/init`, and
    // the spawners resolve their children by path (`/initramfs/sbin/<name>`), retiring
    // the kernel-embedded `ImageId` images. Built by `cmd_build` before this runs.
    //
    // **The list is the same in every build mode.** The initramfs a test boots is therefore
    // the initramfs a release boots, so the boot path under test is the boot path that ships.
    // Until 2026-08-11 a test image's was 680 KB against a release's 323 KB, and both carried
    // programs with no bootstrap role at all.
    for (ino, (name, _why)) in (2u32..).zip(INITRAMFS_PROGRAMS.iter()) {
        let elf = userspace_bin_path(name);
        let bytes = fs::read(&elf).map_err(|e| format!("read {name} ELF {}: {e}", elf.display()))?;
        cpio_entry(&mut buf, ino, &format!("sbin/{name}"), &bytes);
    }
    cpio_entry(&mut buf, 0, "TRAILER!!!", b"");
    // The tripwire. Checked before the write so a build that trips it does not leave an image
    // behind that boots and looks fine.
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

    // 1. Allocate the raw disk.
    {
        let f = fs::File::create(out)?;
        f.set_len(IMAGE_SIZE_MIB * 1024 * 1024)?;
    }

    // 2. GPT: an EFI System Partition (FAT32, ESP_SIZE_MIB starting at 1 MiB) and
    //    the ext4 `nitrox-root` filesystem filling the rest. The slice-6 GPT driver
    //    enumerates every non-empty entry (no type-GUID filter) and binds
    //    `/dev/disk/by-partlabel/nitrox-root` at boot — so the second partition
    //    rides the existing boot disk; no separate QEMU drive is needed.
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

    // Query each partition's on-disk extent (robust to GPT's end-of-disk reserve).
    let (esp_lba, esp_sectors) = partition_extent(out, 1)?;
    let (root_lba, root_sectors) = partition_extent(out, 2)?;

    // A scratch dir for the per-partition images + the ext4 staging tree.
    let work = out.with_extension("partbuild");
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir_all(&work)?;

    // 3. Build the ESP as a separate, exactly-partition-sized FAT32 image (so the
    //    FAT is bounded to the partition), then splice it in. mformat on a plain
    //    file formats the whole file; no `@@offset` games.
    let esp = work.join("esp.img");
    // **A test image's second filesystem** (administration Part C.5b): a small ext4, labelled
    // `nitrox-scratch`, as one more Limine module, which the kernel publishes as a RAM disk. It is
    // what the storage service finds to auto-mount on a test boot, and so what lets `boot-probe`
    // mount, read and write through `/svc/storage` on every CI run. The disk it sits beside holds
    // only `init`'s root and the FAT ESP, neither of which the service mounts. A **machine**
    // difference, not a software one: the programs are the release programs, as `check-images`
    // holds, and a release boot simply has one disk fewer.
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

    // 4. Build the ext4 `nitrox-root` filesystem as a separate, partition-sized
    //    image populated at creation (`mke2fs -d`, no root/mount), then splice it
    //    in. The feature set matches the fs-server-ext4 reader's support (the
    //    Part-2 fixture uses the same flags). The staging tree holds the milestone
    //    file the Part-6 init loop reads.
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

    // Leave `work/` in place for inspection; `cmd_image` rebuilds it each run.
    Ok(())
}

/// Build an ESP image of `sectors` sectors: Limine, its configuration, the kernel and its font
/// licence, the initramfs, and any `extra` `(source, ESP path)` files — the live image's
/// `root.img`. Shared by both image builders so neither can forget a file the other ships.
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
    // **The kernel carries a font, so its licence travels with it.** `kernel/src/fbcon/glyphs.rs`
    // embeds Terminus Font, and the SIL OFL lets a bundled copy ship only if "each copy contains
    // the above copyright notice and this license" — the same reason `LICENSE-DejaVu.txt` is
    // staged beside the TrueType faces in `/system/fonts`. Beside the kernel, because that is the
    // file the face is inside (PR #296 review).
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

/// The scratch filesystem's label: the name the storage service mounts it under.
const SCRATCH_LABEL: &str = "nitrox-scratch";

/// Where the scratch filesystem rides on a test image's ESP.
const SCRATCH_ESP_PATH: &str = "/boot/scratch.img";

/// What the scratch filesystem's `README` says, which `boot-probe` reads through the storage
/// service and holds to its own copy of this text.
const SCRATCH_README: &str = "nitrox-scratch: a test image's second filesystem, mounted by the storage service\n";

/// Its size: room for the probe's writes, and little to load.
const SCRATCH_MIB: u64 = 8;

/// **A file of [`SCRATCH_AHEAD_PAGES`] pages on the scratch filesystem**, byte `i` being `i % 251`,
/// which `boot-probe` reads through a mapping to see fills read ahead (Phase 6 Part H.4): nothing
/// opens it before then, so its pages come from the device.
const SCRATCH_AHEAD: &str = "ahead";

/// Its length in pages: two fills' worth at 32 pages a fill.
const SCRATCH_AHEAD_PAGES: usize = 64;

/// Build the test image's scratch filesystem at `out`: an ext4 labelled [`SCRATCH_LABEL`] holding a
/// `README` and [`SCRATCH_AHEAD`], with the feature set `fs-server-ext4` reads. A bare filesystem, with no partition
/// table, so its label is its own.
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

/// A test image's `limine.conf`, from the release one (`base`): the scratch filesystem as a module
/// after the initramfs. `base` must hold one entry and its initramfs line once, as
/// [`live_limine_conf`] requires, so the module cannot land in an entry nobody boots.
pub(crate) fn test_limine_conf(base: &str) -> R<String> {
    let initramfs_line = "    module_path: boot():/boot/initramfs\n";
    if base.matches(initramfs_line).count() != 1 || base.matches("\n/").count() + base.starts_with('/') as usize != 1 {
        return Err("boot/limine.conf no longer has one entry with its initramfs line once".into());
    }
    Ok(base.replacen(initramfs_line, &format!("{initramfs_line}    module_path: boot():{SCRATCH_ESP_PATH}\n"), 1))
}
