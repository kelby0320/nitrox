# Filesystem data path (kernel ↔ fs-server contract)

**Status:** Implemented — the kernel page-cache/mapping path between fs-server and client,
with deferrals (a periodic writeback daemon, per-page dirty tracking) marked inline. Verified
2026-08-05; the writeback triggers corrected 2026-09-22, and a claim of dirty tracking that the
code never had corrected 2026-09-24. **One object per file, dirty objects kept until a sync, and
`sys_ns_sync`** built by administration Part C.1 (2026-09-24, § *One object per file*), with
`File::Forget` for a file its server frees.

How file **data** moves between a userspace filesystem server, the kernel page cache, and
the block device. This contract is **filesystem-agnostic**: `fs-server-ext4` is the first
implementer, but FAT32, or any future block filesystem, speaks the same protocol and uses
the same kernel interface. Nothing here names ext4 concepts — a filesystem's *own*
structures (ext4 extents/bitmaps, a FAT cluster chain, …) live behind this contract, in that
server's own document (e.g. `docs/architecture/ext4-fs-server-rw.md`).

Path *resolution* (name → file) is the separate `Namespace::Resolve` forwarding contract
(`docs/architecture/namespace-and-resource-servers.md`). This document is only about a
resolved file's **bytes**. (Small *synthetic* content — `/dev/log`, `current-generation` — is
served as an eager `MemoryObject` snapshot at resolve time, a different path this contract
does not cover.)

## Two data paths, one per filesystem class

There are two ways a `FileObject` fills (and flushes) its pages. They are **not competing
alternatives** — which one a file uses is determined by **what backs the filesystem**, not by
a performance tradeoff. Picking wrongly isn't slower, it's impossible.

### Model A — block filesystems (ext4, FAT32, …)

The file **is a sequence of device blocks**, so the fs-server hands the kernel a map of those
blocks and steps out of the data path. It becomes a **metadata / allocation oracle**: it says
*where a file's bytes live on the device* (`MapRange`) and *allocates more blocks on growth*
(`AllocRange`), but **never reads or writes file data**. The kernel owns the file-data path
end to end — it reads/writes the file's device blocks **zero-copy** straight into/out of cache
pages via block IRPs (the cache frame is the DMA target): no per-fault userspace round-trip,
no copy, and the block layer sees real disk layout (merge / read-ahead). For a block
filesystem there is no choice to weigh — Model A strictly dominates shipping bytes — so it is
**always** Model A. **This slice builds Model A**, and ext4 is its first (only) implementer.

### Model B — non-block filesystems (network, synthetic, transforming/overlay)

The backing store has **no device-block map**: the "file" is remote bytes, computed content,
or a transform of another file. The server *cannot* produce a block map, so it serves the
bytes itself — **`File::ReadRange(file, byte-range) → bytes`** — and the kernel copies them
into the cache page. The per-miss copy + IPC round-trip is inherent here; Model A isn't
"slower," it's **unavailable** (there are no LBAs to hand over). Model B ships today and needs
no new machinery in this slice. **No non-block fs-server exists yet** — but the `FileObject`
producer (below) is the shared seam the first one (a netfs, a `/proc`-like server) plugs into.

The rest of this document specifies **Model A** — the block-filesystem path this slice builds.
Model B is the existing `ReadRange` path, retained for the non-block case.

## The Model A protocol (filesystem-neutral)

Two operations — the fs-server's oracle surface. They speak only in **device block runs**,
never in any filesystem's internal structures. A **`BlockRun`** is a contiguous mapping:

```
BlockRun {
    file_block:   u64,   // starting block offset within the file
    device_lba:   u64,   // starting logical block address on the device (0 = hole → zero-fill)
    length:       u32,   // number of contiguous blocks
}
```

- **`MapRange(file, block-range) → [BlockRun]`** — translate a range of the file to the device
  blocks that currently back it (holes reported as `device_lba = 0`). **Read-only**, no side
  effects. A server produces these from whatever it keeps internally (ext4 walks its extent
  tree; FAT32 walks its cluster chain) — the wire result is the same neutral run list.
