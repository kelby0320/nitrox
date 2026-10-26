# The safety survey: defects, opportunities and their triage

**Status: planned 2026-10-26 (the maintainer's call); no subsystem surveyed yet.** Part I of
[`pre-phase-7.md`](pre-phase-7.md) fills it: each `/safety-survey` run, a section a subsystem.

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

- [ ] **kobj-d1** `kernel/src/handle/table.rs:417` — `HandleTable::lookup` step 7 bumps the
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
  needs). abi/ownership/soundness/concurrency/opportunities reviewers all found it. High.

- [ ] **kobj-d2** `kernel/src/syscall/table.rs:1971` — A handle move is not atomic, so two
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
  kobj-o5. abi/ownership. High.

- [ ] **kobj-d3** `kernel/src/sched/ipc.rs:481` — When a userspace server dies, outstanding
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

- [ ] **kobj-d4** `kernel/src/syscall/table.rs:491` — `sys_process_spawn` commits (the child is
  running) and then returns `Err` if the parent's Process-handle `allocate` fails, before the move
  loop at `:501-505`. Every handle the caller asked to move stays in the parent while the child
  holds it too, so a move becomes a copy, and the parent loses its only handle to the running
  child. The doc comment at `:328-330` promises that moves close the source only after the spawn
  commits. A process fills the global table (no per-process quota: `sys_handle_duplicate` until
  `OutOfHandles`) so that the child-side allocations succeed and the parent's does not, then spawns
  with `move_mask` set on a `TRANSFER`-only handle. Unverified by a boot. Demonstrated by reading
  the path, which is deterministic with no race (`:484-499`). A host test needs a current-thread
  harness that `syscall/table/tests.rs` lacks, or a boot-probe that fills the table and spawns
  with a move. ownership. Medium.

- [ ] **kobj-d5** `kernel/src/handle/grace.rs:48` — `GraceTracker::enter_read` announces the
  reader with a Release store followed only by an Acquire fence. On x86-TSO nothing orders that
  store before the later load of `entry.object` (`table.rs:403`), which is a Dekker-style
  protocol, so `drain_expired` can see the context as quiescent while a reader still holds the
  slot. Today the effect is masked: a recycled slot fails the reader's seq or generation check at
  steps 8-9. The natural fix for kobj-d1, deferring the last release to `is_grace_period_past`,
  would rely on this tracker and would still let a reader dereference a freed object. It needs a
  SeqCst fence (or a locked op) after the store. Unverified. A loom or herd7 model of `enter_read`
  against close plus `drain_expired` would settle it. The writer side already gets a full barrier
  from the rank-3 spinlock. ownership. Medium.

