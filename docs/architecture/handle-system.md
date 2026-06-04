# Handle system

The Nitrox kernel uses **handles** as the sole names for kernel
objects. A handle is a 64-bit opaque integer; the kernel-side state
that backs it is a single row in the **handle table** described here.

This document covers the implementation in `kernel/src/handle/`. For
the wire format and validation algorithm — both normative — see
[`docs/spec/handle-encoding.md`](../spec/handle-encoding.md). For why
the kernel uses capability handles rather than UID/path-based
authority, see [`docs/rationale/why-capabilities.md`](../rationale/why-capabilities.md).

Status: Phase 1 substrate. Brings up the table, lookups, allocation,
close + deferred reclamation, and the rights model. The kernel-object
substrate that handles *point at* (`KObjectHeader`, `Process`,
`Thread`, ...) is the next slice; this slice ships the type-erased
plumbing those objects will plug into.

## What a handle is, and what it is not

A handle is a 64-bit capability:

- **Identifies** one kernel object the calling process owns.
- **Carries** a set of operations the process may perform on that
  object — the [rights bitmask](#rights-model).
- **Lives** in a per-process slot of the (globally numbered)
  segmented table, with an `owner_pid` checked on every lookup.

A handle is *not* a kernel object itself. The handle table is the
**capability lookup layer**: handles point at kernel objects, but
they have no per-instance data of their own beyond what is stored in
the table entry. The kernel-object substrate (`KObjectHeader`,
`ObjectRef`, the per-type structs) lives in its own module,
`kernel/src/object/` — see [`crate::object`] and
`docs/architecture/overview.md` § "Kernel objects".

## Module layout

| Path                                    | Role                                                                |
|-----------------------------------------|---------------------------------------------------------------------|
| `kernel/src/libkern/handle.rs`          | `RawHandle`, `Rights`, `KObjectType` — pure value types, shared    |
|                                         | with userspace later                                                |
| `kernel/src/handle/mod.rs`              | Module entry; `try_acquire_refcount`/`release_refcount` seam;       |
|                                         | `current_ctx_id` shim                                               |
| `kernel/src/handle/entry.rs`            | `HandleEntry` (cache-line, 64 B, seqlock + atomics);                |
|                                         | `WriteGuard`, `read_snapshot`                                       |
| `kernel/src/handle/segment.rs`          | `SegmentEntries` block allocation, Fisher-Yates freelist init       |
| `kernel/src/handle/prng.rs`             | `Xorshift64` — small PRNG for freelist shuffles                     |
| `kernel/src/handle/grace.rs`            | `GraceTracker` — RCU-style deferred reclamation                     |
| `kernel/src/handle/type_rights.rs`      | Allocation-time rights/type compatibility matrix                    |
| `kernel/src/handle/table.rs`            | `HandleTable` — public API, the rank-3 lock                         |

## Storage layout

The table is a two-level structure:

```text
HandleTable
  ├── directory: [AtomicPtr<SegmentEntries>; 256]   ← lock-free reads
  ├── grace: GraceTracker (lock-free atomics)
  └── inner: SpinLock<Inner>                        ← rank 3
        ├── segment_meta: [SegmentMeta; 256]         (free_head, free_count per seg)
        ├── segments_count: u32
        ├── defer_ring: DeferredQueue                (256-slot ring)
        └── prng: Xorshift64

SegmentEntries (one per allocated directory slot)
  └── [HandleEntry; 4096]                            ← exactly 256 KiB
```

- **Directory:** an inline `[AtomicPtr; 256]`. Each non-null slot
  points at a fully-initialised `SegmentEntries` block. Lookups go
  `directory[seg_id].load(Acquire)` and follow the pointer.
- **Segment growth:** segments are allocated on demand. The first one
  is grown eagerly at `HandleTable::try_new` time so the first
  allocation does not pay grow latency. A segment is exactly 256 KiB
  (4096 × 64-byte entries), one buddy order-6 block — sized so the
  allocator has no rounding waste.
- **Per-segment metadata** lives in `Inner.segment_meta` rather than
  inline in `SegmentEntries`. Inlining would inflate the segment to
  256 KiB + 8 bytes, rounded by the buddy to 512 KiB — half wasted.
- **Total cap:** 256 segments × 4096 slots = 1,048,576 handles per
  table, matching the spec's `DIRECTORY_LEN × SEGMENT_LEN`.

Per spec § "Handle table entry layout", each `HandleEntry` is exactly
64 bytes, cache-line aligned. A compile-time `const _ = assert!`
catches drift.

## Concurrency model

Two layers of synchronisation, by design.

### Rank-3 spinlock — for table-structural changes

A single `SpinLock<Inner>` serialises allocate / close / restrict /
duplicate / segment-grow bookkeeping. It is rank 3 in
[`kernel/docs/lock-ordering.md`](../../kernel/docs/lock-ordering.md):
above the rank-4 `AddressSpace` lock so a future syscall can hold an
`AddressSpace` lock while consulting the handle table, and well above
the rank-6 allocators because segment growth must drop the lock
before calling `kmalloc`.

`allocate` and `close` both `drain_expired()` first thing under the
lock, returning any deferred-close slot whose grace period has
elapsed to its segment's freelist.

### Per-entry seqlock — for the lookup hot path

`HandleEntry::seq` is an `AtomicU32` toggled even → odd → even by
writers, with the metadata fields (`generation`, `owner_pid`,
`rights`, `object_type`) read non-atomically between matching even
seq values. Readers never take the rank-3 lock; the only mutex they
touch is the [`GraceTracker`]'s per-context counter, which is itself
lock-free.

The single-writer precondition (only one writer in the odd-seq
window) is enforced externally: every writer holds the rank-3 lock.
The seqlock writer's first load is therefore `Relaxed`; a debug
assertion fires if it ever observes an odd value on entry.

`HandleEntry::object` is its own `AtomicPtr<()>`, separate from the
seqlock-guarded metadata, so lookup step 6 ("is the object non-null?")
is a single `Acquire` load outside the retry loop.

### Segment growth — the lone rank cross

`grow_one` is the only path that spans the rank-3 → rank-6 boundary.
The sequence is **drop the lock, allocate the segment, reacquire the
lock, publish or discard**. If two threads race to grow the same
segment id, the loser frees its spare and the outer `allocate` loop
retries — segments are 256 KiB but races are rare (and impossible on
Phase 1's single CPU).

## Allocation path

```rust
allocate(owner_pid, object, object_type, rights) -> RawHandle
```

1. Reject if `rights` are not valid for `object_type` per the
   compatibility matrix.
2. Loop: take the rank-3 lock, `drain_expired()`, then walk segments
   from `next_segment_hint` looking for one with `free_count > 0`.
3. Pop the segment's freelist head, bump the entry's generation
   counter, write metadata under a [`WriteGuard`], publish the object
   pointer last with `Release`.
4. If every segment is full, drop the lock, `grow_one()`, retry.

Successive allocations from a fresh segment return slot indices in
**shuffled order** — Fisher-Yates over `0..SEGMENT_LEN` at segment
init produces a freelist whose pop order is pseudo-random. Combined
with the 32-bit generation counter and the owner-PID check, this
defeats handle guessing.

The shuffle PRNG ([`Xorshift64`]) is seeded from a value supplied at
table construction. Production code will seed from `RDTSC` at boot;
the entropy slice will later re-seed from `RDRAND/RDSEED`. Seed
quality affects only the visible distribution of issued slot indices,
never correctness or safety.

## Lookup path

```rust
lookup(handle, caller_pid, required_rights) -> LookupOk
```

Implemented in `HandleTable::lookup`, matching spec § "Validation
algorithm" step-for-step.

1. `enter_read` on the grace tracker.
2. `handle != NULL`.
3. `seg_id < DIRECTORY_LEN`.
4. `directory[seg_id]` is non-null (Acquire load).
5. `slot_id < SEGMENT_LEN`.
6. `read_snapshot(entry)` — seqlock loop returning a consistent
   metadata tuple plus the observed seq value.
7. `entry.object` is non-null (Acquire load).
8. `try_acquire_refcount(object, type)` — the **ObjectRef seam**
   (see below).
9. Re-read seq; if it changed or is odd, release refcount and retry
   the inner loop.
10. `generation` matches the handle's encoded generation.
11. **`owner_pid` matches `caller_pid`** — the security-critical step.
12. `required ⊆ entry.rights`.
13. Return.

Every error path that has already incremented the refcount releases
it before returning.

### ObjectRef seam

Step 8 calls `try_acquire_refcount(*mut (), KObjectType) -> bool`, which
reads the `KObjectHeader` at offset 0 of the object pointer and bumps
its refcount with `Arc`-upgrade semantics (failing if the count was
already zero — the object is being torn down). Error/retry paths call
`release_refcount(*mut (), KObjectType)`, which drops the reference and
runs the object's destructor (dispatched on `KObjectType`) if it was the
last. On success, step 13 wraps the acquired reference in an `ObjectRef`
(the RAII holder in `kernel/src/object/`); `lookup` returns
`LookupOk { object: ObjectRef, rights }` and dropping the `LookupOk`
releases the reference. A `#[cfg(test)] FAIL_NEXT_ACQUIRE` flag still
forces the step-8 failure branch deterministically. See
[`docs/spec/abi-version-hash.md`](../spec/abi-version-hash.md) for the
`KObjectHeader` layout and `kernel/src/object/header.rs` for the
ownership model.

## Close + deferred reclamation

```rust
close(handle, caller_pid) -> ClosedObject
```

1. Take the rank-3 lock; validate generation, owner, non-null object.
2. Under a [`WriteGuard`], `object.store(null, Release)`. **Generation
   is not bumped** — per spec § "Generation counter behavior", the
   bump happens on the *next allocation* that reuses the slot.
3. Snapshot the grace tracker's `current_epoch` as `deferred_epoch`
   **before** draining (drain advances the epoch).
4. `drain_expired()`. Then push the new `DeferredClose { handle,
   epoch: deferred_epoch }` onto the ring.

The returned `ClosedObject(*mut (), KObjectType)` carries the previous
object pointer **and its type**, transferring the handle's one
reference to the caller; `close` itself does **not** decrement. The
caller releases it via `ObjectRef::from_raw(ptr, ty)` + drop after
`close` returns. Keeping the decrement out of `close` keeps object
destruction (which calls `kfree`, a rank-6 allocator lock) off the
rank-3 handle-table lock, and makes a racing `lookup` safe: the slot's
reference is conceptually live until the caller takes it, so a
concurrent `try_acquire` always observes a positive count (pins the
object) or zero (object dying). The wrapper type makes
`Result<ClosedObject, _>` Send-able for closures that span thread
boundaries; bare `*mut ()` is `!Send` and would poison any closure
that touched it.

### Why deferred reclamation

A reader that has just observed `entry.object != null` (step 7 of
the algorithm) may still be using the pointer when close runs. If
the slot returned to the freelist immediately, the next allocation
could install a new object over the same slot and the reader would
have a stale pointer to live memory belonging to someone else.

The grace mechanism solves this: the slot waits on
`defer_ring` until every context that may have been inside a lookup
critical section at `deferred_epoch` has either quiesced or moved on
to a strictly later epoch. Only then does the slot return to its
segment's freelist.

### Defer ring sizing

`DEFER_RING_CAPACITY = 256`. Sized to absorb a burst of closes
between `allocate` / `close` drain opportunities. On overflow, close
releases the rank-3 lock, calls `yield_for_grace` (`spin_loop` in
production, `std::thread::yield_now` in hosted tests), reacquires the
lock and retries. In Phase 1's single-CPU world the loop body never
executes: the closing thread is the only possible reader and any
prior lookup it ran has already quiesced by the time `close` is
called, so the very first drain frees a slot. Multi-thread host
stress tests exercise the backoff path: a reader spinning on
`read_snapshot`'s seqlock under sustained writer pressure may not
have quiesced yet, and yielding lets it complete.

## Grace tracking

[`GraceTracker`] is keyed by **context id**, an opaque `u32`. What a
context represents is intentionally vague:

- Phase 1 single-CPU pre-Process: every operation runs in context 0.
- SMP (Phase 3): `current_ctx_id()` returns the CPU id.
- Per-process (post-`Process` slice): the calling process's
  `ctx_id`.

The tracker is an `AtomicU64 current_epoch` plus an array of
`AtomicU64 ctx_observed[256]`. A reader entering a critical section
writes the current epoch into its slot; on exit it sets the high
bit (`QUIESCED_BIT`). `is_grace_period_past(deferred)` walks every
context's slot and returns `true` only when every context is either
quiesced or at a strictly later epoch.

`current_ctx_id` is a private free function in `kernel/src/handle/mod.rs`
with `#[cfg(not(test))]` / `#[cfg(test)]` branches. Replacing it
under SMP or Process is the one place that needs to change; the rest
of the handle table is unaffected.

The mechanism is correct on the single-CPU pre-Process kernel even
though it appears degenerate: the closing thread quiesces (by
returning from its previous lookup, if any) before the close itself
runs, so the first drain after each close immediately frees.

## Rights model

```rust
pub struct Rights(u64);

// Generic (bits 0–3)
DUPLICATE | TRANSFER | INSPECT | WAIT

// Principal (bits 8–19) — type-specific
READ | WRITE | EXECUTE | SIGNAL | TERMINATE | LOOKUP | BIND
| MAP_READ | MAP_WRITE | MAP_EXEC | SEND | RECV

// Modifier (bits 32–37)
SEEK | APPEND | TRUNCATE | UNBIND | ENUMERATE | INSPECT_MEMORY
```

Subset semantics live on `Rights`: `r1.is_subset_of(r2)` iff
`(r1 & r2) == r1`. The handle table never amplifies rights;
`restrict` and `duplicate` both intersect.

`is_rights_compatible(KObjectType, Rights)` enforces the spec's
type-rights compatibility matrix at allocate time, rejecting
nonsensical combinations (e.g. `MAP_WRITE` on a `Process` handle).
Generic rights are valid on every type; modifier rights are not
constrained per type today.

## Per-process owned-handle list

`HandleEntry::next_owned` is reserved for an intrusive linked list
threading every handle a process owns, used at process exit to
release them all. The field is **declared but unused** this slice:
`allocate` writes `RawHandle::NULL`, `close` ignores it. The `Process`
slice wires it up.

## Phase 1 limitations

- `current_ctx_id()` returns 0 in production builds. SMP / Process
  will plug in real ids.
- The PRNG seed comes from a caller-supplied `u64`. Production code
  will seed from `RDTSC`; the entropy slice swaps to `RDRAND/RDSEED`.
- `next_owned` field exists; the list it threads is not built. The
  owned-handle list (release-at-exit) is wired up in the `Process`
  slice, not the handle-syscalls slice.
- No per-process quota enforcement. The spec's "soft cap" of 65,536
  handles per process is unenforced until `Process` exists.
- `sys_handle_close`, `sys_handle_duplicate`, `sys_handle_restrict`,
  `sys_handle_stat` **are** now exposed (stable syscall numbers 0–3),
  backed by a single global `HandleTable` instance
  (`kernel/src/handle/global.rs`); the dispatcher resolves the caller's
  pid via `sched::current_owner_pid`. They are first exercised from ring 3
  in the Memory objects slice (userspace's first handle is minted by
  `sys_memory_create`).

## Invariants exercised by host tests

The host test suite (`kernel/src/handle/**/tests`) verifies:

- `HandleEntry` is exactly 64 bytes, 64-byte aligned (compile-time
  assert + runtime).
- Allocate → lookup → close round-trip; cross-pid lookups return
  `NotOwner`; insufficient or superset rights return `NoAccess`;
  subset rights succeed.
- Close makes the handle invalid; double-close returns
  `InvalidHandle`; close → reallocate bumps the generation; the old
  handle then fails the generation check.
- `generation` wraps from `u32::MAX` to `0`.
- `restrict` cannot amplify; `duplicate` requires `DUPLICATE` and
  intersects rights.
- Segment grow at the 4097th allocation; `OutOfHandles` on
  directory exhaustion (via a `#[cfg(test)]` cap override).
- `stat` returns the live snapshot when `INSPECT` is granted.
- Type-rights matrix rejects `MAP_WRITE` on `Process` etc.
- 8-thread allocate/lookup/close stress preserves cross-pid
  isolation; closes drain to zero handles.
- Concurrent torn-read torture: a writer churns the same slot while
  readers spin on lookup; any success returns an internally
  consistent metadata tuple (proves the seqlock never tears).
- `FAIL_NEXT_ACQUIRE` flag forces step-7 failure and the lookup
  returns `InvalidHandle`.

## Where to read more

- [Spec — handle encoding](../spec/handle-encoding.md) — normative
  wire format, validation algorithm, rights matrix.
- [Spec — syscall ABI](../spec/syscall-abi.md) — `sys_handle_*`
  prototypes.
- [Rationale — why capabilities](../rationale/why-capabilities.md) —
  capability-based authority over UID/ACL.
- [Rationale — rejected approaches](../rationale/rejected-approaches.md)
  — global-vs-per-process tables, typestate vs const-generic rights.
- [Kernel lock ordering](../../kernel/docs/lock-ordering.md) — rank-3
  segment lock and its discipline.
- [Architecture overview](overview.md) — how the handle layer fits
  into the rest of the kernel.
