# fs-server-ext4 read-write

**Status:** Implemented — `userspace/fs-server-ext4`, read **and** write, with host tests
that build an image with `mke2fs` and require `e2fsck -fn` to find it clean. Individual
deferrals are marked inline. Verified 2026-09-17, when cross-group allocation landed and the
deferred list was swept against the code. Administration Part C.1 (2026-09-24): the block-file
reply carries the inode number as the file's id, `File::Touch` names a file by it, and a grow
zeroes on the device what it adds. C.1b (2026-09-24): an unlink or a replacing rename frees the
file only after the kernel has answered `File::Forget` for it. C.3 (2026-09-24): a read-only mode,
and `s_state` kept — § *Read-only mounts, and how a filesystem was left*. C.5b–C.5c (2026-09-25):
**the storage service spawns it for every filesystem it mounts**, keeps its control channel, and
unmounts it with `Meta::Unmount`, which `boot-probe` drives on every `test-qemu` run
([`storage.md`](storage.md)). Phase 6 Part D.4 (2026-10-06): a server nothing can reach exits.
Phase 6 Part E.1 (2026-10-06): **the protocol is `libfsserver`'s** — the bootstrap, the loop,
directory sessions, `File::Forget` and `File::Touch`, `Meta::Unmount`, and `ReadOnly` — shared with
`fs-server-fat` ([`fat-fs-server.md`](fat-fs-server.md)); this crate is ext4 as a `Volume` and a
`_start`, and keeps its 4 KiB `Disk`. PR #365's review (2026-10-07): **a removed directory is
refused to a session still holding its inode**, since a removal keeps the inode's mode and extent
and zeroes only its link count; every operation by inode now asks for a link counted. And a server
that panics exits, rather than spinning with resolves waiting on it. Phase 6 Part G (2026-10-07):
**its formatter has a second caller**, `disk --format DEVICE ext4` — § *Making one*. Phase 6 Part
H.1 (2026-10-08): **extent trees** — written, cut and freed at any depth; an extent's length as
ext4 defines it; and a grow, a create, a `mkdir` or a rename that fails takes nothing with it —
§ *The extent tree*. Phase 6 Part H.4 (2026-10-09): **blocks are allocated and freed a run at a
time**, and **a grow's reply marks what it added `UNWRITTEN`**, so its zeroes are not read back —
§ *Part D*.

How `fs-server-ext4` becomes writable — its **ext4-specific realization** of the generic
Model A data-path contract. Read the contract first: **`docs/architecture/filesystem-data-path.md`**
defines the fs-neutral protocol (`MapRange`/`AllocRange`/`BlockRun`), the kernel interface
(`FileObject` producer, dirty tracking, writeback, `MAP_WRITE`, `sys_file_sync`), the shared
device, and the data-before-metadata ordering. This document covers only *how ext4 backs
that contract*: extent trees, bitmaps, inodes, directory entries — none of which the kernel
or the protocol knows about.

Phase-3 spine item 4 (the write path — "write files to home"); milestone: *log in →
per-user namespace → write files to a home directory* (auth/session, item 5, follows).

## Starting point (the read-only reader)

`userspace/fs-server-ext4/` parses, read-only: the superblock, group descriptors, inodes,
the **extent tree** (`extent_find`, a lookup per block, until Phase 6 Part H.1), and directory
entries. It **never reads the
block or inode bitmaps** and mutates nothing. Fixtures are built journalless and
checksumless — `mke2fs … -O ^has_journal,^64bit,^metadata_csum,^resize_inode` — which bounds
this slice (below). It serves `File::ReadRange` today (Model B); this slice moves it to
Model A.

## ext4's realization of the Model A ops

- **`MapRange` ← the extent tree.** ext4 already stores a file as extents (`ext4_extent` leaves
  under an `ext4_extent_header`, walked by `extent.rs`'s `Lookup`, which reads each leaf once).
  Producing a `BlockRun` list for a range is that walk, re-exposed as the op: each ext4 extent → one
  `BlockRun` (`file_block = ee_block`, `device_lba = 48-bit start`, `length` from `ee_len`), extents
  that continue each other on the device merged; a sparse hole, and an **unwritten** extent, →
  `device_lba = 0`. Read-only; the reader's logic, lifted into a served op. *(A FAT32 server would
  instead produce the same `BlockRun`s from its cluster chain — the wire result is identical; that
  is the point of the neutral contract.)*
