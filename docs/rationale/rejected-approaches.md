# Rejected Approaches

This document records design alternatives that were considered during the design phase and not chosen. The point is not to relitigate decisions but to preserve the reasoning so that future-you (and future readers, and AI assistants working on the project) understand why things are the way they are. Several of these are choices that look natural until you trace through their consequences.

Entries are organized roughly by subsystem.

## Per-process handle tables instead of a global handle table

**Approach:** Each process holds its own handle table, like Unix file descriptors. Handles are small integers that are only meaningful within the issuing process.

**Why considered:** Smaller tables (per-process scaling), simpler concurrency (no cross-process coordination), familiar to anyone coming from Unix.

**Why rejected:** Handle transfer becomes a multi-table operation. To send a handle from process A to process B via IPC, the kernel must allocate a slot in B's handle table, install the object reference, and then either drop A's slot (move) or keep it (duplicate). This is a coordinated update across two tables — at the very least, both tables' locks have to be acquired in a deadlock-safe order. With a global table, transfer is a single operation: change the slot's `owner_pid` (move), or allocate a new slot for the destination (duplicate). The global table is also strictly better for accounting — the kernel has a complete inventory of all live handles to every object. Revocation, refcounting, and audit are simpler. The cost is a larger table that has to scale, addressed by the segmented design.

## Synchronous syscalls as the primary interface

**Approach:** Standard Unix-style. `sys_read` blocks until data is ready; `sys_recv` blocks until a message arrives. Add async-style mechanisms (something like io_uring) on the side for cases that need them.

**Why considered:** Simpler ergonomics for the common case. Most application code wants synchronous semantics — "give me the next byte" — and is happy to block while waiting.

**Why rejected:** The synchronous model bakes the kernel's blocking semantics into every syscall. Programs that want to wait on multiple things concurrently need separate threads or explicit multiplexing primitives (`select`/`poll`/`epoll` and family). Cancellation becomes a question of "send a signal? close the fd? what happens to the in-flight syscall?" — none of which has clean answers in Unix. The async-first model with `sys_wait` as a single unifying primitive composes much better, and the runtime libraries provide synchronous wrappers for application code that wants synchronous semantics. See [why-async-syscalls.md](why-async-syscalls.md).

## Unix signals

**Approach:** Async event delivery via signal handlers, with mask manipulation for critical sections.

**Why considered:** It's how every Unix program does it. Vast amounts of existing code know how to deal with it.

**Why rejected:** Signals have well-known structural problems — async-signal-safety restrictions, race conditions, coalescing of events of the same type, narrow payloads, poor interaction with threads, no clean library composition story, the EINTR/restart dance, the irreducible cognitive load of per-thread signal masks. The notification queue in Nitrox does what signals were trying to do, but with structured typed payloads, no async-safety restrictions, and clean integration with the unified `sys_wait` primitive. See [why-no-signals.md](why-no-signals.md).

## A global VFS tree in the kernel

**Approach:** Linux/BSD-style. The kernel maintains a tree of mount points, dentry cache, inode table, and the rest of the VFS apparatus. Filesystems are kernel modules that hook into this.

**Why considered:** It's the dominant model. Every Unix has it. Vast performance optimization in a few popular implementations.

**Why rejected:** It puts filesystem-format-specific code in the kernel, which is a major source of kernel bugs and CVEs. It conflates naming with access control (the dentry cache caches both, and access checks happen during traversal). It assumes a single global naming structure that processes interact with through the same API surface, which is fundamentally incompatible with per-process namespaces. Nitrox's approach — per-process namespace objects, resource servers as the dispatch mechanism, filesystem code in userspace — moves filesystem complexity out of the kernel and decouples naming from access. The runtime cost is some extra IPC for filesystem operations; in practice, the bulk of file I/O goes through page cache mappings that don't traverse the userspace fs-server in the hot path.

## ACL-based filesystem permissions

**Approach:** Each file has an ACL. The kernel checks the calling process's identity (UID/GID) against the ACL on every open, read, write.

**Why considered:** Familiar. Well-understood. Tools and conventions exist for managing ACLs.

**Why rejected:** Authority in Nitrox is held in handles, not derived from identity. Once a process has a handle, the operations it can perform are determined by the handle's rights — not by the caller's identity, not by the resource's metadata. Access control happens at handle issuance time (deciding what handle to grant) rather than at every operation. This is what capabilities mean. Filesystem ACLs would be a parallel access control mechanism that conflicts with the handle model and adds nothing — the namespace layer already determines what each process can name, and the rights layer determines what they can do with what they've named. See [why-capabilities.md](why-capabilities.md).

