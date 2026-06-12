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
- **Phase 1 (Kernel substrate):** **complete** — every milestone clause
  ships (IPC between two processes, a parent spawning + reaping them, and
  supervised exception suspend/resume). Memory foundation (buddy / slab / `libkern` containers),
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
  test) are all in. Threading and the context switch are also in: the
  `Thread` object carries its saved kernel register state, kernel stack,
  lifecycle state and entry point; a Rust-emitted `#[unsafe(naked)]`
  `context_switch` performs the cooperative switch; and a minimal
  round-robin scheduler runs kernel threads (demonstrated by the boot-time
  worker round-robin on the serial console). Syscall entry/exit is also in:
  the `syscall`/`sysretq` fast path (MSR setup, per-CPU `swapgs` block, the
  naked entry stub + `SyscallFrame`), a `match`-dispatch table with `KError`
  encoding, and `sys_kprint`. The **first userspace process** is also in — the
  **substrate-works milestone**: an embedded `ET_EXEC` (`userspace/hello`) is
  loaded into an `AddressSpace`, wrapped in a `Process`, and run as a
  scheduler-driven user thread that prints from ring 3 and exits via
  `sys_process_exit`; the scheduler now manages per-thread CR3. The real
  syscall surface then landed in full — handle ops, memory objects, clocks +
  timers, `sys_wait`, notifications, IPC (`sys_channel_create`/`send`/`recv`)
  with handle transfer, process spawn + lifecycle + `ChildExited`, and finally
  `sys_thread_create` + supervised exception suspend/resume — closing the Phase 1
  milestone. Next: Phase 2 (ACPI, SMP, the filesystem + a real init/service
  manager, namespaces, and the async-I/O ring).
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

- No NASM anywhere. Limine drops the kernel into long mode with paging,
  a GDT, and a stack already set up, so a pure-Rust `extern "C" fn _start`
  is sufficient. The context switch, originally slated for NASM, also
  landed as Rust-emitted `naked_asm!` — consistent with every other piece
  of kernel assembly and free of any assembler in the build. (Decision
  log, 2026-05-13 and 2026-05-29.)
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

- [x] `Thread` kernel object with saved register state (saved stack
      pointer; the per-arch representation stays inside the arch layer), kernel
      stack, lifecycle state, and entry point (`kernel/src/object/thread.rs`).
      Sched params are round-robin-implicit; no priority/class yet.
- [ ] FPU state: XSAVE area per thread, init values, save/restore primitives
      — **deferred** to the first-userspace-thread slice. The kernel is
      soft-float and never touches the FPU, so eager XSAVE has nothing to
      preserve between kernel threads and cannot be exercised until
      userspace exists.
- [x] Context switch emitted from Rust as a `#[unsafe(naked)]`
      `context_switch` (`kernel/src/arch/x86_64/context.rs`), **not** NASM —
      consistent with the kernel's existing Rust-emitted asm and free of an
      assembler in the build. (Decision log, 2026-05-29.)
- [x] New-thread bootstrap: fabricated initial frame + naked
      `thread_trampoline` → `thread_enter` runs the body.
- [x] Minimal cooperative scheduler: round-robin between kernel threads,
      no classes yet (`kernel/src/sched.rs`); boot-time worker demo proves
      it on the serial console.
- [ ] TLS support: FS_BASE handling, `sys_thread_set_tls` — **deferred**
      until syscalls and userspace exist (FS_BASE is unused in ring 0).

#### Syscall entry/exit

- [x] `syscall` instruction handler (x86_64) with `swapgs`, register save
      (`kernel/src/arch/x86_64/syscall.rs`): MSR setup (EFER.SCE, STAR,
      LSTAR, SFMASK, KERNEL_GS_BASE), per-CPU `CpuLocal`, the naked
      `syscall_entry` stub building a `SyscallFrame`, and `sysretq`. GDT
      reordered for the SYSRET selector constraint (user data 0x18, user
      code 0x20, TSS → 0x28).
- [x] Syscall dispatch table (`kernel/src/syscall/{mod,table,error}.rs`):
      `match`-on-number dispatch, `KError` (`#[repr(i32)]`) + `isize`
      encoding.
- [x] First syscall: `sys_kprint(ptr, len)` (debug only — copy a user
      buffer in via the SMAP-safe primitive, write to serial).
- [x] Tested by a throwaway hand-assembled ring-3 blob (`run_user_demo` in
      `main.rs` + `arch::enter_user`/`syscall_debug_exit`) that calls
      `sys_kprint` then a debug-exit syscall which round-trips back to the
      kernel. Serial shows `hello, ring3` from ring 3. **This harness is
      throwaway** — the next slice replaces it with a real ELF-loaded
      process and a scheduler-driven user thread.

