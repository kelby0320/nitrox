# The bug bash: findings and their triage

**Status: started 2026-10-14; triaged 2026-10-22 with the maintainer — the bash's eight runs,
its two re-runs and what PR #365's trials left: 154 findings, 129 once a defect several runs found
is one, 68 to fix in Part D, 60 later (4 in Part E, 56 in Part H) and 1 not. The same day, the rest:
the five found before the bash, the trials' unchecked test gaps and doc claims, and the laptop's
two bugs, five to fix, one already fixed and three later. Group A fixed 2026-10-22, with sched-7
and kobj-9 from groups E and B and the test gaps that ride with them; sched-10 stays open for one
mutation. Group B fixed 2026-10-22: the twelve capability and rights leaks, and the test gap that
rides with them, kmem-11. Group C fixed 2026-10-23: ext4's findings and those in the code both
servers share, then FAT's, with fs-3's two halves and the trials' doc claims. Group D fixed
2026-10-24: the block layer's bounds, AHCI's recovery, a kept USB slot's commands, the shutdown's
flush walk and the hot-plug window.** Part D of
[`pre-phase-7.md`](pre-phase-7.md) fills it: the two bugs found on the laptop, what PR #365's two
trial reviews found that `main` still has, and each `/bug-bash` run, a section a subsystem.

## How a finding is kept

The author appends each run's findings as its report gives them, a checkbox each:

- `[ ] **ID** file:line — the defect in a sentence. Scenario. Evidence. Lens. Severity.`

Each run's findings come as one section, headed with the subsystem and the commit of `main` it
hunted, in the shape `.claude/skills/bug-bash/report.md` gives.

The ID is the subsystem's short name and a number, such as `kobj-3`. Triage, with the maintainer,
adds one word after the severity: **fix** (in Part D), **later** (in Part H of
[`pre-phase-7.md`](pre-phase-7.md), or in Part E where it says so), or **no** (with the reason).
**This file is a later finding's record**: a part of the plan schedules it, so it takes no
`TODO(tag)` and no `deferred-decisions.md` entry. A finding another run found first names it —
**fix**, as kobj-1 — and goes with it. A test gap marked fix is written with the fix it names,
which changes the code it guards. A finding is reproduced before it is fixed, and its box is
ticked with the commit that fixed it and the test that fails without it.

## Findings

### Found on the laptop — Phase 6 Part H's runs, 2026-10

Two bugs a person met on the laptop, not found by a run. Neither is reproduced yet, and each is
reproduced before it is fixed.

- [ ] **laptop-1** `userspace/nxterm` — output appears only after the pointer moves, every time in
  the laptop's runs 3 and 4. The QEMU dry runs of Phase 6 Part H may reproduce it if run from
  `nxterm`; those recorded were typed on serial, which never passes through it, so trying that
  comes first. Unverified. **Fix**, reproduced first.
- [ ] **laptop-2** `userspace/desktop-shell` — the Applications menu's search clears while typing,
  now and then. A reliable way to make it happen comes first. Unverified. **Fix**, reproduced first.

### Found detailing Part C — main at 226a54f6, 2026-10-19

These were found by the author while measuring Part C, not by a run. No lens hunted, and none
was demonstrated: each is read from the source, and is reproduced before it is fixed.

- [x] **desk-1** `userspace/compositor/src/input.rs:449` — a press on a maximized window's title
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
    **Fix**: done by `7fcda1ff`, in PR #388's review: nothing moves until the pointer has gone 4
    pixels; `a_click_that_shakes_by_less_than_the_threshold_moves_nothing_and_snaps_nothing` holds
    it.
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
    Logic. Medium. **Later.**
- [ ] **nxsh-2** `userspace/nxsh/src/eval.rs:1268` — `cd` and `exit` each read their first
  positional argument and ignore the rest.
  - `cd a b` goes to `a`, and `exit 1 2` exits with 1, each saying nothing, against the shell's
    fail-loud rule.
  - `` `cd` takes one path `` fires only for an argument that is not positional (`:1280`), and so
    does `exit`'s check (`:1293-1318`).
  - Demonstrated by a temporary host test: `cd /a /b` left `PWD` at `/a`, and `exit 1 2` exited
    with 1, neither with an error. Logic. Low. **Later.**

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
    Medium: it fails CI on branches that touch none of it. **Fix.**

### Found building Part C.1 — branch pre-phase-7/part-c1-paths at 2116dbcf, 2026-10-19

One flake on a branch that changed only the shell and `rename`, in a path it does not touch.

- [x] **usb-1** `tools/xtask/src/usb.rs:120` — a USB keyboard hot-plugged after the hub thread's
  first round sometimes never enumerates.
  - Scenario: `test-qemu --kvm` adds `usb-kbd` over QMP as soon as `usb: first round` appears
    (`usb.rs:58-65`).
  - In the failing run, every device present at boot enumerated (the registry lists six), but no
    line for the added keyboard followed, so the guest missed the port's change event, or never
    acted on it.
  - Demonstrated: failed once in three local `test-qemu --kvm` runs; the other two passed, with
    the guest unchanged. Error paths. Medium: a device plugged in later is not seen.
    **Fix**, with group D.
  Fixed in `2a22622e`: a port's change is decided by the port read after its clear
  (`hub::connect_change`), since every empty port carries a stale change from the controller's reset
  and a device connecting before its clear lost its own; fails without it:
  `a_device_connecting_as_a_stale_change_is_cleared_is_seen`. Reproduced 1 in 90 runs (under host
  load); that the window is what failed it fits but was not caught by a probe in 70 loaded runs.

### kernel-objects — main at 510381f4, 2026-10-20

Lenses: logic, error-paths, trust-boundary, tests, architecture; returned nothing: none.
Found 24, refuted by a boot 0, reported 13.

- [x] **kobj-1** `kernel/src/object/timer.rs:206` — Dropping a Timer never removes its deadline-heap
  entry, so the entry dangles. `Timer::drop` only asserts that no waiters remain, and
  `dispatch_destroy` just frees the box. The heap entry is a raw address, not an ObjectRef. A
  process calls `sys_timer_create` and `sys_timer_set(h, now+X, interval)`, then
  `sys_handle_close(h)` or exits with the timer armed. When X elapses, `fire_expired_deadlines`
  in the timer IRQ writes `in_heap`, reads the waiters and re-arms through freed memory. A
  periodic timer keeps doing this every interval. Demonstrated: a host probe armed a one-shot
  timer, dropped it, and the heap kept an entry whose target was the freed address. A new timer
  reused the slot, and firing the stale entry at now=200 disarmed it (its deadline went
  1_000_000 -> 0). The close and exit paths remove only `DeadlineKind::Thread` entries. logic,
  architecture. High. **Fix.**
  Fixed in `fcc06ce8`; fails without it:
  `a_timer_destroyed_while_armed_leaves_nothing_in_the_heap_naming_it`.
- [x] **kobj-2** `kernel/src/syscall/table.rs:2394` — `build_and_install_file_blocks` keeps the
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
  its own probe did not run. logic, error-paths, trust-boundary. High. **Fix.**
  Fixed in `4043ae92`; fails without it:
  `a_file_blocks_file_is_writable_only_as_far_as_its_server_may_write_the_device` and
  `a_file_blocks_reply_installs_a_file_no_more_writable_than_its_server_holds_its_device`.
- [x] **kobj-3** `kernel/src/object/ipc_channel.rs:99` — An allocation over the buddy allocator's
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
  not run. architecture. High. **Fix.**
  Fixed in `e56c6612`; fails without it: `an_order_past_the_largest_block_is_refused`,
  `a_large_alloc_past_the_largest_block_is_refused`,
  `copying_an_image_past_the_largest_block_is_refused`,
  `a_file_blocks_header_claiming_more_runs_than_its_body_holds_is_refused` and
  `a_block_map_the_page_cache_cannot_use_is_refused`; the channel depth falls to 512, the deepest
  whose ring fits.
- [x] **kobj-4** `kernel/src/mm/elf.rs:182` — `parse_phdr` computes `off + ELF64_PHDR_SIZE`
  without a checked add. `load_elf` only checks `e_phoff + i*56`, so an `e_phoff` within 56
  bytes of `u64::MAX` overflows, and with overflow-checks on in the dev profile the kernel
  panics. Any process passes a MemoryObject, or a file it can map, holding a valid ELF64 header
  with `e_phnum`=1, `e_phentsize`=56 and `e_phoff`=`u64::MAX-8` as `SpawnArgs.image` to
  `sys_process_spawn`. Demonstrated: a host probe with those header bytes panicked with
  'attempt to add with overflow' at elf.rs:182:22. logic. High. **Fix.**
  Fixed in `badc91cd`; fails without it:
  `a_program_header_offset_at_the_top_of_the_range_is_truncated`.
- [x] **kobj-5** `kernel/src/syscall/table.rs:2160` — A forwarded-resolve reply installs its
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
  error-paths, trust-boundary, architecture. Medium. **Fix.**
  Fixed in `912e0dda`; fails without it: `a_reply_for_a_client_that_exited_leaves_no_handle_behind`
  and `a_forwarded_reply_for_a_client_that_exited_pins_nothing`.
- [x] **kobj-6** `kernel/src/object/ipc_channel.rs:366` — An endpoint sent over its own channel
  keeps a reference to itself in its own receive ring, so it is never freed. `sys_channel_send`
  (table.rs:1934-1937) checks only TRANSFER on each handle and does not refuse the channel's own
  endpoint. A process calls `sys_channel_create(depth=1024)` for A and B, then
  `sys_channel_send(A, msg, [B])`, then closes A or exits. B's refcount never reaches zero, so
  `IpcChannel::drop` never runs. B's ring (about 4 MiB at depth 1024) and every object queued in
  it stay allocated, up to seven MemoryObjects of up to 16 MiB per queued message. Repeating this
  from short-lived processes exhausts kernel memory. Demonstrated: a host probe pushed
  `send_push(a, msg, [b])` and dropped both creation references. It asserted two destroys and
  got 1 (left 1, right 2). error-paths. Medium. **Fix.**
  Fixed in `ff52c1b0`, its own endpoint, and in `f8c3bc43`, an endpoint already holding one, which
  PR #391's review found the first left open through two channels; fails without them:
  `a_send_that_could_close_a_cycle_of_endpoints_is_refused`,
  `an_endpoint_holding_another_in_a_held_send_is_refused_too`, and boot-probe's transfer check
  under `test-qemu`. A cycle through a namespace binding remains: `TODO(ipc-handle-cycles)`.
- [x] **kobj-7** `kernel/src/syscall/table.rs:1620` — `sys_io_submit` on a block device does not
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
  on QEMU, so the hang or DMA overrun is not shown. trust-boundary. Medium. **Fix.**
  Fixed in `bc9731b9`, in the block layer for every driver: part of a block refused
  `InvalidArgument` before an IRP exists, a range past the end completing its operation
  `InvalidArgument`; fails without it: `a_block_request_off_its_devices_blocks_reaches_no_driver`,
  and boot-probe's `block_bounds_test` on the root partition (test-qemu).
- [x] **kobj-8** `kernel/src/syscall/table.rs:855` — `sys_memory_map` requires only the MAP_*
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
  said medium); ranked low as latent. **Fix.**
  Fixed in `48cecf0b`; fails without it: boot-probe's `map_needs_read_test`.
- [x] **kobj-9** `kernel/src/syscall/table.rs:1934` — `sys_channel_send`, and
  `sys_process_spawn`'s handle loop at 446, look up each listed handle separately and do not
  reject a handle listed twice. One handle sent as `[H, H]` arrives as two handles, and the
  second `close_and_release` fails silently. A holder of H with TRANSFER but not DUPLICATE
  (service-mgr hands out LOOKUP|TRANSFER namespace handles) thus copies it. This contradicts
  `handle-encoding.md:199` and the move-only transfer semantics in `ipc-message-format.md:127`.
  Spawn with `handles=[H, H]` does the same, but only the channel path was run. Demonstrated: a
  boot probe sent `[H, H]`, count=2, with a DUPLICATE-less INSPECT|TRANSFER memory handle over a
  loopback channel. Send returned 0, and recv returned 0 with count 2. error-paths, tests. Low.
  **Fix.**
  Fixed in `ff52c1b0`; fails without it: `a_handle_listed_twice_is_refused_by_a_send_and_a_spawn`,
  and boot-probe's transfer check under `test-qemu`. A spawn's duplicate grant without `DUPLICATE`,
  which PR #391's review found copied a handle too, in `ddfe3b4b`: boot-probe's spawn-grant check.
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
  trust-boundary: low); ranked low. **Later.**
- [ ] **kobj-11** `kernel/src/syscall/table.rs:483` — When allocating the parent's Process
  handle fails after the child thread is queued, `sys_process_spawn` returns an error while the
  child runs. It skips the move-close loop at 493-497, so the parent's move_mask handles stay
  open although the child already holds them. This needs a handle-table segment grow to fail
  after `spawn_user`. The parent sees a failed spawn and may retry, starting a second child.
  Unverified: needs a failure injection in the global table's segment grow and a boot to
  settle. error-paths. Low. **Later.**
- [ ] **kobj-12** `kernel/src/handle/table.rs:588` — No test hands `duplicate()` a `new_rights`
  with bits the source lacks, so replacing `info.rights & new_rights` with `new_rights` passes
  the whole kernel host suite. This is a test gap, not a defect in code that works, and it is
  listed because the break would be rights amplification on `sys_handle_duplicate` (a holder of
  DUPLICATE|SIGNAL could mint TERMINATE; `allocate`'s type check does not stop amplification
  within a type). Demonstrated: the one-line break left 986 passed, 0 failed. The restrict twin
  is caught by `restrict_on_cannot_amplify`. tests. The reviewer rated it high; as a test gap,
  low. **Later.**
- [ ] **kobj-13** `kernel/src/syscall/table.rs:1163` — Several syscall-level guards have no host
  coverage; these are test gaps, not defects. Removing `require_syscap(BIND_NAMESPACE)` in
  `sys_ns_bind`, `direct_bindable(rights)?`, `Rights::LOOKUP` on `sys_ns_derive`, the
  direct-bind `!base.is_empty()` rejection, or `requested & binding_rights` in `install_for`
  leaves the host suite green. So does removing, in `sys_channel_send`, the per-handle TRANSFER
  lookup, the `payload_len > IPC_PAYLOAD_SIZE` reject or `sender_pid`, and removing
  `sys_memory_map`'s `size > max_bytes` check. Unverified: the host-survival claim was not
  re-run, and no per-break boot controls were run. A boot probe for bind-needs-TRANSFER exists
  and passes. Boot runs with each break would show which guards the QEMU probes catch. tests.
  Low. **Later.**

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

- [x] **kmem-1** `kernel/src/mm/elf.rs:128` — `load_elf` returns `e_entry` unchecked, and
  `spawn_user` hands it to `enter_user`'s `iretq`. `sys_thread_create` checks its entry with
  `UserPtr::new`; the spawn path checks nothing. Any process builds a MemoryObject holding an ELF
  with a valid PT_LOAD and `e_entry = 0x8000_0000_0000` (non-canonical), then calls
  `sys_process_spawn`. The `iretq` raises #GP at CPL 0, which `dump_and_halt` turns into a halt of
  the whole machine. Demonstrated: a host probe showed `load_elf` returning Ok for non-canonical
  and kernel-half entries. `cargo xtask test-qemu --kvm` on an Intel i7-10875H, with a hand-built
  ELF swapped into the 15th spawn: the kernel took `#GP` with cs 0x0008, the `iretq` frame showed
  rip=0x0000800000000000 and cs=0x23, and the dump ended "stopping every CPU". Lenses: logic,
  trust-boundary. High. **Fix.**
  Fixed in `badc91cd`; fails without it: `an_entry_outside_the_user_half_is_refused`.
- [x] **kmem-2** `kernel/src/syscall/table.rs:885` — User mappings may reach `USER_VIRT_END`, so the
  last canonical user page (0x7FFF_FFFF_F000) can be mapped executable, by a hinted
  `sys_memory_map` (`end > USER_VIRT_END` is the only bound) or an ELF PT_LOAD. A `syscall` in its
  last two bytes saves the non-canonical return RIP 0x0000_8000_0000_0000, and the `sysretq` exit
  path does not check RCX. On Intel, `sysretq` then faults in ring 0 with the user RSP already
  loaded, and the vector-13 gate has no IST. Demonstrated: a host probe showed `load_elf` accepting
  the PT_LOAD at `USER_VIRT_END - PAGE_SIZE`. `test-qemu --kvm` on an Intel host, with an exec
  PT_LOAD there and a `syscall` in its last two bytes: the kernel stopped at cs 0x0008 inside
  `syscall_entry` with rcx=0x0000800000000000, rsp on the user stack, and the machine halted. The
  serial output was interleaved, so the vector line was garbled. AMD is unaffected. Linux
  reserves this page for this reason. Lens: logic. High. **Fix.**
  Fixed in `badc91cd`; fails without it: `a_segment_in_the_top_page_of_the_user_half_is_refused` and
  `no_way_of_mapping_reaches_the_top_page_of_the_user_half`, each of the four mapping calls reverted
  alone.