## Inheritable handles (Unix fork-style)

**Approach:** Child processes automatically inherit all of the parent's handles unless explicitly marked CLOEXEC-equivalent.

**Why considered:** It's how Unix works. The fork+exec pattern depends on inheritance for stdin/stdout/stderr to flow through.

**Why rejected:** Implicit inheritance is a security hazard. Programs can leak handles to children unintentionally — a handle the parent forgot about gets passed to every spawned child, including ones that have no business holding it. CLOEXEC was invented to opt handles out of inheritance, but defaults are powerful and "inherit by default" produces ongoing low-grade leakage. Nitrox uses explicit handle grants — the parent lists exactly which handles to pass to the child, with what attenuated rights, in what slot. The convention "stdin/stdout/stderr/log/notification/namespace" still exists; the child looks up these handles by slot name during startup, but they're explicitly granted by the parent. There's no possibility of accidental inheritance.

## argv as char\*\*[] of null-terminated strings

**Approach:** Standard C convention. `int main(int argc, char \*\*argv)`. Environment is a similar `extern char \*\*environ`.

**Why considered:** Universal in C-derived systems. Programs already know how to deal with it.

**Why rejected:** It's a stringly-typed interface for what is fundamentally structured data. Programs spend time parsing argv into typed values (integers, paths, flags). Programs writing tools that pass arguments to other programs spend time formatting structured data into strings. Both directions involve serialization that the runtime could do once. Nitrox passes argv as a typed `Value::List<String>` and environment as a `Value::Record<String, Value>` — programs receive structured data directly. Programs that want the C convention call `libos` helpers that flatten the structured form to char\*\*[] — the conversion happens at a single boundary instead of throughout the system.

## A single fs-server per filesystem type, mounting multiple volumes

**Approach:** One fs-server-ext4 process holding multiple block device handles, serving multiple ext4 filesystems internally with mount-table-style routing.

**Why considered:** Resource savings (one process for many filesystems). Familiar to anyone thinking in Unix VFS terms.

**Why rejected:** Capability isolation. A single fs-server holding multiple block device handles is a single failure domain — a bug or compromise affects all filesystems it serves. Separate processes per block device mean each fs-server's authority is scoped to its block device. Process-level isolation is enforced by the OS without relying on the fs-server's internal correctness. The resource cost (one process per mounted filesystem) is small in absolute terms. The Nitrox model is "one fs-server process per block device, one resource server registration per fs-server, namespace composition does the rest."

## fs-server registers itself into the namespace

**Approach:** Grant fs-server `SysCaps::BIND_NAMESPACE`; fs-server calls `sys_ns_bind` on itself at startup.

**Why considered:** Fewer moving parts, no supervisor handshake, simpler protocol.

**Why rejected:** Principle of least authority. fs-server's only legitimate use of `BIND_NAMESPACE` is to register itself once — but the capability persists, and a bug or compromise in fs-server could bind additional entries. The supervisor pattern has init or service manager perform the registration based on a startup handshake; fs-server never holds `BIND_NAMESPACE`. See [why-supervisor-registration.md](why-supervisor-registration.md).

## const-generic Handle\<T, const R: Rights\>

**Approach:** Encode rights as a const generic parameter on the handle type. Compile-time enforcement of generic and modifier rights via type-level bitflag operations.

**Why considered:** Maximum compile-time enforcement. Every rights manipulation is a type-level operation; misuse is a compile error.

**Why rejected:** Requires `generic_const_exprs` and `adt_const_params` from nightly Rust. Nitrox is committed to stable Rust only — see [why-rust.md](why-rust.md) for the reasoning. The chosen design uses typestate marker types for principal rights (`Handle<T, M>` where `M` is `ReadOnly`, `WriteOnly`, etc.) with a runtime `Rights` field for generic and modifier rights. The most common mistakes (writing to a read-only handle, looking up on a no-LOOKUP namespace) are still caught at compile time. The runtime check on modifier rights is fine because the kernel re-validates on every syscall anyway — userspace runtime checks are belt-and-suspenders.

## Lazy FPU save (TS-bit trickery)

**Approach:** Mark the FPU as inactive on context switch via the TS bit on amd64. The first FPU instruction in the new thread traps; the trap handler saves the previous thread's FPU state and restores the new one. Threads that never touch FPU save no state.

**Why considered:** Smaller context switch cost for threads that don't use SSE/AVX. Historically common; Linux did this for years.