#### First userspace process

- [x] Construct a `Process` with `AddressSpace` from a hardcoded ELF image:
      the embedded `userspace/hello` ELF (static, non-PIE `ET_EXEC`, built by
      `cargo xtask` before the kernel and `include_bytes!`d) is loaded by
      `mm::elf::load_elf` into a fresh `AddressSpace`, wrapped in
      `Process::try_new_user` (the `Process` now owns its address space).
- [x] Start its main thread via the scheduler: `Thread::try_new_user` +
      `sched::spawn_user`; on first run `thread_enter` descends to ring 3 via
      the neutral `arch::enter_user`. The scheduler now loads each thread's
      page-table root on switch-in (kernel/boot root or the process root),
      which also restores the boot root before a dying user thread's address
      space is reaped. Per-thread `TSS.RSP0` + per-CPU syscall stack are set
      on descent. The throwaway `run_user_demo`/`enter_user(cr3)`/
      `syscall_debug_exit` harness is removed.
- [x] Verified: `cargo xtask qemu` serial shows `hello from ring 3 (pid 1)`
      (printed by `sys_kprint` from ring 3), then the process exits via
      `sys_process_exit` → `sched::exit` and the boot thread resumes.
- [x] **This is the substrate-works milestone** — reached.

> **2026-06-04 re-sequencing.** The remaining Phase 1 slices were reordered
> so the IRQ/timer/preemption **infrastructure** precedes the **blocking**
> subsystems that depend on it. The async-first model makes `sys_wait` the
> one blocking primitive, and wait queues / blocking IPC / notification
> (exception) delivery all funnel through it — they need timers (deadlines),
> an interrupt controller (IRQ-driven wakeup), and a `Blocked` thread state,
> all of which used to be ordered *after* them. New order:
> handle ops → memory objects → arch traits → timers → preemptive scheduling
> → wait queues → notifications → IPC → other syscalls. See the decision log.

#### Handle operation syscalls

Synchronous; no blocking dependencies. Builds on the existing **global**
handle table — a single globally-numbered segmented table with a per-entry
`owner_pid` checked on every lookup (per-process tables are explicitly
rejected; transfer would otherwise be a two-table operation — see
`docs/rationale/rejected-approaches.md`). What this slice added: a single
global `HandleTable` instance (once-init cell, `kernel/src/handle/global.rs`),
the dispatcher resolving the **calling process's pid** (current thread →
`owner_pid`, via `sched::current_owner_pid`) as `caller_pid`, the four
handlers, and the `HandleInfo` `#[repr(C)]` boundary type.

> **`next_owned` deferred.** Wiring the `HandleEntry::next_owned` owned-handle
> list for release-at-exit (mentioned in the 2026-06-04 re-sequencing note) is
> **moved to the Process slice** — it needs a `Process` list-head field and an
> exit-path walk (process-lifecycle work). The field stays `RawHandle::NULL`
> until then. (Decision log, 2026-06-04 slice-12.)

> **Sequencing.** This slice's deliverable is "the operations exist and are
> correct" (host-tested), not a userspace-capability milestone. Userspace
> obtains its first handle by *creating* an object (`sys_memory_create`, next
> slice), not by bootstrap delivery — so the **Memory objects** slice is where
> these syscalls first run in ring 3. Inter-process handle delivery
> (`SpawnArgs.handles`) stays in "Other syscalls".

- [x] `sys_handle_close`
- [x] `sys_handle_duplicate`
- [x] `sys_handle_restrict`
- [x] `sys_handle_stat`

#### Memory objects

