# Why Rust

Nitrox is implemented in Rust throughout — kernel, drivers, services, runtime libraries, and the shell. This document explains that choice. The relevant context is that Nitrox follows Latte, an earlier Unix-like hobby OS in C; the language decision was a deliberate departure based on direct experience with the alternative.

## What Latte taught

Latte was implemented in C using GCC, NASM, Make, and GRUB/Multiboot. C is the historical default for OS work, and the choice was reasonable for that project. It was also the source of the most significant ongoing friction.

The friction wasn't with C as a language so much as with what C lets through. Out-of-bounds array writes that corrupted unrelated kernel structures and produced symptoms hours later. Use-after-free in IRP completion paths where the lifetime relationship between an IRP and its initiating process wasn't visible in the type system. Integer overflow in size calculations producing tiny allocations that subsequent code happily wrote past. Functions with three pointer arguments where the documentation said "buffer must outlive the call" and no compiler could verify the claim. Each of these is recoverable with discipline; their accumulation across a multi-year project is not.

The other Latte lesson was about the complexity tax of multi-language assembly. C plus NASM plus the Multiboot conventions plus the GRUB build dance plus inline asm conventions plus C macro hygiene — every one of these surfaces required attention, and changes that crossed boundaries were proportionally more painful. A modern OS toolchain wants fewer such boundaries.

## Why Rust

The case for Rust on a project like this is dense. Several things matter, each independently.

**The borrow checker forbids the most expensive bugs.** Use-after-free, double-free, data races, iterator invalidation — the entire class is rejected at compile time rather than discovered through runtime symptoms. For a hobby project where there's no team to do thorough code review, the compiler doing this work is structurally important. The bug you don't write is the bug you don't have to debug.

**The capability and handle system maps onto Rust's type system unusually well.** A typed `Handle<T, M>` where `T` is the kernel object type and `M` is a typestate marker encoding the principal access mode lets the compiler enforce things that other languages would have to enforce at runtime. Calling `read()` on a `Handle<File, WriteOnly>` becomes a compile error. Attenuation is a method that consumes the original and returns a more restricted variant. Handle transfer can be expressed as ownership transfer. The system's central abstraction is directly representable in the language; the rest of the architecture follows from this fit.

**`#![no_std]` and `#[no_std]` ecosystems are mature.** Rust supports kernel and embedded development without runtime support. The `core` crate is freestanding. The `alloc` crate enables heap-using code in environments with custom allocators. The `compiler_builtins` crate provides the low-level primitives. None of this is afterthought-quality — Rust was designed with this use case in mind, and the tooling reflects that.

**Cross-compilation is a first-class concern.** A custom target JSON file describes the kernel's ABI to LLVM. `cargo build --target x86_64-nitrox.json` produces a kernel ELF for the target architecture. Future aarch64 support is a target file change, not a toolchain rebuild. Latte required cross-compilation tooling assembled by hand; Rust includes it.

**The `unsafe` boundary is explicit and auditable.** All unsafe operations — raw pointer dereferences, MMIO, inline assembly, manual lifetime tricks — must be marked `unsafe`. The compiler tracks where unsafe code is. The total surface is auditable as a function of the source. In Latte, every line of C was implicitly "unsafe" in the Rust sense; in Nitrox, the unsafe surface is approximately 10-15% of the kernel codebase, mostly the architecture abstraction layer and the user-memory access primitives. The remainder is safe Rust where the compiler enforces invariants.

**Tooling is integrated.** `rustc`, `rust-analyzer`, `clippy`, `rustfmt` work together. Build configuration is `Cargo.toml`. There's no separate makefile system, no autotools, no manual dependency tracking. The development inner loop is `cargo build`; the test inner loop is `cargo test` (host) and `xtask test-qemu` (kernel-side). Latte's build system was an organic accumulation of Make and shell that got harder to maintain as the project grew.

**For AI-assisted development, the compiler validates generated code.** Modern AI tools can produce substantial amounts of code quickly. The question is whether the generated code is correct. With Rust, code that compiles has already passed a strong set of correctness checks — no out-of-bounds accesses, no use-after-free, no data races on shared mutable state. AI-generated C code can compile and still be deeply broken; AI-generated Rust code that compiles is much more likely to actually work. Over a multi-year project where AI assistance is part of the development model, this matters.

## Why not other languages

**C** was the language Latte used. Its limitations are described above.

**C++** offers RAII and templates, both useful, but doesn't solve the lifetime problem. Modern C++ (smart pointers, move semantics, concepts) is closer to Rust in spirit, but the language carries decades of legacy that cannot be turned off — implicit conversions, header-based modularity, exceptions optional, undefined behavior pervasive. The discipline required to write modern C++ correctly in a kernel context is comparable to the discipline required for C; the language doesn't enforce it.

**Zig** is a thoughtful modern systems language with explicit memory management and good control over allocation. It's a credible alternative to Rust for OS work. The reasons it wasn't chosen are practical rather than principled: Rust's ecosystem is more mature, the borrow checker enforces lifetime correctness in ways Zig's allocators don't, and the typed-handle design fits Rust's type system better than Zig's. Zig would have been a defensible choice; Rust was a better one.

