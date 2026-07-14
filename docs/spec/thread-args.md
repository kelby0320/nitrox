# ThreadArgs — `sys_thread_create` argument block

This document specifies `ThreadArgs`, the `#[repr(C)]` block a process passes to
[`sys_thread_create`](syscall-abi.md) by `UserPtr<ThreadArgs>` to start another
thread in the **calling process**.

**Status:** Pre-stabilization. Phase 1 implements the form below; FPU/TLS state
and a richer thread-attributes form are deferred past Phase 1.

## Layout

```rust
#[repr(C)]
pub struct ThreadArgs {
    pub entry:     u64,      // offset 0  — ring-3 entry point VA
    pub user_sp:   u64,      // offset 8  — initial user stack pointer (stack top)
    pub arg0:        u64,    // offset 16 — opaque bootstrap word, delivered in rdx
    pub class:       u8,     // offset 24 — 0 = TimeShared (default), 1 = RealTime
    pub rt_priority: u8,     // offset 25 — RealTime fixed priority 0..=99
    pub nice:        i8,     // offset 26 — TimeShared nice -20..=19
    pub cpu_affinity:u8,     // offset 27 — affinity mask; 0 = no restriction
    pub _reserved:  [u8; 36],// offset 28 — must be zero
}
```

Total size 64 bytes, 8-byte aligned. The offsets are pinned by compile-time
asserts in `kernel/src/libkern/thread.rs`. **An ABI-version-hash input** (a
cross-boundary layout, like [`SpawnArgs`](process-spawn-args.md) / `IpcMsg`).

## Fields

- **`entry`** — the ring-3 instruction pointer the new thread begins at. Must be
  a non-null user-half address. That it is *mapped executable* is the caller's
  responsibility (an unmapped/non-executable entry simply faults the new thread,
  which is then contained by the suspend/terminate path).
- **`user_sp`** — the new thread's initial `rsp`. The caller owns the stack: it
  allocates + maps a region (e.g. via `sys_memory_create` + `sys_memory_map` with
  `MAP_READ | MAP_WRITE`) and passes the **top** (stacks grow down). Must be a
  non-null user-half address.
- **`arg0`** — an opaque word delivered to the new thread at entry (in `rdx`),
  mirroring the spawn register-bootstrap ABI. `rdi`/`rsi` are zero for a
  `sys_thread_create` thread (unlike a spawned process's main thread, which
  receives its bootstrap handles there).
- **`class`/`rt_priority`/`nice`/`cpu_affinity`** — the scheduling parameters, filled
  into the former reserved block by the SysCaps slice (size unchanged, 64). A zeroed
  block is TimeShared / nice 0 / no affinity — the historical default. **`class =
  RealTime` requires the `REAL_TIME` [syscap](../architecture/syscaps.md)** (else
  `NoAccess`); `nice`/affinity are ungated. Invalid class / out-of-range priority or
  nice → `InvalidArgument`.
- **`_reserved`** — must be all-zero; a non-zero byte returns `InvalidArgument`
  (forward-compatibility).

## Result

On success returns a `Thread` handle in the caller's table carrying
`SIGNAL | TERMINATE | INSPECT | DUPLICATE`. The new thread is enqueued runnable
immediately. The handle is the supervisor capability behind exception handling:
holding it, a sibling thread can read a faulted thread's registers
([`sys_thread_get_registers`](syscall-abi.md)) and resume or terminate it
([`sys_exception_resume`](syscall-abi.md)).

## Errors

- `FaultFromUser` — the `args` pointer is unreadable.
- `InvalidArgument` — `entry`/`user_sp` is null or outside the user half, a
  reserved byte is non-zero, or the caller is not a user process.
- `OutOfMemory` — the kernel could not allocate the thread or its kernel stack.
- A handle-table error (`OutOfHandles`, …) if the result handle cannot be
  installed; the thread is already running and is left unreferenced by the caller.

## Deferred (past Phase 1)

- **FPU/TLS** — the thread starts with no per-thread TLS base and the kernel's
  soft-float state; `sys_thread_set_tls` and FPU save/restore are Phase 2+.
- A richer attributes form (priority, name, CPU affinity at creation).
