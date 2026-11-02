# The syscall surface, consolidated

**Status: planned 2026-10-08, for between Phase 6 and Phase 7 (the maintainer's call); detailed
2026-11-02.** The review below found the drift and the shape a consolidation could take; [*Part E
in detail*](#part-e-in-detail) is the plan, and where they differ it wins.

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

## Today's surface (checked 2026-10-08, and unchanged 2026-11-02)

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

## Part E in detail

*Detailed 2026-11-02, measured on `main` at PR #415's merge (`bddd32b5`), the maintainer's calls on
the order, the authority model, the test harness and the two unused calls folded in.* Where the
review above and this differ, this is the plan. **Eleven sub-parts, a PR each, in two halves with
Part J between them.**

### What the pass found

- **The surface has not moved since the review**: forty-two numbered syscalls and two debug ones.
  `thread_set_affinity` and `io_cancel` have no caller at all.
- **Userspace makes 1,100 raw syscalls**, down from the 1,183 Part I measured, and `test-harness`
  holds 435 of them. About ninety sites name the ten numbers the review folds, most of them tests.
- **Four things the review drew do not hold:**
  1. **A namespace cannot be an `io_submit` resource.** `ns_sync` and `ns_held` name a mount by a
     path, an `IoOp` carries no path, and a mount has no handle: a `UserspaceServer` binding
     resolves to a registration record no process is ever given, on purpose. They are resolves.
  2. **`ns_held` cannot ride on the flush's answer.** The storage service asks it again after a
     pause, with nothing left to flush (`held_files`, `storage-service/src/main.rs:215`).
  3. **Spawn need not learn to wait.** It already takes a memory object, and it copies a file into
     a `KVec` before it loads it (`image_bytes`, `FileObject::read_to_kvec`). With a `Read` on a
     file, the waiting moves to the caller, where a `PendingOperation` already answers for it.
  4. **Unmapping lets no page go, and a window of a file holds its pages until the object drops.**
     The cache is an index, not an owner: an object leaves with its last user, and has no way to
     give back part of itself. "A window at a time" needs a way to say so (E.8).
- **Entropy is ambient today, and the review's fold would end that.** `entropy_create` takes no
  right and no binding, so any process has it. No session binds `/dev/entropy`, and Phase 7's
  `HashMap` wants it in every program (E.9).

### The heavy paths

Measured with per-process counters in `dispatch`, in a probe not kept: the release image under KVM,
a real login, then each command typed at `nxsh`. The table is exact: a dump at each spawn and exit
bounds a program's run.

| A program, spawn to exit | Its own syscalls | The filesystem server's |
|---|---:|---:|
| `rename` | 20 | 32 |
| `whoami` | 23 | 0 |
| `mkdir` | 24 | 73 |
| `copy` of a small file | 40 | 82 to 86 |
| `list .` | 43 | 10 |
| `remove` | 45 | 74 |

**Any program costs the shell and the tty server about 100 more**, 78 before it starts and 25 while
it runs, and **a key typed about 7**, by the slope across commands of different lengths. A login is
at most 510 across every process, the tail of the boot among them. Over the session's 3,431 calls:
**`wait` 24%, `recv` 23%, `send` 18%, `close` 9%, `debug_kprint` 8%, `io_submit` 3%**, and
`file_sync` is one a copy. So:

- **The review's folds are for the surface, not for speed.** The ten numbers are a rounding error
  on every path measured. They make the surface smaller, and nothing faster.
- **The weight is IPC.** A round trip is six calls, and two thirds of all calls are that plumbing.
  A combined call would earn its place only by a measured time, which nothing here has; so **no
  new call, and the ring stays out**: at 3% of calls, `io_submit` is not what it would speed up.
- **Three redundancies are worth folding, all below the surface**, and each goes with its crate:
  `ns_children` makes a call per binding of the whole namespace on every listing, 13 in a session
  (E.9); `service-mgr` calls `recv` on every service for each wake, 23 for one event, 342 against
  36 waits at boot, as it reads none of `sys_wait`'s results (E.3); `copy_from` resolves its
  destination up to four times (E.2).
- **The filesystem server's cost is its metadata**, a block a call: `submit`, `wait`, `close`, two
  thirds of its calls in a copy. That is a cache's question and not this part's.

A graphical login and launch were not measured.

### The surface it ends with

Forty-two to **twenty-eight**:

| Change | Sub-part | Syscalls |
|---|---|---:|
| `ns_lookup` carries an operation: create, grow, truncate, rename, flush, held | E.7 | −6 |
| `file_sync` is a `Flush` on the file | E.7 | −1 |
| `ns_derive` is `ns_create(from)` | E.9 | −1 |
| Entropy is a char node: `entropy_create`, `entropy_read` | E.9 | −2 |
| `thread_set_affinity` and `io_cancel` removed (below) | E.9 | −2 |
| `clock_set` and `power` are `Write`s to char nodes | E.10 | −2 |

What stays: the handles' four, the memory three, `clock_read`, `timer_create`, `timer_set`, `wait`,
`notif_recv`, the channels' three, `process_spawn`, `process_exit`, `process_terminate`,
`thread_exit`, `thread_create`, `thread_get_registers`, `exception_resume`, `ns_create`,
`ns_lookup`, `ns_bind`, `ns_unbind`, `ns_enumerate` and `io_submit`.

**`thread_set_affinity` and `io_cancel` go for now, and need implementing** (the maintainer's
call): `io_cancel` has only ever answered `Unsupported`, and its number was reserved for an ABI
that is not yet stable. Both are in the Phase 7 plan, for the `std` port to bring back with a
consumer, and each gets a deferred entry when it goes.

**The original's unbuilt calls:** `thread_set_tls` is Phase 7's. The ring stays in the design as an
additive optimisation, wanting a measurement. `device_map_mmio` waits on Phase 8 saying where its
drivers live. `release_initramfs` and `exception_extend_timeout` have no consumer and leave the
design.

### The order

**libos and every crate on it first, then Part J, then the surface** (the maintainer's call). The
question was whether Part J's P1 reaches what this part rewrites, and by measurement it hardly
does: 196 downcasts and `from_raw`s in 36 kernel files, of which the functions this part deletes or
rewrites hold about 11. But three things order the work:

- **Callers move once.** With every crate on libos, a surface change is one crate's, where it would
  be about ninety sites now. So libos is written to the surface this part ends with, on today's
  syscalls.
- **The userspace half touches no kernel code**, so it needs nothing from Part J.
- **The surface's new kernel code is its most delicate**: threads that hold files, buffers and
  `PendingOperation`s, copying between frames. P1's typed references and P2's bounded views are
  what that code wants to be written on.

So the first half is E.1 to E.5, Part J follows, and the second half is E.6 to E.11.

### E.1 libos, the safe layer (P7), and the ratchet

Every class P7 lists, as a type or a function. Each is `no_std`, alloc-free and over `libkern`
alone, as the conditions of 2026-10-27 hold it, and answers in `Result`:

| Today, by hand | libos |
|---|---|
| A `u64` closed on every path | `Handle<T, M>`, and `Ref<'a, T, M>` for a borrow, where `Handle::borrow` gives a whole handle with no lifetime (rt-o8) |
| `static mut [u8; 4096]`, offsets by hand | `Msg`: header and payload typed. A failed send gives its handles back, which ends `TODO(capture-refused-leak)`'s handle half. A receive's status is a `KError`, ending `TODO(recv-kerror)`, with `Received` left in `libkern` |
| `sys_wait` over static arrays, `IoResult` by offset | `WaitSet<N>`, on the stack, answering which signalled |
| Map, `from_raw_parts`, unmap or forget | `Mapping`, unmapped on drop. One another process writes gives no `&[u8]` (desk-d17) |
| Lookup and wait by hand, 141 callers of three helpers in `test-harness` alone | `Namespace::lookup`, with `bind`, `create`, `unbind` and `bindings` |
| The `file_*` numbers | `fs::{create, grow, truncate, rename, flush}`, in the target's shape |
| `SpawnArgs` by hand, stacks by hand | `Spawn`, by path; `Thread`, owning its stack (rt-o6, th-o6) |
| Six sleeps, a `kprint` and `exit` per crate | `time::{now, sleep}`, `log`, `exit` |
| A hand-sized entropy buffer | `entropy::fill` |
| `_start`'s four registers as `u64`s | `boot::Start`: owned, typed (rt-o7) |
| Each server's reply plumbing | `server::{reply, reply_error}` (sup-o6). Each keeps its own loop |

**The ratchet**: `cargo xtask check-syscalls`, in CI from this PR, reads `unsafe-inventory`'s count
per crate against `tools/xtask/raw-syscalls.toml`, and fails a rise, and a stale entry as
`check-size` does. `eshell` and `libheap` are listed with their reasons. `libos`'s mock answers as
the kernel does (`libos.md`), and each wrapper is tested through it. Author; `/pr-review`.

### E.2 The libraries, and the wire formats (P8)

`librsproto`, `libstream`, `libfs`, `libfsserver`, `libsession`, `libsurface`, `liblog`, `libprompt`
and `libdraw`: 143 raw syscalls. `librsproto` and `libstream` go first, as the decision of
2026-10-27 has it. `copy_from` resolves its destination once.

**P8 goes with them**, as `librsproto` is the file both change:
- **One bounded reader** (proto-o4): `Reader<'a>`, for about 220 reads at fixed offsets across 77
  parsers, so a guard and its offsets cannot disagree, and three iterators re-walk with it.
  **The control is the survey's:** a `parse_ping` guard weakened by a byte panics today, and answers
  `None` after.
- **The kernel's copies checked** (proto-o6): an `AbiFamily` pairing `kernel/src/rsproto.rs` with
  `librsproto`, a `put_block_run`, and `RECORD_LEN` asserted.

The implementer, a crate at a time, its diff read; the `Reader` to the same brief. `/pr-review`.

### E.3 The supervisors and the servers

`init`, `service-mgr`, `device-mgr`, `storage-service`, `auth-service`, `tty-server`,
`input-server`, `view-broker`, `profile-server`, `clipboard-server`, `logging-service`, `session-mgr`
and `desktop-session-mgr`: 263. Each crate's own rules stay, `auth-service`'s no `alloc` among them.

**`service-mgr` reads what its wait says.** With `WaitSet`, it calls `recv` on what signalled. A
death still needs every service looked at, as `ChildExited` names one by its `pid`, so that pass
stays and runs when the notification channel signals. The control is the probe's count: one event,
one `recv`.

The author keeps `init` and `service-mgr`, the critical path; the implementer has the rest.
`/pr-review`, and a boot's worth of gates on each.

### E.4 The desktop and the programs

`desktop-shell`, `compositor`, `coreutils`, `hello`, `nxterm`, `nxsh`, `nxinstall`, `heartbeat`,
`nxfiles` and `nxedit`: 202. The last of `static mut` as an out-param goes with them, and
`BufferSource::pixels` stops being a slice (desk-d17). The implementer; `/pr-review`.

### E.5 The test harness

**Plumbing moves, and a refusal stays raw** (the maintainer's call, and the exception the Definition
of Done asks to be agreed). 117 of its 435 are `handle_close`, 39 `memory_map`, 38 `wait`. **The
rule:** a call stays raw only if it passes what a safe wrapper cannot express, a bad handle,
pointer, length or right, because then the call is the test. Those stay in named modules under
`bootprobe/`, with their own entry in the ratchet's table. **The risk is a moved test that stops
provoking what it tests and still passes**, so any test moved with a refusal in it has its control
run again. The implementer; the author reads which stayed.

**With E.5 the raw count is `eshell`'s, `libheap`'s and the refusals'**, and Part J comes next.

### E.6 The mapping path

Three bash findings and the gap, which are one path and Phase 7's first:
- **`memory_map` takes an offset**: `sys_memory_map(obj, hint, size, rights, offset)`, page-aligned,
  for a `FileObject`. `ld.so` cannot map a segment without it.
- **kmem-29**: the search and the insert under one hold, so a hint-less map cannot lose a race.
  `map_file` and `map_object` take a placement, not a range. **Control:** the bash's two-thread
  probe, refused `Overlap` today.
- **kmem-15**: `map_file_page` re-derives the page's index under the second hold, from the VMA as
  it now stands. With offsets this is no longer rare. **Control:** a host test interleaving
  `file_backing`, `unmap_covering`, `map_file` and `map_file_page`, which the bash says would
  settle it.
- **kmem-28**: a kernel copy that touches a valid, unfaulted page demand-pages it rather than
  failing, in `pf_dispatch`, before the recovery table. Only for a copy made with no lock held:
  each `copy_*_user` under a lock is checked first, by `lockrank`. **Control:** a message sent from
  an untouched page of a mapped file, refused today.

Author; `/pr-review`; a boot-probe test of each.

### E.7 A resolve carries its operation, and a flush answers through a PO

**The shape**: `sys_ns_lookup(ns, path, len, rights, op)`, where `op` is a `UserPtr<ResolveOp>`, a
small `#[repr(C)]` struct as `IoOp` is, and `NULL` is a plain lookup. The common call stays four
registers and every plain site unchanged. Rename's second path and its flags will not fit in
registers, which settles the review's question.

```rust
#[repr(C)]
pub struct ResolveOp {
    pub code: u32,      // create, grow, truncate, rename, flush or held
    pub flags: u32,     // a rename's
    pub size: u64,      // a size change's
    pub dest_ptr: u64,  // a rename's destination
    pub dest_len: u64,
}
```

**`Flush` and `Held` are resolves**, answering through the PO: `result` is the files written, or
those still held. `Held` waits on nothing and is complete at once. **A flush on a file is
`io_submit(file, Flush)`**, needing `MAP_WRITE`, as `file_sync` does, which ends kmem-30.

**What blocks moves to a kernel thread of its own, an operation each.** `write_back` is sound on a
thread that may sleep and stays as it is. `sched::spawn` starts a thread holding the file or
registration and the PO, it completes the PO and returns, and a kernel thread that returns exits
(`sched::thread_enter`). A thread an operation, at most `FILE_OPS_MAX` at a time, beyond which one
is refused `WouldBlock` synchronously, because one worker's queue would let a stick that has stopped
answering hold up the root filesystem's flush. It is what `file_sync` costs today, a thread stopped
on a device, except that the thread is not the caller's. **So a supervisor can wait on a flush with
a deadline**, which it could not while `init` and the storage service called these in-line.

**Controls:** the existing `ns_sync` and `ns_held` tests over the new calls; a flush that does not
return before its write-back, by a slowed device in a host test; a flush of a stick pulled, as
`check-media` does. Author; `/pr-review`.

### E.8 Files as resources, a copy, and spawn

**`Read` and `Write` on a `FileObject`**, the first resources that are not devices:
- **Rights**: a file's are its `MAP_READ` and `MAP_WRITE`. No new ones: a right carries intent at a
  lookup (`SEND|RECV` asks a server for a directory), so a new right on a resolve is not free, and
  a mapping already means both.
- **Offsets** are bytes, with none of a block device's alignment.
- **A `Write` is dirty until a `Flush`**, as a store through a mapping is and as a device's write
  cache is: `Flush`'s meaning is already "what was written before it is durable". It marks the
  object dirty as a writable mapping does. Past the file's end it is refused, as growth is a
  resolve's.
- **It runs on E.7's thread**: `fault_in_page` and a copy between frames, with a whole page's
  `Write` skipping the fill.

**A copy** is `io_submit(destination, Write, buffer = source file)`, as the review drew it, and
needs the source's `MAP_READ`. Each op is one window, up to `MAX_USER_COPY_SIZE`, and `libfs` loops.
**`IO_NOCACHE`**, the `flags` modifier `io-operation.md` reserved, lets a window's pages go when it
completes: written back first if dirty, then freed, **if nothing maps the object**, which needs a
count of its mappings beside `writable_maps`. This ends `copy-limit`, on the engine Part H built.
**Controls:** a copy larger than the machine's memory, in QEMU given little, with the frames in use
read before and after; a copy with a mapping held, whose pages stay.

**Spawn takes a memory object and nothing else.** `image_bytes` loses its `FileObject` arm, so
`process_spawn` never touches a device and async-first holds without an exception. `Spawn` in libos
does the `Read`, into an object it then closes. It costs no copy it did not have: a file was read
into a `KVec` already. `ld.so` is unaffected, as it maps. Rights: `MAP_READ`, as now.
**Controls:** `process_spawn` of a `FileObject` refused, and `Spawn`'s deadline ending a `Read` that
does not finish, through libos's mock. Author; `/pr-review`.

### E.9 Entropy, namespaces, and the two removed

- **Entropy is a char node.** `/dev/entropy` becomes a `DeviceNode`, read through `submit_read`
  into a memory object, the review's plan with no new resource kind: **`EntropyObject` goes, and
  the ABI hash changes**. Unseeded, a read's PO completes when the pool seeds, `result` 0, and
  `entropy::fill` reads again, as a caller of `entropy_read` does today. Boot-probe's seed tests
  stay.
- **So entropy needs a binding, and every namespace gets one.** It is ambient today, and a program
  with no `/dev/entropy` could not build a `HashMap` in Phase 7. `libsession::build` binds it in
  both columns and for each application, and the view broker in what it constructs. **Adding a
  member to a session is a design decision each time** (`session-mgr/CLAUDE.md`); this one grants
  random bytes and nothing else, the authority every process has now.
- **`ns_create(from)`**: `NULL` for empty, a namespace for a copy of it. **`ns_derive` goes.**
- **`ns_enumerate` takes a count**: `(ns, index, out, count)` fills up to 64 `NsEntry`s and answers
  how many. A listing makes one call. **The control is the probe's: 13 to 1.**
- **`TODO(direct-bind-copy)`**: a direct bind asks `DUPLICATE`, as the moves' rule says, once every
  in-tree direct bind has been checked. Its trigger was this pass.
- **`thread_set_affinity` and `io_cancel` are removed**, each with its entry in
  `deferred-decisions.md`.

Author for the kernel's half; `/pr-review`.

### E.10 One authority model

**A handle, for both** (the maintainer's call, finding 5). Each privileged system operation is a
`Write` to a kernel char node, as a keyboard's lights are:

- **The clock**: eight bytes, nanoseconds, to a clock node. The kernel hands `init` the node, as it
  does `SystemControl` today, and it passes down to the view broker, as `SYSTEM_CLOCK` does, through
  `service-mgr`. The broker binds it into a `with admin` program's namespace at `/dev/clock`, as it
  does a disk. **`SYSTEM_CLOCK` and `sys_clock_set` go.**
- **Power**: one byte, `POWER_HALT` or `POWER_REBOOT`, to a power node `init` alone holds. It stays
  a handle held and never a binding, so no namespace can offer it. **`SystemControl` and `sys_power`
  go, and the ABI hash changes.** It still returns only to refuse, and it may wait in the call: the
  machine is stopping, so nothing is left to answer through a PO. That one exception is written
  down.

**Neither is bound in the root namespace**: a kernel node there would give both to everything that
holds it. `syscaps.md` loses a row, leaving five: `BIND_NAMESPACE` and `REAL_TIME` wired, three
defined. **Controls:** `date --set` without the grant, refused; with it, the clock moved;
`check-shutdown` and `check-logout`, unchanged. Author; `/pr-review`.

### E.11 The numbers, and the record

Numbers were assigned in landing order, so the removals leave holes. **One commit renumbers `0`
to `27` in family order**, in `libkern`, the kernel and `syscall-abi.md`, held by `abi-sync-check`,
and since libos by then holds every caller, the change is those files and the raw refusals. Numbers
are no ABI-hash input. `syscall-abi.md`, `io-operation.md`, `handle-system.md`, `syscaps.md`,
`libos.md` and `filesystem-data-path.md` are rewritten as each sub-part lands, not here; this
checks them, and takes `release_initramfs` and `exception_extend_timeout` out of `syscall-abi.md`.
The implementer; `/pr-review`.

### What this part does not do

No new call for speed, and no ring. No page-cache reclaim beyond `IO_NOCACHE`: the clock algorithm
stays deferred. No partial `memory_unmap` (`TODO(mm)`) and no `thread_set_tls`: Phase 7's, with
their consumers. No per-page dirty bit. No change to the filesystem server's metadata I/O.
