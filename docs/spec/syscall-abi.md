# Syscall ABI Specification

This document specifies the Nitrox syscall interface: calling convention, return value encoding, error space, and the complete set of syscalls with their signatures. This is a normative contract between the kernel and userspace; both sides must agree on every detail here.

**Status:** Pre-stabilization. The syscall set may change before the v1.0 ABI freeze. Until then, the canonical source is `kernel/src/syscall/table.rs`; this document tracks it.

## Calling convention

### x86_64

Syscall entry uses the `syscall` instruction. Register conventions follow System V AMD64 ABI for the C-equivalent function call, with the syscall number in `RAX`:

| Register | Purpose |
|---|---|
| `RAX` | Syscall number (input); return value (output) |
| `RDI` | Argument 1 |
| `RSI` | Argument 2 |
| `RDX` | Argument 3 |
| `R10` | Argument 4 (note: `RCX` is clobbered by `syscall`) |
| `R8` | Argument 5 |
| `R9` | Argument 6 |

Syscalls take at most 6 arguments. Calls requiring more pass a pointer to a struct containing the additional fields.

`RCX` and `R11` are clobbered by the `syscall` instruction (saved RIP and RFLAGS respectively). The kernel saves and restores **all** other general-purpose registers (including the argument registers `RDI`/`RSI`/`RDX`/`R10`/`R8`/`R9`), so apart from `RAX` (the return value) and the `syscall`-clobbered `RCX`/`R11`, every register a caller holds across a `syscall` is preserved. (Userspace syscall wrappers therefore need only declare `RCX`/`R11` clobbered and `RAX` as the result.)

The kernel preserves the user thread's FS_BASE across syscall entry/exit. The kernel uses `swapgs` to swap GS_BASE for per-CPU kernel data.

### aarch64 (deferred implementation)

Syscall entry uses the `svc` instruction with immediate `0`. Register conventions:

| Register | Purpose |
|---|---|
| `X8` | Syscall number (input) |
| `X0`-`X5` | Arguments 1-6 (input); `X0` is return value (output) |

This follows Linux's aarch64 convention.

## Return value convention

All syscalls return a single `isize` value:

- **Negative values** are `KError` discriminants. See the `KError` enum in `kernel/src/syscall/error.rs` (the canonical source until a `docs/reference/error-codes.md` catalogue is written) for the complete list.
- **Non-negative values** are operation-specific. Common patterns: a count of bytes transferred, a handle value, or `0` for "success with no value."

Userspace code typically wraps this as `Result<NonNegative, KError>`:

```rust
pub fn check(ret: isize) -> Result<isize, KError> {
    if ret < 0 { Err(unsafe { core::mem::transmute::<i32, KError>(ret as i32) }) }
    else { Ok(ret) }
}
```

## Pointer arguments

All pointer arguments to syscalls are typed as `UserPtr<T>` or `UserMutPtr<T>` in the kernel. The userspace ABI passes raw integers; the kernel wraps them in the type-safe wrappers immediately. Userspace code typically passes these as Rust references that are coerced to raw pointers at the syscall boundary.

The kernel validates every pointer argument before access:
- Range check: address must fall in the user half of the address space
- Length check: `address + length` must not overflow or cross into kernel space
- Maximum size: total copy size must not exceed `MAX_USER_COPY_SIZE` (16 MiB)

Page faults during user memory access are recovered via the exception table; the syscall returns `FaultFromUser` (`-31`) if the user buffer is not accessible.

## Syscall numbering

Syscall numbers are not yet stabilized. The current convention is sequential allocation in `kernel/src/syscall/table.rs`, with stable assignments to be made before the v1.0 ABI freeze. Userspace code should reference syscalls by name through `libkern`, not by number.

The first stable numbers, allocated sequentially from `0`, are the handle operations:

| Number | Syscall |
|---|---|
| `0` | `sys_handle_close` |
| `1` | `sys_handle_duplicate` |
| `2` | `sys_handle_restrict` |
| `3` | `sys_handle_stat` |
| `4` | `sys_memory_create` |
| `5` | `sys_memory_map` |
| `6` | `sys_memory_unmap` |
| `7` | `sys_clock_read` |
| `8` | `sys_timer_create` |
| `9` | `sys_timer_set` |
| `10` | `sys_wait` |
| `11` | `sys_notif_recv` |
| `12` | `sys_channel_create` |
| `13` | `sys_channel_send` |
| `14` | `sys_channel_recv` |

