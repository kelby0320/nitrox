# Nitrox Implementation Plan

Working document tracking implementation progress. Updated as work proceeds — this is meant to be edited freely, not preserved as a snapshot.

## How to use this document

- Each phase has a goal, a checklist of work items, and a milestone definition ("how do I know this phase is done?").
- Check items off (`- [x]`) as they're completed.
- Items can be reordered within a phase if dependencies allow. The order shown is a suggested execution order, not a strict requirement.
- Add sub-items under any task if it grows complex enough to need breakdown.
- When deviating from the plan, note it inline (`Note: ...`) rather than rewriting silently — the reasons matter later.
- Phases overlap in practice. "Phase 1" being the focus doesn't mean nothing from Phase 2 can be touched; it means Phase 1's milestone is the next target.

## Cross-references

Throughout this document, links to `docs/architecture/`, `docs/spec/`, and `docs/rationale/` point to specific documents that contain the design and rationale. The architecture overview at `docs/architecture/overview.md` is the recommended entry point if context is needed.

## Current status

- **Phase 0 (Foundation):** complete (or nearly — verify against the checklist below before declaring done)
- **Phase 1 (Kernel substrate):** ready to start
- **Phase 2 (Filesystem and namespace):** not started
- **Phase 3 (Service ecosystem):** not started
- **Phase 4+ (Shell, display, networking):** not started

---

## Phase 0: Foundation

**Goal:** kernel boots in QEMU, prints to the serial console, halts. The development loop is working.

**Why this phase matters:** every subsequent piece of work is dramatically easier with a working dev loop. Investing extra time here pays off across the entire project. Resist the temptation to perfect, but get to "kernel boots and prints" before going further.

### Tasks

- [x] Monorepo set up with three workspaces (`kernel/`, `userspace/`, `tools/`) per the structure in [docs/architecture/overview.md]
- [x] Top-level repo structure (`docs/`, `.cargo/`, `.gitignore`, `README.md`, `LICENSE`)
- [x] `CLAUDE.md` files in place (root, `kernel/`, `userspace/`, `userspace/libkern/`, `userspace/init/`)
- [x] `.claude/settings.json` configured
- [x] Custom target JSON for `x86_64-unknown-none` in `kernel/.cargo/config.toml`
- [x] `cargo build-std` configuration working for the kernel target
- [x] NASM boot stub in `kernel/src/arch/amd64/boot.asm`
- [x] Limine boot protocol integration: request structs in kernel binary, response handling in `kernel_main`
- [x] Minimal `kernel_main` that prints to serial via early UART
- [x] Limine configuration file builds correctly
- [x] `tools/xtask/` workspace with the `xtask` binary crate
- [x] `xtask build` — builds kernel, assembles disk image
- [x] `xtask qemu` — runs the kernel under QEMU with serial console captured
- [x] `xtask qemu-debug` — runs QEMU with GDB stub enabled
- [x] `xtask test` — runs host-side unit tests (stub OK; will grow)
- [x] `xtask test-qemu` — runs integration tests in QEMU with `isa-debug-exit` (stub OK; will grow)
- [x] GitHub Actions CI running `cargo build` and `xtask test` on every push
- [x] `docs/` populated with the foundational documents (overview, rationale, spec)
- [x] v5.1 design doc archived at `docs/history/design-doc-v5.1.md`
- [x] Decision log started at `docs/history/decision-log.md`

### Milestone

`xtask qemu` boots Limine, kernel prints "Hello from Nitrox" (or similar) to serial, halts. CI is green.

### Notes / deviations

(Add notes here about anything that diverged from plan during Phase 0.)

---

## Phase 1: Kernel substrate

**Goal:** the kernel infrastructure that everything else needs. Memory, handles, kernel objects, basic scheduling, the first userspace process.

**Why this phase matters:** this is where the kernel becomes a real kernel. Most of the foundational architecture lands here. The pieces are interdependent — order matters.

### Tasks (in suggested execution order)

#### Memory foundation

- [x] Buddy allocator for physical pages
  - DMA zone (below 16MB) + Normal zone
  - Uses Limine's HHDM for physical-to-virtual translation
  - Host-testable: write the buddy logic with mocked free lists; run in `cargo test`