- **`AllocRange(file, block-range) → [BlockRun]`** — allocate device blocks to back a range
  that is currently a hole / past EOF, and return their runs, for a flush that grows the file.
  **Mutates the filesystem's metadata** (its allocator + its block map + the inode/dir-entry
  equivalent). **Deferred, and nothing calls it**: no `AllocRange` exists in the kernel or any
  server, and `writeback` skips a page over a hole rather than allocating for it — a file grows
  through `sys_file_grow`'s resolve, whose reply carries the new blocks. The op is designed for a
  write-back that allocates, which dirty tracking would make possible (the Part C review,
  2026-09-24, found this line describing it as current).

Naming is deliberately neutral: `MapRange`/`AllocRange`/`BlockRun`, not "extents." These are
new **`Block`-category** ops (`0x03xx`) in the RS wire format (`docs/spec/rsproto-block-ops.md`).

## The kernel interface (filesystem-neutral)

Also fs-agnostic — the kernel never knows which filesystem backs a file.

- **`FileObject` producer** (`kernel/src/object/file_object.rs`): the seam that selects the
  data path. The **Model A** producer carries a **device reference, the file's identity, and its
  `BlockRun` map** — the map arrives in the resolve reply (`OBJECT_KIND_FILE_BLOCKS`), and is
  kept under the object's lock, since a size change replaces it. A page fault translates the
  page's `file_block → device_lba` and issues a block **read** IRP into the cache frame. The
  **Model B** producer carries `{server, file-suffix}` and fills via `ReadRange` — the variant a
  non-block fs-server uses. A file has one producer, fixed by its filesystem's class.
- **Writable mappings, and dirty per object**: `sys_memory_map` grants `MAP_WRITE` on a
  `FileObject` when requested and permitted, and a store faults in a writable PTE. The object
  counts its writable mappings, and one mapped writable is **dirty** (§ *One object per file*).
  There is no per-page dirty bit (`TODO(page-dirty-tracking)`). This bullet said a store "marks
  the `CachePage` dirty" until 2026-09-24, when the administration Part C detail pass found no such
  state in the code.
