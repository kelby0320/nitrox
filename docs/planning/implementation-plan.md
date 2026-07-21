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
- **Phase 2 (Filesystem and namespace):** **complete (2026-06-26)** — the
  prerequisite band + slices 1–9 shipped; boots Limine → kernel/PCI → init → ext4
  mount (userspace fs-server) → demand-paged reads → a live `eshell>`. Slice 10 (FAT,
  read-only) deferred to Phase 3.
- **Phase 3 (Service ecosystem):** **in progress, nearly done** — the kernel-first band
  (slices 0–3b) and the userspace-runtime band (slices 4–7: libheap, libos core, SysCaps,
  libos authority surface) are **done**, bar the cross-CPU deschedule IPI (gated on the
  first multi-threaded user process) and **`libstream`** (the one open runtime lib). The
  service-ecosystem *machinery* is complete (service-mgr + supervision, RS startup protocol,
  path-based ELF spawn, RW ext4 fs, the auth/session login chain, logging + profile servers).
  **Definition of Done** (see §Phase 3 → Definition of Done, and the decision log 2026-07-20)
  = close the two open milestone clauses: **libstream + a typed-log demo** and a **`/proc`
  scheduler-stats surface**. The remaining backlog services are **consumer-driven and defer
  to Phase 4**. std is now a **serious compatibility target** (not deferred indefinitely) —
  see the 2026-07-20 std stance.
- **Phase 4+ (a usable windowed desktop → browser, networking, sysadmin):** not started.
  North star = a compositor on the boot framebuffer + a shared GUI toolkit + a GUI
  terminal/file-browser/editor, with a typed-shell + coreutils CLI as the first stepping
  stone. See §Phase 4+ and the decision log (2026-07-20).

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

## Phase 2: Filesystem and namespace — **COMPLETE (2026-06-26)**

**Goal:** the namespace subsystem, the resource server protocol, the first real filesystem. Init runs, processes its bootstrap manifest, mounts ext4, reads files.

> **Status: complete.** The prerequisite band + slices 1–9 are all done and the
> milestone below is met and QEMU-proven (Limine → kernel/PCI → init from
> initramfs → spawn fs-server-ext4 → mount ext4 `/` → read `/system/current-generation`
> → reaping loop, now also dropping to `eshell` on a critical-path failure).
> **Slice 10 (FAT, read-only) is deferred to Phase 3** — parity-only, not on the
> boot path. The one quality issue surfaced at close (single-page demand-fault
> latency, ~325 ms/page) is a documented Phase-3 optimization, mitigated for now
> by trimming the `large.bin` fixture 64 → 8 pages. See the decision log
> (2026-06-26, Phase 2 close).

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
>   VMA lookup → fault-in) and the `MappingKind::FileBacked` VMA variant —
>   **both now landed** (`phase-2/demand-paging`): `AddressSpace::fault_in`
>   resolves not-present user faults and the `FileBacked` variant + dispatch
>   arms await the page cache's producer.
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

- [x] **Architecture docs.** `docs/architecture/drivers-and-irps.md` (the IRP /
  completion-routine / `InterruptObject` contract the storage slice implements)
  is **done** (`phase-2/drivers-irps-doc`). `docs/architecture/namespace-and-resource-servers.md`
  (the namespace data model + resolution + async-lookup contract + the
  resource-server model — `KernelServer`/`UserspaceServer`/`OpStatus`/registry) is **done**
  (`phase-2/namespace-design`) — it gates slice 1.
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
- [x] **Demand-paging `#PF` handler** (not-present fault → active-AS VMA
  lookup → fault-in) **+ `MappingKind::FileBacked`** VMA variant. **Done**
  (`phase-2/demand-paging`): `pf_dispatch` offers a not-present ring-3 fault to
  `AddressSpace::fault_in` (VMA lookup → access check → alloc-zero-map-flush)
  before the fatal SegFault path; `map_vma_lazy` reserves anonymous ranges
  unbacked and the ELF loader reserves user stacks this way (PT_LOAD stays
  eager — file bytes). `MappingKind::FileBacked` + its dispatch arms exist for
  the page cache (no producer yet). Proven by a boot smoke test + the userspace
  demo running on a demand-faulted stack. Unblocks lazy `MemoryObject` paging
  (the `MAX_SIZE` cap — needs a sparse object frame table + accounting) and the
  page cache. See the decision log (2026-06-12).
- [x] **`PendingOperation` kernel object + `sys_wait` I/O-completion
  integration** (the long-promised "async-I/O slice"). **Done**
  (`phase-2/pending-operation`): a one-shot waitable `PendingOperation`
  (`object/pending_op.rs`) wired into the generic wait/wake machinery (3 sched
  dispatch arms + `signal_pending_op`); `sys_wait` reports its completion status
  via `IoResult.status`. First consumer: the IPC **`Block`** send mode — a full
  ring holds the message in a per-endpoint pending-sender queue and returns a PO
  that completes (the message delivered) when the peer next receives; close
  completes held senders `PeerClosed`. Proven by host tests + a parent demo
  (`blocking send completed via PendingOperation`). **`BlockBounded`** (the
  deadline-bounded variant) is carved out to its own follow-up (it needs the
  deadline-heap kind extension + a `sys_channel_send` deadline arg) — still
  `Unsupported`. `IoRing` lands with the rsproto transport when needed. Gates the
  storage, fs-server, and page-cache slices. See the decision log (2026-06-12).
- [x] **IPC `BlockBounded` send mode** (follow-up to the above). **Done**
  (`phase-2/block-bounded`): the deadline-heap `Entry` grew a 3-way kind
  (`Thread`/`Timer`/`PendingSend`) + channel back-pointer; a timer-tick arm cancels
  a held send whose delivery deadline elapsed (PO completes `TimedOut`); a 6th
  `sys_channel_send` arg carries the deadline. Timed-out sends are reclaimed
  outside `SCHED` via **reclaim-on-recv** (swept on the next recv / at close).
  Proven by host tests + a parent demo (`blocking send timed out via
  PendingOperation`). See the decision log (2026-06-12).
- [x] **DMA-capable allocation** (page-multiple alignment / a `dma_alloc`
  path; the `align > SLAB_SIZE` deferral). **Done** (`phase-2/dma-alloc`):
  `mm::dma::DmaBuffer` — an RAII, zeroed, physically-contiguous, page-aligned
  block from the buddy allocator (order-`k` blocks are `2^k × PAGE_SIZE`-aligned)
  exposing both a CPU/HHDM pointer and its `phys()` address, for AHCI command
  lists / FIS / PRDTs. DMA **zones** stay deferred (no address-constrained device
  on the no-legacy baseline). Proven by host tests + a boot smoke test. See the
  decision log (2026-06-12).

> **The Phase 2 prerequisite band is complete.** All seven prerequisites —
> drivers-and-IRPs doc, ACPI tables, IOAPIC, DPC queue, demand paging,
> `PendingOperation`/async-I/O + IPC `Block`/`BlockBounded`, and DMA-capable
> allocation — have landed. Phase 2 proper (the storage slice → fs-server → page
> cache) can begin.

#### 1. Namespace foundation (the per-process name-resolution substrate)

Design: [`docs/architecture/namespace-and-resource-servers.md`]. Broken into a
docs-first design pass (**done**) + three code parts, each its own PR. The
`UserspaceServer` trait / `OpStatus` / registry / IPC-forwarded lookup are *designed*
here but **implemented with slice 3** (resource servers) — there are no servers to
route to until then. Lookup is a `PendingOperation` from the start (a real lookup
forwards over IPC → async); slice 1 binds **direct handles** and returns a
pre-signalled PO carrying the resolved handle via `IoResult.result`.

- [x] **Part A — design doc** (`phase-2/namespace-design`): the model, path grammar,
  longest-prefix resolution, binding kinds, async-lookup contract, capability model,
  cache, kernel/userspace split, slice-1-vs-slice-3 scope. Spec: `sys_ns_*` numbers
  22–25 reserved + `IoResult.result` word noted.
