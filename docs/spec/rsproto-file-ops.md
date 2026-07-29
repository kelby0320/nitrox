# Resource Server Protocol — File operations

The `File` category (`op = 0x06xx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). These operations give a
client **positioned, stateless access to a file's content** — the byte-level
reads that back a demand-paged, page-cache-filled file mapping.

**Status:** Pre-stabilization. Introduced with Phase 2 slice 8 (the kernel page
cache); the directory operations landed with the Phase 4 dir-ops slice (2026-07-23)
and gained per-entry metadata with coreutils Milestone 1 (2026-07-24).

The category spans **two different client relationships**, which is why the
transports differ:

| Op | Client | Transport |
|---|---|---|
| `ReadRange` (`0x0600`) | the **kernel** (page-cache fill) | the server's forwarding channel; the kernel hand-codes the request/reply in `kernel/src/rsproto.rs` |
| `Touch` (`0x0606`) | the **kernel** (post-writeback `mtime`) | the server's forwarding channel; **no reply** |
| `ReadDir` (`0x0601`), `Mkdir` (`0x0602`), `Unlink` (`0x0603`), `Rmdir` (`0x0604`), `Rename` (`0x0605`) | an ordinary **userspace process** | a **directory session channel** — direct client↔server RPC, no kernel involvement |

`librsproto` (`userspace/librsproto/src/file.rs`) is the userspace mirror for both
and the canonical source for byte-level details.

`File` is deliberately distinct from the neighbouring categories:

| Category | Level | Role here |
|---|---|---|
| `Stream` (`0x02`) | byte, **cursor-based** | sequential read/write/seek; not used for page fills |
| `Block` (`0x03`) | **device block runs**, fs-neutral | **Model A** page-cache path for **block** filesystems — [rsproto-block-ops.md](rsproto-block-ops.md) |
| `File` (`0x06`) | byte, **positioned, stateless** | **Model B** page-cache fill (`ReadRange`) for **non-block** filesystems |

Model A (block filesystems) and Model B (non-block) are complementary — one data path per
filesystem class, not competing alternatives (see
[filesystem-data-path.md](../architecture/filesystem-data-path.md)). This document specifies
Model B (`ReadRange`); Model A's `MapRange`/`AllocRange` are in
[rsproto-block-ops.md](rsproto-block-ops.md).

## The file-mapping flow (Model B)

1. A client `sys_ns_lookup`s a file path under a `UserspaceServer` mount. The
   kernel forwards a `Namespace::Resolve` with the **`RESOLVE_FILE_LAZY`** flag
   ([rsproto-namespace-ops.md](rsproto-namespace-ops.md)).
2. The server replies `object_kind = OBJECT_KIND_FILE`, with `content_len` set to
   the **total file size** and **no handle** in `handles[0]`. The kernel builds a
   page-cache object (a `FileObject`) of that size, pointed back at this server,
   and installs it into the client's handle table.
3. The client `sys_memory_map`s the file handle. The mapping is **lazy** — no
   pages are resident.
4. On the first touch of a page, the kernel's page-fault handler issues a
   **`File::ReadRange`** for that page's byte range and **blocks the faulting
   thread** until the server replies. The reply's `MemoryObject` is copied into
   the page-cache frame, the page is mapped, and the thread resumes.

The fill is **stateless**: each `ReadRange` re-identifies its file by the same
path `suffix` the lazy `Resolve` used (the kernel stores it on the page-cache
object). A server-side open-file cookie is a possible Phase-3 optimization.

## ReadRange (`op = 0x0600`)

Read a byte range of a lazily-resolved file. The kernel sends this to fill one
page of a file-backed mapping; the server replies with the bytes as a transferred
read-only `MemoryObject`.

### Request body

```rust
#[repr(C, packed)]
pub struct ReadRangeRequest {
    pub offset: u64,      // offset 0  — file byte offset (page-aligned)
    pub len: u32,         // offset 8  — bytes requested (≤ one page)
    pub suffix_len: u16,  // offset 12 — length of the path suffix
    pub _reserved: u16,   // offset 14
    // offset 16: suffix bytes (suffix_len) — the file path, no leading '/'
}
```

Fixed prefix length: **16 bytes**, then `suffix_len` suffix bytes. `handle_count
= 0` (the request carries no handles).

### Reply body (success)

```rust
#[repr(C, packed)]
pub struct ReadRangeReply {
    pub content_len: u32, // offset 0 — valid bytes in handles[0] (≤ len)
    pub _reserved: u32,   // offset 4
}
```

Wire length: **8 bytes**. The filled bytes ride in `IpcMsg.handles[0]` as a
read-only `MemoryObject` of at most one page. `content_len` is the number of
valid bytes; if it is short of `len` (a tail at end-of-file), the remainder of the
page is zero (the page-cache frame starts zeroed). The server transfers the
`MemoryObject` with `MAP_READ | TRANSFER`; the kernel copies out the content and
drops it.

### Error reply

Flagged `RS_FLAG_REPLY | RS_FLAG_ERROR`; the body is the standard `ErrorBody`
(12-byte prefix; see the wire-format spec). The kernel fails the page fault with
the carried `KError`.

## The directory session

A directory path resolved through the namespace yields an `OBJECT_KIND_CHANNEL` —
a session channel the server mints, **scoped to one directory inode**. The channel
*is* the directory: every op below addresses entries **by name**, never by path, so
a client holding a directory handle structurally cannot reach outside it. No kernel
change was needed for this (see the decision log, 2026-07-23).

### ReadDir (`op = 0x0601`)

Enumerate the directory. Listings that exceed one message paginate via an opaque
`cursor`.

```rust
#[repr(C, packed)]
pub struct ReadDirRequest {
    pub cursor: u64,      // 0 = from the start; else a prior reply's next_cursor
}
```

Reply body: a 12-byte header (`next_cursor: u64`, `entry_count: u16`, 2 reserved),
then `entry_count` packed entries. Each entry is a **24-byte prefix** followed by
`name_len` name bytes (no padding between entries):

| Offset | Field | Type | Meaning |
|---|---|---|---|
| 0 | `inode` | `u32` | Server-defined entry identity |
| 4 | `kind` | `u8` | `DIRENT_KIND_{UNKNOWN,FILE,DIR,SYMLINK}` |
| 5 | `name_len` | `u8` | Name length (names are ≤ 255 bytes) |
| 6 | `mode` | `u16` | POSIX `st_mode` — format bits + permissions; `0` if unreported |
| 8 | `size` | `u64` | Byte size (a directory reports its own directory-data size) |
| 16 | `mtime` | `i64` | Modification time, seconds since the Unix epoch; `0` if unknown |

`next_cursor == 0` means the listing is complete. A `cursor` is opaque to the
client — for the ext4 server it is a byte offset into the directory's data.

**Why the metadata is in the entry.** `list` reports
`Table<{name, size, kind, modified}>`, and carrying those fields inline keeps a
listing at **one round trip per reply** instead of `1 + N` (a per-name `Stat` op).
The server reads each entry's inode locally, which is far cheaper than a client
round trip. `mode` occupies what would otherwise be alignment padding ahead of
`size`, so it is free. A server that does not track a field sends `0`; `kind` may
then be recovered from `mode`'s format bits.

`.` and `..` are included — they are real directory entries, and filtering them is
a display decision, not a protocol one.

### Mkdir (`0x0602`) / Unlink (`0x0603`) / Rmdir (`0x0604`)

Request body: `name_len: u16` followed by the name bytes. Success is an empty-body
reply; failure is the standard error reply.

### Rename (`0x0605`)

Request body: `old_len: u16` + the old name + `new_len: u16` + the new name (each
length immediately precedes its own name). Both names are resolved **within this
directory**; a rename that crosses directories goes through `RESOLVE_RENAME` on the
resolve path instead (`rsproto-namespace-ops.md`), because it names two directories and a
session is scoped to one by construction.

### Touch (`0x0606`)

Request body: `suffix_len: u16`, two reserved bytes, then the suffix naming the file under
the mount. **No reply**, and `request_id` is `0` — nothing correlates it.

The odd one out of the `File` ops on three counts, all following from who sends it. It
comes from the **kernel**, on the **forwarding channel** rather than a directory session,
and it asks for nothing back.

It exists because of Model A. The kernel owns the file-data path, so an in-place,
**same-length** overwrite travels from the page cache to the device with no resolve and no
IPC at all: the server has no way to observe that the file changed, and its `mtime` would
keep reporting the last *size* change. After flushing such a file (`sys_file_sync`), the
kernel names it to the server, and the server stamps the modification time.

Two properties are deliberate:

- **No timestamp on the wire.** The server reads its own clock. A time supplied by whoever
  wrote the file would be a time the writer gets to choose, and timestamps are not the
  writer's to choose — the same reasoning that put the wall clock behind a syscall rather
  than a caller-supplied argument.
- **No reply, nothing pending.** The data is already durable when this is sent, so there is
  nothing for a caller to wait on and a lost notification costs only a stale timestamp.
  Ordering still holds where it matters: the request enters the same endpoint ring as
  forwarded resolves, so a subsequent lookup of that file is processed after it.

The stamp is applied on **sync**, not on the individual write, because the kernel keeps no
per-page dirty bit (`TODO(page-dirty-tracking)`).

## Versioning

Adding `File` is a new category (minor version bump per the wire-format spec's
evolution rules). Older servers that do not advertise `File` in `Meta::QueryCaps`
are never sent `RESOLVE_FILE_LAZY`; the kernel falls back to the eager
`RESOLVE_FILE_AS_MEMOBJ` path (slice 7).

The `ReadDir` entry prefix widened from 8 to 24 bytes when per-entry metadata was
added. Both sides are in-tree and pre-stabilization, so this was a flag-day change
rather than a negotiated one; `librsproto` is the single definition both speak.
