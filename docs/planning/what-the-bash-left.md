# What the bash left

**Status: detailed 2026-11-19; H.1, H.2 and H.3 built 2026-11-20; H.4 rescoped and built
2026-11-23, H.5 folded into it; H.6 to H.9 built 2026-11-23; H.10 built 2026-11-24.** Part H of
[`pre-phase-7.md`](pre-phase-7.md): the findings of the bug bash triaged **later**, which
[`bug-bash.md`](bug-bash.md) records, checked against `main` and fixed, each with a test that fails
without it.

## Why

The bash ran on `main` at `510381f4` on 2026-10-20. Part D fixed what crossed a boundary, and the
rest waited here so Part E was not held for it:
- the supervisors' and the parsers' defects;
- the test gaps no fix of Part D's wrote;
- two ext4 defects.

Phase 7 builds on all of it. A test gap is a guard nothing holds: the next change to its code can
drop it and every suite stays green.

## Part H in detail

*Detailed 2026-11-19, measured on `main` at #458's merge (`3c6919a4`), the maintainer's calls
folded in.* Where the outline in `pre-phase-7.md` and this differ, this is the plan. **Ten
sub-parts, a PR each.**

### What the pass found

Fifty-one findings were open. Fifty were marked later, less the eleven Part I fixed (ten moved
there, and kmem-26 with kmem-9) and the four Part E fixed. The fifty-first is sched-10, marked fix
in Part D, which left it "open, for Part H" for the one mutation it could not catch.
Parts D, E, I and J changed much of the code they name, so each was checked against today's `main`
before it was planned. Seven agents each took a group, in worktrees of their own, without a boot.
- **A test gap:** each mutation the finding names was applied to today's code and run against the
  crate's whole suite.
- **A defect:** reproduced with a temporary host test where the code is host-reachable, and
  otherwise read, and marked so.

What they found is below.