- **`AllocRange` ← block bitmap + extent insertion.** To back a hole / grow a file, ext4:
  allocates device blocks from the **block bitmap** (new machinery — the reader never touches
  bitmaps), **inserts** them into the file's extent tree (splitting leaves / adding index
  nodes as needed — the reader only *walks* the tree), and updates the free-block counts in the
  group descriptor + superblock. Returns the new blocks as `BlockRun`s.

## ext4 metadata mutation (the new write machinery)

Extends structures the reader already reads; all written via the fs-server's **own
read-write device handle** (`sys_io_submit` Write), never by the kernel:

| ext4 structure | reader today | RW adds |
|---|---|---|
| Block bitmap | *not read* | allocate/free device blocks |
| Inode bitmap | *not read* | allocate an inode (file creation) |
| Extent tree | walk (`Lookup`) | append, split and deepen on growth; cut and fold on a truncate (`extent.rs`) |
| Inode | read mode/size/flags/`i_block` | update size (off 4/108), mtime/ctime, block count |
| Directory block | linear lookup (`dir_lookup`) | insert an `ext4_dir_entry_2` (split `rec_len` / new dir block) |
| Group desc + superblock | read a few fields | update free-block / free-inode counts |

**A file is freed in two halves** (administration Part C.1b). `unlink_at`, and a `rename_path`
that replaces a file, remove the name and return the inode whose last link is going. Its link is
still counted and its blocks still allocated. `release_inode` frees them, and the server calls it
only once the kernel has answered `File::Forget` for the inode
([`filesystem-data-path.md`](filesystem-data-path.md) § *One object per file*). The kernel may be
writing the file's pages to those blocks, and a block this server freed and handed to another
file before that write landed would be overwritten with a dead file's bytes. Between the halves the
inode is unattached with its link counted, the state `rename_path` could already leave on a crash,
and `e2fsck` moves it to `lost+found`. The server waits for the answer on buffers of its own,
since it asks from inside `serve_loop`'s walk of a batch of wait results. If the kernel cannot
be asked, the inode is not freed: a leaked block can be repaired, a block claimed twice cannot.

**No checksums.** The fixtures are `^metadata_csum`, so there is nothing to maintain. Enabling
`metadata_csum` (group-desc / inode / extent / dir / bitmap checksums across every write above)
is a feature-gated later addition.

## The extent tree

