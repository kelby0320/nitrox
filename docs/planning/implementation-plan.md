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

- **Phase 0 (Foundation):** complete — kernel boots under QEMU+OVMF and
  renders a framebuffer boot screen. See the Phase 0 deviation notes for
  where it diverged from the original checklist.
- **Phase 1 (Kernel substrate):** in progress — the
  address-spaces-and-paging and user-memory-access slices are
  complete. Memory foundation (buddy / slab / `libkern` containers),
  kernel diagnostics (serial, GDT/TSS/IDT, fault dumps), the
  `ArchPaging` trait + x86_64 4-level page-table primitive, the VMA
  tree (interval-augmented intrusive RB-tree), `AddressSpace` (VMA
  tree + page-table root + lock under one `SpinLock<Inner>`), the
  static ELF loader, the shared higher-half kernel mapping across
  address spaces, per-thread kernel stacks with guard pages, and the
  user-memory-access discipline (`UserPtr<T>`/`UserMutPtr<T>`,
  exception table + `#PF` recovery, five copy primitives, boot-time
  SMAP/SMEP enable), and the handle-table slice (segmented table,
  per-entry seqlocks, lock-free lookup, shuffled freelist allocation,
  RCU-style deferred reclamation, owner-PID enforcement, ~30 host
  unit tests including multi-thread torn-read torture), and the
  kernel-object substrate (`KObjectHeader` + atomic refcount,
  `ObjectRef` RAII holder with `match`-on-`KObjectType` destructor
  dispatch, the first concrete `Process` and `Thread` types, the real
  `try_acquire_refcount`/`release_refcount` seam closing the
  `duplicate` TOCTOU, plus a multi-thread duplicate-vs-close torture
  test) are all in. Next: threading and the context switch (`Thread`
  register/FPU state, NASM context switch, the minimal round-robin
  scheduler).
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
- [x] Kernel entry point — pure-Rust `extern "C" fn _start` in `kernel/src/main.rs` (Limine sets up long mode, paging, GDT, and a stack, so no NASM boot stub is needed in Phase 0 — see deviation note)
- [x] Limine boot protocol integration: request structs in kernel binary, response handling in `kernel_main`
- [x] Minimal `kernel_main` that renders a boot screen to the framebuffer (serial output deferred — see deviation note)
- [x] Limine configuration file builds correctly
- [x] `tools/xtask/` workspace with the `xtask` binary crate
- [x] `xtask build` — builds kernel, assembles disk image
- [x] `xtask qemu` — runs the kernel under QEMU with serial console captured
- [x] `xtask qemu-debug` — runs QEMU with GDB stub enabled
- [x] `xtask test` — runs host-side unit tests (stub OK; will grow)
- [ ] `xtask test-qemu` — QEMU integration tests via `isa-debug-exit` (not built in Phase 0 — see deviation note)
- [x] GitHub Actions CI running `cargo build` and `xtask test` on every push
- [x] `docs/` populated with the foundational documents (overview, rationale, spec)
- [x] v5.1 design doc archived at `docs/history/design-doc-v5.1.md`
- [x] Decision log started at `docs/history/decision-log.md`

### Milestone

`xtask qemu` boots Limine, the kernel renders a boot screen to the framebuffer, then halts. CI is green. (Serial output was deferred to Phase 1 — see the deviation note below.)

### Notes / deviations

- No NASM boot stub. Limine drops the kernel into long mode with paging,
  a GDT, and a stack already set up, so a pure-Rust `extern "C" fn _start`
  is sufficient. A NASM stub returns for the context-switch path in
  Phase 1. (Decision log, 2026-05-13.)
- No serial output. Phase 0 renders to the framebuffer instead; the
  serial console was deferred. It lands in the Phase 1 "Kernel
  diagnostics" slice. (Decision log, 2026-05-13.)
- `xtask test-qemu` was not built — there is no QEMU integration-test
  harness yet. It lands when the first test that needs it does (serial
  output is a prerequisite). Tracked in the cross-cutting Testing
  workstream.
