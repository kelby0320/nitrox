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

- [ ] **sched-d1** `kernel/src/sched/exit.rs:400` — `thread_exception_frame` returns the raw
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
  abi, ownership, soundness, concurrency, opportunities. Medium.

- [ ] **sched-d2** `kernel/src/sched/ipc.rs:527` — Outstanding page-cache fills
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
  opportunities. Medium.

- [ ] **sched-d3** `kernel/src/dpc.rs:71` — The safe `pub fn enqueue(dpc: &Dpc)` takes a borrow of
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
  passes under Miri. abi, ownership, soundness, concurrency, opportunities. Medium.

- [ ] **sched-d4** `kernel/src/arch/x86_64/idt.rs:663` — An NMI (vector 2, per-CPU IST2 stack) that
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
  it follows from the shared IST2 top. Probes reverted. concurrency. Medium.

- [ ] **sched-d5** `kernel/src/dpc.rs:98` — The DPC queue is reserved to 64 on the premise that
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
  Medium.

- [ ] **sched-d6** `kernel/src/sched/exit.rs:437` — `reap_pending` is the only drain of the global
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
  Medium.

- [ ] **sched-d7** `kernel/src/sched/ipc.rs:312` — `us_server_attach` overwrites an endpoint's
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
  reg_a's lookup left unsignalled at server death. abi, ownership. Medium.