Synchronous; no blocking dependencies. First slice with a real
"userspace can do X" milestone — `sys_memory_create` mints a `MemoryObject`
**and its handle** (tagged with the caller's pid) and returns it, so this is
also where the handle syscalls first run end-to-end in ring 3.

- [x] `current_process()` → `AddressSpace` resolution (current thread →
      `Process`; the small shared primitive, on top of the handle-ops slice's
      `sched::current_owner_pid`). `sys_memory_map` maps into it.
- [x] `MemoryObject` kernel object — **owns its frames** (eager alloc + zero;
      freed on last-ref drop); mapped via `AddressSpace::map_object` (a
      `MappingKind::Object` VMA holding an `ObjectRef`), so double-map aliases.
- [x] `sys_memory_create` (allocates the object + a handle in the global table)
- [x] `sys_memory_map` / `sys_memory_unmap` (numbers 5/6; `unmap` whole-VMA in
      Phase 1, `size` not yet honored)
- [x] Userspace can allocate memory now
- [x] Handle-ops ring-3 exercise: `hello` calls `sys_memory_create` →
      `sys_memory_map`, round-trips a byte through the mapped page, then
      `sys_handle_stat`/`duplicate`/`restrict`/`close` on the handle — the
      end-to-end proof deferred from the handle-ops slice.
- [x] Fixed a syscall-ABI bug surfaced by the ring-3 exercise: the entry stub
      no longer zeroes the argument registers on `sysretq` (it preserves all
      GPRs but `RAX`/`RCX`/`R11`, per the spec). See the decision log.

#### Architecture trait completion

Moved ahead of the blocking subsystems: the IRQ controller and CPU/FPU
primitives are prerequisites for timers, preemptive scheduling, and
therefore wait queues / blocking IPC / notifications.

- [x] `ArchIrq` — **local APIC only** for Phase 1: discovered via the
      `IA32_APIC_BASE` MSR (no ACPI). Brought up in **xAPIC (MMIO)** mode (the
      register page mapped uncached through the kernel vmap) — **not** x2APIC,
      which QEMU/TCG does not emulate (see the decision log). IF stays masked;
      the timer LVT is the Timers slice, the spurious/timer IDT stubs + `IF=1`
      the Preemptive slice. IOAPIC + external-device IRQ routing need ACPI
      (MADT) and are deferred to Phase 2 (the UART is polled).
- [x] `ArchCpu` — feature detection (`has_apic`) + `halt` (the new surface
      this slice needs). Folding the existing CPU boot free fns in is the Arch
      boundary normalization slice.
- [ ] `ArchFpu` (XSAVE/XRSTOR) — **deferred until a userspace thread can touch
      the FPU.** The preemptive-scheduling slice (2026-06-08) considered wiring
      it but found no consumer: the kernel is soft-float and the single user
      thread is soft-float, so no thread touches the FPU and a preempt→switch→
      resume cannot corrupt FPU/XMM state. It lands with its first real consumer
      (a hard-float userspace target or a second FPU-using thread), wired into
      both switch paths then.
- [x] `ArchUserAccess` — formalises the existing SMAP copy primitives as a
      neutral trait (asm + exception table unchanged).
- [x] `ArchSmp` — single-CPU stub (`cpu_count()==1`, `current_cpu()==0`,
      `send_ipi` unimplemented); full SMP is Phase 3.

#### Arch boundary normalization

Pure, behaviour-preserving refactor (no downstream dependency — can float):
apply the arch-boundary trait convention to the legacy free-fn surface. Fold
the paging companions (`translate`, `active_root`, `init_kernel_template`) into
`ArchPaging`; gather the CPU free fns (`init_cpu_tables`, `init_protections`,
`set_kernel_stack`, `halt_loop`) into `ArchCpu`. Leave naked-asm glue
(`context_switch`/`enter_user`/syscall entry), the `serial` singleton, and
`abi` data as free fns/modules. Re-points callers in `sched`/`mm`/`main`. See
`docs/conventions/arch-boundary.md`.

- [x] Fold paging-companion free fns into `ArchPaging`
      (`translate`/`active_root`/`init_kernel_template`)
- [x] Gather CPU boot free fns into `ArchCpu` (`init_tables`/`init_protections`/
      `set_kernel_stack`/`halt_loop`)

#### Timers and clocks (timekeeping foundation)

Scoped to the minimum that unblocks preemptive scheduling (the next slice).
The `Timer` kernel object, the deadline min-heap, and `sys_timer_create`/`set`
are deferred to **Wait queues** below — their consumers (firing via the
unmasked timer IRQ, `sys_wait` deadlines, notification signalling) live there,
so building them earlier would be untested scaffolding. (Decision log,
2026-06-08.)

- [x] `ArchTimer` trait with x86_64 implementation: **LAPIC timer (count-down
      mode, not TSC-deadline — TCG) + TSC, calibrated against the legacy PIT**
      (no ACPI). HPET (which needs ACPI to locate) is deferred to Phase 2.
      Arming methods are dormant (IF=0) but the countdown is observable.
- [x] `sys_clock_read` — **Monotonic only** this slice; `Realtime`/`ProcessCpu`/
      `ThreadCpu` return `Unsupported` (Realtime needs a wall-clock offset
      service; the per-CPU clocks need scheduler CPU accounting).

#### Preemptive scheduling (single-CPU)

Switches the cooperative scheduler (the threading slice) to a preemptive
one, still on a single CPU. Depends on a periodic timer (Timers and clocks)
and an enabled interrupt controller (`ArchIrq`). Deliberately separate from
SMP: get preemption correct on one CPU first; multiple CPUs come in Phase 3.
(Decision log, 2026-05-29; re-sequenced 2026-06-04; landed 2026-06-08.)

- [x] `IrqSpinLock` — the `cli` + save/restore-`RFLAGS` lock variant. Audit
      done: only `SCHED` (rank 1) and `SERIAL` (rank 7) are reachable from the
      timer IRQ, so only those two became `IrqSpinLock`; all other locks stay
      plain `SpinLock` (the handler touches nothing else and never allocates).
- [x] Enable interrupts (`IF=1`) — the model-wide flip from "interrupts masked
      everywhere"; IF control added to `ArchCpu` (`interrupts_enable/disable/
      restore/enabled`); armed at boot after the scheduler + timer are up.
- [x] Preemptive switch path — the timer IRQ stub builds the full interrupt
      frame (like the exception stubs) and **reuses** `context_switch` from
      inside the handler: the frame sits on the kernel stack below the switch's
      parked callee-saved frame, so a later resume returns into the stub
      epilogue and `iretq`s back. The cooperative `yield_now`/`exit` path is
      retained; both share the `switch_to_next` core.
- [x] Timer-tick reschedule: scheduler-side quantum (`QUANTUM_TICKS`, one
      10 ms tick) → round-robin reschedule. Round-robin only (no classes yet).
- [x] Idle thread: a kernel thread that `hlt`s (IF=1) when the run queue is
      empty; kept out of the ready/reap sets, reaps the exited boot thread.
- [ ] `Blocked` thread state + block/unblock — **moved to the Wait queues
      slice** (its only consumer is `sys_wait`; adding it here would be dead
      code). See Wait queues.
- [ ] Wire eager FPU save/restore (`ArchFpu`) into **both** switch paths —
      **deferred** until a userspace thread can actually touch the FPU
      (kernel is soft-float and the single user thread is soft-float, so no
      thread touches the FPU and a preempt→switch→resume cannot corrupt
      FPU/XMM state — eager XSAVE would be dormant code).

Note: the threading slice's global `SCHED`/`current` are explicit
single-CPU stand-ins. Phase 3 SMP refactors `SchedState` into per-CPU
instances, `current` into GS-based per-CPU data, and points
`current_ctx_id()` (the handle-table grace shim, currently constant 0) at
`arch::cpu_id()`. The cooperative switch and `Thread` layout are unchanged
by that refactor.

#### Wait queues

With the IRQ-driven scheduler and timers in place, `sys_wait` (the unified
blocking primitive) and per-object wait queues land.

- [x] `Blocked` thread state + block/unblock scheduler operations
      (`block_current_and_switch` / `make_runnable`; blocked threads parked in
      `SchedState::blocked`).
- [x] `Timer` kernel object (`object/timer.rs`; syscalls 8/9).
- [x] Kernel timer deadline min-heap (`sched::deadline`, a `KVec` binary heap;
      tagged entries for timer-fire vs `sys_wait` thread-deadline).
- [x] `sys_timer_create` / `sys_timer_set` (numbers 8/9).
- [x] Wait queues + per-thread wait slots — realized as a pre-reserved
      `KVec<*mut ()>` waiter list per `Timer` + a fixed wait-slot array on
      `Thread` (`MAX_WAIT_HANDLES` = 8), **not** the intrusive-linked-list
      design — simpler and allocation-free for Phase 1; the intrusive list is a
      later scale/SMP optimization.
- [x] `sys_wait` with multi-handle support and deadline (number 10; deadline via
      the min-heap; **direct wakeup on the periodic tick**, not via DPC).
- [ ] DPC integration for wakeup — **deferred**: the direct-wakeup tick path
      suffices; build the DPC queue when a device-IRQ consumer exists.
- [ ] Unified wait across `PendingOperation`/`IpcChannel`/`NotificationChannel`/
      `Process` — **deferred to those slices** (their objects don't exist yet).
      `sys_wait` supports `Timer` now; the wait API is generic (embeddable in any
      kobject), so adding a waitable later is just "embed a waiter list + signal
      it".

#### Notifications

Ordered before IPC so IPC's dead-peer path has its `PeerClosed` variant; the
exception-delivery path uses the wait-queue blocking primitive above.

- [x] `NotificationChannel` kernel object per [docs/spec/notification-format.md]
- [x] Bounded queue (default 64 entries) in kernel memory
- [x] `Notification` (flat 64-byte record) with sparse category-based discriminants
- [x] `sys_notif_recv` (syscall 11); `NotificationChannel` is a 2nd `sys_wait` waitable
- [x] Exception notification variants: `SegFault`, `IllegalInsn`, `DivideByZero`
      wired (real producer). `ChildExited` (needs spawn + real exit) and
      `PeerClosed` (needs IPC) defined as discriminants only — no producer yet.
- [x] Exception delivery path — **suspend + supervised resume/terminate** (slice
      ③, was post-mortem): ring-3 fault → notification + **suspend** the faulting
      thread; a supervisor resumes or terminates it. The kernel survives.
- [x] Overflow handling (exception-priority eviction + `NotificationsDropped`)
- [x] Exception **suspend** + `sys_exception_resume` with `Resume`/`Terminate`
      (+ `sys_thread_get_registers`) — **slice ③**: a ring-3 fault now suspends
      the faulting thread (uniform across all user-fault vectors) and a supervisor
      resumes or terminates it. The debugger extras (`ResumeSkip`/`ModifyAndResume`,
      auto-terminate timeout, exception-channel priority chain) stay Phase 2.

#### IPC

- [x] `IpcChannel` kernel object per [docs/spec/ipc-message-format.md] (an
      endpoint **pair**: two `IpcChannel` kobjects with mutual peer pointers)
- [x] Per-channel queue with configurable depth, slot pool allocation
      (per-endpoint receive ring, pre-allocated, default depth 16)
- [x] `sys_channel_create` (syscall 12)
- [x] `sys_channel_send` (syscall 13) — **NoBlock** only; Block / BlockBounded
      deferred to the async-I/O slice (they need a `PendingOperation`)
- [x] `sys_channel_recv` (syscall 14) — `WouldBlock` if empty + `sys_wait`-able
- [x] Handle transfer mechanics during send (**slice ②**) — always move,
      `TRANSFER`-gated, install at recv into the receiver's table
- [x] Dead-peer handling: send/recv `PeerClosed` errors + blocked-recv wakeup.
      The async `PeerClosed` **notification** is **deferred to Phase 2** (multi-
      holder "every holder" delivery wants handle duplication + a holder registry)

#### Final slices: process spawn → handle transfer → threads + exceptions

The original "Other syscalls" step is split into three focused slices (each its
own explore → design → implement → verify cycle). Slice ① delivers most of the
milestone; ② clears the IPC handle-transfer deferral; ③ finishes the milestone's
`sys_exception_resume` clause.

##### Slice ① — Process spawn + lifecycle + `ChildExited` (done)

- [x] `sys_process_spawn` (syscall 15) — allocates the child's initial handles
      in the global table tagged with the child's `owner_pid` (move/duplicate);
      Phase-1 forms: kernel-embedded image selector + register bootstrap ABI
      (filesystem/`MemoryObject` image + stack bootstrap block → Phase 2)
- [x] `sys_process_exit` (16), `sys_thread_exit` (17) — real versions: exit
      status → parent's `ChildExited` notification, replacing the debug
      `sys_process_exit`. (Multi-thread teardown lands with `sys_thread_create`.)
- [x] `ChildExited` producer — delivered to the parent's notification channel at
      exit time (so a `sys_wait`ing parent wakes promptly)
- [x] pid allocation; bootstrap-register entry ABI; multi-user-thread CPU-state
      fixes (per-switch trap/syscall-stack re-arm; `KERNEL_GS_BASE` re-assert)
- [x] `sys_thread_set_affinity` (syscall 18; a no-op until SMP; Phase 3)
- [x] Demo: `userspace/parent` spawns two `userspace/child` processes that talk
      over IPC; the parent reaps both via `ChildExited`

##### Slice ② — IPC handle transfer (done)

- [x] Handle transfer mechanics during `sys_channel_send` (the `count > 0` path:
      **always move**, `TRANSFER`-gated, atomic-or-fail with move-on-commit;
      references pinned "in flight" in the queued message)
- [x] `sys_channel_recv` installs the transferred handles into the receiver's
      table + surfaces their values and the count
- [ ] Async `PeerClosed` **notification** — **deferred to Phase 2** (the dead-peer
      error path ships; the "every holder" delivery wants handle duplication + a
      per-endpoint holder registry, a Phase-2-shaped design)
- [x] Demo: a child transfers a `MemoryObject` to its sibling, which maps it and
      reads back the shared marker

##### Slice ③ — Threads + minimal exception resume/terminate (done)

- [x] `sys_thread_create` (syscall 19; a `Thread` handle, the supervisor
      capability) + multi-thread process-exit teardown — `exit_process` scans the
      run/blocked/suspended queues by `owner_pid` and reaps the siblings (a
      per-process thread list lands in Phase 2; the scan is correct now)
- [x] `sys_thread_get_registers` (syscall 20; reads a suspended thread's saved
      `ExceptionFrame` into the neutral `RegisterValues`)
- [x] Exception **suspend** (uniform across all user-fault vectors, via the
      shared stub epilogue) + `sys_exception_resume` (syscall 21) with
      **`Resume`** / **`Terminate`** dispositions — finishes the milestone's
      "resume or terminate" clause. The heavy debugger extras (`ResumeSkip` /
      `ModifyAndResume`, auto-terminate timeout, exception-channel priority chain)
      stay deferred to Phase 2.
- [x] Demo: `userspace/parent` creates a worker thread that segfaults, receives
      the `SegFault`, inspects its registers, and terminates it — before the
      existing spawn/transfer demo and a final `sys_process_exit`.

**Phase 1 milestone met** — the kernel substrate is complete (see the milestone
below; every clause now ships).

**Punted past Phase 1 (consumer-gated):** FPU `XSAVE` save/restore + TLS
(`sys_thread_set_tls`) — userspace is soft-float, so no thread touches the FPU
even with multiple processes (no consumer until a hard-float userspace exists);
the DPC queue and the `xtask test-qemu` harness (Phase 2+/opportunistic).

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

> **2026-06-11 re-sequencing (stock-take after Phase 1).** The original
> Phase 2 ordering silently assumed several pieces of infrastructure that do
> not yet exist, and had one internal ordering inversion. A dependency audit
> (see the decision log entry of 2026-06-11) found:
>
> - **The "async-I/O slice" is referenced but never defined.** The Phase 1
>   status note and the IPC slice both defer `PendingOperation` + blocking IPC
>   send "to the async-I/O slice," but no slice built it. Every block-device
>   read (AHCI → fs-server → page cache) is an async operation that needs it.
> - **Device IRQs need an IOAPIC, which needs ACPI MADT parsing** — Phase 1
>   shipped LAPIC-only and deferred IOAPIC "to Phase 2" without giving it a
>   slice. PCI ECAM likewise needs the ACPI MCFG table. (This is the small
>   pure-Rust *table-parsing* layer, distinct from the ACPICA/AML work that is
>   correctly deferred to its own trigger — see `why-phased-acpi.md`.)
> - **The DPC/softirq queue** was deferred in Phase 1 "until a device-IRQ
>   consumer exists" — storage drivers are that consumer.
> - **The page cache needs a demand-paging `#PF` handler** (not-present →
>   VMA lookup → fault-in) and the `MappingKind::FileBacked` VMA variant,
>   both still Phase-1 stubs (the current `#PF` handler only consults the
>   exception table).
> - **Entropy was listed both as its own slice and as an item inside the
>   in-kernel-RS slice** (`/dev/entropy`), a forward self-reference.
> - **FAT was justified as "required to boot Limine"** — false; UEFI/Limine
>   read the ESP, not Nitrox. Nothing in the Phase 2 milestone consumes it.
>
> The missing infrastructure is now scheduled explicitly as a **prerequisite
> band** ahead of slice 1, the slices are reordered, and the misleading notes
> are corrected. (These prerequisites are genuine Phase 2 feature work; they
> are distinct from the Phase 1.5 code-quality hardening pass also recorded
> in the decision log on 2026-06-11.)

