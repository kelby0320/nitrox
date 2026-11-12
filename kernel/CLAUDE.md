# kernel/CLAUDE.md

Kernel workspace constraints. Loaded when Claude Code reads files under `kernel/`.

## Build environment

- **`#![no_std]`** — no Rust standard library
- **`#![no_main]`** — the ELF entry point (currently `_start` in `main.rs`, eventually an arch-specific stub) is the kernel's first instruction; there is no Rust `main`.
- **No `alloc` crate at startup.** `KBox`, `KVec`, etc., become available only after the kernel allocator is initialized in early boot (see `kernel/src/main.rs` initialization sequence). Code in the early-init path cannot allocate; later code can.
- **Target**: the built-in `x86_64-unknown-none` rustc target. It already implies soft-float, no MMX/SSE, and no red zone — exactly the semantics the kernel needs. Staying on a built-in target keeps us on stable Rust without `-Z build-std`, which is nightly-only. If we ever need a feature the built-in spec doesn't expose, we switch to a custom JSON at that point — but not before. **The kernel stays here permanently**; the custom `x86_64-unknown-nitrox.json` that Phase-4 hard float requires is a *userspace-only* target (see `docs/decision-log.md` 2026-07-21 floating-point). The aarch64 equivalent is `aarch64-unknown-none`. (Original decision: 2026-05-13.)
- **`panic = "abort"`** — no stack unwinding in the kernel.
- The kernel itself is soft-float and executes no x87/SSE/AVX instruction — every `f64` lowers to a `compiler_builtins` call. But the **FP/SIMD register file belongs to userspace**, and the kernel owns keeping one thread's registers from leaking into another's. As of Phase 4 (2026-07-21) it does: `arch::fpu_init_cpu` enables the units per-CPU (BSP in `main.rs`, each AP in `ap_cpu_init`), every schedulable `Thread` carries an `ArchFpuState` save area, and `sched::switch_into` swaps it **eagerly** on each context switch — `XSAVE`/`XRSTOR` when CPUID advertises it, `FXSAVE`/`FXRSTOR` otherwise. Eager, not lazy (`CR0.TS`), specifically to avoid CVE-2018-3665-style cross-process leakage; see `kernel/src/arch/x86_64/fpu.rs`. AVX-512 is deliberately not enabled (area-size cost for an SSE2 userspace baseline).

## No external crates

The kernel uses no third-party Rust crates. All data structures (`KVec`, `KString`, intrusive linked lists, red-black trees, spin locks, atomics-based queues) are in `kernel/src/libkern/` or equivalent. Don't add `serde`, `bytemuck`, `bitflags` (we use a hand-rolled bitflag pattern), or any other ecosystem crate.

This applies to bootloader integration too: the Limine boot protocol bindings are hand-rolled `#[repr(C)]` types in `kernel/src/limine.rs`, not the `limine` crate from crates.io. Pin the protocol revision in source and re-validate against `limine-bootloader/limine-protocol` when bumping it.

The one planned exception is ACPICA via FFI in Phase 2 of ACPI support. Phase 2 is not yet active. See `docs/rationale/why-phased-acpi.md`. If the time comes, the integration is a documented exception, not a general retreat from the no-external-crates rule.

If you think a library would help, propose it in the decision log first. Don't introduce dependencies silently.

## `unsafe` policy

The kernel uses `unsafe` in well-defined places: the architecture abstraction layer, MMIO accessors, inline assembly, raw pointer ops on hardware-mapped memory, and user-memory access primitives. The remainder of the kernel is safe Rust where the compiler enforces invariants.

Rules:

