# userspace/eshell/CLAUDE.md

Constraints for the **emergency shell** (`eshell`). Loaded when working under
`userspace/eshell/`.

## What this is

The first **interactive** userspace program (Phase 2 slice 9): a minimal shell on
the serial console. init spawns it after boot (an interactive `eshell>` prompt) and
drops to it on a critical-path failure. It reads keyboard input from `/dev/console`
(a char `DeviceNode`) via the universal `sys_io_submit(Read)` + `sys_wait` path, does
its own echo + line editing, and runs a few inspection commands.

## Constraints (same family as init)

eshell is critical-path-adjacent (it's the recovery surface), so it follows init's
rules:

- **`#![no_std]` + `#![no_main]`.** Bare-target `_start`; a `#[panic_handler]` that
  cannot escalate.
- **`libkern` only.** Does **not** depend on `libos`, `librt`, `libstream`, or
  `librsproto` — like init, it works directly on the raw syscall surface. (It speaks
  no rsproto: it reads/maps resources via `sys_ns_lookup` + `sys_io_submit` +
  `sys_memory_map`.)
- **No `alloc`.** Fixed `.bss` buffers (the line buffer, the read buffer handle); no
  `#[global_allocator]`. If a future command needs `alloc`, copy init's `BumpAlloc`.
- **No `panic!()` / `unwrap()`** in normal operation. Every error case logs and
  continues (the prompt must survive a bad command).
- **Bounded** memory/time/input (a fixed-size line buffer; reads are bounded).

## I/O model

- **Input:** look up `/dev/console` (the inherited root namespace grants it), then
  loop `sys_io_submit(console, {Read, buffer, len})` → `sys_wait(po)` → process the
  bytes. The console delivers **raw** bytes; eshell owns the line discipline (echo,
  backspace, CR/LF → end of line).
- **Output:** `sys_kprint` (via `libkern::kprint`) for the prompt, echo, and command
  output. A symmetric console *write* through `sys_io_submit` is a later refinement.

## Build

Bin-only, bare target — mirrors `userspace/parent` exactly: `.cargo/config.toml`
(static, non-PIE, `x86_64-unknown-none`), `build.rs` (passes `user.ld` via
`rustc-link-arg`), `user.ld` (fixed low-half ET_EXEC). Spawned by the kernel via the
embedded `ImageId::Eshell`.

## Forbidden

- Depending on `libos`/`librt`/`libstream`/`librsproto`.
- `panic!()`/`unwrap()` outside provably-impossible cases.
- Unbounded loops/allocation/input.
- A device-specific input syscall — input goes through `sys_io_submit`.
