# Why Async-First Syscalls

Nitrox has no syscalls that block. There is no `sys_read` that puts the caller to sleep until data arrives. Every operation that could block returns a `PendingOperation` handle immediately; the caller blocks, if it wants to, by calling `sys_wait` on a list of pending operations and other waitable handles. This document explains the choice.

## What "syscall blocks" looks like in Unix

When a Unix process calls `read()` on a file descriptor that has no data ready, the kernel:

1. Receives the syscall
2. Notices that data isn't available
3. Marks the calling thread as blocked
4. Schedules another thread
5. Eventually, when data arrives, marks the original thread as runnable
6. Resumes the thread, which returns from the syscall

The thread is stuck inside the kernel for the duration. This is the synchronous-blocking model, and it has a few consequences that matter.

**One blocking syscall per thread.** A thread can be in exactly one blocking syscall at a time. To wait on multiple things concurrently, you need multiple threads, or you need explicit multiplexing primitives (`select`, `poll`, `epoll`, `kqueue`).

**The kernel chooses the wait granularity.** The kernel's blocking semantics are baked into each syscall. `read` blocks until *some* data is available. `recvmsg` has a `MSG_WAITALL` flag that can change this. Different syscalls have different blocking behaviors, and userspace inherits whatever the kernel decided.

**Async I/O is bolted on later.** POSIX `aio_*` calls, Linux's `io_submit`, IOCP on Windows, kqueue, epoll, io_uring — all of these are mechanisms for getting around the synchronous-blocking model. Each is an addition to a system that started synchronous and is trying to bolt on async. Each has its own quirks, gaps, and learning curve.

**Cancellation is a mess.** What happens if you want to cancel a `read` that's already in progress? Send a signal to interrupt it (gets `EINTR`, see [why no signals](why-no-signals.md))? Close the file descriptor (might or might not unblock; varies by what was blocked)? Use `pthread_cancel` (one of the more dangerous operations in pthreads)? There's no clean answer.

## What async-first looks like

Every Nitrox syscall that could block — `sys_io_submit`, `sys_channel_send`, `sys_channel_recv`, `sys_ns_lookup`, `sys_memory_map` (in some cases) — returns a `PendingOperation` handle immediately. The handle represents the in-flight operation. The kernel returns control to the caller right away.

The caller does whatever it wants with the pending handle. It might:

- Call `sys_wait(&[pending], deadline)` to block until completion
- Call `sys_wait(&[pending, other_pending, channel_recv, timer], deadline)` to wait on multiple things at once
- Set the operation aside and check on it later
- Call `sys_io_cancel(pending)` to cancel it
- Pass it to another thread that handles waiting

The unifying primitive is `sys_wait`. It takes a list of waitable handles (any combination of pending operations, IPC channels, timers, notification channels, process exits) and blocks until at least one signals. Returns the completed handle(s).

```rust
sys_wait(
    handles:  UserPtr<RawHandle>,
    count:    usize,
    results:  UserMutPtr<IoResult>,
    deadline: u64,              // monotonic ns; 0 = poll, u64::MAX = forever
) -> isize
```

`PendingOperation` is just another waitable kernel object. It's not a special-cased syscall mechanism; it's a kernel object like any other.

## What this gets us

**Concurrent waiting is trivial and free.** Want to wait on input from a channel, a child exit notification, and a timer? Pass three handles to `sys_wait`. There's no `select`/`poll`/`epoll` family with their respective quirks; there's one syscall.

**The kernel doesn't decide your wait granularity.** You decide whether to wait at all, how long to wait, what to wait on. The kernel never blocks you in a syscall against your will.

**Cancellation is a clean operation.** `sys_io_cancel(pending)` cancels the operation. The pending handle signals (with `Cancelled` status), and any thread waiting on it wakes up. There's no "is the syscall going to return EINTR or finish?" — the operation either completed before the cancel arrived, or it's cancelled.

**Async I/O is the default, not a special path.** Userspace runtimes can build async executors, fiber schedulers, and concurrent abstractions on top of `sys_wait` directly. There's no second-class status for synchronous code; it just calls `sys_wait` after each `sys_io_submit`. No code path is privileged.

**Composition with the io_uring-style ring is natural.** The high-throughput path (`sys_ring_create` and `sys_ring_notify`) is purely additive. It uses the same `IoOp` and `PendingOperation` types as the per-syscall path. Code can transparently switch between modes; the runtime can use the ring for hot paths and per-syscall for cold paths without the application noticing.

