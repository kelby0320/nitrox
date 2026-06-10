# Notifications

Nitrox has no Unix signals (see `docs/rationale/why-no-signals.md`). Async
events — hardware faults, child exits, IPC peer-close, resource changes — are
delivered as structured **`Notification`** values into a per-process **bounded
queue**, the `NotificationChannel`. A process drains it with `sys_notif_recv`,
and the channel is `sys_wait`-able (it signals when its queue goes empty →
non-empty), so notifications compose with timers and any other waitable in a
single `sys_wait`.

The wire format is fixed at 64 bytes (a `u32` discriminant + a 60-byte
little-endian body); see `docs/spec/notification-format.md` for the variant
layouts and the `Unknown`-fallback forward-compatibility rule.

## NotificationChannel

- One per process (a kernel object, `KObjectType::NotificationChannel`).
- A bounded FIFO queue (default 64 entries) plus a drop counter, in kernel
  memory — a copy model, not shared memory.
- Waitable: enqueueing into an empty queue wakes any `sys_wait`ers, exactly like
  a `Timer` firing (the kernel reuses the same wait-queue machinery).
- **Overflow / exception-priority eviction:** when the queue is full, an
  **exception** notification (the `0x0100` category) evicts the oldest
  *non-exception* entry so fault information is never lost under pressure; any
  other notification (or a full-of-exceptions queue) is dropped and a counter
  increments. The next `sys_notif_recv` returns a synthetic
  `NotificationsDropped { count }` before further entries.
- All channel state lives under the single rank-1 scheduler lock for Phase 1
  (single-CPU); see `kernel/docs/lock-ordering.md`.

## Exception delivery (post-mortem)

When a thread faults in ring 3 (page fault, invalid opcode, divide error, …),
the kernel — instead of halting — builds the matching `Notification`
(`SegFault` / `IllegalInsn` / `DivideByZero`, carrying the faulting thread id
and address), enqueues it on the faulting process's `NotificationChannel`,
wakes the channel's waiters, and **terminates the faulting thread** (reusing the
normal thread-exit/reap path). A supervisor that holds the channel learns of the
crash; the kernel stays alive. A kernel-mode fault is still fatal (it dumps and
halts).

This is the **default-action** half of the design. The **debugger** half —
suspending the faulting thread, inspecting its registers
(`sys_thread_get_registers`), and resuming it (`sys_exception_resume` with a
`Disposition`) — is deferred until a real userspace supervisor exists (process
spawn), since `sys_exception_resume`'s only caller is a supervisor holding the
faulting thread's handle.

## Status (this slice)

- **Implemented:** `NotificationChannel`, the `Notification` value type +
  exception variants, `sys_notif_recv`, `sys_wait` over a channel, the overflow
  / exception-priority-eviction policy, and post-mortem exception delivery.
- **Deferred to their producers' slices:** `ChildExited` (process spawn + real
  exit), `PeerClosed` (IPC), the debugger suspend/resume path
  (`sys_exception_resume` + register inspection + the 30 s auto-terminate
  timeout + the debugger exception-channel priority chain), and per-process
  queue-capacity tuning via spawn flags. The variant discriminants for the
  deferred producers are defined now (the ABI), without producers.
- **Demo:** the kernel boot thread acts as a stand-in supervisor — it owns the
  first user process's channel, blocks on it via `sys_wait`, and reports the
  `SegFault` the process delivers when it deliberately faults.