*(Phase 6 Part H.1, 2026-10-08; `userspace/fs-server-ext4/src/extent.rs`.)* An inode's `i_block`
holds four extents. A file with more is a tree: the inode an index over blocks of `(block_size -
12) / 12` entries — 340 at 4 KiB — and up to five levels. **Until H.1 this server wrote only the
inode's four**: a fifth was refused `Unsupported`, which on 4 KiB blocks stopped a directory at
about 815 files with data or 814 subdirectories (each file's data, each `mkdir`'s block, lands
between the directory's own blocks, so every directory block is an extent); free and truncate
refused a tree, so a file Linux wrote with one was read here and never freed — its removal took
the name and left an unattached inode holding its blocks.

- **Appending** (`extent::append`) adds an extent at the file's end, merged into the last when it
  continues it. A full leaf gains a sibling to its right holding the new extent alone, so the
  leaves an append fills stay full, as ext4's own appends leave them; a full inode moves its
  entries into a block below it, one level deeper, as `ext4_ext_grow_indepth` does. An append is
  **atomic**: on an error nothing it wrote is reachable and every tree block it took is given
  back. A tree block's allocation goal is the first data block, so it fills the earliest gap
  rather than the block a file's next data would continue into.
- **Growing** appends **a run at a time** — the blocks the allocator hands out contiguously, up
  to the 32,768 an extent can hold — so a tree changes once a run rather than once a block.
  Blocks past `i_size` (an `fallocate` that kept the size leaves them) are cut first.
- **Cutting** (`extent::truncate`) frees every block from a logical block on, drops the extents
  that held them, shortens the one across it, frees each tree block left empty, and **folds**
  what is left back into the inode while it fits: an index over one child of four entries or fewer
  takes them in its place. Without the fold a cut leaves a tree `e2fsck` calls a level too deep.
  A file is freed whole by cutting at `0`.
- **Looking up** (`extent::Lookup`) reads a leaf when a walk reaches it and keeps it while the walk
  stays inside, so a block map, a directory scan or a read costs a read per leaf, not a descent
  per block.
- **An extent's length is ext4's**: an `ee_len` up to 32,768 is a written extent of that many
  blocks, and above it an *unwritten* extent of `ee_len - 32768` — allocated and never written —
  which reads as zeroes. Until H.1 the reader masked the top bit, so a full 32,768-block extent,
  which Linux writes for a large file and this server for a file that grows through 32,768
  contiguous free blocks, read as a hole, and an unwritten one read its blocks' old contents. **An
  unwritten extent maps as a hole**, so the kernel writes back no page over one, as over any hole
  ([`filesystem-data-path.md`](filesystem-data-path.md)).

**What fails takes nothing with it** (H.1, found by host probes while the tree was planned). A
grow that fails cuts the tree where it began and writes the inode to match — until then every
block it had taken stayed in use, on a filesystem the grow did not fit all of its free space. A
create whose directory cannot grow gives its inode back; a `mkdir` gives back its inode, its block
and its group's count of directories, which is now **its own group's** — it was always group 0's;
and a rename within a directory that cannot take the new name puts the old one back, where the
file had been left with none.

The tree is held to **e2fsprogs** in the host tests: `debugfs`' reading of every map, and `e2fsck
-fn` clean — 4,096 files of 16 KiB in one directory; two files grown in turn to two levels at both
block sizes and cut back through each; a two-level tree `mke2fs -d` wrote, read, cut and removed;
`ee_len` `0x8000` and an unwritten extent; and every failure above.

## Read-only mounts, and how a filesystem was left

*(Administration Part C.3, 2026-09-24.)*

**A read-only mount is the server's.** Its supervisor sets `FS_SETUP_READ_ONLY` in the setup
message (`init` does for `init.toml`'s `"ro"`), and the server serves through
`fs_server_ext4::ReadOnly`, a `BlockReader`/`BlockWriter` whose every write is refused with
`FsError::ReadOnly`. So read-only isn't a check each mutation has to remember: whichever
mutation is reached, by a session or a resolve, it fails at its first write having changed
nothing, and the server answers `NoAccess` with the reason `read-only mount`. A grow or create
that arrives as a resolve is refused, rather than falling through to the file's current size as
other grow failures do. The same type answers `read_only()`, from which the block-file reply
takes its `FILE_BLOCKS_READ_ONLY` mark, so the kernel installs the files without `MAP_WRITE`;
the mark and the refusal are one fact. It never writes the superblock.

**A filesystem knows how it was left.** A writable mount clears `s_state`'s clean bit
(`mark_mounted`) before it answers `Ready`, so the filesystem never looks clean while it can
change, and a mount that cannot write that is refused (`Unservable::StateUnwritable`). An
unmount sets the bit again as its last write (`mark_clean`), on `Meta::Unmount` from its
supervisor on the control channel. The server then replies and exits
([`rsproto-wire-format.md`](../spec/rsproto-wire-format.md) § *Meta::Unmount*). A filesystem found
not clean is reported (`the filesystem was not cleanly unmounted last time`) and served anyway;
a repair tool is `TODO(fs-repair)`. **`init`'s mounts are unmounted by a shutdown** (administration
Part E.4): `sys_ns_sync`, then `Meta::Unmount`, last mounted first. A machine turned off without
one is still reported on its next boot.

**The control channel stays open after `Ready`**, and takes a wait slot while it is. A directory
session may use the last slot only once it has closed. **`init` keeps its end** for the shutdown's
unmount, since Part E.4a, so the root server's session capacity is `MAX_SESSIONS - 1`, 30, for the
whole boot, as the storage service's mounts' has always been. A closed control channel is still
ordinary.

**A server nothing can reach exits** (Phase 6 Part D.4). Its forwarding endpoint's other end is
held by its registration and every file it handed out. Once all of them have gone — a mount let go
without an unmount, as the storage service lets go of a USB disk that left — a receive there
answers `PeerClosed` for the rest of the boot. The server logs `fs-server: nothing can reach this
server any more; exiting` and exits, rather than spinning a CPU on it, as `device-mgr` does in the
same place. It writes nothing on the way out: its disk, if it left, takes nothing.

## Making one

**`fs_server_ext4::mkfs` lays out an empty filesystem** (Phase 5 Part H.2): superblock and its
`sparse_super` backups, the group descriptors, each group's bitmaps and zeroed inode table, and a
root directory — layout only, no journal and no `64bit`, so up to 16 TiB at 4 KiB blocks. `e2fsck
-fn` is its oracle, and this crate's reader reads what it writes. **Two callers**, with the same
parameters — 4 KiB blocks and an inode per 16 KiB:
- **`nxinstall`**, for the root it installs, through a window onto the disk it holds
  (`libfsserver::disk::PartitionIo`, `nxinstall`'s own until Part G);
- **`disk --format DEVICE ext4`** (Phase 6 Part G), on a partition or in the one partition of a
  whole removable disk, after zeroing the filesystem's first mebibyte so no older boot sector
  outlives it; the label up to 16 characters, `nitrox` by default. `check-storage` formats a blank
  stick so, and the host's `e2fsck -fn` finds the partition clean
  ([`storage.md`](storage.md) §8b).

## Journaling (jbd2) — deferred

ext4's crash-atomicity mechanism is the **jbd2 journal**, and it is entirely out of scope this
slice — *forced*, not merely chosen: the fixtures are `^has_journal`, so there is **no on-disk
journal to write or replay**. Per the generic contract, the kernel still orders data before
metadata (best-effort), but a crash mid-write may leave the image needing an offline `e2fsck`.

Full jbd2 journaling + **replay-on-mount** is its own later slice, which must first flip the
fixtures to `has_journal`. (FAT32 would have no analogue — journaling is squarely an
ext4-server concern, which is why it lives here and not in the data-path contract.)

## Slice staging

Each part builds on proven machinery and is independently verifiable.

- **Part A — design** ✅ (this doc + `filesystem-data-path.md` + `rsproto-block-ops.md`; a
  decision-log entry).
- **Part B — Model A read fill (no writes)** ✅. The `FileObject` read path is Model A: the
  file's `BlockRun` map is delivered inline in the lazy resolve reply (`OBJECT_KIND_FILE_BLOCKS`,
  which also transfers the device); a fault reads the page's block zero-copy from the device
  into the cache frame. **Verified by the existing read tests** (`current-generation`, the
  8-page `large.bin`) — the hardest new machinery de-risked before any write correctness.
  (The standalone `MapRange` op is deferred; the resolve reply carries the initial map.)
- **Part C — overwrite-in-place** ✅. `MAP_WRITE` mappings + `sys_file_sync` + kernel write
  IRPs to **existing** LBAs (`FileObject::writeback`). Writing back all resident pages is
  correct for overwrite; per-page dirty tracking is deferred.

  The original note here said "the fs-server stays read-only (an overwrite changes no
  metadata)". That was true of the *data* and wrong about the consequence: an overwrite
  changes `mtime`, and this is exactly the path on which the server cannot see it happen.
  A same-length rewrite goes from the page cache to the device with no resolve and no IPC,
  so before Slice C4 a file edited repeatedly in place still reported the timestamp of its
  last **size** change — usually its creation. Fixed by `File::Touch`: after a successful
  write-back of a Model A file, the kernel sends the server the file's id, and the
  server stamps `mtime` **from its own clock** (no timestamp on the wire — a writer does
  not choose the time its write appears to have happened). It carries no reply and is
  registered as nothing pending: the data is already durable, so a dropped notification
  costs a stale timestamp rather than a failed sync. Ordering still holds where it matters,
  because it enters the same endpoint ring as forwarded resolves — a subsequent lookup of
  that file is processed after it. To name the file at all, Model A's producer had to start
  holding the file's identity; it is used by nothing on the data path, which is the point.

  **By inode since administration Part C.1**, and by suffix before. The kernel keeps one object
  per file and writes a dirty one back at a sync or an unmount, long after the resolve that named
  it, and a rename may have handed that name to another file by then. The id is the inode number,
  which `map_file` reports and the block-file reply carries. `touch_file` stamps only a live
  regular file, since the id comes off the wire and a touch can trail the unlink of what it names.
- **Part D — file growth** ✅. `grow_file` (block-bitmap allocation + extent-tree extension +
  inode size/block-count) — **`e2fsck`-verified**. Triggered by grow-on-resolve
  (`RESOLVE_GROW` + `sys_file_grow`): the server grows the file, then replies its map; the
  client writes the new region + syncs. The fs-server now holds a read-write device handle.

  **A grow zeroes, on the device, everything it adds**: the old last block past the old size, and
  every block it allocates (administration Part C.1). The kernel fills a page it does not hold from
  the blocks the map names, so the device is what a reader of the new range sees. Until then it saw
  whatever those blocks last held. After a truncate and a grow, that was the file's own cut bytes,
  since the allocator's goal is the block the truncate just freed. On any grow, it could be a
  deleted file's contents. A boot showed it on a freshly created file's blocks. The cost is a write
  per block grown, which a writer then overwrites; unwritten extents would avoid it, and nothing yet
  needs to. **The read that followed it is gone** (Phase 6 Part H.4): the grow's reply marks those
  blocks `UNWRITTEN`, so the kernel fills them as zero without reading the zeroes back. The zeroes
  stay, so a crash before the write-back reads zero and never another file's bytes, until a
  measurement asks for unwritten extents ([`phase-6-usb.md`](../planning/phase-6-usb.md) § *H.4's
  second fix, in detail*).

  **Blocks are allocated and freed a run at a time** (Phase 6 Part H.4, 2026-10-09). `alloc_run`
  takes as many free blocks in a row as a grow wants from one pass over a group's bitmap, and writes
  the bitmap, the descriptor and the superblock once for the run; `free_run` clears a run a group at
  a time, counting only bits that were set. One block at a time each was five reads and three writes
  — the descriptor's and the superblock's writes are part of a block, which `Disk` reads first — and
  on the laptop that was eight of a copy's twelve commands a block and all of a removal's. Where a
  run starts and ends is where the one-block allocator, called again from each block's end, put each
  block; `alloc_block` and `free_block` are runs of one. Held by a count:
  `an_eight_mebibyte_grow_and_its_removal_cost_commands_a_run_not_a_block`, through a test device
  that charges commands as `Disk` sends them.
- **Part E — file creation** ✅. `create_file` (inode allocation via the inode bitmap +
  `ext4_dir_entry_2` insertion by splitting an existing entry's slack in the parent
  directory, then inode init as an extents regular file) — **`e2fsck`-verified**. Triggered
  by create-on-resolve (`RESOLVE_CREATE | RESOLVE_GROW` + `sys_file_create`): the server
  creates the file (idempotent — an existing name returns its inode), grows it to the
  requested size, then replies its map; the client writes + syncs, and a subsequent plain
  `sys_ns_lookup` of the new path resolves (proving the directory entry is on disk).
  **Allocation reaches every block group** since Phase 5 Part H.2 (2026-09-17): `alloc_run`
  scans outward from the goal's group and `alloc_inode` takes the first group with a free
  inode, each clamped to the last group's short tail. It was group 0 only until the installer
  made a root the size of a 931 GiB disk and found it held about 112 MiB. Directory growth — a
  new directory block when the last has no slack — landed 2026-07-29.

**Deferred**: jbd2 journaling + replay-on-mount (needs `has_journal` fixtures); `metadata_csum`
checksums; a periodic writeback daemon; per-page dirty tracking; read-ahead / clustered fill; the
standalone `MapRange`/`AllocRange` ops (the resolve reply carries the map today); the fs-server
open-file cookie; and inode **locality** — a new inode goes in the first group with a free one
rather than near its parent, which is correct and worse for a seek pattern.

Extent-tree splitting and index nodes left this list with Phase 6 Part H.1 (2026-10-08) — §
*The extent tree*.

**Five things this list claimed were deferred and were not** (corrected 2026-09-17):
truncate (2026-07-24), delete and rename, new-directory-block growth (2026-07-29), and
cross-group allocation (above). The crate's own `CLAUDE.md` had recorded each as it landed;
this list did not, which is the drift a "Deferred" section collects if nothing sweeps it
when an item closes.

## Verification

- A **writable** ext4 partition in the boot disk (root may stay read-only-bound; add a
  RW-bound mount, or make root RW). Host-side, extend the `mke2fs` fixture with a writable image.
- Per part: existing read tests unchanged (B); overwrite → re-read (C); append → re-read + size
  (D); create → resolve + read (E).
- **Durability across remount**: re-read after tearing down + re-spawning the fs-server (or a
  reboot) to prove the change hit disk, not just the page cache.
- **Offline `e2fsck -fn`** on the resulting image in `xtask` (clean, modulo the no-journal
  posture) as an on-disk-correctness check.

## See also

- `docs/architecture/filesystem-data-path.md` — the generic contract this realizes
- `docs/spec/rsproto-block-ops.md` — the Model A wire ops (`MapRange`/`AllocRange`)
- `userspace/fs-server-ext4/` + its `CLAUDE.md` — the reader RW extends
- `docs/decision-log.md` 2026-06-25 — Model A vs Model B
