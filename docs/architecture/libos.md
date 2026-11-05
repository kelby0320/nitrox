# libos — the typed, async userspace runtime

**Status:** Implemented. `userspace/libos` is the safe layer every userspace crate is to reach the
kernel through: typed handles and borrows, messages, waits, mappings, namespaces and files,
spawning and threads, the clocks, the log and the bootstrap registers, over the `Op` future and
`block_on`. Thirty-three crates use it directly: the libraries, the system's servers and
supervisors, and the desktop and the programs; the test harness moves onto it in pre-Phase-7 Part
E.5, and `cargo xtask check-syscalls` holds each crate's raw syscalls to its count as it does.
Sections marked *deferred* (chiefly the multi-task executor) are genuinely unbuilt. Verified
2026-11-04.

`libos` is the **typed, async face of the syscall surface**. Before it, every userspace binary
called the raw `libkern` surface directly: bare `u64` handles closed by hand on every path,
`IoResult` decoded by byte offset, a `po_wait` helper copied into fifteen crates, messages built in
`static mut [u8; 4096]` buffers, and mappings kept as bare addresses. libos makes each of those a
type that cannot be misused, or whose misuse needs `unsafe`.

## Position in the stack

```
Application / services
  ↓
libstream   librsproto                 ← typed streams (TSM1), RS protocol
  ↓
libos                                  ← THIS
  ↓
libkern    libheap                     ← raw syscalls; the #[global_allocator]
  ↓
syscall instruction
```

libos depends on `libkern` (raw syscalls + ABI types) and `core` only. It is **`#![no_std]` and
uses no `alloc`**, keeps nothing that must be started, and makes no call until asked: what makes
it fit for supervisors and PID 1 (decision log, 2026-10-27). `eshell` and `libheap` stay on
`libkern` by choice, and so do the test harness's refusals, a call that passes what a safe wrapper
cannot express; `tools/xtask/raw-syscalls.toml` lists them.

**It is written to the syscall surface Part E ends with** (`docs/planning/syscall-surface.md`),
over today's calls, so a caller is written once: `Namespace::flush` and `fs::flush` are `async`
though today's calls answer within themselves, `Spawn::path` hands the kernel a file it reads
within the spawn, and `entropy::fill` makes a source for each call. Each is
`TODO(surface-shape)`, and changes underneath as its sub-part lands.

## Handles: owned, borrowed, and in flight

```rust
pub struct Handle<T, M> { raw: RawHandle, extra: Rights, .. }   // owned: closed on drop
pub struct Ref<'a, T, M> { raw: RawHandle, extra: Rights, .. }  // borrowed: Copy, closes nothing
pub struct Handles<const N: usize>                               // owned, on their way somewhere
```

- **`T` — the object-type marker**: `Memory`, `File`, `Namespace`, `Notify`, `Channel`,
  `Resource`, `Process`, `Thread`. It fixes which operations exist.
- **`M` — the mode marker**, the object's **principal** rights as a type, so the wrong
  operation is a compile error: `MapRead`/`MapReadWrite`/`MapExec` for memory and files,
  `NsReadOnly`/`NsMutable` for namespaces, `ReadOnly`/`WriteOnly`/`ReadWrite` for a resource, and
  `Only` where no consumer distinguishes modes (a channel, a process, a thread). Operations are
  gated by sealed traits (`CanLookup`, `CanBind`, `CanMapWrite`, …).
- **`extra: Rights` — the runtime band**: the generic and modifier rights, which the kernel
  checks on every call.

**A `Handle` owns, a `Ref` borrows.** A `Ref` is `Copy`, has no `Drop`, and lives no longer than
the handle it borrows (`Handle::as_ref`, or `From<&Handle>`); it cannot attenuate, so no borrow
narrows its owner's capability in place. Every operation is written once for both, by the
`both!` macro in `handle.rs`. Building either from a raw value is `unsafe` for its **ownership**
claim, as `std`'s `OwnedFd::from_raw_fd` and `BorrowedFd::borrow_raw` are: a wrong `T` or `M`
claims nothing memory rests on, as the kernel type- and rights-checks every call, so `lookup` is
safe and a mistaken type answers `Err` at its first use.

**Attenuation consumes `self`**, through `sys_handle_restrict`: `without_transfer`,
`without_duplicate`, and `into_read_only` for memory and files. `duplicate(&self, rights)` makes
a second handle, needing `DUPLICATE`; `stat(&self)` reads `HandleInfo`, needing `INSPECT`.

