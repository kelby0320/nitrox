# The safety survey: defects, opportunities and their triage

**Status: planned 2026-10-26 (the maintainer's call); its thirteen subsystems surveyed
2026-10-26 and 2026-10-27, and triaged 2026-10-27 with the maintainer; every item marked fix fixed
by 2026-11-01.** Part I of
[`pre-phase-7.md`](pre-phase-7.md) filled it, a `/safety-survey` run a section, and fixes what is
marked fix.

## What it looks for

Two kinds of item, as `.claude/skills/safety-survey/rules.md` defines them:

- **A defect**: a size, an ownership or a lifetime held by convention where no type holds it, and
  wrong — memory written past its end or used after it is freed, a handle or a mapping leaked or
  released twice, a `SAFETY` argument that does not hold, a safe function that lets its caller
  break the `unsafe` beneath it. A latent one counts, with the ordinary change that would make it
  bite.
- **An opportunity**: `unsafe`, or a raw pointer, correct today, that a type could hold instead —
  removed where a safe way exists, narrowed to the operation that needs it, or put behind one
  abstraction for every site. One item is a pattern, its sites counted.

Each section starts with the subsystem's inventory, `cargo xtask unsafe-inventory` over its paths,
so two surveys of one tree count alike.

## How an item is kept

The author appends each run's section as its report gives it, a checkbox an item:

- `[ ] **<id>-d<n>** file:line — the defect in a sentence. Scenario. Evidence. Lens. Severity.`
- `[ ] **<id>-o<n>** kind: the pattern — its sites, counted. The abstraction, its home, what it
  closes, what it depends on. Lens. Value, effort.`

The ID is the subsystem's short name, `d` or `o`, and a number, such as `kobj-d3` or `rt-o1`,
apart from the bash's `kobj-3`. Triage, with the maintainer, adds one word after the last field:
**fix** (in Part I), **plan** (an opportunity that goes into Part E's detail pass, or a part of
its own), **later** (past Phase 7, with the maintainer's agreement), or **no** (with the reason).
**This file is an item's record**, as `bug-bash.md` is a finding's: a part of the plan schedules
it, so it takes no `TODO(tag)` and no `deferred-decisions.md` entry. A defect is reproduced before
it is fixed, and its box is ticked with the commit that fixed it and the test that fails without
it; an opportunity's, with the commit that finished it.

## The triage

Triaged 2026-10-27 with the maintainer, on a page that started each item at a suggested verdict:
the maintainer kept every suggestion and settled three calls. 354 ids, 281 once an item several
runs found is one.

- **221 fix**, in Part I. First kplat-d1 (any process crashes the kernel), drv-d1 (an AHCI
  completion under live DMA) and kobj-o4 (the test heap that hides a use-after-free from Miri);
  then the other high defects, kobj-d1, kobj-d2, kmem-d1, kmem-d2, kplat-d2, fs-d1, sh-d1 and
  sh-d2; then the rest. Ten are defects the bash found and Part H held, which move here with them:
  kmem-9, kobj-10, kobj-11, drv-8, proto-7, proto-8, proto-14, proto-15, sec-12 and sup-9.
- **120 plan**, in eight layers, each ending a class rather than an instance:

  | | Layer | Where |
  |---|---|---|
  | P1 | The typed kernel object layer: tag and pointer tied, liveness and the scheduler lock as types | Part J |
  | P2 | Frames: an owned frame, and a bounded view of its bytes | Part J |
  | P3 | Page tables: typed roots, atomic slots, one walk | Part J |
  | P4 | Per-CPU state: an index that cannot outlive the code that pins it | Part J |
  | P5 | Device memory and I/O: in-flight IRPs, DMA and MMIO accessors, device teardown | Part J |
  | P6 | Unsafe that needs none: functions whose only contract is "ring 0" | Part J |
  | P7 | libos, userspace's safe layer: owning handles, message buffers, waits, mappings | Part E |
  | P8 | The wire formats: one bounded reader, and the kernel's copies of them checked | Part E |

- **10 later**, past Phase 7: nine small changes with no defect behind them, and sched-d15, a CPU
  whose x2APIC id is over 255.
- The three these counts leave out: kmem-d12 and kmem-d14 are the bash's kmem-15 and kmem-28, and
  go with them into Part E; kobj-d12 is two defects, its `try_new_pair` half fixed as sched-d12 and
  its `Drop` half planned as sched-d11. kmem-o7 is split as well, though counted in P3: its first
  step is there, its second later.
- **Two more, found reviewing PR #400** (rt-d12, rt-d13, at the end): both fix, with libos's other
  holes.

The calls:

- **libos is to be every userspace crate's syscall layer**, and the "no libos" rule of
  `session-mgr`, `libsession` and `desktop-session-mgr` is lifted. `eshell` and `libheap` stay on
  `libkern`, the two agreed exceptions. The decision log, 2026-10-27, holds its conditions.
- **P1 to P6 are a part of their own**, Part J, since they change no syscall.
- **The VMA tree stays.** kmem-o7's second step, replacing the tree with a sorted vector, is later:
  its insert cost grows with the VMA count, and a balanced tree is the maintainer's preference when
  the std port shows how many mappings a process makes. Its first step, deleting the augmentation
  nothing reads, is in P3. What to answer then is `find_free_range`, which scans every VMA while
  libheap maps one per 64 KiB arena.

Two sections count their opportunities differently from their lists: kernel-objects says 14 and
lists 13, the test harness 16 and 17. Each is kept as its report gave it.

## Items

### kobj — main at 6d390537, 2026-10-26

Lenses: abi, ownership, soundness, concurrency, opportunities; returned nothing: none.
Defects: found 32, refuted by a boot 0, reported 15 (32 findings merged as duplicates). Opportunities: 14 (30 proposed, merged).

The inventory, `cargo xtask unsafe-inventory kernel/src/syscall kernel/src/object kernel/src/handle kernel/src/libkern kernel/src/rsproto.rs kernel/src/entropy.rs`:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `kernel/src/syscall` | 2117 | 101 | 0 | 0 | 0 | 0 | 1 | 30 | 0 | 0 | 0 |
| `kernel/src/object` | 3557 | 178 | 99 | 16 | 0 | 0 | 2 | 91 | 0 | 0 | 1 |
| `kernel/src/handle` | 825 | 28 | 3 | 3 | 0 | 0 | 1 | 1 | 0 | 0 | 0 |
| `kernel/src/libkern` | 1843 | 35 | 3 | 8 | 0 | 0 | 9 | 6 | 0 | 0 | 0 |
| `kernel/src/rsproto.rs` | 368 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/entropy.rs` | 168 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `total` | 8878 | 342 | 105 | 27 | 0 | 0 | 13 | 128 | 0 | 0 | 1 |

**Defects**

- [x] **kobj-d1** `kernel/src/handle/table.rs:417` — `HandleTable::lookup` step 7 bumps the
  refcount of an object that a concurrent close may already have freed. The grace tracker defers
  only the slot's reuse. `close_on` (`syscall/table.rs:733`), `sys_handle_close` and
  `close_all_owned_by` drop the last `ObjectRef` at once, so the object is `kfree`d between step 6
  (read `entry.object`) and step 7 (CAS on the header). Thread A is in any handle syscall on `h`,
  and A may be another process, because the owner check is step 10. A has just read
  `entry.object`. On another CPU the owner closes `h`, the last reference drops and the slab writes
  its freelist link over word 0, which is the refcount. A's CAS then hits that word. In the kernel
  the link is an HHDM address above `MAX_REFCOUNT`, so `try_acquire` panics with "kobject refcount
  overflow" and the kernel crashes. If the slot was already reallocated, A bumps a foreign
  object's count and step 8 releases it under the old type tag, so `dispatch_destroy` can run the
  wrong destructor. Any process can retry the window in a loop. The SAFETY comments at
  `handle/mod.rs:67,76` and `header.rs:63`, and `handle-system.md` § Close ("a concurrent
  try_acquire always observes a positive count"), assume the memory outlives its count.
  Demonstrated by four independent host probes (a `cfg(test)` hook between steps 6 and 7 that
  closes and drops the handle, then reads the slot's first word). One run, `cargo test --lib
  --target x86_64-unknown-linux-gnu probe_survey_lookup`, printed `destroyed before step 7: 1;
  freed slot word 0x7e0b0f5fc0a0; at step 7 0x7e0b0f5fc0a1`, so the slab freelist link was
  incremented. A paused-reader variant aborted the slab on its next `kmalloc` with a misaligned
  pointer. The probes were removed. The same probe passes silently under Miri, and
  `concurrent_torn_read_torture` cannot see it either, because the slab sits over a leaked host
  `Vec` (see kobj-o4). Fix shape: park the `ClosedObject` in the `DeferredClose` entry and release
  it in `drain_expired` once the grace period has passed (see kobj-d5 for the barrier that fix
  needs). abi/ownership/soundness/concurrency/opportunities reviewers all found it. High. **Fix.**
  Fixed in `fad8abff`: a close, and the exit sweep, wait out every lookup read section open when the
  entry is nulled, before the caller releases the object; fails without either wait:
  `a_close_waits_for_a_lookup_about_to_take_its_reference`,
  `a_sweep_waits_for_a_lookup_about_to_take_its_reference`.

- [x] **kobj-d2** `kernel/src/syscall/table.rs:1971` — A handle move is not atomic, so two
  concurrent moves copy a handle that lacks `DUPLICATE`. `sys_channel_send` looks each handle up
  with `TRANSFER`, queues the message, then calls `close_and_release` (`:2008`, `:2031`), which
  ignores a failed close (`:1866`). `sys_process_spawn` does the same (lookup `:455`, close
  `:503`), as do the forwarded-reply paths (`:2120`, `:2155`, `:2218`, `:2683`, `:2715`). A process
  holds `h` with `TRANSFER` and no `DUPLICATE`, such as init's and storage-service's spawn grants
  or libfsserver's device dup. Thread 1 calls `sys_channel_send(ch_a, [h])` and thread 2, on
  another CPU, calls `sys_channel_send(ch_b, [h])` or a spawn that moves `h`. Both lookups pass
  before either close, both receivers get the object, and the second close fails silently. This is
  the copy that the per-call duplicate check (`:347`, `:1959`) and boot-probe's
  `spawn_copy_needs_duplicate_test` say is refused. Any untrusted process with two threads gains
  authority. Unverified by a boot. Demonstrated by reading the path: nothing in the table or the
  syscall holds a lock or an in-transit mark between lookup and close, and `dispatch` runs
  unserialised on every CPU. A two-thread `-smp 2` boot-probe that sends one `TRANSFER`-only
  handle on two channels in a loop, counting receivers that get it, would settle the timing. See
  kobj-o5. abi/ownership. High. **Fix.**
  Fixed in `39e2a5c9`, as kobj-o5: a move claims its handle, and a second claim fails; fails without
  the claim's check or mark: `a_claimed_handle_cannot_be_claimed_again_and_commits_to_closed`. The
  syscall paths cannot run on the host, so `a_lookup_for_transfer_is_a_copy_and_a_move_is_a_claim`
  (tools) holds that every move is a claim; it names spawn's move by lookup in the old source.

- [x] **kobj-d3** `kernel/src/sched/ipc.rs:481` — When a userspace server dies, outstanding
  forwarded page-cache fills (`UserspaceServerReg.pending_fill`) are never completed.
  `ipc_endpoint_closing` and `us_reg_detach` (`:272`) drain and fail only pending lookups
  (`take_pending_next`). The faulting thread waits in `FileObject::wait_status`
  (`file_object.rs:1177`), which loops on `wait_on_io` with no deadline and never answers Ended,
  so it never leaves the kernel and its process can never finish exiting. A fill also forms a
  cycle (`reg` → `pending_fill.file_obj` → `Producer::FsServer.reg` → `reg`), so the registration,
  its kernel endpoint, the `FileObject` and its frames are never freed. A server answers a resolve
  with `OBJECT_KIND_FILE`, a client faults a page, the kernel sends `File::ReadRange`, then the
  server crashes before replying. The lookup PO is failed `PeerClosed`; the fill PO is not. The
  same hang reaches `sys_process_spawn` when the image is such a file (`read_to_kvec` →
  `fault_in_page`). No in-tree server replies `OBJECT_KIND_FILE` today (they reply `FILE_BLOCKS`),
  so it is latent, but any server adopting the documented reply makes it bite. Demonstrated by
  four host probes. One, `cargo test --lib --target x86_64-unknown-linux-gnu
  probe_server_death_fails_pending_fills_too`, printed `lookup signalled=true fill
  signalled=false`, then `reg destroys=0 file destroys=0 po destroys=1` after every outside
  reference was dropped. The probes were removed. Fix shape: drain `pending_fill` in both
  teardown paths, completing each PO `PeerClosed`. abi/ownership/soundness/concurrency. Medium.
  **Fix**, with the bash's kobj-10, moved here from Part H.
  Fixed in `7938344d`: `fail_outstanding` fails fills as well as lookups at all three teardown
  sites, and drops each fill's file outside `SCHED`; fails without the fill drain:
  `a_dying_server_fails_its_fills_and_frees_their_cycle`, `a_registration_dropped_fails_its_fills`.

- [x] **kobj-d4** `kernel/src/syscall/table.rs:491` — `sys_process_spawn` commits (the child is
  running) and then returns `Err` if the parent's Process-handle `allocate` fails, before the move
  loop at `:501-505`. Every handle the caller asked to move stays in the parent while the child
  holds it too, so a move becomes a copy, and the parent loses its only handle to the running
  child. The doc comment at `:328-330` promises that moves close the source only after the spawn
  commits. A process fills the global table (no per-process quota: `sys_handle_duplicate` until
  `OutOfHandles`) so that the child-side allocations succeed and the parent's does not, then spawns
  with `move_mask` set on a `TRANSFER`-only handle. Unverified by a boot. Demonstrated by reading
  the path, which is deterministic with no race (`:484-499`). A host test needs a current-thread
  harness that `syscall/table/tests.rs` lacks, or a boot-probe that fills the table and spawns
  with a move. ownership. Medium. **Fix**, with the bash's kobj-11, moved here from Part H.
  Fixed in `39e2a5c9`: the parent's Process handle is allocated before the child runs, so nothing
  fails after it does and every claim commits. Structural; the spawn path cannot run on the host,
  and test-qemu, test-interactive, check-login and check-storage pass.

- [x] **kobj-d5** `kernel/src/handle/grace.rs:48` — `GraceTracker::enter_read` announces the
  reader with a Release store followed only by an Acquire fence. On x86-TSO nothing orders that
  store before the later load of `entry.object` (`table.rs:403`), which is a Dekker-style
  protocol, so `drain_expired` can see the context as quiescent while a reader still holds the
  slot. Today the effect is masked: a recycled slot fails the reader's seq or generation check at
  steps 8-9. The natural fix for kobj-d1, deferring the last release to `is_grace_period_past`,
  would rely on this tracker and would still let a reader dereference a freed object. It needs a
  SeqCst fence (or a locked op) after the store. Unverified. A loom or herd7 model of `enter_read`
  against close plus `drain_expired` would settle it. The writer side already gets a full barrier
  from the rank-3 spinlock. ownership. Medium. **Fix.**
  Fixed in `fad8abff`: the reader and the closer each fence `SeqCst`; fails without the reader's:
  `a_reader_and_a_closer_never_both_miss`, which saw rounds where each missed the other in 6 of 10
  runs of a million rounds on x86.

- [x] **kobj-d6** `kernel/src/mm/user_access.rs:177` — `copy_from_user<T: Copy>` and `copy_to_user<T:
  Copy>` (`:205`) accept any `Copy` type. A `T` with invalid bit patterns (`bool`, an enum, `char`,
  `NonZero`) built from user bytes is undefined behaviour, and a `T` with padding copies
  uninitialised kernel stack bytes to user memory. Latent: the 3 `copy_from_user` types
  (`SpawnArgs`, `ThreadArgs`, `IoOp`) and the 8 `copy_to_user` types are padding-free integer
  structs. One ordinary change makes it bite, such as typing `IoOp.opcode` as `IoOpcode` or
  `ThreadArgs.class` as `SchedClass`, adding a `bool`, or adding a `u8` to `HandleInfo` or
  `NsEntry` (the size asserts would be updated and would not catch the new padding). Any process
  could then pass a discriminant of 7. Demonstrated under Miri: `MIRIFLAGS=-Zmiri-disable-isolation
  cargo +nightly-2026-07-20 miri test --lib --target x86_64-unknown-linux-gnu
  probe_copy_from_user` gave "constructing invalid value ... encountered 0x00000007, but expected a
  valid enum tag" at `user_access.rs:199`. A second probe with `bool` on byte 2 failed the same
  way. The probes were removed. See kobj-o6. soundness/opportunities. Medium. **Fix.**
  Fixed in `efb315e2`: `libkern::Pod`, implemented through `pod!`, which fails the build unless
  every field is `Pod` and the fields fill the struct, bounds `copy_from_user` and `copy_to_user`,
  and a copy starts from `T::zeroed()`. Fails without it: the survey's probe, a struct holding a
  `bool` read from bytes of 7, is undefined behaviour under Miri on the old code, and with this does
  not compile (E0277). `pod!` fails a padded struct, a `bool` field and a field not named.

- [x] **kobj-d7** `kernel/src/syscall/table.rs:1202` — `sys_ns_bind`'s test for an existing
  registration (`us_forward_existing_reg`) and its install of the back-pointer (`us_server_attach`,
  `:1223`) are separate SCHED holds around an allocating region. Two concurrent binds of one fresh
  endpoint each create a `UserspaceServerReg`, and the second attach clobbers the first's
  back-pointer, the rival-clobber that the comment at `:1199` says this check prevents. A
  supervisor with `BIND_NAMESPACE` binds one new server endpoint at two paths from two threads.
  Replies are routed to the surviving registration, whose per-registration request ids also start
  at 1, so an answer meant for client 100's `/secret` lookup completes client 200's `/public`
  lookup (a cross-client capability misdelivery), and lookups through the first registration stay
  pending until it is torn down. Reachable only by a trusted supervisor. Demonstrated by a host
  probe in `sched/ipc.rs` that ran the exact call sequence of two interleaved binds, two forwarded
  lookups and a reply: it printed `reply routed to R2` and "request 1's answer completes the lookup
  of client 200 for \"public\"". It was removed. Fix shape: one SCHED hold for check and attach.
  ownership/concurrency. Medium. **Fix.**
  Fixed in `d43466d6`: `us_server_attach` attaches unless a live registration is there, which it
  returns to share, in one `SCHED` hold; fails if it replaces one:
  `a_second_attach_of_an_endpoint_gets_the_first_registration`.

- [x] **kobj-d8** `kernel/src/drivers/mod.rs:197` — `pub(crate) fn adopt<T>(obj: KBox<T>, ty:
  KObjectType) -> ObjectRef` and its private twin `io/mod.rs:160` are safe functions that wrap
  `ObjectRef::from_raw` for any `T` under any tag. Sibling: `HandleTable::allocate(&self, pid,
  object: *mut (), ..)` is safe and stores any pointer, which lookup step 7 dereferences. Safe code
  can pair a box with the wrong destructor, and a `T` smaller than `KObjectHeader` has its header
  read out of bounds. Every current caller passes a matching tag, so it is latent: one wrong tag
  in a driver compiles with no `unsafe`. Demonstrated by a probe in `object/header.rs`:
  `adopt(NotificationChannel, EntropyObject)` gave `notification_channel_destroys=0,
  entropy_object_destroys=1`, and `adopt(KBox::try_new([1u64, 0xDEAD]), SystemControl)` was
  destroyed through the `SystemControl` arm (`cargo test --lib --target x86_64-unknown-linux-gnu
  probe_safe_adopt`). It was removed. See kobj-o1 and kobj-o2. soundness/opportunities. Medium.
  **Fix.**
  Fixed in `9a28d4b0`: `drivers::adopt` and io's twin are `unsafe`, their contract the pairing, and
  `adopt_node` stays safe, its type fixing the tag. Fails without it: safe code adopting a
  `NotificationChannel` under the `EntropyObject` tag ran the wrong destructor (destroy counts `(0,
  1)`); with this it needs an `unsafe` block. The sibling `HandleTable::allocate` is left to P1.

- [x] **kobj-d9** `kernel/src/syscall/table.rs:611` — `sys_thread_get_registers` takes the
  suspended thread's `ExceptionFrame` address under SCHED (`thread_exception_frame`,
  `sched/exit.rs:400-408`), releases SCHED, then reads 144 bytes through the safe trait fn
  `read_from_exception_frame(frame_ptr: usize)` (`idt.rs:692`, `registers.rs:39`), which forms
  `&ExceptionFrame` from any integer. The SAFETY argument "a suspended thread stays parked" does
  not hold: `resume_suspended` (`exit.rs:359`) needs only SCHED. Two holders of `SIGNAL` on thread
  T (`sys_thread_create` grants it): X loops `sys_thread_get_registers(T)` while Y calls
  `sys_exception_resume(T, Resume)`. T re-enters the kernel on the same stack and X copies torn or
  foreign kernel-stack words to user, a data race and a stack disclosure. The `Thread` ObjectRef
  keeps the stack mapped, so it is not a crash. Unverified by a boot. Demonstrated by reading the
  path. A three-thread SMP boot-probe (one faults, one reads, one resumes), checking the registers
  for values T never held, would settle it. Fix: copy `RegisterValues` out while SCHED is held, and
  make the frame read `unsafe` or take a typed frame. abi/ownership/soundness/opportunities. Low.
  **Fix**, as sched-d1.
  Fixed in `3d1174b5`, as sched-d1.

- [x] **kobj-d10** `kernel/src/libkern/spinlock.rs:91` — `SpinLockGuard` (and `IrqSpinLockGuard`,
  `:249`) get auto `Send` and `Sync` from `&'a SpinLock<T>`, so a guard over a `Send + !Sync` value
  such as a `Cell` is `Sync`, and a guard can be dropped on another CPU than the one whose preempt
  count and lock rank it raised (std's `MutexGuard` is `!Send`, and `Sync` only for `T: Sync`).
  Latent: nothing shares a guard across CPUs today. A scoped-spawn host test or a future helper
  that hands `&guard` to a worker gets a data race in safe code. Demonstrated: a compile probe
  asserting `is_sync`/`is_send` on both guards over `Cell<u32>` passed. A second probe, run under
  Miri as `probe_guard_shares`, reported "Data race detected" in `Cell::get`. They were removed.
  Fix: `unsafe impl<T: Sync> Sync` and a `PhantomData<*const ()>` marker for `!Send`.
  ownership/soundness. Low. **Fix.**
  Fixed in `ef7cf4fd`: both guards carry a `PhantomData<*const ()>`, so neither is `Send`, and are
  `Sync` only for `T: Sync`; compile-time checks beside them hold all four properties. Fails without
  it: an unconditional `Send`, or `Sync` without the bound, fails the build (E0283).

- [x] **kobj-d11** `kernel/src/sched/current.rs:31` — Shared `&Thread` is formed in 13 places
  (`sched/current.rs:31,45,59,86`; `sched/exit.rs:24,50,104,145,221,244,286,465`;
  `sched/spawn.rs:263`) while other CPUs write `Thread`'s plain, non-`UnsafeCell` scheduler fields
  through raw pointers under SCHED (`set_cpu_mask`, `set_ending`, `set_process_ended`). The module
  doc's "never forming a `&mut Thread`" does not cover the retag race. At `exit.rs:145`,
  `end_process_if_last` holds `th` across `Thread::set_process_ended`'s raw write
  (`thread.rs:429`) and reads `th.owner_pid()` at `:158`. Thread A is in a syscall while thread B
  of the same process writes A's `cpu_mask`. Demonstrated under Miri: `probe_shared_thread_ref`
  reported "Data race detected between (1) retag read ... and (2) non-atomic write" at
  `thread.rs:566`. The existing test `the_last_thread_out_of_a_process_wakes_the_reaper` fails
  under Miri with "trying to retag from ... for SharedReadOnly ... that tag does not exist" at
  `exit.rs:158`, though it passes natively. Hardware forgives it. See kobj-o3. soundness/
  opportunities. Low. **Fix**, raised to Medium.
  Fixed in `f34e8a4f`: the 13 sites read `owner_pid`, `tid` and `process` through raw-place
  accessors, and `owner_pid_of` no longer calls the `&self` method; fails without it, under Miri:
  `the_last_thread_out_of_a_process_wakes_the_reaper_with_the_status_its_exit_recorded`, at
  `exit.rs:158`. All 61 `sched::` tests pass under Miri.

- [ ] **kobj-d12** `kernel/src/object/ipc_channel.rs:627` — `IpcChannel::drop(&mut self)` (and
  `Timer::drop`, `timer.rs:215`, same shape) unlinks itself only inside its body, under SCHED.
  Until then the surviving peer's uncounted `peer` pointer (or the deadline heap) still reaches the
  object while the protected `&mut self` is live, so another CPU writes its `UnsafeCell` interior.
  Separately, `try_new_pair` (`:282`) derives each peer pointer from `&*a`, and the following
  `a.inner.get_mut()` (`kbox.rs:116`) invalidates it under Stacked Borrows. Endpoint A's last
  handle closes while another CPU sends on B: B reads `peer == A` and the send is reported `Sent`
  into an endpoint that is going away. Demonstrated: a hook at the top of `ipc_endpoint_closing`
  printed `send on B during A's Drop: Sent { woke_edge: true }` natively. Under Tree Borrows the
  write was "forbidden ... protected tag". Existing tests `closing_one_end_marks_peer_closed`,
  `transfer_moves_with_message_and_releases_on_drop` and
  `blocking_send_to_full_ring_queues_then_recv_promotes_fifo` fail under default Miri at
  `ipc_channel.rs:332`, so the IPC suite cannot be checked by Miri. `Timer` was not run. Sibling,
  unverified: `Thread::fpu_area_ptr` (`thread.rs:716`) derives a write pointer from `&KBox`.
  Fix: unlink before the `&mut` exists, and derive peer pointers from `KBox::into_raw`.
  soundness/opportunities. Low.
  **Fix** for its `try_new_pair` half, as sched-d12, and **plan** for its `Drop` half, as sched-d11,
  in P1.
  Its `try_new_pair` half fixed in `656f7e02`, as sched-d12, and its `fpu_area_ptr` sibling in
  `aaceb893`, as sched-d9; the `Drop` half stays planned.

- [ ] **kobj-d13** `kernel/src/object/memory_object.rs:203` — `MemoryObject::copy_to_kvec` (and
  `FileObject::read_to_kvec`, `file_object.rs:1043`) forms `&[u8]` over frames while a writable
  user mapping of the same object may store to them on another CPU, so the SAFETY claim "not
  aliased" is false. A parent maps its image `MAP_WRITE` and writes while another thread runs
  `sys_process_spawn(image)`. There is no TOCTOU, since the loader parses the copy: only torn
  image bytes and the abstract-model race remain. Demonstrated under Miri: `probe_copy_to_kvec_vs`
  (with `-Zmiri-permissive-provenance`) reported "Data race detected between (1) retag read ... and
  (2) non-atomic write". It was removed. Fix: `ptr::copy_nonoverlapping` into the `KVec`'s spare
  capacity (see kobj-o11). soundness/opportunities. Low. **Plan**, in P2.

- [x] **kobj-d14** `kernel/src/handle/global.rs:72` — `global::get()` is a safe `pub fn` that
  returns `&'static HandleTable` via `assume_init_ref` and checks `READY` only with `debug_assert`
  (`:73`, `:78`). A release build calling it before `init()` forms a reference to uninitialised
  memory. Latent: only `main.rs` (after `init`, `main.rs:231`) and syscall handlers call it. Early
  boot code that allocates a handle before `handle::global::init` (for example from
  `entropy::init`, `clock::init`, `dpc::init` or `IrqRouter::init`) would use the zeroed table, with
  a zero-capacity defer ring that `close()` treats as permanently full. Unverified. A release-
  profile host test calling `get()` before `init()` would settle it. Fix: check `READY`
  unconditionally, one load. soundness. Low. **Fix.**
  Fixed in `68203f0c`: the check is an `assert`; fails without it, in a release build:
  `the_table_is_refused_before_init`. Host tests run in debug, where the old `debug_assert` fired
  too.

- [x] **kobj-d15** `kernel/src/handle/entry.rs:120` — The seqlock `WriteGuard` publishes the odd
  seq with a Release store but has no release fence before the Relaxed field stores that follow.
  Release orders only operations before the store, so on a weakly ordered CPU a reader can see new
  fields with the old even seq on both sides and accept a torn snapshot. Latent until the aarch64
  port, since x86 TSO keeps stores in order. Standard writer: `store(s+1, Relaxed); fence(Release);`
  field stores; `store(s+2, Release)`. Unverified. A litmus test on aarch64, or Miri's weak-memory
  emulation on a two-thread writer/reader test, would settle it. opportunities. Low. **Fix.**
  Fixed in `68203f0c`: a Relaxed odd `seq` and a Release fence; fails without it under Miri's
  weak-memory emulation, on 16 of 16 seeds: `a_snapshot_is_never_torn`.

**Opportunities**

- [ ] **kobj-o1** encapsulate: the kernel object's type tag and its pointer are tied together
  only by hand. Adoptions write `ObjectRef::from_raw(KBox::into_raw(b).as_ptr() as *mut (),
  KObjectType::X)`, and downcasts write `unsafe { &*(r.as_ptr() as *const X) }` after a `match` or
  `lookup_typed` several lines up. `ObjectRef::object_type()` returns the tag the creator passed,
  not the header's own, and two safe helpers accept any `T` under any tag — 157 sites (103
  non-test downcasts by `grep -rnE 'as_ptr\(\) as \*(const|mut)
  (crate::object::)?(Process|Thread|MemoryObject|...)'` over non-test `kernel/src` lines, plus 54
  hand-tagged adoptions by `grep -E 'from_raw\(KBox::into_raw|ObjectRef::from_raw\(\w+,
  KObjectType::|adopt\('`; 30 downcasts are in `syscall/table.rs`, 12 in `file_object.rs`), such as
  `syscall/table.rs:358`, `syscall/table.rs:1129`, `object/file_object.rs:386`,
  `drivers/mod.rs:197`. `unsafe trait KObject { const TYPE: KObjectType; }` implemented per object
  type, whose impl promises `#[repr(C)]` with the header first (const-assert on
  `offset_of!(header) == 0`). Then safe `ObjectRef::new<T: KObject>(KBox<T>)` taking the tag from
  `T`, `ObjectRef::downcast<T: KObject>(&self) -> Option<&T>` comparing the header's tag, a
  `Ref<T>` pin that derefs to `&T`, and `lookup_as::<T>(h, pid, rights)` replacing `lookup_typed`
  plus the cast. Delete `drivers::adopt` and `io::adopt`. Checked against `table.rs:358` (match arm
  becomes `downcast`), `table.rs:1129` (`lookup_as::<Namespace>`), `file_object.rs:386` (parameter
  becomes `&Ref<FileObject>`) and `table.rs:2024` (`ObjectRef::new(po)`). `Thread` gets no `&Thread`
  downcast until kobj-o3 or kobj-d11 is fixed. Home: `kernel/src/object/header.rs`; `lookup_as` in
  `handle/table.rs`. Closes: a type-confused destroy (kobj-d8, demonstrated) and a cast made
  without its tag check. Depends on: nothing for the 13 non-`Thread` types. abi/ownership/
  soundness/concurrency/opportunities. Value high, effort medium. **Plan**, in P1.

- [ ] **kobj-o2** encapsulate: the handle table passes ownership as a raw `(*mut (), KObjectType)`
  pair. Every `allocate` caller does `into_raw` and, on `Err`, rebuilds the reference with `drop(unsafe
  { ObjectRef::from_raw(..) })`. `close` and `close_owned_batch` return a `Copy`
  `ClosedObject(pub *mut (), pub KObjectType)` that each caller adopts exactly once — 31 sites
  (`grep -n '\.allocate(' kernel/src/syscall/table.rs kernel/src/main.rs` = 26 calls, plus
  `HandleTable::duplicate` = 27; `grep -rn 'from_raw(co.0, co.1)'` = 4 close adoptions, including the
  `close_owned_batch` sweep), such as `syscall/table.rs:864`, `syscall/table.rs:1437`,
  `syscall/table.rs:2793`, `handle/table.rs:589`. `HandleTable::install(&self, pid, obj:
  ObjectRef, rights) -> Result<RawHandle, (HandleError, ObjectRef)>`, and `close(..) ->
  Result<Closed, HandleError>` where `#[must_use] struct Closed(ObjectRef)` is neither `Copy` nor
  public-field. `close_owned_batch(.., out: &mut [Option<Closed>])`. The raw `allocate` becomes
  private. Checked against `sys_memory_create` (`:864`, which passes `ptr` and the tag separately),
  `install_for` (`:1437`) and the `sys_channel_recv` install loop (`:2793`), each of which loses its
  `into_raw`/`from_raw` pair and its `unsafe` block. Home: `kernel/src/handle/table.rs`. Closes: a
  reference leaked or released twice on a failed install, a pointer installed under the wrong tag,
  the safe `allocate` that plants any pointer (kobj-d8), and the lookup use-after-free (kobj-d1):
  `Closed` is what the grace drain hands out once the release is deferred. Depends on: nothing; it
  fits best with the kobj-d1 fix and gains from kobj-o1. ownership/soundness/opportunities/
  concurrency. Value high, effort medium. **Plan**, in P1.

- [ ] **kobj-o3** encapsulate: scheduler-owned interior state is reached through `unsafe fn
  accessor(obj: *mut (), ..)` whose contract is "live, pinned by the caller, and SCHED held" by
  prose only. Six types expose `unsafe fn inner<'a>(obj) -> &'a mut Inner` with an unbounded
  lifetime, so two calls alias unchecked, and `Thread` does it with about 40 raw-field accessors.
  Each type also carries its own `unsafe impl Send/Sync` — 91-93 accessors (`grep -E 'unsafe fn
  [a-z_]+(<.a>)?\((obj|reg|po|ch|timer|thread|...): \*mut \(\)' kernel/src/object/*.rs`: 40 in
  `thread.rs`, 13 `ipc_channel`, 10 `timer`, 8 `pending_op`, 7 `notification_channel`, 7
  `interrupt_object`, 6 `userspace_server`, 1 more), with about 170 call sites in `sched/`, such as
  `object/ipc_channel.rs:328`, `object/pending_op.rs:78`, `object/timer.rs:88`,
  `object/thread.rs:415`. `struct SchedCell<T>(UnsafeCell<T>)` with `unsafe impl<T: Send> Sync` and
  `fn get<'g>(&'g self, _: &'g mut SchedState) -> &'g mut T`, where `&mut SchedState` is obtainable
  only through `SCHED.lock()`. Taking it `&mut` lets one interior be borrowed at a time, and
  accessors become safe methods. `Thread`'s scheduler fields move into `SchedCell<ThreadSched>`,
  while `saved_sp`, the FPU area and `on_cpu` (touched by `context_switch` without SCHED) keep a
  narrow raw API. Checked against `signal_pending_op_with_result`, `ipc_send_push`,
  `block_current_and_switch` and `timer_arm`, each of which already holds `g`. Home:
  `kernel/src/sched.rs` (next to `SchedState`), used from `kernel/src/object/*`. Closes: the shared
  `&Thread` race (kobj-d11), two live `&mut Inner` on one object, interior touched without SCHED
  and the 12 per-type Send/Sync claims. Depends on: kobj-o1 for the `&T` receiver (otherwise the
  liveness half stays `unsafe`). ownership/soundness/concurrency/opportunities. Value high, effort
  large. **Plan**, in P1.

- [x] **kobj-o4** encapsulate: host tests and Miri cannot see use-after-free of kernel objects.
  The kernel slab sits over a leaked 16 MiB host `Vec`, so a freed object is still inside a live
  allocation — 11 `kmalloc`/`kfree` call sites outside the allocator (`grep -E '\bkmalloc\(|\bkfree\('`
  over non-test lines, excluding `mm/slab.rs` and `mm/heap.rs`: `KBox` 3, `KVec` 3,
  `handle/segment.rs` 5), such as `libkern/kbox.rs:41`, `libkern/kbox.rs:135`,
  `libkern/kvec.rs:190`, `mm/test_support.rs:15`. Under `cfg(all(test, miri))` route `kmalloc`/
  `kfree` to `std::alloc::alloc`/`dealloc` with the layout recorded in a hidden header or a side
  table, so Miri sees each `KBox`/`KVec` as its own allocation. The production slab keeps its own
  host tests. Home: `kernel/src/mm/slab.rs` and `mm/test_support.rs`. Closes: a kernel-object use
  after free passing every host test and Miri; the kobj-d1 probe is silent under Miri today.
  Depends on: nothing, though Miri on the IPC suite also needs the kobj-d12 peer-pointer fix first.
  opportunities. Value high, effort small. **Fix**, first.
  Finished in `86723eed`: under Miri, `kmalloc` and `kfree` go to the host allocator.

- [x] **kobj-o5** encapsulate: a handle move is a lookup with `TRANSFER`, a commit into a ring,
  child or reply, then an ignored-failure `close_and_release`, with nothing claiming the entry in
  between — 8 sites (the move-commit `close_and_release` calls in `syscall/table.rs`, read off
  `grep close_and_release`: `:503` spawn, `:2008` and `:2031` send, `:2120`, `:2155`, `:2218`,
  `:2683`, `:2715` forwarded replies, across 3 move paths), such as `syscall/table.rs:503`,
  `syscall/table.rs:2008`, `syscall/table.rs:2218`. `HandleTable::claim(&self, h, pid, required)
  -> Result<Claim<'_>, HandleError>` marks the entry in transit under the rank-3 lock, so a second
  claim (or a lookup asking for `TRANSFER`) fails. `Claim::object(&self) -> &ObjectRef`,
  `Claim::commit(self)` closes the entry and cannot fail, and dropping an uncommitted claim
  restores it. Home: `kernel/src/handle/table.rs`. Closes: a move that copies (kobj-d2) and the
  spawn late-failure path (kobj-d4), where the claims would simply be committed. Depends on: nothing;
  fits best with kobj-o2. ownership. Value high, effort medium. **Fix.**
  Fixed in `39e2a5c9`: `HandleTable::claim`, `Claim::commit` and a drop that gives the handle back,
  at the send, the forwarded replies and the spawn; each of six mutations fails one of the claim
  tests.

- [x] **kobj-o6** encapsulate: plain `#[repr(C)]` ABI data is reinterpreted by hand — per-type
  `as_bytes` through `from_raw_parts(self as *const Self as *const u8, size_of)`, `mem::zeroed`,
  `unsafe KBox::try_new_zeroed` for `StoredMsg` with its restated SAFETY at each call, and
  hand-rolled `[u8; N*8]` loops for handle and `IoResult` arrays. The user-copy primitives bound `T`
  only by `Copy` — 27 sites (10 byte views by `grep -E 'from_raw_parts|as \*const \[u8|as \*mut
  \[u8'` over `object`, `libkern`, `syscall`; 6 zeroed constructions by `grep -rn
  'try_new_zeroed\|mem::zeroed()'`; 11 copy callers by `grep -rn 'copy_from_user(\|copy_to_user('
  kernel/src | grep -v 'fn \|tests'`), such as `libkern/device.rs:246`, `syscall/table.rs:1829`,
  `syscall/table.rs:1977`, `object/ipc_channel.rs:69`. `pub unsafe trait Pod: Copy + 'static {}`
  (no padding, every bit pattern valid), implemented through a macro that const-asserts
  `size_of::<T>()` equals the sum of the field sizes. Free functions `bytes_of`, `bytes_of_mut` and
  `slice_bytes`, bound `copy_from_user<T: Pod>` and `copy_to_user<T: Pod>`, add
  `copy_array_from_user`/`copy_array_to_user`, and make `KBox::<T: Pod>::try_new_zeroed()` safe (or
  a safe `StoredMsg::try_new_boxed`). Checked against `DeviceRecord`, `StoredMsg` (`bytes_of_mut`
  for the receive bounce), `IoResult` (the `sys_wait` cast) and `SpawnArgs`. Home: new
  `kernel/src/libkern/pod.rs`, with the bound in `kernel/src/mm/user_access.rs`. Closes: invalid
  values built from user bytes (kobj-d6) and kernel padding copied out. It also removes the 9-10
  `as_bytes` unsafe blocks and `sys_wait`'s `from_raw_parts`. Depends on: nothing. abi/ownership/
  soundness/opportunities. Value medium, effort small. **Fix.**
  Fixed in `efb315e2`, `d4b1fa5e` and `f114be3b`: `Pod` and `pod!`, with `bytes_of`, `bytes_of_mut`,
  `byte_array_of`, `slice_bytes` and `slice_bytes_mut`. The ten byte views and `sys_wait`'s cast go,
  `KBox::try_new_zeroed` is safe for a `Pod`, and `copy_array_from_user` and `copy_array_to_user`
  replace the hand-built handle arrays, their user side bytes as before. Pinned by `pod!`'s compile
  checks, `plain_data_round_trips_through_its_bytes`,
  `an_array_is_copied_through_user_bytes_at_any_alignment` and
  `a_typed_copy_moves_exactly_its_values_bytes`.

- [ ] **kobj-o7** narrow: the scheduler's entry points take a type-erased `*mut ()` with the
  contract "a live `T` pinned by the caller's reference", so every syscall site writes `unsafe {
  crate::sched::f(obj.as_ptr(), ..) }` on a reference it already holds as an `ObjectRef` — 60 sites
  (`grep -E 'crate::sched::[a-z_]+\([a-z_.]*\.as_ptr\(\)'` over non-test `kernel/src`: 25 in
  `syscall/table.rs`, 5 in `file_object.rs`, 12 in `drivers/xhci`, the rest in `power.rs`, `io/`
  and `drivers/`; the 29 `pub unsafe fn` wrappers they call declare the same contract), such as
  `syscall/table.rs:520`, `syscall/table.rs:1095`, `syscall/table.rs:1856`,
  `file_object.rs:1106`. Entry points take `&Ref<PendingOperation>`, `&Ref<Timer>`, `&Ref<Thread>`
  and so on, for example `pub fn timer_arm(t: &Ref<Timer>, ..)`. Leave the drop-time three
  (`ipc_endpoint_closing`, `timer_closing`, `us_reg_detach`) raw. Checked against
  `set_thread_affinity` (`:520`), `timer_arm` (`:1095`) and `notif_try_recv` (`:1856`), each of
  which already holds the typed lookup's `ObjectRef`. Home: `kernel/src/sched/*.rs`. Closes: a
  sched call after the `ObjectRef` dropped, or on the wrong type, and removes about 26 unsafe
  blocks from `syscall/table.rs`. Depends on: kobj-o1. ownership/soundness. Value medium, effort
  medium. **Plan**, in P1.

- [x] **kobj-o8** encapsulate: bounded lists pushed under SCHED rely on a `try_reserve` at
  construction matching a separate `MAX` constant, and the waiter list (`add_waiter`/`remove_waiter`/
  `take_waiters` over `KVec<*mut ()>` plus `push_within_capacity().expect`) is copied into five
  object types — 36 sites (`grep -cE 'unsafe fn (add_waiter|remove_waiter|take_waiters)'` = 15, plus
  `grep -c push_within_capacity` = 21), such as `object/pending_op.rs:144`,
  `object/ipc_channel.rs:460`, `object/notification_channel.rs:107`, `entropy.rs:152`.
  `ArrayVec<T, const N: usize>` (inline array, `try_push` that never allocates, remove/drain) and
  `WaitQueue<const N: usize>` built on it, replacing the five waiter lists, so the capacity is the
  type. Checked against `pending_op.rs:144`, `ipc_channel.rs:460` and `notification_channel.rs:107`
  (where a missing reserve silently drops the notification). Home: `kernel/src/libkern/arrayvec.rs`;
  `WaitQueue` in `kernel/src/object/mod.rs`. Closes: a reserve that drifts from its constant, so a
  push refuses or drops, or grows under rank-1 SCHED. Depends on: nothing. opportunities. Value
  medium, effort small. **Fix**, as sched-o5.
  Fixed in `978ba2dc`: a waitable's waiters are a `WaitQueue` over an inline `ArrayVec`, whose
  capacity is its type, and a wake takes the list itself; the five waiter tests pin the order taken,
  and a `take` that keeps the list fails all five.

- [ ] **kobj-o9** encapsulate: handles installed partway through a syscall are rolled back by hand
  on each later early return — 17 sites (`grep -E
  '(close_and_release|spawn_rollback_child_handles|recv_rollback_installed)\('
  kernel/src/syscall/table.rs`: 5 spawn rollbacks at `:424,443,458,469,486`, 5 in
  `sys_channel_create` at `:1908-1923`, 2 PO rollbacks at `:1707,1745`, 5 recv rollbacks at
  `:2801-2827`), such as `syscall/table.rs:458`, `syscall/table.rs:1917`, `syscall/table.rs:1707`,
  `syscall/table.rs:2809`. `struct Installed<const N: usize> { pid, handles: [RawHandle; N], len }`
  with `push(h)` and `commit(self) -> [RawHandle; N]`, whose `Drop` closes every handle not
  committed. Checked against `sys_channel_create`, `sys_process_spawn` and `sys_channel_recv`.
  Home: `kernel/src/syscall/table.rs` (or `kernel/src/handle/`). Closes: a handle left in a table,
  pinning its object, when a new early return forgets the rollback. Depends on: nothing. ownership.
  Value medium, effort small. **Plan**, in P1.

- [ ] **kobj-o10** encapsulate: uncounted back-pointers to kernel objects (`IpcChannel.peer`,
  `IpcChannel.us_reg`, `CachedFile.obj`, the handle entry's object pointer) are each upgraded with
  `try_acquire`, or dereferenced, under whatever lock the target's `Drop` takes before freeing.
  Nothing ties the upgrade to that lock, and the defensive `is_live` magic check reads memory that
  may already be freed — 13 sites (`grep -E 'try_acquire\(|try_acquire_refcount\('` excluding the
  definition = 7 upgrades: `userspace_server.rs:216,246,287`, `sched/ipc.rs:253,341`,
  `handle/table.rs:417`, `drivers/console.rs:224`; plus 4 raw derefs of `us_reg`/`peer` in
  `sched/ipc.rs` and 2 `peer` derefs in `IpcChannel::send_push`/`send_or_queue`), such as
  `object/userspace_server.rs:216`, `sched/ipc.rs:253`, `handle/table.rs:417`.
  `struct WeakObj<T: KObject, W>`: a raw `*mut T` set and cleared only with a `&mut W` witness, with
  `upgrade(&self, &W) -> Option<Ref<T>>` and `get<'w>(&self, &'w W) -> Option<&'w T>`, where `W` is
  the lock the target's free path is ordered after (`SchedState` for `peer`/`us_reg`, the files
  guard for the cache, the grace read guard for the handle table once close is deferred). Checked
  against all three examples. Home: `kernel/src/object/header.rs`. Closes: upgrading a back-pointer
  without holding what the free path waits for, the shape of kobj-d1. Depends on: kobj-o1, and the
  kobj-d1 fix for the handle-table instance. opportunities. Value medium, effort medium.
  **Plan**, in P1.

- [ ] **kobj-o11** encapsulate: frame contents are reached by raw HHDM arithmetic, `(frame.as_u64() +
  heap::hhdm_offset()) as *mut u8`, each site with its own "owned, mapped, unaliased" SAFETY claim,
  some forming `&[u8]` over frames a user mapping or DMA can write, and false for a borrowed
  aperture — 11 sites in scope (`grep -E 'hhdm_offset\(\)'` over non-test lines of
  `kernel/src/{object,syscall,handle,libkern}`: `file_object` 5, `memory_object` 4,
  `syscall/table` 2; 26-35 kernel-wide), such as `object/memory_object.rs:79`,
  `object/memory_object.rs:203`, `object/file_object.rs:1350`, `syscall/table.rs:2732`.
  `frame_fill(f, at, len, byte)`, `frame_write(f, at, &[u8])`, `frame_read(f, at, &mut [u8])` and
  `frame_copy(src, dst, len)`, each asserting `at + len <= PAGE_SIZE` and using raw copies only,
  reached through `MemoryObject`/`FileObject` methods taking `&self` (a `FrameRef<'a>` borrowed
  from the owner that carries ownership and caching). Checked against `copy_to_kvec`,
  `copy_fill_into_frame` and `reserve`'s zeroing. Home: `kernel/src/mm/` (`heap.rs` or a new
  `frame.rs`). Closes: a reference over concurrently written frames (kobj-d13) and per-site
  `min(PAGE_SIZE)` arithmetic. Depends on: nothing. soundness/opportunities. Value medium (low per
  one reviewer), effort small. **Plan**, in P2.

- [ ] **kobj-o12** encapsulate: the kernel's rsproto codec writes and parses the 28-byte RS
  envelope by hand-coded offsets — 98 sites (`awk 'NR<624' kernel/src/rsproto.rs | grep -cE
  'get_u(16|32|64)\('` = 40 less 3 definitions = 37 reads, `put_` = 64 less 3 = 61 writes), such as
  `rsproto.rs:265`, `rsproto.rs:398`, `rsproto.rs:146`, `rsproto.rs:432`. Five builders repeat the
  same eight `put_*` header lines and five parsers re-derive magic, flags, `body_len` and the body
  slice, with body reads that index and panic past the slice. `struct RsEnvelope { op, request_id,
  flags, body_len, handle_count }` with `parse(buf) -> Option<(RsEnvelope, Body<'_>)>`, `write(out,
  op, body_len) -> Option<&mut [u8]>` and `Body::{u16_at, u32_at, u64_at}(off) -> Option<_>`.
  Checked against `parse_reply`, `parse_read_range_reply` and `file_blocks_reply_header`. Home:
  `kernel/src/rsproto.rs`. Closes: a reply read at an unchecked offset (nothing reaches one today:
  a 200,000-input fuzz found none) and a builder writing a header field at a different offset.
  Depends on: nothing. abi. Value low, effort small. **Later.**

- [ ] **kobj-o13** remove: raw `kmalloc` scratch and in-place segment initialisation in
  `handle/segment.rs`, where `KVec` and `KBox` exist — 7 sites (`grep -n unsafe
  kernel/src/handle/segment.rs` before the tests: 6 blocks at lines 64, 81, 89, 101, 109, 121, 126
  and 1 unsafe fn `free_entries` at 137; the only raw `kmalloc`/`kfree` user outside the
  containers), such as `handle/segment.rs:74`, `handle/segment.rs:101`, `handle/segment.rs:125`.
  `perm: KVec<u32>` with `try_reserve(SEGMENT_LEN)` and `swap(i, j)`; the 256 KiB entries block
  becomes a `KBox<SegmentEntries>` built in place by a `KBox::try_new_with(|slot: &mut
  MaybeUninit<T>| ..)`, published with `into_raw` and freed with `from_raw` in `Drop`. Home:
  `kernel/src/handle/segment.rs`, plus a small constructor in `kernel/src/libkern/kbox.rs`. Closes:
  a leak or double free of the scratch buffer on a new early return, and raw index arithmetic.
  Depends on: nothing. opportunities. Value low, effort small. **Later.**

What held up: `mm/user_access.rs` range-checks every copy with `checked_add`, aligns `UserPtr<T>`,
and validates the whole `copy_cstr` range. Every user length is bounded before it slices a fixed
buffer (`sys_kprint`, namespace paths, `sys_entropy_read`, `sys_wait`, channel send/recv,
spawn). There is no double fetch: `SpawnArgs`, `ThreadArgs`, `IoOp`, messages and handle arrays
are copied once and used from the copy, and spawn parses the ELF from a kernel copy.
`StoredMsg::seal` zeroes everything past the payload, and every `as_bytes`/`copy_to_user` ABI
type was checked field by field for padding (none). `cargo xtask abi-sync-check` reported 193
values in agreement. A 200,000-input probe fed the rsproto parsers and `block_map` random replies
with no panic, reaching the deep paths. `ObjectRef` follows Arc discipline (Relaxed increment,
Release decrement, Acquire fence, refcount overflow guard) and `try_acquire` refuses zero.
`lookup` gives back its step-7 reference on every later exit, `close` hands the entry's one
reference to the caller, and no `ObjectRef` is dropped under SCHED on the IPC paths. The
`us_reg` and `peer` back-pointers and the `FileObject` cache are cleared or upgraded under
the lock their `Drop` takes. `sys_channel_recv`, `sys_channel_create` and `sys_process_spawn`
roll back every installed handle on each pre-commit failure. `KBox`, `KVec` and `KString`
(ZSTs, growth, `clear`, `remove`) and the spinlock acquire/release orderings hold, as do the
handle-table segment publication, the seqlock reader, the `on_cpu` guard, and the bounce-buffer
wipe in `sys_entropy_read` (checked in the built ELF with objdump).

### sched — main at 135055a8, 2026-10-26

Lenses: abi, ownership, soundness, concurrency, opportunities; returned nothing: none.
Defects: found 35, refuted by a boot 0, reported 35 (the same defects merged into 15 below). Opportunities: 25 (merged into 11 below).

The inventory, `cargo xtask unsafe-inventory` over its paths:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `kernel/src/sched.rs` | 337 | 25 | 3 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/sched` | 1674 | 162 | 38 | 0 | 0 | 0 | 0 | 20 | 0 | 0 | 0 |
| `kernel/src/dpc.rs` | 60 | 1 | 0 | 2 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| `kernel/src/clock.rs` | 40 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/object/pending_op.rs` | 91 | 9 | 8 | 2 | 0 | 0 | 0 | 2 | 0 | 0 | 0 |
| `kernel/src/object/notification_channel.rs` | 104 | 8 | 7 | 2 | 0 | 0 | 0 | 2 | 0 | 0 | 0 |
| `kernel/src/object/timer.rs` | 96 | 11 | 9 | 2 | 0 | 0 | 0 | 2 | 0 | 0 | 0 |
| `kernel/src/libkern/spinlock.rs` | 136 | 7 | 0 | 4 | 0 | 0 | 0 | 5 | 0 | 0 | 0 |
| `kernel/src/libkern/lockrank.rs` | 262 | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/arch/smp.rs` | 8 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/arch/x86_64/smp.rs` | 66 | 5 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/arch/x86_64/context.rs` | 75 | 3 | 4 | 0 | 0 | 0 | 0 | 2 | 0 | 2 | 0 |
| `kernel/src/arch/x86_64/resched.rs` | 6 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/arch/x86_64/timer.rs` | 143 | 8 | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `total` | 3098 | 242 | 77 | 12 | 0 | 0 | 0 | 34 | 0 | 2 | 0 |

**Defects**

- [x] **sched-d1** `kernel/src/sched/exit.rs:400` — `thread_exception_frame` returns the raw
  kernel-stack address of a suspended thread's `ExceptionFrame` and drops SCHED, and
  `sys_thread_get_registers` (`syscall/table.rs:611-613`) then reads it through the safe
  `ArchRegisters::read_from_exception_frame(frame_ptr: usize)` (`arch/x86_64/idt.rs:692`, which
  forms `&ExceptionFrame` from any usize) with nothing keeping the thread suspended. The SAFETY
  argument at `idt.rs:693-697` ("the thread stays parked while suspended") does not hold.
  Thread T (its handle has SIGNAL) faults and is suspended. Thread A gets `frame_ptr`. Thread B's
  `sys_exception_resume`, or a sibling's `sys_process_exit` through `end_siblings`, resumes T on
  another CPU. T's next trap or syscall lands at the same stack top and overwrites the 176 bytes. A
  then reads them with no synchronisation: a data race on a live kernel stack, with kernel return
  addresses and saved registers copied to user memory. There is no use-after-free, because A's
  handle pins the Thread and its stack. The deferred ModifyAndResume write-back built the same way
  would write into a running thread's stack, which is the ordinary change that makes this high.
  Fix: decode `RegisterValues` under SCHED and return them, and make the frame decoder unsafe or
  private. The decoder taking any usize is a second defect: a Miri probe with a dropped Box's
  address reports a dangling `&ExceptionFrame` at `idt.rs:697`.
  Demonstrated by five host tests in `sched/tests.rs`, each of which ran the real functions in
  syscall order and then simulated the thread reusing its stack by overwriting the frame buffer.
  `thread_exception_frame` → Some(p); `resume_suspended` or `end_siblings` → true;
  `take_disposition`; overwrite; `read_from_exception_frame(p)` returned the overwritten word (for
  example rip = 0xffff80000badf00d), and `thread_exception_frame` was then None. The overwrite is
  the one simulated step, since no host test runs a thread. Miri showed the any-usize decoder.
  abi, ownership, soundness, concurrency, opportunities. Medium. **Fix.**
  Fixed in `3d1174b5`: with kplat-o4, the arch fault path decodes the registers,
  `suspend_with_fault` stores them in the `Thread` under `SCHED`, and `suspended_registers` copies
  them out under it; fails without it: a host test on the old API (not kept, its API gone) read the
  overwrite, rip 0xbdbd_bdbd_bdbd_bdbd, after a resume.
  `a_suspended_threads_registers_are_its_faults_until_it_resumes`, and test-harness fails a read
  whose rip is 0.

- [x] **sched-d2** `kernel/src/sched/ipc.rs:527` — Outstanding page-cache fills
  (`UserspaceServerReg.pending_fill`) are never completed when a userspace server dies.
  `ipc_endpoint_closing` (`:508`, `:527`) and `us_reg_detach` (`:281`) drain only the lookups, with
  PeerClosed. The fill's PendingOperation is never signalled, and the cycle reg →
  `PendingFill.file_obj` → `Producer::FsServer{reg}` is never broken. A server replies
  OBJECT_KIND_FILE (`table.rs:2398`), a client maps the file and faults, and `us_forward_originate_fill`
  records the fill. The server dies or its binding is unbound before it replies. The faulting
  thread loops in `file_object.rs:1177` `wait_status` (`wait_on_io(u64::MAX)`), which on an exit wake
  sees TimedOut and waits again. It can never end, so its process never ends, no handles are swept,
  and the registration, endpoint, FileObject, cache frame and PO leak. This is an unkillable hang
  plus a leak. No shipping server replies OBJECT_KIND_FILE today. It is not the recorded
  `TODO(unsupervised-fault)`, which is a fill that fails, not one that never completes. Fix: one
  `fail_all` on the registration that drains both tables, used at all three sites.
  Demonstrated by host tests in `object/userspace_server.rs` and `sched/ipc.rs` (since removed). A
  channel pair (k, s) with a registration attached and one lookup and one fill outstanding, then
  `drop(s)`: "lookup signalled=true fill signalled=false". After dropping the registration too, the
  fill was still unsignalled. A second test with `Producer::FsServer{reg}` counted 0 destroys of
  reg, file and PO (expected 1, 1, 1). Only `take_pending_fill_matching` (a matching reply, or
  originate's rollback) takes `pending_fill`. abi, ownership, soundness, concurrency,
  opportunities. Medium. **Fix**, as kobj-d3.
  Fixed in `7938344d`, as kobj-d3.

- [x] **sched-d3** `kernel/src/dpc.rs:71` — The safe `pub fn enqueue(dpc: &Dpc)` takes a borrow of
  any lifetime and stores its address in the global queue. `run_pending` (`:122`) later turns that
  address back into `&Dpc` and calls through it. Safe code can therefore queue a Dpc that is dead
  by the drain. `io/mod.rs:140-141` already queues a stack-local Dpc, whose `ctx` is an
  InterruptObject the function then drops. That is sound only because the self-test runs on the BSP
  before `bring_up_aps`. Moving it after AP bring-up lets another CPU's tick-tail drain take the
  address first, so the self-test returns and a CPU calls `signal_interrupt` on a freed object
  through a dead stack slot (an indirect call in IRQ context). The 9 heap-embedded IRP DPCs hold
  only by the convention that the IRP box is parked as its handler's last act. Fix:
  `enqueue(&'static Dpc)` plus an `unsafe fn enqueue_raw(NonNull<Dpc>)` (see sched-o3).
  Demonstrated: Miri on safe code only, `{ let d = dpc_for(&c); enqueue(&d); } run_pending();`
  (also a Box dropped before the drain) reports "constructing invalid value of type &dpc::Dpc:
  encountered a dangling reference" at `dpc.rs:122`. Control: `enqueue_then_drain_runs_handler_once`
  passes under Miri. abi, ownership, soundness, concurrency, opportunities. Medium. **Fix.**
  Fixed in `1b7057f2`: `enqueue` takes `&'static Dpc`, and the unsafe `enqueue_raw` queues a heap
  DPC valid until its handler runs; the self-test's DPC is a static. The survey's program is
  undefined behaviour under Miri on the old code (a dangling `&Dpc` at `dpc.rs:143`) and does not
  compile with this (E0597), so it is not kept as a test.

- [x] **sched-d4** `kernel/src/arch/x86_64/idt.rs:663` — An NMI (vector 2, per-CPU IST2 stack) that
  lands in ring 3 is treated as a user fault: `exception_dispatch` → `user_fault` →
  `suspend_with_fault(frame_ptr)` with the frame on the IST2 stack. `is_user_fault` checks only
  `cs & 3` outside STOPPING. The thread is parked and context-switched off IST2, leaving its
  ExceptionFrame, saved_sp and callee-saved frame on a stack the TSS hands to the CPU's next NMI.
  `suspend_with_fault`'s premise (`exit.rs:301`, "the ExceptionFrame stays on this frozen kernel
  stack") fails. #DF on IST1 with a ring-3 CS does the same. A BMC or chipset NMI, a future NMI
  watchdog or a profiler arrives while a user thread runs. The thread is reported as a segfault and
  parked on CPU X's IST2. A second NMI on X overwrites its parked context, or the thread resumes on
  CPU Y while X takes an NMI, and `context_switch` pops a clobbered return address in ring 0:
  kernel memory corruption. No NMI source exists today, hence medium. Fix: handle vectors 2 and 8
  from ring 3 without suspending: log and return on the IST stack, never park.
  Demonstrated by a QEMU boot at -smp 4 (release image, greeter driven by keystrokes). HMP `nmi`
  is dropped because LAPIC LINT1 is masked, so a probe had the tick on CPU 0 send an APIC NMI to
  CPUs 1-3 whenever they ran a user-owned thread, and made ring-0 NMIs return. Serial showed
  `PROBE: NMI from ring 3` then `*** unhandled ring-3 fault ... pid 15 tid 19 segfault @ 0x41e6b3
  ***`, and the compositor's outbox overflowed afterwards. The second-NMI overwrite was not booted;
  it follows from the shared IST2 top. Probes reverted. concurrency. Medium. **Fix.**
  Fixed in `21059535`: a fault is a user fault only from ring 3 and on the thread's own stack: a
  ring-3 NMI is logged and returned from, a ring-3 #DF is fatal, and no thread is ended from an IST
  stack; fails without it: `an_nmi_or_double_fault_from_ring_3_is_not_a_user_fault`, at "vector 2
  from ring 3". The NMI path is not booted.

- [x] **sched-d5** `kernel/src/dpc.rs:98` — The DPC queue is reserved to 64 on the premise that
  the bound is the number of distinct Dpc objects, but every IRP embeds its own Dpc. The block
  layer's refusal paths queue it from thread context with no drain: `submit_within`
  (`io/block.rs:119`) and `forward`'s window and rebase refusals (`:515`, `:540`). Nothing bounds
  how many accumulate between interrupt tails, and the 65th enqueue panics ("DPC queue reserve
  exhausted"). A holder of a block DeviceNode handle with READ (the storage service, an fs server)
  issues 65 or more `sys_io_submit` reads past the device end between two ticks (about 2.5-10 ms).
  Each returns at once with a PO and its refused IRP queued from thread context. The result is a
  kernel crash. A corrupt filesystem whose block map runs past the partition is the plausible
  untrusted route; that was not shown. Fix: complete refused IRPs without a thread-context DPC (or
  raise the completion self-interrupt as `ramdisk_submit` does), or make the IRP queue intrusive
  and unbounded.
  Demonstrated by a host test in `io/block.rs` (since removed). `dpc::init()`, a 64-block disk and
  a backend that never completes, then `dispatch_block_irp(.., offset = 64*512, 512)` 65 times
  without `run_pending`. Submits 0-63 returned Ok; submit 64 panicked at `dpc.rs:98`. concurrency.
  Medium. **Fix.**
  Fixed in `b54b5e30`: with drv-o5, a completed IRP is linked onto one list that one static DPC
  drains, so the queue holds static DPCs only; fails without it:
  `more_refusals_than_the_dpc_queue_holds_each_complete`, whose 65th refused read before a drain
  panicked at `dpc.rs:114`.

- [x] **sched-d6** `kernel/src/sched/exit.rs:437` — `reap_pending` is the only drain of the global
  completed-IRP list (`io::block::reclaim_completed`), the char-driver reclaim lists and each CPU's
  reap list, and it runs only from `idle_body`, `exit_thread`, `exit_process` and
  `suspend_with_fault`. `yield_now` has no production caller, and neither the preemptive switch nor
  blocking reaps. The claim in `lock-ordering.md` that "the next scheduler entry's `reap_pending`
  drains the list" does not hold. An unprivileged process starts one busy-loop thread per online CPU,
  so the idle thread never runs. Each completed block IRP's IrpBox (PO, device ref, frags, pinned
  buffer) then piles up on RECLAIM without bound, one per page-cache fill and per `sys_io_submit`.
  An exited thread's Thread, Process and AddressSpace wait in `reap[cpu]` until the next exit on
  that CPU. Kernel memory grows until the load stops. Fix: drain from a thread guaranteed to run
  (the reaper) when the lists are non-empty, or from the blocking switch path.
  Unverified: no busy-loop program was available in the image. A reading and grep of `main` found
  `reap_pending()` called only from `idle_body` (`spawn.rs:282`), `exit_thread`/`exit_process`
  (`exit.rs:204`, `:216`), `suspend_with_fault` (`:269`), `yield_now` (`sched.rs:385`, no production
  caller) and tests, and `reaper_body` only closes handle tables. To settle it, boot with N busy user
  threads plus a file-reading loop and watch RECLAIM's length or free memory grow. concurrency.
  Medium. **Fix.**
  Fixed in `588fcaaf`: the completion that brings the completed-IRP list to 32 boxes wakes the
  reaper, which drains it and re-checks it before parking; fails without it:
  `reclaim_under_load_self_test`, two threads spinning on every CPU, which panicked test-qemu with
  "128 of 128 completed IRPs left unreclaimed".

- [x] **sched-d7** `kernel/src/sched/ipc.rs:312` — `us_server_attach` overwrites an endpoint's
  `us_reg` back-pointer unconditionally, and `sys_ns_bind` checks `us_forward_existing_reg`
  (`syscall/table.rs:1202`) in a SCHED hold of its own before allocating and attaching (`:1223`).
  Two binds of one endpoint can interleave and leave two registrations, both starting request ids at
  1, with replies routed only to the one attached last. A supervisor holding BIND_NAMESPACE binds
  server endpoint K at two paths from two threads. Both see no registration and attach in turn, so
  `K.us_reg` = reg_b. Client A (pid 10) looks up through binding A and client B (pid 20) through
  binding B, and both arrive at the server as request id 1. The server answers A's request, the
  kernel routes the reply to reg_b, and B's lookup completes with the handle the server resolved
  for A's path and base: B gains A's object. A's lookup never completes until reg_a is destroyed,
  and a server death drains only the last-attached registration. The doc on `set_us_reg` ("Set once")
  does not hold. Reachable only by a trusted party. Fix: check and attach in one SCHED hold
  (attach refuses when `us_reg` is set).
  Demonstrated by host tests (since removed) using the real sched entry points in the interleaved
  order: `us_forward_existing_reg(k)` → None twice; two registrations attached; lookups from pid 10
  and 20; the reply to request 1 completed pid 20's lookup (assertion: left 20, right 10). The
  interleaving itself was shown by reading the code (two SCHED holds), not run. A second test showed
  reg_a's lookup left unsignalled at server death. abi, ownership. Medium. **Fix**, as kobj-d7.
  Fixed in `d43466d6`, as kobj-d7.

- [x] **sched-d8** `kernel/src/libkern/spinlock.rs:91` — `SpinLockGuard` (and `IrqSpinLockGuard` at
  `:249`) holds only `&SpinLock<T>`, so it is auto-Sync and auto-Send whenever `T: Send`. Through a
  shared `&guard`, Deref gives `&T` on several threads for a `T` that is not Sync. Sending a guard
  to another CPU would drop it there, decrementing the wrong CPU's preempt counter or restoring that
  CPU's IF. Safe code that locks a `SpinLock<Cell<u32>>` and shares `&guard` with two threads that
  both call `.set()` races. No kernel path shares a guard today; the first lock over a `!Sync` value
  whose guard is lent across CPUs reaches it. Fix: a `PhantomData<*const ()>` marker (so `!Send`)
  plus `unsafe impl<T: Sync> Sync` on both guards, as std's `MutexGuard`.
  Demonstrated: Miri on safe code (`SpinLock<Cell<u32>>` and an `IrqSpinLock` twin, `&guard` into
  `std::thread::scope`) reports "Data race detected between (1) non-atomic write ... and (2)
  non-atomic read" in `Cell::get`/`Cell::replace`. That the probe compiled shows the guard is Sync.
  abi, ownership, soundness, concurrency, opportunities. Low. **Fix**, as kobj-d10.
  Fixed in `ef7cf4fd`, as kobj-d10.

- [x] **sched-d9** `kernel/src/object/thread.rs:716` — `Thread::fpu_area_ptr` derives the
  `*mut ArchFpuState` it gives `switch_into` from `&raw const **area`, which goes through
  `KBox::deref` (a shared reference). `fpu_save` (an XSAVE store) then writes through a pointer
  whose provenance permits reads only, on every context switch. The SAFETY comment's "live,
  64-byte-aligned images" does not establish write permission. The hardware forgives it and asm is
  opaque to the optimiser, but a Rust-side write there would be UB. Fix: take the pointer from the
  box's raw pointer (`&raw mut **area` through `&mut`, or keep the area as `NonNull`).
  Demonstrated: Miri on `Thread::try_new_boot` then `fpu_init_area(Thread::fpu_area_ptr(obj))` (the
  pure-Rust sibling with the same contract) reports "attempting a write access using <tag> ... but
  that tag only grants SharedReadOnly permission", the tag created at `thread.rs:716`. Control:
  deriving it as `match &mut (*p).fpu { Some(area) => &raw mut **area }` passes; the file was
  restored. A second probe (one byte written through the pointer) gave the same report. ownership,
  soundness. Low. **Fix.**
  Fixed in `aaceb893`: `fpu_area_ptr` derives the area's pointer from `&mut`, and
  `ArchFpuState::initial()` is its only constructor; fails without it, under Miri:
  `the_fpu_area_pointer_may_be_written_through`.

- [x] **sched-d10** `kernel/src/sched/exit.rs:50` — Whole-struct `&Thread` is formed under SCHED
  at 13 sites (`exit.rs:24,50,104,145,221,244,286,465`, `current.rs:31,45,59,86`, `spawn.rs:263`)
  on the argument that SCHED serialises all access. But `context_switch` stores the outgoing
  thread's `arch.sp` after `switch_into` released SCHED, so the retag races that store. CPU B blocks
  T (pushed to `blocked`, SCHED released) and is executing `mov [rdi], rsp`. CPU A takes SCHED in
  `has_live_siblings`, `end_siblings`, `place_spawned` or `current_owner_pid` and retags
  `&*(T as *const Thread)` to read `owner_pid`. The two accesses are unordered: a data race in
  Rust's model, forgiven by the hardware because only `owner_pid` is used. The Thread accessor
  contract ("single-CPU, SCHED serialises all access") misses that `arch.sp` is written outside
  SCHED. Fix: read the field through a raw place (`ptr::read(&raw const (*p).owner_pid)`), or put
  `arch` in an `UnsafeCell` (sched-o2).
  Demonstrated: Miri on a host test in which one thread unlocks a `Mutex<SchedState>`, then writes
  `saved_sp` through `saved_sp_mut_ptr` (as `context_switch` does) and sets a relaxed flag, while
  the other waits on the flag, locks and calls `has_live_siblings`. It reports "Data race detected
  between (1) non-atomic write ... and (2) retag read of type object::thread::Thread" at
  `exit.rs:50`. Control: the same interleaving through the raw-field accessor `Thread::is_ending`
  passes, as does moving the write before the unlock. soundness, concurrency. Low.
  **Fix**, as kobj-d11.
  Fixed in `f34e8a4f`, as kobj-d11.

- [ ] **sched-d11** `kernel/src/object/ipc_channel.rs:629` — `IpcChannel::drop`, `Timer::drop`
  (`timer.rs:221`) and `UserspaceServerReg::drop` (`userspace_server.rs:489`) run with a live
  `&mut self` (Box retag, protected Drop argument) until their closing hook acquires SCHED. Other
  CPUs still reach the object through uncounted pointers and mutate it under SCHED: a send through
  `peer` writes its ring, the tick's `fire_timer` writes the timer from the deadline heap, and
  `ipc_endpoint_closing` or `try_acquire` reaches the registration through `us_reg`. Endpoint A's
  last handle closes on CPU 1 and `IpcChannel::drop(&mut self)` spins on SCHED in
  `ipc_endpoint_closing`. CPU 2 holds SCHED in `ipc_send_push(B)` and `b.peer` still names A, so
  `send_push` writes A's ring (`self.len += 1`) while CPU 1's `&mut A` is live. Fix: in
  `dispatch_destroy`, run the type's closing hook on the raw pointer before `KBox::from_raw`.
  Demonstrated: a host test in which thread B took SCHED and set a flag then ran
  `IpcChannel::send_push(b)`, while thread A waited on the flag and dropped the last ref to a. Miri
  (Stacked Borrows) reports "trying to retag from <..> for SharedReadOnly permission ... tag does
  not exist", the peer tag invalidated by the Unique retag at `kbox.rs:116`. Under Tree Borrows it
  reports "Data race detected between (1) non-atomic write ... ipc_channel.rs:149 self.len += 1 and
  (2) retag read of type IpcChannel ... drop_in_place". concurrency. Low. **Plan**, in P1.

- [x] **sched-d12** `kernel/src/object/ipc_channel.rs:283` — `IpcChannel::try_new_pair` takes each
  peer pointer from a shared borrow (`&*b as *const Self`), then retags the box Unique through
  `a.inner.get_mut()` / `b.inner.get_mut()`. That invalidates the pointer just stored in the other
  endpoint, so every later `inner(peer)` reached from `sched/ipc.rs` (`send_push`, `send_or_queue`,
  `clear_peer`) uses an invalidated tag. This is UB under Stacked Borrows, not under Tree Borrows,
  and nothing has miscompiled. It stops Miri on any test that builds an IPC pair, which hides the
  IPC paths from Miri. Fix: take the pointers with `&raw mut *a` after the `get_mut` writes, or from
  `KBox::into_raw`.
  Demonstrated: on unmodified main, `MIRIFLAGS='-Zmiri-disable-isolation -Zmiri-ignore-leaks'
  cargo +nightly-2026-07-20 miri test --lib --target x86_64-unknown-linux-gnu
  sched::ipc::tests::an_endpoint_holding_another_in_a_held_send_is_refused_too` reports "trying to
  retag from <1785035> for SharedReadOnly permission ... but that tag does not exist in the borrow
  stack" at `ipc_channel.rs:332`, the tag created at `:283` and invalidated by the Unique retag in
  `KBox::deref_mut` (`kbox.rs:116`). The same test passes with `-Zmiri-tree-borrows`. opportunities.
  Low. **Fix.**
  Fixed in `656f7e02`: `try_new_pair` gives each endpoint the other box's own pointer, from
  `KBox::into_raw`, so no later borrow of a box retires it; fails without it, under Miri's Stacked
  Borrows: `an_endpoint_holding_another_in_a_held_send_is_refused_too`, which now passes there with
  the 17 `ipc_channel` tests.

- [x] **sched-d13** `kernel/src/dpc.rs:107` — `run_pending_in` drains into `[0usize; DPC_RESERVE]`
  with `buf[..n].copy_from_slice(&q)`, where `n` is the queue's length. The length is bounded by the
  KVec's capacity, not by `DPC_RESERVE`. The two agree only because `try_reserve` from an empty KVec
  allocates exactly the amount requested. Changing KVec growth to round up to an allocator bucket,
  or reserving the queue larger in `init`, makes a drain with more than 64 distinct DPCs queued
  panic at an interrupt tail with an out-of-range slice. Fix: take the buffer size from the queue's
  type (sched-o4) or drain in chunks.
  Demonstrated by a host test (since removed): a fresh queue reserved to `DPC_RESERVE+1` (capacity
  128) and 65 distinct Dpcs queued panics at `dpc.rs:111`, "range end index 65 out of range for
  slice of length 64". In production `init` reserves exactly 64 from empty. ownership. Low. **Fix.**
  Fixed in `aa9a89e2`: the DPC queue is an `ArrayVec` in its static, and a drain takes the list by
  value, so the batch is the queue's type; `a_queue_of_its_capacity_drains_whole` and
  `a_dpc_past_the_capacity_is_fatal` hold the bound at both sides. The survey's reproduction, a
  queue reserved larger than its drain buffer, cannot be written now.

- [x] **sched-d14** `kernel/src/sched.rs:368` — `alloc_pid` uses `next_pid.wrapping_add(1)`, so
  after 2^32-2 spawns pids come round to 0, 1, 2, and so on. `sched.rs:238` says "No pid reuse" and
  `docs/architecture/handle-system.md:343` treats pid reuse as no hazard, yet handle ownership is
  keyed solely by `owner_pid` (`handle/global.rs`). After about 4.29 billion
  `sys_process_spawn` calls a child gets pid 0, whose handles `reap_pending` never sweeps
  (`exit.rs:470` skips pid 0), then pid 1 (the boot parent) and other live pids. A child sharing a
  live process's pid passes that process's `owner_pid` checks and gains its authority. Fix: refuse
  to spawn at exhaustion, or widen to u64.
  Demonstrated by a host test in `sched/tests.rs` (since removed): `SCHED.next_pid = u32::MAX`, then
  `alloc_pid()` four times gave 0xffffffff, 0, 1, 2 ("the pid source wrapped"). The spawn count
  needed keeps it low. abi. Low. **Fix.**
  Fixed in `3fe982fa`: `take_pid` refuses once the pids are spent, and a spawn is refused
  `OutOfMemory`; fails without it: `pids_are_refused_once_spent_never_reused` (a wrapping add, or a
  saturating one).

- [ ] **sched-d15** `kernel/src/arch/x86_64/smp.rs:39` — `adopt_dense_index` matches `hw_apic_id()`
  (CPUID.01H:EBX[31:24], 8 bits) against the 32-bit `lapic_id`s the BSP bound. An AP whose x2APIC
  id is 256 or more and whose low byte equals another CPU's id adopts that CPU's dense index: the
  dense-index collision class this kernel has hit before. `identity_bound()` is also false for it,
  so its park leaves its online bit set. The comment's "sufficient while MAX_CPUS <= 255" confuses
  the CPU count with the id values. Limine is asked with flags 0 today (`limine.rs:332`), so ids
  are xAPIC ids of 255 or less. Once x2APIC is requested and APs start (`TODO(smp-cpu-count)`
  records the count), an id such as 256 reads 0 and shares the BSP's GDT/TSS, scheduler slots and
  preempt counters. Fix: read the full id from CPUID.0BH/1FH EDX, or the x2APIC ID MSR after
  `enable_this_cpu`.
  Unverified: reading confirms the 8-bit read (`smp.rs:21-24`) against the 32-bit binds
  (`main.rs:416/436`). A boot with `-smp 1,sockets=2,cores=256,threads=1,maxcpus=512 -device
  max-x86_64-cpu,socket-id=1,core-id=0,thread-id=0` gave an id-256 AP, but ACPI listed 2 CPUs and
  the kernel brought up only `cpu 0 online ... 1 of max 8`, so the AP was never started. To settle
  it, request x2APIC in `limine.rs` and boot that topology. concurrency. Low. **Later.**

**Opportunities**

- [ ] **sched-o1** encapsulate: scheduler-owned state is reached through `pub(crate) unsafe fn
  x(obj: *mut ())` accessors whose contract is "live, pinned, and SCHED held", or through `unsafe fn
  inner<'a>(obj) -> &'a mut Inner` with an unbounded lifetime. Nothing proves the lock is held or
  stops two `&mut Inner` from coexisting — about 169 sites (the call sites in sched outside tests:
  86-88 Thread-accessor calls by `grep -oE 'Thread::[a-z_]+\('` over `sched.rs` and
  `sched/{current,exit,ipc,objects,placement,preempt,spawn,timers,wait}.rs`, plus 83 calls to the
  other types' accessors by `grep -rnoE '\b(Timer|PendingOperation|NotificationChannel|InterruptObject|IpcChannel|UserspaceServerReg)::[a-z_]+\('`
  over the same files; they serve 94 accessor fns by `grep -c 'pub(crate) unsafe fn'` over
  `kernel/src/object/{thread,timer,pending_op,notification_channel,ipc_channel,interrupt_object,userspace_server}.rs`
  = 40+10+7+6+17+6+8), such as `kernel/src/object/timer.rs:86`, `kernel/src/object/thread.rs:415`,
  `kernel/src/sched/timers.rs:81-118`. A `SchedToken` (zero-sized, a field of `SchedState`, so only
  the SCHED guard reaches one) and `SchedCell<T>(UnsafeCell<T>)` with `fn get<'a>(&'a self, _: &'a
  mut SchedToken) -> &'a mut T`. Each object keeps its SCHED-guarded state in one cell and its
  accessors become safe `&self` methods taking the token. Thread splits into immutable identity, a
  `SchedCell<ThreadSched>`, its atomics (`on_cpu`, `wait_phase`), and the asm-written `arch`/`fpu`
  fields behind their own `UnsafeCell`, so a `&Thread` is always sound. Checked against `fire_timer`,
  `signal_pending_op_with_result`, wait()'s registration, `Timer::add_waiter` and
  `IpcChannel::send_push` (the peer dereference stays one narrowed unsafe): in each, every cell
  borrow ends before the next `&mut SchedState` call. In `kernel/src/libkern` (`SchedCell`) and
  `kernel/src/sched.rs` (`SchedToken`). Closes: an accessor called without SCHED; a `&mut Inner`
  outliving its hold or aliased; and the `&Thread`-over-a-concurrently-written-`arch` race of
  sched-d10; it removes most of sched's 162 unsafe blocks of this kind. Depends on: nothing for the
  lock half; sched-o2 for the liveness half. abi, ownership, soundness, concurrency, opportunities.
  Value high, effort large. **Plan**, as kobj-o3, in P1.

- [ ] **sched-o2** encapsulate: scheduler entry points take a type-erased `*mut ()` (and
  `wait_on`/`wait_on_io` take `&[usize]`) whose kind and liveness the caller promises, though nearly
  every caller already holds a type-checked ObjectRef and passes `.as_ptr()` — about 98 sites (29
  `pub unsafe fn` by `grep -rhoE "pub unsafe fn [a-z_]+" kernel/src/sched.rs kernel/src/sched/`; 26
  entry points once the 3 Drop hooks are excluded; external call sites by `grep -rnE
  "sched::(<those names>)\(" kernel/src | grep -v ^kernel/src/sched`: 92-98, of which 72-77 pass
  `.as_ptr()`, 30 `complete_pending_op`, 10-12 `pending_op_completion`, 11 `wait_on`/`wait_on_io`;
  plus 19 `&*(x as *const Thread|Process)` casts inside sched), such as
  `kernel/src/syscall/table.rs:1095` (`timer_arm(ok.object.as_ptr(), ..)` after `lookup_typed`),
  `kernel/src/object/file_object.rs:1180`, `kernel/src/sched/objects.rs:93` (`request_terminate`),
  `kernel/src/sched/current.rs:31`. A `KObject { const TYPE }` trait and `KRef<T: KObject>(ObjectRef,
  PhantomData<T>)` made only by `ObjectRef::downcast::<T>() -> Result<KRef<T>, ObjectRef>`
  (or a `Typed<'r, T>` borrowed from `&'r ObjectRef`), with `Deref<Target = T>`, and a `Waitable<'a>`
  for the five waitable kinds. The entry points become safe: `complete_pending_op(po: &KRef<PendingOperation>,
  ..)`, `timer_arm(t: &KRef<Timer>, ..)`, `wait_on(objs: &[&ObjectRef], ..)`, and the borrow is the
  pin. Checked against `sys_timer_set` (`lookup_typed`), `sys_wait` (`refs[]`, already type-checked),
  `file_object.rs:1180` (`po: &ObjectRef`), `xhci::settle` and `drivers/input` `ParkedRead.po`. About
  12 callers that hold only a raw pointer pinned by an IRP or driver struct (`io/block.rs:382`,
  `ahci.rs:676/695/814`, `xhci/mod.rs:704/753`, `ps2/mod.rs:266`, `xhci/storage.rs:775`,
  `io/mod.rs:29`, `syscall/table.rs:1414`, `file_object.rs:842`) keep an `unsafe fn ..._raw` or store
  a `KRef` in the IRP. In `kernel/src/object/header.rs` (beside ObjectRef), used by `kernel/src/sched`
  and `kernel/src/syscall/table.rs`. Closes: a wrong-kind or dangling object pointer reaching a
  SCHED-held accessor, the class behind the stale-`us_reg` crash and the `is_live`/magic backstops.
  Depends on: nothing. abi, ownership, soundness, concurrency, opportunities. Value high, effort
  large. **Plan**, as kobj-o7, in P1.

- [ ] **sched-o3** encapsulate: per-CPU statics indexed by `this_cpu()` whose correctness rests on
  a comment that the caller cannot migrate between reading the index and using the slot — 28 sites
  (`grep -rn "with_preempt_off(\|with_resched_pending(\|with_ending(" kernel/src | grep -v "fn
  with_\|tests"` gives 12; `grep -n "\[cpu\]" kernel/src/libkern/lockrank.rs` gives 16), such as
  `kernel/src/sched.rs:414`, `kernel/src/sched/preempt.rs:119`, `kernel/src/sched/exit.rs:90`,
  `kernel/src/libkern/lockrank.rs:271`. `CpuLocal<T>` with `fn get(&self, _: &impl NoMigrate) ->
  &T`, where `NoMigrate` is implemented by `IrqSpinLockGuard`, an `IrqsOff` RAII and a `PreemptOff`
  RAII, and `fn remote(&self, _: &SchedToken, cpu) -> &T` for `end_siblings`' cross-CPU store. The
  `cfg(test)` thread-local backing lives inside the type once. Checked against `on_timer_tick`
  (guard), `preempt_disable` (mask) and `end_if_ending` (IF=0 at the return to ring 3). In
  `kernel/src/libkern`, used by sched and lockrank. Closes: touching another CPU's slot after a
  migration between `this_cpu()` and the access, the SMP wrong-slot class seen before. Depends on:
  nothing (the remote accessor wants sched-o1's token). concurrency. Value medium, effort medium.
  **Plan**, as kplat-o3, in P4.

- [x] **sched-o4** narrow: `dpc::enqueue(&Dpc)` is safe for any lifetime, though statics need
  nothing more and heap-embedded IRP DPCs are sound only by the convention that the box outlives its
  drain — 22 sites (`grep -rn 'dpc::enqueue(' kernel/src | grep -v tests | grep -v ^kernel/src/dpc.rs`:
  12 static, 9 IRP-embedded at `ahci.rs:572,634`, `xhci/storage.rs:751`, `io/block.rs:119,515,540,896`,
  `io/ramdisk.rs:239`, `object/file_object.rs:1255`, and 1 stack-local at `io/mod.rs:141`), such as
  `kernel/src/drivers/ahci.rs:669` (`&AHCI_INTR_DPC`), `kernel/src/io/block.rs:515`
  (`&(*irp).dpc`), `kernel/src/io/mod.rs:141`, `kernel/src/drivers/ps2/mod.rs:317`. `pub fn
  enqueue(dpc: &'static Dpc)` for the statics and `pub unsafe fn enqueue_raw(dpc: NonNull<Dpc>)`
  with the contract "the Dpc stays valid until its handler has run" for the IRP and stub-fill ones;
  the self-test's stack Dpc becomes a static. Checked against `AHCI_INTR_DPC`/`PIT_DPC`
  (static, fits), the `io/block.rs` IrpBox (heap, `enqueue_raw`) and the file_object StubFillBox
  (heap, `enqueue_raw`). In `kernel/src/dpc.rs`. Closes: the dangling-Dpc use from safe code
  (sched-d3), and it marks each by-convention site. Depends on: nothing. abi, soundness. Value
  medium, effort small. **Fix.**
  Fixed in `1b7057f2`: as sched-d3.

- [x] **sched-o5** encapsulate: lists pushed under SCHED or at an interrupt tail are plain KVecs
  reserved up front and pushed only with `push_within_capacity`/`push_reserved`, and the five
  waiter pushes each say "the type should say so" — 15-21 sites (`grep -rn "push_within_capacity\|push_reserved("
  kernel/src/sched.rs kernel/src/sched/ kernel/src/object/ kernel/src/dpc.rs | grep -v "fn
  push_reserved\|tests"` gives 21 code lines, 10 `push_reserved` plus 11 within-capacity; 5 by
  `grep -rn "the type should say so" kernel/src`), such as `kernel/src/sched/wait.rs:29`,
  `kernel/src/sched/exit.rs:158`, `kernel/src/object/ipc_channel.rs:542`, `kernel/src/dpc.rs:99`.
  `Reserved<T>(KVec<T>)` built by `with_capacity(n) -> Result<Self, AllocError>` outside the lock,
  exposing `push(&mut self, T) -> Result<(), T>` that never grows, plus pop, remove, iter, len and
  capacity, with no `try_push`, `try_reserve` or `Default` (so no `mem::take`). `SchedState`'s run,
  blocked, suspended and reap lists, deadlines, `deferred_drops`, `ended_pids` and the DPC queue
  all become `Reserved`, whose capacity is a type-level constant so the DPC drain buffer takes `CAP`
  from it. Checked against `SchedState.blocked` (static empty, swapped for a reserved one in `init`),
  the Timer waiter list and the DPC queue. In `kernel/src/libkern` (beside `kvec.rs`). Closes: an
  allocation or free under the rank-1 lock or in IRQ context through a growing push, and a drain
  buffer shorter than its list (sched-d13). Depends on: nothing. ownership, concurrency,
  opportunities. Value medium, effort small. **Fix.**
  Fixed in `18823fdf`: with `978ba2dc` and `aa9a89e2`: the scheduler's lists, the entropy seed
  waiters, an endpoint's held sends and a notification queue are `Reserved`, which cannot grow, the
  waiters and the DPC queue `ArrayVec`s; `Reserved`'s and `ArrayVec`'s tests, the latter under Miri
  with strict provenance.

- [ ] **sched-o6** encapsulate: five waitable objects each keep `waiters: KVec<*mut ()>` reserved to
  `MAX_WAITERS` = 4 and repeat `add_waiter`/`remove_waiter`/`take_waiters(obj, out: &mut [*mut ()])`
  (the out buffer is caller-sized, checked by `debug_assert!(out.len() >= n)` only), and sched
  repeats the take, `mark_signaled`, `try_wake`, `make_runnable` loop five times — 20-25 sites
  (`grep -rnE 'fn (add_waiter|remove_waiter|take_waiters)\b' kernel/src/object` gives 15;
  `grep -n 'take_waiters(' kernel/src/sched.rs kernel/src/sched/*.rs | grep -v tests` gives 5
  wake loops; plus 5 constructor `try_reserve(Self::MAX_WAITERS)?` calls), such as
  `kernel/src/object/timer.rs:153`, `kernel/src/object/pending_op.rs:144`,
  `kernel/src/object/ipc_channel.rs:460`, `kernel/src/sched/objects.rs:108` (the same loop at
  `:126`, `:151`, `:205` and `sched/timers.rs:81`). `WaiterSet<const N: usize> { slots: [*mut (); N],
  len: u8 }`, inline with a const `new`, `add(&mut self, t) -> Result<(), Full>`,
  `remove(&mut self, t)` and `take(&mut self)` returning an owned batch iterated by value; one
  `wake_all(g: &mut SchedState, set, target)` in sched holds the single unsafe block and its
  argument (each waiter is pinned in `blocked` until it unregisters under SCHED). `Thread::wait_snapshot`
  returns `[(usize,bool); MAX_WAIT_HANDLES]` by value. Checked against Timer (KVec reserved 4 →
  array), PendingOperation and IpcChannel `recv_waiters`: all fit. In `kernel/src/object` (new
  `waiters.rs`) and `kernel/src/sched/objects.rs`. Closes: a short out buffer passed to
  `take_waiters` (a panic in IRQ/DPC context under SCHED), growth under SCHED through a waiter push,
  and drift among the five wake loops. Depends on: nothing (composes with sched-o1). abi, soundness,
  opportunities. Value medium, effort small. **Plan**, in P1.

- [ ] **sched-o7** encapsulate: uncounted back-pointers (`IpcChannel.peer`, `IpcChannel.us_reg`) that
  the target's Drop clears under SCHED are followed raw, and `ipc_endpoint_closing` reads a magic
  word through a possibly stale `us_reg` (`is_live`) as a defence — 19 sites
  (`grep -rnoE 'IpcChannel::(peer_of|clear_peer|us_reg_of|set_us_reg)\(|UserspaceServerReg::is_live\(' kernel/src/sched.rs kernel/src/sched/*.rs`
  excluding ipc.rs tests: peer_of 9, clear_peer 1, us_reg_of 5, set_us_reg 2, is_live 2), such as
  `kernel/src/sched/ipc.rs:18` (`closes_a_cycle`), `ipc.rs:247` (`us_forward_reg_for_send`),
  `ipc.rs:291` (`us_reg_detach`), `ipc.rs:504` (`ipc_endpoint_closing`). `SchedWeak<T>(Cell<*const
  T>)` kept in the SCHED-guarded state, with `get<'a>(&self, _: &'a SchedToken) -> Option<&'a T>`
  (sound because the target's Drop must take SCHED to clear it), `set`, `clear_if(me, &mut
  SchedToken)` and an `upgrade` through `try_acquire` for the two sites that need an owned ref. In
  `kernel/src/sched`. Closes: following a back-pointer after its target is freed (a stale-`us_reg`
  crash happened here before); it also makes the `is_live` read of possibly freed memory
  unnecessary, and a single `attach` that refuses when set would hold sched-d7. Depends on: sched-o1
  (the token). opportunities. Value medium, effort medium. **Plan**, as kobj-o10, in P1.

- [ ] **sched-o8** narrow: under SCHED, code clones an owned ObjectRef only to read through it
  (`th.process_ref()`) and argues at each site that dropping the clone "is never the last
  reference"; elsewhere it takes a raw child pointer and argues the owner keeps it alive — 12 sites
  (`grep -n 'process_ref()\|notification_channel_ptr()\|parent_notif_ptr()\|endpoint_ptr(' kernel/src/sched.rs kernel/src/sched/*.rs | grep -v tests`
  gives 13 lines: 5 `process_ref` clones dropped under SCHED, 3 channel-pointer reads and 4
  `endpoint_ptr` reads; `current.rs:86` clones deliberately and is excluded), such as
  `kernel/src/sched/exit.rs:25` (`deliver_child_exited`), `exit.rs:146` (`end_process_if_last`),
  `spawn.rs:266` (`place_spawned`), `ipc.rs:84` (`us_forward_notify`). `Thread::process(&self) ->
  Option<&Process>`, `Process::notification_channel(&self) -> Option<&NotificationChannel>`,
  `Process::parent_notif(&self)` and `UserspaceServerReg::endpoint(&self) -> &IpcChannel`, each
  borrowing the reference the owner already holds. Checked against `deliver_child_exited`,
  `end_process_if_last`, `place_spawned` and `suspend_with_fault`, which use only `&self` Process
  methods; the setters take `&mut self`, so the fields are immutable once shared. In
  `kernel/src/object/{thread,process,userspace_server}.rs`. Closes: an ObjectRef dropped under SCHED
  (the allocator under rank 1) at the clone sites, and a raw child pointer used after its owner is
  gone. Depends on: nothing (made safe internally by sched-o2's `downcast_ref`). opportunities.
  Value medium, effort small. **Plan**, in P1.

- [ ] **sched-o9** encapsulate: ObjectRefs that a SCHED-held path must release are carried out of
  the locked block by hand (`drop(g); drop(x)`, `Err(returned) => returned`, orphans arrays dropped
  after release, `mem::forget` before a panic, a move into `deferred_drops`), each relying on the
  author remembering that a drop under the rank-1 lock reaches the allocator spinlock — 11 sites
  (`grep -n "drop(g);\|Err(returned) => returned\|mem::forget(returned)\|mem::forget(held)\|deferred_drops, po" kernel/src/sched.rs kernel/src/sched/*.rs kernel/src/object/ipc_channel.rs`
  gives 12 hits, less `sched.rs:737`), such as `kernel/src/sched/ipc.rs:151`,
  `kernel/src/sched/ipc.rs:296`, `kernel/src/sched/ipc.rs:548`, `kernel/src/sched/spawn.rs:187`,
  `kernel/src/sched/wait.rs:53`. A `SchedGuard` wrapping `IrqSpinLockGuard<SchedState>` with
  `fn defer(&mut self, r: ObjectRef)` into an inline bag sized for the largest burst
  (`2*US_PENDING_MAX` plus a few); its Drop releases SCHED first, then drops the bag in thread
  context or moves it into `deferred_drops` in interrupt context. Add a debug assertion in
  `ObjectRef::drop` that the lock tracker holds no Sched rank. Checked against
  `us_forward_originate` (`pl.po`), `ipc_endpoint_closing` (up to 16 orphans) and `spawn_inner` (the
  refused ref). In `kernel/src/sched.rs`. Closes: an ObjectRef dropped under the rank-1 lock reaching
  the allocator, the cross-CPU deadlock `make_runnable`'s comment describes, and the reason
  `deferred_drops` exists. Depends on: nothing. ownership. Value medium, effort medium.
  **Plan**, in P1.

- [ ] **sched-o10** encapsulate: the switch-away tail is written out five times — pick next or fall
  back to idle, take current, set both states, take the saved-SP slot, park prev on a list, install
  next, then `unsafe { switch_into(g, slot, prev, next) }`; `switch_into`'s doc says "four parking
  dispositions" and names four callers, and `park_reaper` is a fifth — 5 sites
  (`grep -n 'unsafe { switch_into(' kernel/src/sched.rs kernel/src/sched/*.rs`; each also has one
  `saved_sp_mut_ptr` and two `set_state` unsafe calls), such as `kernel/src/sched.rs:678-712`
  (`switch_to_next`), `kernel/src/sched/wait.rs:10-38` (`block_current_and_switch`),
  `kernel/src/sched/exit.rs:168-198` (`finish_exit`), `exit.rs:526-554` (`park_reaper`; also
  `suspend_with_fault` at `:280-334`). `enum Park { Ready, Blocked, Suspended { frame: usize },
  Exited, Reaper }` and one safe `fn switch_away(g: IrqSpinLockGuard<'_, SchedState>, prev:
  ObjectRef, park: Park)` that derives prev's state from `park`, parks prev, picks next (with the
  idle fallback and a hook for suspend's stranded-fault report) and calls a private `switch_into(g,
  prev, next)`. That `switch_into` computes the outgoing slot from prev itself (today `out_slot` is
  always `saved_sp_mut_ptr(out_obj)`, a redundant parameter the contract must tie to `out_obj`). The
  list bookkeeping can be split out for host tests, as `clear_online_bit` was. In
  `kernel/src/sched.rs`. Closes: a switch path that parks prev on one list with another list's state
  (`park_reaper` sets Blocked but parks in the reaper slot) or passes an outgoing slot belonging to a
  different thread; it leaves a single unsafe switch call. Depends on: nothing. opportunities. Value
  medium, effort small. **Plan**, in P1.

- [ ] **sched-o11** narrow: deadline-heap entries carry their targets as bare `usize` (`target`,
  `channel`, 0 when unused) beside a separate `DeadlineKind`, cast back to `*mut ()` by kind at fire
  time and matched by `(usize, kind)` on removal — 11-12 sites (`grep -rn 'deadline::Entry
  {\|deadline::remove(\|DeadlineKind::\(Thread\|Timer\|PendingSend\) =>' kernel/src/sched.rs kernel/src/sched/*.rs | grep -v tests`
  gives 4 constructions, 4 removes and 3-4 fire arms), such as `kernel/src/sched/wait.rs:173`,
  `kernel/src/sched/ipc.rs:409`, `kernel/src/sched/timers.rs:47-67` (the Thread arm
  `top.target as *mut ()`), `timers.rs:171`. `enum Target { Thread(NonNull<Thread>),
  Timer(NonNull<Timer>), PendingSend { po: NonNull<PendingOperation>, channel: NonNull<IpcChannel>
  } }` held in `Entry`, with `deadline::remove` matching on `Target` equality. Checked: all three
  constructions and the fire match fit directly. Liveness stays a protocol (removal before the
  free). In `kernel/src/sched/deadline.rs`. Closes: a target read as the wrong kind (a Timer address
  read as a thread) and a PendingSend entry with no channel. Depends on: nothing. abi,
  opportunities. Value low, effort small. **Plan**, in P1.

What held up: the `on_cpu` handoff in `switch_into` and `context_switch` (guard raised under SCHED,
cleared only after the `saved_sp` store, spun on before `saved_sp` and the FPU image are read), the
FPU save/restore order, and the push/pop order matching `fabricate_frame`'s seven slots were checked
independently by four lenses and are sound on x86 TSO. Per-CPU reap lists are drained only by the CPU
the thread switched off, so no stack is freed in use. Every ObjectRef clone dropped under SCHED is a
Process clone whose thread still holds a reference; `push_reserved`, `make_runnable` and
`resume_parked` `forget` before they panic; refs are carried out of locked blocks and dropped after
the guard. `wait()` registers and blocks in one SCHED hold, unwinds partial registration, and removes
its deadline entry on every resume; duplicate handles in `sys_wait` balance, and `sys_wait` pins
every object across the wait. Deadline-heap entries all leave before their target (Timer in
`Timer::drop`, PendingSend on promote or close, Thread on resume), and each blocking send's fresh PO
keeps `(target, kind)` unique. `IpcChannel.peer` and `us_reg` never go stale (`peer` is cleared under
SCHED, `try_acquire` refuses zero, attach only sets a registration on its own endpoint), so the
`is_live` check is a backstop. Preemption counters, the latch, `end_if_ending`'s IF=0 read, lock
guard drop order and rank handling, the AP bring-up under one SCHED hold, `Thread::cpu_mask`
(held by the `MAX_CPUS <= 8` const assert), the arch timer's clamps and saturation, `sys_wait`'s
count check and the DPC drain's handler-frees-its-own-Dpc case all hold. The unsafe `Send`/`Sync`
impls on PendingOperation, Timer and NotificationChannel are sound on SMP because every non-test
access is under SCHED, although their comments still say "single-CPU". `arm_kernel_stack_for` and
`fpu_area_ptr` run after SCHED is released, against their "SCHED held" comments, and are sound
because the fields never change after construction. Tests caught the breaks listed above:
Miri found the any-lifetime `enqueue`, the shared-guard race, the `fpu_area_ptr` provenance, the
`&Thread`-versus-`arch.sp` race, the `IpcChannel::drop` retag and the `try_new_pair` invalidation;
host tests found the frame reuse, the unfailed fills, the double attach, the DPC overflow and the
pid wrap; one boot found the ring-3 NMI. Miri (Stacked Borrows) also passed on the
process-ending, periodic-timer, `resume_suspended` and `enqueue_then_drain_runs_handler_once` tests.
No host test reaches `switch_into`, `context_switch`, the five switch-away tails, `thread_enter`,
`end_if_ending` or the arch MMIO/MSR code, and Miri stops on any test that builds an IPC pair until
sched-d12 is fixed.

### kernel-memory — main at 135055a8, 2026-10-26

Lenses: abi, ownership, soundness, concurrency, opportunities; returned nothing: none.
Defects: found 40, refuted by a boot 0, reported 19 (the 40 merge into 19 distinct defects).
Opportunities: 11 (from 26).

The inventory, `cargo xtask unsafe-inventory` over its paths:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `kernel/src/mm` | 2284 | 96 | 20 | 3 | 0 | 0 | 3 | 123 | 0 | 0 | 4 |
| `kernel/src/tlb.rs` | 68 | 5 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/arch/paging.rs` | 54 | 0 | 10 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/arch/user_access.rs` | 5 | 0 | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/arch/x86_64/paging.rs` | 305 | 16 | 15 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| `kernel/src/arch/x86_64/user_access.rs` | 96 | 5 | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| `kernel/src/arch/x86_64/tlb.rs` | 6 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/object/memory_object.rs` | 136 | 4 | 1 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| `kernel/src/object/file_object.rs` | 902 | 30 | 1 | 0 | 0 | 0 | 1 | 15 | 0 | 0 | 0 |
| `total` | 3856 | 157 | 54 | 3 | 0 | 0 | 5 | 139 | 0 | 2 | 4 |

**Defects**

- [x] **kmem-d1** `kernel/src/mm/kvmap.rs:58` — `vmap_alloc_pages` is bounded only by
  `KERNEL_VMAP_END` (16 TiB), but `kvmap::init` templates only the first 512 GiB PDPT, so a
  kernel stack past that boundary is mapped under one root only. Each thread takes 5 vmap pages
  (20 KiB) that are never reclaimed, and `sys_thread_create` needs no authority. After about 26.8M
  creations (refused ones included: `spawn_user_sched` allocates the stack before
  `place_spawned` refuses at MAX_THREADS) the cursor passes `0xFFFF_C080_0000_0000`. The next
  `KernelStack::new(root)` links a fresh PDPT into that process's PML4 only. `switch_into` then
  loads another root while still on that stack (sched.rs:613-618 relies on "every stack is mapped
  in every root"), and the kernel takes #PF, then #DF, then halts.
  Demonstrated: a host test in the kstack tests builds the template as boot does and makes two
  AddressSpaces. A stack inside the first 512 GiB translates in both (control). After
  `vmap_alloc_pages` spends the rest of the window, `KernelStack::new` returns
  `0xffffc08000001000`, translated in its own root and `None` in the other. The crash itself
  needs a boot or 26.8M cycles. `memory-management.md:627` states the limit and no code enforces
  it. Four lenses found it independently. Lenses: ownership, soundness, concurrency,
  opportunities. High. **Fix**, with the bash's kmem-9, moved here from Part H.
  Fixed in `165dae67`: the vmap hands out VA only inside the window `init` links, and a freed
  stack's VA is reused; fails without it:
  `the_last_page_the_vmap_hands_out_is_mapped_under_every_root`,
  `a_vmap_hands_out_va_up_to_its_end_and_no_further`.
- [x] **kmem-d2** `kernel/src/mm/addr_space.rs:617` — `map_object`'s out-of-memory rollback
  (`rollback_object_map`) clears PTEs that are live in a running address space and issues no
  TLB invalidation, and `sys_memory_map`'s error arm (syscall/table.rs:935) issues none either.
  Process P has threads on CPUs A and B. A's `sys_memory_map` installs pages 0..k, then fails
  allocating a page table for page k+1. B touches the range in the window and caches a
  translation. A rolls back with no flush and drops the object. When the last handle closes,
  `MemoryObject::drop` frees frames that B's TLB still maps. An untrusted process can race it,
  though it needs memory exhaustion.
  Unverified: reading the code confirms the OOM arm calls `Paging::unmap_page` with no flush and
  that `flush_tlb_all` runs only on the Ok path. It needs SMP plus a forced page-table OOM
  mid-range. A fix is `shootdown_all` in the rollback before the object is released.
  Lens: concurrency. High. **Fix.**
  Fixed in `88a5edea`: `map_object`'s rollback is shot down before its object comes back;
  fails without it: `a_failed_object_map_is_shot_down_before_its_object_comes_back`.
- [x] **kmem-d3** `kernel/src/mm/slab.rs:334` — `kmalloc` accepts `align == SLAB_SIZE` (it refuses
  only larger). `large_alloc` then puts the object at `align_up(32, 4096) = 4096`, the block's
  second page, but `kfree` and `large_free` find the descriptor at `ptr & SLAB_MASK`, which is
  the object's own first bytes. `KBox::<IpcMsg>::try_new_zeroed()` (libkern's
  `#[repr(C, align(4096))]`) and drop reads `owner` and `obj_size` from message bytes. With zeros
  it frees one frame at phys+4096 in an order-1 block (the other leaks and the coalesce bitmap is
  corrupted). With a nonzero `owner` it dereferences message bytes as a `&SlabCache`. Latent:
  nothing allocates at 4096 alignment today, and one `KBox` or `KVec` of any `align(4096)` type
  would trigger it.
  Demonstrated: slab host probes. `kmalloc(4096, 4096)` gave `p & SLAB_MASK == p` instead of
  `p - 4096`. Dropping a zeroed `KBox<IpcMsg>` changed `frames_freed_by_this_thread` by 1 where 2
  were taken. `large_alloc(64, align)` finds the descriptor for 8/64/1024/2048 and not 4096.
  After `large_free`, `alloc(0)` handed out a page of the live allocation. Lenses: abi, ownership,
  soundness, opportunities. Medium. **Fix.**
  Fixed in `fe8d7697`: a page-aligned object's descriptor is the page below it; fails without
  it: `a_page_aligned_allocation_is_freed_whole`.
- [x] **kmem-d4** `kernel/src/mm/kstack.rs:197` — `KernelStack` stores the `PhysAddr` root it was
  built under and walks it in `Drop`, and `KernelStack::new(root)` is a safe fn that forwards any
  frame to unsafe `map_page`. Nothing ties the root's lifetime to the stack. It holds only
  because `Thread` declares `stack` before `process` and kernel threads are spawned only under
  the boot root. Two ordinary changes break it: reordering `Thread`'s fields, or spawning a
  kernel thread from a syscall. `Drop` then walks a freed, reused PML4, writes through it, and
  leaves the stack's PTEs in the shared vmap while freeing their frames.
  Demonstrated: three host probes in the kstack tests. After `drop(asp)` and reusing the root
  frame, `drop(stack)` leaves `translate(other, base)` as `Some` (the control, in order, gives
  `None`). A planted word in a frame the stack never owned reads 0 after the drop instead of
  `0xDEAD0001`. `KernelStack::new(memory_object.frames()[0])` with no `unsafe` wrote a PDPT link
  into the MemoryObject's data (word 384 became `0x7cae77ff0007`). Lenses: ownership, soundness,
  concurrency, opportunities. Medium. **Fix.**
  Fixed in `165dae67`: a `KernelStack` keeps no root, and the vmap is edited through the boot
  root alone; structural, held by `a_stack_outlives_the_address_space_it_was_made_under`.
- [x] **kmem-d5** `kernel/src/tlb.rs:125` — the shootdown publishes `REQUEST_ALL`, `REQUEST_VA`
  and `ACKED=0` with plain stores, then sends the IPI with a bare x2APIC ICR WRMSR. x2APIC
  WRMSR is not serializing (SDM 3A §11.12.3 asks for `MFENCE;LFENCE` first), `regs::wrmsr` is
  `options(nomem)` so the compiler may move the stores past it, and the `Release` on `ACKED`
  pairs with nothing in `on_ipi`. Two outcomes: (a) a target's `fetch_or` of `ACKED` lands
  before the buffered `store(0)` and the ack is lost. The initiator spins forever holding
  `LOCK` with preemption off, hanging every later unmap and kernel-stack free. (b) a target
  reloads CR3 before the PTE clear is visible, re-caches the old translation and acks, and the
  frames are freed while still mapped. The same `send_ipi` carries the resched IPIs.
  Unverified: no `mfence` or `lfence` appears in `kernel/src` or in the objdump of `shootdown`.
  KVM traps ICR writes (which serializes them) and TCG cannot reorder, so only real SMP hardware
  or a litmus test settles it. The fix is `fence(SeqCst)` plus `lfence` before the ICR write, as
  Linux does with `weak_wrmsr_fence`. Lenses: ownership, soundness, concurrency, opportunities.
  Medium. **Fix.**
  Fixed in `6d8f188c`: the ICR is written only by `write_icr`, after `mfence; lfence`; held by
  `the_icr_is_written_once_after_the_x2apic_fence` (a tools test) and the disassembly's 18 ICR
  writes. Unverified as before: only real SMP hardware shows the race.
- [x] **kmem-d6** `kernel/src/mm/vmm.rs:245` — safe functions cast an `ObjectRef` to a concrete
  type behind only a `debug_assert` on its tag, so in release a wrong-kind reference is read as
  the wrong type. `Vma::drop` calls `writable_unmapped` whenever `mapping == FileBacked` and
  WRITE is set, and `mapping` and `object` are `pub` fields no type ties together.
  `AddressSpace::map_object` (addr_space.rs:316), eight `FileObject` methods (file_object.rs:383-1030)
  and `syscall::write_back` (table.rs:2235) do the same. Today every caller checks `object_type`
  first. A new caller passing an unchecked ref makes it bite: safe code builds a `FileBacked` VMA
  holding a `MemoryObject`, and the drop decrements the low half of its `frames` pointer.
  Demonstrated: a safe-code host test in the vmm tests. Reading the frames afterwards aborts
  with "slice::from_raw_parts requires the pointer to be aligned", and the `MappingKind::Object`
  control passes. Release probe `map_object(range, uprot(), file_object_ref)` panics at
  addr_space.rs:349 with "index out of bounds: the len is 0 but the index is 0" (FileObject
  memory read as `MemoryObject.frames`). Lenses: soundness, opportunities. Medium. **Fix.**
  Fixed in `054c54d2`: the 14 `debug_assert_eq!` type checks before a downcast are `assert_eq!`, and
  `write_back` and `Vma::drop` gain one. Fails without it:
  `a_file_mapped_as_a_memory_object_stops_at_its_type`, in a release build with `map_object`'s check
  debug-only, panics on an index out of bounds rather than the assertion, and
  `a_file_backed_vma_over_another_kind_of_object_stops_at_its_drop` aborts the test process without
  `Vma::drop`'s.
- [x] **kmem-d7** `kernel/src/mm/user_access.rs:177` — safe `copy_from_user<T: Copy>`
  `assume_init`s user bytes as `T`, but `Copy` does not make every bit pattern valid (`bool`,
  enums, `char`, `NonZero`, references), and `copy_to_user<T: Copy>` copies padding out to user
  memory. Latent: the 3 `copy_from_user` and 8 `copy_to_user` callers use integer-only,
  padding-free types. A `bool` in `ThreadArgs` or an enum in `IoOp` is UB, and a padded struct
  leaks kernel stack bytes.
  Demonstrated: Miri (nightly-2026-07-20) on a probe test, `copy_from_user(UserPtr::<bool>)`
  over a byte holding 2, reports UB at user_access.rs:199 `out.assume_init()`. Lens: ownership.
  Medium. **Fix**, as kobj-d6.
  Fixed in `efb315e2`, as kobj-d6.
- [x] **kmem-d8** `kernel/src/sched/ipc.rs:527` — when a userspace server's endpoint closes,
  `ipc_endpoint_closing` (and `us_reg_detach`, line 281) fails every pending lookup but never
  touches `pending_fill`, so a Model B page-cache fill's PO is never completed. `wait_status`
  (file_object.rs:1177) re-waits on anything but Signaled, process exit included, so the
  faulting thread is stuck in the kernel for ever and its process is never reaped.
  `PendingFill` holds the FileObject, whose producer holds the registration, which holds
  `pending_fill`: the cycle leaks them all. Reachable through a trusted server only, and the
  kernel builds `Producer::FsServer` for any non-block resolve reply (table.rs:2398).
  Demonstrated: a host test in the userspace_server tests (an IpcChannel pair, one lookup and
  one fill outstanding, server end dropped). Lookup PO signalled `true`, fill PO signalled
  `false`. Lenses: soundness, opportunities. Medium. **Fix**, as kobj-d3.
  Fixed in `7938344d`, as kobj-d3.
- [x] **kmem-d9** `kernel/src/object/file_object.rs:790` — `mark_ready` (called by the Model B
  fill reply, table.rs:2703) readies whichever page now has the index, while `settle` was made
  to match by PO for this reason. A stale fill reply can mark a successor page Ready while its
  own fill is pending. Page i fills (frame A, PO1), a resize retires it, a regrow and a fault
  reserve i again (frame B, PO2). PO1's reply readies B and a third faulter maps it. PO2 then
  fails and `settle` frees B, which is still mapped in user space: a use-after-free. Latent:
  only Model A objects are resized today.
  Demonstrated: a host probe in the file_object tests ran that sequence. It reported "mapped
  PhysAddr(125966602964992); settle freed 1 frame(s); lookup(2) = None". Lenses: ownership,
  opportunities. Medium. **Fix.**
  Fixed in `4185caef`: the reply settles the page by its fill's PO; fails with the old `mark_ready`:
  `a_stale_fill_reply_leaves_its_indexs_next_page_loading`, through the real reply path.
- [x] **kmem-d10** `kernel/src/drivers/mod.rs:197` — `pub(crate) fn adopt<T>(obj: KBox<T>, ty:
  KObjectType) -> ObjectRef` is safe but wraps the unsafe `ObjectRef::from_raw`, so any box
  under any tag becomes an `ObjectRef` whose last drop runs `dispatch_destroy` for `ty`. The
  private copy at io/mod.rs:161 and test copies (sched/ipc.rs:560, drivers/input/mod.rs:290) are
  the same. A mis-tagged call among the 30-odd sites compiles with no `unsafe`, and the last
  close destroys the allocation as the wrong type.
  Demonstrated: a host probe compiled `adopt(MemoryObject::try_new(PAGE).unwrap(),
  KObjectType::Timer)` with no `unsafe` and printed "adopted as Timer". The drop was not run.
  Lens: opportunities. Medium. **Fix**, as kobj-d8.
  Fixed in `9a28d4b0`, as kobj-d8.
- [ ] **kmem-d11** `kernel/src/mm/heap.rs:45` — `heap::buddy_free`, `BuddyAllocator::free`
  (buddy.rs:182) and `BuddyPager::free` are safe fns that write the free-list link through the
  HHDM and hand the frame out again. Frame ownership is held by convention at 18 hand-written
  free sites. A double free, or a free while a mapping or DMA still uses the frame (the shape of
  the early free fixed in #396), corrupts memory with no `unsafe` at the call site.
  Demonstrated: a buddy host probe on a local allocator freed a live frame with the safe call.
  The owner's first word read 0x0 and the next `alloc` returned the same frame. Lens:
  opportunities. Medium. **Plan**, in P2.
- [ ] **kmem-d12** `kernel/src/mm/addr_space.rs:458` — `map_file_page` re-checks only that some
  FileBacked VMA of the same object covers `addr`, not that `addr` still maps to the page index
  `file_backing` computed in an earlier hold of the AS lock. Thread A faults at 6P in F mapped at
  [4P,8P) (page 2) and parks on the fill. B unmaps and maps F at [5P,9P), where 6P is page 1. A's
  `map_file_page(6P, F, frame2)` returns true and page 2's frame sits where page 1 belongs. There
  is no use-after-free (the frame is F's own), but the process sees and writes back the wrong
  page. The same window also makes a fault on a remapped region fatal where a retry would work.
  Demonstrated: a host test in addr_space/tests.rs ran those steps. `map_file_page` returned
  true and `translate(6P) == frame2` (a second probe asserts the equivalent with frame 1 at 5P).
  Lenses: abi, ownership, concurrency. Low. **Plan**, as the bash's kmem-15, in Part E.
- [x] **kmem-d13** `kernel/src/object/file_object.rs:598` — write-back takes a page's device
  position from its first block and writes `pages * PAGE_SIZE` contiguously from there, and
  `block_map` (table.rs:2622) accepts block sizes under a page without refusing a run boundary
  inside a page. The recorded `TODO(ext4-subpage-runs)` covers the fill from the wrong blocks. It
  does not say that write-back overwrites the blocks after the page's first run, which may
  belong to other files or metadata. For example, an ext4 with 1 KiB blocks and a file whose
  extent ends inside a page: `sys_file_sync` writes 4 KiB from the first block, and 3 KiB land on
  whatever follows on the device.
  Unverified: the path is pointed at. A host test of `write_runs` with runs [file 0..2 -> lba
  100, file 2..4 -> lba 500] at block_size 1024 should issue one 4096-byte IRP at byte 100*1024.
  Lens: abi. Low. **Fix.**
  Fixed in `adb5f75d`: `block_map` refuses a run that does not begin and end on a page boundary;
  fails without it, or with only the start checked:
  `a_block_map_the_page_cache_cannot_use_is_refused`.
- [ ] **kmem-d14** `kernel/src/arch/x86_64/idt.rs:258` — copy-primitive recovery runs before any
  demand fault-in, so the kernel never populates a valid but lazily backed user page (an untouched
  stack page or an untouched FileBacked page). The copy fails with `Fault`, and `sys_channel_recv`
  has already popped the message, so the message is destroyed and its transferred handles are
  closed. The decision log records it as a corollary, but `deferred-decisions.md` has no entry and
  the page cache now exists. No known caller hits it, because today's callers touch pages first.
  Demonstrated by reading the code, not by a boot: `pf_dispatch` returns at
  `lookup_recovery(f.rip)` before `try_fault_in`, and there is no prefault step. A boot test
  receiving into an untouched page of a fresh file mapping would show it. Lens: abi. Low.
  **Plan**, as the bash's kmem-28, in Part E.
- [x] **kmem-d15** `kernel/src/arch/x86_64/idt.rs:258` — recovery matches the faulting RIP alone,
  and `copy_bytes_raw` registers `rep movsb` for both directions, so a fault on the kernel-side
  pointer is also turned into `UserAccessError::Fault` instead of `dump_and_halt`. A dangling
  bounce buffer or a bad length past a kernel buffer is hidden and reported as a user error.
  Unverified: reasoned from code (`lookup_recovery_in` compares only `fault_pc`). Check
  `read_cr2() < USER_VIRT_END` before patching RIP, and boot with a deliberately bad kernel-side
  pointer. Lens: abi. Low. **Fix.**
  Fixed in `593803be`: `lookup_recovery` takes the faulting address too, and recovers only one in
  the user half. Fails without it: a boot probe copying from an unmapped kernel address printed
  "recovered as a user fault: true" and booted on; with this it stops at the `#PF` dump.
  `only_a_fault_on_a_user_address_is_recovered` fails at `USER_VIRT_END` without the check.
- [x] **kmem-d16** `kernel/src/arch/x86_64/cpu.rs:190` — the kernel sets and checks SMAP and SMEP
  but never sets or checks CR0.WP. That `copy_to_user` cannot write a read-only user mapping
  holds only because Limine sets WP on the BSP and the APs. With WP clear, a process holding a
  read-only mapping of a shared page-cache page could have the kernel write into it through
  `sys_clock_read` or `sys_channel_recv` out pointers.
  Unverified: grep finds no `write_cr0` that sets bit 16. Assert `read_cr0() & (1<<16) != 0` on
  every CPU at bring-up. It holds under Limine today. Lens: abi. Low. **Fix.**
  Fixed in `6c8efd4b`: `init_protections` sets CR0.WP on every CPU. Fails without it: with a probe
  clearing WP first, as a bootloader might, every CPU ran with WP 0 and a write to a read-only
  static landed; with this the write faults.
- [x] **kmem-d17** `kernel/src/mm/kvmap.rs:111` — `remap_mmio_page` repoints a page of the shared
  vmap and flushes only the local TLB, and its `# Safety` contract does not say that the page
  must have been used by one CPU only. Safe today because its only caller, `pci::enumerate`
  under `device::init` (main.rs:239), runs before `bring_up_aps` (main.rs:283). A rescan or
  hotplug after SMP start, with the thread migrating, would read the previous function's config
  space through a stale entry. `shootdown_page` has no callers at all.
  Unverified: path pointed at (`unmap_page`, `map_page`, `flush_tlb_page`, no `shootdown_page`).
  Lenses: ownership, soundness, concurrency. Low. **Fix.**
  Fixed in `165dae67`: `MmioWindow::repoint` shoots down the page it changes; fails without it:
  `a_repointed_window_reads_the_new_frame_and_shoots_down_the_old`.
- [ ] **kmem-d18** `kernel/src/object/memory_object.rs:203` — `copy_to_kvec` forms a `&[u8]` over
  each frame under a SAFETY comment saying it is "not aliased", but MemoryObject frames are
  mapped into user spaces by design. `file_object.rs:1043` (`read_to_kvec`) does the same over
  cache frames, and `DmaBuffer::as_slice` and `as_mut_slice` (mm/dma.rs:78, 84) hand out
  references to memory a device writes. A process mapping a MemoryObject writable and writing it
  from one thread while another passes it as a spawn image gives the kernel a shared reference to
  changing bytes, a data race in Rust's model.
  Unverified: Miri cannot tell this from a raw copy. Copy with `ptr::copy` from raw pointers (or
  the user-copy asm), never forming the reference, and correct the SAFETY comments.
  Lenses: soundness, concurrency. Low. **Plan**, as kobj-d13, in P2.
- [x] **kmem-d19** `kernel/src/object/memory_object.rs:106` — `try_new_borrowed`'s `# Safety` does
  not require the range to be HHDM-mapped, but safe accessors (`copy_in` at line 174,
  `copy_to_kvec` at 192) and the I/O paths dereference every frame through the HHDM, and
  `sys_process_spawn` (table.rs:360) and `sys_io_submit` (table.rs:1646) accept a Borrowed
  object. Today the only borrowed range is the framebuffer, which Limine maps in the HHDM. A
  second aperture (a PCI BAR) would let a holder aim device DMA at registers or make `copy_in`
  take a kernel #PF.
  Unverified: grep finds no `FrameOwnership` check outside memory_object.rs. A host test handing
  a Borrowed object to `sys_io_submit`'s validation, or refusing Borrowed buffers there, would
  settle it. Lenses: soundness, opportunities. Low. **Fix.**
  Fixed in `a36786e9`, its code moved first in `f74d100c` and `ac6255a9`:
  `MemoryObject::kernel_reachable`, and a spawn's image and an I/O buffer refuse a borrowed object
  with `InvalidArgument`, a fill's reply with `Unsupported`. Fails without it:
  `a_borrowed_memory_object_is_refused_where_the_kernel_reaches_its_frames` crashes the test process
  (SIGSEGV) without the image's or the fill's check, and fails without the buffer's.

**Opportunities**

- [ ] **kmem-o1** encapsulate: a type-erased `ObjectRef` is downcast by hand with `unsafe {
  &*(r.as_ptr() as *const T) }` behind a `debug_assert` or an earlier match, fresh boxes are
  adopted with `from_raw` or the safe `adopt<T>` helpers under any tag, and
  `FileObject::writable_unmapped` takes `*mut ()` — 128 sites (103 production downcasts, 32 of
  them MemoryObject/FileObject, plus 23 `ObjectRef::from_raw(KBox::into_raw` adoptions and 2 safe
  `adopt<T>` helpers; counted with `&\*\(\s*[a-z_.]+(\.as_ptr\(\))?\s+as\s+\*(const|mut)
  ...\)` over `kernel/src`, skipping test modules; 14 of file_object.rs's 30 unsafe blocks), such
  as `kernel/src/mm/addr_space.rs:316`, `kernel/src/object/file_object.rs:386`,
  `kernel/src/mm/vmm.rs:245`, `kernel/src/drivers/mod.rs:197`. `unsafe trait KObject { const
  TYPE: KObjectType; }` (macro-implemented, with `assert!(offset_of!(T, header) == 0)`),
  `ObjectRef::adopt<T: KObject>(KBox<T>)` (the tag comes from `T`), `ObjectRef::downcast<T:
  KObject>(&self) -> Option<&T>` and an owning `TypedRef<T>`. `Vma`'s `mapping` and `object`
  collapse into a private `enum Backing { Anonymous, Object(TypedRef<MemoryObject>),
  File(TypedRef<FileObject>) }`, and `writable_unmapped` takes `&FileObject`. Checked against
  `map_object`, `writable_mapped`, `Vma::drop` and table.rs:892's size match. It can be staged one
  object type at a time. In `kernel/src/object/header.rs` (Backing in `kernel/src/mm/vmm.rs`).
  Closes: type confusion between kernel objects (kmem-d6) and mis-tagged adoption (kmem-d10).
  Also no `offset_of!(T, header) == 0` assert exists for any type, and `ObjectRef`'s unchecked
  `unsafe impl Send/Sync` claims every object is Send + Sync while `Process` is not (VmaTree's
  `NonNull` links), so the supertrait would force an explicit, argued impl. Depends on: nothing.
  Lenses: abi, ownership, soundness, concurrency, opportunities. Value high, effort medium.
  **Plan**, as kobj-o1, in P1.
- [ ] **kmem-o2** encapsulate: physical frames are bare `Copy` `PhysAddr` values from
  `heap::buddy_alloc` and returned by hand with the safe `buddy_free(addr, order)`; the buddy
  only `debug_assert`s a free, and 8 alloc sites carry their own unsafe zeroing block — 28 to 32
  sites (18 or 19 production `buddy_free(` plus 10 to 13 `buddy_alloc(`; counted with `grep -rn
  'buddy_free(' / 'buddy_alloc('` over `kernel/src`, excluding tests and the fn definitions),
  such as `kernel/src/mm/addr_space.rs:147`, `kernel/src/object/memory_object.rs:225`,
  `kernel/src/object/file_object.rs:781`, `kernel/src/mm/kstack.rs:204`. A `!Copy` `Frame` for
  order 0 (and `FrameBlock { phys, order }` for order n) that frees with its own order on `Drop`,
  with `alloc_zeroed()` as the only zeroing unsafe, `phys()`, `into_pte(self)` to hand a frame to
  a page table and `unsafe from_pte(PhysAddr)` to take it back at unmap. `buddy_free` becomes
  unsafe and crate-private. `MemoryObject.frames` becomes `KVec<Frame>` for Owned and a separate
  Borrowed variant. `KernelStack` holds `[Frame; 4]` (fields drop after `Drop`'s shootdown, the
  required order). `CachePage.frame` and `retired` become `Frame`, so `settle(false)` is
  `drop(page)`. `DmaBuffer` holds one `FrameBlock`. Checked against `MemoryObject`,
  `KernelStack`, `FileObject` and `DmaBuffer`. In `kernel/src/mm/heap.rs` (or a new
  `mm/frame.rs`). Closes: frame double free, leak and wrong-order free (kmem-d11, the d3
  large_free misroute, the Borrowed/Owned flag kept by hand, hand-written rollback loops). It
  also lets host tests carry provenance so Miri can check frame accesses. Depends on: nothing;
  pairs with kmem-o6. Lenses: ownership, soundness, opportunities. Value high, effort medium
  (one lens said large). **Plan**, in P2.
- [x] **kmem-o3** encapsulate: kernel-half (vmap) mappings are made through
  `Paging::map_page`/`unmap_page` with a caller-chosen root (`active_root()`, a process root, or
  the root stored in `KernelStack`) and a local-only TLB flush, although the tables are shared by
  every address space, and nothing enforces the 512 GiB pre-allocated window — 9 sites (`grep -rnE
  "Paging::(map_page|unmap_page)\(" kernel/src --include=*.rs`, excluding tests: kstack.rs:70,78,197;
  kvmap.rs:84,105,110; ioapic.rs:185; main.rs:775,793), such as
  `kernel/src/mm/kstack.rs:70`, `kernel/src/mm/kstack.rs:197`, `kernel/src/mm/kvmap.rs:84`,
  `kernel/src/arch/x86_64/ioapic.rs:185`. kvmap owns the window and takes no root:
  `map(frames: &[(PhysAddr, PageFlags)], guard_pages) -> Result<VmapRange, AllocError>` edits the
  template's shared tables (or walks the immortal boot root captured with the template, via a
  `KernelHalf(PhysAddr)` token), `VmapRange::unmap(self)` runs `shootdown_all` before returning
  the frames, a reusable window type has `repoint(&mut self, phys)` that runs `shootdown_page`,
  and `vmap_alloc_pages` refuses VA beyond the PML4 slots pre-allocated at boot. `KernelStack`
  drops its `root` field. Checked against `KernelStack::new`/`drop`, pci's `remap_mmio_page`
  window and ioapic's one-frame map. In `kernel/src/mm/kvmap.rs` (boot root recorded beside
  `KERNEL_TEMPLATE` in `arch/x86_64/paging.rs`). Closes: a kernel stack mapped in one address space
  only (kmem-d1), `KernelStack` walking a freed PML4 (kmem-d4), stale cross-CPU translations
  after a shared remap (kmem-d17). Depends on: nothing; pairs with kmem-o2. Lenses: ownership,
  soundness, concurrency. Value high, effort small to medium. **Fix.**
  Finished in `165dae67`: kvmap owns the window; `VmapFrames`, `MmioWindow`, `map_device`.
- [x] **kmem-o4** encapsulate: each page-table unmap returns its old frame, and the caller is
  trusted to invalidate other CPUs' TLBs before the frame or its owning object can be freed;
  `rollback_object_map` does not — 8 sites (`grep -rn "Paging::unmap_page(" kernel/src
  --include=*.rs`, excluding tests, the fn definition and arch/x86_64/paging.rs), such as
  `kernel/src/mm/addr_space.rs:540`, `kernel/src/mm/addr_space.rs:623`,
  `kernel/src/mm/kstack.rs:197`, `kernel/src/mm/kvmap.rs:105`. A TLB-gather type `struct
  Unmapped<'a> { frames: KVec<PhysAddr>, objects: KVec<KBox<Vma>> }` returned by every unmap
  helper. It is released only by `finish(self)`, which calls `tlb::shootdown_all()` first, or by
  `finish_unloaded(self, _: NeverLoaded)` for an address space no CPU has loaded (ELF load,
  `AddressSpace::drop`). Checked against `sys_memory_unmap`, `KernelStack::drop` and
  `rollback_object_map` (which would have to finish before returning the object). In
  `kernel/src/mm/addr_space.rs`, with `tlb.rs`. Closes: a frame, or the object owning it, freed
  while another CPU may still cache its translation (kmem-d2). Depends on: nothing. Lens:
  concurrency. Value high, effort medium. **Fix.**
  Finished in `88a5edea`: an unmap returns an `Unmapped`, shot down before anything it held is
  freed, and `map_vma` takes `&mut self`.
- [ ] **kmem-o5** encapsulate: frame contents are reached by hand-computing `phys +
  hhdm_offset()` at each site, then a raw write, a copy, or a `&[u8]` slice, with each site
  re-arguing its bounds and liveness — 44 HHDM-arithmetic sites (`grep -rn "hhdm_offset()"
  kernel/src --include=*.rs | grep -v "tests.rs\|/tests/\|mm/heap.rs\|mm/test_support"`; other
  lenses counted 20 to 46 with narrower filters), 14 of them content accesses to object frames,
  such as `kernel/src/object/memory_object.rs:203`, `kernel/src/object/file_object.rs:1043`,
  `kernel/src/syscall/table.rs:2732`, `kernel/src/mm/elf.rs:306`. A bounds-checked
  `FrameView`/`FrameRef` reached only through an owner's borrow (or a `Frame`), with `zero`,
  `fill`, `copy_in(off, &[u8])` and `copy_out(off, &mut [u8])`, each asserting `off + len <=
  PAGE_SIZE` and copying with `ptr::copy_nonoverlapping`. It never forms a `&[u8]` over memory
  another CPU or a device may write; `MemoryObject::{read_at, write_at}` sit on top. Checked
  against `copy_to_kvec`, `read_to_kvec`, `copy_fill_into_frame`, `zero_tail`, and `reserve`'s
  zeroing. A softer form is `unsafe fn hhdm::page(phys) -> &mut [u8; PAGE_SIZE]` so slice
  indexing panics instead of overrunning. It would also be the place that refuses Borrowed
  aperture frames. In `kernel/src/mm/frame.rs` (beside `heap::hhdm_offset`). Closes: off-page
  offset or length at one of the sites, and shared references over user-shared frames
  (kmem-d18, kmem-d19). Depends on: nothing (usable over `PhysAddr`; the `Frame` type, kmem-o2,
  for owned frames). Lenses: abi, ownership, concurrency, opportunities. Value medium, effort
  medium. **Plan**, as kobj-o11, in P2.
- [ ] **kmem-o6** narrow: every `ArchPaging` operation takes `root: PhysAddr` under the unsafe
  contract "a valid top-level table owned by the caller" (10 unsafe trait fns, 15 more in the
  x86_64 impl), and safe wrappers such as `KernelStack::new(root)` forward a caller's value —
  29 production call sites (`prodgrep` over `Paging::(map_page|unmap_page|translate|
  attribute_index|inherit_kernel_mappings|ensure_kernel_intermediate|init_kernel_template|
  set_page_table)\(`; 17 in this subsystem's files: addr_space 9, kstack 3, kvmap 4, elf 1), such
  as `kernel/src/mm/addr_space.rs:163`, `kernel/src/mm/kstack.rs:70`, `kernel/src/mm/elf.rs:300`,
  `kernel/src/mm/kvmap.rs:84`. `PageTable { root }`: owning and `!Copy`, created by
  `new_user()` (zero plus inherit the kernel half), with `fn kernel() -> &'static PageTable` naming
  the template root, safe `translate` and user-half `unmap`, and `map` keeping only the
  phys/aliasing half of the contract (or taking a `Frame` and being safe). `AddressSpace` holds a
  `PageTable` in its lock. Checked against addr_space.rs:163, kstack.rs:70/197 and elf.rs:300.
  In `kernel/src/arch/paging.rs` (trait) and `kernel/src/mm/addr_space.rs` (owner). Closes: a
  safe fn passing an arbitrary root to `map_page` (kmem-d4), and `load_elf` translating a live AS
  without its lock. It also gives the 512 GiB vmap bound one place to live. Depends on: nothing;
  best with kmem-o2. Lens: opportunities. Value medium, effort medium. **Plan**, in P3.
- [ ] **kmem-o7** remove: `VmaTree` is an intrusive red-black tree over raw `NonNull<Vma>` links
  and holds 102 of the subsystem's 139 raw derefs, plus 23 unsafe blocks and 4 unsafe fns; its
  interval augmentation `subtree_max_end` is maintained by `recompute_max_end`,
  `update_max_end_up_from` and both rotations, but no query reads it — 23 sites (`cargo xtask
  unsafe-inventory kernel/src/mm/vmm.rs`; `grep -n subtree_max_end kernel/src/mm/vmm.rs` shows no
  read outside maintenance and tests), such as `kernel/src/mm/vmm.rs:334`,
  `kernel/src/mm/vmm.rs:483`, `kernel/src/mm/vmm.rs:594`, `kernel/src/mm/vmm.rs:697`. Step 1:
  delete the unread augmentation. Step 2: replace the tree with a sorted `KVec<KBox<Vma>>` keyed
  on `range.start` (binary-search lookups, neighbour overlap check, O(n) insert and remove, which
  needs `KVec::try_insert` reserved before the AS lock as unmap's frames vector already is).
  Checked against `insert`, `find_covering` and `iter_overlapping`. VMA counts are tens per
  process. In `kernel/src/mm/vmm.rs` (and `kernel/src/libkern/kvec.rs`). Closes: raw-pointer link
  corruption in the tree; it removes nearly all of the subsystem's raw-deref column. Medium only
  because the tree already passes Miri and the decision log chose it deliberately. Depends on:
  nothing for step 1; revisiting the decision-log VmaTree entry for step 2. Lens: opportunities.
  Value medium, effort medium. **Plan**, in P3,
  for its first step, deleting the augmentation; **later** for its second, replacing the tree: the
  maintainer's call, a balanced tree rather than a vector when it is revisited.
- [x] **kmem-o8** narrow: `copy_from_user`/`copy_to_user` are generic over `T: Copy`, and ABI
  types reinterpret themselves as bytes in hand-written unsafe blocks, each with its own padding
  claim — 21 sites (11 typed copies: `grep -rn 'copy_to_user(\|copy_from_user(' kernel/src
  --include=*.rs | grep -v 'mm/user_access.rs\|copy_slice\|copy_cstr'`, 3 from and 8 to; plus 10
  byte-view blocks: libkern/notification.rs:189, object/ipc_channel.rs:66 and :74,
  libkern/device.rs:246/255/360/369, libkern/block.rs:138, libkern/framebuffer.rs:75,
  syscall/table.rs:1829), such as `kernel/src/syscall/table.rs:342`,
  `kernel/src/syscall/table.rs:1829`, `kernel/src/object/ipc_channel.rs:74`,
  `kernel/src/libkern/notification.rs:189`. `pub unsafe trait UserPod: Copy + 'static`
  (repr(C), no padding, every bit pattern valid), implemented through a macro that const-asserts
  `size_of` equals the sum of the field sizes. Then `copy_from_user<T: UserPod>`,
  `copy_to_user<T: UserPod>`, slice variants and one `bytes_of`/`bytes_of_mut`/`slice_as_bytes`,
  which removes the 10 byte-view blocks. Checked against `SpawnArgs`, `ThreadArgs` (its `i8`
  included), `IoOp`, `RegisterValues`, `HandleInfo`, `NsEntry`, `StoredMsg::as_bytes_mut` and
  `sys_wait`'s `[IoResult]` view; all integer-only and padding-free. In
  `kernel/src/mm/user_access.rs` (trait; impls beside the ABI types in `kernel/src/libkern`).
  Closes: invalid-value UB from user bytes (kmem-d7) and padding leaks to user memory. Depends on:
  nothing. Lenses: abi, ownership, soundness, opportunities. Value medium, effort small.
  **Fix**, as kobj-o6.
  Fixed in `efb315e2` and `d4b1fa5e`, as kobj-o6.
- [ ] **kmem-o9** encapsulate: drivers reach `DmaBuffer` memory through the raw `virt()` pointer,
  with their own offset arithmetic, volatile accesses and slice building, and sometimes store the
  address as a `u64` in their own structs; `as_slice`/`as_mut_slice` hand out references over
  memory a device may be writing — 19 sites (`grep -rn "\.virt()" kernel/src --include=*.rs |
  grep -v ^kernel/src/mm/dma.rs` = 20, less boot_selftest.rs:539), such as
  `kernel/src/drivers/xhci/mod.rs:229`, `kernel/src/drivers/xhci/hub.rs:905`,
  `kernel/src/drivers/ahci.rs:600`, `kernel/src/drivers/xhci/hid.rs:446`.
  `DmaBuffer::read::<T: UserPod>(&self, off) -> T` and `write::<T: UserPod>(&self, off, v: T)`
  (volatile, `off + size_of::<T>() <= len()` checked) plus `copy_in`/`copy_out(off, &[u8])`;
  `virt()` becomes crate-private and `as_slice`/`as_mut_slice` are retired. Checked against the
  xHCI ring's `u32` read/write at an index (mod.rs:229/239), the DCBAA slot write (hub.rs:905) and
  the AHCI command-list read (ahci.rs:600). In `kernel/src/mm/dma.rs`. Closes: a ring or context
  offset past the buffer, references over device-written bytes, and a raw address stored in a
  driver struct (hid.rs:446, storage.rs:861) that outlives its `DmaBuffer`. Depends on: kmem-o8's
  marker trait. Lens: abi. Value medium, effort medium. **Plan**, as drv-o3, in P5.
- [ ] **kmem-o10** narrow: `AddressSpace` repeats `unsafe { Paging::map_page/unmap_page(root, ..)
  }` with the same SAFETY argument ("root is this AS's PML4, lock held, the frame is fresh or the
  object's"), though holding `&mut Inner` already proves the lock and the root — 8 sites (`grep -n
  "unsafe { Paging::\(map_page\|unmap_page\)" kernel/src/mm/addr_space.rs`: lines 163, 264, 349,
  467, 540, 586, 608, 623), such as `kernel/src/mm/addr_space.rs:163`,
  `kernel/src/mm/addr_space.rs:264`, `kernel/src/mm/addr_space.rs:540`,
  `kernel/src/mm/addr_space.rs:586`. Safe methods on the locked `Inner`: `map(&mut self, va,
  frame, flags) -> Result<(), MapError>` and `unmap(&mut self, va) -> Option<PhysAddr>` (or a
  `UserTables<'g>` borrowed from `&mut Inner` whose `unmap` returns `Unmapped`), keeping the arch
  `unsafe` inside them. Checked against `map_vma`'s install loop, `fault_in`'s anonymous arm and
  `unmap_covering_deferred`. In `kernel/src/mm/addr_space.rs`. Closes: a future AS path editing
  tables without the lock or through the wrong root (none seen today; it makes the rank-4 lock
  claim structural). Depends on: kmem-o2 and kmem-o4. Lenses: soundness, concurrency. Value
  medium, effort small. **Plan**, in P3.
- [x] **kmem-o11** narrow: x86_64 page-table walks dereference `*table_ptr(x).add(index)` raw at
  every level under a per-function SAFETY argument; `translate` and `attribute_index` duplicate
  the same four-level walk, and shared kernel-half slots are read with plain loads but linked
  with an atomic CAS (`ensure_table`) — 22 sites (`prodgrep '\*table_ptr\(|\*pml4\.add\(|\*leaf\b|
  \(\*leaf\)|pt\.add\(|table\.add\(' kernel/src/arch/x86_64/paging.rs`, in 7 functions), such as
  `kernel/src/arch/x86_64/paging.rs:174`, `kernel/src/arch/x86_64/paging.rs:281`,
  `kernel/src/arch/x86_64/paging.rs:374`, `kernel/src/arch/x86_64/paging.rs:482`.
  `TableRef<'a>(&'a [AtomicU64; 512])` with one `unsafe fn at(phys)` and safe `entry`, `child` and
  `set`, plus a single `walk(root, v) -> Option<(Pte, Level)>` serving both `translate` and
  `attribute_index`. In `kernel/src/arch/x86_64/paging.rs`. Closes: per-level SAFETY arguments
  repeated in 7 functions, the duplicate walk, and mixed atomic and plain access to the lock-free
  kernel-half tables. Depends on: nothing (kmem-o6 for the trait-level contract). Lens:
  opportunities. Value low, effort small. **Fix.**
  Fixed in `991a8726`: a `TableRef` (an `[AtomicU64; 512]` and its level, one `unsafe fn at`) and
  one walk, reading each slot once, serve `translate` and `attribute_index`;
  `a_walk_ends_at_a_huge_page_and_a_map_inside_one_is_refused`.

What held up: the two asm copy primitives (register constraints, `cld`, `clac` on every exit, only
the user-side instruction in the extable, the `max==0` guard, the linker KEEP). `UserPtr`
validation and `validate_user_range`'s checked arithmetic, and the typed copies' all-integer,
padding-free types with their const asserts. `sys_memory_map`'s rights, size, hint, stack-guard
and canonical-range checks, and `sys_memory_unmap`'s order (PTEs under the AS lock, then
`shootdown_all`, then free, then the VMA drop). `AddressSpace::drop` without a shootdown (the
next root is loaded before any reap, no PCID, no GLOBAL user PTEs). `VmaTree` (it passes Miri on
the two torture tests, with only HHDM int-to-pointer warnings), the buddy coalesce bitmap and
`order > MAX_ORDER` refusal, slab geometry for `align <= 2048`, and `KBox`/`KVec` with ZSTs. The
ELF loader's checked offsets and its copy of the image into kernel memory (no TOCTOU).
`ensure_table`'s CAS link with the loser freeing its table (host-tested). `tlb::wait_satisfied`
with the departed-CPU rule, and user copies running IF-masked. Page-cache frame lifetimes:
`wait_status` is a real barrier, `settle` matches by PO, `resize` retires frames until `Drop`, and
a Model B reply takes the pending fill exactly once. `map_file`'s writable count on every refusal
path, `MemoryObject::try_new` rollback, `DmaBuffer`'s order handling and the zeroing of every
frame handed to user space. The breaks tests caught are those marked Demonstrated above.

### drivers — main at 135055a8, 2026-10-26

Lenses: abi, ownership, soundness, concurrency, opportunities; returned nothing: none.
Defects: found 38, refuted by a boot 0, reported 21 (duplicates merged). Opportunities: 18.

The inventory, `cargo xtask unsafe-inventory kernel/src/drivers kernel/src/pci kernel/src/io kernel/src/device.rs kernel/src/arch/x86_64/ps2.rs`:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `kernel/src/drivers` | 7393 | 140 | 6 | 4 | 0 | 0 | 3 | 51 | 0 | 0 | 24 |
| `kernel/src/pci` | 448 | 5 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 |
| `kernel/src/io` | 911 | 57 | 3 | 7 | 0 | 0 | 1 | 41 | 0 | 0 | 0 |
| `kernel/src/device.rs` | 638 | 5 | 0 | 0 | 0 | 0 | 0 | 4 | 0 | 0 | 0 |
| `kernel/src/arch/x86_64/ps2.rs` | 185 | 14 | 3 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `total` | 9575 | 221 | 13 | 11 | 0 | 0 | 4 | 96 | 0 | 0 | 26 |

**Defects**

- [x] **drv-d1** `kernel/src/drivers/ahci.rs:590` — `on_interrupt` reads PxCI at line 661 outside
  the port lock, and `take_retired` then swaps out whatever IRP is in flight without re-reading
  PxCI under the lock. An interrupt that saw slot 0 idle therefore completes a command another CPU
  issued in between. Scenario: slot 0 is idle and an interrupt arrives (an unsolicited D2H Register
  FIS, a shared INTx line, or the COMRESET FIS after `recover()`). The ISR reads PxCI=0 and waits
  on the port lock while a submit on another CPU, or `drain_queue`, issues a command. The ISR then
  takes that IRP and completes it IoError with 0 bytes (a FLUSH completes Success), while the HBA
  is still running it. The IrpBox is reclaimed, so the buffer frames can be freed or reused under
  live DMA, and `drain_queue` rebuilds slot 0's command table under the running command.
  Demonstrated: host tests in `ahci/tests.rs` on `fake_disk` (since removed) read PxCI (idle),
  submitted a read, then called `take_retired`. It returned the read with PxCI=1, and after `retire`
  and `run_pending` the status was -40 (IoError) with inflight null. The FLUSH variant gave Success
  with PxCI=1. A second probe, after `on_interrupt` with PXIE_TFEE and `claim` + `recover`, gave the
  same result. Control: adding `|| read32(d.port_base, PX_CI) & 1 != 0` to `take_retired`'s early
  return made both pass, with all 15 and 16 existing AHCI tests still green. Fix: read PxCI inside
  `take_retired`, under `pending`. Lenses: concurrency, ownership, opportunities. High.
  **Fix**, first.
  Fixed in `4795f961`: `take_retired` reads PxCI under the port lock; fails without it:
  `a_command_issued_after_the_slot_read_idle_is_not_retired`.

- [x] **drv-d2** `kernel/src/dpc.rs:98` — the DPC queue reserves 64 entries on the claim that "the
  bound is the number of distinct Dpc objects" (dpc.rs:23-25). But every IrpBox has its own inline
  Dpc (irp.rs:291), so the number of pending DPCs is the number of IRPs completed between two
  interrupt tails. Scenario: a process holding a block DeviceNode with READ loops
  `sys_io_submit(Read, offset = device size, length = 512)`. `submit_within` (io/block.rs:116-120)
  refuses each one by enqueueing that IRP's own DPC and raises no interrupt. Only a 10 ms timer tick
  or a device IRQ drains the queue, so the 65th enqueue hits the `debug_assert` (dev profile, live
  in every image) or the explicit `panic!` at line 101. The same follows from a `Rescan` or `Eject`
  loop on a partition (window_refusal, block.rs:515/540), a retired window, an ejected USB unit, a
  dead AHCI disk, and AHCI's or USB storage's full 32-entry queue (ahci.rs:634, storage.rs:751). On
  an HDD, about 97 outstanding writes are enough. Demonstrated: host tests in `io/block.rs`, run
  four times by four lenses and since removed: `dpc::init()`, then 65 `dispatch_block_irp(Read, past
  the end)` calls or 100 `dispatch_block_control(Rescan)` calls on a partition, with no
  `run_pending`. Output: submits 0..64 succeed, then "panicked at src/dpc.rs:98:5: DPC queue
  capacity exceeded". Not booted, so whether 65 submits fit inside one tick on a running kernel is
  untested. Fix: an intrusive link in the IrpBox the way RECLAIM links it (see drv-o5). Lenses:
  abi, soundness, concurrency, opportunities. Medium. **Fix**, as sched-d5.
  Fixed in `b54b5e30`, as sched-d5.

- [x] **drv-d3** `kernel/src/drivers/xhci/storage.rs:1512` — `Disks::issue_hub` overwrites
  `dev.inflight` and rewrites the wrapper page without checking that no IRP command is in flight.
  `bind` runs hub commands for LUN 1.. after LUN 0's disk and partitions are published, with
  `recovering` clear (`recover()` and `rescan()` take the device first; `bind` does not). Scenario:
  a multi-LUN stick or card reader (GET MAX LUN >= 1). A filesystem server's read on LUN 0 starts
  between LUN 1's INQUIRY, TEST UNIT READY, READ CAPACITY and read_table. Its `Owner::Irp` is
  replaced by `Owner::Hub`. The IRP is never completed and is not handed back by `depart`, so its
  IrpBox, PO and buffer leak and the submitter waits forever. Its data and CSW events are taken as
  the hub command's stages. A fault recorded during bind makes `on_transfer` ignore every hub
  command, which times out after 5 s. An IRP queued behind a hub command is never started, because
  the `Owner::Hub` completion does not call `dev.next()`. Demonstrated: host tests in
  `xhci/storage/tests.rs` (since removed): `take(fake.dev(..))`, submit a Read on LUN 0 (started),
  then `issue_hub(i, po, 1, INQUIRY, 36)` returned true and inflight became `Owner::Hub { po }`.
  `depart(i)` then handed back `queued=0 held=[None, None, None]` with the read's status still
  Pending. Lenses: ownership, soundness, concurrency. Medium. **Fix.**
  Fixed in `e1202620`: the binding holds the device, as a rescan does, and `issue_hub` refuses one
  not held or with a submit's command in flight; fails without it:
  `a_hub_command_goes_only_on_a_held_device_and_never_replaces_a_read`, and `check-live --kvm`
  fails without the binding's hold.

- [x] **drv-d4** `kernel/src/drivers/ahci/bringup.rs:167` — `drivers::probe` calls `ahci::init` for
  every 01.06.01 function (drivers/mod.rs:36-37), and `init` stores each disk into the AHCI static
  with no "already claimed" check (xhci::init has one at xhci/mod.rs:299). The watch thread's SAFETY
  claim "published once, never withdrawn" (ahci.rs:687) then fails. Scenario: a machine with two
  AHCI controllers, each with a disk. The ISR, the three static DPCs, `poll_complete_inflight` and
  the watch thread all serve the second disk, while the first stays published with its own backend.
  Disk 1's first IRP hangs, and its queue then refuses with IoError. Over INTx, disk 1's IS is never
  cleared and the level line storms the BSP. Every disk 1 interrupt is also an idle-slot ISR run
  against disk 2, which makes drv-d1 reachable continuously. Demonstrated: booted the release image
  on q35 with its built-in AHCI holding the system disk plus a second `-device ich9-ahci` holding a
  64 MiB disk. The log shows two "port 0 disk ready" lines and two MSI vectors (0x30, 0x31). The
  AHCI static ended up pointing at the later controller. The first controller's disk failed its
  boot-time partition-table read with "partitions: the table did not read" (`Unread::Io`, not "no
  table"). Boot worked only because the system disk happened to enumerate last. No one-controller
  control was run. Lens: concurrency. Medium.
  **Fix**, with the bash's drv-8, moved here from Part H.
  Fixed in `ea25abea`, with drv-o11: a second controller is declined before it is touched. Shown
  by a boot with a second `ich9-ahci` behind a root port: on main the system disk's table did not
  read and / did not mount; here the second is declined and / mounts.

- [x] **drv-d5** `kernel/src/io/ramdisk.rs:199` — `RamDisk::transfer` bounds-checks only
  `irp.offset + irp.length` against the backing, then copies each fragment's own `len`. The SAFETY
  premise "the frags sum to len" is held only by `build_frags` in io::block, and `Irp` and
  `IrpBuffer` are pub with pub fields. Scenario: latent, since every IRP today comes from
  `build_frags`. Two ordinary changes would make it bite: implementing
  `TODO(block-transfer-split)` by trimming `length` per sub-command over a shared fragment array,
  or a Tier 2 filter driver (irp-layout.md says they walk `&mut Irp`) changing `length`. A write at
  the last block with `length` 512 and one 1024-byte fragment overruns the backing by 512 bytes. For
  the live image the backing is the Limine root module, so the write lands in kernel memory. AHCI
  (sector count) and USB storage (CBW length) cap at `length`; the RAM disk does not.
  Demonstrated: host test in `io/ramdisk.rs` (since removed): a 2048-byte disk over a 2560-byte
  Vec, Write at offset 1536 with length 512 and one 1024-byte fragment. Status 0, transferred 512,
  and the guard bytes [2048..2560] were overwritten (they were 0xAB). Miri also reports
  "attempting to access 1024 bytes, but got alloc...+0x600 which is only 512 bytes from the end of
  the allocation" at ramdisk.rs:209. Fix: copy `min(f.len, remaining)`, or check the fragment sum
  (see drv-o1). Lenses: abi, soundness, opportunities. Medium. **Fix.**
  Fixed in `2bdc48f7`: fragments short of the length are refused and only the length moves; fails
  without it: `a_transfer_moves_its_length_whatever_its_fragments_hold`.

- [x] **drv-d6** `kernel/src/drivers/mod.rs:197` — `pub(crate) fn adopt<T>(obj: KBox<T>, ty:
  KObjectType) -> ObjectRef` is a safe function with the whole contract of `ObjectRef::from_raw`:
  `T` must be the type `ty` names, header first. io/mod.rs:160 has a private copy. Scenario:
  latent. All current non-test callers (13 to 30 counted, by lens) pass the matching tag. One
  copy-pasted tag at a new call site compiles without `unsafe`, and the last drop runs
  `dispatch_destroy` as the wrong type, a type-confused free into the wrong slab cache.
  Demonstrated: host tests (since removed) called `adopt(InterruptObject..., MemoryObject)` and
  `adopt(MemoryObject, PendingOperation)` in safe code; the header said the true type and the
  ObjectRef said the wrong one (the test forgot the ref). Under Miri,
  `adopt(PendingOperation, MemoryObject)` then drop gives "constructing invalid value of type
  NonNull<mm::PhysAddr>: encountered 0" at libkern/kvec.rs:218, through `dispatch_destroy`. Lenses:
  abi, ownership, soundness, opportunities. Medium. **Fix**, as kobj-d8.
  Fixed in `9a28d4b0`, as kobj-d8.

- [x] **drv-d7** `kernel/src/dpc.rs:71` — `pub fn enqueue(dpc: &Dpc)` is safe and takes a borrow of
  any lifetime, but the queue keeps the address until a later drain calls its handler. Safe code
  can queue a stack or heap Dpc that is gone by then, and the drain reads a function pointer from
  dead memory. Scenario: `io::interrupt_object_self_test` (io/mod.rs:140-142) enqueues a stack Dpc
  and calls `run_pending`; that is sound only because it runs before SMP with interrupts masked.
  Moving it after AP bring-up would let another CPU's timer tail snapshot the address before this
  function returns and call `dpc.handler` out of a dead frame. Demonstrated: Miri on a test (since
  removed) that builds a stack Dpc, calls the public `enqueue`, returns, scribbles the stack and
  calls `run_pending`: "constructing invalid value of type &dpc::Dpc: encountered a dangling
  reference" at dpc.rs:122. Fix: `enqueue(&'static Dpc)` plus an `unsafe fn` for IRP-inline DPCs
  (see drv-o12). Lens: soundness. Medium. **Fix**, as sched-d3.
  Fixed in `1b7057f2`, as sched-d3.

- [x] **drv-d8** `kernel/src/drivers/xhci/hub.rs:626` — `data_bytes(mem, len: u16)` and
  `control_in`'s `write_bytes` (hub.rs:638) are safe functions taking any length up to 65,535 over
  the one-page data DmaBuffer. The page bound is held only at call sites (hub.rs:459's
  `.min(PAGE_SIZE)` and `read_interface_descriptor`'s clamp at :617). Scenario: latent. A new
  descriptor read of a device-supplied `wTotalLength` without the clamp (a BOS descriptor), or
  raising `CONFIG_MAX` and dropping the `.min`, would make `control_in` zero memory past the page
  and let the USB device DMA up to 64 KiB past it. An untrusted USB device would corrupt kernel
  memory. Demonstrated: host test in hub.rs tests (since removed): `device_mem()`, then
  `data_bytes(&mem, 3*4096)` returned a 12,288-byte slice over a 4,096-byte page. Lens:
  opportunities. Medium. **Fix.**
  Fixed in `7558d42d`: both cut the length to the page; fails without it:
  `a_read_of_the_data_page_stops_at_the_page`.

- [x] **drv-d9** `kernel/src/drivers/ahci/bringup.rs:156` — the boot log line computes `sectors *
  SECTOR_SIZE as u64` unchecked. `sectors` is IDENTIFY words 100..103 read as a full u64
  (ahci.rs:292-297), with only zero refused, and overflow checks are on in every image. Scenario: a
  disk or HBA emulation whose IDENTIFY word 103 is non-zero (2^55 sectors or more) panics the
  kernel inside `ahci::init` with "attempt to multiply with overflow", and the boot stops before
  userspace. The rest of the block layer uses checked or saturating arithmetic on the same value.
  Unverified: path read only. QEMU sizes the disk from its image, so it cannot report this. A host
  test of a factored-out size formatter fed `words[103] = 0x0080` would settle it. Fix:
  `saturating_mul`, or log in sectors and refuse counts above 2^48. Lens: abi. Medium. **Fix.**
  Fixed in `378ba883`: a safe `identity` refuses a count past LBA48; fails without it:
  `a_sector_count_past_lba48_is_refused`.

- [x] **drv-d10** `kernel/src/drivers/ahci.rs:390` — AHCI wants word-aligned PRDT bases (DBA bit 0
  reserved) and even byte counts (DBC bit 0 set), but the block layer accepts any `buf_offset`
  (`whole_blocks` checks only `offset` and `length`). Scenario: a process holding a disk submits
  Read with `buf_offset` = 4095 and length 512. `build_frags` gives [frame0+4095, len 1] and
  [frame1, len 511]. `build_command` writes DBA = frame0+4095 and DBC = 0 (odd). AHCI 1.3.1 §4.2.3.3
  leaves the behaviour undefined, and an HBA that honours the odd address and rounds the count up
  would write one byte past `frame0`, into an unrelated physical page. QEMU's AHCI model is
  byte-exact, so no gate shows it. Unverified: a host test shows `build_frags(&[0x100000,
  0x200000], 4095, 512)` giving the odd fragments (since removed). The hardware effect needs the
  laptop's Sunrise Point AHCI, with a read at `buf_offset` 4095 and a canary in the physically
  adjacent frame. The boot verifier could not reproduce it under QEMU. Fix: require an even or
  block-aligned `buf_offset` for block devices, as Linux's `queue_dma_alignment` does. Lens: abi.
  Medium. **Fix.**
  Fixed in `aa6170b1`: `dispatch_block_irp` refuses an odd `buf_offset`; fails without it:
  `a_block_request_off_its_devices_blocks_or_words_reaches_no_driver`. Still unverified on hardware.

- [x] **drv-d11** `kernel/src/drivers/ahci/bringup.rs:105` — `init` programs PxCLB/PxFB and starts
  the port (FRE and ST, lines 92-100) before three fallible allocations. Each failure at lines 105,
  112 and 135 returns `declined(OUT_OF_MEMORY)` with `cmd_list`, `fis` and `cmd_table` dropped back
  to the buddy allocator while the HBA still owns them. Scenario: out of memory at boot. The next
  D2H, PIO or SDB FIS (a link event, a COMINIT, a hot-plug) is written at `fis`+0x20..0x60, into
  whatever page the allocator has handed out since. xhci/mod.rs:488-493 fixed the same class by
  boxing before RUN; AHCI was not swept. Unverified: path read only (the boot verifier read lines
  100-135 and confirmed the order). An allocation-failure seam at line 105, followed by a check that
  PxCMD.FRE is clear before the DmaBuffers drop, would settle it. Fix: `stop_port` first, and
  forget the buffers when it fails (see drv-o7). Lenses: abi, ownership, soundness, opportunities.
  Low. **Fix.**
  Fixed in `f540c17f`, with drv-o4: every allocation comes before the port starts, and the disk
  is `Programmed` from then on; `a_declined_disk_frees_its_memory_only_once_its_port_stopped` shows
  the stop's proof decides the free. An allocation failure itself has no host seam.

- [x] **drv-d12** `kernel/src/drivers/xhci/mod.rs:579` — `stop()` discards both `wait_for` results
  (HCH set, HCRST complete). Every decline after USBCMD.RUN (lines 500, 505, 511) then drops the
  boxed `Xhci`, which frees the DCBAA, the scratchpad pages (the controller's own working memory),
  the command and event rings and the ERST, even if the controller is still running. Scenario: a
  controller that does not answer the bring-up No Op (mod.rs:503-506) and also does not halt within
  `HALT_NS`. `stop()` writes HCRST while HCH is clear (xHCI 1.2 §5.4.1 forbids it) and returns, and
  the controller can keep writing frames the allocator has handed out. The module doc's rule "a
  decline after the controller started stops it first" is held by intent. Unverified: read only. It
  needs a controller that ignores Run/Stop, which QEMU's model does not do. Fix: `stop()` returns
  whether HCH set, and `mem::forget(boxed)` when it did not. Lenses: ownership, soundness. Low.
  **Fix.**
  Fixed in `f540c17f`, with drv-o4: `stop` resets, and gives the proof, only once HCH is set; fails
  without it: `a_declined_controller_frees_its_memory_only_once_halted`.

- [x] **drv-d13** `kernel/src/drivers/xhci/hub.rs:384` — `control_in`'s SAFETY argument (hub.rs:634-638)
  is that any ep0 failure that can leave a transfer live "ends the device". `hid::bind` instead ends
  only its own binding when a request is in doubt (hid.rs:501, 516, timeouts included), and
  `hub::bind` then calls `storage::bind`, which issues GET MAX LUN on the same default endpoint and
  data page. Scenario: a composite HID+storage device NAKs its report-descriptor GET_DESCRIPTOR past
  `TRANSFER_NS`. `storage::bind`'s `control_in_byte` pushes new TRBs behind the stale ones and
  zeroes `mem.data`. The stale transfer can complete into `mem.data` while `data_bytes()` hands out
  a `&[u8]` over it, and its short-packet event writes its residual into the new Waiting record.
  The bytes stay inside the device's own page, but the unsafe block's precondition is false and GET
  MAX LUN reads a stale byte. Unverified: path read only (hid.rs:498-503 and 512-518,
  hub.rs:380-385). No QEMU device is composite and slow enough to reach it. Lens: abi. Low. **Fix.**
  Fixed in `53963f71` for HID, then for every class in `3a9baa69`: a default endpoint left in
  doubt is refused every later control transfer; fails without it:
  `a_default_endpoint_in_doubt_is_asked_nothing_more`.

- [x] **drv-d14** `kernel/src/drivers/xhci/desc.rs:187` — `Endpoint::dci()` turns a device-supplied
  `bEndpointAddress` into a Device Context Index without rejecting endpoint number 0. Scenario: a USB
  stick whose configuration lists bulk IN 0x81 and bulk OUT 0x00. `storage_interface` accepts it
  with out DCI 0, and `configure_endpoints` (context.rs:196-205) writes the endpoint context over
  the input slot context. An IN 0x80 becomes DCI 1 and overwrites the default endpoint's context.
  Doorbell target 0 is reserved, and two classes on one composite device can share a DCI, so
  `storage::on_transfer` swallows the HID's events. Everything stays in bounds, but a
  device-supplied index is used without a range check. Demonstrated: host test in
  `xhci/context.rs` (since removed): `storage_interface` over that configuration printed "out dci 0
  in dci 3", and `configure_endpoints` printed "slot ctx dw2 0xdeadb001 add flags 0x9", failing the
  assertion that the slot context's dword 2 is 0. Lens: abi. Low. **Fix.**
  Fixed in `53963f71`: `configure_endpoints` refuses an index below 2, or one twice, writing
  nothing; fails without it:
  `endpoints_naming_the_default_index_or_one_twice_are_refused_unwritten`.

- [x] **drv-d15** `kernel/src/drivers/xhci/storage.rs:1271` — `run()` copies `len` bytes of the
  binding buffer into `out` on any Passed status, ignoring the CSW residue. The buffer is never
  cleared between commands (`issue()` zeroes only the CSW area, lines 470-479). Scenario: a stick
  returns 8 of 18 sense bytes (residue 10) to REQUEST SENSE after an INQUIRY. `sense_answer` reads
  ASC/ASCQ at bytes 12-13, which are stale INQUIRY bytes, and TEST UNIT READY's "no medium" test or
  `sense_outcome` judges by them. A short READ CAPACITY likewise takes its block count and length
  from stale bytes. `hub::control_in` zeroes its page before each transfer for exactly this reason;
  the bulk path does not. Unverified: path read only. A host seam over `run_once` returning
  `Passed { residue: 10 }` after a pre-filled buffer would settle it. Lens: abi. Low. **Fix.**
  Fixed in `e1202620`: `issue_hub` clears the bytes it asks for; fails without it:
  `a_hub_commands_buffer_is_cleared_before_it_is_sent`.

- [x] **drv-d16** `kernel/src/io/block.rs:622` — `window_of` decides that a backend's `ctx` is a
  leaked `&'static Partition` by comparing `backend.submit as *const ()` with `partition_submit as
  *const ()`. Rust does not guarantee function-pointer identity (identical bodies may be merged, and
  the cast sidesteps `unpredictable_function_pointer_comparisons`). Scenario: latent. If another
  `fn(*mut Irp, *mut ())` compiled to an identical body, LLVM's merge-functions could give both one
  address; `window_of` would cast a RamDisk, AhciDisk or USB storage's packed integer to
  `&Partition` and `retire_window` would store to it. Conversely a Partition could compare unequal
  and miss its retire on rescan. Unverified: by reading (`core::ptr::fn_addr_eq`'s documentation).
  Fix: a tag in `BlockBackend`, or `block_info().kind == BlockKind::Partition` (see drv-o14). Lens:
  abi. Low. **Fix.**
  Fixed in `086cbccd`: a partition's node carries its window, set only by `try_new_partition`;
  fails without it: `a_node_is_a_window_by_what_made_it_not_by_its_backends_function`.

- [x] **drv-d17** `kernel/src/drivers/ahci.rs:468` — `publish_disk` ignores whether `device::register`
  took the node. `register` returns nothing and drops the node when the table cannot grow
  (device.rs:521-526, 176-184). It then calls `Counted::attach(&r)` on the SAFETY argument that the
  table holds the node for the kernel's lifetime. When registration failed, `r` drops at the end of
  the function and the disk's `Counted` keeps a dangling `*const IoStats`. io/ramdisk.rs:363-365
  (`publish_modules`) has the same shape; storage.rs:1078-1085 attaches only after `register`
  succeeded. Scenario: OOM growing the device table. Nothing can submit to an unregistered node
  today, so `record()` never writes the pointer; any change that reaches the backend without the
  registry makes every completion a `fetch_add` on freed slab memory. Unverified: path pointed at.
  A host test failing registry growth, then `Counted::record` under Miri, would settle it. Lenses:
  soundness, opportunities. Low. **Fix.**
  Fixed in `9e422204`, as drv-o18.

- [x] **drv-d18** `kernel/src/drivers/xhci/hid.rs:996` — `depart`'s keyboard branch offers its key
  releases in their own reader-lock holds and only then retires the node with no releases (line
  999). A report the DPC decoded before `depart` took the Bound entry can be pushed in between and
  land after the releases, the case `Nodes::push`'s comment says retiring under the lock prevents.
  The mouse branch retires with its releases in one hold and does not have the gap. Scenario: a USB
  keyboard still typing as its departure runs (a contact bounce, which `hub::main` treats as a
  departure then an arrival). `on_transfer` decodes a press under the hid lock, `depart` offers the
  release, the DPC's `deliver` calls `NODES.push` (accepted, the node is not retired yet), then
  `retire` follows. The departed node's ring ends with the key down. Demonstrated: host test in
  `xhci/hid/tests.rs` (since removed): `readers[i].lock().offer([release 30, syn])`, then
  `push(i, epoch, [press 30, syn])`, then `retire(i, &[])`. The ring's key events were
  [(30,0),(30,1)], ending on a press. The existing single-hold test shows the late push dropped.
  Lens: concurrency. Low. **Fix.**
  Fixed in `f077ec46`: `Nodes::retire` pushes the releases in the hold that retires the node;
  fails without it: `a_departing_keyboards_releases_end_its_ring_whatever_the_dpc_pushes`.

- [x] **drv-d19** `kernel/src/drivers/xhci/mod.rs:647` — `xhci_dpc` can run as two concurrent
  instances (`dpc.rs` clears `queued` before the handler, and the queue is global). Each drains a
  16-event batch under the `events` lock and acts on it after releasing, so events from consecutive
  batches can be acted on out of ring order. Scenario: instance A on an AP's timer tail drains a
  batch ending in an EP0 short Data-stage event; an MSI re-enqueues the DPC and instance B on the
  BSP drains the next batch holding that transfer's Status event. B runs `ep0_event` first, takes
  the record with `w.short = 0` and completes the transfer as whole, so `control_in` returns `len`
  bytes where fewer arrived. The same interleaving can deliver two HID reports in swapped order.
  Unverified: read of `xhci_dpc`, dpc.rs:13-16, `ep0_event` and `hid::on_transfer`. A host test
  driving two batches through `on_event` in swapped order against a Waiting record would settle it.
  Lens: concurrency. Low. **Fix.**
  Fixed in `06bd48dc`: `xhci_dpc` drains through `dpc::OneAtATime`; fails without it:
  `an_instance_arriving_meanwhile_is_run_after_the_first_not_beside_it`, and under Miri
  `every_instances_work_is_taken_by_a_round_after_it`.

- [x] **drv-d20** `kernel/src/drivers/xhci/ring.rs:328` — `Consumer::next` reads the whole event TRB
  through `DmaSlots::read` (dwords 0, 1, 2, then 3, mod.rs:228-236) and only then checks the cycle
  bit in dword 3, with no acquire fence between the cycle check and the other dwords. Linux reads
  the cycle first, then `rmb()`, then the rest. Scenario: the DPC reads dwords 0-2 of event slot k
  (the previous lap's), the controller's write lands, and the DPC reads dword 3 with the new cycle,
  consuming the event with the old pointer and code. A Command Completion matching nothing leaves
  the hub thread to time out and mark the controller wedged; a transfer event naming the wrong TRB
  is lost and leaves a storage command to its 30 s deadline. It depends on the controller's 16-byte
  write not being observed atomically (QEMU writes it with one `dma_memory_write`). Unverified:
  read only. A `Slots` stand-in whose read returns dwords from two laps would settle it once the
  reader reads dword 3 first (see drv-o11). Lens: concurrency. Low. **Fix.**
  Fixed in `dec4dcbb`, with drv-o16: `RingMem` reads dword 3 first, then an acquire; fails
  without it: `an_event_written_while_it_is_read_is_taken_whole_or_not_yet`.

- [x] **drv-d21** `kernel/src/io/ramdisk.rs:132` — for an Owned backing, `RamDisk::base()` returns
  `v.as_ptr()`. `KVec` has no inherent `as_ptr`, so this goes through Deref to a `&[u8]` and the
  pointer carries SharedReadOnly permission; `transfer()` then writes through it for a Write IRP,
  mutation behind a shared reference with no `UnsafeCell`, from `&self`. Scenario: latent. The
  Owned backing is only the bring-up ramdisk, which `io::self_test` reads and never registers.
  Registering it, or writing to it in a test, gives UB that optimisation may exploit.
  Demonstrated: Miri on a test (since removed) that builds `RamDisk::try_new()` and transfers a
  Write IRP with one 512-byte fragment: "attempting a write access using <tag> ... but that tag
  only grants SharedReadOnly permission", the tag created at ramdisk.rs:132 (Stacked Borrows). Fix:
  keep a raw pointer taken from `&mut` at construction, or wrap the bytes in `UnsafeCell`. Lens:
  soundness. Low. **Fix.**
  Fixed in `3eb3defd`: the RAM disk takes the base of the bytes it owns once, through a pointer that
  may write; fails without it, under Miri: `a_disk_over_its_own_bytes_writes_and_reads_them_back`.

**Opportunities**

- [ ] **drv-o1** encapsulate: an in-flight IRP is passed as a raw `*mut Irp` through the safe
  `fn(*mut Irp, *mut ())` of `BlockBackend::submit`. Each driver derefs it raw, rebuilds its own
  fragment slice with its own null and zero-count handling, and completes it by hand
  (`set_completion` + `dpc::enqueue(&(*irp).dpc)`); "completed exactly once" and "fragments sum to
  length" are held by convention — 53 sites (46 occurrences of `*mut Irp` / `AtomicPtr<Irp>` in
  drivers/ and io/ outside tests, plus the 7 completion sites; the 27 raw `(*irp)` lines, 3
  non-test `from_raw_parts(.*PhysFrag` and 7 non-test `set_completion(` are subsets), such as
  `kernel/src/io/block.rs:116`, `kernel/src/drivers/ahci.rs:630`,
  `kernel/src/drivers/xhci/storage.rs:747`, `kernel/src/io/ramdisk.rs:195`. `pub(crate) struct
  InFlight(NonNull<Irp>)`, not Copy, minted only by io::block's dispatch (carrying the IrpBox
  pointer, so completion has full provenance and the tests' complete path is Miri-clean). It offers
  safe `op()`, `range()`, `frags()` (built once, empty for count 0, sum == length asserted when
  minted), `rebase(&mut self, off)` for a partition, `complete(self, status, transferred)` which
  consumes it, and `keep(self)` as an explicit leak for a command a stuck device may still move. A
  debug Drop catches a token dropped without either. `BlockBackend::submit` takes it; AHCI's
  `PendingRing` and storage's `IrpCmd`, `Action`, `stranded`, `rescan` and `Departed` hold it. The
  hashed Tier-2 Irp layout is unchanged. In `kernel/src/io/irp.rs` or beside IrpBox in
  `io/block.rs`. Closes: the RAM disk overrun (drv-d5), double or lost completion (drv-d3's IRP
  would have been a dropped token), the three variants of null and zero-count handling, and safe fn
  pointers that dereference raw arguments. Depends on: nothing. Lenses: abi, ownership,
  opportunities. Value high, effort medium. **Plan**, in P5.

- [ ] **drv-o2** encapsulate: kernel objects are adopted from a `KBox` and downcast from an
  `ObjectRef` by hand. `ObjectRef::from_raw(KBox::into_raw(x).as_ptr() as *mut (),
  KObjectType::Y)` is written inline or through the untyped safe `adopt` helpers, and `&*(r.as_ptr()
  as *const DeviceNode|MemoryObject)` relies on "type checked by the caller" behind safe pub fns
  (`dispatch_block_irp`, `max_frags`, `read_blocking`, `Partition::new`, `retire_window`,
  `window_busy`, `device::register*`, `gpt::init`) — 50 sites in the subsystem's paths (27 casts by
  python scan of non-test code, 10 `ObjectRef::from_raw(KBox::into_raw`, 13 `adopt` helper calls;
  108 kernel-wide; lenses counted 25 to 146 depending on pattern breadth), such as
  `kernel/src/io/block.rs:143`, `kernel/src/device.rs:98`, `kernel/src/drivers/mod.rs:197`,
  `kernel/src/drivers/gpt.rs:56`. `pub unsafe trait KObject { const TYPE: KObjectType; }`, sealed
  and implemented once per header-first `#[repr(C)]` object type; safe `ObjectRef::adopt<T:
  KObject>(KBox<T>) -> ObjectRef` taking the tag from `T`; `ObjectRef::downcast<T: KObject>(&self) ->
  Option<&T>` checking `object_type() == T::TYPE` (what `Counted::attach` does by hand at
  io/stats.rs:104). Better still, `TypedRef<T: KObject>(ObjectRef)` with `Deref<Target = T>`, so
  `dispatch_block_irp` takes `&TypedRef<DeviceNode>` and the registry stores
  `TypedRef<DeviceNode>`. `from_raw` also gains a `debug_assert` that the header's tag equals `ty`.
  In `kernel/src/object/header.rs`. Closes: the type-confused adopt (drv-d6) and unchecked casts
  reached from safe signatures; it removes 27 of the subsystem's 96 raw derefs. Depends on:
  nothing. Lenses: abi, ownership, soundness, opportunities. Value high, effort medium.
  **Plan**, as kobj-o1, in P1.

- [ ] **drv-o3** encapsulate: DMA memory is read and written through raw pointers from
  `DmaBuffer::virt()`, or u64 addresses kept beside the buffer, at hand-computed offsets: AHCI
  command FIS, PRDT, command header and IDENTIFY page, the xHCI ERST, scratchpads and DCBAA, the
  hub's ep0 data page (`data_bytes`, `control_in`), the storage wrapper and binding pages, and the
  HID report buffer — 28 sites (18 `.virt()` sites outside tests behind about 30 unsafe blocks;
  grep over ahci.rs and xhci/{mod,hub,storage,hid}.rs: ahci 8, xhci/mod 6, hub 4, storage 7, hid
  3), such as `kernel/src/drivers/ahci.rs:389`, `kernel/src/drivers/xhci/storage.rs:474`,
  `kernel/src/drivers/xhci/hub.rs:628`, `kernel/src/drivers/xhci/mod.rs:569`. Bounds-checked
  volatile accessors on `DmaBuffer` taking `&self`: `read<T: Copy>(off)`, `write<T: Copy>(off, v)`,
  `copy_from`, `copy_to`, `fill`, each checking `off + len <= self.len()`; `as_slice()` dropped
  (a `&[u8]` over device-written memory is a data race in Rust's model); IDENTIFY copied once into
  `[u16; 256]` so `format_identity` stops being unsafe; `DmaBuffer::region() -> DmaRegion { virt,
  phys, len }` (Copy) with the same accessors for the addresses USB storage and HID keep. In
  `kernel/src/mm/dma.rs`. Closes: a write past a DMA structure the device then follows — the
  demonstrated `data_bytes` slice (drv-d8), the PRDT count guarded by convention twice
  (ahci.rs:363-366, 491), the DCBAA slot index. Depends on: nothing. Lenses: abi, opportunities.
  Value high, effort medium. **Plan**, in P5.

- [x] **drv-o4** encapsulate: device memory handed to a controller, then freed by drop on a path
  that has not proved the controller quiescent: decline paths after a port or controller was
  started, and teardown that forgets memory by hand when a stop fails — 9 sites (read:
  ahci/bringup.rs:105, :112, :135; ahci.rs:285 (identify forgets `data` when `stop_port` fails);
  xhci/mod.rs:500, :505, :511; xhci/hub.rs:289, :293), such as
  `kernel/src/drivers/ahci/bringup.rs:105`, `kernel/src/drivers/xhci/mod.rs:505`,
  `kernel/src/drivers/xhci/hub.rs:293`, `kernel/src/drivers/ahci.rs:285`. `struct
  Programmed<T>(ManuallyDrop<T>)`, made when an address is written to a device; its Drop leaks, and
  it releases `T` only through `fn release(self, _: Quiesced) -> T`, where `Quiesced` is minted only
  by a stop that verified the device halted (AHCI `stop_port` returning true, xHCI HCH observed,
  Disable Slot succeeding). Fits AHCI bring-up (cmd_list, fis, cmd_table), xHCI init (dcbaa,
  scratchpads, rings) and `hub::release` (DeviceMem). In `kernel/src/mm/dma.rs`. Closes: device DMA
  into memory the allocator has handed out again (drv-d11, drv-d12); the comment at
  xhci/mod.rs:488-490 records a fixed instance. Depends on: nothing. Lens: soundness. Value high,
  effort medium. **Fix.**
  Fixed in `f540c17f`: `mm::dma::Programmed` and `Quiesced`, at AHCI's bring-up and IDENTIFY,
  xHCI's declines after RUN and `hub::release`; fails without it:
  `programmed_memory_is_freed_only_once_its_device_stopped`.

- [x] **drv-o5** encapsulate: IRP completion through each IRP's own inline Dpc — `set_completion` +
  `dpc::enqueue(&(*irp).dpc)` in every driver, so the global DPC queue holds one entry per completed
  IRP and its fixed reserve bounds nothing — 7 sites (`grep -rn
  'dpc::enqueue(&(\*irp).dpc)\|enqueue(unsafe { &(\*irp).dpc })'` outside tests), such as
  `kernel/src/io/block.rs:119`, `kernel/src/io/block.rs:515`, `kernel/src/drivers/ahci.rs:634`,
  `kernel/src/drivers/xhci/storage.rs:751`. `pub(crate) unsafe fn io::block::complete(irp: *mut Irp,
  status: i32, transferred: u64)` pushes the IrpBox onto an intrusive COMPLETED list (the box already
  carries a link field) under a leaf `IrqSpinLock` and enqueues one static IRP DPC, which pops and
  signals each PO and then moves the box to RECLAIM. The DPC queue then holds only static Dpcs, so
  `DPC_RESERVE` is a real bound. It also replaces seven hand-written pairs with one call. In
  `kernel/src/io/block.rs`. Closes: DPC-reserve exhaustion, a kernel panic reachable by any holder of
  a block handle (drv-d2). Depends on: nothing. Lens: concurrency. Value high, effort small.
  **Fix.**
  Fixed in `b54b5e30`: drivers complete with `io::irp::complete`, which links the IRP by `next` for
  one static `FINISH` DPC and its `on_complete`; the `Irp` loses its inline `dpc`, an ABI-hash
  change. `more_refusals_than_the_dpc_queue_holds_each_complete`.

- [ ] **drv-o6** encapsulate: "a DPC must never drop an `ObjectRef`" (the same-CPU SlabCache
  deadlock, decision log 2026-08-06, and the earlier input bug input/mod.rs cites) is held by
  hand-rolled hand-offs: io::block's RECLAIM list, `input::Reader`'s `parked`/`to_drop`/`delivering`,
  console.rs's copy of Reader (`Inner`, `read_now`, `take_ready`, `owe`, `reclaim`, plus its own
  `copy_into_memobj`), PS/2's `lights_done` Waiting/Completing/Completed, and the xHCI/storage PO
  take-back (`Waiting` + `hub::wait`, `Owner::Hub` + `run_once`); `owe` overwrites `to_drop` and so
  drops in place if its `debug_assert` breaks in a release build — 6 sites (4 reclaim hand-offs by
  `grep -rn 'fn owe\b\|Completion::Completing\|RECLAIM.lock()\|fn take_back'`, plus the 2 PO
  take-back sites; `sched::reap_pending` calls each by name, sched/exit.rs:441-445), such as
  `kernel/src/drivers/console.rs:93`, `kernel/src/drivers/input/mod.rs:154`,
  `kernel/src/drivers/ps2/mod.rs:258`, `kernel/src/drivers/xhci/storage.rs:1602`. One
  `Handoff<T>` / `DeferredDrop<T>`: `park(T)`, allocation-free and refusing rather than dropping in
  place; `lend(&mut self) -> Option<Loan<T>>` under the driver's lock, where `Loan` derefs to `T`
  and its Drop is a debug panic; `give_back(Loan<T>)`; `take_owed()`/`take_all()` for thread
  context only, dropping outside the lock; registered once with `reap_pending`. A `PoLease` on it
  covers the timed waits. `console` moves onto `input::Reader` generic over its ring (`trait Drain {
  fn is_empty(&self) -> bool; fn drain_into(&mut self, &mut [u8], now: u64) -> usize; }`) and
  deletes its `copy_into_memobj` for `input::deliver`/`deliver_now`. In `kernel/src/libkern` or
  `kernel/src/sched` (beside `deferred_drops`), with the console half in `drivers/input/mod.rs`.
  Closes: an `ObjectRef`'s last drop in DPC or IRQ context, and another CPU dropping what a DPC is
  using — both have bitten this codebase. Depends on: nothing. Lenses: abi, ownership, soundness,
  concurrency, opportunities. Value high, effort medium. **Plan**, in P5.

- [ ] **drv-o7** narrow: MMIO accessors are safe functions that dereference whatever base and offset
  they are given. AHCI and xHCI `read32/write32/write64(base: u64, off: u64)`, PCI
  `MmioCfg::read32/write32(off: u16)` over a 4 KiB window with no range or alignment check, and
  storage's doorbell write. xHCI's operational, runtime and doorbell bases come from
  CAPLENGTH/RTSOFF/DBOFF read from the device and are never checked against BAR0's mapped size
  (xhci/mod.rs:354-355), and PORTSC offsets come from MaxPorts — 93 call sites of the three
  accessors (`grep -cE '\b(read32|write32|write64)\('`: ahci.rs 28, bringup.rs 13, xhci/mod.rs 45,
  hub.rs 6, hid.rs 1) behind 7 accessor definitions (ahci.rs:204, 210; xhci/mod.rs:252, 258;
  pci/mod.rs:49, 58; storage.rs:524), such as `kernel/src/drivers/ahci.rs:204`,
  `kernel/src/drivers/xhci/mod.rs:252`, `kernel/src/pci/mod.rs:55`,
  `kernel/src/drivers/xhci/hub.rs:873`. `kvmap::map_mmio` returns `Mmio { base, len }` with safe
  `read32(off)`/`write32(off, v)` asserting `off % 4 == 0 && off + 4 <= len`, and `sub(off, len) ->
  Option<Mmio>` so xHCI's op/rt/db windows and AHCI's port window are checked once at init; PCI's
  `MmioCfg` becomes a 0x1000-byte `Mmio`; a `cfg(test)` `Mmio::over(&'static mut [u32])` keeps the
  AHCI fake-register tests. In `kernel/src/mm/kvmap.rs`. Closes: a device-supplied offset (DBOFF,
  RTSOFF, MaxPorts, a slot ID) or a wrong base turning a register access into a write to arbitrary
  kernel memory, and out-of-window or unaligned config reads through a safe trait method. Depends
  on: nothing. Lenses: abi, soundness, opportunities. Value medium, effort medium. **Plan**, in P5.

- [ ] **drv-o8** encapsulate: the scheduler's object calls (`complete_pending_op`,
  `pending_op_is_signaled`, `pending_op_completion`, `signal_interrupt`, `interrupt_consume`,
  `wait_on`) take `*mut ()` with "pinned by a reference the caller holds", and most callers pass
  `.as_ptr()` of an `ObjectRef` they hold, each in its own unsafe block; a sleep is spelled
  `wait_on(&[], ...)` (hub.rs:920) — 35 sites (47 calls outside tests in drivers/, io/ and
  device.rs, 35 passing `as_ptr()` of a held ref; other lenses counted 31 and 32; 52 kernel-wide),
  such as `kernel/src/io/block.rs:441`, `kernel/src/drivers/xhci/hub.rs:820`,
  `kernel/src/drivers/input/mod.rs:217`, `kernel/src/device.rs:768`. Safe typed forms over a
  borrowed, type-checked ref: `PoRef::complete(&self, status, result)`, `is_signaled`,
  `completion`, `IntrRef::signal`/`consume`, `sched::wait_refs(&[&ObjectRef], deadline, now)` and
  `sched::sleep_until(deadline)`; the borrow satisfies "pinned for the call". The remaining 12
  genuinely raw sites (xHCI `PoPtr`, storage `Owner::Hub`, `irp.completion`, PS/2 lights, AHCI's
  leaked wake/intr) keep the unsafe fn. In `kernel/src/sched/objects.rs` and
  `kernel/src/sched/wait.rs`. Closes: a PO or interrupt operation on a wrong-typed or already-dead
  object address. Depends on: drv-o2 (or a runtime `object_type()` check in each wrapper). Lenses:
  soundness, concurrency, opportunities. Value medium, effort medium. **Plan**, as kobj-o7, in P1.

- [x] **drv-o9** encapsulate: AHCI slot-0 state (`inflight`, `issued_at`, `dead`, `fault`) is kept
  as atomics with comments saying "accessed under pending's lock", plus `unsafe fn issue_locked`
  whose contract is "the caller holds `pending` and slot 0 is idle" — 23 sites (`grep -c
  'inflight\.\|issued_at\.\|\.dead\.\|fault\.\(load\|store\|compare\)' kernel/src/drivers/ahci.rs`),
  such as `kernel/src/drivers/ahci.rs:506`, `kernel/src/drivers/ahci.rs:596`,
  `kernel/src/drivers/ahci.rs:610`, `kernel/src/drivers/ahci.rs:727`. `IrqSpinLock<Port { ring:
  PendingRing, slot0: Option<NonNull<Irp>>, issued_at: u64, fault: Fault, dead: bool }>` with a
  safe `fn issue(port: &mut Port, regs: &Regs, irp)`, since the guard is the proof. The ISR's fault
  report, its PxCI read and the retire happen in one hold, and the lock-free peeks (`due_at`,
  `poll_disk`) take the lock. In `kernel/src/drivers/ahci.rs`. Closes: decisions taken on device or
  driver state read outside the port lock (drv-d1), with the lock held by structure rather than by
  comment. Depends on: nothing. Lens: concurrency. Value medium, effort small. **Fix.**
  Finished in `1a8de805`: slot 0's state is `PortState`, inside the port lock.

- [x] **drv-o10** narrow: `dpc::enqueue(&Dpc)` is a safe API whose borrow outlives the call —
  static DPCs, IRP-inline DPCs and one stack DPC all go through it — 22 sites (`grep -rn
  'dpc::enqueue\|enqueue(&' kernel/src --include=*.rs` outside dpc.rs: 12 static (ioapic.rs:145,
  console.rs:212, xhci/mod.rs:640, ps2/mod.rs x6, ahci.rs x3), 8 IRP-inline (block.rs x4,
  ahci.rs:572/634, storage.rs:751, ramdisk.rs:239), file_object.rs:1255, io/mod.rs:141 stack), such
  as `kernel/src/dpc.rs:71`, `kernel/src/io/mod.rs:141`, `kernel/src/io/block.rs:119`,
  `kernel/src/drivers/xhci/mod.rs:640`. `pub fn enqueue(dpc: &'static Dpc)` (the 12 static sites
  need no change) and `pub unsafe fn enqueue_pinned(dpc: &Dpc)` whose `# Safety` says the Dpc stays
  at its address until its handler has run; the 9 heap/IRP sites already sit in unsafe blocks, and
  the stack Dpc in io/mod.rs becomes a static. In `kernel/src/dpc.rs`. Closes: use-after-free of a
  queued Dpc reached from safe code (drv-d7). Depends on: nothing. Lens: soundness. Value medium,
  effort small. **Fix**, as sched-o4.
  Fixed in `1b7057f2`, as sched-o4.

- [x] **drv-o11** encapsulate: a static `AtomicPtr<T>` is published once, then `unsafe { &*p }` with
  "published once, never withdrawn" at every ISR, DPC and thread entry; xHCI already has a safe
  `controller()` accessor (xhci/mod.rs:128) that five sites bypass by hand; Partition and RamDisk
  are leaked with `unsafe { &*KBox::into_raw(b).as_ptr() }`, and AHCI patches `(*disk).sectors`
  through the raw pointer after leaking it — 17 sites (13 unsafe derefs of `AHCI.load`/`XHCI.load`:
  ahci.rs 647/676/688/814/822/834, xhci/mod.rs 131/517/637/653/776, hub.rs:123, hid.rs:1030; plus
  4 hand leaks: bringup.rs:134, block.rs:608, io/mod.rs:52, ramdisk.rs:343), such as
  `kernel/src/drivers/ahci.rs:647`, `kernel/src/drivers/xhci/mod.rs:653`,
  `kernel/src/drivers/xhci/hid.rs:1030`, `kernel/src/io/block.rs:608`. `pub struct Published<T: Sync
  + 'static>(AtomicPtr<T>)` with `const fn new()`, `fn publish(&self, b: KBox<T>) ->
  Result<&'static T, KBox<T>>` (compare-exchange from null with Release, refusing a second publish)
  and `fn get(&self) -> Option<&'static T>` (Acquire); a plain `KBox::leak(self) -> &'static T`
  covers RamDisk and Partition. The `Sync` bound forces an argued `unsafe impl Sync` where `Xhci`
  today shares `DmaBuffer` fields across CPUs without one (checked sound: `dcbaa` is written only by
  the hub thread, scratchpads and ERST never after init). In `kernel/src/libkern` (beside KBox and
  IrqSpinLock). Closes: a second publish silently replacing the first (drv-d4), and dereferencing a
  published pointer before initialisation finishes. Depends on: nothing. Lenses: ownership,
  soundness, concurrency, opportunities. Value medium, effort small. **Fix.**
  Fixed in `ea25abea`: `libkern::Published`, AHCI's and xHCI's statics, the 13 derefs gone, and
  kernel/CLAUDE.md names it. The hand leaks of the RAM disks and partition windows stay: many
  values, where a `Published` holds one. Fails without it:
  `a_value_is_published_once_and_a_second_is_refused`.

- [ ] **drv-o12** encapsulate: `BlockBackend`'s `ctx: *mut ()` is re-cast at every submit and poll
  (Partition, AhciDisk, RamDisk), or is an integer smuggled through the pointer (USB slot/lun/epoch,
  storage.rs:136; HID and PS/2 indexes, hid.rs:75). Its `submit`, `poll` and `ctx` fields are pub,
  so safe code can pair any ctx with any submit; `window_of` identifies a Partition by comparing
  fn-pointer addresses (block.rs:622) — 11 sites (read: `backend.submit`/`poll` calls at
  block.rs:114, 316, 438, 523, 533, 574; ctx dereferences at block.rs:499, 573, 627, ahci.rs:486-488,
  ramdisk.rs:232; other lenses counted 5 and 8), such as `kernel/src/io/block.rs:622`,
  `kernel/src/io/block.rs:499`, `kernel/src/drivers/ahci.rs:486`, `kernel/src/io/ramdisk.rs:232`.
  `enum BlockCtx { Partition(&'static Partition), Ahci(&'static AhciDisk), Ram(&'static RamDisk),
  Usb { slot: u8, lun: u8, epoch: u32 }, #[cfg(test)] Test(..) }` dispatched by `match`, as
  kernel/CLAUDE.md prefers, so `window_of` becomes `matches!(ctx, BlockCtx::Partition(p))`; or
  private fields with `unsafe fn` submit/poll called only through `BlockBackend::submit(&self,
  irp)`. In `kernel/src/io/block.rs`. Closes: reading one backend's context as another's type, and a
  window missed on rescan because two function addresses compare equal or unequal (drv-d16).
  Depends on: drv-o11 (for the `'static` device refs). Lenses: ownership, soundness, opportunities.
  Value medium, effort medium. **Plan**, in P5.

- [ ] **drv-o13** encapsulate: the storage `Dev` and HID `Bound` table entries keep raw u64 virtual
  addresses of rings, wrapper, binding and report pages that live in `DeviceMem.class`, owned by
  the hub thread. `RawSlots(u64)` is a safe struct whose read and write dereference any address,
  and every access argues the memory lives "as long as the entry is in the table"; the
  free-after-Disable-Slot order is held by the call order of `depart` and `release`. hid's
  `RawSlots` also lacks the Release fence that `DmaSlots` and storage's `RawSlots` put before the
  cycle dword — 10 sites (unsafe blocks over a stored address: storage.rs 215, 224, 229, 474, 478,
  532, 1277; hid.rs 375, 384, 817; addresses captured at storage.rs:861/870/872 and
  hid.rs:446/463), such as `kernel/src/drivers/xhci/storage.rs:202`,
  `kernel/src/drivers/xhci/storage.rs:1277`, `kernel/src/drivers/xhci/hid.rs:362`,
  `kernel/src/drivers/xhci/hid.rs:817`. `Dev` and `Bound` own their DmaBuffers, moved in by `bind`;
  `struct DmaRing { mem: DmaBuffer, producer: Producer }` with one bounds-checked `Slots` impl;
  `storage::depart` and `hid::depart` return them in `Departed`, and `finish()` frees them only
  when `release()` disabled the slot (`Released::Freed`), forgetting them otherwise (the idea
  `Retired(KVec<DmaBuffer>)` + a `SlotDisabled` proof from a successful Disable Slot). The storage
  tests' `Fake` would allocate DmaBuffers on the host heap. In `kernel/src/drivers/xhci/`
  (`ring.rs`, `storage.rs`, `hid.rs`, `hub.rs`). Closes: a table entry outliving the memory it
  addresses (the DPC touching a freed ring or report page), freeing a device's memory before its
  slot is disabled, and safe `RawSlots` writes to an arbitrary address. Depends on: drv-o3 and
  drv-o4 for the accessors and for `release` (not required). Lenses: ownership, soundness,
  opportunities. Value medium, effort medium. **Plan**, in P5.

- [ ] **drv-o14** remove: MemoryObject bytes are read through hand-computed HHDM pointers (frame +
  `hhdm_offset`) instead of a bounded accessor, and `copy_into_memobj` is duplicated as an `unsafe
  fn(*const ())` (console.rs:115, input/mod.rs:241) although every caller holds an `&ObjectRef`,
  which pins the object just as well — 10 sites (2 `copy_into_memobj` definitions, 4 unsafe calls
  at console.rs:156/176 and input/mod.rs:215/229, 4 HHDM frame reads at block.rs:450-458,
  drivers/mod.rs:190-194, input/mod.rs:255-260, io/mod.rs:111-119, from `grep -rn 'hhdm_offset()'
  kernel/src/drivers kernel/src/io`), such as `kernel/src/io/block.rs:458`,
  `kernel/src/drivers/input/mod.rs:260`, `kernel/src/drivers/console.rs:115`,
  `kernel/src/drivers/mod.rs:194`. `MemoryObject::copy_out(&self, offset: usize, dst: &mut [u8]) ->
  usize`, the bounded twin of the existing `copy_in`; `copy_into_memobj` then takes `buffer:
  &ObjectRef` and becomes safe, and the console uses the one shared helper. In
  `kernel/src/object/memory_object.rs` and `kernel/src/drivers/input/mod.rs`. Closes: reads past a
  MemoryObject's frames held only by `sys_io_submit`'s earlier bound, and a dangling `*const ()`
  reaching the copy. Depends on: nothing (drv-o2 optional). Lenses: opportunities, soundness
  (the console copy). Value medium, effort small. **Plan**, in P2.

- [ ] **drv-o15** encapsulate: a PendingOperation is borrowed by raw pointer (`PoPtr`,
  `Owner::Hub { po }`, `Action::Hub`) while the hub thread's `ObjectRef` pins it, and "take the
  record back on timeout, else wait for the completion already on its way" is hand-written twice;
  registration overwrites an armed record silently (the shape of drv-d3) — 10 sites (record
  registration hub.rs:651, 679, 714, 791 and storage.rs:1516 (`issue_hub`); waits hub.rs:816
  (`wait`) and storage.rs:1291 (`run_once`); DPC completions xhci/mod.rs:704, :753 and
  storage.rs:775), such as `kernel/src/drivers/xhci/hub.rs:816`,
  `kernel/src/drivers/xhci/storage.rs:1291`, `kernel/src/drivers/xhci/mod.rs:746`. `Armed<'po>`,
  made by `arm(&ObjectRef, slot: &IrqSpinLock<Option<Waiting>>)`, with `wait(self, bound) ->
  Option<(u8, u64)>` doing take-back-or-wait-out; the DPC side gets a `Completer` it must consume
  via `complete(code, result)` after letting the lock go. The raw pointer never leaves the pair,
  and registration refuses while a record is already armed. In `kernel/src/drivers/xhci/mod.rs`
  (or `sched`, beside `wait_on`). Closes: completing a PO after its waiter dropped it, and one
  record silently replacing another. Depends on: nothing. Lens: ownership. Value medium, effort
  medium. **Plan**, in P5.

- [x] **drv-o16** encapsulate: three hand-written `Slots` impls over TRB ring memory (xhci/mod.rs
  `DmaSlots`, storage.rs `RawSlots`, hid.rs `RawSlots`) disagree on ordering: `DmaSlots` and
  storage fence(Release) before the cycle dword, hid writes all four dwords with no fence although
  the `Slots` trait says dword 3 hands the TRB over, and all three read the cycle dword last
  (drv-d20) — 3 sites (`grep -n 'impl Slots for' kernel/src/drivers` minus the test impl in
  ring.rs, 9 volatile accesses between them), such as `kernel/src/drivers/xhci/mod.rs:223`,
  `kernel/src/drivers/xhci/storage.rs:204`, `kernel/src/drivers/xhci/hid.rs:364`. One
  `struct RingMem { virt: u64 }` in ring.rs implementing `Slots`: `read` loads dword 3, then
  `fence(Acquire)`, then dwords 0..2; `write` stores dwords 0..2, `fence(Release)`, then dword 3;
  built only from a `DmaBuffer` of `RING_TRBS * 16` bytes (`RingMem::of(&DmaBuffer)`), so the
  TRB-count invariant behind every SAFETY comment holds by construction. In
  `kernel/src/drivers/xhci/ring.rs`. Closes: accepting a torn event TRB, and a ring copy that hands
  a TRB to the controller before its fields are in place; hid is ordered today only by x86 TSO,
  which matters once aarch64 lands. Depends on: drv-o3's region accessors (optional). Lenses: abi,
  concurrency. Value medium, effort small. **Fix.**
  Fixed in `dec4dcbb`: `ring::RingMem`, the one `Slots` over ring memory; fails without its order:
  `a_trb_is_handed_over_by_its_last_store`.

- [ ] **drv-o17** narrow: `arch/x86_64/ps2.rs` opens a separate `unsafe { inb(..) }` or `unsafe {
  outb(..) }` at every access to the i8042's two fixed ports, restating the same SAFETY argument —
  12 sites (`grep -c 'unsafe { inb\|unsafe { outb' kernel/src/arch/x86_64/ps2.rs`; the other 2 of
  14 unsafe blocks are ioapic installs), such as `kernel/src/arch/x86_64/ps2.rs:71`,
  `kernel/src/arch/x86_64/ps2.rs:95`, `kernel/src/arch/x86_64/ps2.rs:147`,
  `kernel/src/arch/x86_64/ps2.rs:179`. Four private fns `status() -> u8`, `read_data() -> u8`,
  `write_cmd(u8)` and `write_data(u8)` holding the only unsafe port accesses; `pulse_reset` stays
  `unsafe fn` for its machine-reset contract. In `kernel/src/arch/x86_64/ps2.rs`. Closes: no defect
  class; it cuts 12 SAFETY arguments to 4. Depends on: nothing. Lenses: ownership, opportunities.
  Value low, effort small. **Later.**

- [x] **drv-o18** narrow: `unsafe fn Counted::attach(&self, node: &ObjectRef)` stores a pointer into
  a DeviceNode's `IoStats` on the contract "registered in the device table, which holds it
  forever", but `register()` returns nothing, so callers cannot tell when the table dropped the node
  (drv-d17) — 3 sites (`grep -rn '\.attach(&' kernel/src | grep -v tests.rs`: ahci.rs:468,
  ramdisk.rs:365, storage.rs:1085; device.rs:1291 is in a test), such as
  `kernel/src/drivers/ahci.rs:468`, `kernel/src/io/ramdisk.rs:365`,
  `kernel/src/drivers/xhci/storage.rs:1085`. Make `attach` safe: it checks the type as it already
  does and pins the node with its own reference (`node.clone().into_raw()`, never released), so the
  counters outlive every `record()` whatever the registry did; alternatively `register()` returns
  `bool` and `attach` takes the registry's proof. In `kernel/src/io/stats.rs`. Closes: a dangling
  `IoStats` pointer when registration failed under memory pressure. Depends on: nothing. Lens:
  opportunities. Value low, effort small. **Fix.**
  Fixed in `9e422204`: `Counted::attach` is safe and pins its node; fails without it:
  `attaching_pins_the_node_its_counters_are_in`.

What held up: IrpBox lifetime. `_po`, `_buffer` and `_device` pin every raw pointer in the Irp;
`irp_complete_dpc` signals the PO and links the box onto the allocation-free RECLAIM list as its
last access; `reclaim_completed` frees it in thread context exactly once; `run_pending_in` clears
`queued` before the handler and never touches the Dpc after it. The partition Dekker pair
(`entering` / `retired`, SeqCst on all four accesses) is host-tested through `partition_submit`.
Fragment bounds hold on every path: `dispatch_block_irp*` refuse `max_frags` overruns before the box
exists, `sys_io_submit` bounds `buf_offset + length` by the page-rounded `MemoryObject::size()`,
AHCI's `MAX_PRDT_ENTRIES` is derived from `CMD_TABLE_BYTES` and USB storage clamps to `MAX_FRAGS`.
`read_blocking` checks `pending_op_is_signaled` before trusting status 0. Parsers hold:
`partitions.rs` (entry size 128..=512 dividing the block, capped at 32 blocks, checked counts,
`fits()`), the xhci `desc.rs` walkers (every `bLength` bounded by `wTotalLength` and the bytes
received, stop at zero), the HID report-descriptor walk (checked `mul`/`add`, `REPORT_BITS_LIMIT`,
bounded push stack, reads through `get`), and PCI (capability walk masked to 0xFC and slot-bounded,
`read_msi` refuses structures past 0x100). The AHCI FAULTED/RECOVERING protocol keeps a command
that was really in flight retired exactly once, and a port that will not stop keeps its command and
buffers (drv-d1 is the one path that bypasses it, for an idle slot). xHCI init boxes the controller
before RUN, `hub::release` and `storage::finish` disable the slot before freeing or completing, and
`hub::wait` / `run_once` take the record back under its lock or wait out the DPC's completion.
Epoch-encoded USB disk and HID node contexts are refused under the device lock once a slot is
reused, and `depart` removes table entries under their locks before the memory goes. The
`input::Reader`, console and PS/2 lights hand-offs never drop an `ObjectRef` in a DPC. Lock ranks
hold: no leaf lock is held across `complete_pending_op` or `signal_interrupt`, and
`bind_partition_names` releases PARTITIONS before `Namespace::bind`. `device.rs` `announce` /
`read_changes` lose no wakeup. `Xhci` is shared across CPUs without `Sync`, but `dcbaa` is written
only by the hub thread and the scratchpads and ERST never after init, so no race. The tests caught
the breaks in drv-d1 through drv-d8, drv-d14, drv-d18 and drv-d21, and the boot caught drv-d4.

### kernel-platform — main at 135055a8, 2026-10-26

Lenses: abi, ownership, soundness, concurrency, opportunities; returned nothing: none.
Defects: found 35, refuted by a boot 0, reported 22 (13 duplicates merged). Opportunities: 15 (34 proposed, merged by pattern).

The inventory, `cargo xtask unsafe-inventory` over its paths:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `kernel/src/arch` | 4299 | 216 | 141 | 0 | 12 | 0 | 6 | 18 | 1 | 54 | 7 |
| `kernel/src/main.rs` | 756 | 57 | 1 | 0 | 9 | 0 | 0 | 17 | 0 | 0 | 0 |
| `kernel/src/lib.rs` | 26 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/limine.rs` | 322 | 2 | 0 | 10 | 0 | 0 | 0 | 0 | 0 | 0 | 2 |
| `kernel/src/boot_selftest.rs` | 419 | 8 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/cmdline.rs` | 63 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/handoff.rs` | 68 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/initramfs.rs` | 77 | 1 | 1 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| `kernel/src/fbcon` | 730 | 4 | 5 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| `kernel/src/framebuffer.rs` | 39 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/klog.rs` | 158 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `kernel/src/power.rs` | 97 | 7 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| `kernel/src/report.rs` | 86 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `total` | 7140 | 297 | 149 | 11 | 21 | 0 | 7 | 36 | 1 | 54 | 10 |

**Defects**

- [x] **kplat-d1** `kernel/src/arch/x86_64/idt.rs:158` — No interrupt or exception entry stub
  normalises RFLAGS.DF or AC (no `cld`/`clac`), so an entry from ring 3 runs Rust code with
  whatever DF/AC the user left set. A ring-3 thread runs with DF set when the timer, a device
  IRQ or an exception arrives. `timer_dispatch` and `device_irq_dispatch` reach
  `dpc::run_pending` and `memset`, and `pf_dispatch` reaches `AddressSpace::fault_in` and
  `memset`. The kernel's `rep movs`/`rep stos` then run backwards and write below their
  destination. With AC set, SMAP is off for the whole handler. Only the syscall path is
  covered, by SFMASK. All six hand-written prologues (idt.rs:158, 224, 348, 410, 451, 549) are
  alike; objdump shows no `cld` in `memcpy`/`memset`.
  Demonstrated by boot: init patched to run `std` then spin, debug image under QEMU. After
  `init: up (pid 1)` the kernel took `#PF`, rip 0, cs 0x8, rflags 0x10446 (DF set), r12 =
  the memcpy/memset address. The unpatched image boots. Any ring-3 process can crash the
  kernel. Fix: `cld; clac` at the top of every stub (see kplat-o9). concurrency. High.
  **Fix**, first.
  Fixed in `ebb1c277`, and in `a57a676b` with no SMAP needed: every interrupt and exception stub
  opens with `entry_prologue!`, which opens with `clear_entry_flags!` (`pushfq; and; popfq`);
  fails without it: `check-irq-scope`'s third rule, on the original `idt.rs` at all six stubs,
  `an_interrupt_stub_opens_with_a_prologue_that_clears_df_and_ac`, and
  `the_entry_flag_clear_leaves_df_and_ac_clear`.
- [x] **kplat-d2** `kernel/src/sched.rs:756` — `thread_enter`'s ring-3 descent calls
  `set_kernel_stack`, `set_syscall_kernel_stack` and `arm_user_entry_cpu_base` (gdt.rs:249,
  syscall.rs:98, 114) with IF=1 and preemption on. Each reads the CPU index with RDTSCP, then
  writes that CPU's slot (TSS.RSP0, `CpuLocal.kstack_top`, `KERNEL_GS_BASE`). New user thread T
  reads cpu=A. A tick preempts it, A switches to T2 and arms A's RSP0 for T2, and idle CPU B
  steals T. T resumes on B and writes A's slot (or loads A's block into B's `KERNEL_GS_BASE`).
  T2's next ring-3 interrupt on A, or T's next syscall on B, then pushes onto T's live kernel
  stack, or two CPUs share one `rsp_scratch`/`kstack_top`. The SAFETY argument at
  syscall.rs:115-117 ("other CPUs touch only their own slots") does not hold. Kernel-stack
  corruption reachable by any process that creates threads, but the window is a few
  instructions per first run. The same hazard class is already fixed by masking IF in
  `stop_other_cpus` (cpu.rs:55-66). `arm_kernel_stack_for` (under SCHED, IF=0) and
  `ap_cpu_init` are safe.
  Unverified: no host test reaches RDTSCP/WRMSR. A seam for `rdtscp_aux` in a host test, or
  masking IF across the descent and test-qemu at `-smp 4` under thread churn, would settle
  it. Fix: do the three writes inside `thread_enter`'s SCHED-held block (see kplat-o3).
  ownership, concurrency. High. **Fix.**
  Fixed in `11128592`: the first descent arms under `SCHED`; fails without it: the setters'
  debug assertion, which panicked `test-qemu` at the first user thread.
- [x] **kplat-d3** `kernel/src/arch/x86_64/idt.rs:662` — A vector-2 NMI (IST2) or a #DF (IST1)
  taken in ring 3 goes through `is_user_fault` to `user_fault` and `suspend_with_fault`. That
  parks the thread with its ExceptionFrame and context on the per-CPU IST stack, not on its own
  kernel stack, which breaks the claim at sched/exit.rs:301 ("the ExceptionFrame stays on this
  frozen kernel stack until resume"). The next NMI on that CPU resets RSP to the same IST2 top
  and overwrites the parked frames. A resume then returns through corrupted frames, possibly
  on another CPU while the first reuses the stack, and `sys_thread_get_registers` reads the
  clobbered frame, possibly another process's user registers. Latent today because only stop
  NMIs are sent and LINT1 is never programmed. An NMI watchdog or a stop NMI sent to a CPU
  that is not stopping would make it live.
  Demonstrated by boot: debug image, init spinning in ring 3, a temporary probe sending an NMI
  IPI to its CPU. Log: `exc2 cs=0x23 ... stopping=false`, then `user_fault vec=2 frame_ptr=
  0xffffffff800d1f10`, which is inside IST2, not a 0xffffc000... thread stack. A second NMI
  on that CPU entered at the same rsp. Probes reverted. abi, opportunities. Medium.
  **Fix**, as sched-d4.
  Fixed in `21059535`, as sched-d4.
- [x] **kplat-d4** `kernel/src/arch/x86_64/idt.rs:692` — `ArchRegisters::read_from_exception_frame
  (frame_ptr: usize)` is a safe trait fn that dereferences any address as an ExceptionFrame.
  Its one caller, `sys_thread_get_registers` (syscall/table.rs:611-614), gets the address from
  `thread_exception_frame` under SCHED, releases SCHED, then reads with no lock. The SAFETY
  claim "the thread stays parked while suspended" holds only at the instant SCHED was held. A
  process with SIGNAL on its own suspended thread T runs two threads. One calls
  `sys_thread_get_registers(T)` and gets the address. The other calls `sys_exception_resume(T)`
  in the window. T iretqs, and its next syscall or trap reuses that stack. The copy-out is
  then a data race and hands user memory kernel return addresses and stack words. The stack
  stays mapped (the Thread owns it), so this is a stale read, not a use-after-free. The
  ModifyAndResume writeback assigned to this trait (decision-log.md:2933), written the same
  way, would write user-chosen values onto a live kernel stack.
  Demonstrated: host test in sched/tests.rs (removed). `resume_suspended` returned true after
  `thread_exception_frame` gave the frame, the test overwrote words 14 and 17 as the next entry
  would, and `read_from_exception_frame` returned rax=0xffffffff80123456
  rip=0xffffffff80001000 (`probe_get_registers`, 1 passed). abi, ownership, concurrency,
  opportunities. Medium. **Fix**, as sched-d1.
  Fixed in `3d1174b5`, as sched-d1.
- [x] **kplat-d5** `kernel/src/mm/user_access.rs:180` — `copy_from_user<T: Copy>` and
  `copy_to_user<T: Copy>` are safe fns that accept any `Copy` type. `copy_from_user`
  `assume_init`s user bytes as T, so a T with invalid bit patterns (bool, enum, char,
  reference) is undefined behaviour. `copy_to_user` of a padded T copies uninitialised kernel
  padding to user memory. Latent: every type passed today (RegisterValues, HandleInfo, NsEntry,
  Notification, RawHandle, usize, ThreadArgs, SpawnArgs, IoOp) is padding-free with all bit
  patterns valid. Adding a `bool` or enum field to ThreadArgs, SpawnArgs or IoOp would make a
  user-chosen byte 0x02 an invalid value in the kernel.
  Demonstrated: Miri (nightly-2026-07-20) on a probe test (removed) reading
  `copy_from_user::<bool>` of byte 0x02: "constructing invalid value of type bool: encountered
  0x02" at user_access.rs:199. Native run printed "read as false". abi. Medium. **Fix**, as kobj-d6.
  Fixed in `efb315e2`, as kobj-d6.
- [x] **kplat-d6** `kernel/src/main.rs:145` — The kernel command line is recorded as a
  `&'static [u8]` into Limine's bootloader-reclaimable memory (`c_bytes` returns an unbounded
  lifetime, main.rs:543), and `/proc/cmdline` serves it on every lookup. The `'static` holds
  only because the buddy takes `MEMMAP_USABLE` alone. Triggering change: reclaiming
  `MEMMAP_BOOTLOADER_RECLAIMABLE` once the kernel owns its stack (decision-log ~192). The frame
  is then handed out again, and `kernel_server.rs:163` copies `cmdline::line()` into a
  MemoryObject, so a process resolving `/proc/cmdline` gets up to 1024 bytes of whatever lives
  there. The same convention covers `limine_framebuffer()`'s `&'static Framebuffer`
  (main.rs:615), the bootloader name and version strings, and the parked APs past MAX_CPUS
  spinning on `goto_address` in Limine's memory.
  Demonstrated: Miri on a probe test in cmdline.rs (removed): `c_bytes` over a Vec, `record()`,
  drop the Vec, `line().to_vec()` gave "constructing invalid value of type &[u8]: encountered a
  dangling reference (use-after-free)" at cmdline.rs:52 (`probe_recorded_line`). ownership.
  Medium. **Fix.**
  Fixed in `018a00a4`: `cmdline::record` copies the line, up to 1024 bytes, into the kernel's own
  memory; fails without it: `the_kept_line_is_a_copy_cut_at_its_bound`, whose shape run on the old
  `record`, given `c_bytes`'s unbounded lifetime, read the overwritten source. The framebuffer's
  layout and the module paths were already copied. Limine's page tables, the stacks the
  processors started on and the APs parked past `MAX_CPUS` still live in its memory, which
  boot-flow.md now says.
- [x] **kplat-d7** `kernel/src/arch/x86_64/paging.rs:482` — The page-table walkers (`translate`,
  `attribute_index`, `walk_to_pt`) read PTEs with plain non-atomic loads, and `map_page`,
  `unmap_page` and `ensure_kernel_intermediate` write leaf and PML4 slots with plain stores,
  while the kernel half is edited lock-free and `ensure_table` links with a CAS (paging.rs:255).
  A plain access racing the CAS on one slot is a data race, which is undefined behaviour in
  Rust's model though x86 hardware does not tear an aligned 8-byte access. Two CPUs creating
  kernel stacks in a fresh 2 MiB vmap region race a concurrent walk (fault-path
  `scan_stack_candidates`/`dump_return_addresses`, `of_mapping`). A more aggressive compiler or
  another architecture could duplicate or tear the read.
  Demonstrated: Miri on a probe test (removed). One thread ran `map_page`, the main thread ran
  `translate` on a fresh root. "Data race detected between non-atomic read (translate,
  paging.rs:482) and atomic read-modify-write (ensure_table compare_exchange, paging.rs:255)".
  Native run passed. soundness, opportunities. Medium. **Fix.**
  Fixed in `991a8726`: with kmem-o11, every entry is read and written atomically through a
  `TableRef`; fails without it, under Miri:
  `a_walk_beside_a_mapper_in_one_tree_reads_what_is_linked`, a data race at `translate` against
  `ensure_table`'s CAS.
- [x] **kplat-d8** `kernel/src/arch/x86_64/smp.rs:95` — `ArchSmp::init_this_cpu(index: u32)` is
  safe and writes any value to IA32_TSC_AUX. Per-CPU accesses then index `static mut` arrays
  with `.add(idx)` guarded only by a `debug_assert`, which release builds drop (gdt.rs:143-146,
  255; syscall.rs:69). A caller passing an index of MAX_CPUS (8) or more, for example one that
  took Limine's `SmpInfo.processor_id` as the dense index (limine.rs:353 documents it as "the
  logical CPU index Nitrox uses") on a machine with sparse ACPI ids, makes `gdt::init`,
  `set_kernel_stack` and `set_syscall_kernel_stack` write past `TSSES`/`GDTS`/`CPUS`, and the
  next syscall loads RSP from an out-of-bounds block. The only caller passes 0 (main.rs:265),
  and the AP path's `adopt_dense_index` is below MAX_CPUS by construction.
  Demonstrated by path, read end to end. No probe was run. opportunities. Medium. **Fix.**
  Fixed in `ecb228ae`: `init_this_cpu` refuses an index past `MAX_CPUS`; fails without it:
  `an_index_past_max_cpus_is_refused`.
- [x] **kplat-d9** `kernel/src/arch/x86_64/user_access.rs:109` — `copy_cstr_raw`'s loop runs
  `lodsb`/`stosb` before it compares the count with `max_len`. With `max_len` = 0 it writes one
  byte into a destination its contract says has zero writable bytes, then reports
  NoTerminator (or Ok(1) if that byte was NUL). The `cfg(test)` stub writes nothing, so host
  tests cannot see it. Latent: the only caller, `copy_cstr_from_user`, returns early on an
  empty buffer (mm/user_access.rs:257). A new caller of `UserAccess::copy_cstr` with an empty
  destination gets a one-byte kernel overrun.
  Demonstrated: host probe running the same instruction sequence without stac/clac and the
  extable entry (removed). dst=[0xAA,0xAA], src="x\0", max_len=0 gave code 2, count 1, dst
  [78, aa]. The stub gave [aa, aa]. A second probe gave `backing=[ee, 00, ee, ee]` with
  `result=0 count=1`. abi, opportunities. Low. **Fix.**
  Fixed in `efb315e2`: `ArchUserAccess::copy_cstr` takes its destination as a slice and refuses an
  empty one before the routine, and the host stub runs the routine's loop byte by byte. Fails
  without it: `a_string_copy_into_an_empty_destination_writes_nothing` reads `[0x78, 0xAA]`.
- [x] **kplat-d10** `kernel/src/framebuffer.rs:52` — `record_aperture` records
  `phys_base = address - hhdm` with no page-alignment check, but
  `MemoryObject::try_new_borrowed` requires a page-aligned base (memory_object.rs:100). The
  SAFETY comment at kernel_server.rs:330-334 never mentions alignment. Firmware reporting a
  sub-page framebuffer base makes the first `/dev/framebuffer` map reach `unreachable!` in
  `map_object` (mm/addr_space.rs:361) with the address-space lock held, and panics the kernel
  instead of refusing.
  Demonstrated: host probes (removed) with address 0x1000_0800 (hhdm 0) and 0x8000_0800:
  `record_aperture` returned true, `try_new_borrowed` succeeded, and `map_object` panicked
  "ArchPaging::map_page returned Misaligned for a page-aligned per-page address". abi,
  ownership. Low. **Fix.**
  Fixed in `66583f92`: `record_aperture` refuses a base inside a page, and returns its reason, which
  the boot logs. Fails without it:
  `an_aperture_is_recorded_only_from_a_page_aligned_base_in_the_hhdm` records a base a byte below a
  page.
- [x] **kplat-d11** `kernel/src/arch/x86_64/gdt.rs:18` — On the BSP, `gdt::init` (via
  `init_tables`, main.rs:117) indexes `GDTS`, `TSSES`, `DF_STACKS` and `NMI_STACKS` by
  `rdtscp_aux` before `init_this_cpu(0)` writes TSC_AUX (main.rs:265), so the SAFETY arguments
  rest on firmware leaving the reset default 0, with only a `debug_assert` on the bound. With
  TSC_AUX left at k in 1..7, the BSP loads slot k's GDT and TR while `set_kernel_stack` later
  writes slot 0's RSP0, so ring-3 interrupts land on RSP0=0. AP k later rewrites the TSS the
  BSP has loaded, and its own `ltr` of a busy descriptor may fault (not run). At k >= 8 a
  release build writes past the arrays. syscall.rs:62-70 has the same pattern but runs after
  `init_this_cpu(0)`.
  Unverified: QEMU resets TSC_AUX to 0 in every boot run. Writing TSC_AUX=0 at the top of
  `kernel_main` and booting firmware that leaves it nonzero, or a probe that writes it before
  `init_tables`, would settle it. abi, ownership, concurrency. Low. **Fix.**
  Fixed in `ecb228ae`: the BSP writes index 0 first thing; shown by a boot with `TSC_AUX` left at
  3, which timed out after a kernel #PF with the old order and passes with the fix.
- [x] **kplat-d12** `kernel/src/main.rs:775` — `plain_mapping` returns None through `.ok()?`
  when `map_page` fails at page k > 0 and does not unmap pages 0..k. `report_framebuffer_cost`
  unmaps only on Some (main.rs:733-737), so part of the second alias of the aperture, with
  different caching from the console's mapping, outlives the measurement, which the comment at
  main.rs:731-732 says must not happen. The vmap VA is never reused, so it stays for the life
  of the system. Reachable only on page-table-frame OOM during boot.
  Unverified: by path, read end to end; a fault-injected `map_page` in a boot would settle it
  (main.rs is the binary and has no host tests). abi, ownership, opportunities. Low. **Fix.**
  Fixed in `165dae67`: `plain_mapping` is `kvmap::map_device`, whose `map_all` undoes a partial
  map; fails without the undo: `a_range_that_runs_out_of_page_tables_is_undone`, which drives
  `map_all` through `VmapFrames` rather than `map_device`.
- [x] **kplat-d13** `kernel/src/main.rs:493` — The SAFETY arguments for the Limine response
  reads (main.rs:55, 493-496, 1108-1111) say a raw-pointer copy, or `static mut` itself, stops
  the optimiser folding the pre-Limine null. Neither does. The read survives only because each
  request is `#[used]`, and reading memory written outside the program is the job of a
  volatile read (as `BaseRevision::supported` already does). A request static without
  `#[used]`, or a toolchain that treats used globals differently, folds `response` to null and
  the kernel reports "no Limine ... response" on a boot where Limine answered. 13 read sites
  use the pattern, and main.rs:406 reads the field directly without it.
  Demonstrated: `rustc -O` on a `static mut` in `.limine_requests` read with
  `(&raw const R).read().is_null()`. With `#[used]` it emits `cmpq $0, R(%rip)`. Without it
  emits `movb $1, %al`. abi. Low. **Fix.**
  Fixed in `018a00a4`, with kplat-o2: `Request::response` reads volatile. Shown by `rustc -O` with
  no `#[used]`: the volatile read stays `cmpq $0, A+40(%rip)`, where the old raw-pointer copy and a
  `Relaxed` atomic load both fold to `movb $1, %al`.
- [x] **kplat-d14** `kernel/src/arch/x86_64/ioapic.rs:217` — `X86IoApic::route` casts the 32-bit
  x2APIC destination with `dest as u8`, silently truncating, where its MSI sibling
  `irq_install::compose` refuses ids above 0xFF (test
  `a_destination_wider_than_the_field_is_refused_rather_than_truncated`). On a machine whose
  boot CPU's x2APIC id is 256 or more, `install_intx` and `install_isa_irq` (ioapic.rs:294,
  irq_install.rs:50) route the line to `id & 0xFF`, another CPU or none, and the device's
  interrupts are lost instead of the install being refused.
  Unverified: QEMU boots use APIC id 0 or 1; it needs a machine with an x2APIC id of 256 or
  more. abi. Low. **Fix.**
  Fixed in `a53111c5`: `route` names the destination with `u8::try_from` and says whether it
  routed, and `install_intx` and `install_isa_irq` ask before taking a vector and return
  `Option<u8>`: AHCI declines, the console leaves RX unarmed, PS/2 polls. Fails without it:
  `a_destination_wider_than_the_entry_is_refused_rather_than_truncated`. Boot probes drove each
  refusal through its caller. Still unverified on a boot CPU with an x2APIC id of 256 or more.
- [x] **kplat-d15** `kernel/src/klog.rs:237` — `copy_into_frames(frames: &[PhysAddr])` is a safe
  pub fn that writes the log through the HHDM into whatever frames it is given; its SAFETY
  comment (klog.rs:253-255, "within an owned, HHDM-mapped frame") is a claim about the caller
  that nothing enforces. Latent: the single caller (kernel_server.rs:306) passes a fresh
  MemoryObject's own frames. A caller passing a borrowed object's frames (the framebuffer
  aperture), a shared MemoryObject's, or any computed PhysAddr overwrites that memory from safe
  code. Taking `&MemoryObject` (`copy_in`, memory_object.rs:174) would remove klog's only
  unsafe block.
  Demonstrated: host probes (removed). A `Box<[u8; 4096]>` borrowed only immutably, its
  PhysAddr passed with no unsafe in the test, had 11 log bytes written into it
  (`victim[0..4]` = [112, 114, 111, 98]; `victim[0] != 0xAB`). ownership, opportunities. Low.
  **Fix.**
  Fixed in `5ac6215f`: `klog::copy_into_object(&MemoryObject)` writes through `copy_in`, and
  klog.rs has no unsafe left; fails without it: `the_log_is_copied_into_its_object_cut_at_its_size`
  (a cap past the object, or every run at offset 0).
- [ ] **kplat-d16** `kernel/src/main.rs:874` — `run_first_userspace` allocates pid-1
  handle-table entries (`notif_h`, then `ns_h` and `system_control_h`) as raw u64s, then returns
  on twelve later failure paths (main.rs:877, 888, 894, 900, 934, 943, 949, 962, 972, 980,
  1001, 1043) without closing them. Each entry keeps its object (notification channel, root
  namespace, system-control object) referenced forever. A boot-time OOM in `Namespace::try_new`,
  any `bind_kernel_server`, or `spawn_user` leaks them to a process that will never exist.
  Unverified: by path (`notif_h` at main.rs:863-871, and the early returns read). Reachable
  only by a boot-time OOM or bind failure, not injected. ownership. Low. **Plan**, in P1.
- [x] **kplat-d17** `kernel/src/arch/x86_64/ioapic.rs:55` — IOAPIC registers use an indirect
  IOREGSEL-then-IOWIN protocol and `set_rte_mask` is a read-modify-write, but nothing
  serialises them, and the `ArchIrqRouter` route/mask/unmask contracts do not ask callers to
  (7 call sites: 130, 137, 190, 199, 200, 221, 222). Correctness rests on every caller running
  at boot on one CPU. Latent: all routes happen in `drivers::probe` on the BSP before SMP, and
  mask/unmask have no caller outside ioapic.rs. A driver that masks its level-triggered INTx
  line in its ISR and unmasks it in a DPC, or a post-SMP install, would interleave two CPUs'
  IOREGSEL writes so one's IOWIN lands in the other's register. No lock appears in
  kernel/docs/lock-ordering.md.
  Unverified: no host seam for the MMIO; a host test with a fake register pair driven in the
  interleaved order would settle it. concurrency. Low. **Fix.**
  Fixed in `b3040217`: the registers are an `IoApic` in `IOAPIC: IrqSpinLock<Option<..>>`, a leaf,
  reached only through its guard; a route writes its entry, and a mask its read-modify-write, in
  one hold. Structural, so no test drives the interleaving; the rewritten entry bound has
  `an_ioapic_serves_its_gsis_and_neither_neighbour`.
- [x] **kplat-d18** `kernel/src/tlb.rs:96` — `shootdown` reads `me` and `online` before
  `preempt_disable` (line 113), then takes the sole-online-CPU fast path and invalidates
  "locally". If the thread migrates in between, it invalidates the new CPU's TLB and the CPU
  that holds the stale entry is never told. While APs come online, a BSP thread (for example
  `reap_pending` dropping a KernelStack) reads online={0}, is preempted, AP1 comes online and
  steals it, and the BSP keeps its translation of a page the caller then frees. The window
  exists only while the online set grows (bring-up, any future hotplug).
  Unverified: needs a host seam for the CPU id; moving the reads after `preempt_disable`
  would settle it. concurrency. Low. **Fix.**
  Fixed in `dc2ad6ee`: preemption goes off before the shootdown reads its CPU; fails without it:
  `pinned_cpu`'s assertion, in the host tests.
- [x] **kplat-d19** `kernel/src/fbcon/mod.rs:447` — `with_console_waiting`, `yield_to_userspace`
  and `with_console` compute `me = current_cpu()+1` (fbcon/mod.rs:388-390, 446-450, 495-505)
  before taking CONSOLE, which masks IRQs. A thread migrated in between stores another CPU's
  number in HOLDER while it holds the lock. A `kprintln` on that other CPU then sees
  HOLDER==me and drops its line from the grid that `reclaim_for_stop` repaints as the
  diagnosis, and a fault inside the console on the new CPU waits PATIENCE spins instead of
  taking the re-entry shortcut. No memory is at stake.
  Unverified: needs a host seam for `current_cpu()`; reading the CPU after the lock is held
  would settle it. concurrency. Low. **Fix.**
  Fixed in `dc2ad6ee`: `HOLDER` is read only with interrupts masked; fails without it:
  `this_holder`'s assertion, which panicked `test-qemu` through `with_console_waiting`.
- [x] **kplat-d20** `kernel/src/arch/x86_64/serial.rs:50` — `SerialPort::new(base: u16)` is a
  pub safe `const fn`, and `init`/`write_byte`/`read_byte`/`set_loopback`/`enable_rx_interrupt`
  do `outb`/`inb` at base+0..7 for any base. The SAFETY arguments ("the COM1 base port, which
  this driver exclusively owns") rely on base == COM1_BASE, which nothing enforces.
  `arch/mod.rs:45` re-exports the module, so code outside the arch layer can call
  `SerialPort::new(0xCF8).init()` and do port I/O on the PCI configuration mechanism with no
  unsafe block. No code outside serial.rs uses `SerialPort` today.
  Demonstrated by path, read end to end; grep outside serial.rs found no use. opportunities.
  Low. **Fix.**
  Fixed in `08f925b0`: `SerialPort` holds nothing and only serial.rs makes one;
  `SerialPort::new(0xCF8)` and `SerialPort(())` from drivers/ no longer compile (E0599, E0603).
- [x] **kplat-d21** `kernel/src/arch/x86_64/syscall.rs:114` — `set_syscall_kernel_stack(top: u64)`
  and `ArchCpu::set_kernel_stack(top: u64)` (arch/cpu.rs:23, TSS.RSP0) are safe, yet the value
  becomes the stack pointer the hardware loads on the next syscall or ring-3 trap
  (syscall.rs:194, gdt.rs:248-258). Validity (a live, mapped kstack top that outlives the
  thread's time on this CPU) is held only by sched's `arm_kernel_stack_for`. Latent: all four
  call sites (sched.rs:506, 507, 756, 757) pass `Thread::kstack_top` of a pinned thread. A
  caller that "disarms" with `set_kernel_stack(0)` gets a #DF on the next ring-3 interrupt. One
  that arms a top whose KernelStack can drop while the thread is current makes the next trap
  push its frame into freed and reused frames, triggered from ring 3.
  Demonstrated by path, read end to end. opportunities. Low. **Fix.**
  Fixed in `7be20327`: both setters take a `KernelStackTop`, which only a `KernelStack` makes;
  structural.
- [x] **kplat-d22** `kernel/src/arch/x86_64/regs.rs:339` — `cli`, `sti` (regs.rs:339, 353),
  `halt_loop`'s `cli;hlt` (cpu.rs:106) and `idle_halt`'s `sti;hlt` (cpu.rs:128) are
  `asm!(..., options(nomem))`, which lets the compiler move or merge memory accesses across the
  points where interrupts are masked or unmasked. An IF-only critical section around a plain
  (non-atomic) access, such as a plain RMW of per-CPU state (preempt.rs:135-138 warns against
  that but reasons as if the mask would bracket it), is then not a critical section. Latent: the
  current IF-only sections (lockrank, preempt) use atomics, and `IrqSpinLock` data sits behind
  Acquire/Release, so no current site was found affected.
  Demonstrated: standalone probe compiled `-O` for x86_64-unknown-none. With `nomem`,
  `window_nomem` read `*p` once before `cli` and reused it in the window (`movq (%rdi),%rax;
  cli; leaq 1(%rax),%rcx; movq %rcx,(%rdi); sti`). With the memory clobber, `window_mem`
  reloaded `*p` after `cli`. opportunities, soundness. Low. **Fix.**
  Fixed in `6d8f188c`: no `cli`, `sti` or `hlt` is `nomem` (with `halt`'s, swept); fails without
  it: `no_cli_sti_or_hlt_is_marked_nomem`.

**Opportunities**

- [ ] **kplat-o1** encapsulate: page tables reached as `root: PhysAddr` and `table_ptr(phys) ->
  *mut Pte`, every slot read and written with plain loads and stores except `ensure_table`'s CAS,
  and tree validity a caller obligation — 71 sites (`grep -rn 'Paging::\(map_page\|unmap_page\|
  translate\|attribute_index\|ensure_kernel_intermediate\|inherit_kernel_mappings\|
  set_page_table\)' kernel/src | grep -v '//'`, 36 of them translate/attribute_index; 19 PTE
  accesses in paging.rs before its test module, counted by `grep -nE '\*table_ptr\(|\*pml4\.add|
  \*leaf|...'`), such as `kernel/src/arch/x86_64/paging.rs:482`, `paging.rs:375`, `paging.rs:462`,
  `kernel/src/mm/kvmap.rs:84`. A `#[repr(transparent)] struct Table([AtomicU64; 512])` with
  safe `entry(&self, Index9) -> Pte`, `set` and `link_or_get`, behind one `unsafe fn table<'a>
  (phys) -> &'a Table`, and typed roots (`Root::active()`, `AddressSpace::root()`) so
  `translate` and `attribute_index` become safe. The atomic slot cell alone is medium effort;
  typed roots make it large. Closes: the CAS-versus-plain-read data race (kplat-d7), a dangling
  root, an out-of-range slot index. Depends on: nothing. soundness, opportunities. Value high,
  effort large. **Plan**, in P3.
- [x] **kplat-o2** encapsulate: every Limine request read by hand (`(&raw const X).read()
  .response`, null check, `&*`, raw `.add(i)` array walks, `c_bytes`/`CStr::from_ptr` strings,
  each with its own SAFETY text) plus ten needless `unsafe impl Sync` on `static mut`
  request statics — 56 sites (36 unsafe blocks counted by awk over main.rs:400-640 and
  1048-1135 less `init_buddy`, acpi.rs:495, 501 and mm/buddy.rs:324, 328; plus `grep -c 'unsafe
  impl Sync' kernel/src/limine.rs` = 10 and `grep -rn 'static mut .*Request' kernel/src` = 10),
  such as `kernel/src/main.rs:406`, `main.rs:497`, `main.rs:1112`, `kernel/src/arch/x86_64/
  acpi.rs:495`, `kernel/src/limine.rs:101`. `#[repr(C)] struct Request<R, X = ()> { id,
  revision, response: AtomicPtr<R>, extra }` (auto-Sync, no `static mut`, no `unsafe impl`)
  with `fn response(&self) -> Option<&'static R>` doing one volatile/Acquire load. Response
  types get safe accessors (`MemoryMapResponse::entries()`, `SmpResponse::cpus()`,
  `ModuleResponse::modules()`, bounded `LimineFile::path()`/`bytes()`, bootloader name and
  version). An optional `BootToken<'b>` ties borrows to the boot phase so nothing Limine-owned
  is stored `'static`. Removing all ten `unsafe impl Sync` was built clean (cargo check lib and
  bins, default and `selftest,test-harness`, plus `cargo test --lib --no-run` on the host) and
  is the zero-dependency first step; the wrapper subsumes it. Decoding moves from the bin-only
  main.rs into the lib, where fabricated responses reach it under host tests and Miri. Closes:
  the folded-null read (kplat-d13), the `'static` into reclaimable memory (kplat-d6), the
  unbounded `CStr::from_ptr` on module paths (main.rs:1071), about 32 of main.rs's 57 unsafe
  blocks. Depends on: nothing. abi, ownership, soundness, opportunities. Value high, effort
  medium. **Fix.**
  Done in `018a00a4`: one `limine::Request<R, X>`, its response an `AtomicPtr`, with no `static
  mut` and no `unsafe impl Sync`; the responses' pointers private behind safe accessors that walk
  to the count and cut strings at a bound; the buddy's memory-map walks safe. Unsafe blocks in
  main.rs, acpi.rs, buddy.rs and limine.rs went from 100 to 66. Tests:
  `a_request_reads_the_response_limine_wrote`, `an_array_is_walked_to_its_count_past_its_nulls`
  (a walk that stops at a null, or one past the count, fails it). No `BootToken`: nothing kept past
  boot borrows Limine's memory.
- [ ] **kplat-o3** encapsulate: a CPU index read from hardware (`rdtscp_aux`/`Smp::current_cpu`)
  and then used to index per-CPU state, with migration between read and use left to each
  caller, the per-CPU arrays indexed through unchecked `.add(idx)`, and `init_this_cpu(u32)` a
  safe fn — 11 sites for the read-then-use pattern (`grep -rn "rdtscp_aux()\|
  Smp::current_cpu()" kernel/src --include=*.rs`, less comments, definitions and boot_selftest:
  tlb.rs:96, 183; sched.rs:315; lockrank.rs:259; gdt.rs:19; syscall.rs:63; online.rs:37;
  handle/mod.rs:106; fbcon/mod.rs:388, 447, 496) and 6 `.add(idx)` sites over 5 `static mut`
  arrays (gdt.rs:143-146, 255; syscall.rs:69), such as `kernel/src/arch/x86_64/gdt.rs:249`,
  `kernel/src/arch/x86_64/syscall.rs:63`, `kernel/src/fbcon/mod.rs:447`, `kernel/src/tlb.rs:96`.
  `struct CpuIndex(u8)` constructible only as `CpuIndex::BSP` or from `adopt_dense_index` (so
  `< MAX_CPUS` is a type invariant), `init_this_cpu(CpuIndex)`, `struct PerCpu<T>([UnsafeCell
  <T>; MAX_CPUS])` with `slot(&self, CpuIndex) -> *mut T`, and `with_mine<R>(&'static self,
  _off: &IrqsOff, f)` or a `Pinned<'a>` token obtainable only from an `IrqSpinLockGuard`, an
  interrupts-masked guard or a preempt guard, required by `set_kernel_stack`,
  `set_syscall_kernel_stack` and `arm_user_entry_cpu_base`. `thread_enter` then has to make
  the token, which is the fix. Home: kernel/src/arch (CpuIndex, PerCpu), guard constructors in
  libkern spinlock/preempt. Checked against `gdt::set_kernel_stack`, `syscall::this_cpu_block`,
  fbcon (read after CONSOLE is held) and `tlb::shootdown` (move the read under the preempt
  guard). Closes: a thread migrating between index read and write (kplat-d2, kplat-d18,
  kplat-d19), an out-of-range TSC_AUX (kplat-d8, kplat-d11), and 5 of the arch layer's 12
  `static mut`. Depends on: nothing. abi, ownership, concurrency, opportunities. Value high,
  effort medium. **Plan**, in P4.
- [x] **kplat-o4** narrow: a suspended thread's ExceptionFrame location carried as a bare
  `usize` (`Thread::exception_frame`) and dereferenced by a safe `ArchRegisters` fn after SCHED,
  the lock that proves the thread parked, is dropped — 6 sites (`grep -rn
  "exception_frame\|read_from_exception_frame\|frame_ptr" kernel/src --include=*.rs | grep -v
  tests`: made at idt.rs:675, stored at sched/exit.rs:308, handed out at sched/exit.rs:400-408,
  read at syscall/table.rs:611-613 via idt.rs:692-697; plus raw frame derefs at idt.rs:133, 255,
  504, 654 and syscall.rs:169), such as `kernel/src/arch/x86_64/idt.rs:675`,
  `kernel/src/sched/exit.rs:308`, `kernel/src/sched/exit.rs:407`, `kernel/src/syscall/table.rs:613`.
  `suspend_with_fault` stores decoded `RegisterValues` in the Thread under SCHED, and
  `suspended_registers(thread, &SchedGuard) -> Option<RegisterValues>` reads them under the lock.
  For ModifyAndResume, keep the frame pointer private to arch with `with_suspended_frame<R>(g:
  &mut SchedGuard, thread, f) -> Option<R>`. `irq_dispatcher!` can hand bodies `&mut
  ExceptionFrame` converted once. Home: kernel/src/sched/exit.rs and kernel/src/arch/
  registers.rs. Closes: reading, and later writing, a frame the thread has left (kplat-d4).
  Depends on: nothing. ownership, opportunities. Value high, effort small. **Fix.**
  Fixed in `3d1174b5`: as sched-d1: no frame address leaves the arch layer, `ArchRegisters` and its
  any-`usize` decoder are gone, and `irq_dispatcher!` hands its bodies a `&mut ExceptionFrame`
  converted once.
- [ ] **kplat-o5** encapsulate: kernel objects travel type-erased — adopted with `ObjectRef::
  from_raw(KBox::into_raw(x).as_ptr() as *mut (), KObjectType::T)`, downcast with `&*(r.as_ptr()
  as *const T)`, and passed to `unsafe fn f(obj: *mut ())` — 235 sites (downcasts `grep -rnE
  '&\*\(\w+(\.\w+)*\.as_ptr\(\) as \*(const|mut) [A-Za-z_:]+\)' kernel/src` = 105; type-erased
  unsafe fns `grep -rnE 'unsafe fn \w+(<[^>]*>)?\(\w+: \*mut \(\)' kernel/src` = 130; the one-line
  adopt form `grep -rn "ObjectRef::from_raw(KBox::into_raw" ... | grep -v "tests\|test_support"`
  = 50, within these), such as `kernel/src/main.rs:859`, `main.rs:1031`, `kernel/src/power.rs:151`,
  `power.rs:374`, `kernel/src/arch/x86_64/idt.rs:311`. `trait KObject { const TYPE:
  KObjectType; }`, safe `ObjectRef::adopt<T: KObject>(KBox<T>)` and `ObjectRef::downcast<T:
  KObject>(&self) -> Option<&T>` (checks `object_type()`, borrow tied to the reference), and
  pinned-pointer fns taking `&ObjectRef` or `Pinned<'a, T>`. Home: kernel/src/object/header.rs.
  Closes: a tag that does not match the allocation (which `dispatch_destroy` would free as the
  wrong type), a cast to the wrong concrete type, a raw pointer used after the ref that pinned
  it drops. Depends on: nothing; spans object and sched, seen here from power.rs, idt.rs and
  main.rs. ownership, soundness, opportunities. Value medium, effort large.
  **Plan**, as kobj-o1, in P1.
- [ ] **kplat-o6** encapsulate: physical frames written through `(phys + hhdm_offset()) as *mut
  u8` from a bare `PhysAddr`, ownership asserted only in a SAFETY comment — 44 sites (`grep -rn
  'hhdm_offset()' kernel/src` less comments, the definition and tests.rs; 19 of them write, by
  `... | grep "mut u8\|write_bytes"`; 10 in this subsystem), such as `kernel/src/klog.rs:252`,
  `kernel/src/object/memory_object.rs:150`, `kernel/src/arch/x86_64/paging.rs:217`,
  `kernel/src/arch/x86_64/acpi.rs:479`. An owned `Frame` from the buddy (freed on drop or
  `into_phys`) with safe `bytes()`/`bytes_mut()`, one `unsafe fn hhdm<T>(PhysAddr) -> *mut T`
  for borrowed cases (firmware tables, page tables), and `MemoryObject` exposing frames as views
  so klog's fill is `memobj.copy_in(off, run)` and `copy_into_frames` takes `&mut MemoryObject`.
  Home: kernel/src/mm. Closes: safe code writing a frame it does not own (kplat-d15). Depends
  on: nothing. ownership, opportunities. Value medium, effort medium. **Plan**, in P2.
- [ ] **kplat-o7** encapsulate: handle installation by `global::get().allocate(owner, raw_ptr,
  ty, rights)` after `ObjectRef::into_raw`, with a manual `drop(ObjectRef::from_raw(p, t))` on
  the error arm — 27 sites (`grep -rn "global::get().allocate(" kernel/src --include=*.rs |
  grep -v "tests\|test_support" | wc -l` = 26: main.rs 3, syscall/table.rs 23; plus
  handle/table.rs:589), such as `kernel/src/main.rs:863`, `main.rs:994`, `main.rs:1012`,
  `kernel/src/syscall/table.rs:589`. `fn install(&self, owner: u32, obj: ObjectRef, rights:
  Rights) -> Result<Handle, (HandleError, ObjectRef)>`: the table adopts the reference on
  success and returns it on failure. Home: kernel/src/handle/global.rs. Closes: a reference
  reclaimed twice or never on an allocate error path, a type tag that disagrees with the
  pointer (the class behind kplat-d16). Depends on: nothing. ownership. Value medium, effort
  medium. **Plan**, as kobj-o2, in P1.
- [ ] **kplat-o8** remove: `unsafe fn`s whose whole contract is "ring 0" and whose bodies cannot
  break memory safety (TLB flushes, `wbinvd`, masking interrupts, MTRR/PAT reads after a CPUID
  check, walking the always-valid active root); `ArchCpu::halt` has no callers — 24 sites
  (call sites by `grep -rnF`, less definitions and comments: `flush_tlb_page(` 7,
  `flush_tlb_all(` 2, `regs::invlpg(` 1, `regs::wbinvd(` 2, `MemoryTypes::at(` 1, `of_mapping(`
  1, `log_configuration(` 1, `interrupts_disable` 7, `regs::cli` 2), such as
  `kernel/src/arch/paging.rs:150`, `kernel/src/arch/cpu.rs:111`,
  `kernel/src/arch/memory_types.rs:75`, `kernel/src/arch/cpu.rs:85`. Make `flush_tlb_page`,
  `flush_tlb_all`, `invlpg`, `wbinvd`, `cli`, `interrupts_disable` and `MemoryTypes::{at,
  of_mapping, log_configuration}` plain fns, keep `unsafe` on `sti`, `interrupts_enable`/
  `restore` and `idle_halt` (they can break the IF=0 context-switch window), and delete
  `ArchCpu::halt`. Checked against tlb.rs:89, main.rs:777 and libkern/spinlock.rs:138. Home:
  kernel/src/arch/{cpu.rs,paging.rs,memory_types.rs} and arch/x86_64/regs.rs. Closes: about 16
  `unsafe fn` declarations and 24 call-site obligations whose SAFETY text is the vacuous "ring
  0", diluting review of the ones that matter. Depends on: `of_mapping` is cleaner after the
  typed-root part of kplat-o1; the rest on nothing. opportunities. Value medium, effort small.
  **Plan**, in P6.
- [x] **kplat-o9** encapsulate: the interrupt/exception entry prologue (error-code slot, vector,
  15 GPR pushes, `mov rdi, rsp`, call, unwind, iretq) written out six times — 6 sites (`grep -c
  'push rax\\npush rbx' kernel/src/arch/x86_64/idt.rs` = 6: `exception_stub!`, vec14,
  `timer_stub`, `tlb_shootdown_stub`, `reschedule_ipi_stub`, `device_irq_stub!`), at
  `kernel/src/arch/x86_64/idt.rs:158`, `idt.rs:224`, `idt.rs:348`, `idt.rs:549`. One
  `entry_stub!(name, vector, errcode: cpu|dummy, dispatch = path)` macro emitting the shared
  prologue (with `cld; clac` once) and epilogue; the copies differ only in the error-code push,
  vector immediate and dispatcher symbol (checked against `timer_stub`, vec14 and
  `device_irq_stub`). Home: kernel/src/arch/x86_64/idt.rs, beside `irq_dispatcher!`. Closes: an
  entry path missing a flag normalisation (kplat-d1 lives in all six copies). Depends on:
  nothing. concurrency. Value medium, effort small. **Fix.**
  Finished in `ebb1c277`: `entry_prologue!` and `entry_epilogue!`, the six stubs' frames.
- [x] **kplat-o10** narrow: user copies bounded only by `T: Copy`, byte reinterpretations of
  `#[repr(C)]` ABI types held sound by convention, and raw copy primitives taking the kernel
  side as `(ptr, len)` — 13 sites for the type bound (`grep -n 'copy_from_user(\|
  copy_to_user(' kernel/src/syscall/table.rs`, 3 + 8; `grep -n 'as \*const Self as \*const u8'
  kernel/src/libkern`: framebuffer.rs:75, block.rs:138) plus 8 for the primitives (`grep -rn
  'UserAccess::copy_bytes\|UserAccess::copy_cstr' kernel/src`: 5 in mm/user_access.rs, 2
  forwarding impls, the trait), such as `kernel/src/syscall/table.rs:534`, `table.rs:1632`,
  `table.rs:614`, `kernel/src/libkern/framebuffer.rs:75`, `kernel/src/mm/user_access.rs:265`.
  `pub unsafe trait AbiPlain: Copy + 'static {}` (no padding, every bit pattern valid, backed by
  a const size-equals-sum-of-fields assert at each impl) bounding `copy_from_user`/`copy_to_user`
  and providing `as_bytes` once; `ArchUserAccess::copy_from(dst: &mut [u8], src: UserRange) ->
  bool`, `copy_to(dst: UserRange, src: &[u8]) -> bool` and `copy_cstr(dst: &mut NonEmptySlice
  <u8>, ...)` where `UserRange` is constructible only by `validate_user_range`. Checked against
  `ThreadArgs`, `HandleInfo`, `FramebufferInfo`, `copy_slice_from_user`, `copy_cstr_from_user`
  and `copy_from_user::<SpawnArgs>`. Home: kernel/src/libkern (trait), kernel/src/mm/
  user_access.rs and kernel/src/arch/user_access.rs. Closes: the invalid-bool read and padding
  leak (kplat-d5), the one-byte overrun at `max_len == 0` (kplat-d9), a kernel-side length that
  disagrees with the buffer. Depends on: nothing. abi, opportunities. Value medium, effort
  medium. **Fix**, as kobj-o6.
  Fixed in `efb315e2`, `d4b1fa5e` and `f114be3b`, as kobj-o6. The arch copy is `copy_in(&mut [u8],
  src)` and `copy_out(dst, &[u8])`, and `copy_cstr` takes a slice, so no kernel-side length can
  disagree with its buffer. No `UserRange` is made: the user side stays an address its caller
  validates.
- [ ] **kplat-o11** encapsulate: boot-published firmware and module tables kept as `static mut`
  arrays or structs with a separate count/flag atomic, written once by an `unsafe fn init`
  ("called once") and lent out as `&'static` through `from_raw_parts`, plus the device-IRQ
  handler registry storing fn pointers as `usize` and transmuting them back — 10 sites
  (`grep -rn "(&raw mut \(IOAPICS\|OVERRIDES\|CPU_APIC_IDS\|ECAM\|FADT\))\|(&raw const
  \(IOAPICS\|OVERRIDES\|CPU_APIC_IDS\|ECAM\|FADT\))" kernel/src/arch/x86_64/acpi.rs` = 10; 5
  `static mut` and 7 unsafe blocks in acpi.rs, plus the initramfs `BLOB_PTR`/`BLOB_LEN` pair
  at initramfs.rs:18-41 and the one transmute at idt.rs:512), such as `kernel/src/arch/x86_64/
  acpi.rs:590`, `acpi.rs:631`, `acpi.rs:467`, `kernel/src/initramfs.rs:40`. `struct BootOnce<T>
  { state: AtomicU8, value: UnsafeCell<MaybeUninit<T>> }` with `publish(&self, v) -> Result<(),
  T>` (Release, refuses a second call) and `get(&self) -> Option<&'static T>` (Acquire), fitting
  IOAPICS+count, ECAM+count, FADT+flag and `BootOnce<&'static [u8]>` for the blob. For the
  handler registry, `IrqSpinLock<[Option<extern "C" fn()>; 8]>` read in the dispatcher (already
  IF=0), as `framebuffer::APERTURE`, `cmdline::LINE` and `partitions::BOOT_DISK` do. Home:
  kernel/src/libkern. Closes: a second `Platform::init` mutating data behind a live `&'static`
  slice, count/data publication drifting apart, and the subsystem's only transmute. Depends on:
  nothing. ownership, concurrency, opportunities. Value low, effort small. **Later.**
- [ ] **kplat-o12** encapsulate: device vectors registered from a pool of eight with no release;
  a driver that registers and then declines keeps the slot, and exhausting the pool is an assert
  — 8 sites (`grep -rn "install_msi\|install_software\|install_intx\|install_isa_irq\|
  register_device_handler" kernel/src --include=*.rs` call sites: serial.rs:178, ps2.rs:293,
  295, ahci/bringup.rs:177, 209, xhci/mod.rs:454, io/ramdisk.rs:263, ioapic.rs:243), such as
  `kernel/src/arch/x86_64/idt.rs:534`, `kernel/src/drivers/ahci/bringup.rs:177`,
  `kernel/src/drivers/xhci/mod.rs:454`, `kernel/src/arch/x86_64/serial.rs:178`.
  `register_device_handler(handler) -> Result<DeviceVector, PoolFull>`, where `DeviceVector`
  clears its slot on drop unless `commit()` is called once the device is programmed;
  `install_msi`, `install_intx` and `install_software` return the guard. Home: kernel/src/
  arch/x86_64/idt.rs and kernel/src/arch/irq_install.rs. Closes: a vector leaked on a bring-up
  failure path and the boot-time assert when the pool runs out (the release image already uses
  5 or 6 slots, the test image 7, io/ramdisk.rs:35-38). Depends on: nothing. ownership. Value
  low, effort small. **Plan**, in P5.
- [x] **kplat-o13** narrow: hardware stack pointers travel as `u64`: the TSS.RSP0 and syscall
  stack setters are safe fns taking any `u64`, and `fabricate_frame` is an `unsafe fn` whose
  contract is "top has 56 writable bytes below it", all three values coming from a KernelStack
  — 6 sites (`grep -rn 'set_syscall_kernel_stack\|set_kernel_stack('` finds 4 calls: sched.rs:
  506, 507, 756, 757; `grep -rn 'fabricate_frame('` finds 2 production calls: object/thread.rs:
  281, 338), such as `kernel/src/arch/cpu.rs:23`, `kernel/src/arch/x86_64/syscall.rs:114`,
  `kernel/src/arch/x86_64/context.rs:191`, `kernel/src/sched.rs:506`. `pub struct
  KernelStackTop(u64)` returned only by `KernelStack::top(&self)` and taken by
  `set_kernel_stack` and `set_syscall_kernel_stack` (documented "the stack must outlive this CPU
  running its thread"); `KernelStack::fabricate(&mut self, trampoline) -> ArchThreadContext`
  replaces the unsafe `fabricate_frame`. Checked against `arm_kernel_stack_for` and
  `Thread::try_new_runnable`/`try_new_user`. Home: kernel/src/mm/kstack.rs with kernel/src/arch/
  {cpu.rs, x86_64/syscall.rs, x86_64/context.rs}. Closes: loading an arbitrary or stale RSP0 or
  syscall stack from safe code (kplat-d21). Depends on: nothing. abi, opportunities. Value low,
  effort small. **Fix.**
  Finished in `7be20327`: `KernelStackTop`, and `KernelStack::fabricate` in place of the unsafe
  `fabricate_frame` at its callers.
- [x] **kplat-o14** encapsulate: index/data port pairs (CMOS 0x70/0x71, IOAPIC IOREGSEL/IOWIN,
  PCI 0xCF8/0xCFC) driven by unlocked select-then-access helpers, with serialisation held by
  callers (`clock::SETTING` for RTC writes, "called once" for the RTC read, boot-only use for
  the IOAPIC, a stopped machine for reset) — 5 helper fns (rtc.rs:57 `read_reg`, rtc.rs:72
  `write_reg`, ioapic.rs:55 `read_reg`, ioapic.rs:71 `write_reg`, reset.rs:137-138 the CF8/CFC
  pair), with 7 IOAPIC call sites (ioapic.rs:130, 137, 190, 199, 200, 221, 222), such as
  `kernel/src/arch/x86_64/rtc.rs:57`, `kernel/src/arch/x86_64/ioapic.rs:55`,
  `kernel/src/arch/x86_64/reset.rs:137`, `kernel/src/arch/x86_64/rtc.rs:72`. `struct IndexedPort
  { lock: IrqSpinLock<()>, .. }` with `lock(&self) -> IndexedGuard` whose `read(idx)`/
  `write(idx, v)` select and access under one hold; for the IOAPIC, `static IOAPIC:
  IrqSpinLock<Option<IoApicRegs>>` (a Leaf rank) with safe `read(&mut self, idx)`/`write`, and
  `route`, `set_rte_mask` and `init` run under the guard. Home: kernel/src/arch/x86_64/regs.rs
  and ioapic.rs. Closes: two contexts interleaving select and access so a write lands in
  another register (kplat-d17). Depends on: nothing. ownership, concurrency. Value low, effort
  small. **Fix.**
  Done in `b3040217` for the IOAPIC and `1dec3728` for the CMOS pair: `arch::Rtc` is the one value
  that reaches it, held by `clock::RTC`, which replaces `clock::SETTING`. No shared `IndexedPort`:
  the pairs share neither width nor rule (the CMOS keeps its NMI bit). The PCI configuration pair's
  one user is the reset, on a stopped machine, so it takes no lock.
- [x] **kplat-o15** narrow: the FP/SIMD save area handled through raw `*mut`/`*const
  ArchFpuState` even where the caller holds a `KBox` or `&mut` — 4 sites (`grep -rn
  'fpu_save\|fpu_restore\|fpu_init_area' kernel/src`, less arch/mod.rs: object/thread.rs:186,
  sched.rs:638, 642, fpu.rs:427 in the selftest), such as `kernel/src/arch/x86_64/fpu.rs:184`,
  `fpu.rs:234`, `kernel/src/object/thread.rs:186`, `kernel/src/sched.rs:638`.
  `ArchFpuState::initial() -> Self` (zeroed area plus FCW/MXCSR defaults, the only constructor,
  so every value is a valid image), `fn save(&mut self)` and `fn restore(&self)`; alignment is
  already a `#[repr(align(64))]` type invariant. Checked against `new_fpu_area` and
  `switch_into`'s swap. Home: kernel/src/arch/x86_64/fpu.rs. Closes: restoring an uninitialised
  or foreign image (#GP from MXCSR or XSTATE_BV) and an unaligned or dangling area pointer.
  Depends on: nothing; the sched call sites keep raw access until kplat-o5 gives them `&Thread`.
  opportunities. Value low, effort small. **Fix.**
  Fixed in `aaceb893`: `ArchFpuState::initial()` is a const fn with the architectural defaults and
  the only constructor, so every area is an image `restore` can load;
  `the_initial_area_is_the_architectural_defaults`.

What held up: `SyscallFrame` and `ExceptionFrame` match their stubs' push and pop order, pinned
by `offset_of!`/`size_of` asserts, and RSP is 16-byte aligned at each `call`. `sysretq` to a
non-canonical RCX cannot happen: `USER_MAP_END` keeps the top user page unmapped and all four
user-mapping paths check it. Every IDT gate is DPL 0, the TSS iomap base equals the TSS size,
and STAR carries RPL 3. The user-copy asm sets `cld` before `rep movsb`, registers the right
fault PC, and clears AC on both paths. SFMASK clears IF, DF and AC on syscall entry; NMI uses
IST2 over both user-stack windows and the exit path does `cli` before `pop rsp`. FPU: the XSAVE
size is re-read after XCR0 and asserted, the area is 64-byte aligned, and `init_area` seeds
FCW/MXCSR. ACPI parsers read bounds-checked slices capped at `MAX_TABLE_BYTES`, with host tests
for short tables, and the caches publish count-after-data with Release/Acquire. `initramfs::
lookup` uses `checked_add` and bounds every slice. fbcon `Screen::new` refuses a null or
misaligned base and `put()` checks x and y (Miri: `after_the_yield_nothing_is_painted_and_the_
reclaim_repaints_everything` passed with no UB). `klog` append/runs/`copy_into_frames` stay
inside prefix, ring and cap, and `push` uses `try_lock`. `ensure_table`'s CAS link and
give-back of the losing table is sound (host test `a_cpu_beaten_to_an_empty_slot_takes_the_
table_linked_first`); the defect is the plain accesses beside it. Miri passed
`map_translate_unmap_round_trip` and `fabricate_frame_lays_out_trampoline_and_zeroed_regs`.
Limine bindings match limine-protocol field by field; the buddy takes only `MEMMAP_USABLE`, so
no `&'static` view of Limine, ACPI or module memory is reused today. Module paths and the
framebuffer descriptor are copied, not retained. `unmap_plain` flushes only locally, but the
vmap bump allocator never reuses a VA. `stop_other_cpus` masks IF before reading its APIC id,
and `leave_online` refuses an unbound CPU. `power()` takes no lock and allocates nothing after
the stop. `context_switch` clears `on_cpu` only after the resume RSP store, and FPU save
precedes it. `arm_kernel_stack_for` re-arms RSP0, `kstack_top` and `KERNEL_GS_BASE` under SCHED
with IF=0. `copy_cstr_from_user` refuses `max == 0` before the raw primitive. Every type
copied to or from user memory today is padding-free with valid-for-all bit patterns. The
`nomem` on `cli`/`sti` is sound under the project's atomics-for-IRQ-shared-state rule (a host
snippet showed the compiler does move plain stores across it, so kplat-d22 stays a latent
defect). `elapsed_ns` saturates for a cold AP. `install_msi` addresses (0xFEEx_x000) are never
refused by `program_msi`, so the MSI-then-INTx fallback leaks no vector today.

### runtime — main at 135055a8, 2026-10-27

Lenses: abi, ownership, soundness, opportunities; returned nothing: none.
Defects: found 23, refuted by a boot 0, reported 11 (after merging duplicates). Opportunities: 13 (34 raised, merged).

The inventory, `cargo xtask unsafe-inventory` over its paths:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `userspace/libkern` | 1305 | 20 | 12 | 0 | 0 | 16 | 1 | 0 | 0 | 2 | 10 |
| `userspace/libos` | 477 | 19 | 5 | 0 | 0 | 13 | 0 | 1 | 0 | 0 | 0 |
| `userspace/libheap` | 236 | 15 | 6 | 3 | 0 | 5 | 0 | 3 | 0 | 0 | 0 |
| `userspace/libfs` | 526 | 30 | 0 | 0 | 0 | 31 | 0 | 0 | 0 | 0 | 0 |
| `userspace/liblog` | 86 | 4 | 0 | 0 | 0 | 4 | 0 | 0 | 0 | 0 | 0 |
| `userspace/libtime` | 143 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `userspace/hello` | 239 | 45 | 0 | 0 | 8 | 26 | 0 | 2 | 0 | 2 | 3 |
| `userspace/heartbeat` | 188 | 10 | 0 | 0 | 4 | 7 | 0 | 0 | 0 | 0 | 0 |
| `total` | 3200 | 143 | 23 | 3 | 12 | 102 | 1 | 6 | 0 | 4 | 13 |

**Defects**

- [x] **rt-d1** `userspace/libos/src/exec.rs:133` — `Op` is a public, safe `Future`, but
  `Op::poll` casts the data pointer of whatever `Waker` polls it to `&Reactor` and writes a
  72-byte `Registry` through it. Its SAFETY claim, that only `block_on` builds the waker, is not
  enforced. Scenario: an `Op` that is still pending is polled with `Waker::noop()`, a
  join/select that wraps the waker, a second executor, or the multi-task executor
  `docs/architecture/libos.md` plans. `Reactor::register` then reads and writes 72 bytes at
  that waker's data (null, or another executor's task state). A clone of `block_on`'s waker that
  escapes the call (`waker_clone` shares the pointer into `block_on`'s stack frame) dangles
  once `block_on` returns.
  Demonstrated: host probe tests, removed. A scripted not-ready `Op` polled with a waker over a
  zeroed `[u64; 9]` overwrote it (`[1000, 0, ..., 1]`). `Waker::noop()` panicked with 'null
  reference produced' at exec.rs:133 in debug, and read garbage in release (`range end index
  137424457960672`). Under Miri (`probe_waker_clone_outlives_block_on`, `probe_foreign_waker`):
  'Undefined Behavior: constructing invalid value of type &exec::Reactor: encountered a dangling
  reference'. The control, `deferred_op_polls_then_waits_then_completes`, is clean. Fix: give
  clones their own vtable and test `ptr::eq(cx.waker().vtable(), &VTABLE)` before the cast,
  else `wake_by_ref` and return `Pending`. With that change both probes pass under Miri and all
  19 libos tests pass.
  abi, ownership, soundness, opportunities. Medium. **Fix.**
  Fixed in `aedd2368`: `block_on`'s waker has a vtable nothing else names and its clones another;
  `Op::poll` reads a reactor only through the first, else wakes and returns `Pending`. Fails
  without it: `an_op_reads_no_wakers_data_but_block_ons` (a foreign waker's data written); with
  clones on `VTABLE`, Miri reports the dangling `&Reactor`.

- [x] **rt-d2** `userspace/libos/src/objects.rs:44` — `Handle<Memory, M>::unmap(&self, addr, len)`
  is a safe fn that never reads `self` and passes any address to `sys_memory_unmap`. The kernel
  ignores `len` and the handle and unmaps the whole VMA covering `addr` (table.rs:949-966). The
  doc on `map`, 'valid while `self` lives', is untrue in both directions. Scenario: safe code
  calls `mem.unmap(vec.as_mut_ptr(), 1)`, or passes a stale address whose VA libheap's next arena
  reused. The whole 64 KiB libheap arena, or a file mapping another slice borrows, vanishes under
  live `Box`/`Vec`. The next access faults, or after a later map reuses the VA it touches
  another object's memory. Callers today (libsurface screen.rs:37, libdraw text.rs:250,
  test-harness uiclient.rs:159) pass what `map` returned, so this is latent. One wrong address
  makes it bite.
  Demonstrated: probe tests under `#[forbid(unsafe_code)]`, removed. `unmap(<page of a Vec>, 1)`
  compiles with no `unsafe` and returns `Ok` on the mock. The kernel effect is shown by the code
  path (objects.rs -> sys.rs:106 -> table.rs:949 `unmap_covering_deferred`), not executed. A
  boot-probe step would settle it: Box a value, unmap its page, read it. Fix: make `unmap`
  unsafe, or return an owning `Mapping` (see rt-o3).
  abi, ownership, soundness, opportunities. Medium. **Fix.**
  Fixed in `670a3414`: `unmap` is an `unsafe fn` with its contract, as the triage chose ahead of
  P7's `Mapping`; a `#[forbid(unsafe_code)]` probe unmapping a `Vec`'s page no longer compiles
  (E0133).

- [x] **rt-d3** `userspace/libos/src/objects.rs:167` — `thread_create(&ThreadArgs)` is a safe fn
  that starts a thread at any `entry` on any `user_sp`. The kernel checks only that both are
  non-zero user-half addresses (table.rs:538-542; 'that they are mapped is the caller's
  responsibility'). Scenario: safe code sets `user_sp` to the end of a `Vec`'s buffer or into its
  own stack frame, or drops the `Vec` afterwards, or sets `entry` to a Rust-ABI fn. The new
  thread's pushes and spills overwrite live or freed heap, or it returns into garbage. The five
  callers (test-harness stage.rs:507, 629, harness.rs:159, 362, bootprobe/exits.rs:242) map a
  dedicated stack they never unmap, held by comment only (stage.rs:485). The same shape applies
  to `spawn(&SpawnArgs)` and `move_mask`.
  Demonstrated: probe test, removed. `ThreadArgs` with `user_sp` at the end of a local `[u8;
  4096]` (and another with `entry=0x1000`, `user_sp` inside a live `Vec`) compiles with no
  `unsafe` and `thread_create` returns `Ok` on the mock. The kernel side is by reading
  table.rs:538-542. Fix: `unsafe fn` with a `# Safety` section, or a typed
  `Thread::start(extern "C" fn(u64) -> !, Stack, arg0)` (see rt-o6).
  abi (medium), soundness (low), opportunities (low). Medium. **Fix.**
  Fixed in `670a3414`: `thread_create` is an `unsafe fn` with its contract, ahead of P7's
  `Thread::start`; a probe starting a thread on a `Vec` no longer compiles (E0133). `spawn`'s
  `move_mask` stays with rt-o6.

- [x] **rt-d4** `userspace/liblog/src/lib.rs:103` — `open_source` resolves a SEND endpoint into
  `Logger`, which is `#[derive(Copy, Clone)]` with no `Drop`, and nothing in the crate ever
  closes it. `Logger::new` also wraps endpoints the caller still owns (heartbeat main.rs:217),
  so the type cannot say which handles it owns. Scenario: bootprobe/services.rs:399, heartbeat
  main.rs:172 and view-broker main.rs:1708 each open a source, log once and drop the `Logger`. The
  handle stays in the table, and the logging service's per-source channel stays held, until
  the process exits. Each caller leaks once today. A service that opens a source per request
  fills the logging service's 26 source slots (MAX_SOURCES = 32-2-2-2, main.rs:54) after 26
  calls, and the service then refuses every new log source system-wide ('source table full',
  main.rs:341).
  Demonstrated: path pointed at. lib.rs:16-19 has no `Drop` and no `SYS_HANDLE_CLOSE` of
  `handle` (grep); `open_source` closes only the PO (line 118); `grep -rn 'open_source('
  userspace` finds the 3 callers. Fix: an owning, non-`Copy` `Logger` closing on drop, plus a
  borrowing constructor for endpoints a supervisor hands over.
  abi, ownership, soundness. Low. **Fix.**
  Fixed in `b67e9959`: `Logger` is not `Copy`; one `open_source` resolved closes on drop, and
  `Logger::borrowing` wraps an endpoint its caller keeps. A boot opening and dropping 40 sources
  resolved 18, with 22 "source table full", without the drop; 40 of 40 with it.

- [x] **rt-d5** `userspace/libdraw/src/text.rs:247` — libos `Handle::map` returns a bare pointer
  with no owner, and three callers leave mappings behind. In `load`, the `?` on
  `data.try_reserve_exact(size)` returns between `obj.map` (line 244) and `obj.unmap` (line
  250). Scenario: a font up to MAX_FONT_BYTES (8 MiB) loads while the heap cannot reserve it.
  `ImpossibleSize` is returned and the mapping, with the font's page-cache pages, is pinned for
  the process's life. Two siblings leak on the success path: init/src/main.rs:577 maps
  /system/current-generation once per boot and never unmaps it, and libdraw/src/acquire.rs:52
  `read_info` maps /dev/framebuffer/info and never unmaps, called twice at compositor start
  (compositor main.rs:2214 and acquire.rs:89).
  Demonstrated for init and acquire.rs by reading (no unmap on any path; compare libsurface
  screen.rs:34-37 and uiclient.rs:158-159, which unmap). Unverified for the text.rs early return:
  no host test reaches `load`; an OOM-injected boot, or a host test behind a libos seam with a
  failing allocator, would settle it.
  ownership (demonstrated), opportunities (unverified). Low. **Fix.**
  Fixed in `2cde6627`: init unmaps both pages once read, libdraw's `read_info` once its record is
  copied, and the font load reserves before it maps. A boot logging each unmap showed init's two
  and the compositor's two, each succeeding; the reorder is shown by reading.

- [x] **rt-d6** `userspace/libos/src/exec.rs:141` — `Op::drop` closes an in-flight PO, but for a
  forwarded lookup the kernel still installs the resolved handle in the caller's table when the
  server replies. An abandoned lookup leaves a handle nothing holds. libfs `po_wait` and liblog
  `open_source` do the same whenever `sys_wait` does not return 1. Scenario: a `Handle::lookup`
  future is dropped before its userspace server answers, through a select or timeout combinator
  or the planned executor's cancellation. `complete_resolve_reply` allocates into `pl.owner_pid`
  (table.rs:2196) regardless of whether the PO handle is still open, and `deliver_to_client`
  (table.rs:822-828) closes it only when the process has no threads. One handle leaks per
  abandoned lookup, pinning the object. Today `block_on` always drives a lookup to completion.
  Unverified: needs a boot-probe step that issues `sys_ns_lookup` against a server delaying its
  reply, closes the PO, lets the reply land, and counts handles. Not booted.
  ownership, soundness. Low. **Fix**, raised to Medium.
  Fixed in `fc9ce772`: a lookup's object is parked in its PO and installed when `sys_wait` reads the
  completion, the maintainer's choice; a PO freed unread releases it. Fails with the install at
  reply time: `a_reply_to_a_lookup_its_client_closed_makes_no_handle`.

- [x] **rt-d7** `userspace/test-harness/src/inputclient.rs:72` — input-testclient's `po_wait`
  never closes its PendingOperation; the file imports no close at all. Scenario: each run
  resolves /dev/input/new, waits and decodes, and the PO stays in the table. It is the only one
  of the 15 `fn po_wait*` clones that does.
  Demonstrated: `grep -n 'HANDLE_CLOSE\|syscall1\|fn close' inputclient.rs` finds nothing; all
  15 bodies dumped with awk, and each of the other 14 closes `po` on both paths.
  ownership. Low. **Fix.**
  Fixed in `aea50b2c`: its `po_wait` closes the PO after the wait; every `fn po_wait*` found
  (thirteen) now closes its PO.

- [x] **rt-d8** `userspace/libos/src/exec.rs:122` — `Op::poll` sets `done` but never reads it, so
  a re-poll after `Ready` waits on the still-open PO again and returns the same `IoResult`,
  contrary to the field's comment ('a stray re-poll after Ready is a no-op'). Scenario: a
  combinator re-polls a completed lookup `Op`. It gets a second `Ready(Ok(IoResult { result:
  handle }))`, and objects.rs:374-377 turns each `Ready` into an owning `Handle::from_raw`, so
  one handle gets two owners and is closed twice. `lookup`'s own async body never re-polls, so
  this is latent.
  Demonstrated: probe test, removed. `script_next(0, 777, true)`; `Op::submit`; poll twice with
  `Waker::noop()`. Output: first and second both `Ready(Ok(IoResult { handle: 1000, ..., result:
  777 }))`, and the assertion that the second poll is `Pending` failed. Fix: return `Pending` or
  panic when `done`, or take the PO out on `Ready`.
  abi. Low. **Fix.**
  Fixed in `d22ea05c`: a second poll after `Ready` is an error and waits on nothing. Fails
  without it: `a_completed_op_hands_its_result_out_once`.

- [x] **rt-d9** `userspace/libos/src/exec.rs:169` — when no `Op` registered a handle, `block_on`
  calls `sys::wait(&[], .., 0)` as a 'poll-yield, not a tight spin', but the kernel refuses count
  0 at once with `InvalidArgument` (table.rs:1764-1766) before any scheduling. Scenario: a
  hand-written future returns `Pending` without registering anything (it waits on a flag
  another thread sets). `block_on` re-polls in a loop of rejected zero-length waits and burns a
  CPU instead of yielding.
  Demonstrated: path pointed at (exec.rs:167-169 passes an empty slice; kernel check quoted
  above). Not run on target.
  abi. Low. **Fix.**
  Fixed in `fae8cbf5`: with nothing registered, `block_on` sleeps 1 ms on a timer it makes once.
  Fails without it: `block_on_sleeps_with_nothing_to_wait_on`.

- [x] **rt-d10** `userspace/libos/src/sys.rs:23` — `sys::wait(handles, results, deadline)` is a safe
  fn that passes `handles.len()` as the count and `results` with no length. The kernel writes
  one 24-byte record per signalled handle, so a shorter `results` is overrun. Scenario: a new
  libos wait (the planned multi-task executor's wait-any, or a two-handle poll) passes
  `zeroed_results::<1>()` for two signalled handles. The kernel writes 48 bytes into a 24-byte
  stack array. The current callers (exec.rs:122, 169, 171) all hold the precondition by
  construction.
  Unverified on target (no caller does this today). Host probe, removed: two ready `Op`s passed
  to `sys::wait(&[a.po, b.po], &mut one, 0)` compile with no `unsafe`, and the mock, which
  bounds-checks, panicked 'index out of bounds: the len is 1 but the index is 1' at sys.rs:187.
  The target body (sys.rs:27-33) has no such check.
  ownership. Low. **Fix.**
  Fixed in `904504f0`, with rt-o11: `sys::wait` cuts both slices to the shorter in code the host
  build runs. Fails without it: `a_wait_takes_no_more_handles_than_its_results_hold`.

- [x] **rt-d11** `userspace/libfs/src/lib.rs:181` — `read_file` and `copy_file` stat one resolve
  of the path (`file_size`, which closes its handle) and then map a second, independent resolve
  using that stale size, so the size crosses from one object to another. Scenario: between
  `file_size` (lines 174 and 69) and the second lookup (181 and 94), another process renames a
  different file over the path or truncates it. If the new file is larger, `read_file` returns
  only its first `size` bytes and `copy_file` writes a truncated copy and reports `size` as
  copied. If it shrank within the same page count the tail reads as zeros; if it lost pages the
  map fails with `Io(InvalidArgument)`. No memory is overrun, because `sys_memory_map` bounds a
  mapping by npages (table.rs:891-900).
  Unverified: needs a boot with a second process racing a rename or truncate against
  `read_file`; libfs issues raw syscalls, so no host test reaches it. Fix: one lookup with
  MAP_READ|INSPECT, then stat and map that same handle, as libdraw text.rs:233-244 does.
  abi. Low. **Fix.**
  Fixed in `af5778a2`: `copy_file` resolves the source once and stats and maps that handle;
  `read_file` already did. Structural; no host test reaches libfs's raw syscalls.

**Opportunities**

- [ ] **rt-o1** encapsulate: out-parameter syscalls called raw with a caller-chosen buffer or
  `static mut` (`sys_handle_stat` into `HandleInfo`, `sys_notif_recv`, `sys_ns_enumerate`,
  `sys_clock_read`, `sys_channel_create`) — 125 sites (`grep -rnE
  "syscall[0-6]\(\s*(libkern::)?(syscall::)?SYS_(HANDLE_STAT|NOTIF_RECV|NS_ENUMERATE|CLOCK_READ|CHANNEL_CREATE)\b"
  --include=*.rs userspace | grep -v '^userspace/libkern/' | wc -l`: stat 18, notif 16,
  enumerate 6, clock 45, channel_create 40; 13 in runtime crates; the opportunities lens
  counted 127 with its script), plus 38 `static mut` out-params, such as
  `userspace/libfs/src/lib.rs:58`, `userspace/hello/src/main.rs:87`,
  `userspace/heartbeat/src/main.rs:100`, `userspace/libos/src/objects.rs:383`. Thin typed
  wrappers that own the out-param and return it by value: `handle_stat(h) ->
  Result<HandleInfo, KError>`, `clock_read(clock) -> Result<u64, KError>`,
  `notif_recv(ch) -> Result<Option<Notification>, KError>`, `ns_enumerate(ns, i) ->
  Result<Option<NsEntry>, KError>`, `channel_create(depth) -> Result<(u64, u64), KError>`, in
  libkern (its CLAUDE.md allows thin Result wrappers, and eshell, auth-service and init link
  libkern without alloc), with typed faces in libos (`Handle::stat()`, a clock module). Checked
  against libfs `file_size`, hello's stat and clock steps, heartbeat `clock_now`, hello's
  channel create, and service-mgr/src/main.rs:441-443, which already does this by hand. Closes:
  an out-buffer sized by the caller's expectation rather than by what the kernel writes (the
  2026-07-24 stack smash, a 16-byte buffer for the 24-byte `HandleInfo`), stale reads after a
  failed clock read, and shared `static mut` out-params. Depends on: nothing; write alongside
  Part E's syscall-surface changes so each is written once. abi, ownership, opportunities.
  Value high, effort medium. **Plan**, in P7.

- [ ] **rt-o2** encapsulate: a PendingOperation waited on by hand with a raw `sys_wait` into a
  hand-sized `[u8; 24*N]`, the `IoResult` decoded at byte offsets 0/8/16, and the PO closed by
  hand — 99 sites in 46 files (`grep -rnE "syscall[0-6]\(\s*(libkern::)?(syscall::)?SYS_WAIT\b"
  --include=*.rs userspace | grep -v '^userspace/libkern/' | wc -l`; 8 in runtime; the other
  script counted 111 uses in 50 files), including 15 copies of `fn po_wait` (`grep -rn 'fn
  po_wait' --include=*.rs userspace`), 41 hand-sized one-record buffers, 25 byte-offset status
  decodes and 25 sites with count != 1, such as `userspace/libfs/src/lib.rs:384`,
  `userspace/liblog/src/lib.rs:113`, `userspace/heartbeat/src/main.rs:154`,
  `userspace/libos/src/sys.rs:60`. In libkern, for the libkern-only crates (eshell,
  librsproto, auth-service): `wait(handles: &[u64], results: &mut [IoResult], deadline) ->
  Result<&[IoResult], KError>`, which refuses `results.len() < handles.len()`; `wait_one(h,
  deadline) -> Result<IoResult, KError>`; and `po_complete(po) -> Result<u64, KError>`, which
  waits, closes and maps status. In libos, make `Op` constructible from every PO-returning
  syscall (today `Op::from_po` is `pub(crate)`, exec.rs:111) with `ns_lookup`, `file_create`,
  `file_truncate` and `file_rename` returning `Op`, driven by `block_on`, and make `sys::wait`
  public and checked. Checked against libfs `po_wait` (four resolves), liblog `open_source`,
  heartbeat `recv_log_handoff` and nxterm `wait_any` (n handles). Closes: a result buffer
  sized by expectation (the desktop-session-mgr 256-vs-4096 class applied to `sys_wait`'s
  count*24 write), `IoResult` read at hand-typed offsets, a PO leaked on an early return
  (rt-d7), the rt-d10 overrun, and the drift of 15 copies of a helper `libos.md` says was
  collapsed. Depends on: nothing for the libkern wrapper; for libos, crates that link only
  libkern taking libos as a dependency (it is `no_std`, no alloc) or the maintainer extending
  libkern's one-guard exception as for `Received`. abi, ownership, soundness, opportunities.
  Value high, effort medium. **Plan**, in P7.

- [ ] **rt-o3** encapsulate: a mapping kept as a bare address, with a slice built by
  `from_raw_parts`/`copy_nonoverlapping` and an unmap the caller must remember, with some sites
  never unmapping — 87 sites (`grep -rnE "syscall[0-6]\(\s*(libkern::)?(syscall::)?SYS_MEMORY_MAP\b"
  --include=*.rs userspace | grep -v '^userspace/libkern/'` = 81 raw, plus 6 libos
  `Handle::map` callers; 95 raw maps against 78 unmaps by the other grep; libfs alone has 10
  mapping lifetimes in 10 of its 30 unsafe blocks), such as `userspace/libos/src/objects.rs:35`,
  `userspace/libfs/src/lib.rs:183`, `userspace/libsurface/src/screen.rs:31`,
  `userspace/init/src/main.rs:577`. `Handle<Memory, M>::map(&self, len) -> Result<Mapping<'_,
  M>>`: `Mapping` unmaps its own base on `Drop`, lends `bytes(&self) -> &[u8]` tied to its
  lifetime (`bytes_mut`, `copy_in` where `M: CanMapWrite`; `copy_out`, `copy_from_mapping`),
  offers `into_raw()` for a mapping kept for the process's life (the framebuffer aperture,
  acquire.rs:99) and `Stack`'s base for rt-o6. The free-standing safe `unmap(addr, len)` is
  removed. Home: libos (objects.rs); `FileObject` handles need a `File` marker or `Memory`
  typing. Checked against libfs `read_file`, `write_file` (drop before `sys_file_sync`, which
  needs the unmap-before-flush order at :233-243) and `map_copy`; libsurface `screen::read`;
  libdraw `text::load`; coreutils `whoami`. Closes: rt-d2, rt-d5, slices outliving their unmap,
  a `&[u8]` built over memory another process writes (libsurface screen.rs:33). Depends on:
  nothing for the type; libfs adopting it needs rt-o4. abi, ownership, soundness,
  opportunities. Value high, effort medium. **Plan**, in P7.

- [ ] **rt-o4** encapsulate: libfs holds files, POs and mappings as raw `u64` handles closed by
  hand on every path, although layering (libfs above librsproto above libos) allows libos —
  30 sites (30 `unsafe` blocks: 13 `SYS_HANDLE_CLOSE`, 4 `SYS_MEMORY_MAP`, 5 `SYS_MEMORY_UNMAP`, 2
  `SYS_FILE_SYNC`, 1 each of stat, lookup, create, truncate, rename, enumerate and wait; `grep -c
  'unsafe {' userspace/libfs/src/lib.rs`), plus `lookup_wait` handing a raw `u64` to 16 external
  callers (`grep -rn 'libfs::lookup_wait' --include=*.rs userspace`), and liblog's one close,
  such as `userspace/libfs/src/lib.rs:60`, `userspace/libfs/src/lib.rs:99`,
  `userspace/libfs/src/lib.rs:183`, `userspace/libfs/src/lib.rs:261`. libfs and liblog over
  libos: `lookup` returns an owning `Handle<File|Memory, M>` closed on `Drop` (`lookup_wait` ->
  `Result<Handle<..>, i32>`); libos gains a `File` marker with `stat()` and `sync()`, and
  `Namespace::create_file`, `truncate`, `rename`; liblog's `Logger` owns its handle. Checked
  against `file_size`, `copy_file` (two handles closed on three paths), `read_file` (one lookup,
  stat that handle, map, `to_vec`), `write_file` and whoami's caller. Home: libos (file ops)
  with libfs rewritten over it. Closes: a handle leaked or closed twice on an early return (14
  hand-placed closes in runtime, 356 userspace-wide), rt-d4, and by construction the rt-d11
  stat-one-map-another race; nearly all of libfs's 30 unsafe blocks go, the largest share of
  the subsystem's unsafe after hello. Depends on: rt-o1, rt-o2, rt-o3. abi, ownership,
  soundness, opportunities. Value high, effort medium. **Plan**, in P7.

- [ ] **rt-o5** encapsulate: an IPC message built and read as raw `[u8; 4096]`, `payload_len`
  stamped and parsed at bytes 4..8 by hand with a local `const PAYLOAD_OFF: usize = 24` — 67
  sites (`grep -rnE "\[4\.\.8\]\.copy_from_slice\(&\(" --include=*.rs userspace | wc -l`; 4 in
  fs-server-ext4 may not be IPC; 19 local `PAYLOAD_OFF` copies; about 93 `SYS_CHANNEL_SEND` lines
  in 35 files; 3 in runtime), such as `userspace/liblog/src/lib.rs:84`,
  `userspace/heartbeat/src/main.rs:132`, `userspace/libkern/src/control.rs:187`,
  `userspace/hello/src/main.rs:279`. Reuse libkern's `IpcMsg` (or an align(8) twin) as the only
  send and receive buffer: `payload_mut(&mut self) -> &mut [u8; IPC_PAYLOAD_SIZE]`,
  `set_payload_len(n) -> Result<(), TooLarge>`, `payload(&self) -> &[u8]` bounded by
  `min(payload_len, IPC_PAYLOAD_SIZE)`, `sender_pid()`, with `channel_send(ch, &IpcMsg,
  &[u64], mode) -> Result<i64, KError>` beside `channel_recv`; libos's `Handle<Channel, M:
  CanSend>::send` sits above it (libos has no Channel object today). Home: libkern
  abi.rs/channel.rs. Checked against liblog `emit`, heartbeat `emit_beat_typed`, hello's
  self-send, `control::recv`. Closes: a payload bound that disagrees with the kernel's. liblog
  and heartbeat encode into `buf[24..]` (4072 bytes) but `StoredMsg::seal` accepts 4008
  (`IPC_PAYLOAD_SIZE`, ipc_channel.rs:56-58), so a 4009-4072 byte record is encoded and then
  refused with `InvalidArgument`, the send result ignored (a probe encoding 4040 bytes made a
  4064-byte record). Today the outcome matches liblog's documented 'dropped' behaviour only by
  luck. Also closes header fields at hand-typed offsets. Depends on: nothing. abi,
  soundness, opportunities. Value medium, effort small to medium. **Plan**, in P7.

- [ ] **rt-o6** narrow/encapsulate: `spawn(&SpawnArgs)` and `thread_create(&ThreadArgs)` are safe
  wrappers over raw ABI blocks; a thread's stack and entry, and the handles `move_mask` moves,
  are held only by comments at the callers — 27 sites (22 `SpawnArgs {` literals outside
  libkern/libos, `grep -rn 'SpawnArgs {' --include=*.rs userspace`; 5 `thread_create` callers, all
  in test-harness; 21 non-zero `move_mask`), such as `userspace/libos/src/objects.rs:167`,
  `userspace/libos/src/objects.rs:155`, `userspace/test-harness/src/stage.rs:446`,
  `userspace/service-mgr/src/main.rs:417`. First `unsafe fn thread_create` with a `# Safety`
  contract (`entry` is a never-returning `extern "C" fn`, `user_sp` tops a stack nothing else
  uses for the thread's life); then `Thread::start(entry: extern "C" fn(u64) -> !, stack:
  Stack, arg0)` with `Stack::map(bytes)` an owned mapping handed to the thread (needs rt-o3);
  and `Spawn::new(&image).namespace(..).grant_move(Handle<T, M>, rights).grant_copy(&Handle,
  rights).arg0(x).spawn() -> Result<Handle<Process, Only>, (Error, Grants)>`: moves consume the
  `Handle` on success and come back on failure (the kernel is atomic-or-fail,
  process-spawn-args.md:77 and closes moved handles in the parent on success, table.rs:497-501),
  copies borrow. Checked against stage.rs `run_grants` (one moved channel), service-mgr
  `spawn_with_control` (moves `child_end`, closes by hand on failure, main.rs:425-431), init's
  spawns, stage.rs:486 and bootprobe/exits.rs:225. Home: libos objects.rs. Closes: rt-d3, and a
  moved handle closed by its old owner after a successful spawn or leaked after a failed one.
  Depends on: nothing for the `unsafe fn` and `Spawn` builder; rt-o3 for `Stack`. abi,
  ownership, opportunities. Value medium, effort medium. **Plan**, in P7.

- [ ] **rt-o7** encapsulate: bootstrap handles arrive at `_start` as bare `u64`s and travel as
  `root_ns: u64`; each consumer re-wraps with `unsafe Handle::borrow` or declares its own `unsafe
  fn f(root_ns: u64)` — 24 sites (9 `Handle::<..>::borrow(` in 8 files and 15 `unsafe fn` with a
  raw namespace parameter, regex `unsafe fn \w+(<[^>]*>)?\([^)]*\b(root_ns|ns|namespace)\s*:\s*u64`;
  52 `extern "C" fn _start(` and 52 `#[panic_handler]`), such as
  `userspace/heartbeat/src/main.rs:213`, `userspace/libos/src/handle.rs:148`,
  `userspace/libsurface/src/screen.rs:22`, `userspace/libdraw/src/text.rs:219`. A libos entry
  shim (the thin-entry seam `libos.md` records) turns rdi/rsi/rdx/rcx into `Startup { notif:
  Handle<Notify, Only>, root: Handle<Namespace, NsReadOnly>, handle0: RawHandle, arg0: u64 }`
  held for the process's life, with `&'static Handle<Namespace, NsReadOnly>` passed down. Home:
  libos. Checked against heartbeat `_start`, libdraw `text::load` (takes `&Handle<Namespace,
  impl CanLookup>`, no longer unsafe), libsurface `screen::read`. Closes: a borrow that outlives
  its owner and the per-crate unsafe raw-namespace contracts. Depends on: the entry/std-port
  seam decision (`libos.md`). opportunities. Value medium, effort large. **Plan**, in P7.

- [ ] **rt-o8** narrow: `Handle::borrow(raw, rights)` yields a full `Handle` with no lifetime, on
  which safe `without_transfer`, `without_duplicate`, `into_read_only` and `into_raw` act on a
  handle the borrow does not own, while `from_raw`, `borrow` and `Namespace::lookup` are `unsafe`
  on a type/mode claim that no memory safety rests on (a wrong `T` or `M` only yields `Err`,
  because the kernel type- and rights-checks every operation) — 17 sites outside libos (`grep -rnE
  "\.lookup::<|Handle::<[^>]*>::borrow\(" --include=*.rs userspace | grep -v libos/`: 8 lookup
  and 9 borrow; the other grep `>::borrow(RawHandle` found 11 borrows), such as
  `userspace/libsurface/src/ipc.rs:111`, `userspace/init/src/main.rs:567`,
  `userspace/libdraw/src/acquire.rs:47`, `userspace/test-harness/src/harness.rs:563`. A
  `HandleRef<'a, T, M>`: `Copy`, no `Drop`, no `retype`, `without_*`, `into_read_only` or
  `into_raw`; `Handle::as_ref(&self)`, `unsafe fn HandleRef::from_raw(RawHandle)` for bootstrap
  words, `lookup`/`enumerate`/`bind` written once for both; make `lookup` and `from_raw` safe
  (at most a debug type check). Home: libos handle.rs/objects.rs. Checked against the lookup
  sites (ipc.rs:111, init:567), the bind site (harness.rs:563), `map`, `read`/`write`, `recv`
  and `enumerate`. Closes: attenuating an owner's capability in place through a borrow
  (forbidden only in `borrow`'s doc), `into_raw` on a borrow reaching a spawn's `move_mask`, a
  borrow outliving its owner, and `unsafe` blocks that hold no memory obligation and dilute
  those that do. Depends on: nothing. ownership, soundness. Value medium, effort small.
  **Plan**, in P7.

- [ ] **rt-o9** remove: local re-implementations of `kprint` and `exit` over raw
  `SYS_DEBUG_KPRINT` / `SYS_PROCESS_EXIT` although `libkern::debug::{kprint, exit}` exist (`exit` is
  `-> !` via noreturn asm) — 24 sites (`grep -rnE
  "syscall[0-6]\(\s*(libkern::)?(syscall::)?SYS_DEBUG_KPRINT\b" --include=*.rs userspace | grep -v
  '^userspace/libkern/'` = 10, the same for `SYS_PROCESS_EXIT` = 15, heartbeat in both), such
  as `userspace/heartbeat/src/main.rs:71`, `userspace/heartbeat/src/main.rs:92`,
  `userspace/session-mgr/src/main.rs:36`, `userspace/desktop-shell/src/main.rs:118`. Delete the
  local fns and use `libkern::{kprint, exit}`; heartbeat's `kprint_u32` becomes
  `libkern::kprint_u64` or `debug::Line`. Home: libkern::debug (already there). Closes: nothing
  new; removes 24 unsafe blocks and the spin-after-exit loops that paper over a non-diverging
  `syscall1`. Depends on: nothing. abi. Value low, effort small. **Plan**, in P7.

- [ ] **rt-o10** remove: `static mut` buffers in hello and heartbeat used as syscall out-params and
  receive buffers, with the `unsafe` `channel_recv_raw` used where the safe `channel_recv` with
  a stack buffer would do — 19 sites (`grep -rnw "static mut" userspace/hello/src
  userspace/heartbeat/src` = 12, hello 8 and heartbeat 4; `grep -rn "channel_recv_raw("` = 5, hello
  :261, :275, :302, :312 and heartbeat :161; heartbeat's local kprint/exit are rt-o9; 299
  `static mut` in 38 files and 53 `channel_recv_raw` against 18 `channel_recv` userspace-wide),
  such as `userspace/hello/src/main.rs:33`, `userspace/hello/src/main.rs:275`,
  `userspace/heartbeat/src/main.rs:66`, `userspace/heartbeat/src/main.rs:161`. Stack locals
  (the default user stack is 8 MiB, abi.rs:26; heartbeat already keeps 4 KiB send buffers
  there) passed as `&raw mut` to the remaining raw syscalls, and `libkern::channel_recv(ch,
  &mut [0; IPC_MSG_SIZE])` for receives. hello's raw syscalls stay, since exercising them is its
  job. Home: hello and heartbeat (no new code). Checked against heartbeat `recv_log_handoff`, hello's
  four receives and its `STAT_BUF`/`CLOCK_BUF`. Closes: a data race on a shared static the day
  either program grows a second thread (`thread_create` is one call away). Depends on: rt-o1
  for the CLOCK/STAT statics to lose their remaining `unsafe` and not just the `static`.
  soundness, opportunities. Value low, effort small. **Plan**, in P7.

- [x] **rt-o11** narrow: 6 of libos's 13 `cfg(not(test))` seam functions in `sys.rs` are safe but
  take an address as `u64` or rely on an unchecked length: `wait` (`results.len() >=
  handles.len()` held by comment), `memory_unmap(addr)`, `ns_lookup(path, len)`, `ns_bind(path,
  len)`, `ns_enumerate(out)`, `notif_recv(out)` — 6 sites (read of sys.rs, not a grep; the
  module is private and every caller is correct today), such as
  `userspace/libos/src/sys.rs:60`, `userspace/libos/src/sys.rs:106`,
  `userspace/libos/src/sys.rs:120`, `userspace/libos/src/sys.rs:127`. Typed signatures:
  `ns_lookup(ns, path: &[u8], rights)`, `ns_bind(ns, path: &[u8], res)`, `ns_enumerate(ns, i,
  out: &mut NsEntry)`, `notif_recv(q, out: &mut Notification)`, `wait` taking `min(handles.len(),
  results.len())` (or `&mut [IoResult; MAX_WAIT]`), and `memory_unmap` as `unsafe fn` or
  reached only from `Mapping::drop`. The mock takes the same types and records lengths, so host
  tests exercise what the real call is handed. Home: libos sys.rs. Closes: the rt-d10 overrun
  the mock hides, and the shape of the rt-d2 defect. Depends on: rt-o3 for `memory_unmap`.
  abi, ownership, soundness, opportunities. Value low, effort small. **Fix.**
  Done in `904504f0`: `ns_lookup` and `ns_bind` take `&[u8]`, `ns_enumerate` an `&mut NsEntry`,
  `notif_recv` an `&mut Notification`, `wait` cuts its slices; `memory_unmap` is unsafe with
  rt-d2. The mock answers as the kernel does and records what a wait was handed.

- [x] **rt-o12** narrow: libheap's engine turns `usize` addresses back into pointers with `as *mut`,
  so pointers it hands out carry only exposed provenance and Miri runs it only in permissive
  mode — 5 sites (`grep -nE "\) as \*(mut|const) [A-Za-z]|[a-z_]+ as \*mut u8"
  userspace/libheap/src/lib.rs`: lines 225, 228, 271, 278, 288; :345 is the kernel's address
  and inherent), such as `userspace/libheap/src/lib.rs:225`, `userspace/libheap/src/lib.rs:271`,
  `userspace/libheap/src/lib.rs:288`. Keep the arena base as `*mut u8` and derive slot and
  header pointers with `base.with_addr(aligned)` / `ptr.map_addr(..)` (stable since 1.84); the
  `ArenaSource`'s integer becomes a pointer once, at map. Home: libheap. Closes: provenance-
  dependent UB the host tests cannot see; today Miri with `-Zmiri-strict-provenance` stops at
  lib.rs:225 on `many_allocations_grow_arenas_without_overlap`. Depends on: nothing.
  soundness, opportunities. Value low, effort small. **Fix.**
  Fixed in `d49969f0`: every pointer libheap hands out is derived from its mapping's pointer, and
  the one address-to-pointer step is the kernel's mapping; fails without it, under Miri's strict
  provenance: the crate's 9 tests, which stopped at `lib.rs:225`.

- [ ] **rt-o13** remove: `DeviceRecord::read` and `as_bytes` reinterpret bytes as the struct with
  `unsafe`, while the same file's `IoSnapshot::get` and abi.rs's `FramebufferInfo::from_bytes`
  already decode field by field in safe code — 2 sites (`grep -n 'unsafe'
  userspace/libkern/src/device.rs`: :197 `read_unaligned`, :204 `from_raw_parts`), such as
  `userspace/libkern/src/device.rs:197`, `userspace/libkern/src/device.rs:204`,
  `userspace/libkern/src/device.rs:437`. Field-wise little-endian decode in `read`, and a
  `to_le_bytes() -> [u8; 144]` encoder for device-mgr's Arrived body (device-mgr/src/main.rs:567).
  Home: libkern device.rs. Closes: a reinterpretation sound only while the struct has no
  padding, which only offset asserts hold today. Depends on: nothing. opportunities. Value
  low, effort small. **Later.**

What held up: libkern's `syscall6` asm (no `nomem`/`readonly`, rcx/r11 clobbered, matching the
kernel entry stub, which preserves rdi/rsi/rdx/r8/r9/r10) and `exit`'s `noreturn` (the kernel's
`sys_process_exit` is `-> !`). Every `size_of`/`offset_of` assert in libkern against the kernel's
(SpawnArgs 104, ThreadArgs 64, IpcMsg 4096, IoResult 24, IoOp 40, HandleInfo 24, NsEntry 272,
Notification 64, RegisterValues 144, DeviceRecord 144, and the rest). `channel_recv_raw` and
`Received`: exact buffer types, count clamped, `take`/`take_all` zero what they hand out, `Drop`
closes each remaining handle once, and an error holds nothing (the kernel rolls back at
table.rs:2808-2829); Miri clean. `DeviceRecord::read`/`as_bytes` and the `device.rs` readers
(checked arithmetic, length-checked, `name_len` clamped); Miri clean. `mem*` and `scrub`.
libheap: slot alignment, large-header placement for every power-of-two align, no allocation on
the grow path, unmap before close, a `Guard` that drops on every early return, OOM returns null;
Miri clean under permissive provenance on three tests. libfs: every handle and mapping released on
every path (13 close sites traced), and a stale size cannot overrun because the kernel bounds a
file mapping by npages (table.rs:891-900) and a truncated page's frame stays for mappings that
faulted it in. Handle generations turn a stale or double close into `InvalidHandle`. Spawn is
atomic-or-fail. 14 of 15 `po_wait` clones close their PO; 16 `lookup_wait` callers sampled
close theirs. `enter_user` gives `_start` an RSP that is 8 mod 16, matching the SysV alignment hard-float
spills need; heartbeat's `_start` matches the kernel's boot handle order. hello and heartbeat
`static mut` are single-threaded and touched only through `&raw`. liblog's 4072-byte window is
bounded by the kernel's `seal`, so the documented drop holds. The kernel's `sys_ns_lookup` copies
the path synchronously. libtime has no unsafe and bounds every field before arithmetic. The
`Waker` fix in rt-d1 was applied as a control and passed all 19 libos tests and both Miri
probes.

### supervision — main at 135055a8, 2026-10-27

Lenses: abi, ownership, soundness, opportunities; returned nothing: none.
Defects: found 35, refuted by a boot 0, reported 14 (35 findings merged into 14 distinct defects).
Opportunities: 12 (29 proposals merged).

The inventory, `cargo xtask unsafe-inventory` over its paths:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `userspace/init` | 838 | 78 | 2 | 0 | 16 | 55 | 2 | 5 | 0 | 1 | 0 |
| `userspace/libinittoml` | 335 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `userspace/service-mgr` | 2318 | 59 | 1 | 0 | 15 | 43 | 2 | 4 | 0 | 0 | 0 |
| `userspace/device-mgr` | 939 | 26 | 0 | 0 | 5 | 19 | 3 | 0 | 0 | 0 | 1 |
| `userspace/storage-service` | 2175 | 38 | 0 | 0 | 5 | 32 | 3 | 0 | 0 | 0 | 0 |
| `userspace/logging-service` | 718 | 24 | 0 | 0 | 10 | 11 | 3 | 2 | 0 | 0 | 0 |
| `total` | 7323 | 225 | 3 | 0 | 51 | 160 | 13 | 11 | 0 | 1 | 1 |

**Defects**

- [x] **sup-d1** `userspace/logging-service/src/main.rs:392` — `received()` is a safe fn that
  returns a `librsproto::Message<'static>` borrowed from `static mut RECV_MSG`, and the next safe
  `recv()` has the kernel overwrite that buffer while the borrow can still be live. Any caller
  that keeps the message across another receive reads another client's bytes behind a live shared
  reference, which is UB. Examples are a reworked `serve_read_session` that receives the next
  request before answering this one, or `drain_source` peeking ahead. Today's callers
  (`serve_read_endpoint` 415, `serve_read_session` 446) drop `m` before their next `recv`.
  Demonstrated: a probe that held `received()` across `recv(h)` in safe code built clean with
  `cargo check --release --target ../x86_64-unknown-nitrox.json -Z build-std=core,alloc,compiler_builtins`.
  Miri on a replica of the pair reports `Undefined Behavior: trying to retag ... for
  SharedReadOnly permission ... that tag does not exist in the borrow stack`. Probes restored.
  abi, ownership, soundness, opportunities. Medium. **Fix.**
  Fixed in `48235002`: the receive buffer is an `Inbox` the serve loop owns, whose `message()` and
  `payload()` borrow from it, so no receive can happen while one is held. A compile probe holding a
  message across `inbox.recv` fails with E0502.
- [x] **sup-d2** `userspace/storage-service/src/main.rs:1115` — `serve_dir` sizes its ReadDir body
  buffer as the IPC payload (`MSG_LEN - PAYLOAD_OFF - 64` = 4008 bytes) instead of `RS_MAX_BODY`
  (3980), and `let _ = send(..)` at 1128-1129 drops the refusal. A listing that fills past 3980
  bytes sends no reply. A USB stick with a 128-entry GPT gives 130 entries. 117 pack into 3993
  bytes, `librsproto::encode` returns `None`, and `ls`, Files or `disk` on `/svc/storage/info` or
  `/dev/storage/info` hangs for good, because `librsproto::session` round trips wait with
  `u64::MAX`. device-mgr (main.rs:535) and libfsserver (server.rs:68) size the same buffer to
  `RS_MAX_BODY`. Demonstrated: a temporary host test replaying `serve_dir`'s loop over
  `table::entries` for 1 disk + 128 partitions printed `entries=130 packed=117 body=3993
  RS_MAX_BODY=3980 encoded=None`. The control with an `RS_MAX_BODY` buffer printed `packed=116
  body=3958 encoded=Some(3986)`. The kernel publishes every in-use GPT entry (up to 128) and
  device-mgr's replay depth reaches them. Test removed. abi, soundness, opportunities. Medium.
  **Fix.**
  Fixed in `796a06cf`: the page is `fit::dir_page`, in the library, sized to `RS_MAX_BODY`, and a
  reply not sent is answered with an error. `a_full_gpts_listing_pages_into_replies_that_encode`
  fails with the old 4008-byte buffer.
- [x] **sup-d3** `userspace/service-mgr/src/main.rs:1248` — an admin Start or Restart of a
  declaration that bring-up has not reached yet is allowed, and when bring-up reaches it,
  `start(i, true)` spawns it again and overwrites `proc_h` and `ctrl` without closing them.
  Neither `advance()` nor `start()` checks `running`. On the test image, nxterm waits (`after`)
  for display-selftest. An admin Start of nxterm in that window spawns it, `started` is unchanged,
  and when display-selftest exits `advance()` returns `Step::Start(nxterm)`. The first instance's
  process handle and control channel leak, two instances run, and the first one's exit is never
  attributed. For a server, the first instance's Ready is never read. Unverified: a host test in
  lib.rs showed `decide(OP_SERVICES_START, Found{running:false,..}) == Ok(Action::Start)` and
  `bringup::next(entries, started=2, true) == Step::Start(2)`. The end-to-end overwrite is in
  main.rs, which does not build on the host. A test-image boot whose probe sends an admin Start of
  nxterm while display-selftest runs would print `starting service 'nxterm'` twice. The fix is to
  skip or refuse when `running` is true. ownership. Medium.
  **Fix**, with the bash's sup-9, moved here from Part H.
  Fixed in `106601be`, with sup-9: `decide` refuses a Start, or the Restart of a service not
  running, that bring-up has not reached, with `WouldBlock`; the maintainer's choice of the two.
  `a_start_bring_up_has_not_reached_is_refused` fails without it.
- [x] **sup-d4** `userspace/service-mgr/src/main.rs:1369` — `lookup_serving` closes its lookup's
  PendingOperation at the deadline, but the kernel still installs the server's late reply handle
  in service-mgr's table, where nothing knows its value. `derive()` resolves device-mgr's
  `info-endpoint` or storage-service's `session-endpoint` with `DERIVE_TIMEOUT_NS` (5 s), and
  storage-service can block up to 30 s inside mount/wait_ready. After the deadline, the late reply
  is installed into `pl.owner_pid` (kernel table.rs:2195-2214), and `deliver_to_client` (822-828)
  closes it only if the owner has no threads. One handle leaks in a process that never exits. The
  server also keeps its minted endpoint counted against its bound (`MAX_INFO_ENDPOINTS` 2,
  session_ends 4), so two late answers leave device-mgr refusing `WouldBlock` until it restarts.
  This repeats on each restart of the server. Siblings: init `ns_lookup_wait`:145-148, service-mgr
  `ns_lookup`:139-140, device-mgr `lookup`:146 and storage-service `po_wait`:161 close the PO
  without its result, but only when `sys_wait` itself fails. Unverified: code read of
  `lookup_serving`, `complete_resolve_reply` and `deliver_to_client`. A boot where device-mgr
  answers `info-endpoint` more than 5 s late (a probe sleep in `mint_info_endpoint`) would show
  `info_ends.len()` at 1 after service-mgr logged "gave no info-endpoint" and service-mgr's
  handle count growing. ownership, opportunities. Medium. **Fix**, as rt-d6.
  Fixed in `fc9ce772`, as rt-d6: service-mgr's late answers no longer leave a handle or hold
  device-mgr's endpoint slot.
- [x] **sup-d5** `userspace/init/src/main.rs:420` — init discards the process handle of every
  fs-server and of the profile server it spawns (`let _ = fs_h;`, `let _ = ps_h;` at 559), and
  leaks it on every failure after the spawn: 377-410 and 516-553. The comments say init keeps
  them, but no static or list holds the value. A Process drops its AddressSpace only when the
  object drops, so an exited server's address space stays pinned in PID 1 for the life of the
  machine. The bound is the number of `[[mount]]`s plus 1. This is reached when a server refuses
  its disk, never sends Ready, or exits at shutdown. `init/CLAUDE.md`'s "On ChildExited, close the
  process handle" does not happen. `spawn_eshell` (613) and `supervise` (728) do close theirs.
  Demonstrated: `grep -n 'fs_h\|ps_h' userspace/init/src/main.rs` shows only the spawn, the `< 0`
  test and the discard, with no store and no close. Unverified: that the frames stay pinned in a
  running system; a boot that kills a mount's server and reads free memory would settle it. abi,
  ownership, soundness, opportunities. Low. **Fix.**
  Fixed in `e6a4ecf2`: every spawn goes through libos's safe `spawn`, and init drops each process
  handle at once, as it already closed eshell's and service-mgr's, so a server's exit frees it.
  Structural; the gates pass.
- [x] **sup-d6** `userspace/service-mgr/src/main.rs:402` — safe functions take raw pointers that
  they dereference, or hand to the kernel to write and then read. `spawn_with_control(root_ns,
  path, args: *mut SpawnArgs)` writes `(*args).image`, `handles`, `handle_count` and `move_mask`.
  logging-service `make_channel(o0: *mut u64, o1: *mut u64)` (main.rs:169) has the kernel write
  two handles through them and then `read`s them. Safe code can pass null, dangling or a live
  `String`'s address, and the kernel writes handle numbers over it, corrupting the heap with no
  `unsafe` at the caller. The kernel rejects only null, misaligned or out-of-user-half pointers.
  Today every caller passes `&raw mut` of a matching static (spawn_with_control: 549, 578;
  make_channel: 347, 379, 425, 722). init's equivalent `spawn_program` is correctly an `unsafe
  fn`, and the other crates' `make_channel(depth)` use locals. Demonstrated: probes calling
  `spawn_with_control(0, b"/bin/x", core::ptr::null_mut())` and `make_channel(dangling, dangling)`
  from safe fns built clean for `x86_64-unknown-nitrox`. A positive control in service-mgr (a
  `u8 = "str"` mismatch) gave `E0308`, so the bin is type-checked. Probes restored. abi,
  ownership, soundness, opportunities. Low. **Fix.**
  Fixed in `f013319d` and `48235002`: `spawn_with_control` takes its `SpawnArgs` by value and spawns
  through libos, and logging-service's `make_channel` takes no pointers.
- [x] **sup-d7** `userspace/storage-service/src/main.rs:399` — `DeviceIo::read_at`'s SAFETY
  argument `intra + want <= span <= SCRATCH` does not hold, because the record's
  `logical_block_size` is taken unvalidated from a USB stick's READ CAPACITY (kernel scsi.rs:96,
  `try_new_block`). A 520-byte block at offset 0 with a 64 KiB read gives `span` = 66040 > 65536.
  A 128 KiB block gives `span` = 131072, and at offset 70000 `SCRATCH - intra` (line 379) wraps in
  release and panics in debug. The slice at 401 is sound only because the kernel refuses an `IoOp`
  whose `buf_offset + length` passes the buffer (table.rs:1650) and the code requires `moved ==
  span`, and the comment states neither. Today such a stick's reads fail with `FsError::Io`, so
  every probe of it reads as empty. A change that maps the scratch larger than its object, or
  drops the `moved != span` check, would read past the mapping. Demonstrated: the lines 377-380
  arithmetic replayed in Python gives span 66000 (3000), 66040 (520) and 131072 (131072), all
  `span<=SCRATCH` False, and True for 512 and 4096. abi, soundness, opportunities. Low. **Fix.**
  Fixed in `4ddff7dd`: the arithmetic is `fit::scratch_read`, in the library, which refuses a block
  that does not divide the scratch, and the SAFETY comment names what holds it.
  `a_block_that_does_not_divide_the_scratch_is_refused` fails on 520 without the check.
- [x] **sup-d8** `userspace/init/src/main.rs:181` — init maps exactly one page of
  `/initramfs/etc/init.toml` ("assumed to fit") and parses to the last non-NUL byte of that page,
  while the storage service maps the object's whole `HandleInfo.size` (main.rs:313-321, 457-459),
  so the two consumers `libinittoml` exists to keep in agreement read different text. A manifest
  over 4096 bytes cut at an entry boundary has init mounting only the first page's entries, while
  the storage service parses all of them, believes the rest are init's, and mounts those devices
  nowhere. Cut mid-entry, init drops to eshell for a valid manifest. Today's manifest is about 200
  bytes (tools/xtask/src/image.rs:38), so this is latent. Demonstrated: a temporary test in
  `libinittoml/src/manifest.rs` with a comment-padded manifest whose first `[[mount]]` ends at
  byte 4096, run as `cargo test -p libinittoml --lib probe_a_manifest_past_one_page`, printed
  "init (one page) sees 1 mount(s); the storage service (whole object) sees 2". The kernel maps a
  prefix of a larger object without complaint (table.rs:878). Test removed. abi, opportunities.
  Low. **Fix.**
  Fixed in `e6a4ecf2`: init maps the whole object, and both consumers read its text through
  `libinittoml::manifest::text`. `a_manifest_past_one_page_is_read_whole` fails with `text` cut to a
  page, or ended after the last non-NUL as init did.
- [x] **sup-d9** `userspace/init/src/main.rs:181` — init maps the init.toml page (181/189) and
  `/system/current-generation` (577/585) and never unmaps either, so PID 1 keeps both mappings,
  and the second pins a root-filesystem FileObject through shutdown's sync and `Meta::Unmount` of
  `/`. libos `Memory::map` returns a raw `*mut u8` documented "valid while `self` lives", but the
  kernel mapping outlives the handle. read_manifest's comment at 239-240 says the mapping was kept
  on purpose; read_current_generation has no such comment. Demonstrated: `grep -n -i
  'unmap\|MEMORY_MAP\|\.map(' userspace/init/src/main.rs` finds the two maps and no unmap.
  ownership, opportunities. Low. **Fix**, as rt-d5.
  Fixed with rt-d5, in `2cde6627`: init unmaps both pages once read.
- [x] **sup-d10** `userspace/device-mgr/src/main.rs:176` — `read_registry` and `read_io` (176,
  204) and storage-service `with_mapped` (main.rs:327) unmap with size 0, which works only because
  `sys_memory_unmap` ignores `size` today (table.rs:949, `_size` unused). syscall-abi.md:529 says
  the size "is not yet honored" and plans to. Once honoured, every hotplug re-read, every `io.tsm`
  resolve and every manifest read would leave a mapping that pins a snapshot object. The same
  files pass the real length elsewhere (device-mgr 226, storage 486). Latent until that kernel
  change. Demonstrated: the three call sites pass 0 and the kernel ignores it, by code read. No
  test can run it today. ownership, opportunities. Low. **Fix.**
  Fixed in `4a9835ee`: each passes `info.size`, what it mapped. Latent until the kernel honours the
  size, so no test reaches it.
- [x] **sup-d11** `userspace/device-mgr/src/main.rs:654` — the `/dev/registry/changes` handle is
  dropped without a close when the change page cannot be created or mapped: the struct literal
  stores `changes: if change_at == 0 { 0 } else { changes }` and the other arm never closes it, so
  the handle stays in device-mgr's table for its life. This is reached when `SYS_MEMORY_CREATE` or
  `SYS_MEMORY_MAP` fails at startup. Demonstrated: code read of main.rs:637-654. ownership. Low.
  **Fix.**
  Fixed in `fcc30620`: the page is made only once the node resolved, and both handles are closed if
  it cannot be mapped. Reached only by a failed create or map at startup.
- [x] **sup-d12** `userspace/device-mgr/src/main.rs:694` — the main loops of device-mgr (694-731)
  and storage-service (main.rs:1477-1519) read wait record `j` lazily from `static mut
  WAIT_RESULTS` after the handlers for earlier records have run nested waits (`lookup`, `po_wait`,
  `wait_until`, `park`) into the same static. This is correct only because every nested wait has
  one handle and so rewrites record 0, which the loop has already read. A nested wait that takes
  two handles is the ordinary change that breaks it: `lookup` that also answers its endpoint while
  it waits (the `lookup_serving` pattern service-mgr's CLAUDE.md requires), or `wait_ready` plus a
  timer. Records j+1.. are then overwritten, and the loop dispatches on another wait's handle and
  status, for example `follow(status)` with a lookup's status. Paths: device-mgr `follow` ->
  `read_registry` -> `lookup` (142), `arrived` -> `lookup`, `serve_resolve(io)` -> `read_io` ->
  `lookup`; storage `serve_resolve(File)` -> `refresh_unmounted` -> `read_at` -> `po_wait` (155),
  `unmount` -> `held_files` -> `park` -> `wait_until` (132). service-mgr already uses a stack
  buffer in `wait_serving`. Unverified: main.rs is not host-testable. A host test of an extracted
  dispatch with a two-handle nested wait would settle it. abi, ownership. Low. **Fix.**
  Fixed in `796c3add` (storage) and `c96ba220` (device-mgr): each loop's wait records are a local
  array of its own, out of any handler's reach. Structural.
- [x] **sup-d13** `userspace/storage-service/src/main.rs:161` — `po_wait` closes the PO and
  reports failure when `sys_wait` returns without it signalled, though the read it waited on may
  still be in flight into the shared Scratch object. `read_at` returns `Err(Io)` and the next
  `read_at` submits into the same buffer at offset 0 while the first IRP can still write there.
  The `&[u8]` formed at line 401 for the second read then covers bytes the device is still
  writing, and the probe or partition-table parse can read the first transfer's data. A trigger is
  `sys_wait` returning `OutOfMemory` from waiter registration (kernel sched/wait.rs:182-190). This
  is the class of #396's early `Ended`. Unverified: needs fault injection that fails
  `obj_add_waiter` for one PO while its IRP is in flight, then a check of what the next read
  returns. No boot was run; widening a window is not allowed. A host test faking a wait that
  returns unsignalled and then submitting a second read into the same Scratch would show it.
  ownership, soundness. Low. **Fix.**
  The trigger named above cannot fire: this `po_wait` waits on one PO with no deadline, where
  registering a waiter fails only past `MAX_WAITERS` (four, reserved at construction) and no
  deadline entry is pushed. The item is latent, as fs-d4 says of the same helper.
  Fixed in `951a71f0`: `po_wait` waits again while the answer is `OutOfMemory`, so it reports a read
  only once it has completed. Structural, and latent, as noted above.
- [x] **sup-d14** `userspace/init/src/main.rs:692` — handoffs are read by position, and a handoff
  whose send fails is closed with nothing sent in its place, so every later handle shifts one
  slot. The code sends an empty message for a zero handle but not for a failed send. Same in
  service-mgr `send_handle` (478-481) for the nine login-chain handoffs. If init's first send
  fails (kernel OOM on the bounce allocation, or a handle without TRANSFER, table.rs:1962-1976),
  service-mgr takes the fs endpoint as its bind-righted root (619-623), so every bind fails. It
  takes the profile endpoint as fs, so each session binds `/home` to the profile server, and the
  third receive waits 30 s. A failed tty send to session-mgr shifts clip into tty, and so on.
  Unverified: needs a send failure injected mid-sequence. Read of init main.rs:676-699 and
  service-mgr 463-482, 556-560 and 584-586. ownership. Low. **Fix.**
  Fixed in `f013319d` (service-mgr) and `e6a4ecf2` (init): a refused handle is closed and an empty
  message sent in its place, as for a zero one. Structural: no test injects a send failure.

**Opportunities**

- [ ] **sup-o1** encapsulate: one safe `Result`-returning wrapper per register-only, slice-input
  or owned-out-param syscall, where each site today is its own `unsafe` block whose SAFETY
  argument is only handle ownership — 154 sites (`grep -ohE 'syscall[0-9]\(\s*SYS_[A-Z_]+'` plus
  `grep -A1 'syscall[0-9]\($'` for the 17 multi-line calls, over the six source files: 160 raw
  syscalls, less the 6 `SYS_MEMORY_UNMAP`, which the mapping guard takes), such as
  `userspace/init/src/main.rs:335`, `userspace/service-mgr/src/main.rs:597`,
  `userspace/logging-service/src/main.rs:169`, `userspace/device-mgr/src/main.rs:156`. Of them, 78
  are register-only (46 handle close, 9 map, 8 duplicate, 5 memory create, 2 terminate, 2 exit, 2
  ns_create, timer x2, power, test_exit) and 76 pass pointers to locals or slices. The wrappers
  are `handle_close(h)`, `handle_duplicate(h, rights) -> Result<u64>`, `handle_stat(h) ->
  Result<HandleInfo>`, `channel_create(depth) -> Result<(u64, u64)>`, `clock_read(clock) ->
  Result<u64>`, `ns_lookup/bind/unbind/sync/held(ns, &[u8], ..)`, `ns_create`, `memory_create`,
  `process_terminate`, `notif_recv`, `timer_*`, `power`; checked against init's three channel
  creates, logging `make_channel`, service-mgr `dup_endpoint` and device-mgr's stat-then-map.
  Home: libos, its `sys` seam made public (it already holds 11 of these over the host mock), or
  libkern, whose CLAUDE.md already asks for "thin Result<> wrappers" that do not exist. Five
  crates re-implement the same helpers (22 redefinitions: `close` x4, `make_channel` x4,
  `now_ns`/`clock_now` x4, `kprint`, `exit`). Closes: safe functions that have the kernel write
  through a caller's raw pointer (sup-d6); static out-params; `unsafe fn`s whose contract is not
  memory (`dup_endpoint`, `close_retained_endpoints`). It leaves `unsafe` only where memory
  crosses: wait, send/recv, map, spawn. Depends on: Part E's syscall changes, so callers move
  once (they would rename wrappers, not add sites). abi, soundness, opportunities. Value high,
  effort medium. **Plan**, as rt-o1, in P7.
- [ ] **sup-o2** encapsulate: raw `u64` handles owned by convention and closed by hand at each
  exit, in locals, struct fields, `Vec<u64>`s and handle-holding statics — 143 manual close
  points (`grep -nE '(^|[^_a-z.])close\(' <the six files> | grep -v 'fn close('` = 101 helper
  calls, plus `grep -n SYS_HANDLE_CLOSE` = 46 raw closes less the 4 inside the helpers = 42), such
  as `userspace/init/src/main.rs:337-421` (`mount_one`'s five failure paths each close a different
  subset, and none closes `fs_h`), `userspace/service-mgr/src/main.rs:1545-1575` (`reap`),
  `userspace/storage-service/src/main.rs:1016` (Mount's `ns`, `control` and `process` closed by
  hand at each of three ends), `userspace/device-mgr/src/main.rs:525`. Use libos's owning
  `Handle<T, M>` (Drop closes) with the `Channel` marker and `Send/Recv/SendRecv` modes that
  `libos.md` designs and libos lacks (`grep Channel libos/src` finds none), an owning `Received`
  that yields `Handle`s, and a `send(msg, handles)` that consumes what it moves and returns it on
  failure. `Mount`, `Supervised` and `Vec<Handle>` then end by drop. Home: libos (handle.rs,
  objects.rs). Closes: handles leaked on error paths (sup-d5, sup-d11) and overwrite leaks
  (sup-d3), and double closes made possible by copied teardown. Generation numbers already make a
  double close harmless, so leaks are the class closed. Depends on: sup-o1 and the send API in
  sup-o4, and Part E. ownership, opportunities. Value high, effort large. **Plan**, in P7.
- [x] **sup-o3** remove: `static mut` used as scratch, out-param, spawn block or process state
  where a local or a struct field does the same job — 51 sites (`grep -n '^static mut'` over the
  five mains: init 16, service-mgr 15, logging 10, device-mgr 5, storage 5), such as
  `userspace/service-mgr/src/main.rs:182` (CTRL_OUT0/1 read back through raw pointers, while
  `make_channel` at 686 in the same file uses locals), `userspace/init/src/main.rs:67-112` (four
  static SpawnArgs, where storage-service `mounting.rs:24` builds a local),
  `userspace/service-mgr/src/main.rs:169-172` (CLOCK_BUF for one clock read),
  `userspace/init/src/main.rs:30-31`. These include 11 out-param scalars (CTRL0/1, CTRL_OUT0/1,
  SRC_OUT0/1, CLOCK_BUF, NOTIF x2), 7 SpawnArgs, 6 state handles and 27 buffers. A classifier over
  all 225 `unsafe {` blocks found 39 whose only unsafe operation is a static access and 55 more
  where it is half of the unsafety. Out-params become locals, and process state becomes fields of
  an `Init` struct like service-mgr's `Mgr`. Buffers become owned by the loop struct
  (`Box<[u8; 4096]>`) or the stack, which is 8 MiB (kernel arch/x86_64/abi.rs:26). Home: each
  crate's main.rs. Closes: reentrancy through a shared buffer (sup-d12), ownership by convention
  in init's state statics, and the static that makes `received() -> Message<'static>` (sup-d1)
  possible. Depends on: nothing. opportunities, ownership. Value high, effort medium. **Fix.**
  Fixed in each crate's commit: storage `796c3add`, device-mgr `c96ba220`, logging `48235002`,
  service-mgr `f013319d`, init `e6a4ecf2`. Each static is a local, a field or a value passed down;
  none of the 51 is left.
- [ ] **sup-o4** encapsulate: IPC messages through `static mut [u8; 4096]` buffers, the header
  parsed by byte offset, the payload sliced with `from_raw_parts(add(24), len.min(4072))`, sends
  framed by hand into `[24..]` — 34 sites (`grep -rn "channel_recv_raw("` = 10 receives, 9
  payload slices, `grep -rn SYS_CHANNEL_SEND` = 15 sends; they serve 17 buffer statics), such as
  `userspace/logging-service/src/main.rs:392`, `userspace/storage-service/src/main.rs:285`,
  `userspace/service-mgr/src/main.rs:715`, `userspace/init/src/main.rs:364`. Each server owns a
  `MsgBuf`/`Box<IpcMsg>`. libkern's `IpcMsg` gains `payload(&self) -> &[u8]` clamped to
  `IPC_PAYLOAD_SIZE` and `frame(&mut self, op, request_id, flags, body, handle_count) ->
  Option<()>`. `channel_recv(ch, &mut IpcMsg) -> Result<Received, i64>` hands out views borrowed
  from the buffer. A safe `channel_send(ch, &IpcMsg, handles, mode)` hands handles back on
  failure, with a `librsproto::Body` of exactly `RS_MAX_BODY` for writers such as
  `DirReplyWriter`. Checked against device-mgr `send`/`recv`, init `wait_ready`, logging
  `reply_records` and `drain_source`. Home: libkern (`channel`, `abi::IpcMsg`) beside the existing
  `channel_recv` and `Received`, with a librsproto reply helper above it. Closes: a `'static`
  borrow of a receive buffer held across the next receive (sup-d1), a body buffer sized as the
  payload (sup-d2), the 4072-vs-4008 payload clamp at 10 sites including libkern `control.rs:33`,
  stale header flags carried from a receive into the next send through one static (init
  `IPC_MSG`), and positional handoffs with nowhere to send a placeholder (sup-d14). Depends on:
  nothing; easier after sup-o1. abi, ownership, soundness, opportunities. Value high, effort
  medium. **Plan**, as rt-o5, in P7.
- [ ] **sup-o5** encapsulate: `sys_wait` through static `WAIT_HANDLES`/`WAIT_RESULTS`, each
  `IoResult` decoded by byte offset (status at 8..12, result at 16..24) — 14 sites (`grep -rn
  SYS_WAIT` over the five src dirs: init 4, service-mgr 4, storage 3, device-mgr 2, logging 1; 13
  through the 10 static buffers, one through locals at service-mgr:1342; 9 hand-decoders), such as
  `userspace/init/src/main.rs:122-143`, `userspace/device-mgr/src/main.rs:692-701`,
  `userspace/storage-service/src/main.rs:151-160`, `userspace/service-mgr/src/main.rs:1331-1346`.
  `wait<'r>(handles: &[u64], out: &'r mut [IoResult; MAX_WAIT_HANDLES], deadline) -> Result<&'r
  [IoResult], KError>` refuses `handles.len() > MAX_WAIT_HANDLES` and returns typed results.
  Add a `WaitSet<const N>` whose `push(h) -> Result<(), Full>` replaces the silent drop past 32
  (service-mgr, device-mgr, storage) and the panic past it (logging), and a `wait_one(h, deadline)`
  for nested waits. Results come back as a borrowed slice of a caller-owned array, so the borrow
  checker forbids a nested wait clobbering an outer loop's records. Checked against init
  `ns_lookup_wait`, device-mgr's dispatch loop, service-mgr `wait_serving` and storage `po_wait`.
  Home: libos (`sys::wait` exists privately, without the length check). Closes: nested waits
  overwriting an outer loop's results (sup-d12); status read at the wrong width, as storage
  main.rs:157 records ("read whole, AlreadyExists would never compare equal"); results buffers
  sized by expectation; a handle dropped silently at capacity. It removes all 10 wait statics.
  Depends on: nothing (sup-o1 helps). abi, ownership, soundness, opportunities. Value high, effort
  medium. **Plan**, as rt-o2, in P7.
- [ ] **sup-o6** encapsulate: each server hand-writes the same reply plumbing (send, `reply_error`,
  `reply_channel`, `reply_with_object`/`reply_table`, `reply_subnamespace`, `serve_dir`), and the
  copies have drifted on the rules each must keep — 19 sites (`grep -nE 'fn
  (send|send_raw|send_rs|reply_error|reply_channel|reply_with_object|reply_table|reply_subnamespace|serve_dir)\b'`;
  15 header writes by byte offset), such as `userspace/device-mgr/src/main.rs:209`,
  `userspace/storage-service/src/main.rs:214`, `userspace/storage-service/src/main.rs:1115`,
  `userspace/service-mgr/src/main.rs:795`. One difference is that device-mgr's `reply_with_object`
  and `reply_channel` callers (318-324, 352-356, 378-383) close and answer nothing on a failed
  send, though a forwarded resolve waits with no deadline, while storage's `reply_with_handle` and
  service-mgr's `reply_channel` answer an error. Another is that service-mgr's `refuse` uses a
  fixed `ERROR_BODY_LEN + 160` body where storage's `error_reply` (1315) sizes to the reason after
  a 108-byte reason was lost. A `librsproto::server` module offers `send`, `reply_error` sized to
  the reason, `reply_object(ch, id, bytes)` (create, map, copy, unmap, narrow, send, error reply
  on any failure), `reply_handle` that answers an error when the send fails,
  `reply_subnamespace`, and a `serve_read_dir` bounded by `RS_MAX_BODY`. Checked against the three
  `reply_with_object` copies, the four `reply_error` copies and the two `serve_dir`s. Home:
  librsproto (server-side module; the four servers link it, init does not serve). Closes: bodies
  sized against the wrong bound (sup-d2), a failed reply that hangs a forwarded resolve, and IPC
  header bytes stamped by offset at 15 sites. Depends on: sup-o4 and sup-o7. opportunities. Value
  medium, effort medium. **Plan**, in P7.
- [ ] **sup-o7** encapsulate: memory mapped by hand (stat, map, `from_raw_parts` or
  `copy_nonoverlapping`, unmap with a caller-supplied length, or never) — 10 map sites with 6
  unmaps (`grep -rn "SYS_MEMORY_MAP\|SYS_MEMORY_UNMAP\|\.map(PAGE"` over the five src dirs; 6
  mapping slices at init 189 and 585, device-mgr 170 and 201, storage 325 and 401, and 3
  create-map-copy-unmap reply copies at service-mgr 825-861, device-mgr 209-232, storage
  469-490), such as `userspace/init/src/main.rs:181`, `userspace/device-mgr/src/main.rs:151`,
  `userspace/storage-service/src/main.rs:313`, `userspace/service-mgr/src/main.rs:825`. A libos
  `Mapping` guard holds `(NonNull<u8>, len)`, derefs to `[u8]` of exactly the mapped length,
  unmaps its own length on Drop, and does not borrow the handle (storage's Scratch legitimately
  outlives it). `Handle<Memory, M>::map_whole(&self)` sizes from `sys_handle_stat`, and `with_mapped`
  and `Memory::from_bytes(&[u8], rights)` (create, fill, unmap, narrow) cover the copies. It
  replaces libos's `map -> *mut u8` and its safe `unmap(addr, len)`, which lets safe code unmap
  any address. Checked against init `read_manifest`, device-mgr `read_registry` and storage
  `with_mapped`. Home: libos (`objects.rs`, beside `Handle<Memory, M>::map`). Closes: mappings
  never unmapped (sup-d9); unmaps that rely on the kernel ignoring the size (sup-d10); a one-page
  read of a larger file (sup-d8); a slice longer than its mapping (sup-d7, sound today only by the
  kernel's check); a slice outliving its unmap; and reply-copy drift, since service-mgr narrows
  the object it hands out and the other two send the creation handle, harmless only because the
  kernel installs requested and granted rights intersected. Depends on: nothing. abi, ownership,
  soundness, opportunities. Value medium, effort small. **Plan**, as rt-o3, in P7.
- [x] **sup-o8** encapsulate: a hand-written "`sys_ns_lookup`, wait on the PO, read the result at
  16..24, close the PO" helper per crate, each closing the PO whether or not the lookup completed
  — 5 sites (`grep -rnE "^fn (ns_lookup_wait|ns_lookup|lookup)\("` gives 4, plus service-mgr
  `lookup_serving`, which must keep answering routes while it waits), such as
  `userspace/init/src/main.rs:116`, `userspace/service-mgr/src/main.rs:110`,
  `userspace/device-mgr/src/main.rs:133`, `userspace/storage-service/src/main.rs:304`.
  `libfs::lookup_wait(ns, path, rights) -> (i32, u64)` (libfs/src/lib.rs:358) already exists and
  is safe; service-mgr links libfs, and device-mgr and storage would add it. init, which libfs's
  layer is closed to, uses libos `Namespace::lookup` + `block_on` as it already does at main.rs:570.
  The timeout case also needs a kernel half: a forwarded lookup whose PO has no userspace handle
  left drops its result instead of installing it. Home: libfs (existing) and libos for init, plus
  the kernel's `complete_resolve_reply` (table.rs). Closes: three `unsafe` blocks and the
  byte-offset status decode per copy, and results orphaned by a PO closed before completion
  (sup-d4 and its siblings). Depends on: nothing for the dedup; a kernel change to `sys_ns_lookup`
  completion (Part E's syscall surface) for the timeout case. ownership, soundness. Value medium,
  effort small for the dedup, medium with the kernel half. **Fix.**
  Fixed in `fc9ce772`, the kernel half, and `4a1ddbd6`: service-mgr, device-mgr and storage-service
  resolve through `libfs::lookup_wait`. init keeps its copy until libos's `block_on` stops spinning
  on a failing wait (rt-d12, group R).
- [ ] **sup-o9** encapsulate: the five event loops (init `reap_loop`, service-mgr `Mgr::run`,
  device-mgr and storage-service `_start` loops, logging `serve_loop`) hold all 225 unsafe blocks,
  and no host test or Miri reaches any of them — 5 sites (per-file `grep -c unsafe`: 0 in every
  lib.rs, ready.rs, unmount.rs, probe.rs, ring.rs, path.rs, service_toml.rs and libinittoml; 225
  in the five `#![no_main]` main.rs files plus storage `mounting.rs`), such as
  `userspace/service-mgr/src/main.rs:1057-1112`,
  `userspace/storage-service/src/main.rs:1456-1525`,
  `userspace/device-mgr/src/main.rs:663-733`, `userspace/logging-service/src/main.rs:622-714`.
  Write each loop's dispatch against libos's typed calls (sup-o5, sup-o4, sup-o7), whose `sys`
  seam already swaps in a scriptable mock under `cfg(test)`, so each state struct (`Mgr`,
  `Manager`, `Service`) moves into its lib and a host test or Miri drives it with scripted
  messages, exits, late Readys and closed peers. Home: libos (the `sys` mock grown to channels and
  multi-handle waits) and each crate's lib.rs. Closes: the absence of any CI reach into this
  `unsafe`. It would have reached three of this survey's defects (sup-d1, sup-d2, sup-d4) without
  a boot. Depends on: sup-o1, sup-o2, sup-o5. opportunities. Value medium, effort large.
  **Plan**, in P7.
- [x] **sup-o10** remove: `SpawnArgs` kept as `static mut` templates, stamped in place and spawned
  through a raw pointer (an `unsafe fn` in init, a safe fn over `*mut` in service-mgr) — 4 spawn
  calls carrying 7 static SpawnArgs (`grep -n 'SYS_PROCESS_SPAWN\|static mut SPAWN'`: init 166,
  service-mgr 371 and 422, storage `mounting.rs` 36; init 4 statics, service-mgr 3), such as
  `userspace/init/src/main.rs:157`, `userspace/service-mgr/src/main.rs:402`,
  `userspace/service-mgr/src/main.rs:354`, `userspace/storage-service/src/mounting.rs:25`. Build
  `SpawnArgs` as locals from `const` templates, as storage `mounting.rs` already does, and call
  `libos::spawn(&SpawnArgs) -> Result<Handle<Process, Only>>`, which exists and is safe. An
  optional builder over owned handles hands moved handles back on failure. The returned handle
  owns the process, so it is stored or dropped (closed). Checked against init `spawn_program`,
  service-mgr `spawn_service` and storage `mounting::spawn`. Home: libos (`objects::spawn`,
  existing; device-mgr, storage, logging and service-mgr add the dependency, init has it).
  Closes: process handles discarded unclosed (sup-d5); the safe `spawn_with_control(*mut
  SpawnArgs)` (sup-d6); fields carried between spawns in a shared static (SPAWN_SERVICE's
  `handles[0]`/`rights[0]` keep the previous child's values, masked today by `handle_count = 0`).
  It removes 7 statics and 1 `unsafe fn`. Depends on: nothing (the builder wants owned handles
  from sup-o2). abi, ownership, soundness, opportunities. Value medium, effort small. **Fix.**
  Fixed in `f013319d` and `e6a4ecf2`: `const` templates copied into locals, spawned through libos's
  `spawn`, which service-mgr now links. Storage's spawn already built a local and closes its handle
  on every path, so it is left.
- [ ] **sup-o11** remove: local redefinitions of operations that already have a safe form —
  `kprint` over raw `SYS_DEBUG_KPRINT`, `exit` over raw `SYS_PROCESS_EXIT`, and an `asm!("pause")`
  loop — 5 sites (`grep -n 'fn kprint\|fn exit\|SYS_PROCESS_EXIT\|asm!'` over the mains), such as
  `userspace/service-mgr/src/main.rs:94`, `userspace/logging-service/src/main.rs:141`,
  `userspace/logging-service/src/main.rs:146`, `userspace/init/src/main.rs:1021`. Use
  `libkern::debug::{kprint, exit}` (device-mgr and storage already do) and
  `core::hint::spin_loop()` (service-mgr 2247 and logging 150 already do). Home: libkern::debug /
  core. Closes: three `unsafe` blocks and the subsystem's only inline assembly. Depends on:
  nothing. opportunities. Value low, effort small. **Plan**, as rt-o9, in P7.
- [ ] **sup-o12** encapsulate: `ChildExited` decoded by byte offset from a static `Notification`
  (pid at body[0..4], code at body[8..12]), ignoring `ExitStatus.kind` — 2 sites (`grep -n
  'KIND_CHILD_EXITED'` over the mains), `userspace/init/src/main.rs:777-780` and
  `userspace/service-mgr/src/main.rs:2193-2198`, with the layout in
  `docs/spec/notification-format.md:108-112`. `Notification::child_exited(&self) ->
  Option<ChildExited { pid: u32, status: ExitStatus { kind, code } }>` on the libkern ABI type,
  plus libos `Handle<Notify, Only>::recv() -> Result<Option<Notification>>` with a local
  out-value. Home: libkern `abi.rs` (the accessor) and libos `objects.rs`. Closes:
  byte-offset decoding of a kernel ABI body at each consumer, and the crash-read-as-exit-code
  confusion that service-mgr's `should_restart` inherits. Depends on: nothing. opportunities.
  Value low, effort small. **Plan**, in P7.

What held up: every receive buffer holds a whole message (all 4096-byte, kernel copies exactly
`IPC_MSG_SIZE`), and every `sys_wait` results buffer holds 24 bytes per waitable handle with each
wait set within 32 (service-mgr's budget is held by a lib test). Payload slices clamp to 4072,
the wrong bound against 4008, but harmless because `seal()` refuses over 4008 on send. Hand-decoded
`IoResult`, `ChildExited` and init's rsproto Ready/Unmount offsets match the kernel and
`librsproto::encode`, and init's tests pin the constants. All ten `channel_recv_raw` sites go
through `libkern::Received`, so no arrived handle leaks. Failed NOBLOCK sends and failed spawns
leave the sender's handles in place (kernel table.rs:1939-2015, 340-516), so every close-on-failure
is a single close, and the 31-bit handle generation makes a stale value refused rather than
aliasing. All five processes are single-threaded, no reference to a static is alive across a write
to it at any current site, and entry stack alignment is right for the hard-float target. Mapped
snapshots are sized by `HandleInfo.size` and parsed by header count, and the volatile change-page
read follows a completed PO. `with_mapped`'s closure cannot let the slice escape. The kernel
attenuates forwarded-reply handles to requested and granted rights intersected, so the full-rights
handles device-mgr and storage reply with amplify nothing. The scope was wrong in three places:
logging's ring is a process-local `Ring` and its two raw derefs (the `READ_BODY` scratch) are
sound, storage-service `probe.rs` has no `unsafe` (the device reads are `DeviceIo` in main.rs),
and `libinittoml`, `service_toml.rs` and every `lib.rs` are safe Rust. None of the 225 `unsafe`
blocks is reached by a host test or Miri.

### security — main at 135055a8, 2026-10-27

Lenses: abi, ownership, soundness, opportunities; returned nothing: none.
Defects: found 28, refuted by a boot 0, reported 14 (28 findings merged by duplicate). Opportunities: 11 (26 merged).

The inventory, `cargo xtask unsafe-inventory` over its paths:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `userspace/auth-service` | 668 | 52 | 0 | 0 | 15 | 33 | 7 | 5 | 0 | 0 | 1 |
| `userspace/view-broker` | 2214 | 31 | 0 | 0 | 8 | 23 | 1 | 3 | 0 | 0 | 0 |
| `userspace/session-mgr` | 322 | 17 | 0 | 0 | 7 | 11 | 1 | 0 | 0 | 0 | 0 |
| `userspace/libsession` | 782 | 72 | 0 | 0 | 14 | 54 | 4 | 1 | 0 | 0 | 0 |
| `userspace/libusers` | 270 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `userspace/libcrypto` | 247 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `userspace/libviews` | 168 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `userspace/libprompt` | 178 | 3 | 0 | 0 | 0 | 3 | 0 | 0 | 0 | 0 | 0 |
| `total` | 4849 | 175 | 0 | 0 | 44 | 124 | 13 | 9 | 0 | 0 | 1 |

**Defects**

- [x] **sec-d1** `userspace/session-mgr/src/main.rs:224` — When a successful login's namespace
  fails to build, the loop `continue`s without `tty_close`, so the terminal opened at line 187
  leaks, and with it one of tty-server's 14 terminal slots (MAX_TTYS = (32-3)/2). A
  /system/users record whose home the kernel refuses as a subtree base (`/home/alice/`, `/`, a
  relative path, over 128 bytes) is accepted by auth-service with the home unchanged. On each
  correct login libsession::build's `/home` bind fails (lib.rs:384) and build_namespace returns
  0. A failed SYS_NS_CREATE takes the same path. After about 13 such logins tty-server answers
  every `/dev/tty` resolve with WouldBlock: the serial login prints 'no terminal for login'
  forever, and nxterm windows and the broker's token redeem fail too. Demonstrated: host
  probes (restored) showed `authenticate` returning Authenticated with home `/home/alice/`,
  `/`, `home/alice` and a 300-byte home (`cargo test -p auth-service --lib survey_probe`), and
  the kernel's `SubtreeBase::from_path` refusing all of them and accepting `/home/alice`
  (`cargo test --lib --target x86_64-unknown-linux-gnu survey_probe`). The only `tty_close`
  calls are at 252-254 and 274-276, and tty-server/src/main.rs:43 and :290 refuse the 15th
  terminal. ownership, soundness. Medium. **Fix**, with the bash's sec-12, moved here from Part H.
  Fixed in `fa31e763` and `cbbf09e0`, with sec-12: session-mgr's session is a labelled block the
  namespace failure breaks out of, and the terminal is closed once, after it, on every path; and
  auth-service answers `Denied` for a record whose home `libusers::valid_home` refuses, the kernel's
  subtree-base rules, after the derivation (the maintainer's choice of where). The close is
  structural; `a_home_no_session_can_bind_logs_no_one_in` fails without the check, or with its bound
  moved by one.
- [x] **sec-d2** `userspace/libos/src/exec.rs:133` — `Op::poll` is a safe `Future` impl that
  turns `cx.waker().data()` into `&Reactor`. It is sound only when libos's `block_on` built the
  Waker, so a foreign Waker makes safe code produce a null or foreign reference. libos is
  outside the subsystem, but it is the home the opportunities below propose. Safe code that
  submits an Op with the PO not yet complete and polls it with `Waker::noop()` runs
  `&*(null as *const Reactor)`, and `register()` writes a 72-byte Registry through it. Under
  another executor's Waker the write lands in that executor's data. It is latent today because
  only `block_on` polls an Op. The multi-task executor libos.md plans, or any join that wraps
  the Waker, would make it memory corruption in init. Demonstrated: a temporary host test
  `probe_op_polled_by_a_foreign_waker` (scripted not-ready mock, `Waker::noop()`) aborted with
  'panicked at libos/src/exec.rs:133:32: null reference produced'. Probe removed. opportunities.
  Medium. **Fix**, as rt-d1.
  Fixed with rt-d1, in `aedd2368`.
- [x] **sec-d3** `userspace/libsession/src/lib.rs:208` — `authenticate` (and `views_call`, line
  252) takes the next message on a long-lived channel as the answer to its request. The request
  id is always 1 and is never compared, and the reply's principal is never compared with
  `user`, so a reply left unread is taken by the next call as its own. One auth channel is
  shared by session-mgr, desktop-session-mgr and the broker. Check A (correct password) is
  answered AUTHENTICATED, but `sys_channel_recv` fails with OutOfMemory, which it returns
  before popping (kernel/src/syscall/table.rs:2771), so `.ok()?` returns None and the reply
  stays queued. Check B (any user, wrong password) then pops A's reply: session-mgr logs in the
  typed name with A's home, and the broker allows B's elevated view. Every later answer stays
  shifted by one. `views_call` gives the next login the previous user's `/s/<id>` the same way.
  The same bite follows from putting a deadline on these waits, when a slow PBKDF2 outruns it.
  Contrast session-mgr `tty_request` (fresh ids) and the broker's `ask_auth` (closes the
  channel on give-up). Unverified: fail one `sys_channel_recv` allocation on the auth channel
  under QEMU, or add a host seam for libsession's syscalls. abi. Medium. **Fix.**
  Fixed in `d8ecec11`, with sec-o3 (`15982e22`): `authenticate` and `views_call` go through
  `round_trip`, which skips a reply to any request but its own, each call with an id of its own,
  the broker's checks included. `a_reply_to_another_request_is_skipped` fails with either half of
  the comparison removed; the loop around it makes syscalls.
- [x] **sec-d4** `userspace/auth-service/src/main.rs:73` — `ADMIN_REPLY` is sized
  `MSG_LEN - PAYLOAD_OFF - 64` = 4008 (a whole payload) where a reply body may be at most
  `RS_MAX_BODY` = 3980. A List body of 3981..=4008 bytes is built as `Admin::Reply`, then
  `send_reply`'s `encode` refuses it and returns without sending anything. A hand-written
  /system/users of at most 4096 bytes with a few long names and homes reaches it (for example
  10 records of `<199-byte name>::1::<199-byte home>`, 4040 bytes). The broker's List (before
  every policy judgement, account write and `Accounts`) then stalls for ACCOUNTS_WAIT_NS (5 s),
  drops its admin session and reports 'auth-service did not answer', on every call while the
  file stays. The library's `ListTooLong` refusal never reaches the broker. A file written by
  libusers cannot reach it. Demonstrated: three independent host probes (restored).
  `serve_admin(OP_AUTH_LIST)` into a 4008-byte buffer returned `Admin::Reply(4002..4007)`,
  `librsproto::encode` into the 4072-byte tail returned None ('list body 4007 bytes;
  RS_MAX_BODY 3980', 'encode -> None'). Control: a reply buffer of `RS_MAX_BODY` gives
  `Refused(ListTooLong)`. abi, soundness, opportunities. Low. **Fix.**
  Fixed in `8a210a6f`, with sec-o9: `serve_admin` takes its reply as a `[u8; RS_MAX_BODY]`, so no
  other size compiles. `a_list_that_does_not_fit_is_refused` lists exactly `RS_MAX_BODY` bytes and
  encodes them, and has one byte more refused; with the 4008-byte buffer it is answered again.
- [x] **sec-d5** `userspace/view-broker/src/main.rs:1447` — `let (Ok((setup_ours,
  setup_child)), Some((life_ours, life_child))) = (pipe(1), make_channel(1)) else {..}`
  evaluates both creates, and when exactly one fails the else arm closes `image` and `view_ns`
  but not the pair that was created, so two channel handles leak per failed start. An allowed
  request reaches `start()` while the kernel heap or the broker's handle table is short. The
  pair stays in the essential broker's table for the boot, holding its kernel rings, and each
  later failure adds two. Demonstrated by the path: both pairs are plain `u64`s with no Drop
  (libstream setup.rs:244-253, main.rs:273-278), and the else arm at 1448-1450 names neither.
  Not run. abi, ownership, soundness, opportunities. Low. **Fix.**
  Fixed in `4d11c253`: the pair that was made is closed with the image and the view. Structural.
- [x] **sec-d6** `userspace/libsession/src/lib.rs:1042` — Two error paths in `spawn_leader`
  return -1 without giving back what they took. A failed setup send leaves the leader's Process
  handle `h` open, so the leader is unsupervised and the process pinned. A failed `pipe`
  (lines 948-953) leaves the `image` handle (resolved at 940) open. The leader (nxsh, or
  desktop-shell under desktop-session-mgr) dies before its setup message is sent, so the Block
  send returns PeerClosed (or the kernel refuses the bounce buffer or PO handle). Each login
  retry leaks one Process handle, and every caller of the function (session-mgr,
  desktop-session-mgr, desktop-shell, service-mgr) shares it. A pipe failure leaks the image's
  FileObject handle on every attempt. Demonstrated by the path: `h` is closed only at 1064 and
  1081, the `sent.is_err()` return at 1042-1045 closes only `setup_mgr`, and the pipe `Err` arm
  at 950-953 never reaches the image close at 965. Not run, since every step is a syscall.
  ownership, soundness, opportunities. Low. **Fix.**
  Fixed in `991ad0e1`: a failed pipe closes the image, and a failed setup asks the leader to stop
  and waits for its exit, as a shutdown does, since its `ChildExited` would otherwise end the next
  session. Structural; test-interactive passes.
- [x] **sec-d7** `userspace/session-mgr/src/main.rs:299` — `login()` reads the typed password
  into the stack array `pass: [u8; 128]` and never zeroes it, against session-mgr's CLAUDE.md
  ('does not keep it'). `libsession::authenticate` scrubs its own two copies, and
  desktop-session-mgr zeroes its equivalent (main.rs:383). After each attempt the password
  stays in a dead frame of a supervisor that lives for the boot, until later frames overwrite
  it. The tty READ_LINE reply in `RECV_MSG` is also kept if the following `tty_set_echo` send
  fails, until the next successful receive. Demonstrated by the path: `grep scrub
  userspace/session-mgr/src/main.rs` finds nothing, and 299-307 fill, use and drop `pass`.
  Memory not read. abi, ownership, soundness, opportunities. Low. **Fix.**
  Fixed in `fa31e763`, with sec-o4: the password and the tty reply buffer are `Secret`s, zeroed on
  every return; desktop-session-mgr's password too. Structural.
- [x] **sec-d8** `userspace/libcrypto/src/hmac.rs:19` — `HmacSha256::new` leaves the key in `k`,
  in `ipad` and in the `opad` field (key XOR 0x5c), and nothing zeroes them. The type has no
  Drop, and `pbkdf2_hmac_sha256` clones it once per iteration. When PBKDF2 keys it the key is
  the password, so auth-service's stack holds password XOR 0x5c after the request returns, and
  `scrub_request` zeroing `RECV_MSG` does not satisfy auth-service/CLAUDE.md. Demonstrated: a
  probe test (restored) recovered the key from `opad` XOR 0x5c; `needs_drop::<HmacSha256>()`
  was false and a clone carried the same bytes (`cargo test -p libcrypto --lib survey_probe`:
  1 passed). ownership. Low. **Fix.**
  Fixed in `7ba29c27`: `Sha256` and `HmacSha256` zero their state when dropped, and `new` its
  locals. `a_dropped_mac_is_zeroed` and `a_dropped_hasher_is_zeroed` read a dropped value back as
  zeroes, and fail with any of the wipes removed; both pass under Miri.
- [x] **sec-d9** `userspace/libsession/src/lib.rs:719` — `registry_blocks` maps the registry
  snapshot with `info.size` (703) but unmaps it with size 0. It frees the mapping only because
  `sys_memory_unmap` ignores its size argument today (`_size`, TODO(mm),
  kernel/src/syscall/table.rs:946-949). Latent: implementing the partial unmap
  (deferred-decisions.md:2343 recommends rejecting an unmap size that does not cover the VMA)
  makes `SYS_MEMORY_UNMAP(addr, 0)` unmap nothing or fail. Every `disks` grant (view-broker
  `start` → `rebind_block_devices_except` → `block_indices` → `registry_blocks`, as in `with
  admin nxinstall`) would then leak a registry mapping in the broker. Demonstrated by the path:
  every other unmap in scope passes the mapped length. Not run. soundness, opportunities. Low.
  **Fix.**
  Fixed in `991ad0e1`: the unmap passes `info.size`, the class's last site in userspace.
- [x] **sec-d10** `userspace/libsession/src/lib.rs:219` — `authenticate` copies
  `min(home.len(), home_out.len())` bytes and still reports success. The supervisors pass
  256-byte buffers while auth-service's reply admits a home up to about 500 bytes, so a longer
  home is silently cut and `/home` is bound at a path the record does not name. `tty_request`
  does the same to a typed line (session-mgr main.rs:371), so a password over the 128-byte
  `pass` buffer is checked as its prefix. Only the trusted user database reaches it today
  (libusers writes `/home/<name>`, at most 38 bytes). One ordinary change, such as allowing a
  chosen home in `account add`, would make it reachable by an untrusted input. A home over 128
  bytes is refused by the kernel (NS_SUBTREE_BASE_MAX) before the truncation matters, which
  becomes sec-d1's path. Demonstrated by the path: `let n = r.home.len().min(home_out.len());
  ...; (true, n)` at 219-221, and a host probe that a 300-byte home authenticates and comes
  back whole. The wrong-/home binding itself was not run. abi, opportunities. Low. **Fix.**
  Fixed in `d8ecec11` and `fa31e763`: a home, a broker answer or a typed line past its buffer is
  refused, never cut, and an over-long typed password is checked as empty and denied.
  `a_home_that_does_not_fit_is_refused` fails with the old copy.
- [x] **sec-d11** `userspace/view-broker/src/main.rs:1489` — `start()` treats an `Err` from
  `send_setup_full` as 'the stream handles are still ours', which is not always true. A Block
  send that committed closes the moved handles first, then fails to install the PO handle on a
  full table (kernel/src/syscall/table.rs:2030-2041, 'the message was still committed') and
  returns Err. The broker then runs `p.handles.close()` on values it no longer owns and sends
  SYS_PROCESS_TERMINATE to a program whose setup was delivered. Handle generations make the
  closes harmless today, so ownership is held by that check and by convention, since
  `send_setup_full`'s Result (libstream setup.rs:293-306) cannot say whether the handles moved.
  Unverified: a full broker handle table under QEMU, or a kernel host test of
  `sys_channel_send`'s Block path at a full table asserting the sender's handles are gone while
  Err is returned. abi, ownership. Low. **Fix.**
  Fixed in `f9dcedf5`, PR #403's review: a blocking send makes its PO's handle before the push and
  closes it if the push is refused, so an `Err` moves nothing. The broker's half went with proto-o2
  in `2991d0dc`: `send_setup_full` takes every handle on every return.
- [x] **sec-d12** `userspace/auth-service/src/main.rs:365` — `serve_session` frees the session
  slot and closes the channel on any receive error. Its comment says a closed peer frees it,
  but OutOfMemory, which `sys_channel_recv` returns with the message still queued, closes a
  live client's channel too. During a transient kernel OOM auth-service's recv on session-mgr's
  oracle channel fails and `free_session` closes it. session-mgr resolved /svc/auth once 'for
  the life of the machine' and never re-resolves it, so every later serial login reads
  PeerClosed and prints 'login incorrect' until reboot. The broker re-resolves `auth_ch` only
  while it is 0, so every `with` password fails for the boot. Compare the broker's `recv_full`,
  which treats only PeerClosed as gone. Unverified: OOM injection into one `sys_channel_recv`
  under QEMU. abi. Low. **Fix.**
  Fixed in `8a210a6f`: only `PeerClosed` frees a session; any other failure parks 10 ms and keeps
  it, and the forwarding endpoint's receive parks too. Structural.
- [x] **sec-d13** `userspace/view-broker/src/main.rs:73` — The child end of the life channel is
  spawned with every right an endpoint has: `SPAWN.rights` is `u64::MAX`, intersected with
  `channel_endpoint_rights` (SEND|RECV|DUPLICATE|TRANSFER|INSPECT|WAIT). A program that
  duplicates or transfers its life end to a child and exits queues its ChildExited code, but
  its life channel never closes. The client stays Running, holding the process handle,
  `view_ns` and the grants until its session ends, and `Exits` pairs the orphaned code with the
  next program whose life closes, so later exits are reported with the previous program's code.
  This is not the residual TODO(child-exit-attribution) records (two exits in one wake). The
  premise needs checking: the program is never told the life handle's slot, so it must find it
  by enumerating its handles. Unverified: a boot probe that runs a program duplicating its
  handles and checks the reported codes. ownership. Low. **Fix.**
  Fixed in `4d11c253`: the life end is spawned with no rights, so it can be neither duplicated nor
  moved, and no grant gives a program the syscap a bind needs. Structural; test-interactive's
  programs are reported exited with their codes.
- [x] **sec-d14** `userspace/libsession/src/lib.rs:27` — Safe `pub fn`s read and write
  process-wide `static mut` buffers (`authenticate`, `ns_lookup`, `views_open_session` and
  `views_close_session`, `build`, `build_namespace`, `spawn_leader`, `drain_for_stop`,
  `rebind` and `unbind_block_devices`; the statics are SEND_MSG, RECV_MSG, WAIT_HANDLES,
  WAIT_RESULTS, NOTIF, SPAWN_SHELL and SESSION_HAS_*). Every SAFETY argument is 'single-threaded
  supervisor', which nothing in the signatures holds. Latent: none of its five consumers
  (view-broker, session-mgr, desktop-session-mgr, desktop-shell, service-mgr) creates a thread
  today (grep for `SYS_THREAD_CREATE` / `thread_create` finds none outside libkern, libos and
  test-harness). A thread in any of them (`libos::thread_create` allows it) could interleave
  `authenticate` and `views_call` so that one sends the other's bytes, or a mix carrying a
  password, or one's scrub zeroes the other's message. Unverified: the interleaving needs
  syscalls, so neither a host test nor Miri reaches it; sec-o2 and sec-o5 make it structural.
  opportunities. Low. **Fix.**
  Fixed in `0039c4bb`, with sec-o8 and sec-o10, and the buffers too (the maintainer's call):
  libsession has no `static mut`. Structural.

**Opportunities**

- [ ] **sec-o1** encapsulate: raw u64 handles closed by hand on every exit path, and structs of
  handles that hold ownership only by comment — 69 sites (`grep -nE
  "SYS_HANDLE_CLOSE|(^|[^a-z_.])close\([a-z_*]|\.close\(\)"` over auth-service/src/main.rs,
  view-broker/src/main.rs, session-mgr/src/main.rs, libsession/src/lib.rs,
  libprompt/src/ipc.rs and libviews/src/lib.rs, less the three helper bodies; the lenses
  counted 63 to 69), such as `userspace/view-broker/src/main.rs:1447`,
  `userspace/session-mgr/src/main.rs:224`, `userspace/libsession/src/lib.rs:1042`,
  `userspace/libviews/src/lib.rs:66` (`Handed` derives Copy over owned handles). An owning
  handle that closes on Drop and gives itself up with `into_raw()` when a send or spawn moves
  it, so a send takes owned handles. That is libos's `Handle<T, M>` plus Channel and File
  object types, with `Received::take` returning one. `Handles` and `Handed` become structs of
  `Option<Handle<..>>` without Copy. Checked against view-broker `start()` (`view_ns`
  dropped on each early return), session-mgr's per-login tty, `spawn_leader` (image and process
  handle) and auth-service `open_auth_session`'s reclaim. Home: libos (`handle.rs`), or a
  minimal `OwnedHandle(u64)` in libkern beside `Received` while session-mgr keeps its 'no
  libos' rule. Closes: a handle leaked on an error path or a `continue` (sec-d1, sec-d5,
  sec-d6), a handle closed after a send moved it (sec-d11), and a Copy struct of handles used
  after the move. Depends on: libos gaining Channel and File types, and lifting the 'no libos'
  rule in userspace/session-mgr/CLAUDE.md (which libsession follows), or the libkern variant.
  ownership, soundness, opportunities. Value high, effort large. **Plan**, as sup-o2, in P7.
- [ ] **sec-o2** encapsulate: message I/O through `static mut` 4 KiB buffers, with the IpcMsg
  header (`payload_len` at 4..8, `handle_count` at 8) stamped by hand, a `from_raw_parts`
  payload view clamped to `MSG_LEN - PAYLOAD_OFF` (4072, not `IPC_PAYLOAD_SIZE` 4008), a raw
  `SYS_CHANNEL_SEND` and a hand-placed scrub — 17 sites (`grep -rnE
  "channel_recv_raw\(|SYS_CHANNEL_SEND," auth-service/src view-broker/src session-mgr/src
  libsession/src libprompt/src | grep -v "use "`: 7 receives each with a payload
  `from_raw_parts`, and 10 sends; they cover 12 of the 44 `static mut`s; userspace-wide the
  pattern has 53 raw receives and 92 raw sends across 35 files), such as
  `userspace/auth-service/src/main.rs:305`, `userspace/view-broker/src/main.rs:293`,
  `userspace/libsession/src/lib.rs:183`, `userspace/session-mgr/src/main.rs:332`. `IpcBuf([u8;
  IPC_MSG_SIZE])` with `recv(&mut self, ch) -> Result<(Received, &[u8]), RecvError>` (payload
  bounded by `IPC_PAYLOAD_SIZE`, borrowed from `&mut self`), `send_rs(&mut self, ch, op, rid,
  flags, body, handles)` (stamps the header in one place, refuses over `IPC_HANDLE_MAX`
  handles, returns `TooLong` when `encode` refuses rather than silently, and can scrub once the
  kernel has its copy) and `scrub()`. The buffer lives in a server's own state or on the stack,
  as `libprompt::ipc` and `libkern::control::recv` already do. Home: libkern::channel beside
  `Received` (alloc-free, so auth-service can use it) or libos; the rsproto envelope layer
  (`send_rs`, `recv_rs`) in librsproto behind `io`. Checked against auth-service
  `serve_session`, view-broker `recv_full`/`send`/`ask_auth`, libsession
  `authenticate`/`views_call` and session-mgr `tty_request`. Closes: a payload slice that
  outlives the next receive into the same static (auth-service holds `m.body` and `name`
  across `install()` safely only because neither receives), the 4072 clamp held only by the
  kernel's `seal`, a silently dropped over-long reply (sec-d4), a forgotten scrub of a
  password-bearing send, a header `handle_count` that disagrees with the syscall's, and
  view-broker `send`'s `REPLY_HANDLES[..handles.len()]` panic for over 8 handles. Depends on:
  nothing for the libkern variant; libos lacks a Channel marker, and auth-service must stay
  alloc-free. abi, ownership, soundness, opportunities. Value high, effort medium.
  **Plan**, as rt-o5, in P7.
- [x] **sec-o3** encapsulate: a request/reply exchange on a long-lived channel written out by
  hand with a fixed request id (1); some sites take the next message as the answer, others
  match the id but must close the channel on a timeout — 8 sites (the six fixed-id sends found
  by `grep -n "OP_AUTHENTICATE, 1,\|op, 1, 0, body\|OP_STORAGE_IN_USE, 1,\|OP_TTY_REDEEM, 1,\|
  send(term, op, 1,\|admin, op, 1," userspace/{libsession,view-broker,libprompt}/src/*.rs`,
  plus session-mgr `tty_request` and `libprompt::ipc::call`/`answer`, which libviews uses with
  `REQUEST_ID = 1`), such as `userspace/libsession/src/lib.rs:170`,
  `userspace/libsession/src/lib.rs:232`, `userspace/view-broker/src/main.rs:651`,
  `userspace/session-mgr/src/main.rs:325`. `round_trip(ch, buf: &mut [u8; IPC_MSG_SIZE],
  next_id: &mut u64, op, body, handles: &[u64], deadline) -> Result<(Message<'_>, Received),
  CallError>`: it sends with a fresh id, skips replies whose id or op differ, and reports
  `Stale` on deadline or receive failure so the caller must reopen the channel. It extends the
  safe, length-checked `librsproto::session::round_trip` (already shared by Dir, Desktop and
  Clipboard) with id matching, a deadline and handles. Home: librsproto::session (`io`
  feature). Closes: a reply read by position (sec-d3, the stale AUTHENTICATED and `views_call`
  session id) and the tty INTERRUPT bug session-mgr's CLAUDE.md records. Depends on: nothing;
  shares its buffer type with sec-o2. abi. Value high, effort medium. **Fix.**
  Fixed in `15982e22`: `round_trip` waits again past a reply whose id or op is not its request's.
  No deadline: no caller wants one.
- [x] **sec-o4** encapsulate: how long a password lives in memory depends on hand-placed
  `scrub()` calls; the two places without one leak it — 32 sites (`grep -nE
  "scrub\(|scrub_request\(\)"` over auth-service, view-broker, session-mgr, libsession,
  libprompt and libviews sources, less `fn scrub` and `use libkern`: 30 `scrub(` calls plus
  auth-service's 2 `scrub_request()`), such as `userspace/view-broker/src/main.rs:986`,
  `userspace/libprompt/src/lib.rs:69`, `userspace/libsession/src/lib.rs:195`,
  `userspace/session-mgr/src/main.rs:299` (missing). `libkern::Secret<B: AsMut<[u8]>>(B)`:
  alloc-free, volatile-zeroed on Drop, `Deref<Target = [u8]>`, no Clone, no Debug; it works for
  `[u8; N]` and `Vec<u8>`. libcrypto's `Sha256` and `HmacSha256` implement Drop to zero their
  own state. Fits libprompt `ask_password`'s return, the broker's `Pending.queued` and
  `State::Changing`, and session-mgr's `pass`; the static IPC buffers are covered by
  `IpcBuf::scrub` (sec-o2). Home: libkern (`Secret`, beside `scrub`), libcrypto (its own
  Drop). Closes: a password copy dropped unscrubbed (sec-d7, sec-d8) and a scrub skipped by an
  early return added later. Depends on: nothing. ownership, soundness. Value medium, effort
  medium. **Fix.**
  Fixed in `d8ecec11` (`libkern::Secret`, `a_dropped_secret_is_zeroed`) and `7ba29c27`
  (libcrypto's own drop), on the login paths: libsession's request buffers, session-mgr's
  password and tty reply, desktop-session-mgr's password and auth-service's request buffer. The
  broker's and libprompt's scrubs stay for P7 (the maintainer's call).
- [ ] **sec-o5** encapsulate: sys_wait through a hand-sized results byte buffer, `IoResult`
  decoded by byte offset (handle 0..8, status 8..12, result 16..24), and the lookup-and-wait PO
  helper rewritten in each crate — 9 sites (`grep -rn "SYS_WAIT," auth-service/src
  view-broker/src session-mgr/src libsession/src libprompt/src | grep -v "use "`; 7 decode by
  offset; 8 of the `static mut`s are wait arrays; `libfs::po_wait` and
  `librsproto::session::po_wait` are copies outside the subsystem), such as
  `userspace/auth-service/src/main.rs:263`, `userspace/session-mgr/src/main.rs:107`,
  `userspace/libsession/src/lib.rs:851`, `userspace/view-broker/src/main.rs:1746`.
  `libkern::wait(handles: &[u64], results: &mut [IoResult], deadline) -> Result<&[IoResult],
  i64>` that refuses `results.len() < handles.len()` and returns only the records written,
  with `po_wait(po) -> (i32, u64)` closing the PO and a no-alloc `lookup(ns, path, rights)`;
  optionally a `WaitSet<const N>` with `push`, `wait` and `fired`. libos has the same `wait`
  privately (`sys.rs:23`), whose SAFETY holds `results.len() >= handles.len()` only 'by
  construction at every call site'. Home: libkern (no alloc; auth-service and libsession may
  not take libos), with libos wrapping it. Closes: a results buffer sized by expectation
  (auth-service's const assert, session-mgr's 48 bytes, libprompt's check), offset typos (the
  earlier HandleInfo 16-vs-24-byte stack smash), a nested wait clobbering a shared result
  buffer, and a handle silently dropped past capacity. Removes about 13 unsafe blocks and 8
  static muts. Depends on: nothing (a WaitSet/libos variant also needs `Op::from_po` public and
  sec-d2 fixed). abi, ownership, soundness, opportunities. Value medium, effort small.
  **Plan**, as rt-o2, in P7.
- [ ] **sec-o6** encapsulate: namespace construction as raw syscalls, each in its own unsafe
  with a 'valid namespace handle, path, endpoint' SAFETY line — 29 sites (`grep -rnE
  "SYS_NS_BIND|SYS_NS_UNBIND|SYS_NS_CREATE|SYS_NS_DERIVE"` over the five crates, less `use`:
  23 bind, 3 unbind, 2 create, 1 derive), such as `userspace/libsession/src/lib.rs:353`,
  `userspace/libsession/src/lib.rs:453`, `userspace/view-broker/src/main.rs:1269`,
  `userspace/view-broker/src/main.rs:1389`. `impl<M: CanBind> Handle<Namespace, M> {
  bind_at(&self, path: &[u8], resource: &impl AsRaw, base: Option<&[u8]>) -> Result<()>;
  unbind; derive }` and `Namespace::create()`, making the subtree-versus-direct choice
  explicit rather than implied by argument count (libos's `bind(path: &str, resource)` takes
  no base and no byte path). Checked against libsession `build` (13 binds of one shape), the
  broker's `bind_endpoint` (with a base) and the KernelLog direct-handle bind. Home: libos
  (`objects.rs`). Closes: the thinnest of the set; it moves (ptr, len) pairs out of the call
  sites (none is wrong today) and keeps the resource handle borrowed across the bind. Depends
  on: sec-o1 for typed resource handles, or accept RawHandle meanwhile. opportunities. Value
  medium, effort small. **Plan**, in P7.
- [ ] **sec-o7** encapsulate: a mapping handled by hand in three steps, `SYS_MEMORY_MAP`, then
  `from_raw_parts` or `copy_nonoverlapping`, then `SYS_MEMORY_UNMAP(addr, size)`, with the slice
  length and the unmap size each restated — 4 sites (`grep -rnE
  "SYS_MEMORY_MAP|SYS_MEMORY_UNMAP"` over the five crates: 8 calls, 4 map/unmap pairs, with 3
  raw slices and one copy), such as `userspace/auth-service/src/main.rs:149`,
  `userspace/auth-service/src/main.rs:580`, `userspace/libsession/src/lib.rs:703`,
  `userspace/libsession/src/lib.rs:823`. `Mapping<M> { addr: NonNull<u8>, len }`, returned by
  `Handle<Memory|File, M: MemMode>::map(len)`, offering `bytes(&self)` and (for write modes)
  `bytes_mut(&mut self)`, with Drop unmapping at its own `len` (an explicit drop before
  install's `file_sync`). Checked against `load_db`, `install`, `registry_blocks` and
  `bind_session_user`. Home: libos (`objects.rs`; `Memory::map` returns a raw `*mut u8`
  today). Closes: an unmap whose size is not the map's (sec-d9), a slice longer than the
  mapping (held by const asserts), a slice used after its unmap, and a mapping leaked on an
  early return. Depends on: a File object marker in libos. ownership, opportunities. Value
  medium, effort small. **Plan**, as rt-o3, in P7.
- [x] **sec-o8** remove: `static mut` used only as a syscall out-parameter, argument block or
  value set once, where a stack local, a struct field or an atomic does the same safely — 12
  sites (read of each declaration `grep -nE '^\s*static mut'` lists in the four main files, 44
  in all, less message buffers, wait buffers, SESSION_HAS_* and auth-service's tables:
  auth-service ROOT_NS, ENTROPY, USER_DB_LEN, CH_OUT0/CH_OUT1; session-mgr NOTIF_CH, TTY_RID;
  view-broker CLOCK_BUF, NOTIF, SPAWN; libsession NOTIF, SPAWN_SHELL), such as
  `userspace/auth-service/src/main.rs:78`, `userspace/view-broker/src/main.rs:66`,
  `userspace/session-mgr/src/main.rs:32`, `userspace/libsession/src/lib.rs:885`. Use what the
  code already does nearby: view-broker's `make_channel` takes stack out-params where
  auth-service uses CH_OUT0/1, and libsession's `now_ns` uses a stack `u64` where the broker
  uses CLOCK_BUF; `SpawnArgs` (104 bytes) and `Notification` (64) are stack locals; values set
  once become fields of a server struct (the broker's `Broker`) or `AtomicU64` (as libsession's
  `STOP_ASKED`). Home: each crate. Closes: references to `static mut`, and the library-level
  statics behind safe pub fns (sec-d14); drops one or two unsafe blocks per site. Depends on:
  nothing. abi, soundness, opportunities. Value low, effort small. **Fix.**
  Fixed in `0039c4bb`, `fa31e763`, `8a210a6f` and `4d11c253`: every site named, each now a local,
  a field or an atomic.
- [x] **sec-o9** encapsulate: auth-service's state as parallel `static mut`s — 12 sites (6
  statics, USER_DB with USER_DB_LEN, NEW_DB, ADMIN_REPLY, SESSION_CH with SESSION_ADMIN,
  reached from 12 unsafe blocks: 3 `from_raw_parts` at 159, 391, 457, 5 raw derefs at 458,
  463, 467, 474, and the session-table accesses; `grep -nE "raw (const|mut)
  (USER_DB|NEW_DB|ADMIN_REPLY)\b|USER_DB_LEN =|SESSION_CH\[|SESSION_ADMIN\["
  auth-service/src/main.rs`), such as `userspace/auth-service/src/main.rs:391`,
  `userspace/auth-service/src/main.rs:456`, `userspace/auth-service/src/main.rs:349`.
  `Db { bytes: [u8; DB_MAX], len }` with `get(&self) -> &[u8]` and `replace(&mut self, new:
  &[u8]) -> Result<(), TooLarge>`, inside a `Server { db, new_db, admin_reply: [u8;
  RS_MAX_BODY], sessions: [Option<(u64, bool)>; MAX_SESSIONS], .. }` owned by `_start`'s frame
  (8 MiB stack, no alloc) with `&mut self` methods. Checked against `load_db`, `serve_session`,
  `serve_admin_request` and `open_auth_session`/`free_session`. Home:
  userspace/auth-service (main.rs). Closes: a length outgrowing its buffer (held by two
  separate checks), a reply buffer sized by local arithmetic instead of `RS_MAX_BODY` (sec-d4),
  and parallel arrays drifting (the failed-transfer path at 356 clears SESSION_CH but not
  SESSION_ADMIN). Depends on: nothing. opportunities. Value low, effort small. **Fix.**
  Fixed in `8a210a6f`: one `Server` in `_start`'s frame, its session table `Option<(channel,
  admin)>`; auth-service has no `static mut`.
- [x] **sec-o10** remove: libsession records what the last build bound in seven `static mut
  SESSION_HAS_*` bools read through seven safe `session_has_*()` fns, although `build()`
  already returns the same facts as `Built` — 7 sites (`grep -n "static mut SESSION_HAS_"
  userspace/libsession/src/lib.rs`: 7 statics, 1 writer and 7 reader unsafe blocks; `grep -rn
  session_has_` outside libsession: 10 caller reads, 5 in session-mgr and 5 in
  desktop-session-mgr), such as `userspace/libsession/src/lib.rs:299`,
  `userspace/libsession/src/lib.rs:758`, `userspace/session-mgr/src/main.rs:227`,
  `userspace/desktop-session-mgr/src/main.rs:162`. Both supervisors call `build(&spec) ->
  Option<Built>` (or `build_namespace` returns `Built`) and read `b.bin`, `b.tty`,
  `b.console`, `b.clipboard`, `b.views`, `b.devices`, `b.storage`; delete the seven statics and
  the readers. Home: libsession. Closes: a flag read as 'the session's' that belongs to
  another build, and a safe fn over `static mut`; removes 7 of libsession's 14 `static mut`s
  and 8 unsafe blocks. Depends on: nothing. abi, ownership, soundness, opportunities. Value
  low, effort small. **Fix.**
  Fixed in `0039c4bb`: both supervisors read the `Built` that `build` returns, and the flags,
  their readers and `build_namespace` are gone.
- [ ] **sec-o11** remove: local copies of libkern helpers, each with its own unsafe: `kprint`
  (x3), process exit (x2) and a hand-written volatile zeroing loop (x1) — 6 sites (`grep -rnE
  "^fn kprint|^fn exit|SYS_PROCESS_EXIT|write_volatile"` over the five crates: 8 lines, one
  comment and one fn line belonging to the same exit), such as
  `userspace/auth-service/src/main.rs:84`, `userspace/auth-service/src/main.rs:416`,
  `userspace/session-mgr/src/main.rs:55`, `userspace/libsession/src/lib.rs:84`.
  `libkern::debug::kprint`, `libkern::debug::exit` (-> `!`) and `libkern::scrub`, which exist
  and are re-exported at libkern's root. Home: libkern (exists). Closes: little beyond
  duplicated SAFETY arguments; auth-service's `exit` spins on `spin_loop` after the syscall
  instead of being noreturn, and `scrub_request` duplicates `libkern::scrub`'s volatile loop
  (the volatile column's only site). Depends on: nothing. opportunities. Value low, effort
  small. **Plan**, as rt-o9, in P7.

What held up: Every receive lends the kernel a whole 4096-byte buffer, every send passes a full
page whose handle array and header count agree, and the 4072-byte payload clamp stays inside the
page, with `StoredMsg::seal` refusing a `payload_len` over 4008. Every SYS_WAIT results buffer
holds 24 bytes per handle waited on, and HandleInfo (24), IoResult (24), Notification (64) and
the ChildExited offsets match the kernel. auth-service's `load_db` and `install` (size at most
`DB_MAX` at most `PAGE`, const-asserted; mapped, copied, unmapped and closed on every path),
`USER_DB_LEN <= DB_MAX`, and the aliasing in `serve_admin_request` (distinct statics, `USER_DB`
written after the shared borrow ends) are sound. No RECV_MSG slice outlives the next receive:
the broker's `recv_full` copies the body out, and no payload view reaches the handle array.
Every handle a received message carries closes through `Received`; no site uses `let _ =`.
Handle generations (31-bit) make stale closes fail, which the broker's handle-keyed tables
depend on. Spawn is atomic-or-fail, so the closes after a failed spawn are correct. The rsproto
and libusers parsers are bounds-checked with `get`/`checked_add`, `handle_count() >= 1` is
checked before `handles[0]`, and libusers, libcrypto and libviews contain no unsafe. No
consumer of libsession or the servers creates a thread, so the single-thread arguments hold
today. Password scrubbing holds at every hop except session-mgr's `pass` and libcrypto's HMAC
state. The broker's cached endpoints cannot go stale (restart policy `never`). No host test
reaches any of the 175 unsafe blocks, and Miri cannot run them because each makes a syscall.
The breaks that tests caught are sec-d1, sec-d4, sec-d8 and sec-d2.

### filesystems — main at 135055a8, 2026-10-27

Lenses: abi, ownership, soundness, opportunities; returned nothing: none.
Defects: found 12, refuted by a boot 0, reported 10 (two pairs of duplicates merged). Opportunities: 11 (25 proposed, merged by pattern).

The inventory, `cargo xtask unsafe-inventory userspace/fs-server-ext4 userspace/fs-server-fat userspace/libfsserver`:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `userspace/fs-server-ext4` | 2617 | 1 | 0 | 0 | 1 | 0 | 0 | 1 | 0 | 0 | 0 |
| `userspace/fs-server-fat` | 2427 | 1 | 0 | 0 | 1 | 0 | 0 | 1 | 0 | 0 | 0 |
| `userspace/libfsserver` | 1631 | 80 | 1 | 0 | 11 | 40 | 15 | 0 | 0 | 0 | 0 |
| `total` | 6675 | 82 | 1 | 0 | 13 | 40 | 15 | 2 | 0 | 0 | 0 |

**Defects**

- [x] **fs-d1** `userspace/fs-server-ext4/src/ext4.rs:870` — Block and inode bitmaps are indexed
  by a bit number bounded by s_blocks_per_group / s_inodes_per_group. `parse_superblock`
  (ext4.rs:292-303) never checks those against the 8*block_size bits a bitmap block holds, and
  only `bitmap[..bs]` of the 4 KiB `[0u8; MAX_BLOCK]` scratch is read or written back. A bit past
  32767 indexes out of the buffer and panics. A bit between bs*8 and 32767 reads the scratch's
  unread zero tail as free, and the bit set there is never written. Panic: a 4 KiB-block ext4
  image (a USB stick) with s_blocks_per_group = 65536, s_blocks_count = 65536, group 0's bitmap
  all 0xFF and a nonzero bg_free_blocks_count passes `check_device`, and the server says Ready.
  The first grow reaches `alloc_run`, which scans past bit 32767. `bit_clear` indexes
  bitmap[4096], the server panics and exits, and every client is failed with PeerClosed. Double
  allocation: on 1 KiB blocks, s_blocks_per_group = 16384 (the bitmap holds 8192 bits) and a full
  group-0 bitmap. Bits 8192..16383 read as free from the zeroed tail, so the same block goes to
  every grow. The same pattern sits at `alloc_inode` (1093/1096), `free_run` (1388/1389) and
  `free_inode` (1329). Linux refuses both geometries in `ext4_fill_super`.
  Demonstrated: probe tests in fs-server-ext4/src/volume/hostile_tests.rs, since removed. (a)
  `fixture_blocks(4096, 4096, 0)`, sb+32 = 65536, sb+4 = 65536, group-0 bitmap filled with 0xFF:
  `vol.check()` returned Ok; `vol.grow_file("/system/current-generation", 100000)` panicked with
  'index out of bounds: the len is 4096 but the index is 4096' at ext4.rs:686:5. (b)
  `fixture_blocks(1024, 16384, 0)`, created /a and /b, set sb+32 = 16384, filled the bitmap with
  0xFF, bg_free_blocks_count = 100: `check()` Ok; `grow_file` on /a and /b both returned Ok(0);
  `map_file` gave both `[BlockRun { file_block: 0, device_lba: 8193, length: 1 }]`, one block
  given to two files, and group 1's first block, which the grow also zeroed. Run with
  `cd userspace && cargo test -p fs-server-ext4 --lib probe_ -- --nocapture`. abi. High. **Fix.**
  Fixed in `8bd6f2d1`: the superblock is refused, as Linux refuses it, for a group of more blocks or
  inodes than a bitmap block has bits, or an inode count the groups do not hold; a `Bitmap` over the
  bytes read answers `Corrupt` past them (fs-o2); and the class swept, an inode number past
  `s_inodes_count` is `Corrupt`. Fails without it: `a_group_wider_than_its_bitmap_block_is_refused`
  (the survey's two geometries, a panic at 4 KiB and block 8193 to both files at 1 KiB on the old
  code), `a_superblock_is_held_to_its_groups_at_each_edge`, `a_bitmap_bit_past_its_block_is_corrupt`
  and `a_record_naming_an_inode_past_the_last_is_corrupt`, each with only its own guard removed.

- [x] **fs-d2** `userspace/fs-server-ext4/src/ext4.rs:296` — The group-descriptor stride is read
  from s_desc_size even though 64bit is refused. Linux and e2fsprogs use 32 whenever 64bit is off
  (`EXT2_DESC_SIZE`). On a filesystem with s_desc_size nonzero and 64bit off, every descriptor
  offset after group 0 (`read_inode`, `inode_offset`, `group_desc_offset`) disagrees with every
  other tool. An ext4 filesystem that `e2fsck -fn` calls clean, with s_desc_size = 64 and 64bit
  off: the server reads group g's descriptor at 64*g, which is Linux's group 2g. Files whose
  inodes are in group 1 resolve NotFound. Once group 0's inodes are used up, a create takes its
  bit from the wrong group's bitmap, hands out an inode number already in use, writes the new
  inode over another (Linux's inode 257), and links the new name to an existing file whose link
  count stays 1. Removing either name then frees what the other still reaches.
  Demonstrated: probe (removed), `fixture_blocks(1024, 32768, 65536)`, 4 groups of 128 inodes,
  s_desc_size = 0 from mke2fs. Created 200 files, then set sb+254 = 64. `e2fsck -fn` reported
  clean. `map_file("/f126")` (inode 141, group 1) returned Err(NotFound). `create_file("/new")`
  returned Ok with inode 129, already a file's. e2fsck afterwards, s_desc_size reset to 0: 'Inode
  129 ref count is 1, should be 2' and 'Unattached zero-length inode 257'. Fix: desc_size = 32
  while 64bit is refused. abi. Medium. **Fix.**
  Fixed in `997d1ed0`: a descriptor is 32 bytes while `64bit` is refused. Fails without it:
  `descriptors_are_32_bytes_apart_without_64bit` (s_desc_size 64 after 200 creates: `/f190` read
  `NotFound` on the old code).

- [x] **fs-d3** `userspace/fs-server-ext4/src/extent.rs:685` — That each tree block has one parent
  is held only by convention. `cut()` and `free_tree()` walk and free a child once per index
  entry that names it, so a block named by several entries is freed repeatedly, and the work
  multiplies by up to block_cap (340 at 4 KiB) at each level. At depth 5 that is about
  4*340^4 = 5e10 leaf walks. A crafted ext4 file whose extent tree reuses one child block under
  many index entries, when unlinked (`release_inode` -> `free_inode_blocks` -> truncate 0),
  truncated, or grown (`grow_file_from` cuts past the old end first), keeps the single-threaded
  server in the walk for practically ever. A forwarded resolve has no deadline, so every client
  of the filesystem waits. The shared blocks are also freed many times: `free_run` tolerates that
  by counting only bits that were set, and i_blocks is clamped.
  Demonstrated: probe in extent.rs tests (removed). `interleaved(12)` builds a depth-1 tree with
  one leaf of 12 extents. The root was rewritten to 4 index entries naming that same leaf.
  `truncate(..., keep 0)` returned Ok after 156 device writes against 39 for the honest tree: 4x,
  one walk per naming entry. Each further level multiplies by its node's entry count; `header()`
  allows depth 5. A depth-2 tree (one index block of 340 entries naming one leaf) would settle
  the full blowup, expected ~1360x. abi. Medium. **Fix.**
  Fixed in `150ec40a`: every node a cut reads has its keys rising inside the range its parent's
  entry gives it, the root's inside all of them, and an empty node is not freed, so a block holding
  entries has one place in the tree. Fails without it:
  `a_leaf_named_by_four_entries_is_corrupt_to_cut` (156 writes against 39 on the old code),
  `a_leaf_named_by_every_entry_of_an_index_block_is_corrupt_to_cut` (3,279 against 41),
  `a_leaf_named_below_its_keys_is_corrupt_to_cut`,
  `an_empty_leaf_named_twice_is_not_freed_while_named` and
  `a_root_whose_keys_do_not_rise_is_corrupt_to_cut`, each with only its own guard removed.

- [x] **fs-d4** `userspace/libfsserver/src/disk.rs:37` — `po_wait` closes the PendingOperation
  and returns (-1, 0) whenever `sys_wait` returns anything other than one signalled record,
  without the I/O having completed. `Disk`, `SectorDisk` and `PartitionIo` then return
  `FsError::Io` and reuse the same scratch MemoryObject for the next transfer, while the
  abandoned IRP still holds that object and DMAs into it. Latent: giving `po_wait` a deadline (so
  a server does not hang on a stick pulled mid-transfer), or any new `sys_wait` failure, makes it
  bite. A read that times out returns Err; `MetaCache::read_meta` (cache.rs:117-118) at once
  reissues the read into the same scratch, and `Disk::read_at` builds a `&[u8]` over a page the
  first transfer may still be writing. On `SectorDisk`: (1) READ of run A is submitted into the
  scratch; (2) the wait returns early and the PO is closed, so `read_at` returns Io; (3) FAT's
  'a failed write may have landed' undo calls `write_at(B)`, which copies B's bytes into the
  scratch and submits a WRITE; (4) A's READ DMA completes into the scratch; (5) the WRITE's DMA
  reads the scratch, and A's bytes land on disk at B. The same give-up is copied into
  server.rs:407-414 (`forget`, where it errs safely: the file is kept), nxinstall/src/main.rs:102
  and test-harness/src/pattern.rs:265. The kernel-side twin was #396.
  Unverified: today the `waited != 1` branch cannot be reached. `sys_wait`
  (kernel/src/syscall/table.rs:1763) returns early only for TimedOut (no deadline is given), for
  OutOfMemory (`PendingOperation::add_waiter` fails only past MAX_WAITERS, and the server is the
  PO's only waiter), or for Ended at process exit. `sys_io_cancel` is always Unsupported, and
  the kernel `IrpBox` holds the buffer (io/block.rs:178-185). A host seam for disk.rs's two
  syscalls (as libos/src/sys.rs has) whose wait returns TimedOut once would settle it: the next
  submit would name the same scratch. Path: disk.rs:19-38, :106-108, :245-247, then
  cache.rs:115-119. soundness, opportunities. Medium. **Fix.**
  Fixed in `9630a10c` (delegated): `po_wait` answers `None` for a failed wait, and a disk's
  `Scratch` that sees one is lost, copying nothing into it and submitting nothing through it again;
  nxinstall's buffer is a `Scratch` too. Fails without it: disk/tests.rs, through a syscall seam: a
  wait failing on each disk and on `Disk`'s zeroes, then nothing submitted and the scratch as it
  was.

- [x] **fs-d5** `userspace/libfsserver/src/server.rs:992` — `server::run` and `server::bootstrap`
  (server.rs:979) are safe `pub fn`s, but they reach the eleven process-global `static mut`
  buffers (RECV_MSG, REPLY_MSG, REPLY_HANDLES, FORGET_MSG, CONTENT, WAIT_*, SESSION_*, CONTROL,
  DIR_BODY), and every SAFETY argument rests on 'single-threaded'. Nothing in their signatures
  stops a second thread, or a `Volume` impl called from inside `serve()`, from calling either.
  Scenario 1: a second serve loop on another thread (one way to lift TODO(server-fanout), or to
  serve two volumes from one process). Thread A, in `handle_forwarded_resolve`, holds
  `req = received()`, a `&'static` over RECV_MSG, across `serve()`. Thread B's
  `recv_on(serve_end)` has the kernel write the next message into RECV_MSG, so A parses B's
  request. Both stamp REPLY_MSG and REPLY_HANDLES, so a reply can carry the other's memory object
  or device duplicate, and B's `send_forwarded` failure path closes a handle A staged. Scenario
  2: a `Volume` impl calls `bootstrap` from `map_file` or `read_file`, and the kernel overwrites
  RECV_MSG under the live `req`. Neither server does either today. The SAFETY comments at :61,
  :113, :122, :308, :764 and :1022 all justify themselves as 'single-threaded'. The two `_start`
  fns are likewise safe `pub fn`s that make a `&mut` to STORE.
  Unverified: no host test reaches `run` or `bootstrap`, since the first act of each is a raw
  syscall, and Miri cannot run syscalls. Making both `unsafe fn` with a one-thread contract, or
  handing the buffers out once through an AtomicBool-guarded take with a host test that a second
  take is refused, would settle it. ownership, soundness. Medium. **Fix.**
  Fixed in `4eb4e046`, with fs-o1: the loop's state is one `Server` on `run`'s stack and a request
  is borrowed from its inbox, so server.rs has no `static mut` and the borrow checker forbids a
  receive while a request lives. Structural, so no runtime control; test-qemu serves the root
  filesystem through it.

- [x] **fs-d6** `userspace/fs-server-ext4/src/ext4.rs:547` — A directory session's ReadDir cursor
  is supplied by the client (server.rs:625) and used as a raw byte offset into the directory,
  checked only against the block's bounds, not the record grid. Bytes at a non-record offset,
  such as the inside of a name the client chose, are parsed as a record and their inode is
  stat'ed. A client with a session on /box mkdirs a name whose bytes form a record (inode 14,
  rec_len 0x0C0C, name_len 1), then sends ReadDir with cursor 32, inside that name. The reply
  holds an entry for inode 14, /system/current-generation, outside /box, with its mode, size and
  mtime: metadata disclosure past the session's confinement, no memory unsafety. FAT's cursor is
  a slot number and cannot land mid-entry.
  Demonstrated: probe (removed), `fixture_blocks(4096, 4096, 0)`; `mkdir_at(2, "box")`;
  `mkdir_at(box, <14 LE u32, 0x0C0C LE, 1, 1, 'X','Y','Z','W'>)`; `vol.read_dir(box, 12+12+8, ..)`
  emitted `[(14, 33204, 21, [88])]`: inode 14, mode 0o100664, size 21, name "X". Fix: validate
  the cursor by walking its block from offset 0 to it, or encode it as a block plus record
  index. abi. Low. **Fix**, raised to Medium.
  Fixed in `d4bd688a`: a cursor inside a block resumes at the first record at or past it, found from
  the block's start, as Linux's readdir does. Fails without it:
  `a_cursor_inside_a_record_resumes_at_the_next` (the survey's name read `[14]` on the old code).

- [x] **fs-d7** `userspace/fs-server-ext4/src/ext4.rs:1136` — Names entering ext4 through a
  session are checked only for '/', '.' and '..' (libfsserver's `entry_name`, serve.rs:20, plus
  ext4's own '/' and length checks in `dir_insert`, `mkdir_at`, `create_file`). A name containing
  NUL bytes is written to disk, and e2fsck and Linux call it illegal. FAT's `names::valid`
  refuses control characters. Any process with a directory session sends Mkdir (or a rename, or
  a RESOLVE_CREATE name) containing 0x00 bytes; the server writes the entry, and `e2fsck -fn`
  then reports the filesystem damaged.
  Demonstrated: same probe as fs-d6: `mkdir_at(box, <name with three 0x00 bytes>)` returned Ok;
  `e2fsck -fn` printed "Entry '^N^@^@^@^L^L^A^AXYZW' in /box (15) has illegal characters in its
  name. Fix? no". abi. Low. **Fix.**
  Fixed in `f632efcb`: a create, a `mkdir` and a rename's new name refuse a control character, NUL
  among them, as FAT does. Fails without it: `a_name_with_a_control_character_is_not_written`, at
  each of the three writers.

- [x] **fs-d8** `userspace/libfsserver/src/disk.rs:76` — If a later step fails, `Disk::new` and
  `SectorDisk::new` leak what they have already made. Neither has a `Drop`, so a dropped one
  keeps its scratch memory object and mapping (and `Disk` its 64 KiB zero span) for the life of
  the process. `PartitionIo`, beside them, does both properly. `Disk::new`: if SYS_MEMORY_MAP
  fails, the scratch handle stays open (disk.rs:76-78); if the zero span's SYS_MEMORY_CREATE
  fails, the scratch handle and its mapped page stay (81-83). `SectorDisk::new`: if the map
  fails, the 64 KiB scratch handle stays (227-229). A dropped `Disk` or `SectorDisk` leaks 4 KiB
  + 64 KiB, or 64 KiB, plus a mapping. Not visible today: the only constructor call is
  `server::bootstrap`, which exits on Err and never drops the disk. It bites when a second
  consumer makes one and drops it, as nxinstall and `disk --format` already do with a
  `PartitionIo` per partition.
  Demonstrated by path: `grep -n 'impl Drop' userspace/libfsserver/src/disk.rs` finds only line
  441, `PartitionIo`'s; the three returns above carry no SYS_HANDLE_CLOSE or SYS_MEMORY_UNMAP,
  whereas `PartitionIo::new` (350-354) closes its scratch when the map fails. Of 4 maps in
  libfsserver, the two never unmapped are `Disk`'s and `SectorDisk`'s. Not host-testable: each
  constructor's first statement is a syscall. ownership. Low. **Fix.**
  Fixed in `9630a10c` (delegated): `Scratch` owns its object and mapping and gives both back on
  drop, and `Disk` closes its zero span. Fails without it: disk/tests.rs's failed-create and drop
  tests.

- [x] **fs-d9** `userspace/libfsserver/src/server.rs:607` — `serve_session` copies a session
  request body into a 600-byte stack buffer and silently cuts a longer one. A single-name op
  whose name is over 598 bytes, or a rename whose two names total over 596, then fails its
  length-prefixed parse and is answered Unsupported. Yet `entry_name`'s doc (serve.rs:17-19) says
  FAT names run to 765 bytes, and `Fat::mkdir_at` makes them; the buffer's comment is sized for
  ext4. A client on a FAT directory session sends File::Mkdir (or Unlink or Rename) with a
  699-byte UTF-8 name (233 CJK characters): the 701-byte body is cut to 600, the parse returns
  None, and the server replies Unsupported, though `Fat::mkdir_at` accepts the name and
  `fsck.fat -n` finds the image clean. A stick made on Linux can hold such a file that Nitrox
  cannot remove or rename by that name. TODO(fat-long-utf8-names) records only the listing by
  short alias, not session ops.
  Demonstrated: a temporary test in fs-server-fat/src/volume/write_tests.rs (restored)
  reproduced the copy and parse, then called `Fat::mkdir_at`; `cargo test -p fs-server-fat --lib
  probe_session_body_cap -- --nocapture` printed `PROBE: 699-byte name: session parse None,
  Fat::mkdir_at Ok, listed true, fsck clean`. opportunities. Low. **Fix.**
  Fixed in `4eb4e046`: a session's request is answered by `serve::session` from the message as
  received, with no copy. Fails without it: `a_session_takes_a_name_as_long_as_a_fat_holds` (a
  699-byte name made, renamed and removed; refused with the 600-byte cut back).

- [x] **fs-d10** `userspace/libfsserver/src/server.rs:339` — `try_resolve_directory` clamps a
  suffix longer than MAX_SUFFIX to its first 1024 bytes and resolves that as a directory, while
  `serve::resolve` (serve.rs:149), `try_resolve_rename` and `apply_size` refuse or skip the same
  request as too long. The two guards disagree; today only the kernel's NS_PATH_MAX (1024, equal
  to MAX_SUFFIX; kernel/src/object/namespace.rs:27, syscall/table.rs:1563-1575) keeps it
  unreachable. Latent: raise NS_PATH_MAX (or lower MAX_SUFFIX) and a resolve of a path over 1024
  bytes opens a directory session on whatever its first 1024 bytes name, an ancestor or a sibling
  cut mid-component, inside the same binding, where `resolve()` would answer TooLarge.
  Unverified: not reachable today. Moving the decode into a pure function and testing a 1100-byte
  suffix, or a kernel change that lengthens the path, would settle it. abi. Low. **Fix.**
  Fixed in `4eb4e046`: `serve::directory_open` opens no session for a suffix over `MAX_SUFFIX`,
  which `serve` refuses `TooLarge`. Fails without it: `a_suffix_past_the_longest_opens_no_directory`
  (with the clamp back, a 1,100-byte suffix opened its cut sibling).

**Opportunities**

- [x] **fs-o1** encapsulate: the serve loop's state lives in 11 process-global `static mut`s
  (RECV_MSG, REPLY_MSG, REPLY_HANDLES, FORGET_MSG, CONTENT, WAIT_HANDLES, WAIT_RESULTS,
  SESSION_CH, SESSION_DIR, CONTROL, DIR_BODY) plus each binary's `static mut STORE`. Each access
  is its own `unsafe` block argued 'single-threaded', `received()` lends a `&'static [u8]` over
  the buffer the next receive overwrites, and 16 implicit whole-array references
  (`REPLY_MSG[4..8].copy_from_slice`, `&mut REPLY_MSG[24..]`, `&CONTENT[..len]`) escape edition
  2024's `static_mut_refs` lint — 50 sites (48 of the 58 `unsafe {` blocks in server.rs touch a
  static, found by a brace-matching classifier over `unsafe {` blocks; `grep -c '^static mut'
  server.rs` = 11; `grep -n 'raw mut STORE' userspace/fs-server-*/src/main.rs` = 2), such as
  `userspace/libfsserver/src/server.rs:315`, `userspace/libfsserver/src/server.rs:920`,
  `userspace/libfsserver/src/server.rs:765`, `userspace/fs-server-ext4/src/main.rs:34`. One
  `struct Server { inbox, out, forget, content, dir_body, wait, sessions, control, store }`, made
  once (on `_start`'s 8 MiB stack, about 84 KiB, or in one static handed out by a take-once
  AtomicBool cell), with every helper a `&mut self` method and `Inbox::recv(&mut self, ch) ->
  Result<(Message<'_>, Received), i64>` borrowing the inbox so no receive can happen while a
  message slice lives, in libfsserver::server (the take-once cell beside it, or in libos).
  Checked against `handle_forwarded_resolve` (inbox, content, reply and forget's outbox are four
  disjoint fields), `try_resolve_rename` and `serve_session`. Closes: a receive overwriting a
  payload still borrowed, two live `&mut` to one static, a second `&mut` to STORE, and safe
  `run`/`bootstrap` causing UB (fs-d5). Depends on: nothing. abi, ownership, soundness,
  opportunities. Value high, effort medium. **Fix.**
  Fixed in `4eb4e046`: one `Server`, made by `run` on its stack, an `Inbox` and a `State` of
  endpoints, outboxes, buffers and sessions, in place of eleven `static mut`s.

- [x] **fs-o2** narrow: ext4 bitmaps live in a `[0u8; MAX_BLOCK]` scratch of which only `[..bs]` is
  read and written back, but `bit_clear`, `bit_set` and `bit_unset` index the whole scratch with
  a bit number derived from the disk — 9 sites (`grep -nE 'bit_(clear|set|unset)\('
  userspace/fs-server-ext4/src/ext4.rs`, excluding the definitions and test-only `block_in_use`,
  over 4 bitmap buffers at 851, 1082, 1327, 1375), such as
  `userspace/fs-server-ext4/src/ext4.rs:870`, `userspace/fs-server-ext4/src/ext4.rs:1093`,
  `userspace/fs-server-ext4/src/ext4.rs:1329`, `userspace/fs-server-ext4/src/ext4.rs:1388`.
  `struct Bitmap<'a>(&'a mut [u8])` built from `&mut bitmap[..bs]`, with `is_free`, `set` and
  `unset` answering Corrupt past `len * 8`, in fs-server-ext4 (ext4.rs); it complements a
  superblock geometry check like Linux's. Checked against `alloc_run`, `alloc_inode`, `free_run`
  and `free_inode`. Closes: a bitmap bit indexed past the bytes read from disk (fs-d1). Depends
  on: nothing. abi. Value high, effort small. **Fix.**
  Fixed in `8bd6f2d1`, with fs-d1: `Bitmap`, held by `a_bitmap_bit_past_its_block_is_corrupt`.

- [ ] **fs-o3** remove: a device transfer is a raw `SYS_IO_SUBMIT` followed by a hand-rolled
  `po_wait` that decodes IoResult status@8 and result@16 by byte offset, closes whatever u64 it
  is given, and maps an unfinished wait to a failed transfer; `forget()` repeats the wait inline,
  `serve_loop` decodes handle@0 by hand — 8 sites (5 submits, `grep -n SYS_IO_SUBMIT
  userspace/libfsserver/src/disk.rs`; 3 hand-parsed result sites, `grep -nE
  'results\[(8|16)\]|WAIT_RESULTS\[off' userspace/libfsserver/src/{disk,server}.rs`), such as
  `userspace/libfsserver/src/disk.rs:19`, `userspace/libfsserver/src/disk.rs:102`,
  `userspace/libfsserver/src/disk.rs:241`, `userspace/libfsserver/src/server.rs:410`. Outside
  the subsystem there are 13 more `fn po_wait` copies (coreutils/src/bin/disk.rs:512 calls this
  one; nxinstall/src/main.rs:102, test-harness/src/pattern.rs:265). Use libos: a blocking
  `Op::wait(self) -> Result<IoResult>` that waits again on any return that is not this PO
  signalled and never closes a PO whose I/O is in flight, plus a public `Op::from_po` for
  forget's send and a public typed multi-handle `wait(&[RawHandle], &mut [IoResult])`, in libos
  (exec.rs, objects.rs). Not `block_on` as it stands: `Op::poll` waits with deadline 0 before and
  after the blocking wait, 3 waits per 4 KiB block on `Disk`'s hot path against 1, and
  libfsserver's CLAUDE.md asks for a measured number before the device layer's cost changes.
  Checked against `Disk::read_block`, `SectorDisk::transfer`, `PartitionIo::submit` and
  `forget`. Closes: a scratch reused while an abandoned IRP still DMAs into it (fs-d4); a PO
  leaked on an early return; also makes the wait host-testable under libos's mock. Depends on:
  libfsserver taking a libos dependency (no_std, no alloc, nothing in libfsserver's CLAUDE.md
  forbids it). abi, ownership, soundness, opportunities. Value high, effort small. **Plan**, in P7.

- [ ] **fs-o4** encapsulate: IPC messages are handled as `[u8; 4096]` with hand-written header
  offsets (payload_len at [4..8], handle_count at [8]) and payload views at `PAYLOAD_OFF` with
  length MSG_LEN - PAYLOAD_OFF = 4072, not IPC_PAYLOAD_SIZE = 4008, so they overlap the handle
  array; the kernel's `seal()` refusing over 4008 is all that keeps that harmless — 34 sites
  (16 header statements: `grep -nE '_MSG\[4\.\.8\]|_MSG\[8\] =|RECV_MSG\[4\]'
  userspace/libfsserver/src/server.rs`; 18 payload views: `grep -nE
  'add\(PAYLOAD_OFF\)|\[PAYLOAD_OFF\.\.\]|\[24\.\.\]'`), such as
  `userspace/libfsserver/src/server.rs:164`, `userspace/libfsserver/src/server.rs:319`,
  `userspace/libfsserver/src/server.rs:128`, `userspace/libfsserver/src/server.rs:924`. Type the
  buffers as libkern's `#[repr(C, align(4096))] IpcMsg` with `payload(&self) -> &[u8]` bounded by
  `payload_len.min(IPC_PAYLOAD_SIZE)` and `compose(&mut self, handles, f) -> Option<usize>`
  stamping payload_len and handle_count together (an `Outbox::stage` over it), plus
  `channel_recv_msg(ch, &mut IpcMsg)` beside `channel_recv`, in libkern (abi.rs, channel).
  Checked against `send_ready` (165-166), `reply_dir_handle` (546-547) and `received` (319-323).
  Closes: a payload view sized by the wrong bound, a header stamped at the wrong offset or left
  stale (written separately at 7 send sites); 9 raw-slice unsafe blocks become field access.
  Depends on: pairs with fs-o1. abi, soundness. Value medium, effort small.
  **Plan**, as rt-o5, in P7.

- [ ] **fs-o5** encapsulate: server.rs's loop makes its raw syscalls inline, so none of the rules
  libfsserver/CLAUDE.md states for it is held by a host test, only by QEMU gates; serve.rs's pure
  core is the only host-tested part — 25 sites (`grep -c 'syscall[0-9]'
  userspace/libfsserver/src/server.rs` = 23, plus 2 `channel_recv_raw` at 123, 309), such as
  `userspace/libfsserver/src/server.rs:274`, `userspace/libfsserver/src/server.rs:375`,
  `userspace/libfsserver/src/server.rs:506`, `userspace/libfsserver/src/server.rs:421`. Make the
  loop generic over a small `Kernel` trait (recv, send, wait, close, memory create/map,
  duplicate, clock), or build it on the libos types with libos's sys seam exposed to downstream
  tests, in libfsserver::server over libos, so `handle_forwarded_resolve`, `serve_session` and
  `serve_control` run on the host through each server's volume as serve.rs does. Closes: rules
  held only by gates (every forwarded request answered; free only after Forget; sessions end on
  rmdir; `.` and `..` refused; rename before the session path). Depends on: fs-o1, fs-o3, fs-o4,
  fs-o7 and fs-o8; libos's sys mock is `#[cfg(test)]` inside libos (sys.rs:118-130) with no
  Cargo feature, so a downstream crate's tests link the real syscalls. opportunities. Value
  medium, effort large. **Plan**, as sup-o9, in P7.

- [ ] **fs-o6** encapsulate: scratch and content MemoryObjects are created and mapped by hand,
  reached through `scratch_addr as *const u8` with `from_raw_parts` or `copy_nonoverlapping`
  (bounds held by an `assert!` or a comment), and unmapped, or never unmapped, by hand; error
  paths leak — 20 sites (disk.rs unsafe blocks other than the 7 submit/wait ones: 6 raw
  slice/copy at :146, :191, :257, :264, :378, :385 and 9 create/map/close/unmap at :69, :75, :80,
  :220, :226, :342, :349, :352, :445; plus `make_content_memobj`'s 5 blocks at server.rs:217,
  :223, :226, :230, :236; 4 create+map sequences, 1 hand-written Drop at :441), such as
  `userspace/libfsserver/src/disk.rs:146`, `userspace/libfsserver/src/disk.rs:257`,
  `userspace/libfsserver/src/disk.rs:446`, `userspace/libfsserver/src/server.rs:231`. A `!Sync`
  `Mapping<M>` owning its `Handle<Memory, M>`: `new(mem, len) -> Result<Self>` (closing the
  object if the map fails), bounds-checked `copy_in(&self, at, &[u8])` and `copy_out(&self, at,
  &mut [u8])` as bounded raw copies through `&self` (no Rust reference over memory a device DMAs
  into), `handle()` for `IoOp.buffer`, `into_handle(self)` that unmaps and keeps the object,
  Drop unmapping then closing, in libos (objects.rs, beside `Handle<Memory>::map`, which returns
  `*mut u8` today). Checked against `Disk::read_at`, `SectorDisk` copy_in/copy_out,
  `PartitionIo` (its Drop disappears) and `make_content_memobj`. Closes: scratch handles leaked
  on error paths and never unmapped (fs-d8); a slice taken over a mapping longer than the
  mapping (`Disk` maps PAGE bytes and moves IO_BLOCK per transfer with nothing tying the two: a
  probe with IO_BLOCK = 8192 built and passed the disk tests, and only the kernel's size check
  on the preceding read stops it). Depends on: nothing (`Handle<Memory>::create/map/unmap/
  into_read_only` exist) and libfsserver taking libos. abi, ownership, soundness, opportunities.
  Value medium, effort small. **Plan**, as rt-o3, in P7.

- [ ] **fs-o7** encapsulate: handles are bare u64s closed by hand at each exit, and a message is
  staged in REPLY_MSG/REPLY_HANDLES/FORGET_MSG and sent with raw `SYS_CHANNEL_SEND`; whether a
  transferred handle is ours (until the send succeeds) or the kernel's is tracked by convention
  — 18 sites (`grep -nE 'syscall[0-9]' userspace/libfsserver/src/server.rs` = 23, minus
  `make_content_memobj`'s 5: SEND 7, CREATE 1, CLOCK_READ 1, HANDLE_CLOSE 5, DUPLICATE 1,
  WAIT 3; 7 header stamps via `grep -n '\[4\.\.8\]\.copy_from_slice'`, 3 stagings, 2 closes after
  a failed send), such as `userspace/libfsserver/src/server.rs:277`,
  `userspace/libfsserver/src/server.rs:533`, `userspace/libfsserver/src/server.rs:148`,
  `userspace/libfsserver/src/server.rs:388`. The send-side twin of `Received`: `Outgoing`
  (handles owned, closed on drop unless sent) with `channel_send(ch, msg, payload_len, handles,
  mode) -> Result<(), (i64, Outgoing)>`, stamping payload_len and handle_count itself and
  returning the untaken handles on failure (the kernel's send is atomic-or-fail, table.rs:
  1966-1979), in libkern::channel beside `Received`, or as libos `Handle<Channel, M>` with
  `Channel::pair()`, `wait` and a clock wrapper once libos has a Channel kind (absent today:
  libos handle.rs:19-29). Sessions then hold owned endpoints. Checked against `send_ready`,
  `send_forwarded` (the close loop at 282 becomes `drop(unsent)`) and `open_dir_session` (the
  close at 586). Closes: a transferred handle leaked or closed twice after a failed send (both
  paths are right by hand today; the class seen before is 'a failed reply hangs a forwarded
  resolve'). Depends on: nothing for libkern; libos Channel kind and Part E's syscall surface
  for the libos form; pairs with fs-o1. ownership, soundness, opportunities. Value medium,
  effort medium. **Plan**, as sup-o2, in P7.

- [ ] **fs-o8** encapsulate: the session table and control slot (SESSION_CH, SESSION_DIR,
  CONTROL) are bare u64 endpoints and directory ids in three `static mut`s; a slot is freed by
  writing 0 and closing the handle by hand, and the wait set's width (<= MAX_WAIT_HANDLES) holds
  only because `session_capacity()` and the wait-set build agree, one invariant kept by two
  guards — 12 sites (blocks naming SESSION_CH, SESSION_DIR or CONTROL, cross-checked with `grep
  -nE '\b(SESSION_CH|SESSION_DIR|CONTROL)\b' userspace/libfsserver/src/server.rs`: 62, 496, 511,
  562, 577, 593, 621, 765, 810, 838, 844, 1023, including the hand closes at 501 and 845), such
  as `userspace/libfsserver/src/server.rs:60`, `userspace/libfsserver/src/server.rs:765`,
  `userspace/libfsserver/src/server.rs:494`, `userspace/libfsserver/src/server.rs:838`. `struct
  Sessions { slots: [Option<(Endpoint, u64)>; MAX_SESSIONS], control: Option<Endpoint> }` with
  `open` (a full table hands the endpoint back), `slot_of`, `dir`, `end_on(dir)`,
  `close_control()` and `wait_set(serve_end, &mut [u64; MAX_WAIT_HANDLES]) -> usize`, the one
  place that holds the width; `rmdir_at`'s `freeing` becomes `|id| sessions.end_on(id)`; in
  libfsserver::server (a sessions module; Endpoint from libos once it has a channel handle,
  otherwise a local owned newtype). Checked against `open_dir_session` (562-580),
  `end_sessions_on` (508-515) and `serve_control`'s PeerClosed arm (842-848). Closes: the width
  invariant split across two guards; a slot cleared without its endpoint closed, or closed twice;
  a stale handle number matching a live slot; makes capacity 30/31 and slot reuse host-testable.
  Depends on: fs-o1 for the struct. opportunities. Value medium, effort small. **Plan**, in P7.

- [ ] **fs-o9** narrow: counts returned by a `Volume` are trusted as slice bounds by the loop
  (`Mapped::runs` indexes `runs[..n]`; `read_file` and `read_file_range` lengths index
  `CONTENT[..len]`); both Volumes keep them in bounds, but a third that answers a file's size
  rather than the bytes copied would panic the server — 3 sites (manual read of serve.rs and
  server.rs), such as `userspace/libfsserver/src/serve.rs:171`,
  `userspace/libfsserver/src/serve.rs:182`, `userspace/libfsserver/src/serve.rs:219`,
  `userspace/libfsserver/src/server.rs:232`. `Volume` methods return sub-slices of the caller's
  buffer: `fn read_file<'o>(&self, path: &[u8], out: &'o mut [u8]) -> Result<&'o [u8], FsError>`,
  likewise `read_file_range`, and `map_file` returning `(Mapped, &'r [BlockRun])`, in libfsserver
  (volume.rs). Closes: a Volume answering a count larger than the buffer it filled crashes the
  server. Depends on: nothing. abi. Value low, effort small. **Later.**

- [ ] **fs-o10** encapsulate: test helpers build a 1 MiB Store with
  `Box::<Store<SERVER_BLOCKS>>::new_zeroed().assume_init()`, restating outside the type that
  all-zero is a valid Store — 3 sites (`grep -rn 'new_zeroed().assume_init'
  userspace/fs-server-ext4 userspace/fs-server-fat`), such as
  `userspace/fs-server-ext4/src/lib.rs:1477`,
  `userspace/fs-server-fat/src/volume/write_tests.rs:724`,
  `userspace/fs-server-fat/src/volume/write_tests.rs:742`. `Store::<N>::boxed() ->
  Box<Store<N>>` behind a host-only feature (e.g. `alloc`) enabled by the server crates in
  dev-dependencies, in libfsserver::cache. Closes: a field added to Store that zero does not
  inhabit (an enum, a NonZero, a reference) turning all three sites into UB with no compile
  error. Depends on: nothing. abi, soundness. Value low, effort small. **Later.**

- [x] **fs-o11** narrow: each server's 1 MiB `static mut STORE` is reached once by
  `&mut *(&raw mut STORE)`, justified only by '_start runs once'; these are the only `unsafe`
  blocks in either server crate — 2 sites here (`grep -rn '&mut \*(&raw mut'
  userspace/fs-server-ext4/src userspace/fs-server-fat/src`; 16 across userspace), such as
  `userspace/fs-server-ext4/src/main.rs:34`, `userspace/fs-server-fat/src/main.rs:33`. A
  `TakeOnce<T>(UnsafeCell<T>, AtomicBool)` with one `unsafe impl<T: Send> Sync`, `const fn
  new(v)` and `take(&'static self) -> Option<&'static mut T>` (a second take answers None), in
  libos, or `libfsserver::cache::server_store()` for Store alone; subsumed if fs-o1 is taken.
  Closes: a second `&mut` to the same static, and the last `unsafe` in the two server crates.
  Depends on: nothing. ownership, opportunities. Value low, effort small. **Fix.**
  Fixed in `4eb4e046`: each binary's store is a `cache::OnceStore`, its one `&mut` handed out once.
  Fails without it: `a_store_is_taken_once`.

What held up: the `unsafe` byte paths are sound. Every `recv` buffer is exactly the size the
kernel writes (4096 bytes, at most 8 handle words in `Received`), and every send reads whole
`StoredMsg`s from 4096-byte buffers with count <= 1 handle. The wait count stays <=
MAX_WAIT_HANDLES (`session_capacity` leaves one slot free while CONTROL is open, and CONTROL only
goes to zero), and WAIT_RESULTS is walked within `waited <= count`. Hand-parsed offsets agree with
libkern's asserted `IoResult` and `IpcMsg` layouts. Every IoOp transfer fits its MemoryObject, and
slices over a scratch are made only after `po_wait` saw completion and dropped before the next
submit. `send_forwarded`, `forget` and `open_dir_session` close handles exactly when the kernel
left them with the sender, since `complete_forwarded_reply` always consumes on success and every
earlier error leaves the table unchanged. `make_content_memobj` unmaps before restricting and
sending, and no earlier file's bytes reach a later client, because the memobj is fresh and
page-rounded, both volumes fill `out[..n]` whole, and `StoredMsg::seal` zeroes the stale tail.
`forget` uses `FORGET_MSG` and local wait buffers, so it cannot clobber REPLY_MSG's live `reply`
or the WAIT_RESULTS batch. A session freed earlier in a batch is skipped by the slot lookup, and
31-bit handle generations stop a stale value from reaching another object. `end_sessions_on`
cannot free the calling session's own slot, though the SAFETY comment at server.rs:620 says
more than is true. Suffix, body and reply bounds hold (`resolve`, `serve_read_range`,
`apply_size` and `try_resolve_rename` check <= MAX_SUFFIX before copying; the kernel's forwarded
suffix is <= 1024; `serve_session` cuts at 600 bytes and the length-prefixed parses refuse the
cut; `DirReplyWriter` and `mark_grown` clamp to the reply). ext4 directory walks go through
`record()`, the extent `header()` bounds entries and depth (<= 5), `descend()` and `read_node`
check each child's depth, and `seg()`/`cut()` check each run is on the device. FAT `bpb::parse`
validates geometry, chain walks are bounded, Lfn ordinals index a 260-unit buffer, and the
RefCell borrows (`cache`, `ids`, `orphans`) are never doubled; `MetaCache` holds its borrow across
the device read without re-entry. Both `STORE` references are taken once, and the test-only
`new_zeroed().assume_init()` is sound (Miri ran a zeroed-Store probe through `read_meta`,
`write_at` and `forget` with no UB). `po_wait`'s early-return branch is unreachable today (see
fs-d4). Probes were restored (`git checkout` of extent.rs and hostile_tests.rs, write_tests.rs)
and `git status` was clean at each lens's end.

### protocols — main at 135055a8, 2026-10-27

Lenses: abi, ownership, soundness, opportunities; returned nothing: none.
Defects: found 20, refuted by a boot 0, reported 20 (the 20 findings merge into 8 items below).
Opportunities: 19 proposed, merged into 6 patterns.

The inventory, `cargo xtask unsafe-inventory userspace/librsproto userspace/libstream userspace/libgpt`:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `userspace/librsproto` | 3518 | 10 | 0 | 0 | 0 | 10 | 0 | 0 | 0 | 0 | 0 |
| `userspace/libstream` | 1142 | 9 | 0 | 0 | 0 | 9 | 0 | 0 | 0 | 0 | 0 |
| `userspace/libgpt` | 326 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `total` | 4986 | 19 | 0 | 0 | 0 | 19 | 0 | 0 | 0 | 0 | 0 |

**Defects**

- [x] **proto-d1** `userspace/nxsh/src/main.rs:292` — Every error return in
  `NitroxHost::run`/`run_pipeline` after the first `pipe()` leaks the handles the pipeline holds
  and leaves spawned stages blocked. The root cause is at the protocol boundary:
  `libstream::setup::send_setup_full` documents its handles as moved, but when it fails before the
  commit they are still the caller's, and nxsh closes only the terminal. At a prompt, type
  `let s = "<1000 x's>"` (1,012 bytes, under LINE_MAX 1024), then `echo x (s) (s) (s) (s) (s)`.
  The stage's argv is 5,005 bytes, so `SetupPayload::encode` returns `SetupTooLarge`
  (setup.rs:185) before any syscall, and no handle moves. main.rs:293-297 closes `terminal` and
  returns Err. That leaks the stage's stdout tx, stderr duplicate, rx, `setup_shell`, process
  handle, `err_rx` and `err_tx`, and in a longer pipeline stdin, the feed and earlier stages'
  process handles. Because `setup_shell` stays open, the spawned stage waits forever in
  `recv_setup`'s `sys_wait` (setup.rs:391), and its process handle never reaches `children`, so it
  can be neither terminated nor reaped. The same class occurs at main.rs:245/246 (a `pipe()`
  failure), 259 (a spawn failure: a file starting `\x7fELF` passes the magic-only `what_runs` and
  fails the spawn, leaving `setup_stage` open and 6+ handles per attempt) and 417 (a stage writing
  more than MAX_CAPTURE returns before closing rx and before reap, so the tail stage blocks on a
  full ring). Each attempt costs the long-lived login shell its handles until logout.
  Demonstrated: a host test added to nxsh/src/eval.rs ran the two lines through `MockHost`
  (`cargo test -p nxsh --lib --target x86_64-unknown-linux-gnu probe_survey`). `Host::run`
  received argv lengths [4,1,1000,1000,1000,1000,1000], and `SetupPayload{streams:
  STDOUT|STDERR, argv, terminal: true}.encode()` returned `Err(SetupTooLarge)`; the file was
  restored. The kernel leaves the sender's handles in place on every failure before the commit
  (kernel/src/syscall/table.rs:2045). The leak is shown by reading nxsh main.rs:220-298 and
  404-436, not by a boot; a boot would show the hung stage and the growing handle count.
  nxterm backend.rs:288-296 and view-broker main.rs:1484-1492 close everything in the same
  situation. Lenses: abi, ownership, soundness, opportunities. Medium. **Fix.**
  Fixed in `2991d0dc`, with sh-o4 and proto-o2: every pipe, spawn and duplicate nxsh makes is made
  through `nxsh::pipeline::Pipeline`, whose drop closes what it still holds, then stops and waits
  for every stage not reaped, and `send_setup_full` takes its handles on every return. Fails without
  it: `a_start_failing_at_any_call_gives_everything_back` (each of a three-stage start's 20 calls
  failing in turn) and `a_started_pipeline_gives_everything_back_left_or_reaped`.
- [x] **proto-d2** `userspace/libstream/src/wire.rs:532` — `read_value` recurses once per nesting
  level (List -> `read_list` -> `read_value`; Record -> `Schema::decode` -> `read_row_values` ->
  `read_value`) with no depth limit, and dropping the decoded `Value` recurses the same way. A
  nested List costs 5 input bytes per level, and docs/spec/typed-stream-format.md:93 says lists
  "may nest arbitrarily". A stage, possibly in a restricted view, writes a one-column List stream
  whose cell is nested 40,000 or more deep (about 200 KB, under nxsh's 1 MiB MAX_CAPTURE).
  nxsh's `decode_output` (eval.rs:1946) calls `Table::decode` and overflows the 8 MiB user stack
  into its guard, so the login shell faults and is suspended. `open x.tsm` (ops.rs:784, any size,
  for example from a stick), nxfiles (main.rs:53) and clip's stdin (clip.rs:215) do the same.
  Single-message decoders (the setup payload, logging-service `render_typed` main.rs:498,
  view-broker `view_env` main.rs:1654) are bounded to about 800 levels by the 4,008-byte payload,
  so any change that decodes a chunked stream in a service would let a peer crash it. Fix: a
  depth limit carried by `ByteSource` or the decoder.
  Demonstrated: host probes (removed) decoded nested lists on a thread with an 8 MiB stack,
  release build. Depths of 20,000 to 30,000 decoded; depth 40,000 (200,027 bytes) aborted with
  "thread has overflowed its stack" (SIGABRT), as did 50,000, 200,000 and 209,000 (1,045,026
  bytes). What the target does with the fault needs a boot. Lenses: abi, ownership, soundness,
  opportunities. Medium (soundness rated it medium as latent-high; the other three low).
  **Fix**, with the bash's proto-7, moved here from Part H.
  Fixed in `10bcbb11` (delegated): the codec reads and writes at most 64 levels of `List` and
  `Record`, refusing deeper `TooLarge`. Fails without it:
  `a_very_deep_list_is_refused_without_overflowing_the_stack` (aborts on the old code),
  `one_level_past_max_depth_is_refused_both_ways` and `records_and_lists_count_together`.
- [x] **proto-d3** `userspace/libstream/src/wire.rs:590` — The TSM1 decoder's memory use has no
  bound relative to its input. A Null-typed column takes 0 bytes per row, yet each row allocates a
  `Vec<Value>` of `schema.fields.len()` entries at 32 bytes each, so one `REC_DATA` byte costs
  32 bytes times the column count. A stage writes a schema of 1,000 empty-named Null columns
  (about 4 KB) and then about 1M one-byte rows, which fits under nxsh's 1 MiB MAX_CAPTURE
  (main.rs:61). `Table::decode` then needs about 33 GB and nxsh aborts on OOM; `open x.tsm`
  (ops.rs:784) has no size cap at all. Nothing limits memory per process (rlimits are deferred,
  docs/rationale/deferred-decisions.md:2307), so other processes' allocations may fail first
  (unverified). Fix: a value budget carried by the decoder, or a cap of rows x columns per input
  byte.
  Demonstrated: a host probe with `cargo test --release -p libstream --lib`, file restored. A
  5,017-byte stream (1,000 Null columns, 1,000 one-byte rows) decoded to 1,000 x 1,000 `Value`s,
  at least 32,000,000 bytes, about 6,400 times the input. The system-wide effect needs a boot.
  Lenses: abi, soundness. Medium. **Fix**, with the bash's proto-8, moved here from Part H.
  Fixed in `10bcbb11` (delegated): a `ByteSource` yields at most its length plus 4,096 values. Fails
  without it: `zero_byte_cells_cannot_outgrow_the_input` and `the_value_budget_is_exact`.
- [x] **proto-d4** `userspace/libgpt/src/table.rs:298` — `disk_blocks: alternate_lba + 1` adds a
  field read from an untrusted disk with no overflow check: it panics where overflow checks are on
  and wraps `disk_blocks` to 0 where they are off. A stick whose GPT header has `alternate_lba =
  u64::MAX` and a recomputed header CRC is read by storage-service `read_table`
  (storage-service/src/main.rs:441) on every arriving disk, and by nxinstall (main.rs:324).
  Today's release build has overflow checks off and no consumer reads `disk_blocks`, so nothing
  fails. Adding `overflow-checks = true` to userspace `[profile.release]`, or any caller doing
  arithmetic on `disk_blocks`, makes the same stick abort the storage service (`panic = "abort"`),
  which makes it high. Fix: `checked_add` and return `ReadError::Unsupported`.
  Demonstrated: a host test built a valid table, set header bytes 32..40 to `u64::MAX` and
  recomputed the CRC (file restored). Debug panicked with "attempt to add with overflow" at
  libgpt/src/table.rs:298:22; release returned Ok with `disk_blocks = 0`. Lenses: abi,
  soundness, opportunities. Medium (abi rated it medium as latent; the others low).
  **Fix**, with the bash's proto-15, moved here from Part H.
  Fixed in `10bcbb11` (delegated): `alternate_lba.checked_add(1)`, `Unsupported` on overflow. Fails
  without it: `an_alternate_header_at_the_last_possible_block_is_unsupported_not_a_panic`.
- [x] **proto-d5** `userspace/libstream/src/setup.rs:314` — `send_setup_full` returns Err in two
  opposite ownership states. Before the send (an encode error, or r < 0) the handles are still the
  caller's. After a committed `SENDMODE_BLOCK` send (waited < 1 at :314, or PO status < 0 at :318)
  the kernel has already closed them in the sender. Callers cannot tell the two apart and close
  them on any Err. The setup message queues because the stage's ring is full, then the stage dies
  before delivery. The PO completes PeerClosed, and nxterm (backend.rs:296) closes `terminal`
  again while view-broker (main.rs:1489) runs `p.handles.close()` on handles that already moved.
  The kernel's generation check makes these closes fail harmlessly today. It would bite with
  handle-number reuse, or if a setup channel were reused for a second message.
  Unverified: the kernel closes the handles on Sent or Queued (kernel/src/syscall/table.rs, the
  `sys_channel_send` Block arm), and setup.rs:314-320 then returns Err. A fresh depth-4 setup
  channel takes the message as Sent, so the Queued-then-PeerClosed order needs a gate with a full
  ring and a stage that exits before the message is delivered. Lenses: soundness. Low. **Fix.**
  Fixed in `2991d0dc`: `send_setup_full` takes its handles by value and consumes them on every
  return, closing them itself when the message could not be made or the kernel refused it; nxsh,
  nxterm and the view broker close none. Fails without it: boot-probe's
  `a_refused_setup_message_closes_its_handles_test` (test-qemu fails with the close removed). The
  committed half, the double close, has no deterministic trigger; it ends because no caller closes.
- [x] **proto-d6** `userspace/libstream/src/channel.rs:114` — `ChannelReceiver::receive` appends
  every message to one `Vec` until the producer sends `last`, with no size limit. nxsh already
  replaced it with its own loop capped at MAX_CAPTURE (main.rs:361-412), which shows the cap
  belongs in this API. A producer that never finishes, piped into `clip --copy`
  (coreutils/src/bin/clip.rs:215), grows clip's heap until an allocation fails; clip compares the
  text with MAX_CLIP_BYTES (about 3.9 KB) only afterwards. The test-harness readers use it too.
  Unverified: found by reading channel.rs:114-121 and clip.rs:215-229. A host test with a `MsgPort`
  whose `recv` never returns true plus a counting allocator would settle it, or a boot running
  `<endless producer> | clip --copy`. Lenses: abi, ownership, opportunities. Low. **Fix.**
  Fixed in `10bcbb11` (delegated): `receive` takes a cap and fails `TooLarge` past it; clip and the
  harness pass 1 MiB. Fails without it: `receive_stops_once_the_stream_passes_the_cap`.
- [x] **proto-d7** `userspace/libstream/src/wire.rs:668` — `Schema::encode` writes a field name's
  length as `f.name.len() as u16` with no range check, then writes every byte of the name;
  `WireErrorRecord::encode` does the same with `field_name` at :733. A name of 65,536 bytes or
  more is framed with a wrapped length, and the reader parses the rest of the name as further
  fields, types, rows or a terminator chosen by whoever supplied the name. librsproto's encoders
  all check `> u16::MAX` before such a cast. Only a producer that forwards a 64 KiB column name
  from data reaches it today (whether any current producer does is unverified).
  Demonstrated: host tests added to wire.rs, file restored. A schema with a 65,539-byte name
  encoded Ok, and `decode_header` returned `Err(BadTypeTag(97))`, the byte after the wrapped
  3-byte name read as a type tag. A second probe with a name whose tail spelled
  `\x04\x00\x01\x06\x00\x00\x00forged` decoded as one field named "ok" followed by a `REC_DATA`
  row `Ok([Str("forged")])`. Lenses: abi, soundness. Low.
  **Fix**, with the bash's proto-14, moved here from Part H.
  Fixed in `10bcbb11` (delegated): both encoders refuse a name over 65,535 bytes before writing.
  Fails without it: the two `*_name_over_u16_is_refused_before_any_write` tests.
- [x] **proto-d8** `userspace/librsproto/src/session.rs:158` — `Dir::rename` builds its request in
  a 512-byte buffer, but two names of the maximum 255 bytes need 2+255+2+255 = 514 bytes.
  `rename_request` (file.rs:384-387) returns None, so the rename fails client-side with
  `DirError::Protocol`. Only test-harness calls `Dir::rename` today; real moves go through
  `RESOLVE_RENAME`. Unverified: shown by arithmetic from the code; a host test renaming
  255-byte names would settle it. Lenses: abi. Low. **Fix.**
  Fixed in `10bcbb11` (delegated): the buffer is `RENAME_REQUEST_MAX`. Fails without it:
  `rename_request_takes_two_longest_names`.

**Opportunities**

- [ ] **proto-o1** encapsulate: `Dir`, `Clipboard` and `Desktop` hold their endpoint as a raw
  `u64` with an explicit `close(self)` and no `Drop`, so every caller writes `let r = op;
  x.close(); r` on every path. `from_endpoint` says it "takes ownership" yet its Err path neither
  closes nor returns the endpoint, and test-harness (harness.rs:2941) uses it as a borrow. The
  three `connect`/`open` functions repeat one NS_LOOKUP, `po_wait`, status and `endpoint == 0`
  sequence. The documented reason for no `Drop` ("dropping cannot report a failure",
  session.rs:50) does not hold, because `close` returns `()`. 66 construction sites, 20 outside
  test-harness (`grep -rn 'Dir::open\|Dir::from_endpoint\|Desktop::connect\|Desktop::from_endpoint
  \|Clipboard::connect\|Clipboard::from_endpoint' --include=*.rs userspace | grep -v librsproto/ |
  grep -v '// \|/// ' | wc -l`); 6 of the 19 unsafe blocks are the three NS_LOOKUPs and three
  endpoint closes (`grep -n unsafe userspace/librsproto/src/{session,clipboard,desktop}.rs`),
  such as `userspace/librsproto/src/session.rs:64`, `userspace/librsproto/src/clipboard.rs:377`,
  `userspace/librsproto/src/desktop.rs:180`, `userspace/libfs/src/lib.rs:470`. A
  `session::Endpoint(u64)` that closes on drop, with `into_raw`; `from_endpoint(Endpoint, buf) ->
  Result<Self, (Endpoint, E)>` handing the endpoint back on refusal; one `session::connect(ns,
  path, rights) -> Result<Endpoint, WireError>` in place of three lookups; or one `Session<'a>`
  wrapped by all three clients. Home: `librsproto::session` (a crate-local newtype, or libos
  `Handle` on the `io` feature; session-mgr and libsession take librsproto without `io`, so
  libos is open here, and libsurface/src/ipc.rs:106-116 already resolves a channel that way).
  Closes: an endpoint leaked by an early return or `?` between connect and close, each leak
  holding one of the server's MAX_SESSIONS slots (libsurface ipc.rs:121 records "a leaked
  endpoint eventually makes /dev/draw/new fail for every process"), and a borrowed endpoint
  closed by its new owner. Depends on: nothing (the `po_wait` wrapper in proto-o3 if it lands
  first). Lenses: abi, ownership, soundness, opportunities. Value high, effort medium.
  **Plan**, as sup-o2, in P7.
- [x] **proto-o2** encapsulate: `send_setup_full(channel, &Streams, Option<u64>, ...)` moves the
  stream and terminal handles on success and leaves them with the caller on most failures. `Streams`
  is `Copy` over `Option<u64>` and `pipe()` returns `(u64, u64)`, so each caller restates the rule
  by hand and they disagree: nxsh closes only the terminal (proto-d1), nxterm closes the terminal,
  view-broker closes everything including handles already moved (proto-d5). 26 sites: 16 `pipe()`
  calls in 5 files plus 10 `send_setup*` callers outside libstream, 5 in production (`grep -rn
  '[^_a-z]pipe(' userspace --include=*.rs | grep -v ^userspace/libstream | grep -v 'fn pipe\|//'
  | wc -l`; `grep -rn 'send_setup\(_env\|_full\)\?(' userspace --include=*.rs | grep -v
  ^userspace/libstream`), such as `userspace/libstream/src/setup.rs:270`,
  `userspace/nxsh/src/main.rs:292`, `userspace/view-broker/src/main.rs:1483`,
  `userspace/nxterm/src/backend.rs:281`. `send_setup_full(channel, streams: Streams, terminal:
  Option<Endpoint>, argv, env) -> Result<(), WireError>` takes every handle by value and closes
  what it was given on any error, as libviews::request does (libviews/src/lib.rs:103); `Streams`
  stops being `Copy`, and `pipe` returns owning handles. Alternatively return `Err` carrying the
  handles back only when the kernel refused before the commit (`SetupError::NotSent {..}` versus
  `Lost`). nxterm and view-broker lose their hand-written close code; desktop-shell and libsession
  pass empty streams. Home: `libstream::setup`. Closes: the nxsh pipeline leak and the stale
  double close. Depends on: an owned-handle type (proto-o1's `Endpoint`, or libos). Lenses: abi,
  ownership, soundness, opportunities. Value high, effort medium. **Fix.**
  Fixed in `2991d0dc`: the handles are taken by value and gone on every return. Owning handle types
  are P7's (proto-o1), so `Streams` stays `Copy` until then.
- [ ] **proto-o3** encapsulate: every syscall in the subsystem is a raw `syscallN` with a
  hand-built pointer, length and result buffer: WAIT x6 (each with a `[0u8; 24]` result decoded at
  bytes 8..12 and 16..24), HANDLE_CLOSE x6 (3 endpoints, 3 POs), NS_LOOKUP x3, CHANNEL_SEND x3,
  CHANNEL_CREATE x1. The `po_wait` helper (wait, close the PO, read status) is copied 15 times
  across 14 files that disagree on what a failed wait returns, 34 `SYS_HANDLE_CLOSE, po` sites
  repo-wide. 19 sites (`grep -n unsafe userspace/librsproto/src/*.rs userspace/libstream/src/*.rs
  | grep -v '// SAFETY'`), such as `userspace/librsproto/src/session.rs:239`,
  `userspace/librsproto/src/session.rs:273`, `userspace/libstream/src/channel.rs:218`,
  `userspace/libstream/src/setup.rs:311`. Safe `Result` wrappers beside `libkern::channel_recv`:
  `wait_one(h, deadline) -> Result<IoResult, i64>` (typed to the ABI `IoResult`),
  `po_wait(po, deadline)` that closes the PO exactly once on every path, `ns_lookup(ns, path,
  rights)`, `channel_send(ch, &[u8; IPC_MSG_SIZE], handles, mode, deadline)` refusing
  `handles.len() > IPC_HANDLE_MAX`, `channel_create(depth)` and `handle_close(h)`. Checked against
  session.rs:239/273, channel.rs:198/218 and setup.rs:293/311/391: one `wait_one` serves the four
  waits, one `channel_send` the three sends, one `ns_lookup` the three lookups. This removes every
  unsafe block in the subsystem. Home: libkern (its CLAUDE.md asks for safe wrappers; `channel_recv`
  is the precedent). An owning `PendingOp` guard belongs in libos (`Op::from_po` is `pub(crate)`
  today), but libstream is linked into session-mgr through libsession, whose "still no libos" rule
  keeps it off libos. Closes: a result or message buffer shorter than what the kernel copies (the
  HandleInfo 16-vs-24-byte stack smash and the 256-byte receive classes, here for sends and waits),
  a PO leaked or closed twice, and the copies' disagreement. Depends on: nothing. Lenses: abi,
  ownership, soundness, opportunities. Value high, effort medium. **Plan**, in P7.
- [ ] **proto-o4** encapsulate: librsproto parsers check `body.len() >= CONST` and then read at
  fixed offsets with `get_u16/32/64` or `from_le_bytes([b[i], ..])`, which panic past the end, so
  guard and offsets agree only by hand. Three iterators validate at parse time and then index
  unchecked: `AccountList::iter` (auth.rs:218), `ClipList::entries` (clipboard.rs:318) and
  `views::Args::next` (views.rs:177). All 77 parsers hold today. A one-byte slip in a guard is a
  panic in a server (storage-service `parse_mount`, auth-service, libfsserver
  `parse_resolve_request`, the compositor's surface readers). About 220 sites (179 `get_u*` reads
  by `awk` skipping `#[cfg(test)]`, 41 `from_le_bytes([b[..` reads), such as
  `userspace/librsproto/src/file.rs:44`, `userspace/librsproto/src/auth.rs:107`,
  `userspace/librsproto/src/surface.rs:219`, `userspace/librsproto/src/views.rs:177`. A
  `pub(crate) struct Reader<'a> { b: &'a [u8], at: usize }` with `u8/u16/u32/u64/i32() ->
  Option<_>`, `bytes(n)`, `lenpfx_u16()`, `skip(n)`, `rest()` and `finish()`, the no-alloc twin of
  `libstream::wire::ByteSource`; iterators re-walk with the same `Reader`. Checked against
  `parse_read_range_request`, `parse_authenticate_reply`, `WindowInfo::read` and `parse_mount`.
  Home: librsproto `lib.rs`. Closes: a length guard that disagrees with its offsets (weakening
  `parse_ping`'s guard by one byte made a fuzz probe panic at lib.rs:286 at once). Depends on:
  nothing. Lenses: opportunities. Value medium, effort medium. **Plan**, in P8.
- [ ] **proto-o5** encapsulate: each IO module hand-rolls its channel I/O: IPC header offsets
  (payload_len at 4, handle_count at 8, flags at 10, payload at 24) redefined in session.rs:23-27,
  channel.rs:148-151 and setup.rs:235-237, a hand `payload_len` write, a `buf.len() <
  IPC_MSG_SIZE` check on a caller's `&mut [u8]`, "recv; on WouldBlock wait; loop", and `E_WOULDBLOCK
  = -11` / `E_PEERCLOSED = -13` literals. Two ways to hold it: type the message, and give
  libos a `Channel` marker with modes (libos.md's mode table lists Send/Recv/SendRecv but
  `handle.rs` has none; the doc says "settle when wrapping Channel"). About 67 sites for the typed
  handles (3 endpoint closes, 36 `IpcPort::new(`, 15 `pipe(`, 13 `send_setup*` outside libstream),
  and 20 for the message (3 raw sends, 10 offset consts, 7 run-time length checks; repo-wide 30
  offset constants in 22 files and about 79 raw send sites), such as
  `userspace/librsproto/src/session.rs:212`, `userspace/libstream/src/channel.rs:183`,
  `userspace/libstream/src/setup.rs:379`, `userspace/librsproto/src/session.rs:219`. Use the
  existing `libkern::abi::IpcMsg` (offset asserts exist) with `set_payload(&[u8]) -> Option<()>` and
  `payload() -> &[u8]` clamped to `IPC_PAYLOAD_SIZE`, so `Dir`, `Clipboard`, `Desktop` and
  `round_trip` take `&mut IpcMsg` or `Box<IpcMsg>` instead of a slice and its check; with the libos
  `Channel`, `Dir<'a> { endpoint: Handle<Channel, SendRecv> }`, `IpcPort { endpoint: Handle<..> }`,
  `pipe(depth) -> Result<(Handle, Handle)>` and a non-`Copy` `Streams`, plus a typed `KError`
  resolving libkern's `TODO(recv-kerror)`. Home: libkern `abi::IpcMsg` and `channel.rs` for the
  message; libos for `Channel`. Closes: a send from a buffer shorter than a message (the kernel
  copies a whole 4,096 bytes; the send-side twin of desktop-session-mgr's receive defect),
  `payload_len` disagreeing with the bytes, header-offset drift across 25 files, and (with
  exported libos mock seam) host tests for `round_trip`, `send_setup` and `recv_setup` ordering.
  Depends on: libos gaining a `Channel`; for libstream, session-mgr's no-libos rule (via
  libsession) must be revisited or the type lives in libstream first. Lenses: abi, soundness,
  opportunities. Value medium, effort large. **Plan**, in P7.
- [ ] **proto-o6** encapsulate: wire layouts shared across the kernel boundary are checked by
  nothing. kernel/src/rsproto.rs copies librsproto's constants by hand (30 shared names, all equal
  today) and `abi-sync-check` covers libkern only. The `FILE_BLOCKS` `BlockRun` layout is written
  by hand in libfsserver and parsed by hand in the kernel with no librsproto codec.
  `devices::RECORD_LEN = 144` is a literal whose doc says "both sides assert", but librsproto
  asserts nothing. 32 sites (a script comparing `const NAME: ty = value;` lines found 30 shared
  names, 0 differing; plus 1 `BlockRun` layout and 1 `RECORD_LEN`), such as
  `kernel/src/rsproto.rs:75`, `userspace/libfsserver/src/serve.rs:240`,
  `userspace/librsproto/src/devices.rs:26`, `tools/xtask/src/checks/abi.rs:41`. An `AbiFamily` in
  tools/xtask/src/checks/abi.rs pairing kernel/src/rsproto.rs with librsproto's lib.rs,
  namespace.rs, file.rs and error.rs; a `librsproto::namespace::put_block_run(out, file_block,
  device_lba, length, flags)` for libfsserver; and `#[cfg(feature = "io")] const _: () =
  assert!(RECORD_LEN == size_of::<libkern::device::DeviceRecord>());`, as `RS_MAX_PAYLOAD` already
  has. Home: tools/xtask/src/checks/abi.rs and librsproto. Closes: the kernel and a userspace
  server reading the same reply at different offsets after one side changes. Depends on: nothing.
  Lenses: abi. Value low, effort small. **Plan**, in P8.

What held up: all 19 unsafe blocks have SAFETY arguments that hold. Every send buffer is a whole
4,096-byte message (a run-time check at session.rs:219, `Box<[u8; IPC_MSG_SIZE]>` in channel.rs
and setup.rs), every wait takes count 1 with a 24-byte result buffer, every handle count is at
most 4, and the hand-decoded `IoResult` and IPC header offsets match libkern's asserted layout.
Every receive goes through `libkern::channel_recv`, whose `Received` guard closes a peer's
attached handles; `recv_setup` checks `split_handles` against the kernel-attested count before
taking any handle, and unknown bitmap bits and a terminal with no handle are refused. Each PO is
closed exactly once on every path, a failed wait included. `librsproto::decode` checks magic and
`body_len` with `checked_add`; the peer-claimed rsproto `handle_count` is never used to index
outside tests (view-broker main.rs:861 compares it with what it received); every encoder checks
its narrowing casts. Fuzz probes (about 80 librsproto decoders on 3,000,000 inputs and the
libstream decoders on 2,000,000, debug and release) found no panic, with positive controls (a
guard weakened by one byte panicked at once) and the probes removed afterwards; `with_capacity`
hints are capped at 64 and `ByteSource::take` checks bounds before any allocation. `libgpt`
bounds the header size (92..=512), entry size (128), count (1..=1024), array bounds
(`checked_mul`/`checked_add`) and both CRCs before parsing entries, and refuses more than 8
partitions. `libgpt::mbr` only builds tables, though the scope said it parses MBR. The 30 shared
constants in kernel/src/rsproto.rs equal librsproto's. `session::round_trip` does not check the
reply's request id, which is sound only while every channel it serves is strictly request/reply
(true today). Callers of `Dir`, `Clipboard` and `Desktop` in libfs, nxedit, nxterm, nxfiles,
desktop-shell, profile-server, nxsh and coreutils close the session on every path. The inventory
is 19 blocks (session.rs 6, setup.rs 5, channel.rs 4, clipboard.rs 2, desktop.rs 2); the scope's
7/6/5/3/3 counts matched `use` lines too.

### desktop — main at 135055a8, 2026-10-27

Lenses: abi, ownership, soundness, opportunities; returned nothing: none.
Defects: found 38, refuted by a boot 0, reported 17 (duplicates merged). Opportunities: 12 (29 proposed).

The inventory, `cargo xtask unsafe-inventory` over its paths:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `userspace/compositor` | 3702 | 79 | 0 | 0 | 13 | 39 | 6 | 0 | 0 | 0 | 0 |
| `userspace/desktop-shell` | 4206 | 63 | 0 | 0 | 15 | 42 | 4 | 0 | 0 | 0 | 0 |
| `userspace/desktop-session-mgr` | 500 | 18 | 0 | 0 | 4 | 11 | 0 | 0 | 0 | 0 | 1 |
| `userspace/libui` | 3825 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `userspace/libdraw` | 2087 | 18 | 7 | 0 | 0 | 1 | 4 | 0 | 0 | 0 | 0 |
| `userspace/libsurface` | 788 | 20 | 5 | 0 | 0 | 12 | 1 | 0 | 0 | 0 | 0 |
| `userspace/libinput` | 377 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `userspace/input-server` | 987 | 36 | 0 | 0 | 9 | 23 | 4 | 0 | 0 | 0 | 1 |
| `userspace/clipboard-server` | 404 | 24 | 0 | 0 | 10 | 10 | 2 | 4 | 0 | 0 | 0 |
| `userspace/nxfiles` | 2663 | 7 | 0 | 0 | 0 | 5 | 0 | 0 | 0 | 0 | 0 |
| `userspace/nxedit` | 2414 | 5 | 0 | 0 | 0 | 2 | 0 | 0 | 0 | 0 | 0 |
| `userspace/nxterm` | 1540 | 32 | 4 | 0 | 6 | 22 | 2 | 0 | 0 | 0 | 0 |
| `userspace/libterm` | 1313 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `total` | 24806 | 302 | 16 | 0 | 57 | 167 | 23 | 4 | 0 | 0 | 2 |

**Defects**

- [x] **desk-d1** `userspace/desktop-session-mgr/src/main.rs:421` — `open_greeter` maps two new
  shared buffers on every call and overwrites `addrs[i]` without unmapping the old pair. The crate
  has no `SYS_MEMORY_UNMAP` anywhere. Someone at the greeter types a wrong password and presses
  Enter repeatedly (logins and logouts do the same). Each submit destroys the window (:362), then
  `open_greeter` (:389) maps 2 x 191,760 bytes (47 pages each). The handles moved at attach, so
  the stale mappings are the last reference. About 376 KiB leaks per attempt, unbounded, in a
  supervisor that never exits.
  Demonstrated by path: `grep -rn 'UNMAP\|unmap' userspace/desktop-session-mgr/src/` is empty,
  while the same grep finds `overview.rs:40` in desktop-shell (positive control). `open_greeter`
  runs after every submit and at startup (:301). Not booted. Counting free frames across N refused
  logins would put a number on it. abi, ownership, soundness, opportunities. Medium. **Fix.**
  Fixed in `e07bd09d`, with desk-o6: the pool goes when the window does. A probe under
  check-logout read addresses climbing through a boot, and reads the same two after every logout.

- [x] **desk-d2** `userspace/nxterm/src/backend.rs:206` — `Backend::output` is a safe fn returning
  `Option<&'static [u8]>` into `static mut MSG`. `output()`, `typed()`/`send()` and `recv_is_ok`
  on any tab's `Backend` overwrite `MSG`, so safe code can hold the slice across a write. That is
  an aliasing violation, and the bytes change underneath it. The only caller today (main.rs:580)
  feeds each chunk before the next call. One ordinary change makes it bite: answering a terminal
  query (DSR `ESC[6n`, which libterm drops today) with `typed(reply)` inside the output loop, or
  collecting several tabs' output before feeding.
  Demonstrated two ways. A compile probe added a safe fn that holds `a.output()` across
  `b.output()` and `a.typed(..)`. `cargo check -p nxterm --bin nxterm --target
  x86_64-unknown-linux-gnu` accepted it without error or warning. A faithful Miri reproduction
  (same static, same unsafe blocks, receive simulated) reports "Undefined Behavior: trying to
  retag ... SharedReadOnly ... later invalidated by a write access" at send's copy. Run natively,
  the held slice reads `XYllo` after a send and `WORLD` after a second receive. The reproduction
  is out of tree because `backend.rs` is in the bin crate. The probe was restored. abi, ownership,
  soundness, opportunities. Medium. **Fix.**
  Fixed in `a818ddbe`: each `Backend` owns its buffer and `output` borrows from `&mut self`.
  Holding the slice across `typed` is `E0502`, where the old signature compiled.

- [x] **desk-d3** `userspace/nxterm/src/main.rs:334` — `spawn_shell` returns the shell's process
  handle (SIGNAL|TERMINATE) and `reconcile_backends` only tests it `< 0` and drops it. Every tab
  leaks a Process handle in nxterm's table and pins the exited shell's Process object. Opening
  and closing tabs or windows (`open_window` also calls `reconcile_backends`) grows the table
  until allocation is refused.
  Demonstrated by path: `backend.rs:268-271` takes `child` from `SYS_PROCESS_SPAWN` and :300
  returns it. `grep -rn 'spawn_shell(' userspace` finds only the definition and this caller. The
  kernel returns `parent_handle.bits()` (table.rs:491-509), and desktop-shell's `launch` closes
  the same value (apps.rs:390/416). abi, ownership. Medium. **Fix.**
  Fixed in `a818ddbe`: `spawn_shell` closes it and says only whether the shell started. A
  probe closing what the old one returned found it open every time.

- [x] **desk-d4** `userspace/libos/src/objects.rs:44` — `Handle<Memory, M>::unmap(&self, addr, len)`
  is a safe fn, but the kernel unmaps the whole VMA covering `addr` and ignores both the size and
  the handle (`sys_memory_unmap` -> `unmap_covering_deferred`, table.rs:949). Safe code can
  therefore unmap memory a live reference points into (heap, stack, another mapping). Latent. The
  desktop callers (`libsurface/screen.rs:37`, `libdraw/text.rs:250`) pass the address `map()` just
  returned. One ordinary change makes it bite: unmapping twice (an error path plus a later cleanup)
  or after another `map()` reused the range.
  Demonstrated by reading the signature and the kernel handler. The unsafe is not on the signature.
  ownership. Medium. **Fix**, as rt-d2.
  Fixed with rt-d2, in `670a3414`.

- [x] **desk-d5** `userspace/compositor/src/main.rs:1576` — `map_attached_buffer` maps whatever
  object the client transferred, though the spec says `AttachBuffer` carries a MemoryObject, and
  the SAFETY comment claims "a MemoryObject". Nothing checks the type (no `SYS_HANDLE_STAT`;
  `HandleInfo.object_type` exists). An app resolves a file with `MAP_READ|TRANSFER`, sends it as
  an AttachBuffer whose geometry fits within the file, and commits. `sys_memory_map` maps a
  FileObject lazily (table.rs:887-931), so each composite faults pages in through the fs server on
  the compositor's single thread. If the stick is pulled, or a fill fails (Oom), the fault is fatal
  (idt.rs:318-330) and the compositor, and the session with it, dies. It is also reachable by any
  app (untrusted).
  Unverified: read at 135055a8, not booted. A boot would settle it: a test client attaches a stick
  file as buffer 0, commits, ejects the stick (check-media's flow) and forces a repaint. The same
  boot would test the fix, refusing `object_type != MemoryObject` before the map. abi, soundness.
  Medium (would be high if the compositor counts as a critical service). **Fix.**
  Fixed in `d30b02dc`: an attach that is not a `MemoryObject` is refused before `dispatch`,
  which needs `INSPECT` (`rsproto-surface-ops.md`). `a_session_takes_memory_and_whole_bodies_test`
  is a client on `/dev/draw/new`, and fails without it. No stick was pulled.

- [x] **desk-d6** `userspace/libdraw/src/acquire.rs:52` — `read_info` maps the FramebufferInfo
  object and never unmaps it, and the kernel mints a fresh object per resolve
  (kernel_server.rs:361). The compositor calls it twice at start (main.rs:2214 and inside
  `acquire`, :89), so two info pages and their objects stay pinned for the process's life. A
  future caller that re-reads the geometry leaks a page per call. The same class is in
  `libdraw/src/text.rs:247`: `load` returns through the `?` on `try_reserve_exact` between its map
  (:244) and unmap (:250), leaking the font-file mapping and pinning the file.
  Demonstrated by path: `grep -n unmap userspace/libdraw/src/acquire.rs` is empty, while
  `libsurface/src/screen.rs:37-39` unmaps the same kind of object and says why. abi, ownership,
  soundness, opportunities. Low. **Fix**, as rt-d5.
  Fixed with rt-d5, in `2cde6627`.

- [x] **desk-d7** `userspace/libdraw/src/acquire.rs:99` — `acquire` maps `info.byte_len` bytes but
  wraps them in `RawFramebuffer::new`, whose contract needs `geometry.byte_len()` = pitch x
  height. `geometry_from` (:62-76) never reads `byte_len`, so the SAFETY argument ("maps byte_len
  writable bytes") names the wrong quantity. It holds only because the kernel computes
  `byte_len = pitch.saturating_mul(height)` (kernel/src/framebuffer.rs:55). If byte_len were
  smaller (firmware-reported size, another boot protocol on the planned aarch64 port), the
  mapping would be shorter than the slice, and `copy_damage` would write past it into whatever
  `find_free_range` put next to it. One check, `info.byte_len >= geometry.byte_len()`, makes it
  structural.
  Demonstrated by path (acquire.rs:61-76, :99; framebuffer.rs:337-343). abi, soundness. Low.
  **Fix.**
  Fixed in `d240d674`: `geometry_from` refuses it. Fails without it:
  `an_aperture_short_of_its_last_row_is_refused`.

- [x] **desk-d8** `userspace/desktop-shell/src/wallpaper.rs:88` — `open_wallpaper` unmaps none of
  its shared buffers on any path. On a failed attach, a refused commit (:98) or a vanished window
  (the `!ok` exit, :108-112), the screen-sized mappings (about 4 MB each at 1280x800) stay mapped
  for the shell's life. On success both stay too, though nothing writes them after the copy.
  `Wallpaper` stores no addresses. The handle half of a failed attach is the recorded
  capture-refused-leak. The mapping half is not.
  Demonstrated by path: nothing in `wallpaper.rs` calls `SYS_MEMORY_UNMAP` or `release_buffer`;
  `overview.rs:209-213` releases on the same kind of failure. abi, soundness, opportunities. Low.
  **Fix.**
  Fixed in `e07bd09d`, with desk-o6: the pool goes on every path, success included. By
  reading: nothing forces these paths in a boot.

- [x] **desk-d9** `userspace/desktop-shell/src/overview.rs:196` — when `attach_with_format` fails in
  `open_overview` (and `open_wallpaper`, wallpaper.rs:88), the MemoryObject handle is left open.
  The cleanup unmaps and destroys the window but never closes `handle`. libsurface transfers the
  handle only on a successful send (buffers.rs:113-114), so a refused NOBLOCK send (session ring
  full, compositor gone) leaves it this process's. Each failed overview open leaks a screen-sized
  object (about 4 MB). It is distinct from the recorded capture-refused-leak at overview.rs:411.
  Unverified: read, not booted. A boot with the attach send forced to fail, counting handles,
  would settle it. `libsurface/src/buffers.rs:install` shows the correct cleanup for the same
  failure. ownership. Low. **Fix.**
  Fixed in `e07bd09d`, with desk-o6, whose `install` closes the handle. By reading: nothing
  forces a failed attach in a boot.

- [x] **desk-d10** `userspace/nxterm/src/backend.rs:250` — `spawn_shell` documents `terminal` as
  moved, but its early `return -1` paths (no `/bin/nxsh` at :248-250, setup-channel failure at
  :252-255) never close it, unlike the spawn-failure (:276) and send-failure (:296) paths.
  `attach`'s `make_channel()?` at :145 returns without closing the `tty` resolved at :140.
  `reconcile_backends` keeps the Backend anyway (main.rs:329-337), so the tty server never sees
  PeerClosed and the terminal outlives its tab. Reachable with a namespace lacking `/bin/nxsh` or
  a failed channel create, and each further tab leaks another handle.
  Demonstrated by path (the two close-sites and the early returns, side by side). abi, ownership,
  opportunities. Low. **Fix.**
  Fixed in `a818ddbe`: each early return closes it. With `/bin/nxsh` made to miss and each
  `Backend` kept, a probe ran out of terminals at round 13, and has one in all 20 now.

- [x] **desk-d11** `userspace/nxterm/src/main.rs:695` — the wait set spans the session channel plus
  every backend of every window, but `MAX_TABS` (MAX_WAIT_HANDLES - 1) caps tabs per window only.
  Two windows of 31 tabs give 63 handles, and 32 windows of one tab give 33. The kernel refuses
  a wait over 32 (table.rs:1767), `wait_any` returns false and :696 calls `fail(...)`, which exits
  nxterm and every shell in every window (a QEMU failure exit under the test-harness kernel).
  Demonstrated: a temporary host test building two `App`s with `MAX_TABS` NewTab each failed
  (`1 + a.tabs().len() + b.tabs().len() <= MAX_WAIT_HANDLES`; "waits on 63 handles", and 33 with
  15 per window). The const assert at lib.rs:2317 checks one window. Probe restored. ownership,
  opportunities. Low. **Fix.**
  Fixed in `55b2cd35`: `MAX_TABS` counts the process's windows, and a window past it is
  refused. Not reachable today, as a tab gets a handle only with a terminal and the tty server has
  14. Fails without it, and with the bound moved by one:
  `the_cap_counts_the_process_s_other_windows`. What `main` passes in is by reading.

- [x] **desk-d12** `userspace/compositor/src/main.rs:1704` — `handle_session_request` copies each
  request body into `MAX_BODY` = 4 + `MAX_TITLE` = 260 bytes, sized for SetTitle, the comment
  calling the min "a bounds check, not a policy". StartDrag allows 12 + 512 + 64 = 588 bytes
  (rsproto-surface-ops.md:1442-1443), and libsurface sizes it that way (lib.rs:574). nxfiles
  dragging a file with a path over about 248 bytes gets InvalidArgument (path_len runs past the
  cut body) after logging "dragging". A 200-byte path with a 64-byte name is accepted with the
  name cut to 48 bytes, and the drop carries the shortened name. The const assertion at main.rs:58
  ties MAX_BODY to MAX_TITLE only.
  Demonstrated by two host probes (removed). A 328-byte StartDrag (306-byte path) parsed whole,
  but `StartDrag::read` on the first 260 bytes was `None`. A 278-byte StartDrag failed
  `assert_eq!(whole, cut)`, with the name 60 vs 42 'n'. A second probe in librsproto's tests
  found the same: 300-byte path `None`, 200+64 gives a 48-byte name. abi, soundness. Low. **Fix.**
  Fixed in `d30b02dc`: `MAX_BODY` is `RS_MAX_BODY`. `a_session_takes_memory_and_whole_bodies_test`
  fails with the old bound, a 300-byte path answered `InvalidArgument` rather than `NotFound`.

- [x] **desk-d13** `userspace/compositor/src/main.rs:1354` — `Manage::Close` (desktop-shell's
  insist_on_close) destroys a window and its popups through the manager path, which never drops
  their `MappedBuffer`s. `srv.buffers.retain` runs only at :1558 (close_session), :1573 and :1991
  (after a session request is Applied). A hung app force-closed by a double middle-click keeps its
  two buffers mapped until some client next commits (in practice the clock, up to a minute) or
  indefinitely on a quiet desktop. A delay, not a permanent leak, and window ids are never
  reused, so nothing aliases.
  Demonstrated by path: `grep -n 'buffers.retain' userspace/compositor/src/main.rs` finds only
  those three lines, none inside `handle_manager_request` (:1291-1481). ownership, soundness. Low.
  **Fix.**
  Fixed in `d30b02dc`: `drain_stack_events`, which every removal passes through, drops them,
  and the two hand-placed `retain`s go. A one-off probe under check-logout read 0 mappings of gone
  windows after a close, and 10 with that line removed.

- [x] **desk-d14** `userspace/compositor/src/main.rs:1916` — an AttachBuffer carrying no handle is
  still applied: `dispatch` records the new geometry for (window, buffer), but the mapping is
  replaced only `if op == OP_ATTACH_BUFFER && handle != 0`. A client attaches (W,B) at 8x8 with
  memory, re-attaches (W,B) at 4096x4096 without a handle and commits. The stack says 64 MiB
  while `pixels(W,B)` is still the old 256-byte mapping. Today `compose_into` (lib.rs:943) and
  `box_downscale` skip a short buffer. One more reader that trusts the stack's geometry
  (a full-size capture, a screenshot) would read past the mapping, and any client can reach it.
  Demonstrated by path: server.rs:182-189, lib.rs:724-726, main.rs:1916. The existing test
  `a_short_buffer_is_skipped_rather_than_read_past_its_end` covers the readers, not this
  producer. ownership. Low. **Fix.**
  Fixed in `d30b02dc`: an attach with no memory is refused, and memory is mapped before
  `dispatch`, so the stack never holds geometry with none behind it.
  `a_session_takes_memory_and_whole_bodies_test` fails without it.

- [x] **desk-d15** `userspace/compositor/src/main.rs:2121` — when the supervisor's control channel
  reads `Closed`, it is dropped from the wait set (`control = 0`) and the handle is never closed.
  The same in `input-server/src/main.rs:825` and `clipboard-server/src/main.rs:378`.
  `libkern::control::recv` does not close it. Bounded at one handle per process, but no path ever
  closes it.
  Demonstrated by path (`Control::Closed => control = 0` with no `SYS_HANDLE_CLOSE`). ownership.
  Low. **Fix.**
  Fixed in `660b2e6c`: `control::recv` closes it, for all ten callers, and auth-service
  closes its endpoint the same way. Fails without it: `only_a_channel_closed_for_good_is_closed`.

- [x] **desk-d16** `userspace/compositor/src/server.rs:113` — `static mut TITLE_TRUNCATED_LOGGED`
  lives in the compositor library and is read and written without synchronisation by the safe
  `pub fn dispatch` (via `note_title_truncated`). The "single-threaded server" argument holds for
  the binary but not for libtest, which gives every test its own thread. Today only
  `title_tests::an_over_long_title_is_stored_truncated` reaches it, and a second over-long-title
  test is an ordinary addition.
  Demonstrated: a Miri probe calling `set_title` with a `MAX_TITLE+50` title from two threads
  reported "Data race detected between (1) non-atomic write on thread `server::title_t` and (2)
  non-atomic read on thread `unnamed-2`" at server.rs:120/123. Probe removed, `git status` clean.
  soundness, opportunities. Low. **Fix.**
  Fixed in `d188a633`, with desk-o7: an `AtomicBool`.
  `at_most_one_thread_reports_a_title_truncated` passes under Miri, which reports a data race on
  the old `static mut`.

- [ ] **desk-d17** `userspace/compositor/src/main.rs:159` — `BufferSource::pixels` builds a `&[u8]`
  over a mapping the client holds read-write (libsurface maps `MAP_READ|MAP_WRITE`,
  buffers.rs:170), and `do_capture` (:1244) makes a `&mut [u8]` over the manager's object. A
  reference promises that nothing writes the bytes while it lives, and the other process can break
  that at any time. No pixel value reaches an index or bound today (blend and average only), so
  the visible result is a torn frame, but the SAFETY argument covers the mapping's lifetime and
  not `from_raw_parts`' immutability requirement.
  Unverified: cross-process, so no host test or Miri reaches it. A copy-only reader (`ptr::copy`
  or volatile loads) behind a Mapping type (see desk-o5) would settle it. soundness,
  opportunities. Low. **Plan**, in P7.

**Opportunities**

- [ ] **desk-o1** encapsulate: handles held as raw `u64` and closed by hand on every path,
  including after a send that may or may not have moved them — 73 sites of `SYS_HANDLE_CLOSE` (86
  with input-server's 13 `close()` calls), counted by `cd userspace && grep -rnE
  'syscall[0-9]\((libkern::)?(syscall::)?SYS_HANDLE_CLOSE' --include=*.rs <the 13 crates> | wc
  -l`; by crate compositor ~21, desktop-shell ~23, nxterm ~14, input-server ~5, clipboard-server
  3, libsurface 3, nxfiles 2, dsm 2. Such as `userspace/nxterm/src/backend.rs:271`,
  `userspace/compositor/src/main.rs:1185`, `userspace/desktop-shell/src/apps.rs:379`,
  `userspace/desktop-shell/src/overview.rs:187`. Own every handle as libos `Handle<T, M>`
  (`into_raw` only at a committed transfer). Add a libos channel object with `send(&self, msg,
  handles: [Handle; N]) -> Result<(), Refused<N>>`, which consumes on success and hands back on
  refusal; `Received::take(i) -> Option<owning handle>`; Transport::request and `Window::attach`
  take the handle by value, which settles their contract by type. In libos (`handle.rs`,
  `objects.rs`; `libkern::channel::Received`; libsurface's Transport signature). Checked against
  nxterm `spawn_shell`, compositor `open_session` (both ends drop on a refused reply),
  desktop-shell `launch` (loses 9 closes) and `map_attached_buffer`. Closes: handles dropped on
  early return or refused send (desk-d3, d9, d10, d15, the handle half of capture-refused-leak,
  the class b4cc3a39 fixed by hand). Depends on: a libos channel create/send/recv API (libos has
  none yet), and the typed message buffer (desk-o2) for send's signature. Note:
  desktop-session-mgr's CLAUDE.md inherits "no libos", yet `cargo tree` shows libos already
  linked through libsurface and libdraw, so that rule needs a decision. abi, ownership,
  soundness, opportunities. Value high, effort large. **Plan**, as sup-o2, in P7.

- [ ] **desk-o2** encapsulate: IPC messages as raw `static mut [u8; 4096]` buffers, framed by hand
  (payload_len read at [4..8], slices via `from_raw_parts(base+24, len.min(4096-24))`, headers
  stamped at [4..8] and [8], raw `SYS_CHANNEL_SEND`) — 30 to 45 sites, counted by `grep -rnE
  'from_le_bytes\(\[[A-Za-z_.]+\[4\]'` (16 receive parses), `grep -rnE
  '\[4\.\.8\]\.copy_from_slice'` (17 send stamps), `from_raw_parts` over receive statics (13),
  `channel_recv_raw(` (15), over 12 message-buffer statics and 5 send-handle arrays;
  `PAYLOAD_OFF` and `MSG_LEN` are redefined in five crates, desktop-shell uses the literals 24 and
  4096, and nobody uses libkern's `IpcMsg`. Such as `userspace/compositor/src/main.rs:1695`,
  `userspace/clipboard-server/src/main.rs:239`, `userspace/nxterm/src/backend.rs:219`,
  `userspace/input-server/src/main.rs:392`. An owned `IpcMsg`/`MsgBuf` (libkern's
  `#[repr(C, align(4096))]`, or boxed in each server's state) with `recv(&mut self, ch) ->
  Result<(Decoded<'_>, Received), i64>` (the view borrows the buffer) and `send(&mut self, ch,
  op, id, flags, body, handles)`, stamping both header fields. The payload is clamped to
  `IPC_PAYLOAD_SIZE` (4008), not 4072, which also covers the handle slots. In libkern (`IpcMsg`
  and accessors, beside `channel_recv`) with the encode helper in librsproto (beside `encode` and
  `round_trip`) or libos; nxfiles already receives into a `Box<[u8; IPC_MSG_SIZE]>`. Checked
  against compositor `handle_session_request`, clipboard `serve_session` (separate recv and reply
  buffers) and nxterm `Backend::output`, which becomes `fn output(&mut self) -> Option<&[u8]>`.
  Closes: a view of a static buffer outliving the next receive/send (desk-d2), buffers sized by
  expectation (the desktop-session-mgr 256-byte class), the 4072/4008 clamp disagreement, and
  desk-d12's MAX_BODY. Removes about 10 `static mut` buffers and 30 unsafe blocks. Depends on:
  nothing (the 8 MiB stack is decided; the "32 KiB stack" comments are stale); moving handles in
  a send waits for desk-o1. abi, ownership, soundness, opportunities. Value high, effort medium.
  **Plan**, as rt-o5, in P7.

- [ ] **desk-o3** encapsulate: `sys_wait` called by hand over `static mut WAIT_HANDLES` and
  `WAIT_RESULTS`, with 24-byte `IoResult` records decoded by byte offset and the result buffer
  sized by a comment — 17 to 21 sites, counted by `grep -rn 'SYS_WAIT,' --include=*.rs <the 13
  crates> | wc -l` (21 hits, 17 to 18 calls once import lines are removed), over 12 wait-array
  statics and 7 hand-decoding functions (compositor:2097, clipboard:358, input-server:266 and
  :805, desktop-shell apps.rs:272, nxterm backend.rs:59, nxfiles:334). Such as
  `userspace/compositor/src/main.rs:2085`, `userspace/input-server/src/main.rs:255`,
  `userspace/nxterm/src/backend.rs:53`, `userspace/nxfiles/src/main.rs:328`. Make libos's
  private `sys::wait(handles, &mut [IoResult], deadline)` public with `results.len() >=
  handles.len()` checked, and put `WaitSet<const N: usize> { handles, results: [IoResult; N],
  len }` over it: `push(h) -> Result<(), Full>`, `wait(deadline) -> Result<&[IoResult], KError>`,
  N const-checked against `MAX_WAIT_HANDLES`. In libos (using libkern's `IoResult`,
  abi.rs:287). Checked against compositor `serve_loop`, input-server `po_completion` (a
  `WaitSet<1>` stops it overwriting record 0 of the loop's batch) and nxfiles `wait_for`. Closes:
  a result buffer smaller than the wait set, as in a81cf9c0 (desktop-shell, 24 bytes past a
  static); a wait set over the kernel limit (desk-d11, nxterm's `Full` becomes "no more tabs"
  instead of exit). Depends on: nothing. abi, ownership, soundness, opportunities. Value high,
  effort medium. **Plan**, as rt-o2, in P7.

- [ ] **desk-o4** narrow: `unsafe fn`s whose only contract is "root_ns must be a live namespace
  handle". A stale handle fails the generation check and returns InvalidHandle, so the contract
  guards no UB, yet it makes every caller write `unsafe` — 14 of the subsystem's 16 `unsafe fn`s
  with 26 to 31 unsafe call blocks, counted by `grep -rnE 'unsafe fn [a-z_]+(<[^>]*>)?\('
  --include=*.rs <the 13 crates>` (16, minus `RawFramebuffer::new` and `ChannelTransport::new`)
  and `grep -rn 'unsafe { .*(load_ui|load_mono|connect|manage|screen::read|read_info|acquire(|
  attach(root|spawn_shell|open_window|reconcile_backends)'`. The 14: libdraw `read_info`,
  `acquire`, `load`, `load_ui`, `load_mono`, `load_themed`; libsurface `screen::read`, `connect`,
  `manage`, `resolve`; nxterm `attach`, `spawn_shell`, `open_window`, `reconcile_backends`. Such
  as `userspace/libdraw/src/text.rs:265`, `userspace/libsurface/src/ipc.rs:81`,
  `userspace/libsurface/src/screen.rs:22`, `userspace/nxterm/src/backend.rs:139`. Take `ns:
  &Handle<Namespace, NsReadOnly>` instead of `root_ns: u64`, borrowed once in each `_start` with
  `unsafe { Handle::borrow(RawHandle(root_ns), Rights::LOOKUP) }`; optionally a checked
  `lookup_as::<Memory, MapRead>` that confirms the type with `sys_handle_stat`, which is also the
  check desk-d5 needs. In the callee crates (libdraw, libsurface, nxterm), libos supplying the
  lookup. Checked against `load_ui`, `ChannelTransport::connect` and nxterm `attach`, which only
  perform lookups through it. Closes: no defect class, but the unsafe that remains marks real
  memory hazards. Depends on: a one-time borrow of the bootstrap namespace in each `_start` (or
  libos's thin-entry seam). abi, soundness, opportunities. Value high, effort medium.
  **Plan**, in P7.

- [ ] **desk-o5** encapsulate: shared mappings as raw `(*mut u8, len)` pairs, unmapped by hand or
  not at all — 12 map sites against 10 to 13 unmaps and 9 `from_raw_parts` slices over mappings,
  counted by `grep -rnE 'syscall[0-9]\((libkern::)?SYS_MEMORY_MAP|obj\.map\(' --include=*.rs
  <the 13 crates>`; `shared_buffer()` is copied in three or four places, and the compositor
  (`MappedBuffer`), libsurface (`Mapping`/`BufferPool`) and input-server (`Device.buf_addr`) each
  hand-roll an owner. Such as `userspace/libdraw/src/acquire.rs:52`,
  `userspace/desktop-session-mgr/src/main.rs:61`, `userspace/desktop-shell/src/main.rs:162`,
  `userspace/compositor/src/main.rs:1239`. `Handle<Memory, M>::map(&self, len) ->
  Result<Mapping<M>>`: a Mapping owns the VMA and unmaps once on Drop, with `read_into(off,
  &mut [u8])`, and `write_from` for `CanMapWrite`. Memory a peer can write is reached only by
  copy (`ptr::copy`), and slice access exists only on a private mapping, via an `unsafe fn
  assume_unshared`. Libos's safe `unmap(addr)` goes away; `RawFramebuffer::from_mapping(Geometry,
  Mapping)` checks `len >= geometry.byte_len()` and removes its `unsafe fn new`; one
  `SharedBuffer::create(len)` replaces the shared_buffer copies. In libos (`objects.rs`, with the
  `sys.rs` mock for host tests). Checked against `text::load`, `screen::read`, compositor
  `reply_with_object` and `MappedBuffer`, `read_info` and the greeter's `addrs`. Closes: mappings
  leaked on a path out (desk-d1, d6, d8, the mapping half of capture-refused-leak, the history at
  15534384 and 77f5aeff), the safe-unmap hole (desk-d4), the length behind desk-d7, and the
  shared-pixel reference (desk-d17). Host-testable once libos's mock sits under it. Depends on:
  nothing (the handle that moves at attach shares desk-o1's send contract). abi, ownership,
  soundness, opportunities. Value high, effort medium. **Plan**, as rt-o3, in P7.

- [x] **desk-o6** remove: desktop-session-mgr and desktop-shell hand-roll shared window buffers
  (own `shared_buffer`, raw `*mut u8` arrays, `copy_nonoverlapping`, manual release) where
  `libsurface::buffers::BufferPool` already does it safely and libui uses it (`window.rs:83`) — 3
  buffer sets, counted by `grep -n 'copy_nonoverlapping\|fn shared_buffer\|shared_buffer(len)'
  userspace/desktop-session-mgr/src/main.rs userspace/desktop-shell/src/*.rs` (3 copies, 2
  definitions, 4 callers; removes 7 unsafe blocks), such as
  `userspace/desktop-session-mgr/src/main.rs:421`, `userspace/desktop-shell/src/overview.rs:258`,
  `userspace/desktop-shell/src/wallpaper.rs:82`. `BufferPool::in_format(&mut window, Size,
  BUFFERS, format)` plus `pool.write(id, &frame)`, unmapped on drop; GREETER_PITCH and
  `Screen::pitch()` are width*4, which is its `pitch_of`; the overview needs ARGB8888; the
  wallpaper writes after attach, still before its commit. In `userspace/libsurface`, with changes
  in the callers. Closes: a mapping outliving its window (desk-d1, d8; the overview's hand-kept
  release, which leaked 9 MB once in 77f5aeff). `install` also closes and unmaps on a failed
  attach, which closes desk-d9 for these sites. Depends on: nothing. opportunities. Value high,
  effort small. **Fix.**
  Finished in `e07bd09d`. Also the mapping half of `TODO(capture-refused-leak)`.

- [x] **desk-o7** remove: `static mut` used as syscall out-params and diagnostic counters where a
  local or an atomic serves — 21 sites (of 57 `static mut`), counted by `grep -rnE 'static mut
  (CTRL_OUT[01]|CH_OUT[01]|CH[01]|CLOCK_BUF|[A-Z_]*LOGGED|REDRAWS)\b'` (19) plus `SPAWN_APP`,
  `NOTIF` and `OVERRUNS_LOGGED`: 14 out-params (CLOCK_BUF x3, CTRL_OUT0/1 x2, CH_OUT0/1 x2,
  CH0/CH1, NOTIF), 6 counters (ROUTES_LOGGED, INPUT_DIAGS_LOGGED, TITLE_TRUNCATED_LOGGED,
  OVERRUNS_LOGGED, REDRAWS, GEOMETRY_LOGGED) and SPAWN_APP. Such as
  `userspace/compositor/src/main.rs:87`, `userspace/compositor/src/server.rs:113`,
  `userspace/input-server/src/main.rs:94`, `userspace/desktop-shell/src/apps.rs:293`. Locals for
  out-params (libsurface `ipc.rs:139-141` and nxfiles already read the clock into a local;
  compositor `main.rs:1974` too), thin safe `libkern::channel_create` and `clock_read`, and
  `AtomicU32`/`AtomicBool` (Relaxed) for the counters. Checked against compositor `now_ns`,
  `make_channel`, `log_route`, input-server `make_channel` and dsm's REDRAWS. In each crate, with
  the clock and channel-pair helpers in libkern/libos. Closes: the data race on a library static
  (desk-d16) and unsafe blocks wider than their operation: `route_one_batch`'s 152-line block
  (main.rs:896-1048) exists only for `INPUT_DIAGS_LOGGED`; about 45 of the 302 unsafe blocks wrap
  these statics. Depends on: nothing. abi, ownership, soundness, opportunities. Value medium,
  effort small. **Fix.**
  Finished in `d188a633`, by the implementer. `libkern` gains `channel_create` and
  `clock_read`, used by the desktop's crates; the others' helpers move with P7.

- [ ] **desk-o8** encapsulate: server state kept in `static mut` tables whose "nothing else holds
  it" claim rests on call order — 10 statics (SESSION_CH and MANAGER_CH in the compositor;
  DESKTOP_SERVE, DESKTOP_SESSIONS, SHELL_NOTIF, NOTIF and SPAWN_APP in desktop-shell; RING,
  SESSION_CH and REPLY_BODY in clipboard-server), 62 uses, counted by `grep -nE
  '\b(SESSION_CH|MANAGER_CH|DESKTOP_SERVE|DESKTOP_SESSIONS|SPAWN_APP|SHELL_NOTIF|NOTIF|RING|REPLY_BODY)\b'`
  excluding declarations. Such as `userspace/compositor/src/main.rs:81`,
  `userspace/desktop-shell/src/desktop_sessions.rs:214`,
  `userspace/clipboard-server/src/main.rs:42`, `userspace/desktop-shell/src/apps.rs:293`. Make
  them fields of an owned `Server` struct passed as `&mut` (the compositor and input-server
  already have one); clipboard's `Ring` can be a `_start` local (8 MiB stacks since 2026-08-10).
  In each server binary. Closes: a table or buffer aliased across a call that changes it,
  prevented today only by single-threading and call order. Depends on: desk-o2 (the receive and
  reply buffers move with this state). soundness. Value medium, effort medium. **Plan**, in P7.

- [ ] **desk-o9** encapsulate: the compositor keeps a buffer's geometry in `WindowStack` and its
  mapping in a second table (`Server.buffers`), kept in step by hand-placed retains — 4 sites
  (`grep -n 'buffers.retain\|buffers.push' userspace/compositor/src/main.rs`: 1558, 1573, 1583,
  1991), such as `userspace/compositor/src/main.rs:1558`, `:1573`, `:1991`, `:1916`. Make
  `WindowStack` generic over a payload, `AttachedBuffer<P> { id, geometry, mem: P }`, with
  `attach(req, mem: P)` requiring the memory and destroy/replace dropping it. `BufferSource`
  goes away because the stack lends pixels from P; host tests use `P = Vec<u8>`, the binary `P =
  Mapping`. In compositor `lib.rs` (WindowStack) with `main.rs` supplying the Mapping. Closes: a
  mapping outliving its window (desk-d13) and geometry with no memory behind it (desk-d14), both
  held today only by two length checks. Depends on: desk-o5. ownership. Value medium, effort
  medium. **Plan**, in P7.

- [ ] **desk-o10** remove: the namespace lookup's wait-and-decode copied into three crates, the
  copy-paste libos was built to replace — 3 sites (`grep -rnE
  'syscall[0-9]\((libkern::)?SYS_NS_LOOKUP'`), `userspace/input-server/src/main.rs:236` (lookup +
  `wait_one` + `po_completion`), `userspace/nxterm/src/backend.rs:84` (`ns_lookup` + `po_wait`),
  `userspace/desktop-shell/src/apps.rs:262` (`ns_lookup` + `wait_one`). Use
  `block_on(ns.lookup::<T, M>(path, rights))`, which libdraw and libsurface already use; the
  caller reads `Error::code()` for the NotFound vs WouldBlock that `verify_app_namespace` needs.
  In libos. Checked: all three want (status, handle). Closes: a status read from a wait record
  the last wait did not write, and PO handles each copy must remember to close. Depends on:
  nothing; a safe lookup (desk-o4) removes the remaining unsafe. opportunities. Value medium,
  effort small. **Plan**, in P7.

- [ ] **desk-o11** narrow: a transferred handle taken out of `Received` before the code knows it
  will keep it, then closed by hand on each branch — 2 sites: compositor `handle_session_request`
  takes handle 0 unconditionally (main.rs:1702) and closes it at 6 sites (`grep -n 'if handle !=
  0' userspace/compositor/src/main.rs`: 1713, 1752, 1803, 1842, 1885, 1922), and input-server
  `take_notices` calls `sent.take_all()` (main.rs:669), leaving `apply()` responsible for every
  handle. Take the handle only for the op that keeps it, as `handle_manager_request` does for
  Capture (:1284-1288): `(m.op == OP_ATTACH_BUFFER).then(|| sent.take(0)).flatten()`, and pass
  `&mut Received` to `apply()`, taking 0 only on Arrived. In `userspace/compositor/src/main.rs`
  and `userspace/input-server/src/main.rs`. Removes 5 unsafe closes. Closes: a new op branch
  before `dispatch` that forgets the close, leaking one client-supplied handle per request (the
  class b4cc3a39 fixed by hand). Depends on: nothing (desk-o1 would make it structural).
  opportunities. Value medium, effort small. **Plan**, in P7.

- [ ] **desk-o12** remove: raw syscalls that have, or need only, a one-line safe wrapper — 24
  sites, counted by `grep -rnE
  'syscall[0-9]\((libkern::)?SYS_(DEBUG_KPRINT|PROCESS_EXIT|TEST_EXIT|CLOCK_READ|NS_ENUMERATE|HANDLE_STAT)'`:
  DEBUG_KPRINT 3 and PROCESS_EXIT 8 (clipboard, desktop-shell and dsm redefine `libkern::kprint`
  and `libkern::exit`), CLOCK_READ 8, TEST_EXIT 3, NS_ENUMERATE 1, HANDLE_STAT 1. Such as
  `userspace/clipboard-server/src/main.rs:56`, `userspace/desktop-session-mgr/src/main.rs:39`,
  `userspace/desktop-shell/src/apps.rs:247`. Use the existing `libkern::kprint`/`libkern::exit`
  and libos `Handle<Namespace>::enumerate` (`bound_in`'s `from_raw_parts` over a local `NsEntry`
  becomes `&entry.path[..len]`), plus new `clock_read(clock) -> Result<u64, KError>` and
  `test_exit(ok)` in libkern (`debug.rs`, a clock fn) and `stat` in libos. Closes: duplicated
  wrappers drifting apart, such as the three exit loops that spin if the syscall returns.
  Depends on: nothing. opportunities. Value low, effort small. **Plan**, as rt-o9, in P7.

What held up:
- **Receive sizing.** The desktop-session-mgr 256-byte class cannot recur. All 18 receives (15
  raw, 3 safe) take a `[u8; IPC_MSG_SIZE]` by type, every static receive buffer is exactly that,
  and the kernel writes all 4096 bytes. All 17 raw sends pass a 4096-byte buffer. The handles
  out-param is `IPC_HANDLE_MAX` inside `Received`.
- **Wait results.** All 18 to 21 `sys_wait` sites have at least 24 bytes per handle. The compositor
  and clipboard are const-asserted, desktop-shell holds 9 for 9 pushes, input-server is at most 15
  of 32, and the rest size theirs locally. The one failure is nxterm's handle count (desk-d11),
  not its buffer.
- **Received handles.** `Received` closes every untaken handle on drop. The compositor, clipboard,
  input-server (`notice()` fixes the handle count per kind, so anything else is Malformed and
  closes all), desktop-shell, nxterm, nxfiles and libsurface close or adopt every handle. Refused
  sends close what stayed behind in compositor `open_session`/`open_manager`/`reply_with_object`,
  input-server `open_consumer`, clipboard, `serve_desktop_endpoint`, nxterm `attach` and
  `BufferPool::install`. A stale forwarded resolve reply makes the kernel close the transferred
  handles, so no slot leaks.
- **Compositor MappedBuffer.** Drop unmaps once, `pixels()` borrows `&self`, and both retain sites
  need `&mut srv`. `compose_into` and `box_downscale` skip a short buffer
  (`a_short_buffer_is_skipped_rather_than_read_past_its_end`). The kernel refuses a map longer
  than the object (table.rs:899-901) and `parse_attach_buffer_request` refuses `pitch < width*4`.
  Re-attaching the on-screen buffer is refused (DuplicateBuffer), and window ids never reuse, so
  a lingering mapping cannot be read as another window's. `do_capture` and `reply_with_object`
  unmap before replying.
- **Client-writable memory.** It is only read as pixels, and no value is used as an index. Every
  length and geometry comes from the compositor's own stack, and font bytes are copied out before
  `ab_glyph` parses them.
- **libsurface BufferPool.** A failed attach unmaps and closes, the old mapping is unmapped after
  the new attach, only free buffers are replaced, and `write()` refuses a length mismatch.
  `ChannelTransport` never drops a Release and reports an oversized body as a loss. desktop-shell's
  260-byte manager drain does likewise.
- **input-server.** `harvest` reads at most 512 of a 4096-byte page. `retire()` may abandon a
  parked read because the driver pins the buffer object. `po_completion`'s overwrite of
  `WAIT_RESULTS[0..24]` is safe as argued (count=1, record 0 already read). The lights page is
  copied at submit by both PS/2 and xHCI.
- **Other conventions.** clipboard-server's raw derefs of `RING` and `REPLY_BODY` are sound
  (separate statics, one `&mut` at a time). desktop-shell `launch` closes image, namespace and
  process handle, and closes both setup ends only on failure, as the kernel moves a handle only
  on success (table.rs:501-505). The old 16-vs-24-byte `HandleInfo` smash is gone. desktop-session-mgr
  copies at most 64 and 128 bytes of name and password and zeroes the password with
  `write_volatile`. No desktop binary creates a thread, so the 57 `static mut` race nothing in
  shipped binaries (desk-d16 is the host-test exception). Handles carry a 31-bit generation, so a
  stale `u64` errors rather than aliasing.
- **Parsers.** Wire parsers on these paths check declared lengths with checked adds
  (`DroppedEvent`, `StartDrag`, `ClipEntry`, `ClipList`, `DesktopIndex`, libsurface
  `window_event`, `FramebufferInfo::from_bytes`). A libterm fuzz of 12,000 escape-heavy streams at
  4 grid sizes, then resized, did not panic (the control, an out buffer of 1, panicked at
  parse.rs:294). A libdraw PNG fuzz of 100,000 mutants of 5 valid PNGs, debug and release, did
  not panic (IHDR size-checked, inflate limited to the size the header implies, allocations
  fallible). Both probes were removed. libdraw `RawFramebuffer` is clean under Miri
  (`a_raw_framebuffer_over_an_allocation_matches_the_in_memory_one`). libui, libinput and libterm
  contain no unsafe.
- **Doc/source disagreement, not an item.** No receiver checks the rsproto `handle_count` against
  the IPC handle count, though rsproto-wire-format.md (Validation) calls a mismatch a protocol
  error. nxterm's AttachBackend encodes 0 while moving one handle (backend.rs:105), so enforcing
  the spec would refuse every terminal attach.
- **Host-test reach.** Only two of the subsystem's unsafe sites run under a host test (libdraw
  `RawFramebuffer`, and compositor `server.rs`'s `static mut`). Every other unsafe site is in a bin
  crate's `main.rs` with no host test, so no test can reach desk-d1, d3, d6 to d10. desk-o1, o3 and
  o5, in libos behind its `sys.rs` mock, would make close-on-every-path and unmap-on-every-path
  testable.

### shell — main at 135055a8, 2026-10-27

Lenses: abi, ownership, soundness, opportunities; returned nothing: none.
Defects: found 32, refuted by a boot 0, reported 13. Opportunities: 12.

The inventory, `cargo xtask unsafe-inventory` over its paths:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `userspace/nxsh` | 7392 | 62 | 0 | 0 | 10 | 43 | 8 | 0 | 0 | 0 | 0 |
| `userspace/coreutils` | 4340 | 40 | 0 | 0 | 6 | 35 | 2 | 0 | 0 | 0 | 0 |
| `userspace/tty-server` | 887 | 38 | 0 | 0 | 9 | 29 | 3 | 0 | 0 | 0 | 1 |
| `userspace/libline` | 306 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `userspace/eshell` | 291 | 23 | 0 | 0 | 2 | 18 | 2 | 0 | 0 | 1 | 1 |
| `userspace/nxinstall` | 1155 | 22 | 1 | 0 | 7 | 15 | 3 | 4 | 0 | 0 | 0 |
| `userspace/profile-server` | 581 | 35 | 0 | 0 | 11 | 17 | 3 | 2 | 0 | 0 | 0 |
| `total` | 14952 | 220 | 1 | 0 | 45 | 157 | 21 | 6 | 0 | 1 | 2 |

**Defects**

- [x] **sh-d1** `userspace/tty-server/src/main.rs:449` — OP_TTY_ATTACH_BACKEND puts whatever
  handle a client sent into the serve loop's wait set without checking its object type or rights.
  Any process holding a terminal (a session's /dev/tty, or the sibling nxsh hands each stage)
  sends AttachBackend carrying a MemoryObject (TRANSFER but no WAIT) or a channel end restricted
  to drop WAIT. tty-server replies OK and adds the handle to WAIT_HANDLES (569-571). Kernel
  sys_wait looks every handle up with Rights::WAIT and fails the whole call (table.rs:1783-1793),
  so main.rs:584 `continue`s and rebuilds the same set for ever. tty-server (essential, restart
  never) spins at 100% CPU and serves no terminal again, the console login and nxterm windows
  included; the bad backend is never removed because removal needs the loop to serve.
  Demonstrated: path read on 135055a8 by all four lenses (main.rs:443-451 checks only
  `handle != 0`; lib.rs:321-329 stores any u64; the kernel lines above); not booted. A boot gate
  that sends AttachBackend with a memory object and then writes to a terminal would show the
  spin. abi, ownership, soundness, opportunities. High. **Fix.**
  Fixed in `6961407d`: a backend is a channel end with `SEND`, `RECV` and `WAIT`, anything else
  refused and closed; fails without it: `tty_refuses_a_backend_it_cannot_wait_on_test` (boot-probe),
  and `a_backend_is_a_channel_it_can_send_receive_and_wait_on` with any one right or the type check
  left out.
- [x] **sh-d2** `userspace/profile-server/src/main.rs:294` — profile-server forwards the client's
  requested rights, MAP_WRITE included, into its own root-namespace lookup of the store file and
  re-exports the handle, so a store binary may be reachable writable by any process with /bin or
  /applications bound. A session program calls sys_ns_lookup(session_ns, "/bin/list",
  MAP_READ|MAP_WRITE); main.rs:599 passes requested_rights to resolve_in_store, which adds only
  TRANSFER (294) before ns_lookup on root_ns through init's read-write mount. The caller gets a
  MAP_WRITE handle to the shared page-cache object of a system binary, visible to every later
  spawn, and to disk with SYS_FILE_SYNC. This contradicts
  docs/architecture/profiles-and-namespace-projection.md (store as LOOKUP|READ|MAP_READ) and
  sidesteps TODO(mount-write-authority). Unverified: traced on paper (kernel table.rs:1538
  ignores binding rights for a userspace-server binding; rsproto.rs:115 file_blocks_rights keeps
  MAP_WRITE on a writable device). To settle it, boot a session, look up /bin/<prog> with
  MAP_WRITE, map it writable, write one byte and read it from another process; the control is
  masking requested_rights to READ|MAP_READ|MAP_EXEC|INSPECT before line 294. opportunities.
  High. **Fix.**
  Fixed in `2af3c0df`, demonstrated first: a lookup of `/bin/list` asking for `MAP_WRITE` was
  granted it. The server drops `WRITE`, `MAP_WRITE`, `APPEND` and `TRUNCATE` from the request; an
  allow-list broke a program's `copy`. Fails without it: `store_is_read_only_test`.
- [x] **sh-d3** `userspace/nxsh/src/main.rs:297` — Host::run's early returns close nothing and
  stop nothing. Setup send refused after the spawn (292-297, closes only `terminal`), a capture
  over MAX_CAPTURE (417-418, skipping rx close 424, reap 427, err_rx close 432), a pipe `?` at
  241/245/246 and a spawn failure at 259 each leak every pipe, the diagnostic channel,
  setup_shell and the process handles, and leave the spawned stages running. `touch /x (s)` with
  a 4,497-byte argv: send_setup_full's encode() returns SetupTooLarge before any syscall, seven
  handles leak, and `touch` blocks for ever in recv_setup because setup_shell is still open. A
  stage writing over 1 MiB blocks for ever on a pipe whose reader nxsh holds, and keeps its
  sibling terminal, one of tty-server's 14 machine-wide slots; about a dozen occurrences make
  /dev/tty refuse every new terminal. A leftover ChildExited can also be counted by a later
  pipeline's reap, ending it early with the wrong status (unverified). A file starting `\x7fELF`
  that is not a static ET_EXEC makes the spawn fail and leaks the same set. Demonstrated: a
  temporary MockHost host test (removed) handed Host::run a 4,497-byte argv and SetupPayload::
  encode returned Err(SetupTooLarge); `list <5000 x>` reached Host::run unchecked; paths read at
  the lines above; stranded processes and tty-server's terminal count need a boot to see.
  abi, ownership, soundness, opportunities. Medium. **Fix**, as proto-d1.
  Fixed in `2991d0dc`, as proto-d1.
- [x] **sh-d4** `userspace/tty-server/src/main.rs:624` — The console read's PendingOperation is
  never closed: each completed read sets `read_po = 0` and submits another, leaking one PO handle
  and kernel object per console read in an essential server that is never restarted. Every
  keystroke burst on the serial console, and every failed read, leaks one; at handle exhaustion
  sys_io_submit fails and console input, Ctrl-C included, stops for good. Demonstrated: path
  pointed at and grep run (`read_po` appears only at 521, 530, 542, 573-574, 616, 624, none
  closing it); kernel sys_wait reports a PO's completion without closing it (table.rs:1763-1843);
  eshell's po_wait (main.rs:54) and tty-server's own po_wait (172-173) close theirs. Not booted;
  counting handles per keystroke would show it. abi, ownership, soundness, opportunities. Medium.
  **Fix.**
  Fixed in `6961407d`: each consumed console read's PO is closed. A probe stat-ing it found 16 held
  after a test-interactive run before, none after.
- [x] **sh-d5** `userspace/tty-server/src/main.rs:450` — attach_backend returns the previous
  backend's channel for the caller to close, and the serve loop drops it with `Some(_)`. A second
  AttachBackend on one terminal retires the first backend (lib.rs:327, 332-342) and tty-server
  keeps its handle open for ever outside the wait set; the first emulator never sees PeerClosed,
  against rsproto-tty-ops.md § Lifetime, and a loop of attaches grows the handle table without
  bound. Demonstrated: `cargo test -p tty-server --lib re_pointing_a_terminal_hands_back_the_
  backend_it_left` passes, asserting `Some(0xAAA)` comes back; main.rs:449-453 binds it to `_`
  and closes only in the None arm; attach_backend's own doc says dropping it would leak.
  abi, ownership, soundness, opportunities. Medium. **Fix.**
  Fixed in `6961407d`: the backend a re-attach retires is closed; fails without it:
  `tty_reattach_closes_the_old_backend_test`.
- [x] **sh-d6** `userspace/nxsh/src/main.rs:469` — `save` (Host::write_file) creates at
  bytes.len() through SYS_FILE_CREATE, which only grows, so saving over a longer file keeps the
  old tail, and a 0-byte save creates the file and then fails at the size-0 map with 'cannot
  map'. `save` 100 bytes then 10 bytes to /home/a.txt leaves 100 bytes; `[] | save /home/e.txt`
  reports failure for a file that now exists. The SYS_FILE_SYNC result (~504) is also discarded.
  Demonstrated: `cargo test -p fs-server-ext4 --lib a_grows_reply_marks_the_blocks` passes,
  asserting a grow to less answers its size (libfsserver volume.rs:81-84 states the contract);
  nxsh main.rs:472-498 never truncates, the kernel refuses size 0 at table.rs:897, and
  libfs::write_file (lib.rs:210-251) shrinks first and handles size 0. Not booted. abi,
  soundness, opportunities. Low. **Fix.**
  Fixed in `ee47c111`, as sh-o6: `save` goes through `libfs::write_file`; test-interactive's step 5b
  saves one row over two and an empty list, and read "resaved=2" and "cannot map" against the old
  nxsh.
- [x] **sh-d7** `userspace/profile-server/src/main.rs:130` — read_manifest maps and parses
  exactly one page of the manifest, with no stat of its size, and never unmaps it, so a manifest
  over 4096 bytes is cut silently and the packages past the cut vanish from /bin while the server
  logs 'manifest loaded'. A cut `path = "..."` loses its closing quote and is dropped; a cut
  inside a UTF-8 sequence makes from_utf8 fail and yields no packages. Latent: today's manifest
  is about 0.5 KiB with three packages (tools/xtask/src/image.rs:199-229); adding packages is
  the ordinary change that triggers it. Demonstrated: a temporary test (removed) parsed the first
  4096 bytes of a 40-package, 4,301-byte manifest as read_manifest does and got 38 packages with
  no error; main.rs:130 maps PAGE, 137 slices PAGE, 141 closes the handle and nothing unmaps.
  abi, ownership, soundness, opportunities. Low. **Fix.**
  Fixed in `2af3c0df`: the manifest is read at its stat size and unmapped. The test image's manifest
  runs past a page with the test package after it; with the read cut back to 4096 bytes, test-qemu
  loses the test programs and hangs.
- [x] **sh-d8** `userspace/nxsh/src/main.rs:452` — Host::read_file maps and copies the file's
  whole stat size into one Vec with no cap, while stage output is capped at MAX_CAPTURE and
  libfs::read_file at MAX_COPY (8 MiB). `open` on a multi-GiB file (a sparse file grown towards
  u32::MAX, or a disk image on a stick) asks for that allocation and may take the login shell
  down through the panic handler. Unverified: whether the map succeeds at that size and whether
  the failed allocation panics or aborts; a boot running `open` on a file larger than free RAM
  settles it. abi, soundness. Low. **Fix.**
  Fixed in `ee47c111`, as sh-o6: `open` goes through `libfs::read_file`, refused past `MAX_COPY`.
  Still unverified as the survey left it: no gate opens a file over 8 MiB.
- [x] **sh-d9** `userspace/eshell/src/main.rs:286` — cmd_lsblk unmaps with size 0, as does
  coreutils/src/bin/disk.rs:472 (device_info). This is right today only because sys_memory_unmap
  ignores its size and drops the whole VMA (table.rs:949 takes `_size` unused). syscall-abi.md:529
  names partial or splitting unmap as a later refinement; once it honours `size`, eshell leaks one
  mapping per `lsblk` for the life of the shell and disk.rs one per device_info call.
  Demonstrated: path pointed at (eshell maps info.size at 238, disk.rs maps 4096 at 463). Latent,
  made to bite by implementing the refinement. ownership. Low. **Fix.**
  Fixed in `ee47c111`: both unmap the size they mapped. A kernel probe logging every unmap short of
  its mapping found none from `disk` in check-storage, whose format steps call `device_info`; no
  gate runs eshell. The probe found the rest of the class only in sup-d10's and sec-d9's sites.
- [x] **sh-d10** `userspace/nxsh/src/main.rs:294` — The SAFETY argument 'the send failed, so the
  terminal never moved; it is ours to close' is false when send_setup_full fails after its
  Block-mode send committed (the wait fails, or the PO completes PeerClosed because the stage
  closed its setup endpoint while the send was held). The handles have moved, and nxsh closes a
  value it no longer owns; generation counters make the stale value fail InvalidHandle, so it is
  harmless today only by that. Demonstrated: path pointed at (libstream setup.rs:293-320 returns
  Err after a committed send; syscall-abi.md § sys_channel_send; kernel ipc_channel.rs:605-620;
  handle-encoding.md § Generation counter). soundness. Low. **Fix**, as proto-d5.
  Fixed in `2991d0dc`, as proto-d5: nxsh closes nothing a setup message took.
- [ ] **sh-d11** `userspace/nxsh/src/main.rs:459` — Several sites form a Rust reference over a
  FileObject's shared page-cache mapping that another process may write at once: `&[u8]` in nxsh
  read_file (459) and read_head (stage.rs:102), eshell cmd_cat (206), profile-server
  read_manifest (137), and `&mut [u8]` in nxsh write_file (498). That is undefined behaviour in
  Rust's model, though the hardware forgives it; libfs::read_file and write_file make the same
  copies with ptr::copy_nonoverlapping and form no reference. Unverified: needs two processes
  mapping one file with a write during the copy; no host test reaches these syscall paths and
  Miri cannot run them. soundness. Low. **Plan**, in P7.
- [x] **sh-d12** `userspace/nxsh/src/main.rs:84` — `fn signalled(n: i64, handle: u64)` is safe but
  walks `n` 24-byte records of the 96-byte WAIT_RESULTS through raw pointer arithmetic with no
  bound, so any n > 4 reads past the static. Latent: every caller passes sys_wait's return for at
  most 4 handles; waiting on a fifth handle without growing WAIT_RESULTS makes it bite.
  Demonstrated: Miri on a verbatim copy (removed), `signalled(5, 0xdeadbeef)`, reported 'attempting
  to access 8 bytes, but got alloc+0x60 which is at or beyond the end of the allocation of size 96
  bytes'. opportunities. Low. **Fix.**
  Fixed in `ee47c111`: `signalled` clamps its count to the records `WAIT_RESULTS` holds; Miri on a
  verbatim model fails at `n = 5` before and passes after.
- [x] **sh-d13** `userspace/profile-server/src/main.rs:478` — reply_success ignores a failed
  send, so the resolved store handle it meant to move stays in profile-server's table and the
  forwarded resolve is never answered; tty-server open_tty (318-327) answers no error on a failed
  send either. This breaks the rule libfsserver's send_forwarded keeps (every forwarded request
  is answered, handles from a refused reply closed). If the kernel refused the reply (OutOfMemory,
  a handle missing TRANSFER), the client's lookup would wait for ever, as forwarded resolves have
  no deadline. Unverified: today the handle carries TRANSFER (requested at 294) and the forwarding
  endpoint completes uncorrelated replies with Ok(0) (kernel table.rs:2146-2151), so no ordinary
  failure was found; a send with a handle restricted to drop TRANSFER would settle it.
  opportunities. Low. **Fix.**
  Fixed in `6961407d` and `2af3c0df`, demonstrated first: with `TRANSFER` dropped from the reply's
  handle by a probe, `/bin/list`, `/bin`, `/dev/tty` and an `OpenSibling` went unanswered past 3 s.
  A refused reply now closes what it carried and answers the send's error, `NoAccess` there; the
  sweep added `OpenSibling` and the directory session.

**Opportunities**

- [ ] **sh-o1** encapsulate: raw u64 handles closed by hand on every path, and a received handle
  taken as whatever the server assumes — 54 sites (`grep -rn 'SYS_HANDLE_CLOSE,'` over
  userspace/{nxsh,coreutils,tty-server,eshell,nxinstall,profile-server}/src, minus imports:
  nxsh 15, tty-server 12, coreutils 12, eshell 6, profile-server 5, nxinstall 4), such as
  `userspace/nxsh/src/main.rs:297`, `userspace/tty-server/src/main.rs:450`,
  `userspace/tty-server/src/main.rs:624`. Use libos's owning `Handle<T, M>` (Drop closes,
  `into_raw` to move one into a message or spawn), grown with the kinds this subsystem holds and
  libos lacks (Channel with `create_pair`, send and `recv` returning `Received`, and waitable;
  `Process::terminate`; Timer; Entropy; File; `handle_stat`) and a PendingOp whose `wait(self)`
  closes. attach_backend then returns `Option<Handle<Channel, _>>`, which closes when dropped;
  tty-server's `read_po` becomes an `Option<Op>` that closes on replacement. With a libos `Sys`
  seam behind it, NitroxHost::run's error paths become host-testable. Checked against
  NitroxHost::run, disk.rs `write`'s five early-return closes and tty-server attach_backend.
  Closes: handles and processes leaked on early returns (sh-d3, sh-d4, sh-d5) and closing a
  handle that already moved (sh-d10). Depends on: libos gaining the wrappers above; eshell is
  excluded by its CLAUDE.md (6 sites). abi, ownership, soundness, opportunities. Value high,
  effort large. **Plan**, as sup-o2, in P7.
- [ ] **sh-o2** encapsulate: rsproto messages sent and received by hand through `static mut
  [u8; 4096]` (encode at [24..], payload_len stamped at [4..8], handle_count at [8], handles in
  a second static, raw SYS_CHANNEL_SEND; receive then `from_raw_parts(MSG + 24,
  len.min(4096 - 24))` with an unbounded lifetime) — 22 sites (13 sends: `grep -rn
  "\[4\.\.8\]\.copy_from_slice\|\[OFF_PAYLOAD_LEN\.\."` over the six crates; 9 receives: `grep -rnE
  "(TTY|RECV|ERR)_MSG\[4\], "` over nxsh, tty-server, profile-server), such as
  `userspace/tty-server/src/main.rs:415`, `userspace/tty-server/src/main.rs:245`,
  `userspace/profile-server/src/main.rs:407`, `userspace/nxsh/src/main.rs:979`.
  `librsproto::io::MsgBuf` (one page, boxed or in a static): `recv(&mut self, ch) ->
  Result<(Message<'_>, Received), i64>` clamps payload_len to IPC_PAYLOAD_SIZE and borrows the
  buffer; `send(&mut self, ch, op, request_id, flags, body, handles: &[u64], mode)` derives
  payload_len and the handle count from its arguments and, on a refused send, hands back the
  handles that did not move (the shape of libfsserver's send_forwarded). libprompt::ipc's
  send/recv move down into it. Checked against tty-server serve_tty and open_sibling,
  profile-server serve_session and nxsh tty_exchange/tty_await_reply. Closes: a message view
  outliving the next receive into the same static (tty-server serve_tty's `body` is live across
  perform() by call order only), a send buffer shorter than the 4096 bytes sys_channel_send
  copies, hand-written bounds that disagree with the kernel (nxinstall raw_send clamps to 4072
  where the kernel refuses past 4008), and moved handles stranded by a refused reply (sh-d13).
  Removes 22 unsafe blocks and 9 statics. Depends on: nothing. abi, ownership, soundness,
  opportunities. Value high, effort medium. **Plan**, as rt-o5, in P7.
- [ ] **sh-o3** encapsulate: raw SYS_WAIT into hand-sized buffers, IoResult decoded by byte offset
  (handle @0, status @8, result @16), and PendingOperation lifecycles with private po_wait copies
  — 13 call sites (`grep -rn "syscall4(SYS_WAIT\|SYS_WAIT," userspace/{nxsh,coreutils,tty-server,
  eshell,nxinstall,profile-server}/src`, minus imports; nxsh 4, tty-server 2, profile-server 2,
  eshell, nxinstall, sleep, disk, entropy 1 each), 7 `static mut` wait-buffer pairs, 4 private
  po_wait definitions and 5 near-identical lookup wrappers, such as
  `userspace/tty-server/src/main.rs:582`, `userspace/nxsh/src/main.rs:393`,
  `userspace/nxinstall/src/main.rs:102`, `userspace/profile-server/src/main.rs:93`. Two parts.
  In libos, make its private `sys::wait` public as `wait<'r>(handles, out: &'r mut
  [IoResult; N], deadline) -> Result<&'r [IoResult], Error>`, or a `WaitSet<const N: usize>` with
  `push(h) -> Result<usize, Full>` and a typed iterator, each wait owning its results; and use
  `Op` (closes the PO on Drop) with `block_on` for one-shot waits and `Option<Op>` for a server's
  console read. In libkern, a thin `wait(&[u64], &mut [IoResult], deadline)` that refuses
  `results.len() < handles.len()`, so eshell can use it. Checked against nxsh's capture and reap
  loops, tty-server serve_loop, nxinstall Io::submit and sleep's two-handle wait. Closes: a PO
  never closed (sh-d4), a results buffer smaller than the handle count (sh-d12), and a nested
  wait clobbering records the caller still reads (nxsh reap then drain_diagnostics ->
  tty_await_reply; profile-server's ns_lookup shares the serve loop's buffers; tty-server added
  PO_RESULTS after exactly this). Depends on: libos making its wait public; eshell stays on
  libkern. abi, ownership, soundness, opportunities. Value high, effort medium.
  **Plan**, as rt-o2, in P7.
- [x] **sh-o4** encapsulate: nxsh Host::run holds a dozen raw handles and a `Vec<u64>` of
  children with cleanup written per path, missing on 6 exits — 6 sites (`sed -n 192,435p
  userspace/nxsh/src/main.rs | grep -n 'return Err\|)?;'` gives 8 exits; 2 come before any handle
  exists), at `userspace/nxsh/src/main.rs:245`, `userspace/nxsh/src/main.rs:259`,
  `userspace/nxsh/src/main.rs:297`, `userspace/nxsh/src/main.rs:418` (also 241 and 246).
  `struct Pipeline { owned: Vec<u64>, children: Vec<u64>, reaped: bool }` with `own(h)`,
  `hand_over(h)` (forget it once a send or spawn moved it), `child(proc)`, and a Drop that closes
  what is still owned and, unless reaped, closes the capture read end, terminates every child and
  drains their ChildExited. It also encodes the setup payload before the spawn. Home:
  userspace/nxsh/src/pipeline.rs (main.rs is at its size entry), beside stage.rs's `Images`.
  Closes: handles and processes stranded by an early return (sh-d3) and the stale ChildExited
  they leave for the next reap. Depends on: nothing; sh-o1's handle type would shrink it.
  ownership, abi, opportunities. Value high, effort medium. **Fix.**
  Fixed in `2991d0dc`: `nxsh::pipeline::Pipeline`, in the library half behind `pipeline::Sys` so it
  is host-tested; the binary's `Sys` is src/stages.rs.
- [ ] **sh-o5** encapsulate: map, `from_raw_parts` slice, then unmap (or forget to), with the
  mapped, read and object lengths chosen separately — 11 sites (`grep -rn 'SYS_MEMORY_MAP,'` gives
  12, 9 of them file reads and 1 a mapped write; `grep -rn from_raw_parts` gives 21, 11 over
  mappings), such as `userspace/nxsh/src/main.rs:459`, `userspace/nxsh/src/stage.rs:102`,
  `userspace/profile-server/src/main.rs:137`, `userspace/coreutils/src/bin/whoami.rs:110`.
  A `Mapping` from `Handle<File|Memory, MapRead>::map_all()` (stat, then map exactly the size)
  with `len()`, `copy_out(off, &mut [u8])` and `to_vec()` via copy_nonoverlapping, as
  libfsserver's PartitionIo does, `write` for writable modes, and Drop that unmaps its own
  (addr, len). Checked against nxsh read_file, whoami, disk device_info and write_file. Home:
  libos objects.rs (`map` returns a raw `*mut u8` today). Closes: references over shared
  page-cache mappings (sh-d11), mappings never unmapped (profile-server read_manifest, nxinstall
  devices), size-0 unmaps (sh-d9) and a manifest cut at a page (sh-d7). Depends on: sh-o1, for
  `&Handle`; eshell keeps its 2 sites. ownership, soundness, opportunities. Value medium, effort
  medium. **Plan**, as rt-o3, in P7.
- [x] **sh-o6** remove: nxsh's own file I/O and lookup helpers duplicate libfs, already a
  dependency — 3 helpers, 15 unsafe blocks (`awk 'NR>=437 && NR<=508 && /unsafe/'` gives 11,
  `NR>=747 && NR<=783` gives 4; `grep -n "SYS_MEMORY_MAP,\|SYS_FILE_CREATE,\|syscall4(SYS_NS_LOOKUP"`
  locates them), at `userspace/nxsh/src/main.rs:437`, `userspace/nxsh/src/main.rs:469`,
  `userspace/nxsh/src/main.rs:755`. Call `libfs::read_file` (MAX_COPY cap, copy without a
  reference), `libfs::write_file` (truncates first, returns the sync result) and
  `libfs::lookup_wait`; stage.rs read_head stays. libfs::read_file needs one small extension, to
  report a directory as an error rather than as empty, which nxsh relies on. Home: libfs. Closes:
  sh-d6, sh-d8 and the reference-over-mapping UB at main.rs:459 and :498. Depends on: that
  extension. abi, soundness. Value medium, effort small. **Fix.**
  Done in `ee47c111`: nxsh's file helpers call `libfs::read_file`, `write_file` and `lookup_wait`.
  The extension: `file_size` refuses a directory, as its doc said, and `read_file` looks a file up
  once.
- [ ] **sh-o7** narrow: a handle received over IPC is kept as a given object type with only `!= 0`
  checked — 3 sites (every `sent.take(` and kept `terminal`/`streams` handle in the paths), such
  as `userspace/tty-server/src/main.rs:443`, `userspace/nxsh/src/main.rs:1089`,
  `userspace/coreutils/src/stage.rs:129`. A checked constructor
  `Handle::<Channel, M>::from_received(raw) -> Result<Self>` (or `Received::take_as::<T>`) that
  calls sys_handle_stat, verifies the object type and the WAIT/SEND/RECV rights the mode needs,
  and closes a wrong one; libos.md already anticipates it. Home: libos handle.rs, beside the
  unchecked `unsafe fn from_raw`. Closes: a client-supplied handle of the wrong type or rights
  poisoning a server's wait set (sh-d1). Depends on: sh-o1's Channel type. ownership,
  opportunities. Value medium, effort small. **Plan**, in P7.
- [ ] **sh-o8** remove: `static mut` used only as a syscall out-parameter, argument block or
  per-call scratch, where a local does the job — 12 sites (`grep -rn 'static mut'` gives 45;
  CLOCK_BUF in sleep.rs and date.rs, CTRL_OUT0/1 in tty-server and profile-server, NOTIF/NOTIF_BUF
  in nxsh, with.rs and coreutils stage.rs, SPAWN in nxsh, FRONT/BACK in nxinstall; ownership
  also counts TTY_HANDLES/REPLY_HANDLES), such as `userspace/tty-server/src/main.rs:57`,
  `userspace/coreutils/src/bin/sleep.rs:34`, `userspace/nxinstall/src/main.rs:99`,
  `userspace/coreutils/src/stage.rs:21`. Plain locals, as libstream::setup::pipe, nxsh now_ns
  and with.rs token_deadline already do; a stack array or Vec for FRONT/BACK (sources() keeps a
  FRONT_BYTES array on the stack). Home: each binary. Closes: two live `&mut` to one static in
  nxinstall and about 20 unsafe blocks that only reach the statics. Depends on: nothing. abi,
  ownership, soundness, opportunities. Value low, effort small. **Plan**, in P7.
- [ ] **sh-o9** remove: profile-server keeps its state in `static mut` (INDEX, SESSION_CH,
  SESSION_DIR, PENDING_DIR) and reborrows the index as `&'static [Entry]` — 4 sites
  (`grep -n '^static mut \(INDEX\|SESSION_\|PENDING_DIR\)'`), at
  `userspace/profile-server/src/main.rs:48`, `userspace/profile-server/src/main.rs:267`,
  `userspace/profile-server/src/main.rs:595`. `struct Server { index: Vec<(String, Vec<Entry>)>,
  sessions: [(u64, &'static str); MAX_SESSIONS] }` passed as `&mut` through serve_loop, with
  `index(&mut self, ..) -> &[Entry]` and the pending-dir handoff as a return value; tty-server's
  Registry is the model. Home: profile-server. Closes: a `&'static` reborrow whose soundness
  rests on a comment (it holds today; see below). Depends on: nothing. ownership. Value low,
  effort small. **Later.**
- [ ] **sh-o10** encapsulate: `librsproto::session::Dir` is closed by an explicit `close(self)`
  with no Drop — 4 sites in the subsystem (`grep -rn 'Dir::open'`; 4 more in libfs), at
  `userspace/nxsh/src/main.rs:528`, `userspace/coreutils/src/bin/mkdir.rs:134`,
  `userspace/profile-server/src/main.rs:204`. `impl Drop for Dir` closing the endpoint, with
  `close(self)` kept as an early release. Home: librsproto session.rs. Closes: a session endpoint
  leaked by an early return between open and close; none found today. Depends on: nothing.
  ownership. Value low, effort small. **Plan**, as sup-o2, in P7.
- [ ] **sh-o11** encapsulate: a coreutil opens a one-shot service session with
  `libfs::lookup_wait(DIR_SESSION_RIGHTS)`, calls round_trip and closes the endpoint with a raw
  syscall — 4 sites (`grep -rn 'round_trip(session' userspace/coreutils/src/bin`), at
  `userspace/coreutils/src/bin/service.rs:216`, `userspace/coreutils/src/bin/shutdown.rs:85`,
  `userspace/coreutils/src/bin/log.rs:95`. `librsproto::session::Session`, owning the endpoint
  and closing it on Drop: `Session::open(ns, path) -> Result<Session, i32>` (NotFound kept
  distinct) and `ask(&mut self, buf, op, body) -> Result<usize, WireError>`. Home: librsproto
  session. Closes: a session endpoint leaked on an early return; removes 4 unsafe blocks.
  Depends on: nothing. opportunities. Value low, effort small. **Plan**, in P7.
- [ ] **sh-o12** remove: raw operations where core or the type already offers a safe one — 3
  sites (the inventory's only asm, its only `unsafe fn`, and a raw slice over a local struct
  field), at `userspace/eshell/src/main.rs:367`, `userspace/nxsh/src/main.rs:1175`,
  `userspace/nxinstall/src/main.rs:891`. `core::hint::spin_loop()` for `asm!("pause")` (the
  tty-server and profile-server exit loops already use it), `&entry.path[..len]` as eshell
  main.rs:157 does, and a safe `pub extern "C" fn _start` as every other bin's is. Home: each
  binary. Closes: nothing new; removes 3 unsafe sites and the only asm. Depends on: nothing.
  soundness, opportunities. Value low, effort small. **Plan**, as rt-o9, in P7.

What held up: every receive goes through libkern's `Received` into a whole 4096-byte page, with
every payload slice clamped to 4096-24, and every send passes a whole page within
IPC_PAYLOAD_SIZE; unwanted handles close on drop. sys_wait results buffers fit the handle counts
passed, and tty-server's wait set stays within 3 + 2*MAX_TTYS = 31 <= 32 (profile-server's is
1 + 31 = 32) because a backend left with no terminal is retired; tty-server's po_wait uses its own
PO buffers, so a nested wait cannot clobber the serve loop's records. profile-server's
`&'static [Entry]` over `static mut INDEX` is sound: a push moves only the outer tuples, and a
replica holding the slice across 8 reallocating pushes passes Miri under Stacked Borrows and Tree
Borrows. PENDING_DIR and SESSION_DIR hold `&'static str` from PROJECTED, never slices of
RECV_MSG, and `bin/../x` cannot traverse. ChildExited offsets, IoResult offsets and the NsEntry
path clamp agree with the specs and libkern; mapped reads stay inside their mappings (the kernel
refuses a map past the object); console read counts are clamped (64 and 256); entropy buffers are
used only when the read returned 0. The Images guard, read_file, write_file and read_head close
and unmap on every path, tty-server's open_tty and open_sibling reclaim both ends when the reply
does not go out, and nothing in the subsystem creates a thread. 20,000 rounds of random bytes
through libline::Discipline and the tty-server Registry gave no panic (probe removed).

### test-harness — main at 135055a8, 2026-10-27

Lenses: abi, ownership, soundness, opportunities; returned nothing: none.
Defects: found 25, refuted by a boot 0, reported 25 (13 distinct once duplicates merge). Opportunities: 35 proposed (16 distinct once merged).

The inventory, `cargo xtask unsafe-inventory userspace/test-harness`:

| path | code lines | unsafe block | unsafe fn | unsafe impl | static mut | raw syscall | raw slice | raw deref | transmute | asm | volatile |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `userspace/test-harness` | 10927 | 494 | 4 | 0 | 31 | 414 | 17 | 12 | 0 | 9 | 23 |
| `total` | 10927 | 494 | 4 | 0 | 31 | 414 | 17 | 12 | 0 | 9 | 23 |

**Defects**

- [x] **th-d1** `userspace/libos/src/objects.rs:167` — `libos::thread_create` is a safe `pub fn`
  that starts a thread at any `entry` on any `user_sp`. Safe code can start a thread on a live
  local array or on a mapping it later unmaps, and the new thread's frames then overwrite memory a
  Rust reference owns. The kernel checks only that both addresses are non-null user addresses
  (table.rs:524-541). All 5 callers (harness.rs:159, :362; stage.rs:507, :629;
  bootprobe/exits.rs:242) map a fresh stack and never unmap it, so the invariant holds by
  convention only. Demonstrated: a `#[forbid(unsafe_code)]` fn that passed a local `[u8; 256]` as
  the stack and then wrote `stack[0]` built with `cargo check -Z build-std=core,alloc,compiler_builtins
  --bin test-stage` and no error; the probe was removed. abi, soundness. Medium. **Fix**, as rt-d3.
  Fixed with rt-d3, in `670a3414`.
- [x] **th-d2** `userspace/test-harness/src/bootprobe/storage.rs:273` — `past_two_tib_test` makes
  one `&mut [u8]` over the DMA page and keeps it live across `io(IO_OPCODE_WRITE)` and
  `io(IO_OPCODE_READ)`, then compares through it at :284. The device's write invalidates the
  reference, so the read is UB, and the SAFETY comment argues bounds only. Today LLVM cannot
  exploit it. Moving the compare into a helper taking `buf: &mut [u8]` makes it a `noalias`
  argument, and LLVM may then forward the zeros and report "what was read back past 2 TiB is not
  what was written" on a working disk. Demonstrated: three independent Miri models of the same
  sequence (slice over an exposed address, fill, a write through
  `with_exposed_provenance_mut`, read) fail under Stacked Borrows ("trying to retag ... that tag
  does not exist in the borrow stack") and Tree Borrows ("reborrow through ... is forbidden").
  The control, with the slice made after the I/O as `RootDevice::read_at` does, passes. The models
  are analogs, since Miri cannot run the syscall. No other site keeps a reference across a device
  write. abi, ownership, soundness, opportunities. Low. **Fix.**
  Fixed in `009c098c`: no slice over the page outlives the device's I/O; Miri on a model of the
  sequence fails before, under Stacked and Tree Borrows, and passes after.
- [x] **th-d3** `userspace/test-harness/src/bootprobe/main.rs:363` — boot-probe's `receive()`
  calls `take_all()` on `libkern::Received`, which disarms its close-on-drop, and moves the
  handles into its own `Received` with `handles: Vec<u64>` and no `Drop`. 9 of the 20 `receive()`
  callers never close them: views_receive (main.rs:331, behind every `views_call`),
  exits.rs:110, views.rs:402, views.rs:456-457 (keeps the first, leaks the rest), accounts.rs:311,
  policy.rs:51, policy.rs:183, received.rs:184 and files.rs:570. Latent, because no views or tty
  reply carries a handle today. A broker answering Started with the program's life channel, or a
  tty token reply carrying a channel, would leak one handle per call. Demonstrated by path:
  main.rs:356-371 empties the guard into a `Vec<u64>`. The other 11 callers close
  `m.handles` by hand (12 `for_each` loops). The three lenses counted the unclosed callers as
  8 of 15 or 9 of 20; the 9-of-20 count is used here. abi (as opportunity), ownership, soundness,
  opportunities. Low. **Fix.**
  Fixed in `009c098c`, as th-o8; fails without it:
  `receive_closes_what_its_caller_does_not_take_test`.
- [x] **th-d4** `userspace/test-harness/src/harness.rs:2354` — Every coreutil spawn looks up the
  image handle and never closes it, though `sys_process_spawn` only looks the image up
  (table.rs:352-353; process-spawn-args.md:43: "the spawner closes it after spawn").
  `run_coreutil_in` (:2354), `run_coreutil_capture_in` (:1340), `dead_stage_closes_its_pipe_demo`
  (:1471) and `stage_spawn_demo` (:227) leak it; stage.rs:444 (`run_grants`) does too but exits
  at once. A test-qemu run makes about 54 to 56 such calls (42 `run_coreutil_in`, 12 capture), so
  that many `/bin` file handles stay pinned until the harness exits. `exit_storm_demo` (:997),
  `fp_hardfloat_demo` (:3435) and boot-probe's `spawn_stage` close theirs. Demonstrated by path:
  `grep -n "let (st, img)\|HANDLE_CLOSE, img" harness.rs` shows 6 lookups and 2 closes. abi,
  ownership, opportunities. Low. **Fix.**
  Fixed in `009c098c`: each spawn closes its image. `sys_ns_held` on `/system` across a test-qemu
  run's coreutil spawns rose from 4 to 14 before, and stays at 1 after.
- [x] **th-d5** `userspace/test-harness/src/bootprobe/views.rs:226` — `view_broker_test` opens
  client channels until the broker refuses one (:218-236). Five of its six failure returns (226,
  236, 249, 253, 258) do not close them; only the loop at :260 does. A misbehaving broker then
  leaves boot-probe holding a whole wait set of clients. The broker counts each as two slots in
  its machine-wide wait set (rsproto-views-ops.md:36-40), so every later views resolve is refused
  WouldBlock. `view_grants_end_test`, `storage_grant_test`, `policy_test`, `grace_test` and
  `accounts_test` would then fail for the wrong reason and the broker would stay wedged for the
  boot. Demonstrated by path: the returns skip :260. Failure paths only. ownership. Low. **Fix.**
  Fixed in `009c098c`: the clients close as they drop. A probe failing the test with its clients
  open left the views tests after it failing "no supervisor channel" before, none after.
- [x] **th-d6** `userspace/test-harness/src/bootprobe/files.rs:694` — On success, `overwrite_test`
  (:694), `grow_test` (:777) and `create_test` (:893) never close the file handle or unmap their
  writable mapping, and `read_large_file` (:934) closes the handle but never unmaps its 32 KiB
  mapping. `/system/rwtest` (mapped twice) and `/system/created` stay held and mapped until
  boot-probe exits, so anything later that checks the root mount has let go of its files
  (`sys_ns_held` on `/system`, an unmount) would see them held. Demonstrated by path: no
  `SYS_MEMORY_UNMAP` anywhere in files.rs:684-969, and the closes are only on the failure paths
  (698, 781, 897). ownership, soundness, opportunities. Low. **Fix.**
  Fixed in `009c098c`: each test unmaps and closes its file. Held files across the files tests rose
  from 3 to 6 before, 1 to 1 after.
- [ ] **th-d7** `userspace/test-harness/src/harness.rs:3727` — `forward_demo` releases nothing it
  made on success: the namespace with the userspace-server binding (3727/3736), `send_end`
  (3724), the resolved MemoryObject (3846) and both mappings (3782, 3853). The same success-path
  leak is in `ns_demo` (mapping at :607), `initramfs_demo` (:864), `read_block_sector0` (:3562,
  one DMA page per call), `worker_exception_demo` (stack object :343, mapping :349) and
  `stream_transport_demo` (stack mapping at :144, deliberately kept). Error paths in
  `block_send_demo` (441/448), `block_bounded_demo` (508), `ns_demo` (565-626),
  `dev_entropy_lookup_demo` (735/745) and `proc_self_demo` (816/825/835) also return without
  closing. Demonstrated by path: `awk 'NR>=3713 && NR<=3865 && /close\(|SYS_HANDLE_CLOSE|SYS_MEMORY_UNMAP/'
  harness.rs` prints nothing. Bounded: the harness exits at the end of the chain. ownership,
  soundness, opportunities. Low. **Plan**, in P7.
- [x] **th-d8** `userspace/test-harness/src/harness.rs:3156` — `rename_wait` discards
  `sys_wait`'s return and reads the status from the shared `WAIT_RESULTS[8..12]` anyway. Its own
  SAFETY comment says those bytes hold the result "only when it returned 1". A failed wait (for
  example OutOfMemory from a registration over its reserve, table.rs:1840) returns the previous
  wait's status, usually 0, so `rename_demo` takes a rename that never completed as done. Of the
  20 sites that parse `WAIT_RESULTS[8..12]`, this is the only one that skips the check. Today the
  deadline is `u64::MAX` on a fresh PO, so no input makes it fail. Unverified: nothing was run.
  A host test cannot reach it, since `syscallN` runs Linux syscalls on the host. A boot with
  `SYS_WAIT` forced to fail once after a successful lookup would settle it. abi, soundness,
  opportunities. Low. **Fix.**
  Fixed in `009c098c`: `rename_wait` goes through `po_wait`, which checks the wait. A probe failing
  the wait read status 0 before, -1 after.
- [ ] **th-d9** `userspace/test-harness/src/stage.rs:621` — The crate's user threads run on stacks
  with no guard gap. Each stack is mapped at exactly its size with hint 0, so first-fit places it
  directly against the previous mapping, and `EXIT_SPAWNING` carves 48 stacks from one mapping.
  Other sites: stage.rs:487-507 (4 pages), harness.rs:132-160 (8 pages), harness.rs:337-363 (1
  page), bootprobe/exits.rs:222-243 (4 pages). A thread body that grows (a deeper call, a debug
  build) overruns into the next-lower thread's live stack or a libheap arena with no fault.
  Demonstrated in part: a temporary kernel host test mapping two 4-page anonymous VMAs via
  `find_free_range` + `map_vma_lazy` asserted `b.start == a.end` and passed (vmm.rs:413-435); the
  probe was restored. Unverified: the overrun itself, which a boot with a thread recursing past
  its stack would show. soundness. Low. **Plan**, in P7.
- [x] **th-d10** `userspace/test-harness/src/inputclient.rs:104` — `lookup` never closes the
  PendingOperation that `sys_ns_lookup` returned, because this copy of `po_wait` (:72-96), unlike
  the other 11 hand-rolled copies, does not close the PO. One PO handle leaks per
  `Stream::open` of `/dev/input/new`. Demonstrated by path: no `SYS_HANDLE_CLOSE` in
  inputclient.rs:72-106. harness.rs:789, bootprobe/main.rs:117/157 and pattern.rs:271 all close.
  ownership. Low. **Fix**, as rt-d7.
  Fixed with rt-d7, in `aea50b2c`.
- [ ] **th-d11** `userspace/test-harness/src/composebench.rs:166` — `libdraw::acquire::acquire`
  requires that "nothing else may write the mapping" (acquire.rs:85-86), but compose-bench (and
  display-selftest, display.rs:82) acquire the aperture while the compositor, an essential
  service in the same image (tools/xtask/src/image.rs:158-165, kept in the bench image at
  :184-196), also owns and writes it. compose-bench takes `&mut [u8]` over the whole aperture
  (`fb.bytes_mut()` at :142 and :198), so a compositor damage copy racing a `frame_in_place` or
  `copy_damage` is a cross-process data race behind a live `&mut`. In display-selftest `_fb` is
  unused, so it is latent; hashing the screen through `_fb.bytes()` would make it bite. The two
  SAFETY comments cover only `root_ns` being live. Unverified: the contract violation is confirmed
  by reading; a boot of the bench image with a client committing during the E2 loop would show
  the torn frames. soundness. Low. **Plan**, in P7.
- [x] **th-d12** `userspace/test-harness/src/bootprobe/files.rs:85` — `RootDevice::read_at`
  hard-codes `const SECTOR: u64 = 512`, but the kernel refuses any block request that is not
  whole logical blocks (`whole_blocks`, kernel/src/io/block.rs:97). On a device reporting
  4096-byte blocks (xhci/storage.rs:1070 takes `block_len` from READ CAPACITY) every
  `RootDevice` read returns InvalidArgument, and overwrite, grow, create, file_cache, unlinked and
  the storage-admin device checks all report FAIL on a correct filesystem. RAM disk
  (`RAMDISK_BLOCK` = 512) and AHCI are 512, so no gate reaches it. Unverified: a boot of
  `image --live --selftest` on a QEMU usb-storage device with `logical_block_size=4096` would
  settle it. abi. Low. **Fix.**
  Fixed in `009c098c`: `RootDevice` reads whole pages from a page boundary, to the device's end,
  whole blocks of any size up to a page; no `info` resolves under a `by-partlabel` name to ask. A
  probe making the kernel ask 4 KiB blocks of its requests failed every root-device check before,
  none after.
- [x] **th-d13** `userspace/test-harness/src/bootprobe/storage.rs:71` — Latent: 17
  `SYS_MEMORY_UNMAP` calls pass size 0 and work only because the kernel ignores `size`
  (`sys_memory_unmap(addr, _size)` at table.rs:949, TODO(mm); syscall-abi.md:529). If the kernel
  made the fix `deferred-decisions.md:2343` proposes and refused a size that does not cover the
  VMA, each call would fail unchecked and leak: storage.rs:71 and storage_admin.rs:207 would
  keep files held, `storage_admin_test`'s unmount of nitrox-scratch would be refused WouldBlock,
  and `RootDevice::drop` (files.rs:113) would leak a page per open. Sites: main.rs:402,
  files.rs:113/182/183/227/229/276, storage.rs:71/286, storage_admin.rs:207, registry.rs:581/647,
  exits.rs:91, gates.rs:208, harness.rs:3473/3505/3611. Counted by `grep -rnE "SYS_MEMORY_UNMAP,
  [a-z_.]+( as u64)?, 0\)" userspace/test-harness/src | wc -l` = 17. Unverified: a kernel branch
  refusing size 0 and a test-qemu run would settle it. opportunities. Low. **Fix.**
  Fixed in `009c098c`: each unmap passes the size it mapped. A kernel probe logging unmaps short of
  their mapping found boot-probe's and test-harness's before, none after.

**Opportunities**

- [ ] **th-o1** encapsulate: handles are bare `u64`s closed by hand on every path, success and
  failure — 374 sites (113 `syscall1(SYS_HANDLE_CLOSE` by `grep -rnoE
  'syscall[14]\((libkern::)?SYS_HANDLE_CLOSE'` plus 261 `close(x)` calls by `grep -rnoE
  '\bclose\([a-z_]' | grep -v 'fn close'`; 146 lookup calls return `(status, u64)`), such as
  `harness.rs:2354`, `bootprobe/files.rs:688`, `bootprobe/views.rs:218`, `harness.rs:3727`.
  Use libos's owning `Handle<T, M>` for every kind held, adding Channel, Timer, Pending and File
  markers next to Memory/Namespace/Notify/Resource/Process/Thread, and a typed lookup
  `lookup::<K>(ns, path, rights) -> Result<Handle<..>>` over `block_on(Op)`, so the PO and the
  result are both owned. `into_raw()` is used only where a spawn or send moves the handle. Checked
  on harness.rs:2354, exits.rs:102-108 and files.rs:688. In libos (handle.rs, objects.rs).
  Closes: handles left open on success and failure paths (th-d4, th-d5, th-d6, th-d7, th-d10) and
  double closes; removes 113 unsafe close blocks. Depends on: libos markers for
  Channel/Timer/Pending/File; easier after th-o3. ownership, opportunities. Value high, effort
  large. **Plan**, as sup-o2, in P7.
- [ ] **th-o2** encapsulate: a mapping is a bare address that is turned into `from_raw_parts`
  slices or raw volatile accesses, with a hand-written unmap that some paths never reach — 38 to
  48 sites (38 `SYS_MEMORY_MAP` by script, plus 9 `map_file(` calls; 32 unmaps, 17 of them size 0;
  17 `from_raw_parts`; 22 `_volatile`), such as `bootprobe/storage.rs:273`, `harness.rs:3197`,
  `bootprobe/main.rs:393`, `bootprobe/files.rs:34`. An owning `libos::Mapping<'h>` returned by
  `Handle<Memory | File, M>::map(len)`, unmapping with its own length on drop, with `bytes()` for
  immutable snapshots, volatile `read_at`/`write_at` for pages another party writes, and a scoped
  `with_mut(|b: &mut [u8]| ..)` so no reference lives across a call that lets the kernel write
  the page; `as_mut_slice` exists only for MapReadWrite. `RootDevice` holds one and
  `composebench::write_throughput(*mut u8, len)` takes `&mut [u8]`. Checked on read_all,
  `with_sched_stats`, write_file and gates.rs:194-209. In libos objects.rs, beside
  `Handle<Memory,_>::map`, which returns a `*mut u8` and unmaps nothing. Closes: a `&mut` held
  across a device write (th-d2), mappings never unmapped (th-d6, th-d7), size-0 unmaps (th-d13),
  offsets past the mapped length. Depends on: th-o1 for the File marker. abi, ownership,
  soundness, opportunities. Value high, effort medium. **Plan**, as rt-o3, in P7.
- [ ] **th-o3** encapsulate: out-params of kernel calls held by convention as raw pointers cast
  to `u64` — 46 sites (20 `CHANNEL_CREATE`, 12 `CLOCK_READ`, 5 `NOTIF_RECV`, 5 `HANDLE_STAT`, 3
  `ENTROPY_READ`, 1 `THREAD_GET_REGISTERS` by `grep -rnE "syscall[0-9]\((libkern::)?SYS_(...)"`;
  3 hand decodes of `Notification.body[8]`), such as `harness.rs:128`, `harness.rs:800`,
  `harness.rs:912`, `harness.rs:294`, `composebench.rs:46`. Typed libos calls:
  `channel(depth) -> Result<(Handle, Handle)>` (libstream `setup::pipe` is already used 9 times
  against 20 raw creates), `clock_now(ClockId) -> Result<u64>`, `handle_stat(&Handle) ->
  Result<HandleInfo>`, `Entropy::read(&mut [u8])` with the length bound checked,
  `Handle<Thread>::registers()`, the existing `Handle<Notify, Only>::recv`, and a libkern
  `Notification::child_exit_code()`. Checked on harness.rs:128, :800-808 and :912. In libos
  objects.rs, with the accessor in libkern abi.rs. Closes: an out-param smaller than what the
  kernel writes — this crate's own HandleInfo 16-vs-24 smash (harness.rs:798) — and a clock
  out-param shared through a static, with the 'failed read leaves its last value' reads at
  harness.rs:2004 and inputclient.rs:478. Depends on: nothing. opportunities, soundness. Value
  high, effort medium. **Plan**, as rt-o1, in P7.
- [ ] **th-o4** encapsulate: raw `SYS_WAIT` on a hand-sized result buffer with the `IoResult`
  decoded from byte offsets (status at 8..12, result at 16..24), each caller checking `== 1` for
  itself — 39 sites (35 `grep -rnE "syscall[456]\((libkern::)?SYS_WAIT,"` plus 4 multi-line;
  17 status decodes, 12 to 13 result decodes, 10 to 14 local helper clones: `po_wait` x4,
  `po_wait_pair`, `ns_lookup_wait`, `ns_lookup`, `wait_one`, `wait_status`, `po_poll`), such as
  `harness.rs:760`, `harness.rs:3156`, `bootprobe/deadlines.rs:17`, `bootprobe/files.rs:507`,
  `inputclient.rs:72`. Publish what libos has privately: a safe `libos::wait(handles: &[RawHandle],
  out: &mut [IoResult], deadline) -> Result<usize>` that refuses `out.len() < handles.len()`
  (today's private `sys::wait`) and `wait_one(h, deadline) -> Result<IoResult, KError>`
  returning the record only when the kernel returned 1, with TimedOut and WouldBlock as `Err`;
  `po_wait` is then built once. Host-testable under libos's sys mock. Checked on `ns_lookup_wait`,
  `wait_status`, `po_poll` (deadline 0) and inputclient's pump. In libos (exec.rs, sys.rs), or a
  thin `Result` wrapper in libkern beside `channel_recv`. Closes: a result buffer sized by
  convention (the 16-vs-24 smash), a status read from a wait that did not return 1 (th-d8), and
  `IoResult` byte offsets restated 17 times. Depends on: nothing. abi, ownership, soundness,
  opportunities. Value high, effort medium. **Plan**, as rt-o2, in P7.
- [ ] **th-o5** remove: `static mut` as per-call scratch for syscall out-params, argument blocks
  and message buffers — 31 statics (`grep -rn "static mut" userspace/test-harness/src`), 103 of
  the 494 unsafe blocks naming one (81 in harness.rs, 10 inputclient.rs, 6 bootprobe, 3
  restartprobe.rs, 3 stage.rs), 8 `&*(&raw const X)` derefs under 7 `clippy::deref_addrof`
  allows, such as `harness.rs:32`, `harness.rs:1155`, `inputclient.rs:68`, `restartprobe.rs:25`,
  `stage.rs:158`. Stack locals of the exact ABI type (`[IoResult; N]`, `u64`, `Notification`,
  `SpawnArgs`, `ThreadArgs`, `RegisterValues`, a 4096-byte stack message), as pattern.rs:265,
  deadlines.rs:13, exits.rs:20, stage.rs:417 and bootprobe/main.rs:302 already do;
  `STREAM_E0` travels in `ThreadArgs.arg0` (delivered in rcx). Checked on harness `WAIT_*`,
  restartprobe `MSG` and harness `SPAWN_STAGE`. In place, in each bin. Closes: a data race on a
  shared static once a second thread (harness `stream_producer`, `worker_fault`; stage's exit
  threads) calls a helper, an overrun that clobbers the neighbouring static instead of faulting,
  and unenforced 'only this thread touches it' SAFETY claims. Depends on: th-o4 and th-o9 for the
  `WAIT_*` and message statics (smaller once the typed wrappers land). abi, ownership,
  soundness, opportunities. Value high, effort medium. **Plan**, in P7.
- [ ] **th-o6** encapsulate: starting a thread by hand — `SYS_MEMORY_CREATE`, `SYS_MEMORY_MAP`, a
  `ThreadArgs` literal with `_reserved: [0; 36]` and the safe `libos::thread_create`; handle and
  pointer values travel through statics and `AtomicU64`s, while `arg0` is always 0 — 5 sites
  (`grep -rn "thread_create(&"`), all in this crate: `harness.rs:159`, `stage.rs:487`,
  `stage.rs:629`, `bootprobe/exits.rs:242`. An owned `Stack::map(pages)` (never unmapped) with
  `Stack::carve(region, i)` for `exit_spawner`'s pre-mapped block, and `spawn_thread(stack, entry:
  extern "C" fn(u64, u64, u64, u64) -> !, arg0, affinity) -> Result<Handle<Thread, Only>>`
  consuming it; the raw `thread_create` becomes an `unsafe fn` with the stack contract written
  down. Checked on all five sites. In libos (objects.rs). Closes: safe code starting a thread on a
  borrowed or freed stack (th-d1), leaked stack objects and handles, and a thread reading a static
  written after it started. Depends on: nothing for the typed entry and owned stack; for the guard
  half (th-d9), a kernel way to keep an unmapped range below a caller-supplied stack (a reserved
  no-access VMA, or a gap `find_free_range` leaves; Part E). abi, ownership, soundness,
  opportunities. Value high, effort medium. **Plan**, as rt-o6, in P7.
- [ ] **th-o7** remove: private copies of a namespace lookup plus PO wait — 3 helpers, 141 callers
  (`harness.rs:760` `ns_lookup_wait`, 29 callers; `bootprobe/main.rs:89` `ns_lookup`, 111;
  `inputclient.rs:96` `lookup`, 1). `libfs::lookup_wait(ns, path, rights) -> (i32, u64)`, already
  used 5 times in this crate (stage.rs:366/444, pattern.rs:209/220/245), returns the kernel
  status unchanged (so `session_fanout_demo`'s WouldBlock and policy.rs:356's NotFound still
  see it) and every caller already treats handle 0 as failure; it gives `(-1, 0)` where these
  return `(0, 0)` for a handle-less success. Later this becomes the libos typed lookup (th-o1).
  In libfs (exists). Closes: three more hand-parsed `IoResult` copies with their own static wait
  buffers (th-d10). Depends on: nothing. abi, opportunities. Value medium, effort small.
  **Plan**, in P7.
- [x] **th-o8** encapsulate: boot-probe's `Received` strips `libkern::Received`'s guard — 20
  `receive()` callers (`grep -rnE "(^|[^_a-z])receive\(" userspace/test-harness/src/bootprobe |
  grep -v "fn receive\|views_receive\|ChannelReceiver"`), 12 closing `m.handles` by hand and 9
  not, such as `bootprobe/main.rs:363`, `bootprobe/main.rs:331`, `bootprobe/files.rs:570`,
  `bootprobe/storage_admin.rs:131`, `bootprobe/views.rs:457`. Keep the `libkern::Received`
  inside the struct with `take(i) -> Option<u64>` and `count()`, so untaken handles close on
  drop and the 12 `for_each(close)` loops go; `storage_admin.rs:131-136` and `views.rs:457` become
  `take(0)`. Checked on those two and services.rs:29-33. In bootprobe/main.rs, or
  `librsproto::session::recv` if th-o9 lands first. Closes: the leak at th-d3, the class
  `libkern::Received` was written to close. Depends on: nothing. abi, ownership, opportunities.
  Value medium, effort small. **Fix.**
  Done in `009c098c`: `Received` keeps `libkern::Received`, with `take` and `count`; the twelve
  close loops go.
- [ ] **th-o9** encapsulate: IPC messages framed and parsed by hand in raw `[u8; 4096]` buffers
  (payload length at [4..8], handle count at [8], rsproto at [24..]) and sent with a raw
  `SYS_CHANNEL_SEND` — 14 to 15 sites (14 sends by grep; 8 `[4..8].copy_from_slice` header writes,
  one deliberately raw at authority.rs:146; 7 header reads), such as `restartprobe.rs:53`,
  `stage.rs:388`, `bootprobe/main.rs:302`, `harness.rs:2726`; libprompt::ipc::send is a third
  copy outside the crate. A libkern `channel_send(ch, &[u8; IPC_MSG_SIZE], handles: &[u64], mode)`
  (or a page-sized `MsgBuf`) as the twin of `channel_recv`, so the page the kernel copies is part
  of the type, plus librsproto `send(ch, op, request_id, flags, body, handles) -> bool` and
  `recv(ch, &mut [u8; IPC_MSG_SIZE]) -> Result<(Message, Received), i64>`. Checked on
  `stage::rs_send/rs_recv`, bootprobe `rs_send/receive`, restartprobe `send` and harness
  `tty_request`. In libkern (channel.rs) and librsproto (io, session). Closes: a send or receive
  buffer shorter than the page the kernel copies (all 14 are 4096 today by convention; the
  desktop-session-mgr 256-byte receive was this class), header offsets disagreeing with
  `IpcMsgHeader`, and payload slices running past `IPC_PAYLOAD_SIZE` into the handle area.
  Depends on: nothing. abi, opportunities. Value medium, effort medium. **Plan**, as rt-o5, in P7.
- [ ] **th-o10** encapsulate: spawning a program by path by hand (look up the image, fill
  `SpawnArgs`, spawn, then remember to close the image; 4 of 9 sites forget) — 9 sites
  (`grep -rnE 'syscall1\(SYS_PROCESS_SPAWN|[^_]spawn\(&'`), such as `harness.rs:2370`,
  `harness.rs:1355`, `bootprobe/exits.rs:15`, `stage.rs:444`. `spawn_path(ns, path, args:
  SpawnArgs) -> Result<Handle<Process, Only>>` that resolves the image, spawns and closes the
  image on every outcome, as bootprobe `spawn_stage` does by hand; for Tier-1 stages a
  `libstream::setup` wrapper adds the setup channel. Checked on `run_coreutil_in`, `spawn_stage`
  and `run_grants`. In libos beside `spawn` (which already returns an owning `Handle<Process>`).
  Closes: the never-closed spawn images (th-d4, about 56 per run). Depends on: nothing. ownership.
  Value medium, effort small. **Plan**, in P7.
- [ ] **th-o11** encapsulate: DMA buffers built by hand (an `IoOp` submitted raw on a memory
  object that is mapped and read around the I/O, then closed whether or not the wait completed)
  — 8 sites (`SYS_IO_SUBMIT`; three map and read the buffer: `harness.rs:3567`,
  `bootprobe/files.rs:97`, `bootprobe/storage.rs:277`), such as `storage.rs:273`, `files.rs:84`,
  `harness.rs:3549`, `pattern.rs:251`. `IoBuffer { mem, map }` with `read(&mut self, dev, offset,
  len) -> Result<&[u8]>` and `write(&mut self, dev, offset, data) -> Result<usize>` over libos's
  `Handle<Resource,_>::read/write` and `block_on`; the returned slice borrows `self` mutably, so
  none outlives the next I/O, and a failed wait keeps the buffer. Checked on the refusal tests
  through `Op::submit`'s `Err`. In libos. Closes: a reference held over memory a device writes
  (th-d2), and a DMA buffer closed or unmapped while its I/O may still be in flight after a
  failed wait (harness.rs:3568-3578, `RootDevice::drop` after a failed read). Depends on: th-o2.
  soundness. Value medium, effort small. **Plan**, in P7.
- [ ] **th-o12** encapsulate: `librsproto::session::Dir` is closed only by an explicit `.close()`
  on every path (its doc says drop cannot report a failure, but `close()` returns `()`) — 46
  sites (`grep -rnE "Dir::(open|from_endpoint)\("` 46; `grep -rnoE "\.close\(\)"` 46), such as
  `harness.rs:1007`, `bootprobe/files.rs:326`, `harness.rs:3249`. A `Drop` for `Dir` that closes
  its endpoint, with `into_endpoint()` for the one borrowing use (`harness.rs:2941`). In
  librsproto session.rs. Closes: a session left open on an early return, holding one of a
  server's `MAX_WAIT_HANDLES - 2` slots (`session_fanout_demo` shows the table is finite).
  Depends on: nothing. opportunities. Value low, effort small. **Plan**, as sup-o2, in P7.
- [ ] **th-o13** narrow: `unsafe` library calls whose only contract is "this `u64` is a live
  namespace handle I own" — 16 to 17 sites (`grep -rnE "ChannelTransport::(connect|manage)|
  acquire::acquire\(|text::load_(ui|mono)\(|Handle::<[A-Za-z]+, *[A-Za-z]+>::(borrow|from_raw)"`
  16, plus `load_ui`/`load_mono` at uiclient.rs:279/283), such as `uiclient.rs:185`,
  `uiclient.rs:1111`, `inputclient.rs:277`, `display.rs:82`. Take `&Handle<Namespace, impl
  CanLookup>`; each `_start` wraps its bootstrap register once (one unsafe `Handle::borrow`, or a
  libos thin-entry shim per libos.md) and later calls are safe; `acquire` stays unsafe only for
  its real clause, the exclusive writer. Checked on `connect`, `manage` and `load_ui`. In
  libsurface ipc.rs, libdraw text.rs and acquire.rs, and libos. Closes: the copy-pasted SAFETY
  sentence that hides the one clause that is broken (th-d11). A wrong value is a kernel
  InvalidHandle, never UB. Depends on: the libos thin-entry shim for the full form. soundness,
  opportunities. Value low, effort medium. **Plan**, as desk-o4, in P7.
- [ ] **th-o14** remove: inline asm with safe or intrinsic equivalents — 9 sites (`grep -rnE
  "asm!\(\"pause\"|\"cpuid\"|\"xgetbv\""`: 5 `pause` loops, 2 `cpuid`, 2 `xgetbv`), such as
  `stage.rs:42`, `bootprobe/gates.rs:119`, `harness.rs:114`, `stage.rs:65`. `core::hint::spin_loop()`
  (already at exits.rs:212 and stage.rs:527); `core::arch::x86_64::__cpuid_count`, which compiled
  with no `unsafe` on nightly-2026-07-20 in a no_std probe; `_xgetbv(0)` inside a
  `#[target_feature(enable = "xsave")]` fn, where the call stays unsafe but the asm goes; and one
  FP-check module shared by test-stage and boot-probe (the check is duplicated). In `core::arch`
  and `core::hint`, and a shared module in test-harness. Closes: 7 of 9 asm sites and the
  hand-routed `rbx` clobber. Depends on: nothing. opportunities. Value low, effort small.
  **Plan**, in P7.
- [ ] **th-o15** encapsulate: sleeping reimplemented six times (create a timer, arm it, wait,
  close it; or wait with a deadline on a channel or notification that is not meant to signal) —
  6 helpers, 13 callers (`grep -rnE "fn (timer_sleep_ms|timer_nap_ms|nap|sleep_ns)\(|
  syscall4\((libkern::)?SYS_TIMER_SET"`), such as `harness.rs:904`, `stage.rs:127`,
  `bootprobe/registry.rs:7`, `inputclient.rs:510`. `libos::sleep_until(deadline_ns) ->
  Result<()>` over an owning one-shot timer. Checked on harness.rs:904, stage.rs:127 and
  services.rs:58-77. In libos. Closes: static clock scratch read after a failed read, and a sleep
  that ends early (inputclient's `sleep_ns` waits on `notif`, which its own `wait_event_before`
  at :496 says is never drained). Depends on: th-o3 (`clock_now`). opportunities. Value low,
  effort small. **Plan**, in P7.
- [ ] **th-o16** remove: kernel snapshot objects (`/proc/sched/stats`, `/proc/self/status`,
  `/initramfs/etc/init.toml`) mapped at exactly one page whatever size the kernel reports, read
  as a 4096-byte slice and unmapped by hand — 6 sites (`grep -rnE "SYS_MEMORY_MAP, mem, 0,
  (PAGE|4096), RIGHT_MAP_READ\)"`), such as `bootprobe/gates.rs:194`, `harness.rs:2532`,
  `harness.rs:3486`, `harness.rs:864`. `libfs::read_file(ns, path)` (used at pattern.rs:144; maps
  the stat'd size and copies out) with `libfs::unpadded`, or boot-probe's `read_all`
  (main.rs:393). In libfs (exists). Closes: a snapshot outgrowing one page being parsed truncated
  (`idle_cpus` would undercount and fail `dead_log_source_demo` falsely at about 40 online CPUs;
  `MAX_CPUS` is 8 today) and the leaked initramfs mapping. Depends on: nothing. abi. Value low,
  effort small. **Plan**, in P7.
- [ ] **th-o17** remove: kernel error discriminants copied as literals where
  `KError::X.as_i32()` exists (harness.rs:22 already does it for TimedOut) — 3 sites (`grep -rn
  "const KERR_\|== -[0-9]\+\b"`): `harness.rs:636` (`status2 == -10`), `harness.rs:2999`
  (`KERR_WOULD_BLOCK = -11`), `harness.rs:3134` (`KERR_UNSUPPORTED = -52`). Use
  `KError::NotFound.as_i32()`, `WouldBlock`, `Unsupported`. In libkern::error (exists). Closes:
  silent drift of an ABI discriminant in a test that would then assert the wrong refusal;
  abi-sync-check holds libkern to the kernel but not these copies. The three literals match
  today. Depends on: nothing. abi. Value low, effort small. **Later.**

What held up: Every `SYS_WAIT` (39 sites) passes count 1 with a result buffer of at least 24
bytes, and the 16-vs-24 `HandleInfo` smash at harness.rs:798 has not come back; every kernel
out-param is the exact ABI type (HandleInfo, Notification, RegisterValues, clock `u64`, entropy
reads within `ENTROPY_READ_MAX`). All 14 `SYS_CHANNEL_SEND` buffers are whole 4096-byte pages,
handle arrays carry at most 2 handles, and receives go only through `channel_recv` or
`channel_recv_raw` (check-recv holds). Payload slices clamp to `4096-24` at
harness.rs:2759, restartprobe.rs:120, inputclient.rs:147/192, stage.rs:417 and
bootprobe/main.rs:361; `forward_demo`'s unclamped index at :3760 is bounds-checked, so a bad
length panics rather than overruns. Every `from_raw_parts` length equals the mapped length
(`sys_memory_map` refuses a size past the object's, table.rs:898), and the raw byte loops stay
below the mapping. `RootDevice::read_at` holds (a host copy over offsets 0..8191 and 11 lengths
passed; dropping the `PAGE - intra` bound fails), and `read_all` and `block_demo` copy out before
they unmap. `libkern::Received` closes on drop at the harness, stage, inputclient and restartprobe
sites. Spawn is all-or-nothing, so the post-failure closes at exits.rs:105-108, 137-141, 181-185
and received.rs:234-236 are right; `sys_ns_bind` clones its resource reference, so the closes
after bind are right; `sys_memory_unmap` ignores size, so the `unmap(addr, 0)` calls do unmap.
`block_send_demo` can reuse `MSGBUF` while its send is pending, since the kernel copies into a
bounce buffer at submit (table.rs:1978). Cross-process counters (stage.rs:562/570/641,
exits.rs:82) are `AtomicU64` on page-aligned mappings never unmapped under their users, and the
second-thread stacks are never unmapped. Every `static mut` is touched only by its process's
main thread (the second threads use stack buffers or one pre-written static), so there is no
data race today, and libheap's spinlock covers the two allocating threads. The cpuid/xgetbv asm is
correctly scoped (`xgetbv` only after OSXSAVE, `rbx` routed through `rsi`, AVX2 fns called only
after the feature check), and the `enter_user` stack-top bias makes the callers' `base+size`
stacks correct. Library parsers reached from here hold their bounds (libsurface `poll_event`,
librsproto `round_trip` and `Dir::open`, libstream chunks, `Table::decode`, `BlockDeviceInfo`,
`InputEvent`). The hand-copied `KError` literals and the `ChildExited` `body[8..12]` parse match
the kernel and notification-format.md. uiclient's pixel stores satisfy `y*pitch + x*4 + 4 <=
len`. Deferred and not counted: TODO(svc-auth-ungated) at bootprobe/policy.rs:312. The only break
a test caught is th-d2, by Miri on a model (th-d1 and th-d9 by a compile check and a kernel host
test; none of the probes was left in place). No host test reaches any unsafe in this crate; it is
bins only, with no lib target, so the proposed abstractions become host-testable only behind
libos's sys mock.

### Found reviewing PR #400 — branch pre-phase-7/part-i-survey at ff6de3a3, 2026-10-27

Two holes in libos the runtime's run missed, found by the review's error-paths lens and shown
again by the author with the libos mock made faithful to the kernel, where a handle that does not
exist fails the whole wait. Both probes were removed.

- [x] **rt-d12** `userspace/libos/src/exec.rs:122` — `Op::poll` takes any `sys_wait` return below
  1 as "not ready", errors included, and `block_on` (:171) ignores its blocking wait's result, so a
  wait that keeps failing is polled again for ever instead of returning its error. A PO handle
  closed by a stale close elsewhere (the shape of rt-d8) makes every wait on it fail at once with
  InvalidHandle, and `block_on` spins. In a supervisor that keeps the run queue from ever emptying,
  which stops deferred handle reclamation for the whole machine. `desktop-session-mgr` reaches
  `block_on` today through `libdraw::text::load_ui` and `libsurface::screen::read`. Demonstrated:
  a host probe ran an `Op` whose PO was then closed under `block_on`, through a wrapper that gives
  up after 100,000 polls; it was still re-polled after 200,000 `sys_wait`s. The control, without
  the close, completed. Fix: `Ready(Err)` for any negative result but WouldBlock, and a failed
  blocking wait ends `block_on`. error-paths. Medium. **Fix.**
  Fixed in `d22ea05c` and `fae8cbf5`: only a poll's `WouldBlock` is "not ready", any other wait
  error completes the `Op`, and a failed blocking wait sleeps before the next round. Fail without
  them: `an_op_whose_wait_fails_completes_with_the_error`,
  `a_failed_wait_sleeps_before_the_next_poll`.
- [x] **rt-d13** `userspace/libos/src/exec.rs:48` — `Reactor::register` drops every handle past the
  eighth ("only costs an extra poll round"), but `block_on` then waits on the eight it kept with no
  deadline, so a future pending on nine or more operations, where only a dropped one will signal,
  blocks for ever. Latent: libos has no join, so only a hand-written future polling more than eight
  `Op`s reaches it; the multi-task executor `libos.md` plans would. Demonstrated: a host probe
  polled nine pending `Op`s under `block_on`, and the first blocking wait was given eight handles,
  not the ninth. Fix: when a round drops a registration, wait with a deadline of zero, so the
  dropped operations are polled again. error-paths. Low. **Fix.**
  Fixed in `fae8cbf5`: a round that left a handle out waits until a deadline 1 ms off. Fails
  without it: `a_ninth_operation_is_polled_again`, through a mock that panics on a wait for
  ever nothing will end.