#### Phase 2 prerequisites (land before the namespace slice)

These were implicit in the original plan; each gates one or more later slices.
Author the two missing architecture docs first — slices 1 and 5 implement
*against* contracts that have not been written.

- [~] **Architecture docs.** `docs/architecture/drivers-and-irps.md` (the IRP /
  completion-routine / `InterruptObject` contract the storage slice implements)
  is **done** (`phase-2/drivers-irps-doc`). `docs/architecture/namespace-and-resource-servers.md`
  (the `ResourceServer`/`OpStatus`/registry contract slice 1 implements) is still
  to be written — it gates slice 1, not the prereq code items, so it lands just
  before slice 1.
- [x] **ACPI table parser** (pure-Rust RSDP → XSDT/RSDT → MADT + MCFG; no AML).
  Enables IOAPIC (MADT) and PCI ECAM (MCFG). No external crate. Gates the
  IOAPIC and storage slices. **Done** (`phase-2/acpi-tables`): behind a new
  arch-neutral `ArchPlatform` trait (`arch/platform.rs`) — the x86 ACPI parser
  (`arch/x86_64/acpi.rs`) exposes only the PCIe ECAM regions neutrally; the
  MADT interrupt-routing facts (IOAPIC/GSI/source-overrides) stay arch-internal
  for the IOAPIC item. See the decision log (2026-06-11).
