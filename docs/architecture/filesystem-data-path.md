# Filesystem data path (kernel ↔ fs-server contract)

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
  equivalent). The kernel calls this only when flushing dirty pages with no backing block.

Naming is deliberately neutral: `MapRange`/`AllocRange`/`BlockRun`, not "extents." These are
new **`Block`-category** ops (`0x03xx`) in the RS wire format (`docs/spec/rsproto-block-ops.md`).

## The kernel interface (filesystem-neutral)

Also fs-agnostic — the kernel never knows which filesystem backs a file.

- **`FileObject` producer** (`kernel/src/object/file_object.rs`): the seam that selects the
  data path. The **Model A** producer carries a **device reference + the file's `BlockRun`
  map** (fetched via `MapRange`, cached on the object); a page fault translates the page's
  `file_block → device_lba` and issues a block **read** IRP into the cache frame. The **Model
  B** producer carries `{server, file-suffix}` and fills via `ReadRange` — the variant a
  non-block fs-server uses. A file has one producer, fixed by its filesystem's class.
- **Writable mappings + dirty tracking**: `sys_memory_map` grants `MAP_WRITE` on a `FileObject`
  when requested and permitted; a store faults in a writable PTE and marks the `CachePage`
  **dirty**.
- **Writeback**: a dirty page is flushed by a block **write** IRP from the cache frame to its
  `device_lba` (via `AllocRange` first if it has no backing block), then marked clean. Triggers:
  **`sys_file_sync`** (an `msync`-style syscall) and **unmap / teardown** of a `MAP_WRITE` VMA.
  A periodic writeback daemon is deferred.
- **Shared device by capability**: the block device (`/dev/blk/N`, a kernel `DeviceNode`) is
  reachable by two handles — the fs-server keeps a read-write handle for **metadata** I/O, and
  the `FileObject` producer references the same device so the kernel can IRP **file data**
  directly. Both are legitimate capabilities to one disk.

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
- `docs/history/decision-log.md` 2026-06-25 — Model A vs Model B
- `kernel/src/object/file_object.rs` — the `FileObject` producer seam
- `docs/architecture/drivers-and-irps.md` — the IRP / block path
