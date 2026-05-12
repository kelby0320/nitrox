# kernel/CLAUDE.md

Kernel workspace constraints. Loaded when Claude Code reads files under `kernel/`.

## Build environment

- **`#![no_std]`** — no Rust standard library
- **`#![no_main]`** — entry point is in `arch/<arch>/boot.asm`, not `main.rs`
- **No `alloc` crate at startup.** `KBox`, `KVec`, etc., become available only after the kernel allocator is initialized in early boot (see `kernel/src/main.rs` initialization sequence). Code in the early-init path cannot allocate; later code can.
- **Custom target**: `x86_64-unknown-none.json` (and aarch64 equivalent). `cargo build-std` compiles `core` and `compiler_builtins` from source.
- **`panic = "abort"`** — no stack unwinding in the kernel.
- **`-mno-sse -mno-mmx -msoft-float`** in target JSON. The kernel does not use FPU; user FPU state is saved/restored on context switch.

## No external crates

The kernel uses no third-party Rust crates. All data structures (`KVec`, `KString`, intrusive linked lists, red-black trees, spin locks, atomics-based queues) are in `kernel/src/libkern/` or equivalent. Don't add `serde`, `bytemuck`, `bitflags` (we use a hand-rolled bitflag pattern), or any other ecosystem crate.

The one planned exception is ACPICA via FFI in Phase 2 of ACPI support. Phase 2 is not yet active. See `docs/rationale/why-phased-acpi.md`. If the time comes, the integration is a documented exception, not a general retreat from the no-external-crates rule.

If you think a library would help, propose it in the decision log first. Don't introduce dependencies silently.

## `unsafe` policy

The kernel uses `unsafe` in well-defined places: the architecture abstraction layer, MMIO accessors, inline assembly, raw pointer ops on hardware-mapped memory, and user-memory access primitives. The remainder of the kernel is safe Rust where the compiler enforces invariants.

Rules:

- Every `unsafe` block has a `// SAFETY:` comment explaining the invariants that make it sound.
- Don't extend `unsafe` to new files without a reason. The total `unsafe` surface is roughly 10-15% of the kernel; adding `unsafe` should require justification.
- Wrap `unsafe` operations in safe abstractions where possible. The arch layer is unsafe internally but exposes safe traits.
- Pointer dereferences in safe code through `UserPtr<T>` / `UserMutPtr<T>` are forbidden. The only way to access user memory is via the copy primitives in `kernel/src/mm/user_access.rs`.

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
- Integration tests run in QEMU via `xtask test-qemu` using the `isa-debug-exit` device. Test harness in `tests/qemu-tests/`.
- Add a test for any non-trivial bug fix.
- Don't write tests that depend on QEMU-specific behavior (memory layout details, timing) unless absolutely necessary.

## Forbidden patterns

- `panic!()` outside of explicitly-unrecoverable error paths. Use `Result<T, KError>` or `Option<T>`.
- `unwrap()` outside of tests or known-impossible cases (with `// unwrap: <reason>` comment).
- Allocating in the page fault handler, IRQ handler, or DPC.
- Holding a spinlock across a function call that might allocate or block.
- Manual `Box::leak` patterns. If you need a `'static` reference, the design is wrong.
- Direct hardware port I/O outside the arch layer.

## Useful pointers

- Architecture: `docs/architecture/overview.md`
- Handle system specifics: `docs/spec/handle-encoding.md`, `docs/architecture/handle-system.md`
- Lock ordering: `kernel/docs/lock-ordering.md`
- ABI version: `docs/spec/abi-version-hash.md`