- [x] SLUB-inspired slab allocator on top of buddy
  - Wires the buddy allocator into the boot path. Exposes `kmalloc` /
    `kfree` / `kzalloc`. See `docs/architecture/memory-management.md`.
  - Note: 2026-05-20 — the slab originally also registered a
    `#[global_allocator]` to enable `extern crate alloc`. That was
    removed: kernel code uses the fallible `libkern` containers, not
    `alloc`. See the decision log entry of 2026-05-20.
- [x] `KBox<T>` and `KVec<T>` in kernel's `libkern` module
- [x] `KString` + `core::fmt::Write` + `kformat!` in `libkern`
- [ ] Intrusive linked list — deferred to the scheduler / wait-queue
  slice, where its first real consumer lands
- [ ] Red-black / interval tree — deferred to the VMA slice; build the
  interval-augmented variant directly against the VMA manager's needs
- [ ] `Arc`-equivalent for refcounted kernel object references
  (`KArc` / `ObjectRef`) — deferred to the kernel-object-infrastructure
  slice; its shape depends on `KObjectHeader` + the seqlock protocol
  - Note: 2026-05-20 — the original three lines grouped six structures
    into the memory foundation. Reordered to a just-in-time schedule:
    `KBox` / `KVec` / `KString` now (zero design risk, needed within
    1–2 slices); the intrusive list, tree, and `KArc` when their first
    consumer lands, since each one's API is defined by a consumer that
    does not exist yet. See the decision log entry of 2026-05-20.

#### Address spaces and paging

- [ ] `ArchPaging` trait in `kernel/src/arch/` with amd64 implementation
  - `map_page`, `unmap_page`, `flush_tlb_*`, `set_page_table`
  - All `unsafe`, all with SAFETY comments
- [ ] VMA structure with red-black tree storage
- [ ] Address space construction from an ELF image
- [ ] Higher-half kernel mapping shared across all address spaces
- [ ] Per-thread kernel stack with guard page

#### User memory access discipline

- [ ] `UserPtr<T>` and `UserMutPtr<T>` opaque wrapper types
- [ ] Exception table mechanism: `(fault_pc, recovery_pc)` pairs registered at compile time
- [ ] Copy primitives: `copy_from_user`, `copy_to_user`, `copy_slice_from_user`, `copy_slice_to_user`, `copy_cstr_from_user`
- [ ] SMAP/SMEP discipline: `stac`/`clac` only within copy routines
- [ ] Page fault handler that consults the exception table before VMA lookup
- [ ] [docs/spec/user-memory-access.md] (write this spec while implementing)

#### Handle table

- [ ] Segmented handle table per [docs/spec/handle-encoding.md]
- [ ] `HandleEntry` with seqlocks
- [ ] Lookup path (lock-free common case)
- [ ] Allocation with randomized slot allocation (shuffled free list)
- [ ] Close with deferred reclamation
- [ ] Per-process quiescent state counter for RCU-style grace periods
- [ ] Owner-PID enforcement on every lookup
- [ ] Host-testable: build the handle table standalone, hammer it from threads, verify invariants

#### Kernel object infrastructure

- [ ] `KObjectHeader` with refcount and type tag
- [ ] `KObjectType` enum
- [ ] Match-dispatch pattern for type-specific operations
- [ ] `ObjectRef` RAII refcount holder with try_acquire seqlock interaction
- [ ] First kernel objects: `Process`, `Thread` (no other types yet)

#### Threading and context switch

- [ ] `Thread` kernel object with register state, FPU context, kernel stack, sched params
- [ ] FPU state: XSAVE area per thread, init values, save/restore primitives
- [ ] Context switch stub in NASM (`kernel/src/arch/amd64/context_switch.asm`)
- [ ] Rust-side context switch handler called from NASM stub
- [ ] Minimal scheduler: round-robin between kernel threads, no classes yet
- [ ] TLS support: FS_BASE handling, `sys_thread_set_tls` (when syscalls exist)

#### Syscall entry/exit

- [ ] `syscall` instruction handler (amd64) with `swapgs`, register save
- [ ] Syscall dispatch table
- [ ] First syscall: `sys_kprint(ptr, len)` (debug only — write user bytes to kernel log)
- [ ] Test by writing a tiny userspace "hello world" that calls `sys_kprint` and exits via halt

