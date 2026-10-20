# The bug bash: findings and their triage

**Status: started 2026-10-14; five findings untriaged, three from Part C's detail pass, one from
its PR's CI and one from building C.1.** Part D of
[`pre-phase-7.md`](pre-phase-7.md) fills it: the two bugs found on the laptop, what PR #365's two
trial reviews found that `main` still has, and each `/bug-bash` run, a section a subsystem.

## How a finding is kept

The author appends each run's findings as its report gives them, a checkbox each:

- `[ ] **ID** file:line — the defect in a sentence. Scenario. Evidence. Lens. Severity.`

Each run's findings come as one section, headed with the subsystem and the commit of `main` it
hunted, in the shape `.claude/skills/bug-bash/report.md` gives.

The ID is the subsystem's short name and a number, such as `kobj-3`. Triage, with the maintainer,
adds one word after the severity: **fix** (in Part D), **later** (with a `TODO(tag)` and its
`deferred-decisions.md` entry), or **no** (with the reason). A finding is reproduced before it is
fixed, and its box is ticked with the commit that fixed it and the test that fails without it.

## Findings

### Found detailing Part C — main at 226a54f6, 2026-10-19

These were found by the author while measuring Part C, not by a run. No lens hunted, and none
was demonstrated: each is read from the source, and is reproduced before it is fixed.

- [ ] **desk-1** `userspace/compositor/src/input.rs:449` — a press on a maximized window's title
  bar that moves at all snaps it.
  - Scenario:
    - at 1360×768, a maximized window's title bar is rows 31–61;
    - the top snap zones' trigger band is rows 30–53 (`desktop-shell/src/placement.rs:28-41`);
    - a move starts on the press, with no threshold (`input.rs:449-461`);
    - a zone is entered on any motion (`:605-623`).
  - So a click with a pixel of jitter snaps the window to the top half, or to a quarter at either
    end of the band, and the shell drops its restore rectangle
    (`desktop-shell/src/main.rs:749-752`).
  - After Part C's C.3 the top zone maximizes instead, but the corners still quarter the window.
  - Unverified: a press, a one-pixel motion and a release over QMP on a maximized `nxterm` would
    settle it. Logic. Medium.
- [ ] **nxsh-1** `userspace/nxsh/src/eval.rs:636` — assigning to an existing `$env` field changes
  the `$env` binding alone: a spawned program still gets the old value, and the next `cd` reverts
  the binding.
  - Scenario, with `PWD=/` and `HOME=/home/a`: after `$env.HOME = "/x"`, `$env.HOME` reads `/x`,
    but a program then run gets `HOME=/home/a`; after `cd /tmp`, `$env.HOME` reads `/home/a`.
  - An assignment updates the binding's slot (`:636-655`). `cd`, the prompt and every spawned
    stage read the interpreter's own record instead (`cwd()` `:203`, `env_str` `:1328`,
    `host.run(…, &self.env)` `:1680`). Only `set_env_entry` (`:230`) updates that record, and it
    republishes the record over the binding (`rebind_env`, `:220`).
  - That contradicts `rebind_env`'s promise that "the binding and the record never disagree"
    (`:219`), and the field's doc (`:141-144`), which says assigning to `$env` needs nothing beyond
    field access. A field the record lacks cannot be assigned at all: `$env.FOO = "x"` fails with
    "record has no field `FOO`" and runs nothing.
  - Demonstrated by PR #385's review, and again by a temporary host test: `/x` from `$env.HOME`,
    `HOME=/home/a` in the stage's environment, `/home/a` after a `cd`, and the new field refused.
    Logic. Medium.
- [ ] **nxsh-2** `userspace/nxsh/src/eval.rs:1268` — `cd` and `exit` each read their first
  positional argument and ignore the rest.
  - `cd a b` goes to `a`, and `exit 1 2` exits with 1, each saying nothing, against the shell's
    fail-loud rule.
  - `` `cd` takes one path `` fires only for an argument that is not positional (`:1280`), and so
    does `exit`'s check (`:1293-1318`).
  - Demonstrated by a temporary host test: `cd /a /b` left `PWD` at `/a`, and `exit 1 2` exited
    with 1, neither with an error. Logic. Low.

### Found on PR #385's CI — main at 226a54f6, 2026-10-19

A gate failed on a branch that changes only docs. It is the gate's own defect.

- [ ] **gate-1** `tools/xtask/src/guest_input.rs:30` — `click_at` counts a press that reached the
  guest off its exact coordinates as a miss, and discards the transcript, though that press may
  have hit its target and acted.
  - Scenario: `check-login --kvm`'s unsaved-buffer step (`gates/login/dialogs.rs:114`):
    - the press at the dialog's Discard button arrived at x=266 rather than 223, still on the
      button, and `nxedit` discarded and closed;
    - `click_at` matches only `press at x=223 y=448` (`:17`), so it called `skip_to_end` (`:30`),
      which consumed `nxedit: discarding the unsaved buffer`;
    - it clicked twice more, on the Files window behind;
    - the step then waited 45 s for the line already consumed (`dialogs.rs:115`).
  - Its doc comment (`:5-9`) names a retry's hazard for the bar's toggles, not for a button that
    ends a dialog.
  - Demonstrated: the transcript of PR #385's CI, run 39291063887, job 118191523426. Tests.
    Medium: it fails CI on branches that touch none of it.

### Found building Part C.1 — branch pre-phase-7/part-c1-paths at 2116dbcf, 2026-10-19

One flake on a branch that changed only the shell and `rename`, in a path it does not touch.

- [ ] **usb-1** `tools/xtask/src/usb.rs:120` — a USB keyboard hot-plugged after the hub thread's
  first round sometimes never enumerates.
  - Scenario: `test-qemu --kvm` adds `usb-kbd` over QMP as soon as `usb: first round` appears
    (`usb.rs:58-65`).
  - In the failing run, every device present at boot enumerated (the registry lists six), but no
    line for the added keyboard followed, so the guest missed the port's change event, or never
    acted on it.
  - Demonstrated: failed once in three local `test-qemu --kvm` runs; the other two passed, with
    the guest unchanged. Error paths. Medium: a device plugged in later is not seen.

### kernel-objects — main at 510381f4, 2026-10-20

Lenses: logic, error-paths, trust-boundary, tests, architecture; returned nothing: none.
Found 24, refuted by a boot 0, reported 13.

- [ ] **kobj-1** `kernel/src/object/timer.rs:206` — Dropping a Timer never removes its deadline-heap
  entry, so the entry dangles. `Timer::drop` only asserts that no waiters remain, and
  `dispatch_destroy` just frees the box. The heap entry is a raw address, not an ObjectRef. A
  process calls `sys_timer_create` and `sys_timer_set(h, now+X, interval)`, then
  `sys_handle_close(h)` or exits with the timer armed. When X elapses, `fire_expired_deadlines`
  in the timer IRQ writes `in_heap`, reads the waiters and re-arms through freed memory. A
  periodic timer keeps doing this every interval. Demonstrated: a host probe armed a one-shot
  timer, dropped it, and the heap kept an entry whose target was the freed address. A new timer
  reused the slot, and firing the stale entry at now=200 disarmed it (its deadline went
  1_000_000 -> 0). The close and exit paths remove only `DeadlineKind::Thread` entries. logic,
  architecture. High.
- [ ] **kobj-2** `kernel/src/syscall/table.rs:2394` — `build_and_install_file_blocks` keeps the
  transferred DeviceNode's object and discards its rights (`tr.rights` is never read).
  `FileObject::writeback` then issues Write IRPs to that device with no rights check. The only
  read-only gate is the `FILE_BLOCKS_READ_ONLY` flag the server sets itself (table.rs:2433). A
  read-only mount's server holds its device as READ|TRANSFER|DUPLICATE, and `mounting.rs:132-136`
  says it cannot write the device even if it tried. It answers a resolve with
  `OBJECT_KIND_FILE_BLOCKS`, flags 0 and runs at LBAs of its choice. A client asking MAP_WRITE
  gets a writable FileObject, and `sys_file_sync` or `sys_ns_sync` writes the dirty pages to
  those LBAs. This is not the mount-write-authority deferral: here a server without the WRITE
  right writes the disk. Demonstrated: a host probe with a READ|TRANSFER device and flags 0
  returned status 0, and the handle carried MAP_READ|MAP_WRITE. After `writeback`, the recording
  device saw a Write at offset 503808 (LBA 123), length 4096. error-paths also read the path;
  its own probe did not run. logic, error-paths, trust-boundary. High.
- [ ] **kobj-3** `kernel/src/object/ipc_channel.rs:99` — An allocation over the buddy allocator's
  4 MiB MAX_ORDER block, reachable from user input, trips `debug_assert!(order <= MAX_ORDER)`
  (buddy.rs:163) and panics the kernel. xtask builds the kernel with plain `cargo build`
  (dev profile), so debug assertions are on in every boot, the release image included. Any
  process calls `sys_channel_create(.., queue_depth=1024)`; a RingSlot is over 4096 bytes, so
  the `slots.try_reserve` goes past 4 MiB, to order 11. The same class is reached by
  `sys_process_spawn` with an image MemoryObject over 4 MiB (`sys_memory_create` allows up to
  16 MiB, then `copy_to_kvec` reserves it). It is also reached by an fs-server FILE_BLOCKS reply
  with a large `run_count`: `runs.try_reserve(header.run_count)` at table.rs:2589 runs on an
  unbounded server u32 before the per-run body-length check. Demonstrated: a host probe of
  `try_new_pair(1024)` panicked with 'buddy: alloc order 11 > MAX_ORDER', and `try_new_pair(900)`
  was fine. A 5 MiB spawn image panicked and a 3 MiB one was fine. The FILE_BLOCKS case was read,
  not run. architecture. High.