- [x] **kmem-3** `kernel/src/mm/addr_space.rs:274` — `AddressSpace::fault_in`'s Anonymous arm
  assumes the PTE is still absent and hits `unreachable!()` when `map_page` returns
  `AlreadyMapped`. `map_file_page` accepts `AlreadyMapped` for the same race. Two threads of one
  process on different CPUs touch the same untouched page of a lazy VMA, such as the initial
  thread's 8 MiB stack. Both fault not-present, the first maps the page, and the second takes the
  AS lock, finds the PTE present and panics the kernel ("fault_in: map_page rejected a fresh
  page-aligned not-present mapping"). Reachable with `sys_thread_create` and no privilege.
  Demonstrated: a host test did `map_vma_lazy`, then `fault_in` twice on one page, and the second
  call panicked at `addr_space.rs:274`. The two-CPU interleaving itself was not booted. Lenses:
  error-paths, trust-boundary, tests. High. **Fix.**
  Fixed in `183111ab`; fails without it:
  `a_fault_on_a_page_another_cpu_just_mapped_is_a_retry_and_keeps_no_frame`.
- [x] **kmem-4** `kernel/src/mm/elf.rs:182` — `parse_phdr` computes `off + ELF64_PHDR_SIZE`
  unchecked. `load_elf` bounds `e_phoff + i*56` but not the `+56`, and every image boots a
  dev-profile kernel (`overflow-checks = true`). A MemoryObject holding an ELF with `e_phoff` near
  `u64::MAX` and `e_phnum >= 1`, passed to `sys_process_spawn` (which needs only MAP_READ on the
  image and no syscap), panics the kernel instead of returning Truncated. Demonstrated: a host test
  with `e_phoff = u64::MAX - 8` panicked with "attempt to add with overflow" at `elf.rs:182`. The
  logic lens noted that a release build wraps into the bounds-checked readers and returns
  Truncated, so only the shipped dev-profile kernel is affected. Lenses: error-paths,
  trust-boundary. High. **Fix**, as kobj-4.
  Fixed in `badc91cd`; fails without it:
  `a_program_header_offset_at_the_top_of_the_range_is_truncated`.
- [x] **kmem-5** `kernel/src/mm/buddy.rs:163` — `BuddyAllocator::alloc` has
  `debug_assert!(order <= MAX_ORDER)` ahead of its graceful `return None`, and slab's `large_alloc`
  passes `order_for_size(total)` unchecked. In the debug-built kernel every image boots, any
  kmalloc over about 4 MiB panics instead of failing. A process makes a 5 MiB MemoryObject
  (`sys_memory_create` allows 16 MiB) and spawns from it: `copy_to_kvec` calls `try_reserve`,
  which reaches `buddy_alloc(11)` and the panic. The image need not be a valid ELF, because the
  copy happens first. Any FileObject executable over 4 MiB (`read_to_kvec`) does the same, as does
  `sys_memory_unmap` of a mapping of 512Ki pages or more. Shipped binaries are all under 1 MiB.
  Demonstrated: a host test with `MemoryObject::try_new(5 MiB)` then `copy_to_kvec()` panicked at
  `buddy.rs:163` ("buddy: alloc order 11 > MAX_ORDER"). Lenses: error-paths, trust-boundary. High.
  **Fix**, as kobj-3.
  Fixed in `e56c6612`; fails without it: `an_order_past_the_largest_block_is_refused`,
  `a_large_alloc_past_the_largest_block_is_refused`,
  `copying_an_image_past_the_largest_block_is_refused`,
  `a_file_blocks_header_claiming_more_runs_than_its_body_holds_is_refused` and
  `a_block_map_the_page_cache_cannot_use_is_refused`; the channel depth falls to 512, the deepest
  whose ring fits.
- [x] **kmem-6** `kernel/src/arch/x86_64/paging.rs:224` — `ensure_table` is a non-atomic
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
  TODO(unexplained-df), and may be a cause of that #DF. Lens: error-paths. High. **Fix.**
  Fixed in `338d744f`; fails without it:
  `a_cpu_beaten_to_an_empty_slot_takes_the_table_linked_first`, deterministic by a test-only hook
  between the read and the link.
- [x] **kmem-7** `kernel/src/syscall/table.rs:855` — `sys_memory_map` requires only the MAP_* bits
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
  run on the host. Lenses: logic, error-paths, trust-boundary, tests. Medium. **Fix**, as kobj-8.
  Fixed in `48cecf0b`, as kobj-8; fails without it: boot-probe's `map_needs_read_test`.
- [x] **kmem-8** `kernel/src/mm/elf.rs:223` — A PT_LOAD's `p_memsz` is bounded only by
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
  Medium. **Fix.**
  Fixed in `badc91cd`; fails without it:
  `an_image_whose_segments_claim_more_than_the_budget_is_refused`.
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
  Medium. **Later.**
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
  not run. Lenses: logic, error-paths. Medium. **Later**, as kobj-10.
- [x] **kmem-11** `kernel/src/mm/addr_space.rs:631` — No test pins `page_flags_for`'s WRITABLE,
  NO_EXECUTE and USER translation, and this function builds every user PTE. A regression that
  drops NX, makes read-only mappings writable or marks kernel mappings user-accessible would
  silently remove W^X and read-only protection, and boot gates probably would not notice.
  Demonstrated: three mutations in `mm::addr_space` all survived, namely the EXEC test forced to
  `false`, the WRITE branch forced on, and the USER branch forced on. Only the WRITE_COMBINING
  branch was killed. Lens: tests. Medium. **Fix**, with kobj-8.
  Fixed in `48cecf0b`; fails without it:
  `a_page_is_writable_executable_and_user_only_as_its_protection_says`, each of the three mutations
  applied in turn.
- [x] **kmem-12** `kernel/src/mm/addr_space.rs:574` — No test catches a leaked or double-freed frame
  on unmap. A regression could leak an Anonymous frame per page, or collect Object and FileBacked
  frames into `frames`, so `sys_memory_unmap` would `buddy_free` frames a MemoryObject or the page
  cache still owns, a double free that corrupts the buddy lists. The
  `matches!(boxed.mapping, Anonymous)` guard in `unmap_covering_deferred` is the only barrier and
  is untested. Demonstrated: three mutations in `mm::addr_space` survived, namely
  `free_vma_pages`'s Anonymous arm changed to `{}`, the `matches!` guard changed to `true`, and
  `capacity - len < pages` changed to `< 0`. Lens: tests. Medium. **Fix**, with kmem-3.
  Fixed in `183111ab`; fails without it:
  `an_unmap_frees_an_anonymous_mappings_frames_and_never_an_objects`; its third mutation by
  `e56c6612`'s `unmapping_an_object_needs_no_room_for_frames_and_an_anonymous_mapping_does`.
- [x] **kmem-13** `kernel/src/mm/elf.rs:218` — The ELF loader's `p_filesz > p_memsz` check, its
  `file_end > bytes.len()` check and its `aligned_end > USER_VIRT_END` bound have no test, and
  neither does PF_W/PF_X mapping into protection bits. ELF bytes come from a disk or filesystem
  server, which is not trusted. A regression dropping a check gives a kernel panic on a malformed
  image ("just-mapped virtual address must translate", or a slice index), and one dropping the
  flag mapping gives W+X segments. Demonstrated: five mutations in `mm::elf` survived, namely the
  `p_filesz > p_memsz` check forced to false, the `file_end` check forced to false, `aligned_end`
  loosened by one page, and `PF_W` and `PF_X` each forced to true. Lens: tests. Medium.
  **Fix**, with kobj-4.
  Fixed in `badc91cd`; fails without it:
  `a_segment_with_more_file_than_memory_or_past_the_image_is_refused` and
  `a_segment_is_writable_or_executable_only_as_its_flags_say`, four of its five mutations. The
  fifth, the loader's bound loosened by a page, fails nothing: the address-space layer refuses the
  same segment with the same error, and is now the bound.
- [ ] **kmem-14** `kernel/src/object/memory_object.rs:71` — Nothing tests that fresh MemoryObject
  or DmaBuffer frames are zeroed. A regression removing the zeroing in `sys_memory_create` would
  hand userspace stale kernel or other-process pages, a cross-process information leak. The host
  test heap probably hands out already-zero memory, so the zeroing is never observable.
  Demonstrated: four mutations survived. Removing `write_bytes` in `MemoryObject::try_new`
  (tests `object::memory_object`), removing the partial-failure `buddy_free(done, 0)` rollback,
  removing `write_bytes` in `DmaBuffer::alloc` (tests `mm::dma`), and `DmaBuffer::drop` freeing at
  order 0 instead of `self.order`, which leaks higher-order buffers. Lens: tests. Medium. **Later.**
- [ ] **kmem-15** `kernel/src/mm/addr_space.rs:453` — `map_file_page` re-validates that a
  FileBacked VMA for the same object still covers `addr`, but not that `addr` still corresponds to
  the page index the fault filled. Thread A faults at X in a mapping of file F at base B (index
  i) and parks on the fill. Thread B unmaps it and maps F again at B', which still covers X. A
  then installs page i's frame where page j belongs, so the process sees (or writes) the wrong
  bytes. The effect is confined to files the process already maps. Unverified: a host test
  interleaving `file_backing`, `unmap_covering`, `map_file` and `map_file_page` would settle it.
  `try_fault_in` (`idt.rs:315-319`) takes the index from `file_backing`, and the frame is
  installed without re-deriving it under the second AS-lock hold. Lens: logic. Low.
  **Later**, in Part E.
- [x] **kmem-16** `kernel/src/object/file_object.rs:540` — `begin_fill`'s read-ahead adjacency test
  computes `first * bs + (n * PAGE_SIZE) as u64` unchecked. `block_map` guarantees only that each
  run's own end fits a u64, so a run map near the top of the byte range overflows when the next
  page lies past it. A filesystem server replies FILE_BLOCKS with bs = 512 and runs (0, D, 8) and
  (8, D+8, 1), D = u64::MAX/512 - 10, which `block_map` accepts. A fault on page 0 with read-ahead
  checks page 2 and the kernel panics. Only a malicious registered server reaches it; honest
  servers cannot produce such LBAs. Demonstrated: a host probe through
  `mapped_file`/`miss`/`begin_fill` panicked at `file_object.rs:540:30` ("attempt to add with
  overflow"). `begin_write`'s matching `first * bs + (k * PAGE_SIZE)` was not probed. Lens:
  trust-boundary. Low. **Fix.**
  Fixed in `655de0b6`; fails without it:
  `a_page_that_would_lie_past_the_top_of_the_device_ends_a_run`, each sum unchecked in turn.
- [ ] **kmem-17** `kernel/src/mm/vmm.rs:413` — `find_free_range`'s upper bound (`MMAP_MAX`, which
  keeps hint=0 mappings out of the stack guard gap) and its exact-fit boundary have no test. If
  the bound regressed, `sys_memory_map(hint=0)` could return ranges in the guard gap and a stack
  overrun would land in mapped memory. An off-by-one in the exact-fit comparison would skip a gap
  that fits. Demonstrated: three mutations in `mm::vmm` survived, namely the `> max` early return
  forced to `if false`, the final `cursor+size <= max` forced true, and `s >= cursor+size` changed
  to `s >`. Lens: tests. Low. **Later.**
- [x] **kmem-18** `kernel/src/mm/buddy.rs:142` — The `order > MAX_ORDER` guard in
  `BuddyAllocator::alloc` is untested (only the `debug_assert` fires in debug), and neither is
  `BuddyAllocator::new`'s `bitmap_reservation` rounding to whole pages. In a release build
  without the guard, `alloc(order > 10)` would index `free_lists` out of bounds, and a rounding
  bug would let pass 2 strip frames from the next usable entry. Reach is low, since
  `DmaBuffer::alloc` and `order_for` already bound the order. Demonstrated: two mutations in
  `mm::buddy` survived, namely the guard changed to `if false` and `bitmap_reservation` using
  `bitmap_bytes`. See kmem-5 for the `debug_assert` that makes the guard unreachable in the dev
  build. Lens: tests. Low. **Fix**, with kobj-3.
  Fixed in `e56c6612`; fails without it: `an_order_past_the_largest_block_is_refused`. Its second
  mutation, the bitmap reservation's rounding, is equivalent: Limine's usable entries are
  page-aligned, so both choose the same region.
- [ ] **kmem-19** `kernel/src/object/file_object.rs:437` — `resize`'s
  `g.retired.try_reserve(gone)?`, the all-or-nothing allocation before pages are removed, can be
  deleted with every `file_object` test still passing. Without it, the later
  `let _ = g.retired.try_push(p.frame)` drops its Result when the vector cannot grow, so the
  frame is neither freed nor tracked, and the allocation also happens under the lock. With the
  reserve, a truncate that cannot grow `retired` returns Err and changes nothing. No test covers
  that failure. Demonstrated: removing the reserve and running `object::file_object` survived.
  Lens: tests. Low. **Later.**

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

- [x] **fs-1** `userspace/fs-server-ext4/src/ext4.rs:1747` — `rename_at` validates only the new
  name, so a directory session can rename its own `..` or `.` entry to an ordinary name. A session
  in a namespace bound with subtree base `/home/alice` renames `..` to `up`. `/home/alice/up` then
  names `/home`, and repeating it from there reaches `/`, which is a way out of the confined
  subtree. The directory also loses its `..` record. With `.` it gets an entry pointing at itself
  and no `.` record. Neither `parse_rename_request` nor `serve_session` (server.rs:681) checks the
  names. Demonstrated: host tests on an mke2fs fixture. `rename_at(alice, "..", "up")` returned Ok.
  `resolve_dir("/home/alice/up/bob")` gave Bob's inode. `stat_file` of Bob's file succeeded. A second
  hop reached `/system`. `e2fsck -fn` reported "Second entry 'up' ... should be '..'" and "'..' in
  /d is <The NULL inode>". FAT is not affected, because `dir::entries` skips dot entries. Lenses:
  logic, error-paths, trust-boundary, tests (four reviewers found it independently). high. **Fix.**
  Fixed in `25bd9eb9`, for every name a session sends; fails without it:
  `a_directorys_dot_entries_are_never_renamed` and boot-probe's `dot_entries_test`.

- [x] **fs-2** `userspace/libfsserver/src/server.rs:650` — A directory session is bound to a bare id
  (an ext4 inode number, a FAT first cluster). `live_dir` rejects only a freed id, not a reused
  one. A session is open on `/tmp/d`, then `/tmp/d` is rmdir'd. The next mkdir anywhere takes the
  same id (ext4 takes the lowest free inode; FAT takes the cluster once the allocation hint wraps).
  The stale session then lists, creates, renames and unlinks inside the new directory, outside its
  namespace. Demonstrated: on ext4, session id 17 equalled the new `/secret/inner` id, and
  `mkdir_at(17, "planted")` created `/secret/inner/planted`. Another probe made `mkdir_at(18, ...)`
  land in `/home/bob/private`. On FAT (16 MiB, 4 KiB clusters), session 4 equalled `/secret/inner`
  and `read_dir` listed `victim.txt`. `rename_at` and `unlink_at` through the stale id both
  returned Ok. The existing test `a_removed_directory_is_refused_to_a_session_still_holding_it`
  covers only the non-reuse case. Lenses: logic, error-paths, tests. high. **Fix.**
  Fixed in `6ce9737a`: an rmdir ends every session on the directory; fails without it: boot-probe's
  `removed_dir_session_test`.

- [x] **fs-3** `userspace/fs-server-ext4/src/ext4.rs:1705` — A rename writes the new name first and
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
  **Fix.**
  ext4's half fixed in `14fbd440`: a file counts its new name before it is written and its old until
  it is removed, a directory loses its old name first, a failed step is undone, and a rename to a
  name its file has already does nothing; fails without it:
  `a_rename_stopped_at_any_write_never_leaves_a_name_reaching_what_is_freed` and
  `a_rename_to_a_name_its_file_has_already_does_nothing`.
  FAT's half fixed in `eaaf7066`: a failed step is undone, last first, and a rename finding its file
  under both names, as a crash between the writes leaves it, finishes, deleting the old name and
  releasing nothing. Removing one of the two names before then still frees what the other holds,
  since FAT has no link count; `deferred-decisions.md` says so under `TODO(fs-repair)`. Fails
  without it: `a_rename_stopped_at_any_write_leaves_the_file_under_a_name`. And a mount that
  leaves the two names itself, its undo failing, or that finds the volume corrupt, refuses every
  change after, as Linux's FAT goes read-only (the maintainer's call); fails without it:
  `a_mount_that_finds_its_filesystem_corrupt_refuses_every_change_after`.

- [x] **fs-4** `userspace/fs-server-ext4/src/ext4.rs:226` — `parse_superblock` and `check_device`
  refuse only INCOMPAT_64BIT and blocks over 4 KiB. They accept metadata_csum, an unreplayed
  journal (needs_recovery), meta_bg and bigalloc, and mount them read-write. An ext4 made with
  `mke2fs -t ext4 -O ^64bit` on a removable disk is mounted writable. `mark_mounted` writes
  `s_state` without updating the superblock checksum, so Linux and `e2fsck` refuse the filesystem
  after the first write. A disk with needs_recovery would have its changes overwritten by a later
  Linux journal replay. The crate's CLAUDE.md says these are rejected. Demonstrated: `dumpe2fs`
  showed has_journal and metadata_csum. `check_device` returned Ok, and `create_file` and
  `grow_file` returned Ok. `e2fsck -fn` then failed with "Superblock checksum does not match
  superblock". `check_device` also returned Ok with INCOMPAT_RECOVER (0x4) set. Lens: logic. high.
  **Fix.**
  Fixed in `16daae29`: an incompat feature the reader does not read, or a journal to replay, refuses
  the filesystem; a ro_compat feature its writes do not keep, or a missing filetype or extent,
  refuses a writable mount, and the storage service mounts such a filesystem read-only; fails
  without it:
  `a_feature_this_server_cannot_keep_refuses_a_writable_mount_and_one_it_cannot_read_any` and
  storage-service's `an_ext4_its_server_would_only_read_is_mounted_read_only`. The sweep found an
  insert writing over a Linux-indexed directory's hash tree, fixed in `e34de7c4`:
  `a_directory_linux_indexed_is_made_linear_before_a_name_goes_in`.

