# Boot Flow

**Status:** Current. Describes the boot as it runs today — UEFI → Limine → kernel → `init`
→ fs-server → `service-mgr` → `auth-service` → `session-mgr` → login → `nxsh` (Phases 0–3
complete, 2026-07-21). Every stage below is exercised on each CI run by
`cargo xtask test-qemu` (headless, adjudicated by `isa-debug-exit`) and
`cargo xtask test-interactive` (expect-driven over the serial console).
Verified against source 2026-08-21, when the filesystem checks moved out of `init` (the display arm's position in § 5 changed that day).

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

Limine loads the kernel ELF, scans it for our request statics (the `.limine_requests`
bracketed region — see `kernel/linker.ld` and `kernel/src/main.rs`), and sets up:

- 64-bit long mode, with 4-level paging
- A higher-half kernel mapping anchored at `0xffffffff80000000`
- A higher-half direct map of physical memory (HHDM)
- The framebuffer (linear, 32 bpp, driven by Limine's response struct)
- A 64 KiB stack in bootloader-reclaimable memory
- A bootloader GDT with `CS=0x28`, `DS=0x30`
- `RFLAGS.IF = 0` (interrupts disabled)

…and jumps to our ELF entry, `_start`. Per the Limine protocol the return address pushed
onto the stack is zero; the kernel must not return.

**The Limine bindings are hand-rolled** `#[repr(C)]` types in `kernel/src/limine.rs`, not
the `limine` crate — the kernel takes no external crates. Bump `LIMINE_VERSION` in `xtask`
and the bindings together.

## 3. Kernel `_start` → `kernel_main`

`kernel/src/main.rs` declares six request statics plus the two bracketing markers, linked
into `.limine_requests*` by `kernel/linker.ld`:

- `BASE_REVISION` — `BaseRevision::new(6)`, the protocol revision we require. Checked
  before anything else is trusted.
- `FRAMEBUFFER_REQUEST`, `MEMMAP_REQUEST`, `HHDM_REQUEST`, `MODULE_REQUEST`, `SMP_REQUEST`
- `REQUESTS_START` / `REQUESTS_END` — mandatory under base revision 6.

All but `BASE_REVISION` are `static mut`: Limine writes their `response` field after the
binary is loaded, and a plain `static` would let rustc constant-fold the read.

`kernel_main` then brings the system up in this order — the ordering is load-bearing, and
each step's rationale is in the source comments:

1. **Serial first.** It touches only fixed I/O ports, so every later step can report
   progress *and failure* to the console before anything else exists.
2. **CPU tables** — GDT + TSS, then IDT (`arch::Cpu::init_tables`).
3. **Memory** — walk Limine's memory map, bring up the buddy allocator and the slab over
   it. This is the first code to walk firmware structures and the first place a fault can
   happen, which is why the IDT is already live.
4. **Paging** — `paging_init` enables NX and captures the kernel-half PML4 template every
   future `AddressSpace::new` inherits. Must precede any address-space construction.
5. **initramfs** — register the Limine-loaded module so the `/initramfs` resource server
   can serve it. Needs the HHDM.
6. **Platform discovery** — ACPI on x86_64: the PCIe ECAM window and the interrupt-routing
   topology. Missing or malformed tables are logged, not fatal.
7. **Local APIC** (x2APIC), then **TSC + LAPIC timer calibration** against the legacy PIT.
8. **DPC queue**, then the **interrupt router** (IOAPIC).
9. **Global handle table.**
10. **Scheduler** (`sched_bringup`), then **AP bring-up** (`bring_up_aps`) via Limine's SMP
    response — capped at `MAX_CPUS`, extras left parked (a supported configuration, not a
    failure). Absent an SMP response the system stays single-CPU. **Fatal** since 2026-08-19
    if a CPU we launched fails to come online within 5 s, whether it faulted on the way in or
    never reached our code: the kernel's view of the machine must match the machine, so it
    stops rather than booting a topology nobody chose. See `docs/decision-log.md`.
11. **Display aperture** — record Limine's framebuffer (physical base, geometry, channel
    layout) so `/dev/framebuffer` can serve it. Must precede the next step, which binds
    that path into init's namespace.
12. **Boot screen**, then the first userspace process.

## 4. The first userspace process

`run_first_userspace` arms the syscall fast path, then builds pid 1 by hand — this is the
one process nothing else can construct:

- Allocate an address space; load `/sbin/init` from the initramfs (halt if absent).
- Allocate the process and a notification channel, and a handle to it.
- Allocate a namespace and bind the initial set: `/dev/entropy`, `/dev/console`,
  `/dev/log`, `/proc/self/*`, `/proc/sched/stats`, `/initramfs`, `/dev/blk`, and
  `/dev/framebuffer` (the display aperture, plus its `info` leaf — recorded at step 11 of
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
   and bind that endpoint at the mount point. Any failure → the emergency path.
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

Under the `selftest` / `test-harness` builds init additionally runs the **demo chain**
(`run_test_harness`) after step 5 and before handing off, and a non-zero exit fails the run
there. It also spawns the display self-test and the graphical test clients. Those are the
last things in this file that a release build does not do — retrofit Part C2 turns them into
service declarations.

**The filesystem checks are no longer init's** (retrofit Part C1, 2026-08-21): the large-file
read, overwrite, grow, create and subtree-bind checks moved to `boot-probe`, a declared
service `service-mgr` starts, so they run *after* the step-5 handoff rather than between
steps 2 and 3. They also gate the boot verdict now, which they never did here — every failure
path in init was a bare `return` after a `FAIL` print. `init` keeps one thing they need: the
`/subtreetest` binding in `mount_one`, which cannot become declaration data because nothing
in a declaration can express a namespace bind.

**Who fires the verdict** is `boot-probe`, not init; init only ever fires FAIL, for a
critical-path boot failure or a crashed demo chain. See
[`qemu-integration-tests.md`](../conventions/qemu-integration-tests.md).

## 6. service-mgr → session-mgr → login

`service-mgr` reads service declarations, constructs each service's namespace and handle
set, spawns it and supervises it. On the login path specifically:

1. **`auth-service`** — spawn + `Ready` handshake, yielding the client channel.
2. **`session-mgr`** — spawned with re-delegated `BIND_NAMESPACE`, then handed the
   fs-server endpoint and the auth channel.

`session-mgr` presents login, authenticates against `auth-service`, constructs the session
namespace, and spawns `nxsh` into it with empty syscaps. **`nxsh` is the login leaf** as of
2026-07-31; the throwaway `usersh` is gone.

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

See [qemu integration tests](../conventions/qemu-integration-tests.md).

## History

This document previously described the boot as a four-phase plan with Phases 1–3 unbuilt.
Those phases are complete; the plan itself is preserved in
[`docs/planning/`](../planning/implementation-plan.md) (`phase-0-foundation.md` through
`phase-3-service-ecosystem.md`), which is where the historical sequencing belongs. The
decision log records the reasoning behind individual steps.