#### First userspace process

- [ ] Construct a `Process` with `AddressSpace` from a hardcoded ELF image
- [ ] Start its main thread
- [ ] Verify it runs, calls `sys_kprint`, output appears on serial
- [ ] **This is the substrate-works milestone**

#### Handle operation syscalls

- [ ] `sys_handle_close`
- [ ] `sys_handle_duplicate`
- [ ] `sys_handle_restrict`
- [ ] `sys_handle_stat`

#### Memory objects

- [ ] `MemoryObject` kernel object
- [ ] `sys_memory_create`
- [ ] `sys_memory_map` / `sys_memory_unmap`
- [ ] Userspace can allocate memory now

#### IPC

- [ ] `IpcChannel` kernel object per [docs/spec/ipc-message-format.md]
- [ ] Per-channel queue with configurable depth, slot pool allocation
- [ ] `sys_channel_create`
- [ ] `sys_channel_send` with Block / NoBlock / BlockBounded modes
- [ ] `sys_channel_recv`
- [ ] Handle transfer mechanics during send (move and duplicate paths)
- [ ] Dead-peer handling (`PeerClosed` notification, send/recv errors)

#### Notifications

- [ ] `NotificationChannel` kernel object per [docs/spec/notification-format.md]
- [ ] Bounded queue (default 64 entries) in kernel memory
- [ ] Notification enum with sparse category-based discriminants
- [ ] `sys_notif_recv`
- [ ] First notification variants: `ChildExited`, `SegFault`, `PeerClosed`
- [ ] Exception delivery path: thread fault → suspend → notification
- [ ] `sys_exception_resume` with Disposition enum
- [ ] Overflow handling (exception-priority eviction)

#### Wait queues

- [ ] `WaitQueue` with intrusive linked list per object
- [ ] `WaitNode` pre-allocated array on `Thread`
- [ ] `sys_wait` with multi-handle support and deadline
- [ ] DPC integration for wakeup (DPCs queued from IRQ context; wake threads via wait queue)
- [ ] Unified wait works across `PendingOperation`, `IpcChannel`, `Timer`, `NotificationChannel`, `Process`

#### Timers and clocks

- [ ] `Timer` kernel object
- [ ] Kernel timer min-heap
- [ ] `ArchTimer` trait with amd64 implementation (TSC + APIC timer + HPET for calibration)
- [ ] `sys_timer_create` / `sys_timer_set`
- [ ] `sys_clock_read` (Monotonic, Realtime, ProcessCpu, ThreadCpu)

#### Other syscalls

- [ ] `sys_process_spawn` per [docs/spec/syscall-abi.md]
- [ ] `sys_process_exit`, `sys_thread_exit`
- [ ] `sys_thread_create`
- [ ] `sys_thread_set_affinity`
- [ ] `sys_thread_get_registers`

#### Architecture trait completion

- [ ] `ArchIrq` (interrupt controller, APIC + IOAPIC on amd64)
- [ ] `ArchCpu` (CPU init, feature detection, halt)
- [ ] `ArchSmp` (SMP bootstrap, IPI) — basic version, full SMP comes in Phase 3
- [ ] `ArchFpu` (XSAVE/XRSTOR)
- [ ] `ArchUserAccess` (SMAP/PAN window management)

### Milestone

Two userspace processes communicate via IPC. Both are spawned by a third (parent) process. The parent receives `ChildExited` notifications via `sys_wait` on its notification channel. Hardware exception (segfault) is delivered to the faulting process's notification channel; the process can resume or terminate via `sys_exception_resume`.

### Notes / deviations

(Add notes here.)

---

## Phase 2: Filesystem and namespace

**Goal:** the namespace subsystem, the resource server protocol, the first real filesystem. Init runs, processes its bootstrap manifest, mounts ext4, reads files.

### Tasks (in suggested execution order)

#### Namespace and resource server foundation