- Every `unsafe` block has a `// SAFETY:` comment explaining the invariants that make it sound.
- Don't extend `unsafe` to new files without a reason. The total `unsafe` surface is roughly 10-15% of the kernel; adding `unsafe` should require justification. `cargo xtask check-unsafe` holds each module's count of each kind of `unsafe` (blocks, `unsafe fn`s and `unsafe impl`s, `#[unsafe(..)]` attributes, `unsafe extern` blocks and `static mut`s) to `tools/xtask/unsafe-counts.toml`, and a raise is made by hand in the commit that adds the `unsafe`, saying why.
- Wrap `unsafe` operations in safe abstractions where possible. The arch layer is unsafe internally but exposes safe traits.
- Pointer dereferences in safe code through `UserPtr<T>` / `UserMutPtr<T>` are forbidden. The only way to access user memory is via the copy primitives in `kernel/src/mm/user_access.rs`.
- A frame of RAM is owned by a `Frame`, and a block of them by a `FrameBlock` (`kernel/src/mm/frame.rs`), freed as it drops: the buddy's frees are `unsafe` and `mm`'s alone, never a call by hand. A page table that maps a frame takes it by `Frame::into_pte`, as an address space does its root and a `VmapFrames` its frames, so no view reaches a frame the kernel reaches another way; an unmap takes an anonymous page's back by the `unsafe` `Frame::from_pte`, dropping it after its shootdown.
- Page tables are owned (`kernel/src/mm/page_tables.rs`): the boot root by `KernelTables`, an address space's by `UserTables`, behind its lock. A root is a bare address only where a CPU loads it, which stays `unsafe`; an owned frame is mapped by the safe `UserTables::map_owned`, and a user half refuses any address at or past `USER_MAP_END`; no map links a kernel-half top-level table, which every root shares.
- Per-CPU state is reached under a pin (`kernel/src/arch/percpu.rs`): a `CpuPin`, lent by an `IrqsOff`, an `IrqSpinLockGuard`, a `PreemptOff` or the scheduler's held state, under which `CpuIndex::current` reads the CPU's index. The arch layer's arrays are `PerCpu`s, reached by raw slot; a shared value per CPU is a `cpu_local!`. Only `leave_online` reads the index unpinned.
- A frame's bytes are reached through a `FrameView` its owner lends (`kernel/src/mm/frame.rs`): `MemoryObject::copy_in` and `copy_out`, a `FileObject`'s cache pages, `AddressSpace::copy_in`, and the RAM disk's copies of an I/O request's frames. Every access is an aligned word atomic, never a reference, as a user mapping may write the frame meanwhile. Memory no owner lends (a firmware table, a page table, a DMA buffer's memory) is reached at `mm::frame::hhdm`, in its caller's `unsafe`; the slab reaches its own pages through its `BuddyPager`.

## Lock ordering

The kernel has a documented lock ordering rank. Violating it is a deadlock. See `kernel/docs/lock-ordering.md` (also referenced from architecture docs).

Rough rank (top to bottom acquisition):

1. Scheduler runqueue lock
2. Wait queue lock
3. Handle table segment alloc_lock
4. Kernel object internal locks (VMA tree, namespace binding tree, etc.)
5. IPC channel lock
6. Allocator locks

Debug builds track acquisition order and panic on violations. If you need to take locks in an order that conflicts with this rank, that's an architectural change — propose it in the decision log first.

## Kernel object dispatch

Kernel objects are dispatched via `match` on `KObjectType`, not via `dyn Trait`. Reasons in `docs/spec/handle-encoding.md` and the architecture overview:

- 8-byte type-erased pointer (vs. 16-byte fat pointer) keeps `HandleEntry` cacheline-sized
- Exhaustive match enforcement on every dispatch site
- Better inlining

Don't introduce trait objects for kernel object operations. Per-type traits are fine for clarity within a type's implementation; cross-type dispatch is via the `KObjectType` discriminant.

Reach a concrete type through `kernel/src/object/header.rs`, never a cast: a new object is a `Ref::new` or `ObjectRef::new` of its box, a handle is looked up with `lookup_as::<T>`, and an `ObjectRef` of any kind becomes a `T` by `downcast`/`downcast_ref`, which read the tag the type fixed in the header. A field or parameter that only ever holds one kind is a `Ref<T>`. A new object type is a `kobject!`, and a `Shared` impl argues why a `&T` is sound. A handle is made with `HandleTable::install`, which hands the reference back on a refusal; a syscall that installs more than one, or installs one before a step that can fail, holds them in an `Installed<N>`, never a rollback by hand. A scheduler entry point takes its object borrowed, a `&T` (a `&Ref<Thread>` for a thread), the borrow its pin; only `complete_pending_op_at` takes an address, for a completer whose pin is its waiter's, which may let go once it sees the completion, before a borrow held for the call would end. What `SCHED` guards in an object is a `SchedCell`, borrowed with the token the held lock keeps; a thread's parked context, which the switch writes with the lock let go, is reached only by address, under its `on_cpu` guard. An uncounted pointer between objects is a `Weak<T>`, kept behind the lock its target's closing takes, and a target unlinks itself in `KObject::closing`, which runs before its box is rebuilt and takes a `Closing` only `dispatch_destroy` makes; such a type is made as a `Ref`, as a box's drop would skip it.

## ABI hash awareness

Changes to the kernel's exported symbol surface (anything declared with `export!`), to ABI-critical type layouts (`KObjectHeader`, `Notification`, `Irp`, etc.), or to enum discriminant values invalidate the kernel ABI version hash. This forces all loadable kernel modules to be rebuilt. See `docs/spec/abi-version-hash.md` for what's in the hash.

This is intentional — strict equality matching prevents subtle ABI mismatches. But it means trivial-looking changes can have project-wide implications. When making such changes, mention the ABI hash impact in the commit message.

## Code style specifics

- Prefer `KVec` over arrays where the size is dynamic. Static `[T; N]` is fine for fixed-size tables.
- Use `AtomicU32`/`AtomicU64` for any field accessed lock-free. Don't claim a field is "atomic" without using the atomic types.
- Inline assembly via `asm!` macro. Document each register's purpose in comments.
- Hardware register reads/writes via wrapper functions in `kernel/src/arch/<arch>/regs.rs`, not raw `asm!` calls scattered through the codebase.
- Comments link to relevant architecture or spec docs by relative path: `// see docs/architecture/handle-system.md` or `// per docs/spec/handle-encoding.md §...`

## Testing

- Unit tests for any logic that doesn't need the kernel runtime go in standard `#[cfg(test)]` modules; run via `cargo xtask test`. The handle table, the namespace resolution algorithm, ABI type encoding/decoding are all testable host-side.
- Integration tests run in QEMU via `cargo xtask test-qemu`: it boots the `test-harness` build headless and adjudicates the whole boot (kernel → init → mount → userspace demos) from QEMU's `isa-debug-exit` exit code. Today the self-test payload (`src/boot_selftest.rs` under the kernel's `selftest` feature, plus the `test-harness` crate's demo chain and `boot-probe`, started from the test image's declarations) *is* the test suite; a per-case framework under `tests/qemu-tests/` remains deferred (see `docs/conventions/qemu-integration-tests.md`). A kernel `panic!` fails the run (the panic handler writes the fail verdict under `test-harness`).
- `cargo xtask miri` runs the unit tests under Miri, nightly in CI. A test it cannot run or finish says why beside itself: `#[cfg_attr(miri, ignore = "...")]`.
- Add a test for any non-trivial bug fix.
- Don't write tests that depend on QEMU-specific behavior (memory layout details, timing) unless absolutely necessary.

