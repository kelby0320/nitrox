# Why No Signals

Nitrox does not have Unix signals. Asynchronous events that signals would carry — process exit, hardware exceptions, child status, peer disconnect, power events — are delivered through the **notification queue**, a different mechanism with different properties. This document explains the choice.

## What signals are good for

Signals are Unix's mechanism for delivering asynchronous events to processes. They have specific virtues:

**They interrupt blocked syscalls.** A process stuck in `read()` can be woken up. This matters because Unix syscalls block by default, and without interruption, a process waiting for input can be impossible to terminate cleanly.

**They're delivered no matter what the process is doing.** Even if the process isn't checking for events, a signal will fire. This makes signals suitable for things the kernel must communicate, like segmentation faults and termination requests.

**They're a small, fixed vocabulary.** The signal table is well-known and stable. Programs know what `SIGTERM` means without a registry lookup.

## What signals are bad at

The list of problems with signals as a mechanism is long and well-established.

**Async-signal-safety is an arcane discipline.** A signal handler can fire at any point in the program, including inside the middle of a `malloc` call, inside a stdio buffer flush, inside any non-reentrant function. The handler can therefore call only "async-signal-safe" functions — a tiny subset of the C standard library. `printf` is not async-signal-safe. Most things you'd want to do in a handler aren't. Real-world signal handlers either limit themselves to setting a flag and returning, or they're subtly broken.

**Signal masks are a per-thread shared resource.** What signals are blocked at any moment depends on the thread's signal mask, the process's mask, the handler's mask. Library code that wants to ensure a signal doesn't fire during a critical section has to manipulate these masks, and the manipulation is itself signal-unsafe in subtle ways.

**Signals carry almost no information.** A signal number and (with `siginfo_t`) a small structured payload. Compare to "the kernel needs to tell us a child exited with status 137 and PID 4523" — `SIGCHLD` fires, the handler must then go query `wait()` to find out which child and why. The signal is a doorbell, not a message.

**Signals are racy.** Multiple signals of the same type are coalesced. If two children exit simultaneously and both fire `SIGCHLD`, you get one signal, and you have to loop calling `wait()` to find out you have two events. If you miss the loop, the second event is lost.

**Signals interact poorly with threads.** Which thread receives a process-directed signal? Which thread's stack runs the handler? Different Unixes answer differently; POSIX has rules but the rules are subtle and implementations have bugs. For multi-threaded programs, signal handling is one of the hardest things to get right.

**Signals interact poorly with libraries.** A library that needs to handle `SIGINT` must coordinate with the application that may also want to handle `SIGINT`. There's no clean composition story. The convention is "the application owns signals, libraries don't touch them," which means libraries that legitimately need async events can't use the kernel-provided mechanism.

**Re-entry into the kernel from a signal handler is restricted.** Most kernel operations cannot be safely invoked during signal delivery. The handler can do almost nothing.

**The signal-restartable-syscall problem.** A signal interrupting a blocking syscall returns `EINTR`, requiring the caller to detect this and retry. Some syscalls are auto-restartable; others aren't; the rules vary by signal handler flags. Application code is full of `while (read(...) == -1 && errno == EINTR) ;` loops that exist solely to deal with this.

The accumulated weight of these problems means signals are something experienced Unix programmers actively avoid. They're used because they're the only mechanism available, not because they're a good fit for what programs actually need.

## What Nitrox uses instead: the notification queue

Each Nitrox process has exactly one `NotificationChannel` kernel object. The kernel delivers structured `Notification` values into a per-process bounded queue. The process reads them via `sys_notif_recv`. The notification channel is waitable — it can be combined with any other waitable handles in a single `sys_wait` call.

The notification value is a typed enum:

```rust
#[repr(C, u32)]
pub enum Notification {
    SegFault     { thread: ThreadId, addr: VAddr, kind: FaultKind } = 0x0100,
    IllegalInsn  { thread: ThreadId, addr: VAddr } = 0x0101,
    ChildExited  { child: ProcessId, status: ExitStatus } = 0x0200,
    PeerClosed   { handle: RawHandle } = 0x0201,
    PowerEvent   { kind: PowerEventKind } = 0x0500,
    MemoryPressure { level: PressureLevel, free_pages: u64 } = 0x0501,
    // ...
}
```

The sparse, category-based discriminants reserve ranges per category (exceptions in `0x0100`, lifecycle in `0x0200`, power in `0x0500`, etc.) so new variants can be added without conflicts. A program receiving a discriminant it doesn't recognize sees `Unknown { kind, ... }` via kernel-side translation, allowing forward compatibility.

## What this fixes

**No async-signal-safety problem.** Notifications aren't delivered by interrupting the process. The process reads them when it's ready, in normal program flow. Any code that runs in response to a notification runs in a normal context with the full library available.

**No silent coalescing.** Each notification is a separate event. Two children exiting produce two `ChildExited` notifications with different payloads.