**`Handles<N>` holds handles a send or a spawn moves, or a receive brought**, and closes each
still there when it drops. `push` takes a `Handle` (handed back when full); `take::<T, M>(i)`
gives one out once.

## The safe layer

| Module | What it holds |
|---|---|
| `memory` | `Handle<Memory, MapReadWrite>::create`, and `map(len) -> Mapping<M>` on memory or a file |
| `channel` | `Msg`, and a `Channel` endpoint's `pair`, `send`, `send_held`, `send_by` and `recv` |
| `wait` | `WaitSet<N>`, borrowing up to `N` handles or operations, and `Signalled`, which of them a wait found |
| `ns` | a namespace's `create`, `lookup`, `start_lookup`, `copy`, `bind`, `bind_server`, `unbind`, `bindings`, `flush` and `held` |
| `fs` | `create`, `grow`, `truncate`, `rename` and `flush`: a file by path |
| `process` | `Spawn`, by image or by path, and `exit` |
| `thread` | `Stack`, `start`, the calling thread's `exit`, and a thread suspended on a fault: its `fault_registers`, and `settle`, to resume it or end it |
| `time` | `now`, `wall`, `sleep`, `sleep_until`, `Deadline`, and a `Timer`'s `create`, `set` and `disarm` |
| `log` | `write`, and `Line`, one write a line |
| `entropy` | `fill`, and `try_fill` for a caller that cannot wait for the seeding |
| `boot` | `Start`: `_start`'s four registers, owned and typed |
| `control` | a service's control channel, read: an op, nothing, or closed and taken |
| `system` | `stop`, the machine halted or reset with the system-control object; `set_clock`, with the clock grant; and `test_verdict` |
| `io` | a `Resource`'s `read` and `write`, and a notification queue's `recv` |

**A `Mapping` lends no reference to its bytes.** Another process mapping the same object, or a
device the kernel lent it to, may write them at any time, and a Rust reference promises nothing
does while it lives. So it copies: `read_into`, `write_from` and `fill`, each refused
`InvalidArgument` past the end. A caller who knows nothing else writes them takes a slice with
`unsafe fn assume_unshared`. It unmaps itself on drop; `unmap` does it now and says what the
kernel answered, which a drop cannot; `into_raw` keeps it for the process's life, as the
framebuffer's aperture is kept. **libdraw reads a buffer another process shares through
`Pixels::Shared`**, a piece of a row copied out at a time, so a compositor never borrows a
client's pixels.

**A send never loses a handle, and a receive never leaks one.** `send` takes the handles from a
`Handles`, which the kernel moves only when the message is queued or held; a refused send leaves
them there, to retry or to close. `recv` answers what the message brought as a `Handles`, so a
handle a sender attached and the receiver never asked for is closed, and its failure is a
`KError`: `WouldBlock` for an empty ring, `PeerClosed` once the peer has gone and nothing is left.
Any other refusal, such as `OutOfMemory` for a full handle table, leaves the message queued, so a
wait on the channel fires again at once, and a loop that waits again without a pause spins.
`Msg::payload` is bounded by what a message holds whatever its header says, and `set_len` refuses
past it. `send_held` and `send_by` answer an `Op` that completes when the peer has the message,
at once for a reply on a server's forwarding endpoint, which the kernel takes within the call;
`send_by`'s, given up at its deadline, completes `TimedOut`, the message and its handles dropped.

**A `WaitSet` borrows what it waits on**, so nothing in it closes while it is waited on, holds at
most `MAX_WAIT_HANDLES`, by a compile-time check, and refuses a handle twice. Its `wait` answers
the indices that signalled, with each one's `IoResult`; a poll that found nothing and a deadline
that passed are empty answers, not errors. **An operation in flight waits beside handles**:
`add_op` puts an `Op` in the set, and `Op::completion_in` takes what the wait read for it, so a
server that keeps a device read outstanding while it serves its channels costs no second wait, and
an `Op` whose completion was taken waits for nothing as it drops. `Namespace::start_lookup` gives
a lookup the same shape, its handle taken from the wait that found it.

**A spawn moves its grants or hands them back.** `Spawn::grant` takes a `Handle`, which leaves
this process when the child starts and comes back in `Refused` if the kernel refuses: it starts a
child whole or not at all. `grant_copy` borrows one, which needs `TRANSFER` and `DUPLICATE`. The
first grant is the child's `rdx`. **Without `namespace()`, the child shares this process's root**,
`LOOKUP` only: it is not a copy, so a later bind in it reaches the child. The older
`spawn(&SpawnArgs)` stays for the callers not yet moved.

