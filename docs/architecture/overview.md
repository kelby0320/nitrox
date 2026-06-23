# Nitrox — Architecture Overview

Nitrox is a hobby operating system written in Rust. This document is the entry point to the project's architecture documentation. It's intended to be read in one sitting and to give you a working mental model of the system. It is not a specification — it is orientation.

For specific details: where this document says "the handle table is segmented," the [handle system architecture document](handle-system.md) describes the segmented structure in depth, and [the handle encoding spec](../spec/handle-encoding.md) gives the bit-level layout. The pattern is consistent throughout — every overview claim has a corresponding architecture document with depth, and where contracts matter, a spec document with precision.

## What Nitrox is

Nitrox is a from-scratch operating system targeting x86_64 (UEFI only) with aarch64 designed in from the start. It is the successor to Latte, an earlier Unix-like hobby OS written in C. Nitrox is more ambitious: rather than reimplementing Unix in a new language, it is an attempt at a coherent modern design that learns from Unix without copying its mistakes.

The design takes the things Unix got right — composable pipelines, treating system state as named resources, a powerful shell environment — and rebuilds them on a foundation that doesn't carry Unix's weaker patterns: signals, synchronous syscalls, a global VFS tree, ambient authority through UID/GID, and the cluster of compatibility constraints that come with POSIX. The result is closer in spirit to Plan 9, Fuchsia, and the seL4 capability literature than to Linux or BSD, while remaining a system designed to be used rather than a research artifact.

The implementation language is Rust throughout — kernel, userspace services, and runtime libraries. The kernel is `#![no_std]` with no external crates (one planned future exception, ACPICA, documented in the [power management rationale](../rationale/why-phased-acpi.md)). The bootloader is Limine. The build system is Cargo plus an `xtask` workspace for tasks beyond Cargo's native capabilities.

## The two ideas the system rests on

Nitrox is structured around two principles that work together:

> **You find resources by name. You access them by capability.**

These are distinct concerns. Naming and access are conflated in Unix (a file path identifies a file *and* opening it triggers a permission check on every traversal) and in most systems that descend from it. In Nitrox they are separated, and the separation is what makes the rest of the architecture work.

**Naming is per-process and structural.** Every process has a namespace — a private hierarchical map from names to resources. Looking up `/foo/bar` in your namespace is meaningful only relative to your namespace. There is no global namespace. A sandboxed process's namespace simply does not contain the resources it shouldn't access; lookups don't fail with "permission denied," they fail with "not found." Sandboxing is by construction, not by check.

**Access is by capability handle.** When a name lookup succeeds, you receive a handle — an opaque integer that the kernel knows refers to a specific kernel object with specific allowed operations. You cannot manufacture a handle; you can only receive one from the kernel or from someone who already has one. Once you hold a handle, the namespace is no longer involved — you operate directly on the kernel object through the handle. The kernel checks, on every operation, that your handle permits what you're trying to do.

The combination is powerful: a process's authority is exactly the set of handles it holds, plus whatever it can look up in its namespace. Restricting a process means giving it a smaller namespace and fewer initial handles. There is no UID to compromise, no group membership to escalate through, no "root" to become.

The [namespace and capabilities rationale](../rationale/why-capabilities.md) goes deeper on why this design over the alternatives.

## How the system is divided

```
┌─────────────────────────────────────────────────────────────────┐
│ Userspace                                                       │
│                                                                 │
│   User applications                                             │
│   Shell, display server, compositor (deferred)                  │
│   Service manager, session manager, profile servers             │
│   Resource servers (fs-servers, netstack, etc.)                 │
│   Init (PID 1)                                                  │
│                                                                 │
│   Runtime libraries: libos, librt, libstream, librsproto        │
│   Raw syscall layer:  libkern                                   │
└────────────────────────────────────┬────────────────────────────┘
                                     │ syscall boundary (~30 syscalls)
┌────────────────────────────────────┴────────────────────────────┐
│ Kernel                                                          │
│                                                                 │
│   Handle table, kernel objects, namespace engine                │
│   Memory management (buddy, slab, VMM, page cache)              │
│   IPC channels, notification queues                             │
│   Scheduler (per-CPU runqueues, three classes)                  │
│   IRP-based driver framework                                    │
│   Tier 1 drivers: PCI, AHCI/NVMe, GPT, console                  │
│   In-kernel resource servers: /proc, /dev, /initramfs, ...      │
│                                                                 │
│   Architecture abstraction layer: x86_64 (initial), aarch64     │
└─────────────────────────────────────────────────────────────────┘
                                     │
┌────────────────────────────────────┴────────────────────────────┐
│ Hardware (via UEFI firmware and Limine bootloader)              │
└─────────────────────────────────────────────────────────────────┘
```

