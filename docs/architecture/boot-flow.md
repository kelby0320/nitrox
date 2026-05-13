# Boot Flow

This document describes how a Nitrox machine progresses from power-on to a
running system. The flow is split into phases that map onto the project's
implementation roadmap; in pre-v0.1, only Phase 0 (foundation boot) is
implemented — later phases are described aspirationally and will be filled
in as they are built.

For exact contracts (Limine protocol layout, syscall ABI, etc.), see the
corresponding `docs/spec/` document. For the rationale behind specific
choices, see `docs/rationale/`.

---

## Overview

```
UEFI firmware
  └─► Limine (BOOTX64.EFI)              ◄── Phase 0 implemented
        └─► Nitrox kernel _start
              └─► kernel_main
                    └─► framebuffer text + halt loop

  ── Phase 1+ (deferred) ──
        ├─► IDT, exception model
        ├─► Physical/virtual memory management
        ├─► Handle table, IPC, scheduler
        ├─► In-kernel resource servers (/proc, /dev, /initramfs)
        └─► PID 1 (init) spawned from initramfs
              └─► fs-servers, service-mgr, session-mgr
                    └─► login, user shell
```

---

## Phase 0 — Foundation boot (implemented)

Goal: get a recognisable boot indicator on the framebuffer, prove the
toolchain, prove the Limine integration, prove the higher-half load.

### 1. Firmware: UEFI loads Limine

QEMU is launched with OVMF as `pflash`. UEFI scans removable media,
finds an EFI System Partition (GPT type `EF00`) on the virtual disk,
and loads `EFI/BOOT/BOOTX64.EFI` from the FAT32 inside. That binary is
Limine's UEFI loader, vendored under `tools/build-cache/limine/` by
`tools/xtask`.

Disk image layout (built by `cargo xtask image`):

```
nitrox.hdd (64 MiB raw, GPT)
└── partition 1 (EFI System, FAT32, "NITROX_ESP")
    ├── /EFI/BOOT/BOOTX64.EFI         ← Limine v12.2.0
    ├── /boot/limine/limine.conf      ← vendored from boot/limine.conf
    └── /boot/kernel                  ← our ELF
```

### 2. Limine reads `limine.conf` and finds the kernel

The config — `boot/limine.conf` — names one entry pointing at
`boot():/boot/kernel`. Timeout is 0 so the entry boots immediately.

Limine then loads the kernel ELF, scans the binary for our request
statics (the `.limine_requests` bracketed region — see
`kernel/linker.ld` and `kernel/src/main.rs`), sets up:

- 64-bit long mode, with 4-level paging
- A higher-half kernel mapping anchored at `0xffffffff80000000`
- A higher-half direct map of physical memory (HHDM — declared but not
  yet consumed by Phase 0)