- [ ] **kobj-d6** `kernel/src/mm/user_access.rs:177` — `copy_from_user<T: Copy>` and `copy_to_user<T:
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
  way. The probes were removed. See kobj-o6. soundness/opportunities. Medium.

- [ ] **kobj-d7** `kernel/src/syscall/table.rs:1202` — `sys_ns_bind`'s test for an existing
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
  ownership/concurrency. Medium.

- [ ] **kobj-d8** `kernel/src/drivers/mod.rs:197` — `pub(crate) fn adopt<T>(obj: KBox<T>, ty:
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

- [ ] **kobj-d9** `kernel/src/syscall/table.rs:611` — `sys_thread_get_registers` takes the
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

- [ ] **kobj-d10** `kernel/src/libkern/spinlock.rs:91` — `SpinLockGuard` (and `IrqSpinLockGuard`,
  `:249`) get auto `Send` and `Sync` from `&'a SpinLock<T>`, so a guard over a `Send + !Sync` value
  such as a `Cell` is `Sync`, and a guard can be dropped on another CPU than the one whose preempt
  count and lock rank it raised (std's `MutexGuard` is `!Send`, and `Sync` only for `T: Sync`).
  Latent: nothing shares a guard across CPUs today. A scoped-spawn host test or a future helper
  that hands `&guard` to a worker gets a data race in safe code. Demonstrated: a compile probe
  asserting `is_sync`/`is_send` on both guards over `Cell<u32>` passed. A second probe, run under
  Miri as `probe_guard_shares`, reported "Data race detected" in `Cell::get`. They were removed.
  Fix: `unsafe impl<T: Sync> Sync` and a `PhantomData<*const ()>` marker for `!Send`.
  ownership/soundness. Low.

- [ ] **kobj-d11** `kernel/src/sched/current.rs:31` — Shared `&Thread` is formed in 13 places
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
  opportunities. Low.

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

- [ ] **kobj-d13** `kernel/src/object/memory_object.rs:203` — `MemoryObject::copy_to_kvec` (and
  `FileObject::read_to_kvec`, `file_object.rs:1043`) forms `&[u8]` over frames while a writable
  user mapping of the same object may store to them on another CPU, so the SAFETY claim "not
  aliased" is false. A parent maps its image `MAP_WRITE` and writes while another thread runs
  `sys_process_spawn(image)`. There is no TOCTOU, since the loader parses the copy: only torn
  image bytes and the abstract-model race remain. Demonstrated under Miri: `probe_copy_to_kvec_vs`
  (with `-Zmiri-permissive-provenance`) reported "Data race detected between (1) retag read ... and
  (2) non-atomic write". It was removed. Fix: `ptr::copy_nonoverlapping` into the `KVec`'s spare
  capacity (see kobj-o11). soundness/opportunities. Low.

- [ ] **kobj-d14** `kernel/src/handle/global.rs:72` — `global::get()` is a safe `pub fn` that
  returns `&'static HandleTable` via `assume_init_ref` and checks `READY` only with `debug_assert`
  (`:73`, `:78`). A release build calling it before `init()` forms a reference to uninitialised
  memory. Latent: only `main.rs` (after `init`, `main.rs:231`) and syscall handlers call it. Early
  boot code that allocates a handle before `handle::global::init` (for example from
  `entropy::init`, `clock::init`, `dpc::init` or `IrqRouter::init`) would use the zeroed table, with
  a zero-capacity defer ring that `close()` treats as permanently full. Unverified. A release-
  profile host test calling `get()` before `init()` would settle it. Fix: check `READY`
  unconditionally, one load. soundness. Low.

- [ ] **kobj-d15** `kernel/src/handle/entry.rs:120` — The seqlock `WriteGuard` publishes the odd
  seq with a Release store but has no release fence before the Relaxed field stores that follow.
  Release orders only operations before the store, so on a weakly ordered CPU a reader can see new
  fields with the old even seq on both sides and accept a torn snapshot. Latent until the aarch64
  port, since x86 TSO keeps stores in order. Standard writer: `store(s+1, Relaxed); fence(Release);`
  field stores; `store(s+2, Release)`. Unverified. A litmus test on aarch64, or Miri's weak-memory
  emulation on a two-thread writer/reader test, would settle it. opportunities. Low.

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
  soundness/concurrency/opportunities. Value high, effort medium.

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
  concurrency. Value high, effort medium.

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
  large.

- [ ] **kobj-o4** encapsulate: host tests and Miri cannot see use-after-free of kernel objects.
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
  opportunities. Value high, effort small.

- [ ] **kobj-o5** encapsulate: a handle move is a lookup with `TRANSFER`, a commit into a ring,
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
  fits best with kobj-o2. ownership. Value high, effort medium.

- [ ] **kobj-o6** encapsulate: plain `#[repr(C)]` ABI data is reinterpreted by hand — per-type
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
  soundness/opportunities. Value medium, effort small.

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
  medium.

- [ ] **kobj-o8** encapsulate: bounded lists pushed under SCHED rely on a `try_reserve` at
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
  medium, effort small.

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
  Value medium, effort small.

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
  one reviewer), effort small.

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
  Depends on: nothing. abi. Value low, effort small.

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
  Depends on: nothing. opportunities. Value low, effort small.

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
