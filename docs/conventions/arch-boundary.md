# The architecture-abstraction boundary

Nitrox isolates all CPU- and platform-specific code behind a single
architecture-neutral interface. This document is the normative convention;
it is enforced by both the compiler and a CI lint.

## The rule

> Kernel code **outside `kernel/src/arch/`** may use the arch layer **only**
> through the neutral `crate::arch` interface. It must never name an
> architecture-specific module (`arch::x86_64::…`, future `arch::aarch64::…`)
> or expose architecture jargon (`gdt`, `idt`, `cr3`, `rsp`, MSR names, …) in
> the identifiers it touches.

All architecture-specific code lives under `kernel/src/arch/<arch>/` (today
`x86_64/`). The neutral surface is whatever `kernel/src/arch/mod.rs`
re-exports — nothing more.

## How it is enforced

1. **The compiler (primary).** `arch/mod.rs` declares the architecture
   submodule **private** (`mod x86_64;`, not `pub mod`). A path like
   `crate::arch::x86_64::gdt` therefore does not resolve outside `arch/` —
   it is a hard compile error. Re-exporting items from a private module is
   allowed, so the curated neutral surface still works.

2. **`cargo xtask check-arch` (regression net).** A lint walks
   `kernel/src/` (skipping `arch/`) and fails if any non-comment line names
   `arch::x86_64` / `arch::aarch64`. It catches what the compiler can't:
   stale doc-links, comments, and accidental re-exports. It runs in CI
   alongside `build` and `test`.

3. **This convention (intent).** Naming. Even within the neutral surface,
   do not surface architecture jargon. `arch::set_kernel_stack` — not
   `arch::gdt::set_kernel_stack`; `arch::init_syscalls` — not
   `arch::syscall::init`.

## Adding a new arch operation

1. Implement it in the active architecture's submodule
   (`arch/x86_64/<area>.rs`).
2. Expose it through `arch/mod.rs` under a **neutral name** — either a free
   function (wrapping the arch-specific entry point) or a re-export of a
   neutral-named item. Prefer free-function wrappers when the underlying
   name is jargon (see `set_kernel_stack` / `init_syscalls` in
   `arch/x86_64/mod.rs`).
3. Call it from kernel code as `crate::arch::<name>`.

When a second architecture lands, `arch/mod.rs` selects the implementation
with `#[cfg(target_arch = …)]`; the neutral names and every call site stay
unchanged.

## What is *not* required

- **Comments may use architecture terms.** Saying "PML4", "CR3", or "RSP" in
  a comment that describes the concrete behaviour is fine and often clearer.
  The boundary is about *identifiers and paths reachable outside `arch/`*,
  not prose.
- **Neutral trait modules** are architecture-neutral by construction and are
  public. Each holds a `trait Arch<X>` (all-static methods, `unsafe` where they
  touch hardware) + supporting types; the active architecture's `X86<X>` impl
  is re-exported from `arch/mod.rs` under a neutral alias. Today:
  `arch::paging` (`ArchPaging` → `Paging`), `arch::irq` (`ArchIrq` → `Irq`),
  `arch::cpu` (`ArchCpu` → `Cpu`), `arch::user_access` (`ArchUserAccess` →
  `UserAccess`), `arch::smp` (`ArchSmp` → `Smp`); future `arch::timer`,
  `arch::fpu`.
- A few re-exported module names are themselves neutral and acceptable:
  `arch::abi` (the platform ABI constants) and `arch::serial` (the debug
  console). Their *names* carry no architecture jargon even though their
  implementations are arch-specific.

## When a subsystem earns a trait

Use a **trait** for each behavioural subsystem whose operations are ordinary
functions with genuinely divergent per-architecture logic and a neutral
consumer (paging, cpu, irq, user-access, smp, timer, fpu). Group a subsystem's
operations under one trait; do **not** scatter them as loose free functions.

Keep **free functions / plain modules** for:

- **Naked-asm entry/switch glue** — `context_switch`, `enter_user`, the syscall
  entry/stack setup. These are single-call-site, irreducibly arch-specific
  glue; an all-static "namespace trait" around them is ceremony without payoff.
- **Stateful singletons** — `serial` (a `SERIAL` lock + log macros).
- **Pure data** — `abi` (constants).

## Examples

```rust
// OK — neutral interface. Behavioural subsystems are trait methods reached
// through the re-exported alias (the trait must be in scope to call them):
use crate::arch::{Cpu, Paging};
use crate::arch::cpu::ArchCpu;
use crate::arch::paging::ArchPaging;
Cpu::init_tables();
Cpu::halt_loop();
let root = Paging::active_root();

// OK — neutral data/singleton modules:
use crate::arch::{abi, serial};

// COMPILE ERROR — module `x86_64` is private:
use crate::arch::x86_64::gdt;

// REJECTED by `cargo xtask check-arch` even in a string/odd context:
//   crate::arch::x86_64::idt::init()
```