The kernel is small — it provides mechanism, not policy. Filesystems, network stacks, package management, the display system, and the shell are all userspace. The kernel's job is to implement handles, namespaces, memory, IPC, scheduling, and the IRP-based driver framework, plus enough in-kernel resource servers to bootstrap userspace. Beyond that, everything is a process.

The userspace side has its own structure. Init is minimal — it bootstraps the system from the initramfs and hands off to the service manager. The service manager supervises the running services. Resource servers (filesystem drivers, network stack, device-specific servers) are ordinary userspace processes that speak a standard protocol. Applications use a layered runtime: `libkern` for raw syscalls, `libos` for typed handles and async I/O, `librt` for synchronous wrappers, `libstream` for typed structured I/O, `librsproto` for the resource server protocol.

## The major kernel concepts

### Handles and capabilities

A handle is an opaque 64-bit integer that names a kernel object and encodes specific permitted operations on it. Every syscall that operates on a kernel object takes a handle. The kernel's global handle table — segmented for scaling — maps handle values to entries containing the object pointer, the rights bitmask, the owner process ID, and a generation counter that detects use-after-close.

The security guarantee is owner enforcement, not cryptographic unforgeability: even a correctly-guessed handle value belonging to another process is rejected because the kernel checks `owner_pid` against the calling process on every lookup.

Handles can be transferred between processes through IPC channels. The kernel mediates every transfer atomically; the sending process loses access (move) or retains access (duplicate); attenuation is supported (the destination receives a handle with strict subset of rights).

The userspace runtime wraps raw handles in a typed `Handle<T, M>` where `T` is the kernel object type and `M` is a typestate marker encoding the principal access mode. Calling `read()` on a `Handle<File, WriteOnly>` is a compile error.

See: [handle system architecture](handle-system.md), [handle encoding spec](../spec/handle-encoding.md).

### Kernel objects

A kernel object is anything that can be referenced by a handle. The complete list is fixed: Process, Thread, Namespace, MemoryObject, IpcChannel, NotificationChannel, Timer, InterruptObject, PendingOperation, IoRing, EntropyObject, DeviceNode, UserspaceServerReg. Each kernel object begins with a common header (refcount, type tag) so generic code can manipulate them through type-erased pointers; type-specific operations dispatch through a `match` on the type tag.

Internal kernel data structures (page table entries, VMA trees, IRPs, scheduler runqueues) are not kernel objects — they're not handle-accessible.

See: [kernel objects reference](../reference/kernel-objects-catalogue.md).

### Namespaces and resource servers

The namespace is itself a kernel object: a hierarchical map from name components to bindings. A binding can be a direct handle to an object, a sub-namespace, a path-rewrite rule, or — most commonly — a registration pointing at a resource server.

A resource server is anything that responds to a standard protocol: given a path context and a name, return a handle. The kernel has compiled-in resource servers for `/proc`, `/dev`, `/sys`, `/initramfs`, `/dev/framebuffer`, and a few others. Userspace resource servers communicate over IPC channels and are dispatched to via their endpoint handles.

The boundary between kernel and userspace resource servers is intentional. Filesystems are userspace, so a buggy filesystem can't corrupt kernel memory. Hardware-rooted resources (process objects, device nodes, the kernel log) are in-kernel because they have to be.

Resource servers do not register themselves. A privileged supervisor process — init, or a service manager acting on init's behalf — calls `sys_ns_bind` to register an endpoint into a namespace. The capability to do so (`SysCaps::BIND_NAMESPACE`) is concentrated in supervisors and never granted to ordinary resource servers. A resource server's only namespace operation is to be the target of a binding, not to perform one.

See: [namespace and resource servers architecture](namespace-and-resource-servers.md), [why supervisor-mediated registration](../rationale/why-supervisor-registration.md), [librsproto wire format spec](../spec/rsproto-wire-format.md).

### Memory management

Three layers. The buddy allocator manages physical page frames. The SLUB-inspired slab allocator handles kernel object allocation on top of the buddy. The virtual memory manager owns the per-process address space — VMAs in a red-black tree, page tables in arch-specific format, fault handling that integrates with the page cache for file-backed regions.