- Arch directory is `kernel/src/arch/x86_64/`, matching the Rust target
  triple `x86_64-unknown-none` and `cfg(target_arch = "x86_64")`. The
  `x86_64` naming is standardized across `CLAUDE.md` and the `docs/`
  tree (2026-05-20 doc-sync; see the decision log).

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
- [x] Intrusive linked list — deferred to the scheduler / wait-queue
  slice, where its first real consumer lands
- [x] Red-black / interval tree — deferred to the VMA slice; build the
  interval-augmented variant directly against the VMA manager's needs
- [x] `Arc`-equivalent for refcounted kernel object references
  (`ObjectRef`) — landed in the kernel-object-infrastructure slice as
  an RAII holder over `KObjectHeader`'s atomic refcount, in
  `kernel/src/object/header.rs`
  - Note: 2026-05-20 — the original three lines grouped six structures
    into the memory foundation. Reordered to a just-in-time schedule:
    `KBox` / `KVec` / `KString` now (zero design risk, needed within
    1–2 slices); the intrusive list, tree, and `KArc` when their first
    consumer lands, since each one's API is defined by a consumer that
    does not exist yet. See the decision log entry of 2026-05-20.

#### Kernel diagnostics and early fault handling

Pulled forward ahead of paging — this is the slice that makes the paging
work debuggable. Until it lands, `panic!`/`expect()` halt silently and a
CPU fault triple-faults with no output. Serial and a dump-and-halt IDT
are one unit; both belong before the first paging code.

- [x] Port I/O wrappers (`inb`/`outb`/`inw`/`outw`/`inl`/`outl`) plus a
  `read_cr2` in `kernel/src/arch/x86_64/regs.rs` — per `kernel/CLAUDE.md`,
  hardware-register access lives in the arch layer's `regs.rs`
- [x] Polled 16550 UART driver on COM1 in `kernel/src/arch/x86_64/serial.rs`
  - `init` + `write_byte`, no interrupts, no allocation
  - Behind a `SpinLock`; usable before paging and inside the panic handler
- [x] `kprint!` / `kprintln!` macros over a `core::fmt::Write` serial sink
  - Single sink for now; the multi-sink logging service is Phase 3 — do
    not pre-build it
- [x] Rewrite `#[panic_handler]` to dump `PanicInfo` (location + message)
  to serial before halting
- [x] Minimal IDT with dump-and-halt handlers for CPU exceptions
  - Dumps vector, error code, `CR2`, and key registers to serial
  - IST with a dedicated double-fault stack; the kernel's own GDT + TSS
    were added with it (the IST needs a TSS, the TSS needs a descriptor
    in a GDT the kernel owns)
  - IRQs stay masked (no DPCs yet), so `SpinLock` is still sufficient;
    `IrqSpinLock` arrives with the later interrupt-controller work
  - Dump-and-halt only; the exception-table-consulting `#PF` handler is
    a later item under "User memory access"
  - Note: 2026-05-20 — handlers cover all 32 CPU exception vectors
    (0–31), not just `#UD`/`#GP`/`#PF`/`#DF`: a uniform 32-stub macro is
    no more work and gives complete coverage. Stubs are naked Rust
    functions (`#[unsafe(naked)]` + `naked_asm!`), the `x86-interrupt`
    ABI being nightly-only.