- [ ] `Namespace` kernel object with binding tree (RB-tree or similar)
- [ ] Path resolution engine (parse, walk, dispatch)
- [ ] Lookup cache (recently-resolved paths → handles)
- [ ] `ResourceServer` trait per [docs/architecture/namespace-and-resource-servers.md]
- [ ] `OpStatus` (Completed, Pending, Rejected)
- [ ] `ResourceServerRegistry` (flat list of registered servers)
- [ ] `sys_ns_create`
- [ ] `sys_ns_lookup`
- [ ] `sys_ns_bind` (gated by `SysCaps::BIND_NAMESPACE`)
- [ ] `sys_ns_unbind`

#### In-kernel resource servers

- [ ] Initramfs resource server (parses Limine-loaded CPIO newc blob)
- [ ] Device resource server stub (`/dev`)
- [ ] Process resource server stub (`/proc`)
- [ ] Kernel log resource server (`/dev/log`)
- [ ] Entropy resource server (`/dev/entropy`) — with the entropy subsystem from below
- [ ] Framebuffer resource server (`/dev/framebuffer`) — for pre-compositor era
- [ ] Synthetic `/proc/self/*` resources

#### Entropy

- [ ] Hardware RNG access (RDSEED preferred, RDRAND fallback)
- [ ] Software entropy mixing (TSC jitter at interrupt dispatch)
- [ ] ChaCha20 CSPRNG with periodic reseed
- [ ] `EntropyObject` handle, blocks until pool is seeded

#### Init (PID 1)

- [ ] `userspace/init/` crate, `libkern + alloc` only
- [ ] Initial handle set reception from kernel
- [ ] Minimal TOML parser (just enough for init.toml schema)
- [ ] init.toml parsing per [docs/spec/init-toml-schema.md]
- [ ] Critical-path mount processing loop (topological sort by mount_point depth)
- [ ] Reaping loop for `ChildExited` notifications
- [ ] `sys_release_initramfs` syscall and init's call to it once boot is stable

#### Storage drivers

- [ ] PCI/PCIe enumeration via ECAM (MCFG-based on amd64)
- [ ] DeviceNode kernel objects for discovered devices
- [ ] AHCI driver (start here; simpler than NVMe)
- [ ] IRP framework per [docs/architecture/drivers-and-irps.md]
- [ ] InterruptObject kernel object for hardware IRQs
- [ ] Block device resource server registration

#### Partition handling

- [ ] GPT driver (Tier 1)
- [ ] Partition DeviceNode registration
- [ ] `/dev/disk/by-partuuid/*` and `/dev/disk/by-partlabel/*` namespace entries

#### Filesystem in userspace

- [ ] `userspace/librsproto/` crate per [docs/spec/rsproto-wire-format.md]
- [ ] Meta operations: Hello, Goodbye, QueryCaps, Ping, Ready
- [ ] Version negotiation
- [ ] `userspace/fs-server-ext4/` crate
  - [ ] ext4 superblock parsing
  - [ ] Inode reading
  - [ ] Directory lookup
  - [ ] File data reading via extents
  - [ ] Read-only mode is the Phase 2 target; RW is Phase 3
- [ ] Resource server startup protocol implementation
  - [ ] Control channel + Ready handshake
  - [ ] Init binds the endpoint via `sys_ns_bind`

#### Page cache integration with fs-server

- [ ] Page cache for file-backed memory in kernel
- [ ] `sys_memory_map` on file handles asks fs-server for extents
- [ ] Kernel reads blocks into page cache pages, maps into client address space
- [ ] Writeback (deferred to Phase 3 along with fs-server-ext4 RW)

#### Emergency shell

- [ ] `userspace/eshell/` crate
- [ ] Minimal command interface over serial console
- [ ] Inspect mounts, list block devices, read kernel log, edit initramfs files, reboot
- [ ] Init invokes eshell on critical-path failure

#### FAT for completeness (RO is fine for now)

- [ ] `userspace/fs-server-fat/` crate (FAT32/FAT16/FAT12 read-only)
- [ ] Required because UEFI mandates FAT32 for the ESP

### Milestone

`xtask qemu` boots to a system that:
1. Boots Limine from the FAT32 ESP
2. Kernel comes up, initializes subsystems, enumerates PCI
3. Init starts from the initramfs
4. Init reads `init.toml`, spawns fs-server-ext4 for the ext4 root partition, waits for Ready, binds the endpoint at `/`
5. Init reads `/system/current-generation` and logs the contents to the kernel log
6. Init enters its reaping loop