**Why rejected:** Two reasons. First, the Lazy FP State Restore vulnerability (CVE-2018-3665) demonstrated that lazy FPU switching can leak microarchitectural state across processes. Second, modern compilers emit SSE for `memcpy`/`memset` aggressively; nearly every thread touches FPU state within a few instructions of starting. Lazy save is a pessimization in practice. Nitrox uses eager XSAVE on every context switch, which is approximately 15 lines of arch code instead of the 100+ lines lazy switching requires.

## Shared-memory SPSC notification ring

**Approach:** The notification queue is a shared-memory ring buffer between kernel and userspace. The kernel writes notifications directly into shared memory; userspace reads them without a syscall.

**Why considered:** Lower per-notification cost. No copy across the kernel/user boundary on the read side.

**Why rejected:** The kernel-copy model (notifications are stored in kernel memory; `sys_notif_recv` copies one to user memory) is simpler and gives up almost nothing. Notifications are low-frequency events (faults, child exits, signal-equivalents) — the syscall cost per notification is irrelevant. Shared memory introduces ABI constraints (the ring layout becomes part of the kernel/user contract), an extra mapping in every process, head/tail synchronization complexity, and reduces the kernel's freedom to evolve internal queue representation. The high-throughput path (the io_uring-style ring, used for I/O completions) is where shared memory makes sense; for notifications, it doesn't. See the kernel-copy notification design in `architecture/notifications.md`.

## ResourceServer trait with separate sync and async methods

**Approach:** The resource server trait has both `lookup_sync` and `lookup_async`, with implementations choosing which to provide. The kernel dispatches to the appropriate one based on whether it expects a sync or async response.

**Why considered:** Cleaner API surface for resource servers that genuinely operate synchronously (kernel resource servers with cached data).

**Why rejected:** The unified `OpStatus` return — `Completed`, `Pending`, `Rejected` — collapses both into a single signature without losing the optimization. Kernel resource servers with cache-hot data return `Completed` immediately with output populated. Userspace resource servers always return `Pending` because IPC is async by nature. Kernel resource servers that need to do I/O return `Pending` and signal completion later. One API, three answers, all cases handled. This is what the v5 design landed on.

## Signing and ABI-compatible kernel module loading

**Approach:** LKMs are cryptographically signed by a trusted authority. The kernel verifies signatures before loading. ABI compatibility is preserved across kernel versions so older modules continue to work.

**Why considered:** Production OS feature. Nice to have for security and operational convenience.

**Why rejected:** Out of scope for the initial implementation. Module signing requires a trust hierarchy (root keys, signing infrastructure) that's outside the scope of a hobby OS. ABI compatibility across versions is a strong constraint that limits the kernel's evolution; the initial design uses a build-hash ABI version that requires modules to be rebuilt against the running kernel. These can be added later if the project grows past the hobby-OS scope. The capability discipline plus the requirement that LKM loading needs `SysCaps::LOAD_MODULE` provides adequate authorization control for the initial scope.

## Built-in network bootstrapping

**Approach:** The kernel includes a network stack that's available during boot for network-mounted root filesystems, network logging, etc.

**Why considered:** Useful for diskless workstations, server fleet provisioning.

**Why rejected:** Network stack is a major engineering project (smoltcp is 15K lines; a from-scratch stack is 30-50K). Not on the critical path for any common deployment. PXE boot is handled by Limine before the kernel runs; the kernel doesn't need network for PXE. Network mounting can use the same userspace fs-server architecture as block-device-backed filesystems; the network stack itself is a userspace netstack server, deferred per the implementation phasing.

## In-kernel IOMMU management

**Approach:** The kernel manages IOMMU programming directly, with a high-level abstraction over VT-d / AMD-Vi / ARM SMMU.

**Why considered:** Cleaner separation; IOMMU is a kernel concern.

**What was actually chosen:** The kernel does manage IOMMU, but the management is tightly integrated with the device handle subsystem. When a device handle is granted to a userspace resource server, the kernel programs the IOMMU at the same time, constraining DMA to the memory regions the userspace driver legitimately holds. There's no separate "manage the IOMMU" subsystem in the kernel; it's a side effect of granting device access. This is a clarification rather than a rejected approach — flagging it because the alternative ("kernel has explicit IOMMU management API") was considered briefly.

## Where to read more

- [Why capabilities](why-capabilities.md)
- [Why Rust](why-rust.md)
- [Why no signals](why-no-signals.md)
- [Why async syscalls](why-async-syscalls.md)
- [Why content-addressed store](why-content-addressed-store.md)
- [Why supervisor-mediated registration](why-supervisor-registration.md)
- [Why phased ACPI](why-phased-acpi.md)
- [Deferred decisions](deferred-decisions.md)
