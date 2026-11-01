# userspace/libkern/CLAUDE.md

`libkern` workspace constraints. Loaded when Claude Code reads files under `userspace/libkern/`.

## What libkern is

`libkern` is the raw syscall layer. It contains:

- `unsafe extern "C"` declarations for every syscall
- `#[repr(C)]` ABI types: `RawHandle`, `Rights`, `IoOp`, `IoResult`, `IpcMsg`, `Notification`, `SpawnArgs`, etc.
- `UserPtr<T>` / `UserMutPtr<T>` opaque pointer wrappers (mirrored from kernel)
- `KError` enum
- A few thin convenience wrappers (`Result`-returning versions of the raw syscalls)

That's it. No higher-level abstractions, no async executor, no typed handles, no memory management beyond what the syscalls provide directly.

## Build environment

- **`#![no_std]`** — no Rust standard library
- **No `alloc`.** This crate must work before any allocator is initialized. Used by init and the kernel itself's userspace test harness, both of which run before there's a heap.
- **Stable Rust only.**
- **No external crates.** Same rule as the kernel. Anything `libkern` needs must be implemented in `libkern` or be in `core`.

The "no `alloc`" rule is the most-violated rule for newcomers. Don't import `Vec`, `String`, `Box`, or `BTreeMap`. Don't write `cfg(feature = "alloc")` to add them either — `libkern` is no-alloc, period. If you need a dynamic structure, the layer above (`libos`) is the right place.

## ABI authority

`libkern` is the canonical source for the userspace side of the kernel ABI. Any change to:

- Syscall signatures
- ABI type layouts (`#[repr(C)]` structs)
- Enum discriminant values
- Right bit positions
- Constants like `IPC_MSG_SIZE`

is a synchronization point with the kernel. The kernel has its own copy of these definitions in `kernel/src/syscall/abi.rs` (or similar); when `libkern` changes, the kernel's copy must change identically. `cargo xtask abi-sync-check` validates that the two copies match.

## `unsafe` policy

Every syscall function is `unsafe` because the actual `syscall` instruction is unsafe. Wrap them in safe `Result`-returning helpers where possible:

```rust
// The raw syscall — unsafe
pub unsafe fn sys_handle_close_raw(h: RawHandle) -> isize { /* asm */ }

// Safe wrapper — checks the return value
pub fn sys_handle_close(h: RawHandle) -> Result<(), KError> {
    let ret = unsafe { sys_handle_close_raw(h) };
    if ret < 0 {
        Err(unsafe { core::mem::transmute(ret as i32) })
    } else {
        Ok(())
    }
}
```

The safe wrapper is what other code uses. The raw `unsafe` version is exposed for cases where direct return-value access is needed (e.g., `sys_io_submit` returns a positive handle value, not just `()`).

Every `unsafe` block has a `// SAFETY:` comment.

## The syscall surface is canonical

This is the place where the syscall numbers, signatures, and behavior are defined. Other crates use what's here; they don't define their own.

When adding a new syscall:

1. Add the raw `unsafe extern` declaration in `libkern/src/syscall.rs`
2. Add ABI types if needed (in `libkern/src/abi/`)
3. Add the safe wrapper
4. Update `docs/spec/syscall-abi.md` to match
5. Run `cargo xtask abi-sync-check` to verify the kernel side matches
6. Update any higher-layer crates (`libos`, `librt`) if they should expose the new operation

Don't skip step 4. The spec doc is part of the contract.

## What goes here, what doesn't

| Goes in `libkern` | Doesn't |
|---|---|
| Raw syscall functions | Async executors |
| ABI types (`#[repr(C)]`) | Typed handles (`Handle<T, M>`) |
| `UserPtr<T>` wrappers | RAII guards, but the two below |
| `KError`, `Rights`, etc. | Path resolution helpers |
| Thin `Result<>` wrappers | Stream readers/writers |
| Constants (sizes, limits) | Custom data structures |

If something doesn't strictly belong here but you find yourself wanting to put it here, the answer is almost always "put it in `libos` instead."

**One guard here is `channel::Received`** (pre-Phase-7 Part D, group F): the handles a message
brought, closed when it drops unless taken. `sys_channel_recv` installs whatever a sender attaches,
so every receiver must close what it did not ask for, those that do not depend on libos
(`auth-service`, `librsproto`, `libstream`) included, and the lowest layer is the only one they
all share. Receive through `channel_recv`, or
`channel_recv_raw` into a `static mut` buffer, never a raw `SYS_CHANNEL_RECV`:
`cargo xtask check-recv` fails any other userspace file that names it.

**The other is `Secret`** (pre-Phase-7 Part I, group T): bytes `scrub`bed when they drop, for a
password and the buffers it passes through, so an early return cannot skip the scrub. It scrubs
where it lies, so its life ends with a scope and never with `drop`, which is a move and scrubs a
copy. It sits beside `scrub` for the same reason `Received` is here: `auth-service` and the login
supervisors share it.

## Testing

`libkern` is testable on the host with a mock syscall implementation. The `libkern_test` feature (or similar) replaces real syscalls with a recorded/replayable mock so unit tests can run in `cargo test`.

For tests that need real syscalls, use the QEMU integration test harness — but most `libkern` tests should be doable host-side.

## Forbidden patterns

- `Vec`, `String`, `Box`, or any heap type
- External crates beyond `core`
- Higher-level abstractions (those go in `libos`)
- Inline-asm syscalls that bypass the canonical declarations in `libkern/src/syscall.rs`
- Modifying ABI types without updating the kernel side and the spec doc

## Useful pointers

- Syscall ABI: `docs/spec/syscall-abi.md`
- Handle encoding: `docs/spec/handle-encoding.md`
- IPC message format: `docs/spec/ipc-message-format.md`
- Notification format: `docs/spec/notification-format.md`
