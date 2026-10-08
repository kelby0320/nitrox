# userspace/fs-server-fat/CLAUDE.md

Constraints for the FAT filesystem server. Loaded when working under `userspace/fs-server-fat/`.

## What this is

**The second filesystem server** (Phase 6 Part E): FAT12, FAT16 and FAT32, read-write, with long
names, for USB sticks. A library and a binary, as `fs-server-ext4` is. **The protocol is
`libfsserver`'s** — the bootstrap, the loop, directory sessions, `File::Forget`, `File::Touch` and
`Meta::Unmount` — so this crate is FAT and a `_start`. See
`docs/architecture/fat-fs-server.md` for what it does, and `userspace/libfsserver/CLAUDE.md` for the
loop.

## Structure

- **`src/bpb.rs`** — the boot sector: the geometry, the type (FAT32 by its boot sector, FAT12 and
  FAT16 by count), and `servable`, **the server's rule**: clusters of at least a page. The library
  itself works at any cluster size, which is what lets its tests run on small images.
- **`src/table.rs`** — the FAT through a cache of its sectors. Reads go through `slot`, which never
  evicts a dirty sector; **a write path's reads go through `slot_rw`** (`next_rw`), which may, after
  writing it out. A walk on the read path with every slot dirty is refused `Io`, not served stale.
- **`src/names.rs`**, **`src/dir.rs`**, **`src/time.rs`** — names, directory entries, times as UTC.
- **`src/volume.rs`** — `Fat`: every operation, and its `Volume` impl. **`change` wraps every
  mutation**: read-only refused first, the cache flushed at the end, after a failure too.
- **`src/mkfs.rs`** (Phase 6 Part G.2) — making an empty FAT, what `disk --format` writes: FAT16
  or FAT32 by size, **never with clusters under a page**, so the server serves everything it makes.
  The boot sector is written last. Its tests hold each type and cluster size at its boundary's
  neighbours to `fsck.fat -n`, mtools and `Fat`.
- **`src/main.rs`** — the `[[bin]]`: `server::bootstrap` with `SectorDisk::new`, then `server::run`
  over a `Fat`, through `ReadOnly` for a read-only mount. Alloc-free.

## Rules

- **`no_std`, no `alloc`.** Every buffer is the caller's or bounded on the stack, as in ext4's
  library. The binary has no `#[global_allocator]`.
- **A FAT is anyone's bytes.** Every field read off one is checked before it is used; a malformed
  one is an `FsError`, never a panic, and every chain walk is bounded by the volume's cluster
  count. `garbage_in_any_structure_is_an_error_never_a_panic` holds that as a class: a new reader
  of on-disk bytes belongs in what it exercises. (It missed the first such panic until it mutated
  real entries rather than scattering bytes: PR #365's review found it.)
- **Data, then the chain, then the entry.** A grow zeroes, writes the FAT, then the entry; a
  truncate writes the entry, then the cut, then frees; a removal frees nothing until `release`,
  which the loop calls after `File::Forget`. Changing that order is changing what a crash can do:
  read `fat-fs-server.md` §5 first. **`crash_tests` crashes each change after every write** and
  checks what is left; every flush in the write path is one its removal fails, so a flush no test
  needs is a question to answer, not one to leave (PR #365 review).
- **The write path batches** (`TODO(fs-throughput)`): allocation in one pass, zeroes in 64 KiB
  writes, the FAT's dirty sectors written together. `a_one_mebibyte_grow_costs_a_handful_of_writes`
  counts it; a change that makes it slower should say so with a number.
- **A filesystem found dirty stays dirty** at its unmount. Nothing here repairs one.
- **Tests run against the host's tools**: `mformat`, `mkfs.fat`, `mcopy` and `mdir` build and read
  the images, and **`fsck.fat -n` must find every image clean after every change**. A test of a
  writer that only reads back through this crate tests the writer against itself.

## Not built

Clusters under a page, files in more than 64 runs, long names over 255 bytes of UTF-8, and the
`File::Touch` table's bound: each is in `docs/rationale/deferred-decisions.md` with its trigger
(`fat-small-clusters`, `map-range`, `fat-long-utf8-names`, `fat-touch-table`). Sectors other than
512 bytes, attributes beyond read-only and directory, and repair are not either.