- [ ] **kobj-4** `kernel/src/mm/elf.rs:182` — `parse_phdr` computes `off + ELF64_PHDR_SIZE`
  without a checked add. `load_elf` only checks `e_phoff + i*56`, so an `e_phoff` within 56
  bytes of `u64::MAX` overflows, and with overflow-checks on in the dev profile the kernel
  panics. Any process passes a MemoryObject, or a file it can map, holding a valid ELF64 header
  with `e_phnum`=1, `e_phentsize`=56 and `e_phoff`=`u64::MAX-8` as `SpawnArgs.image` to
  `sys_process_spawn`. Demonstrated: a host probe with those header bytes panicked with
  'attempt to add with overflow' at elf.rs:182:22. logic. High.
- [ ] **kobj-5** `kernel/src/syscall/table.rs:2160` — A forwarded-resolve reply installs its
  handle into `pl.owner_pid` without checking the process still exists, and the comment at 2164
  ('A dead caller pid takes this path — fails cleanly') is false: `HandleTable::allocate` has no
  liveness check. The exit sweeps (`close_all_owned_by`, the reaper, `reap_pending`) run once per
  pid. A client calls `sys_ns_lookup` on a userspace-server mount and exits before the server
  replies. The reply, through `complete_resolve_reply`, `build_and_install_file`,
  `build_and_install_file_blocks` or `continue_resolve`, allocates a handle owned by the dead
  pid that nothing will ever close. A FILE_BLOCKS FileObject then keeps `sys_ns_held` above 0,
  so the storage service refuses to unmount or eject that mount until reboot. A channel
  endpoint never reaches PeerClosed on the server side, and a MemoryObject leaks. A client that
  times out resolves under load leaks steadily. Demonstrated: host probes (three lenses) ran
  the sweep and then a reply for the dead pid. `close_all_owned_by` returned 1 on a second call,
  a reply gave status 0 with a live handle, and `cache_objects().len()` was 1. A swept-pid
  `allocate` also succeeded. The real race against the reaper was not booted. logic,
  error-paths, trust-boundary, architecture. Medium.
- [ ] **kobj-6** `kernel/src/object/ipc_channel.rs:366` — An endpoint sent over its own channel
  keeps a reference to itself in its own receive ring, so it is never freed. `sys_channel_send`
  (table.rs:1934-1937) checks only TRANSFER on each handle and does not refuse the channel's own
  endpoint. A process calls `sys_channel_create(depth=1024)` for A and B, then
  `sys_channel_send(A, msg, [B])`, then closes A or exits. B's refcount never reaches zero, so
  `IpcChannel::drop` never runs. B's ring (about 4 MiB at depth 1024) and every object queued in
  it stay allocated, up to seven MemoryObjects of up to 16 MiB per queued message. Repeating this
  from short-lived processes exhausts kernel memory. Demonstrated: a host probe pushed
  `send_push(a, msg, [b])` and dropped both creation references. It asserted two destroys and
  got 1 (left 1, right 2). error-paths. Medium.
- [ ] **kobj-7** `kernel/src/syscall/table.rs:1620` — `sys_io_submit` on a block device does not
  check that offset and length are multiples of the block size, or that offset+length lies
  within the device. `device-node.md` and `io-operation.md` require a synchronous
  InvalidArgument. `dispatch_block_irp` leaves bounds to the caller, `partition_rebase` checks
  only the end, and AHCI truncates (`lba = offset/512`, `count = (length/512) as u16`,
  ahci.rs:725-726). xhci storage and the ramdisk do check. A holder of an AHCI-backed DeviceNode
  with READ, such as a filesystem server, storage-service or nxinstall, submits a Read of length
  100. It reaches the disk as READ DMA EXT with sector count 0, which ATA reads as 65536
  sectors, against a 100-byte PRDT. A misaligned offset silently targets another LBA. Real
  HBAs may stall slot 0, and the driver has no command timeout on the interrupt path, so later
  IRPs, the root filesystem's included, could hang. Demonstrated (the missing validation only):
  a boot probe on the root partition accepted reads of (off 0, len 100), (off 100, len 512) and
  (off 0, len 600). All completed status 0 with 100, 512 and 600 bytes. The disk did not wedge
  on QEMU, so the hang or DMA overrun is not shown. trust-boundary. Medium.
- [ ] **kobj-8** `kernel/src/syscall/table.rs:855` — `sys_memory_map` requires only the MAP_*
  bits present in the requested `rights`. With `rights`=0 nothing is required, and the mapping
  is installed as `Protection::USER`, which is present and readable. A MemoryObject or
  FileObject handle without MAP_READ can be mapped and read, against `syscall-abi.md` (a
  mapping cannot amplify). A process holding such a handle, restricted to INSPECT|TRANSFER for
  example, calls `sys_memory_map(h, 0, size, 0)`. No in-tree grantor issues a MAP_READ-less
  memory or file handle today, so it is latent. Demonstrated: a boot probe on an INSPECT|TRANSFER
  duplicate got a mapping (4295081984) and a volatile read succeeded. A host probe showed
  `MAP_READ` gives NoAccess and `rights`=0 gives Ok(0x100000000), with `translate` finding the
  object's frame and `page_flags_for` = USER|NO_EXECUTE. logic, error-paths, trust-boundary.
  Reviewers disagreed on severity (the boot verifier and two lenses said low, trust-boundary
  said medium); ranked low as latent.
- [ ] **kobj-9** `kernel/src/syscall/table.rs:1934` — `sys_channel_send`, and
  `sys_process_spawn`'s handle loop at 446, look up each listed handle separately and do not
  reject a handle listed twice. One handle sent as `[H, H]` arrives as two handles, and the
  second `close_and_release` fails silently. A holder of H with TRANSFER but not DUPLICATE
  (service-mgr hands out LOOKUP|TRANSFER namespace handles) thus copies it. This contradicts
  `handle-encoding.md:199` and the move-only transfer semantics in `ipc-message-format.md:127`.
  Spawn with `handles=[H, H]` does the same, but only the channel path was run. Demonstrated: a
  boot probe sent `[H, H]`, count=2, with a DUPLICATE-less INSPECT|TRANSFER memory handle over a
  loopback channel. Send returned 0, and recv returned 0 with count 2. error-paths, tests. Low.
- [ ] **kobj-10** `kernel/src/sched/ipc.rs:453` — When a userspace server's endpoint dies,
  `ipc_endpoint_closing` and `us_reg_detach` drain only the registration's pending lookups,
  never its pending fills (`File::ReadRange`, Model B). A thread faulting a lazy
  `OBJECT_KIND_FILE` page waits in `wait_for_fill` with `u64::MAX` and never wakes, and nor does
  any later faulter of that page. The PendingFill holds the FileObject, which holds the
  registration, so the objects leak in a cycle. A server exits before replying to a ReadRange.
  In-tree servers answer FILE_BLOCKS only, so a shipped system reaches this only through a
  custom server. Demonstrated: host probes with one lookup and one fill, then the server end
  dropped, gave 'lookup signalled true (-13, 0); fill signalled false (0, 0)'. The FileObject
  refcount was 2 (the test's reference plus the stranded fill's), and nothing drains
  `pending_fill`. logic, trust-boundary. Reviewers disagreed on severity (logic: medium,
  trust-boundary: low); ranked low.
- [ ] **kobj-11** `kernel/src/syscall/table.rs:483` — When allocating the parent's Process
  handle fails after the child thread is queued, `sys_process_spawn` returns an error while the
  child runs. It skips the move-close loop at 493-497, so the parent's move_mask handles stay
  open although the child already holds them. This needs a handle-table segment grow to fail
  after `spawn_user`. The parent sees a failed spawn and may retry, starting a second child.
  Unverified: needs a failure injection in the global table's segment grow and a boot to
  settle. error-paths. Low.
- [ ] **kobj-12** `kernel/src/handle/table.rs:588` — No test hands `duplicate()` a `new_rights`
  with bits the source lacks, so replacing `info.rights & new_rights` with `new_rights` passes
  the whole kernel host suite. This is a test gap, not a defect in code that works, and it is
  listed because the break would be rights amplification on `sys_handle_duplicate` (a holder of
  DUPLICATE|SIGNAL could mint TERMINATE; `allocate`'s type check does not stop amplification
  within a type). Demonstrated: the one-line break left 986 passed, 0 failed. The restrict twin
  is caught by `restrict_on_cannot_amplify`. tests. The reviewer rated it high; as a test gap,
  low.
- [ ] **kobj-13** `kernel/src/syscall/table.rs:1163` — Several syscall-level guards have no host
  coverage; these are test gaps, not defects. Removing `require_syscap(BIND_NAMESPACE)` in
  `sys_ns_bind`, `direct_bindable(rights)?`, `Rights::LOOKUP` on `sys_ns_derive`, the
  direct-bind `!base.is_empty()` rejection, or `requested & binding_rights` in `install_for`
  leaves the host suite green. So does removing, in `sys_channel_send`, the per-handle TRANSFER
  lookup, the `payload_len > IPC_PAYLOAD_SIZE` reject or `sender_pid`, and removing
  `sys_memory_map`'s `size > max_bytes` check. Unverified: the host-survival claim was not
  re-run, and no per-break boot controls were run. A boot probe for bind-needs-TRANSFER exists
  and passes. Boot runs with each break would show which guards the QEMU probes catch. tests.
  Low.