- [x] **IOAPIC bring-up + external IRQ routing.** The Phase-1 `ArchIrq`
  deferral (LAPIC-only). Without it no device interrupt is deliverable, so
  AHCI cannot signal completion. **Done** (`phase-2/ioapic`): a new
  arch-neutral `ArchIrqRouter` trait (`arch::IrqRouter`, x86 impl `X86IoApic`,
  distinct from `ArchIrq` the per-CPU local controller) + IDT device-IRQ vectors
  (0x30..) with a handler registry; brings up the IOAPIC from the cached MADT
  facts, masks the 8259s, and a PIT self-test proves GSI→IOAPIC→vector→ISR→EOI
  end-to-end. See the decision log (2026-06-11). (The `IrqSpinLock` audit for
  new IRQ-reachable locks lands with the DPC item / real device handlers.)
- [x] **DPC / softirq queue** (the Phase-1 "DPC integration for wakeup"
  deferral). Device IRQ handlers defer their real work here (no allocation /
  unbounded work in IRQ context). **Done** (`phase-2/dpc`): `kernel/src/dpc.rs`
  — an inline `Dpc { handler, ctx, queued }` + a pre-reserved global queue
  (single-CPU stand-in, per-CPU at SMP); `enqueue` from an ISR, `run_pending`
  drained at the interrupt-dispatch tail (a leaf `IrqSpinLock`). The timer's own
  deadline-firing stays inline (timekeeping work, not migrated — a correction to
  `drivers-and-irps.md`); the queue serves device ISRs. Proven by the PIT
  self-test driving a DPC end-to-end. See the decision log (2026-06-12).
