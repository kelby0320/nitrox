# New Hobby OS — Design Document
*Version 5.1 — v5 with clarifications on resource server registration, bootstrap mount topology, and emergency recovery*

---

## Changes in v5.1

v5.1 incorporates clarifications from design review after v5 was written. The architecture is unchanged; these are refinements and explicit statements of principles that were implicit or underspecified in v5.

**Substantive additions and clarifications:**

1. **Resource Server Startup Protocol** (new subsection in §4 Resource Server Model). Resource servers do not register themselves. They hand an endpoint handle to a supervisor process over a control channel; the supervisor (holding `SysCaps::BIND_NAMESPACE`) binds the endpoint into the namespace. `BIND_NAMESPACE` is concentrated in supervisory processes (init, service manager, session manager), never held by ordinary resource servers.

2. **Filesystem scoping policy lives in namespace rights, not fs-server** (§5 Content-Addressed Store, §5 Filesystem Drivers). fs-server is a dumb filesystem server — it reads and writes blocks and serves the resource-server protocol. Read-only-ness of `/store`, scoping of `/home/alice` to a subtree, and all similar policy decisions are enforced by the rights granted when namespace bindings are created. fs-server itself has no special knowledge of `/store` or `/home`.

3. **Bootstrap Mount Topology** (new subsection in §5 Userspace Architecture). Init reads a bootstrap manifest (`/etc/init.toml` on the initramfs) describing critical-path mounts. Init spawns fs-servers for these mounts in dependency order before proceeding to normal boot. The initramfs must contain the closure of fs-server binaries and kernel modules needed for critical-path storage.

4. **Emergency shell** (§5 Core Services, §13 Subsystem Catalogue). A minimal shell binary in the initramfs that init drops into when critical-path mounts fail. Provides enough capability to inspect and edit initramfs configuration from the serial console.

5. **Revised Boot Flow** (§11). Generalized to handle multiple critical-path mounts driven by the bootstrap manifest, not hardcoded to a single ext4 root.

6. **Initramfs closure property** (§3 Bootloader, §5 Bootstrap Mount Topology). Made explicit: the initramfs is built with every fs-server binary and kernel module needed to bring the system up to the point where it can read its real configuration. Computed at system image build time based on the target system's mount topology.

7. **Encrypted root / LVM deferred** (§14 Non-Goals and Deferred Work). The architecture accommodates LUKS and LVM as block device filter drivers, but initial scope does not include encrypted-root support.

No kernel mechanism changes. No syscall additions. No ABI changes. The surface area that shifts is: what lives in the initramfs (more than before), what init does (driven by config rather than hardcoded), and how resource servers get registered (supervisor does it, not the RS itself).

---

## Table of Contents