**A thread's stack is its own for the rest of the process's life**: nothing tells a process that
a thread of its has ended, so nothing could give the stack back safely. `Stack::new` is one
mapping each, and `start` hands the kernel its top, which enters every thread with `rsp` 8 below a
16-byte boundary, as a `call` would leave it (`enter_user`). A thread's entry is `extern "C"
fn(u64, u64, u64, u64) -> !`, `rcx` its `arg0`. An overrun lands in the mapping below until a
hint-less map leaves a gap there, `TODO(thread-stack-guard)`.

## The async model

**`Op` — the core future.** Every potentially-blocking call is an `Op`, or an `async fn` that
awaits one: a future that wraps an in-flight `PendingOperation` handle and resolves to its
`IoResult`, which the method turns into what it answers (bytes transferred, a resolved handle,
`()`). It is `#[must_use]` and impls `core::future::Future`. It is one of three kinds, by what
its drop must see to: an I/O's, a lookup's, or one whose completion is a status alone, such as
a held send's.

- **Constructed by** the async method (`handle.read(..)`), which issues
  `sys_io_submit` immediately (the submit never blocks) and holds the returned PO.
  (A pre-signalled PO — zero-length / cache hit — is handled uniformly.)
- **`poll`**: **under `block_on`, an operation costs one wait.** The first poll registers the PO
  with the reactor (via the `Waker`) and returns `Pending`, making no call: the wait after it reads
  a PO done already. A later poll takes what that wait read for its PO and decodes the 24-byte
  `IoResult` (`status@8`, `result@16`) into `Ready(Result<R>)`. A poll that finds nothing read
  for it, and every poll under another executor, reads the completion with a non-blocking
  `sys_wait`: complete is `Ready`, `WouldBlock` registers again, and any other error completes
  the op with it, so a PO that can no longer be waited on is not polled for ever. A completion
  read twice costs only the second wait: a lookup's handle is installed by the first read, and
  every later read reports the same handle. A second poll after `Ready` is an error and waits on
  nothing: the result, which may be a handle, is handed out once.