1. **Four are gone**, fixed by later work that did not tick them. They are ticked now, with the
   commit that closed each.
   - **sup-7:** `init` and storage-service read `init.toml` by one rule, `manifest::text`
     (`e6a4ecf2`, the survey's sup-d8).
   - **kmem-10:** both teardown paths fail a pending fill (`7938344d`, kobj-d3). Its tests pass,
     and fail with the fill's signal removed.
   - **proto-5:** librsproto's parsers read through a bounded `Reader`, fuzzed by prefix and
     corruption (`662163e6`). Every mutation the finding names, rewritten for today's code, is
     killed.
   - **kmem-19:** the push after the reservation is `push_within_capacity(..) else unreachable!`
     (`ac2ed261`), so removing the reservation now fails 6 tests. The refused reservation stays
     untested, as nothing on the host can make a vector's reservation fail.
2. **Six are partly closed**, and what is left is planned below.
   - **sched-10:** removing `wait_on`'s poll path, the mutation Part D could not catch, now aborts
     the kernel's suite, by accident: J.15's `armed` test polls, and the poll falls through to a
     deadline with no slot promised. Nothing tests the poll itself.
   - **kmem-14:** zeroing is in `Frame::alloc_zeroed` (Part J). An object made from `alloc` is
     caught only by test order.
   - **fs-13:** its adjacent defect, an over-long suffix opening a directory, is fixed
     (`4eb4e046`). Four bounds are still untested.
   - **sec-15:** the IPC scrubs are held (`640a682b`). The four in `libprompt`'s `lib.rs` are not.
   - **proto-12:** the reader's guards are held. Two `u16` caps no caller can reach are untested.
   - **pr365-11:** two of its thirteen are killed by tests written since (T1-27, T2-17), and T1-13
     by three of its four mutations.
3. **Fifteen defects remain**, each reproduced unless it says "by reading".
   - **ext4: fs-9 and fs-11.**
     - **fs-9:** a lost directory-size write wedges the directory, and so does a crash at the
       same write, found by losing each write in turn (`Failing::after`).
     - **fs-11:** a crafted stick's extent over the inode table is served by `map_file` before any
       unlink. The kernel's write-back of that file would write into the inode table, and an
       unlink, a truncate or a growth's cut frees the block to the next allocation, which zeroed
       the root inode in the reproduction. A bitmap bit cleared over the inode table has
       `alloc_run` hand the block out the same way.
   - **service-mgr: sup-3, sup-4, sup-8, sup-10 and sup-11.**
     - **sup-3:** at the defaults, the backoff is 0 for attempts 55 to 63.
     - **sup-4:** a route is drained with no bound, by reading.
     - **sup-8:** a duplicate critical declaration sends the boot to `eshell`.
     - **sup-10:** the exit-code grace ends on a route's wake, with 999 ms left.
     - **sup-11:** route copies are sent only at the login chain's start, by reading.
   - **storage-service and the view broker: sup-2, sup-5, sup-6, sec-4 and sec-10.**
     - **sup-2:** libgpt and the kernel read a GPT differently in eight cases.
     - **sup-5:** a re-probe on every `info/` resolve, with no deadline, by reading.
     - **sup-6 and sec-4:** one client can hold every slot.
     - **sec-10:** a grant inside a view is refused as "not there to grant".
   - **auth-service: sec-13.** A failed truncate of `users.new` leaves a stale tail, so a removed
     account comes back at the next boot. Shown over `libos`'s mock with
     `ignore_next_truncate`.
   - **nxsh: nxsh-1 and nxsh-2.**
4. **Twenty-six test gaps remain, as findings.** Their mutations still survive:
   - kobj-12, kobj-13, kmem-17, proto-16;
   - drv-9 to drv-15;
   - proto-6, proto-11, proto-17;
   - sec-7, sec-8, sec-9, sec-14;
   - sup-12 to sup-16;
   - fs-16, pr365-7, pr365-8.

   **Every one of them, and every partial one above, can be held by host tests; none needs a
   boot.** sched-10's poll path, which Part D found no host test could reach, is reached now.
5. **Four findings reach further than the bash recorded.**
   - **sup-2 can match the wrong partition, not only none.**
     - An entry off the disk is skipped by the kernel and kept by libgpt, which shifts libgpt's
       index by one.
     - A root given by `gpt-partuuid:` then matches its neighbour of the same label and size,
       with `init_known` true.
     - The real root looks free to automount, and mounting it a second time puts two writers on
       one ext4. No shipped manifest uses `gpt-partuuid:`.
   - **sup-8's sibling is an appended block that boots.** A name skipped first, for a bad
     endpoint, does not stop a later block of the same name, once another declaration lies between
     them: the duplicate check reads only the names taken. `[service.x]` with a bad endpoint, then
     `[service.y]`, then `[service.x] executable = "/evil"`, takes `/evil`. When the first `x` is
     critical, its skip sends the boot to `eshell`; when it is not, `/evil` boots today. A fix that
     stopped counting the skip of a name later taken would let it boot either way. Two adjacent
     headers are one declaration, whose first `executable` wins, and are not this.
   - **nxsh-1 has four more stale readers:**
     - a bare `cd` goes to the old `HOME`;
     - `~` expands to the old `HOME`;
     - `$env.PWD = …` is ignored by the next stage;
     - `$env = {}` is accepted, and the next stage still gets the whole record.
   - **The slot class is wider than sec-4 and sup-6.** Every server that any session can reach
     shares one pool, with no per-session bound and no idle reclaim:
     - the root filesystem, 30 sessions, as `init` keeps its control channel (`libfsserver`);
     - tty-server, 14 terminals;
     - the clipboard, 30;
     - the profile server, 31;
     - each FAT stick, 30;
     - device-mgr's `/dev/devices`, 25.

     A full root filesystem stops every listing, `cd` and `mkdir` on the machine. The profile
     server then caches an empty index, so `/applications` and `/system/manual` stay empty until
     it restarts. **Only the view broker can tell sessions apart**, by its `/s/<id>` base: a
     forwarded resolve reaches a server zeroed, with no sender.

     **Deferred 2026-11-23**, with sec-4 and sup-6 (the maintainer's call below), all but the
     profile server's cached index, which H.4 fixes.
6. **What the bash found beside its findings**, each fixed in the sub-part it rides with:
   - **The spawn path's syscap intersection is untested.** With `pp.syscaps() & requested` made
     `requested` (`kernel/src/syscall/table/spawn.rs`), every kernel test passes, and a child could
     then be given syscaps its parent lacks. The memory-object reply's rights intersection has the
     same gap.
   - **A kernel test flake.** PID 4491 is used by two kernel tests, and each ends by closing every
     handle that PID owns. Run concurrently, one closes the other's handles: one agent saw it once
     in about 37 full runs.
   - **Drivers:**
     - `xhci/desc.rs` has seven arm-length guards untested, each a panic on a short descriptor.
     - `hid/descriptor.rs`'s `id_slot` bound panics if loosened.
     - `pci`'s 32-bit BAR masks are untested; `FakeCfg`'s sizing values leave out the type bits
       hardware returns.
   - **ext4:**
     - `on_device` admits block 1 on a 1 KiB-block filesystem, the primary superblock.
     - `mark_clean` writes `STATE_CLEAN` at unmount whatever the mount found and whatever
       failed: a volume found unclean, or one whose create was refused, is recorded clean. FAT
       keeps both marked.
     - `create_file` leaks the inode when the inode's own write is refused.
     - `grow_file_from` leaves `i_blocks` short when a file whose size write was lost is grown
       again.
   - **Equivalent guards:** two guards no test can kill, as the code after each refuses the same
     inputs: FAT's numeric-tail leading-zero check, and the FSInfo hint's validity check at load.
   - **Stale words:**
     - `deferred-decisions.md` says a lost ext4 write costs "nothing but free space", which is
       wrong for a directory.
     - `init/CLAUDE.md` says `init`'s tests do not use `libos`'s mock; `unmount.rs`'s do.
     - boot-probe's `main.rs` says it has no syscaps; the image grants it `BIND_NAMESPACE`.
7. **Several findings misstate their consequence, though not the gap itself.**
   - **sec-9:** the mutated SHA-256 and HMAC panic. They do not hash wrong: auth-service would
     crash on a password of 65 or 119 bytes.
   - **proto-16:** the real cost is the error body's minimum. Without it, a server's empty error
     reply panics the kernel.
   - **proto-11:** the `read_row_values` cap guards nothing hostile.
   - **drv-10:** reaching outside a partition now takes three regressions, since `submit_within`
     checks the node's span first (`bc9731b9`).
   - **drv-15:** a `logical_min` 1 test exists; its first usage puts the two guards over each
     other.
   - **kmem-17:** one of its three mutations is equivalent.
   - **sup-16:** the finding's first sentence describes the mutant, not the code.

   Each test is written to the true consequence.
8. **Most fixes land against a size entry.** These files are at their entries in
   `size-baseline.toml`, and nothing may grow them:
   - `service-mgr/src/main.rs` (1,855);
   - `storage-service/src/main.rs` (1,161);
   - `view-broker`'s `main.rs` (1,263) and `lib.rs` (1,645);
   - `nxsh/src/eval.rs` (4,140);
   - `fs-server-ext4/src/ext4.rs` (1,846);
   - the kernel's `pci/mod.rs` (1,032), `syscall/table.rs` (1,544) and
     `object/file_object.rs` (1,093).

   `storage-service/src/lib.rs` is at 1,000 with no entry. Three kernel files need their tests
   moved out before tests can land: `pci/mod.rs`, `drivers/hid/descriptor.rs` (889 lines) and
   `drivers/xhci/desc.rs` (807).

   Most fixes therefore start by moving code into a library module or a test file of its own.
   That is also what lets a host test reach it: much of what the bash called "no host test can
   reach" is in a binary.

### The maintainer's calls (2026-11-19)

- **Every shared server, by one mechanism.** *Reversed 2026-11-23, below.* Each server any
  session reaches learns the session from its binding, as `/dev/views` already does. It then
  keeps a per-session share of its slots and reclaims an idle one when full. That covers the view
  broker, storage-service, device-mgr, the root filesystem and every FAT stick through
  `libfsserver`, tty-server, the clipboard and the profile server, over two sub-parts.
- **The kernel's per-process quotas stay deferred.** The 512-thread cap and the deadline heap's
  slots are system-wide, and one process can take them all. The 2026-10-22 entry called this Part
  H's quota question. It is not a bash finding, and it needs per-process accounting in the kernel.
  `TODO(deadline-slot-quota)` keeps its trigger, and its entry says Part H left it.
- **The kernel records each partition's GUID** (sup-2). The device record carries the partition's
  unique GUID, and storage-service matches `gpt-partuuid:` on it and stops reading the table a
  second time. This is an ABI change.
- **`$env` writes through** (nxsh-1). An assignment to `$env`, or to one of its fields, changes
  the record the shell itself reads:
  - what each program gets;
  - what `cd` and `~` use;
  - a new field adds a variable;
  - `$env.PWD` stays `cd`'s to change.

  The spec, which says `$env` cannot be assigned, changes with it.

### The maintainer's call (2026-11-23)

- **The shared servers' slots stay the machine's.** H.4's detail found what a share a session
  would take:
  - an id in every session's bindings;
  - a pool in seven servers;
  - an eviction notice that librsproto's `Dir` resumes from;
  - for the filesystems, a session marker no path can begin with. Only a NUL byte, or a session
    endpoint of each server's own, could give one.

  The harm is a program the user ran holding every slot of a server until it exits, and the
  kernel's per-process quotas, which one process can exhaust too, stay deferred. So sec-4, sup-6
  and the slot class are deferred to `TODO(server-fanout)`. A server's limit is the `sys_wait`
  width, and the readiness mechanism that entry describes would make it the machine's memory
  instead. Its trigger stands, and the entry now carries the bash's evidence.
- **What is wrong at any load stays, in H.4:** the profile server's index kept with a package
  missing, and a media session held past its answer.

### The order

1. **The defects first, worst first.** ext4's two (H.1) are a crafted stick destroying a
   filesystem. Then service-mgr (H.2), storage-service and the broker (H.3), the profile server's
   index and an eject's session (H.4), and the shell (H.6).
2. **Then the test gaps, by subsystem:** the kernel's guards (H.7), the drivers' (H.8), the
   parsers' and the credential path's (H.9), and the filesystems' (H.10).

Each sub-part needs only those before it that touch the same files: H.4 follows H.3 into
storage-service's `main.rs`.

### How each is held

- **Review and tests:** every sub-part gets a `/pr-review` and the host tests, and runs the KVM
  gates its files reach.
- **A defect's fix** comes with a test that fails without it, which is its control.
- **A test gap's test** must kill each mutation its finding names, on today's code, and pass
  unmutated. That run is the control, and the PR says what each one killed.
- **Each finding's box in `bug-bash.md`** is ticked with the commit that fixed it and the test that
  fails without it.
- **The test gaps' tests are bounded and specified**, so they go to the implementer, the agents'
  probe tests as their spec. The author runs every control, and keeps the defects, the kernel and
  the ABI.
- **Miri, once, at Part H's end**, over the job's targets Part H changes.

Part H is done when H.10 has merged and what Miri reports is triaged.

### H.1 ext4: a directory that cannot grow, an extent over metadata, and a volume recorded clean

**fs-9.** `dir_insert` grows a directory by a block:
1. `append_block` maps block N past the end;
2. the block is written;
3. the directory inode's new size is written last.

If that size write is lost, the extent tree's leaf already maps block N past the size, which is
unchanged. Every later growth appends at N again, and `extent::add` refuses it as `Corrupt`. Every
create, mkdir and rename into the directory then fails until an offline `e2fsck`. A crash before
step 3 leaves the same state. Linux refuses the same remap (`ext4_append`), runs the growth under
its journal, and on a failed update remounts read-only rather than healing.

- **The fix cuts the tree at the size before it grows**, as `grow_file_from` already does for a
  file: `extent::truncate(rw, sb, dir_inode, nblocks, ..)` before `append_block`. That covers a
  lost write and a crash alike, since a growth always starts by cutting what lies past the end.
- **A cut that freed anything recounts `i_blocks` from the tree.** The cut subtracts what it frees,
  but on disk nothing says whether blocks past the size were ever counted:
  - Linux's `fallocate` with `KEEP_SIZE` counts them;
  - a lost inode write after `extent::append` wrote a leaf does not.

  So neither subtracting nor not subtracting is right, and the cut alone leaves `i_blocks` two
  sectors short after a crash between the leaf write and the size write. The recount walks the
  tree:
  - data extents, written and unwritten;
  - the tree's own nodes;
  - the xattr block, if `i_file_acl` is set.

  It takes a block of stack a level, as `free_tree` does, and a growth that cut nothing pays
  nothing. **`grow_file_from` has the same flaw today**: a file grown again after its size write
  was lost is left `i_blocks` short. It takes the recount too.
- **A refused size write is undone as the block write's is.** The size goes back, and the tree is
  cut.
- **A create whose inode write is refused frees the inode.** `create_file` returns without
  `free_inode` today, the same class as the growth.
- **`dir_insert` moves out of `ext4.rs` first**, into `ext4/`, as `ext4.rs` is at its entry.
- **Test, in `volume/crash_tests.rs`:**
  - **The fixture:** a full directory with a depth-1 tree on 1 KiB blocks, files with 250-byte
    names, three to a block, each with a data block between the directory's, so its next create
    grows it.
  - **The stops:** each of the growing create's eleven writes is lost in turn, once and from then
    on. After each, on its own copy of the image, a create and a mkdir, both with 250-byte names,
    must succeed, as a short name fits a full block's slack and never grows it.
  - **e2fsck may never show:**
    - `end of extent exceeds`;
    - `i_blocks is`;
    - `i_size is`;
    - a bitmap difference with `+`;
    - the crate's `NEVER` set;
    - an unattached inode, when the write was lost once.
  - **e2fsck may show**, as `TODO(ext4-failed-write-undo)`'s recorded cost:
    - a bitmap difference with `-`;
    - a free count one above what is counted;
    - from then on, an unattached zero-length inode, since a crash between an inode's write and its
      link cannot be undone without a journal.
  - **Today it fails at once(9), after(8) and after(9)**, the later operations refused. With the
    cut and no recount, `i_blocks` shows at all three.
  - **`grow_file_from`'s recount** gets a test of its own: a depth-1 file whose final inode write
    was lost, grown again, and checked by e2fsck.
- **`deferred-decisions.md`'s `ext4-failed-write-undo` entry** is corrected: a lost write cost a
  directory more than free space until this.

**fs-11.** `on_device` bounds an extent only to the device, and `alloc_run` only to the bitmap. A
crafted stick's extent over a group's inode table:
- is served to the kernel by `map_file`, so write-back of that file writes into the inode table;
- is freed by an unlink, a truncate, a growth's cut past the size, or a directory growth's cut,
  so the next allocation takes the block and zeroes it.

A crafted stick whose bitmap marks an inode-table block free has it handed out by `alloc_run` the
same way. Either way the root inode was zeroed in the reproductions, and e2fsck could not proceed.

Linux checks extents, frees and allocations against its system zone (`ext4_sb_block_valid`): each
group's superblock and descriptor copies, its two bitmaps and its inode table.
- **The check needs no table, and no state from a mount.** The library has no `alloc`, `Ext4`
  holds only its reader, and `nxinstall` calls the library's functions with no mount at all. So
  the check is made per group:
  - the superblock and descriptor-table copies a group holds are computed from the superblock's
    own fields (`sparse_super`, the table's blocks, `s_reserved_gdt_blocks`);
  - the group's bitmaps and inode table are read from its descriptor, through the metadata cache;
  - with `flex_bg`, from the descriptors of its flex group, one block of them.

  A run touches at most two groups at 4 KiB blocks.
- **A writable mount checks that the layout is one the per-group check covers.** That means every
  group's metadata lies in its own group, or in its flex group's first group, which is what
  Nitrox's `mkfs` and `mke2fs` make. A filesystem whose layout is otherwise (a fallback of
  `mke2fs`, or one `resize2fs` moved) is served read-only, with the reason, as `writable()` serves
  a feature it does not keep. A read-only mount neither allocates nor frees. The check is never
  skipped silently.
- **Where it is made:**
  - **where an extent is read:** the leaf and index sites in `extent.rs`, so `map_file` never
    hands the kernel a metadata extent;
  - **where a run is freed:** `ext4::free_run`, which every free goes through, the cuts,
    `free_tree`, `fold`, the give-backs and `free_block` among them;
  - **where a run is allocated:** `alloc_run`, which refuses a free bit over metadata as
    `Corrupt`.
- **`on_device` refuses block `first_data_block`** and below, as Linux does, which shuts out the
  primary superblock on a 1 KiB-block filesystem.
- **Test, in `volume/hostile_tests.rs`:**
  - an extent pointed at each kind of metadata block, including a backup superblock on a fixture
    of several groups:
    - `map_file` answers `Corrupt`;
    - an unlink and release, a truncate, a growth over the extent past the size, and a directory
      growth over one past its size each leave the block's bit set;
  - a bitmap bit cleared over an inode-table block, then an allocation whose goal is that block:
    refused, and the root inode intact;
  - the neighbour case: an extent at the first block past the inode table maps, frees and
    allocates as before;
  - a writable mount of a layout outside the rule is served read-only, with its reason.
- **Not here:** an extent over another file's blocks. Linux does not check that either; only an
  fsck's scan can.

**A volume recorded clean.** ext4's unmount writes `STATE_CLEAN` whatever it found and whatever
failed.
- A filesystem found not cleanly unmounted (the mount only logs "serving it anyway") is recorded
  clean at its unmount, which hides the crash's damage from the next `e2fsck -p`.
- So is one whose change failed during the mount.

FAT keeps a `found_dirty` for both. ext4's volume does the same: it remembers a state found
unclean, and marks itself on a change that fails. Its unmount then leaves the state as it was
found, the error bit included. Tests:
- a volume found unclean stays unclean after a mount and an unmount;
- one whose create was refused stays unclean;
- one found clean, with nothing failed, is recorded clean.

**Gates:** `check-storage`, `check-media` and `check-install`, which write ext4. No new gate.

**Built 2026-11-20**, in five commits. Where it departs from the above:
- **More left `ext4.rs` than `dir_insert`**: the allocator, the growth, the insert, `s_state` and
  the superblock moved to `ext4/` together, since each fix touched one and the file was at its size
  entry.
- **The fs-9 test sweeps a growing `mkdir` and rename too**, the create's class, and checks each
  write refused once straight after it as well, since an undo is what makes that clean. The create
  whose inode write is refused has a test of its own: its leak reads in `e2fsck` exactly as a lost
  count write's does.
- **The layout rule is "inside its flex group"**, not "in its own group or its flex group's first":
  `mke2fs` on 1 KiB blocks spills a flex group's inode tables past its first group, and the check
  reads the same descriptors either way. `the_zone_is_the_metadata_dumpe2fs_names` holds it to
  `dumpe2fs` at every block of seven layouts, `sparse_super2` and a reserve among them.
- **A volume is marked by any write lost, and any operation answering `Io` or `Corrupt`**, reads
  included (the maintainer's call, on the review): a change can answer `Ok` or `TooLarge` with a
  write lost inside it, so `Ext4` reaches its device through a watcher that notes each refused
  write. A refusal for what was asked, a name taken, marks nothing. `was_left_clean` counts the
  error bit as not clean, as `e2fsck -p` does.
- **Two gates assumed the old record**: `check-storage` and `check-recovery` mark the copy's root
  not clean before boot, mount it writable and unmount it. They now expect it left not clean, the
  table saying so, and on the host `e2fsck` clean with `s_state` still not clean. So an installed
  root a crash left not clean is now reported so at every boot until something checks it, which
  `TODO(fs-repair)`'s entry says.
- **A test fixture held a copy of its own image**: `fixture_blocks` made the image inside the tree
  `mke2fs -d` copied, filling most of a small fixture's group 0. Fixed first, in its own commit.
- **Four older tests used blocks no file can hold**: two 32,768-block extents through a backup
  superblock, now on a layout with none past group 0; a free-run comparison over the inode tables,
  now across group 1's end into group 2; and geometry edges whose inflated inode count is now also
  a layout no writer may keep, checked read-only.

### H.2 service-mgr and init

Five defects, three test gaps and a stale line, in a binary whose `main.rs` is at its entry. **The
first commit moves into `lib.rs` what the fixes and their tests need**, with its size entry
lowered:
- `compute_backoff` and `should_restart`, onto `RestartConfig`;
- the count of skipped critical declarations;
- `wait_codes`;
- `serve_resolves`;
- the routes' bookkeeping.

This is the move that lets a host test reach each one over `libos`'s mock, which service-mgr
already takes for its tests.

- **sup-3: the backoff saturates.** `1u64.checked_shl(attempts)`, then `checked_mul` by the
  initial, else `u64::MAX`, then the cap. Test: attempts 54, 55, 63, 64 and `u32::MAX` give the
  cap at 1 s/5 min, and attempt 56 gives it at 100 ms.
- **sup-8 and its sibling: a duplicate is checked against every name seen, taken or skipped.** A
  name's first block decides, whether it was taken or skipped, so an appended block can no longer
  replace it. A critical declaration skipped as a duplicate no longer counts as unreadable.
  - Test, over the parser and `unfit`:
    - a whole duplicate of a critical declaration boots, and so does an appended block;
    - a block skipped for its endpoint, another declaration, then a good block of the same name:
      neither block is taken, whether the first is critical or not, and a critical first sends the
      boot to `eshell`. The declaration between is what tells the fix from today's code, since
      adjacent headers already take neither.
- **sup-10: the exit-code grace waits its whole length.** `wait_codes` loops until the
  notification queue fires or `until` passes, as `lookup_serving` does. Test over the mock: a
  route's resolve during the grace no longer ends it. `mock::sleeps()` pins it, and a five-line
  control that queues a notification shows the late code taken.
- **sup-4: a pass answers a bounded number of resolves per route.** `serve_resolves` and the root
  and session loops of `serve_services` take at most `PER_PASS` each. A ring left non-empty wakes
  the next wait at once, and so do the admin and power sessions' loops. Test over the mock: more
  than `PER_PASS` queued, one pass answers exactly `PER_PASS`, and the rest stay queued.
- **sup-11: a route copy is sent for a server that is not live yet.**
  - `route_copy` finds or makes the route with `self.route(..)`, as a normal boot does, with no
    `live` filter.
  - A route that is not live answers `NotFound` until its server registers, which is what a
    restart already looks like.
  - Test over the mock: a copy taken before Ready, then the registration; a resolve on the copy is
    then answered.
- **sup-14: `init`'s unmount answer is held to its request.** A wrong magic is refused, a test
  only. A reply id other than the request's is refused too, which is a code change in `init`'s
  critical path: `answer` takes the request's id, and `unmount_one` passes it. The author makes it.
- **sup-15, sup-16: tests.**
  - A mistyped `policy` falls back to `Never` and a mistyped `backoff` to the default.
  - `parse_duration_ns` saturates past `u64::MAX` nanoseconds and refuses a number past `u64`.
  - libinittoml refuses `key = "a"b"`.
- **`init/CLAUDE.md`'s Testing section** says what `unmount.rs`'s tests already use.

**Gates:** `test-interactive`, `check-login` and `check-shutdown`, which start, stop and supervise
services.

**Built 2026-11-20**, in eight commits. Where it departs from the above:
- **The first commit is a refactor, not a move**: the routes were `Mgr`'s methods, so they went to
  `service_mgr::routes` behind a `Routes`, `wait_codes` as a function of the queue and the routes,
  and `main.rs` delegates. Behaviour unchanged; its size entry fell from 1,855 to 1,689 over the
  part, the review's fixes included.
- **sup-4 needed no code to wake the next wait**: a wait on a channel is satisfied while its ring
  is non-empty (`syscall-abi.md`), so a pass that stops at `PER_PASS` (16) is served again at once.
  Every loop takes through `routes::take`, the admin and power sessions' and endpoints' too.
- **sup-10's grace also ends by the clock**, for a route that keeps signalling, which the bound made
  testable. The notification that comes after a route's wake is libos's `mock::notify_after_wait`.
  On the way: a timed-out wait moves the mock's clock 1 ms, where the kernel returns only at the
  deadline, so the loop ends on an empty wake rather than reading the clock alone.
- **sup-11's test shows the copy is the route its registration finds**: the copy carries
  `TRANSFER | DUPLICATE` and no `SEND`, so the resolves are put on the route itself.
- **sup-14's request id is a constant**, `unmount::REQUEST_ID`, which `flush_then_ask` sends and
  `unmount_one` checks.
- **The test gaps' tests were written here**, not briefed to the implementer: each was a few lines
  beside a fix in the same file.
- **Its review** (#461) made the bound count every message, decoded or not, and bound a look at a
  control channel too; moved the session lists' walk into `routes::serve_each`; had the login
  supervisors resolve the broker's supervisor channel again at a login (`libsession`); and planned
  storage-service's unmount answer into H.3.

### H.3 storage-service and the broker: one GPT reading, a probe that waits, grants in a view

- **sup-2: the kernel records each partition's GUID.**
  - The device record grows by the partition's unique GUID. The kernel's `register_partition`
    fills it.
  - `libkern`'s `DeviceRecord`, `docs/spec/device-node.md` and `rsproto-devices-ops.md` change
    with it, and `abi-sync-check` holds the two sides.
  - storage-service's `source_device` matches `gpt-partuuid:` on the record and no longer calls
    `read_table` for it. libgpt stays for what writes a table.
  - Tests:
    - a kernel test that a registered partition's record carries its GUID;
    - storage-service tests of `source_device` over records for each of the eight cases,
      including the wrong match;
    - a boot that holds the match end to end. No gate boots a `gpt-partuuid:` root today, so one
      gate's image names its root by GUID, and the boot checks storage-service knows `init`'s
      mount. Which gate, H.3 settles.
- **sup-5: a re-probe is rate-limited.**
  - A probe that waits on a device cannot be cut short, as a dropped I/O waits out its device
    before the buffer is free again.
  - So `refresh_unmounted` runs at most once an interval, decided in a library module with a clock
    seam. Between runs a resolve is answered from what the last probe found. A name that names no
    device no longer probes at all.
  - Test: the decision, over a moving clock.
- **sec-10: a grant inside a view binds the broker's endpoint.** The grant's bind moves out of
  `main.rs` into a library module, beside `clock.rs`. On `AlreadyExists` it unbinds the caller's
  binding and binds its own: the view gets the broker's endpoint, not whatever the caller had.
  `bind_forwarding`, for policy and accounts, goes the same way. Test over the mock: each grant in
  a view made from a view.
- **The unmount's answer is held to its request**, as `init`'s is since sup-14: PR #461's review,
  finding 6. `unmount` takes any `OP_UNMOUNT` without the error flag as recorded, whatever its
  request id and whether `RS_FLAG_REPLY` is set, and `tear_down` sends the same request. The check
  moves into a library module with the code H.3 moves out of `main.rs`, where a host test holds it:
  another request's answer, a request rather than a reply, and an error.
- **sup-12, sup-13 and T1-24: storage-service's probe tests, in `tests/probe.rs`.**
  - `partition_entries` with each mark broken, and its neighbours.
  - The RamDisk arm.
  - `fat_label`'s FAT12/16 marks and sector sizes 0, 256 and 8192.
  - Its two fallbacks: a FAT whose type string alone is cleared, and the 4 KiB-sector label.

**Gates:** `check-storage`, `check-media`, `check-install`, `check-recovery` and `test-interactive`
(`with`).

**Built 2026-11-20**, in five commits. Where it departs from the above:
- **sup-2's boot is the test image's**: its root partition is built with a fixed unique GUID and
  its `init.toml` names it by `gpt-partuuid:`, so every test-image boot holds the match, and
  `boot-probe`'s storage test, which checks `nitrox-root` is `init`'s at `/`, fails without it. The
  GUID is the record's last field, at offset 144: 160 bytes, `REGISTRY_VERSION` 3.
- **sup-5's probe runs when the mounts have changed, too**, whatever the time: a device just
  unmounted was last read before its mount. The interval is two seconds, or four times the last
  probe's length if more, so probes hold a fifth of the thread at most. An administrator's `Mount`
  and `Reread` read their devices as before.
- **sec-10's bind is `spawning::grant`**, not a module of its own beside `clock.rs`: `lib.rs` is
  at its size entry. `test-interactive`'s step 20b holds it, a `with admin whoami` inside
  `with admin nxsh`, which fails without it as the bug bash's boot did. Its review added the
  kernel's log, a fresh snapshot in place of an outer view's, and the `disks` grant, which takes
  out what the copy held first.
- **No code moved out of storage-service's `main.rs`**: each fix fitted, the resolve's file arm
  going to `Service::table_file` in `disks.rs`, and `main.rs` fell from 1,160 lines to 1,149. The
  unmount's answer is `mounts::unmount_recorded`, in the library.
- **The probe tests were the implementer's**, each mutation run again by the author.

### H.4 The profile server's index, and an eject's session

*Rescoped 2026-11-23*, by the maintainer's call above: what of the shared servers' slots is wrong
at any load.

- **The profile server keeps a projection's index only once every package read whole.**
  - A package whose directory was refused, or whose listing was cut short, is skipped as before.
    The view is then not kept, and the next lookup or listing builds it again.
  - A package with no such directory, `NotFound`, is read whole: most have no `applications/`.
  - The merge and the keep move into `profile_server::index`.
  - Test: one package refused and another served, then both once the refusal clears, and the
    view kept after.
- **A media session closes once it has answered its request**, as its comment and the spec say it
  is held for one. The eject moves out of storage-service's `main.rs`, into `media.rs`.
  - Test: `boot-probe`'s storage admin test, its first eject's session closed after the answer.

**Gates:** `test-qemu`, every gate that logs in (the profile server's `/applications`),
`check-storage` and `check-media`.

**Built 2026-11-23**, in two commits. Where it departs from the above:
- **The profile index is held on the host, not over the mock.** The binary opens each package's
  directory and says whether it read whole. The merge, the keep and which failure is "no such
  directory" are `profile_server::index`'s. The release gates' boots show the last:
  `/applications` and `/manual` are each built once, every package without one counted read.
- **`boot-probe`'s storage admin test opens a second media session** for its second eject.
- **Its review (#463) narrowed when a view not kept is built again**: at a listing's first page and
  for a name it lacks, which is `WouldBlock` while a package that may hold it has not read. Each
  later page reads the view its listing began with. The package's read moved into the library.
- **And found libfsserver answering `NotFound` for a directory whose walk lost a read**: it fell
  through to the file path, where a directory is `NotFound`, which the profile server takes for
  "no such directory". Only a walk that finds no directory falls through now; any other failure is
  the answer.

### H.5 Folded into H.4

The rest of the shared servers' slots is deferred, 2026-11-23. `libfsserver`'s admission is
unchanged, so H.10 tests its bounds as they are.

### H.6 The shell's environment, and two commands that ignore an argument

- **nxsh-1: `$env` writes through.**
  - An assignment whose root is `$env`, in any form, is applied to a copy, and the copy is judged
    as a whole, not by the assignment's syntax. It must be a record of strings, and its `PWD`
    must be there and unchanged; otherwise the assignment is refused, naming `cd` for `PWD`. Then
    it becomes the record the shell reads, which is what `cd`, `~`, every stage's environment and
    `program_path` all use.
  - The forms judged alike: a field (`$env.HOME = …`), a name index (`$env["PWD"] = …`), the
    whole record (`$env = {}`, `$env = ($env | update PWD "/x")`) and a value that is not a
    record (`$env = 5`).
  - A new field adds a variable.
  - The binding is republished from the record, so the two never disagree again, as
    `rebind_env` promises.
  - The code goes in a module of its own; `assign` calls it in one line, as `eval.rs` is at its
    entry.
  - `shell-language.md` says `$env` is assignable, with a version bump, and `$args` stays
    unassignable.
  - Test, a new `env_tests.rs` driven through the REPL:
    - assign a field, run a program, and read its environment, and the same by a name index;
    - each form that would remove or change `PWD`, or make `$env` not a record, refused, with the
      directory unchanged;
    - `cd` and read again;
    - a bare `cd` and `~`;
    - a new field;
    - `$env.PWD` refused.
- **nxsh-2: `cd` and `exit` take one argument or none.** Both match `c.args.as_slice()`, as `help`
  does. A second argument is refused, and a flag is named as a flag. Test: `cd /a /b` is refused
  with the directory unchanged, and `exit 1 2` is an error, not an exit.

**Gates:** `test-interactive` and `check-terminal`.

**Built 2026-11-23**, a commit a finding. Where it departs from the above:
- **The copy is judged by whether a program can be handed it, not as "a record of strings"**: the
  login's environment carries `PATH` as a List, so that rule would have refused every assignment.
  A value must fit its field's type, as the encoder requires, and is not null; a new field takes
  its value's type; `HOME`, when there is one, is a String, as `cd` and `~` read it; and `PWD` is as
  it was.
- **The whole-record form is `merge`**: the shell has no `update`, so `$env = ($env | merge {…})`
  is what the tests and the spec use.
- **`rebind_env` stays in `eval.rs`**, as the binding's slot is private there; `set_env_entry`
  moved to `environment.rs` with the assignment. `eval.rs` fell from 4,140 lines to 4,126.
- **Its review (#464) made `$env` one environment, not a binding** (the maintainer's call,
  2026-11-23): a `def` or a closure writing it left the prompt's `$env` behind, as a `cd` in a
  `def` already did. `$env` now reads the record wherever it is read, `rebind_env` is gone, and no
  binding takes its name, refused by the parser. `parse.rs`'s argument-list tests moved to a file
  of their own to make room.
- **And held the copy to more**: each name once and none empty, a variable to the type it had
  whichever form assigns it, a Table refused as itself, and the whole of it encoded as a program's
  setup message would be, so nothing nested or oversized reaches a stage.

### H.7 The kernel's guards, held

Tests only, but one helper, in new test files, since `syscall/table.rs` and
`object/file_object.rs` are at their entries.
- **kobj-12, and the spawn's and the memory reply's siblings.** `duplicate` is asked for a right
  its source lacks, and refuses it. A spawned child asks for syscaps its parent lacks, and gets
  none of them. A memory object's reply is asked for more than it grants, and gets only what it
  grants.
- **kobj-13: each syscall's own guards, run as a user.**
  - An `as_user(pid, syscaps, ..)` helper, in a new `syscall/table/tests/authority.rs`, runs a
    call as a process with chosen syscaps. Its cases:
    - a bind without `BIND_NAMESPACE`, or without `TRANSFER`;
    - a direct bind with a base;
    - `ns_create` from a namespace without `LOOKUP`;
    - `granted`'s intersection;
    - a send without `TRANSFER`;
    - a send whose payload length passes the payload;
    - the sender's pid stamped;
    - a map of two pages of a one-page object.
  - The guards are tested as functions today, and none where it is called.
- **kmem-14:** a frame is dirtied and freed, then an object is made, and the object reads zero.
  The host's buddy is one allocator every concurrent test shares, so the object may not get the
  dirtied frame, and the one it gets may be zero by chance. The test therefore checks the object
  holds the dirtied frame by its address, and tries again when another test took it.
- **kmem-17:** `find_free_range` refuses a request one page larger than its window, takes the
  whole window, and finds a hole that fits exactly.
- **proto-16:**
  - an empty error body, a three-byte one and an eleven-byte one are each `Malformed`, where
    dropping the minimum panics the kernel;
  - a forget with a bad magic is refused;
  - each request builder refuses a suffix of 65,536 bytes and takes 65,535, the sized builder's
    and the rename's destination among them.
- **sched-10:** a poll, `wait_on` with a deadline of 0, on objects none of which is ready answers
  `TimedOut` at once and registers no waiter, so removing the poll path fails a test, not an
  abort.
- **The PID 4491 flake:** one of the two tests is renumbered, and a test that the kernel tests'
  PIDs are distinct holds the class.

**Built 2026-11-23**, a commit a finding, every mutation each names failing a test. Where it
departs from the above:
- **kobj-12's duplicate does not refuse**: it intersects, so the test holds that the copy gets
  none of the rights its source lacks.
- **The spawn's syscaps are read from a test-only record**, `process::MADE_WITH`, the syscaps each
  user process was made with: a host test cannot start the child, so the parent's handle to it is
  refused and the child read as it was made. That is a second test-only helper beside `as_user`.
- **sched-10's poll lists an object nothing can wait on**, beside a channel not ready: without the
  poll path its registration fails and the poll answers `OutOfMemory`, a failed test, where a list
  of waitable objects would block or abort.
- **kmem-17's in-loop bound is an equivalent mutation**, as the pass found.
- **The pid check scans the kernel's sources** for the form each test names its pid in, a `u32`
  constant in the 4000s, however it is written; plumbing's `LIVE` took 4493.
- **Its review (#465) held each intersection's other half**, the ask's: a lookup asked less than a
  binding or a server grants gets no more than it asked. It added the map's rights, and a spawned
  child's handle rights, `held & asked`, read from a test-only record of the last install asked
  for, as the spawn rolls the child's handles back.

### H.8 The drivers' guards, held

The tests modules of `pci/mod.rs`, `drivers/hid/descriptor.rs` and `drivers/xhci/desc.rs` move to
files of their own first, a move the implementer makes, and `pci/mod.rs`'s entry is lowered. Then
each guard gets an input that only that guard decides. The surviving mutants all pass the suite
today because the broken code and the correct code give the same answer on the existing inputs.
- **drv-9:**
  - an entry size of 64, which panics without the range;
  - 192, which reads as an empty table without the divisor;
  - five entries declared with a stale sixth;
  - `0xFFFFFFFF` entries, read to the cap.
- **drv-10:** each of `partition_rebase`'s four overflows decided by itself, and a dispatch through
  a partition node at an offset that wraps.
- **drv-11 and the seven arm guards:** one short descriptor per guard, each a panic without it. The
  existing zero-length test, which hangs rather than fails when its guard goes, gets a bound. Two
  checks are not of a length: `configuration_total`'s `bLength` below 9, and `hid_interfaces`'
  first interrupt-IN endpoint winning, held by an interface with two.
- **drv-12:**
  - `dispatch_block_irp` refuses a request past its device's fragments.
  - **USB storage refuses what it clamped**, as AHCI's backstop does: it no longer issues 65
    fragments as 64 and completes them whole. That is a code change, about five lines, with its
    test in `storage/tests/refusals.rs`.
- **drv-13 and `id_slot`:** each rejection after a valid mouse, so a corrupt item cannot pass for
  "no mouse".
  - A truncated item, and a truncated long item.
  - A second mouse collection.
  - A second key array.
  - Five pushes, one past `STACK_MAX`, and a pop with nothing pushed.
  - Sixteen report ids and a seventeenth.
- **drv-14:**
  - a multifunction header (`0x80`);
  - 4- and 8-byte I/O BARs;
  - sizing read-backs that keep their type bits, as hardware returns them.
- **drv-15:** `logical_min` 1 with the first usage at `0x05`, so the filter alone decides. The test
  compares `Layout::read`'s `Held`, as events cannot show a held usage that maps no key.

**Built 2026-11-23**, a commit a finding, every mutation each names failing a test, 39 in all.
Where it departs from the above:
- **USB storage refuses before it queues anything**: `command_for` answers `InvalidArgument` for a
  request past 64 fragments, and `issue`'s slice stays as the bound of the array it fills.
- **The zero-length walk runs on a thread of its own**, and a test fails when it has not ended in
  five seconds; a descriptor of one byte is held beside it.
- **The PCI fixtures answer a sizing read with their type bits**, the 64-bit one too, so each
  mask's control fails; a 32-bit prefetchable BAR holds the 32-bit memory mask.
- **drv-15's `FIRST_KEY` bound is held as well**, by slots naming usages 3 and 2.
- **Its review (#466) held the cap where it falls**, the 128th entry read and the 129th not, and
  the window node's wrapping end by its own guard. A long HID item past its bytes is refused as a
  short one is, a mouse collection inside the first does not end it, and `issue` asserts the
  fragments `command_for` refuses.

### H.9 The parsers and the credential path, held

- **proto-6:** libgpt's `read`, given crafted headers resealed with a valid CRC. Three of its
  guards panic storage-service on a crafted stick when dropped.
  - The array past the buffer.
  - `header_size` 91, 92, 512 and 513.
  - Entry lengths 8, 127 and 129.
  - Counts 0, 1024 and 1025, the last in a buffer long enough to hold it.
- **proto-11:** a list and a schema whose counts claim four billion items are `UnexpectedEof`.
  Without the caps the test aborts on the allocation, which fails the run.
- **proto-12:** both name encoders refuse a 65,536-byte name and take 65,535; so do
  `resolve_request`'s and `subnamespace_reply`'s caps, which have the same gap.
- **proto-17:** a table ended by an error record, and by tag `0x7F`, is `BadRecordTag` through
  `Table::decode` and the reader.
- **sec-7:** two 32-byte values that differ at each single position compare unequal.
- **sec-8:** an empty password is denied for an account that exists, through `authenticate` and
  `serve_authenticate`.
- **sec-9:**
  - known answers for 55, 56, 63 and 64 bytes, from NIST's short-message vectors;
  - HMAC with a 65-byte key equals HMAC with its digest, and a 64-byte key does not.
- **sec-14:**
  - an odd-length salt;
  - iterations `4294967295`, `4294967296` and `4294967297`;
  - an empty home refused.
- **sec-15:** `libprompt`'s four scrubs, seen through a test allocator that counts freed blocks
  holding a marker. It has a reason under Miri, which would read freed bytes.
- **sec-13: a leftover `users.new` cannot leave a stale tail.**
  - `install` asks `INSPECT`, and after the create refuses unless the file's size is the new
    database's, as `libfs::open_sized` does.
  - `install` moves into auth-service's library to be tested, with `libos`'s mock as a
    dev-dependency.
  - `auth-service/CLAUDE.md`'s rule that the library makes no syscalls changes in the same commit.
  - Test, with `ignore_next_truncate`: a leftover longer than the new database fails the install
    and leaves `/system/users` as it was. An ordinary install writes exactly the new bytes.

**Gates:** `check-login`, `check-recovery` and `test-interactive` (`with`, `account`).

**Built 2026-11-23**, a commit a finding, every mutation each names failing a test but one, which
cannot. Where it departs from the above:
- **proto-12 holds the whole class**: every length cap in `librsproto` at its width and one past,
  the storage, auth, meta, log and views encoders' and the error body's too, and a listing read to
  its count; and each named maximum, the clipboard's, the desktop's and the surface's, at itself
  and one past, by its writer and its reader.
  - Untested: three `u32` lengths, a log message's, a view request's environment and
    `reply_object`'s, each a 4 GiB boundary.
  - The clipboard session's check on a copy is an equivalent mutation: the encoder it calls
    refuses the same.
- **proto-17 holds the stream bitmap as well**, as the bash named it: its old case's handle count
  decided it.
- **proto-6 holds the array's checked start and end**, and **sec-14 the iteration count's
  multiply** beside its add.
- **proto-11's test counts what is reserved**, through a test allocator, where a host granting
  the claim lazily would pass a reader without its caps. **Its row cap is an equivalent
  mutation**: it counts fields already decoded, each at least four bytes of input.
- **sec-9's known answers are OpenSSL's**, an implementation independent of this one: NIST's file
  would be a download.
- **sec-15 runs under Miri**: the allocator, its blocks zeroed when made, reads only byte buffers.
  Read whole, a block holding a struct's padding is uninitialised, as Miri found.
- **sec-13's install asks `INSPECT`**, and a write-back that fails is held to leave the file as it
  was. The next install's truncate cuts what that left, and each refusal names its step: a size
  that cannot be read, a leftover's tail, a database past a page. The model cannot fail a file's
  create or a rename.

### H.10 The filesystems' guards, held

- **fs-13:** `serve_read_range` refuses a suffix of `MAX_SUFFIX + 1` and serves one of `MAX_SUFFIX`.
  A read past the content is clamped. `apply_size` and the rename refuse an over-long suffix.
- **fs-16:** `MetaCache::forget` of a range whose end is not 4 KiB-aligned lets go of the partial
  block, and a range that ends on a boundary keeps the next.
- **pr365-7:** the 1 MiB grow's write count is exact, 19, not "at most 24".
- **pr365-8:** a forwarded truncate to 0 sends `OP_FILE_FORGET` with the ended id, then the reply,
  then the release, through `libfsserver`'s server loop over the mock and a spy.
- **pr365-11's ten:**
  - a failed flush leaves the cache as the device (T1-13);
  - the volume label is not a name (T1-23);
  - the 64-run limit and its neighbour (T1-26);
  - the long-name ordinal bound (T2-18): the garbage test enumerates its ordinal and flag pairs,
    as its seed never drew the one that panics;
  - the long-name checksum and part checks (T2-19);
  - the numeric tail's window, by its reads (T2-20);
  - the rewrite's slot bound (T2-21);
  - the FSInfo hint (T2-22);
  - FAT32's reserved nibble (T2-23);
  - DEL in a name (T2-24).
- **The two equivalent guards.**
  - The numeric tail's leading-zero check goes: `tail_of` refuses the same names, and the window
    starts past tail 0.
  - The FSInfo hint's check at load stays, as a disk's hint is untrusted input. A comment says
    `allocate` checks it again.
- **The numeric tail's bound** is derived from `MAX_SLOTS`, so the `TooLarge` after its loop is
  reached, rather than a constant only 2,048 colliding names could show.

**Gates:** `check-storage`, `check-media` and `check-copy`.

**Built 2026-11-24**, in two commits, libfsserver's and FAT's, every mutation each names failing a
test. Where it departs from the above:
- **fs-13 holds `directory_open`'s bound as well.** Its test refused a suffix whose whole path
  named nothing, so the walk found nothing with the bound or without it. The path now names a
  directory, and one of `MAX_SUFFIX` opens a session.
- **The FSInfo hint's check at load is held, not commented**: it is no equivalent mutation.
  Without it, an unmount that allocated nothing writes back a hint outside the volume, where it now
  writes the first cluster.
- **T1-13 is held for every change**: the change sweep holds the mount's FAT cache to the device
  after each failed write.
- **The 64-run limit is held at 64 and 65**, its test moved from 71.
- **The numeric tail's bound is `MAX_SLOTS` itself**: a full root of one basis is `TooLarge`
  after 64 walks, and the test counts the walks.
- **T1-27 and T2-17**, held by tests written since, are named in `bug-bash.md`.

### Miri at Part H's end

Over the job's targets that Part H changes, the kernel among them, and any target a sub-part adds
to the job. The test allocator of H.9 carries its reason under Miri.

### What Part H does not do

- **The kernel's per-process quotas**: threads and deadline slots, the maintainer's call.
  `TODO(deadline-slot-quota)` stays.
- **A share a session of the shared servers' slots** (sec-4, sup-6 and their class), the
  maintainer's call of 2026-11-23. `TODO(server-fanout)` carries them.
- **An extent over another file's blocks** (fs-11's last clause): only a filesystem check's scan
  can find it, and Linux does not check it either.
- **The starvation of sup-4 shown in a boot.** The bound is held by host tests, and the flood that
  would show it needs a client of several threads that no gate has.
- **A stray NUL in `init.toml`.** It drops every mount after it, for `init` and storage-service
  alike, now that both read by one rule. The manifest is trusted initramfs data.