## Forbidden patterns

- `panic!()` outside of explicitly-unrecoverable error paths. Use `Result<T, KError>` or `Option<T>`.
- `unwrap()` outside of tests or known-impossible cases (with `// unwrap: <reason>` comment).
- Allocating in the IRQ handler or DPC — and **freeing counts as allocating**:
  an `ObjectRef` drop can reach the allocator locks, so no drops there (or under
  `SCHED`, which debug builds assert on every drop) either. A `SCHED` hold lets go of
  references through a `Deferred<N>` bag (`kernel/src/sched/deferred.rs`), dropped after the
  lock; an interrupt's bag parks them in `deferred_drops`. Two documented
  exceptions, both reached only from ring 3, where nothing is held: the `#PF`
  fault-in path (`map_page` may allocate intermediate page-table frames —
  `kernel/docs/lock-ordering.md` § The AddressSpace lock), and a thread ending
  itself at an interrupt's return to ring 3 (§ A thread ends itself).
- Holding a spinlock across a function call that might allocate or block.
- Manual `Box::leak` patterns. State that lives as long as the kernel is held by an owner that
  never frees it: a Tier 1 controller's by a `libkern::Published`, which takes its box once, and
  an object's by a reference kept to it, as a disk's counters keep their node. Short of those, if
  you need a `'static` reference, the design is wrong.
- Direct hardware port I/O outside the arch layer.
- Referencing architecture internals (`arch::x86_64::*`) from code outside
  `kernel/src/arch/`. The `x86_64` submodule is private; reach the arch
  layer only through the neutral `crate::arch` interface, and don't surface
  arch jargon (`gdt`, `cr3`, MSR names, …) in neutral names. Enforced by
  `cargo xtask check-arch`. See `docs/conventions/arch-boundary.md`.

## Useful pointers

- Architecture: `docs/architecture/overview.md`
- Handle system specifics: `docs/spec/handle-encoding.md`, `docs/architecture/handle-system.md`
- Lock ordering: `kernel/docs/lock-ordering.md`
- ABI version: `docs/spec/abi-version-hash.md`