- [ ] **sched-d8** `kernel/src/libkern/spinlock.rs:91` — `SpinLockGuard` (and `IrqSpinLockGuard` at
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
  abi, ownership, soundness, concurrency, opportunities. Low.

- [ ] **sched-d9** `kernel/src/object/thread.rs:716` — `Thread::fpu_area_ptr` derives the
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
  soundness. Low.

- [ ] **sched-d10** `kernel/src/sched/exit.rs:50` — Whole-struct `&Thread` is formed under SCHED
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
  (2) retag read of type IpcChannel ... drop_in_place". concurrency. Low.

- [ ] **sched-d12** `kernel/src/object/ipc_channel.rs:283` — `IpcChannel::try_new_pair` takes each
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
  Low.

- [ ] **sched-d13** `kernel/src/dpc.rs:107` — `run_pending_in` drains into `[0usize; DPC_RESERVE]`
  with `buf[..n].copy_from_slice(&q)`, where `n` is the queue's length. The length is bounded by the
  KVec's capacity, not by `DPC_RESERVE`. The two agree only because `try_reserve` from an empty KVec
  allocates exactly the amount requested. Changing KVec growth to round up to an allocator bucket,
  or reserving the queue larger in `init`, makes a drain with more than 64 distinct DPCs queued
  panic at an interrupt tail with an out-of-range slice. Fix: take the buffer size from the queue's
  type (sched-o4) or drain in chunks.
  Demonstrated by a host test (since removed): a fresh queue reserved to `DPC_RESERVE+1` (capacity
  128) and 65 distinct Dpcs queued panics at `dpc.rs:111`, "range end index 65 out of range for
  slice of length 64". In production `init` reserves exactly 64 from empty. ownership. Low.

- [ ] **sched-d14** `kernel/src/sched.rs:368` — `alloc_pid` uses `next_pid.wrapping_add(1)`, so
  after 2^32-2 spawns pids come round to 0, 1, 2, and so on. `sched.rs:238` says "No pid reuse" and
  `docs/architecture/handle-system.md:343` treats pid reuse as no hazard, yet handle ownership is
  keyed solely by `owner_pid` (`handle/global.rs`). After about 4.29 billion
  `sys_process_spawn` calls a child gets pid 0, whose handles `reap_pending` never sweeps
  (`exit.rs:470` skips pid 0), then pid 1 (the boot parent) and other live pids. A child sharing a
  live process's pid passes that process's `owner_pid` checks and gains its authority. Fix: refuse
  to spawn at exhaustion, or widen to u64.
  Demonstrated by a host test in `sched/tests.rs` (since removed): `SCHED.next_pid = u32::MAX`, then
  `alloc_pid()` four times gave 0xffffffff, 0, 1, 2 ("the pid source wrapped"). The spawn count
  needed keeps it low. abi. Low.

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
  it, request x2APIC in `limine.rs` and boot that topology. concurrency. Low.

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
  Value high, effort large.

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
  large.

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

- [ ] **sched-o4** narrow: `dpc::enqueue(&Dpc)` is safe for any lifetime, though statics need
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
  medium, effort small.

- [ ] **sched-o5** encapsulate: lists pushed under SCHED or at an interrupt tail are plain KVecs
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
  opportunities. Value medium, effort small.

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
  opportunities. Value medium, effort small.

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
  (the token). opportunities. Value medium, effort medium.

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
  Value medium, effort small.

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
  medium, effort small.

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
  opportunities. Value low, effort small.

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

- [ ] **kmem-d1** `kernel/src/mm/kvmap.rs:58` — `vmap_alloc_pages` is bounded only by
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
  opportunities. High.
- [ ] **kmem-d2** `kernel/src/mm/addr_space.rs:617` — `map_object`'s out-of-memory rollback
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
  Lens: concurrency. High.
- [ ] **kmem-d3** `kernel/src/mm/slab.rs:334` — `kmalloc` accepts `align == SLAB_SIZE` (it refuses
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
  soundness, opportunities. Medium.
- [ ] **kmem-d4** `kernel/src/mm/kstack.rs:197` — `KernelStack` stores the `PhysAddr` root it was
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
  concurrency, opportunities. Medium.
- [ ] **kmem-d5** `kernel/src/tlb.rs:125` — the shootdown publishes `REQUEST_ALL`, `REQUEST_VA`
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
  Medium.
- [ ] **kmem-d6** `kernel/src/mm/vmm.rs:245` — safe functions cast an `ObjectRef` to a concrete
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
  memory read as `MemoryObject.frames`). Lenses: soundness, opportunities. Medium.
- [ ] **kmem-d7** `kernel/src/mm/user_access.rs:177` — safe `copy_from_user<T: Copy>`
  `assume_init`s user bytes as `T`, but `Copy` does not make every bit pattern valid (`bool`,
  enums, `char`, `NonZero`, references), and `copy_to_user<T: Copy>` copies padding out to user
  memory. Latent: the 3 `copy_from_user` and 8 `copy_to_user` callers use integer-only,
  padding-free types. A `bool` in `ThreadArgs` or an enum in `IoOp` is UB, and a padded struct
  leaks kernel stack bytes.
  Demonstrated: Miri (nightly-2026-07-20) on a probe test, `copy_from_user(UserPtr::<bool>)`
  over a byte holding 2, reports UB at user_access.rs:199 `out.assume_init()`. Lens: ownership.
  Medium.
- [ ] **kmem-d8** `kernel/src/sched/ipc.rs:527` — when a userspace server's endpoint closes,
  `ipc_endpoint_closing` (and `us_reg_detach`, line 281) fails every pending lookup but never
  touches `pending_fill`, so a Model B page-cache fill's PO is never completed. `wait_status`
  (file_object.rs:1177) re-waits on anything but Signaled, process exit included, so the
  faulting thread is stuck in the kernel for ever and its process is never reaped.
  `PendingFill` holds the FileObject, whose producer holds the registration, which holds
  `pending_fill`: the cycle leaks them all. Reachable through a trusted server only, and the
  kernel builds `Producer::FsServer` for any non-block resolve reply (table.rs:2398).
  Demonstrated: a host test in the userspace_server tests (an IpcChannel pair, one lookup and
  one fill outstanding, server end dropped). Lookup PO signalled `true`, fill PO signalled
  `false`. Lenses: soundness, opportunities. Medium.
- [ ] **kmem-d9** `kernel/src/object/file_object.rs:790` — `mark_ready` (called by the Model B
  fill reply, table.rs:2703) readies whichever page now has the index, while `settle` was made
  to match by PO for this reason. A stale fill reply can mark a successor page Ready while its
  own fill is pending. Page i fills (frame A, PO1), a resize retires it, a regrow and a fault
  reserve i again (frame B, PO2). PO1's reply readies B and a third faulter maps it. PO2 then
  fails and `settle` frees B, which is still mapped in user space: a use-after-free. Latent:
  only Model A objects are resized today.
  Demonstrated: a host probe in the file_object tests ran that sequence. It reported "mapped
  PhysAddr(125966602964992); settle freed 1 frame(s); lookup(2) = None". Lenses: ownership,
  opportunities. Medium.
- [ ] **kmem-d10** `kernel/src/drivers/mod.rs:197` — `pub(crate) fn adopt<T>(obj: KBox<T>, ty:
  KObjectType) -> ObjectRef` is safe but wraps the unsafe `ObjectRef::from_raw`, so any box
  under any tag becomes an `ObjectRef` whose last drop runs `dispatch_destroy` for `ty`. The
  private copy at io/mod.rs:161 and test copies (sched/ipc.rs:560, drivers/input/mod.rs:290) are
  the same. A mis-tagged call among the 30-odd sites compiles with no `unsafe`, and the last
  close destroys the allocation as the wrong type.
  Demonstrated: a host probe compiled `adopt(MemoryObject::try_new(PAGE).unwrap(),
  KObjectType::Timer)` with no `unsafe` and printed "adopted as Timer". The drop was not run.
  Lens: opportunities. Medium.
- [ ] **kmem-d11** `kernel/src/mm/heap.rs:45` — `heap::buddy_free`, `BuddyAllocator::free`
  (buddy.rs:182) and `BuddyPager::free` are safe fns that write the free-list link through the
  HHDM and hand the frame out again. Frame ownership is held by convention at 18 hand-written
  free sites. A double free, or a free while a mapping or DMA still uses the frame (the shape of
  the early free fixed in #396), corrupts memory with no `unsafe` at the call site.
  Demonstrated: a buddy host probe on a local allocator freed a live frame with the safe call.
  The owner's first word read 0x0 and the next `alloc` returned the same frame. Lens:
  opportunities. Medium.
- [ ] **kmem-d12** `kernel/src/mm/addr_space.rs:458` — `map_file_page` re-checks only that some
  FileBacked VMA of the same object covers `addr`, not that `addr` still maps to the page index
  `file_backing` computed in an earlier hold of the AS lock. Thread A faults at 6P in F mapped at
  [4P,8P) (page 2) and parks on the fill. B unmaps and maps F at [5P,9P), where 6P is page 1. A's
  `map_file_page(6P, F, frame2)` returns true and page 2's frame sits where page 1 belongs. There
  is no use-after-free (the frame is F's own), but the process sees and writes back the wrong
  page. The same window also makes a fault on a remapped region fatal where a retry would work.
  Demonstrated: a host test in addr_space/tests.rs ran those steps. `map_file_page` returned
  true and `translate(6P) == frame2` (a second probe asserts the equivalent with frame 1 at 5P).
  Lenses: abi, ownership, concurrency. Low.
- [ ] **kmem-d13** `kernel/src/object/file_object.rs:598` — write-back takes a page's device
  position from its first block and writes `pages * PAGE_SIZE` contiguously from there, and
  `block_map` (table.rs:2622) accepts block sizes under a page without refusing a run boundary
  inside a page. The recorded `TODO(ext4-subpage-runs)` covers the fill from the wrong blocks. It
  does not say that write-back overwrites the blocks after the page's first run, which may
  belong to other files or metadata. For example, an ext4 with 1 KiB blocks and a file whose
  extent ends inside a page: `sys_file_sync` writes 4 KiB from the first block, and 3 KiB land on
  whatever follows on the device.
  Unverified: the path is pointed at. A host test of `write_runs` with runs [file 0..2 -> lba
  100, file 2..4 -> lba 500] at block_size 1024 should issue one 4096-byte IRP at byte 100*1024.
  Lens: abi. Low.
- [ ] **kmem-d14** `kernel/src/arch/x86_64/idt.rs:258` — copy-primitive recovery runs before any
  demand fault-in, so the kernel never populates a valid but lazily backed user page (an untouched
  stack page or an untouched FileBacked page). The copy fails with `Fault`, and `sys_channel_recv`
  has already popped the message, so the message is destroyed and its transferred handles are
  closed. The decision log records it as a corollary, but `deferred-decisions.md` has no entry and
  the page cache now exists. No known caller hits it, because today's callers touch pages first.
  Demonstrated by reading the code, not by a boot: `pf_dispatch` returns at
  `lookup_recovery(f.rip)` before `try_fault_in`, and there is no prefault step. A boot test
  receiving into an untouched page of a fresh file mapping would show it. Lens: abi. Low.
- [ ] **kmem-d15** `kernel/src/arch/x86_64/idt.rs:258` — recovery matches the faulting RIP alone,
  and `copy_bytes_raw` registers `rep movsb` for both directions, so a fault on the kernel-side
  pointer is also turned into `UserAccessError::Fault` instead of `dump_and_halt`. A dangling
  bounce buffer or a bad length past a kernel buffer is hidden and reported as a user error.
  Unverified: reasoned from code (`lookup_recovery_in` compares only `fault_pc`). Check
  `read_cr2() < USER_VIRT_END` before patching RIP, and boot with a deliberately bad kernel-side
  pointer. Lens: abi. Low.
- [ ] **kmem-d16** `kernel/src/arch/x86_64/cpu.rs:190` — the kernel sets and checks SMAP and SMEP
  but never sets or checks CR0.WP. That `copy_to_user` cannot write a read-only user mapping
  holds only because Limine sets WP on the BSP and the APs. With WP clear, a process holding a
  read-only mapping of a shared page-cache page could have the kernel write into it through
  `sys_clock_read` or `sys_channel_recv` out pointers.
  Unverified: grep finds no `write_cr0` that sets bit 16. Assert `read_cr0() & (1<<16) != 0` on
  every CPU at bring-up. It holds under Limine today. Lens: abi. Low.
- [ ] **kmem-d17** `kernel/src/mm/kvmap.rs:111` — `remap_mmio_page` repoints a page of the shared
  vmap and flushes only the local TLB, and its `# Safety` contract does not say that the page
  must have been used by one CPU only. Safe today because its only caller, `pci::enumerate`
  under `device::init` (main.rs:239), runs before `bring_up_aps` (main.rs:283). A rescan or
  hotplug after SMP start, with the thread migrating, would read the previous function's config
  space through a stale entry. `shootdown_page` has no callers at all.
  Unverified: path pointed at (`unmap_page`, `map_page`, `flush_tlb_page`, no `shootdown_page`).
  Lenses: ownership, soundness, concurrency. Low.
- [ ] **kmem-d18** `kernel/src/object/memory_object.rs:203` — `copy_to_kvec` forms a `&[u8]` over
  each frame under a SAFETY comment saying it is "not aliased", but MemoryObject frames are
  mapped into user spaces by design. `file_object.rs:1043` (`read_to_kvec`) does the same over
  cache frames, and `DmaBuffer::as_slice` and `as_mut_slice` (mm/dma.rs:78, 84) hand out
  references to memory a device writes. A process mapping a MemoryObject writable and writing it
  from one thread while another passes it as a spawn image gives the kernel a shared reference to
  changing bytes, a data race in Rust's model.
  Unverified: Miri cannot tell this from a raw copy. Copy with `ptr::copy` from raw pointers (or
  the user-copy asm), never forming the reference, and correct the SAFETY comments.
  Lenses: soundness, concurrency. Low.
- [ ] **kmem-d19** `kernel/src/object/memory_object.rs:106` — `try_new_borrowed`'s `# Safety` does
  not require the range to be HHDM-mapped, but safe accessors (`copy_in` at line 174,
  `copy_to_kvec` at 192) and the I/O paths dereference every frame through the HHDM, and
  `sys_process_spawn` (table.rs:360) and `sys_io_submit` (table.rs:1646) accept a Borrowed
  object. Today the only borrowed range is the framebuffer, which Limine maps in the HHDM. A
  second aperture (a PCI BAR) would let a holder aim device DMA at registers or make `copy_in`
  take a kernel #PF.
  Unverified: grep finds no `FrameOwnership` check outside memory_object.rs. A host test handing
  a Borrowed object to `sys_io_submit`'s validation, or refusing Borrowed buffers there, would
  settle it. Lenses: soundness, opportunities. Low.

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
  (one lens said large).
- [ ] **kmem-o3** encapsulate: kernel-half (vmap) mappings are made through
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
  soundness, concurrency. Value high, effort small to medium.
- [ ] **kmem-o4** encapsulate: each page-table unmap returns its old frame, and the caller is
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
  concurrency. Value high, effort medium.
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
  medium.
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
  best with kmem-o2. Lens: opportunities. Value medium, effort medium.
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
  Value medium, effort medium.
- [ ] **kmem-o8** narrow: `copy_from_user`/`copy_to_user` are generic over `T: Copy`, and ABI
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
  marker trait. Lens: abi. Value medium, effort medium.
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
  medium, effort small.
- [ ] **kmem-o11** narrow: x86_64 page-table walks dereference `*table_ptr(x).add(index)` raw at
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
  opportunities. Value low, effort small.

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
