# Deferred Decisions

This document tracks design decisions that have been deliberately deferred — known concerns that aren't being addressed in the initial implementation, with the reasoning for why deferral is acceptable. The goal is to make the deferrals explicit rather than implicit, so that future readers (and future-you) understand what's been knowingly omitted versus what's been overlooked.

There's a distinction between **non-goals** (things that won't be done at all) and **deferrals** (things that will be done, but not now). This document covers both, marking which is which.

For the reasoning behind specific architectural choices, see the topical rationale documents (e.g., `why-capabilities.md`, `why-no-signals.md`). This document covers what *isn't* being built, where the previous documents cover what *is*.

## Non-goals (permanent)

These are not going to be done. Architecture is structured to not require them.

**POSIX compatibility as a primary goal.** Nitrox does not aim to be a POSIX-compliant operating system. Programs written for POSIX do not, in general, work on Nitrox without modification. A POSIX compatibility shim may be added later as a pragmatic concession for ported software (see deferrals below), but it's not a constraint on the native interface design. Native Nitrox programs use the handle-based interface with typed data, async-first I/O, and capability discipline.

**Global ambient authority (UID/GID).** Authority is held in handles, never derived from process identity. There is no "user" concept at the kernel level. The session manager and authentication service handle the human-facing user model, but the kernel doesn't know what a user is.

**Unix signals.** The notification queue replaces signals. See [`why-no-signals.md`](why-no-signals.md). There's no plan to add signals later.

**A global VFS tree in the kernel.** The kernel does not maintain a global mount table or filesystem dentry cache. Per-process namespaces and resource servers replace this entirely. There's no plan to add a kernel VFS later.

**A monolithic kernel with filesystem code inside it.** Filesystems are userspace. There's no plan to move them into the kernel.

**Synchronous syscalls that block.** Every potentially-blocking operation returns a `PendingOperation` handle. See [`why-async-syscalls.md`](why-async-syscalls.md).

**KPTI / Meltdown-class speculative execution mitigations.** Nitrox is a hobby OS where the security model is capability-based sandboxing, not "protect against speculative execution side channels in untrusted userspace running on untrusted hardware." This is a reasoned choice for the project's scope, not an oversight. Adding KPTI would significantly impact syscall performance and complicates the higher-half kernel design.

**Legacy (pre-2014) x86 hardware.** There is no requirement to run on old machines. The kernel already requires SMEP and SMAP (it enables and asserts them; the dev loop passes `+smep,+smap`) — SMAP is Broadwell, so the de-facto x86 floor is **≈ 2014**. The baseline is roughly **x86-64-v2 ISA plus SMEP/SMAP**; on any CPU meeting it, an invariant TSC and x2APIC are also guaranteed. This is a deliberate "no legacy" scope choice, not an oversight, and it is what lets the kernel assume modern features rather than carry fallback paths for ancient hardware. (BIOS/legacy-boot is separately out of scope — the project is UEFI + Limine only.)

## Open deferrals