The kernel half of every address space is shared (single set of higher-half page table entries, copied into every address space at creation). Syscall entry is a ring transition, not a page-table switch. Context switching between threads in the same address space is a register swap.

Address space layout uses 28 bits of ASLR for ELF base, stack, and mmap arena. Every thread gets a stack with a guard page; every kernel stack has a guard page below it.

User memory access from kernel code goes through a strict discipline: `UserPtr<T>` and `UserMutPtr<T>` opaque types that don't deref, copy primitives that open SMAP/PAN windows briefly, and an exception table that catches page faults during copy and resumes at a recovery PC.

See: [memory management architecture](memory-management.md), [user memory access spec](../spec/user-memory-access.md).

### Process and thread model

A process is an address space, a namespace handle, a current working directory subtree handle, a list of owned handles, a system capability bitmask, and a set of threads. A thread is a register state, an FPU context, a kernel stack, scheduling parameters, and a TLS base.

There are no UIDs, no GIDs, no process groups, no session IDs. Children are tracked by a `creator` field for reparenting purposes; on creator exit, orphans reparent to init. Multiple processes can hold handles to the same child process and all receive `ChildExited` notifications independently. "Reaping" is closing the last process handle.

Process spawn takes an explicit list of handle grants. Children do not inherit handles automatically. Argv and environment are typed structural values (a list of strings, a map of strings to typed values) rather than null-terminated C strings.

See: [process model architecture](process-model.md), [why no signals](../rationale/why-no-signals.md).

### IPC and notifications

These are deliberately separate subsystems with different shapes.

**IPC channels** are bidirectional, peer-to-peer, message-oriented queues between processes. A message is fixed-size (4 KiB payload + up to 8 transferable handles) plus optional companion `MemoryObject` handles for bulk data. Channels are backpressure-aware with explicit send modes (Block, NoBlock, BlockBounded). Every IPC operation returns a `PendingOperation` handle; threads block by calling `sys_wait` on these handles, never inside the IPC syscall itself.

**Notification queues** are kernel-to-process only. The kernel delivers structured `Notification` values (typed enum with sparse discriminants) into a per-process bounded queue. Hardware exceptions (segfault, illegal instruction, divide-by-zero) are notifications. Process lifecycle events (`ChildExited`, `PeerClosed`) are notifications. Power events and memory pressure are notifications. The forward-compatible enum design means new notification variants can be added without breaking existing programs — they see them as `Unknown { kind }`.

The two channels are separate because their delivery requirements differ. IPC is peer communication; notifications are kernel-mediated events that must be delivered reliably even when the process is in a degraded state (during a fault, under memory pressure, etc.).

See: [IPC architecture](ipc.md), [notification format spec](../spec/notification-format.md).

### Scheduling

Three scheduling classes: RealTime (fixed priority, FIFO within priority, requires a syscap to use), TimeShared (CFS-like virtual-runtime fair scheduling, the default), Idle (per-CPU placeholder running `hlt`). Per-CPU runqueues with work stealing. Affinity placement on wake.

The unified blocking primitive is `sys_wait` over a list of waitable handles. Threads do not block "inside" syscalls — every syscall that could block returns a `PendingOperation` handle, and the thread blocks on `sys_wait` of that (and possibly other) handles. This is the same primitive used for "wait for IPC message," "wait for child exit," "wait for timer," "wait for I/O completion." There is no `read(2)` that blocks.

See: [scheduler architecture](scheduler.md), [why async-first syscalls](../rationale/why-async-syscalls.md).

### Drivers and IRPs

Hardware drivers are kernel modules — either Tier 1 (compiled into the kernel ELF, selected by Cargo features, used for boot-path drivers) or Tier 2 (loadable at runtime by a userspace driver manager). Filesystem drivers are not kernel modules; they're userspace processes.

The driver framework uses I/O Request Packets (IRPs) flowing through driver stacks. Each IRP is initiated by an upper-layer request, flows down through a stack of drivers, hits hardware, and returns asynchronously through completion routines. Filter drivers can be inserted transparently into stacks for encryption, compression, or logging. The model is borrowed from Windows NT, which got this part right.

Userspace drivers are possible — the kernel can grant a userspace resource server an `InterruptObject` handle and program the IOMMU to constrain DMA to memory regions the driver legitimately holds.

See: [drivers and IRP architecture](drivers-and-irps.md).

