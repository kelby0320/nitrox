# userspace/usersh/CLAUDE.md

Constraints for the throwaway user shell. Loaded when working under
`userspace/usersh/`.

## What this is

The leaf a login lands in: session-mgr spawns it into a **constructed per-user
namespace** (its `/home` is the user's home subtree of the fs-server, RW; `/dev/console`
is its I/O) with **empty syscaps** — a genuine sandbox. Its job is to prove the session
works: write a file to `$HOME` and read it back.

**This is a deliberate throwaway** (the real shell is Phase 4). Do not invest in it.
Under `test-harness` it runs the home-write proof and **exits** with its verdict (its
exit code is session-mgr's boot-verdict signal); otherwise it prints a welcome and drops
into a minimal console loop.

## Constraints (eshell family)

- **`#![no_std]` + `#![no_main]`, no `alloc`.** Fixed `.bss` buffers, `libkern` only
  (no `libos`/`librsproto`) — like eshell: minimal layers between a leaf and the syscall.
- **No `panic!()`/`unwrap()`** outside provably-impossible cases; a `#[panic_handler]`
  that just exits.
- **Bounded** input (a fixed line buffer; bounded reads).

## The sandbox is the point

usersh runs with **empty syscaps** in a namespace that binds only `/home` (its home
subtree) + `/dev/console`. It must reach nothing else — a lookup of `/dev/blk`, another
user's home, or the raw fs root simply fails (not bound). Don't add resources to its
world; that is session-mgr's namespace construction, not usersh's business. The shell
demonstrates *sandboxing by namespace construction*.

## Forbidden

- `alloc` / `#[global_allocator]`; `libos`/`librsproto`.
- Any syscap use (it has none) or assuming ambient authority.
- Investing in the interactive shell (throwaway — the real shell is Phase 4).
