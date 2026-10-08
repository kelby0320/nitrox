# The syscall surface, consolidated

**Status: planned 2026-10-08, for between Phase 6 and Phase 7 (the maintainer's call).** Not
detailed: this is the review that found the drift and the shape a consolidation could take. The
detail pass comes when it is next, as every part's does.

## Why

The maintainer prefers **a few syscalls that work on many kinds of object** to many that each
handle one case, and suspected the surface had drifted from the original design's tightness. The
question came up in Phase 6 Part H's detail pass, over a copy: a `SYS_FILE_COPY` was proposed,
and **its name said it worked on files only**, whatever its arguments took — a special case
dressed as a general call. The review below found the suspicion right.

Part H takes nothing from here: it measures and fixes copy throughput **below** the syscall
surface, and what it finds that only the surface can fix comes here
([`phase-6-usb.md`](phase-6-usb.md) § *Part H in detail*).

## The original design's rules

[`os-design-v5.1.md`](../archive/os-design-v5.1.md) § *Syscall Interface*, checked 2026-10-08:
- **"Syscall table is small (~30 entries)."** Its complete set lists **thirty-two**, and a two-call
  I/O ring as a purely additive optimisation (counted in the PR #370 review; the tables below
  agree: twenty-eight built as designed, four never built).
- **"No `sys_read`, `sys_write`, `sys_open` — all I/O goes through `sys_io_submit`."** An `IoOp`
  names a resource and a buffer; its spec calls the pair resource-agnostic "so future resource
  kinds … reuse them" ([`io-operation.md`](../spec/io-operation.md)).
- **Async-first**: every potentially-blocking operation returns a `PendingOperation`, and a thread
  blocks in `sys_wait` alone ([`why-async-syscalls.md`](../rationale/why-async-syscalls.md)).
- **Authority is a handle**, and a few privileged classes of operation are ambient `SysCaps`
  ([`syscaps.md`](../architecture/syscaps.md)).

## Today's surface (checked 2026-10-08)

Forty-two numbered syscalls, and two debug ones (`0xFFFF_0000` `debug_kprint`, `0xFFFF_0002`
`test_exit`, the second only in a test build).

| Group | Syscalls | Against the original |
|---|---|---|
| Handles | `handle_close` (0), `handle_duplicate` (1), `handle_restrict` (2), `handle_stat` (3) | as designed |
| Memory | `memory_create` (4), `memory_map` (5), `memory_unmap` (6) | as designed; `memory_map` maps a file from offset 0 only |
| Time | `clock_read` (7), `timer_create` (8), `timer_set` (9) | as designed |
| Waiting | `wait` (10), `notif_recv` (11) | as designed |
| Channels | `channel_create` (12), `channel_send` (13), `channel_recv` (14) | as designed |
| Processes and threads | `process_spawn` (15), `process_exit` (16), `thread_exit` (17), `thread_set_affinity` (18), `thread_create` (19), `thread_get_registers` (20), `exception_resume` (21) | as designed |
| Namespaces | `ns_create` (22), `ns_lookup` (23), `ns_bind` (24), `ns_unbind` (25) | as designed |
| I/O | `io_submit` (28), `io_cancel` (29) | as designed; `io_submit` takes devices only — block, and character (a console, the raw input nodes and `/dev/registry/changes` read; a keyboard's lights written) |
| **Added: entropy** | `entropy_create` (26), `entropy_read` (27) | drift — finding 3 |
| **Added: namespaces** | `ns_enumerate` (30), `ns_derive` (37), `ns_sync` (38), `ns_held` (39) | `ns_enumerate` sound; the rest drift — findings 2 and 4 |
| **Added: files** | `file_sync` (31), `file_grow` (32), `file_create` (33), `file_truncate` (34), `file_rename` (35) | drift — findings 1 and 2 |
| **Added: processes** | `process_terminate` (36) | sound: a request on a process handle, gated by `SIGNAL` |
| **Added: the system** | `power` (40), `clock_set` (41) | sound each, but two authority models — finding 5 |

**Never built from the original**, and not drift: `thread_set_tls`, `exception_extend_timeout`,
`device_map_mmio`, `release_initramfs`, and the ring's `ring_create` and `ring_notify`.

## What drifted

1. **Four syscalls that are one.** `file_create`, `file_grow`, `file_truncate` and `file_rename`
   are each `sys_ns_lookup` in the kernel's dispatch, with a different `ResolveOp`
   (`kernel/src/syscall/table.rs`): a namespace resolve that carries an operation to the
   filesystem's server. Four numbers, named for files, for one call with a mode.
2. **Two blocking syncs.** `file_sync` and `ns_sync` block inside the syscall: "a durability point
   is something the caller wants to know it has reached"
   ([`syscall-abi.md`](../spec/syscall-abi.md)). A `PendingOperation` says when it has been reached
   as well as a return does. `IoOpcode::Flush` already exists, for block devices. **They are not the
   only calls that block** (PR #370 review):
   - **`process_spawn`** reads a file-backed image's pages inside the call, a device round trip
     each (`FileObject::read_to_kvec`, from `kernel/src/syscall/table.rs`) — every launch of a
     program from the root filesystem;
   - **`power`** waits on its flush before it acts;
   - **`debug_kprint`** writes to the console and returns, by design.

   Folding the syncs leaves spawn's fills, and `power`'s wait, which is the machine going down.
3. **Entropy twice over.** `entropy_create` mints the same `EntropyObject` a resolve of
   `/dev/entropy` returns (`kernel/src/object/kernel_server.rs`), and `entropy_read` is a read of
   its own that answers with data or with a `PendingOperation`, as the pool is seeded or not —
   where the original's read is `sys_io_submit`.
4. **Two namespace calls that fold.** `ns_derive` is "create a namespace, as a copy of this one":
   a form of `ns_create`. `ns_held` answers one question for an unmount — how many files under a
   registration are still held — which a namespace flush's answer could carry.
5. **Two authority models for system operations.** `power` is authorised by a `SystemControl`
   object, which only `init` holds; `clock_set` by the ambient `SYSTEM_CLOCK` syscap. Both models
   are the original design's; two privileged system operations choosing differently is a choice
   to make once, not a defect.

**And a gap, not drift**: `memory_map` maps a file from offset 0 only, and unmapping leaves a
file's frames with its page-cache object until the object drops. So `libfs` copies one mapping
of a whole file, and refuses one over 8 MiB (`MAX_COPY`) — the limit Part H's detail pass found.

## A tightened surface, as a starting point

| Change | Syscalls |
|---|---|
| **One resolve that carries an operation**: `ns_lookup` with an op — create, grow, truncate, rename — in place of the four `file_*` numbers | −4 |
| **Files and namespaces as `io_submit` resources**: `Flush` on a file is `file_sync`, on a namespace `ns_sync`, each answering through a `PendingOperation` — the two syncs brought under async-first; spawn's fills remain (finding 2) | −2 |
| **Entropy by lookup and `io_submit`**: `/dev/entropy` resolved, and a `Read` into a memory object | −2 |
| **`ns_held` in the flush's answer**, or in `handle_stat` of a binding | −1 |
| **`ns_derive` as `ns_create(from)`** | −1 |

Forty-two to thirty-two: **the original's own count**, though not its set — `ns_enumerate`,
`process_terminate`, `power` and `clock_set` are in it, and the four never built are not.

**A copy is then no new syscall**: `sys_io_submit(destination file, Write, buffer = source file,
offsets, length)` — one more use of the entry point the original built for every kind of I/O, and
the end of the 8 MiB limit, since the kernel streams it a window at a time and lets each window's
pages go. **Files and entropy would be its first resources that are not devices.** Part H's fixes —
clustered fills and write-backs, if the measurement points there — are the engine it would run on.

## What it touches

- **The syscall numbers**, which are **not** ABI-hash inputs
  ([`syscall-abi.md`](../spec/syscall-abi.md),
  [`abi-version-hash.md`](../spec/abi-version-hash.md)): renumbering touches `abi-sync-check`'s
  kernel-to-`libkern` constants and the spec's numbering. **The hash** changes only if `IoOp`'s
  layout or `IoOpcode`'s discriminants do — new resource kinds need neither, new opcodes would.
- **Every caller**: `libfs`, the storage service, the coreutils, `nxinstall`, both filesystem
  servers' setup, `boot-probe`, and every test that makes these calls.
- **The specs**: [`syscall-abi.md`](../spec/syscall-abi.md),
  [`io-operation.md`](../spec/io-operation.md), the handle and namespace docs.

## Open, for its detail pass

- **What `io_submit` means for a file**: the rights each side needs (a file handle carries
  `MAP_*` rights, a device — block or character — `READ` and `WRITE`), and what a `Write` to a file
  promises about durability — written back when it completes, or dirty until a `Flush`.
- **`process_spawn`'s fills**: whether a launch from a filesystem answers through a
  `PendingOperation`, as every other call that waits on a device would, or stays outside
  async-first with a reason written down.
- **How a resolve carries its op**: a code and an argument in registers, or a small struct by
  pointer, as `IoOp` is.
- **Whether `ns_held` folds into the flush's answer or into `handle_stat`.**
- **One authority model for system operations, or both** (finding 5).
- **The heavy paths**, the maintainer's second question: syscalls per common operation — a copy, a
  listing, a login, a launch — measured, and the redundancies found folded. A one-file copy makes
  about twenty today and resolves its source twice, which `libfs` can stop doing with no change to
  the surface at all.
- **Whether the original's unbuilt calls stay in the design**: the ring, `device_map_mmio` (a
  userspace driver's, should Tier 2 go that way), `release_initramfs`.