Numbers are assigned in landing order, not in the order syscalls appear below.

Syscall numbers are **not** part of the kernel ABI version hash (`docs/spec/abi-version-hash.md`).

### Debug syscalls (not ABI-stable)

A small set of **debug-only** syscalls exists to bootstrap and exercise the kernel before the stable syscall surface lands. They occupy a deliberately high, non-stable number range (`0xFFFF_0000+`) so they never shadow the stable sequential numbers, and they are **excluded from the v1.0 ABI freeze** — they may change or be removed without notice.

- `sys_debug_kprint(ptr: UserPtr<u8>, len: usize) -> isize` (`0xFFFF_0000`) — copy `len` user bytes (bounded) and write them to the kernel serial log; returns the byte count. The non-async exception to the async-first rule (it completes immediately).
- `sys_debug_exit(status: i32) -> !` (`0xFFFF_0001`) — used only by the throwaway ring-3 bootstrap harness to return control to the kernel; removed when the first real userspace process lands.

## The complete syscall set

### Handle Operations

```rust
fn sys_handle_close(h: RawHandle) -> isize
```
Releases the calling process's reference to the handle. After this returns, the handle value is invalid for the calling process.

```rust
fn sys_handle_restrict(h: RawHandle, new_rights: Rights) -> isize
```
Attenuates `h`'s rights **in place** to `h.rights & new_rights`; `h` keeps the same value and remains valid. Cannot amplify rights. Requires no right (this is self-attenuation). Returns `0`.

```rust
fn sys_handle_duplicate(h: RawHandle, new_rights: Rights) -> isize
```
Returns a new handle referring to the same kernel object, with rights = `h.rights & new_rights`. Original `h` remains valid. Requires `DUPLICATE` right on `h`.

```rust
fn sys_handle_stat(h: RawHandle, out: UserMutPtr<HandleInfo>) -> isize
```
Writes metadata about `h` to `*out`. Requires `INSPECT` right on `h`. Returns `0`.

`HandleInfo` is a fixed `#[repr(C)]` record (16 bytes, 8-byte aligned, no interior padding):

```rust
#[repr(C)]
pub struct HandleInfo {
    pub rights: u64,       // offset 0  — Rights::bits()
    pub object_type: u32,  // offset 8  — KObjectType discriminant
    pub generation: u32,   // offset 12 — handle generation counter
}
```

`owner_pid` is intentionally not reported: a process can only `stat` handles it owns (the table enforces `owner_pid == caller`), so it would always equal the caller's pid.

### I/O Core

```rust
fn sys_io_submit(resource: RawHandle, op: UserPtr<IoOp>) -> isize
```
Submits an I/O operation. Returns a `PendingOperation` handle (positive value). The operation runs asynchronously; use `sys_wait` to wait for completion.

```rust
fn sys_io_cancel(pending: RawHandle) -> isize
```
Requests cancellation of an in-flight operation. The pending handle will signal with `Cancelled` status if cancellation succeeds, or with the natural completion if cancellation arrives too late.

```rust
fn sys_wait(
    handles:  UserPtr<RawHandle>,
    count:    usize,
    results:  UserMutPtr<IoResult>,
    deadline: u64,
) -> isize
```
Blocks until at least one handle in `handles[0..count]` signals, or until `deadline` (absolute monotonic nanoseconds) passes. Special deadline values: `0` = poll, `u64::MAX` = no timeout. Writes one `IoResult` per signaled handle to `results`. Returns the count of signaled handles (positive), `TimedOut` if the deadline elapsed with nothing signaled, or `WouldBlock` for a poll (`deadline == 0`) that found nothing ready. (Syscall number `10`.)

**Implemented for `Timer`, `NotificationChannel`, and `IpcChannel` handles** (a notification channel is signaled when its queue is non-empty; an IPC endpoint when its receive ring is non-empty or its peer has closed); other waitable types (`PendingOperation`, `Process`) return `Unsupported` until their slices land. `count` is capped at `MAX_WAIT_HANDLES` (8). Deadlines resolve on the periodic scheduler tick (~10 ms granularity), not exactly.