- The framebuffer (linear, 32 bpp, driven by Limine's response struct)
- A 64 KiB stack in bootloader-reclaimable memory
- A bootloader GDT with `CS=0x28`, `DS=0x30`
- `RFLAGS.IF = 0` (interrupts disabled)

…and jumps to our ELF entry, `_start`. Per the Limine protocol the
return address pushed onto the stack is zero; the kernel must not
return.

### 3. Kernel `_start` → `kernel_main`

`kernel/src/main.rs` declares four statics in the `.limine_requests*`
sections (linked in by `kernel/linker.ld`):

- `BASE_REVISION` — `BaseRevision::new(6)`, the protocol revision we
  require. Limine zeroes its inner `revision` field if it supports the
  requested revision; we check this before trusting anything else.
- `FRAMEBUFFER_REQUEST` — `FramebufferRequest::new()`; the response
  pointer is populated by Limine before jump.
- `REQUESTS_START` / `REQUESTS_END` — markers that bracket the request
  region for fast scanning, mandatory under base revision 6.

`kernel_main`:

1. Verifies `BASE_REVISION.supported()`. If false, halt.
2. Reads `FRAMEBUFFER_REQUEST.response`; bails if null.
3. Dereferences the first `Framebuffer` in the response array.
4. Builds an `FbWriter` over its linear mapping (Phase 0 only handles
   32 bpp framebuffers — anything else means halt).
5. Clears the framebuffer to a dark slate (`Rgb::BG`).
6. Writes `NITROX KERNEL` in cyan, then `PHASE 0: BOOT OK` in white,
   using the hand-coded 8x16 font in `kernel/src/font.rs`.

Then control returns to `_start`, which calls `arch::halt_loop` — a
`cli; hlt` loop. The kernel halts cleanly with output visible.

### 4. End of Phase 0

No interrupts have been enabled. No IDT exists. No allocator has been
brought up. The kernel is intentionally a brick that *displays a sign*.

---

## Phase 1 — kernel core (deferred)

Brings up the structures that turn the brick into a kernel.

1. **CPU plumbing.** Replace Limine's GDT with our own. Install an IDT,
   route hardware exceptions to handlers, set up the IST stacks for
   double-fault and NMI.
2. **Physical memory.** Walk Limine's memory-map response, mark
   `bootloader-reclaimable` regions for later release, hand `usable`
   regions to the buddy allocator.
3. **Virtual memory.** Build the kernel-half page tables we own (rather
   than relying on Limine's transient ones), set up the slab allocator
   over the buddy, define the VMA tree shape for processes (even though
   no process exists yet).
4. **Interrupts.** Set up the LAPIC, the I/O APIC, a timer source. Allow
   the `cli; hlt` to be replaced with `sti; hlt` once the IDT is sound.
5. **Logging.** Promote the framebuffer console to a real kernel log
   sink. Add a serial backend (writes also go to QEMU's `-serial` for
   automated tests).

After Phase 1, the kernel can execute code in response to interrupts
and own its own memory.

---

## Phase 2 — capabilities, IPC, the first process (deferred)

1. Handle table (segmented), kernel object framework, `Rights` checks.
2. Namespace engine and in-kernel resource servers:
   `/proc`, `/dev`, `/initramfs`, `/dev/framebuffer`.
3. IPC channels and notification queues.
4. Scheduler (per-CPU runqueues, three classes — RealTime/TimeShared/Idle).
5. Spawn `init` (PID 1) from the initramfs with the initial handle set
   and full system capabilities.

After Phase 2 the kernel has a userspace process running.

---

## Phase 3 — userspace boot (deferred)

Init reads `/etc/init.toml` from the initramfs and processes critical-path
mounts in dependency order, spawning fs-server instances and binding their
endpoints. From there it spawns the service manager, which brings up
logging, audit, device manager, package manager, session manager, and
eventually the session manager presents login.

Failure at any critical-path stage drops into `eshell`, the emergency
shell. See the (yet-to-be-written) `emergency-recovery.md`.

---

## State at each phase boundary

| State                            | Phase 0 | Phase 1 | Phase 2 | Phase 3 |
|----------------------------------|:-------:|:-------:|:-------:|:-------:|
| Long mode + paging               |   ✓     |   ✓     |   ✓     |   ✓     |
| IDT, exception handling          |         |   ✓     |   ✓     |   ✓     |
| Physical-memory bookkeeping      |         |   ✓     |   ✓     |   ✓     |
| Allocator (buddy + slab)         |         |   ✓     |   ✓     |   ✓     |
| Interrupts enabled, timer        |         |   ✓     |   ✓     |   ✓     |
| Handle table, kernel objects     |         |         |   ✓     |   ✓     |
| Namespace + in-kernel RS         |         |         |   ✓     |   ✓     |
| IPC, notifications, scheduler    |         |         |   ✓     |   ✓     |
| PID 1 (init) running             |         |         |   ✓     |   ✓     |
| fs-servers, service-mgr          |         |         |         |   ✓     |
| Session manager, user shell      |         |         |         |   ✓     |