**Rich payloads.** A notification carries a typed structured value, not just a number. `ChildExited { child, status }` tells you which child and how it died in a single message — no separate `wait()` round-trip to discover the details.

**Composable with any other waitable.** `sys_wait` takes a list of handles and blocks until any of them signals. A program that wants to wait for "input on a channel OR notification of child exit OR a timer expiring" passes all three handles to `sys_wait` and gets the one that fired. There is no equivalent in Unix without `pselect`/`ppoll` and signal masks dance.

**No interaction with signal masks.** There are no signal masks. There is the queue and the explicit decision to read from it.

**Threaded programs work cleanly.** Notifications are delivered to a process, but reading is per-handle. A worker thread that holds the notification channel handle reads notifications. No question of "which thread got the signal."

**Library composition is straightforward.** Libraries don't intercept notifications; the application reads them. A library that wants to know about lifecycle events does so by being given the appropriate handle by its caller.

**No EINTR.** Notifications don't interrupt syscalls. A blocked `sys_wait` is unblocked when something it's waiting on signals — including the notification channel itself if a notification arrived. The "syscall got interrupted, please retry" pattern doesn't exist.

## Hardware exceptions: the priority chain

Hardware exceptions (segfault, illegal instruction, divide-by-zero) are delivered through the notification queue with a priority handling chain similar to Mach exception ports.

When a thread faults:

1. The kernel suspends the thread and saves its full register state.
2. If a debugger has registered a process-level exception channel for this process, the notification is delivered there first.
3. If no debugger, or if the debugger forwards the exception, the notification goes to the process's own notification channel.
4. If the process doesn't handle the exception (no `sys_exception_resume` call within a timeout), the kernel terminates the thread.

The handler — debugger or self — receives the notification, inspects state via `sys_thread_get_registers`, decides what to do, and calls `sys_exception_resume(thread, disposition)` with one of:

- `Resume` — retry the faulting instruction (used when the handler resolved the cause)
- `ResumeSkip` — skip the instruction (used for breakpoints)
- `Terminate { code }` — kill the thread
- `ModifyAndResume { register_update }` — change registers, then resume

The faulting thread waits in suspended state during all of this. There is no async stack interruption, no reentrancy hazard. The handler runs in normal context.

This is what `SIGSEGV`/`SIGILL`/`SIGFPE` were trying to do. The notification model does it cleanly.

## Process termination: not a signal

In Unix, `SIGTERM` requests cooperative termination; `SIGKILL` forces it. Both are signals. In Nitrox:

**Cooperative termination** is `Notification::TermRequest`, which the kernel can deliver when an authorized party wants the process to exit. The process reads it in normal flow, performs cleanup, calls `sys_process_exit`. There's no async interruption.

**Forced termination** is `sys_process_terminate(handle)` — a syscall that requires `TERMINATE` rights on the target process handle. The process is destroyed immediately. There's no signal to "fire"; the kernel just tears the process down. The process doesn't get to interfere because the operation isn't expressed as a signal it can ignore.

Process handles with `TERMINATE` rights are a capability that supervisors hold for processes they manage. Without that handle, you can't terminate the process — there's no "kill any process owned by my UID" parallel mechanism.

## What's lost

Honestly: not much that isn't trivially recovered.

The one thing signals do that takes some thought to replicate is **interrupting a stuck process**. In Unix, you can `kill -9` a process that's wedged. In Nitrox, the equivalent is `sys_process_terminate` with `TERMINATE` right — same concept, different mechanism. The capability discipline means you need the handle, not just root access. For administrative termination of misbehaving processes, the privilege broker provides this.

Programs that historically used `signal(SIGINT, handler)` to clean up on Ctrl-C now read `Notification::TermRequest` from their notification channel. The terminal/shell delivers the `TermRequest` instead of issuing `SIGINT`. The model is structurally the same; the mechanism is cleaner.

Programs that relied on `SIGALRM` for timeouts use `Timer` kernel objects directly, with `sys_wait` providing the unified blocking primitive. This is more work in some sense (you set up a Timer object) and less work in another (you don't have to handle timer-during-syscall raciness).

## Influences

**Mach exception ports** are the direct conceptual ancestor of the priority-chain exception delivery. Mach's separation of the exception delivery mechanism from the signal mechanism is part of what's preserved here.

**Plan 9** doesn't have signals in the Unix sense; processes communicate through channels. Plan 9's `note` mechanism is a small step toward what Nitrox does, though more limited.

**Fuchsia / Zircon** also avoids Unix signals and uses ports/channels for asynchronous events. The convergent design across capability-based modern systems is a strong signal that this is the right direction.

## Where to read more

- [Notification format spec](../spec/notification-format.md) — exact wire format of every notification variant
- [IPC architecture](../architecture/ipc.md) — IPC channels (distinct from notifications; covers the peer-to-peer case)
- [Process model architecture](../architecture/process-model.md) — termination, exit status, reaping
- [Why async-first syscalls](why-async-syscalls.md) — relates to the EINTR/syscall-restart story