### Namespace

```rust
fn sys_ns_lookup(
    ns:       RawHandle,
    path:     UserPtr<u8>,
    path_len: usize,
    rights:   Rights,
) -> isize
```
Looks up `path` in namespace `ns`, requesting at most `rights`. Returns a `PendingOperation` handle. The pending operation completes with either a handle to the resolved resource (with rights ≤ requested) or an error.

```rust
fn sys_ns_bind(
    ns:       RawHandle,
    path:     UserPtr<u8>,
    path_len: usize,
    resource: RawHandle,
) -> isize
```
Binds `resource` (typically an IPC channel endpoint or a kernel resource server registration) at `path` in `ns`. Requires `BIND` right on `ns` and `BIND_NAMESPACE` system capability. Returns `0` on success.

```rust
fn sys_ns_unbind(
    ns:       RawHandle,
    path:     UserPtr<u8>,
    path_len: usize,
) -> isize
```
Removes the binding at `path` in `ns`. Requires `UNBIND` right on `ns`.

```rust
fn sys_ns_create() -> isize
```
Creates a new empty `Namespace` kernel object. Returns a handle with full rights.

### Process and Thread

```rust
fn sys_process_spawn(args: UserPtr<SpawnArgs>) -> isize
```
Spawns a new process per the `SpawnArgs` struct (see [SpawnArgs spec](process-spawn-args.md)). Returns a handle to the new process. The child's initial handle table is populated from `args.handles`; child's namespace is `args.namespace`.

```rust
fn sys_thread_create(args: UserPtr<ThreadArgs>) -> isize
```
Creates a new thread in the calling process. Returns a thread handle.

```rust
fn sys_thread_exit(status: i32) -> !
```
Exits the calling thread. Does not return.

```rust
fn sys_process_exit(status: i32) -> !
```
Exits the calling process (terminates all its threads). Does not return.

```rust
fn sys_thread_set_tls(tls_base: usize) -> isize
```
Sets the calling thread's TLS base register (FS_BASE on x86_64, TPIDR_EL0 on aarch64).

```rust
fn sys_thread_set_affinity(thread: RawHandle, cpu_mask: CpuMask) -> isize
```
Restricts which CPUs `thread` may run on. Requires `SIGNAL` right.

```rust
fn sys_thread_get_registers(thread: RawHandle, out: UserMutPtr<RegisterValues>) -> isize
```
Writes the saved register state of `thread` to `*out`. Thread must be suspended (typically due to exception). Requires `SIGNAL` right.

```rust
fn sys_exception_resume(thread: RawHandle, disposition: Disposition) -> isize
```
Resumes a suspended thread with the specified disposition (Resume, ResumeSkip, Terminate, ModifyAndResume). Requires `SIGNAL` right. **Deferred** (the debugger/suspend model): the notifications slice uses a **post-mortem** model — a ring-3 fault delivers a `Notification` (`SegFault`/`IllegalInsn`/`DivideByZero`) to the faulting process's `NotificationChannel` and **terminates** the faulting thread (the kernel survives, vs. the old halt). Suspend + `sys_exception_resume` + register inspection land when a real userspace supervisor exists (process spawn).

```rust
fn sys_exception_extend_timeout(thread: RawHandle, additional_ns: u64) -> isize
```
Extends the deadline before the kernel auto-terminates a suspended thread. Used by debuggers that need more time to inspect.

### Memory

```rust
fn sys_memory_create(size: usize, flags: MemFlags) -> isize
```
Allocates a `MemoryObject` of `size` bytes (rounded up to page size), zero-filled, owned by the calling process; the object owns its physical frames for its lifetime. Returns a handle with full rights (`MAP_READ | MAP_WRITE | MAP_EXEC | DUPLICATE | INSPECT | TRANSFER`). `MemFlags` is a reserved `#[repr(transparent)]` bitflags `u64`; **no flags are defined yet, so `flags` must be `0`** — any set bit returns `InvalidArgument`. `size` of `0` returns `InvalidArgument`; `size` above the Phase 1 cap (16 MiB) returns `TooLarge`.