- [ ] **Demand-paging `#PF` handler** (not-present fault → active-AS VMA
  lookup → fault-in) **+ `MappingKind::FileBacked`** VMA variant. Completes
  the Phase-1 `#PF` stub and the `Anonymous`-only `MappingKind`. Unblocks
  both lazy anonymous/`MemoryObject` paging and the page cache. Also enables
  retiring the eager per-page allocation in `AddressSpace::map_vma`.
- [ ] **`PendingOperation` kernel object + `sys_wait` I/O-completion
  integration** (the long-promised "async-I/O slice"), plus the `Block` /
  `BlockBounded` IPC send modes that were deferred to it, and the `IoRing`
  transport if the rsproto wire format needs it. This is the blocking
  primitive every device/fs request depends on. Gates the storage, fs-server,
  and page-cache slices.
- [ ] **DMA-capable allocation** (page-multiple alignment / a `dma_alloc`
  path; the `align > SLAB_SIZE` deferral). AHCI command lists / PRDTs need
  physically-contiguous aligned buffers. Folded into the storage slice if not
  landed earlier.

#### 1. Namespace and resource server foundation

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

#### 2. Entropy

Moved ahead of the in-kernel resource servers: the `/dev/entropy` server in
the next slice depends on this subsystem (the original plan listed it in both
places — a forward self-reference).