**Capability transfer of in-flight operations works.** Because pending operations are kernel objects with handles, they can be transferred between processes. A process can hand off "the read I'm waiting on" to another process (with appropriate rights). This isn't a common pattern but it's possible because the model is consistent.

## What about ergonomic synchronous code?

Programmers often want to write code that looks synchronous: "read the file, parse it, write the result." Forcing every operation through `sys_wait` would be hostile to this style.

The runtime libraries solve this. `librt` provides synchronous wrappers:

```rust
pub fn read<M: CanRead>(h: &Handle<File, M>, buf: &mut [u8]) -> Result<usize>
```

Internally, this is `sys_io_submit` followed by `sys_wait` on the resulting pending handle. The caller's thread blocks on `sys_wait` (not inside the syscall), and from the caller's perspective, the call is synchronous. The model is async; the API surface is sync where the programmer wants it to be.

For more sophisticated programs, `libos` provides an async executor built on `sys_wait`. The unit of concurrency is a Rust `Future`, scheduled cooperatively. Multiple Futures can wait concurrently on a single OS thread by all yielding into the executor's central `sys_wait` call.

For programs that prefer fibers (Go-style green threads), `librt` includes a fiber scheduler. Fibers yield on what looks like blocking calls; the scheduler resumes them when the underlying handles signal.

The point: applications choose their concurrency model. Async/await, fibers, threads with sync calls, raw pending-handle juggling — all are equally first-class because the underlying primitive accommodates them all.

## Where this design comes from

**io_uring** (Linux, 2019) is the most mature example of this model in production. Its submission/completion ring architecture is deliberate ancestry for Nitrox's `sys_ring_create`/`sys_ring_notify`. The core insight — that async-first is fundamentally better than sync-first with async bolted on — is now mainstream.

**Fuchsia / Zircon** uses a similar async-first model, with port/wait primitives and pending-completion handles.

**Windows IOCP** (Input/Output Completion Ports) predates io_uring by decades and embodies the same idea. NT was async-first from the beginning; the NT designers understood that synchronous I/O was a Unix legacy.

**The async/await Rust ecosystem** demonstrates that async-first works for application-level programming as well as kernel I/O. The design pattern of "futures composed by an executor that drives them via a single wait primitive" maps cleanly onto kernel-mediated `sys_wait`.

## What's lost

**Convenience for the simplest case.** A program that wants to "read a file" is doing two syscalls (`sys_io_submit` + `sys_wait`) instead of one (`read`). The runtime library's sync wrapper makes this invisible to programmers, but it's there at the syscall layer.

**Some kernel implementation simplicity.** A sync syscall just blocks the calling thread; an async syscall has to allocate a `PendingOperation` kernel object, link it into wait queues, and signal completion later. The kernel does more bookkeeping. This cost is real but bounded — pending operations are slab-allocated, completions go through DPCs, and the high-throughput path uses the ring for amortization.

**Per-syscall overhead in the simplest non-ring case.** Two syscalls instead of one for a single blocking op. The ring eliminates this for hot paths; for cold paths it's noise.

These costs are acceptable for the architectural cleanliness gained.

## How this interacts with the rest of the system

**No `EINTR`-style restart loops.** A `sys_io_submit` can't be interrupted in the middle (it's not blocking). A `sys_wait` returns whatever signaled, including a `Cancelled` status if cancellation arrived. Application code never has to retry around interrupted syscalls.

**The notification channel is just another waitable.** A program that wants to handle hardware exceptions, child exits, etc., adds its notification channel to the `sys_wait` list. Notifications are no different from pending operations from the wait perspective.

**Cancellation propagates cleanly.** Closing a `PendingOperation` handle is equivalent to canceling the operation. Closing the resource the operation targets propagates `PeerClosed` to the pending operation. Composing async operations with normal capability lifecycle works because both are expressed in the same handle-and-event vocabulary.

**The ring is additive, not replacement.** A high-throughput service that wants zero-syscall hot paths uses `sys_ring_create` and submits operations through the ring. Everything else uses `sys_io_submit` and `sys_wait` per operation. The same `IoOp` shapes flow through both. There's no "ring-only" or "syscall-only" code path.

## Where to read more

- [Syscall ABI spec](../spec/syscall-abi.md) — exact signatures of every syscall
- [Why no signals](why-no-signals.md) — relates to the EINTR/syscall-restart elimination
- [IPC architecture](../architecture/ipc.md) — channel send/recv as pending operations
- [Scheduler architecture](../architecture/scheduler.md) — how `sys_wait` integrates with scheduling
- [io_uring documentation (Linux kernel)](https://kernel.dk/io_uring.pdf) — original paper on the precedent design
