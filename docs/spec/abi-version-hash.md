# Kernel ABI Version Hash Specification

This document specifies the kernel ABI version hash — the 32-byte hash baked into the kernel image and required to match in every loadable kernel module. The hash ensures modules are built against an exactly-compatible kernel.

**Status:** Pre-stabilization. Hash inputs are subject to change.

## Purpose

Loadable kernel modules (Tier 2 LKMs) interact with the kernel through:

- The exported symbol table — function signatures the kernel makes available to modules
- The layout of types passed across the boundary — `Irp`, `KObjectHeader`, `Notification` enum discriminants, etc.
- The kernel's internal calling conventions

Any change to these interfaces silently breaks modules built against the previous version. Loading a module with mismatched ABI would produce subtle, hard-to-diagnose corruption — wrong field offsets, mismatched function signatures, type confusion.

The ABI version hash is a build-time fingerprint of every interface element a module depends on. The kernel embeds the hash; modules embed the hash they were built against. The module loader compares; mismatch is a hard rejection.

The hash is intentionally strict: no forward or backward compatibility across versions. Every kernel build produces a new hash if any covered input changed; every module must be rebuilt against the running kernel.

## Hash inputs

The ABI version hash is a SHA-256 of the canonical serialization of the following inputs:

### Exported symbol table

For each exported symbol (declared via `export!` in kernel source):

- Symbol name (UTF-8, length-prefixed)
- Symbol type signature, canonicalized as a string

The canonical signature includes the function's full type signature: argument types, return type, calling convention. Type names use fully-qualified paths (e.g., `kernel::handle::RawHandle`, not just `RawHandle`).

Example exported symbols:
```rust
export!(fn kmalloc(size: usize) -> *mut u8);
export!(fn kfree(ptr: *mut u8));
export!(fn register_irq_handler(vec: u32, handler: fn(IrqCtx)) -> Result<IrqId, KError>);
```

The symbol table is sorted by name for canonical ordering. Order must be stable across builds for the hash to be reproducible.

### Kernel configuration hash

Cargo features that affect the ABI surface, joined and hashed:

- Feature names enabled (sorted)
- Target architecture (`x86_64`, `aarch64`)
- Pointer width (always 64 for supported targets)
- Endianness (always little-endian for supported targets)

Build profile (debug vs release) is **not** included — same ABI either way. Optimization-level differences are not ABI-visible.

### KObjectType enum layout

Every variant of `KObjectType` and its discriminant value:

```
KObjectType::Process              = 0x01
KObjectType::Thread               = 0x02
KObjectType::Namespace            = 0x03
... etc
```

Adding a new variant changes the hash. Removing or renumbering a variant changes the hash.

### Notification enum layout

Every variant of `Notification` and its discriminant value, plus the byte offsets and types of fields within each variant. Adding a new variant changes the hash; the wire-format compatibility (Unknown fallback) protects userspace, but kernel modules need exact match.

### IoOp and IoResult layouts

Field offsets, types, and sizes of:
- `IoOp`
- `IoResult`
- `IoOpcode` enum

### KError enum layout

Every variant and its discriminant value.

### Rights bitflag values

The bit positions assigned to each named right (`READ`, `WRITE`, `LOOKUP`, etc.).

### KObjectHeader layout

Field offsets, types, sizes, and alignment.

### IRP layout

Field offsets, types, sizes of `Irp` and its key sub-types (`IrpStack`, `IrpStatus`, etc.).

### Architecture-specific types

Selected types from the arch layer that modules might encounter:

- Architecture page table flags type
- Interrupt context type passed to IRQ handlers (`IrqCtx`)
- Architecture-specific register save state if exposed to modules

Types that are kernel-internal and never crossed by modules are not in the hash.

## Computation

The hash is computed at kernel build time by `build.rs`. The procedure:

1. Compile the kernel to extract symbol declarations and type layouts (using `rustc --print=type-sizes` and similar mechanisms, plus Rust's stable ABI introspection where available).
2. Canonicalize each input per the rules above.
3. Concatenate the canonical serialization in a fixed order (alphabetical by category name).
4. SHA-256 the result.
5. The 32-byte hash is written to `kernel/build/abi-version.bin` and embedded in the kernel ELF as a `.kernel_abi_version` section.

The hash is also written to `kernel/target/<profile>/abi-version.hex` for use by module build scripts.

```rust
// In kernel/src/main.rs (built by build.rs)
#[link_section = ".kernel_abi_version"]
#[no_mangle]
pub static KERNEL_ABI_VERSION: [u8; 32] = include_bytes!("../build/abi-version.bin").clone();
```

## Module embedding

Each loadable kernel module embeds the kernel ABI version it was built against:

```rust
// In a module's main.rs
#[link_section = ".module_abi_version"]
#[no_mangle]
pub static MODULE_ABI_VERSION: [u8; 32] = *include_bytes!(concat!(
    env!("KERNEL_BUILD_DIR"), "/abi-version.bin"
));
```

The module's `Cargo.toml` references the kernel build directory; building a module requires the kernel to have been built first (and unchanged since).

## Loading check

When the module loader (a userspace service requiring `SysCaps::LOAD_MODULE`) attempts to load a module:

1. Open the module ELF.
2. Locate the `.module_abi_version` section.
3. Read 32 bytes.
4. Compare to `KERNEL_ABI_VERSION` in the running kernel.
5. If unequal: refuse to load. Log: "Module <name> built against ABI <module_hash>, kernel is <kernel_hash>." Module owner must rebuild.
6. If equal: proceed with ELF parsing, relocation, etc.

The check is a single `memcmp(... 32);`. There is no version negotiation, no compatibility table, no range matching. Either the hashes match exactly or the module is rejected.

## What changes the hash

Examples of changes that **change** the ABI hash:

- Adding a new exported symbol
- Changing the signature of an existing exported symbol
- Adding a field to `KObjectHeader`
- Adding a variant to `Notification`
- Changing the discriminant value of an existing `KObjectType` variant
- Renumbering a `Rights` bit
- Adding a Cargo feature that affects exported types
- Switching to a different `target.json` (different architecture, etc.)

Examples of changes that **don't** change the hash:

- Modifying the implementation of an exported function (signature unchanged)
- Adding a kernel-internal type not exposed to modules
- Changing optimization level
- Switching debug/release profile
- Renaming a kernel-internal function not declared with `export!`
- Adding inline assembly inside a function body

## Tooling

`xtask abi-show`: prints the kernel's current ABI version hash.

`xtask abi-diff <other_kernel_build_dir>`: compares ABI versions of two kernel builds and prints a diff of which inputs changed.

`xtask abi-explain`: lists all the inputs to the current hash, useful for understanding what's in the contract.

## Limitations

**Build reproducibility.** The hash depends on rustc producing deterministic type layout and symbol metadata. Rust does not currently guarantee stable ABI across compiler versions. In practice, hashes are stable across rebuilds with the same rustc version; rustc upgrades may change hashes even without source changes. This is acceptable — a rustc upgrade is itself a reason to rebuild modules.

**No partial compatibility.** A module that uses only one exported symbol still has the full ABI version in its hash. A change to an unrelated symbol invalidates the module. This is intentional — the strict-equality model is simpler and avoids subtle compatibility bugs at the cost of forced rebuilds.

**Not a security mechanism.** The ABI hash prevents accidental incompatibility. It does not prevent a malicious party from building a module against the current ABI hash with malicious code. Kernel module loading is gated by `SysCaps::LOAD_MODULE`; that's the security boundary.

## Where to read more

- [LKM infrastructure architecture](../architecture/drivers-and-irps.md)
- [Why no signing initially](../rationale/deferred-decisions.md)
