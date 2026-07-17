# Resource Server Protocol — Block operations

The `Block` category (`op = 0x03xx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). These operations back the **Model A**
data path: the fs-server is a metadata/allocation oracle that maps a file's blocks to device
LBAs and allocates more on growth, while the **kernel** owns file-data I/O directly against the
block device. See [filesystem-data-path.md](../architecture/filesystem-data-path.md) for the
contract and [ext4-fs-server-rw.md](../architecture/ext4-fs-server-rw.md) for the first
implementer.

**Status:** Pre-stabilization. Introduced with the fs-server-ext4 read-write slice. A
**kernel↔server ABI** — the kernel hand-codes the request/reply (`kernel/src/rsproto.rs`);
`librsproto` (`userspace/librsproto/src/block.rs`) carries the userspace mirror.

**Filesystem-neutral.** These ops speak only in **device block runs** — never in any
filesystem's internal structures. ext4 produces runs by walking/inserting its extent tree;
a FAT32 server would produce the *same* runs from its cluster chain. The wire format makes no
filesystem assumptions.

## `BlockRun`

The unit of both replies: one contiguous mapping from a file's blocks to the device.

```rust
#[repr(C, packed)]
pub struct BlockRun {
    pub file_block:  u64,  // offset 0  — starting block offset within the file
    pub device_lba:  u64,  // offset 8  — starting device LBA (0 = hole: not allocated)
    pub length:      u32,  // offset 16 — number of contiguous blocks
    pub flags:       u32,  // offset 20 — see below
}
```

Wire length: **24 bytes**. `flags`:

| bit | name | meaning |
|---|---|---|
| 0 | `UNWRITTEN` | allocated but never written — reads as zero; the kernel zero-fills the page rather than reading the device |

`device_lba = 0` marks a **hole** (unallocated); the kernel zero-fills such a page. (A hole and
an `UNWRITTEN` run both read as zero; they differ only in whether backing blocks exist.)

"Block" here is the **filesystem block size** (from the superblock), which the kernel learns at
mount; it need not equal the device sector size (the kernel scales LBAs accordingly).

## MapRange (`op = 0x0300`) — read-only

Translate a range of a file's blocks to the device blocks that currently back it. **No side
effects.** The kernel uses this to fill reads (Model A) and to locate existing blocks when
flushing an overwrite.

### Request body

```rust
#[repr(C, packed)]
pub struct MapRangeRequest {
    pub start_block: u64,  // offset 0  — first file block
    pub block_count: u32,  // offset 8  — blocks to map
    pub suffix_len:  u16,  // offset 12 — length of the path suffix
    pub _reserved:   u16,  // offset 14
    // offset 16: suffix bytes (suffix_len) — the file path, no leading '/'
}
```

Fixed prefix: **16 bytes**, then `suffix_len` suffix bytes. `handle_count = 0`. The file is
re-identified statelessly by `suffix` (as `File::ReadRange` does).

### Reply body (success)

```rust
#[repr(C, packed)]
pub struct MapRangeReply {
    pub run_count: u32,  // offset 0 — number of BlockRun that follow
    pub _reserved: u32,  // offset 4
    // offset 8: run_count × BlockRun (24 bytes each)
}
```

The runs cover `[start_block, start_block + block_count)` in order (holes included as
`device_lba = 0` runs). `handle_count = 0` — runs are inline (no data rides in the message).
A range whose run list would exceed the payload is a short reply: the runs cover a prefix of
the request and the kernel re-requests from the first uncovered block.

## AllocRange (`op = 0x0301`) — mutating

Allocate device blocks to back a range that is currently a hole / past EOF, insert them into
the file, and return their runs. The kernel calls this only when flushing dirty pages that
have no backing block. **Mutates filesystem metadata** (the allocator + the file's block map +
inode size/mtime). Per the data-path contract, the kernel writes the data blocks (write IRP)
before this allocation's metadata is made durable.

### Request body

Identical layout to `MapRangeRequest` — `{start_block, block_count, suffix_len, _reserved,
suffix}` — naming the file range to allocate.

### Reply body (success)

Identical layout to `MapRangeReply` — the newly-allocated runs (each now with a non-zero
`device_lba`; `UNWRITTEN` until the kernel writes them). `handle_count = 0`.

## Error reply

Flagged `RS_FLAG_REPLY | RS_FLAG_ERROR`; the body is the standard `ErrorBody` (12-byte prefix;
see the wire-format spec). `AllocRange` reports `KError::NoSpace` when the filesystem is full.
The kernel fails the fault / flush with the carried `KError`.

## Versioning

`Block` is a new category (minor version bump per the wire-format evolution rules). A server
advertises it in `Meta::QueryCaps`; a mount whose server advertises `Block` uses Model A, and
the kernel wires that server's device handle for direct file-data I/O.