Disk image is built by `xtask build-disk` with a real ext4 partition containing test data.

### Notes / deviations

(Add notes here.)

---

## Phase 3: Service ecosystem

**Goal:** the userspace ecosystem. Service manager, profile servers, runtime libraries, the standard services. Scheduler matures. SMP works.

### Tasks (rough order; many can proceed in parallel)

#### Scheduler maturation

- [ ] Three scheduler classes: RealTime, TimeShared, Idle
- [ ] Per-CPU runqueues
- [ ] Work stealing
- [ ] Affinity placement on wake
- [ ] `sys_thread_set_affinity` fully functional

#### SMP

- [ ] Full SMP bring-up via Limine
- [ ] Per-CPU initialization (GS base, per-CPU data structures)
- [ ] Per-CPU scheduler instances
- [ ] Per-CPU APIC timers
- [ ] TLB shootdown via IPI with active_cpus mask

#### Service manager

- [ ] `userspace/service-mgr/` crate
- [ ] TOML parser for service declarations per [docs/spec/service-toml-schema.md]
- [ ] Dependency graph + topological startup
- [ ] Process supervision with restart policies (never, on-failure, always)
- [ ] Exponential backoff for restarts
- [ ] Resource Server Startup Protocol for spawned RS-style services
- [ ] Lifecycle control via per-service control channels

#### Runtime libraries (full versions)

- [ ] `userspace/libos/` crate
  - [ ] `Handle<T, M>` with typestate
  - [ ] Async executor over `sys_wait`
  - [ ] All kernel object types wrapped
- [ ] `userspace/librt/` crate
  - [ ] Synchronous wrappers
  - [ ] Fiber scheduler (Go-style)
- [ ] `userspace/libstream/` crate
  - [ ] `TableWriter`, `TableReader`
  - [ ] `record_read`
  - [ ] `#[derive(TypedRecord)]` proc macro
  - [ ] Initial supported types per [docs/spec/typed-stream-format.md]

#### Profile server

- [ ] Generic profile server binary
- [ ] Profile manifest format (TOML in store)
- [ ] System profile manifest in initramfs (transitional) and store (post-bootstrap)
- [ ] Init or service manager binds profile server at `/bin`, `/lib`

#### Content-addressed store

- [ ] Store layout convention on the ext4 root: `/store/<hash>-<name>-<version>/`
- [ ] Read-only namespace bindings for `/store` (rights enforce immutability)
- [ ] Package manager daemon (basic: list, add, remove store paths; manage generations)
- [ ] Generation manifests
- [ ] GC (mark reachable store paths, sweep unreachable)

#### Logging service

- [ ] Log channel handle creation (capability-gated)
- [ ] `LogRecord` structure per architecture doc
- [ ] Logging service collects, indexes, persists records
- [ ] Multiple sinks: persistent DB, serial, in-memory ring

#### Audit subsystem

- [ ] Kernel audit ring buffer
- [ ] Audit service drains and persists
- [ ] Chained records (hash of previous) for tamper detection
- [ ] `SysCaps::AUDIT_CONTROL` for management

#### Other services

- [ ] Device manager (kernel module loader for Tier 2, `/dev` population)
- [ ] Namespace manager (system namespace coordination)
- [ ] Time sync service (NTP — depends on networking; defer the NTP part)
- [ ] OOM daemon (handles `Notification::MemoryPressure`)
- [ ] Mount daemon (post-boot dynamic mount/unmount)
- [ ] Crash reporter (exception notification handler, dumps, `rustfilt` symbolication)

#### Authentication and session management

- [ ] Authentication service (initially: trivial password file in store)
- [ ] Session manager
- [ ] Per-user namespace construction (overlay layers, subtree handles)
- [ ] User shell spawn with constructed namespace

#### fs-server-ext4 read-write

- [ ] Write path: block allocation, extent updates, journal interaction
- [ ] Writeback from kernel page cache (dirty page flushing)
- [ ] Filesystem consistency on power loss (journal replay on mount)

### Milestone