What held up: Deleting the owner-pid, generation or rights-subset check in handle lookup fails
several tests. Close and restrict owner and generation checks fail named tests, and so do
`restrict_on_cannot_amplify`, `duplicate_requires_duplicate_right` and the `entropy_read`,
`sys_wait` and `sys_channel_send` count bounds. The reviewers read these as sound: the handle
lookup order and the NotOwner-to-InvalidHandle collapse. `duplicate` and `restrict` only
intersect, and `allocate` enforces the type-rights matrix. `sys_channel_send` checks TRANSFER on
every handle before moving any, and `sys_channel_recv` rolls back on every copy-out fault.
`sys_process_spawn` rolls back the child's handles before the enqueue, derives child syscaps as
parent & requested, and gives the child a LOOKUP-only root. User pointers go through
`UserPtr`/`UserMutPtr` with checked ranges, and counts are bounded before copying. The
grace-tracker per-CPU ids are safe because syscall bodies run with IF=0. Namespace
`validate_path` and the SUBNAMESPACE continuation checks hold. `block_map` has a test at each
bound, and the reply parsers use checked arithmetic. Cache frames are zeroed before a fill,
timer re-arm saturates, and affinity falls back from offline CPUs. Kernel and userspace ABI
types match through compile-time asserts and `abi-sync-check`, with no drift found.

### kernel-memory — main at 510381f4, 2026-10-20

Lenses: logic, error-paths, trust-boundary, tests, architecture; returned nothing: none. The hunt is
incomplete: the architecture lens's own findings were lost when a safety classifier stopped its
response, so only what it checked and found sound is below. A fresh run of that lens would recover
them.
Found 29, refuted by a boot 0, reported 19 (duplicates merged).

- [ ] **kmem-1** `kernel/src/mm/elf.rs:128` — `load_elf` returns `e_entry` unchecked, and
  `spawn_user` hands it to `enter_user`'s `iretq`. `sys_thread_create` checks its entry with
  `UserPtr::new`; the spawn path checks nothing. Any process builds a MemoryObject holding an ELF
  with a valid PT_LOAD and `e_entry = 0x8000_0000_0000` (non-canonical), then calls
  `sys_process_spawn`. The `iretq` raises #GP at CPL 0, which `dump_and_halt` turns into a halt of
  the whole machine. Demonstrated: a host probe showed `load_elf` returning Ok for non-canonical
  and kernel-half entries. `cargo xtask test-qemu --kvm` on an Intel i7-10875H, with a hand-built
  ELF swapped into the 15th spawn: the kernel took `#GP` with cs 0x0008, the `iretq` frame showed
  rip=0x0000800000000000 and cs=0x23, and the dump ended "stopping every CPU". Lenses: logic,
  trust-boundary. High.
- [ ] **kmem-2** `kernel/src/syscall/table.rs:885` — User mappings may reach `USER_VIRT_END`, so the
  last canonical user page (0x7FFF_FFFF_F000) can be mapped executable, by a hinted
  `sys_memory_map` (`end > USER_VIRT_END` is the only bound) or an ELF PT_LOAD. A `syscall` in its
  last two bytes saves the non-canonical return RIP 0x0000_8000_0000_0000, and the `sysretq` exit
  path does not check RCX. On Intel, `sysretq` then faults in ring 0 with the user RSP already
  loaded, and the vector-13 gate has no IST. Demonstrated: a host probe showed `load_elf` accepting
  the PT_LOAD at `USER_VIRT_END - PAGE_SIZE`. `test-qemu --kvm` on an Intel host, with an exec
  PT_LOAD there and a `syscall` in its last two bytes: the kernel stopped at cs 0x0008 inside
  `syscall_entry` with rcx=0x0000800000000000, rsp on the user stack, and the machine halted. The
  serial output was interleaved, so the vector line was garbled. AMD is unaffected. Linux
  reserves this page for this reason. Lens: logic. High.
