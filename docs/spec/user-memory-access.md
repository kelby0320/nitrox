# User Memory Access Specification

This document specifies the contract between kernel code that needs to read or write user memory and the user-memory-access subsystem (`kernel/src/mm/user_access.rs`). It covers the opaque pointer types, the copy primitives, the exception table mechanism, and the user-access protection discipline.

**Status:** Pre-stabilization. Subject to change before v1.0 ABI freeze.

## Goal

The kernel must never:

- dereference a user-supplied address directly,
- fetch instructions from a user page,
- silently accept a faulting access as kernel state.

Every read from or write to user memory goes through this subsystem. Faults during a copy turn into [`UserAccessError::Fault`]; faults outside a copy halt the kernel.

## The opaque pointer types

```rust
#[repr(transparent)]
pub struct UserPtr<T>    { /* opaque u64 + phantom T */ }
#[repr(transparent)]
pub struct UserMutPtr<T> { /* opaque u64 + phantom T */ }
```

- Constructed from a raw `u64` (typically a syscall argument) via `UserPtr::new(addr)` / `UserMutPtr::new(addr)`. Validation: `addr < USER_VIRT_END` and `addr` aligned for `T`. Failure returns `Err(UserAccessError::BadAddress)` or `Err(UserAccessError::Misaligned)`.
- No `Deref`, no `read`, no `write`, no `as_ptr`. The held `u64` is `pub(crate)` only.
- The kernel-wide rule (also in [kernel/CLAUDE.md](../../kernel/CLAUDE.md#L36)): the *only* sanctioned path from `UserPtr<T>` to bytes in memory is one of the copy primitives below.

## Copy primitives

| Function | Direction | Signature |
|---|---|---|
| `copy_from_user<T: Copy>` | user → kernel | `(src: UserPtr<T>) -> Result<T, UserAccessError>` |
| `copy_to_user<T: Copy>` | kernel → user | `(dst: UserMutPtr<T>, src: &T) -> Result<(), _>` |
| `copy_slice_from_user` | user → kernel | `(dst: &mut [u8], src: UserPtr<u8>) -> Result<(), _>` |
| `copy_slice_to_user` | kernel → user | `(dst: UserMutPtr<u8>, src: &[u8]) -> Result<(), _>` |
| `copy_cstr_from_user` | user → kernel | `(dst: &mut [u8], src: UserPtr<u8>) -> Result<&[u8], _>` |

Every primitive runs three steps in order:

1. **Validate the access.** The user pointer was already checked against `USER_VIRT_END` at construction; the primitive additionally checks that `addr + len` stays inside the user half and that the length arithmetic does not overflow. Failures: `BadAddress`.
2. **Open the user-access window, perform the copy, close the window.** The window is bracketed by the platform's "permit kernel access to user pages" instruction pair (`stac` / `clac` on x86_64 with SMAP; `msr PAN, 0` / `msr PAN, 1` on aarch64 with PAN). The copy itself uses the platform's bulk-byte or load/store primitive (`rep movsb` for single-T and slice copies; a `lodsb`/`stosb` loop for the cstr variant on x86_64).
3. **Convert any `#PF` inside the window into `Err(UserAccessError::Fault)`** via the exception table.

The boundary between steps is enforced by inline assembly. The window-opening / -closing instructions and the `.user_access_table` section emission live nowhere outside the arch's user-access module (`kernel/src/arch/<arch>/user_access.rs`).

### Error variants

```rust
pub enum UserAccessError {
    BadAddress,    // addr or addr+len outside [0, USER_VIRT_END), or arithmetic overflow
    Misaligned,    // addr not aligned for T
    Fault,         // #PF during the copy — user page unmapped, protected, etc.
    NoTerminator,  // copy_cstr_from_user filled dst without finding a NUL
}
```

`copy_cstr_from_user` returns `Ok(&dst[..k])` where the `k`th byte is the NUL terminator; the caller can index `..k-1` to get the string body without the terminator.

### Partial completion

`rep movsb` may make progress before faulting. Specifically:

- `copy_from_user` / `copy_slice_from_user`: on `Fault`, the kernel-side destination may hold a partial copy. The caller observes `Err` and discards the destination.
- `copy_to_user` / `copy_slice_to_user`: on `Fault`, the user-side destination may hold a partial copy. The kernel cannot undo the write; the user observes whatever bytes landed before the fault. Callers that need atomicity must layer their own check (or accept partial writes — for syscall returns, "argument validation failed" usually means the syscall did not happen, even if a partial write occurred).
- `copy_cstr_from_user`: on `Fault`, the kernel-side destination may hold a partial copy. The returned `Err` carries no length.

## Exception table

### Layout

A linker-bracketed section `.user_access_table` holds 16-byte entries:

```rust
#[repr(C)]
struct ExtableEntry {
    fault_pc: u64,
    recovery_pc: u64,
}
```

The section is in the kernel's rodata segment. Linker symbols `__start_user_access_table` and `__stop_user_access_table` bracket the entries; if no entries are registered (Phase 0, slice 1 of Phase 1, or any aarch64 port before its user-access code lands), the symbols are equal and the table is empty.

### Encoding

Each entry holds two absolute 64-bit kernel-text PCs. No relative offsets — Nitrox has no KASLR, so the per-entry cost difference (16 vs 8 bytes) is not worth the relocation-arithmetic complexity. If KASLR is ever added the encoding becomes 32-bit relative offsets, matching Linux's `_ASM_EXTABLE_UA` layout.

### Registration

Inline asm inside the copy primitives emits each entry via:

```asm
.pushsection .user_access_table, "a"
.balign 8
.quad <fault_pc>
.quad <recovery_pc>
.popsection
```

The `<fault_pc>` is a local numeric label inside the same `asm!` block, sitting on the single instruction that can fault on user memory (`rep movsb`, or `lodsb` in the cstr variant on x86_64). The `<recovery_pc>` is a local label that closes the user-access window and returns a failure result. Registration is a build-time concern only; nothing in the kernel runtime modifies the table.

### Lookup

The `#PF` handler (`kernel/src/arch/x86_64/idt.rs::pf_dispatch`) walks the bracketed slice linearly, comparing `frame.rip` against each `fault_pc`. On a match, the handler writes `entry.recovery_pc` into `frame.rip` and returns. The naked stub then `iretq`s to the recovery PC. On a miss, the handler falls through to `dump_and_halt` — a `#PF` outside a registered window is a kernel bug, not a recoverable condition.

The lookup is linear over the table. The Phase 1 entry count is one per copy primitive (5 today), easily fits in a single cacheline, and runs only on the rare faulting path. A sorted table + binary search would be theoretical work; revisit if Phase 2 ever pushes the entry count past a few dozen.

### Recovery contract

Recovery code is responsible for:

1. **Closing the user-access window.** The CPU-saved flags state has the window-open bit set (`EFLAGS.AC=1` on x86_64; `PSTATE.PAN=0` on aarch64) because the fault occurred inside the window. The first instruction of the recovery sequence must reset the bit so the kernel runs under user-access enforcement again.
2. **Signalling failure.** Writing a non-zero status into the asm block's output register so the Rust wrapper returns `Err(UserAccessError::Fault)`.

The recovery code runs in the same `asm!` block as the fault site, with the same register live-range tracking, so the wrapper just reads the output register after the asm and dispatches on its value.

## User-access protection

Two CPU-enforced rules govern kernel access to user memory:

- **No accidental data access.** Kernel data loads / stores against a page marked user `#PF` unless the kernel has explicitly opened the access window. On x86_64 this is **SMAP** (Supervisor Mode Access Prevention) — controlled by `CR4.SMAP` and gated per-access by `EFLAGS.AC`; the `stac` / `clac` instructions toggle `AC`. On aarch64 the equivalent is **PAN** (Privileged Access Never) — controlled by `SCTLR_EL1.SPAN` and gated per-access by `PSTATE.PAN`; the `msr PAN, #imm` and `set / clear pan` instructions toggle it.
- **No accidental execution.** Instruction fetches by the kernel from a page marked user `#PF`. On x86_64 this is **SMEP** (Supervisor Mode Execution Prevention) — controlled by `CR4.SMEP`. On aarch64 the equivalent is **PXN** (Privileged Execute Never) — set per-PTE on user pages. Both are hardware-only; no software cooperation needed.

The window-open / window-closed transitions live exclusively inside the copy primitives' inline asm in `kernel/src/arch/<arch>/user_access.rs`. There are no Rust-visible wrappers for them: any kernel code calling such a wrapper would be opening the user-access window without the matching exception-table registration, which is exactly the bug class this subsystem exists to prevent.

### Boot enable

[`arch::init_protections`](../../kernel/src/arch/x86_64/paging.rs) is the arch-neutral entry point. `main.rs::paging_init` calls it once at boot; an SMP AP-bring-up path will call it once per AP when SMP lands. It panics if any feature the kernel hard-requires is missing from the running CPU.

On x86_64 the function:

1. Sets `EFER.NXE` (so the NX bit in page tables is honoured).
2. Reads CPUID 7.0:EBX bits 7 (SMEP) and 20 (SMAP); panics if either is absent.
3. Reads `CR4`, ORs in `CR4.SMEP | CR4.SMAP`, writes back.

On a future aarch64 port the same entry point handles PAN configuration (`SCTLR_EL1.SPAN`) and the PAN feature-flag detection (`FEAT_PAN` / `FEAT_PAN2` / `FEAT_PAN3`); PXN is per-PTE, so no global enable.

Phase 1's hard requirement on these features keeps the copy-primitive asm straight-line (no runtime branch around the window-open instructions). A future hardening pass could soften it to a runtime-optional feature; the decision is recorded in the slice-2 decision log entry.

### CPU model requirement under QEMU

The `xtask qemu` command runs QEMU with `-cpu qemu64,+smap,+smep`. The base `qemu64` model carries long mode, NX, and basic SSE; the `+smap,+smep` opt-ins add what the kernel actually requires. Default `qemu64` (no opts) lacks SMAP, and the kernel's `init_protections` would panic at boot.

Named CPU models like `Haswell-v4` or `Broadwell-v4` also work as a base, but TCG silently drops five features those models advertise (PCID, x2APIC, TSC-deadline, INVPCID, SPEC-CTRL), printing warnings on every boot. The kernel doesn't use those features today, so the minimalist `qemu64,+features` form is preferred: as future slices introduce real dependencies (`ArchTimer` will want `+tsc-deadline`, etc.), the xtask command grows by one flag at a time and stays a self-documenting record of which CPU features the kernel requires. (`ArchIrq` brings the local APIC up in xAPIC/MMIO mode precisely *because* TCG drops `x2apic`, so it needs no flag — see the decision log.)

## Architecture split

The subsystem is split into an arch-neutral half and an arch-specific half:

- [`kernel/src/mm/user_access.rs`](../../kernel/src/mm/user_access.rs) — the arch-neutral half. Owns `UserPtr<T>`, `UserMutPtr<T>`, `UserAccessError`, the validation helpers, the five public `copy_*_user` primitives, the `ExtableEntry` struct, and the `lookup_recovery` function called by the `#PF` handler.
- [`kernel/src/arch/x86_64/user_access.rs`](../../kernel/src/arch/x86_64/user_access.rs) — the x86_64 half. Owns `copy_bytes_raw(dst, src, len) -> bool` and `copy_cstr_raw(dst, src, max) -> CstrCopyOutcome`. The inline asm with `stac` / `clac` / `rep movsb` / `lodsb`, and the `.pushsection .user_access_table` entry emission, all live here.

The arch primitives return simple signals (`bool` for byte copies, an `enum` for cstr) and never reference `UserAccessError`. The mm layer maps those signals to its richer error type. This keeps the arch layer free of upward dependencies.

When aarch64 is implemented its raw primitives live in `kernel/src/arch/aarch64/user_access.rs` and use the equivalents called out in the [User-access protection](#user-access-protection) section above: PAN replaces SMAP, PXN replaces SMEP, and a bytewise `ldrb` / `strb` loop replaces `rep movsb` / `lodsb`. Boot enable goes through the same arch-neutral `arch::init_protections` entry point — it configures `SCTLR_EL1.SPAN` and checks `FEAT_PAN` rather than touching `CR4`.

The public surface (`UserPtr<T>`, `UserMutPtr<T>`, the five copy primitives, `UserAccessError`) is arch-neutral and does not change for that port. The shape of the arch-private interface (`copy_bytes_raw`, `copy_cstr_raw`, `CstrCopyOutcome`) is what each arch implements. An `ArchUserAccess` trait — listed in [docs/planning/implementation-plan.md](../planning/implementation-plan.md#L379) under "Architecture trait completion" — would be a thin formalisation once a second arch exists.

## Out of scope

- **Page-fault demand allocation.** A `#PF` on a legitimate user mapping today still terminates the kernel (no scheduler, no active address space). When the scheduler arrives, `pf_dispatch` will grow a VMA-lookup branch between exception-table lookup and `dump_and_halt`. Tracked in the slice-1 decision log entry.
- **Atomic copies.** Compound primitives (e.g. "copy this struct atomically") are layered on top by syscall handlers; the primitives in this document are the byte-level foundation.
- **Cross-thread aliasing.** `UserPtr<T>` carries `PhantomData<*const T>`, so it is neither `Send` nor `Sync` by auto-trait inference. Callers that need to pass user pointers across threads must opt in explicitly with a justification.
