# libos — the typed, async userspace runtime

**Status:** Design (pre-implementation). Target: Phase 3 slice 5. Living document —
update as implementation reveals subtleties.

`libos` is the **typed, async face of the syscall surface**. Today every userspace
binary calls the raw `libkern` surface directly: bare `u64` handles, `IoResult`
decoded by byte offset, and a `po_wait` helper (submit → `sys_wait` → decode → close)
**copy-pasted verbatim into init, eshell, parent, and fs-server**. libos replaces that
with (a) `Handle<T, M>` typestate wrappers that make rights errors compile failures,
and (b) an `Op` future + `block_on` so an async I/O reads as `handle.read(buf).block_on()?`.

## Position in the stack

```
Application / services
  ↓
libstream   librsproto                 ← typed streams (deferred), RS protocol
  ↓
libos                                  ← THIS: Handle<T,M>, Op future, block_on
  ↓
libkern    libheap                     ← raw syscalls; the #[global_allocator]
  ↓
syscall instruction
```

libos depends on `libkern` (raw syscalls + ABI types) and `core` only. It is
**`#![no_std]` and uses no `alloc`** — see "The alloc-free constraint" — so the
heap-free binaries (eshell, parent, fs-server) can adopt it, not just init.

## Scope (slice 5) and non-scope

**In:** `Handle<T, M>` wrappers over the *solid* objects (Memory, IpcChannel,
Namespace, NotificationChannel, Entropy, PendingOperation); the `Op` future over
`sys_wait`; single-op async methods (`read`/`write`/`ns_lookup`/…); `block_on`; an
`io::Error`-shaped error.

**Out (deferred, deliberately):**
- **The multi-task `spawn`/run-loop executor.** It needs `alloc` (heterogeneous task
  storage) and has no consumer — init is sequential, eshell is a single read loop, and
  today's fs-server handles one request at a time. It lands with the first
  concurrency-heavy service. `block_on` (drive *one* future) covers every current
  caller. The design below is shaped so the multi-task executor is a clean addition,
  not a rewrite.
- **`Handle<Process>` / `Handle<Thread>` and syscap-gated calls** (thread/process
  spawn, affinity) — those wrap ABIs (`ThreadArgs`/`SpawnArgs` + SysCaps) that finalize
  in slices 6–7.
- **`libstream`** (typed structured I/O) — consumer-less until the shell/service-mgr
  era; wants its own wire-protocol/streaming design pass.

## The `Handle<T, M>` typestate model

The design is the one already committed on paper (`docs/history/os-design-v5.1.md`
§ "Handle typestate"); libos implements it. A handle is:

```rust
pub struct Handle<T, M> {
    raw: RawHandle,          // the 64-bit capability (libkern)
    extra: Rights,           // generic + modifier rights, checked at runtime
    _t: PhantomData<T>,      // object-type marker
    _m: PhantomData<M>,      // mode marker (principal rights, checked at compile time)
}
```

- **`T` — the object-type marker.** One zero-sized type per kernel object libos wraps:
  `Memory`, `Channel`, `Namespace`, `Notify`, `Entropy`, `Pending`. `T` fixes which
  operations *exist* (`Handle<Channel, _>` has `send`/`recv`; `Handle<Memory, _>` has
  `map`).
- **`M` — the mode marker.** Encodes the object's **principal** rights as a type, so
  the wrong operation is a *compile* error. Per-type mode tables (from v5.1):
  | Object | Modes |
  |---|---|
  | `Memory` | `MapRead`, `MapReadWrite`, `MapExec` |
  | `Channel` | `Send`, `Recv`, `SendRecv` |
  | `Namespace` | `NsReadOnly`, `NsMutable` |
  | `Notify` / `Entropy` / `Pending` | `Only` |
- **`extra: Rights` — the runtime band.** Generic rights (`DUPLICATE`/`TRANSFER`/
  `INSPECT`/`WAIT`) and modifier rights (`SEEK`/`APPEND`/…) are checked at runtime
  against `extra`, not encoded in `M`. They don't fit the mode lattice cleanly and the
  kernel re-validates every call anyway, so encoding them as types buys little. This is
  the deliberate compile-time/runtime split from v5.1.