- [x] **fs-5** `userspace/fs-server-ext4/src/ext4.rs:1075` — `dir_insert` computes `used =
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
  Lenses: trust-boundary, tests. high. **Fix.**
  Fixed in `52e98a89`: every directory scan reads its records through `record`; fails without it:
  `garbage_in_any_structure_is_an_error_never_a_panic`, new, and
  `a_record_is_refused_unless_its_length_holds_it`. That test also timed a 364 s request on a file
  sized past 2^32 blocks, fixed in `cae4645c`:
  `past_the_last_block_an_extent_can_address_the_rest_is_one_hole`.

- [x] **fs-6** `userspace/fs-server-ext4/src/ext4.rs:1627` — `dir_repoint` compares `&buf[off + 8..off
  + 8 + name_len]` without checking `off + 8 + name_len <= bs`, so a crafted directory block makes
  the server panic. A directory whose first block ends in an 8-byte entry at offset 4088 with
  name_len 2 and a nonzero inode is moved with `rename_path`. A rename that replaces an existing
  destination reaches the same code. The server exits and every client of that mount gets
  PeerClosed. Demonstrated: a host test panicked at ext4.rs:1627:60 with "range end index 4098 out
  of range for slice of length 4096". Reading the replacing-rename path (the trust-boundary
  reviewer's finding) is unverified; a host test feeding `rename_path` a crafted destination
  directory would confirm it. Lenses: logic, trust-boundary. high. **Fix.**
  Fixed in `52e98a89`, with fs-5; fails without it:
  `a_record_whose_name_runs_past_its_block_is_an_error_to_rename_over`, which since `14fbd440` also
  requires the refusal to write nothing.

- [x] **fs-7** `userspace/fs-server-ext4/src/ext4.rs:1665` — `rename_path` does not limit the
  destination leaf to 255 bytes, and `dir_insert` stores `name.len() as u8`, so a long name is cut
  mod 256 and the rename reports success. `create_file`, `mkdir_at` and `rename_at` all check. A
  256-byte name gets name_len 0, so the file vanishes from lookup and listing. A 300-byte name
  becomes its first 44 bytes, and if an entry of that name exists the directory holds two entries
  with one name. The kernel's `validate_path` bounds only the whole path, not a component.
  Demonstrated: `rename_path(/system/f, /system/<256 x 'a'>)` returned Ok(None). `/system` then
  listed only `.` and `..`, and `read_file` under both names gave NotFound. `e2fsck` reported
  "Entry '' in /system (13) has a zero-length name". For 300 bytes, `stat_file` of the full name gave
  NotFound and of the 44-byte prefix gave Ok(12288). FAT is not affected. Lenses: logic,
  error-paths. high. **Fix.**
  Fixed in `3265783c`, PR #392's, whose review's sweep found it again; fails without it:
  `a_name_past_255_bytes_is_refused_by_every_insert`.

- [x] **fs-8** `userspace/fs-server-ext4/src/ext4.rs:1512` — `unlink_at` and `rename_path` can
  return Err after the irreversible step, and the orphan they held is lost with the error. In
  `unlink_at`, the name is removed and `drop_link` has produced the inode when `touch_inode(dir)?`
  (an mtime stamp) fails. In `rename_path`, the touches and `adjust_links` after `drop_link` fail
  the same way. The server runs `forget_then_release` only on Ok (server.rs:667), so the inode and
  all its blocks leak. The client is told the unlink failed although the name is gone, and a retry
  gets NotFound. Nothing here can run `e2fsck`. Demonstrated: a write-failure sweep of the ext4
  ops, with closures that use `?` as the server does. "unlink: write 1/9 once: Err(Io): Unattached
  inode 17" and "rename_path_replace: write 2/10 once: Err(Io): Unattached inode 18". FAT does not
  have this. Lens: error-paths. medium. **Fix.**
  Fixed in `14fbd440`: once a change's name has changed nothing after fails it, and the sweep made
  rmdir, create and mkdir answer so too; fails without it:
  `a_name_taken_away_returns_what_it_freed_whatever_write_is_refused_after` and
  `what_a_change_answers_is_what_it_did`.

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
  physical block 388, len 1)". Lens: error-paths. medium. **Later.**

- [x] **fs-10** `userspace/fs-server-ext4/src/ext4.rs:1692` — The guard against moving a directory
  into its own subtree is a byte-prefix test, `new_parent_path.starts_with(old_path)`, with no
  component-boundary check. It refuses a legal move into a sibling whose name starts with the
  source's name. `libfs::rename` maps the resulting Unsupported to CrossDevice, so callers fall
  back to a slow, non-atomic copy plus unlink. The reviewer also noted that `rename_path('/ab',
  '/./ab/x')` would get past the guard, but the kernel's `validate_path` rejects `.` components for
  bindings and the lookup and rename paths were not checked (unverified). FAT uses the ancestor
  walk `dir::within`. Demonstrated: `rename_path(/ab, /abc/ab)` and `(/d, /dd/d)` returned
  Err(Unsupported). The library-level `/ab` to `/./ab/x` call returned Ok(None) and `e2fsck`
  reported "Entry 'x' in . is a link to '.'". Lenses: logic, error-paths, tests. medium. **Fix.**
  Fixed in `c2fbace4`: the guard walks up from the new parent by its `..` records; fails without it:
  `a_directory_moves_beside_a_name_that_begins_with_its_own_but_never_into_itself` and
  `a_loop_of_parent_records_is_corrupt_to_move_a_directory_into`.

- [ ] **fs-11** `userspace/fs-server-ext4/src/extent.rs:163` — `on_device()` bounds an extent only to
  `[first_data_block, blocks_count)`, so an extent over a group's bitmaps, inode table or GDT, or
  over another file's blocks, is accepted and then freed by truncate, unlink or release through
  `free_run`. The metadata blocks are cleared in the bitmap and the allocator hands them out. A
  crafted stick has a file whose extent points at group 0's inode-table block; after unlink, a
  later grow or create reuses it and overwrites inodes. Linux's `ext4_inode_block_valid` check is
  absent. Demonstrated: patching a created file's extent start to the inode-table block (34). Its
  bitmap bit was 1 before and 0 after `unlink_at` plus `release_inode`. Lens: tests. medium.
  **Later.**

- [x] **fs-12** `userspace/fs-server-fat/src/table.rs:282` — `allocate()` aborts with `?` on a used
  cluster whose FAT entry is reserved or out of range (Corrupt) after it has marked free clusters.
  The partial chain is neither freed nor are later clusters tried, and `change()` flushes the cache
  anyway, so it reaches the disk. On FAT16 with file A at clusters 2-3, cluster 4 free and cluster 5
  holding 0xFFF1, `grow_file(A)` marks 4 as end-of-chain, hits 5 and returns Corrupt. Cluster 4 is
  allocated to nobody, and every later grow or mkdir that scans through 5 fails the same way, so
  one bad entry makes the volume unwritable. Demonstrated: `grow_file(/A.TXT, 10000)` returned
  Err(Corrupt) and cluster 4's FAT entry afterwards read [255,255], not 0. `hostile_tests` only
  checks for no panic. Lens: tests. medium. **Fix.**
  Fixed in `fd4e7f60`: an entry not free is a cluster in use, as Linux reads it, so an allocation
  passes over a reserved one, and any failure inside it gives back what it took; fails without it:
  `an_allocation_passes_over_a_reserved_entry`.

- [ ] **fs-13** `userspace/libfsserver/src/serve.rs:78` — The MAX_SUFFIX bound in `resolve()` and
  `serve_read_range()`, and the `.min(content.len())` clamp on the read length, are untested. A
  suffix of 1025 bytes or more returns TooLarge today. If the bound were lost, `path_buf[1..1+len]`
  would panic and the server would hang. Adjacent defect: `try_resolve_directory` (server.rs:381)
  truncates an over-long suffix with `.min(MAX_SUFFIX)` instead of refusing it. A suffix of 1025
  bytes or more whose first 1024 bytes name a directory therefore opens a session instead of
  TooLarge. That function is syscall-bound and can only be checked in a boot. Demonstrated:
  deleting either `req.suffix.len() > MAX_SUFFIX` check, or the `.min(content.len())`, left
  libfsserver, fs-server-fat and fs-server-ext4 `--lib` all green. The truncation was found by
  reading. Lens: tests. medium. **Later.**

- [x] **fs-14** `userspace/fs-server-fat/src/volume.rs:706` — FAT's `rmdir_at` NotEmpty check, the
  `is_dir` refusal in `unlink_at`, `names::valid` in `mkdir_at` and in rename, and the `grow_file`
  "chain shorter than size" check have no test that reaches them. Without NotEmpty, rmdir of a
  non-empty directory would free its chain and orphan its children's clusters. Without the `is_dir`
  check, `unlink_at` on a directory frees a live directory chain. Without the name checks, names
  with `:` or `*` would be written to the volume. Demonstrated: five mutations (each guard deleted)
  left `cargo test -p fs-server-fat --lib` fully green. No FAT test mentions NotEmpty, though the
  ext4 rmdir test covers it. Lens: tests. medium. **Fix**, with pr365-1.
  Fixed in `eaaf7066`: rmdir's `NotEmpty` and the grow's chain check were already held by tests; the
  other three now are, by `a_refusal_comes_before_anything_is_written` and
  `a_rename_onto_an_entry_a_fat_cannot_name_is_refused`, each failing with its guard removed.

- [x] **fs-15** `userspace/fs-server-ext4/src/ext4.rs:804` — The allocator's `start + len <
  capacity` bound and `.min(gfree)` cap in `alloc_run`, and the `!bit_clear` guard in `free_run`,
  are untested. Without the first, a short final group's spare bitmap bits can be allocated as
  blocks past the end of the filesystem. Without the second, the group's free count wraps below
  zero. Without the third, a double free counts its blocks free twice. An image from another tool
  with spare bits clear, or a free count that disagrees with its bitmap, would then get an extent
  off the device. Demonstrated: three mutations, each leaving `cargo test -p fs-server-ext4 --lib`
  green. Lens: tests. medium. **Fix**, with fs-8.
  Written with fs-8 in `14fbd440`: `the_allocator_keeps_to_the_filesystem_and_to_its_counts`,
  failing with each of the three guards removed.

- [ ] **fs-16** `userspace/libfsserver/src/cache.rs:195` — `MetaCache::forget` has no test for a
  freed range whose end is not 4 KiB-aligned, and the rounding of the end of the range is
  unchecked. FAT's data region need not start on a 4 KiB boundary. If a freed cluster ends mid-block
  and the partial block stays cached, the kernel writes file data there behind the server's back. A
  later `read_meta` of that block (reused as metadata) then sees stale bytes. Demonstrated:
  changing `(offset + len).div_ceil(BLOCK)` to `(offset + len) / BLOCK` left libfsserver,
  fs-server-ext4 and fs-server-fat `--lib` green. Lens: tests. low. **Later.**

- [x] **fs-17** `userspace/libfsserver/src/server.rs:329` — `maybe_grow` notes only
  `FsError::ReadOnly`. Every other create, grow or truncate failure falls through, and the resolve
  answers success with the file mapped at its old size. The spec says the reply's content_len is
  the new size, and the kernel installs the reply's size without comparing. On a full disk, a
  `libfs::write_file` or `copy_file` of a new file gets a create that holds 0 bytes, and the map
  then fails, so the caller reports InvalidArgument rather than no-space. A FAT create with a
  forbidden name returns NotFound instead of InvalidArgument. The only silent-success case is
  ext4 with sub-page blocks, which is the recorded TODO(ext4-subpage-runs). Unverified: boot a
  full 1 KiB-block ext4 and run `copy_file`; it shows whether the copy is reported as success when
  short. Lens: error-paths. low. **Fix.**
  Fixed in `5be08622`: `libfsserver::serve::apply_size` fails a resolve with each failure, for FAT
  too; fails without it: `a_resolve_whose_create_grow_or_truncate_fails_answers_the_failure`, and
  boot-probe's `refused_create_test` with the server's call site swallowing it.

- [x] **fs-18** `userspace/libfsserver/src/server.rs:614` — When `reply_dir_handle`'s send fails,
  `open_dir_session` closes the session and the client endpoint but sends no error reply, while the
  other failure arms reply WouldBlock or KernelError. A forwarded lookup has no deadline, so the
  caller's PendingOperation never completes, and a `Dir::open` in Files, nxsh or a libfs walk waits
  forever with nothing logged. Unverified: injecting a failing `SYS_CHANNEL_SEND` for that reply in
  a boot shows whether the lookup hangs or completes with an error. No existing gate produces the
  trigger (a full queue or kernel out of memory). Lens: error-paths. low. **Fix.**
  Fixed in `e6f6d404`, with every forwarded reply the kernel did not take; no host test can fail a
  syscall, so a probe build whose reply carried a handle the kernel refuses showed the boot hang at
  the open without the fix, and the open answered with it.

- [x] **fs-19** `userspace/libfs/src/lib.rs:775` — `copy_tree` has no check that the destination
  lies inside the source, and its listing helpers pass server-supplied names to `join()`
  unchecked. (a) `copy_tree(ns, "/a", "/a/b")` creates `/a/b`, then re-lists `/a`, which now holds
  `b`, and recurses until MAX_TREE_DEPTH (32) fails with TooDeep. That leaves about 32 nested
  junk directories; coreutils only rejects target == src and nxfiles has no guard. (b) A name
  containing `/` from a crafted disk goes into `remove_tree` and `copy_tree` unchecked, so they act
  on a different file in a subdirectory than the one listed. Names `.` and `..` are filtered;
  embedded `/` is not. Unverified: (a) holds by reading, and `copy -r /dir /dir/sub` in a boot
  would confirm it. (b) needs a crafted stick with an entry named `a/b`, and whether a server can
  list such a name is unchecked. libfs has only pure-helper host tests. Lens: tests. low. **Fix.**
  Fixed in `a0e9c905`: `copy_tree` refuses a destination in its source (`IntoItself`), and the
  walks' listing a name no entry has (`Name`); fails without it:
  `a_path_is_within_a_directory_a_component_at_a_time`,
  `a_listed_name_no_entry_can_have_stops_a_walk`, and boot-probe's `copy_into_itself_test`.

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

- [x] **drv-1** `kernel/src/drivers/xhci/storage.rs:688` — A bulk-only read whose data stage ends
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
  High. **Fix.**
  Fixed in `4257f5cc` (USB) and `bc7e017d` (AHCI's PRDBC, not yet booted on the laptop's HBA); fails
  without it: `a_short_read_whose_status_says_nothing_is_missing_fails` and
  `a_command_that_moved_less_than_its_length_fails`.

- [x] **drv-2** `kernel/src/drivers/ahci.rs:869` — `poll_complete_inflight` discards
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
  logic, error-paths. High. **Fix.**
  Fixed in `5e699918`: a polled wait that gives up recovers the port and fails the command, never
  retiring it, and IDENTIFY stops the port before its buffer goes; fails without it:
  `a_command_a_polled_wait_gave_up_on_is_failed_not_retired`.

- [x] **drv-3** `kernel/src/drivers/gpt.rs:236` — `/dev/disk/by-partlabel/<label>` goes to the
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
  **Fix.**
  Fixed in `3cea152b`, the storage service's matching with it; fails without it:
  `a_name_two_partitions_carry_goes_to_the_boot_mediums`,
  `a_name_two_partitions_carry_is_the_boot_mediums`, and check-storage, whose disk gains a decoy
  `nitrox-live`.

- [x] **drv-4** `kernel/src/syscall/table.rs:2393` — A FILE_BLOCKS resolve reply discards the
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
  architecture. High. **Fix**, as kobj-2.
  Fixed in `4043ae92`, as kobj-2; fails without it:
  `a_file_blocks_file_is_writable_only_as_far_as_its_server_may_write_the_device` and
  `a_file_blocks_reply_installs_a_file_no_more_writable_than_its_server_holds_its_device`.

- [x] **drv-5** `kernel/src/drivers/ahci.rs:822` — The AHCI driver has no error recovery and no
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
  later I/O proceeds. Lenses: logic, error-paths. High. **Fix.**
  Fixed in `5e699918`: a task file or fatal error wakes a watch thread, which recovers the port
  (AHCI 1.3.1 §6.2.2.1) and fails the command `IoError`; the same thread enforces a 30 s command
  deadline; a port that will not stop keeps its command and refuses the rest. Fails without it:
  `a_task_file_error_is_recovered_its_command_failed_and_the_next_issued` and
  `a_command_past_its_deadline_is_recovered_and_failed`. On QEMU, `blkdebug` failing one sector
  showed the recovery and every probe after it passing; the kept-PxCI path is the host tests' (QEMU
  clears it).

- [x] **drv-6** `kernel/src/drivers/ahci.rs:726` — Nothing refuses a block request whose offset or
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
  trust-boundary, tests, architecture (five reports of one defect). Medium. **Fix**, as kobj-7.
  Fixed with kobj-7 in `bc9731b9`.

- [x] **drv-7** `kernel/src/drivers/xhci/hub.rs:273` — `hub::depart` calls `storage::finish`
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
  held IRPs on the Kept path, as the memory is leaked. Lens: error-paths. Medium. **Fix.**
  Fixed in `b061bbae`: `storage::finish` takes what became of the slot, and a kept slot keeps its
  commands, never completed, a rescan alone completing; fails without it:
  `a_departure_completes_its_commands_only_once_the_slot_is_disabled`.

- [ ] **drv-8** `kernel/src/drivers/ahci.rs:321` — `drivers::probe` calls `ahci::init` for every
  AHCI-class function, but `AHCI.store(disk)` is unconditional and the ISR, `ahci_poll` (which
  ignores its ctx argument), `poll_complete_inflight` and the DPCs all read that single global. A
  machine has two AHCI controllers with a disk each. Disk 2 overwrites disk 1, so disk 1's polled
  boot reads and its interrupts are handled against disk 2's state. Disk 1's `read_blocking` sees
  inflight null and returns false, its table "did not read", and its IRP stays in flight so later
  IRPs queue and then fail IoError. If disk 1 holds `nitrox-root`, `/` cannot mount. Unverified:
  QEMU with two `-device ahci` controllers, each with a drive; check for the "partitions: the table
  did not read" line. No xtask gate has that configuration. Lens: trust-boundary. Medium. **Later.**

- [ ] **drv-9** `kernel/src/drivers/partitions.rs:97` — Test gap in untrusted-input guards: the GPT
  entry-size range check, the `.min(entries)` clamp on the array and the `.min(GPT_ENTRY_BLOCKS_MAX)`
  cap are each deletable with all 14 partitions tests passing. The `is_multiple_of` half of the size
  guard is also deletable with nothing failing. The existing test uses only entry size 96, which
  the divides-a-block check alone rejects. Without the range check, entry size 64 panics slicing
  `e[56..128]` (partitions.rs:149). Without `.min(entries)`, a header declaring 5 entries with a
  stale sixth in the block publishes a phantom sixth partition. Without the cap, a header declaring
  0xFFFFFFFF entries asks `try_reserve` for 512 GiB, so the disk reads as NoMemory instead of its
  first 128 entries. Demonstrated: temporary tests showed the first two (panic; left 6, right 5),
  restored by `git checkout`. The third is from reading. Lens: tests. Medium. **Later.**

- [ ] **drv-10** `kernel/src/io/block.rs:526` — Test gap on the partition sandbox boundary:
  `partition_rebase`'s three overflow guards (`block_count.checked_mul(sector_size)`,
  `offset.checked_add(length)` and the final `checked_add` of `start_lba*sector_size`) can each
  become `wrapping_*` with all 8 io::block tests passing. The one existing test uses small values.
  If one regresses, a request with offset near u64::MAX and a length that wraps back inside the
  span passes the bounds check and is forwarded at a wrapped absolute offset, and AHCI truncates
  the LBA to 48 bits. That could land outside the partition, and CI would not notice. The final
  `checked_add` alone currently catches one wrap shape, so the guards cover each other and the pair
  is untested. Demonstrated by per-guard mutation: each left 8/8 passing. Lens: tests. Medium.
  **Later.**

- [ ] **drv-11** `kernel/src/drivers/xhci/desc.rs:131` — Test gap: the `len >= 9` guard on
  interface descriptors in `interfaces()` is not pinned, and removing it passes all 19 desc tests.
  The `wTotalLength`-versus-`b.len()` bound, the `len<2` check in `descriptors()`, the
  `(b[0] as usize) < 9` check in `configuration_total` and the first-endpoint-wins check in
  `hid_interfaces` also survive deletion. With the guard gone, a configuration whose last
  descriptor is an interface descriptor with bLength 4 at the end of the buffer (config 9 bytes,
  then `[4,4,0,0]`, wTotalLength 13) makes `interfaces()` index `b[here+4]` and panic (desc.rs:133).
  That is a kernel panic from a USB device's bytes if the guard regresses. Demonstrated by a
  temporary test `probe_short_iface` with the guard removed, then restored. Lens: tests. Medium.
  **Later.**

- [ ] **drv-12** `kernel/src/io/block.rs:127` — Test gap: `dispatch_block_irp`'s own `max_frags`
  check, which the code calls the PRDT-overflow protection, has no test. Changing `>` to
  `> max_frags + 1` passes all io::block tests, and only the `_into_frames` variant is tested.
  AHCI's backstop in `submit()` would still refuse, but the USB path only clamps
  (`frags.len().min(MAX_FRAGS)`) and would silently truncate a transfer that the CSW then reports
  whole. Deleting the storage.rs clamps (in `issue()` and `start()`) also fails no test. Only the
  boot-time self-test (drivers/mod.rs:147) exercises the limit. Demonstrated by mutation: io::block
  8 passed, storage 27 passed. Lens: tests. Low. **Later.**

- [ ] **drv-13** `kernel/src/drivers/hid/descriptor.rs:222` — Test gap: several of `walk()`'s
  untrusted-input rejections have no test. Each of these mutations passed all 14 descriptor tests:
  a truncated item at the end, a truncated long item, Pop with nothing pushed (`checked_sub` to
  `saturating_sub`), a Push past `STACK_MAX` (to modulo), an Input item in a seventeenth report ID,
  the `done` flag that stops a second mouse or keyboard collection, and the array-already-set check
  in `take_input` (a second key array). 7 of 13 mutations survived. A regression would read a
  layout from a corrupt descriptor instead of falling back to the boot protocol: a second
  collection's fields would overwrite the first's, or a mouse's offsets would come from state a
  Pop never restored. Demonstrated by mutation. Lens: tests. Low. **Later.**

- [ ] **drv-14** `kernel/src/pci/mod.rs:195` — Test gap: the multifunction mask in
  `decode_function` (`header_type_raw(cfg) & 0x7F == 0`) is not pinned, and replacing it with
  `== 0` passes all 28 pci tests. The I/O BAR address mask (`0xFFFF_FFFC` versus `..F0`) and the
  low-nibble mask on a 64-bit BAR's low dword are also unpinned. If the multifunction mask
  regresses, every function of a multifunction device (header type 0x80, such as the ICH9 AHCI at
  00:1f.2 and many xHCI controllers) gets empty BAR windows. The driver finds no MMIO BAR and
  silently declines the controller, and only a boot on such hardware would show it. A dropped bit 2
  in the I/O mask would size 4- or 8-byte I/O BARs wrongly. Demonstrated by mutation. Lens: tests.
  Low. **Later.**

- [ ] **drv-15** `kernel/src/drivers/hid/keyboard.rs:132` — Test gap: the key array's
  `value >= a.logical_min` filter and the `FIRST_KEY` lower bound in `hold()` are not pinned.
  Replacing the filter with `true` passes all 14 keyboard tests, because every test layout uses
  `logical_min` 0. A report descriptor with `logical_min` 1 sends 0 in an empty slot. Without the
  filter `hold()` gets `first + (0 - 1)`, which can land on a real usage and press a phantom key on
  every report. Demonstrated by mutation. Lens: tests. Low. **Later.**

- [x] **drv-16** `kernel/src/power.rs:89` — `flush_every_disk` walks `/dev/blk/<n>` from 0 and
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
  is None while `block(1)` is Some. Lens: architecture. Low. **Fix.**
  Fixed in `20fe57d9`: the walk goes from each live block device to the next
  (`Registry::block_from`), past a departed one; fails without it:
  `a_walk_of_the_block_devices_passes_a_departed_one`.

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

### protocols — main at 510381f4, 2026-10-20

Lenses: logic, error-paths, trust-boundary, tests, architecture; returned nothing: none.
Found 29, refuted by a boot 0, reported 18 (11 were duplicates, merged).

- [x] **proto-1** `kernel/src/object/file_object.rs:540` — Read-ahead and write-back overflow
  u64 on a run that `block_map` accepted. `block_map` (table.rs:2592) checks only that each run's
  device end in bytes fits a u64, but `begin_fill` computes `first * bs + (n * PAGE_SIZE) as u64`
  (write-back does the same at line 593). A run ending within a page of u64::MAX bytes passes
  `block_map` and then overflows. The kernel dev profile has overflow-checks on, so the kernel
  panics while holding the FileObject lock. A fs server with a block DeviceNode (max_frags >= 2:
  AHCI, xHCI storage, RAM disk) answers a FILE_BLOCKS resolve with block_size 512, content_len
  8192 and one run (file_block 0, device_lba u64::MAX/512 - 1, length 1). The first fault on page
  0 then panics with 'attempt to add with overflow'. The shipped ext4 and FAT servers cannot
  produce such a run, so this needs a misbehaving server. Demonstrated: a probe appended to
  `a_block_map_the_page_cache_cannot_use_is_refused` showed the run is accepted. In the
  file_object tests, `fill_ahead(file_of(&f), 0, 512, 31)` on that run panicked at
  file_object.rs:540:30 (`cd kernel && cargo test --lib --target x86_64-unknown-linux-gnu`).
  Probes removed. logic. High. **Fix**, as kmem-16.
  Fixed in `655de0b6`; fails without it:
  `a_page_that_would_lie_past_the_top_of_the_device_ends_a_run`, each sum unchecked in turn.
- [x] **proto-2** `kernel/src/syscall/table.rs:2394` — `build_and_install_file_blocks` takes the
  transferred DeviceNode and drops its rights (`tr.rights`). Whether the file is writable depends
  only on the server's own FILE_BLOCKS_READ_ONLY flag (line 2433). The sibling arms respect
  rights: MEMOBJ/CHANNEL attenuate by `pl.requested & tr.rights` (line 2158) and SUBNAMESPACE
  requires LOOKUP (line 2462). The storage service mounts a stick read-only and hands the server
  a device handle with READ|TRANSFER|DUPLICATE and no WRITE (mounting.rs:132-136; storage.md
  line 203). A misbehaving or compromised server answers a MAP_WRITE resolve with
  OBJECT_KIND_FILE_BLOCKS, its READ-only DeviceNode and flags 0. The kernel installs the file
  with `pl.requested | INSPECT`. The client maps it, writes and calls `sys_file_sync`, and
  `FileObject::writeback` (file_object.rs:883-915) issues `IoOpcode::Write` IRPs through
  `dispatch_block_irp_into_frames`, which checks no rights. `sys_io_submit` would refuse that
  write (table.rs:1610), so the path amplifies authority. The fill path likewise reads without
  checking READ. Not the recorded TODO(mount-write-authority), which concerns client binding
  rights. Unverified: read only, no boot. A host test of `build_and_install_file_blocks` with a
  READ-only TransferRef and flags 0 should show MAP_WRITE installed. A boot with a ro mount whose
  server sends flags 0, then a client write plus `sys_file_sync` and a host read of the sector,
  would show the disk written. Requiring WRITE in `tr.rights` before granting MAP_WRITE should
  turn it into NoAccess. logic, trust-boundary, architecture. High. **Fix**, as kobj-2.
  Fixed in `4043ae92`, as kobj-2; fails without it:
  `a_file_blocks_file_is_writable_only_as_far_as_its_server_may_write_the_device` and
  `a_file_blocks_reply_installs_a_file_no_more_writable_than_its_server_holds_its_device`.
- [x] **proto-3** `kernel/src/rsproto.rs:59` — No test covers the `body_len` bound in
  `reply_body`, `parse_reply` and `parse_read_range_reply`, the only guard between a hostile
  server's reply and an out-of-range slice panic in the kernel. A reply whose body_len exceeds
  the IPC payload would make `&msg[28..end]` panic if a check were removed. `reply_body` has no
  test at all. This is a test gap; the code is correct today. Demonstrated: each of the three
  checks removed in turn, and all 14 rsproto tests still passed (`cargo test --lib --target
  x86_64-unknown-linux-gnu rsproto`). Restored. tests. High. **Fix**, with kobj-3.
  Fixed in `e56c6612`; fails without it: `a_reply_body_may_not_run_past_its_message`, each of the
  three checks removed in turn.
- [x] **proto-4** `kernel/src/rsproto.rs:127` — The `BlockRun` stride (24) and index-to-offset
  arithmetic in `file_blocks_run` are pinned by no test; only run 0 is ever decoded. A wrong
  stride would make runs 1 and up read the wrong file_block, device_lba and length, so a Model A
  file with two or more extents fills from, or writes back to, the wrong disk blocks. Test gap;
  the code is correct today. Demonstrated: `FILE_BLOCKS_PREFIX_LEN + i * BLOCK_RUN_WIRE_LEN`
  changed to `i * 16`, and the rsproto tests all passed. Restored. tests. High.
  **Fix**, with kobj-3.
  Fixed in `e56c6612`; fails without it: `each_block_run_is_read_at_its_own_offset`.
- [ ] **proto-5** `userspace/librsproto/src/namespace.rs:113` — The length checks in
  `parse_resolve_grow_size`, `parse_resolve_rename` (both the tail+4 and tail+4+dest_len checks)
  and `file::parse_rename_request` have no test with a truncated or overrunning body. If one
  were dropped, a client with a namespace handle could send a cut-short RESOLVE_RENAME,
  RESOLVE_GROW or Rename and the filesystem server (a critical service) would slice out of
  range and panic. Test gap; the code is correct today. Demonstrated: each check replaced with
  `if false` individually, and every `cargo test -p librsproto --lib` run still passed.
  Restored. tests. High. **Later.**
- [ ] **proto-6** `userspace/libgpt/src/table.rs:261` — The guards in `read()` on the header_size
  range, entry_len == 128, count in 1..=1024 and `front.len() < end` are all untested. A disk
  with a valid-CRC header whose array_lba or count points past the buffer storage-service reads
  (FRONT_BYTES, about 17 KiB) would panic that critical service in `front[start..end]` if the
  Truncated check were removed. A header_size over 512 would likewise panic in
  `scratch[..header_size]`. Test gap; the code is correct today. Demonstrated: `front.len() <
  end` changed to `if false`, and the header_size range, entry_len check, `count == 0` and
  `count > 1024` mutated one by one. All 19 libgpt tests passed each time. Restored. tests.
  High. **Later.**
- [ ] **proto-7** `userspace/libstream/src/wire.rs:532` — `read_value` and `read_list` recurse
  with no depth limit, and so does a Record through `Schema::decode` and `read_row_values`. A
  List level costs 5 bytes and a Record level 8, and dropping or displaying a decoded value
  recurses too. The spec says values "may nest arbitrarily", and no deferral covers a depth
  limit. A pipeline stage's TSM1 output, or a `.tsm` file given to `open` (ops.rs:784), holds
  one List cell nested 40,000 or more deep (~200 KB, under nxsh's 1 MiB MAX_CAPTURE).
  nxsh's `decode_output` (eval.rs:1946) calls `Table::decode`, overflows the 8 MiB user stack
  (DEFAULT_USER_STACK_SIZE), hits the guard and the login shell dies. Demonstrated by five
  lenses with release host probes decoding on an 8 MiB-stack thread. Depths of 20,000 to 30,000
  (100 to 150 KB) decoded. Depths of 40,000 (200 KB), 50,000, 100,000 and 200,000 (1 MB) aborted
  with 'thread has overflowed its stack' (SIGABRT). Probes removed. logic, error-paths,
  trust-boundary, tests, architecture. Medium. **Later.**
- [ ] **proto-8** `userspace/libstream/src/wire.rs:590` — Decoding is not proportional to input.
  A Null-typed column takes 0 bytes per row and a row is one REC_DATA byte, but
  `read_row_values` allocates a 32-byte Value per column per row, so decoding allocates
  columns × rows × 32 bytes. A stage emits N Null columns and M bare REC_DATA bytes. nxsh's
  `decode_output` (capture capped at 1 MiB) or `open x.tsm` (`ops::decode_from`, no cap) then
  exhausts the heap and nxsh aborts, which defeats MAX_CAPTURE's purpose. The logging service's
  `render_typed` (TableReader, row by row) turns one 3,957-byte message (660 Null columns, 1,300
  rows) into 1,300 log records, ~5.1 MB of formatted text in total, each line kprinted whole
  under the kernel SERIAL lock; the stall on real serial hardware is not measured.
  Demonstrated: a release host probe with a counting allocator decoded an 8,017-byte stream
  (1,000 Null columns, 4,000 rows) to 4,000,000 Values with a 131,219,456-byte heap peak
  (16,367×). A 64,017-byte stream (4,000 columns, 48,000 rows) aborted with 'memory allocation
  of 131072 bytes failed' against a 2 GiB cap. A 5,017-byte stream with 1,000 columns and 1,000
  rows gave ~32 MB. Probes removed. error-paths, trust-boundary, architecture. Medium. **Later.**
- [ ] **proto-9** `userspace/libstream/src/channel.rs:234` — `IpcPort::recv` passes `recv_handles`
  and `count` to SYS_CHANNEL_RECV, which installs every transferred handle in the receiver's
  table, and never closes or returns them. nxsh's `drain_diagnostics` (main.rs:127-132) and
  librsproto `session::round_trip` (session.rs:251-262) close the same class, each with a
  comment saying why. nxsh reads a pipeline tail's stdout through `IpcPort::recv`
  (main.rs:379-416). A stage sends empty, non-`last` messages with 8 handles each, which never
  trip MAX_CAPTURE, so nxsh keeps installing handles it cannot name. They pin the stage's
  objects after it exits and eventually nxsh's spawns and lookups fail with OutOfHandles. The
  handle table is global with no per-process quota. `clip` reading stdin through
  ChannelReceiver (clip.rs:215) is said to leak the same way; that claim is unchecked.
  Unverified: code path confirmed by reading `channel.rs:235-261` and `sys_channel_recv`
  (table.rs:~2746-2769); no boot. A boot with a stage that sends handles on stdout, then
  counting nxsh's handles or watching its next spawn, would settle it. logic, error-paths,
  trust-boundary, architecture. Medium. **Fix.**
- [ ] **proto-10** `userspace/logging-service/src/main.rs:601` — `drain_source` receives log
  appends with `recv(h)`, which writes RECV_HANDLES and RECV_COUNT, and never closes them. Only
  `received()` (main.rs:401-410) closes transferred handles, and it runs on the read-session
  path. Any holder of a log channel sends appends or TSM1 bodies with 8 handles attached; the
  service installs and keeps them, and the next recv overwrites the values, so they can never
  be closed. The handles and the sender's pinned objects outlive the sender, and the service is
  `essential`, so service-mgr will not restart it to reclaim them. Unverified: read only. A test
  process sending appends with transferred handles on a log channel, while watching the
  service's handle count or the global table's live slots, would settle it. error-paths. Medium.
  **Fix.**
- [ ] **proto-11** `userspace/libstream/src/wire.rs:573` — The `SCHEMA_FIELD_HINT_CAP` caps on
  `Vec::with_capacity` in `read_list`, `read_row_values` and `Schema::decode`, the allocation
  defence against hostile counts, have no test. A peer sending a u32 count of 4,000,000,000
  followed by a few bytes would make a consumer abort on a gigabyte reservation if a cap were
  removed. Test gap; the code is correct today. Demonstrated: `.min(SCHEMA_FIELD_HINT_CAP)`
  removed at each of the three sites in turn, and every libstream host test still passed.
  Restored. tests. Medium. **Later.**
- [ ] **proto-12** `userspace/librsproto/src/file.rs:333` — `DirEntryIter`'s truncated-entry
  guard (`body.len() < end`) and its `remaining == 0` stop, plus the name-length limits (u8::MAX
  in the dir-entry writer, u16::MAX in the name and rename encoders), are untested. A Readdir
  body whose last entry's name_len runs past the body would make the iterator slice out of range
  and crash the client. An over-long name not refused at encode time is truncated to a u8 or
  u16 length, so the wire length disagrees with the bytes written. Test gap; the code is correct
  today. Demonstrated: each guard removed or loosened individually, and every librsproto test
  still passed. tests. Medium. **Later.**
- [x] **proto-13** `kernel/src/syscall/table.rs:2589` — `block_map` calls
  `runs.try_reserve(header.run_count as usize)` before the body is checked to hold that many
  runs. `run_count` is a server-chosen u32, but a reply body carries at most (3980-32)/24 = 164
  runs. A server answers FILE_BLOCKS with run_count 50,000,000 and a 32-byte body, and the
  kernel asks for ~1.2 GB before `file_blocks_run` first fails. Two other reviewers judged this
  harmless, because the buddy allocator's MAX_ORDER=10 caps a request at a transient 4 MiB
  that fails at once, so the real cost may be nil. Unverified: read only. A host test with a
  huge run_count, or a look at what `try_reserve` does against kmalloc's cap, would settle it.
  Bounding run_count by `(body.len() - 32) / 24` first removes the question. logic. Low.
  **Fix**, as kobj-3.
  Fixed in `e56c6612`; fails without it: `an_order_past_the_largest_block_is_refused`,
  `a_large_alloc_past_the_largest_block_is_refused`,
  `copying_an_image_past_the_largest_block_is_refused`,
  `a_file_blocks_header_claiming_more_runs_than_its_body_holds_is_refused` and
  `a_block_map_the_page_cache_cannot_use_is_refused`; the channel depth falls to 512, the deepest
  whose ring fits.
- [ ] **proto-14** `userspace/libstream/src/wire.rs:668` — `Schema::encode` writes
  `f.name.len() as u16` without refusing names over u16::MAX, and `WireErrorRecord::encode` does
  the same for `field_name` (line 733). Encoding reports Ok with a wrapped length, and the
  reader parses the rest of the name's bytes as the field's type, modifiers and the stream's
  records. A producer builds a column name from more than 64 KiB of input (say headers of an
  untrusted file), and the consumer sees a different, shorter schema or an error. Demonstrated:
  a 65,538-byte crafted name ("ab", 0x02, 0x00, then padding) encoded Ok. `Table::decode` read
  one field "ab" of type Int and failed with Err(BadRecordTag(122)) on the next name byte
  (`cargo test -p libstream --lib`). Probe removed. logic. Low. **Later.**
- [ ] **proto-15** `userspace/libgpt/src/table.rs:298` — `read()` computes disk_blocks as
  header[32..40] + 1, which overflows when a valid-CRC header carries backup_lba u64::MAX. A
  build with overflow checks panics 'attempt to add with overflow' in the storage service or
  installer. In release, which userspace uses, it wraps to 0, and no production caller reads
  `disk_blocks` (two other reviewers judged it harmless on that ground). `read()` also does not
  check that partition LBAs lie within first_usable..=last_usable. Demonstrated: a host test
  forged the CRC after setting the backup LBA to u64::MAX, and `cargo test -p libgpt --lib`
  panicked at table.rs:298. The release wrap is by inspection of the profile, not run. Probe
  removed. tests. Low. **Later.**
- [ ] **proto-16** `kernel/src/rsproto.rs:408` — Several kernel codec guards are untested:
  `forget_request`'s magic check, the u16::MAX suffix limit in `build_resolve_request`,
  `build_read_range_request` and `build_resolve_request_rename`, and the error-body minimum
  length in `parse_reply` and `parse_read_range_reply`. Without the suffix limit a suffix over
  65,535 bytes gets a truncated u16 length while body_len carries the full length, so the server
  parses a different path than the kernel meant. Without the forget magic check any message with
  op 0x0607 is a Forget. Test gap; the code is correct today. Demonstrated: each guard removed
  individually, and the rsproto tests all survived. Restored. tests. Low. **Later.**
- [ ] **proto-17** `userspace/libstream/src/setup.rs:93` — The unknown-bit rejection in
  `Streams::from_bitmap`, and the wrong-op and BadRecordTag handling in `Table::decode`, are
  untested. A setup bitmap with unknown bits and a matching handle count would have its extra
  bits ignored and the handles misassigned or leaked. An unknown record tag in `Table::decode`
  would be accepted as end of table instead of BadRecordTag. Test gap; the code is correct
  today. Demonstrated: `bitmap & !STREAM_MASK != 0` changed to `if false` and the BadRecordTag
  arm replaced with a `break`; all libstream tests passed. Restored. tests. Low. **Later.**
- [x] **proto-18** `kernel/src/syscall/table.rs:263` — `sys_file_truncate`, `sys_file_grow` and
  `sys_file_create` narrow the u64 size register with `a4 as u32` before it reaches the wire
  (`ResolveOp::Size(a4 as u32, ...)`, table.rs:251, 253, 263). A size of 4 GiB or more is taken
  modulo 2^32 instead of refused with TooLarge. `sys_file_truncate(ns, path, MAP_READ|MAP_WRITE,
  0x1_0000_000A)`, meant as "just over 4 GiB", forwards new_size=10 and the server frees
  everything past byte 10 with a success status. `sys_file_grow(4 GiB + 4096)` grows to 4096
  bytes. `SYS_FILE_RENAME`'s `a5 as u16` flags are narrowed the same way. libfs caps its own
  sizes at 8 MiB (MAX_COPY), so only direct syscall users reach this today. Unverified: read
  only. A host test of the syscall's size argument at 2^32 + n would settle it. architecture.
  Low. **Fix.**
  Fixed in `20d88936`, with `sys_channel_send`'s mode likewise; fails without it:
  `a_file_size_or_rename_flags_past_what_the_wire_carries_are_refused`.

What held up: all eight kernel `rsproto.rs` parsers (`parse_reply`, `parse_read_range_reply`,
`reply_body`, `subnamespace_reply`, `file_blocks_reply_header`, `file_blocks_run`,
`forget_request`, `reply_op`) check every offset and use `checked_add` on `body_len`; 2M random
inputs with RS_MAGIC set made none panic. About 70 librsproto decoders, their iterators and
`path::resolve` took 3M to 5M random inputs in debug (overflow checks) and release with no
panic; a negative control (dropping `parse_name_request`'s length bound) panicked at once.
`block_map` refuses block sizes that are not a power of two in 512..=4096 and runs whose file,
device or byte end overflows, and `partition_submit` rebases and bounds every IRP. `continued_path`
and `continued_dest` validate the consumed count and the joined path, and SUBNAMESPACE needs
LOOKUP. MEMOBJ and CHANNEL replies are attenuated by the transferred rights. ReadRange fills are
clamped to a page into a zeroed frame, so a short or lying reply leaks no stale memory.
`libgpt::read` bounds header_size (92..=512), entry size (128) and count (1..=1024), uses checked
arithmetic on the array range, verifies both CRCs and refuses more than 8 partitions. `build` and
`mbr::build` guard range, overlap and size. libstream bounds every `ByteSource` read and caps
`with_capacity` hints at 64. `SetupPayload::decode` and `IpcPort::recv` are bounded by one IPC
payload (~4 KiB, about 800 nesting levels, safe on the 8 MiB stack; the same holds for the log
service's `render_typed` and the view broker's `view_env`). `split_handles` refuses a claimed
terminal that arrived without a handle. `session::round_trip` closes transferred handles and
`Dir::read_dir` refuses a cursor that does not advance. Mutation tests killed by the existing
suite: the libgpt CRC and truncation guards, MAX_PARTITIONS, `build`'s overlap, range and
DiskTooSmall checks, all eight `mbr::build` guards, the kernel's file-blocks prefix and per-run
bounds, REPLY-flag and resolve-reply length checks, envelope magic and body_len, and libstream's
`take()` EOF and setup-length checks. Gap, not a finding: `cargo xtask abi-sync-check` does not
cover the `kernel/src/rsproto.rs` and librsproto pair, so drift between them would be caught
only by hand-laid layout tests. The two recorded deferrals (TODO(clipboard-chunking),
TODO(popup-follows-parent)) were not reported. proto-3 to proto-6, proto-11, proto-12 and
proto-15 to proto-17 are test gaps in code that is correct today, and by rules.md that is not
itself a finding. They are listed because the hunt reported them, and the maintainer may
want to drop them.

### security — main at 510381f4, 2026-10-20

Lenses: logic, error-paths, trust-boundary, tests; returned nothing: none.
Found 19, refuted by a boot 0, reported 15 (four were duplicates, merged).

- [x] **sec-1** `kernel/src/syscall/table.rs:1943` — `sys_channel_send` copies the sender's whole
  4096-byte buffer and never zeroes the payload past `payload_len`, and `sys_channel_recv`
  (line 2771) copies all 4096 bytes back out. `docs/spec/ipc-message-format.md` lines 81 and 144
  say the kernel zero-fills those bytes. tty-server sends every reply from one static
  `REPLY_MSG` (`tty-server/src/main.rs:247`) and never scrubs it. A `READ_LINE` reply carrying a
  typed password therefore stays at bytes 52 onward of every later short reply (the `SET_MODE`
  and `WRITE` acks have empty bodies and rewrite only bytes 24 to 51). Scenario: Bob types his
  password at the serial login, or at `with` or passwd through libprompt. Any process holding
  `/dev/tty`, which both session kinds bind (a GUI app in Alice's session, for example), sends
  `OP_TTY_WRITE` or `OP_TTY_SET_MODE` and reads the raw 4 KiB reply, which has Bob's password at
  bytes 52 onward. That lets it log in as Bob or pass `with admin`. The view broker's static
  `REPLY_MSG` (`view-broker/src/main.rs:63`) carries the tail of earlier replies (a `Show`
  policy text, other sessions' denial reasons) to any client in the same way. Demonstrated by
  host probes, all restored. A kernel test in `object/ipc_channel.rs` pushed a message with
  `payload_len` 28 and `hunter2` at `payload[28..35]`, and the bytes arrived at the peer. With
  the spec's zero-fill added as a control, the same test failed. A librsproto test encoded a
  `READ_LINE` reply with `hunter2` and then an empty `SET_MODE` reply into one buffer, and
  `msg[52..59]` was still `hunter2`. The end-to-end read of one process's password by another
  was not booted. error-paths, trust-boundary. High. **Fix.**
  Fixed in `db81c70d`; fails without it: `a_sealed_message_carries_nothing_past_its_payload`, and
  boot-probe's `payload_sealed_test`.

- [x] **sec-2** `userspace/view-broker/src/main.rs:615` — `end_session` unbinds only the storage,
  policy and accounts grants and the block devices. The services (`/dev/services/admin`), power
  (`/dev/power`), logs (`/dev/logs`) and kernel-log (`/dev/log`) grants stay bound in the view.
  `rsproto-views-ops.md` says CloseSession "unbinds their grants", and the forcible-kill entry
  in `deferred-decisions.md` relies on that so that nothing new can be resolved through them.
  Scenario: Alice runs `with admin <program>`, and the program or a child sharing its namespace
  ignores `TerminateRequested`. She logs out and session-mgr sends CloseSession. The program can
  still resolve `/dev/services/admin` to stop or start services, `/dev/power` to shut down, and
  `/dev/logs` to read every service's records and the broker's audit, after the login that
  authorized it has ended. A related gap, not run: `pair_exits` closes `view_ns` when the
  elevated program exits (main.rs:1586) without unbinding, so its descendants keep every grant,
  and CloseSession neither unbinds those grants nor asks those processes to stop. Demonstrated
  by reading the code, not by a boot. `start()` binds `SERVICES_GRANT` (1343), `POWER_GRANT`
  (1357), `LOGS_GRANT` (1371) and the kernel-log snapshot (1389). The file's only `SYS_NS_UNBIND`
  is the loop at 615-617 over `[STORAGE_GRANT, POLICY_GRANT, ACCOUNTS_GRANT]`, plus
  `unbind_block_devices`. `git log -S` shows that list came with D.3 (58a54d42) and was not
  extended by E.2b, E.4d or E.6. The unbind does reach a running program, because a spawned
  child shares its parent's namespace object (`kernel/src/syscall/table.rs:412-427`). A QEMU
  gate (`with admin` on a program that ignores terminate, log out, resolve
  `/dev/services/admin`) would confirm it end to end. logic, error-paths, trust-boundary.
  Medium. **Fix.**
  Fixed in `ec85409c`, at a program's exit too; fails without it: boot-probe's
  `view_grants_end_test`.

- [x] **sec-3** `userspace/view-broker/src/main.rs:919` — A request's grants are decided against
  `/system/views.toml` when the Request arrives and stored in `Pending`. `check()` then calls
  `start()`, which spawns with `p.grants` (line 1304) without reading the policy again. Nothing
  times out a `State::Password`. Scenario: bob sends `with admin nxsh`, the broker answers
  NeedPassword, and bob leaves the prompt open. An administrator installs a policy without bob's
  admin rule. bob then types his password, and the broker binds disks, storage, services, views
  and accounts and spawns nxsh with admin grants the policy no longer gives him. The file's own
  rule at main.rs:57 says the policy is "Read for every request: it is small, and a stale copy
  is a bug." Demonstrated by reading the path. `request()` reads the policy and decides at 919
  and stores `grants` in `Pending` (about 925). `password()` (956) only queues the password.
  `check()` (983) authenticates and calls `start()` (1012), which loops over `p.grants` (1304)
  and never calls `read_policy` or `decide` again. Only three failures, `client_gone` or
  `end_session` end a `State::Password`. Not run. error-paths. Medium. **Fix.**
  Fixed in `4b69182f`; fails without it: boot-probe's `policy_test`, two requests waiting for a
  password across an install.

- [ ] **sec-4** `userspace/view-broker/src/main.rs:531` — `serve_resolve` admits a client channel
  only by the broker's global wait-set load, with no per-session limit. Idle client channels
  are never reaped, and `end_session` leaves them in place. Scenario: a GUI app in any session
  resolves `/dev/views` 13 times (14 slots in all, 13 once the two login supervisors hold
  theirs) and holds the channels open, sending nothing. Every other session's `with` and
  `account`, and the desktop's Restart and Shut down through libviews, then get WouldBlock on
  resolving `/dev/views`. Elevation and the power button are gone for every user until that
  process closes its channels, even after its own session ends. The `deferred-decisions.md`
  availability entry (604-612) covers only root-namespace holders on `/svc/auth` and says
  sessions are not on that boundary. Demonstrated by reading the code and by running
  `cargo test -p view-broker --lib a_client_is_admitted_only_with_room`, which passes. It
  asserts that 14 clients fill `MAX_WAIT_HANDLES` = 32 (FIXED 3 + 2 each). An idle client counts
  as two slots in `load()` (1601-1613). `end_session` (598-646) touches only Running, Password
  and Changing clients. The exhaustion itself was not booted. trust-boundary. Medium. **Later.**

- [x] **sec-5** `userspace/session-mgr/src/main.rs:376` — `tty_request` takes the first message on
  the login terminal if its op matches, and never steps over tty-server's unsolicited
  `OP_TTY_INTERRUPT` (request id 0). A Ctrl-C at the serial login prompt then puts every later
  request one reply behind for the rest of `login()`. Scenario: at `nitrox login:` the person
  presses Ctrl-C. tty-server sends INTERRUPT (rid 0) and then the `READ_LINE`'s empty reply.
  session-mgr reads the INTERRUPT (op mismatch, so an empty name), and each later request takes
  the previous request's reply. The password read returns at once with nothing typed. The next
  attempt's reads return WouldBlock without waiting, because tty-server still holds a
  `READ_LINE` nobody consumes. `login incorrect` prints for attempts nobody made. Echo is
  switched back on while the stray read is outstanding, so a password typed at the `password:`
  prompt is echoed in clear. Recovery comes only when `login()` gives up after three attempts
  and reopens the terminal. Demonstrated by a temporary probe in `tty-server/src/lib.rs`
  (reverted). It drove the real `routing::Registry` with a model of `tty_request` (first
  message taken, op and error checked, request id ignored) through `login()`'s step sequence,
  with Ctrl-C during the username read. Buggy: every step returned Mismatch and none blocked for
  input. Typing `hunter2\r` after the second `password: ` wrote it to the console. Control (the
  same model skipping rid-0 messages, as `libprompt::tty` does): the resumed user read got an
  empty line, then password write Ok, echo off Ok, password read Blocked, and nothing echoed.
  logic. Medium. **Fix.**
  Fixed in `f2f7146d`; fails without it: test-interactive, a Ctrl-C at `nitrox login:` and the
  password after it.

- [ ] **sec-6** `userspace/auth-service/src/main.rs:379` — auth-service never closes handles that
  arrive with a message. `serve_resolve` (312) and `serve_session` (381) receive into
  `RECV_HANDLES` and `RECV_COUNT`, and nothing closes them. The comment "an Authenticate
  carries no transferred handles" describes well-behaved senders, not something enforced.
  view-broker's `serve_client` closes handles on every op but Request. Scenario: any process
  (`/svc/auth` is reachable by all, per the recorded TODO, which covers reachability but not
  this leak) opens a session channel and sends Authenticate messages, or resolve requests, each
  with up to 8 handles. Every received handle is installed in auth-service's table and never
  closed. Repeating this grows the table until it fails, after which the essential service
  cannot accept sessions or create channels, and nobody can log in. Verified by grep: the only
  uses of `RECV_HANDLES` and `RECV_COUNT` are the declaration and the two recv call sites. No
  per-process handle quota was found. Not run to exhaustion. A boot gate that sends a 3-handle
  message on an oracle session many times and checks that the handle count stays flat would
  settle it. tests. Medium. **Fix.**

- [ ] **sec-7** `userspace/libcrypto/src/lib.rs:36` — No test fails if `ct_eq` compares only the
  last byte. `ct_eq_basic` is the only test, and every unequal pair in it differs in the last
  byte or in length. Scenario: changing `diff |= a[i] ^ b[i]` to `diff = a[i] ^ b[i]` leaves
  `cargo test -p libcrypto --lib` at 17 passed, 0 failed. With that break, `password::verify`
  accepts any password whose 32-byte PBKDF2 output ends in the stored verifier's last byte,
  about 1 guess in 256. The libusers and auth-service tests use real PBKDF2 and also pass,
  since a random wrong password differs in the last byte 255 times in 256. Demonstrated by
  mutation, restored. tests. Medium. **Later.**

- [ ] **sec-8** `userspace/auth-service/src/lib.rs:50` — No test fails if `authenticate` accepts an
  empty password. Nothing logs in through `authenticate` or `serve_authenticate` with a wrong
  or empty password against a user that exists, and `wrong_password_denied` sends only
  `b"wrong"`. Scenario: changing the guard to `Some(rec) if rec.verifies(password) ||
  password.is_empty()` leaves auth-service's 10 tests passing, and every existing account then
  logs in with an empty password. libusers asserts `!r.verifies(b"")`, but that checks the
  record, not the service's decision. Demonstrated by mutation, restored. tests. Medium. **Later.**

- [ ] **sec-9** `userspace/libcrypto/src/sha256.rs:98` — The padding boundary at 55 and 56 buffered
  bytes is untested, and so is the HMAC key-length boundary. The vectors are the empty string,
  `abc`, a 56-byte message and 1000 `a`s. The 56-byte message leaves `buf_len` 0 after the
  first block, so it never reaches the `rem <= 56` branch. `PASSWORD_MAX` lets passwords of 65
  to 128 bytes take the hashed-down key path, and 64 versus 65 bytes is untested. Scenario:
  `rem <= 56` changed to `rem < 56` in `finalize` leaves 17 passed. `key.len() > BLOCK_LEN`
  changed to `> BLOCK_LEN + 1` in `hmac.rs` also leaves 17 passed. A 55-byte message hash, or
  the verifier of a 65-byte password, would then be wrong, locking out or colliding every
  account with such a password. Demonstrated by mutation, restored. The reviewers' hand check
  of the 55/56/63-byte tails found the code correct as it stands. tests. Medium. **Later.**

- [ ] **sec-10** `userspace/view-broker/src/main.rs:1322` — Each endpoint grant's bind treats any
  non-zero `SYS_NS_BIND` result as "not there to grant". The view is a copy of the caller's
  namespace, and a caller already in a view has those paths bound, so the kernel answers
  AlreadyBound. Only the kernel-log grant tolerates AlreadyExists (1394). Scenario: in a shell
  from `with admin nxsh`, the person runs `with admin disk --unmount x` or `with power
  shutdown --reboot`, both allowed by policy. The broker checks the password (or uses the grace
  window), then `start()` fails at the first endpoint grant with "the storage service is not
  there to grant" or "service-mgr's power endpoint is not there to grant". Both reasons are
  false, and the audit records "could not start". Demonstrated by a boot. A temporary
  test-interactive probe (restored) logged in as alice, ran `with admin nxsh`, then `with power
  shutdown --help`. The audit read `view: alice power shutdown — could not start: service-mgr's
  power endpoint is not there to grant`. The pattern `bound = endpoint != 0 && SYS_NS_BIND(..)
  == 0` is at Storage 1322-1335, Services 1341-1349, Power 1355-1363 and Logs 1369-1377, and
  `bind_endpoint` for views and accounts (1411, 1418) works the same way. `with.rs:277` sends
  `sys_ns_derive` of its own namespace unpruned. logic, trust-boundary. Low. **Later.**

- [ ] **sec-11** `userspace/view-broker/src/main.rs:1086` — A wrong current password on
  ChangePassword delays the session's next check but does not forget the session's
  remembered-password windows. `check()` (1020-1023) does both, `pacing.failed` and
  `grace.forget()`. `check_change()` (1084-1089) calls only `pacing.failed`. The spec
  (`rsproto-views-ops.md` 97-98) says "A refused password forgets every window of the session",
  but its ChangePassword text says only "a failure like any other: it holds the next check", so
  whether the forget is required there is ambiguous. Scenario: alice's correct password for
  `with admin` at terminal T opens a 5-minute window. A program in her session sends
  ChangePassword with a wrong current password. The answer is Denied "wrong password", but the
  window stays open and the next `with admin` from T starts without asking. After a wrong
  `with` password the window would have been forgotten. The divergence is real by reading the
  code. Unverified: not run, and the intended behaviour needs the maintainer's ruling. logic.
  Low.
  **No**: the maintainer's ruling. A wrong current password on ChangePassword holds the next check
  and leaves the session's windows open, as the code does; the spec's sentence is narrowed to
  `with`'s password.

- [ ] **sec-12** `userspace/session-mgr/src/main.rs:236` — When the session namespace fails to
  build after a successful login, the loop `continue`s without `tty_close`. The success path
  (264-266) and the denied path (286-288) both close the terminal. Scenario: a login whose
  namespace build fails (`sys_ns_create` out of memory, or an account home in `/system/users`
  that is not a valid subtree base, such as one with a trailing `/`) leaks one terminal. A new
  `tty_open` runs at 199. tty-server keeps every terminal in its one wait set of 32, so after
  about 30 such logins `tty_open` fails, and the serial console loops on "no terminal for
  login" until reboot. Demonstrated by reading the code. Unverified: forcing the namespace
  failure was not run. logic. Low. **Later.**

- [ ] **sec-13** `userspace/auth-service/src/main.rs:580` — `install()` discards the status of the
  `SYS_FILE_TRUNCATE` that clears a leftover `/system/users.new`, then creates the file with
  `SYS_FILE_CREATE`, which only grows. If the leftover is longer than the new database and the
  truncate failed, its tail survives and is renamed into `/system/users`. `libfs::write_file`
  (lib.rs:215-221) checks for this by truncating and then comparing the file size.
  `auth-service` does not (line 582, `let (_, h) = po_wait(po as u64);`). Scenario: an earlier
  admin write failed after create (at map, sync or rename), leaving `users.new` with S1 bytes.
  A later write produces a shorter database of n bytes (removing the account on the file's last
  line, for example), and its truncate fails. The users file is then n new bytes followed by
  bytes n to S1 of the stale copy. Memory holds the n bytes, so the running system looks right,
  but the next boot's `load_db` reads the whole file and the stale records, such as the removed
  account, come back. Unverified: it needs a fault injected into `SYS_FILE_TRUNCATE` after a
  failed install, then a boot-time reload. error-paths. Low. **Later.**

- [ ] **sec-14** `userspace/libusers/src/lib.rs:374` — Three parser bounds on user-database input
  have no test: odd-length hex, u32 overflow in the iterations field, and an empty home field.
  Scenario: (a) removing the odd-length check in `hex_decode` leaves 18 passed, so a salt field
  with an odd digit count decodes with its last nibble dropped, and a corrupt record
  authenticates under a salt different from the one written. (b) Replacing the
  `checked_mul`/`checked_add` in `parse_u32` with wrapping ops leaves 18 passed, so an
  iterations field of 4294967297 reads as 1, a trivially weak cost. (c) Dropping `||
  home.is_empty()` from `Record::parse` leaves 18 passed, so a record with no home is accepted
  and the empty home is returned to the caller. Demonstrated by three mutations, each restored.
  tests. Low. **Later.**

- [ ] **sec-15** `userspace/libprompt/src/lib.rs:119` — No test would notice if the password
  scrubbing in `confirm` or `ask_password_then` were removed. `confirm` consumes its Vecs, so a
  host test cannot observe them afterwards. Scenario: removing `scrub(&mut second);` in
  `confirm` leaves `cargo test -p libprompt --lib` at 1 passed, and the second typed copy of a
  new password would then remain in freed heap memory, which the module says it prevents
  ("every buffer here is zeroed"). The same holds for `scrub` in `ipc::send` and `ipc::recv`,
  which touch only syscalls. Demonstrated by mutation, restored. The impact is unverified; a
  test that hands `confirm` a buffer it can still inspect, such as one read through a test
  allocator after the drop, would settle it. tests. Low. **Later.**

What held up: authentication cost does not reveal which names exist, since an unknown user runs
a dummy PBKDF2 at `DEFAULT_ITERATIONS`, which equals `libusers::ITERATIONS`, and the verifier is
compared with `ct_eq` over the whole digest. The libcrypto RFC vectors catch the PBKDF2
iteration handling. The libusers `MAX_FILE` splice bound, the name and password-length rules
and the verifier length were each caught by a named test. The view-broker policy module's
mutations (the `rule.views` match in `decide`, the wildcard-with-others check, comment-in-string,
bare-name `..`, pacing, grace windows, slot admission, `Suffix::parse`, `Exits::closed` dedupe,
`refuse_removal`, session id increment) were each caught, as was the auth-service admin list
behaviour. The librsproto auth and views decoders are overflow-checked and exact, and `handles[0]`
cannot go out of bounds. Broker handles are closed on every non-Request op and deny path. The
kernel rejects `.` and `..`, so a session cannot forge another session's `s/<id>`, `policy/<id>`
or `accounts/<id>`. Password scrubbing in the broker, libsession, libprompt and auth-service
held on every path read. Auth-service's install order (truncate, create, map, unmap, sync,
rename, then memory) and its admin versus oracle routing held. The client environment decoder is
bounded by the 4 KiB message against an 8 MiB stack. All host suites pass at 510381f4. Every
probe was restored.

### scheduler — main at 510381f4, 2026-10-20

Lenses: logic, error-paths, trust-boundary, tests; returned nothing: none, but the architecture lens's report was cut off before any findings came back, so its empty list is not a clean result and the hunt is incomplete until it is rerun.
Found 17, refuted by a boot 0, reported 10 (17 raw findings; 7 merged as duplicates).

- [x] **sched-1** `kernel/src/object/timer.rs:206` — Destroying a Timer never removes its deadline-heap
  entry. The heap holds a raw pointer with no reference, and neither `Timer::drop` nor
  `dispatch_destroy` calls `deadline::remove`. Any process, with no rights needed, runs
  `sys_timer_create`, `sys_timer_set(t, now+X, 0)`, then `sys_handle_close(t)` or exits. When X
  passes, `fire_expired_deadlines` runs in IRQ context under SCHED and writes the freed slab slot
  (`set_in_heap`, `take_waiters`, `interval`, `set_armed`; `sched/timers.rs:54-59`). If the slot now
  holds another Timer, that timer is disarmed early and its own heap entry is orphaned. A periodic
  timer re-pushes itself and writes the freed slot every interval. The test image hits this on
  every boot (`userspace/hello` closes an armed 10 s timer), and `sleep 30` stopped with Ctrl-C
  does too. Demonstrated: four host probes in `sched/tests.rs`, each since removed. In the main
  one, timer A is armed for t=1000 and its only ref dropped (`timer_destroys == 1`). Timer B then
  reuses A's slot and is armed for 1e9. `fire_expired_deadlines(2000)` leaves B with `deadline=0`
  and `in_heap=false`, and the heap names the freed A. A control that disarmed A before the drop
  left no entry. logic, error-paths, trust-boundary, tests. high. **Fix**, as kobj-1.
  Fixed in `fcc06ce8`; fails without it:
  `a_timer_destroyed_while_armed_leaves_nothing_in_the_heap_naming_it`.

- [x] **sched-2** `kernel/src/sched/wait.rs:53` — A wake panics the kernel when every
  affinity-permitted run queue is at `READY_RESERVE` (32). `make_runnable` (and `resume_suspended`,
  `exit.rs:379`) has no refusal path, although spawn refuses at a full queue. A process uses
  `sys_thread_create` with `cpu_affinity=1` (TimeShared, no capability) to make 32 busy threads on
  CPU 0, and has one more thread pinned to CPU 0 blocked on a timer. When the timer fires,
  `pick_target_cpu` returns CPU 0 again, `place_thread` fails and the timer IRQ panics, although
  other CPUs' queues are empty. On a 1-CPU machine 32 runnable threads suffice with no pinning. The
  `READY_RESERVE` wake cap is not among the recorded deferrals (only `REAP_RESERVE` is).
  Demonstrated: a host probe (removed) with 2 CPUs online filled `ready[0]` with 32 mask=1 threads
  and called the real `make_runnable` on a blocked mask=1 thread. CPU 1 had room, and the call
  panicked at `wait.rs:71` with 'wake placement: no ready queue would take the thread'. logic.
  high. **Fix.**
  Fixed in `deabe050`; fails without it:
  `every_thread_the_cap_admits_can_block_wake_and_be_reaped_on_one_cpu` and
  `a_thread_counts_from_its_first_placement_until_its_reap_is_drained`; a 512-thread cap every
  scheduler list is reserved to hold.

- [x] **sched-3** `kernel/src/sched/wait.rs:29` — Blocking has no capacity check, and the 64-entry
  global `blocked` list panics when full. `wait_on` goes straight to `block_current_and_switch`,
  which calls `push_reserved`. The 64-entry `suspended` list (`exit.rs:322`) does the same. The doc
  at `sched.rs:98-102` says blocking beyond the reserve "is refused". One process makes threads
  with `sys_thread_create` (no per-process limit), each calling `sys_wait` on a handle that never
  signals. Resource servers already occupy many slots, so fewer than 64 may be needed. The next
  `sys_wait` by any thread panics with 'blocked list: reserve exhausted'. 65 threads that each
  fault in ring 3 overflow `suspended` the same way. `TODO(unsupervised-fault)` records only the
  leak, not this panic. Demonstrated: a host probe (removed) on a test `SchedState` measured
  blocked cap=64, suspended cap=64, heap cap=16, ready cap=32. The 65th `push_reserved(&mut
  st.blocked, ..)` panicked with 'blocked list: reserve exhausted'. `git show aec52d83` shows an
  earlier `debug_assert` plus `try_push` was turned into this panic. logic, error-paths. high.
  **Fix.**
  Fixed in `deabe050`; fails without it:
  `every_thread_the_cap_admits_can_block_wake_and_be_reaped_on_one_cpu` and
  `a_thread_counts_from_its_first_placement_until_its_reap_is_drained`; a 512-thread cap every
  scheduler list is reserved to hold.

- [x] **sched-4** `kernel/src/sched/exit.rs:59` — `reap_matching` assumes ready threads "register on
  no wait objects". That is false for a thread a waker just moved blocked to ready: it stays on the
  other wait objects' waiter lists and on the deadline heap until `wait_on`'s resume path runs.
  Thread A of process P calls `sys_wait([X, T], finite deadline)`, X signals, and A sits in ready
  for up to a quantum, since syscall return does not reschedule. Sibling B calls
  `sys_process_exit`, and `exit_process` reaps A from ready and frees it. T's waiter list and the
  heap's Thread entry still name A. When T or the deadline fires, `wait_try_wake` and
  `wait_mark_signaled` run on freed memory. If a new Thread reused the slot, an unrelated thread
  wakes early with a wrong result. Demonstrated: a host probe (removed) registers inert thread A
  (pid 7) on timers t1 and t2 with a deadline entry and parks it in `blocked`. Firing t1 moves A
  to `ready[0]`, and `reap_matching(ready[0], reap[0], 7)` reaps it. After the reap list dropped,
  A was freed, was still a t2 waiter and still had a heap Thread entry. error-paths. high. **Fix.**
  Fixed in `deabe050`; fails without it:
  `a_woken_thread_reaped_before_it_runs_leaves_nothing_naming_it`.

- [ ] **sched-5** `kernel/src/sched/exit.rs:86` — When `exit_process` reaps a sibling blocked in
  `sys_wait` (or woken but not yet resumed), the sibling is never resumed. `sys_wait`'s on-stack
  `refs: [Option<ObjectRef>; 32]` never drop, so each waited-on object leaks one reference. Thread
  A of a process waits on IPC endpoint E while thread B calls `sys_process_exit`. A's stack
  reference keeps E alive forever, so `IpcChannel::drop` and `ipc_endpoint_closing` never run and
  the peer never sees `PeerClosed` (at most `MAX_WAIT_HANDLES-1` objects per thread leak). Timers
  and POs leak the same way. Demonstrated: a probe on the `test-qemu` image (since restored). A
  child holds one end of a channel and the parent waits on the peer. Control: the child alone
  calls `process_exit` and the parent's wait returns 1. Test: the child first starts a second
  thread blocked in `sys_wait` on that endpoint, then calls `process_exit`. The parent's wait
  times out (-12) after 2 s. error-paths. medium. **Fix.**

- [ ] **sched-6** `kernel/src/sched/deadline.rs:31` — The deadline heap is one global reserve of 16
  entries (`HEAP_RESERVE`, `spawn.rs:32`), shared by every armed timer, finite-deadline `sys_wait`
  and BlockBounded send, with no per-process bound. Because of sched-1, closing or exiting does not
  give the slots back. A process arms far-future timers until refused, and may then exit. From then
  on every `sys_timer_set` returns OutOfMemory, and every finite-deadline `sys_wait` that finds
  nothing ready returns OutOfMemory at once. service-mgr (`main.rs:1392`) and session-mgr
  (`main.rs:79`) spin instead of sleeping. storage-service's `park` (`main.rs:147`) ignores the
  failed `sys_timer_set` and waits `u64::MAX` on an unarmed timer, so an eject with held files
  hangs. Demonstrated: a probe on the `test-qemu` image (since restored). 11 far-future timers
  armed (the other 5 slots were in use), then OutOfMemory (-4). After closing all 11 and waiting
  50 ms, a fresh `sys_timer_set` still returned -4, and a finite-deadline `sys_wait` on an
  unsignalled timer returned -4 immediately. logic, error-paths, tests. medium. **Fix.**

- [x] **sched-7** `kernel/src/sched/timers.rs:97` — `fire_timer` disarms a one-shot timer
  (`set_armed(0,0)`) when it fires even if no thread waits, and `Timer::already_signaled` requires
  `deadline_ns != 0`, so an elapsed timer never reads as signaled again. storage-service `park(5
  ms)` does `sys_timer_set(now+5ms)` then `sys_wait(timer, u64::MAX)`. If it is preempted between
  the two for at least 5 ms, a tick fires and disarms the timer with no waiter. The later wait
  then blocks forever with no heap entry and hangs the storage service during an eject with held
  files. A deadline-0 poll returns WouldBlock for an elapsed timer too. `coreutils sleep` has the
  same window, but Ctrl-C recovers it. Demonstrated: a host probe (removed) armed a one-shot at
  100. `already_signaled(150)` was true, and after `fire_expired_deadlines(st, 120)` it was false
  and the timer was no longer scheduled. A second probe armed 1000, fired at 2000 with no waiters,
  and `already_signaled(3000)` failed. error-paths, tests. medium. **Fix.**
  Fixed in `1b6ee7ea`, and in `76e0dddd` for a waiter whose clock was read before the fire, which
  PR #391's review found; fails without them:
  `a_one_shot_timer_fired_with_no_waiter_still_reads_signaled`.

- [ ] **sched-8** `kernel/src/sched/exit.rs:181` — When the last thread of a process ends through
  `exit_thread` (`sys_thread_exit`, or a supervisor's Terminate after a fault), `set_process_ended`
  runs but the pid is never pushed to `ended_pids` and the reaper is not woken, although the
  comment there says it is deferred to the reaper. `exit_process` does both (`exit.rs:237-240`).
  Only the per-CPU `reap_pending` sweep closes the handle table. On a CPU kept busy by TimeShared
  threads, `on_timer_tick` never calls `reap_pending`, so the table stays open and peers wait
  indefinitely for `PeerClosed`. Demonstrated: a probe on the `test-qemu` image (since restored).
  With 12 spinning TimeShared threads, a child whose sole thread ended via `sys_thread_exit` never
  delivered `PeerClosed`, and the parent's wait timed out (-12) after 3 s. With the spinners
  stopped, the same wait returned 1. Control under the same load: a child using `sys_process_exit`
  closed promptly (wait=1). error-paths. low. **Fix.**

- [x] **sched-9** `kernel/src/sched/deadline.rs:84` — Test gap: the sift-up branch of
  `deadline::remove`, taken when the entry swapped into the hole is smaller than its new parent, is
  uncovered. Breaking it could leave a later deadline above an earlier one, so
  `fire_expired_deadlines` (which stops at the first entry later than `now`) would delay wakes.
  Demonstrated: a mutation collapsing the if/else to `sift_down(h, i)` still passed all 986 host
  tests; restored. tests. low. **Fix**, with kobj-1.
  Fixed in `fcc06ce8`; fails without it:
  `removing_an_entry_whose_replacement_is_smaller_than_its_parent_keeps_the_heap_ordered`.

- [ ] **sched-10** `kernel/src/sched/timers.rs:50` — Test gap: nothing on the host exercises
  `fire_expired_deadlines`, `fire_timer`, `timer_arm` or `wait_on`, and sched-1 and sched-7 sit in
  exactly this code. Six mutations each passed the 986 host tests: `wrapping_add` instead of
  `saturating_add` for the periodic next deadline; treating a deadline as expired early;
  periodic re-arm disabled; `timer_arm` pushing for `deadline_ns == 0`; `wait_on`'s deadline==0
  poll path removed; `has_deadline` forced false. Unverified: the mutation runs were not repeated
  by the verifier. The boot probes for sched-5, sched-6 and sched-8 did show those paths run only
  under a QEMU gate. tests. low. **Fix**, with kobj-1.
  Five of its six mutations now fail a test: the four in the timer code by `fcc06ce8`'s
  `sched::tests`, and `has_deadline` forced false by `test-qemu`. Removing `wait_on`'s poll path
  still fails nothing: a poll then blocks until the next tick, which no host test can drive
  without a running thread. Open, for Part H.

What held up: `sys_wait` registration and blocking share one SCHED hold, so no lost wakeup, and a
partial registration is fully unwound. The `wait_try_wake` CAS plus `make_runnable`'s not-in-blocked
check catch a second waker. The `on_cpu` switch-out guard holds in `switch_into`, `stealable_to` and
the reap paths. A 1 ns periodic timer cannot livelock the drain, and deadline arithmetic saturates.
Affinity masks cannot overflow and an empty mask is refused. A mask naming only parked CPUs falls
back through `pick_target_cpu`. Spawn placement refuses at `READY_RESERVE` and drops the ref outside
SCHED. `preempt_enable` saturates and replays only on the 1 to 0 transition. `PendingSend` and
Thread deadline entries are removed on every exit, so they cannot dangle. `clock.rs` arithmetic is
checked and an unset clock returns None. The DPC queue dedups by `queued` and clears it before the
handler. `sys_wait` bounds its count and copies out only `k<=count` records. Mutations were caught
for dropping the `online_mask` check, ignoring affinity, breaking vruntime order, clearing DPC
`queued` late, and notification overflow eviction. One thing was seen but not raised: `fire_timer`'s
periodic re-push swallows a full-heap error with `let _`, so the timer silently stops. It shares
sched-6's sizing cause.

### supervision — main at 510381f4, 2026-10-20

Lenses: logic, error-paths, trust-boundary, tests; returned nothing: none.
Found 21, refuted by a boot 0, reported 16.

- [ ] **sup-1** `userspace/storage-service/src/main.rs:1437` — storage-service sends `Meta::Ready`
  only after it has probed every device and auto-mounted every filesystem, but service-mgr gives
  the whole startup the same 30 s (`READY_TIMEOUT_NS`, counted from spawn). A slow disk or
  fs-server therefore makes service-mgr fail the storage service for the whole boot. A USB stick
  that answers slowly is present at boot. `read_table` and `probe_record` block in `po_wait`
  (`u64::MAX`, no deadline). Each `mount()` can wait 30 s for its fs-server's Ready
  (mounting.rs:69). The kernel's USB command deadline is also 30 s. service-mgr's deadline
  (main.rs:1302) passes first: it logs "sent no Ready within 30 s" and calls `failed_to_start`.
  When the late Ready arrives, `channel_peer_closed` (main.rs:2273-2304) drains it and closes the
  endpoint it carried. storage-service then sees its `serve_end` peerless, logs "forwarding
  endpoint closed" and exits 1 without its unmount chain. No `/svc/storage` is available for the
  rest of the boot (every declaration is policy=never). Sessions get no `/storage`. There is no
  InUse, so the broker refuses `disks`. The `block` class is released. The auto-mounted ext4
  filesystems stay marked not clean. Demonstrated: with `send_ready` delayed 35 s (probe
  restored), `cargo xtask check-storage` printed "sent no Ready within 30 s", "did not come up;
  going on without it", "unexpected message on a control channel (dropped, and 1 handle(s) it
  carried closed)" and "forwarding endpoint closed", and the gate failed. A natural 30 s stall
  from a real stick rests on the code-read USB deadline and was not booted. Lenses: logic,
  error-paths, trust-boundary (three reviewers found it independently). High. **Fix.**

- [ ] **sup-2** `userspace/storage-service/src/lib.rs:95` — the `gpt-partuuid` matching in
  `source_device` reads the disk's table through libgpt and compares libgpt's entry name with the
  kernel's record name. The two read a GPT differently, so a partuuid root that init mounts is
  left unmatched. `init_known` then goes false, nothing is auto-mounted, and every admin Mount,
  Reread and InUse is refused. The view broker refuses the `disks` grant. Five cases: (1) an
  unlabelled root (the kernel names it "partition 2 (unlabelled)", kernel gpt.rs:153; libgpt gives
  an empty name); (2) a non-ASCII, `/` or control-character label (the kernel says unlabelled;
  libgpt gives `?`, table.rs:321); (3) more than 8 partitions (libgpt `MAX_PARTITIONS`, so
  `read_table` yields nothing, main.rs:462); (4) an entry size other than 128; (5) a bad CRC (the
  kernel still binds `by-partuuid`). Demonstrated: temporary host tests in storage-service's
  tests.rs built a real GPT with `libgpt::table::build` and read it as `read_table` does. The
  unlabelled root printed `source_device = None` and `init_known = false`. A nine-partition disk
  printed `read_table = None`. Run with `cargo test -p storage-service --lib probe_ --
  --nocapture`; the probe was removed. Lens: trust-boundary. Medium. **Later.**

- [ ] **sup-3** `userspace/service-mgr/src/main.rs:160` — `compute_backoff`'s exponential arm uses
  `initial_ns.checked_shl(attempts)`, which fails only for shifts of 64 or more. Bits shifted out
  wrap silently, so the backoff becomes 0 for attempts 55-63 (56-63 for a 100 ms initial). A
  service with `backoff = "exponential"` and the default unlimited `max_attempts` that keeps
  failing gets 9 back-to-back restarts after about 4.5 hours at the 5 min cap. It does not wait
  `backoff_max`, as the schema says. No shipped declaration reaches it. The function lives in
  main.rs, so no host test can reach it. Demonstrated: the expression compiled verbatim in
  standalone programs. At 1 s/5 min, attempt 54 gives 300000000000, attempts 55/56/60/63 give 0,
  and attempt 64 gives 300000000000. 200 ms/2 s also gives 0 at 55 and 63. Fix: saturating
  multiply, or clamp the shift. Lenses: logic, trust-boundary, tests (three reviewers; severity
  rated low, low and medium). Medium. **Later.**

- [ ] **sup-4** `userspace/service-mgr/src/main.rs:1192` — `serve_resolves` loops `recv_serve`
  per route until the ring is empty, with no per-pass bound. `serve_services` does the same for
  the root (1846-1856) and the session endpoint (1861-1863). A client that issues forwarded
  resolves faster than service-mgr answers keeps it inside the loop. A session app (or two) on
  another CPU loops async `sys_ns_lookup` on a path behind a route, such as `/dev/tty/x` or
  `/dev/clipboard/x`, and keeps the 64-deep ring refilled. During that time no child exit is
  reaped and no restart fires. No admin or power request is answered (Shutdown included), and no
  Ready deadline is acted on. storage-service and device-mgr take one message per handle per
  pass. Unverified: a two-thread flood client on `/dev/tty` while `service --stop` is issued would
  show whether the stop is answered before the flood ends. Lens: logic. Medium. **Later.**

- [ ] **sup-5** `userspace/storage-service/src/main.rs:642` — every resolve of `info/<name>.tsm`,
  including any session's `/dev/storage/all.tsm`, calls `refresh_unmounted`. That re-probes every
  unmounted device synchronously on the service's only thread, and each read is a `po_wait` with
  no deadline. One slow device with no filesystem (so it is never mounted) costs about 3-4
  commands per probe. While storage-service is in that probe, every `/storage` and `/dev/storage`
  resolve waits, because forwarded resolves have no deadline. desktop-shell's per-launch
  `/storage` check (apps.rs:194, `u64::MAX`) freezes. Any client can repeat the stall. A
  `CTRL_OP_SHUTDOWN` goes unread during it, so service-mgr gives up after storage's 60 s
  `stop_timeout`, and writable auto-mounts are never synced or marked clean. Only Ejected devices
  are skipped (main.rs:586). Unverified: time `cat /dev/storage/all.tsm` on a throttled QEMU
  usb-storage stick with no filesystem, and run `ls /storage` meanwhile. Lens: trust-boundary.
  Medium. **Later.**

- [ ] **sup-6** `userspace/storage-service/src/main.rs:704` — media sessions
  (`MAX_MEDIA_SESSIONS` = 2) and directory sessions (`MAX_DIRS` = 9) are one pool shared by every
  session on the machine. There is no per-client bound and nothing reclaims an unused session; a
  slot is freed only on `PeerClosed` (serve_media 750-760, serve_dir 1094-1104). Every app has
  `/dev/storage` (`session_only` lets Media and Directory through, lib.rs:899-904). One app that
  resolves `/dev/storage/media` twice and `/storage` nine times and holds the channels makes
  every other user's eject get WouldBlock (`open_media_session`) and every listing get WouldBlock
  (`open_dir`, main.rs:1080). The user cannot eject and pulls the stick unsynced. device-mgr has
  the same shape for `/dev/devices` directory sessions (main.rs:55, 389). Unverified: a
  boot-probe client that holds two media sessions should get WouldBlock on a third resolve.
  Lens: trust-boundary. Medium. **Later.**

- [ ] **sup-7** `userspace/storage-service/src/main.rs:483` — storage-service reads `init.toml`
  differently from init. It stops at the first NUL and maps the object's whole size. Init
  (init/src/main.rs:182, 191) maps exactly one page and trims only trailing NULs. This breaks
  `userspace/init/CLAUDE.md`'s rule that it "must read the file exactly as init does". A NUL
  (for example in a comment) between two `[[mount]]` entries makes init mount `/` and `/data`,
  while storage-service sees only `/`. `init_known` stays true, so automount treats `/data`'s
  partition as free and mounts it a second time with its own fs-server-ext4. Two writers then
  share one ext4. A manifest over 4 KiB is likewise cut by init at one page but read whole by
  storage-service. Demonstrated: a temporary libinittoml test, since removed, put a manifest with
  a NUL in a comment between two mounts into a 4096-byte page and sliced it with each rule. It
  printed `init mounts ["/", "/data"]` and `storage mounts ["/"]`. Lens: logic. Low. **Later.**

- [ ] **sup-8** `userspace/service-mgr/src/main.rs:280` — `skipped_critical` counts every skipped
  declaration that says `critical = true`, including a Duplicate (or a NoExecutable re-entered
  block) of a critical server that was already read in full. `bringup::unfit` then reports "a
  critical declaration could not be read". service-mgr starts nothing and asks init for the
  emergency shell (main.rs:694-697). A second `[service.auth-service]` block with
  `critical = true`, whether a whole duplicate or an appended block, boots to eshell although
  every critical server was parsed, and the parser documents that a repeated name is "skipped,
  not re-emitted". Demonstrated: a temporary service_toml.rs test fed `parse_all_reporting`
  through main.rs's Entry mapping and `skipped.iter().any(|s| s.critical)` into `unfit`. It
  printed `Skipped { name: "auth-service", why: Duplicate, critical: true }` and
  `unfit = Some("a critical declaration could not be read")`. The appended block gave
  `NoExecutable` and the same result. The probe was removed. Lens: error-paths. Low. **Later.**

- [ ] **sup-9** `userspace/service-mgr/src/main.rs:1231` — bring-up's `Step::Start` calls
  `start(i, true)` without checking `running`. An admin Start of a declaration bring-up has not
  reached (`start_asked` via `decide`, main.rs:2121-2123) spawns it early. Bring-up later spawns a
  second instance, and `start` overwrites `proc_h` and `ctrl` (1297-1300). The first instance's
  handles leak. Its exit is no longer attributed and lands in `codes` as unattributed. `Stop` and
  shutdown reach only the second. A server's second Ready rebinds the registry and closes the
  first's endpoint. The window is narrow: bring-up waiting on an `after` (up to 20 s) or a
  server's Ready (up to 30 s) while an admin session issues Start for a later declaration, as
  with nxterm `after = ["display-selftest"]` in the test image. Unverified: `service --start
  nxterm` during that wait, then count nxterm processes or "starting service" lines. Lenses:
  logic, error-paths (same defect). Low. **Later.**

- [ ] **sup-10** `userspace/service-mgr/src/main.rs:1374` — `wait_codes` is meant to wait up to
  `CODE_GRACE_NS` for a dead service's exit code, but it returns false when any route wakes the
  wait, because `wait_serving` returns Some only if notif itself is ready. `poll()`'s `while
  dead.len() > codes.len() && self.wait_codes(until)` then stops, and the death is reaped with
  code unknown. `sys_process_exit` closes the control channel before it queues `ChildExited`. A
  resolve reaching any route in that gap makes an on-failure service that exited 0 look like -1,
  so it is restarted and shown as "failed". The real code is logged later as an unsupervised
  child's, or taken as the next death's. `lookup_serving` loops to its deadline; this does not.
  Unverified: on SMP, a server that exits 0 before its Ready while another process resolves
  through a route in a tight loop; look for `code=unknown` followed by "unsupervised child
  exited code=0". Fix: loop until `until`. Lens: error-paths. Low. **Later.**

- [ ] **sup-11** `userspace/service-mgr/src/main.rs:2202` — login supervisors get their route
  copies once, at login-chain start (574-576, 600-602), and only for servers live then
  (`route_copy` requires `r.live`, 1549). If storage-service or device-mgr misses its Ready at
  boot (sup-1, say), session-mgr and desktop-session-mgr get an empty slot. A later
  `service --start storage-service` creates the route and the session-endpoint route, but nothing
  re-sends them, so every current and future login lacks `/storage`, `/dev/storage` or
  `/dev/devices` until reboot. Routes exist so that a restart reaches every session. Unverified:
  boot with storage-service failing its Ready, start it from an admin session, log in, and
  resolve `/storage`. Lens: trust-boundary. Low. **Later.**

- [ ] **sup-12** `userspace/storage-service/src/probe.rs:125` — `partition_entries`, which decides
  whether a whole disk's own filesystem is ignored, has conditions no test pins: the 0xAA half of
  the signature, `e[4] != 0`, `first != 0`, `count != 0` and the bound `end <= blocks`. The
  `RamDisk` arm of `whole` is also unpinned. A regression would treat a disk with a stale sector 0
  as partitioned (a FAT or ext4 that should mount is reported as Nothing), or mount a partitioned
  disk whole over its partitions. Demonstrated: each mutation alone passed `cargo test -p
  storage-service --lib` (5 survivors, plus the `RamDisk` removal). Lens: tests. Low. **Later.**

- [ ] **sup-13** `userspace/storage-service/src/probe.rs:150` — `fat_label`'s FAT12/16 branch
  (the `0x29` extended signature at 0x26 and the FAT string at 0x36) and the sector-size
  allow-list lack negative tests. Removing the `sector[0x26] == 0x29` check, or accepting 0 as a
  sector size, passed all storage-service tests. A non-FAT sector carrying those bytes would be
  probed as FAT and offered for mounting. Demonstrated: the two mutations passed the suite; the
  FAT32 branch equivalents were killed. Lens: tests. Low. **Later.**

- [ ] **sup-14** `userspace/init/src/unmount.rs:42` — `unmount::answer` has no test for a wrong
  `RS_MAGIC`. Reducing the check to the length test passes all of init's tests, so a non-rsproto
  payload with the right bytes at offsets 6 and 16 would be accepted as a clean unmount
  (`Answer::Done`). The `reply_id` is also not matched to the request. Demonstrated: the
  mutation passed `cargo test -p init --lib`; `anything_else_is_unexpected` covers wrong op,
  not-a-reply and truncation only. Lens: tests. Low. **Later.**

- [ ] **sup-15** `userspace/service-mgr/src/service_toml.rs:418` — an unrecognised or malformed
  restart `policy` falls back to `Never`, but no test pins it. Changing the fallback to `Always`
  passed `cargo test -p service-mgr --lib`, so a regression would make every mistyped
  declaration (`policy = "on-failur"`) restart forever. `parse_duration_ns`'s overflow behaviour
  is untested too. Demonstrated by the surviving mutation. Lens: tests. Low. **Later.**

- [ ] **sup-16** `userspace/libinittoml/src/toml_lite.rs:257` — `parse_value` accepts a string
  with an interior quote: `key = "a"b"` parses as the value `a"b` instead of
  `UnterminatedString`. The `body.contains('"')` rejection is untested. Replacing it with
  `false` passed all 19 libinittoml tests. The manifest is trusted initramfs data, so the impact
  is limited. Demonstrated by that mutation. Lens: tests. Low. **Later.**

What held up: about 105 single-line mutations across storage-service, the service-mgr lib,
libinittoml, device-mgr and init were killed by the tests, apart from the survivors above and
some equivalent mutants. The kills covered eject, reread, label validation, watch-list capacity,
endpoint narrowing, bring-up ordering, `service_toml` first-wins, manifest validation and
device-mgr Owners. Sound on reading:
- libinittoml has no panic path on any input.
- init's `ready.rs` and `unmount.rs` parsers handle lying lengths with `checked_add` and `get`,
  and `handles_to_close` caps a bogus count.
- Handles that arrive unasked are closed in every receive path.
- service-mgr's wait budget is 32, held requests are refused on shutdown, and a route cannot be
  walked out of through `..`.
- Session and info-only endpoints cannot mint admin, session or class endpoints.
- storage-service's partition-table and superblock parsers (GPT, MBR, FAT, ext4) have checked
  divisors and bounded slices, and `DeviceIo::read_at` fails with `Err(Io)` rather than reading
  past its mapping.
- Eject refuses the boot medium, a non-removable disk and any disk init mounts.
- device-mgr's subscription depth is capped and Departed goes only to an owner that was handed
  the device.

### kernel-memory, architecture lens re-run — main at 510381f4, 2026-10-20

Lenses: architecture; returned nothing: none.
Found 11, refuted by a boot 0, reported 11.

- [x] **kmem-20** `kernel/src/mm/addr_space.rs:274` — `AddressSpace::fault_in` treats
  `ArchMapError::AlreadyMapped` on an anonymous lazy page as impossible and calls `unreachable!()`,
  but on SMP two threads can take not-present faults on the same page, and the second finds the PTE
  the first installed. Any process: two threads on different CPUs touch the same untouched page of
  a lazily mapped anonymous VMA. The 8 MiB initial stack is lazy, and `sys_thread_create` (no
  privilege needed) accepts a `user_sp` inside it, so two threads can be pointed at one fresh
  stack page. CPU A takes the AS lock and maps the page. CPU B then takes the lock, finds the VMA,
  allocates a frame, gets `AlreadyMapped` from `map_page` and panics the kernel. Demonstrated: a
  host probe in the `addr_space` tests did `map_vma_lazy`, then `fault_in(PAGE*5, Write)` twice,
  the second call standing in for CPU B's late fault. The first returned `Mapped`; the second
  panicked at `addr_space.rs:274` with "entered unreachable code: fault_in: map_page rejected a
  fresh page-aligned not-present mapping". The race itself is read from the code: the #PF gate is
  an interrupt gate (`idt.rs:63`, 0x8E), so on one CPU it cannot happen, but nothing serialises
  two CPUs between the hardware fault and the AS lock. The sibling `map_file_page` already treats
  `AlreadyMapped` as success. Architecture. High. **Fix**, as kmem-3.
  Fixed in `183111ab`; fails without it:
  `a_fault_on_a_page_another_cpu_just_mapped_is_a_retry_and_keeps_no_frame`.
- [x] **kmem-21** `kernel/src/mm/buddy.rs:163` — any kernel allocation over 4 MiB panics. `kmalloc`
  sends it to the slab's `large_alloc` (`slab.rs:402`), which computes a buddy order above
  `MAX_ORDER` (10) and calls `buddy_alloc`, whose `debug_assert!(order <= MAX_ORDER)` fires. The
  kernel ships in the dev profile (xtask runs `cargo build` with no `--release`), so debug
  assertions are on in real images. Any process: `sys_memory_create(5 MiB)` (`MAX_SIZE` is
  16 MiB), then `sys_process_spawn` with that handle as the image. `MemoryObject::copy_to_kvec`
  calls `buf.try_reserve(5 MiB)` (`memory_object.rs:193`), which reaches `large_alloc` at order 11
  and panics the kernel. The same panic follows from spawning any `FileObject` over 4 MiB
  (`read_to_kvec`, `file_object.rs:1017`), such as a large file run from a stick; from
  `sys_memory_unmap` of a FileBacked VMA over about 512K pages (a mapped file of 2 GiB or more),
  since `frames.try_reserve(pages)` at `table.rs:938` reserves 8 bytes per page that a FileBacked
  unmap never pushes; and from `block_map`'s `run_count` (kmem-24). Any `KVec` that doubles past
  about 2 MiB hits it too. Demonstrated: a host probe in the `memory_object` tests,
  `MemoryObject::try_new(5 MiB)` succeeded, then `copy_to_kvec()` panicked at `buddy.rs:163` with
  "buddy: alloc order 11 > MAX_ORDER". Fix the class in `large_alloc` (return null when
  order > `MAX_ORDER`), not at each caller. Architecture. High. **Fix**, as kobj-3.
  Fixed in `e56c6612`; fails without it: `an_order_past_the_largest_block_is_refused`,
  `a_large_alloc_past_the_largest_block_is_refused`,
  `copying_an_image_past_the_largest_block_is_refused`,
  `a_file_blocks_header_claiming_more_runs_than_its_body_holds_is_refused` and
  `a_block_map_the_page_cache_cannot_use_is_refused`; the channel depth falls to 512, the deepest
  whose ring fits.
- [x] **kmem-22** `kernel/src/mm/elf.rs:182` — `parse_phdr` computes `off + ELF64_PHDR_SIZE` with
  unchecked addition on `e_phoff`, which comes straight from the ELF. The kernel is built with
  `overflow-checks = true` (profile.dev), so an `e_phoff` near `u64::MAX` panics instead of
  returning `Truncated`. Any process writes an ELF header with `e_phoff = u64::MAX - 8` and
  `e_phnum >= 1` into a MemoryObject from `sys_memory_create` (full rights, MAP_READ included) and
  passes it to `sys_process_spawn`; `load_elf` then `parse_phdr` hits "attempt to add with
  overflow" and the kernel panics. Demonstrated: a host probe in the `elf` tests, an `ElfBuilder`
  with one PT_LOAD and bytes 32..40 set to `u64::MAX-8`; `load_elf` panicked at `elf.rs:182:22`
  with "attempt to add with overflow". Architecture. High. **Fix**, as kobj-4.
  Fixed in `badc91cd`; fails without it:
  `a_program_header_offset_at_the_top_of_the_range_is_truncated`.
- [x] **kmem-23** `kernel/src/mm/elf.rs:128` — `load_elf` returns `e_entry` from the untrusted ELF
  without checking that it is a canonical user-half address. `enter_user` starts the thread with
  `iretq`, and an `iretq` to a non-canonical RIP raises #GP while still at CPL 0;
  `exception_dispatch` then sees a kernel-mode fault and calls `dump_and_halt`. Any process builds
  an ELF with `e_entry = 0x0000_8000_0000_0000` in a MemoryObject and calls `sys_process_spawn`.
  The load succeeds, `spawn_user` queues the child thread, and its first entry to ring 3 halts the
  machine. `sys_thread_create` cannot be used this way, because it puts `entry` through
  `UserPtr::new` (< `USER_VIRT_END`); the ELF path has no such check. Demonstrated: a host probe,
  `load_elf` with `entry(0x0000_8000_0000_0000)`, returned `Ok(EntryInfo { entry_point:
  VirtAddr(140737488355328), .. })`. Boot verified: with a probe making `load_elf` return that
  entry, `cargo xtask test-qemu --kvm` spawned init and its first `enter_user` `iretq` raised
  vector 13 (#GP) in ring 0. The dump shows CPU#0 `idt::vec13` -> `enter_user+0x3d` ->
  `thread_enter`, "stopping every CPU", then a test timeout. Under TCG, QEMU reports the `iretq` as
  a ring-3 segfault ("unhandled ring-3 fault, pid 1"), so the halt shows on KVM only, as it would
  on real hardware. A child from `sys_process_spawn` takes the same path. Architecture. High.
  **Fix**, as kmem-1.
  Fixed in `badc91cd`; fails without it: `an_entry_outside_the_user_half_is_refused`.
- [x] **kmem-24** `kernel/src/syscall/table.rs:2589` — `block_map` trusts the reply's `run_count`:
  it reserves `run_count` `BlockRun`s (24 bytes each) before checking that the body holds that
  many, and a 4 KiB message fits about 170. A filesystem server's Model A resolve reply claims
  `run_count = 200000` while carrying one run. `runs.try_reserve(200000)` asks `kmalloc` for about
  4.8 MiB, which reaches the buddy at order 11 and panics via the assert in kmem-21. With that
  fixed it still tries to allocate up to about 96 GiB before the body-length check rejects the
  reply. Block sizes and run ends in these replies are otherwise validated. Demonstrated: a host
  probe in the `syscall::table` tests, a body of 32 + 24 bytes with block_size 512, `run_count`
  200000 and one valid run; `block_map` panicked with "buddy: alloc order 11 > MAX_ORDER". Fix:
  bound `run_count` by `(body.len() - 32) / 24` before reserving. Architecture. Medium.
  **Fix**, as kobj-3.
  Fixed in `e56c6612`; fails without it: `an_order_past_the_largest_block_is_refused`,
  `a_large_alloc_past_the_largest_block_is_refused`,
  `copying_an_image_past_the_largest_block_is_refused`,
  `a_file_blocks_header_claiming_more_runs_than_its_body_holds_is_refused` and
  `a_block_map_the_page_cache_cannot_use_is_refused`; the channel depth falls to 512, the deepest
  whose ring fits.
- [x] **kmem-25** `kernel/src/syscall/table.rs:855` — `sys_memory_map` requires only the MAP_* bits
  named in `rights`, yet always installs a user-readable mapping (prot starts at
  `Protection::USER`, and x86 has no read bit). A request with `rights = 0`, or MAP_WRITE alone,
  maps a MemoryObject or FileObject readable without MAP_READ, which elsewhere is the authority to
  read contents (spawn image, io Write buffer). A process attenuates a MemoryObject handle to
  MAP_WRITE | TRANSFER (or TRANSFER | INSPECT) with `sys_handle_restrict` and sends it to a server.
  MAP_WRITE alone is meaningful: `sys_io_submit` Read requires exactly that on its destination
  buffer. The server calls `sys_memory_map(h, 0, size, 0)`; `required` is empty, `lookup`'s subset
  check passes (`handle/table.rs` step 11), and it gets a readable mapping of memory it was denied
  MAP_READ on. Unverified: not run end to end, and no in-tree issuer hands out such a handle
  today. By the code, `req & MAP_*` is empty when `rights == 0`, `HandleTable::lookup` accepts the
  empty set, and `page_flags_for` sets USER on a present leaf. `syscall-abi.md` says "the handle
  must carry every requested MAP_* bit", and nothing makes reading require MAP_READ. A host test
  that restricts a handle to MAP_WRITE and maps with `rights = 0` would settle it. Fix: always
  require MAP_READ for any mapping. Architecture. Medium. **Fix**, as kobj-8.
  Fixed in `48cecf0b`, as kobj-8; fails without it: boot-probe's `map_needs_read_test`.
- [ ] **kmem-26** `kernel/src/mm/kvmap.rs:55` — kernel-stack virtual addresses are never reclaimed:
  `vmap` is a bump allocator and `KernelStack::Drop` unmaps only the leaves. Thread churn
  therefore (a) retains a page-table frame for every 2 MiB of vmap (about 102 stacks) forever, and
  (b) after about 26.8M stacks crosses the one PML4 slot whose PDPT is shared via the kernel
  template, so new stacks are mapped only in the creating address space. Any process loops on
  `sys_thread_create` plus thread exit (ambient, no syscap). Each cycle permanently uses 20 KiB of
  vmap and about 40 bytes of physical page-table memory; on a 256 MiB machine a few million cycles
  exhaust RAM. On a machine with more than about 1 GiB, after about 26.8M cycles a new user
  thread's `KernelStack` lands at 0xffffc08000000000 under a PML4 entry that exists only in that
  process's root. The first switch away loads another CR3 while still on that stack and faults,
  and `sys_process_spawn`'s `fabricate_frame` writes the child's stack from the parent's CR3.
  Either way the kernel crashes. `memory-management.md` lists "no freelist, negligible churn" and
  "No production consumer today"; both premises are stale now that `sys_thread_create` exists, and
  the page-table frame leak is not mentioned. Demonstrated: a host probe in the `kstack` tests
  built a boot root with `kvmap::init`'s PDPT and snapshotted it as the template. An early stack
  was visible in a second AddressSpace (true). After bumping `VMAP_NEXT` to the 512 GiB edge, a
  stack at 0xffffc08000000000 was mapped in its own AS (true) but not in a fresh AS (false); the
  count needed to get there was 26843545 stacks. Architecture. Medium. **Later**, as kmem-9.
- [x] **kmem-27** `kernel/src/mm/elf.rs:241` — PT_LOAD `p_memsz` has no cap: `map_load_segment`
  eagerly allocates and zeroes a frame for every page with `map_vma`, under the AS spinlock,
  inside `sys_process_spawn`, which runs with IF masked. This is the unpreemptable zeroing loop
  that `MemoryObject::MAX_SIZE` exists to prevent on `sys_memory_create`, and the ELF path
  bypasses that guard. Any process spawns an ELF whose PT_LOAD has `p_memsz` = 64 TiB; `map_vma`
  keeps calling `buddy_alloc` and zeroing until RAM is exhausted, then rolls back. For the whole
  loop (seconds on a large machine) that CPU takes no interrupts, so other CPUs' TLB shootdowns
  stall waiting for its ack, and every other allocation fails: concurrent page faults return Oom
  and kill their threads, and driver allocations fail. The rlimits deferral names `MAX_SIZE` as
  the stand-in for accounting, and this path has none. Demonstrated by code and by boot: there is
  no size check in `load_elf` or `map_load_segment`. A probe inflated a writable PT_LOAD's
  `p_memsz` to 1 TiB for init under `cargo xtask test-qemu --kvm`: "PROBE map_vma of 1024 GiB:
  ok=false cycles=89568028", then "init: ELF load failed: OutOfMemory". The loop ran under the AS
  lock in the spawn path (SFMASK clears IF on syscall entry). The ~90M cycles is on a small-RAM
  guest and grows with RAM. The TLB-ack stall and OOM faults in other processes were not measured.
  Architecture. Medium. **Fix**, as kmem-8.
  Fixed in `badc91cd`; fails without it:
  `an_image_whose_segments_claim_more_than_the_budget_is_refused`.
- [ ] **kmem-28** `kernel/src/arch/x86_64/idt.rs:250` — `pf_dispatch` checks the user-copy
  recovery table before demand paging, so a kernel copy that touches a valid but not-yet-faulted
  page (a lazy anonymous page or any FileBacked page) fails with `UserAccessError::Fault` instead
  of faulting it in. A process maps a file with `sys_memory_map` (FileBacked VMAs are always lazy)
  and passes a pointer into an untouched page as a syscall's source buffer, e.g. a message
  payload; `copy_from_user` faults, the recovery entry turns that into `Fault`, and the syscall
  fails on memory the process can read. In-tree code avoids this by touching mapped pages first,
  and stack probes keep the lazy stack touched, so the impact today is limited. The comment at
  `idt.rs:282` cites `deferred-decisions.md` for this, but there is no such entry (grepped).
  Unverified: by the code, `lookup_recovery(f.rip)` returns the recovery PC before the
  `is_user_fault` / `try_fault_in` branch, `try_fault_in` runs only for ring-3 faults, and
  `mm/user_access.rs` has no pre-fault; `user-memory-access.md` line 162 still describes the
  pre-scheduler state. To settle it, boot a test-image program that maps a file (or lazy anonymous
  memory) and passes an untouched page as the source buffer of `sys_debug_kprint`, and check for
  `Fault`. Architecture. Low. **Later**, in Part E.
- [ ] **kmem-29** `kernel/src/syscall/table.rs:878` — `sys_memory_map(hint = 0)` picks its range
  with `find_free_range` under one AS-lock hold and maps it under another, so two threads mapping
  concurrently can pick the same range. The loser gets `MapError::Overlap`, which reaches the
  caller as InvalidArgument. Two threads of one process each call
  `sys_memory_map(obj, 0, PAGE, MAP_READ)` at the same time, e.g. two allocators growing their
  arenas; both searches return the same window, and the second map fails with a valid request
  refused and an error code nobody would retry on. Demonstrated: a host probe had two
  `find_free_range(PAGE)` calls return the same range (true); `map_object` of the first returned
  `Ok(())`, the second `Err(Overlap)`. Fix: search and insert under one hold. Architecture. Low.
  **Later**, in Part E.
- [ ] **kmem-30** `kernel/src/syscall/table.rs:2188` — `sys_file_sync` is a per-kind verb: its name
  and `lookup_typed(handle, pid, Rights::MAP_WRITE, KObjectType::FileObject)` bind it to
  FileObject, beside `sys_ns_sync` and the per-kind memory calls, where the project prefers a few
  object-generic calls. Any other object that will need a durability point (a block device, a
  future writable MemoryObject backing) needs another syscall number instead of one sync over
  handles. A design note rather than a failure, worth listing in the syscall-surface work.
  Unverified: nothing to run; the maintainer's call on whether the syscall-surface work takes it
  up. Architecture. Low. **Later**, in Part E.

What held up: `user_access.rs` copy primitives check `addr+len` with `checked_add` against
`USER_VIRT_END` (equal to the canonical boundary), handle zero-sized `T`, bound `copy_cstr` by the
buffer, and the arch asm clears DF and closes the SMAP window on both the recovery and success
paths. The hinted path of `sys_memory_map` checks alignment, overflow, the `USER_VIRT_END` bound
and the stack guard gap, bounds size by the object, and cannot amplify MAP_WRITE or MAP_EXEC.
`sys_memory_unmap` frees anonymous frames only after `shootdown_all`, drops the VMA (and
last-ref object frames) after it, and reserves capacity outside the AS lock with a retry.
`map_file_page` re-checks under the AS lock that the VMA still covers the address and belongs to
the same object, and treats `AlreadyMapped` as success (the concurrent-fault panic in kmem-20 hits
only the anonymous path). `block_map` rejects bad block sizes and run ends that overflow;
`FileObject::settle` matches by PO, so a resize during a fill cannot mark a re-reserved page
Ready; retired frames are freed only at Drop; in Model B a failed send (Full or PeerClosed)
withdraws the pending fill first, so a late reply cannot land in a freed frame. The ELF loader
checks `p_vaddr`/`p_offset` congruence, `filesz <= memsz`, `v_end` and `aligned_end` with checked
arithmetic, the `file_end` bound and segment/stack overlap via the VMA tree; only `e_phoff` and
`e_entry` are unchecked (kmem-22, kmem-23). Borrowed (framebuffer) MemoryObjects never hand their
frames to the buddy; `AddressSpace::Drop` uses `get_mut`, so dropping a VMA does not nest
same-rank locks; the scheduler always loads `next_root`, so no CPU keeps a dead process's PML4
loaded. TLB shootdown: `wait_satisfied`'s host test passes, an initiator holds no other lock and
restores IF, and the #PF gate is an interrupt gate, so the race in kmem-20 needs two CPUs. The
arch boundary holds: nothing in `mm/`, `tlb.rs`, `memory_object.rs` or `file_object.rs` names
`arch::x86_64` or uses CR3/invlpg/PML4 jargon. Every probe was restored and `git status` was
clean at 510381f4.

### scheduler, architecture lens re-run — main at 510381f4, 2026-10-20

Lenses: architecture; returned nothing: none.
Found 7, refuted by a boot 0, reported 7. This adds to the earlier scheduler section, whose
architecture lens returned no findings.

- [x] **sched-11** `kernel/src/object/timer.rs:206` — Destroying a Timer never removes its entry
  from the global deadline heap, so the heap keeps a raw pointer to freed memory. A process arms a
  timer with `sys_timer_set`, then closes its only handle or exits. When the deadline passes, the
  tick's `fire_expired_deadlines` calls `set_in_heap`, `take_waiters` and `set_armed` on freed or
  reused slab memory, under SCHED. A periodic timer is never popped for good, so the stale entry
  fires every interval and permanently holds one of the 16 heap slots. Demonstrated: a host probe
  (since removed) called the real `timer_arm` on the global SCHED, then dropped the ObjectRef.
  `timer_destroys` was 1, yet the heap still held 1 entry naming the freed address. The next Timer
  allocated landed on the freed slot ("slot reused by the next Timer: true"). The stale fire then
  disarmed that unrelated live timer (deadline=0, in_heap=false) and left its own heap entry
  orphaned. Neither `Timer::drop` nor `dispatch_destroy`'s Timer arm touches the heap.
  architecture. high. **Fix**, as kobj-1.
  Fixed in `fcc06ce8`; fails without it:
  `a_timer_destroyed_while_armed_leaves_nothing_in_the_heap_naming_it`.
- [x] **sched-12** `kernel/src/sched/exit.rs:57` — `reap_matching` assumes threads in the ready
  queues are registered on no wait objects. A sibling woken from `sys_wait` but not yet run is
  still on its other wait objects' waiter lists and in the deadline heap. `exit_process` reaps and
  frees it, leaving those pointers dangling. Thread A of process P waits on handles X and Y with a
  finite deadline. X signals and A moves to ready, but A has not run its unregister code. Sibling
  B calls `sys_process_exit` first, which is likely on one CPU since syscall return does not
  reschedule. A is reaped and freed while Y's waiter list and the heap still name it. A later
  signal on Y, or the deadline firing, writes into freed memory and can wake whatever Thread now
  occupies that slot. Demonstrated: a host probe (since removed) used the real `signal_channel`,
  `reap_matching` and `fire_expired_deadlines` on a test SchedState. Output: "c2 waiters after
  reap: 1 (first == freed A: true); heap still names A: true", "next Thread took the freed slot:
  true", "B blocked=0 ready=1 (c3 never signalled)". An unrelated thread from another process,
  blocked with no deadline on an object that never signalled, was moved to ready by A's stale
  deadline. architecture. high. **Fix**, as sched-4.
  Fixed in `deabe050`; fails without it:
  `a_woken_thread_reaped_before_it_runs_leaves_nothing_naming_it`.
- [x] **sched-13** `kernel/src/sched/wait.rs:29` — `block_current_and_switch` pushes onto the
  global blocked list (64 entries for the whole system) with `push_reserved` and no capacity check,
  so the 65th blocked thread panics the kernel. `suspend_with_fault` has the same unchecked push
  onto the suspended list, which also holds 64. `BLOCKED_RESERVE`'s doc comment says blocking past
  the limit is "refused", but it is not. Threads blocked in `sys_wait` across all processes reach
  64. Long-lived servers already account for many of them, and `sys_thread_create` has no
  per-process limit. The next `sys_wait` that has to block panics with "blocked list: reserve
  exhausted". `wait_on` (wait.rs:102-167) checks only the per-object and heap reserves. Faulted
  threads stay suspended for the whole boot (TODO(unsupervised-fault)), so the 65th one panics
  too. That consequence is not in the recorded deferral, and the deferred-decisions entry covers
  REAP_RESERVE only. Demonstrated: a host probe printed blocked capacity 64, and `push_reserved` at
  capacity panicked with "blocked list: reserve exhausted — this list is sized so that cannot
  happen". architecture. high. **Fix**, as sched-3.
  Fixed in `deabe050`; fails without it:
  `every_thread_the_cap_admits_can_block_wake_and_be_reaped_on_one_cpu` and
  `a_thread_counts_from_its_first_placement_until_its_reap_is_drained`; a 512-thread cap every
  scheduler list is reserved to hold.
- [x] **sched-14** `kernel/src/sched/timers.rs:118` — A one-shot timer fired by the tick with no
  waiters is disarmed (deadline 0), so `Timer::already_signaled` becomes false. A `sys_wait` that
  arrives after that tick finds nothing signalled and no heap entry, and blocks with nothing to
  wake it. Whether a wait on an expired timer returns depends on whether some CPU's tick ran
  first. A program arms a one-shot timer, then is delayed (for example preempted for a quantum)
  before its `sys_wait` registers. Any CPU's tick at or after the deadline fires and disarms the
  timer, since the heap is global. The later wait blocks. storage-service's `park()`
  (`sys_timer_set` of now+5ms, then `sys_wait` with `u64::MAX`, userspace/storage-service/src/main.rs:140-150)
  would hang its unmount/held-files path for good. coreutils `sleep`
  (userspace/coreutils/src/bin/sleep.rs:94-118) would hang until Ctrl-C. session-mgr's `sleep_ms`
  only loses its 1 s backstop. Demonstrated: a host probe armed a timer at 5000. `already_signaled(6000)`
  was true before `fire_expired_deadlines(6000)` and false after, with the heap empty. architecture.
  high. **Fix**, as sched-7.
  Fixed in `1b6ee7ea`, and in `76e0dddd` for a waiter whose clock was read before the fire, which
  PR #391's review found; fails without them:
  `a_one_shot_timer_fired_with_no_waiter_still_reads_signaled`.
- [ ] **sched-15** `kernel/src/sched/exit.rs:111` — When `exit_process` reaps a sibling blocked in
  `sys_wait`, the thread's kernel stack is freed without `sys_wait`'s `refs` array being dropped,
  so every object it waited on leaks one reference. An IPC endpoint pinned this way never runs
  `IpcChannel::drop`, so its peer never gets PeerClosed. Thread A of process P waits on endpoint E
  and thread B calls `sys_process_exit`. It closes P's handle table, and `reap_blocked_matching`
  unregisters and reaps A. A's stack-held ObjectRef on E is never released, so E's refcount stays
  at 1 or more, and the server on E's peer keeps that client's session state. The SAFETY comment at
  exit.rs:111-112 says the wait objects are kept alive by the thread's `sys_wait` refs, "released
  only when it unblocks". A reaped thread never unblocks: the refs live in `sys_wait`'s frame
  (syscall/table.rs:1752-1767), and `KernelStack`'s Drop frees the frames without unwinding.
  Demonstrated: a boot probe on 510381f4 patched the `sleep` coreutil so its main thread started a
  sibling blocked in `sys_wait` on the stdout pipe endpoint, then called process exit. The harness
  read the pipe's other end, expecting PeerClosed. The sibling was confirmed blocked (it printed
  "waiter running" and never returned from the wait). `test-qemu` then hung after "probe5: exiting
  with sibling blocked": PeerClosed never arrived and the demo chain did not finish. The control
  was the same probe with the sibling faulting on a bad stack, so it never blocked in `sys_wait`.
  There the pipe closed and the child's exit was reaped in 313 ms, matching the 300 ms sleep.
  Probes reverted; git status clean. architecture. medium. **Fix**, as sched-5.
- [ ] **sched-16** `kernel/src/sched/deadline.rs:31` — The deadline heap is a single global
  16-entry reserve with no per-process accounting, and closed armed timers (sched-11) hold their
  slots for good. An unprivileged process, needing no syscap for `sys_timer_create` or
  `sys_timer_set`, creates 16 timers and arms them far in the future. From then on every
  finite-deadline `sys_wait` in the system returns OutOfMemory, every `sys_timer_set` fails, and
  BlockBounded sends silently lose their timeout. service-mgr's main loop (main.rs:1140-1150)
  ignores `sys_wait`'s result and busy-spins until its pending deadline passes by the clock.
  session-mgr's `sleep_ms` (its re-prompt pause after a denied login) returns immediately.
  Unverified: `wait_on` (wait.rs:143-163) returns OutOfMemory when `deadline::push` fails,
  `timer_arm` returns Err the same way, and `ipc_send_push_blocking` (ipc.rs:376-387) discards the
  push failure. The existing test `heap_push_refuses_over_reserve` shows the refusal. Each
  consumer's handling was read, not run. A test that arms 16 timers from one process, then calls
  `sys_wait` with a deadline from another, would settle it. architecture. medium.
  **Fix**, as sched-6.
- [ ] **sched-17** `kernel/src/sched/exit.rs:181` — `exit_thread`'s last-thread branch marks the
  process ended but never queues its pid in `ended_pids` or wakes the reaper, though the comment
  says this is "deferred to the reaper". The handle table is closed only when this CPU next runs
  `reap_pending`. A process's last thread leaves through `sys_thread_exit`, or a supervisor
  terminates a faulted thread via `sys_exception_resume`. If that CPU stays busy, with always
  another ready thread and no idle, yield or exit there, the process's handles stay open. Peers
  waiting for PeerClosed wait as long as the load lasts. This is the failure the reaper thread was
  added to fix for `exit_process`, which pushes `ended_pids` and calls `wake_reaper`
  (exit.rs:237-240) where `exit_thread` (exit.rs:181-187) does neither. `reap_pending` drains only
  this CPU's reap list (exit.rs:415-427). Demonstrated: a boot probe on 510381f4 patched `sleep` to
  call `sys_thread_exit` as the process's only thread, with the harness reading the pipe's other
  end for PeerClosed. Without spinners, PeerClosed arrived in 15 ms. With one user-mode busy-loop
  thread pinned to each CPU (affinity bits 0 to 7, 8 `thread_create` calls succeeded), it arrived
  in 2993 ms, exactly when the spinners' 3 s limit ended and they exited, which runs
  `reap_pending`. The reaper thread was never woken. It is bounded by CPU load, and the path is
  rare, since most exits go through `exit_process`. Probes reverted; git status clean.
  architecture. low. **Fix**, as sched-8.

What held up: `sys_wait` registers and blocks under one SCHED hold, and `wait_try_wake`'s CAS plus
`make_runnable`'s blocked-list lookup absorb a second waker. `reap_blocked_matching` unregisters
blocked siblings from their wait objects and the deadline heap before reaping them.
BlockBounded PendingSend deadlines are removed both when the send is promoted and in
`ipc_endpoint_closing`, so a fired entry never names a freed endpoint. Placement falls back to a
CPU that accepts work when a mask names none, and tests pin the parked-CPU asymmetry (`cpu_online`
vs `online_mask`). The on_cpu guard holds: `switch_into` raises it under SCHED before release and
spins on the incoming thread's guard, and `stealable_to` and `reap_matching` respect it.
`push_reserved` never grows a list under SCHED, and the blocked list's capacity is exactly 64.
dpc.rs keeps one queued instance per DPC (dedup via `queued`), its drain buffer matches the
reserve, and its queue lock is a leaf that never nests with SCHED. In timer.rs, `elapsed_ns`
saturates on a lagging AP's TSC, `ns_to_ticks` clamps, and the u128 math cannot overflow. A
periodic re-arm uses `now.saturating_add(interval)` and is strictly greater than now, so a 1 ns
interval cannot loop the drain, and the re-push always has room because its own entry was just
popped. `sys_thread_create` and `sys_thread_set_affinity` mask bits above MAX_CPUS and reject an
empty mask, RealTime is gated on the REAL_TIME syscap, and the ThreadArgs layout is asserted on
both sides. `close_all_owned_by` drops refs outside the handle-table lock, and IpcChannel's Drop
takes SCHED only from that lock-free context. Not reported, already recorded: the running-sibling
SMP gap in `exit_process` (smp.md § Deferred), REAP_RESERVE (deferred-decisions),
TODO(unsupervised-fault) and TODO(smp-cpu-count).

### PR #365's trial reviews, checked against main — main at 510381f4, 2026-10-20

The two trial reviews of PR #365 (head `55e6bd18`; T1 posted 2026-10-14, T2 2026-10-15) were read
against `main`, not run. What `main` had already fixed, by the original review's fixes (decision
log, 2026-10-07), is left out: T1-1, 2, 4, 10, 12 and 15, and T2-1, 2, 3 and 12. Six more are the
bash's own and go with them: the run count reserved unchecked (T1-3, T2-4) is kobj-3, the
device's rights dropped (T1-29) kobj-2, a rename leaving two names (T1-5, T2-8) fs-3, `allocate`
keeping what it took (T1-9, T2-7) fs-12, a create's real error dropped (T2-10) fs-17, and FAT's
refusals untested (T1-11) fs-14. The rest follow, each still there by reading, and reproduced
before it is fixed.

- [x] **pr365-1** `userspace/fs-server-fat/src/volume.rs:833` — a directory moved to another parent
  has its old name deleted before its `..` is checked or rewritten (`:835`). A move whose `..` is
  malformed, or whose rewrite fails, is answered Corrupt or Io after it is made, and the stale `..`
  then misleads `within()`. T1-6, T2-14. Error paths. Medium. **Fix.**
  Fixed in `eaaf7066`: a moved directory's `..` is checked before anything is written and rewritten
  before the old name goes, and a failure undoes it; fails without it:
  `a_directory_moved_and_stopped_at_any_write_is_under_a_name` and
  `a_crash_anywhere_in_a_directory_move_leaves_its_dotdot_naming_a_parent_holding_it`.
- [x] **pr365-2** `userspace/fs-server-fat/src/bpb.rs:198` — `parse` has no upper bound on the
  cluster count: a FAT16 of 65,525 clusters or more, or a FAT32 over 0x0FFFFFF5, is served, and its
  high clusters collide with the end-of-chain and bad-cluster markers. T1-7, T2-5. Logic. Medium.
  **Fix.**
  Fixed in `c3838cc7`: a FAT16 of 65,525 clusters or more, or a FAT32 over `0x0FFF_FFF5`, is
  refused, and `mkfs` takes its ranges from the same constants; fails without it:
  `a_cluster_count_its_fat_cannot_number_is_refused`.
- [x] **pr365-3** `userspace/fs-server-fat/src/volume.rs:440` — `mark_clean` clears the dirty bit
  after a change that failed partway during this mount, since only a volume found dirty at mount
  stays dirty, so shared or lost clusters are recorded as a clean unmount. T1-8, T2-9. Error paths.
  Medium. **Fix.**
  Fixed in `dd60c45f`: a change that fails on the device, or finds the volume corrupt, leaves it
  marked at its unmount; fails without it:
  `a_change_that_fails_on_the_device_leaves_the_volume_marked`.
- [x] **pr365-4** `userspace/fs-server-fat/src/volume.rs:582` — a grow whose final zero write or FAT
  flush fails leaves the clusters it took chained and lost (`z.finish()?` after `allocate`), and
  `place()` (`:499`) has the same shape. A leak only. T1-21, T2-13. Error paths. Low. **Fix.**
  Fixed in `fd4e7f60`: a grow, and a directory grown, give back what they took when a later step
  fails, the link undone and written before the frees; fails without it:
  `a_grow_stopped_at_any_write_loses_no_cluster`,
  `a_directory_grown_and_stopped_at_any_write_loses_no_cluster` and
  `a_grow_refused_a_write_then_crashed_leaves_no_chain_into_a_free_cluster`. The class swept in
  `eecfa731`.
- [x] **pr365-5** `userspace/fs-server-fat/src/volume.rs:415` — `mark_mounted` writes the dirty bit
  before FSInfo. An FSInfo write that fails refuses the mount but leaves the bit set, so the stick
  never reads clean again. T1-22. Error paths. Low. **Fix.**
  Fixed in `dd60c45f`: FSInfo is written before the state bit; fails without it:
  `a_mount_refused_for_a_failed_write_leaves_it_as_clean_as_found`.
- [x] **pr365-6** `userspace/fs-server-fat/src/table.rs:175` — a directory walk is bounded by the
  volume's cluster count, not by `MAX_SLOTS` (65,536 entries), so a directory whose chain loops
  costs O(volume) reads before it is refused. T2-6. Logic. Low. **Fix.**
  Fixed in `07fbbf55`: a directory's chain is walked to its `MAX_SLOTS` reach, and `within` climbs
  at most a path's 512 components; fails without it:
  `a_loop_in_a_directory_is_refused_within_a_directorys_reach`.
- [ ] **pr365-7** `userspace/fs-server-fat/src/volume/write_tests.rs:411` — test gap: `writes <= 24`
  is loose against the 19 writes a 1 MiB grow records, so the FAT flush could lose its batching
  unseen. T1-25. Tests. Low. **Later.**
- [ ] **pr365-8** `userspace/libfsserver/src/server.rs:419` — test gap: the truncate-to-zero arm,
  `forget_then_release`, is tested at no level; a gate step emptying a file on the FAT stick would
  catch a leak. T1-14. Tests. Low. **Later.**
- [x] **pr365-9** `docs/spec/rsproto-namespace-ops.md:160` — "a page must lie within one run … that
  one the kernel cannot check, and each server guarantees it" is wrong both ways: `block_map` could
  check a page's alignment against the runs, and ext4 with blocks under a page does not guarantee
  it (`TODO(ext4-subpage-runs)`). T1-17, T2-11. Docs. Low. **Fix.**
  Fixed in `b87075a6`, in both docs that said it.
- [x] **pr365-10** `docs/spec/rsproto-block-ops.md:49` — `BlockRun` says a block is the filesystem
  block size the kernel learns at mount, but runs are in the reply's `block_size` (FAT's are
  512-byte sectors). T2-15. Docs. Low. **Fix.**
  Fixed in `b87075a6`.
- [ ] **pr365-11** — the trials' other test gaps, each a mutation that passed the suite at
  `55e6bd18`, not yet run against `main`: no device write made to fail (T1-13); the volume-label
  filter (T1-23); `fat_label`'s fallbacks (T1-24); the 64-run limit at its neighbour (T1-26); the
  chunker's last chunk (T1-27); and eight of FAT's guards (T2-17 to T2-24: the second FAT copy on
  eviction, the long-name ordinal bound and checksum, the numeric-tail window, the rewrite's slot
  bound, the FSInfo hint, FAT32's reserved nibble, DEL in a name). Tests. Low.
  **Later**: each run against `main` as a mutation in Part H first.
- [x] **pr365-12** — the trials' other doc claims, not yet read against `main`: `Meta::Unmount`'s
  "recorded clean" where FAT keeps a volume found dirty (T1-16); `error-codes.md` pointing at the
  deleted `fs_kerror` (T1-18); `File::Forget`'s list missing a truncate to zero (T1-19);
  `fat-fs-server.md` §9 citing deferrals that do not exist (T1-20); the refusal-in-place-of-Ready
  text naming only ext4 (T1-28); and no marker where ext4 accepts a block under a page, with its
  `CLAUDE.md` silent on it (T2-16). Docs. Low.
  **Fix**, with pr365-9 and pr365-10, each read against `main` first.
  Fixed in `b87075a6`, each read against `main` first and each still wrong; the refusal text was
  wider than found, the storage service sending and receiving them too. T2-16's marker was already
  in `map_file`; ext4's `CLAUDE.md` now says it.

### Found on the laptop — main at c5e59d4d, 2026-10-25

Found by the maintainer at the laptop's prompt after reinstalling, not by a run.

- [ ] **laptop-3** `userspace/nxsh/src/paths.rs:131` — `open` and `save` take no relative path
  with a `.` or a `/` in it, so neither takes a plain file name: `open hello.txt` fails, as does
  §4's own example, `open a.txt b.txt`.
  - Their arguments are lexed in expression mode, where `.` and `/` are operators: `hello.txt`
    arrives as a field access, and `Documents/hello.txt` as the name `Documents` and then the
    path `/hello.txt`.
  - `path_args` keeps words, names and strings, and drops anything else without a word
    (`:131-139`).
  - So `open hello.txt` and `save out.txt` fail "needs a path", and `open Documents/hello.txt`
    reads `Documents`, then `/hello.txt`. On the laptop, `Documents` a directory under `/home`,
    it failed "cannot size `/home/Documents`".
  - An absolute path, `~/…`, a quoted path and a bare name with neither all work. `cd` takes its
    argument in word mode, so `cd Documents/sub` works. The gates name absolute paths.
  - Demonstrated on the laptop at `c5e59d4d`, and by a temporary host test: the tokens
    `Ident("Documents")` and `Word("/hello.txt")`; `open hello.txt` and `save out.txt` refused for
    want of a path; `save Documents/out.txt` refused for the format of `/p/Documents`; and
    `open ./Documents/hello.txt` read the file. Logic. Medium. Untriaged.
