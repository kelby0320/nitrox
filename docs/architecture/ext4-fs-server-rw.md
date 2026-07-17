# fs-server-ext4 read-write

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
the **extent tree** (`ext4.rs::extent_find`), and directory entries. It **never reads the
block or inode bitmaps** and mutates nothing. Fixtures are built journalless and
checksumless — `mke2fs … -O ^has_journal,^64bit,^metadata_csum,^resize_inode` — which bounds
this slice (below). It serves `File::ReadRange` today (Model B); this slice moves it to
Model A.

## ext4's realization of the Model A ops

- **`MapRange` ← the extent tree.** ext4 already stores a file as extents (`ext4_extent` leaves
  under an `ext4_extent_header`, walked by `extent_find`). Producing a `BlockRun` list for a
  range is that walk, re-exposed as the op: each ext4 extent → one `BlockRun`
  (`file_block = ee_block`, `device_lba = 48-bit start`, `length = ee_len`); a sparse hole →
  `device_lba = 0`. Read-only; the reader's logic, lifted into a served op. *(A FAT32 server
  would instead produce the same `BlockRun`s from its cluster chain — the wire result is
  identical; that is the point of the neutral contract.)*
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
| Extent tree | walk (`extent_find`) | insert / split on growth |
| Inode | read mode/size/flags/`i_block` | update size (off 4/108), mtime/ctime, block count |
| Directory block | linear lookup (`dir_lookup`) | insert an `ext4_dir_entry_2` (split `rec_len` / new dir block) |
| Group desc + superblock | read a few fields | update free-block / free-inode counts |

**No checksums.** The fixtures are `^metadata_csum`, so there is nothing to maintain. Enabling
`metadata_csum` (group-desc / inode / extent / dir / bitmap checksums across every write above)
is a feature-gated later addition.

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

- **Part A — design** (this doc + `filesystem-data-path.md` + spec updates to
  a new `rsproto-block-ops.md` for `MapRange`/`AllocRange`; a decision-log entry).
- **Part B — Model A read fill (no writes).** Move the `FileObject` read path from Model B
  (`ReadRange` IPC) to Model A: the `MapRange` op (ext4 extent walk) + the device wired to the
  kernel + the kernel block-read-into-cache-page path. **Verified by the existing read tests**
  (files still read correctly, now zero-copy) — de-risks the hardest new machinery (kernel
  block I/O + block map + device wiring) before any write correctness is at stake.
- **Part C — overwrite-in-place.** `MAP_WRITE` + `Dirty` `CachePage` + `sys_file_sync` +
  writeback-on-unmap + kernel write IRPs to **existing** LBAs (`MapRange` only, no allocation).
  Milestone: overwrite a file's bytes → persists.
- **Part D — file growth.** `AllocRange` (block bitmap + extent insertion) + inode size/mtime.
  Milestone: append past EOF → persists.
- **Part E — file creation.** Inode allocation (inode bitmap) + directory entry insertion.
  Milestone: create a new file and write it.

**Deferred**: jbd2 journaling + replay-on-mount (needs `has_journal` fixtures); `metadata_csum`
checksums; a periodic writeback daemon; sub-page dirty ranges; truncate / delete / rename;
read-ahead / clustered fill; the fs-server open-file cookie (a stateful handle anchoring
writeback, retiring the stateless per-range re-send).

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
- `docs/history/decision-log.md` 2026-06-25 — Model A vs Model B
