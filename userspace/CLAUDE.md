# userspace/CLAUDE.md

Userspace workspace constraints. Loaded when Claude Code reads files under `userspace/`.

## Build environment

- **Standard target** (Nitrox-targeted, eventually `x86_64-unknown-nitrox.json` once the target is finalized; currently building against `x86_64-unknown-none` plus the `libkern` syscall surface).
- **`#![no_std]` + `alloc`** is the typical configuration. Userspace runs without the Rust standard library; `alloc` is available via the kernel-provided allocator interface in `libkern`.
- **Stable Rust only.** No nightly features.
- The `std` crate is not yet ported. When it lands, this guidance changes — until then, every userspace crate is `no_std`-with-alloc.

## Crate layering

The userspace runtime is layered. Don't reach below your layer:

```
Application                              ← user code
  ↓
libstream  librsproto                    ← typed I/O, RS protocol
  ↓
librt                                    ← sync wrappers, fiber scheduler
  ↓
libos                                    ← typed Handle<T, M>, async executor
  ↓
libkern                                  ← raw syscall wrappers
  ↓
syscall instruction
```

A crate can depend on anything below it but not above. `libstream` can use `libos`; `libos` cannot use `libstream`. Cyclic dependencies are not allowed and are caught by Cargo.

Application code typically uses `librt` for sync ergonomics or `libos` directly for async work. Reaching down to `libkern` should be rare — that's the raw syscall surface, used by early services and runtime infrastructure, not by ordinary application code.

## Async-first

Every potentially-blocking syscall returns a `PendingOperation` handle. The thread blocks via `sys_wait` on a list of waitable handles, never inside another syscall.

In practice:

- `libos::read()` is `async fn`, internally `sys_io_submit` → executor `await` on `sys_wait`
- `librt::read()` is the sync wrapper: same internal mechanism, but the thread blocks on `sys_wait` for a single handle
- Code at the syscall-wrapper level in `libkern` exposes the raw `sys_io_submit` + `sys_wait` directly

Don't write code that calls a syscall and "expects to block." That's the Unix model and it's not how this system works. If your code looks like `let result = some_syscall(); /* assumes blocking */`, you've misunderstood the model.

## Capability discipline

The kernel enforces capabilities. Userspace code should be capability-correct in addition:

- Don't pass handles around with more rights than necessary. Use `sys_handle_restrict` / `Handle::without_*` to attenuate before transferring.
- A handle granted to a child process should have the minimum rights the child needs.
- Resource servers don't hold `BIND_NAMESPACE`. Coordination supervisors (init, service-mgr, session-mgr) do.

## Per-crate notes

Each crate has its own `CLAUDE.md` for crate-specific guidance:

- `userspace/libkern/CLAUDE.md` — the syscall layer, no_alloc
- `userspace/init/CLAUDE.md` — PID 1, critical-path constraints
- `userspace/eshell/CLAUDE.md` — emergency shell constraints (similar to init)
- `userspace/fs-server-ext4/CLAUDE.md` — filesystem driver
- `userspace/service-mgr/CLAUDE.md` — service supervisor

Read the crate-specific `CLAUDE.md` before significant work in any of these.

## Resource server protocol

Userspace resource servers (`fs-server-*`, `netstack-server`, profile servers) communicate via IPC using the librsproto wire format. Specifics in `docs/spec/rsproto-wire-format.md`.

The startup protocol for any resource server:

1. Supervisor spawns the RS with control IPC channel
2. RS initializes
3. RS sends `Meta::Ready` on the control channel including its endpoint handle
4. Supervisor calls `sys_ns_bind(target_namespace, path, endpoint, rights)`

Don't have an RS try to register itself. Don't grant `BIND_NAMESPACE` to an RS. See `docs/rationale/why-supervisor-registration.md`.

## Configuration files

User-facing configuration is TOML. Service declarations follow `docs/spec/service-toml-schema.md`. Parsing should be tolerant of unknown fields (forward compatibility) but strict about types and required fields.

Don't introduce YAML, JSON5, or custom parsers. The TOML crate (project-internal, in `libkern` or a userspace utility crate — TBD) handles all configuration parsing.

## Testing

- Unit tests in `#[cfg(test)]` modules where possible.
- Integration tests for services run in QEMU.
- Mock the syscall surface for unit testing layers above `libkern` — `libkern` exposes a test mode that records and replays syscalls.

## Forbidden patterns

- `Box::leak` to obtain `'static` references
- Mutex over a `RefCell` (use proper synchronization or rethink)
- Calling syscalls "expecting to block"
- Hardcoding paths that should come from the namespace
- Embedded passwords, secrets, or tokens (even in tests — use fixtures or env vars)
- Network code in early services (they don't have networking yet, and the architecture explicitly defers netstack implementation)