- [ ] **kmem-3** `kernel/src/mm/addr_space.rs:274` — `AddressSpace::fault_in`'s Anonymous arm
  assumes the PTE is still absent and hits `unreachable!()` when `map_page` returns
  `AlreadyMapped`. `map_file_page` accepts `AlreadyMapped` for the same race. Two threads of one
  process on different CPUs touch the same untouched page of a lazy VMA, such as the initial
  thread's 8 MiB stack. Both fault not-present, the first maps the page, and the second takes the
  AS lock, finds the PTE present and panics the kernel ("fault_in: map_page rejected a fresh
  page-aligned not-present mapping"). Reachable with `sys_thread_create` and no privilege.
  Demonstrated: a host test did `map_vma_lazy`, then `fault_in` twice on one page, and the second
  call panicked at `addr_space.rs:274`. The two-CPU interleaving itself was not booted. Lenses:
  error-paths, trust-boundary, tests. High.
- [ ] **kmem-4** `kernel/src/mm/elf.rs:182` — `parse_phdr` computes `off + ELF64_PHDR_SIZE`
  unchecked. `load_elf` bounds `e_phoff + i*56` but not the `+56`, and every image boots a
  dev-profile kernel (`overflow-checks = true`). A MemoryObject holding an ELF with `e_phoff` near
  `u64::MAX` and `e_phnum >= 1`, passed to `sys_process_spawn` (which needs only MAP_READ on the
  image and no syscap), panics the kernel instead of returning Truncated. Demonstrated: a host test
  with `e_phoff = u64::MAX - 8` panicked with "attempt to add with overflow" at `elf.rs:182`. The
  logic lens noted that a release build wraps into the bounds-checked readers and returns
  Truncated, so only the shipped dev-profile kernel is affected. Lenses: error-paths,
  trust-boundary. High.
- [ ] **kmem-5** `kernel/src/mm/buddy.rs:163` — `BuddyAllocator::alloc` has
  `debug_assert!(order <= MAX_ORDER)` ahead of its graceful `return None`, and slab's `large_alloc`
  passes `order_for_size(total)` unchecked. In the debug-built kernel every image boots, any
  kmalloc over about 4 MiB panics instead of failing. A process makes a 5 MiB MemoryObject
  (`sys_memory_create` allows 16 MiB) and spawns from it: `copy_to_kvec` calls `try_reserve`,
  which reaches `buddy_alloc(11)` and the panic. The image need not be a valid ELF, because the
  copy happens first. Any FileObject executable over 4 MiB (`read_to_kvec`) does the same, as does
  `sys_memory_unmap` of a mapping of 512Ki pages or more. Shipped binaries are all under 1 MiB.
  Demonstrated: a host test with `MemoryObject::try_new(5 MiB)` then `copy_to_kvec()` panicked at
  `buddy.rs:163` ("buddy: alloc order 11 > MAX_ORDER"). Lenses: error-paths, trust-boundary. High.
- [ ] **kmem-6** `kernel/src/arch/x86_64/paging.rs:224` — `ensure_table` is a non-atomic
  read-then-write of a shared kernel-half page-table slot. `KernelStack::new` and
  `kvmap::map_mmio` call `map_page` on the shared vmap hierarchy holding no lock, so two CPUs that
  both find the slot empty each link their own new table, and one overwrites the other with all
  its leaves. Two CPUs run `sys_thread_create` or `sys_process_spawn` while the kvmap cursor
  enters a new 2 MiB region, which happens about every 102 stacks because VA is never reclaimed.
  One thread's stack pages land in an orphaned table, and its first run takes a kernel #PF or #DF
  and the kernel stops. Demonstrated: a host test advanced the cursor to a fresh 2 MiB boundary
  and built two `KernelStack`s from two std threads released together. Over 300 rounds, 256 stack
  pages (64 whole stacks) did not translate. The control, with the two builds serialized, lost 0.
  `Thread::try_new_user` and `try_new_runnable` call `KernelStack::new` outside any lock. This is
  a different mechanism from the TLB-staleness theory that `deferred-decisions.md` rules out for
  TODO(unexplained-df), and may be a cause of that #DF. Lens: error-paths. High.
- [ ] **kmem-7** `kernel/src/syscall/table.rs:855` — `sys_memory_map` requires only the MAP_* bits
  the caller asks for (`required = req & MAP_*`) but always installs a present, user-readable
  mapping. With `rights = 0`, or only MAP_WRITE or MAP_EXEC, a MemoryObject or FileObject handle
  with no MAP_READ (INSPECT|TRANSFER after `sys_handle_restrict`, a write-only spawn grant, a
  binding without MAP_READ) maps readable. The spec says a mapping cannot amplify, and
  `sys_process_spawn` and `sys_io_submit(Write)` both treat MAP_READ as the read right. No
  in-tree component hands out a MAP_READ-less handle and relies on it staying unreadable. The
  logic lens rated it high as a way past a capability, and three lenses rated it medium. The fix
  is to always require MAP_READ. Demonstrated: host probes of the syscall's two steps on a handle
  with INSPECT|TRANSFER. `lookup` with an empty requirement succeeded, and `map_object` with
  `Protection::USER` left `Paging::translate` returning Some. The syscall wrapper itself cannot
  run on the host. Lenses: logic, error-paths, trust-boundary, tests. Medium.
- [ ] **kmem-8** `kernel/src/mm/elf.rs:223` — A PT_LOAD's `p_memsz` is bounded only by
  `USER_VIRT_END`, and `map_vma` commits it eagerly, allocating and zeroing a frame per page under
  the AS lock in an IF-masked syscall. `MemoryObject::MAX_SIZE` (16 MiB) exists to prevent
  exactly this, and the ELF path bypasses it. Any process spawns a tiny ELF with `p_memsz` in the
  GiB to TiB range. The CPU zeroes all free RAM with interrupts off before the rollback, so other
  CPUs' allocations fail system-wide (demand faults are fatal to the faulting thread) and their
  shootdowns wait on this CPU's ack. The caller can repeat it. Demonstrated: a host test with a 1
  TiB `p_memsz` returned OutOfMemory only after exhausting the 16 MiB host heap. `test-qemu --kvm`
  with `p_memsz` = 0x600000000000: `load_elf` passed validation and `map_vma` ran until the buddy
  was empty, 74.6 ms of uncapped allocation in one syscall on a guest with about 180 MB, so the
  stall scales with RAM. The effect on other CPUs was not measured. Lenses: logic, trust-boundary.
  Medium.
- [ ] **kmem-9** `kernel/src/mm/kvmap.rs:58` — `vmap_alloc_pages` bounds the bump pointer by
  `KERNEL_VMAP_END` (16 TiB), but only the first 512 GiB has a PDPT in the kernel template, and VA
  is never reclaimed. Past that, a new stack lands in a PML4 slot that `map_page` fills in one
  address space's PML4 only, which breaks the "every kernel stack is mapped in every root"
  invariant. A process creating and exiting threads in a loop takes 5 vmap pages per stack, so
  after about 26.8M creations the cursor passes 0xFFFF_C080_0000_0000. The next stack is mapped
  only in the root given to `KernelStack::new`, so a context switch to another root drops the
  running stack, causing #PF then #DF. Nothing refuses the allocation. Demonstrated: a host test
  made ASes A and B and a stack at 0xffffc00000001000, visible from both. After bumping the cursor
  to 512 GiB, `KernelStack::new(A)` succeeded at 0xffffc08000001000, mapped in A and not in B. On
  small-RAM machines the recorded page-table-frame leak may exhaust memory first. Lens: logic.
  Medium.
- [ ] **kmem-10** `kernel/src/sched/ipc.rs:499` — When a server's endpoint closes
  (`ipc_endpoint_closing`, line 480) or its registration drops (`us_reg_detach`), only pending
  lookups are drained, never `pending_fill`. A Model B page-cache fill whose server dies or never
  replies is never completed. A server replying OBJECT_KIND_FILE gets a client that maps the
  FileObject and touches a page, so `try_fault_in` parks in `wait_for_fill` on the fill PO with no
  deadline. The server exits and the thread hangs in the #PF handler for good, and every later
  fault on that page waits on the same PO. The `PendingFill` also holds the FileObject, whose
  producer holds the registration, a reference cycle. No in-tree server replies OBJECT_KIND_FILE
  today (the fs-servers use Model A), which limits reach; the error-paths lens rated it low.
  Demonstrated: a host test with a pair endpoint, one `begin()` lookup and one `begin_fill()`,
  then the server end dropped, gave "lookup signalled=true, fill signalled=false". Only
  `take_pending_fill_matching` ever takes `pending_fill`. A boot with a Model B test server was
  not run. Lenses: logic, error-paths. Medium.
- [ ] **kmem-11** `kernel/src/mm/addr_space.rs:631` — No test pins `page_flags_for`'s WRITABLE,
  NO_EXECUTE and USER translation, and this function builds every user PTE. A regression that
  drops NX, makes read-only mappings writable or marks kernel mappings user-accessible would
  silently remove W^X and read-only protection, and boot gates probably would not notice.
  Demonstrated: three mutations in `mm::addr_space` all survived, namely the EXEC test forced to
  `false`, the WRITE branch forced on, and the USER branch forced on. Only the WRITE_COMBINING
  branch was killed. Lens: tests. Medium.
- [ ] **kmem-12** `kernel/src/mm/addr_space.rs:574` — No test catches a leaked or double-freed frame
  on unmap. A regression could leak an Anonymous frame per page, or collect Object and FileBacked
  frames into `frames`, so `sys_memory_unmap` would `buddy_free` frames a MemoryObject or the page
  cache still owns, a double free that corrupts the buddy lists. The
  `matches!(boxed.mapping, Anonymous)` guard in `unmap_covering_deferred` is the only barrier and
  is untested. Demonstrated: three mutations in `mm::addr_space` survived, namely
  `free_vma_pages`'s Anonymous arm changed to `{}`, the `matches!` guard changed to `true`, and
  `capacity - len < pages` changed to `< 0`. Lens: tests. Medium.
- [ ] **kmem-13** `kernel/src/mm/elf.rs:218` — The ELF loader's `p_filesz > p_memsz` check, its
  `file_end > bytes.len()` check and its `aligned_end > USER_VIRT_END` bound have no test, and
  neither does PF_W/PF_X mapping into protection bits. ELF bytes come from a disk or filesystem
  server, which is not trusted. A regression dropping a check gives a kernel panic on a malformed
  image ("just-mapped virtual address must translate", or a slice index), and one dropping the
  flag mapping gives W+X segments. Demonstrated: five mutations in `mm::elf` survived, namely the
  `p_filesz > p_memsz` check forced to false, the `file_end` check forced to false, `aligned_end`
  loosened by one page, and `PF_W` and `PF_X` each forced to true. Lens: tests. Medium.
- [ ] **kmem-14** `kernel/src/object/memory_object.rs:71` — Nothing tests that fresh MemoryObject
  or DmaBuffer frames are zeroed. A regression removing the zeroing in `sys_memory_create` would
  hand userspace stale kernel or other-process pages, a cross-process information leak. The host
  test heap probably hands out already-zero memory, so the zeroing is never observable.
  Demonstrated: four mutations survived. Removing `write_bytes` in `MemoryObject::try_new`
  (tests `object::memory_object`), removing the partial-failure `buddy_free(done, 0)` rollback,
  removing `write_bytes` in `DmaBuffer::alloc` (tests `mm::dma`), and `DmaBuffer::drop` freeing at
  order 0 instead of `self.order`, which leaks higher-order buffers. Lens: tests. Medium.
- [ ] **kmem-15** `kernel/src/mm/addr_space.rs:453` — `map_file_page` re-validates that a
  FileBacked VMA for the same object still covers `addr`, but not that `addr` still corresponds to
  the page index the fault filled. Thread A faults at X in a mapping of file F at base B (index
  i) and parks on the fill. Thread B unmaps it and maps F again at B', which still covers X. A
  then installs page i's frame where page j belongs, so the process sees (or writes) the wrong
  bytes. The effect is confined to files the process already maps. Unverified: a host test
  interleaving `file_backing`, `unmap_covering`, `map_file` and `map_file_page` would settle it.
  `try_fault_in` (`idt.rs:315-319`) takes the index from `file_backing`, and the frame is
  installed without re-deriving it under the second AS-lock hold. Lens: logic. Low.
- [ ] **kmem-16** `kernel/src/object/file_object.rs:540` — `begin_fill`'s read-ahead adjacency test
  computes `first * bs + (n * PAGE_SIZE) as u64` unchecked. `block_map` guarantees only that each
  run's own end fits a u64, so a run map near the top of the byte range overflows when the next
  page lies past it. A filesystem server replies FILE_BLOCKS with bs = 512 and runs (0, D, 8) and
  (8, D+8, 1), D = u64::MAX/512 - 10, which `block_map` accepts. A fault on page 0 with read-ahead
  checks page 2 and the kernel panics. Only a malicious registered server reaches it; honest
  servers cannot produce such LBAs. Demonstrated: a host probe through
  `mapped_file`/`miss`/`begin_fill` panicked at `file_object.rs:540:30` ("attempt to add with
  overflow"). `begin_write`'s matching `first * bs + (k * PAGE_SIZE)` was not probed. Lens:
  trust-boundary. Low.
- [ ] **kmem-17** `kernel/src/mm/vmm.rs:413` — `find_free_range`'s upper bound (`MMAP_MAX`, which
  keeps hint=0 mappings out of the stack guard gap) and its exact-fit boundary have no test. If
  the bound regressed, `sys_memory_map(hint=0)` could return ranges in the guard gap and a stack
  overrun would land in mapped memory. An off-by-one in the exact-fit comparison would skip a gap
  that fits. Demonstrated: three mutations in `mm::vmm` survived, namely the `> max` early return
  forced to `if false`, the final `cursor+size <= max` forced true, and `s >= cursor+size` changed
  to `s >`. Lens: tests. Low.
- [ ] **kmem-18** `kernel/src/mm/buddy.rs:142` — The `order > MAX_ORDER` guard in
  `BuddyAllocator::alloc` is untested (only the `debug_assert` fires in debug), and neither is
  `BuddyAllocator::new`'s `bitmap_reservation` rounding to whole pages. In a release build
  without the guard, `alloc(order > 10)` would index `free_lists` out of bounds, and a rounding
  bug would let pass 2 strip frames from the next usable entry. Reach is low, since
  `DmaBuffer::alloc` and `order_for` already bound the order. Demonstrated: two mutations in
  `mm::buddy` survived, namely the guard changed to `if false` and `bitmap_reservation` using
  `bitmap_bytes`. See kmem-5 for the `debug_assert` that makes the guard unreachable in the dev
  build. Lens: tests. Low.
- [ ] **kmem-19** `kernel/src/object/file_object.rs:437` — `resize`'s
  `g.retired.try_reserve(gone)?`, the all-or-nothing allocation before pages are removed, can be
  deleted with every `file_object` test still passing. Without it, the later
  `let _ = g.retired.try_push(p.frame)` drops its Result when the vector cannot grow, so the
  frame is neither freed nor tracked, and the allocation also happens under the lock. With the
  reserve, a truncate that cannot grow `retired` returns Err and changes nothing. No test covers
  that failure. Demonstrated: removing the reserve and running `object::file_object` survived.
  Lens: tests. Low.

What held up: user copies (`validate_user_range` with `checked_add` against `USER_VIRT_END`,
zero-size and empty-slice cases, `lookup_recovery` matching only the two registered PCs, `clac` and
DF=0 on both paths) and the #PF handler's order (recovery before the ring-3 check). Mutation
tests killed loosening the end-of-user-space bound, `wrapping_add`, the ELF alignment and PT_INTERP
checks, the file-object resize and writable-mapping accounting mutations, the Borrowed-ownership
early return in `Drop`, the buddy XOR, `map_file_page`'s same-object check, and `wait_satisfied`'s
acked-versus-departed table. `sys_memory_unmap` clears PTEs under the AS lock, shoots down, and only
then frees anonymous frames and drops the VMA. The `VmaTree` overlap check, `map_vma`, `map_object`
and `map_file` rollbacks and writable-count balance, `FileObject::settle` matching by PO,
`block_map`'s block-size and run-end checks, and partition IRP rebasing all held. So did
`sys_io_submit`'s buffer bound and its `IrpBox._buffer` reference, the `Thread` drop order, the
scheduler loading the next root before a dying address space drops, and `tlb::shootdown`'s ack
bitmask. The intermediate page-table frame leak on unmap is a recorded deferral.

### filesystems — main at 510381f4, 2026-10-20

Lenses: logic, error-paths, trust-boundary, tests; returned nothing: none.
Found 29, refuted by a boot 0, reported 19 (29 findings merged as duplicates into 19).

- [ ] **fs-1** `userspace/fs-server-ext4/src/ext4.rs:1747` — `rename_at` validates only the new
  name, so a directory session can rename its own `..` or `.` entry to an ordinary name. A session
  in a namespace bound with subtree base `/home/alice` renames `..` to `up`. `/home/alice/up` then
  names `/home`, and repeating it from there reaches `/`, which is a way out of the confined
  subtree. The directory also loses its `..` record. With `.` it gets an entry pointing at itself
  and no `.` record. Neither `parse_rename_request` nor `serve_session` (server.rs:681) checks the
  names. Demonstrated: host tests on an mke2fs fixture. `rename_at(alice, "..", "up")` returned Ok.
  `resolve_dir("/home/alice/up/bob")` gave Bob's inode. `stat_file` of Bob's file succeeded. A second
  hop reached `/system`. `e2fsck -fn` reported "Second entry 'up' ... should be '..'" and "'..' in
  /d is <The NULL inode>". FAT is not affected, because `dir::entries` skips dot entries. Lenses:
  logic, error-paths, trust-boundary, tests (four reviewers found it independently). high.

- [ ] **fs-2** `userspace/libfsserver/src/server.rs:650` — A directory session is bound to a bare id
  (an ext4 inode number, a FAT first cluster). `live_dir` rejects only a freed id, not a reused
  one. A session is open on `/tmp/d`, then `/tmp/d` is rmdir'd. The next mkdir anywhere takes the
  same id (ext4 takes the lowest free inode; FAT takes the cluster once the allocation hint wraps).
  The stale session then lists, creates, renames and unlinks inside the new directory, outside its
  namespace. Demonstrated: on ext4, session id 17 equalled the new `/secret/inner` id, and
  `mkdir_at(17, "planted")` created `/secret/inner/planted`. Another probe made `mkdir_at(18, ...)`
  land in `/home/bob/private`. On FAT (16 MiB, 4 KiB clusters), session 4 equalled `/secret/inner`
  and `read_dir` listed `victim.txt`. `rename_at` and `unlink_at` through the stale id both
  returned Ok. The existing test `a_removed_directory_is_refused_to_a_session_still_holding_it`
  covers only the non-reuse case. Lenses: logic, error-paths, tests. high.

- [ ] **fs-3** `userspace/fs-server-ext4/src/ext4.rs:1705` — A rename writes the new name first and
  deletes the old one second (ext4 `rename_path`; FAT `Fat::rename`, volume.rs:823-848). If the
  second write is refused or the server crashes between them, the file has two names backed by one
  ext4 inode (links_count 1) or one FAT chain. Nothing copes with it, and `unlink_at`/`drop_link`
  treat a count of 1 as the last name. A retry of the same replacing rename, or an unlink of either
  name, then returns the file as an orphan. The server forgets and releases it while the other
  name still references it, and the next allocation hands its blocks to another file. Nothing
  repairs this at mount (TODO(fs-repair)). Demonstrated: ext4 with `Failing::once(img, 1)` failed
  the first rename with Err(Io). The retry gave Ok(Some(17)) and release Ok, and `e2fsck` reported
  "Entry 'g' ... has deleted/unused inode 17". After create plus grow, `/d/g` and `/e/other` both
  mapped inode 17 at lba 12. The unlink-after-half-rename variant did the same. On FAT16 with 4 KiB
  clusters, with write 1 of 2 refused, the retry gave Ok(Some(2)) and release(2) Ok. `OTHER.TXT` took
  cluster 2 and `fsck.fat` said "/OTHER.TXT and /G.TXT share clusters". Lens: error-paths. high.

- [ ] **fs-4** `userspace/fs-server-ext4/src/ext4.rs:226` — `parse_superblock` and `check_device`
  refuse only INCOMPAT_64BIT and blocks over 4 KiB. They accept metadata_csum, an unreplayed
  journal (needs_recovery), meta_bg and bigalloc, and mount them read-write. An ext4 made with
  `mke2fs -t ext4 -O ^64bit` on a removable disk is mounted writable. `mark_mounted` writes
  `s_state` without updating the superblock checksum, so Linux and `e2fsck` refuse the filesystem
  after the first write. A disk with needs_recovery would have its changes overwritten by a later
  Linux journal replay. The crate's CLAUDE.md says these are rejected. Demonstrated: `dumpe2fs`
  showed has_journal and metadata_csum. `check_device` returned Ok, and `create_file` and
  `grow_file` returned Ok. `e2fsck -fn` then failed with "Superblock checksum does not match
  superblock". `check_device` also returned Ok with INCOMPAT_RECOVER (0x4) set. Lens: logic. high.

- [ ] **fs-5** `userspace/fs-server-ext4/src/ext4.rs:1075` — `dir_insert` computes `used =
  round4(8 + name_len)` from the on-disk entry and `rec_len - used` without checking `used <=
  rec_len`. The three sibling scans (`dir_lookup` 346, `read_dir_inner` 492, `dir_remove` 1366) do
  carry their bound. A directory entry whose name_len exceeds its rec_len underflows the
  subtraction. Any create, mkdir or rename into that directory on a crafted or corrupt image (for
  example a removable disk mounted read-write) then panics the server and every client gets
  PeerClosed. In release, `232 - 264` wraps, the test passes, and `buf[new_off..]` with new_off
  4128 panics out of bounds. A shallower entry writes a bogus record in bounds and leaves the
  directory corrupt. Demonstrated: patching the root `.` entry to name_len=200 and calling
  `create_file(b"/", b"newfile")` panicked at ext4.rs:1075 with "attempt to subtract with
  overflow". Release panicked at 1081 with "range start index 4128 out of range for slice of length
  4096". A shallow crafted entry returned Ok and `e2fsck` reported "directory corrupted". The
  crate has no hostile-bytes test, unlike FAT's `garbage_in_any_structure_is_an_error_never_a_panic`.
  Lenses: trust-boundary, tests. high.

- [ ] **fs-6** `userspace/fs-server-ext4/src/ext4.rs:1627` — `dir_repoint` compares `&buf[off + 8..off
  + 8 + name_len]` without checking `off + 8 + name_len <= bs`, so a crafted directory block makes
  the server panic. A directory whose first block ends in an 8-byte entry at offset 4088 with
  name_len 2 and a nonzero inode is moved with `rename_path`. A rename that replaces an existing
  destination reaches the same code. The server exits and every client of that mount gets
  PeerClosed. Demonstrated: a host test panicked at ext4.rs:1627:60 with "range end index 4098 out
  of range for slice of length 4096". Reading the replacing-rename path (the trust-boundary
  reviewer's finding) is unverified; a host test feeding `rename_path` a crafted destination
  directory would confirm it. Lenses: logic, trust-boundary. high.

- [ ] **fs-7** `userspace/fs-server-ext4/src/ext4.rs:1665` — `rename_path` does not limit the
  destination leaf to 255 bytes, and `dir_insert` stores `name.len() as u8`, so a long name is cut
  mod 256 and the rename reports success. `create_file`, `mkdir_at` and `rename_at` all check. A
  256-byte name gets name_len 0, so the file vanishes from lookup and listing. A 300-byte name
  becomes its first 44 bytes, and if an entry of that name exists the directory holds two entries
  with one name. The kernel's `validate_path` bounds only the whole path, not a component.
  Demonstrated: `rename_path(/system/f, /system/<256 x 'a'>)` returned Ok(None). `/system` then
  listed only `.` and `..`, and `read_file` under both names gave NotFound. `e2fsck` reported
  "Entry '' in /system (13) has a zero-length name". For 300 bytes, `stat_file` of the full name gave
  NotFound and of the 44-byte prefix gave Ok(12288). FAT is not affected. Lenses: logic,
  error-paths. high.

- [ ] **fs-8** `userspace/fs-server-ext4/src/ext4.rs:1512` — `unlink_at` and `rename_path` can
  return Err after the irreversible step, and the orphan they held is lost with the error. In
  `unlink_at`, the name is removed and `drop_link` has produced the inode when `touch_inode(dir)?`
  (an mtime stamp) fails. In `rename_path`, the touches and `adjust_links` after `drop_link` fail
  the same way. The server runs `forget_then_release` only on Ok (server.rs:667), so the inode and
  all its blocks leak. The client is told the unlink failed although the name is gone, and a retry
  gets NotFound. Nothing here can run `e2fsck`. Demonstrated: a write-failure sweep of the ext4
  ops, with closures that use `?` as the server does. "unlink: write 1/9 once: Err(Io): Unattached
  inode 17" and "rename_path_replace: write 2/10 once: Err(Io): Unattached inode 18". FAT does not
  have this. Lens: error-paths. medium.

- [ ] **fs-9** `userspace/fs-server-ext4/src/ext4.rs:1115` — If `dir_insert`'s directory growth
  loses its last write (the inode's new size), nothing undoes it. Once the directory's extent tree
  has a leaf block (depth 1 or more), the leaf already maps block N past the unchanged i_size.
  Every later insert that needs a block appends at N again, and `extent::add` refuses it as
  Corrupt. Files recover through `grow_file_from`, which cuts past-the-end blocks first; directories
  never do. TODO(ext4-failed-write-undo) records only a cost in free space; this also stops the
  directory from growing. A full directory of about 115 files with a depth-1 tree has its growth
  inode write refused once. Every later create, mkdir or rename into it that needs a block fails
  until an offline `e2fsck`. Demonstrated: `probe_dir_growth_lost_inode_write` lost each of the
  create's 11 writes in turn. Only write 9 (the directory inode) gave "later creates failing: 30
  Some(Some(Corrupt))", and `e2fsck` said "end of extent exceeds allowed value (logical block 6,
  physical block 388, len 1)". Lens: error-paths. medium.

- [ ] **fs-10** `userspace/fs-server-ext4/src/ext4.rs:1692` — The guard against moving a directory
  into its own subtree is a byte-prefix test, `new_parent_path.starts_with(old_path)`, with no
  component-boundary check. It refuses a legal move into a sibling whose name starts with the
  source's name. `libfs::rename` maps the resulting Unsupported to CrossDevice, so callers fall
  back to a slow, non-atomic copy plus unlink. The reviewer also noted that `rename_path('/ab',
  '/./ab/x')` would get past the guard, but the kernel's `validate_path` rejects `.` components for
  bindings and the lookup and rename paths were not checked (unverified). FAT uses the ancestor
  walk `dir::within`. Demonstrated: `rename_path(/ab, /abc/ab)` and `(/d, /dd/d)` returned
  Err(Unsupported). The library-level `/ab` to `/./ab/x` call returned Ok(None) and `e2fsck`
  reported "Entry 'x' in . is a link to '.'". Lenses: logic, error-paths, tests. medium.

- [ ] **fs-11** `userspace/fs-server-ext4/src/extent.rs:163` — `on_device()` bounds an extent only to
  `[first_data_block, blocks_count)`, so an extent over a group's bitmaps, inode table or GDT, or
  over another file's blocks, is accepted and then freed by truncate, unlink or release through
  `free_run`. The metadata blocks are cleared in the bitmap and the allocator hands them out. A
  crafted stick has a file whose extent points at group 0's inode-table block; after unlink, a
  later grow or create reuses it and overwrites inodes. Linux's `ext4_inode_block_valid` check is
  absent. Demonstrated: patching a created file's extent start to the inode-table block (34). Its
  bitmap bit was 1 before and 0 after `unlink_at` plus `release_inode`. Lens: tests. medium.

- [ ] **fs-12** `userspace/fs-server-fat/src/table.rs:282` — `allocate()` aborts with `?` on a used
  cluster whose FAT entry is reserved or out of range (Corrupt) after it has marked free clusters.
  The partial chain is neither freed nor are later clusters tried, and `change()` flushes the cache
  anyway, so it reaches the disk. On FAT16 with file A at clusters 2-3, cluster 4 free and cluster 5
  holding 0xFFF1, `grow_file(A)` marks 4 as end-of-chain, hits 5 and returns Corrupt. Cluster 4 is
  allocated to nobody, and every later grow or mkdir that scans through 5 fails the same way, so
  one bad entry makes the volume unwritable. Demonstrated: `grow_file(/A.TXT, 10000)` returned
  Err(Corrupt) and cluster 4's FAT entry afterwards read [255,255], not 0. `hostile_tests` only
  checks for no panic. Lens: tests. medium.

- [ ] **fs-13** `userspace/libfsserver/src/serve.rs:78` — The MAX_SUFFIX bound in `resolve()` and
  `serve_read_range()`, and the `.min(content.len())` clamp on the read length, are untested. A
  suffix of 1025 bytes or more returns TooLarge today. If the bound were lost, `path_buf[1..1+len]`
  would panic and the server would hang. Adjacent defect: `try_resolve_directory` (server.rs:381)
  truncates an over-long suffix with `.min(MAX_SUFFIX)` instead of refusing it. A suffix of 1025
  bytes or more whose first 1024 bytes name a directory therefore opens a session instead of
  TooLarge. That function is syscall-bound and can only be checked in a boot. Demonstrated:
  deleting either `req.suffix.len() > MAX_SUFFIX` check, or the `.min(content.len())`, left
  libfsserver, fs-server-fat and fs-server-ext4 `--lib` all green. The truncation was found by
  reading. Lens: tests. medium.

- [ ] **fs-14** `userspace/fs-server-fat/src/volume.rs:706` — FAT's `rmdir_at` NotEmpty check, the
  `is_dir` refusal in `unlink_at`, `names::valid` in `mkdir_at` and in rename, and the `grow_file`
  "chain shorter than size" check have no test that reaches them. Without NotEmpty, rmdir of a
  non-empty directory would free its chain and orphan its children's clusters. Without the `is_dir`
  check, `unlink_at` on a directory frees a live directory chain. Without the name checks, names
  with `:` or `*` would be written to the volume. Demonstrated: five mutations (each guard deleted)
  left `cargo test -p fs-server-fat --lib` fully green. No FAT test mentions NotEmpty, though the
  ext4 rmdir test covers it. Lens: tests. medium.

- [ ] **fs-15** `userspace/fs-server-ext4/src/ext4.rs:804` — The allocator's `start + len <
  capacity` bound and `.min(gfree)` cap in `alloc_run`, and the `!bit_clear` guard in `free_run`,
  are untested. Without the first, a short final group's spare bitmap bits can be allocated as
  blocks past the end of the filesystem. Without the second, the group's free count wraps below
  zero. Without the third, a double free counts its blocks free twice. An image from another tool
  with spare bits clear, or a free count that disagrees with its bitmap, would then get an extent
  off the device. Demonstrated: three mutations, each leaving `cargo test -p fs-server-ext4 --lib`
  green. Lens: tests. medium.

- [ ] **fs-16** `userspace/libfsserver/src/cache.rs:195` — `MetaCache::forget` has no test for a
  freed range whose end is not 4 KiB-aligned, and the rounding of the end of the range is
  unchecked. FAT's data region need not start on a 4 KiB boundary. If a freed cluster ends mid-block
  and the partial block stays cached, the kernel writes file data there behind the server's back. A
  later `read_meta` of that block (reused as metadata) then sees stale bytes. Demonstrated:
  changing `(offset + len).div_ceil(BLOCK)` to `(offset + len) / BLOCK` left libfsserver,
  fs-server-ext4 and fs-server-fat `--lib` green. Lens: tests. low.

- [ ] **fs-17** `userspace/libfsserver/src/server.rs:329` — `maybe_grow` notes only
  `FsError::ReadOnly`. Every other create, grow or truncate failure falls through, and the resolve
  answers success with the file mapped at its old size. The spec says the reply's content_len is
  the new size, and the kernel installs the reply's size without comparing. On a full disk, a
  `libfs::write_file` or `copy_file` of a new file gets a create that holds 0 bytes, and the map
  then fails, so the caller reports InvalidArgument rather than no-space. A FAT create with a
  forbidden name returns NotFound instead of InvalidArgument. The only silent-success case is
  ext4 with sub-page blocks, which is the recorded TODO(ext4-subpage-runs). Unverified: boot a
  full 1 KiB-block ext4 and run `copy_file`; it shows whether the copy is reported as success when
  short. Lens: error-paths. low.

- [ ] **fs-18** `userspace/libfsserver/src/server.rs:614` — When `reply_dir_handle`'s send fails,
  `open_dir_session` closes the session and the client endpoint but sends no error reply, while the
  other failure arms reply WouldBlock or KernelError. A forwarded lookup has no deadline, so the
  caller's PendingOperation never completes, and a `Dir::open` in Files, nxsh or a libfs walk waits
  forever with nothing logged. Unverified: injecting a failing `SYS_CHANNEL_SEND` for that reply in
  a boot shows whether the lookup hangs or completes with an error. No existing gate produces the
  trigger (a full queue or kernel out of memory). Lens: error-paths. low.

- [ ] **fs-19** `userspace/libfs/src/lib.rs:775` — `copy_tree` has no check that the destination
  lies inside the source, and its listing helpers pass server-supplied names to `join()`
  unchecked. (a) `copy_tree(ns, "/a", "/a/b")` creates `/a/b`, then re-lists `/a`, which now holds
  `b`, and recurses until MAX_TREE_DEPTH (32) fails with TooDeep. That leaves about 32 nested
  junk directories; coreutils only rejects target == src and nxfiles has no guard. (b) A name
  containing `/` from a crafted disk goes into `remove_tree` and `copy_tree` unchecked, so they act
  on a different file in a subdirectory than the one listed. Names `.` and `..` are filtered;
  embedded `/` is not. Unverified: (a) holds by reading, and `copy -r /dir /dir/sub` in a boot
  would confirm it. (b) needs a crafted stick with an entry named `a/b`, and whether a server can
  list such a name is unchecked. libfs has only pure-helper host tests. Lens: tests. low.

What held up: the kernel's `validate_path` rejects `.`, `..`, empty components and paths over 1024
bytes before they reach a server, so the path-form calls (`rename_path`, create, grow) never see
dot entries. The ext4 scans `dir_lookup`, `dir_remove` and `read_dir_inner` all bound `name_len`
against the block. `extent.rs` checks magic, max, depth, order and `on_device`, and depth strictly
decreases, so a self-referencing tree cannot recurse. `free_run` refuses runs outside the device.
`read_inode` and `touch_file` check the inode number. A write-failure sweep of ext4 create, mkdir,
unlink, rename and directory move found no block both free and in use. `create_file`, `mkdir_at`
and `rename_at` give back what they took when the link fails. A lost final grow write is repaired
by the next grow. FAT is sound: every chain walk is bounded by the cluster count, `change()`
refuses read-only mounts and flushes after a failure, `Cache::allocate` gives back what it took,
`rename` uses the `dir::within` ancestor walk, and long-name ordinals and checksums are checked.
`MetaCache` writes through or drops cached copies on failure. The cache, the FAT crash-ordering
test, the long-name checks, the BPB checks, the read-only flag on file-blocks replies, the ext4
zero-fill on grow, the unlink-directory, rmdir NotEmpty and rename-into-subtree guards, and the
mount and unmount state bits each failed a test when broken.

### drivers — main at 510381f4, 2026-10-20

Lenses: logic, error-paths, trust-boundary, tests, architecture; returned nothing: none.
Found 24, refuted by a boot 0, reported 16 (24 reports merged into 16 distinct defects).

- [ ] **drv-1** `kernel/src/drivers/xhci/storage.rs:688` — A bulk-only read whose data stage ends
  SHORT_PACKET has its real residual thrown away: `Disks::on_transfer` never gets the event's
  residual, so only the CSW residue counts. A stick that moves part of the data, then answers
  Passed with residue 0, completes the IRP as success with the full length. A malicious or broken
  stick holds a FAT or ext4 file, and a process faults a page of it. The fill reads 32 pages
  (READ(10) over the faulting page plus read-ahead). The read-ahead frames come from `buddy_alloc`
  unzeroed (file_object.rs:503, :546). The stick sends a short packet, then a CSW with Passed and
  residue 0, and `settle` marks every page Ready on status 0. The reader then sees stale physical
  memory: other processes' freed pages, kernel heap, and the buddy free-list link in each page's
  first 8 bytes. Copying the file back onto the stick sends that data out. Demonstrated by two host
  probes. (1) A 16-fragment 64 KiB read, a SHORT_PACKET event on the first fragment, then a CSW with
  status 0 and residue 0, gave `Complete { status: 0, transferred: 65536 }`. The existing test
  `a_reads_status_is_asked_for_when_its_data_ends` (storage/tests.rs:256) pins that behaviour as
  intended. (2) In file_object.rs, every free frame was dirtied with 0xA5 and freed, then
  `start_fill` ran with read-ahead on a recording device and the IRP completed as a full success.
  19 read-ahead pages came back holding the old 0xA5 bytes. AHCI has the same shape, since
  `retire()` reports `irp.length` and never reads PRDBC; that half is unverified for SATA firmware.
  Linux keeps the larger of the actual and reported residue. Lenses: error-paths, trust-boundary.
  High.

- [ ] **drv-2** `kernel/src/drivers/ahci.rs:869` — `poll_complete_inflight` discards
  `wait_command_polled`'s timeout. `take_retired` then judges the command by TFD.ERR alone, so a
  command still running (CI bit 0 set, BSY set) retires as Success with the full length, and slot 0
  is marked idle while the HBA owns it. `identify()` (line 462) has the same flaw: on a timeout it
  drops its DmaBuffer while the IDENTIFY is outstanding. At boot `gpt::init` reads LBA 0-1 through
  `read_blocking` and `ahci_poll`. A slow spin-up or a marginal sector takes over `POLL_TIMEOUT_NS`
  (1 s). `read_blocking` returns true with a zero or partial buffer. The disk then reads as "no
  table", no partitions are published, and init cannot find `gpt-partlabel:nitrox-root`. A late
  completion DMAs into freed frames. The next submit rebuilds slot 0 and writes CI=1 under the
  running command, so a write is reported done and never reaches the disk. Demonstrated by two host
  probes. With PX_CI=1 and PX_TFD=BSY, `take_retired` then `retire` gave Success and
  transferred=1024 (512 in the second probe), with CI still 1 and inflight null. A following
  `submit()` of a Write built its PRDT and CFIS over the busy slot. The timeout itself cannot fire
  on the host (`Timer::read_ns` is 0), so that half is from reading the code (wait_command_polled
  :591-606). Its comment claims the IRP completes "with its error status" on a timeout. Lenses:
  logic, error-paths. High.

- [ ] **drv-3** `kernel/src/drivers/gpt.rs:236` — `/dev/disk/by-partlabel/<label>` goes to the
  first boot-time disk published, and a later disk with the same label fails to bind
  (AlreadyBound, one log line). `drivers::probe` registers the AHCI disk before the RAM-disk
  modules (drivers/mod.rs:28-56), and `gpt::init` walks the table in that order. The BOOT flag is
  never consulted, and init (init/src/main.rs:322-331) looks up the label with no BOOT check. Boot
  the live stick on a machine whose internal SATA disk has a GPT partition labelled `nitrox-live`
  (tools/xtask/src/image.rs:353). The internal partition wins the name, and init mounts it as `/`.
  Then `/bin/service-mgr` reads `/system/services.toml` from that disk, so the attacker-written disk
  supplies the live session's services, binaries and user database. check-recovery runs exactly
  this live-boot-with-internal-disk setup. Demonstrated: a host probe recorded the internal disk's
  PartEntry and then made the live disk's bind as `bind_one` would. It gave `Err(AlreadyBound)`,
  and the label named the internal disk, not the live one. The end-to-end mount was not run; it
  needs a live-image boot with such an AHCI disk. Lenses: trust-boundary, architecture. High.

- [ ] **drv-4** `kernel/src/syscall/table.rs:2393` — A FILE_BLOCKS resolve reply discards the
  rights of the block DeviceNode handle the server transfers (`tr.rights`). The MEMOBJ path
  intersects them (`pl.requested & tr.rights`, :2158). Page-cache writeback (file_object.rs:883)
  dispatches Write IRPs through `dispatch_block_irp_into_frames`, which leaves rights to the caller
  (block.rs:96-99), so READ and WRITE are never checked. storage-service gives a server of a
  read-only mount a handle narrowed to READ|TRANSFER|DUPLICATE (mounting.rs:132-136). A buggy or
  compromised server, say one taken over by a crafted filesystem, answers a resolve with
  FILE_BLOCKS and no FILE_BLOCKS_READ_ONLY, and supplies block runs of its choosing. A client that
  maps MAP_WRITE gets a writable FileObject, and its dirty pages are written to a device the
  server was denied WRITE on, at offsets the server picks. The only write guard is the server's
  own flag (table.rs:2433-2434). Unverified: it was read, not run. A QEMU test whose fs server
  holds a READ-only partition handle and replies FILE_BLOCKS without the flag would settle it. The
  client maps MAP_WRITE, writes and syncs, then the test compares the device bytes. Lens:
  architecture. High.

- [ ] **drv-5** `kernel/src/drivers/ahci.rs:822` — The AHCI driver has no error recovery and no
  runtime deadline. The ISR retires slot 0 only when PxCI bit 0 is clear. AHCI 1.3.1 §6.2.2.1 has
  software clear PxCMD.ST to reset PxCI after a task-file error. `stop_port` and `start_port`
  (:421-439) run only from init, `issued_at` feeds statistics only, and PX_IE enables only
  DHRE|TFEE. A SATA disk returns UNC or IDNF, for example a medium error on a bad sector or a read
  past the disk's end. The driver does not check offset against `sectors`, so any whole-disk handle
  holder can cause one. TFES fires, the ISR acks PxIS and retires nothing, and the PO never
  completes. Later IRPs queue in the 32-deep PendingRing and then fail IoError. libfsserver waits
  with deadline u64::MAX (userspace/libfsserver/src/disk.rs:23), so fs-server-ext4 on the root
  partition hangs until reboot. QEMU's `ahci_cmd_done` reportedly clears CI even on error, so no
  gate can show this. Unverified: on a real SATA machine, read an LBA past `sectors` through
  `/dev/blk/<disk>`, or use a drive with a pending sector, and see whether the PO completes and
  later I/O proceeds. Lenses: logic, error-paths. High.

- [ ] **drv-6** `kernel/src/drivers/ahci.rs:726` — Nothing refuses a block request whose offset or
  length is not a multiple of the logical block size, or whose offset is past the device's end,
  though `docs/spec/io-operation.md` (lines 41-45, 96-102) and device-node.md say both are
  InvalidArgument synchronously. `sys_io_submit` (table.rs:1620-1660), `dispatch_block_irp` and
  `partition_rebase` check only the buffer range. AHCI's `issue_locked` truncates:
  `lba = offset/512`, `count = (length/512) as u16`, LBA bits from 48 up drop, and `retire` reports
  `transferred = irp.length`. A filesystem server on a partition handle submits Write offset=100,
  length=256, or a write of the partition's last 256 bytes. The call is accepted and issues WRITE
  DMA EXT at LBA 0 (or the partition's last LBA) with count 0, which ATA reads as 65,536 sectors,
  against a 256-byte PRDT. A length of 768 moves one sector and reports 768. Offset 1000, length
  1000 reads sector 1 only. A whole-disk write at byte offset (2^48+1)*512 lands on LBA 1. QEMU
  finishes the count-0 command having moved nothing. On real hardware it may overrun into the
  neighbouring partition or stall the port, which with drv-5 stalls every partition on the disk.
  That hardware outcome is unverified. The RAM disk also accepts misaligned requests, and xHCI
  refuses them only through the PO (storage.rs:439-445), not synchronously. Demonstrated by host
  probes. In ahci tests, `issue_locked` for Read offset=0, length=256 gave `cmd=0x25 lba=0 count=0
  prdt_bytes=256`, and for Write offset=100, length=256 gave `cmd=0x35 lba=0 count=0`. A Write at
  offset 1000, length 100 gave count 0, and `retire` reported transferred 100. Through
  `dispatch_block_irp` under a partition, a Write at offset span-256, length 256 returned Ok and
  reached the disk at byte 1099520, which AHCI would issue as the partition's last LBA with count
  0. A past-end offset dispatched Ok and was written to lba48 1. Lenses: logic, error-paths,
  trust-boundary, tests, architecture (five reports of one defect). Medium.

- [ ] **drv-7** `kernel/src/drivers/xhci/hub.rs:273` — `hub::depart` calls `storage::finish`
  unconditionally after `release()` returns Kept. That completes the in-flight or stranded IRP
  PeerClosed while the slot is still enabled and its bulk endpoints may still move the IRP's data.
  storage.rs:1393-1396 documents `End::Stranded` as "completed once the slot is disabled". A
  stick's command misses its deadline, and `recover()`'s Reset or Stop Endpoint goes unanswered for
  1 s. That sets `x.wedged`, so the result is `End::Stranded`, then `Recovered::Ended`, then
  `depart`. `release()` returns Kept and leaks DeviceMem on purpose, but `finish()` then completes
  the IRP. Its buffer, a user MemoryObject or page-cache frames (`settle(.., false)` calls
  `buddy_free`), is released while the controller may still DMA into it. A failed Disable Slot
  takes the same path. Unverified: inject a Disable Slot failure, or set `x.wedged`, during a USB
  read and check that the IRP completes while the slot stays enabled. The fix shape is to leak the
  held IRPs on the Kept path, as the memory is leaked. Lens: error-paths. Medium.

- [ ] **drv-8** `kernel/src/drivers/ahci.rs:321` — `drivers::probe` calls `ahci::init` for every
  AHCI-class function, but `AHCI.store(disk)` is unconditional and the ISR, `ahci_poll` (which
  ignores its ctx argument), `poll_complete_inflight` and the DPCs all read that single global. A
  machine has two AHCI controllers with a disk each. Disk 2 overwrites disk 1, so disk 1's polled
  boot reads and its interrupts are handled against disk 2's state. Disk 1's `read_blocking` sees
  inflight null and returns false, its table "did not read", and its IRP stays in flight so later
  IRPs queue and then fail IoError. If disk 1 holds `nitrox-root`, `/` cannot mount. Unverified:
  QEMU with two `-device ahci` controllers, each with a drive; check for the "partitions: the table
  did not read" line. No xtask gate has that configuration. Lens: trust-boundary. Medium.

- [ ] **drv-9** `kernel/src/drivers/partitions.rs:97` — Test gap in untrusted-input guards: the GPT
  entry-size range check, the `.min(entries)` clamp on the array and the `.min(GPT_ENTRY_BLOCKS_MAX)`
  cap are each deletable with all 14 partitions tests passing. The `is_multiple_of` half of the size
  guard is also deletable with nothing failing. The existing test uses only entry size 96, which
  the divides-a-block check alone rejects. Without the range check, entry size 64 panics slicing
  `e[56..128]` (partitions.rs:149). Without `.min(entries)`, a header declaring 5 entries with a
  stale sixth in the block publishes a phantom sixth partition. Without the cap, a header declaring
  0xFFFFFFFF entries asks `try_reserve` for 512 GiB, so the disk reads as NoMemory instead of its
  first 128 entries. Demonstrated: temporary tests showed the first two (panic; left 6, right 5),
  restored by `git checkout`. The third is from reading. Lens: tests. Medium.

- [ ] **drv-10** `kernel/src/io/block.rs:526` — Test gap on the partition sandbox boundary:
  `partition_rebase`'s three overflow guards (`block_count.checked_mul(sector_size)`,
  `offset.checked_add(length)` and the final `checked_add` of `start_lba*sector_size`) can each
  become `wrapping_*` with all 8 io::block tests passing. The one existing test uses small values.
  If one regresses, a request with offset near u64::MAX and a length that wraps back inside the
  span passes the bounds check and is forwarded at a wrapped absolute offset, and AHCI truncates
  the LBA to 48 bits. That could land outside the partition, and CI would not notice. The final
  `checked_add` alone currently catches one wrap shape, so the guards cover each other and the pair
  is untested. Demonstrated by per-guard mutation: each left 8/8 passing. Lens: tests. Medium.

- [ ] **drv-11** `kernel/src/drivers/xhci/desc.rs:131` — Test gap: the `len >= 9` guard on
  interface descriptors in `interfaces()` is not pinned, and removing it passes all 19 desc tests.
  The `wTotalLength`-versus-`b.len()` bound, the `len<2` check in `descriptors()`, the
  `(b[0] as usize) < 9` check in `configuration_total` and the first-endpoint-wins check in
  `hid_interfaces` also survive deletion. With the guard gone, a configuration whose last
  descriptor is an interface descriptor with bLength 4 at the end of the buffer (config 9 bytes,
  then `[4,4,0,0]`, wTotalLength 13) makes `interfaces()` index `b[here+4]` and panic (desc.rs:133).
  That is a kernel panic from a USB device's bytes if the guard regresses. Demonstrated by a
  temporary test `probe_short_iface` with the guard removed, then restored. Lens: tests. Medium.

- [ ] **drv-12** `kernel/src/io/block.rs:127` — Test gap: `dispatch_block_irp`'s own `max_frags`
  check, which the code calls the PRDT-overflow protection, has no test. Changing `>` to
  `> max_frags + 1` passes all io::block tests, and only the `_into_frames` variant is tested.
  AHCI's backstop in `submit()` would still refuse, but the USB path only clamps
  (`frags.len().min(MAX_FRAGS)`) and would silently truncate a transfer that the CSW then reports
  whole. Deleting the storage.rs clamps (in `issue()` and `start()`) also fails no test. Only the
  boot-time self-test (drivers/mod.rs:147) exercises the limit. Demonstrated by mutation: io::block
  8 passed, storage 27 passed. Lens: tests. Low.

- [ ] **drv-13** `kernel/src/drivers/hid/descriptor.rs:222` — Test gap: several of `walk()`'s
  untrusted-input rejections have no test. Each of these mutations passed all 14 descriptor tests:
  a truncated item at the end, a truncated long item, Pop with nothing pushed (`checked_sub` to
  `saturating_sub`), a Push past `STACK_MAX` (to modulo), an Input item in a seventeenth report ID,
  the `done` flag that stops a second mouse or keyboard collection, and the array-already-set check
  in `take_input` (a second key array). 7 of 13 mutations survived. A regression would read a
  layout from a corrupt descriptor instead of falling back to the boot protocol: a second
  collection's fields would overwrite the first's, or a mouse's offsets would come from state a
  Pop never restored. Demonstrated by mutation. Lens: tests. Low.

- [ ] **drv-14** `kernel/src/pci/mod.rs:195` — Test gap: the multifunction mask in
  `decode_function` (`header_type_raw(cfg) & 0x7F == 0`) is not pinned, and replacing it with
  `== 0` passes all 28 pci tests. The I/O BAR address mask (`0xFFFF_FFFC` versus `..F0`) and the
  low-nibble mask on a 64-bit BAR's low dword are also unpinned. If the multifunction mask
  regresses, every function of a multifunction device (header type 0x80, such as the ICH9 AHCI at
  00:1f.2 and many xHCI controllers) gets empty BAR windows. The driver finds no MMIO BAR and
  silently declines the controller, and only a boot on such hardware would show it. A dropped bit 2
  in the I/O mask would size 4- or 8-byte I/O BARs wrongly. Demonstrated by mutation. Lens: tests.
  Low.

- [ ] **drv-15** `kernel/src/drivers/hid/keyboard.rs:132` — Test gap: the key array's
  `value >= a.logical_min` filter and the `FIRST_KEY` lower bound in `hold()` are not pinned.
  Replacing the filter with `true` passes all 14 keyboard tests, because every test layout uses
  `logical_min` 0. A report descriptor with `logical_min` 1 sends 0 in an empty slot. Without the
  filter `hold()` gets `first + (0 - 1)`, which can land on a real usage and press a phantom key on
  every report. Demonstrated by mutation. Lens: tests. Low.

- [ ] **drv-16** `kernel/src/power.rs:89` — `flush_every_disk` walks `/dev/blk/<n>` from 0 and
  stops at the first index `find_block_device` answers None. A departed device's served index is
  never reused and resolves to None, so the walk ends at the first departed disk or partition and
  flushes nothing registered after it. Plug in USB stick A, then B (`/dev/blk/k+n`), then unplug A
  (or rescan it so its partitions depart) and shut down through `sys_power`. The walk ends at A's
  first gone index, B never gets SYNCHRONIZE CACHE, and the log reports "flushed X of Y disks"
  counting only the disks before the gap. storage-service and nxinstall/disk flush their own writes
  first, so this removes the kernel's last backstop, not every flush. Demonstrated: a host probe
  in device.rs tests with sticks A and B, then A departed, then power.rs's
  `while let Some(d) = r.block(index)` walk gave `walk visited [(0, false), (1, false), (2, false)];
  stick B is /dev/blk/Some(4)`. The existing test
  `a_departed_device_is_found_by_no_path_and_its_index_stays_taken` already asserts that `block(0)`
  is None while `block(1)` is Some. Lens: architecture. Low.

What held up: the reviewers' mutations on input/ring.rs (6 of 6) and the device.rs registry (4 of 4)
were all caught, as were 13 of 16 on pci/mod.rs, the partitions.rs end-of-disk, first==0 and
protective-entry rules, xhci/storage.rs `irp_command` offset, lba+count and zero-block checks, the
hid/keyboard.rs roll-over and report-id rules, hid/descriptor.rs size overflow and zero-size
skip, the desc.rs device-descriptor length and bulk-type checks, and the partition-window
rescan/eject refusal. Read and found sound: `partition_rebase` is overflow-checked and a retired
window refuses everything with PeerClosed. `dispatch_block_irp` and the `_into_frames` variant
refuse an over-long fragment list before an IRP exists, and every early Err precedes the IrpBox, so
the caller's PO rollback is complete. The USB `irp_command` refuses misaligned, empty, past-end
and >u16 requests. The descriptor walks in xhci/desc.rs and hid/descriptor.rs are bounded by
`wTotalLength`, each descriptor's length, the 4 KiB report limit and the push/pop depth.
`string_into` is bounded. The HID decode, the hub config and report reads, and the CSW checks are
bounded by what was received. The PCI and xHCI capability walks are bounded and cannot loop. The
GPT/MBR parser uses checked arithmetic, `decode_partlabel` rejects '/' and control bytes, and
`validate_path` refuses '.' and '..'. The registry and I/O snapshot layouts match the userspace
reader field for field, with asserted sizes. No `arch::x86_64` references appear in drivers, pci or
io. Nothing frees in a DPC or ISR. The input and console rings keep whole records, and
`/dev/registry/changes` waiters are capped at 4. The recorded deferrals (gpt-crc,
usb-departed-records, block-transfer-split) were not reported.