- [x] **Part B** (`phase-2/namespace-object`, PR #41) — `Namespace` kernel object +
  binding store + longest-prefix resolution engine (host-tested; no syscalls).
- [x] **Part C** (`phase-2/namespace-syscalls`) — `IoResult.result` (16→24 B) +
  `PendingOperation` result payload; the four `sys_ns_*` syscalls (lookup →
  pre-signalled PO carrying the resolved handle; resolution failures via the PO's
  `NotFound` status, arg/permission/alloc failures synchronous; bind gated by the
  `BIND` handle right, `BIND_NAMESPACE` syscap deferred to the syscap model);
  `Process::namespace` field + boot-time root namespace for pid 1 (handle in `rsi`);
  QEMU `ns_demo` create→bind→lookup→wait→use→unbind.
- [x] **Part D** (`phase-2/namespace-inherit-cache`) — per-`Namespace` lookup cache
  (path→binding-index, flush-on-mutation); spawn-time namespace inheritance via a
  4-register bootstrap ABI (`rdi`=notif, `rsi`=namespace, `rdx`=installed[0],
  `rcx`=arg0) + a `SpawnArgs.namespace` field (`0`=inherit, else a constructed
  restricted namespace; child gets a LOOKUP-only handle); boot banner → Phase 2.
  **Namespace foundation (slice 1) complete.**
- *(slice 3)* `UserspaceServer` trait, `OpStatus`, `UserspaceServerRegistry`,
  IPC-forwarded lookup + cross-context handle install.

#### 2. Entropy

Moved ahead of the in-kernel resource servers: the `/dev/entropy` server in
the next slice depends on this subsystem (the original plan listed it in both
places — a forward self-reference). Design:
[`docs/architecture/entropy.md`]. Broken into a docs-first design pass + three
code parts, each its own PR (mirroring the namespace slice). The read interface is
async by contract (a `PendingOperation` when unseeded) but the pool seeds at boot,
before userspace, so reads are synchronous in practice.

- [x] **Part A — design doc** (`phase-2/entropy-design`): sources (RDSEED/RDRAND +
  TSC jitter), the pool + seeded gate, ChaCha20 + fast-key-erasure + reseed policy,
  boot integration, the `EntropyObject` read contract, lock discipline, kernel/
  userspace + slice-2/slice-3 scope. Spec: `sys_entropy_create = 26` /
  `sys_entropy_read = 27` reserved.
- [x] **Part B** (`phase-2/entropy-csprng-hwrng`, PR #45) — hand-rolled ChaCha20
  CSPRNG (RFC 8439 vectors) with fast key erasure + arch HW-RNG access
  (`arch::Entropy`: RDSEED preferred, RDRAND fallback; CPUID-detected). Host-tested.
- [x] **Part C** (`phase-2/entropy-pool-seeding`) — entropy pool + boot seeding +
  TSC-jitter mixing at interrupt dispatch + periodic/byte-threshold reseed + the
  256-bit seeded gate; the handle-table free-list PRNG now seeds from the CSPRNG
  (`PHASE1_SEED` removed). One `IrqSpinLock<EntropyState>` leaf. QEMU opts in
  `+rdrand,+rdseed`; boot shows `seeded=true`.
- [x] **Part D** (`phase-2/entropy-object-syscalls`) — `EntropyObject` kernel object
  + `sys_entropy_create` / `sys_entropy_read` (returns `0` on synchronous fill when
  seeded; a `PendingOperation` when not, with the seed-latch waking PO waiters from
  the timer tick) + QEMU demo. **Entropy subsystem (slice 2) complete.**

#### 3. In-kernel resource servers

**Scope (decided 2026-06-22 — see decision log).** Slice 3 builds the **in-kernel**
resource-server framework and the servers with an immediate consumer/demo. In-kernel
servers dispatch by **direct kernel function call** (no IPC); the kernel binds them
into pid 1's root namespace at boot, so the whole slice is demoable via the existing
parent process without init.

Deliberately deferred (build-when-consumed, to avoid large unexercised machinery):

- **Userspace-RS path** — IPC-forwarded lookup, cross-context handle install,
  `librsproto`, and the Ready handshake → **slice 7** (the fs-server is the first
  userspace-RS consumer).
- **`/initramfs` + CPIO + `sys_release_initramfs`** → **slice 4** (Init, its only
  consumer).
- **`/dev/framebuffer`** → deferred (needs userspace framebuffer mapping, not built).
- **The filtered/full process server** (`/proc/<pid>`, enumeration) → a later slice:
  it needs a global process registry *and* is the ambient-authority-sensitive
  surface (see the `/proc/self` note below). Slice 3 ships **only `/proc/self`**.

Broken into a prerequisites pass + docs-first + two code parts (mirrors slices 1/2):

**Part 0 — fault diagnostics prerequisite (done).** Motivated by the slice-2 entropy
demo's "hang"; landing it first makes all later slice-3/Init debugging tractable.
Measuring before building (see `decision-log` 2026-06-22, Part 0) corrected two of
the planned premises:

- [x] **Surface unhandled ring-3 faults** (`phase-2/slice3-userspace-rt-fault-diag`).
  A fault that leaves **no runnable thread** to service it (notably an init/pid-1
  crash) suspended silently — a hang. `sched::suspend_with_fault` now detects the
  *scheduler-stranded* case (the dequeue falls through to idle, so no thread remains
  to receive the notification + `sys_exception_resume` it) and emits a last-ditch
  diagnostic (`pid/tid/kind/addr`) via the emergency serial writer. Fires only for
  genuinely-stranded faults — a serviced fault (the worker demo) wakes its supervisor
  before the dequeue and stays silent. (The naïve "no notification channel" condition
  was rejected: pid 1 *has* a channel — it services its own faults — so that check
  never fires for it.)
- ~~Freestanding-userspace mem intrinsics~~ — **dropped (not needed).** Measurement
  showed `compiler_builtins` already supplies `memcpy`/`memset`/`memcmp`/`memmove`
  on-demand for `x86_64-unknown-none` (the kernel defines all four; the parent links
  `memcmp` with zero undefined symbols). The original `a != b` "hang" was a separate
  inlined-`[u8; N]`-equality codegen quirk (infinite loop, no `memcmp` call), not an
  intrinsics gap — documented as a known issue; userspace keeps the manual-loop idiom.

**Part A — design doc (done, `phase-2/slice3-rs-framework-design`).** Formalized the
in-kernel RS framework into `docs/architecture/namespace-and-resource-servers.md`
(extended in place — it's the living RS doc): the kernel-server dispatch model
(`lookup(suffix, rights) -> OpStatus::{Completed(handle) | Rejected(err)}`; `Pending`
reserved for slice 7), the `BindingTarget` enum (`DirectHandle` + `KernelServer`;
`UserspaceServer`/IPC + `SubNamespace`/`Rewrite` deferred), how lookup dispatches
**synchronously** and reuses the slice-1 pre-signalled-PO delivery (`IoResult.result`),
boot-time binding into pid 1's root namespace, the per-server content model (a lookup
returns a handle to a kernel object), and the `/proc/self` authority model below.

**Part B — the framework + `/dev/entropy` (done, `phase-2/slice3-kernel-server-framework`).**
`object/kernel_server.rs` (`KernelServerId`, `OpStatus::{Completed|Rejected}`, the
`dispatch` registry); `BindingTarget`/`ResolvedTarget` in `namespace.rs` (replacing the
bare `ObjectRef` target; `bind_kernel_server`; `unbind`/`resolve` updated, drop
discipline preserved); `sys_ns_lookup` calls a server synchronously → installs the
rights-attenuated handle → pre-signals the PO. The **whole `/dev/entropy` server** was
folded in as the demonstrator (entropy is complete; it closes the loop that motivated
landing entropy first) — bound into pid 1's root namespace at boot (`main.rs`),
inherited by children, exercised by a `parent` QEMU demo (resolve → read). Host-tested
(`kernel_server` dispatch + `namespace` bind/resolve/unbind). No ABI-hash impact.

**Part C — the remaining servers + demo.**

- [x] `/dev/entropy` — lookup returns an `EntropyObject` (reuses slice 2;
  `sys_entropy_read` on the resolved handle). **Landed in Part B** as the framework
  demonstrator.
- [x] `/proc/self/*` — **self-reference only**: `process`/`thread`/`namespace` resolve
  to the **caller's own** objects (from the calling syscall context, no pid parameter).
  **Done** (`phase-2/slice3-proc-self`): per-leaf `KernelServer` bindings with
  type-correct rights (`process`/`thread` → `SIGNAL|TERMINATE`+generic; `namespace` →
  `LOOKUP`+generic, no `BIND`); `sched::current_thread()` added; bound into pid 1's root
  ns at boot; QEMU demo stats process/thread + resolves `/dev/entropy` through the
  returned namespace handle. Registry-free; no cross-process access.
- [ ] *(deferred)* `/proc/self/status` — numeric pid/tid snapshot. Needs a
  `MemoryObject` synthesis primitive (or extended handle introspection); the scalar-via-
  `IoResult.result` shortcut was rejected. See `deferred-decisions.md`.
- [ ] *(deferred)* `/dev` directory stub — `DeviceNode` has no struct, no enumeration
  syscall, no consumer; deferred to a device manager (slice 7) / enumeration. See
  `deferred-decisions.md`.
- [ ] *(deferred)* `/dev/log` — a readable kernel-log snapshot needs a log ring buffer
  (new infra) + the same synthesis primitive.
- [x] QEMU demo: the parent looks these up and uses the results.

> **`/proc/self` authority (no ambient authority).** Reachability is by **namespace
> construction** — `/proc/self` resolves only if a supervisor bound it (a sandbox may
> omit it; it is *not* a kernel-forced universal). What it returns is strictly the
> **caller's own** resources, derived from the running context — there is **no pid
> parameter to forge**, so it grants nothing about other processes (and returned
> handles are still owner-pid-checked on use). Cross-process introspection
> (`/proc/<pid>`, enumeration) is a **separate, narrowly-bound** capability
> (init/admin namespaces) with its own registry — deferred, not built here. See
> `os-design-v5.1.md` §"Synthetic /proc/self" + the namespace-composition examples
> (standard user → filtered `/proc`; admin → full `/proc`; sandbox → none).

#### 4. Init (PID 1) — bootstrapping form

This slice lands a *bootstrapping* init: it starts (handle-set reception, TOML
parsing, reaping loop) on top of slices 1 and 3. Its full critical-path mount
loop is not milestone-complete until the storage + fs-server slices (5–8)
land; see the milestone note.

The **initramfs substrate** lives here (moved from slice 3, 2026-06-22) — its only
consumer is init reading `init.toml` + spawnable images, so it lands where it's used.
It reuses the slice-3 in-kernel RS framework: `/initramfs` is just another in-kernel
server, bound at boot.

Decided as the userspace library scope for this slice (2026-06-23): pull forward only a
real **`libkern`** (init's mandated foundation); `libos`/`librt`/`libstream` stay
Phase 3, `librsproto` slice 7. Path-based spawn + relocating the demos onto the
initramfs defer to slice 7 (driven by fs-servers). Done as ordered PR parts:

- [x] **Part 1 — real `libkern` + migrate the demos** (`phase-2/slice4-libkern`, PR #53):
  the canonical userspace ABI mirror (`syscall`/`error`/`handle`/`abi`/`debug`);
  parent/child/hello migrated off ~485 lines of triplication; host tests in
  `cargo xtask test`.
- [x] **Part 2 — initramfs substrate** (`phase-2/slice4-initramfs`): Limine module
  request (`kernel/src/limine.rs`) + `boot/limine.conf` module + xtask CPIO-newc packer;
  in-kernel CPIO-newc parser (`kernel/src/initramfs.rs`, host-tested); the `/initramfs`
  `KernelServer` (first subtree server) returning a read-only `MemoryObject` copy via the
  new `MemoryObject::try_new_filled`; bound into pid 1's root namespace at boot. Verified
  by the parent demo resolving+mapping `/initramfs/etc/init.toml`.
- [x] **Part 3 — init crate skeleton** (`phase-2/slice4-init-skeleton`):
  `userspace/init` as a bare-target `#![no_std]`+`alloc` lib+bin (libkern only); static-
  arena bump `#[global_allocator]` (host-tested); `_start` handle-set reception + alloc
  proof + clean exit; spawnable via `ImageId::Init` and reaped by the parent demo.
  Surfaced + fixed two userspace-runtime bugs init's first `alloc` use hit: a mis-placed
  `compiler_builtins` `memcpy` (now strong `libkern::mem` intrinsics) and a `/DISCARD`-ed
  `.got` (now kept in all four `user.ld`). See the decision log (2026-06-23).
- [x] **Part 4 — minimal TOML parser + init.toml manifest** (`phase-2/slice4-toml`):
  `init::toml_lite` (the `[[mount]]` / `[mount.options]` / scalar subset) +
  `init::manifest` (`MountSpec` validation + shallowest-first topo-sort), per
  [docs/spec/init-toml-schema.md]. 15 host tests; an on-target smoke test parses an
  embedded sample. The mount-processing loop stays Part 5 / slice 7.
- [x] **Part 5 — init becomes PID 1 + reaping loop + bootstrap skeleton**
  (`phase-2/slice4-init-pid1`): kernel boots init (`ImageId::Init`); init reads+parses
  the real `/initramfs/etc/init.toml`, logs the topo-sorted mount plan, spawns `parent`
  (`ImageId::Parent`) → `child`, and runs the reaping loop. Process tree is now
  init (1) → parent (2) → child (3/4). The mount loop stops before the Ready handshake
  (slice 7); `parent`'s `ns_demo` rebased onto a fresh namespace (its inherited root is
  LOOKUP-only under init). Required + depends on the GS-base `#DF` fix (PR #57).
- [ ] ~~`sys_release_initramfs`~~ — **deferred** to the general resource-server
  lifecycle work (load/unload for kernel + userspace servers); the blob stays mapped
  through bootstrapping. See `deferred-decisions.md`.

#### 5. Storage drivers — **complete**

Depends on the prerequisite band (all complete): ACPI MCFG (ECAM), IOAPIC
(device IRQs), the DPC queue (completion handling), `PendingOperation` (async
reads), DMA allocation (`mm::dma::DmaBuffer` — command lists / PRDTs), and the
uncached (`PageFlags::NO_CACHE`) mapping path for BAR access. Staged as ordered
PR parts (all merged); the Part 0 design decisions are in the decision log
(2026-06-23). End-to-end result: a userspace process resolves `/dev/blk/0` and
reads disk sectors via `sys_io_submit` against the real AHCI controller.

- [x] **Part 0 — specs & decisions** (docs only): the storage-slice ABI and
  object contracts settled before the ABI hash bakes them in.
  [`io-operation.md`](../spec/io-operation.md) (`IoOp`/`IoOpcode`),
  [`irp-layout.md`](../spec/irp-layout.md) (`Irp` + sub-types),
  [`device-node.md`](../spec/device-node.md) (the `DeviceNode` object, resource
  descriptor, `/dev/blk` naming + registry); `syscall-abi.md` /
  `abi-version-hash.md` / `deferred-decisions.md` updated. Key calls: block I/O
  via the generic `sys_io_submit` (28) on a `DeviceNode` handle (no new
  `KObjectType`); one `KernelServerId::BlockDevice` + a kernel registry for the
  dynamic disks; `InterruptObject` built this slice; in-kernel MMIO (not
  `sys_device_map_mmio`).
- [x] **Part 1 — PCI(e) enumeration + `DeviceNode`** (`phase-2/slice5-pci-enum`).
  ECAM walk over `arch::platform::pcie_ecam_regions()` via a single repointed
  uncached scan window (`mm::kvmap::remap_mmio_page`); decode identity/class,
  size BARs (32/64-bit + I/O), read the interrupt line/pin; `DeviceNode` kernel
  object (`object/device_node.rs` + the `dispatch_destroy`/type-rights arms);
  boot-time `device::init()` enumerates into a global table and logs each
  function. Host-tested against a synthetic config space (BAR sizing incl.
  64-bit). No driver claims a node yet. QEMU: discovers the ICH9 AHCI controller
  (`8086:2922` class `01.06.01`) + its ABAR (BAR5) and 5 other functions; boot
  proceeds to init→parent→child cleanly. (Per [io-operation], [irp-layout],
  [device-node].)
- [x] **Part 2 — IRP framework + `InterruptObject` + the I/O core, on a ramdisk**
  (`phase-2/slice5-irp-iocore`). `Irp` + sub-types (`io/irp.rs`, offsets pinned by
  asserts); `InterruptObject` waitable (`object/interrupt_object.rs`, a latching
  edge-counter; 3 sched dispatch arms + `signal_interrupt` from a DPC + consume at
  `sys_wait`); the block I/O core (`io/block.rs`: a `BlockBackend` fn-pointer on
  `DeviceNode` + `dispatch_block_irp` + the `IrpBox` owning wrapper + the
  completion DPC); `sys_io_submit`(28)/`sys_io_cancel`(29, `Unsupported`);
  `IoOp`/`IoOpcode` in both libkerns. Proven by a boot self-test (`io::self_test`)
  on a RAM-backed device (`io/ramdisk.rs`): read 8 KiB → DPC → PO completes
  (status 0, result = bytes) → buffer content verified; and a DPC signals an
  `InterruptObject` → latch → consume. Independent of AHCI register/DMA work.
- [x] **AHCI driver (Part 3)** (`phase-2/slice5-ahci`). Tier 1 driver
  (`drivers/ahci.rs`): `mm::kvmap::map_mmio` of the ABAR (uncached), HBA/port
  bring-up, polled `IDENTIFY DEVICE`, command list / FIS / command-table+PRDT in
  `DmaBuffer`, `READ DMA EXT` issued against the IRP's buffer fragments (the
  controller DMAs straight into the client's `MemoryObject` frames); real IRQ via
  a neutral `arch::install_pci_irq` free function (GSI from the PCI interrupt-line register →
  ISR → IRP completion DPC → PO; the ISR also signals the controller
  `InterruptObject`). `drivers::probe` matches class-`01.06.01` and publishes the
  disk as a block `DeviceNode`. Proven against the **existing AHCI boot disk** (no
  new disk needed): `drivers::self_test` reads sector 0 and verifies the `0x55AA`
  boot signature, mirroring the PIT self-test's brief interrupt window (with a
  polled fallback). QEMU: HBA up, port 0 disk (64 MiB), `read self-test OK …
  via IRQ`, 4/4 clean boots. Phase 2 scope: single controller/single SATA disk,
  one outstanding command; multi-port/NCQ/MSI and ACPI `_PRT` routing deferred.
  The dedicated `xtask build-disk` + ext4 test disk move to Part 4/7 (fs-server).
  `KError::IoError` (-40) added (both libkerns) for device/medium errors.
- [x] **Block device resource server registration (Part 4)**
  (`phase-2/slice5-block-server`). `KernelServerId::BlockDevice` + the
  `block_device_server` (parses the suffix as a decimal index → resolves the
  n-th block-class `DeviceNode` via `device::find_block_device`, the device-table
  registry). The supervisor binds `/dev/blk` (read-only) into init's root
  namespace at boot, **unconditionally** (the registry carries liveness). Disks
  resolve at **`/dev/blk/0`** (component-boundary matching — not `/dev/blk0`). The
  `parent` userspace demo resolves `/dev/blk/0`, `sys_io_submit`s a 512-byte read,
  `sys_wait`s, and verifies the `0x55AA` boot signature — the full userspace block
  path the kernel self-tests stood in for. QEMU: `parent: /dev/blk/0 read OK
  (sector 0 boot sig 0x55AA)`, 4/4 clean boots. (The dedicated `xtask build-disk`
  + ext4 disk arrive with the fs-server, slice 7.)

#### 6. Partition handling — **complete** (`phase-2/slice6-gpt`)

The first **two-layer block IRP stack**: a partition `DeviceNode` rebases a
partition-relative offset to disk-absolute and forwards to the disk (realised by
`BlockBackend` delegation — `io::block::Partition`/`partition_rebase`, not formal
`stack_index` descent; the latter stays designed-ahead for filter drivers).

- [x] **GPT driver (Tier 1)** (`drivers/gpt.rs`): parses LBA 1 (`EFI PART` +
  bounds; CRC deferred) and the entry array, reading the disk synchronously at
  boot via the new `io::block::read_blocking` (a polled read using the new
  `BlockBackend::poll`, since interrupts are masked at probe time).
- [x] **Partition DeviceNode registration**: each used entry becomes a block
  `DeviceNode` over an `io::block::Partition` window, registered in the device
  table — so it also resolves at `/dev/blk/<n>` (the ESP at `/dev/blk/1`).
- [x] **`/dev/disk/by-partuuid/*` + `/dev/disk/by-partlabel/*`**: stable
  direct-handle bindings created at boot (`gpt::bind_partition_names`); the GUID
  is formatted GPT mixed-endian, the label decoded from UTF-16. Read-only.

Proven on the existing GPT boot disk (no new disk needed): QEMU logs `gpt:
partition 0 lba 2048..131038`; the `parent` demo reads sector 0 of the disk
(`/dev/blk/0`), the partition (`/dev/blk/1`), and the partition under
`/dev/disk/by-partlabel/NITROX_ESP` — all verifying the `0x55AA` boot signature.
`partition_rebase` is unit-tested (partition LBA 0 → disk LBA 2048 + bounds).

#### 7. Filesystem in userspace — **the first userspace resource server**

The Phase-2 init milestone: a userspace `fs-server-ext4` reads a read-only ext4
root over the block device and serves it over IPC, reached **transparently
through the namespace**; init mounts it at `/` via the Ready handshake and reads
`/system/current-generation`. **Read model:** a forwarded `sys_ns_lookup` of a
file returns a read-only `MemoryObject` of its content (reuses the `/initramfs`
server pattern, so init's existing lookup→map→read code works verbatim; 64 KiB
cap — slice 8's page cache makes it lazy). The **userspace-RS kernel path** lands
here (moved from slice 3, 2026-06-22): `BindingTarget::UserspaceServer` +
IPC-forwarded lookup + cross-context handle install (the `Pending` `OpStatus`
path the slice-3 framework reserved). Staged as ordered PR parts; design +
decisions in the decision log (2026-06-25). ext4 scope is **minimal** (single
regular file via extents); `librsproto` is **codec + server-side only** (sync
`RsClient` deferred to eshell, slice 9). The async-shaped transport uses
`sys_channel_send` (Block/NoBlock) + `sys_wait`-on-recv (no async executor in
Phase 2).

- [x] **Part 1 — `librsproto` wire codec** (`phase-2/slice7-librsproto`): the
  pure `no_std`/no-`alloc`/no-deps codec — `RsMsgHeader` envelope, explicit LE
  byte serialization, the Meta bodies (Hello/Ping/Ready), and the new
  `Namespace::Resolve` op (`docs/spec/rsproto-namespace-ops.md`,
  `RESOLVE_FILE_AS_MEMOBJ` + 64 KiB cap). 11 host round-trip tests. *(Meta-op
  codec done; the Hello version-negotiation **logic** is the fs-server, Part 4.)*
- [x] **Part 2 — ext4 read-only reader** (host-testable library,
  `phase-2/slice7-ext4-reader`): `userspace/fs-server-ext4/` (lib-only; the
  `[[bin]]` is Part 4). A `BlockReader` trait (`read_at(offset, buf)`) so the
  parser is `no_std`/no-`alloc` (reads into caller buffers + bounded stack
  scratch) and 100% host-tested. Superblock (`0xEF53`, reject 64-bit / >4 KiB
  blocks), group descriptors, inode location, the **extent tree** (`0xF30A`,
  depth 0 + index levels), linear `ext4_dir_entry_2` walk, path resolve →
  `read_file(path, out) -> size`. 6 host tests against **real `mke2fs` images**
  (1 K + 4 K blocks). Skips journal/bigalloc/inline-data/htree-specific/64-bit/RW.
- [x] **Part 3 — kernel transparent-forwarding** (`phase-2/slice7-fwd`):
  `BindingTarget::UserspaceServer` + `ResolvedTarget` arm; `OpStatus::Pending`;
  the new `UserspaceServerReg` kobject (type 13) owning the kernel endpoint + the
  N=1 pending-lookup table; `IpcChannel` `us_reg` back-pointer; `sys_ns_bind`
  `IpcChannel`→`UserspaceServer` branch; `sys_ns_lookup` forwarding arm (originate
  via `IpcChannel::send_push`, leave PO pending); **inline-in-send** reply
  completion (`run_pending` runs only at the interrupt-dispatch tail, so a DPC
  would add a tick of latency — see the decision log) with cross-context install +
  PO signal; dead-server / dead-client / duplicate-reply race handling; the kernel
  hand-coded rsproto Resolve mirror (`kernel/src/rsproto.rs`). **Proven in QEMU by a
  single-process self-forwarding demo in `parent`** (bind a Userspace Server, look
  a path up through it, serve the kernel-forwarded Resolve, map the returned
  MemoryObject) — no second binary / disk needed. *Refinements vs. the original
  plan:* the stub server is the inline `parent` demo (not an embedded ELF), so
  `ImageId::FsServerExt4` + the embedded fs-server move to **Part 4** (their first
  real consumer); a forwarded lookup's returned object takes rights `requested ∩
  the rights the server granted on the transfer` (the bound IPC endpoint's rights
  are not a meaningful content cap) — see `rsproto-namespace-ops.md`.
- [x] **Part 4 — the real `fs-server-ext4` process** (`phase-2/slice7-fs-server`):
  the server `[[bin]]` wiring Part 1 (librsproto) + Part 2 (ext4 reader) + a
  `BlockReader` over `sys_io_submit`. **Alloc-free** (fixed `.bss` buffers, no
  global allocator). Bootstrap: receive the **control channel** in `rdx`; recv the
  **setup message** transferring the read-only device handle; create the forwarding
  channel pair, keep the serving end, send `Meta::Ready` on the control channel
  **transferring the kernel end** (init binds it, Part 6); then the serve loop
  (recv `Namespace::Resolve` → `serve_resolve` → fill + restrict + transfer a
  `MemoryObject`). The request→reply logic (`serve` module, generic over
  `BlockReader`) is **host-tested** against the `mke2fs` fixture (success +
  NotFound + directory + wrong-op + garbage). Adds `ImageId::FsServerExt4 = 3`
  (kernel enum + libkern `IMAGE_FS_SERVER_EXT4` mirror) + the embedded ELF + the
  xtask build step. *(No QEMU yet — the server needs a disk (Part 5) and a
  supervisor to spawn + bind it (Part 6); end-to-end boot is the Part 6 milestone.)*
- [x] **Part 5 — xtask ext4 test disk** (`phase-2/slice7-ext4-disk`): the boot disk
  grows to **128 MiB** with two GPT partitions — the FAT32 ESP (`NITROX_ESP`, 48 MiB)
  and the ext4 `nitrox-root` (filling the rest). Both partitions are built as
  separate, exactly-partition-sized images (so each filesystem is bounded to its
  partition) and **spliced** into the GPT disk at the offsets queried from
  `sgdisk -i`: the ESP via `mformat`/`mcopy`, the rootfs via `mke2fs -d` (populate-
  at-creation, no root/mount; features `^has_journal,^64bit,^metadata_csum,
  ^resize_inode`, 4 KiB blocks — the reader's supported set) staging
  `/system/current-generation`. **Confirmed:** the slice-6 GPT driver enumerates
  *every* non-empty entry (no type-GUID filter) and decodes the ASCII label, so
  `nitrox-root` rides the existing boot disk (no separate QEMU drive); QEMU boots
  clean (`gpt: 2 partition(s)`, the smaller ESP still FAT32-boots) and
  `/dev/disk/by-partlabel/<label>` binds (proven via `NITROX_ESP` in `parent`'s
  block demo). The Part-6 init loop resolves `gpt-partlabel:nitrox-root` → the
  device handle.
- [x] **Part 6 — init mount loop + the milestone** (`phase-2/slice7-mount-milestone`):
  the slice's end-to-end payoff. `manifest::device_ns_path` maps
  `gpt-partlabel:nitrox-root` → `/dev/disk/by-partlabel/nitrox-root`; per `MountSpec`
  (topo order) init resolves the device handle (READ|TRANSFER), `sys_channel_create`s
  a control channel, spawns `fs-server-ext4` (the control endpoint moved in via
  spawn → `rdx`), sends a **setup message** transferring the device handle, awaits
  **Ready** (bounded 30 s, hand-parsed — magic + op, init never speaks librsproto),
  and `sys_ns_bind`s the forwarding endpoint at the mount point. Then the milestone:
  `ns_lookup_wait("/system/current-generation", MAP_READ)` → map → log. **Proven in
  QEMU:** `fs-server: ready` → `init: mounted fs-server-ext4 at /` →
  `init: /system/current-generation = nitrox-rootfs generation 1` — the whole stack
  (ext4 on disk → fs-server `sys_io_submit` → librsproto reply → kernel cross-context
  install → init maps + logs), with the boot staying clean afterward (`parent` demos
  + reaping, no `#DF`/panic). *(Fix found here: the `fs-server-ext4` crate was missing
  the `.cargo/config.toml` + `build.rs` + `user.ld` that force static **ET_EXEC**
  linking — it built as a PIE/`ET_DYN`, which `load_elf` rejects. Copied init's
  lib+bin variant, `rustc-link-arg-bins`, so the fixed-address script reaches the bin
  but not the host lib-test link.)*

**Slice 7 is COMPLETE** — the first userspace resource server, reached transparently
through the namespace, serving a real ext4 filesystem on disk.

Read-only is the Phase-2 target; RW (and writeback) is Phase 3. Path-based spawn
from the initramfs (replacing the embedded `ImageId`) defers to slice 8.

#### 8. Page cache integration with fs-server

Makes file-backed mappings **lazy**: a `sys_memory_map` of a file reserves the range
and faults pages in on demand through a kernel **page cache**, replacing slice 7's
eager whole-file `MemoryObject` copy (and lifting its 64 KiB cap). Depends on the
demand-paging `#PF` handler + `MappingKind::FileBacked` from the prerequisite band —
the fault-in path is what makes "reads files" real.

The page cache, the lazy `FileBacked` VMA, the lazy `sys_memory_map`, and the
**async fault path** (the hard part — a file fault submits the read, **parks** the
faulting thread, and resumes it at the faulting instruction on completion, so the
`#PF` handler never blocks) are built behind a **fill-producer seam** ("fill
page-cache page for file F, offset X") so the fill mechanism is swappable.

Slice 8 uses the **range-read fill (Model B)**: on a miss the kernel asks the
fs-server for the *bytes* of a range (a new rsproto op), reusing the slice-7
fs-server's `BlockReader`/ext4 reader; the kernel copies them into a page-cache page
and maps it. This is the general fill (works for any fs-server, block-backed or not)
and a small delta over slice 7. The **extent fill (Model A)** — fs-server returns
LBA extents and the kernel reads blocks **zero-copy** into cache pages — is the
optimized path for block filesystems and is deferred to Phase 3, where writeback
forces the same extent machinery (see Phase 3 § "fs-server-ext4 read-write"). See the
decision log (2026-06-25 — page-cache fill model).

Built as ordered Parts (each independently provable), mirroring slice 7. The
detailed contracts (`rsproto-file-ops.md`, the `FileObject` handle-encoding entry,
the memory-management page-cache section) are written in their Parts, as in slice 7
— not front-loaded. See the decision log (2026-06-25 — slice 8 fill model + scope).

- [x] **Part 1 — `FileObject` kobject + the page cache** (`phase-2/slice8-file-object`,
  PR #72). The new kobject (type **14**) owns a **sparse per-page cache**
  (`reserve`/`mark_ready`/`lookup`; frames freed on drop) behind the fill-producer
  seam. Host-tested; no fault path. *(The producer fields — fs-server endpoint +
  suffix — deferred to Part 3, their first consumer.)*
- **Part 2 — lazy `FileBacked` mmap + the async fault path** (the hard part), split
  into two for a focused review of the scary async half:
  - [x] **Part 2a — the FileBacked VMA + fault wiring** (`phase-2/slice8-fault-path`):
    `sys_memory_map` on a `FileObject` → `AddressSpace::map_file` (a lazy
    `MappingKind::FileBacked` VMA holding the object, **no PTEs**); `fault_in`'s
    FileBacked arm → `FaultIn::FileBacked` (a signal — it does **not** touch the file
    cache, whose lock is rank 4 like the AS lock and must never nest); `file_backing`
    (re-fetch the object + page index outside the AS lock) + `map_file_page` (install
    the PTE for a resident cache frame, re-validating the VMA). Fully host-tested (5
    tests); no async, no producer — a file fault is still fatal until 2b.
  - [x] **Part 2b — the async fill + block-on-fault + the stub proof**
    (`phase-2/slice8-fault-fill`). `try_fault_in` (the `#PF` handler) on
    `FaultIn::FileBacked` → `AddressSpace::file_backing` → `FileObject::fault_in_page`
    (reserve; create a fill PO; `start_fill`; **block the faulting thread** on the PO
    via the scheduler's `wait_on` — sound: the ring-3 fault holds no kernel locks, and
    the block switches to another thread while the timer keeps the DPC draining) →
    `map_file_page` on wake. The `FileObject` gained a `Producer` (`Stub { base }`;
    Part 3 adds `FsServer`); the stub fill enqueues a DPC that writes the page +
    `mark_ready` + completes the PO. **Proven in QEMU** by a boot fixture (a stub
    `FileObject` bound at `/dev/test/pagecache` in pid-1's namespace) + a `parent`
    demo that maps it and reads one byte from each of 3 pages — a **real user fault**
    that parks + resumes: `page-cache demand-faulted 3 pages ok (0xA0,0xA1,0xA2)`,
    boot clean (no `#DF`/panic). No fs-server/IPC.
- [x] **Part 3 — the `File::ReadRange` wire op** (the Model-B fill contract;
  `phase-2/slice8-readrange`). A new **`File` category at `0x06`**
  (`docs/spec/rsproto-file-ops.md`): `File::ReadRange(offset, len, suffix) → bytes`
  (the bytes ride in `handles[0]` as a ≤1-page `MemoryObject`; `content_len` covers
  the short EOF tail). librsproto codec (`file.rs`) + the kernel mirror
  (`build_read_range_request` / `parse_read_range_reply` / `reply_op` router) + the
  paired `Namespace` additions (`RESOLVE_FILE_LAZY` flag, `OBJECT_KIND_FILE`). `File`
  is kept distinct from `Stream` (`0x02`, cursor I/O) and `Block` (`0x03`, Model A's
  future extent home). Host round-trip tests pin the offsets both sides. **Wire
  contract only** — the kernel send-side + the fault wiring land in Part 4 (a page
  fault blocks the faulting thread, so the *filler* must be a separate process — the
  real fs-server — which arrives in Part 4; isolating the send-side would need
  throwaway two-process scaffolding).
- [x] **Part 4a — the kernel send-side + lazy-resolve plumbing** (dormant;
  `phase-2/slice8-fill-integration`). The `FileObject` gains `Producer::FsServer
  { reg, suffix }`; `start_fill` originates a `File::ReadRange` over the slice-7
  forwarding endpoint (`sched::us_forward_originate_fill`), recording a pending-**fill**
  slot on `UserspaceServerReg` (`PendingFill`, alongside the pending-lookup slot, own
  `request_id`). The reply-completion path routes by `rsproto::reply_op`: a `Resolve`
  reply on `OBJECT_KIND_FILE` **builds a `FileObject`** (no handle; `content_len` = file
  size; producer ← reg + the lookup's inline-stored suffix) and installs it instead of
  an eager `MemoryObject`; a `ReadRange` reply copies the transferred ≤1-page
  `MemoryObject` into the cache frame, marks the page ready, completes the fill PO. The
  kernel now requests `RESOLVE_FILE_LAZY`, but the unchanged fs-server ignores it and
  still replies `MEMOBJ` — **boot stays eager** (the kernel handles both kinds). Host
  tests for the reg's fill slot + stored suffix; QEMU regression = eager milestone +
  stub demo still work.
- [x] **Part 4b — the fs-server side (activates + proves the lazy path)**
  (`phase-2/slice8-fill-integration`). The ext4 reader gained `stat_file` (size, no
  content, no `MAX_FILE` cap) + `read_file_range` (positioned per-block extent read),
  sharing a `resolve_regular_file` helper. `serve` dispatches by op: a
  `RESOLVE_FILE_LAZY` resolve replies `OBJECT_KIND_FILE` + size, no handle; a
  `File::ReadRange` reads the range → replies a `MemoryObject` of the bytes
  (**stateless**, re-resolving per range). Error replies carry the request's op so the
  kernel routes a failed fill to the pending fill (not a lookup) — else the faulter
  hangs. **Proven by the slice-7 milestone going lazy** — init's
  `/system/current-generation` lookup returns a `FileObject` and faults in via
  `ReadRange` from the real fs-server (`init: /system/current-generation = nitrox-rootfs
  generation 1`, boot clean). Retired the Part-2b stub fixture + parent demo
  (`Producer::Stub` stays for host tests). **Slice 8's Model-B core is complete.**
- [x] **Part 5 — disk + the large-file milestone** (`phase-2/slice8-large-file`).
  xtask stages `system/large.bin` (256 KiB / 64 pages) with position-sensitive content
  (`byte[i] = ((i >> 12) ^ i) as u8`); init maps it lazily and reads **every** byte
  (`read_large_file`), each first page-touch a demand fault served by a `File::ReadRange`
  to the fs-server, verifying against the shared `fill_byte`. QEMU: `init: large.bin
  verified 262144 bytes across 64 demand-faulted pages ok` — the 64 KiB cap is gone,
  **multi-page demand faulting proven end to end**. (Multi-page, not multi-extent: a
  256 KiB file is laid contiguously as a single extent; the extent tree's interior-node
  path stays host-tested. init learns the size from a shared `LARGE_FILE_BYTES`
  constant — a temporary bridge; proper discovery (a `HandleInfo.size` field via
  `sys_handle_stat`) is deferred to its first real consumer, eshell `cat` in slice 9.)
  **Phase 2 slice 8 (the kernel page cache) is complete.**

Deferred to Phase 3: the **Model A extent fill** (block-fs zero-copy fast path, added
*alongside* `ReadRange` which stays the general fallback) + writeback (with
fs-server-ext4 RW) — see Phase 3 § "fs-server-ext4 read-write".

#### 9. Emergency shell — `eshell` + the first user input

The first **interactivity**: a serial command shell + the **keyboard/serial input**
subsystem behind it. Input is read through the **universal device interface**
(`sys_io_submit` + `sys_wait`) — the console is a char-class `DeviceNode`, not a
console-specific syscall. **Deferred** (decided with the user): `reboot` (needs an
`ArchPower` interface) and `edit` (needs filesystem write + an editor); the userspace
console/tty server (cooked line discipline) layers on the raw char device later. See
the decision log (2026-06-27) and the design in `docs/conventions/arch-boundary.md`
(`console_arm_rx`) + `docs/spec/io-operation.md` (the char read path).

- [x] **Part 1 — console input subsystem (kernel)** (`phase-2/slice9-eshell`, PR #78):
  interrupt-driven COM1 RX driver (`drivers/console.rs`: ring + parked-read slot +
  ISR→DPC), `DeviceClass::Char` + `CharBackend`, the `sys_io_submit` char branch (a
  stream read completing a PO), `/dev/console` (`KernelServerId::Console`). `install_isa_irq`
  kept arch-internal; the console arms RX via the neutral `arch::serial::console_arm_rx`.
  Proven by a boot loopback self-test.
- [x] **Part 2 — the eshell crate + line editor + interactive launch**
  (`phase-2/slice9-eshell-crate`, PR #79): `userspace/eshell` (new, `no_std`+no-alloc,
  libkern only); a line editor over `/dev/console` via `io_submit`+`wait` (echo,
  backspace, CR/LF); `help` / `echo` / `lsblk`; `ImageId::Eshell = 4`; init spawns it
  as the persistent interactive console. **Proven by a scripted serial session** —
  real typed input through the Part-1 ISR path end to end.
- [x] **Part 3 — `cat` + `HandleInfo.size`** (`phase-2/slice9-cat`): added `size: u64`
  to `HandleInfo` (kernel + libkern; `stat_on` reads the per-type size; the lazy resolve
  grants `INSPECT`), and eshell `cat <path>` (lookup → stat → map → demand-fault → print,
  NUL-trimmed). Closes the slice-8 size-discovery deferral. Also **retired the concurrent
  `parent` demo**: it now runs to completion *before* eshell (the shared
  single-outstanding-command disk was corrupting the fs-server's reads → flaky `cat`),
  giving a clean console — resolving the Part-2 follow-up.
- [x] **Part 4 — `mounts` + `sys_ns_enumerate`** (`phase-2/slice9-mounts`): a
  namespace-binding enumerate syscall (`= 30`; `sys_ns_enumerate(ns, index, out)` →
  `NsEntry { path, path_len, kind, rights }`, requires `LOOKUP`, `NotFound` past the
  end), listing mount points + kernel resources (**not** fs `readdir`). eshell `mounts`
  lists them with kind tags (kernel resource / direct / mount). Proven in QEMU.
- [x] **Part 5 — kernel log ring + `/dev/log`** (`phase-2/slice9-klog`): `kernel/src/klog.rs`
  (a 16 KiB append buffer teed from the serial `write_str` path — `kprint!` + the panic
  writer; `IrqSpinLock::try_lock` keeps the tee panic-safe) + a `/dev/log` resource
  (`KernelServerId::Log`, a `MemoryObject` snapshot). Read with `cat /dev/log` (no bespoke
  `dmesg`). Bonus: `sys_kprint` now translates `\n`→`\r\n`, fixing all userspace terminal
  rendering. Proven in QEMU (the kernel boot log dumps correctly).
- [x] **Part 6 — init failure → eshell** (`phase-2/slice9-init-failure`): implement the
  documented critical-path-failure drop to eshell (`userspace/init/CLAUDE.md` §"Failure →
  eshell"). `mount_all` now returns `bool` (a failed required mount is critical-path);
  `_start` computes `booted` from `read_manifest` + `mount_all` and, when `!booted`, calls
  `emergency(notif)` (logs `init: critical-path failure -- dropping to emergency shell`,
  spawns eshell, enters the reaping loop) instead of running the boot milestones + `parent`
  demo. `supervise` was split into `supervise` (healthy: parent → `reap_loop`), `emergency`
  (failure: log + `spawn_eshell` + `reap_loop`), and a shared `reap_loop(notif, parent_h)`.
  Proven in QEMU both ways: a forced bad device label (`gpt-partlabel:does-not-exist`) drops
  straight to an `eshell>` prompt with no demo, and the operator can still inspect the broken
  system (`mounts` lists every binding *except* the failed `/`, `lsblk`, `cat /dev/log`);
  the healthy boot is unchanged (milestones → `parent` → reap → eshell). **Slice 9 complete.**

#### 10. FAT for completeness (RO is fine for now) — **deferred to Phase 3**

**No Phase 2 milestone clause consumes `fs-server-fat`,** so this slice is
**deferred to Phase 3** (decided 2026-06-26 at Phase 2 close). The ESP's FAT32
is read by UEFI firmware and Limine, *not* by Nitrox — booting never requires
Nitrox to read its own ESP. This server exists for parity/completeness, not for
boot, and ext4 already proves the userspace-filesystem path end to end. Pick it
up when an in-OS FAT consumer appears (e.g. updating the ESP from within the OS).

- [ ] `userspace/fs-server-fat/` crate (FAT32/FAT16/FAT12 read-only) — *Phase 3*
- [ ] Needed only for in-OS access to FAT volumes (e.g. updating the ESP from
  within the OS), not for booting — *Phase 3*

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

### Tasks

Unlike Phase 2's flat list, the **kernel-first** work (scheduler + SMP) is sequenced
into slices 0–3 below. The userspace workstreams that follow (service manager,
runtime libraries, content store, the standard services, auth/session, fs-server RW)
remain an **unsequenced backlog** — most have no design doc yet (see the
2026-06-26 Phase 3 scope analysis in the decision log) and are sliced + given their
missing architecture docs **just-in-time** as we reach them, the same way Phase 2's
slices were defined.

#### Kernel-first slices (sequenced)

The design is committed in `os-design-v5.1.md` §Scheduling (three classes —
RealTime fixed-priority FIFO, TimeShared CFS-like vruntime, Idle) and staged in the
decision log (2026-05-29, "Step 3 = SMP"). Today the kernel is single-CPU
**preemptive** with a **single global `SCHED` lock + flat round-robin runqueue**
(`sched.rs:299,258,474`) and a **stub SMP layer** (`cpu_count→1`,
`send_ipi→unimplemented!`, Limine SMP request unwired, shared GDT/TSS, local-only
TLB flush). The spinlocks are already SMP-correct (atomic CAS). Rollout is
**incremental**: APs first run against the existing global runqueue (slice 1);
per-CPU runqueues are a later refactor (slice 3), so AP bring-up and load-balancing
are bisectable in isolation.

- [x] **Slice 0 — Per-CPU foundation + scheduler/SMP design doc**
  (`phase-3/slice0-percpu-foundation`): wrote `docs/architecture/scheduler.md` (the
  three-class + vruntime + x2APIC + incremental-SMP design) and built the per-CPU
  substrate, still single-CPU. **Per-CPU access is arch-abstracted** — neutral
  `arch::Smp::current_cpu()` (a dense index), implemented x86-side via `RDTSCP` /
  `IA32_TSC_AUX` (`init_this_cpu` sets it; dev loop opts in `+rdtscp`); `MAX_CPUS=8`.
  The arch `CpuLocal` GS block became `CPUS[MAX_CPUS]`; the scheduler's `current`/
  `idle`/`idle_addr` became per-CPU arrays behind `cur_slot`/`idle_slot` accessors
  (single global `ready` + `SCHED` lock retained); `handle::current_ctx_id()` now
  keys on `current_cpu()`. Page-table-root / `active_cpus` tracking was **refined to
  slice 1** (no slice-0 consumer). *Verified:* build clean / check-arch / 8 host test
  suites green; boots identically to today (full `parent` demo → `eshell`), no faults.
  See the decision log (2026-06-26, Phase 3 slice 0).
- [x] **Slice 1 — SMP bring-up (APs on the shared runqueue)** (`phase-3/slice1-smp-bringup`):
  Limine's SMP request + AP startup (atomic `goto_address`, no INIT/SIPI); **x2APIC**
  (committed; MSR accessors, EXTD enable, single-`WRMSR` ICR `send_ipi`); per-CPU
  **GDT/TSS** + the shared IDT loaded per-CPU; per-CPU APIC timer; AP entry
  (`init_this_cpu` → `ap_cpu_init` → arm timer → `sched::ap_run`) creating a per-CPU
  boot+idle and pulling from the **shared** runqueue. Fixed an SMP reap
  use-after-free (per-CPU `reap` lists) + re-assert `KERNEL_GS_BASE` on user-thread
  switch-in. *Verified:* `-smp 4` boots the full userspace (init → ext4 mount → parent
  demo → eshell) **reliably** (6/6), 4 CPUs online, APs executing kernel code; `-smp 1`
  unchanged; check-arch + 8 host suites green. **Two pieces deferred to slice 3's SMP
  hardening** (below): **TLB shootdown + `active_cpus`** (not yet triggered by the
  read-only workload, but required for concurrent cross-CPU unmaps), and **user-thread
  migration safety** — a kernel-stack UAF in `syscall_entry` when a user thread is
  forced to bounce between CPUs under pathological churn (the shared-runqueue model's
  cross-CPU hazard; per-CPU runqueues remove the churn). Also unfixed:
  `has_live_siblings`/`exit_process` only scan parked lists, not other CPUs' `current[]`.
  See the decision log (2026-06-26, Phase 3 slice 1).
- [x] **Slice 2 — Scheduler classes** (`phase-3/slice2-scheduler-classes`): `SchedClass`
  (RealTime / TimeShared / Idle) + `rt_priority` / `nice` / `vruntime` on `Thread`;
  class-aware `dequeue_front` (RealTime by `rt_priority` FIFO → TimeShared by smallest
  `vruntime`; O(n) scan over the shared `ready`); CFS-like vruntime accrual per tick
  (Linux nice-weight table) with a `min_vruntime` floor + wake latency-boost; a
  kernel-thread `spawn_with_class`. *Verified:* a `sched_class_demo` shows the RealTime
  worker finishing **before any** TimeShared round, and the nice-0 worker completing all
  rounds while nice-10 is still on round 1 (vruntime fairness); `-smp 1` + `-smp 4`
  (2/2) clean to eshell, 0 faults; +4 host tests (521 total), check-arch green.
  **Deferred to the SysCaps slice** (no capability system exists yet): the `REAL_TIME`
  syscap gate and the user-facing **`ThreadArgs` class/nice/affinity ABI** — user
  threads default to TimeShared; kernel threads set class directly for now. See the
  decision log (2026-06-29, Phase 3 slice 2).
- [x] **Slice 3 — Per-CPU runqueues + work stealing + affinity** (`phase-3/slice3-percpu-runqueues`):
  `ready`/`min_vruntime` split per-CPU (mirroring `current`/`idle`/`reap`), one `SCHED`
  lock; a `place_thread` policy (kernel threads → least-loaded CPU; waking thread → its
  `last_cpu` home); **work stealing** (idle CPU steals from the busiest peer; routed via
  `pick_next` + an idle-tick trigger); **affinity** (`cpu_mask` on `Thread` honoured by
  placement/steal; `sys_thread_set_affinity` functional, `SIGNAL`-gated — no SysCaps);
  `has_live_siblings` now scans other CPUs' `current[]`. Fixed a placement bug — online
  CPUs are tracked by a `cpu_online[]` **mask** (APs run `ap_init` in arbitrary order, so
  they aren't a dense `0..n` prefix). *Verified:* distribution demo → all APs (`0b1111`);
  affinity demo → each worker on its pinned CPU; `-smp 4` **12/12** clean full boots;
  `-smp 1` unchanged; +6 host tests (524); check-arch green. **Migration decision (option
  B):** the slice-1 `syscall_entry` user-thread-migration UAF reappeared once placement/
  stealing moved user threads, so slice 3 **prevents user-thread migration entirely** —
  user threads stay on their creating CPU (re-home/wake home, never stolen); kernel
  threads distribute. Cost: userspace runs on the BSP for now. **Deferred to slice 3b:**
  TLB shootdown + `active_cpus`; the cross-CPU deschedule IPI (`exit_process` can't yet
  terminate a sibling *running* on another CPU); root-causing the `syscall_entry` hazard
  so user threads can use the APs. See the decision log (2026-06-29, Phase 3 slice 3).
- [ ] **Slice 3b — SMP correctness completion.** TLB shootdown via IPI + per-`AddressSpace`
  `active_cpus` (set/cleared at the CR3 load in `switch_into`; broadcast from the unmap
  sites); a cross-CPU **deschedule IPI** so `exit_process`/kill can stop a sibling running
  on another CPU; root-cause the `syscall_entry` per-CPU-stack hazard so **user threads may
  migrate** (use the APs) safely. *Verify:* concurrent cross-CPU unmap stays coherent; a
  multi-threaded user process runs across CPUs and an aggressive churn stress test is clean.
  - [x] **Hazard root-caused + two fixes landed** (2026-07-01): the migration hazard is a
    bring-up timing race — a user thread running on a not-yet-fully-initialised CPU. Fixed
    (a) **syscall MSRs re-armed at ring-3 descent** (`arm_user_entry_cpu_base`, the SCE
    `#UD`), and (b) **dense CPU indices derived from the hardware APIC id**
    (`bind_cpu_identity` / `adopt_dense_index`) so they're unique by construction (no
    GDT/TSS/scheduler-slot collision). See the decision log (2026-07-01).
  - [x] **TLB shootdown** (broadcast IPI + synchronous ack) — cross-CPU invalidation for the
    shared kernel vmap, wired at the kstack free site (`KernelStack::Drop`). `crate::tlb`
    (neutral coordinator) + `arch::send_shootdown_ipi` (transport, vector 0x40).
  - [x] **Migration hazard fully fixed — KVM 0/150** (2026-07-01): two further root causes.
    (a) **Switch-out race** — a stolen thread could resume from a not-yet-committed
    `saved_sp`; fixed with a Linux-style `Thread::on_cpu` guard (set under `SCHED` in
    `switch_into`, cleared by `context_switch` after committing `saved_sp`; `stealable_to`
    skips guarded threads). (b) **Dense-index collision** — `init_this_cpu(0)` ran *after*
    `bring_up_aps()` on the migratable boot thread, zeroing a migrated AP's TSC_AUX → dense-0
    aliasing; moved before AP bring-up. Also fixed a pre-existing host-test SIGSEGV
    (`flush_tlb_*` privileged ops now `#[cfg(test)]` no-ops). See the decision log.
  - [x] **User-thread migration enabled** (2026-07-01): dropped the `is_user` exclusion in
    `stealable_to` and the creating-CPU pin in `place_thread` — user threads now distribute
    across the APs (least-loaded placement) and are work-stealable, relying on the per-switch
    re-arm of CR3 + TSS.RSP0/syscall-stack/`KERNEL_GS_BASE` (`switch_into` →
    `arm_kernel_stack_for`). *Verified:* `-smp 4` KVM boot-loop **0/150**; scripted `eshell`
    interaction (`help`/`lsblk`/`mounts`/`cat …`) clean across a 50-boot stress batch with
    userspace running on the APs. `Thread::is_user` removed (last consumer gone).
  - [ ] Cross-CPU **deschedule IPI** + per-`AddressSpace` `active_cpus`. Not yet triggered:
    every userspace process today is single-threaded, so `exit_process` never has a sibling
    running on another CPU, and TLB shootdown broadcasts to all online CPUs (correct, if
    unoptimised). Lands with the first multi-threaded user process.

#### Userspace-runtime slices (sequenced)

With the kernel-first band done, the next sequenced work is the **userspace
runtime foundation** the services are built on. Sequencing rationale (decided
2026-07-13 — see the decision log):

- **Defer a real `std` port** (Phase 4+, unchanged). std is POSIX-shaped —
  ambient-authority `fs`/`net`, synchronous blocking `io`, errno, signals,
  `thread_local!` — none of which map onto Nitrox's capability + async-first +
  no-signals model without either lying (reintroducing the Unix patterns the OS
  rejects) or stubbing out half the surface. Its payoff (building *unmodified*
  crates.io crates) isn't needed yet, and it sits on the syscall ABI, which is
  explicitly pre-stabilization. Revisit once the ABI is stabilizing (v0.1+) and a
  concrete external crate justifies it.
- **Invest instead in Nitrox-native runtime libraries** (`libheap`/`libos`/
  `libstream`, on `libkern`) that give services standard-Rust ergonomics (`alloc`
  collections, `async` over `sys_wait`, typed streams) without the POSIX baggage.
  Design their APIs std-shaped where it's free (e.g. an `io::Error`-shaped error
  type) so a later std port is mostly re-exports over these libs, not a rewrite.
  **No `librt` crate** — the Go-style fiber scheduler and a standalone sync-wrapper
  crate were both cut (2026-07-13 decision log): in-process concurrency is `async`
  tasks on the libos executor, and a fiber runtime would be a second, non-standard
  concurrency model that fights a future `std` (`thread_local!`, `std::sync`) with no
  capability upside; a sync-wrapper crate is just `std::io` and would be deprecated by
  the port. The residual blocking convenience (`block_on`) folds into libos.
- **Order by ABI coupling.** The syscall surface these libs wrap is mostly *solid*
  today (handles, memory, `wait`, IPC, notifications, ns, `io_submit`, entropy);
  the parts that will still move are **SysCaps** (unimplemented) and the
  **`SpawnArgs`/`ThreadArgs`** growth (class/nice/affinity + syscap inheritance).
  So wrap the solid core first, and hold the thread-spawn/authority-facing wrappers
  until SysCaps + those ABIs finalize — the same ABI-stability discipline that
  deferred std, applied one level down.
- **Dogfood via init/eshell.** Each library's *first consumer* is init (and eshell)
  — converting them de-hacks the existing critical-path code, validates the lib
  against real code before any service depends on it, and honours the "no code
  without a consumer" rule. Constraint: init is critical-path (no `panic!`/
  `unwrap`); every conversion rides behind the existing gate — still boots to a
  live `eshell>` and passes the scripted `help`/`lsblk`/`mounts`/`cat` stress.

Net order: **allocator → libos core + libstream → SysCaps → the SysCaps-coupled
libos authority surface → services.**

- [x] **Slice 4 — Userspace allocator (freeing heap)** (2026-07-13). Replaced init's
  bump-arena `#[global_allocator]` (which never frees) with a real freeing userspace
  heap. Independent of the syscall-ABI churn ahead (it only consumes
  `sys_memory_create`/`sys_memory_map`/`sys_memory_unmap`), so it led.
  - [x] `userspace/libheap/` crate — a `GlobalAlloc` over `MemoryObject` backing
    (grows on demand by mapping 64 KiB arenas, vs init's fixed arena): segregated
    size-class freelists (16–2048 B, no coalescing) + a dedicated-mapping large path
    (unmap + close on free). `#![no_std]` + core; the [`HeapEngine`] is generic over
    an `ArenaSource` so the logic is host-tested (9 tests) with a `std`-backed source.
  - [x] **Design doc** `docs/architecture/libheap.md` (done 2026-07-13) — backing
    model (multi-arena over `MemoryObject`s), size-class-over-slabs structure, the
    engine/registration split (std-port seam), reclamation policy, init cutover.
  - [x] Freelist guarded by a userspace spinlock (single-threaded per process today,
    but a real lock so future std OS-threads are correct without a redesign). No
    FPU/TLS dependency.
  - [x] **First (and only) consumer:** init drops its bump `#[global_allocator]` and
    uses `libheap`; its `heap.rs` is retired. (eshell needs **no** allocator — it's
    `no_std` without `alloc`, fixed buffers — so there was nothing to migrate there.)
  - *Verified:* 9 libheap host tests (alloc/free/realloc-via-default/alignment/reuse/
    multi-arena grow/large path); full host suite + check-arch green; bare build clean;
    QEMU boots init's allocation-heavy bootstrap on libheap → ext4 mount → parent demo
    chain → `eshell`, with scripted `help`/`lsblk`/`mounts`/`cat` all correct; `-smp 4`
    clean (4 CPUs, no faults). See the decision log (2026-07-13).

- [x] **Slice 5 — `libos` core (the SysCaps-independent runtime)** (2026-07-13). The
  typed + async face of the *solid* syscall surface. **Scoped down:** `libstream` and
  the *multi-task* executor were cut (consumer-less; see the decision log), so slice 5
  is libos core only. Built in parts A–D (one commit each).
  - [x] **Design doc** `docs/architecture/libos.md` — the `Handle<T, M>` typestate
    model (from `os-design-v5.1.md`), the `Op` future over `sys_wait`, `block_on`, the
    `io::Error`-shaped error, the host-test syscall seam, the thin-entry seam. (Part A.)
  - [x] `userspace/libos/` — **`#![no_std]`, no `alloc`**: `Handle<T, M>` typestate
    wrappers (sealed `CanRead`/`CanWrite`/… gate ops; RAII close; `borrow` for
    non-owning views; attenuation-consumes-self) over Memory / Namespace / Notify /
    Resource; the `Op` future (wraps a PO; polls via `sys_wait`); async methods
    (`read`/`write`, `ns.lookup`, …); **`block_on`** (single-task driver, no heap —
    collapses the `po_wait` idiom); `io::Error`-shaped error. (Parts B + C.) 15 host
    tests against a mock syscall seam. *Deferred within libos (no consumer): Channel/
    IPC + Entropy wrappers, namespace bind, file mapping.*
  - [x] **First consumer:** init dogfoods libos — `read_current_generation` now
    `ns.lookup(...).block_on()` + `map()` (borrowing the bootstrap `root_ns`),
    replacing `ns_lookup_wait` + the manual closes. **eshell was deliberately kept
    `libkern`-only** — it's the recovery surface (statically-linked-`busybox`/`sash`
    ethos), so it doesn't take a libos dependency. (Part D — see the decision log; init
    & eshell `CLAUDE.md` reconciled.)
  - **Scope boundary:** no `thread_create`/`process_spawn` wrappers, no syscap-gated
    calls (slices 6–7); **no multi-task `spawn`/run-loop executor** (deferred — needs
    `alloc`, no concurrent consumer). `libstream` deferred (below).
  - *Verified:* 15 libos host tests; init/libos bare-build clean; full host suite +
    check-arch green; QEMU — init's current-generation reads via libos, boots to
    `eshell`, scripted `help`/`lsblk`/`mounts`/`cat` correct; `-smp 1` + `-smp 4` clean.

- [ ] **`libstream` (deferred out of slice 5).** Typed structured I/O
  (`TableWriter`/`TableReader`, `record_read`, `#[derive(TypedRecord)]`) per
  [docs/spec/typed-stream-format.md](../spec/typed-stream-format.md). **No consumer
  until the shell/pipeline era or the service-mgr milestone** (*"a test program
  produces typed TableWriter output to its log channel"*), and it drags in a derive
  proc-macro (first userspace external-crate decision, or a hand-rolled one). Lands
  just-in-time with its first consumer — and wants a **dedicated design pass on the
  `TSM1` wire protocol + streaming model** before implementation. See the decision log
  (2026-07-13).

- [x] **Slice 6 — SysCaps (process-level capabilities)** (2026-07-14). The kernel's
    defining feature — ambient per-process authority — is now real; the handle-`Rights`
    stand-ins are backed by actual syscaps. Built in parts A–C (one commit each).
  - [x] **Design doc** `docs/architecture/syscaps.md` (Part A, 2026-07-14) — the
    6-cap model (from v5.1), storage on `Process`, grant/attenuate-on-spawn
    (`child = parent & args.syscaps`), the `require_syscap` check point, and the ABI
    growth. **Two corrections vs the stub:** affinity stays a **handle right** (not a
    syscap); and — the consumer discipline — **all 6 caps are defined but only 2 gates
    are wired now** (`BIND_NAMESPACE`, `REAL_TIME`), the other four (`LOAD_MODULE`/
    `PHYSICAL_MEMORY`/`SYSTEM_CLOCK`/`AUDIT_CONTROL`) gated by the slice that builds
    their operation.
  - [x] **Part B (plumbing)** — `SysCaps(u64)` type (kernel + userspace mirror,
    host-tested); the immutable `Process.syscaps` field; `SpawnArgs` grown 96→104;
    `sys_process_spawn` inheritance (`child = parent & args.syscaps`); the init boot
    grant (`SysCaps::all()`). Behavior-neutral — no gate enforced.
  - [x] **Part C (enforcement)** — `require_syscap`; the **`BIND_NAMESPACE`** gate on
    `sys_ns_bind` (additional to the `BIND` right → namespace construction is
    supervisor-only); the **`REAL_TIME`** gate + the finalized **`ThreadArgs`**
    class/nice/affinity ABI (into its `_reserved`, size unchanged; RealTime gated, the
    rest ungated); init grants `parent` `BIND_NAMESPACE`. **ABI:** syscall-ABI change
    (self-pinned by asserts + specs), *not* the module hash — source comments corrected.
  - *Verified:* SysCaps + layout host tests; full suite (528 kernel) + check-arch green;
    bare build clean. QEMU — **gate allows** (init mounts fs-server, parent `ns_bind
    /store ok` via its grant) and **gate bites** (parent without the cap: `ns_create ok`
    but `ns_bind FAIL`, even on its own namespace); boots to `eshell`; `-smp 1` + `-smp
    4` clean.

- [x] **Slice 7 — the SysCaps-coupled libos surface** (2026-07-14). The libos pieces
    held back from slice 5, now that SysCaps + `ThreadArgs`/`SpawnArgs` are settled.
    Built in parts A–C (one commit each). (No `librt` crate — cut; see the 2026-07-13
    decision log.)
  - [x] **Design** (Part A, 2026-07-14) — extended `docs/architecture/libos.md` with the
    slice-7 surface. **Thin typed wrappers over the ABI structs, not a fluent builder**
    (consumer-minimal; a builder is a later ergonomic layer, and thin wrappers map
    cleanly onto a future `std::process`/`std::thread` pal): `process::spawn(&SpawnArgs)
    → Handle<Process,Only>`, `thread::create(&ThreadArgs) → Handle<Thread,Only>` (owning
    → RAII close = reaping), and the `BIND_NAMESPACE`-gated `Handle<Namespace,NsMutable>
    ::bind`. Out (consumer-less): runtime `set_affinity`, `terminate`, the Process mode
    lattice.
  - [x] **Part B (wrappers)** — the `Process`/`Thread` markers (`Only` mode);
    `libos::spawn`/`thread_create` (owning handles → RAII reaping); `Handle<Namespace,
    M: CanBind>::bind` (the `BIND_NAMESPACE`-gated call; denial → `PermissionDenied`);
    the `Sys` seam grows `process_spawn`/`thread_create`/`ns_bind` (real + mock).
    `#![no_std]`, no `alloc`. +3 host tests (17 total).
  - [x] **Part C (dogfood)** — `parent` (alloc-free, a demo) adopts all three: its
    worker via `thread_create` (Handle drops → closes, retiring the explicit close), its
    ns-demo bind via `Namespace::bind`, its child spawns via `spawn` (owning handles reap
    on drop, replacing a handle leak). *Verified:* the full parent demo chain runs
    through the wrappers under QEMU (`created worker thread`/`worker terminated`,
    `ns_bind /store ok`, `both children reaped`); boots to `eshell`; `-smp 1`/`-smp 4`
    clean, no faults. init's spawns left raw (surgical scope; its handshake is tangled).
  - **Kernel dependency:** true kernel-thread-backed parallelism (real `std::thread`
    semantics, multicore *within* a process) needs the deferred **TLS (`FS_BASE` /
    `sys_thread_set_tls`) + FPU `XSAVE`** kernel work (Phase 1 deferrals, still
    consumer-gated). Not needed here: `async` tasks on the single-threaded executor
    cover in-process concurrency, so schedule that kernel slice only when a service
    genuinely needs OS-thread parallelism or hard-float.
  - *Verify:* host tests for the spawn/authority wrappers; init spawns a child with
    attenuated syscaps.

The **service backlog below** was originally left **unsequenced** ("slice just-in-time").
After service-mgr (slice A, done 2026-07-15) a real **dependency spine** emerged — most
of the backlog assumes programs are loaded from **paths**, not the kernel-embedded
`ImageId` shim — so the backlog now carries a **recommended ordering** (below), while
still being *sliced* just-in-time within it. The spine, toward the Phase 3 milestone
(services supervised · typed log output · login + per-user namespace + home writes):

1. **Path-based spawn / userspace ELF loader** (next) — retire `ImageId`; load init and
   every program from the **initramfs** (later `/bin`, `/store`). The enabler for
   everything path-based; makes service.toml's `executable` real.
2. **Profile server + content store** — `/bin`, `/lib`, `/store` projection; programs on
   disk. A **read-only** store pre-built into the ext4 image decouples this from
   fs-server RW.
3. **Logging service** — the milestone's "typed log output"; service-mgr's `log` seam
   becomes real.
4. **fs-server-ext4 RW + Model-A page-cache** — the write path ("write files to home").
5. **Auth + session-mgr** — login, per-user namespaces, user-shell spawn. (Must not
   precede SysCaps, slice 6 — done.)
6. **Independent daemons** (device-mgr, audit, namespace-mgr, OOM, mount, crash-reporter)
   — slotted just-in-time as consumers appear.

Global constraint unchanged: service-mgr and everything after depend on the userspace-
runtime slices 4–7.

#### Service manager

- [x] `userspace/service-mgr/` crate (slice A)
- [x] TOML parser for service declarations per [docs/spec/service-toml-schema.md]
      — slice A parses the single-service subset (`[service.<name>]` + `executable` +
      the `[restart]` table); arrays/`[handles]`/multiple services are later slices
- [ ] Dependency graph + topological startup (deferred — a later slice)
- [x] Process supervision with restart policies (never, on-failure, always) (slice A)
- [x] Exponential backoff for restarts (slice A — none/linear/exponential + max_attempts)
- [ ] Resource Server Startup Protocol for spawned RS-style services (deferred — slice B)
- [x] Lifecycle control via per-service control channels (slice A — `CTRL_OP_SHUTDOWN`;
      the protocol grows with health-check/reload)

#### Path-based spawn / userspace ELF loader

**Done (2026-07-16).** Retire the kernel-embedded `ImageId` shim entirely and load every
program from the **initramfs** — the real-OS model (the bootloader hands the kernel an
initramfs; the kernel loads init from it and every subsequent program from a path). Both
halves already exist: the in-kernel initramfs reader (`kernel/src/initramfs.rs`, already
serving files) and the ELF loader (`kernel/src/mm/elf.rs::load_elf`). What's missing is
the **image-source abstraction** — today `SpawnArgs.image` is an enum selecting embedded
bytes; it becomes a handle to the program's bytes.

- [x] **Boot:** the kernel loads `/sbin/init` from the initramfs (initramfs reader →
      `load_elf`) instead of from embedded bytes. Removes the `INIT_ELF` embed.
- [x] **Spawn ABI:** `SpawnArgs.image` becomes a **`MemoryObject` handle** carrying the
      ELF (not an `ImageId`). The **spawner** resolves the program path in userspace
      (`ns_lookup(path, MAP_READ) → MemoryObject` — exactly how init/service-mgr already
      read `init.toml`/`heartbeat.toml`) and passes it to spawn; the kernel maps the ELF
      from the object's pages (via HHDM) and runs `load_elf`. **No filesystem code enters
      the kernel** — the spawner does path resolution.
- [x] **Retire `ImageId` + `kernel/src/embedded_images.rs`** entirely; the libkern
      `IMAGE_*` mirrors go away.
- [x] **xtask:** pack all program ELFs into the initramfs (`/sbin/init`, `/sbin/service-mgr`,
      `/sbin/heartbeat`, `/sbin/fs-server-ext4`, `/sbin/eshell`, + the selftest demos)
      instead of `include_bytes!`-ing them into the kernel.
- [x] **Path resolution for `executable`:** service.toml's `/sbin/heartbeat` resolves
      against the initramfs (a `/sbin` binding, or a documented `/initramfs` prefix) —
      retiring service-mgr's slice-A `image_for_executable` stopgap. `/bin`, `/store`
      resolution arrives with the profile server + store.
- **ABI note:** `SpawnArgs` layout changes (`image` enum → handle) — invalidates the
  spawn-args contract; update `docs/spec/process-spawn-args.md` + `syscall-abi.md` (fix
  its stale "there is no filesystem yet" — untrue since slice 8) and the ABI hash.
- **Verify:** boot with no embedded images — kernel loads init from initramfs; init and
  service-mgr spawn their children from `/sbin/*`; the full slice-A lifecycle still runs;
  `--selftest` + `test-qemu` green. Bounds: the ELF still must be a static `ET_EXEC`
  (the loader's existing constraint); real stacks/argv/guard-pages stay deferred.

#### Runtime libraries (full versions)

> **Sequenced above.** `libheap`/`libos`/`libstream` are now the sequenced **slices
> 4, 5, and 7** (see "Userspace-runtime slices"). The split: `libheap` (slice 4) +
> libos core & libstream (slice 5, SysCaps-independent) land before SysCaps; the
> thread-spawn/authority-facing libos surface (slice 7) lands after. **`librt` was
> cut** — no fiber scheduler (async tasks on the libos executor cover in-process
> concurrency) and no standalone sync-wrapper crate (that's `std::io`, deprecated by
> the eventual std port); see the 2026-07-13 decision log. The checklists live in
> those slices; this section is retained only as the map entry `overview.md` links to.

#### Profile server

- [x] Generic profile server binary (`userspace/profile-server/`; forwarding RS,
  resolve-by-probe, re-exports the store `FileObject`) — store/profile Part D
- [x] Profile manifest format (`[[package]]` table array; host-tested parser) — Part A/D
- [x] System profile manifest in initramfs (transitional) — Part C; store-resident
  manifest (post-bootstrap) still to come with the package manager
- [x] Init binds profile server at `/bin` (RS startup minus device handoff) — Part D.
  `/lib` projection deferred (only `/bin` is exercised this slice)

#### Content-addressed store

- [x] Store layout convention on the ext4 root: `/store/<hash>-<name>-<version>/` — Part C
- [x] Read-only namespace bindings (rights enforce immutability; the root fs is bound
  RO — holds trivially while the fs-server is read-only) — Part A/C
- [ ] Package manager daemon (basic: list, add, remove store paths; manage generations)
- [ ] Generation manifests (only `/system/current-generation` exists today)
- [ ] GC (mark reachable store paths, sweep unreachable)

#### Logging service

- [x] Log channel handle creation (capability-gated) — a client resolves a log path and
  the service mints a per-principal channel; identity is capability-derived (the channel),
  not a SysCap. `docs/architecture/logging.md`, `userspace/logging-service/`
- [x] `LogRecord` structure per architecture doc (`librsproto::log`; trusted/claimed split)
- [x] Logging service collects + stamps records (trusted `principal`/`tier`/`timestamp`/
  `sequence`); indexing + persistence deferred (persistence needs fs-server RW)
- [x] Sinks: serial + in-memory ring (behind one `Sink` trait). Persistent DB (fs-server
  RW) + network (netstack) deferred behind the same trait; ring read-back deferred

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

Design + staging: `docs/architecture/session-and-auth.md` (written 2026-07-20). Four
forks taken at full fidelity: hand-rolled password KDF; **true subtree-scoped** home
isolation (a kernel primitive); service-mgr **spawns** session-mgr with re-delegated
`BIND_NAMESPACE`; a **separate** auth-service. Staged Parts A–E:

- [x] **Part A — `libcrypto` + design doc** (`phase-3/auth-session`): SHA-256 + HMAC +
  PBKDF2-HMAC-SHA256 + `password`/`ct_eq`, `no_std`/`core`-only/no-deps, verified vs
  NIST/RFC 4231/RFC 7914 vectors (17 host tests). Pure `core` so `xtask` links it to
  seed image hashes. Wired into `xtask test`.
- [x] **Part B — subtree-scoped namespace binding** (kernel): a `SubtreeBase` (base
  path) on a `UserspaceServer` binding (`base + suffix` forwarded, leading `/`
  stripped), `..`/`.`-rejecting; `sys_ns_bind` gained `base_ptr`/`base_len` (a4/a5,
  backward-compatible). Host-tested (`from_path`, `resolve` carries base,
  `join_subtree`); boot unaffected. **Multi-binding to one server** (a finding here,
  then resolved with the user): exposing one server through several bindings **shares
  its registration** (bind-mount semantics — one connection, many names) rather than a
  per-binding channel; the pending slot grew N = 1 → a small table (`US_PENDING_MAX`)
  for concurrent in-flight requests. Validated end-to-end under `test-qemu` (init binds
  the fs endpoint a second time as a subtree; a lookup through it resolves correctly).
- [x] **Part C — auth-service + user DB**: the credential-oracle RS speaking the new
  `Auth` rsproto category (`Authenticate` → `AUTHENTICATED{principal,home}`/`DENIED`,
  PBKDF2 verify, dummy-verify on missing user) — wire contract in
  `docs/spec/rsproto-auth-ops.md`. New `librsproto::auth` codec + `auth-service`
  crate (host-tested lib: DB parse + verify + serve; bare-target bin: read
  `/system/users`, Ready-hand a client channel, serve). `passwd`-style `/system/users`
  + `/home/alice` seeded into the ext4 by xtask (one-way verifier only — no secrets
  in-tree). Host-tested; image assembles; boot green. **Spawning/wiring is Part D.**
- [x] **Part D — service-mgr → session-mgr + endpoint plumbing**: init hands the
  retained fs-server endpoint to service-mgr; service-mgr spawns auth-service (RS Ready
  handshake → its client channel) + session-mgr (re-delegated `BIND_NAMESPACE` +
  control channel) and hands session-mgr the fs endpoint + auth channel. session-mgr
  (new bin crate) authenticates the demo user over the auth channel and constructs a
  session namespace binding `/home` as an fs-server subtree (proving `BIND_NAMESPACE` +
  subtree + shared-reg bind-mount). session-mgr fires the `test-harness` verdict.
  Sequenced after the demo chain (a concurrent direct-block + forwarded-lookup hang is
  tracked in `deferred-decisions.md`). Auth is reached over a **direct channel** (not
  bound at `/svc/auth`) since session-mgr is the sole consumer. AHCI concurrent-command
  bug fixed along the way (single-slot queue + DPC-drain).
- [x] **Part E — login + namespace construction + user shell** (the milestone):
  session-mgr authenticates (test-harness auto-login / interactive `nitrox login:` on
  the console), builds the session namespace (`/home` subtree RW + `/dev/console`),
  spawns the new **`usersh`** throwaway shell into it with **empty syscaps**, and reaps
  it. `usersh` `sys_file_create`s `/home/greeting`, writes + syncs + re-reads to verify
  — the fs-RW write path from a sandbox through the subtree binding. eshell demoted to
  emergency-only. Verdict: `test-harness` auto-login → shell home-write; wrong-password
  denied. **The auth + session-mgr slice is complete** — login → per-user namespace →
  user shell → home write runs end to end.

Scope notes (decided 2026-07-17, for when this slice runs):
- **Proper password hashing, if scope allows.** Prefer storing a **password hash** (a hand-
  rolled KDF over a hand-rolled hash — no external crates) rather than the raw password. Beyond
  what's needed to prove the login path, so fold in only if the added scope stays modest; a
  plaintext/trivially-hashed file is the fallback. Note: **audit's chained records need a
  cryptographic hash too** (`2026-07-16` audit design) — a shared hand-written hash primitive
  (SHA-256 / BLAKE2, userspace `no_std`) would serve both; consider building it once.
- **Minimal throwaway user shell.** The real user shell is Phase 4; this slice needs only a
  *very* minimal shell to prove login → per-user namespace → shell → write a file to home.
  Treat it as disposable (reuse/trim `eshell` or a tiny bespoke one) — do not invest in it.

#### fs-server-ext4 read-write + the extent page-cache data path (Model A)

The v5.1 "pure" data path (`os-design-v5.1.md` § File-Backed Memory): the fs-server
becomes a metadata / extent / block-allocation oracle that **never touches file
data**, and the kernel owns the data path end to end. Writeback forces this — to
flush a dirty page the kernel must know its LBAs, which *is* the extent map — so
reads and writes share one extent-based path here, replacing slice 8's Model-B
range-read fill with the **zero-copy extent fill** behind the page cache's
fill-producer seam. (Model B stays the general fallback for non-block / network /
transforming fs-servers, which have no LBA mapping.) See the decision log
(2026-06-25 — page-cache fill model).

- [x] **Model A block read fill** (Part B): the fs-server returns the file's `BlockRun`
  map (delivered inline in the `OBJECT_KIND_FILE_BLOCKS` lazy resolve reply, which also
  transfers the device); the kernel reads each page's block **zero-copy** into the cache
  frame via an async block IRP (`dispatch_block_irp_into_frame`). `MapRange`/`AllocRange`
  are specced (`rsproto-block-ops.md`) as the standalone re-map ops; the initial map rides
  in the resolve reply. Named `BlockRun`, not "extents" — the contract is fs-neutral.
- [x] Write path: block allocation + extent-tree extension + inode update (Part D,
  `ext4::grow_file`, `e2fsck`-verified). Journal interaction deferred (journalless fixtures).
  Overwrite (Part C) is data-only, no metadata write.
- [x] Writeback from kernel page cache (Part C, `FileObject::writeback` + `sys_file_sync`):
  the kernel writes dirty pages to their LBAs via write IRPs; the fs-server allocates blocks
  (on growth) but never touches file data.
- [ ] Filesystem consistency on power loss (journal replay on mount) — deferred to its own
  slice (the fixtures are `^has_journal`; crash consistency is best-effort ordering today).
- [x] File creation (inode allocation + directory-entry insertion) — Part E,
  `ext4::create_file`, `e2fsck`-verified; triggered by create-on-resolve
  (`RESOLVE_CREATE | RESOLVE_GROW` + `sys_file_create` = 33). Group 0 only; new-dir-block
  growth on a full parent directory deferred.
- [ ] Cross-group inode/block allocation, extent-tree splitting / index nodes,
  truncate / delete / rename — deferred (beyond Part E).

### Milestone

`xtask qemu` boots to a "system idle" state with:
- Multiple services running, all supervised by service manager
- A test program started by service manager produces typed (TableWriter-based) output to its log channel
- Two CPUs are visibly active (e.g., scheduler stats accessible via `/proc`)
- A user can log in, get a per-user namespace, and write files to their home directory

### Definition of Done (2026-07-20)

Phase 3 = **"the service-ecosystem machinery is complete and demonstrated,"** not an
exhaustive service catalogue. That machinery is done; a representative service set runs
supervised. DoD is the four milestone clauses above — **two are met** (supervised services;
login → per-user namespace → home write) and **two remain, and are the only gating work:**

- [x] **libstream + a service-mgr-driven typed-log demo** (clause 2 — the last open runtime lib).
  **Done (2026-07-20).** `libstream` (TSM1 wire codec, `TableWriter`/`TableReader`,
  `TypedRecord`); `heartbeat` emits typed beat rows `{seq, uptime_ns, healthy}` to its log
  channel; the logging service detects the `TSM1` magic and renders the decoded table (text
  `LogRecord`s still route to `parse_append`). See the decision log (2026-07-20 "libstream").
- [ ] **`/proc` scheduler-stats surface** (clause 3), pulling forward the *synthesized read-only
  `MemoryObject` snapshot* primitive (also unblocks numeric `/proc/self/status`).
  Building as slice `phase-3/proc-sched-stats`; the primitive is the **capture → format →
  synthesize** discipline (copy `Copy` data under one lock hold; format via `KString` with no
  lock held; wrap in a read-only `MemoryObject` — `try_new_filled` is the existing synthesis
  step, as `/dev/log`/initramfs already exercise):
  - [x] **Part A — counters + capture + format.** Per-CPU `u64` counters in `SchedState`
    (`switches` / `steals` / `placed` / `resched_ipis` / `ticks`), incremented at their event
    sites — all already hold the rank-1 `SCHED` lock, so no atomics; `sched::stats_snapshot()`
    captures them plus instantaneous state (`ready` length, idle-current, online) under one
    hold; the pure `sched::stats::format` renders `cpus_online=N` + one `name=value` row per
    online CPU. Host tests for the formatter; full suite + `test-qemu` green.
  - [x] **Part B — the surface.** `KernelServerId::SchedStats` leaf server at
    `/proc/sched/stats` (the `/dev/log` rights pattern: `MAP_READ` + generic band), bound by
    pid 1 at boot; `scheduler.md` gains § "The stats surface" (counters table + the
    capture → format → synthesize discipline). Host tests (the all-offline snapshot renders
    exactly the header into a fresh `MemoryObject`; leaf suffix rejection); `test-qemu` green.
  - [x] **Part C — `/proc/self/status`.** `KernelServerId::ProcSelfStatus`: `pid=`/`tid=`
    text from the calling syscall context (`sched::current_pid_tid()`, one `SCHED` hold; a
    refcount-free `Thread::has_process` gates kernel/boot callers to *not found*), bound with
    the snapshot-server rights shape. The shared `complete_with_memobj` tail replaces the
    4× duplicated MemoryObject adoption. Closes the deferred numeric-`/proc/self/status`
    entry (`deferred-decisions.md`). Suffix rejection host-tested; success arm is
    QEMU-covered (Part D); full suite + `test-qemu` green.
  - [ ] **Part D — demo + close-out.** QEMU selftest demo parses the snapshot and gates the
    verdict on `switches>0` for ≥2 CPUs; decision-log entry; Phase 3 close-out.

Everything else in the backlog below is **consumer-driven and defers to Phase 4**, landing
with its first consumer (the project's standing deferral discipline). Triage:

- **Defer — blocked or no near consumer:** time-sync (blocked on networking), device manager
  (blocked on the Tier-2 module loader; no loadable-driver need under QEMU), namespace manager
  (premature — supervisors already construct namespaces), mount daemon (no dynamic-mount
  consumer), OOM daemon (no memory-pressure scenario until heavy apps), audit subsystem
  (security; no functional consumer yet — revisit in a hardening pass).
- **Optional early Phase 4:** crash reporter (developer-experience value as userspace grows).
- **When their scale demands it:** service-mgr dependency-graph + RS startup ordering;
  content-store package manager / generations / GC (the package-management + sysadmin layer —
  a Phase 4 north-star component).

**Directory operations** (readdir/mkdir/rmdir) are **not** Phase 3 — no Phase 3 consumer needs
them; they open Phase 4's CLI-complete work, driven by coreutils/the shell. See the decision
log (2026-07-20 "Phase 3 Definition of Done, the `std` stance, and the Phase 4 north star").

### Notes / deviations

- **2026-06-26 — Phase 3 scope analysis + kernel-first sequencing.** Stock-take at
  the start of Phase 3. Phase 3's workstreams sort into three readiness tiers:
  *ready to build* (libstream — `typed-stream-format.md`; service-mgr schema —
  `service-toml-schema.md`), *partially sketched* (scheduler, SMP, libos, profile
  server, content store, fs-server RW/Model-A, auth+session), and *just a checkbox*
  (librt fiber scheduler, logging, audit, OOM/mount/crash-reporter/namespace-mgr/
  device-mgr daemons). ~8 architecture docs referenced by `overview.md` don't exist
  yet; they're written per-workstream as we reach each. The **kernel-first** work
  (scheduler + SMP) was sequenced into slices 0–3 (above), adding the missing
  **slice 0** (per-CPU foundation + the scheduler/SMP design doc) and choosing an
  **incremental SMP rollout** (APs on the shared runqueue first; per-CPU runqueues in
  slice 3). Full analysis in the decision log entry of 2026-06-26.

---

## Phase 4+: A usable windowed desktop (and beyond)

**Goal:** move from toy demos to an OS that looks and behaves like a production system
from a user's perspective. The phase distinction breaks down here — this is ongoing
development rather than discrete phases.

**North star (scoped now): a usable windowed desktop.** A compositor on the boot
framebuffer, one shared GUI toolkit, and three flagship apps — a **GUI terminal**, a **GUI
file browser**, and a **GUI text editor** (MVP = compositor + toolkit + GUI terminal). It is
the common denominator of the whole GUI vision: the browser and every later app are "another
window on this compositor + toolkit." **Subsequent north stars** (a web browser; networking;
a package-management + sysadmin layer) reuse this foundation. See the decision log
(2026-07-20 "Phase 3 Definition of Done, the `std` stance, and the Phase 4 north star") for
the full rationale, including the `std` stance and the browser strategy.

**Stepping-stone path** (each a real, satisfying milestone; roughly ordered):

1. Phase 3 close (libstream + `/proc`) — the gate out of Phase 3.
2. FP/AVX2 + XSAVE (below).
3. dir ops + typed shell + coreutils → **CLI-complete**.
4. framebuffer display server + input routing.
5. compositor + minimal shared toolkit.
6. **GUI terminal** (hosts the shell) — the "looks like an OS" moment.
7. GUI file browser + GUI text editor → a usable desktop.

The **full std cluster** runs as a parallel, consumer-driven track (below) — not a
desktop-MVP gate.

### Floating-point + SIMD (early enablement)

The kernel saves **zero** FPU state today (soft-float everywhere). Real Rust programs and the
ecosystem use hardware float/SIMD. This is the one std-adjacent prerequisite that lands
*early*, ahead of any graphics: it also unblocks a pile of `no_std + alloc` ecosystem crates
(font rasterizers, image codecs) the toolkit will want.

- [ ] Enable SSE/AVX (up to AVX2) for userspace; per-thread `XSAVE`/`XRSTOR` in both switch paths
- [ ] Lazy vs. eager FPU-state policy decided; context-switch cost measured
- [ ] First hard-float userspace thread demonstrated

### CLI-complete: dir ops + the typed shell + coreutils

The first stepping stone and the substrate the GUI terminal will host. Directory operations
land here (their consumer is the shell/coreutils — not Phase 3).

- [ ] Filesystem directory ops: `readdir`, `mkdir`, `rmdir`, unlink/rename (fs-server + rsproto + syscall surface)
- [ ] Basic interactive shell (Rust REPL evolving into the eventual shell over time)
- [ ] Pipeline support (typed streams between processes, over libstream)
- [ ] Built-in operators: sort, filter, take, count, select
- [ ] The `display` verb (renders typed streams to terminal as ANSI tables)
- [ ] Text fallback for processes that emit plain text (auto-wrap as `Table<String>`)
- [ ] A handful of coreutils (`ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, …) — typed-on-Nitrox where structure helps
- [ ] Shell grammar (deferred until shell implementation — see [docs/rationale/deferred-decisions.md])

### Display + input

- [ ] Display server over the persisted **boot framebuffer** Limine hands us (GOP-style, no modesetting — GPUs are too opaque to modeset blind; firmware-fixed resolution, one linear framebuffer, no acceleration)
- [ ] Input routing: keyboard + mouse (PS/2 under QEMU; USB HID later — see below)
- [ ] Font rasterization (a `no_std`-friendly Rust crate, e.g. `fontdue`/`ab_glyph`) + a text/ANSI render path

### Compositor + shared GUI toolkit

- [ ] Compositor (userspace server): windows/surfaces, stacking, focus, damage/redraw
- [ ] Shared GUI toolkit (the "common GUI library"): window creation, an event loop, drawing primitives, basic widgets. **Conventional surface model first** (apps draw into a surface; the compositor composites — Wayland-shaped)
- [ ] `WidgetRecord` model layered on top **later, as the typed opt-in** (programs emit structured UI over a typed stream; the display server renders — the text-floor/typed-stream duality on the screen). The first desktop is **not** gated on this research bet.

### Desktop apps (the north-star MVP)

- [ ] **GUI terminal** (hosts the shell) — the MVP flagship
- [ ] **GUI file browser**
- [ ] **GUI text editor**

### The full std cluster (parallel, consumer-driven)

Not a desktop-MVP gate — the desktop can be built on `no_std + alloc` + crates + FP. Full std
lands with **portable application programs** and the **browser**. `std` is the portable API
for application code; libos/libstream stay the capability-native API for system code. It sits
on the native ABI (no kernel change): `std::fs` resolves paths through the process's root
namespace (bounded ambient, capability-safe); `std::io` blocking maps to `sys_io_submit` +
`block_on`. See the decision log (2026-07-20 std stance; supersedes 2026-07-13).

- [ ] Thread-local storage (`FS_BASE` / `sys_thread_set_tls`)
- [ ] Real `std::thread` — multi-threaded user processes; this triggers the slice-3b **cross-CPU deschedule IPI** (its first consumer) + per-thread FPU/TLS
- [ ] `std` subset over the native ABI: `std::{fs,io,sync,thread}` (`net` after networking)
- [ ] Target spec: `x86_64-unknown-nitrox.json`
- [ ] First non-trivial external Rust crate ported unmodified; a Nitrox program cross-built + run on Linux (portability proof)

### Subsequent north stars

**Web browser** (a capstone / integration test — exercises networking, TLS, threads, FP/SIMD,
graphics, fonts, memory, std at once). Favor a **hybrid**: reuse pure-Rust Servo crates
(`html5ever`, `cssparser`, `selectors`) + a pure-Rust JS engine (`Boa`, restricted subset)
over porting full Servo (SpiderMonkey/C/GPU weight, which would force the POSIX C shim early).
Portable to Nitrox/Linux/Windows.

- [ ] Restricted HTML/CSS/JS engine on pure-Rust crates
- [ ] `rustls`-based HTTPS (needs networking below)

**Networking** (gates `std::net`, NTP, the browser's fetch path):

- [ ] Network driver (e1000 or virtio-net as starting point)
- [ ] Userspace netstack server (smoltcp port or from-scratch)
- [ ] Socket-as-namespace-resource architecture
- [ ] DHCP, DNS
- [ ] TLS-the-protocol via `rustls` + a Rust crypto provider

**Package management + system administration** (the content-store daemon + generations + GC,
pulled up from the Phase 3 backlog; the "sysadmin layer" of a production-feel OS):

- [ ] Package manager daemon (list/add/remove store paths)
- [ ] Generation manifests + atomic switch/rollback
- [ ] Store GC (mark reachable, sweep unreachable)

### Opportunistic / trigger-driven

Landed when a concrete consumer or need appears, not on a fixed schedule:

- [ ] **USB subsystem** (xHCI + USB core + HID) — real-hardware input/storage; QEMU gives PS/2, so it trails the QEMU-first loop
- [ ] **Dynamic linking** — off the std critical path (Rust static-links); an ecosystem/image-size concern
- [ ] **POSIX C shim** — deferred until a must-have C dependency forces it (target the pure-Rust ecosystem first)
- [ ] **Additional filesystems:** fs-server-fat read-write (ESP updates from within the OS; also the orphaned Phase-2 "FAT read-only" deferral folds in here), btrfs/xfs if a use case emerges
- [ ] **Phase 2 ACPI:** vendor ACPICA (`kernel/vendor/acpica/`), OSL (`kernel/src/kacpi/osl/`), `bindgen` integration, power-management daemon — triggered by laptop / graceful-shutdown needs
- [ ] **GPU / compositor acceleration** — modesetting GPU driver is out of scope (opacity); the boot framebuffer is the display substrate
- [ ] **aarch64:** fill `kernel/src/arch/aarch64/` stubs once x86_64 is mature; equivalent userspace work

### Notes

This phase is open-ended. The implementation plan stops being useful as a fine-grained
tracking tool around here; ongoing work is better tracked as GitHub issues / project boards.
The north star and the decision log (2026-07-20) are the durable guides.

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