**Operation gating — sealed marker traits.** Operations are gated by sealed traits the
mode types implement:

```rust
pub trait CanRead: sealed::Sealed {}      // MapRead, MapReadWrite, Recv, SendRecv, …
pub trait CanWrite: sealed::Sealed {}     // MapReadWrite, Send, SendRecv, NsMutable, …

impl<M: CanRecv> Handle<Channel, M> {
    pub fn recv<'b>(&self, buf: &'b mut IpcBuf) -> Op<'b, RecvOutcome> { … }
}
```

A `Handle<Channel, Send>` simply has no `recv` method in scope — misuse is caught by
the type checker. Sealing (`sealed::Sealed`) keeps the trait set closed to libos.

**Attenuation consumes `self`.** Narrowing rights returns a new, more-restricted handle
and invalidates the old one, backed by `sys_handle_restrict`:

```rust
impl Handle<Memory, MapReadWrite> {
    pub fn into_read_only(self) -> Result<Handle<Memory, MapRead>>;  // sys_handle_restrict
}
// generic, any T/M:
pub fn without_transfer(self) -> Result<Self>;
pub fn without_duplicate(self) -> Result<Self>;
```

**Construction / raw interop.** `Handle` is created from a `RawHandle` + `Rights` at the
trust boundary — the `_start` bootstrap words, an `ns_lookup` result, a received IPC
handle. These are `unsafe` constructors (`Handle::from_raw(raw, rights)`) because the
caller asserts the object type matches `T`; libos can `sys_handle_stat` to check the
type where it's cheap. `raw()`/`into_raw()` drop back to `RawHandle` for the raw paths
(spawn ABIs, IPC handle transfer) that still need it.

## The async model

**`Op<'b, R>` — the core future.** Every potentially-blocking call returns an `Op`: a
future that wraps an in-flight `PendingOperation` handle plus the borrowed completion
scratch, and resolves to `R` (bytes transferred, a resolved handle, `()`). It is
`#[must_use]`, borrows its output buffer for `'b`, and impls `core::future::Future`:

- **Constructed by** the async method (`handle.read(buf)`), which issues
  `sys_io_submit` immediately (the submit never blocks) and holds the returned PO.
  (A pre-signalled PO — zero-length / cache hit — is handled uniformly.)
- **`poll`**: reads the PO's completion. If complete, decode the 24-byte `IoResult`
  (`status@8`, `result@16`) into `Ready(Result<R>)`; else register the PO handle with
  the reactor (via the `Waker`) and return `Pending`.
- **`Drop`** closes the PO (and, for a still-in-flight op, is where `sys_io_cancel`
  would go once it's supported — today it's `Unsupported`, so drop-before-completion
  just closes the PO).

**`block_on` — the single-threaded reactor.** Alloc-free; drives one future to
completion:

```rust
pub fn block_on<F: Future>(fut: F) -> F::Output;
```

It polls `fut`; on `Pending`, it `sys_wait`s (deadline = forever, or a caller-supplied
one) on the waitable handles the pending `Op`s registered this round, then re-polls.
This *is* the executor for a single task — the same poll/wait loop a multi-task version
would run, minus the ready-queue. It collapses the copy-pasted `po_wait`: init and
eshell call `handle.read(buf).block_on()?` (or a `deadline`-bounded variant) instead of
hand-rolling submit/wait/decode/close.

**The future↔reactor handoff** goes through the standard `Waker`: libos owns both the
`Op` future and `block_on`, so it constructs the `Waker` (via stable `RawWaker` /
`RawWakerVTable` / `Waker::from_raw`) to reference a small **reactor** holding the
handles pending futures want waited on. *Implementation checkpoint (Part B):* confirm
the exact stable-Rust mechanism for a pending `Op` to hand its PO handle to `block_on`'s
`sys_wait` (Waker data pointer vs. a reactor reference threaded at `Op` construction) —
both are alloc-free; pick the one that stays on stable and reads cleanly.

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

Like `libheap`'s `ArenaSource`, the calls libos makes to the kernel are abstracted
behind a trait so the async machinery is host-testable with no kernel:

```rust
pub trait Sys {
    fn io_submit(res: RawHandle, op: &IoOp) -> isize;
    fn wait(handles: &[RawHandle], results: &mut [IoResult], deadline: u64) -> isize;
    fn handle_close(h: RawHandle);
    // …object-op calls as the wrappers need them
}
```

`cfg(not(test))` → the real libkern `syscallN` calls; `cfg(test)` → a scriptable mock
that completes POs after N polls, returns canned `IoResult`s, and records calls. Host
tests then cover: `Op` decodes `IoResult` correctly; `block_on` polls→waits→completes;
a pre-signalled PO returns without waiting; error status → `Error` mapping; `Handle`
attenuation issues the right `restrict`. (Typestate *misuse* is verified by
compile-fail intent, not runtime tests — the type system is the test.)

## The alloc-free constraint

libos core is `#![no_std]` with **no `alloc`**. `Handle<T,M>` is a `RawHandle` + `Rights`
+ `PhantomData`; the `Op` future lives on the caller's stack; `block_on` polls on the
stack. Nothing here needs a heap. This is a deliberate reach: the survey shows eshell,
parent, and fs-server are all heap-free (fixed `.bss`), and keeping libos core heap-free
lets them adopt `Handle<T,M>` + `block_on` without pulling in `libheap`. The one part
that genuinely needs `alloc` — the multi-task executor's task storage — is exactly the
deferred part; when it lands it is an `alloc`-gated module, not a change to the core.

## The thin-entry seam (design note, not slice-5 scope)

Today each bare binary rolls its own `_start` + `#[panic_handler]` (survey §6), with the
bootstrap register ABI (`rdi`=notif, `rsi`=root-ns, `rdx`=handle0/control, `rcx`=arg0)
hand-decoded per crate. A future consolidation of that glue into a thin libos entry
shim is where a std port's `lang_start` cutover would localize (std owns the real
entry/pre-main init). This slice does **not** consolidate `_start` — it dogfoods init/
eshell onto `Handle`/`block_on` — but the design records the seam so the eventual move
is one place, not five. (Note the load-bearing libkern `mem*` intrinsics any entry glue
must preserve.)

## ABI and dependencies

- **Not part of the kernel ABI version hash** — pure userspace over the public syscalls.
- Depends on `core` + `libkern`. **No `alloc`** in the core.
- Consumes only *solid*-tier syscalls (handles, memory, `sys_wait`, `io_submit`,
  notifications, channels, ns, entropy). The thread/process/authority calls it will
  wrap later depend on ABIs that finalize in slices 6–7.

## Consumers (dogfood)

- **init** migrates to `Handle<T,M>` + `block_on` for its sequential bootstrap and
  reaping loop — replacing raw `u64` handles, byte-offset `IoResult`/`Notification`
  decoding, and its `po_wait`/`ns_lookup_wait` copies. init keeps its critical-path
  rules (no panic/unwrap); libos surfaces errors as `Result`, never panics.
- **eshell**'s console read loop moves onto libos byte I/O + `block_on`, staying
  alloc-free.

## Open questions / deferred

- The exact stable-Rust `Waker`↔reactor plumbing (Part B checkpoint above).
- Whether the mode lattice needs `SendRecv`-style "both" modes as distinct types or a
  trait-bound union — settle when wrapping `Channel`.
- Multi-task executor (`spawn`, ready-queue, `select`-like joins) — deferred to the
  first concurrent consumer; `alloc`-gated.
- `libstream` — separate slice + wire-protocol design pass.
- Consolidating `_start`/panic glue into a libos entry shim — future, tied to the std
  port seam.

## References

- `docs/history/os-design-v5.1.md` § Handle typestate — the authoritative `Handle<T,M>`
  design libos implements.
- `docs/architecture/handle-system.md`, `docs/spec/handle-encoding.md` — the kernel
  handle table + `Rights` this wraps.
- `docs/spec/syscall-abi.md` — `sys_wait`, `sys_io_submit`, `IoOp`/`IoResult`.
- `docs/rationale/why-async-syscalls.md` — the async-first model libos surfaces.
- `docs/architecture/overview.md` § Runtime libraries; `docs/planning/implementation-plan.md`
  slice 5; the 2026-07-13 decision-log entries (userspace-runtime sequencing; libstream
  + multi-task-executor deferral).