- [x] Host-test the descriptor-encoding arithmetic (`IdtEntry::set_handler`,
  the TSS descriptor encoder)
  - Note: 2026-05-20 — the UART register sequence and the `kprintln!`
    formatting path were judged low-value to host-test (a fixed `outb`
    list; `core::fmt`'s own code) and are verified on target instead.

Done when: `xtask qemu` shows a kernel banner and boot progress on the
serial console, a deliberate `panic!` prints file/line/message, and a
deliberate bad dereference prints a `#PF` register dump instead of a
silent reset.

#### Address spaces and paging

- [x] `ArchPaging` trait in `kernel/src/arch/` with x86_64 implementation
  - `map_page`, `unmap_page`, `flush_tlb_*`, `set_page_table`
  - All `unsafe`, all with SAFETY comments
- [x] VMA structure with red-black tree storage
  - [x] `Vma` struct + `VAddrRange`, `Protection`, `MappingKind` types in
    `kernel/src/mm/vmm.rs`. `MappingKind` starts as `Anonymous`-only;
    `FileBacked` / `Device` variants land with their consumers.
    `Protection` is a narrower abstraction than `arch::PageFlags` (WRITE
    / EXEC / USER), translated to `PageFlags` at PTE-install time
  - [x] Intrusive RB-tree node embedded in `Vma`: parent / left / right
    / colour, plus `subtree_max_end` for interval augmentation
  - [x] RB-tree insert with overlap detection, CLRS-textbook fixup
    (rotations + recolour), augmentation maintenance on every structural
    mutation. Remove lands with the next sub-item
  - [x] Point lookup `VmaTree::find_covering(addr)` — plain BST walk;
    interval augmentation isn't needed for point queries
  - [x] Ownership: `VmaTree` owns `KBox<Vma>` (insert takes a box,
    returns it back on overlap rejection). `KBox::into_raw` /
    `from_raw` added for intrusive ownership. Iterative post-order
    `Drop` via parent pointers, no allocation
  - [x] Host-side tests: BST + RB + augmentation invariant checkers
    exercised on every insert across ascending, descending, and
    shuffled-insert sequences (200 randomised inserts, full invariant
    check after each); overlap rejection across all shapes
  - [x] `VmaTree::remove_covering(addr)`: BST-delete with in-order
    successor swap when the target has two children; CLRS-textbook
    delete-fixup with all four cases (mirrored), tested against
    shuffled-insert + shuffled-remove sequences with full invariant
    verification after every operation
  - [x] `find_first_overlapping(range)` — O(log n) leftmost-overlap
    BST walk; `iter_overlapping(range)` — in-order iterator over the
    contiguous overlap run with parent-pointer successor advance;
    `iter()` — full in-order iterator. Augmentation maintained but
    not consumed by these queries (leftmost-overlap is already
    O(log n) without it; pruning matters for future disjoint-range
    queries)
  - [x] Update [docs/architecture/memory-management.md] to point at
    `mm/vmm.rs` and drop the "not yet" annotation in the layer table.
    Added a `## VMA tree` section describing structure, augmentation,
    queries, and the Send/Sync story; added a Phase 1 limitation
    noting the missing `AddressSpace` owner
- [x] Address space construction from an ELF image
  - [x] `AddressSpace` skeleton in `kernel/src/mm/addr_space.rs`:
    `VmaTree` + page-table root paired under a single `SpinLock<Inner>`
    (rank 4). `new()` allocates and zeroes a fresh PML4 frame.
    `map_vma(KBox<Vma>)` validates the range (canonical + user-half),
    pre-checks tree overlap, then allocates+zeros+installs one frame
    per page in lockstep (with full rollback on failure), and commits
    the VMA to the tree. `unmap_covering(addr)` is the inverse. `Drop`
    drains the tree, uninstalls every PTE, frees leaf frames, frees
    the PML4. No TLB flush yet (no AS is "active" until the scheduler
    lands); no higher-half kernel mapping yet (the next sub-item)
  - [x] In-kernel ELF loader for **static** binaries:
    `mm::elf::load_elf(asp, bytes) -> Result<EntryInfo, ElfLoadError>`.
    Hand-rolled ELF64 parser (no external crates), validates header
    (magic / class / data / version / machine / type), walks program
    headers, allocates a VMA + zeroed frames + copies file bytes for
    each PT_LOAD, rejects PT_INTERP (dynamic linking is a userspace
    `ld.so` concern). Sets up an initial 4-page stack VMA at a fixed
    top-of-user-space address; returns the entry point and stack top.
    argv / envp / auxv stack-area setup deferred to "first userspace
    process" where the userspace runtime defines the handoff format
- [x] Higher-half kernel mapping shared across all address spaces:
  `ArchPaging::inherit_kernel_mappings(root)` populates a fresh PML4's
  kernel half from a boot-captured template. x86_64 impl copies entries
  256..512 from a `SpinLock<Option<[u64; 256]>>` snapshot of Limine's
  PML4 (captured by `init_kernel_template(active_root())` at boot,
  before any AS construction). aarch64 (when implemented) will be a
  no-op given TTBR0/TTBR1 split. `AddressSpace::new()` calls it after
  zeroing the freshly-allocated PML4. The intermediate PDPTs the
  template points at are now shared across every AS, so future
  kernel-vmap allocations propagate to every AS automatically
- [x] Per-thread kernel stack with guard page:
  `mm::kvmap` is a bump-pointer kernel-vmap allocator hands out
  virtual address ranges in `0xFFFF_C000_0000_0000..0xFFFF_D000_0000_0000`
  (16 TiB). `mm::kvmap::init` runs at boot before
  `init_kernel_template` and calls
  `ArchPaging::ensure_kernel_intermediate` to pre-allocate the
  vmap PDPT, so the captured template includes it and every AS
  inherits the shared sub-tree. `mm::kstack::KernelStack::new(root)`
  reserves `KERNEL_STACK_PAGES + 1` vmap pages, allocates frames
  for the top N (writable / NX / kernel-only), leaves the bottom
  page as an unmapped guard. Drop unmaps the PTEs and frees the
  frames; the vmap region itself is not reclaimed (no freelist —
  fine for Phase 1's churn rate). No production consumer yet —
  threading consumes when it lands

#### User memory access discipline

- [x] `UserPtr<T>` and `UserMutPtr<T>` opaque wrapper types
- [x] Exception table mechanism: `(fault_pc, recovery_pc)` pairs registered at compile time
- [x] Copy primitives: `copy_from_user`, `copy_to_user`, `copy_slice_from_user`, `copy_slice_to_user`, `copy_cstr_from_user`
- [x] SMAP/SMEP discipline: `stac`/`clac` only within copy routines
- [x] Upgrade the `#PF` handler (installed dump-and-halt in the diagnostics slice) to consult the exception table before VMA lookup
  - Note: the VMA-lookup branch is deferred until the scheduler lands (no active address space exists yet, so a fault that misses the exception table is necessarily a kernel bug). The exception-table consultation is in place; `pf_dispatch` will grow a second decision step when the scheduler arrives.
- [x] [docs/spec/user-memory-access.md] (write this spec while implementing)

#### Handle table

- [x] Segmented handle table per [docs/spec/handle-encoding.md]
- [x] `HandleEntry` with seqlocks
- [x] Lookup path (lock-free common case)
- [x] Allocation with randomized slot allocation (shuffled free list)
- [x] Close with deferred reclamation
- [x] Per-process quiescent state counter for RCU-style grace periods
  - Implemented as a generic `GraceTracker` keyed by `current_ctx_id()`. In Phase 1 (single CPU, no preemption, no `Process`) every operation runs in context 0; the shim is replaced wholesale when SMP or `Process` lands. See `docs/architecture/handle-system.md` § "Grace tracking".
- [x] Owner-PID enforcement on every lookup
- [x] Host-testable: build the handle table standalone, hammer it from threads, verify invariants
  - 8-thread allocate/lookup/close stress preserves cross-pid isolation; multi-thread torn-read torture proves the seqlock never returns inconsistent snapshots. See `kernel/src/handle/table.rs` `#[cfg(test)] mod tests`.

#### Kernel object infrastructure

- [x] `KObjectHeader` with refcount and type tag
  - `#[repr(C)]` `{ refcount: AtomicUsize, object_type: KObjectType }`
    in `kernel/src/object/header.rs`; ABI-critical (see
    `docs/spec/abi-version-hash.md`). `Arc`-discipline orderings:
    `Relaxed` increments, `Release` decrement, `Acquire` fence on the
    last release, fail-at-zero `try_acquire`, and a `MAX_REFCOUNT`
    overflow guard.
- [x] `KObjectType` enum
  - Already declared in `kernel/src/libkern/handle.rs` (`#[repr(u32)]`,
    the v5.1 `repr(u16)` is superseded by source); reused as the
    header's type tag.
- [x] Match-dispatch pattern for type-specific operations
  - `dispatch_destroy` runs the concrete destructor via `match` on
    `KObjectType` (not `dyn`), reconstituting the owning `KBox`.
- [x] `ObjectRef` RAII refcount holder with try_acquire seqlock interaction
  - `lookup` step 7 acquires via the header and step 12 wraps it in an
    `ObjectRef`; `LookupOk { object: ObjectRef, rights }`. `into_raw` /
    `from_raw` transfer references for `duplicate` and `close`.
- [x] First kernel objects: `Process`, `Thread` (no other types yet)
  - Minimal: header + identity fields (`Process` also carries a
    self-check `magic` sentinel used by the torture tests). Register/FPU
    state, address space, sched params, and the Process↔Thread graph
    arrive with the threading / process-management slices.
- [x] **Close `HandleTable::duplicate` TOCTOU.** Closed: `lookup` returns
  an `ObjectRef` holding a reference across the `lookup`→`allocate` gap,
  which `duplicate` transfers straight into the new handle via
  `into_raw` (no decrement in the gap); a concurrent `close` can drop at
  most the source handle's reference, never the object's last one.
  `allocate` adopts the caller-transferred reference without bumping;
  on `allocate` failure `duplicate` reclaims and releases it. `close`
  transfers (does not decrement) the handle's reference to the returned
  `ClosedObject(*mut (), KObjectType)`. Verified by
  `concurrent_duplicate_vs_close_toctou_torture` and the per-operation
  refcount-accounting tests in `kernel/src/handle/table.rs`.

#### Threading and context switch

- [ ] `Thread` kernel object with register state, FPU context, kernel stack, sched params
- [ ] FPU state: XSAVE area per thread, init values, save/restore primitives
- [ ] Context switch stub in NASM (`kernel/src/arch/x86_64/context_switch.asm`)
- [ ] Rust-side context switch handler called from NASM stub
- [ ] Minimal scheduler: round-robin between kernel threads, no classes yet
- [ ] TLS support: FS_BASE handling, `sys_thread_set_tls` (when syscalls exist)

#### Syscall entry/exit

- [ ] `syscall` instruction handler (x86_64) with `swapgs`, register save
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
- [ ] `ArchTimer` trait with x86_64 implementation (TSC + APIC timer + HPET for calibration)
- [ ] `sys_timer_create` / `sys_timer_set`
- [ ] `sys_clock_read` (Monotonic, Realtime, ProcessCpu, ThreadCpu)

#### Other syscalls

- [ ] `sys_process_spawn` per [docs/spec/syscall-abi.md]
- [ ] `sys_process_exit`, `sys_thread_exit`
- [ ] `sys_thread_create`
- [ ] `sys_thread_set_affinity`
- [ ] `sys_thread_get_registers`

#### Architecture trait completion

- [ ] `ArchIrq` (interrupt controller, APIC + IOAPIC on x86_64)
- [ ] `ArchCpu` (CPU init, feature detection, halt)
- [ ] `ArchSmp` (SMP bootstrap, IPI) — basic version, full SMP comes in Phase 3
- [ ] `ArchFpu` (XSAVE/XRSTOR)
- [ ] `ArchUserAccess` (SMAP/PAN window management)

### Milestone

Two userspace processes communicate via IPC. Both are spawned by a third (parent) process. The parent receives `ChildExited` notifications via `sys_wait` on its notification channel. Hardware exception (segfault) is delivered to the faulting process's notification channel; the process can resume or terminate via `sys_exception_resume`.

### Notes / deviations

- 2026-05-27 — VMA tree design call: RB-tree operations are iterative
  rather than recursive. With parent pointers (required for an intrusive
  tree anyway), insert/delete rebalancing walks up the tree naturally;
  search and in-order iteration become iterative trivially. Removes a
  kernel-stack-depth concern as a real tradeoff. Matches Linux
  (`lib/rbtree.c`).
- 2026-05-27 — VMA tree design call: `KBox<Vma>` over a per-address-space
  arena. VMAs come and go constantly (every `mprotect` boundary-cross
  splits a VMA), so an arena either needs an internal free-list (which
  is just the slab again) or fragments. Slab-backed allocation matches
  Linux's `vm_area_cachep` model. Revisit if profiling ever shows the
  slab is a bottleneck — the change is local to `VmaTree`.

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

- [ ] PCI/PCIe enumeration via ECAM (MCFG-based on x86_64)
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

- [ ] x86_64 implementation stable enough that porting is worthwhile
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