```rust
fn sys_memory_map(
    obj:    RawHandle,
    hint:   usize,
    size:   usize,
    rights: Rights,
) -> isize
```
Maps `obj`'s frames into the calling process's address space. `hint` is an advisory page-aligned address (`0` = "anywhere", chosen from a kernel mmap window). `rights` is the `MAP_*` subset to install; the handle must carry every requested `MAP_*` bit (so a mapping cannot amplify — e.g. mapping writable requires `MAP_WRITE`), else `NoAccess`. `size` is rounded up to a page and must be ≤ the object's size. Returns the mapped base virtual address. Mapping the same object twice **aliases the same physical memory** (the object owns the frames).

```rust
fn sys_memory_unmap(addr: usize, size: usize) -> isize
```
Unmaps the mapping at `addr`. **Phase 1 unmaps the whole VMA covering `addr`; the `size` argument is not yet honored** (partial/splitting unmap is a later refinement — see the kernel TODO). Returns `0`, or `InvalidArgument` if nothing is mapped at `addr`. For an object-backed mapping the object's frames are *not* freed — they are released when the object's last handle/mapping is dropped.

### IPC

```rust
fn sys_channel_create(
    end0:        UserMutPtr<RawHandle>,
    end1:        UserMutPtr<RawHandle>,
    queue_depth: u32,
) -> isize
```
Creates a new IPC channel with the specified queue depth (`0` → default 16; `> 1024` → `InvalidArgument`). Writes the two endpoint handles to `*end0` and `*end1`, each carrying `SEND | RECV | DUPLICATE | TRANSFER | INSPECT | WAIT`. (Syscall number `12`.)

```rust
fn sys_channel_send(
    ch:      RawHandle,
    msg:     UserPtr<IpcMsg>,
    handles: UserPtr<RawHandle>,
    count:   usize,
    mode:    SendMode,
) -> isize
```
Sends `*msg` plus `handles[0..count]` over `ch` (requires `SEND`). The kernel stamps `header.sender_pid` / `timestamp`. Returns `0`, `WouldBlock` if the queue is full (`NoBlock`), or `PeerClosed` if the peer endpoint has closed. (Syscall number `13`.)

**Implemented subset:** `mode == NoBlock` only — `Block` / `BlockBounded` (which block via a `PendingOperation`, returning that handle) are deferred to the async-I/O slice and currently return `Unsupported`. Handle transfer is deferred to process spawn: `count` must be `0` (non-zero → `Unsupported`); the `handles`/`count` parameters are retained in the ABI for stability.

```rust
fn sys_channel_recv(
    ch:      RawHandle,
    msg:     UserMutPtr<IpcMsg>,
    handles: UserMutPtr<RawHandle>,
    count:   UserMutPtr<usize>,
) -> isize
```
Receives a message from `ch` (requires `RECV`). Returns `WouldBlock` if no message is queued — the caller `sys_wait`s on `ch` to block — or `PeerClosed` if the inbox is empty and the peer has closed. On success writes the 4096-byte message to `*msg` and the transferred-handle count to `*count` (`0` this slice — handle transfer deferred). (Syscall number `14`.)

### Kernel Objects

```rust
fn sys_timer_create(flags: TimerFlags) -> isize
```
Creates a new `Timer` kernel object (unarmed). Returns a handle carrying `WAIT | DUPLICATE | INSPECT | TRANSFER`. `flags` must be a valid `TimerFlags` (none defined yet → must be 0). (Syscall number `8`; implemented in the wait-queues slice.)

```rust
fn sys_timer_set(timer: RawHandle, deadline_ns: u64, interval_ns: u64) -> isize
```
Programs the timer to fire at `deadline_ns` (**absolute** monotonic ns; `0` disarms) and re-fire every `interval_ns` thereafter. `interval_ns` of `0` is one-shot. Returns `0`. (Syscall number `9`; implemented in the wait-queues slice. Deadlines resolve on the periodic scheduler tick, ~10 ms granularity.)

```rust
fn sys_notif_recv(queue: RawHandle, out: UserMutPtr<Notification>) -> isize
```
Receives one notification from the queue (a `NotificationChannel` handle). Returns `0` and writes the 64-byte `Notification` to `*out` on success; returns `WouldBlock` if the queue is empty. A pending overflow surfaces as a synthetic `NotificationsDropped { count }` before further entries. Gated by handle ownership (no special right; `WAIT` gates *blocking* on the channel via `sys_wait`). (Syscall number `11`; implemented in the notifications slice.)