- [ ] Hardware RNG access (RDSEED preferred, RDRAND fallback)
- [ ] Software entropy mixing (TSC jitter at interrupt dispatch)
- [ ] ChaCha20 CSPRNG with periodic reseed
- [ ] `EntropyObject` handle, blocks until pool is seeded

#### 3. In-kernel resource servers

- [ ] Initramfs resource server (parses Limine-loaded CPIO newc blob)
- [ ] Device resource server stub (`/dev`)
- [ ] Process resource server stub (`/proc`)
- [ ] Kernel log resource server (`/dev/log`)
- [ ] Entropy resource server (`/dev/entropy`) — consumes the entropy subsystem (slice 2)
- [ ] Framebuffer resource server (`/dev/framebuffer`) — for pre-compositor era
- [ ] Synthetic `/proc/self/*` resources

#### 4. Init (PID 1) — bootstrapping form

This slice lands a *bootstrapping* init: it starts (handle-set reception, TOML
parsing, reaping loop) on top of slices 1 and 3. Its full critical-path mount
loop is not milestone-complete until the storage + fs-server slices (5–8)
land; see the milestone note.

- [ ] `userspace/init/` crate, `libkern + alloc` only
- [ ] Initial handle set reception from kernel
- [ ] Minimal TOML parser (just enough for init.toml schema)
- [ ] init.toml parsing per [docs/spec/init-toml-schema.md]
- [ ] Critical-path mount processing loop (topological sort by mount_point depth)
- [ ] Reaping loop for `ChildExited` notifications
- [ ] `sys_release_initramfs` syscall and init's call to it once boot is stable