## The major userspace concepts

### Content-addressed store

The store is an immutable directory tree where every package lives at a path containing a cryptographic hash of its contents and all transitive dependencies (`/store/8d3f2a1c-glibc-2.38/`). Multiple versions coexist without conflict. Updates are atomic (new path, old untouched). Rollback is instant. The model is borrowed from NixOS and Guix.

The store lives on an ordinary ext4 partition, served by the standard fs-server-ext4. Its read-only-once-written property is enforced not by the filesystem but by the namespace layer: every normal process's namespace binds `/store` with rights that don't include `MAP_WRITE`, and the kernel enforces those rights regardless of what the filesystem server might claim. fs-server itself is a dumb filesystem server with no special knowledge of `/store`.

See: [content-addressed store architecture](content-addressed-store.md), [why content-addressed](../rationale/why-content-addressed-store.md).

### Profiles and namespace projection

A profile maps user-visible paths to store paths (`/bin/bash → /store/abc123-bash-5.2/bin/bash`). Profile servers are userspace resource servers backed by a profile manifest. A user's namespace is a composition of profile servers (system profile, user profile), tier-appropriate `/dev` namespace, scoped subtree handles for `/home`, filtered process server views, and so on.

Different users see different namespaces because the session manager constructs them differently. Different roles (standard user, admin, sandboxed app) get different layer compositions. This is where most "policy" lives in the system.

See: [profiles and namespace projection](profiles-and-namespace-projection.md).

### Init and bootstrap mount topology

PID 1 (`init`) is deliberately minimal — its only job is bringing up enough of the system to hand off to the service manager. It reads a bootstrap manifest (`/etc/init.toml` on the initramfs) describing critical-path mounts: the root filesystem and any other partitions that must be available before the service manager can start.

For the common single-partition case, init.toml has one entry (mount the root ext4 partition). For more complex layouts (separate `/home` partition, separate `/store` on a different filesystem), init brings up multiple fs-server instances in dependency order before proceeding.

If a critical-path mount fails, init drops into the emergency shell (`eshell`), a minimal interactive shell bundled in the initramfs that provides enough capability to inspect block devices, edit init.toml, and reboot. Recovery from misconfigured boot is straightforward; you don't need a rescue USB.

The initramfs must contain the closure of software needed for critical-path mounting — all required fs-server binaries, any kernel modules for unusual storage controllers, plus eshell. The system image builder computes this closure at build time based on the target system's mount topology.

See: [boot flow architecture](boot-flow.md), [init.toml schema](../spec/init-toml-schema.md), [emergency recovery](../architecture/emergency-recovery.md).

### Service management

The service manager is a supervised process started by init. It reads service declarations (TOML files describing executable, required handles, dependencies, restart policy), constructs each service's namespace and handle set, spawns it, and supervises it. Capability discipline is enforced structurally — a service receives exactly the handles its declaration lists, and the service manager won't grant rights it doesn't itself hold.

Resource servers (filesystem drivers, network stack, profile servers, etc.) are spawned through a standard protocol: the service manager creates a control IPC channel, spawns the resource server with the channel and the resources it needs, waits for a "Ready" message containing the resource server's endpoint handle, and then binds the endpoint into the appropriate namespace.

See: [init and service management architecture](init-and-services.md), [service.toml schema](../spec/service-toml-schema.md).

### Runtime libraries

Five crates, layered:

- **libkern**: raw syscall wrappers, `#![no_std]`, no `alloc`. ABI types and unsafe `extern` declarations. Used directly by early services (init, fs-servers, eshell).
- **libos**: typed `Handle<T, M>`, async executor built on `sys_wait`. Provides the type-system enforcement of access modes.
- **librt**: synchronous and fiber-based wrappers for code that prefers blocking semantics. Built on libos.
- **libstream**: typed structured I/O, `#[derive(TypedRecord)]` procedural macro for automatic schema derivation, table reading and writing.
- **librsproto**: the resource server wire protocol — message envelope, version negotiation, operation dispatch.

A future `std` port will eventually provide `std::fs`, `std::thread`, `std::net` over the native handle-based interface, enabling the broader Rust ecosystem.

See: [userspace runtime architecture](userspace-runtime.md).

### Shell and typed streams

The Unix shell pipeline is a high point of system design — programs as composable filters, the shell as a composition language. Nitrox preserves this and extends it: programs produce **typed structured data** (tables of records with declared schemas) instead of byte streams, and the system understands these structures generically. Generic operators (sort, filter, select, join, group) work on any table by column name, with column types determining behavior. Handles are first-class values; capabilities flow through pipelines as data.

