# userspace/init/CLAUDE.md

`init` (PID 1) workspace constraints. Loaded when Claude Code reads files under `userspace/init/`.

## What init is

PID 1. The first userspace process. Spawned directly by the kernel with the full initial handle set and full system capabilities. Its job:

1. Receive initial handles from the kernel
2. Read `/etc/init.toml` from the initramfs
3. Process critical-path mounts in dependency order, spawning fs-servers and binding their endpoints
4. Read `/system/current-generation` and spawn the system profile server
5. Spawn the service manager with delegated capabilities
6. Once boot is stable, release the initramfs
7. Enter main loop: reap orphaned processes, handle shutdown notifications

Init is deliberately minimal. It is the most critical-path code in the system. A crash of PID 1 is unrecoverable — the kernel cannot restart it.

## Build environment

- **`#![no_std]` + `alloc`** — same as other userspace
- **Uses `libkern`, `libheap` (its `#[global_allocator]`, slice 4), and `libos`** (the alloc-free typed/async core — `Handle<T,M>` + `block_on`, slice 5). init runs close to the syscall surface, but libos-*core* is fair game: it's `no_std` + no-`alloc` with **no runtime bootstrap** (stack-only handles/futures, no allocator or executor to spin up), so it can't be in a bad state during early boot. **Trajectory: full `std` eventually** — PID 1 on Linux universally links a full libc (systemd + dozens of shared libs; even SysV/BusyBox init link libc), so init is *not* meant to stay minimal. What init still avoids is the *stateful/protocol* runtime (`libstream`, `librsproto`) and anything depending on services that haven't started.
- **Stable Rust only.**

The line init draws is about **runtime state and started services**, not about staying at the raw syscall surface. Init runs before the service ecosystem is up, so it can't depend on anything that registers handlers or relies on services that haven't started. But the alloc-free, bootstrap-free layers (`libkern`, `libheap`, `libos`-core) are fine.

This means:
- `libos` `Handle<T,M>` + `block_on` for typed, async-shaped I/O is fine (init uses it — e.g. `read_current_generation`). Raw `sys_io_submit`/`sys_wait` + `RawHandle` are still available where mixing is simpler.
- Use `IpcMsg` directly for the fs-server handshake, not `librsproto` — a *pragmatic current choice* (init hand-parses the tiny Ready envelope), not because init couldn't; librsproto is a stateful protocol layer init has no need to pull in yet.
- Parse TOML manually (init has its own minimal TOML parser, since a full parser pulls in more than init needs).

## Critical-path discipline

Code that runs before init has handed off to the service manager is critical-path. Different rules apply:

- **No `panic!()` in normal operation.** Every error case must produce a useful message and either retry, fall through to a degraded mode, or invoke the emergency shell. Panics in init produce kernel panics by default.
- **No `unwrap()` outside of provably-impossible cases.** And those cases need a `// unwrap: <reason>` comment.
- **Bounded memory.** Init shouldn't allocate unboundedly. The initramfs is read once; mount processing is bounded by the number of `[[mount]]` entries.
- **Bounded time.** Each mount has a timeout (default 30 seconds). Init must not wait forever for an fs-server to send Ready.
- **Logging via the kernel log handle.** Init has a kernel log channel. Use it. Errors that happen before logging is set up go to the serial console directly via `sys_kprint` (a debug syscall available to processes with appropriate caps).

## Failure → eshell