```rust
fn sys_clock_read(clock: ClockId, out: UserMutPtr<u64>) -> isize
```
Reads the current value of the specified clock (Monotonic, Realtime, ProcessCpu, ThreadCpu) in nanoseconds. Writes to `*out`. Returns `0`. (Syscall number `7`.)

**This slice services `Monotonic` only**; `Realtime`, `ProcessCpu`, and `ThreadCpu` return `Unsupported`. `Realtime` needs a wall-clock offset service, and the per-CPU clocks need scheduler CPU accounting — neither exists yet. The selector and the `out` pointer are validated before any clock is read, so an unknown `ClockId` returns `InvalidArgument` and an unsupported clock returns `Unsupported` without touching `*out`.

```rust
fn sys_device_map_mmio(
    device:     RawHandle,
    region_idx: u32,
    flags:      MmioFlags,
) -> isize
```
Returns a `MemoryObject` handle for the MMIO region indexed by `region_idx` in the device's resource descriptor. Requires appropriate rights on `device`. Subsequent `sys_memory_map` on the returned object installs MMIO PTEs.

```rust
fn sys_release_initramfs() -> isize
```
Unbinds `/initramfs` from the root namespace and frees the initramfs physical pages. One-shot — succeeds once, returns `AlreadyReleased` thereafter. Requires `BIND_NAMESPACE`.

### High-Throughput Ring (additive optimization)

```rust
fn sys_ring_create(sq_depth: u32, cq_depth: u32, flags: RingFlags) -> isize
```
Creates an `IoRing` with the specified submission and completion queue depths. Returns a handle. The ring's shared-memory mappings are obtained via subsequent `sys_memory_map` calls on the handle.

```rust
fn sys_ring_notify(
    ring:         RawHandle,
    to_submit:    u32,
    min_complete: u32,
    deadline:     u64,
) -> isize
```
Notifies the kernel that `to_submit` new entries are in the ring's submission queue, and waits for at least `min_complete` completions. With `RING_KERNEL_POLL` set on the ring, this syscall is unnecessary in the steady state.

## Argument types

Argument types referenced above are defined in `libkern`:

- `RawHandle`: `#[repr(transparent)] pub struct RawHandle(u64);`
- `Rights`: a bitflags type, see [handle-encoding.md](handle-encoding.md)
- `IoResult`: `#[repr(C)]` 16-byte record, 8-aligned, no padding: `handle: u64` (offset 0, the signaled handle), `status: i32` (offset 8, `0` = ready, negative = a `KError`), `reserved: u32` (offset 12, zeroed). Defined in `kernel/src/libkern/io_result.rs`. **Part of the ABI version hash** (`abi-version-hash.md` § "IoOp and IoResult layouts").
- `TimerFlags`: `#[repr(transparent)]` bitflags over `u64`; no flags defined yet (must be 0). Defined in `kernel/src/libkern/timer_flags.rs`.
- `IoOp`, `SpawnArgs`, `ThreadArgs`, `IpcMsg`, `Notification`: see relevant spec documents
- `ClockId`: `#[repr(u32)]` enum, `Monotonic = 0`, `Realtime = 1`, `ProcessCpu = 2`, `ThreadCpu = 3`; defined in `kernel/src/libkern/clock.rs`
- `Disposition`, `SendMode`, `MemFlags`, `MmioFlags`, `RingFlags`: small `#[repr(u32)]` or bitflag enums; values stable, definitions in `libkern`

## Stability

Syscall numbers, names, and signatures are pre-stabilization until v1.0. Until then:

- Source code in `userspace/libkern/src/` is the canonical syscall table
- This document tracks the canonical table; if the two disagree, the source wins and this document is updated

After v1.0, syscall numbers and the existing signatures become a stability commitment. New syscalls may be added (with new numbers); existing ones may not be modified incompatibly.

## Where to read more

- [Handle encoding](handle-encoding.md)
- [IPC message format](ipc-message-format.md)
- [Notification format](notification-format.md)
- `kernel/src/syscall/error.rs` — the `KError` enum (canonical error-code source)
- [Process spawn args](process-spawn-args.md)
- [Why async-first syscalls](../rationale/why-async-syscalls.md)