Programs that produce raw text are not left out. Their output is automatically wrapped as a single-column `Table<String>` with column name `line`, and all generic operators work on it. The floor is Unix; everything above it is opt-in.

The same model extends to GUIs through a `WidgetRecord` type — a program can emit a structured description of an interactive widget, and a display server (terminal in text mode, compositor in GUI mode) renders it appropriately. Programs don't choose their rendering; the display layer does.

See: [shell and typed streams architecture](shell-and-streams.md), [typed stream wire format](../spec/typed-stream-format.md).

## Boot flow at a glance

From power-on to running system:

1. UEFI firmware loads Limine from the FAT32 EFI System Partition.
2. Limine loads the kernel ELF and the initramfs (CPIO blob) from the ESP. It sets up the higher-half kernel mapping, the higher-half direct map of physical memory, the framebuffer, ACPI tables, and brings up all SMP cores.
3. The NASM boot stub jumps to `kernel_main`. The kernel initializes its allocators, handle table, IPC, notifications, namespace engine, scheduler, and DPCs. It enumerates PCI devices via the ACPI MCFG table, brings up Tier 1 storage drivers, and registers in-kernel resource servers (`/proc`, `/dev`, `/initramfs`, etc.) into the root namespace.
4. The kernel spawns init from the initramfs with the root namespace and full system capabilities.
5. Init reads `/etc/init.toml` from the initramfs and processes its critical-path mounts in dependency order: spawn fs-server, wait for Ready, bind endpoint into the system namespace. Failure drops to eshell.
6. Init reads the system manifest from the now-mounted filesystem, spawns the system profile server and binds it into the root namespace.
7. Init spawns the service manager with delegated capabilities and the system namespace. The service manager reads its service declarations and brings up services in dependency order: logging, audit, device manager, namespace manager, time sync, authentication, package manager, session manager.
8. Once boot is stable, init calls `sys_release_initramfs()`. The kernel unbinds `/initramfs` and frees the initramfs memory.
9. Session manager presents login. After authentication, it constructs a per-user namespace and spawns the user's shell.

See: [boot flow architecture](boot-flow.md) for the full sequence with handle flow and the per-step capability state.

## What Nitrox is not

A few things are deliberately out of scope, either permanently or until much later:

- **POSIX compatibility as a primary goal.** A compatibility shim may exist eventually for ported software, but the native interface is handle-based, not POSIX. Programs written for Nitrox use its native model.
- **Unix signals.** Async signal interruption is replaced by the notification queue and the unified `sys_wait` primitive.
- **A global VFS tree.** There is no kernel-side mount table. Path resolution is per-namespace; "mounting" is binding a resource server endpoint into a namespace.
- **UID/GID and ambient authority.** Authority is held in handles, not in the calling process's identity.
- **A monolithic kernel containing filesystem code.** Filesystems are userspace processes.
- **Synchronous syscalls that block.** Every potentially-blocking operation returns a `PendingOperation` handle; threads block only on `sys_wait`.
- **NUMA-aware scheduling and memory allocation** (initially — designed not to preclude, but not implemented).

The full list of explicit non-goals and deferred work is in [non-goals and deferred work](../rationale/deferred-decisions.md).

## Reading paths from here

If you're new to the project and want depth on a specific area:

- **Understanding how a subsystem works:** start in `docs/architecture/`. Each major subsystem has its own document.
- **Understanding why a decision was made:** `docs/rationale/`. These capture the design discussions that produced v5.1 of the design document, plus subsequent refinements.
- **Looking up exact contracts (ABIs, formats, layouts):** `docs/spec/`. These are normative — when implementing or consuming a contract, the spec is the source of truth.
- **Working day-to-day in the repo:** `docs/conventions/` for code style, `unsafe` policy, testing conventions, debugging.
- **Specific lookup questions:** `docs/reference/` for catalogues — every kernel object, every syscall, every error code, every system capability.

The full design document that produced this architecture lives in `docs/history/design-doc-v5.1.md`. It's a comprehensive single artifact rather than a working set, preserved for context and reference.

If you're a future Claude Code session, start with the per-subdirectory `CLAUDE.md` files for environment-specific guidance, then this overview, then whichever architecture/spec/reference document is closest to the task at hand.
