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

## Deferred to later phases

These will eventually be done, but aren't in initial scope. Each entry documents what's deferred and what triggers it.

### Hardware support

**aarch64.** The architecture abstraction layer is designed in from the start. Every arch-specific concern (paging, interrupts, FPU, user memory access, power) is behind a trait. The `kernel/src/arch/aarch64/` directory exists as stubs. Initial implementation targets amd64; aarch64 implementation comes after the amd64 system is mature. Trigger: when there's a specific aarch64 target system to support, or when the amd64 implementation is stable enough to make the porting effort worthwhile.

**5-level paging on amd64 (57-bit virtual addresses).** Ice Lake and later support 5-level paging, allowing virtual address spaces up to 128 PiB. Nitrox uses 4-level paging (canonical 48-bit). The address space is plenty for any conceivable workload. Trigger: a use case that requires it. None foreseen.

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

### Runtime libraries

**TypedRecord support for enums.** The `#[derive(TypedRecord)]` macro initially supports primitive scalars, `String`, `Vec<T>` of TypedRecord, nested structs, `Option<T>`, and `RawHandle`. Enums (tagged unions) are deferred; they require wire-format extensions and more complex derive code. Trigger: a concrete need; not foreseen as urgent.

**TypedRecord support for generics beyond `Vec<T>`.** Same reasoning. Deferred until a concrete need.

**Lifetimes beyond `'static` in TypedRecord.** Same.

**iovec-style scatter/gather user access.** All current copy primitives operate on contiguous buffers. Scatter/gather (vectored I/O equivalents) isn't initially needed. Trigger: a syscall whose performance benefits from it.

**vDSO-equivalent for `sys_clock_read`.** On modern Linux, `clock_gettime` is implemented in vDSO — userspace reads TSC directly, no syscall. Nitrox initially does one syscall per `sys_clock_read`. The API shape leaves room for this optimization later (the `ClockId` enum can map to fixed memory locations) without changing call sites.

### Concurrency primitives

**Priority inheritance for userspace synchronization.** Userspace mutex/condvar implementations built on `sys_wait` don't initially address priority inversion. Trigger: a real-time workload where priority inversion is a problem.

**Deadline scheduling (EDF) as a fourth scheduler class.** RealTime class uses fixed priority, not EDF. Adding EDF is possible without architectural changes — fourth scheduler class. Trigger: a workload that benefits.

**Per-process resource limits (rlimits).** Handle table has a per-process soft cap. CPU time, file descriptor count beyond the global handle cap, process count, memory consumption — none of these have explicit limits initially. The capability model plus the OOM daemon provide partial substitutes. Trigger: deployment scenarios with untrusted multi-tenant workloads.

### Memory management

**NUMA-aware scheduling and memory allocation.** Architecture does not preclude NUMA but does not exploit topology. Single buddy allocator zones, scheduler treats all CPUs as uniform, work stealing ignores topology. Trigger: NUMA hardware where the lack of awareness is producing measurable problems.

**Per-CPU slab caching.** Phase 1's slab allocator uses a single global spinlock per cache. SLUB's per-CPU optimisation (a `current_slab` pointer per CPU, with the cache lock taken only on slow paths) is structurally compatible with the existing state machine but requires per-CPU infrastructure that doesn't exist yet. Trigger: SMP bring-up in Phase 3 introduces per-CPU areas; the slab fast path migrates onto them at that point.

**Empty-slab reclamation back to the buddy.** Once a slab cache grows by one page, that page stays with the cache forever. Production kernels reclaim wholly-empty slabs after a watermark; Nitrox doesn't yet. Trigger: long-running workloads where slab churn produces visible memory bloat, or memory-pressure handling (the OOM daemon) needs a hook to drain caches.

**Alignment greater than `SLAB_SIZE` (4 KiB) in `kmalloc`.** Today `kmalloc(_, align)` for `align > SLAB_SIZE` returns null. The slab's descriptor-at-byte-0 trick relies on the user pointer staying in the first page of the buddy block; alignments above 4 KiB push the user pointer into later pages and break the recovery. Trigger: DMA buffer allocation in Phase 2 needs page-multiple alignments and is the natural client for a real answer here (likely a separate `dma_alloc` path that maintains its own table, not a kmalloc extension).

**DMA / Normal zone split in the buddy.** The buddy treats every Usable frame above 1 MiB as a single pool. ISA-DMA (below 16 MiB) has no fast path. Trigger: a driver that needs ISA-DMA buffers (none yet). See `TODO:` comment in `kernel/src/mm/buddy.rs`.

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
