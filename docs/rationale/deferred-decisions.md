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

## Deferred to later phases

These will eventually be done, but aren't in initial scope. Each entry documents what's deferred and what triggers it.

### Hardware support

**aarch64.** The architecture abstraction layer is designed in from the start. Every arch-specific concern (paging, interrupts, FPU, user memory access, power) is behind a trait. The `kernel/src/arch/aarch64/` directory exists as stubs. Initial implementation targets x86_64; aarch64 implementation comes after the x86_64 system is mature. Trigger: when there's a specific aarch64 target system to support, or when the x86_64 implementation is stable enough to make the porting effort worthwhile.

**5-level paging on x86_64 (57-bit virtual addresses).** Ice Lake and later support 5-level paging, allowing virtual address spaces up to 128 PiB. Nitrox uses 4-level paging (canonical 48-bit). The address space is plenty for any conceivable workload. Trigger: a use case that requires it. None foreseen.

**x2APIC mode (dual-mode local APIC).** The local-APIC bring-up (`arch/x86_64/apic.rs`) uses **xAPIC** (MMIO) only. x2APIC accesses the same registers via MSRs (`0x800 + reg>>4`) with 32-bit APIC IDs; it is the right mode on real hardware and is **mandatory** once SMP exceeds 255 logical CPUs (xAPIC's 8-bit IDs cannot address them; >255 additionally wants IOMMU interrupt remapping). Per the "no legacy hardware" baseline above, every supported CPU *has* x2APIC — so the plan is **dual-mode with boot-time auto-detection (`CPUID.01H:ECX[21]`), preferring x2APIC**, keeping xAPIC for the early-boot transition (firmware hands off in xAPIC mode), as a fallback, and for the TCG dev loop. The xAPIC↔x2APIC difference is localised to the register accessors (`read_reg`/`write_reg`, plus the 32-bit `id()` and the single-MSR ICR write for IPIs), so it is a small, contained change. **Deferred** because: (a) it is only needed at SMP / real-hardware bring-up, and (b) QEMU's TCG only began emulating x2APIC in **9.0** (the dev loop runs older QEMU under TCG), so it cannot be exercised under the current loop without bumping the dev QEMU floor to ≥ 9.0 or using KVM (`-enable-kvm -cpu host`). Trigger: Phase 3 SMP (especially > 255 CPUs) or real-hardware bring-up; implement alongside a QEMU-floor bump or an opt-in `xtask qemu --kvm`. See the decision log (2026-06-11).

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
controller, single SATA disk, one outstanding command** (slot 0). Multi-port /
multi-disk, multiple controllers, NCQ (queued commands), and port multipliers are
deferred to when a configuration needs them. It resolves the controller's GSI
from the **PCI interrupt-line register** (firmware-programmed on QEMU); proper
ACPI `_PRT` routing (which needs AML) is deferred — see `device-node.md`. The
read self-test brings up against the existing AHCI boot disk; the dedicated
`xtask build-disk` + ext4 test disk arrive with the fs-server (slice 7).

**Writeback IRPs.** The page cache initially flows reads only; dirty-page
writeback through write IRPs lands with read-write `fs-server-ext4` (Phase 3).

**Concurrent same-page faults (slice 8 Part 2b).** When a file page fault misses, the
fault path reserves the frame (`Loading`), starts the producer fill, and parks the
thread on a per-*fault* `PendingOperation`. A *second* thread faulting the **same**
page (`Reserve::Loading`) has no handle on that in-flight fill's PO, so it `yield_now`s
and retries until the page is `Ready`. This cannot occur in the milestone (single CPU,
one faulter per `FileObject`), so the yield path is never taken; the proper fix —
store the fill PO in the cache page so a second faulter blocks on it (one wakeup, no
spin) — is deferred. Trigger: a multi-threaded process (or shared `FileObject`) that
faults the same page concurrently. Until then the yield-retry is correct, just
not elegant.

**File-size discovery via `sys_handle_stat` — RESOLVED (slice 9 Part 3).** Slice 8
deferred this: a client holding a lazily-resolved `FileObject` had no way to ask its
size (`HandleInfo` reported only rights/type/generation, and the lazy resolve consumed
`content_len`). **Done in slice 9 Part 3** (the named consumer, eshell `cat`):
`HandleInfo` gained a `size: u64` (16 → 24 bytes, not in the ABI hash), `stat_on` reads
the per-type size (`FileObject.size`, `MemoryObject.size`, else `0`), and the lazy
resolve grants `INSPECT`. See the decision log, 2026-06-27. (The slice-8 large-file
milestone's shared `LARGE_FILE_BYTES` constant remains as a now-unnecessary bridge; a
future cleanup could switch init's verifier to `stat`.)

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

**AHCI single-outstanding-command contention (slice 9 Part 3).** The AHCI driver runs
**one command at a time** (Phase 2; `inflight: AtomicPtr<Irp>`). Two processes issuing
disk reads concurrently (e.g. the demo `parent`'s block reads + the fs-server's ext4
reads driven by eshell `cat`) race the single command slot and corrupt each other's
reads. Slice 9 Part 3 sidesteps it by **sequencing** init's children (the demo runs to
completion before eshell launches), so only one disk consumer is live. The proper fix —
queue IRPs in the driver (a software command queue, or AHCI NCQ with multiple slots) so
concurrent submissions serialise correctly instead of clobbering — is deferred to the
storage hardening in Phase 3 (RW + writeback already pull on the driver). Trigger: any
two processes doing concurrent block I/O.

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

**Forwarded-lookup concurrency (N = 1).** A Userspace Server's
`UserspaceServerReg` (slice 7 Part 3) holds a single pending-lookup slot: one
forwarded `sys_ns_lookup` per server may be outstanding; a second returns
`WouldBlock`. The milestone init path issues lookups serially, so N = 1 suffices.
Raising it to a small fixed array (correlating replies by the already-present
`request_id`) is a localized change — done when boot issues overlapping lookups
(Part 4 if needed). The reply completion is **inline-in-send** (no DPC) because
`run_pending` drains only at the interrupt-dispatch tail — see the decision log
(2026-06-25, slice 7 Part 3).

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

**btrfs, NTFS, XFS, ZFS, etc.** Each is a userspace fs-server binary. None are in initial scope. Trigger: specific deployment needs.

**Encrypted root (LUKS).** Architecture accommodates this — LUKS is a block device filter driver in initramfs; init invokes it before spawning fs-server. Not in initial scope. Trigger: encrypted-root deployment.

**LVM / software RAID at early boot.** Same architectural accommodation as LUKS. Initial scope is direct partition mounts.

**Runtime reconfiguration of critical-path mounts.** Currently requires reboot through eshell. Live remounting of `/`, `/home`, etc., is not supported. Trigger: deployment scenarios where it matters.

### Userspace

**Shell grammar specification.** The shell's data model is committed (typed structured streams, port-based wiring, the display verb, model-view decomposition). The exact syntax is deferred to shell implementation. Trigger: when shell implementation begins.

**`std` port for Nitrox target.** The native interface is handle-based; `std::fs`, `std::thread`, `std::net`, `std::sync`, `std::io` need implementation over the native syscalls. Trigger: stabilization of the syscall ABI plus a desire to enable the broader Rust ecosystem on Nitrox.

**POSIX compatibility shim.** Optional future. Translates POSIX calls to handle-based equivalents. Enables ported C software without native rewrites. Not a design constraint; the native interface design doesn't bend to accommodate POSIX. Trigger: a desire to port specific C software.

### Resource servers (in-kernel)

**Numeric `/proc/self/{pid,tid}` (`/proc/self/status`).** Slice 3 ships the
`/proc/self/{process,thread,namespace}` Kernel Servers (handles to the caller's own
objects) but **not** numeric pid/tid. pid/tid are *attributes* of the `Process`/`Thread`
objects a caller now holds, so the eventual mechanism is itself an open choice — a
**synthesized read-only `MemoryObject` snapshot** (`/proc/self/status`) vs. **extending
handle introspection** (`sys_handle_stat` returns only type/rights/generation today).
The MemoryObject route needs a *synthesis primitive* first (allocate a frame, write
kernel bytes via the HHDM, hand back `MAP_READ`-only) — a reusable building block worth
designing deliberately. **Rejected** alternative: extending the namespace-lookup
contract to return a scalar in `IoResult.result` (a permanent per-path
handle-vs-value ambiguity / footgun). Trigger: a real consumer of numeric pid/tid
(e.g. logging infra), or the first synthesized read-only snapshot (`/proc/self/status`)
that forces the primitive. See the decision log (2026-06-22).

**`/dev` directory stub (enumerable placeholder).** Slice 5 gives `DeviceNode` a
real kernel struct (PCI-discovered nodes; block disks resolve via
`KernelServerId::BlockDevice` at `/dev/blk`), but there is still **no enumeration
syscall** (`ENUMERATE` is defined but unused) and **no listable `/dev` directory**
— lookups resolve a known path to a node; nothing enumerates the children of
`/dev`. A directory-listing surface is deferred until a device manager (slice 7)
or a real enumeration consumer exists. Trigger: either of those. See the decision
log (2026-06-22, 2026-06-23).

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

**vDSO-equivalent for `sys_clock_read`.** On modern Linux, `clock_gettime` is implemented in vDSO — userspace reads TSC directly, no syscall. Nitrox initially does one syscall per `sys_clock_read`. The API shape leaves room for this optimization later (the `ClockId` enum can map to fixed memory locations) without changing call sites.

### Concurrency primitives

**General deferred object reclamation from a `SCHED`/IRQ context.** Code running under the rank-1 `SCHED` lock (or, later, in an IRQ before the scheduler lock is taken) cannot drop an `ObjectRef`/`TransferRef`: object destruction may take a lower-rank lock (e.g. the buddy allocator frees a `MemoryObject`'s frames), which must not nest under `SCHED`. The first concrete instance — a `BlockBounded` IPC send timing out in the timer tick (2026-06-12) — is handled *locally* with **reclaim-on-recv**: the timeout only tombstones the held send (completing its PO `TimedOut`); the actual refs are swept out and dropped outside `SCHED` on the next `recv` (or at channel close). That works because a channel still being received on (or eventually closed) always reaches a safe drop point. The **general** mechanism — a deferred-free list drained at a safe point outside the lock, the DPC queue being its natural vehicle — is deferred until a consumer needs reclamation with no such natural drain (e.g. device-I/O request cancellation, where the completion/cancel runs in a DPC). Trigger: such a consumer; until then per-path reclaim (reclaim-on-recv, `Inner`-drop-at-close) suffices. See the decision log (2026-06-12).

**Priority inheritance for userspace synchronization.** Userspace mutex/condvar implementations built on `sys_wait` don't initially address priority inversion. Trigger: a real-time workload where priority inversion is a problem.

**Deadline scheduling (EDF) as a fourth scheduler class.** RealTime class uses fixed priority, not EDF. Adding EDF is possible without architectural changes — fourth scheduler class. Trigger: a workload that benefits.

**Per-process resource limits (rlimits).** Handle table has a per-process soft cap. CPU time, file descriptor count beyond the global handle cap, process count, memory consumption — none of these have explicit limits initially. The capability model plus the OOM daemon provide partial substitutes. Trigger: deployment scenarios with untrusted multi-tenant workloads.

### Memory management

**NUMA-aware scheduling and memory allocation.** Architecture does not preclude NUMA but does not exploit topology. Single buddy allocator zones, scheduler treats all CPUs as uniform, work stealing ignores topology. Trigger: NUMA hardware where the lack of awareness is producing measurable problems.

**Per-CPU slab caching.** Phase 1's slab allocator uses a single global spinlock per cache. SLUB's per-CPU optimisation (a `current_slab` pointer per CPU, with the cache lock taken only on slow paths) is structurally compatible with the existing state machine but requires per-CPU infrastructure that doesn't exist yet. Trigger: SMP bring-up in Phase 3 introduces per-CPU areas; the slab fast path migrates onto them at that point.

**Empty-slab reclamation back to the buddy.** Once a slab cache grows by one page, that page stays with the cache forever. Production kernels reclaim wholly-empty slabs after a watermark; Nitrox doesn't yet. Trigger: long-running workloads where slab churn produces visible memory bloat, or memory-pressure handling (the OOM daemon) needs a hook to drain caches.

**Alignment greater than `SLAB_SIZE` (4 KiB) in `kmalloc`.** `kmalloc(_, align)` for `align > SLAB_SIZE` returns null (the slab's descriptor-at-byte-0 trick relies on the user pointer staying in the first page of the buddy block; larger alignments push it into later pages and break the recovery). As anticipated, the one client that needs it — **DMA buffers** — got a separate path rather than a `kmalloc` extension: [`mm::dma::DmaBuffer`](../../kernel/src/mm/dma.rs) (2026-06-12, `phase-2/dma-alloc`) allocates a power-of-two block straight from the buddy (whose order-`k` blocks are aligned to `2^k × PAGE_SIZE`), zeroes it, and exposes the **physical address** + contiguity a device needs. So `kmalloc` itself keeps the cap; this is a non-issue now (no remaining client wants `> SLAB_SIZE` alignment from `kmalloc`).

**DMA / Normal zone split in the buddy.** The buddy treats every Usable frame above 1 MiB as a single pool — `DmaBuffer` returns whatever block the buddy gives, with no address-range constraint. A below-16 MiB (ISA-DMA) or below-4 GiB (32-bit-only PCI) zone would only matter for a device that **cannot** do 64-bit DMA, which the project's **no-legacy ≈2014 / x86-64-v2 baseline excludes** (modern AHCI advertises `CAP.S64A`); the dev loop's 256 MiB of RAM is sub-4 GiB regardless. Trigger: a real driver that genuinely needs an address-constrained zone (none foreseen). When it lands, `DmaBuffer::alloc` grows a max-physical-address (DMA-mask) parameter and the buddy a zoned free-list. See the `TODO:` comment in `kernel/src/mm/buddy.rs`.

**Reclaiming empty intermediate page tables on unmap.** `ArchPaging::unmap_page` clears the leaf entry but leaves the PDPT/PD/PT frames it walked through allocated, even when an unmap empties one. Reclaiming them needs a per-table populated-entry count (or a 512-slot scan on every unmap). Phase 1 runs a single address space with little mapping churn, so the leak is negligible. See the `TODO:` comment in `kernel/src/arch/x86_64/paging.rs`. Trigger: address-space teardown (process exit) or `munmap`-heavy workloads make the retained tables a measurable cost.

**Lazy (demand-paged) `MemoryObject` backing.** `sys_memory_create` allocates and zeroes **every** frame eagerly, up front (`MemoryObject::try_new` → one `buddy_alloc(0)` per page). That is why the syscall imposes a `MemoryObject::MAX_SIZE` cap (16 MiB in Phase 1): with eager allocation, a single large create commits that much physical RAM at once and runs an unpreemptable allocate-and-zero loop, which on a small VM (QEMU `-m 256M`, no swap, cooperative scheduler) could exhaust the buddy or stall the machine. The cap is a denial-of-service guard, **not** a designed ceiling — Linux (`mmap(MAP_ANONYMOUS)`/`memfd`) and Windows (pagefile-backed section objects) impose no per-allocation byte limit because they are lazy: reserve the range, fault in demand-zero pages on first touch, and bound the total with system-wide accounting (overcommit policy / `RLIMIT_AS` / cgroups; the commit limit). The real fix is the same here — reserve the object + its VMA cheaply and allocate frames in the page-fault handler on first access — at which point the per-call cap is replaced by per-process committed-memory quotas + address-space limits enforced through the capability model. **The `#PF`-handler half of that gate is now closed** (demand-paging slice, 2026-06-12): `AddressSpace::fault_in` resolves not-present user faults against the VMA tree and `map_vma_lazy` reserves anonymous ranges without backing them — the ELF loader already reserves stacks this way. What remains for *objects* is making `MemoryObject` itself lazy (a sparse per-page frame table, allocated on fault rather than at `try_new`) plus `Process`-level resource accounting (see "Per-process resource limits (rlimits)" above); only then can the `MAX_SIZE` cap be lifted. Trigger: a workload needing objects larger than the cap. Until then, raising the constant only moves the DoS threshold, so it stays small and tied to eager allocation.

**User-stack guard page + grow-down stacks.** The ELF loader reserves a fixed **4-page** user stack (`DEFAULT_USER_STACK_SIZE`) and, as of the demand-paging slice (2026-06-12), backs it lazily via `map_vma_lazy` — each page faults in on first touch. There is **no guard page** below it: a stack overflow runs straight into whatever VMA sits beneath the reservation (today nothing, but eventually the mmap window), silently corrupting it instead of faulting. The demand-paging machinery is exactly what a real stack wants — a larger grow-down reservation with an **unmapped guard page** (and, optionally, demand-growth: a fault just below the current stack extends it rather than SegFaulting). Deferred deliberately: at 4 pages the guard page buys little and the stack size is a placeholder. Trigger: **"real" userspace processes with realistic (larger) stacks** — at that point give the loader/thread-spawn path a guard page below each stack and decide whether to support automatic grow-down. The kernel-thread stacks already have this discipline (vmap allocates 16 KiB + 1 guard page — see `docs/architecture/memory-management.md`); this is the userspace counterpart.

**Range TLB invalidation and cross-CPU shootdown.** `ArchPaging` exposes `flush_tlb_page` (one page) and `flush_tlb_all` (a CR3 reload). There is no range flush — a bulk unmapper issues one `flush_tlb_page` per page — and no cross-CPU shootdown IPI. Phase 1 is single-CPU, so a local flush is a complete flush. Trigger: SMP bring-up (Phase 3) makes a stale TLB entry on another CPU a correctness bug; a `flush_tlb_range` and a `send_shootdown_ipi` land with the per-CPU and IPI infrastructure.

**Debug-build lock-ordering enforcement.** `kernel/CLAUDE.md` documents that debug builds will track acquisition order and panic on violations. The mechanism doesn't yet exist; the only lock-ordering enforcement today is code review and `kernel/docs/lock-ordering.md`. Trigger: enough locks exist that the cost of building the rank-tracker outweighs the cost of a missed bug.

### Testing and CI

**Kernel host-side unit tests.** Phase 0 ships without host-side tests for kernel code. The kernel crate is `#![no_std]` / `#![no_main]` against `x86_64-unknown-none` with `panic = "abort"`; making it host-testable requires splitting into `lib + bin` with conditional compilation. The current Phase 0 kernel has roughly thirty lines of testable arithmetic (`pick_scale`, `text_width`, `Rgb::pack`) that will be replaced when the PSF loader and a proper console land on top of an allocator. Trigger: Phase 1 lands code with real, non-throwaway host-testable logic (handle table operations, namespace resolution, ABI encoding/decoding — all called out in `kernel/CLAUDE.md` as candidates).

**`xtask test` subcommand.** The convention is that `cargo xtask test` runs host-side tests for the OS we are building. With no host-testable kernel/userspace code in Phase 0, a stub subcommand would be ceremony. Trigger: same as above — when there is something to run, the subcommand lands alongside it.

**`xtask test-qemu` integration harness.** A QEMU integration test today would amount to a single "did the kernel reach the end of `kernel_main`?" smoke via `isa-debug-exit`. `xtask qemu` already proves that interactively, and there is no IDT, memory-map handling, allocator, IPC, or scheduler code yet to actually regress. Trigger: Phase 1 introduces a milestone past the Limine handoff that benefits from automated assertion (e.g., "allocator initialised", "first userspace process spawned").

**Image assembly and QEMU smoke in CI.** Phase 0 CI runs `cargo xtask build` only. Adding `cargo xtask image` would exercise the sgdisk + mtools path; adding a QEMU smoke run would exercise the boot path. Both are deferred until there is meaningful regression surface beyond the build itself. Trigger: Phase 1 boot path complexity warrants it.

**`libkern` mock-syscall test mode.** `userspace/libkern/CLAUDE.md` describes a feature-flagged mock that records and replays syscalls for host-side tests of layers above. The crate is a `cargo new` placeholder in Phase 0. Trigger: real syscalls are defined.

### Auditing and observability

**Comprehensive systemwide tracing infrastructure (DTrace/eBPF equivalent).** Per-CPU ring buffers for kernel tracing exist in concept. A full programmable tracing facility (DTrace probes, eBPF-style filters, etc.) is out of scope initially. Trigger: deep performance analysis needs that exceed what `kprintln!` and basic tracing handles.

## How to use this document

When you encounter something that seems unimplemented or absent, check this document first. If it's listed here, the absence is intentional; the reasoning is preserved. If it's not listed here and you think it should be, consider adding an entry — the document is append-only-with-revisions, not a static snapshot.

If you're triggering a deferred item (starting work on TCP/IP, beginning aarch64 port, etc.), update this document at the same time. The deferred entry should either be removed (if the work is being done) or updated with a status note.

The decision log (`history/decision-log.md`) is the place to record the actual decision when a deferred item moves into active work — what triggered it, what the implementation approach is, when the decision was made.