**Ada / SPARK** has the strongest formal-verification story of any practical language. For a project that wanted to formally verify kernel correctness, SPARK would be the right answer. Nitrox is not a formal-methods project — the goal is a working hobby OS, not a verified one. The development velocity cost of SPARK's verification regime exceeds what's justifiable for this scope.

**Go** has good concurrency primitives and a friendly type system, but its garbage collector and runtime are incompatible with kernel work. There's no `#![no_std]` equivalent that strips the GC. Go is the wrong tool for this job.

**Managed languages** (Java, C#, etc.) require runtimes that don't exist in kernel environments. There are research projects on managed-runtime kernels (Singularity, Cosmos), but they're research, not practical.

## Stable Rust only

Nitrox uses **stable Rust**. No nightly features. This was a deliberate choice that constrains some aspects of the design.

The most relevant constraint is `Handle<T, M>`. An earlier design used const generics with `Rights` as a const parameter (`Handle<T, const R: Rights>`), which would have required `generic_const_exprs` and `adt_const_params` from nightly. The chosen design uses typestate marker types instead — `M: Mode` where `Mode` is a sealed trait with concrete implementing types like `ReadOnly`, `WriteOnly`, `ReadWrite`. The compile-time enforcement is comparable; the implementation is on stable.

The reasons for the stable-only constraint:

**Stability is more valuable than features.** Nitrox is a multi-year project. Features that rely on nightly may break, change semantics, or be removed before stabilizing. The cost of refactoring across nightly churn is real and accumulates.

**Stable Rust is genuinely capable.** The combination of traits, generics, GATs (now stable), const generics over primitive types, and procedural macros covers nearly everything kernel and runtime work needs. The cases where nightly buys something fundamental are rare.

**Nightly features are expressive but not always understandable.** `generic_const_exprs` is powerful and produces code that is harder to reason about than equivalent typestate-based designs. The typestate approach happens to be both stable-Rust-compatible and clearer to read.

The one acknowledged exception is the future ACPICA integration (Phase 2 ACPI). Integrating C code via FFI involves `unsafe` in greater quantity and at a different boundary than the rest of the kernel; it's a documented exception rather than a general retreat from the stable-Rust discipline.

## Costs accepted

**Compile times.** Rust compiles slower than C. For a kernel, this matters during the inner loop. Mitigations: incremental compilation, `cargo check` for fast feedback, careful crate decomposition so changes affect a minimum of dependents. The cost is real but manageable.

**Learning curve.** Rust is harder to learn than C. The borrow checker takes time to internalize; lifetimes are a real concept that doesn't appear in most other systems languages. For a solo project where the developer is already familiar with Rust, this is a one-time cost. For onboarding new contributors (if that ever becomes relevant), the curve is steeper than for a C project.

**Some idioms are harder.** Self-referential structures, intrusive linked lists, ad-hoc polymorphism — these are awkward in safe Rust. The kernel needs them anyway, and they're implemented with explicit unsafe code in well-documented places. The awkwardness is a feature, not a bug — these are exactly the patterns that produce subtle bugs in C and benefit from being marked unsafe.

**Ecosystem assumes std.** Most Rust crates target hosted environments and assume `std`. The `no_std` ecosystem is real and growing, but smaller. For kernel work, this means writing more from scratch (the SLUB-inspired allocator, intrusive lists, etc., are project-internal `libkern` types rather than crate dependencies). This aligns with the no-external-crates discipline anyway.

## What this looks like in practice

The kernel is `#![no_std]` without `alloc` until the kernel allocator is initialized, then `alloc` becomes available. All kernel data structures are in `libkern`, a project-internal crate. The arch layer in `kernel/src/arch/` contains the unsafe primitives — page table manipulation, MMIO, interrupt handler stubs, context switch. The rest of the kernel is safe Rust calling into the arch layer through trait abstractions.

Userspace runtime libraries are `#![no_std]` with `alloc`. They build on `libkern` (also `#![no_std]`, no `alloc`) and provide progressively richer interfaces. Application code uses the runtime libraries; it doesn't see syscalls directly.

Build is via `cargo` with custom target JSON, `build-std` to compile `core`/`compiler_builtins` from source, and an `xtask` workspace for higher-level build orchestration (assembling disk images, running QEMU, deploying via PXE). Configuration is via Cargo features; `Cargo.toml` is the single source of truth for build options.

The `unsafe` boundary is documented per-subdirectory. `kernel/src/arch/` is mostly unsafe by nature. `kernel/src/handle/` has unsafe at the seqlock and pointer-cast boundaries. Most other subdirectories have very little unsafe — the safe-Rust enforcement of invariants is doing real work. A `kernel/CLAUDE.md` file (and additional per-subdirectory ones as needed) document the unsafe policy for AI-assisted code generation.

## Where to read more

- [`unsafe` policy](../conventions/unsafe-policy.md) — when unsafe is allowed and how it must be documented
- [Code style](../conventions/code-style.md) — Rust conventions for the project
- [Toolchain reference](../reference/toolchain.md) — specific versions and required tooling
