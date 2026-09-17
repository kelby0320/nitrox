# Boot Flow

**Status:** Current (last checked 2026-09-17, when Phase 5 Part H.1 gave the live image a third menu entry and a fourth thing in its ESP — the installable ESP an installed machine boots from; before that 2026-09-14, when the framebuffer console took the first line of `kernel_main` and Part D made every boot log its handoff and CPU). Describes the boot as it runs today — UEFI →
Limine → kernel → `init` → fs-server → `service-mgr` → `auth-service` → `session-mgr` → login →
`nxsh`, and in a release image on to the graphical session (Phases 0–4 complete, Phase 4 closed
2026-09-10). Every stage below is exercised on each CI run by
`cargo xtask test-qemu` (headless, adjudicated by `isa-debug-exit`) and
`cargo xtask test-interactive` (expect-driven over the serial console).
Verified against source 2026-08-24, when the demo chain and the graphical clients became service declarations and §5 was rewritten (the display arm's position in § 5 changed that day).

## Overview

```
UEFI firmware (OVMF under QEMU)
  └─► Limine (BOOTX64.EFI) — reads limine.conf, loads kernel + initramfs module
        └─► kernel _start → kernel_main
              ├─ serial console, GDT/TSS/IDT
              ├─ buddy + slab allocators, paging, HHDM
              ├─ initramfs module registered
              ├─ platform discovery (ACPI: PCIe ECAM, interrupt routing)
              ├─ local APIC (x2APIC), TSC + LAPIC timer calibration
              ├─ DPC queue, interrupt router (IOAPIC)
              ├─ global handle table
              ├─ scheduler, then AP bring-up (SMP)
              └─► first userspace process
                    └─► init (pid 1)
                          ├─ read /initramfs/etc/init.toml
                          ├─ mount critical path (spawn fs-server per mount)
                          ├─ bind profile-server at /bin
                          ├─ bind logging-service at /log
                          ├─ bind tty-server at /dev/tty
                          ├─ bind input-server at /dev/input/new
                          ├─ bind compositor at /dev/draw
                          └─► service-mgr
                                ├─► auth-service
                                └─► session-mgr ─► login ─► nxsh
```

Failure on the critical path drops to `eshell`, the emergency shell (see § "The
emergency path").

---

## 1. Firmware: UEFI loads Limine

QEMU is launched with OVMF as `pflash`. UEFI scans removable media, finds the EFI System
Partition (GPT type `EF00`) on the virtual disk, and loads `EFI/BOOT/BOOTX64.EFI` from the
FAT32 inside. That binary is Limine's UEFI loader, vendored under
`tools/build-cache/limine/` by `tools/xtask`.

Disk image layout (built by `cargo xtask image`; sizes are `IMAGE_SIZE_MIB` and
`ESP_SIZE_MIB` in `tools/xtask/src/main.rs`):

```
nitrox.hdd (128 MiB raw, GPT — two partitions)
├── partition 1 (EFI System, FAT32, 48 MiB, "NITROX_ESP", type ef00)
│   ├── /EFI/BOOT/BOOTX64.EFI         ← Limine v12.2.0
│   ├── /boot/limine/limine.conf      ← vendored from boot/limine.conf
│   ├── /boot/kernel                  ← our ELF
│   └── /boot/initramfs               ← cpio; the boot-critical closure
└── partition 2 (ext4, "nitrox-root", type 8300, rest of disk)
    └── the root filesystem: /system, /home, /store
```

**The live image** (`cargo xtask image --live`, Phase 5 Part C) is the same boot with its root in
RAM, for a machine whose storage the kernel cannot reach — a USB stick, before there is a USB
driver:

```
nitrox-live.img (GPT — one partition)
└── partition 1 (EFI System, FAT32, "NITROX_ESP")
    ├── /EFI/BOOT/BOOTX64.EFI, /boot/kernel, /boot/LICENSE-Terminus.txt   ← as above
    ├── /boot/limine/limine.conf      ← the release one, with a menu of three entries
    ├── /boot/initramfs               ← the release one, but init.toml names nitrox-live
    ├── /boot/root.img                ← GPT image, one ext4 partition "nitrox-live":
    │                                     the release root, built by the same staging
    └── /boot/install-esp.img         ← FAT32 image: the ESP an *installed* machine boots
                                          from. The release menu, and the release
                                          initramfs, which names nitrox-root
```

**Which modules a boot loads depends on the entry chosen**, because Limine loads what the entry
names. All three load the initramfs and `root.img`; only `Nitrox — install to this machine` also
loads `install-esp.img`, so an ordinary live boot does not read a further 33 MiB off the stick,
hold it for the session, or publish a block device no session on that boot could reach. The
kernel publishes every module after the initramfs as a RAM-backed block device
(`kernel/src/io/ramdisk.rs`); `init` mounts `root.img`'s partition as it would a disk, and the
installer reads both its sources — `install-esp.img` and `root.img` — as ordinary devices.

`cargo xtask check-images` holds three things: the live initramfs to the release one but for
`etc/init.toml`, the filesystem inside `root.img` to the release root partition's, and the
filesystem inside `install-esp.img` to the **release image's own ESP**, file for file. The third
is what catches a module built with the live `limine.conf` or the live initramfs — an installed
machine that mounts the stick it was installed from, which boots once, on the desk, and never
again.

The initramfs holds **four programs and two manifests**, and the rule is narrow: a program is
in the boot image only if it cannot come from a filesystem. `init` (the kernel boot-loads it),
`fs-server-ext4` (it *is* the root mount), `eshell` (the recovery path *for a failed mount*),
and `profile-server` (`/bin` does not exist until it runs). Everything else — the services,
the coreutils, the display arm, the test programs — lives in the content-addressed store and
is projected into `/bin`. The list is the same in every build mode, so the boot path a test
exercises is the boot path that ships. See `tools/xtask/src/main.rs`'s `INITRAMFS_PROGRAMS`,
which pairs each entry with its reason, and the ceiling that fails the build if the list
grows.

The second partition rides the same boot disk on purpose: the GPT driver enumerates every
non-empty entry and binds `/dev/disk/by-partlabel/nitrox-root`, so no separate QEMU drive
is needed.

## 2. Limine reads `limine.conf` and finds the kernel

`boot/limine.conf` names one entry pointing at `boot():/boot/kernel`, with
`module_path: boot():/boot/initramfs`. Timeout is 0 so the entry boots immediately.

**The live image's copy has a menu**, generated by `cargo xtask image --live` from this one:
`timeout: 5`, the entry above first — so the countdown boots it — then `Nitrox — hardware report`,
identical but for `cmdline: hwreport` (Phase 5 Part D), and `Nitrox — install to this machine`,
identical but for `cmdline: install` (Part H.1). Only the live image carries them, because it is
the stick a person boots an unfamiliar machine with; no other image pays for a countdown. **The
default entry passes no command line at all**, which is what keeps an ordinary live boot sandboxed:
`install` is what reaches a session, and only the third entry says it.

Limine loads the kernel ELF, scans it for our request statics (the `.limine_requests`
bracketed region — see `kernel/linker.ld` and `kernel/src/main.rs`), and sets up:

- 64-bit long mode, with 4-level paging
- A higher-half kernel mapping anchored at `0xffffffff80000000`
- A higher-half direct map of physical memory (HHDM)
- The framebuffer (linear, 32 bpp, driven by Limine's response struct), mapped in the higher
  half — the console draws through that mapping from the first line of `kernel_main`
- A 64 KiB stack in bootloader-reclaimable memory
- A bootloader GDT with `CS=0x28`, `DS=0x30`
- `RFLAGS.IF = 0` (interrupts disabled)

…and jumps to our ELF entry, `_start`. Per the Limine protocol the return address pushed
onto the stack is zero; the kernel must not return.

**The Limine bindings are hand-rolled** `#[repr(C)]` types in `kernel/src/limine.rs`, not
the `limine` crate — the kernel takes no external crates. Bump `LIMINE_VERSION` in `xtask`
and the bindings together.

## 3. Kernel `_start` → `kernel_main`

`kernel/src/main.rs` declares ten request statics plus the two bracketing markers, linked
into `.limine_requests*` by `kernel/linker.ld` (the ACPI RSDP request is the arch layer's, in
`kernel/src/arch/x86_64/acpi.rs`, and the linker collects it all the same):

- `BASE_REVISION` — `BaseRevision::new(6)`, the protocol revision we require. Checked
  before anything else is trusted.
- `FRAMEBUFFER_REQUEST`, `MEMMAP_REQUEST`, `HHDM_REQUEST`, `MODULE_REQUEST`, `SMP_REQUEST`
- `BOOTLOADER_INFO_REQUEST`, `FIRMWARE_TYPE_REQUEST`, `DATE_AT_BOOT_REQUEST`, `CMDLINE_REQUEST`
  — facts the boot reports (step 3) and nothing else reads.
- `REQUESTS_START` / `REQUESTS_END` — mandatory under base revision 6.

All but `BASE_REVISION` are `static mut`: Limine writes their `response` field after the
binary is loaded, and a plain `static` would let rustc constant-fold the read.

`kernel_main` then brings the system up in this order — the ordering is load-bearing, and
each step's rationale is in the source comments:

1. **CPU tables** — GDT + TSS, then IDT (`arch::Cpu::init_tables`). First because it prints
   nothing and needs nothing, and everything after it can fault: with the IDT live, a bad
   framebuffer descriptor produces a register dump rather than a silent triple fault.
2. **The screen, then serial.** Neither needs anything but what Limine handed over, so every
   later step can report progress *and failure*. The screen goes first so that the first line is
   on it: `fbcon::init` takes Limine's framebuffer and draws everything COM1 receives from here
   until a client is handed `/dev/framebuffer`, which on a machine with no serial port is the only
   diagnosis there is. See [the framebuffer console](framebuffer-console.md). (Serial came first,
   before the CPU tables, until Phase 5 Part B, 2026-09-14.)
3. **What was handed over, and what this CPU is** (Phase 5 Part D, 2026-09-14). `log_handoff`
   prints the bootloader and its version, the firmware type and base revision, the HHDM offset,
   the firmware's date, the boot entry's command line, and the memory map summed by kind; the
   command line is then parsed (`kernel/src/cmdline.rs`) — `hwreport[=<seconds>]` is its one flag,
   and a word it does not know is **passed on**, not ignored: the whole line is served at
   `/proc/cmdline`, where `install` reaches `libsession` (Part H.1). Never fatal;
   `arch::Cpu::log_identity` prints the vendor, family/model/stepping and brand, and the
   features the kernel requires, uses when present, and warns about, each `+` or `-`. Both only
   read, and both run before the step that panics on a missing required feature
   (`init_protections`), so the line naming what is missing is on the screen first. Later steps
   add their own facts the same way — every ACPI table and MADT entry, each PCI function's
   capabilities and what its driver did with it, the framebuffer's row padding, whether a UART
   answers at COM1 — so every boot's log is a hardware report of the machine it ran on.
4. **Memory** — walk Limine's memory map, bring up the buddy allocator and the slab over
   it. This is the first code to walk firmware structures and the first place a fault can
   happen, which is why the IDT is already live.
5. **Paging** — `paging_init` enables NX and captures the kernel-half PML4 template every
   future `AddressSpace::new` inherits. Must precede any address-space construction.
6. **initramfs** — register the first Limine module so the `/initramfs` resource server
   can serve it, and record every further module as a disk to publish at device probe (the live
   image's `root.img`). Needs the HHDM.
7. **Platform discovery** — ACPI on x86_64: the PCIe ECAM window and the interrupt-routing
   topology. Missing or malformed tables are logged, not fatal.
8. **Local APIC** (x2APIC), then **TSC + LAPIC timer calibration** against the legacy PIT.
9. **DPC queue**, then the **interrupt router** (IOAPIC).
10. **Global handle table.**
11. **Scheduler** (`sched_bringup`), then **AP bring-up** (`bring_up_aps`) via Limine's SMP
    response — capped at `MAX_CPUS`, extras left parked (a supported configuration, not a
    failure). Absent an SMP response the system stays single-CPU. **Fatal** since 2026-08-19
    if a CPU we launched fails to come online within 5 s, whether it faulted on the way in or
    never reached our code: the kernel's view of the machine must match the machine, so it
    stops rather than booting a topology nobody chose. See `docs/decision-log.md`.
12. **Display aperture** — record Limine's framebuffer (physical base, geometry, channel
    layout) so `/dev/framebuffer` can serve it. Must precede the first userspace process, which
    binds that path into init's namespace.
13. **The hardware report**, only on a command line with `hwreport` (`kernel/src/report.rs`). Here
    because every fact it shows now exists — drivers bound, CPUs up, the aperture recorded — and no
    userspace does. It copies the kernel log out of the ring, holds the framebuffer console
    (writes still reach the grid and COM1, and none is drawn over the page), and shows the log a
    page at a time, each with a `— page 2/3 — any key —` prompt, turning on an i8042 key press. No
    key within the bound (120 s, or `hwreport=<seconds>`) ends the whole report rather than the
    page; no keyboard, or no console on the screen, and nothing is held. The keys that turned the
    pages are drained from the keyboard's ring before the next step.
14. **The first userspace process**, then the boot thread retires. (A boot banner was drawn
    here until Phase 5 Part B, clearing the screen with nothing ordering it against the first
    client's frame.)

## 4. The first userspace process

`run_first_userspace` arms the syscall fast path, then builds pid 1 by hand — this is the
one process nothing else can construct:

- Allocate an address space; load `/sbin/init` from the initramfs (halt if absent).
- Allocate the process and a notification channel, and a handle to it.
- Allocate a namespace and bind the initial set: `/dev/entropy`, `/dev/console`,
  `/dev/log`, `/proc/self/*`, `/proc/cmdline`, `/proc/sched/stats`, `/initramfs`, `/dev/blk`
  (whole disks, their partitions and any module RAM disk, each with an `info` leaf), and
  `/dev/framebuffer` (the display aperture, plus its `info` leaf — recorded at step 12 of
  § 3).
- Spawn with exactly two handles — the notification channel and the namespace root.

**init receives two handles and no more.** Everything else it obtains, it obtains by
lookup in the namespace it was given, which is the capability model's opening move rather
than an implementation detail.

## 5. init (pid 1)

`_start(notif, root_ns, …)` in `userspace/init/src/main.rs`:

1. **Read the manifest** — `/initramfs/etc/init.toml`, parsed into an ordered list of
   mounts (shallowest first). Unreadable, unparseable or non-UTF-8 → the emergency path.
2. **Mount the critical path.** Per entry: resolve the device, spawn the fs-server, hand
   over the device handle, wait for its `Ready` message carrying the server's endpoint,
   and bind that endpoint at the mount point. Any failure → the emergency path. The
   server reads the superblock and the root directory **before** it answers, and a device
   it cannot serve gets a refusal instead, which init prints:
   `init: fs-server-ext4 for / on gpt-partlabel:nitrox-live refused: no ext4 filesystem: …`.
3. **Bind the system servers**, each by the same spawn → `Ready` → bind handshake:
   `profile-server` at `/bin` (projecting the store), `logging-service` at `/log`, and
   `tty-server` at `/dev/tty`.

   The tty server is the one **non-fatal** binding: if it fails, init logs "no terminal
   server; sessions will have no `/dev/tty`" and continues.
4. **Bring up the display arm** — `input-server` at `/dev/input/new`, then `compositor` at
   `/dev/draw`. Both non-fatal: a machine with no i8042 has no raw input nodes, the server
   says so and exits, and everything else comes up normally.

   **The order within the step is load-bearing**: the compositor resolves `/dev/input/new`
   during its own startup, before it answers `Ready`. Spawned the other way round it would
   serve the display with no input for the life of the boot, with only a log line to say so.

   **This step is after step 3, not before it** (since 2026-08-11): both are spawned from
   `/bin`, so they cannot start until the profile server has provided it. They used to come
   first only because they were initramfs-resident.
5. **Hand off** to `service-mgr` and stay resident as supervisor.

**`init` runs the same code in both images**, bar one namespace binding (retrofit Part C1
2026-08-21, Part C2 2026-08-24). The demo chain, the display self-test, `nxterm` and the two graphical test
clients used to be spawned here under `selftest`; they are **service declarations** now,
started by `service-mgr` from `/initramfs/etc/services.toml`, which carries them only in a
test image. Their order is the file's order, and `after` holds `boot-probe` until the demo
chain has exited — the sequencing this function used to enforce by running the chain
synchronously.

Step 5 is therefore unconditional: spawn `service-mgr` and supervise it. That branch was
`#[cfg(not(feature = "selftest"))]`, so a test image reaped the demo `parent` as its primary
child instead and PID 1's restart-on-death was code no gate could reach.

**The filesystem checks are no longer init's** (retrofit Part C1, 2026-08-21): the large-file
read, overwrite, grow, create and subtree-bind checks moved to `boot-probe`, a declared
service `service-mgr` starts, so they run *after* the step-5 handoff rather than between
steps 2 and 3. They also gate the boot verdict now, which they never did here — every failure
path in init was a bare `return` after a `FAIL` print. The one thing they need from init — a
second name on the root, `/subtreetest` scoped to `/system` — is data too since Phase 5 Part C.1:
a `[[bind]]` in the test image's `init.toml`, where it was `init`'s last build-mode `cfg`.

**Who fires the verdict** is `boot-probe`, not init; init only ever fires FAIL, and now only
for a critical-path boot failure — a demo chain that dies partway is caught by `test-qemu`'s
transcript check rather than by init reading an exit code. See
[`qemu-integration-tests.md`](../conventions/qemu-integration-tests.md).

## 6. service-mgr → session-mgr → login

`service-mgr` reads service declarations, constructs each service's namespace and handle
set, spawns it and supervises it. On the login path specifically:

1. **`session-mgr`** — spawned with re-delegated `BIND_NAMESPACE`, then handed the fs-server,
   profile-server and tty-server endpoints. It resolves `/svc/auth` itself.
2. **`desktop-session-mgr`** — the same, with its own duplicates of those three endpoints.
   Non-fatal if it fails: a machine with a serial login is degraded, one with neither is
   unreachable.

**`auth-service` is not spawned here.** It was until M7 Part C; it is a resource server bound
at `/svc/auth`, and only `init` can bind into the root namespace — a declared service holds an
inherited LOOKUP-only root. Both supervisors resolve their own session from that path.

Each supervisor presents a login, authenticates against `auth-service`, constructs a session
namespace and spawns a leader into it with empty syscaps: `nxsh` for the serial column
(**the login leaf** as of 2026-07-31; the throwaway `usersh` is gone) and `desktop-shell` for
the graphical one. **Two sessions run concurrently and neither arbitrates** (M7 Part D), which
is what keeps serial the recovery path by construction.

**Servers never register themselves.** In every handshake above, a supervisor holding
`BIND_NAMESPACE` does the binding — see
[why supervisor registration](../rationale/why-supervisor-registration.md).

## The emergency path

Failure on the critical path — no usable manifest, a mount that will not come up, a
required system server that will not bind — drops to `eshell`, a minimal interactive shell
bundled in the initramfs with enough capability to inspect block devices, edit `init.toml`
and reboot. Recovery from a misconfigured boot does not need a rescue USB.

`eshell` deliberately keeps `kprint` and talks to the raw console device rather than the
tty server: its whole precondition is that the normal path failed. See
[console and tty](console-and-tty.md) § "`eshell` is separate, and has to be".

## Where this is verified

| | What it proves |
|---|---|
| `cargo xtask test-qemu` | The whole boot to userspace, headless; the guest writes a verdict to `isa-debug-exit` and a hang is caught by a wall-clock timeout. Runs under **KVM** — the kernel is x2APIC-only and QEMU 8.2's TCG does not emulate x2APIC. |
| `cargo xtask test-interactive` | The login chain end to end over the serial console, expect-driven: the login prompt, a rejected password, a successful login, and shell behaviour after it. |
| `cargo xtask check-fbcon` | The same boot with **no serial port at all** (`-serial none`), read back off the screen: the kernel's first and last lines, userspace's up to the compositor, the hand-over, and a panic taking the screen back. |
| `cargo xtask check-live` | The **live image** booted as a USB stick with no disk: the menu's countdown boots its default entry with no command line, the root module becomes a RAM disk, `init` mounts and reads through it, the greeter comes up within a bound a tick-bound RAM disk cannot meet, and a serial login writes under `/home`. |
| `cargo xtask check-install` | The live image's **installer entry**, on demand rather than in CI: Limine's menu found and its *third* entry chosen, the `install-esp.img` module that entry alone loads becoming a block device, the disk identifying itself, the four devices a session and then the shell hand on, a graphical login, a terminal from the applications modal, and `nxinstall` typed at the shell in it — then **that disk booted on its own**, mounting `gpt-partlabel:nitrox-root` and reaching a greeter with no stick attached. |
| `cargo xtask check-report` | The live image's **hardware report**, with no serial port: Limine's menu found and its second entry chosen, each report page read off the screen and a key pressed for the next, the facts that machine has asserted from the pages — a declined AHCI controller, the module disk, no UART — and the boot going on to the hand-over. `test-qemu` asserts the facts its own boot has from the transcript: the tables, CPUs and IOAPIC, a claimed AHCI controller, COM1 present. |

See [qemu integration tests](../conventions/qemu-integration-tests.md).

## History

This document previously described the boot as a four-phase plan with Phases 1–3 unbuilt.
Those phases are complete; the plan itself is preserved in
[`docs/planning/`](../planning/implementation-plan.md) (`phase-0-foundation.md` through
`phase-3-service-ecosystem.md`), which is where the historical sequencing belongs. The
decision log records the reasoning behind individual steps.