- **`Drop`** closes the PO, **first waiting for an unfinished I/O**: `sys_io_cancel` is
  `Unsupported`, and the device may still be writing the buffer the dropped future borrowed,
  which the caller would otherwise get back while it is written. A wait refused with
  `OutOfMemory` (the PO's waiter queue full) is slept on and tried again; any other refusal is
  for good. A lookup waits for nothing, since the kernel releases a lookup whose PO is closed
  unread; but a wait that read the PO has installed its handle, so the drop polls once and
  closes the handle a successful lookup reports.

**`block_on` — the single-threaded reactor.** Alloc-free; drives one future to
completion:

```rust
pub fn block_on<F: Future>(fut: F) -> F::Output;
```

It polls `fut`; on `Pending`, it `sys_wait`s for ever on the waitable handles the pending
`Op`s registered this round, keeps what that wait read for the poll after it alone, then
re-polls. A round can register eight (the kernel's
`MAX_WAIT_HANDLES` is larger, but `block_on` keeps eight). One that had to leave a handle out,
or in which an `Op` was polled through another waker and so registered nothing, waits only
until a deadline a millisecond off, so that operation is polled again. A round with nothing
registered, or whose wait failed, sleeps that millisecond rather than spinning, in a `sys_wait`
on no handles, which the kernel never refuses. Deadlines resolve on the kernel's 10 ms tick, so
each of these takes up to a tick.
This *is* the executor for a single task — the same poll/wait loop a multi-task version
would run, minus the ready-queue. A sequential caller writes `block_on(handle.read(..))?` in
place of a hand-rolled submit, wait, decode and close.

**The future↔reactor handoff** goes through the standard `Waker`: libos owns both the
`Op` future and `block_on`, so it constructs the `Waker` (via stable `RawWaker` /
`RawWakerVTable` / `Waker::from_raw`) whose data pointer is a small **reactor** holding the
handles pending futures want waited on. **`Op::poll` reads that pointer only from a waker
whose vtable is `block_on`'s own**, which nothing else can name; another executor's waker, or
`Waker::noop()`, is woken instead, with nothing registered, and the poll is counted, so
`block_on` waits with a deadline that round. A clone of `block_on`'s waker gets a vtable of its
own, since it may outlive the reactor, and so is never read as one. The reactor's slots are
atomics, as the `Waker` is `Sync` and an `Op` may be polled with it on another thread.

**Why a future at all, if slice 5 only `block_on`s one at a time?** Because `Op:
Future` is what a later multi-task executor drives, and what `async`/`await` desugars
onto — so the ergonomic and forward-compatible shape is a real `Future`, with `block_on`
as the degenerate single-task driver we need now.

## Error model

`libos::Error` wraps a `KError` and is shaped like `std::io::Error`: a `kind()` that
maps to an `io::ErrorKind`-analog, `From<KError>`, and `Display`. `type Result<T> =
core::result::Result<T, Error>`. Deliberately std-shaped so a future `std::io::Error`
facade re-exports rather than adapts (the "std-shaped where free" rule).

## The host-test syscall seam

The calls libos makes to the kernel are a private module, `sys`, so everything above it is
host-testable with no kernel. Its functions take typed arguments (a path as `&[u8]`, an `&mut
NsEntry`), so what the kernel is handed is what the types say; `sys::wait` waits on no more
handles than its results hold, cutting both in code the host build runs too. `cfg(not(test))` →
the real `libkern` `syscallN` calls, and a receive through `libkern::channel_recv_raw`, the one
place `check-recv` allows; `cfg(test)` → `sys/mock.rs`, **a small model of the kernel that
answers as it does**: a handle table, objects whose bytes are real host memory a mapping reads,
channels with rings and peers and a server's forwarding endpoint, namespaces of direct bindings
and of servers, an endpoint bound as one and its lookups forwarded, which complete when time
passes, and each call's refusals in the kernel's order — `InvalidHandle`, then `NoAccess` for a
missing right, then `InvalidArgument`. It panics on a wait for ever that nothing
will end, a loop of waits that never does, and an `exit`, and counts a close of a handle that is
not there. Every wrapper is tested through it, and each guarantee above has had its test run
against a copy of the code with that guarantee removed. (Typestate *misuse* is verified by
compile-fail intent, not runtime tests: the type system is the test.)

**A crate above libos takes the same model for its own host tests**, through the `mock` feature in
its dev-dependencies: `libos = { path = "../libos", features = ["mock"] }`. `libos::mock` then
holds the model's controls. **Only an unwinding build takes it**: the model needs `std`, which a
binary cannot link beside its own panic handler, and every binary here aborts, so a build with
every feature on still links the real calls into each one. A test build always unwinds. A host
test thread starts with a fresh model, as libtest gives each test a thread of its own.

## The alloc-free constraint

libos core is `#![no_std]` with **no `alloc`**. `Handle<T,M>` is a `RawHandle` + `Rights` +
`PhantomData`; the `Op` future lives on the caller's stack; `block_on` polls on the stack; a
`Msg` is one 4 KiB page, on the stack too. Nothing here needs a heap, so the filesystem servers and
`auth-service` can adopt it without `libheap`. The one part that needs `alloc` — the
multi-task executor's task storage — is the deferred part; when it lands it is an `alloc`-gated
module, not a change to the core.

## ABI and dependencies

- **Not part of the kernel ABI version hash** — pure userspace over the public syscalls.
- Depends on `core` + `libkern`. **No `alloc`** in the core.

## Open questions / deferred

- Multi-task executor (`spawn`, ready-queue, `select`-like joins) — deferred to the first
  concurrent consumer; `alloc`-gated.
- A `Process` mode lattice, and a channel's: no consumer distinguishes them yet.
- Consolidating `_start`/panic glue into a libos entry shim — `boot::Start` takes the registers,
  but each crate still writes its own `_start`, the seam a `std` port's `lang_start` replaces.

## References

- `docs/archive/os-design-v5.1.md` § Handle typestate — the `Handle<T,M>` design libos implements.
- `docs/architecture/handle-system.md`, `docs/spec/handle-encoding.md` — the kernel
  handle table + `Rights` this wraps.
- `docs/spec/syscall-abi.md` — every call the seam makes.
- `docs/rationale/why-async-syscalls.md` — the async-first model libos surfaces.
- `docs/planning/safety-survey.md` § P7 — the classes the safe layer answers;
  `docs/planning/syscall-surface.md` § E.1 — this layer, and the ratchet.