Any critical-path failure (mount fails, bootstrap manifest is malformed, can't find fs-server binary, etc.) drops into emergency mode:

1. Log the failure clearly
2. Look up `/initramfs/sbin/eshell`
3. Spawn eshell with: serial console handle, initramfs handle (LOOKUP+READ+WRITE), `/dev/disk/*` (RO), kernel log handle (RO)
4. Wait for eshell to exit (typically a reboot)

Don't try to "soldier on" past a critical-path failure. If `init.toml` says mount this and it can't be mounted, the system is misconfigured; eshell exists exactly for this case.

## Capability handling

Init holds the kitchen sink at startup. It uses these capabilities for legitimate coordination work:

- `BIND_NAMESPACE` — to register fs-server endpoints in the system namespace
- `LOAD_MODULE` — to load Tier 2 LKMs (delegated to device manager later)
- `PHYSICAL_MEMORY` — only used in extreme recovery scenarios
- `SYSTEM_CLOCK` — delegated to time-sync service

When spawning the service manager, init delegates the subset of capabilities the service manager needs. The service manager doesn't get `PHYSICAL_MEMORY`, for example.

When spawning fs-servers, init grants only the device handles, log channel, and minimal namespace each fs-server needs. fs-servers do NOT get `BIND_NAMESPACE` — init does the binding on their behalf via the Resource Server Startup Protocol.

## TOML parsing

Init has its own minimal TOML parser. It supports only what `init.toml` needs:

- Top-level tables and table arrays (`[[mount]]`)
- String, integer, boolean values
- Sub-tables (one level deep is enough for the schema)

It does NOT support: TOML's full datetime types, nested arrays of tables beyond one level, complex value expressions. If `init.toml` ever needs those, the parser is upgraded — but the schema deliberately avoids them.

Don't add a full TOML library. The minimal parser is in `userspace/init/src/toml_lite.rs` (or similar).

## Initramfs interaction

Init reads files from the in-kernel initramfs resource server bound at `/initramfs/`:

- `/initramfs/sbin/init` — itself (already running)
- `/initramfs/sbin/fs-server-*` — filesystem driver binaries
- `/initramfs/sbin/eshell` — emergency shell
- `/initramfs/etc/init.toml` — bootstrap manifest

Use the namespace handle and `sys_ns_lookup` + `sys_io_submit` (Read opcode) to access these. There's no special initramfs API — it's a regular resource server.

After bootstrap is complete, init calls `sys_release_initramfs()` to free the initramfs memory. Init's own running image stays in memory; it just can't look up `/initramfs/...` paths anymore.

## Reaping

Init is the eventual parent of all orphaned processes (creator-based reparenting). It receives `Notification::ChildExited` for every process it directly created and for every orphan that gets reparented to it.

The reaping loop:

1. `sys_wait` on the notification channel
2. On `ChildExited`, close the process handle (this is "reaping")
3. Log the exit if it was abnormal

That's it. Init doesn't do supervision (the service manager does that for services); it just reaps to release process resources.

## Testing

Init is hard to test in isolation — it's the integration test for half the system. The strategy is:

- Unit tests for the TOML parser (host-side, easy)
- Unit tests for individual coordination steps with mocked syscalls (host-side via `libkern`'s test mode)
- Integration tests via `xtask test-qemu`: boot QEMU with a known initramfs and disk image, verify init reaches "service manager spawned" state

Don't add complex testing scaffolding to init itself. Keep it minimal.

## Forbidden patterns

- Pulling in the *stateful* runtime (`libstream`, `librsproto`) or anything depending on not-yet-started services (`libkern`/`libheap`/`libos`-core are fine; full `std` is the eventual target)
- `panic!()` outside of explicitly-unrecoverable error paths
- `unwrap()` without `// unwrap: <reason>`
- Unbounded loops, unbounded allocation, unbounded waiting
- Doing work that should be the service manager's job (supervision, dependency-ordered startup of non-critical services, etc.)
- Holding `BIND_NAMESPACE` or other potent caps longer than necessary — delegate and drop

## Useful pointers

- Boot flow: `docs/architecture/boot-flow.md`
- Bootstrap mount topology: `docs/architecture/bootstrap-mount-topology.md`
- init.toml schema: `docs/spec/init-toml-schema.md`
- Resource server startup protocol: `docs/architecture/namespace-and-resource-servers.md` § Resource Server Startup Protocol
- Why supervisor-mediated registration: `docs/rationale/why-supervisor-registration.md`