- **Writeback**: `FileObject::writeback` flushes **every resident page** by a block **write** IRP
  from the cache frame to its `device_lba`, dirty or not, since it cannot tell. A page over a hole
  is skipped: growth goes through `sys_file_grow`'s resolve, not writeback. **Two triggers:
  `sys_file_sync`** on one file, an `msync`-style syscall, and **`sys_ns_sync`** on every dirty
  file of a mount. Unmapping a `MAP_WRITE` VMA does *not* write back. After it, the kernel sends
  the server `File::Touch` with the file's id, since an in-place write never reaches the server
  any other way, and its `mtime` would not move. (This bullet said unmap was a trigger until
  2026-09-22; it never was — PR #326 review.)
- **Shared device by capability**: the block device (`/dev/blk/N`, a kernel `DeviceNode`) is
  reachable by two handles — the fs-server keeps a read-write handle for **metadata** I/O, and
  the `FileObject` producer references the same device so the kernel can IRP **file data**
  directly. Both are legitimate capabilities to one disk.

## One object per file

*(Administration Part C.1, 2026-09-24.)* **A registration keeps one `FileObject` per file**, keyed
by an id the server gives the file in its resolve reply: its inode number, for ext4
(`rsproto-namespace-ops.md` § *The `FILE_BLOCKS` body*). Every resolve of the file shares that
object, so two processes mapping one file read each other's writes without a sync, and a sync or
an unmount can enumerate everything a filesystem has handed out. Before, each lookup built its own
object: a file mapped by two processes had two caches, and neither saw the other's writes until a
sync and a fresh resolve.

- **The cache is an index, not an owner.** It holds each object weakly
  (`UserspaceServerReg::files`), and an object leaves it when it drops. So a clean file leaves the
  cache with its last user, and the cache holds what is in use plus what is dirty, never the whole
  disk. An id of `0` means uncached: such a file gets an object of its own.
- **A dirty object holds a reference to itself.** Dirty means mapped writable since the last
  write-back that *began* with no writable mapping and saw none made during it. A mapping present
  at the start could write after its page's IRP read the frame, then go before the end. Until such
  a write-back, the object stays alive and in the cache, however soon its users let go, and the
  next resolve of the file finds it with its pages. **A writer that exits without syncing loses
  nothing until the machine stops**: `sys_ns_sync` writes it, and **the storage service's unmount
  calls it** before the filesystem is marked clean (administration Part C.5c,
  [`storage.md`](storage.md) §8). What is still owed is `init`'s own mounts, which only Part E's
  shutdown will unmount. Since a sync that begins with a writable mapping in place cannot clean,
  `libfs` and `nxsh` unmap before they sync. Otherwise every file they write would stay pinned
  until an unmount.
- **What still holds a file is countable**: `sys_ns_held` counts a registration's live cached
  objects, after the finished IRPs have let go of theirs. Asked after a sync, a non-zero answer is
  someone's handle or mapping, which is what an unmount is refused on.
- **A grow, create or truncate resizes the one object in place.** It is a resolve, and its reply
  carries the new size and map. What a page says stays honest across it. Everything past the
  smaller of the old and new sizes leaves the index, so no fault finds it and no write-back writes
  it, and the page holding that edge is zeroed past it. A mapping that faulted a retired page in
  keeps a valid frame, which is freed with the object (`TODO(retired-frames)`). The server's half
  is that **a grow zeroes on the device what it adds** (`ext4-fs-server-rw.md`), because a page no
  one holds fills from the device. Together, a truncate and a grow read zero over the regrown
  range, a whole page and a partial tail.
- **A second faulter of a page being filled waits on that fill's `PendingOperation`.** The page
  carries it while loading. Whoever wakes first settles the page, matched by the PO rather than
  the index, so a fill retired mid-flight settles nothing else. Before, the second faulter
  `yield_now`ed until the page was ready. That was unreachable while each resolve had its own
  object. Under one object per file every process running one binary shares its image, and the
  yield became a spin: the fault handler runs with interrupts off and `yield_now` returns at once
  when nothing else is ready, so the CPU acknowledged no TLB shootdown and the machine stopped.
  A failed fill fails every faulter waiting on it and leaves the page out of the cache.
- **A server frees a file only after the kernel has forgotten it** (`File::Forget`, Part C.1b).
  The last name's unlink, or a rename that replaces a file, sends the kernel the file's id and
  waits for the answer before freeing a block. The kernel marks the object's cache entry
  forgotten, which puts it out of reach of any resolve or sync. It also stops the object from
  starting device I/O and releases its dirty pin. It answers once the last IRP of the file in
  flight has ended, reads included, so a fill queued before the `Forget` cannot read a block
  after it has become another file's. For that each object counts its IRPs in flight, and **a
  write-back decides each page as it issues its IRP**, under the object's lock, rather than from a
  snapshot taken at its start. The same per-page decision means a truncate's resize governs every
  page after it. Only an IRP already in flight when a truncate lands can reach a block it freed
  (`TODO(truncate-inflight-writeback)`). Until the answer the entry stays, so a second `Forget` of
  the id waits on the same one (`UserspaceServerReg::forget_file`).

## Consistency ordering (filesystem-neutral)

On a growth flush the kernel writes the **data** block (IRP) before the fs-server publishes
the **metadata** that references it (its allocator/block-map/inode update) — never point
durable metadata at not-yet-written data. This poor-man's ordered mode is the neutral
guarantee.

**Atomicity across a crash is per-filesystem, not part of this contract.** Whether a mid-write
crash is recoverable depends on the server's own journaling/logging — ext4 has jbd2, FAT32 has
none. That machinery lives in each server's document, not here; this contract only fixes the
data-before-metadata *ordering*.

## Rejected (block filesystems): shipping dirty bytes to the server

For a Model A (block) filesystem, symmetry might suggest a `WriteRange(file, bytes)` where the
kernel ships dirty bytes to the server to write. Rejected: it would put **file data back
through the fs-server** for a store whose blocks the kernel can address directly, costing a
copy + IPC round-trip per flush for nothing. Under Model A the server writes only metadata;
the kernel writes all file data. (This says nothing about Model B — a non-block server serves
and would write its own bytes, because it has no blocks for the kernel to address.)

## See also

- `docs/architecture/ext4-fs-server-rw.md` — the first implementer (ext4's realization)
- `docs/spec/rsproto-block-ops.md` — the Model A wire ops (`MapRange`/`AllocRange`, `BlockRun`)
- `docs/spec/rsproto-file-ops.md` — the Model B wire op (`ReadRange`)
- `docs/decision-log.md` 2026-06-25 — Model A vs Model B
- `kernel/src/object/file_object.rs` — the `FileObject` producer seam
- `docs/architecture/drivers-and-irps.md` — the IRP / block path
