# userspace/fs-server-ext4/CLAUDE.md

Constraints for the ext4 filesystem server. Loaded when working under
`userspace/fs-server-ext4/`.

## What this is

The first **userspace resource server** (Phase 2 slice 7): a process that serves an
ext4 filesystem over the block device through the resource-server protocol, reached
transparently via the namespace (the kernel forwards `sys_ns_lookup` to it). It is
**not** in the kernel — filesystems are userspace processes (`CLAUDE.md` core rule).

**Now read-write (Phase 3, Model A).** The server is a metadata / block-allocation
oracle: it maps a file's blocks to device LBAs and allocates more on growth, while the
**kernel** owns the file-data path (reads + writes go zero-copy against the device — the
server never touches file data). See `docs/architecture/filesystem-data-path.md` (the
generic contract) and `docs/architecture/ext4-fs-server-rw.md` (this server's write path).

## Structure

- **`src/ext4.rs` + `src/lib.rs` — the parser + write path.** Pure logic behind the
  `BlockReader` (`read_at`) and `BlockWriter` (`write_at`) traits, so it is 100%
  host-tested against an `mke2fs` fixture — including `e2fsck` on the mutated image
  after `grow_file`. `no_std`, **no `alloc`**: buffer-based (`read_file` into a
  caller buffer; `map_file`/`grow_file` into caller `BlockRun` slices); parsing +
  mutation use bounded stack scratch (≤ one 4 KiB block). Do not pull in `alloc` here.
  Write path: `map_file` (extent → block runs), `grow_file` (block-bitmap allocation +
  extent-tree extension + inode update).
- **`src/mkfs.rs` — making an empty filesystem** (Phase 5 Part H.2). Superblock and backups,
  the group-descriptor table, per-group bitmaps and inode tables, a root directory. Same rules
  as `ext4.rs`: `no_std`, **no `alloc`**, bounded stack scratch of one block — which is why the
  descriptor table is written a block at a time rather than built whole (a 931 GiB disk needs
  59 blocks of it, and the first version used one). Host-tested with `e2fsck -fn` **and**
  `dumpe2fs` as oracles, and read back by this crate's own parser: `e2fsck` would accept a
  filesystem the reader cannot walk, and the reader would accept its own mistakes.
- **`src/volume.rs` — ext4 as a `libfsserver::Volume`** (Phase 6 Part E.1): the wrapper the
  server loop calls, delegating each method to `ext4.rs`, and the host tests of the resolve core
  (`libfsserver::serve`) driven through it against the `mke2fs` fixture.
- **`src/main.rs` — the server `[[bin]]`**: a `_start` that takes the device from
  `libfsserver::server::bootstrap` and hands `server::run` an `Ext4` over it — or over the
  `ReadOnly` a read-only mount is served through. **The protocol is `libfsserver`'s** since Part
  E.1, which serves `fs-server-fat` the same way: the setup message, `check_device` and a refusal
  in place of Ready, the forwarding channel and `Meta::Ready`, the serve loop (a Model A resolve
  replying the file's `BlockRun` map and a device handle; `RESOLVE_GROW` and `RESOLVE_CREATE`
  growing or creating first; a `RESOLVE_RENAME` handled **before** the directory-session path,
  which infers "directory open" from the suffix naming a directory), `File::Forget` before an
  unlinked or replaced file's `release_inode`, and the control channel's `Meta::Unmount`, which
  `mark_clean`s, replies and exits. See `userspace/libfsserver/CLAUDE.md`. The device is
  `libfsserver::disk::Disk`: a 4 KiB block per `sys_io_submit` into a one-page scratch
  `MemoryObject`, a write smaller than a block reading the block first (this said
  "sector-at-a-time" until 2026-10-06). **Alloc-free** — fixed `.bss` buffers, no
  `#[global_allocator]`.

## Scope

Implements: superblock (`0xEF53`), block-group descriptors, inodes, the **extent
tree** (`0xF30A`, walk + in-place extend), a linear `ext4_dir_entry_2` directory walk
+ **insert** (split an entry's slack), path resolution to a regular file, block-bitmap
allocation + free-count updates, **inode-bitmap allocation** (new-file creation), and
file growth. **Reject / skip** (return `FsError::Unsupported`/`Corrupt`): the journal,
bigalloc, inline-data inodes, 64-bit block numbers, ≥ 8 KiB blocks, xattrs, ACLs,
symlinks, checksums. htree directories need no special handling (the linear walk is
backward-compatible).

**Write path deferred** (see `docs/architecture/ext4-fs-server-rw.md`, and
`docs/rationale/deferred-decisions.md` — every item here is mirrored there, because a
deferral recorded only in a crate `CLAUDE.md` is one nobody reviews): extent-tree splitting
/ index nodes (depth > 0), `metadata_csum` checksums, and jbd2 journaling + replay (the
fixtures are `^has_journal`). Overwrite is data-only (no metadata change) and is the kernel's writeback;
the server allocates on growth + creation but never touches file data (Model A).

**Now implemented** (was deferred): **cross-group allocation** (2026-09-17) — both
allocators walk every block group, so a filesystem is as large as the disk rather than as
large as group 0; the trigger was the installer making a 931 GiB root that held 112 MiB.
**Test fixtures default to a single group** (4,096 blocks against 8,192 per group), which is
why nothing caught it: use `fixture_blocks` when what you are changing can run out of one.
Also truncate (2026-07-24), rename, delete (in two halves since 2026-09-24: `unlink_at` and a
replacing `rename_path` return the inode, `release_inode` frees it), and
**growing a full directory** (2026-07-29) — a directory whose blocks are all full gains
another, so `mkdir`/`touch`/`copy` no longer stop at one block's worth of entries.

## This crate's library has a second consumer

`nxinstall` links it (`userspace/nxinstall/Cargo.toml`) and calls `mkfs::format`, `create_file`,
`mkdir_at`, `grow_file` and `map_range` directly against a partition of a raw disk — the
installer makes a filesystem and fills it without anything being mounted. **So the lib half is a
format library, not only this server's insides**, and a change to a public signature here breaks
a program that is not in this directory. The bin half (`main.rs`) is still the server and is
nobody else's business.

## Capability discipline

The server receives only what it needs at spawn: a **read-write block-device
handle** (`READ | WRITE`, for metadata I/O; it hands a `DUPLICATE`d copy to the kernel
for the Model A data path) and a **control channel** (for the Ready handshake). It never
holds `BIND_NAMESPACE` — the supervisor (init) binds its endpoint. See
`docs/rationale/why-supervisor-registration.md`.

## Forbidden

- `alloc` in the library half (`ext4.rs`, `mkfs.rs`, `volume.rs`, `lib.rs`) — buffer-based only.
  The list names every module because a rules file that enumerates three of four is one that
  permits the fourth by omission (PR #310 review).
- Touching **file data** — the kernel owns the data path (Model A); the server writes
  only metadata (bitmaps, extent tree, inode, superblock). **One exception, deliberate**
  (administration Part C.1): `grow_file` writes zeroes over what it adds — the old last block's
  tail and every block it allocates — since a page the kernel does not hold fills from the device,
  and a new range must read as zero rather than as whatever those blocks last held. It writes no
  byte a file ever held.
- Freeing a file's blocks before the kernel has answered `File::Forget` for it.
- A mutating path that bypasses the reader it was handed — writing the device another way would
  step around a read-only mount's `ReadOnly`, which is the whole of its enforcement.
- Binding itself into a namespace, or holding `BIND_NAMESPACE`.
- Trusting on-disk structures without bounds-checking (a malformed image must
  yield `FsError`, never a panic or OOB read).