1. [Design Philosophy](#design-philosophy)
2. [Toolchain and Language](#toolchain-and-language)
3. [Bootloader](#bootloader)
4. [Kernel Architecture](#kernel-architecture)
   - [Handle and Capability System](#handle-and-capability-system)
   - [Kernel Objects](#kernel-objects)
   - [Namespace Subsystem](#namespace-subsystem)
   - [Resource Server Model](#resource-server-model)
   - [Memory Management](#memory-management)
   - [Process and Thread Management](#process-and-thread-management)
   - [Scheduler](#scheduler)
   - [Driver Subsystem](#driver-subsystem)
   - [IPC Subsystem](#ipc-subsystem)
   - [Notification Queue](#notification-queue)
   - [DPCs and Wait Queues](#dpcs-and-wait-queues)
   - [Time and Timers](#time-and-timers)
   - [Security Model](#security-model)
   - [Power Management](#power-management)
   - [Entropy](#entropy)
   - [Debugging Infrastructure](#debugging-infrastructure)
   - [Architecture Abstraction Layer](#architecture-abstraction-layer)
5. [Userspace Architecture](#userspace-architecture)
   - [Content-Addressed Store](#content-addressed-store)
   - [Filesystem Drivers](#filesystem-drivers)
   - [Bootstrap Mount Topology](#bootstrap-mount-topology)
   - [Profiles and Namespace Projection](#profiles-and-namespace-projection)
   - [Init and Service Management](#init-and-service-management)
   - [Logging and Auditing](#logging-and-auditing)
   - [Core Services](#core-services)
6. [Syscall Interface](#syscall-interface)
7. [Userspace Runtime Library](#userspace-runtime-library)
8. [User Interface and Shell](#user-interface-and-shell)
9. [Default Program Channels](#default-program-channels)
10. [Reference Projects](#reference-projects)
11. [Boot Flow](#boot-flow)
12. [Kernel Subsystem Catalogue](#kernel-subsystem-catalogue)
13. [Userspace Subsystem Catalogue](#userspace-subsystem-catalogue)
14. [Non-Goals and Deferred Work](#non-goals-and-deferred-work)

---

## Design Philosophy

The OS is a new hobby project, successor to **Latte** (a Unix-like OS in C). The goals are more ambitious: a clean, modern design not tied to POSIX or the Unix architecture, though drawing on its best ideas.

### Core Paradigm: Namespace and Capability

The design rests on two distinct ideas that together define how every piece of system state is addressed and accessed:

> **You find resources by name. You access them by capability.**

This is distinct from both Unix ("everything is a file" — name and access conflated into a path and permission check) and from generic object-oriented design ("everything is an object" — which says nothing about addressing or access control). The two ideas decompose cleanly:

**Namespace addressing:** Every resource the system exposes has a name in a per-process hierarchical namespace. You find things by looking them up. The namespace is the universal addressing layer. Crucially, different processes see different namespaces — a sandboxed process's namespace literally does not contain resources it should not access. There is no global namespace to query, no ambient authority to exploit.

**Capability access:** You do not get access to a resource by knowing its name and having the right credentials. You get access by holding a capability handle — an opaque token the kernel issued to you that encodes specific authority over a specific object. The kernel enforces this on every operation. You cannot manufacture a handle; you can only receive one from an authorized source or from the kernel directly. Possession of the handle is the authorization.

**Uniform protocol:** Resources expose typed operations through handles. Generic tools — the shell, stream operators, display services — can interact with any resource through this uniform interface. This is what enables composability.

See [Reference Projects](#reference-projects) for the specific systems that influenced these ideas.

### What Is Deliberately Avoided

- POSIX compatibility as a primary goal (a POSIX compatibility shim may be added later for ported C software, but it is not a design constraint)
- Global ambient authority (Unix UID/GID as the access control mechanism)
- Unix signals as the async notification mechanism
- A global VFS tree in the kernel
- Monolithic permission-check-at-access-time security model
- 1:1 synchronous syscall-to-library mapping
- A centralized type/schema registry requiring programmer coordination

Additional non-goals are listed in [Non-Goals and Deferred Work](#non-goals-and-deferred-work).

---

## Toolchain and Language

### Language: Rust

Rust is the language for the entire project — kernel, userspace services, the shell, and all system libraries. The choice is deliberate.

**Why Rust:**

The capability and handle system at the core of this design is one of the best possible fits for Rust's ownership model. A typed `Handle<T, M>` where access modes are part of the type, attenuation is a type-level operation, and use-after-close is a compile error directly expresses the design's central abstraction in the language rather than enforcing it through discipline. Over a multi-year solo project, discipline erodes; structure does not.

Rust's ownership model maps naturally onto the IRP driver stack (completion routines cannot hold references to stack frames that have returned), the notification queue (structured typed values rather than ad-hoc integers), and the namespace resolution engine (clear ownership of binding trees).

For AI-assisted development, Rust's compiler acts as a validator on generated code. Subtle memory errors that would require expert review in C are rejected at compile time.

**Stable Rust only.** The design deliberately avoids nightly-only features (in particular `generic_const_exprs` and `adt_const_params`). The `Handle<T, M>` design in `libos` uses typestate marker types rather than const-generic bitflags — see [Userspace Runtime Library](#userspace-runtime-library).

**The `unsafe` boundary:**

Kernel Rust uses `unsafe` blocks in well-defined places: the arch layer (page table manipulation, MMIO, inline assembly, interrupt handler stubs), the allocator internals, raw pointer operations on hardware-mapped memory, and the user-memory access primitives. These are marked explicitly and are auditable. The remainder of the kernel — handle table logic, namespace engine, IRP dispatch, scheduler policy, notification queue — is safe Rust with the compiler enforcing invariants. The arch layer plus these explicit `unsafe` zones are approximately 10-15% of the total kernel codebase.

**External crates:**

The kernel uses no external Rust crates. Kernel Rust is `#![no_std]`, `#![no_main]`, with no `alloc` crate until the kernel's own allocator is initialized. All kernel data structures (`KVec`, `KString`, intrusive linked lists, red-black tree, spin locks, etc.) are implemented in the kernel's own `libkern` crate.

One planned exception exists for the deferred Phase 2 of ACPI support: if AML interpretation becomes necessary (see [Power Management](#power-management)), ACPICA will be integrated via FFI as a documented exception. Phase 1 as initially shipped has no such exception.

Host-side build tooling (`bindgen`, etc.) may use external crates freely; these do not become part of the kernel.

### Compiler: rustc / LLVM Backend

- Single compiler targets any supported architecture — clean cross-compilation for future aarch64 support
- LLVM backend — same code generation quality, LTO support, and sanitizer infrastructure as Clang
- `rust-analyzer` for IDE integration
- `clippy` for lints
- `rustfmt` for style

**Key kernel build configuration:**
- `#![no_std]` — no standard library
- `#![no_main]` — custom entry point via NASM entry stub
- `panic = "abort"` — no stack unwinding in the kernel
- `-C force-frame-pointers=yes` — enables stack unwinding for the debugger
- `-C opt-level=2` for release, `-C opt-level=0 -g` for debug

**Custom target specification:**

A JSON target file describes the kernel's ABI, memory model, and code generation constraints:

```json
{
  "llvm-target": "x86_64-unknown-none",
  "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-f80:128-n8:16:32:64-S128",
  "arch": "x86_64",
  "os": "none",
  "vendor": "unknown",
  "linker-flavor": "ld.lld",
  "linker": "ld.lld",
  "panic-strategy": "abort",
  "disable-redzone": true,
  "features": "-mmx,-sse,+soft-float"
}
```

Cargo `build-std` compiles `core` and `compiler_builtins` from source for this target.

### Assembler: NASM

A small amount of assembly is unavoidable: the kernel entry point (before Rust's calling convention is established), context switch register save/restore stubs, user-memory copy routines (for the PC-discipline required by the exception table), and low-level arch primitives. NASM handles this. The NASM stubs call into Rust handlers immediately — the assembly surface is kept as small as possible.

### Build System: Cargo + cargo xtask

Cargo is the build system for all Rust code. A `cargo xtask` pattern in a `tools/` workspace provides a unified command surface for tasks that go beyond Cargo's native capabilities:

```
xtask build         — build kernel and userspace, assemble disk image
xtask qemu          — build and launch under QEMU
xtask qemu-debug    — launch QEMU with GDB stub enabled
xtask test          — run host-side unit tests
xtask test-qemu     — run integration tests in QEMU with isa-debug-exit
xtask pxe-deploy    — deploy to development hardware via PXE
xtask build-initramfs — compute initramfs closure and assemble CPIO image
```

**Cargo features** replace what KConfig would have provided for binary on/off configuration. Subsystem inclusion (`feature = "ahci"`), debug options (`feature = "kasan"`), and hardware support flags are all expressed as Cargo features with dependency tracking in `Cargo.toml`. `build.rs` handles any configuration that requires generated code.

### Monorepo Structure

The project is a monorepo with multiple distinct Cargo workspaces organized by compilation target boundary:

```
kernel/         — no_std kernel (custom x86_64-unknown-none target)
  Cargo.toml
  src/
    main.rs     — kernel entry (called from NASM boot stub)
    arch/       — amd64/, aarch64/ (unsafe, arch-specific)
    mm/         — memory management
    handle/     — global handle table
    ns/         — namespace objects and resolution
    ipc/        — IPC channels
    notif/      — notification queues
    sched/      — scheduler
    driver/     — IRP framework, driver stack
    kacpi/      — ACPI table parsing (Phase 1); ACPICA OSL (Phase 2, deferred)
    ...
  docs/
    lock-ordering.md

userspace/      — userspace services and libraries (std target)
  Cargo.toml
  libkern/      — raw syscall wrappers, no_std, no alloc
  libos/        — typed Handle<T,M>, async executor
  librt/        — sync wrappers, fiber scheduler
  libstream/    — typed I/O, TypedRecord derive macro
  librsproto/   — resource server wire protocol
  init/         — PID 1
  eshell/       — emergency shell for initramfs
  service-mgr/  — service manager
  fs-server-ext4/   — ext4 filesystem driver
  fs-server-fat/    — FAT filesystem driver
  ...

tools/          — host-native build utilities (xtask, image builder, etc.)
  Cargo.toml
  xtask/
```

ABI types shared across workspace boundaries (kernel ↔ userspace) are explicitly duplicated with a sync-check `xtask` command rather than cross-workspace path dependencies.

### Target Architecture

- **Primary**: amd64 (x86-64), UEFI only — no i386, no BIOS legacy support
- **Future**: aarch64 support designed in from the start via the Architecture Abstraction Layer

---

## Bootloader

### Limine

Limine is a modern, UEFI-native bootloader.

**What Limine provides:**
- Loads the kernel ELF image from disk
- Sets up a 64-bit environment with no real-mode or 32-bit transitional code
- Maps the kernel in the higher half of virtual address space
- Sets up a **Higher Half Direct Map (HHDM)** of all physical memory
- Provides a populated framebuffer
- Provides the full memory map (physical regions, firmware-reserved areas)
- Provides ACPI table pointers
- Starts all SMP cores with per-core stacks
- Loads additional modules (initramfs), passing their physical locations to the kernel

**The Limine Boot Protocol:**

Requests are embedded in the kernel binary as static structs with magic numbers. Limine scans the ELF at boot and fills in responses. The NASM entry stub is the first code to run; it calls the Rust `kernel_main` after the Limine response pointers are available.

**ESP filesystem:** UEFI mandates the EFI System Partition be FAT12/16/32. Limine reads its own binary, configuration, kernel, and initramfs from the ESP.

**Initramfs module:** Limine loads an initramfs module (format: CPIO `newc`) alongside the kernel. See [Bootstrap Mount Topology](#bootstrap-mount-topology) for what the initramfs must contain.

**Advantages over GRUB/Multiboot:**
- UEFI-native
- Higher-half kernel and HHDM set up by the bootloader
- SMP bootstrap handled by Limine
- Same protocol works on aarch64
- Active development, clean documentation

---

## Kernel Architecture

### Handle and Capability System

#### What a Handle Is

A handle is a **process-local opaque token** that identifies a kernel object and encodes authority to perform specific operations on it. The kernel enforces this authority on every syscall that receives a handle.

A handle value is a 64-bit integer:

```
[ 32-bit slot identifier | 32-bit generation counter ]
```

Where the 32-bit slot identifier decomposes as:

```
[ high 12 bits: segment id | low 20 bits: index-in-segment ]
```

- The **slot identifier** locates the slot in the global handle table
- The **generation counter** detects use-after-close — each slot reuse increments the counter; a stale handle value that references an old generation is rejected

In Rust, raw handle values are wrapped in a newtype:

```rust
#[repr(transparent)]
pub struct RawHandle(u64);
```

Userspace code works with the higher-level `Handle<T, M>` type (in `libos`) where `T` is the kernel object type and `M` is a typestate mode marker encoding principal type-specific rights.

#### Security: Owner Enforcement, Not Cryptographic Unforgeability

Handles are **not cryptographically unforgeable** — the handle value is a structured integer. The security guarantee comes from the kernel's **owner enforcement check** performed on every syscall:

1. Extract segment id, slot index, and generation from the handle value
2. Look up the segment, then the entry at that index
3. Check the generation matches (stale handle detection)
4. **Check `owner_pid` matches the calling process** ← the security-critical step
5. Check the requested operation is permitted by the rights bitmask
6. Dispatch to the type-specific operation via the object type discriminant

Even a correctly guessed handle value belonging to another process is rejected because the caller's PID does not match the slot's `owner_pid`.

For defense in depth, slot allocation is **randomized** — the free list is shuffled periodically with a bounded Fisher-Yates pass.

**The accurate security statement is:** a handle is only usable by the process that owns it in the kernel's global handle table.

#### The Global Handle Table

The handle table is a **segmented** structure.

```rust
struct HandleTable {
    directory:    [AtomicPtr<HandleSegment>; DIRECTORY_LEN],
    total_slots:  AtomicU32,
    alloc_hint:   AtomicU32,
}

#[repr(C, align(64))]
struct HandleSegment {
    free_list_head: AtomicU32,
    free_count:     AtomicU32,
    alloc_lock:     SpinLock,
    entries:        [HandleEntry; SEGMENT_LEN],
}

#[repr(C, align(64))]
struct HandleEntry {
    seq:          AtomicU32,      // seqlock; even = stable, odd = mid-update
    generation:   u32,
    owner_pid:    u32,
    rights:       Rights,
    object_type:  KObjectType,
    _pad1:        u32,
    object:       AtomicPtr<()>,
    next_owned:   RawHandle,
    free_next:    u32,
}
```

**Default configuration:** `DIRECTORY_LEN = 256`, `SEGMENT_LEN = 4096`. That's 1M handles system-wide.

#### Lookup Path (Seqlock, Lock-Free Common Case)

```rust
fn lookup(h: RawHandle, caller_pid: u32, required: Rights)
    -> Result<ObjectRef, KError>
{
    let (seg_id, slot_id, gen_expected) = decode(h);
    let segment = directory[seg_id].load(Acquire);
    if segment.is_null() { return Err(KError::InvalidHandle); }
    let entry = &(*segment).entries[slot_id];

    loop {
        let seq1 = entry.seq.load(Acquire);
        if seq1 & 1 != 0 { cpu_pause(); continue; }

        let gen    = entry.generation;
        let owner  = entry.owner_pid;
        let rights = entry.rights;
        let obj    = entry.object.load(Acquire);

        if obj.is_null() { return Err(KError::InvalidHandle); }
        let obj_ref = ObjectRef::try_acquire(obj)
            .ok_or(KError::InvalidHandle)?;

        let seq2 = entry.seq.load(Acquire);
        if seq2 != seq1 { obj_ref.release(); continue; }

        if gen != gen_expected   { obj_ref.release(); return Err(KError::InvalidHandle); }
        if owner != caller_pid   { obj_ref.release(); return Err(KError::InvalidHandle); }
        if !rights.contains(required) { obj_ref.release(); return Err(KError::NoAccess); }

        return Ok(obj_ref);
    }
}
```

`ObjectRef::try_acquire` atomically bumps the object's refcount iff nonzero — the safety anchor that prevents use-after-free across concurrent close.

#### Allocation and Close

Allocation takes the target segment's `alloc_lock`, pops a slot from the free list, and installs the entry under the seqlock write protocol. Close takes the slot's segment `alloc_lock`, validates under the lock, invalidates the slot, and defers object release.

**Randomized slot allocation:** free list shuffled at segment-allocation time and every 4096 closes per segment. PRNG seeded from CSPRNG at boot.

#### Object Lifecycle and Reclamation

Kernel objects are refcounted. Three sources of references:

1. **Handle-table entries** — each active handle holds +1. Dropped on close.
2. **In-flight syscalls** — lookup acquires a ref for syscall duration.
3. **Internal kernel pointers** — e.g., Thread → Process, PendingOperation → target.

When the last ref drops, the object enters a **per-CPU deferred-free queue**. A reclaim point (typically scheduler context-switch) checks if all CPUs have passed at least one quiescent state before freeing. Minimal RCU-style grace period.

#### Rights

**Generic rights** (all handle types):

| Right | Meaning |
|---|---|
| `DUPLICATE` | Handle may be copied via `sys_handle_duplicate` |
| `TRANSFER` | Handle may be sent to another process via IPC |
| `INSPECT` | Type and metadata may be queried |
| `WAIT` | Caller may block until handle signals |

**Type-specific rights (principal):**

| Handle Type | Principal rights |
|---|---|
| Resource / file | `READ`, `WRITE`, `EXECUTE` |
| Process | `SIGNAL`, `TERMINATE` |
| Namespace | `LOOKUP`, `BIND` |
| Memory object | `MAP_READ`, `MAP_WRITE`, `MAP_EXEC` |
| Thread | `SIGNAL`, `TERMINATE` |
| IPC channel | `SEND`, `RECV` |
| Notification channel | (receive end only) |

**Type-specific rights (modifier):**

| Handle Type | Modifier rights |
|---|---|
| Resource / file | `SEEK`, `APPEND`, `TRUNCATE` |
| Process | `INSPECT_MEMORY` |
| Namespace | `UNBIND`, `ENUMERATE` |

#### Attenuation

- `sys_handle_restrict(src, new_rights)` — consumes source, returns new handle with rights = `src.rights ∩ new_rights`. No `DUPLICATE` required.
- `sys_handle_duplicate(src, new_rights)` — preserves source, returns new handle. Requires `DUPLICATE`.

#### Handle Transfer

**Move (in-place ownership reassignment):**

1. Lookup source slot; verify `TRANSFER` right
2. Under source segment's `alloc_lock`:
   - Seqlock write: update `owner_pid` to destination; optionally update `rights`
   - `generation` unchanged
3. Handle value reused — same integer is now valid for destination process

**Duplicate (new slot for destination, source preserved):**

1. Lookup source slot; verify `DUPLICATE` and `TRANSFER`
2. Acquire object refcount for destination
3. Allocate destination slot; install entry under its alloc_lock
4. If source and destination segments differ, acquire both locks in ascending segment-id order

#### Close Concurrency

Handled by seqlock + segment alloc_lock + refcounting. Handle close only drops the handle-table refcount; the object persists until all refs drop.

#### Error Variants (handle subsystem)

- `InvalidHandle` — bad segment/slot/generation, closed slot, or wrong owner (not distinguished, defense-in-depth)
- `NoAccess` — valid handle, missing required right
- `OutOfHandles` — no free slot in any segment

---

### Kernel Objects

A **kernel object** is any data structure that:
1. Lives in kernel memory
2. Is reference-counted with kernel-managed lifetime
3. Can be referred to by a handle table entry
4. Has a defined type with associated operations

#### Object Header and Dispatch

```rust
#[repr(C)]
pub struct KObjectHeader {
    refcount:    AtomicUsize,
    object_type: KObjectType,
}

#[repr(C)]
pub struct Process {
    header: KObjectHeader,
    // process-specific fields
}
```

Dispatch via `match object_type { ... }`, not `dyn`. Reasons: thin pointer (8 bytes vs 16), exhaustiveness enforcement, inlining, no object-safety constraints.

#### The Complete Kernel Object Types

```rust
#[repr(u16)]
pub enum KObjectType {
    Process             = 0x01,
    Thread              = 0x02,
    Namespace           = 0x03,
    MemoryObject        = 0x04,
    IpcChannel          = 0x05,
    NotificationChannel = 0x06,
    Timer               = 0x07,
    InterruptObject     = 0x08,
    PendingOperation    = 0x09,
    IoRing              = 0x0A,
    EntropyObject       = 0x0B,
    DeviceNode          = 0x0C,
    ResourceServerReg   = 0x0D,
}
```

#### What Is Not a Kernel Object

Vma (internal to VMM), page table entries (hardware), Irp (driver-internal), buddy allocator state, slab caches, CPU run queues, DPC nodes, the handle table itself, driver stack frames, ACPI tables.

**The test:** "Can a userspace program hold a handle to this?" If yes, it is a kernel object.

#### Universal Operations

All kernel objects support `kobject_close`, `kobject_stat`, `kobject_wait` via match-dispatch on `object_type`.

---

### Namespace Subsystem

#### What the Namespace Is

The namespace is a kernel object (`Namespace`) that maps names to resources. It is a tree of name-to-handle bindings. It is not a filesystem — it is a *view*.

#### Namespace Object Operations

Rights-gated operations:

- **Lookup** (`LOOKUP`): Resolve a path, return a handle to the bound resource
- **Bind** (`BIND`): Attach a handle or resource server to a name
- **Unbind** (`UNBIND`): Remove a name binding
- **Enumerate** (`ENUMERATE`): List names at a given level

#### Per-Process Namespaces

Every process holds a current namespace handle:

```rust
struct Process {
    header:         KObjectHeader,
    address_space:  AddressSpace,
    ns_handle:      RawHandle,
    cwd_handle:     RawHandle,
    owned_handles:  HandleList,
    syscaps:        SysCaps,
    notif_channel:  NotifChannelId,
    creator:        ProcessId,
    children:       KVec<ProcessId>,
    state:          ProcessState,
}
```

The namespace handle is untyped at the kernel level — rights are tracked via the runtime `Rights` bitmask.

#### Namespace Entries

Each tree entry binds a name component to one of:

1. **A direct handle** — leaf entry resolving to a kernel object (e.g., `/dev/null`)
2. **A kernel resource server** — dispatched via direct function call
3. **A userspace resource server** — dispatched via IPC to an endpoint
4. **A sub-namespace object** — enables overlay composition
5. **A path rewrite rule** — transforms a path prefix before forwarding

#### Namespace as Primary Security Boundary

Resources not present in a process's namespace do not exist from that process's perspective. Sandboxing is by construction, not by permission check.

#### The Kernel's VFS Role

The kernel does **not** maintain a traditional VFS tree. It maintains per-process `Namespace` objects, a flat `ResourceServerRegistry`, the path resolution engine, and the lookup cache. No global mount table, no global dentry cache, no global inode table.

#### Synthetic `/proc/self`

Every process's namespace includes `/proc/self/` with synthetic entries:

| Path | Resolves to |
|---|---|
| `/proc/self/process` | Handle to own `Process` object |
| `/proc/self/thread` | Handle to calling `Thread` object |
| `/proc/self/pid` | Readable resource yielding u32 PID |
| `/proc/self/tid` | Readable resource yielding u32 TID |
| `/proc/self/namespace` | Current namespace handle |
| `/proc/self/cwd` | Subtree handle for CWD |

---

### Resource Server Model

#### The Universal Abstraction

A resource server responds to the resource server protocol: given a path context and a name, return a handle. Resource servers exist at three privilege levels:

- **Kernel resource servers**: Implemented by kernel modules; called via direct function pointer dispatch. Examples: `/dev` (device resource server), `/proc` (process resource server), `/initramfs` (initramfs resource server), `/dev/framebuffer`, `/dev/entropy`.
- **Privileged userspace resource servers**: System services started by init. Communicate via IPC. Examples: fs-server, netstack-server.
- **Unprivileged userspace resource servers**: User-started servers. No hardware access.

#### The ResourceServer Trait

```rust
pub enum OpStatus {
    Completed,
    Pending,
    Rejected(KError),
}

pub trait ResourceServer: Send + Sync {
    fn lookup(&self, req: &mut LookupIrp) -> OpStatus;
    fn submit(&self, irp: &mut Irp)        -> OpStatus;
    fn cancel(&self, irp_id: IrpId);
}
```

Kernel resource servers return `Completed` for cache-hot answers; userspace resource servers always return `Pending` because the IPC round-trip is inherently async.

**Syscall-level fast path:** `sys_ns_lookup` returns a `PendingOperation` handle regardless. If the underlying lookup completed synchronously, the returned handle is pre-signaled so the caller's next `sys_wait` returns without blocking.

#### Resource Server Startup Protocol

**Principle: resource servers do not register themselves. They are registered by a supervisor holding `SysCaps::BIND_NAMESPACE`.**

This is an explicit application of principle of least authority. A resource server's only legitimate namespace operation is its own initial registration — but if granted `BIND_NAMESPACE`, it would also have the authority to bind anything anywhere, which it has no reason to do and could misuse (or be exploited to misuse). The supervisor pattern removes the capability entirely from the resource server and concentrates it in processes that actually need it for coordination work.

**The protocol:**

1. **Supervisor spawns the resource server** with:
   - Any device handles or block device handles it needs
   - A **control IPC channel** between supervisor and RS (bidirectional)
   - The RS's own process namespace (minimal — only what the RS needs to run)
   - Log channel
   - **No `BIND_NAMESPACE` capability**

2. **Resource server initializes** — reads any state it needs (superblock for fs-server, device configuration for driver-backed RS, etc.), sets up internal data structures.

3. **Resource server creates an endpoint** — an IPC channel end that the kernel will route lookup/submit requests to when this RS is bound into a namespace. Internally this involves the RS calling `sys_channel_create` and retaining the receive end; the send end becomes the endpoint handle the kernel will use.

4. **Resource server signals "Ready" on the control channel**, including the endpoint handle in the message.

5. **Supervisor receives the Ready message** and the endpoint handle.

6. **Supervisor calls `sys_ns_bind(target_namespace, path, endpoint, rights=...)`** to register the endpoint as the resource server for `path` in the target namespace, with the rights the supervisor chooses to grant.

7. **The RS is now live** — lookup/submit requests resolving to `path` are routed to its endpoint.

**Ongoing use of the control channel:**

The control channel is not discarded after registration. It remains the management channel between supervisor and RS:

- Supervisor sends: shutdown, reload, health-check requests
- RS sends: error notifications, degraded-state signals, statistics

This is structurally analogous to systemd's control interface for services, but at the IPC-channel level rather than via a filesystem socket.

**Why `BIND_NAMESPACE` is a supervisor-only capability:**

Holders of `BIND_NAMESPACE` at boot: init, service manager (delegated from init), session manager (delegated from service manager for per-session namespaces). These are coordination processes whose job includes namespace construction.

Non-holders: every fs-server, every netstack-server, every device driver, every profile server, every user application. These consume namespace, they don't construct it.

#### Subtree Handles

Resource servers issue **subtree handles** scoped to a specific tree root. The handle context embeds the anchor:

```rust
struct FsHandleContext {
    root_inode:    u64,
    current_inode: u64,
    mount_id:      u32,
    rights:        Rights,
}
```

The server uses `root_inode` as the starting point. `..` traversal above the root returns an error. This is how subtree scoping works — an fs-server can issue a handle scoped to a specific subtree of the filesystem, and clients holding that handle cannot escape above the subtree boundary.

#### Security Implications of Userspace Resource Servers

**Reduced risk:** Bugs in userspace resource servers cannot directly corrupt kernel memory.

**Added risk:** The IPC boundary between kernel and userspace resource servers must be rigorously validated.

**DMA safety requires IOMMU:** Userspace drivers with direct hardware access need the IOMMU to constrain which physical memory a device can DMA to.

#### What Cannot Be a Resource Server

Kernel functionality that must run synchronously in interrupt or fault context: interrupt handlers, page fault handler, CPU context switcher, physical memory allocator, handle table validation, IPC routing, TLB/cache management.

---

### Memory Management

#### Address Space Layout (amd64)

```
USER SPACE (single paging root)
0x0000_0000_0000_0000 - 0x0000_0000_0000_0FFF  NULL page (always unmapped)
0x0000_0000_0000_1000 - 0x0000_0000_000F_FFFF  Reserved (low-address trap region)
0x0000_0000_0010_0000 - 0x0000_7FFF_FFFF_FFFF  User mappings
  |- ELF text/rodata/data/bss  (ASLR base, 28 bits entropy)
  |- brk heap                  (grows up from end of .bss)
  |- mmap arena                (grows down from ~0x0000_7000_0000_0000, ASLR'd)
  |- main thread stack         (top of arena, grows down, guard page below)
  +- additional thread stacks  (allocated from mmap arena with guard pages)

KERNEL SPACE (shared kernel half in every address space)
0xFFFF_8000_0000_0000 - 0xFFFF_BFFF_FFFF_FFFF  HHDM (Limine direct map; 64 TiB)
0xFFFF_C000_0000_0000 - 0xFFFF_CFFF_FFFF_FFFF  Kernel vmap (16 TiB)
0xFFFF_D000_0000_0000 - 0xFFFF_DFFF_FFFF_FFFF  Per-CPU data (16 TiB)
0xFFFF_E000_0000_0000 - 0xFFFF_EFFF_FFFF_FFFF  Driver MMIO vmap (16 TiB)
0xFFFF_F000_0000_0000 - 0xFFFF_FFFF_7FFF_FFFF  Reserved
0xFFFF_FFFF_8000_0000 - 0xFFFF_FFFF_FFFF_FFFF  Kernel image (2 GiB)
```

**Guard pages:** NULL page, below every thread stack, below every kernel stack.

**ASLR entropy:** 28 bits for ELF base, stack, mmap arena; 13 bits for brk. Applied at spawn. PRNG seeded from CSPRNG.

**Per-thread kernel stack:** 16 KB + 1 guard page. Allocated from kernel vmap region.

**Higher-half sharing:** kernel page table entries are shared across all address spaces. Syscall entry is a ring transition, not a page-table switch.

**TLB considerations:** PCID (amd64) with global-bit kernel entries. aarch64 uses TTBR0/TTBR1 split with ASIDs.

#### Physical Memory: Buddy Allocator

Zone organization: DMA (below 16MB) + Normal (bulk of RAM). HHDM gives virtual access via fixed offset.

NUMA not supported initially.

#### Kernel Object Allocation: SLUB-Inspired Slab Allocator

Per-CPU partial slab lists, global lists of full and empty slabs. Exposed via `KBox<T>` and `KVec<T>` in `libkern`.

#### Virtual Memory Manager

```rust
struct Vma {
    range:       VAddrRange,
    prot:        Protection,
    mapping:     MappingKind,   // Anonymous, FileBacked(handle), Device(paddr)
    cow:         bool,
}
```

Red-black tree of VMAs sorted by start address, with interval tree augmentation for O(log n) overlap queries.

**Page fault handler:** arch interrupt handler → Rust handler. Checks exception table; redirects to recovery PC if fault was in a registered user-memory copy range. Otherwise looks up VMA, handles CoW/anonymous/file-backed faults, or delivers SegFault notification.

#### Page Cache, Reclaim, and Swap

Page cache holds file-backed pages, shared across processes. Clock-algorithm reclaim daemon. Anonymous page swap via PTE swap markers.

Memory pressure → `Notification::MemoryPressure` → userspace OOM daemon applies kill policy. Kernel does not make kill decisions.

#### File-Backed Memory and the FS Server Split

On `sys_memory_map` of a file handle:

1. Kernel asks FS server (IPC): "give me the LBA extents for offset X, length Y"
2. FS server returns extent list referencing block device
3. Kernel submits block reads via IRP stack, populates page cache pages
4. Kernel maps page cache pages into client's address space with the rights the client's file handle allows

The FS server never holds mappable memory objects directly. The kernel is the gatekeeper of MAP_READ/MAP_WRITE enforcement.

For writable filesystems, kernel handles writeback via periodic dirty-page flushing, sending write IRPs through the block driver. FS server is asked to allocate blocks for extending files; it doesn't touch the page cache.

#### TLB Shootdown

Per-address-space `active_cpus: CpuMask` tracks where the address space has been loaded. Invalidation: local flush, then IPI to other CPUs in the mask with per-CPU mailbox protocol. PCID optimization on amd64 skips IPIs to CPUs that aren't in the mask. aarch64 uses TLBI broadcast.

#### User Memory Access

```rust
#[repr(transparent)]
pub struct UserPtr<T>(*const T);

#[repr(transparent)]
pub struct UserMutPtr<T>(*mut T);
```

No Deref, no unsafe back-door. Only copy primitives touch raw pointers.

**Copy primitives:**

```rust
pub fn copy_from_user<T: Copy>(src: UserPtr<T>) -> Result<T, FaultInfo>;
pub fn copy_slice_from_user(src: UserPtr<u8>, len: usize, dst: &mut [u8]) -> Result<usize, FaultInfo>;
pub fn copy_to_user<T: Copy>(dst: UserMutPtr<T>, val: &T) -> Result<(), FaultInfo>;
pub fn copy_slice_to_user(dst: UserMutPtr<u8>, src: &[u8]) -> Result<usize, FaultInfo>;
pub fn copy_cstr_from_user(src: UserPtr<u8>, max_len: usize) -> Result<KString, FaultInfo>;
```

**Range validation** catches kernel-pointer-from-userspace attacks trivially.

**Fault-recoverable copy via exception table:** (fault_pc, recovery_pc) pairs registered at compile time. Page fault handler checks table; if fault PC is in a registered range, execution resumes at recovery PC.

**SMAP/SMEP discipline (amd64):** SMEP always on, SMAP opened via `stac` only within copy primitives, closed via `clac`.

**aarch64 equivalent:** PAN bit; `ldtr`/`sttr` unprivileged load/store instructions.

**TOCTOU handling:** all user data copied once into kernel-owned buffers before syscall handlers act on it.

---

### Process and Thread Management

#### Process Object

```rust
struct Process {
    header:         KObjectHeader,
    address_space:  AddressSpace,
    ns_handle:      RawHandle,
    cwd_handle:     RawHandle,
    owned_handles:  HandleList,
    syscaps:        SysCaps,
    notif_channel:  NotifChannelId,
    creator:        ProcessId,
    children:       KVec<ProcessId>,
    state:          ProcessState,
}
```

#### Thread Object

```rust
struct Thread {
    header:         KObjectHeader,
    process:        *mut Process,
    register_state: ArchRegisterState,
    fpu_context:    FpuContext,
    fs_base:        usize,
    kernel_stack:   KernelStack,
    sched_params:   SchedParams,
    state:          ThreadState,
    exception:      Option<ExceptionState>,
    wait_nodes:     KVec<WaitNode>,
    wait_state:     AtomicU8,
    wait_result:    Option<WaitResult>,
}
```

#### FPU / SIMD State

Eager save/restore on every context switch. XSAVE area per-thread, 64-byte aligned. Feature mask: x87 | SSE | AVX always; AVX-512 and PKRU when available. Kernel does not use FPU (`-mno-sse -mno-mmx -msoft-float`).

#### TLS

**amd64:** GS for per-CPU kernel data (swapped via `swapgs` on syscall/exception entry); FS for user TLS (`fs_base` in Thread struct).

```rust
sys_thread_set_tls(tls_base: usize) -> Status
```

**aarch64:** TPIDR_EL0 for user, TPIDR_EL1 for per-CPU kernel.

#### Handle Inheritance and SpawnArgs

Handles **not inherited by default**. Parent explicitly lists handles to grant:

```rust
#[repr(C)]
pub struct SpawnArgs {
    pub executable:      RawHandle,
    pub namespace:       RawHandle,
    pub initial_cwd:     UserPtr<u8>,
    pub initial_cwd_len: usize,
    pub handles:         UserPtr<HandleGrant>,
    pub handle_count:    usize,
    pub argv:            UserPtr<u8>,
    pub argv_len:        usize,
    pub envmap:          UserPtr<u8>,
    pub envmap_len:      usize,
    pub syscaps:         SysCaps,
    pub flags:           SpawnFlags,
    pub _reserved:       [u64; 4],
}

#[repr(C)]
pub struct HandleGrant {
    pub source_handle:     RawHandle,
    pub slot_name:         [u8; 16],
    pub attenuated_rights: Rights,
}
```

Argv/env are typed structural values, not C strings. `initial_cwd` is a path resolved against the child's namespace. Slot names are conventions: `"stdin"`, `"stdout"`, `"stderr"`, `"log"`, `"notification"`, `"namespace"`, and for supervisor-spawned resource servers `"control"`. SysCaps must be ⊆ parent's.

#### Process Tree and Reaping

Kernel tracks `creator` (separate from handle-holders). On creator exit, orphans reparent to init. Handle-holders with `WAIT` right get `Notification::ChildExited`.

**At process exit:**

1. Process → Zombie; exit status latched
2. Walk `children[]`, reparent to init
3. Send `ChildExited` to all handle-holders with `WAIT` right
4. Last `Process` handle close → object freed

Init's role: receive `ChildExited`, close the process handle. That is reaping.

---

### Scheduler

Three classes:

```rust
#[repr(u8)]
pub enum SchedClass {
    RealTime    = 0,
    TimeShared  = 1,
    Idle        = 2,
}
```

**TimeShared (default):** CFS-like; per-CPU red-black tree keyed on vruntime; 4ms time slice.

**RealTime:** fixed priority 0-99; FIFO within priority; requires `SysCaps::REAL_TIME`.

**Idle:** single per-CPU idle thread; `hlt`.

**SMP:** per-CPU run queues, work stealing, affinity placement on wake.

**Context switch targets:** full switch < 1μs (modern amd64), same-AS switch < 300ns.

**CPU affinity:**

```rust
sys_thread_set_affinity(thread: RawHandle, cpu_mask: CpuMask) -> Status
```

NUMA-aware scheduling not implemented.

---

### Driver Subsystem

#### Three Distinct Concepts

- **Kernel module**: deployable unit of Rust code loadable at runtime
- **Device driver**: code managing specific hardware
- **Kernel resource server**: subsystem exposing resources through the namespace

#### Tier 1 vs Tier 2 Modules

**Tier 1 — compiled-in subsystems:**

| Subsystem | Feature flag |
|---|---|
| PCI enumeration | `pci` (always on for amd64) |
| AHCI driver | `ahci` |
| NVMe driver | `nvme` |
| Partition (GPT) driver | `gpt` (always on) |
| Volume manager | `lvm` |
| Initramfs resource server | (always on) |
| Logical console | (always on) |

**Tier 2 — LKMs:** hot-pluggable hardware drivers, optional subsystems, debug tools. Loaded by driver manager via syscall requiring `SysCaps::LOAD_MODULE`.

**Rule:** boot-path = Tier 1. Hot-pluggable/optional = Tier 2. Filesystem drivers are userspace processes regardless of tier.

#### LKM Infrastructure

Export table (`export!` macro, `.kernel_exports` section). ABI version = hash over exported signatures + config + layout of ABI-critical types, baked into kernel as `KERNEL_ABI_VERSION: [u8; 32]`. Module embeds the version it was built against; loader refuses mismatches.

Load: parse ELF, verify ABI, allocate code pages, relocate, resolve externals, run `init_fn`. Roll back on failure.

Unload (synchronous, drain-based): transition Unloading → revoke registrations → drain in-flight IRPs (30s timeout, force-cancel with `SysCaps::LOAD_MODULE`) → wait refcount=0 → run `exit_fn` → free code pages.

Shared IRQs (PCI INTx sharing): chain of handlers, each returning "handled" or "not mine". MSI/MSI-X never shared.

#### The IRP Model

```rust
struct Irp {
    operation:  IrpOp,
    initiator:  ProcessId,
    completion: PendingOpHandle,
    buffer:     KBox<[u8]>,
    offset:     u64,
    params:     IrpParams,
    stack:      IrpStack,
    status:     IrpStatus,
    dpc:        DpcNode,
}
```

IRPs flow through a driver stack; each module completes or forwards. Completion routines fire on the way back up.

#### Device Tree and Driver Discovery

**amd64**: ACPI tables. **aarch64**: Device Tree Blob. `DeviceNode` kernel objects are architecture-independent.

Driver manager matches DeviceNodes to drivers; loads modules; passes `Handle<DeviceNode, M>`.

#### MMIO Allocation

```rust
sys_device_map_mmio(
    device: RawHandle,
    region_idx: u32,
    flags: MmioFlags,
) -> RawHandle
```

Kernel consults the DeviceNode's resource descriptor. Returns `MemoryObject`. For userspace drivers, kernel programs IOMMU simultaneously.

#### Interrupt Objects

Hardware IRQ sources as `InterruptObject` kernel objects. Drivers hold `Handle<InterruptObject, ...>`, block on it. Transferable — same programming model for kernel and userspace drivers.

#### IOMMU Programming

Kernel programs VT-d / AMD-Vi / ARM SMMU whenever a device handle is granted to a userspace resource server.

---

### IPC Subsystem

```rust
#[repr(C)]
pub struct IpcMsg {
    pub header:   IpcMsgHeader,
    pub payload:  [u8; IPC_PAYLOAD_SIZE],    // 4032 bytes; fixed
    pub handles:  [RawHandle; IPC_HANDLE_MAX],  // 8 handles max
}

#[repr(C)]
pub struct IpcMsgHeader {
    pub sender_pid:   ProcessId,
    pub payload_len:  u32,
    pub handle_count: u8,
    pub flags:        u16,
    pub _pad:         u8,
    pub timestamp:    u64,
}

pub const IPC_PAYLOAD_SIZE: usize = 4032;
pub const IPC_HANDLE_MAX:    usize = 8;
```

Total message: 4096 bytes.

**Per-channel queue_depth** (default 16). **Bulk data:** companion `MemoryObject` handles in the handle list.

**Send modes:**

```rust
pub enum SendMode {
    Block,
    NoBlock,
    BlockBounded,
}
```

**Dead peer:** `PeerClosed` error on pending sends + `Notification::PeerClosed` to sender.

**Unified wait:** `sys_wait` accepts any combination of handle types.

---

### Notification Queue

Distinct from IPC: kernel-to-process only; reliable delivery even in degraded state; semantic clarity.

Each process has exactly one `NotificationChannel` kernel object. Kernel-copy model (not shared-memory SPSC): bounded per-process queue in kernel memory, default 64 entries. `sys_notif_recv` copies to user buffer.

#### The Notification Enum

```rust
#[repr(C, u32)]
pub enum Notification {
    Unknown { kind: u32, _reserved: [u8; 60] } = 0,

    // Hardware exceptions — 0x0100 range
    SegFault     { thread: ThreadId, addr: VAddr, kind: FaultKind } = 0x0100,
    IllegalInsn  { thread: ThreadId, addr: VAddr } = 0x0101,
    DivideByZero { thread: ThreadId, addr: VAddr } = 0x0102,
    StackOverflow{ thread: ThreadId } = 0x0103,

    // Process lifecycle — 0x0200 range
    ChildExited  { child: ProcessId, status: ExitStatus } = 0x0200,
    PeerClosed   { handle: RawHandle } = 0x0201,

    // External — 0x0300 range
    TermRequest = 0x0300,

    // Resource — 0x0400 range
    HandleInvalidated { handle: RawHandle } = 0x0400,
    NotificationsDropped { count: u32 } = 0x0401,

    // Power / system events — 0x0500 range
    PowerEvent      { kind: PowerEventKind } = 0x0500,
    MemoryPressure  { level: PressureLevel, free_pages: u64 } = 0x0501,
}
```

**Rules:** Fixed 64-byte variant size on wire. Sparse, category-based discriminants (256 per category). Forward-compat via `Unknown` translation at copy time. Kernel's internal match is exhaustive.

#### Exception Handling

Priority chain: debugger exception channel → process notification channel → default action (terminate). Faulting thread suspended.

```rust
sys_exception_resume(
    thread: RawHandle,
    disposition: Disposition,
) -> Status

#[repr(C, u32)]
pub enum Disposition {
    Resume                 = 0,
    ResumeSkip             = 1,
    Terminate { code: i32 } = 2,
    ModifyAndResume { register_update: UserPtr<RegisterUpdate> } = 3,
}
```

Register inspection via `sys_thread_get_registers`. Timeout default 30s; extendable via `sys_exception_extend_timeout`.

#### Overflow

Exception variants evict oldest non-exception. Other overflows drop; next recv returns `NotificationsDropped`.

---

### DPCs and Wait Queues

#### Deferred Procedure Calls

Per-CPU queue, drained on IRQ return and scheduler entry. Inspired by NT.

```rust
pub struct DpcNode {
    next:    *mut DpcNode,
    handler: fn(ctx: *mut ()),
    ctx:     *mut (),
}
```

DPCs are inline nodes in owning structures (IRP, Timer). No heap allocation on the fast path.

**Three execution contexts** (precedence): IRQ > DPC > Thread. DPC context cannot block; can take spinlocks briefly.

#### Wait Queues

Per-object intrusive wait queue. Thread holds an array of wait nodes (one per handle).

```rust
pub struct WaitQueue {
    head: SpinLock<IntrusiveList<WaitNode>>,
}

pub struct WaitNode {
    list_link: IntrusiveListLink,
    thread:    *mut Thread,
    handle:    RawHandle,
    ready:     AtomicBool,
}
```

`sys_wait` entry: translate handles, pre-allocate wait nodes, insert into each object's queue, check already-signaled for fast path, set WAITING, yield.

Signal from DPC: pop node, mark ready, CAS thread state WAITING → WOKEN, make runnable.

Resume: walk wait_nodes, unlink from queues, release refcounts, return completed handles.

---

### Time and Timers

#### Architecture Hardware

**amd64:** TSC (invariant preferred), HPET for calibration, APIC timer per-core, PIT for early boot only.

**aarch64:** ARM Generic Timer.

#### ArchTimer Trait

```rust
trait ArchTimer {
    fn init(&mut self);
    fn read_ns(&self) -> u64;
    fn set_oneshot(&mut self, deadline_ns: u64);
    fn cancel(&mut self);
    fn frequency_hz(&self) -> u64;
}
```

#### Clock Types

```rust
sys_clock_read(clock: ClockId, out: UserMutPtr<u64>) -> Status

#[repr(u32)]
pub enum ClockId {
    Monotonic     = 0,
    Realtime      = 1,
    ProcessCpu    = 2,
    ThreadCpu     = 3,
}
```

Monotonic is default. Realtime = Monotonic + offset maintained by time-sync service. Setting offset requires `SysCaps::SYSTEM_CLOCK`.

#### Kernel Timer Subsystem

Min-heap of `Timer` kernel objects keyed by expiry. Processed on arch timer IRQ via DPC.

---

### Security Model

#### System Capability Bitmask

```rust
bitflags! {
    pub struct SysCaps: u64 {
        const LOAD_MODULE     = 1 << 0;
        const BIND_NAMESPACE  = 1 << 1;
        const PHYSICAL_MEMORY = 1 << 2;
        const REAL_TIME       = 1 << 3;
        const SYSTEM_CLOCK    = 1 << 4;
        const AUDIT_CONTROL   = 1 << 5;
    }
}
```

Inherited explicitly — can only grant caps the parent holds.

**`BIND_NAMESPACE` concentration:** At boot, init holds `BIND_NAMESPACE`. Init delegates to the service manager and session manager. No ordinary resource server — fs-server, netstack-server, etc. — ever holds `BIND_NAMESPACE`. Registration of resource servers into namespaces is always performed by a supervisor on behalf of the RS. See [Resource Server Startup Protocol](#resource-server-startup-protocol).

#### Capability Bootstrap

At boot, the kernel grants init the initial handle set:
- Root namespace handle (full rights)
- Hardware resource handles (storage devices, interrupt objects)
- Kernel log channel handle
- System control handle
- Initramfs namespace handle
- Full `SysCaps` set

All authority traces back to this initial kernel grant.

#### Policy vs. Mechanism

Kernel enforces mechanism: rights on handles, syscaps on processes, namespace membership. Policy lives in userspace: session manager, service declarations, profile manifests.

#### Privilege Escalation

Escalation is **handle acquisition**, not state change. The privilege broker authenticates, constructs a new namespace with admin resources, spawns a new process with elevated handles.

#### Tiered Device Access

| Tier | Contents |
|---|---|
| `minimal_dev` | `/dev/null`, `/dev/zero`, `/dev/urandom` |
| `user_dev` | Adds `/dev/tty`, `/dev/pts`, `/dev/sound`, `/dev/dri` |
| `admin_dev` | Adds raw block devices, `/dev/mem` |
| `full_dev` | Everything including hardware debug interfaces |

---

### Power Management

#### Phased Approach

**Phase 1 (initial):** Pure Rust ACPI table parsing. Tables: RSDP, XSDT, MADT, FADT, HPET, MCFG, DMAR/IVRS, SRAT. Capabilities: SMP boot, timers, PCIe enumeration via ECAM, IOMMU programming, reboot via FADT ResetReg, basic shutdown (QEMU port write or halt loop). No external crates. `kacpi` subsystem in `kernel/src/kacpi/`.

**Phase 2 (deferred):** ACPICA via FFI. Trigger: laptop features, graceful S5, AML-enumerated devices, C-state/P-state management. Vendored in `kernel/vendor/acpica/`. OSL in `kernel/src/kacpi/osl/` (~30 callbacks). `bindgen` as host-side build tool. `#![allow(unsafe_op_in_unsafe_fn)]` on the OSL boundary. "~10-15% unsafe" claim revised accordingly.

**aarch64:** PSCI; server aarch64 uses ACPI; embedded uses Device Tree.

#### ArchPower Trait

```rust
trait ArchPower {
    fn shutdown(&self) -> !;
    fn reboot(&self) -> !;
    fn suspend(&self, state: SleepState);
    fn cpu_idle(&self, c_state: CState);
    fn device_set_power(&self, node: &DeviceNode, state: DevicePowerState);
}
```

Phase 1: shutdown = halt loop / QEMU port; reboot = FADT ResetReg; suspend/device power unimplemented. Phase 2 replaces with ACPICA.

Power events delivered via `Notification::PowerEvent` (Phase 2).

---

### Entropy

**Hardware sources:** RDSEED preferred on amd64, RDRAND fallback; RNDR on aarch64, SMCCC TRNG fallback.

**Software sources:** TSC jitter at interrupt dispatch times (hundreds of samples accumulated); HPET/TSC skew.

**Not entropy sources:** HHDM addresses, boot parameters, anything deterministic-at-boot.

**Design:** ChaCha20 CSPRNG. Boots: RDSEED/RDRAND + TSC jitter → pool seeded (≥256 bits estimated) → EntropyObject non-blocking. Reseed periodically.

**Userspace interface:** `Handle<EntropyObject, Only>` in every process's initial namespace. Blocks until seeded; non-blocking thereafter.

---

### Debugging Infrastructure

- **Kernel log buffer**: circular ring, 1-4MB, per-entry timestamp/level/subsystem/message. Exposed as kernel RS. `kprintln!` macro.
- **Early serial console**: UART 16550 (amd64) or PL011 (aarch64). Initialized as early as possible.
- **UEFI framebuffer**: Limine-provided, for early graphical output.
- **GDB remote stub**: RSP over serial. Panic/breakpoint drops in. Symbols demangled via `rustfilt`.
- **Stack unwinder**: frame pointer chain + kernel symbol table in ELF.
- **Sanitizers**: Rust bounds checking always; `-Z sanitizer=address` in QEMU; `debug_assert!`.
- **Watchdog**: TCO (amd64).
- **Crash dumps**: panic handler serializes to crash partition / reserved memory / network.
- **Lock ordering**: `kernel/docs/lock-ordering.md`. Debug builds track and panic on violations.

#### SMP Panic Protocol

Panicking CPU disables IRQs → sends NMI to all others → NMI handlers save state to per-CPU debug regions and halt → panicking CPU dumps all state → halt/GDB/reset.

NMI used because regular IPIs can be masked by code holding a spinlock with IRQs off.

---

### Architecture Abstraction Layer

```rust
trait ArchPaging {
    unsafe fn map_page(&mut self, phys: PAddr, virt: VAddr, flags: PageFlags);
    unsafe fn unmap_page(&mut self, virt: VAddr);
    unsafe fn flush_tlb_page(&self, virt: VAddr);
    unsafe fn flush_tlb_range_local(&self, range: VAddrRange);
    unsafe fn flush_tlb_all_local(&self);
    unsafe fn set_page_table(&mut self, root: PAddr);
    fn send_shootdown_ipi(&self, targets: CpuMask);
}

trait ArchIrq { /* enable/disable, register handler, EOI */ }
trait ArchCpu { /* id, halt, init, features */ }
trait ArchSmp { /* init, send IPI, cpu count */ }
trait ArchFpu { /* init_thread, save, restore, storage_size */ }
trait ArchUserAccess { /* open_user_window (stac/PAN), close_user_window */ }
trait ArchPower { /* shutdown, reboot, suspend, idle, device power */ }
```

NASM stubs: `boot.asm`, `context_switch.asm`, `user_copy.asm`.

---

## Userspace Architecture

### Content-Addressed Store

Every package at a path containing a hash of contents + transitive dependencies:

```
/store/8d3f2a1c-glibc-2.38/
/store/f4e9b2d1-openssl-3.1.2/
/store/a7c3e8f2-nginx-1.24.0/
```

**Properties:** No version conflicts, reproducible builds, atomic updates, instant rollback.

**Storage:** the store lives on an ext4 filesystem served by fs-server-ext4. The `/store` subtree is physically the same ext4 inodes as everything else on that filesystem.

**Read-only-once-written enforcement:** this is a **namespace-level property**, not an fs-server-level property. fs-server is a dumb filesystem server and doesn't treat `/store` specially. The immutability emerges from how `/store` is bound in every normal process's namespace:

- Normal process namespaces bind `/store` with rights that permit `LOOKUP`, `READ`, `MAP_READ` — but not `WRITE` or `MAP_WRITE`
- The kernel, enforcing rights on every operation through the handle obtained from namespace lookup, refuses writes and write-mappings
- Even if fs-server were compromised, the kernel's rights enforcement on the handle is independent of fs-server's behavior

**Writing new store paths:** the package manager, which is the one legitimate writer of the store, has a namespace that binds the store subtree with `BIND`, `WRITE`, and `MAP_WRITE` — a distinct namespace from what normal processes see. Same underlying ext4 inodes; different namespace route; different rights; different effective behavior.

**`SysCaps::PHYSICAL_MEMORY`** lets a process map arbitrary physical memory directly, bypassing the namespace-mediated store interface. This is out of scope for the "through the store interface" guarantee — that cap is held only by privileged boot-path code, never by normal processes.

**Garbage collection:** store paths unreachable from any live generation of any profile are eligible for collection by the package manager daemon's GC pass.

### Filesystem Drivers

Two filesystems shipped initially:

**FAT (RO, FAT32/FAT16/FAT12):** required — UEFI mandates FAT32 for the ESP. ~1500 lines Rust for RO. RW is later phase (ESP changes rare).

**ext4 (RO then RW):** primary filesystem for store and general disk use. RO ~3-5K lines, RW 2-3x.

**Architectural model — one fs-server process per block device:**

There is exactly one fs-server process per block device being served. It holds the block device handle, knows the filesystem format, and serves the resource-server protocol to clients. If you have multiple filesystems of the same type (two ext4 partitions on different disks), you run two fs-server-ext4 processes — the same binary, different arguments, different block device handles, different state.

**Single resource server registration per fs-server:**

An fs-server registers **one resource server** representing the entire filesystem it serves. It does not register per-subtree servers with per-subtree policy. fs-server is a dumb filesystem server: it reads and writes blocks, issues handles scoped to subtrees when asked, and honors the rights the kernel enforces on those handles.

All scoping, all mount-point-like divisions, all per-subtree policy (read-only vs. read-write, which users see which subtrees) is done by **namespace composition**. Init, service manager, and session manager construct namespaces that bind different subtrees of the underlying filesystem with different rights in different processes' views.

**Example:** on a single-ext4-partition system, fs-server-ext4 registers once. The system namespace init constructs binds this RS's root as `/` (the system-wide root). When session manager constructs Alice's namespace, it derives a namespace that binds:
- `/store` → RS with `LOOKUP`+`READ`+`MAP_READ`, subtree handle scoped to `/store` on the underlying filesystem, via attenuation
- `/home` → RS with `LOOKUP`+`READ`+`WRITE`+`MAP_READ`+`MAP_WRITE`, subtree handle scoped to `/home/alice`
- No `/system`, no raw filesystem root

Each binding points into the same fs-server process, but with different subtree scopes and different rights. Bob's namespace does the same with `/home` scoped to `/home/bob`. Same fs-server, same underlying inodes, different views.

**Phasing:**

| Phase | Filesystems | Mode |
|---|---|---|
| 1 | FAT (RO), ext4 (RO) | Boot from ESP, read store, run system |
| 2 | FAT (RO), ext4 (RW) | Writable home directories, writable state |
| 3 | FAT (RW) | Bootloader updates from within OS |
| 4+ | btrfs, NTFS, XFS, ... | Additional filesystems as separate fs-servers |

Each filesystem is a separate userspace fs-server binary. The kernel knows nothing about filesystem formats.

### Bootstrap Mount Topology

The single-filesystem case (everything on one ext4 root partition) is the simplest and most common. The design generalizes cleanly to multi-partition layouts where critical system paths (`/usr`, `/home`, `/var`, etc., or their equivalents in this design) are on separate partitions. Historically this was common; it remains common in some deployments.

The mechanism that handles this is a **bootstrap mount manifest** read by init during early boot, with the initramfs providing the closure of software needed to reach the normal-boot state.

#### The Bootstrap Manifest (`init.toml`)

Located at `/etc/init.toml` on the initramfs. Read by init after it comes up. Describes what init must accomplish before handing off to the service manager.

```toml
# Example: everything on a single ext4 root partition
[[mount]]
fs_server    = "fs-server-ext4"
device       = "gpt-partuuid:01234567-89ab-cdef-0123-456789abcdef"
mount_point  = "/"
mode         = "rw"
required_for = "boot"

# Example: multi-partition layout
[[mount]]
fs_server    = "fs-server-ext4"
device       = "gpt-partlabel:root"
mount_point  = "/"
mode         = "rw"
required_for = "boot"

[[mount]]
fs_server    = "fs-server-xfs"
device       = "gpt-partlabel:store"
mount_point  = "/store"
mode         = "ro"
required_for = "boot"

[[mount]]
fs_server    = "fs-server-btrfs"
device       = "gpt-partlabel:home"
mount_point  = "/home"
mode         = "rw"
required_for = "boot"
```

**Fields:**

- `fs_server` — name of the fs-server binary, resolvable within `/initramfs/sbin/`
- `device` — block device identification (by partition UUID, label, or other stable identifier; deliberately not `/dev/sda1`-style since device enumeration order isn't stable)
- `mount_point` — path in the system namespace at which to bind this fs-server's RS
- `mode` — `ro` or `rw`; determines which rights init grants when binding
- `required_for` — `boot` means init must succeed this mount before proceeding; future values might include `emergency-only` (mount lazily) or similar

**Device identification:** requires the GPT driver to have enumerated partitions and registered DeviceNodes with stable identifiers (partition UUID, partition label). This is done during kernel boot (step 7 of the boot flow, driver init). Init then looks up devices by partition UUID or label via a kernel resource server (likely `/dev/disk/by-partuuid/` and `/dev/disk/by-partlabel/`).

#### Init's Processing of the Manifest

1. Parse `init.toml`
2. Topologically sort mount entries by `mount_point` depth (shallower paths mounted first — `/` before `/store`, `/store` before `/store/data`)
3. For each mount, in order:
   - Look up fs-server binary: `/initramfs/sbin/{fs_server}`
   - Look up block device: in `/dev/disk/by-partuuid/{uuid}` or similar
   - Create control channel between init and the fs-server instance
   - Spawn fs-server with SpawnArgs containing the binary, block device handle, control channel, log channel, its own minimal namespace, and (if not the first mount) a namespace handle for the system namespace so fs-server can receive the bind
   - Wait on the control channel for "Ready" message with the RS endpoint handle
   - Call `sys_ns_bind(system_namespace, mount_point, endpoint, rights=derived_from_mode)` to register
4. Once all critical-path mounts are bound, proceed to normal boot (read `/system/current-generation`, spawn profile server, spawn service manager)

If any critical-path mount fails (fs-server crashes during init, Ready never arrives, block device not found, etc.), init drops into **emergency mode**.

#### The Initramfs Closure

The initramfs must contain every piece of software needed for critical-path mounting, plus emergency recovery tools. Specifically:

- `init` — always
- `eshell` — emergency shell (see below)
- **All fs-server binaries referenced by `init.toml`** — e.g., `fs-server-ext4`, `fs-server-xfs`, `fs-server-btrfs` as appropriate
- `init.toml` itself
- Any kernel modules (Tier 2 LKMs) needed for critical-path block device access — for example, if the root filesystem is on an exotic storage controller that isn't Tier 1 compiled into the kernel, its LKM goes in the initramfs and init loads it before attempting to mount
- Any additional filesystem-prep tools needed for critical-path access — for a future encrypted-root configuration, this would include a LUKS driver LKM and a passphrase-prompt tool

**The closure is computed at system image build time.** The build tool (`xtask build-initramfs`) reads the target system's mount manifest, determines required binaries and modules, and assembles the initramfs CPIO. Different target systems produce different initramfs contents. This is a build-time decision, not a runtime one.

#### Emergency Shell (eshell)

A minimal interactive shell, bundled in the initramfs, that init drops into when a critical-path operation fails:

- Critical-path mount fails (fs-server won't come up, block device not present, Ready timeout, etc.)
- Ready but `current-generation` manifest can't be read
- Service manager fails to start
- Generic "something is broken and we can't continue normal boot"

What eshell provides:

- Text command interface over the serial console (UART on amd64) — no framebuffer rendering, no graphical dependencies
- Ability to list what init has and hasn't mounted so far
- Ability to read/write the initramfs contents — specifically, to edit `init.toml` and re-attempt the boot
- Ability to list available block devices via `/dev/disk/...`
- Ability to spawn fs-servers manually for inspection/recovery
- Ability to look at kernel log output
- Ability to reboot the machine

What eshell deliberately doesn't provide:

- Package management
- Network access (the network stack isn't up yet at this point)
- Graphical features
- Full shell scripting language
- Anything that would bloat the initramfs or depend on things that might not be available

eshell is roughly analogous to BusyBox-in-initramfs on Linux: a single minimal binary providing the commands needed to diagnose and recover boot failures. The command set is small enough to implement in a few thousand lines of Rust without external dependencies.

**How you get into eshell:** init detects the failure, logs what went wrong to the kernel log, spawns eshell with:
- Serial console handle (read+write)
- Initramfs namespace handle (LOOKUP + READ + WRITE, so eshell can edit init.toml)
- `/dev/disk/...` access (read-only — inspection only, no block-level writes)
- Kernel log handle (read-only, so operator can see what failed)

**What you do in eshell:** typically, examine kernel log for the failure reason, inspect block devices to verify expected partitions are present, edit `init.toml` to fix the configuration (wrong UUID, wrong fs_server name, etc.), save, and reboot. Alternatively, inspect a partially-mounted system for debugging.

**Post-boot:** eshell exits after reboot or after init resumes. It's not a running process during normal system operation.

#### Non-Critical-Path Mounts

Mounts not required for init to proceed — scratch filesystems (`/tmp` if not tmpfs-backed), data partitions (`/data`, `/backup`), removable media — are handled by the **service manager**, not init. The service manager reads its own mount manifest from the real filesystem (after init has brought up the essentials) and spawns additional fs-servers as services. Dynamic mount/unmount of removable media is handled by a dedicated mount daemon that the service manager supervises.

#### When Only One Filesystem Is Needed

The simplest case: single ext4 root partition, everything on it. `init.toml` has one entry. Init spawns one fs-server-ext4, binds it at `/`, proceeds. Everything "under" `/` — `/store`, `/home`, `/system` — is namespace-scoped subtrees of that one filesystem. This is expected to be the most common configuration and is not a special case in the init flow — it's just the trivial topology.

### Profiles and Namespace Projection

#### What a Profile Is

A **profile** is a namespace configuration — a mapping from exposed paths to store paths.

```
/bin/bash       → /store/abc123-bash-5.2/bin/bash
/bin/python     → /store/def456-python-3.11/bin/python
/lib/libc.so.6  → /store/jkl012-glibc-2.38/lib/libc.so.6
```

#### Profile Servers

A userspace resource server that:
- Loads a profile manifest (TOML in the store)
- Responds to lookups by consulting the manifest
- Returns forwarding addresses into the store resource server

Profile servers do **not** perform access control. Security is in namespace construction.

#### Namespace Construction for Different Roles

**Standard user:**
```
overlay {
    layer[0]: user_profile_server
    layer[1]: system_profile_server
    layer[2]: user_dev namespace
    direct:   /home → fs-server subtree handle scoped to alice's inode
    direct:   /tmp  → fresh tmpfs instance
    direct:   /proc → filtered process server (own processes only)
}
```

**Administrator:**
```
overlay {
    layer[0]: admin_profile_server
    layer[1]: system_profile_server
    layer[2]: admin_dev namespace
    direct:   /proc → full process server
}
```

**Sandboxed application:**
```
overlay {
    layer[0]: app_store_projection
    layer[1]: minimal_dev namespace
    direct:   /data → app data subtree handle
}
```

#### Generations and Atomic Updates

A **generation** is a point-in-time snapshot of a profile. Switching = swap which profile server handles the namespace subtrees. All generations coexist; rollback points to previous generation's profile server.

### Init and Service Management

#### PID 1: Minimal Init

Deliberately minimal. Lifecycle:

1. Receive initial handle set from kernel (including `/initramfs` RS handle, `/dev` RS handle, hardware resource handles, log channel, system namespace handle with `BIND_NAMESPACE` cap)
2. Parse `/etc/init.toml` from the initramfs
3. For each critical-path mount in dependency order:
   a. Look up fs-server binary in initramfs
   b. Look up block device in `/dev/disk/by-*`
   c. Spawn fs-server with control channel and minimal namespace
   d. Wait for Ready on control channel, receive endpoint handle
   e. Bind endpoint into system namespace at mount_point with mode-derived rights
4. On any critical-path failure, spawn eshell and wait (do not proceed)
5. Read `/system/current-generation` manifest (now resolvable through the system namespace)
6. Spawn system profile server; bind into root namespace
7. Spawn service manager; hand it the manifest, appropriate handles, and delegated `BIND_NAMESPACE` for its subtree
8. Once service manager confirms boot stable, call `sys_release_initramfs()`
9. Enter main loop: reap orphans, handle shutdown/reboot notifications

Init uses `libkern` + `alloc` directly; does not depend on `libos`/`librt`.

#### Service Manager

A supervised process started by init. Owns service declarations, dependency graph, supervision (crash detection, exponential-backoff restart), on-demand activation, ordered shutdown. Holds delegated `BIND_NAMESPACE` cap for the subtrees it manages.

#### Service Declarations (TOML)

```toml
[service.network-manager]
executable = "/store/abc123-network-manager/bin/network-manager"
syscaps    = ["BIND_NAMESPACE"]     # only if this service is itself a supervisor
after      = ["device-manager", "logging"]

[service.network-manager.handles]
namespace  = { rights = ["lookup", "bind"], subtree = "/net" }
device     = { path = "/dev/net/eth0" }
log        = { channel = "network" }
control    = { kind = "ipc-channel" }  # manager holds the other end

[service.network-manager.restart]
policy       = "on-failure"
max_attempts = 5
backoff      = "exponential"
```

A service receives exactly the handles listed. Capability model is enforced at spawn.

---

### Logging and Auditing

#### Logging

Capability-gated via log channel handles granted at spawn.

```rust
struct LogRecord {
    timestamp:  u64,
    sequence:   u64,
    level:      LogLevel,
    service:    KString,    // set by service manager, not the service itself
    message:    KString,
    span_id:    Option<u64>,
    trace_id:   Option<u64>,
    fields:     KVec<(KString, Value)>,
}
```

Logging service collects, indexes, manages sinks (persistent DB on disk, serial, network, in-memory ring).

#### Auditing

Kernel generates records for security-significant events: handle creation/destruction/transfer, namespace bind/unbind, SysCaps use, process creation/termination.

Kernel writes to dedicated audit ring buffer; audit service drains and persists. Records chained (hash of previous) for tamper detection. `SysCaps::AUDIT_CONTROL` required for management.

#### Kernel Tracing

High-frequency event recording into per-CPU ring buffers. Tracing handles give userspace read access. For performance analysis.

---

### Core Services

| Service | Responsibility |
|---|---|
| **Emergency shell (eshell)** | Interactive recovery shell bundled in initramfs; invoked by init on critical-path failure |
| **Device Manager** | Device tree watcher, kernel module loader, `/dev` namespace population |
| **Namespace Manager** | System namespace coordination, filesystem mount requests |
| **Network Manager** | Interface configuration, DHCP, DNS, routing (userspace TCP/IP; implementation deferred) |
| **Session Manager** | User session lifecycle, namespace construction per role, profile assignment |
| **Authentication Service** | Credential validation, session token issuance, role-to-capability mapping |
| **Package Manager** | Store management, generation management, GC |
| **Time Sync Service** | NTP/PTP, requires `SysCaps::SYSTEM_CLOCK` |
| **Power Management Daemon** | `Notification::PowerEvent` handler, executes policy (Phase 2) |
| **OOM Daemon** | `Notification::MemoryPressure` handler, applies kill policy |
| **Crash Reporter** | Exception notification handler, dump collection, `rustfilt` symbolication |
| **Privilege Broker** | Escalation requests, admin namespace construction, elevated process spawn |
| **Mount Daemon** | Post-boot dynamic mount/unmount (removable media, user-requested mounts) |
| **Netstack Server** | Userspace TCP/IP (deferred implementation; architecture specified) |
| **Compositor** | GUI composition (deferred implementation; architecture sketched) |

---

## Syscall Interface

### Design Philosophy: Async-First and Minimal

Every I/O operation returns immediately with a `PendingOperation` handle. The calling thread never blocks inside a syscall waiting for I/O — it blocks only via the explicit `sys_wait` call.

Syscall table is small (~30 entries). No `sys_read`, `sys_write`, `sys_open` — all I/O goes through `sys_io_submit`.

### Error Convention

All syscalls return `isize`. Negative values are `KError`:

```rust
#[repr(i32)]
pub enum KError {
    InvalidHandle     = -1,
    NoAccess          = -2,
    OutOfHandles      = -3,
    OutOfMemory       = -4,

    NotFound          = -10,
    AlreadyExists     = -11,
    NotNamespace      = -12,
    PathTooLong       = -13,

    WouldBlock        = -20,
    TimedOut          = -21,
    Cancelled         = -22,
    PeerClosed        = -23,

    InvalidArgument   = -30,
    FaultFromUser     = -31,
    TooLarge          = -32,

    NotReady          = -40,
    AlreadyReleased   = -41,
    InvalidState      = -42,

    NoHardware        = -50,
    HardwareError     = -51,
    Unsupported       = -52,

    KernelError       = -255,
}

pub type Status = Result<(), KError>;
```

All pointer args are `UserPtr<T>` / `UserMutPtr<T>`.

### The Complete Syscall Set

#### Handle Operations
```rust
sys_handle_close(h: RawHandle) -> isize
sys_handle_restrict(h: RawHandle, new_rights: Rights) -> isize
sys_handle_duplicate(h: RawHandle, new_rights: Rights) -> isize
sys_handle_stat(h: RawHandle, out: UserMutPtr<HandleInfo>) -> isize
```

#### I/O Core
```rust
sys_io_submit(resource: RawHandle, op: UserPtr<IoOp>) -> isize
sys_io_cancel(pending: RawHandle) -> isize

sys_wait(
    handles:  UserPtr<RawHandle>,
    count:    usize,
    results:  UserMutPtr<IoResult>,
    deadline: u64,
) -> isize
```

#### Namespace
```rust
sys_ns_lookup(ns: RawHandle, path: UserPtr<u8>, path_len: usize, rights: Rights) -> isize
sys_ns_bind(ns: RawHandle, path: UserPtr<u8>, path_len: usize, resource: RawHandle) -> isize
sys_ns_unbind(ns: RawHandle, path: UserPtr<u8>, path_len: usize) -> isize
sys_ns_create() -> isize
```

#### Process and Thread
```rust
sys_process_spawn(args: UserPtr<SpawnArgs>) -> isize
sys_thread_create(args: UserPtr<ThreadArgs>) -> isize
sys_thread_exit(status: i32) -> !
sys_process_exit(status: i32) -> !
sys_thread_set_tls(tls_base: usize) -> isize
sys_thread_set_affinity(thread: RawHandle, cpu_mask: CpuMask) -> isize
sys_thread_get_registers(thread: RawHandle, out: UserMutPtr<RegisterValues>) -> isize
sys_exception_resume(thread: RawHandle, disposition: Disposition) -> isize
sys_exception_extend_timeout(thread: RawHandle, additional_ns: u64) -> isize
```

#### Memory
```rust
sys_memory_create(size: usize, flags: MemFlags) -> isize
sys_memory_map(obj: RawHandle, hint: usize, size: usize, rights: Rights) -> isize
sys_memory_unmap(addr: usize, size: usize) -> isize
```

#### IPC
```rust
sys_channel_create(end0: UserMutPtr<RawHandle>, end1: UserMutPtr<RawHandle>, queue_depth: u32) -> isize
sys_channel_send(ch: RawHandle, msg: UserPtr<IpcMsg>, handles: UserPtr<RawHandle>, count: usize, mode: SendMode) -> isize
sys_channel_recv(ch: RawHandle, msg: UserMutPtr<IpcMsg>, handles: UserMutPtr<RawHandle>, count: UserMutPtr<usize>) -> isize
```

#### Kernel Objects
```rust
sys_timer_create(flags: TimerFlags) -> isize
sys_timer_set(timer: RawHandle, deadline_ns: u64, interval_ns: u64) -> isize
sys_notif_recv(queue: RawHandle, out: UserMutPtr<Notification>) -> isize
sys_clock_read(clock: ClockId, out: UserMutPtr<u64>) -> isize
sys_device_map_mmio(device: RawHandle, region_idx: u32, flags: MmioFlags) -> isize
sys_release_initramfs() -> isize
```

### High-Throughput Ring

```rust
sys_ring_create(sq_depth: u32, cq_depth: u32, flags: RingFlags) -> isize
sys_ring_notify(ring: RawHandle, to_submit: u32, min_complete: u32, deadline: u64) -> isize
```

Shared ring between kernel and userspace. Ring pages are mapped via a `MemoryObject` marked "kernel-readable without SMAP" — validation happens at ring create, not per-SQE. SQEs are copied from ring to kernel memory before action (TOCTOU safety).

`RING_KERNEL_POLL` flag: kernel polls on a dedicated thread; zero syscalls in steady state. `IO_FLAG_WANT_HANDLE` creates a PendingOperation that integrates with `sys_wait`.

Purely additive optimization; base async interface is self-sufficient.

---

## Userspace Runtime Library

### Crate Structure

| Crate | `no_std` | `alloc` | Purpose |
|---|---|---|---|
| `libkern` | yes | **no** | Raw syscall wrappers, ABI types |
| `libos` | yes | yes | Typed `Handle<T, M>`, async executor |
| `librt` | yes | yes | Sync wrappers, fiber scheduler |
| `libstream` | yes | yes | Typed I/O, TypedRecord derive macro |
| `librsproto` | yes | yes | Resource server wire protocol |

Early services (init, fs-servers, eshell) use `libkern` + `alloc` directly.

### libkern

`#![no_std]`, no `alloc`. Raw syscall `unsafe extern "C"` wrappers + ABI types. No heap.

### libos

Typed handles with typestate modes.

```rust
pub struct Handle<T, M> {
    raw:   RawHandle,
    extra: Rights,
    _t:    PhantomData<T>,
    _m:    PhantomData<M>,
}
```

**Mode types:**

| Kernel object | Modes | Modifier rights (runtime) |
|---|---|---|
| File / Resource | `ReadOnly`, `WriteOnly`, `ReadWrite`, `Executable` | `SEEK`, `APPEND`, `TRUNCATE` |
| Namespace | `NsReadOnly`, `NsMutable` | `BIND`, `UNBIND`, `ENUMERATE` |
| IpcChannel | `Send`, `Recv`, `SendRecv` | (none) |
| MemoryObject | `MapRead`, `MapReadWrite`, `MapExec` | (none) |
| Process | `ProcObserve`, `ProcControl`, `ProcTerminate` | `INSPECT_MEMORY` |
| Thread | same as Process | (as above) |
| Timer, Interrupt, Entropy, PendingOp, NotifChannel | `Only` | (generic only) |

Generic rights (`DUPLICATE`, `TRANSFER`, `INSPECT`, `WAIT`) always runtime.

**Operation gating:**

```rust
pub trait CanRead  {}
pub trait CanWrite {}

impl CanRead  for ReadOnly  {}  impl CanRead  for ReadWrite {}
impl CanWrite for WriteOnly {}  impl CanWrite for ReadWrite {}

impl<M: CanRead> Handle<File, M> {
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> { /* ... */ }
}

impl<M: CanWrite> Handle<File, M> {
    pub async fn write(&self, buf: &[u8]) -> Result<usize> { /* ... */ }

    pub async fn seek(&self, pos: u64) -> Result<()> {
        if !self.extra.contains(Rights::SEEK) { return Err(Error::NoAccess); }
        /* ... */
    }
}
```

**Attenuation:**

```rust
impl Handle<File, ReadWrite> {
    pub fn into_read_only(self) -> Handle<File, ReadOnly> { /* sys_handle_restrict */ }
}

impl<T, M> Handle<T, M> {
    pub fn without_transfer(mut self) -> Self { /* ... */ }
    pub fn without_duplicate(mut self) -> Self { /* ... */ }
}
```

Compiles on stable Rust. Async executor built on `sys_wait`.

### librt

Synchronous and fiber-based wrappers:

```rust
pub fn read<M: CanRead>(h: &Handle<File, M>, buf: &mut [u8]) -> Result<usize>
pub fn fiber_read<M: CanRead>(h: &Handle<File, M>, buf: &mut [u8]) -> Result<usize>
```

### libstream

```rust
#[derive(TypedRecord)]
struct ProcessInfo {
    pid:    u64,
    name:   String,
    cpu:    f64,
    state:  String,
    handle: RawHandle,
}

let mut tw = TableWriter::new(stdout_handle);
for proc in processes {
    tw.write_row(&proc)?;
}
tw.finish()?;
```

**Initial supported types:** primitives, `String`, `Vec<T: TypedRecord>`, nested structs, `Option<T>`, `RawHandle`. **Deferred:** enums, generics beyond `Vec<T>`, non-`'static` lifetimes.

### librsproto

Binary protocol over IPC for all userspace RS.

```rust
#[repr(C)]
struct RsMsgHeader {
    magic:        u32,      // 0x52534D47 = "RSMG"
    version:      u16,      // negotiated per-channel
    op:           u16,      // [category:u8 | specific:u8]
    request_id:   u64,
    flags:        u32,
    body_len:     u32,
    handle_count: u16,
    _reserved:    u16,
}
```

Categories: `Meta`, `Namespace`, `Stream`, `Block`, `Control`, `Power`, `Vendor`.

**Meta required:** `Hello` (version handshake; first message on channel), `Goodbye`, `QueryCaps`, `Ping`.

Body: packed C structs per op, fixed offsets.

Bulk data: inline (< 3.5 KiB), via `MemoryObject` handle, or via `IoRing`.

### std Port (Future)

Once syscall interface stabilizes. `std::fs/thread/net/sync/io` over native handles. Enables full Rust ecosystem on the platform.

### POSIX Compatibility Shim (Optional Future)

Thin shim translating POSIX calls to handle-based equivalents. Not a design constraint.

---

## User Interface and Shell

### Typed Stream Model

Goal: preserve Unix composability while replacing the byte stream with a typed structural model. Text as canonical fallback.

**Wire format:**

```
Stream     := Header Body
Header     := magic:u32("TSM1")  flags:u32  Schema
Schema     := field_count:u32  Field*
Field      := name_len:u16  name:bytes  type:TypeTag  modifiers:u8
Body       := Record*  Terminator
Record     := record_tag:u8(0x01)  field_values
           |  error_tag:u8(0x02)   ErrorRecord
           |  widget_tag:u8(0x03)  WidgetRecord
Terminator := end_tag:u8(0xFF)  exit_status:i32
```

Handle values in streams are encoded as `RawHandle`; handle *transfer* happens via the underlying IPC channel, not the stream data.

### The Core Type System

**Structural types** (what the system understands, fixed set):

```rust
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(Arc<str>),
    Bytes(Arc<[u8]>),
    List(Arc<[Value]>),
    Table(Arc<Table>),
    Record(Arc<Record>),
    Error(Arc<StreamError>),
    Handle(RawHandle),
}
```

**Semantic types** (what programs define, opaque to system): regular Rust structs with `#[derive(TypedRecord)]`.

**Table** is primary inter-program type. Header once, rows stream.

**Schema inference via derive macro.** No registry, no coordination.

**Handles as first-class values** — capabilities travel with data.

**Text fallback:** printf programs are automatically wrapped in single-column `Table<String>` with column `"line"`.

### The Shell as a Data Flow Language

Grammar deferred; semantics committed.

**Duck typing over structural types.** Generic operators work on whatever fields are present.

**Built-in operators:** `sort`, `filter`, `take`, `count`, `select`, `join`, `group`, `display`.

**The display verb** — context-aware rendering (ANSI table in terminal, interactive widget in GUI).

**Port-based wiring** for non-linear data flows. Maps to visual shell.

### GUI Integration

Client-side rendering. Programs emit `WidgetRecord` with a live data stream handle and a control channel.

Three modes per program: batch (piped), interactive terminal (ANSI widget), interactive GUI (native widget). Same program works in all three.

**Compositor architecture (deferred):** GPU driver = Tier 2 LKM. Compositor = userspace server. Clients render into own buffers, submit to compositor. Input devices as resources. Specific protocol TBD.

**Pre-compositor:** `/dev/framebuffer` directly accessible for early userspace.

**Visual shell:** GUI dataflow builder. Same semantics as text shell. Interchangeable within shared subset.

### Complex Interactive Programs

Stream model isn't the right primary model for browsers, video editors, games. Model-view decomposition: composable model with typed ports + monolithic view. OS makes this structural.

| Category | Approach |
|---|---|
| Data transformation tools | Stream-native |
| Separable model/view | Model with typed ports; view is monolithic |
| Rendering engines, browsers, games | Normal programs with window handles |

---

## Default Program Channels

| Channel | Type | Direction | Purpose |
|---|---|---|---|
| `stdin` | Typed stream | Read | Input from upstream, terminal, or null |
| `stdout` | Typed stream | Write | Output to downstream, display, or discarded |
| `stderr` | Typed stream | Write | Diagnostic output; always to display/terminal |
| `log` | Log channel handle | Write | Structured logging (capability-gated) |
| `notification` | NotificationChannel | Read | Kernel events, exceptions, lifecycle |
| `namespace` | Namespace handle | Read | Current namespace |

Supervisor-spawned services additionally receive `control` — an IPC channel end connected to the supervisor for lifecycle management.

---

## Reference Projects

### Operating Systems and Kernels

| Project | Influence |
|---|---|
| Unix (Bell Labs original) | Composable pipelines, stdin/stdout/stderr, shell as programming environment |
| Plan 9 from Bell Labs | Per-process namespaces, `bind`/`mount` model, resource servers |
| Linux | LKM, buddy/slab allocators, practical driver model, Rust-in-kernel `no_std` patterns, initramfs |
| Windows NT | IRP driver stacks, Object Manager, typed kernel objects, unified wait model, DPCs |
| Fuchsia / Zircon | Capability handles, kernel-mediated transfer, typed IPC protocols, userspace netstack |
| seL4 | Formal capability theory, principle of least authority |
| Mach | Exception ports, peer IPC vs. kernel notification distinction |
| FreeBSD | Source layout reference for Latte |

### Package Management

| Project | Influence |
|---|---|
| NixOS | Content-addressed store, generations, atomic updates, rollback |
| GNU Guix | Reinforces NixOS paradigm |

### Bootloaders

| Project | Influence |
|---|---|
| Limine | Chosen — UEFI-native, HHDM, SMP bootstrap |
| GRUB | Used in Latte; replaced |

### Shells and UI

| Project | Influence |
|---|---|
| Nushell | Typed structured streams — primary UI reference |
| Unix shell | Composable pipeline model |
| Node-RED | Visual dataflow wiring paradigm |
| PowerShell | Object-based pipelines |

### I/O and Async

| Project | Influence |
|---|---|
| io_uring (Linux) | Submission/completion ring model |
| DPDK / VFIO | Userspace driver model |

### Recovery Tools

| Project | Influence |
|---|---|
| BusyBox | Minimal single-binary recovery environment — reference for eshell |
| dracut / mkinitcpio | Initramfs closure and generation tools |

### Predecessor

| Project | Notes |
|---|---|
| Latte (kelby0320/latte) | Direct predecessor |

---

## Boot Flow

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. UEFI Firmware                                               │
│    Initializes hardware, locates EFI System Partition          │
│    Loads and executes Limine bootloader from FAT32 ESP         │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 2. Limine Bootloader                                           │
│    Reads kernel ELF from disk (FAT ESP)                        │
│    Reads initramfs module (CPIO newc)                          │
│    Maps kernel to higher half virtual address space            │
│    Sets up Higher Half Direct Map (HHDM)                       │
│    Initializes framebuffer                                     │
│    Reads ACPI tables, builds memory map                        │
│    Starts all SMP cores with stacks                            │
│    Transfers control to NASM boot stub                         │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 3. NASM Boot Stub → kernel_main()                              │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 4. Early Kernel Initialization                                 │
│    Read Limine responses                                       │
│    Initialize early serial console                             │
│    Initialize buddy allocator                                  │
│    Initialize SLUB slab allocator                              │
│    Initialize kernel virtual memory                            │
│    Initialize global handle table                              │
│    Initialize per-CPU data structures                          │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 5. Core Kernel Subsystem Initialization                        │
│    Initialize interrupt controller                             │
│    Install exception/interrupt handlers                        │
│    Initialize ACPI Phase 1 (table parsing)                     │
│    Initialize kernel timer heap                                │
│    Initialize entropy pool                                     │
│    Initialize scheduler                                        │
│    Initialize IPC, notification queue, namespace subsystems    │
│    Initialize resource server registry                         │
│    Initialize DPC queues                                       │
│    Enable interrupts                                           │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 6. SMP Initialization                                          │
│    Bring up additional CPU cores                               │
│    Per-CPU structures, schedulers, timers                      │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 7. Device Discovery                                            │
│    Walk ACPI device tree, build DeviceNode objects             │
│    Register PCI bus RS; enumerate PCIe via ECAM                │
│    Initialize Tier 1 drivers (AHCI, NVMe, GPT per features)    │
│    GPT driver enumerates partitions; registers                 │
│      /dev/disk/by-partuuid/* and /dev/disk/by-partlabel/*      │
│    Program IOMMU for userspace-driver isolation                │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 8. Kernel Resource Server Registration                         │
│    Register in-kernel RSes into root namespace:                │
│      /proc, /dev, /sys, /dev/framebuffer, /dev/entropy,        │
│      /dev/log, /initramfs (from Limine-loaded CPIO)            │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 9. Init Process Launch (from initramfs)                        │
│    Parse initramfs CPIO header; locate /sbin/init              │
│    Map init's ELF, construct address space                     │
│    Construct initial handle set:                               │
│      — root namespace handle (full rights, BIND_NAMESPACE)     │
│      — /initramfs namespace binding (RO)                       │
│      — /dev handle (access to /dev/disk/by-*, kernel log)      │
│      — hardware resource handles                               │
│      — system control handle                                   │
│    Spawn init (PID 1) with full SysCaps                        │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 10. Init: Read Bootstrap Manifest                              │
│    Look up /initramfs/etc/init.toml                            │
│    Parse; topologically sort mounts by mount_point depth       │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 11. Init: Process Each Critical-Path Mount                     │
│    For each [[mount]] in dependency order:                     │
│      a. Look up /initramfs/sbin/{fs_server} → exec handle      │
│      b. Look up /dev/disk/by-*/{device_id} → block handle      │
│      c. sys_channel_create → control channel pair              │
│      d. sys_process_spawn fs-server with:                      │
│           — executable, block device handle                    │
│           — fs-server's own minimal namespace                  │
│           — control channel (fs-server side)                   │
│           — log channel                                        │
│           — if not first mount: system namespace handle for    │
│             fs-server's use when creating endpoint             │
│      e. sys_wait on control channel for Ready message          │
│      f. Extract endpoint handle from Ready message             │
│      g. sys_ns_bind(system_ns, mount_point, endpoint, rights)  │
│    On any failure, goto Emergency Mode (step 11E)              │
└────────────────────────────┬────────────────────────────────────┘
                             │
                    ┌────────┴────────┐
                    ▼                 ▼
           success              any failure
                    │                 │
                    │                 ▼
                    │    ┌──────────────────────────────────┐
                    │    │ 11E. Emergency Mode              │
                    │    │    Log what failed to kernel log │
                    │    │    Look up /initramfs/sbin/eshell│
                    │    │    Spawn eshell with:            │
                    │    │      — serial console handle     │
                    │    │      — initramfs handle          │
                    │    │        (LOOKUP+READ+WRITE)       │
                    │    │      — /dev/disk/* (RO)          │
                    │    │      — kernel log (RO)           │
                    │    │    Wait for eshell exit / reboot │
                    │    └──────────────────────────────────┘
                    ▼
┌────────────────────────────▼────────────────────────────────────┐
│ 12. Init: Namespace Construction and System Launch             │
│    Read /system/current-generation manifest via system ns      │
│    Spawn system profile server; bind into root namespace       │
│    Construct base system overlay namespace                     │
│    Spawn service manager with:                                 │
│      — system namespace (or delegated subtree)                 │
│      — manifest                                                │
│      — delegated BIND_NAMESPACE cap                            │
│      — log, control channel                                    │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 13. Service Manager: Ordered Service Startup                   │
│    Parse TOML service declarations                             │
│    Build dependency graph, topological sort                    │
│                                                                │
│    Tier 1: logging, audit                                      │
│    Tier 2: device manager, namespace manager                   │
│    Tier 3: non-critical fs-servers (mount daemon supervised),  │
│            network manager (netstack deferred), entropy daemon │
│    Tier 4: time sync, authentication, package manager,         │
│            power daemon (Phase 2), OOM daemon, crash reporter  │
│    Tier 5: session manager                                     │
│                                                                │
│    Services requiring BIND_NAMESPACE for their subtrees get    │
│    delegated caps. Others don't.                               │
│                                                                │
│    Each service is spawned per the Resource Server Startup     │
│    Protocol: control channel + Ready handshake + supervisor    │
│    binds endpoint.                                             │
│                                                                │
│    On boot-stable confirmation, service manager signals init,  │
│    which calls sys_release_initramfs() — kernel unbinds        │
│    /initramfs and frees initramfs physical pages               │
└────────────────────────────┬────────────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────────┐
│ 14. Session Manager: User Login                                │
│    Present login interface                                     │
│    Authenticate via authentication service                     │
│    Determine role and current profile generation               │
│    Start or reuse user profile server                          │
│    Construct user namespace (overlay layers per role,          │
│      subtree handles scoped to user directories, attenuated    │
│      rights on /store)                                         │
│    Spawn user shell/session                                    │
│    User SysCaps set (no LOAD_MODULE, no BIND_NAMESPACE, etc.)  │
└─────────────────────────────────────────────────────────────────┘
                             │
                     System is running
```

---

## Kernel Subsystem Catalogue

### Memory Management
- Buddy allocator (zone-organized, DMA + Normal; single node — no NUMA)
- SLUB slab allocator (`KBox`, `KVec`)
- Virtual memory manager (VMA red-black tree, CoW)
- Address space layout (higher-half kernel, guard pages, ASLR)
- Page cache (shared across processes)
- Page reclaim daemon (clock algorithm)
- Swap subsystem (PTE swap markers)
- Memory pressure → userspace OOM daemon
- TLB shootdown (active_cpus mask, IPI with PCID optimization, TLBI broadcast on aarch64)
- User memory access (UserPtr, copy primitives, extable recovery, SMAP/PAN)

### Handle / Capability System
- Segmented global handle table
- HandleEntry with seqlocks
- Randomized slot allocation
- Owner-enforced security
- Rights bitmask (generic + principal + modifier)
- Handle transfer (move in-place / duplicate new-slot)
- Per-type dispatch via KObjectType enum
- ObjectRef RAII refcount with seqlock protocol
- Deferred-free with per-CPU quiescent state counters

### Kernel Objects
- KObjectHeader (refcount, type tag)
- Process, Thread
- Namespace
- MemoryObject
- IpcChannel
- NotificationChannel
- Timer
- InterruptObject
- PendingOperation
- IoRing
- EntropyObject
- DeviceNode
- ResourceServerReg

### Namespace Subsystem
- Namespace kernel object (per-process)
- ResourceServerRegistry (flat list)
- Path resolution engine
- Lookup cache
- Overlay composition
- Synthetic /proc/self/*

### Process and Thread Management
- Process object (address space, ns/cwd handles, SysCaps, notification channel, creator, children)
- Thread object (register state, FPU context, TLS, kernel stack, sched params)
- Spawn with explicit handle set (argv/env as typed values)
- Creator-based reparenting; init reaps via close-on-ChildExited
- Eager FPU via XSAVE; FS_BASE user TLS
- Exception delivery with priority chain
- sys_exception_resume with Disposition

### Scheduler
- RealTime / TimeShared / Idle classes
- Per-CPU runqueues with work stealing
- Context switch (NASM stub → Rust handler; XSAVE + FS_BASE)
- Arch timer preemption

### Driver Subsystem
- Tier 1 compiled-in (PCI, AHCI, NVMe, GPT, LVM, initramfs RS, console)
- Tier 2 LKMs
- LKM infrastructure (ELF load, export table, ABI versioning, refcount-drain unload)
- Irp struct + driver stack framework
- Filter driver registration
- ResourceServer trait (OpStatus)
- Device tree (ACPI / DTB)
- Driver manager
- InterruptObject handles (transferable)
- sys_device_map_mmio (DeviceNode-mediated)
- IOMMU programming

### IPC Subsystem
- IpcChannel (bidirectional, handle pair)
- Fixed 4 KiB IpcMsg + 8 handles; bulk via MemoryObject
- Per-channel queue_depth; Block/NoBlock/BlockBounded send modes
- Handle transfer (atomic, kernel-mediated)
- PendingOperation handles
- Unified sys_wait

### Notification Queue
- NotificationChannel (kernel-copy bounded queue)
- Notification enum (`#[repr(C, u32)]`, sparse categories, fixed 64-byte variants, Unknown forward-compat)
- Priority delivery for exceptions
- Exception priority chain (debugger → process → default)
- sys_exception_resume, sys_thread_get_registers

### DPCs and Wait Queues
- Per-CPU DPC queue (MPSC, drained on IRQ return / sched entry)
- DpcNode inline in owning structures
- Three contexts: IRQ > DPC > Thread
- Per-object WaitQueue with intrusive list
- Pre-allocated WaitNode array on Thread

### Time and Timers
- ArchTimer trait
- Kernel timer min-heap (arch IRQ → DPC)
- Timer kernel object (waitable)
- sys_clock_read (Monotonic, Realtime, ProcessCpu, ThreadCpu)

### Security
- SysCaps bitflag (inherited explicitly, never amplified)
- BIND_NAMESPACE concentrated in supervisors (init, service manager, session manager)
- Capability bootstrap (kernel → init)
- Namespace as primary security boundary
- Tiered /dev namespace objects
- Privilege broker model
- Audit ring buffer (chained, tamper-detectable; AUDIT_CONTROL required)

### Power Management
- Phase 1: kacpi (Rust table parsing, FADT reset, no AML)
- Phase 2 (deferred): ACPICA via FFI
- ArchPower trait
- PowerEvent notifications (Phase 2)

### Entropy
- RDSEED/RDRAND (amd64), RNDR/SMCCC TRNG (aarch64)
- TSC jitter + HPET skew mixing
- ChaCha20 CSPRNG with periodic reseed
- EntropyObject handle

### Debugging Infrastructure
- Kernel log buffer (kprintln!)
- Early serial console
- GDB remote stub (rustfilt symbols)
- Stack unwinder
- Bounds checking + debug_assert!
- QEMU address sanitizer
- TCO watchdog
- NMI-broadcast SMP panic
- Crash dumps
- Lock ordering (kernel/docs/lock-ordering.md)

### Architecture Abstraction Layer
- ArchPaging, ArchIrq, ArchCpu, ArchSmp, ArchTimer, ArchFpu, ArchUserAccess, ArchPower
- NASM stubs: boot.asm, context_switch.asm, user_copy.asm

---

## Userspace Subsystem Catalogue

### Package and Environment Management
- Content-addressed immutable store on ext4
- Immutability enforced at namespace-rights level (not in fs-server)
- Generation-based profiles
- Profile servers
- Profile manifest (TOML in store)
- Package manager daemon
- Garbage collector

### Init and Service Management
- Minimal PID 1 (libkern + alloc direct; no libos/librt)
- CPIO newc initramfs parsing
- Bootstrap manifest (`/etc/init.toml`) processing
- Critical-path mount orchestration
- Emergency mode with eshell fallback
- sys_release_initramfs once boot stable
- Service manager (TOML-driven, supervised, topological startup)
- Service declarations
- Control-channel + Ready-handshake protocol for every supervised service

### Filesystem Drivers
- FAT driver (RO initial, RW optional) — required for ESP
- ext4 driver (RO initial, RW phase 2) — primary filesystem
- One fs-server process per block device; one RS registration per fs-server
- Scoping and policy via namespace composition, not fs-server internals
- Additional filesystems (btrfs, XFS, NTFS, ...) as future separate servers

### Bootstrap Mount Topology
- `init.toml` format and semantics
- Initramfs closure computation at build time (includes fs-servers, LKMs, eshell)
- Critical-path vs non-critical-path mount distinction
- Non-critical mounts handled by mount daemon via service manager
- Stable device identification (partition UUID, partition label) via `/dev/disk/by-*/`

### Emergency Recovery
- eshell: minimal interactive shell in initramfs
- Serial-console-only interface
- Capabilities: list mounts, read/write initramfs files (edit init.toml), read block devices, read kernel log, reboot
- Spawned by init on any critical-path failure
- Exits on reboot; not present during normal operation

### Logging
- Log channel handles (capability-gated)
- LogRecord (timestamp, sequence, level, service set by manager, fields)
- Logging service (collection, indexing, persistence)
- Kernel audit buffer + audit service
- Kernel tracing subsystem

### Core Services
- Emergency shell (eshell; initramfs-resident)
- Device manager, Namespace manager, Network manager (netstack deferred)
- Session manager, Authentication service
- Package manager, Time sync (SYSTEM_CLOCK)
- Power management daemon (Phase 2), OOM daemon, Crash reporter
- Privilege broker, Mount daemon

### Syscall Interface
- Async-first; ~30 syscalls
- sys_io_submit / sys_wait / sys_io_cancel core primitives
- sys_ring_create / sys_ring_notify high-throughput ring (additive)
- sys_clock_read, sys_thread_set_tls, sys_device_map_mmio, sys_release_initramfs, sys_exception_resume
- isize return with negative KError; UserPtr/UserMutPtr for pointer args

### Userspace Runtime Library
- libkern: raw syscalls, no_std, no alloc
- libos: Handle<T, M> typestate, async executor
- librt: sync wrappers, fiber scheduler
- libstream: TableWriter, TypedRecord derive
- librsproto: resource server wire protocol
- std port (future)
- POSIX shim (optional future)

### User Interface and Shell
- Value enum (structural types)
- Table as primary inter-program type
- TypedRecord derive macro
- Handle as first-class Value
- Text fallback (line column)
- TSM1 binary wire format
- Shell operators via duck typing
- display verb
- WidgetRecord for interactive output
- Port-based wiring; visual shell
- Model-view decomposition pattern
- Shell grammar deferred

### Network Stack
- Userspace TCP/IP (deferred implementation)
- Network driver as Tier 1 or Tier 2
- Sockets as namespace resources
- Architecture committed

### Graphics and Compositor
- GPU driver (Tier 2 LKM)
- Compositor as userspace server (deferred)
- Client-side rendering, Wayland-influenced
- Input devices as resources
- Pre-compositor mode: /dev/framebuffer

### Default Program Channels
- stdin, stdout, stderr: typed streams
- log: capability-gated
- notification: kernel events
- namespace: current namespace
- control: present when spawned by a supervisor (IPC channel to supervisor for lifecycle management)

---

## Non-Goals and Deferred Work

Explicit non-goals — not planned, not a design constraint:

- POSIX compatibility as a primary goal
- Global ambient authority (UID/GID)
- Unix signals
- Global VFS tree in kernel
- NUMA-aware scheduling and allocation
- KPTI / Meltdown speculative-execution mitigations
- KASLR
- 5-level paging (57-bit VA)
- Per-process resource limits (rlimits)

Deferred — planned but not in initial scope:

- aarch64 support (architecture abstraction designed in; implementation later)
- ACPICA integration (Phase 2)
- LKM sophisticated features (signing, runtime upgrade, live patching)
- TCP/IP implementation (netstack server; architecture defined)
- GPU driver and compositor (architecture sketched)
- Shell grammar specification
- std port
- POSIX compatibility shim
- Full laptop power management (awaits Phase 2 ACPI)
- CPU C-state/P-state management beyond hlt (awaits Phase 2 ACPI)
- Network boot (PXE handled by Limine; kernel netstack not required)
- Additional filesystems (btrfs, NTFS, XFS, ZFS — as separate fs-servers when needed)
- Fast-path sys_clock_read via vDSO-equivalent
- TypedRecord support for enums and generics beyond Vec<T>
- iovec-style scatter/gather user access
- Priority inheritance for userspace synchronization
- Deadline scheduling (EDF) as a scheduler class
- Read-write FAT driver
- Encrypted root / LUKS (architecture accommodates — LUKS as block device filter driver in initramfs — but not in initial scope)
- LVM / software RAID at early boot (same architectural accommodation; same deferral)
- Runtime reconfiguration of critical-path mounts (currently requires reboot through eshell)