`xtask qemu` boots to a "system idle" state with:
- Multiple services running, all supervised by service manager
- A test program started by service manager produces typed (TableWriter-based) output to its log channel
- Two CPUs are visibly active (e.g., scheduler stats accessible via `/proc`)
- A user can log in, get a per-user namespace, and write files to their home directory

### Notes / deviations

(Add notes here.)

---

## Phase 4+: Shell and beyond

**Goal:** an interactive system. The phase distinction breaks down here — this is ongoing development rather than discrete phases. Items below are roughly ordered by foundational importance.

### Shell

- [ ] Basic interactive shell (Rust REPL evolving into the eventual shell over time)
- [ ] Pipeline support (typed streams between processes)
- [ ] Built-in operators: sort, filter, take, count, select
- [ ] The `display` verb (renders typed streams to terminal as ANSI tables)
- [ ] Text fallback for processes that emit plain text
- [ ] Shell grammar (deferred — see [docs/rationale/deferred-decisions.md])

### Display infrastructure

- [ ] Display server (renders typed streams as ANSI in terminal mode)
- [ ] `/dev/framebuffer` direct rendering for now
- [ ] Future: GPU driver as Tier 2 LKM
- [ ] Future: Compositor as userspace server (deferred — see [docs/rationale/deferred-decisions.md])
- [ ] Future: `WidgetRecord` rendering

### Networking

- [ ] Network driver (e1000 or virtio-net as starting point)
- [ ] Userspace netstack server (smoltcp port or from-scratch)
- [ ] Socket-as-namespace-resource architecture
- [ ] DHCP, DNS

### Additional filesystems

- [ ] fs-server-fat read-write (for ESP updates from within OS)
- [ ] fs-server-btrfs (if a use case emerges)
- [ ] fs-server-xfs (if a use case emerges)

### Phase 2 ACPI

- [ ] Trigger condition reached (laptop / graceful shutdown / etc.)
- [ ] Vendor ACPICA at `kernel/vendor/acpica/`
- [ ] OSL implementation in `kernel/src/kacpi/osl/`
- [ ] `bindgen` build integration
- [ ] Power management daemon

### aarch64

- [ ] amd64 implementation stable enough that porting is worthwhile
- [ ] Fill in `kernel/src/arch/aarch64/` stubs
- [ ] Equivalent userspace work
- [ ] First aarch64 target system identified

### std port

- [ ] Syscall ABI stable enough to commit to
- [ ] `std::fs`, `std::thread`, `std::net`, `std::sync`, `std::io` implementations
- [ ] Target spec: `x86_64-unknown-nitrox.json`
- [ ] First non-trivial external Rust crate ported

### Notes

This phase is open-ended. The implementation plan stops being useful as a tracking tool around here; ongoing work is better tracked as GitHub issues, project boards, or whatever workflow fits.

---

## Cross-cutting workstreams

Things that need ongoing attention across all phases, not phase-specific:

### Testing

- [ ] Host-side unit tests for everything that doesn't require the kernel runtime (allocators, parsers, data structures, ABI encoding)
- [ ] QEMU integration tests via `isa-debug-exit` for everything that does
- [ ] CI runs both on every push
- [ ] Add a test for any non-trivial bug fix

### Documentation

- [ ] Architecture deep-dive docs in `docs/architecture/` written alongside the corresponding implementation
- [ ] Reference catalogues (`docs/reference/`) — kernel objects, syscalls, error codes, syscaps, rights — grown as the kernel grows
- [ ] Convention docs (`docs/conventions/`) — code style, unsafe policy, testing — written from observed patterns

### Decision log

- [ ] `docs/history/decision-log.md` updated whenever a significant decision is made during implementation — what was decided, why, what alternatives were considered

### Conventions enforcement

- [ ] `unsafe` blocks have SAFETY comments (clippy lint where possible)
- [ ] No external crate dependencies introduced into the kernel
- [ ] Lock ordering documented in `kernel/docs/lock-ordering.md` updated as new locks are added

---

## Where this document lives

Recommended location: `docs/planning/implementation-plan.md` or `IMPLEMENTATION.md` at the repo root. The repo root has the advantage of being easy to find; `docs/planning/` keeps the docs tree tidy. Either is fine — pick one and stick with it.