#### 5. Storage drivers

Depends on the prerequisite band: ACPI MCFG (ECAM), IOAPIC (device IRQs), the
DPC queue (completion handling), `PendingOperation` (blocking reads), and DMA
allocation (command lists / PRDTs).

- [ ] PCI/PCIe enumeration via ECAM (MCFG-based on x86_64)
- [ ] DeviceNode kernel objects for discovered devices
- [ ] AHCI driver (start here; simpler than NVMe)
- [ ] IRP framework per [docs/architecture/drivers-and-irps.md]
- [ ] InterruptObject kernel object for hardware IRQs
- [ ] DMA buffer allocation (if not landed in the prerequisite band)
- [ ] Block device resource server registration

#### 6. Partition handling

- [ ] GPT driver (Tier 1)
- [ ] Partition DeviceNode registration
- [ ] `/dev/disk/by-partuuid/*` and `/dev/disk/by-partlabel/*` namespace entries

#### 7. Filesystem in userspace

Decide the transport explicitly: the rsproto client API is async-shaped, so
either `PendingOperation`-backed requests or at least one blocking IPC
direction (both from the prerequisite band) are needed for real request/reply
under backpressure; `NoBlock` send + `sys_wait`-on-recv is the fallback.

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

#### 8. Page cache integration with fs-server

Depends on the demand-paging `#PF` handler + `MappingKind::FileBacked` from the
prerequisite band — the fault-in path is what makes "reads files" real.

- [ ] Page cache for file-backed memory in kernel
- [ ] `sys_memory_map` on file handles asks fs-server for extents
- [ ] Kernel reads blocks into page cache pages, maps into client address space
- [ ] Writeback (deferred to Phase 3 along with fs-server-ext4 RW)

#### 9. Emergency shell

Floats after init (slice 4); its read-kernel-log feature depends on `/dev/log`
(slice 3).

- [ ] `userspace/eshell/` crate
- [ ] Minimal command interface over serial console
- [ ] Inspect mounts, list block devices, read kernel log, edit initramfs files, reboot
- [ ] Init invokes eshell on critical-path failure

#### 10. FAT for completeness (RO is fine for now)

Kept last (or a candidate to defer to Phase 3): **no Phase 2 milestone clause
consumes `fs-server-fat`.** The ESP's FAT32 is read by UEFI firmware and
Limine, *not* by Nitrox — booting never requires Nitrox to read its own ESP.
This server exists for parity/completeness, not for boot.

- [ ] `userspace/fs-server-fat/` crate (FAT32/FAT16/FAT12 read-only)
- [ ] Needed only for in-OS access to FAT volumes (e.g. updating the ESP from
  within the OS), not for booting

### Milestone

`xtask qemu` boots to a system that:
1. Boots Limine from the FAT32 ESP
2. Kernel comes up, initializes subsystems, enumerates PCI
3. Init starts from the initramfs
4. Init reads `init.toml`, spawns fs-server-ext4 for the ext4 root partition, waits for Ready, binds the endpoint at `/`
5. Init reads `/system/current-generation` and logs the contents to the kernel log
6. Init enters its reaping loop

Disk image is built by `xtask build-disk` with a real ext4 partition containing test data.

The milestone is **unchanged** by the 2026-06-11 re-sequencing — only the
slice order and the explicit prerequisite band changed. Note that init
(slice 4) is only *milestone-complete* once the storage/fs-server/page-cache
slices land (it can spawn fs-server-ext4, wait for Ready, and bind `/`).

### Notes / deviations

- **2026-06-11 — Phase 2 re-sequencing.** Added the explicit prerequisite
  band (architecture docs, ACPI table parser, IOAPIC, DPC queue, demand-paging
  `#PF` + `FileBacked`, `PendingOperation`/async-I/O, DMA allocation); moved
  Entropy ahead of the in-kernel resource servers; corrected the FAT
  "required to boot" justification; clarified that slice-4 init is the
  bootstrapping form. Rationale and the full dependency analysis are in the
  decision log entry of 2026-06-11. No milestone change.

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