These will eventually be done, but aren't in initial scope. Each entry documents what's
deferred and what triggers it. **Everything below is still open** — resolved entries move
to the [Resolved](#resolved-kept-for-the-record) table at the bottom, so this section can
be read as the list of what is actually owed.

### Hardware support

**aarch64.** The architecture abstraction layer is designed in from the start. Every arch-specific concern (paging, interrupts, FPU, user memory access, power) is behind a trait. The `kernel/src/arch/aarch64/` directory exists as stubs. Initial implementation targets x86_64; aarch64 implementation comes after the x86_64 system is mature. Trigger: when there's a specific aarch64 target system to support, or when the x86_64 implementation is stable enough to make the porting effort worthwhile.

**5-level paging on x86_64 (57-bit virtual addresses).** Ice Lake and later support 5-level paging, allowing virtual address spaces up to 128 PiB. Nitrox uses 4-level paging (canonical 48-bit). The address space is plenty for any conceivable workload. Trigger: a use case that requires it. None foreseen.

**KASLR (kernel image ASLR).** The kernel image is loaded at a fixed higher-half address. User-space ASLR is implemented (28 bits of entropy for ELF, stack, and mmap arena). Kernel ASLR is a defense-in-depth measure against kernel-mode exploits. Not initially. Trigger: a security hardening pass after the system is mature.

### Power management

**Phase 2 ACPI (ACPICA integration via FFI).** Phase 1 (pure Rust ACPI table parsing, no AML) ships first. ACPICA integration is deferred until needed. See [`why-phased-acpi.md`](why-phased-acpi.md). Triggers: laptop targeting, graceful S5 shutdown requirement, AML-only device support, CPU power management beyond `hlt`, GPE handling.

**Full laptop power management.** Battery, lid switch, thermal zones, AC adapter — all gated on Phase 2 ACPI.

**CPU C-state and P-state management.** Power saving via deep CPU sleep states and frequency scaling. Gated on Phase 2 ACPI.

**Suspend-to-RAM (S3), hibernation (S4), runtime device power management.** All gated on Phase 2 ACPI plus additional design work specific to each.

### Kernel module infrastructure

**Module signing.** LKMs are not cryptographically signed in the initial design. `SysCaps::LOAD_MODULE` is required to load modules; the capability is the access control. Signing would add a trust hierarchy on top. Trigger: production deployment scenarios where unsigned modules are unacceptable. Not foreseen for hobby use.

**ABI-compatible module loading across kernel versions.** Modules must be rebuilt against the running kernel. The build-hash ABI version refuses cross-version loading. ABI compatibility would impose stable interface constraints on the kernel that limit evolution. Trigger: a stable kernel release where ABI compatibility is worth the constraint.

**Live kernel patching / hot upgrade.** Not in scope.

### Drivers and interrupts

See `docs/architecture/drivers-and-irps.md` for the framework these defer from.

**Tier 2 (runtime-loadable) drivers.** Phase 2 ships only Tier 1 drivers
(compiled into the kernel ELF via Cargo features: `pci`, `ahci`, `gpt`). The
userspace driver manager — matching `DeviceNode`s to loadable modules and
handing a driver process a `Handle<DeviceNode>` — needs the kernel-module
loader (`export!` table, ELF relocation, ABI-hash enforcement) which is itself
deferred (see "Kernel module infrastructure" above). Trigger: hot-pluggable or
optional hardware that isn't on the boot path.

**MSI / MSI-X (message-signalled interrupts).** Phase 2 routes device
interrupts through the IOAPIC (legacy line interrupts), which is sufficient for
the QEMU AHCI controller. MSI/MSI-X (and the per-vector affinity they enable)
land when a device needs them. Trigger: NVMe, multi-queue NICs, or performance
work on interrupt-heavy devices.

**A dedicated arch trait for the device-interrupt *installation* facility.**
`install_pci_irq` (the composite that registers a handler in the arch vector
table + routes a GSI to it — Part 3) is currently a **neutral free function**
(`crate::arch::install_pci_irq`), not a method on `ArchIrqRouter`: it spans three
hardware abstractions (the handler registry, the local controller, and the
router) and belongs to none. When the device-interrupt *family* grows a second
member — **MSI/MSI-X install**, **shared-INTx chaining**, or **IRQ teardown**
(Tier 2 module unload drains + unhooks an IRQ) — promote the family into its own
arch trait (e.g. `ArchIrqInstall`), distinct from `ArchIrqRouter` (pure routing)
and `ArchIrq` (the local controller). One method + one consumer today does not
justify the trait (the project builds an abstraction at its second consumer); the
`TODO(msi)` on the function marks the trigger.

**Shared PCI INTx interrupt chaining.** The "chain of handlers, each returning
*mine* / *not mine*" model for shared legacy interrupt lines is deferred; Phase 2
assumes each handled GSI has one owner. MSI/MSI-X are never shared, so this only
matters for legacy INTx sharing. Trigger: real hardware where INTx lines are
shared across functions.

**IOMMU programming and userspace drivers.** Granting a `DeviceNode` /
`InterruptObject` to a userspace driver process safely requires programming the
IOMMU (VT-d / AMD-Vi / SMMU) to constrain the device's DMA to memory the driver
legitimately holds. Phase 2 has only in-kernel drivers, so neither the IOMMU
programming nor userspace driver hosting is built. Trigger: a userspace driver
(e.g. a userspace NIC or GPU driver).

**IRP cancellation and the completion timeout.** The IRP framework lands without
request cancellation or the 30-second force-complete timeout. Phase 2 stacks are
shallow and the boot-path block driver completes promptly. `sys_io_cancel` is
defined (number reserved) but returns `Unsupported` until this lands. Trigger:
long-running or cancellable I/O (network, user-abortable operations) and Tier 2
module unload (which drains in-flight IRPs).

**Async-I/O surface subset.** The [`IoOp`](../spec/io-operation.md) descriptor
ships with only `Read`/`Write` opcodes and no `flags` modifiers; `Flush`/`Trim`,
force-unit-access / no-cache flags, and multi-buffer scatter/gather in one `IoOp`
are deferred to their first consumer (RW filesystems, SSD trim). The block
namespace ships **enumeration-order** whole-disk names (`/dev/blk/0..`);
content-stable `/dev/disk/by-partuuid/*` / `by-partlabel/*` names are slice 6
(they need GPT metadata). The `/dev/blk` binding is **read-only** in Phase 2
(RO `fs-server-ext4`); RW block access lands with RW filesystems (Phase 3).

**Filter drivers.** Transparent insertion of a driver into a stack (encryption,
compression, logging, LUKS, LVM) is part of the IRP design but unimplemented.
Phase 2 has single- and two-layer stacks only (AHCI; GPT-over-block). Trigger:
the first filter use case (encrypted root / LVM, both already deferred under
"Filesystems").

**NVMe.** Phase 2's first storage driver is AHCI (simpler than NVMe). The `nvme`
Tier 1 feature follows. Trigger: NVMe hardware or a faster boot device matters.

**AHCI driver scope.** The Phase 2 AHCI driver (Part 3) supports a **single
controller, single SATA disk, one command *issued* at a time** (slot 0). Multi-port /
multi-disk, multiple controllers, and port multipliers are deferred to when a
configuration needs them. It resolves the controller's GSI from the **PCI
interrupt-line register** (firmware-programmed on QEMU); proper ACPI `_PRT` routing
(which needs AML) is deferred — see `device-node.md`. The read self-test brings up
against the existing AHCI boot disk; the dedicated `xtask build-disk` + ext4 test disk
arrive with the fs-server (slice 7).

> **Concurrent submits are queued, not dropped (2026-07-20).** The driver drives one
> command at a time, but *concurrent* block submits from different clients (a page-fault
> fill, a `sys_io_submit`, another CPU) are now serialised through a small software FIFO
> in front of slot 0 (`PendingRing`, `IrqSpinLock`-guarded) — a second submit while slot 0
> is busy queues and issues when the slot retires, rather than clobbering the in-flight
> IRP (which orphaned its waiter → a hang). This was a real correctness bug found bringing
> up the auth/session login chain concurrently with the demo chain. **Full NCQ — letting
> the controller run up to 32 commands at once across the command list — remains a future
> item** (the queue depth is already `PENDING_DEPTH = 32`, so the software queue converts
> to NCQ slots cleanly when we build it; the trigger is a workload that is I/O-latency
> bound, e.g. an SSD or many concurrent readers).

**Concurrent same-page faults.** When a file page fault misses, the fault path reserves
the frame (`Loading`), starts the producer fill, and parks the faulting thread on a
per-*fault* `PendingOperation`. A *second* thread faulting the **same** page has no handle
on that in-flight fill's PO, so it `yield_now`s and retries until the page is `Ready` — a
spin, not a block. This was unreachable when written (single CPU, one faulter per
`FileObject`); **under SMP it is reachable today**, and `std::thread` makes it ordinary.
The fix — store the fill PO in the cache page so a second faulter blocks on it (one wakeup,
no spin) — is scheduled as **B3 of the pre-CLI substrate-hardening pass**
(`docs/planning/phase-4-desktop.md`).

**Kernel log buffer is keep-early, not keep-recent (slice 9 Part 5).** `klog`
(`/dev/log`) is a **linear append** buffer: it captures kernel `kprint!` output from
boot until its 16 KiB fills, then drops later output. This keeps the early boot /
failure context (what an emergency inspection wants) and comfortably holds a full
boot log. The trade-off: on a long-running or verbose system it stops capturing
recent messages — the opposite of what `dmesg` usually wants for "what just happened."
A keep-recent **ring** (overwrite-oldest, linearised on snapshot) is the refinement;
deferred until the system runs long enough to overflow 16 KiB (eshell/services beyond
boot). The snapshot fill (`copy_into_frames`) already handles the segmented copy a
ring would need.

**AHCI NCQ (multiple in-flight commands).** The driver issues **one command at a time**
(slot 0). The *contention* this used to cause is resolved — concurrent submits from
different clients queue in a software FIFO in front of slot 0 (`PendingRing`, 2026-07-20)
instead of clobbering the in-flight IRP, and the demo/login sequencing that worked around
it has been lifted. What remains is **throughput**: letting the controller run up to 32
commands at once across the command list. The software queue's depth is already
`PENDING_DEPTH = 32`, so it converts to NCQ slots cleanly. Trigger: an I/O-latency-bound
workload (an SSD, or many concurrent readers).

**Stateless `File::ReadRange` fill (slice 8 Part 3).** A page-cache fill names its
file by re-sending the path `suffix` on every `ReadRange` (the same suffix the lazy
`Resolve` used), so the fs-server re-resolves the path per fill rather than handing
back an open-file cookie at resolve time. Simple and correct for the milestone; the
re-resolve cost hides behind the IPC round-trip. A server-side open-file handle
(resolve returns a cookie; `ReadRange` carries it) is the obvious Phase-3
optimization — defer until a profiling case or a stateful fs (RW, where the open
handle anchors writeback) forces it. See `docs/spec/rsproto-file-ops.md`.

**Page-cache scope (slice 8).** The first file page cache (slice 8, the **Model-B**
range-read fill — see the decision log, 2026-06-25) is deliberately minimal on three
axes. **(1) Per-file, not global.** Each `FileObject` owns a sparse page table; two
processes that independently resolve the same path get separate caches. Global,
inode-keyed sharing (one physical page shared across every mapping of a file) needs a
stable file identity the fs-server exposes and is deferred — trigger: a workload that
maps the same file hot from many processes. **(2) No eviction/reclaim.** The cache
grows to the mapped extent and is freed only on unmap / `FileObject` drop; the
clock-algorithm reclaim daemon + `Notification::MemoryPressure` is Phase 3+ — trigger:
caches that can grow past comfortable bounds (large files, many mappings). **(3)
Stateless fill protocol.** `File::ReadRange(suffix, …)` re-sends the path suffix per
range and the fs-server re-resolves suffix→inode each time (cacheable internally); a
stateful `file_id` / open-file table + a `close` op is a later optimization — trigger:
per-fault re-resolution showing up in profiles. The page cache is built behind a
**fill-producer seam** so the **Model-A** extent fill (Phase 3, zero-copy block reads)
slots in *alongside* `ReadRange` without a redesign.

**Demand-fault fill latency — single-page fills, no read-ahead (slice 8; measured
slice 9).** The kernel fills **exactly one 4 KiB page per fault**
(`FileObject::fault_in_page` reserves a single index), and each fill is a full
stateless round-trip: park the faulter → `File::ReadRange` IPC → the fs-server
re-reads the superblock, re-resolves the path, and re-walks the extent tree from
disk (~6–8 emulated AHCI reads) → reply → fill → resume. Measured under QEMU at
**~325 ms per page**; the 64-page `large.bin` milestone fixture made boot a ~20 s
silent wait. Two independent Phase-3 levers close this, and they compose:
- **Kernel read-ahead (clustered fill)** — on a `FileBacked` fault, fill a cluster
  of pages in **one** `ReadRange` (e.g. the rest of the file capped at N pages),
  turning *pages* round-trips into *⌈pages/N⌉*. The single biggest lever; it is the
  natural completion of the slice-8 page cache and needs only the fault path +
  `reserve`/`start_fill` to span a page range (the wire op already carries
  `offset`/`len`).
- **fs-server open-file cookie** — the stateless-fill entry above; makes each
  `ReadRange` O(1) instead of a full re-resolve.

Deferred to Phase 3 rather than pulled forward: both are optimizations of a path
that is *correct* today, and the milestone only needs to **prove** multi-page
demand-faulting, not do it fast. As a stopgap the fixture was trimmed **64 → 8
pages** (still spans 8 position-sensitive pages; boot ~2.8 s instead of ~20 s) — see
the decision log (2026-06-26, Phase 2 close). Trigger to implement: the first
workload that demand-pages a genuinely large file on the boot path, or this latency
showing up in a profile. (Note: read-ahead also multiplies the per-fill disk I/O,
which interacts with the AHCI single-command limit above — both want the same
Phase-3 storage-hardening pass.)

### Networking

**TCP/IP networking.** The architecture is committed: userspace netstack server, network drivers as Tier 1 or Tier 2 modules, sockets as namespace resources. Implementation is deferred. Trigger: a concrete need (wanting to SSH into the system, wanting to download files, etc.). Implementation is a major effort (~15-50K lines depending on whether smoltcp is ported or a stack is written from scratch); deferring keeps the initial system simple while not foreclosing the work.

**Network booting (PXE) by the kernel.** Limine handles PXE before the kernel runs. The kernel itself doesn't need network for PXE. Network-mounted root filesystems can use the same userspace fs-server architecture as local mounts; this is gated on the netstack being implemented.

### Graphics

**GPU driver and compositor.** Architecture is sketched (GPU driver as Tier 2 LKM, compositor as userspace server, client-side rendering, Wayland-influenced protocol). Specific compositor protocol, 3D acceleration scope, window management model — all deferred. Trigger: when the project wants a GUI. Pre-compositor mode (`/dev/framebuffer` as a kernel resource server) is sufficient for early userspace, debug UI, and kernel panic screens.

**Specific compositor/client protocol.** Deferred along with the compositor itself. Likely Wayland-derived but using the resource-server protocol as the wire format. Decision when compositor work begins.

**3D acceleration, OpenGL/Vulkan equivalents, GPU compute.** All deferred. Initial scope is 2D framebuffer rendering.

**Text rendering, fonts, input methods, accessibility.** Downstream of the compositor.

### Filesystems

**Read-write FAT.** Initial FAT support is read-only. The ESP rarely changes after install; reading it is sufficient. Trigger: a need to update the bootloader from within the OS, or some other ESP-write workflow.

**Bulk directory creation is O(N²) block reads.** `dir_insert` scans every existing block
of a directory for a record with enough slack before appending a new block, and the server
caches nothing between calls — so filling a directory with N entries re-reads its blocks N
times. Fine at the scales anything currently does (16 entries costs ~2 s of a TCG boot),
but it is quadratic: a sweep of 40 entries plus a lookup per entry pushed a `test-qemu` run
past its 90 s ceiling (2026-07-29), which is what sized the in-guest check down to
crossing the block boundary rather than stressing it. The fixes are the standard ones —
an htree directory index, or simply a per-directory "first block with room" hint carried
across inserts. Trigger: bulk directory creation on a real path — `copy -r` of a large
tree, or unpacking a package into the store.

**ext4 write-path gaps (`fs-server-ext4`).** The write path is deliberately partial, and
these are the pieces still missing. They were previously recorded only in the crate's
`CLAUDE.md` — which is precisely how three other deferrals went unreviewed (the 2026-07-24
audit) — so they are mirrored here.

- **Extent-tree splitting / index nodes (depth > 0).** `i_block` holds four inline leaf
  extents; a file or directory needing a fifth non-contiguous extent gets `Unsupported`.
  Measured boundary on 4 KiB blocks (2026-07-29): creating **files** in one directory is
  unbounded in practice (2000+ tested — the parent's growth blocks stay contiguous, so one
  extent covers them), while creating **subdirectories** stops at **~814**, because each
  `mkdir` allocates the child's own block between the parent's and so fragments it. Both
  are far past anything the shell or desktop needs. Trigger: a directory or file that
  genuinely needs a deeper tree — very large files, or a directory of thousands of
  subdirectories.
- **Cross-group inode/block allocation.** Creation is group 0 only. Trigger: a filesystem
  large or full enough that group 0 runs out.
- **`metadata_csum` checksums** and **jbd2 journaling + replay.** The fixtures are built
  `^has_journal`; a crash mid-mutation is not recoverable by replay. Trigger: running on
  media where an unclean shutdown matters.

**btrfs, NTFS, XFS, ZFS, etc.** Each is a userspace fs-server binary. None are in initial scope. Trigger: specific deployment needs.

**Encrypted root (LUKS).** Architecture accommodates this — LUKS is a block device filter driver in initramfs; init invokes it before spawning fs-server. Not in initial scope. Trigger: encrypted-root deployment.

**LVM / software RAID at early boot.** Same architectural accommodation as LUKS. Initial scope is direct partition mounts.

**`mtime` on an in-place overwrite.** The fs-server stamps inodes on create, grow, mkdir,
unlink, rmdir and rename — every operation that passes through it. It is **not** told about
a plain overwrite: under Model A the kernel writes file data straight to the device, so
editing a file's existing bytes leaves its `mtime` at the last size change. This is the
timestamp gap users actually notice ("I saved the file and the date didn't change"). Needs
a writeback notification to the server — the natural hook is `sys_file_sync`, which already
crosses into the kernel with the file's identity in hand, though a notification per sync
has its own cost to weigh. Trigger: a text editor, or anything that rewrites a file in
place without changing its length.

**Runtime reconfiguration of critical-path mounts.** Currently requires reboot through eshell. Live remounting of `/`, `/home`, etc., is not supported. Trigger: deployment scenarios where it matters.

### Userspace

**Shell grammar specification.** The shell's data model is committed (typed structured streams, port-based wiring, the display verb, model-view decomposition). The exact syntax is deferred to shell implementation. Trigger: when shell implementation begins.

**`std` port for Nitrox target (now a Phase 4 target, 2026-07-20).** Reframed from "deferred
indefinitely" to a serious, faithful compatibility target — the portable API for *application*
code, riding the native ABI (libos/libstream stay the capability-native API for system code).
`std::fs` resolves paths through the process's root namespace (bounded ambient, capability-safe);
`std::io` blocking maps to `sys_io_submit` + `block_on`; the kernel stays pure. Placement: **FP/AVX2
+ XSAVE lands early** (also unblocks `no_std + alloc` ecosystem crates); the **full cluster**
(thread-local storage, real `std::thread` → the slice-3b deschedule IPI, the `std::{fs,io,sync,thread}`
subset, `x86_64-unknown-nitrox.json`) is **consumer-driven** — it lands with portable programs / the
browser, not as a desktop-MVP gate. See the decision log (2026-07-20; supersedes 2026-07-13).

**Dynamic linking.** Everything is a **static, non-PIE `ET_EXEC`** today: the kernel ELF
loader rejects `ET_DYN` and `PT_INTERP` outright, and each program carries its own copy of
everything it uses. At present that is *right* — the shipping binaries are 13–73 KB
(`no_std` + `alloc`, hand-rolled runtime), so a loader would cost complexity and save
nothing.

**It stops being right at the GUI toolkit**, for a reason worth stating precisely: static
linking defeats page sharing exactly where sharing would pay. Shared file-backed text
(planned as B4a) shares pages across *instances of one program*, because it maps the same
`FileObject`. Two different statically-linked apps that both embed the toolkit hold
byte-identical code in **different files**, so they share nothing — not through B4a, not
through CoW, and not through the content-addressed store, which dedupes whole files. Five
apps each embedding a 2–5 MB toolkit is 10–25 MB of duplicated, unshareable text on a
256 MB machine. Dynamic linking is what extends B4a's sharing *across* programs; B4a is
what makes a loaded library shareable at all (without file-backed mapping, loading a `.so`
would eagerly copy it per process — the complexity with none of the benefit).

What it needs, roughly in order:

1. **Thread-local storage first.** The dynamic TLS models need `FS_BASE` /
   `sys_thread_set_tls`, which is itself deferred in the `std` cluster. Hard prerequisite.
2. **Kernel: accept `ET_DYN` + `PT_INTERP`** — map the interpreter and enter it instead of
   the program. User-space ASLR already supplies the base randomization PIE wants (28 bits
   for ELF/stack/mmap).
3. **A userspace `ld.so`**: map segments (over B4a's file-backed path), walk the dependency
   graph, process relocations (`RELATIVE` / `GLOB_DAT` / `JUMP_SLOT`), resolve symbols, run
   init/fini arrays.
4. **An ABI answer, because Rust has no stable ABI.** Two viable shapes: a **C ABI seam**
   at the library boundary (`extern "C"`, `#[no_mangle]`) — stable across compiler versions
   but awkward for Rust-to-Rust calls; or **whole-system build coherence** — one pinned
   compiler, everything in a generation rebuilt together, Rust ABI used freely within it.
   The **content-addressed store + generations makes the second genuinely viable here** in a
   way it is not on a conventional distro: a generation already *is* a coherent closure, and
   a toolchain bump is just a new generation. Recommended shape: coherence as the default,
   with a deliberate C seam only where a plugin boundary needs to outlive a rebuild.

The store and profile layers already anticipate this — `content-addressed-store.md` reserves
`lib/<library>` and `profiles-and-namespace-projection.md` designs `/lib`, both explicitly
"once dynamic linking exists".

**Scheduled with the GUI toolkit milestone**, alongside the process-memory-model bundle.
Sequencing note: do not build the loader speculatively, but **decide the toolkit's ABI seam
when the toolkit is designed** — retrofitting three shipped apps is far worse than starting
them right. Natural trigger: the second or third GUI app, i.e. the point where one library
would be resident more than once.

**POSIX compatibility shim.** Optional future. Translates POSIX calls to handle-based equivalents. Enables ported C software without native rewrites. Not a design constraint; the native interface design doesn't bend to accommodate POSIX. Trigger: a must-have C dependency (target the pure-Rust ecosystem first — see the 2026-07-20 std stance).

### Resource servers (in-kernel)

**`/dev` directory stub (enumerable placeholder).** Slice 5 gives `DeviceNode` a
real kernel struct (PCI-discovered nodes; block disks resolve via
`KernelServerId::BlockDevice` at `/dev/blk`), but there is still **no enumeration
syscall** (`ENUMERATE` is defined but unused) and **no listable `/dev` directory**
— lookups resolve a known path to a node; nothing enumerates the children of
`/dev`. A directory-listing surface was deferred until a device manager or a real enumeration
consumer existed. **The consumer now exists**: `list` (coreutils Milestone 1) makes
`list /dev` a day-one shell command, and `sys_ns_enumerate` is built but has no user. The
open design question is how a listing tool chooses between *namespace enumeration* (what
`/dev` needs — it is kernel-served) and an *fs-server directory session* (what `list` uses
today). Scheduled as **D3 of the pre-CLI substrate-hardening pass**
(`docs/planning/phase-4-desktop.md`). See the decision log (2026-06-22, 2026-06-23).

### Runtime libraries

**`cargo xtask abi-sync-check`.** `userspace/libkern` is the canonical userspace
mirror of the kernel ABI (syscall numbers, `#[repr(C)]` layouts, `Rights`/`KError`/
`KObjectType` values). A checker that parses both sides and verifies they agree is
deferred: the compile-time `offset_of!`/`size_of` asserts on both the kernel and
`libkern` sides, plus a green `cargo xtask qemu` (the demos exercise nearly the whole
syscall surface against the live kernel), give most of the protection for far less
cost. Build the real checker when a second non-demo consumer (eshell, fs-server) makes
drift likelier. Until then, changing an ABI type means editing both copies by hand.
Trigger: that second consumer, or a drift bug.

**TypedRecord support for enums.** The `#[derive(TypedRecord)]` macro initially supports primitive scalars, `String`, `Vec<T>` of TypedRecord, nested structs, `Option<T>`, and `RawHandle`. Enums (tagged unions) are deferred; they require wire-format extensions and more complex derive code. Trigger: a concrete need; not foreseen as urgent.

**TypedRecord support for generics beyond `Vec<T>`.** Same reasoning. Deferred until a concrete need.

**Lifetimes beyond `'static` in TypedRecord.** Same.

**iovec-style scatter/gather user access.** All current copy primitives operate on contiguous buffers. Scatter/gather (vectored I/O equivalents) isn't initially needed. Trigger: a syscall whose performance benefits from it.

**Per-thread CPU accounting (`ProcessCpu` / `ThreadCpu` clocks) — `TODO(sched-acct)`.**
`sys_clock_read` services `Monotonic` and `Realtime`; the two CPU-time clocks return
`Unsupported` because the scheduler does not accumulate per-thread run time. The natural
home is the context switch (charge elapsed monotonic time to the outgoing thread) plus a
per-`Process` roll-up on reap. Trigger: profiling or accounting that needs CPU time rather
than wall time — a `time` builtin, per-process scheduler statistics beyond the existing
`/proc/sched/stats`, or rlimits (which need CPU-time accounting to enforce a CPU limit).

**vDSO-equivalent for `sys_clock_read`.** On modern Linux, `clock_gettime` is implemented in vDSO — userspace reads TSC directly, no syscall. Nitrox initially does one syscall per `sys_clock_read`. The API shape leaves room for this optimization later (the `ClockId` enum can map to fixed memory locations) without changing call sites.

### Concurrency primitives

**SMP panic path: unsynchronized emergency serial, no stop-IPI (review F8).** The
panic/exception handlers write through `serial::emergency_writer()` — lock-free by
design (a fault while `SERIAL` is held must not deadlock) — which was sound
single-CPU but under SMP can interleave with another CPU's locked serial writes
(garbled diagnostics, not corruption). And a panicking CPU does not stop the
others: no halt/stop IPI exists, so the other CPUs keep scheduling and mutating
state while the panic prints (and, under `test-harness`, while the verdict is
written — a fail verdict still terminates QEMU promptly, so adjudication is
unaffected). Fix shape when taken: a panic-broadcast NMI/IPI parking other CPUs
(`cli; hlt`), then unsynchronized output is genuinely exclusive. Deferred: purely
a diagnostics-quality issue today; revisit with real-hardware bring-up or when a
flaky-boot investigation is hampered by garbled panic output. From the 2026-07-21
substrate review (decision log).

**Explicit grace-tracker quiescence on the syscall path — `TODO(smp)`.** The handle
table's deferred-close reclamation waits for a grace period tracked per *context id*, and
`current_ctx_id()` returns **0 in production builds** — every CPU shares one context. Today
nothing depends on the distinction: every handle syscall routes through a `HandleTable`
method that takes and drops a read guard, marking the context quiescent on drop, so
deferred closes are reclaimed on the next allocate/close. The `TODO(smp)` marks the case
that would break it — a syscall path that touches the table *without* going through such a
method, or a real per-CPU context id where one CPU's quiescence no longer implies
another's. **Worth an explicit look during the pre-CLI hardening pass** rather than
carrying as a comment: the sweep added for exit-time handle reclamation (2026-07-24) is a
new writer on this path. Trigger: that review, a per-CPU `ctx_id`, or a non-method table
access.

**Priority inheritance for userspace synchronization.** Userspace mutex/condvar implementations built on `sys_wait` don't initially address priority inversion. Trigger: a real-time workload where priority inversion is a problem.

**Deadline scheduling (EDF) as a fourth scheduler class.** RealTime class uses fixed priority, not EDF. Adding EDF is possible without architectural changes — fourth scheduler class. Trigger: a workload that benefits.

### Memory management

**The process memory model — one pass, four parts.** These extend the same AS/fault
machinery and should land together rather than as four independent entries each waiting on
the others:

1. **Copy-on-write, private file-backed data.** `MappingKind` has only
   `Anonymous`/`FileBacked` — no private/CoW kind — so a writable `PT_LOAD` cannot be
   mapped from the image. Needs a CoW fault path plus refcounted shared frames.
2. **Lazy `MemoryObject` backing.** `sys_memory_create` allocates and zeroes **every**
   frame eagerly, which is why `MemoryObject::MAX_SIZE` (16 MiB) exists at all — a DoS
   guard, not a designed ceiling. The `#PF` half of the gate is already closed
   (`AddressSpace::fault_in` + `map_vma_lazy`, 2026-06-12); what remains is a sparse
   per-page frame table allocated on fault.
3. **Per-process resource limits (rlimits).** CPU time, process count, committed memory —
   none are bounded today. Lifting `MAX_SIZE` *requires* this: the cap is what currently
   stands in for accounting.
4. **User-stack guard page + grow-down stacks.** The loader reserves a fixed 4-page stack
   with **no guard page**, so an overflow runs into whatever VMA sits beneath it. Kernel
   thread stacks already have the discipline (vmap: 16 KiB + 1 guard page); this is the
   userspace counterpart.

**Shared read-only text is *not* in this bundle** — it needs no CoW (the existing
`FileBacked` kind suffices) and is scheduled as **B4a of the pre-CLI substrate-hardening
pass** (`docs/planning/phase-4-desktop.md`). One design constraint it exposes and this
bundle inherits: every resolve mints a **fresh `FileObject` with its own page cache**, so
sharing across instances requires the spawner to reuse one image handle per program (or,
later, inode-keyed global caching).

**Trigger for the bundle:** the GUI toolkit / desktop-apps milestone — several apps linking
one toolkit, with real `.data`/`.bss` and enough concurrent instances that private copies
and eager allocation start to matter — or, earlier, a profile showing spawn latency or RSS
bound. Without `fork`, CoW's only consumer is the ELF data segment, which after B4a is a
few KB per process; that is why this is scheduled rather than urgent.

**NUMA-aware scheduling and memory allocation.** Architecture does not preclude NUMA but does not exploit topology. Single buddy allocator zones, scheduler treats all CPUs as uniform, work stealing ignores topology. Trigger: NUMA hardware where the lack of awareness is producing measurable problems.

**Per-CPU slab caching.** Phase 1's slab allocator uses a single global spinlock per
cache. SLUB's per-CPU optimisation (a `current_slab` pointer per CPU, the cache lock taken
only on slow paths) is structurally compatible with the existing state machine. **Its
original trigger — "SMP bring-up introduces per-CPU areas" — has fired** (SMP landed in
Phase 3 and per-CPU areas exist), and it was not done, so this is now carried on a
performance trigger instead: allocator contention showing up in a profile, or a workload
where several CPUs allocate hard concurrently.

**Empty-slab reclamation back to the buddy.** Once a slab cache grows by one page, that page stays with the cache forever. Production kernels reclaim wholly-empty slabs after a watermark; Nitrox doesn't yet. Trigger: long-running workloads where slab churn produces visible memory bloat, or memory-pressure handling (the OOM daemon) needs a hook to drain caches.

**DMA / Normal zone split in the buddy.** The buddy treats every Usable frame above 1 MiB as a single pool — `DmaBuffer` returns whatever block the buddy gives, with no address-range constraint. A below-16 MiB (ISA-DMA) or below-4 GiB (32-bit-only PCI) zone would only matter for a device that **cannot** do 64-bit DMA, which the project's **no-legacy ≈2014 / x86-64-v2 baseline excludes** (modern AHCI advertises `CAP.S64A`); the dev loop's 256 MiB of RAM is sub-4 GiB regardless. Trigger: a real driver that genuinely needs an address-constrained zone (none foreseen). When it lands, `DmaBuffer::alloc` grows a max-physical-address (DMA-mask) parameter and the buddy a zoned free-list. See the `TODO:` comment in `kernel/src/mm/buddy.rs`.

**Partial / splitting `sys_memory_unmap` — `TODO(mm)`.** The syscall takes `(addr, size)`
but **ignores `size`**: it unmaps the whole VMA covering `addr`. So an unmap of part of a
mapping silently unmaps all of it — the caller asked for less and got more, with no error.
Every caller today maps and unmaps whole objects, so it has not bitten, but it is a
footgun rather than a limitation. Honouring `size` means splitting a VMA (and, for a
file-backed one, adjusting the cache-page range), which the VMA tree does not do yet.
Trigger: any caller that unmaps a sub-range — an allocator returning part of an arena, or a
`std`-style `munmap`. Until then the argument should arguably be rejected when it does not
cover the whole VMA, rather than ignored.

**Reclaiming empty intermediate page tables on unmap.** `ArchPaging::unmap_page` clears the leaf entry but leaves the PDPT/PD/PT frames it walked through allocated, even when an unmap empties one. Reclaiming them needs a per-table populated-entry count (or a 512-slot scan on every unmap). Phase 1 runs a single address space with little mapping churn, so the leak is negligible. See the `TODO:` comment in `kernel/src/arch/x86_64/paging.rs`. Trigger: address-space teardown (process exit) or `munmap`-heavy workloads make the retained tables a measurable cost.

**Range TLB invalidation (`flush_tlb_range`).** `ArchPaging` exposes `flush_tlb_page`
(one page) and `flush_tlb_all` (a CR3 reload), so a bulk unmapper issues one
`flush_tlb_page` per page. The **cross-CPU shootdown is built** (substrate hardening Parts
B/C, 2026-07-21: `tlb::shootdown`, IF-robust, broadcast on user-page unmap) — only the
*range* form is missing. Trigger: an unmap path whose per-page flush loop is measurable
(large `munmap`, address-space teardown).

**Debug-build lock-ordering enforcement.** `kernel/CLAUDE.md` documents that debug builds will track acquisition order and panic on violations. The mechanism doesn't yet exist; the only lock-ordering enforcement today is code review and `kernel/docs/lock-ordering.md`. Trigger: enough locks exist that the cost of building the rank-tracker outweighs the cost of a missed bug.

### Testing and CI

**`xtask test-qemu` integration harness.** ~~A QEMU integration test today would amount to a single "did the kernel reach the end of `kernel_main`?" smoke via `isa-debug-exit`.~~ **Implemented 2026-07-14** (Phase 3, after the storage/fs/SMP stack gave real regression surface — the SMP migration hazard was caught by a boot loop, not a unit test). `cargo xtask test-qemu` boots the `test-harness` build headless and adjudicates the whole boot from QEMU's `isa-debug-exit` exit code. The self-test payload (`selftest` feature) *is* the suite; a per-case framework under `tests/qemu-tests/` remains deferred (trigger: a test that needs to assert something the boot chain doesn't already exercise). See `docs/conventions/qemu-integration-tests.md`.

