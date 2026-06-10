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

## Exception delivery (suspend + supervised resume/terminate)

When a thread faults in ring 3 (page fault, invalid opcode, divide error, …),
the kernel — instead of halting — builds the matching `Notification`
(`SegFault` / `IllegalInsn` / `DivideByZero`, carrying the faulting thread id
and address), enqueues it on the faulting process's `NotificationChannel`, wakes
the channel's waiters, and **suspends the faulting thread** (a new `Suspended`
scheduler state; its `ExceptionFrame` stays preserved on its kernel stack). The
kernel stays alive. A kernel-mode fault is still fatal (it dumps and halts).

A **supervisor** — a sibling thread holding the faulter's `Thread` handle (from
`sys_thread_create`) — then decides the outcome:

- `sys_thread_get_registers(thread, out)` reads the suspended thread's captured
  registers (from the frame on its kernel stack) for diagnosis.
- `sys_exception_resume(thread, disposition, code)` either **resumes** it
  (disposition `0`: the thread re-enters the faulting instruction — meaningful
  once the supervisor has repaired the fault's cause) or **terminates** it
  (disposition `2`, exiting with `code`).

Mechanically, suspend is just a context-switch away from the faulting thread
(like blocking in `sys_wait`); the `ExceptionFrame` the entry stub built survives
on the frozen kernel stack, and a resume returns up through the dispatcher to the
stub, which `iretq`s the (possibly retried) frame. The uniform stub epilogue
makes *every* user-fault vector suspendable, not just `#PF`.

**Deferred to Phase 2** (the debugger extras): the `ResumeSkip` /
`ModifyAndResume` dispositions, the 30 s auto-terminate timeout
(`sys_exception_extend_timeout`), and the debugger exception-channel priority
chain.

## Status (this slice)

- **Implemented:** `NotificationChannel`, the `Notification` value type +
  exception variants, `sys_notif_recv`, `sys_wait` over a channel, the overflow
  / exception-priority-eviction policy, and **suspend + supervised
  resume/terminate** exception delivery (`sys_thread_get_registers` /
  `sys_exception_resume`, dispositions Resume + Terminate).
- **`ChildExited`:** `sys_process_exit` / `sys_thread_exit` deliver
  `ChildExited { pid, status }` to the parent's notification channel (the
  `parent_notif` recorded on the child `Process` at spawn), at exit time so a
  parent blocked in `sys_wait` wakes promptly. `sys_thread_exit` fires it only
  for a process's **last** thread; `sys_process_exit` tears down the siblings
  first and always fires it.
- **Deferred to Phase 2:** `PeerClosed` is produced (IPC slice); the debugger
  extras (`ResumeSkip` / `ModifyAndResume`, the 30 s auto-terminate timeout, the
  debugger exception-channel priority chain) and per-process queue-capacity
  tuning via spawn flags remain. Their variant discriminants are defined now
  (the ABI), without producers.
- **Demo:** the `parent` process (PID 1) acts as the supervisor — it
  `sys_thread_create`s a worker thread that deliberately faults, receives the
  `SegFault` on its notification channel, reads the worker's registers, prints
  the faulting `rip`, and terminates the worker via `sys_exception_resume`.