**Per-case QEMU test framework (`tests/qemu-tests/`).** `cargo xtask test-qemu` adjudicates
the *whole boot* from one `isa-debug-exit` verdict — the self-test payload is the suite, and
CI runs it (2026-07-24). A per-case framework, where an individual test asserts something
the boot chain does not already exercise, remains deferred. Trigger: a case that cannot be
expressed as "the boot completes and the harness agrees". See
`docs/conventions/qemu-integration-tests.md`.

**`libkern` mock-syscall test mode.** `userspace/libkern/CLAUDE.md` describes a feature-flagged mock that records and replays syscalls for host-side tests of layers above. The crate is a `cargo new` placeholder in Phase 0. Trigger: real syscalls are defined.

### Auditing and observability

**Comprehensive systemwide tracing infrastructure (DTrace/eBPF equivalent).** Per-CPU ring buffers for kernel tracing exist in concept. A full programmable tracing facility (DTrace probes, eBPF-style filters, etc.) is out of scope initially. Trigger: deep performance analysis needs that exceed what `kprintln!` and basic tracing handles.

## Resolved (kept for the record)

Entries that have been **done**, listed here rather than left in the open sections above —
scanning "what is still owed" has to be reliable. The reasoning for each lives in the
decision log entry for the date shown.

| What was deferred | Resolved | How |
|---|---|---|
| x2APIC | 2026-06-26 | Built — and **x2APIC-only**, not dual-mode: the ≈2014 baseline guarantees it, so no xAPIC fallback is carried. The dev loop runs QEMU ≥ 9.0. |
| Concurrent direct-block + forwarded-lookup hang | 2026-07-20 | Not the block/forwarding path — a missing cross-CPU wake; fixed by the reschedule IPI. |
| Writeback IRPs | Phase 3 | Dirty-page writeback landed with read-write `fs-server-ext4` (`FileObject::writeback` / `sys_file_sync`). |
| File-size discovery via `sys_handle_stat` | 2026-06-27 | `HandleInfo` gained `size: u64`; the lazy resolve grants `INSPECT`. |
| Forwarded-lookup concurrency (N = 1) | Phase 2/3 | `US_PENDING_MAX = 8` outstanding forwarded lookups, correlated by `request_id`. |
| File truncate | 2026-07-24 | `sys_file_truncate` → `RESOLVE_TRUNCATE` → `ext4::truncate_file`, kernel-forwarded so the page cache stays coherent. |
| Reclaiming a process's handles at exit | 2026-07-24 | Marked thread + a batched sweep in `reap_pending`; `next_owned` stays unbuilt (the sweep scans). |
| Wall-clock time | 2026-07-24 | `CLOCK_REALTIME` anchored from the CMOS RTC at boot; the fs-server stamps inodes. Setting the clock stays unbuilt — see the open entry. |
| Numeric `/proc/self/{pid,tid}` | Phase 3 | The capture → format → synthesize primitive landed with `/proc/sched/stats`; `/proc/self/status` was its second consumer. |
| General deferred object reclamation from `SCHED`/IRQ context | 2026-07-21 | `SchedState::deferred_drops`, drained by `reap_pending` in thread context (review fix F2). |
| `kmalloc` alignment > `SLAB_SIZE` | 2026-06-12 | Resolved by taking the other path: `mm::dma::DmaBuffer` allocates from the buddy. No remaining client wants it from `kmalloc`. |
| Kernel host-side unit tests | Phase 1 | The kernel is `lib + bin`; `cargo xtask test` runs the host suite (780+ tests across kernel and userspace). |
| `cargo xtask test` subcommand | Phase 1 | Exists and runs the whole host suite. |
| fs-server block I/O in 4 KiB blocks | 2026-07-23 | `DiskReader`'s transfer unit is the 4 KiB block — ~8× fewer device round trips. |
| Image assembly + QEMU smoke in CI | 2026-07-24 | A second CI job runs `xtask test-qemu` (which builds the image), with OVMF/gdisk/mtools/e2fsprogs installed. It had been deferred "until there is meaningful regression surface"; every Milestone 1 regression was caught by this gate and none would have failed the other jobs. |
| `xtask test-qemu` integration harness | 2026-07-14 | Boots the `test-harness` build headless and adjudicates from `isa-debug-exit`. A per-case framework under `tests/qemu-tests/` is still open (below). |

## How to use this document

When you encounter something that seems unimplemented or absent, check this document first. If it's listed here, the absence is intentional; the reasoning is preserved. If it's not listed here and you think it should be, consider adding an entry — the document is append-only-with-revisions, not a static snapshot.

If you're triggering a deferred item (starting work on TCP/IP, beginning aarch64 port, etc.), update this document at the same time. The deferred entry should **move to the Resolved table**, not be left in place with a status note appended — an open section that mixes finished work with owed work cannot be scanned, which is how three deferrals went unnoticed until a consumer tripped over them (the 2026-07-24 audit).

**A deferral only exists if it is in this document.** The three gaps that cost the most to
rediscover — exit-time handle reclamation, the wall clock, file truncate — were each
"recorded" somewhere else: a sentence in an architecture doc promising a later slice, a
`TODO(...)` in the syscall table, a bullet in a crate's `CLAUDE.md`. None was in this list,
so none was ever reviewed. If you write a stub, a `TODO`, or prose that promises future
work, mirror it here. `cargo xtask check-deferrals` enforces the `TODO(tag)` half of that
mechanically.

The decision log (`history/decision-log.md`) is the place to record the actual decision when a deferred item moves into active work — what triggered it, what the implementation approach is, when the decision was made.
