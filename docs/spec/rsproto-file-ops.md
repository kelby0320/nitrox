# Resource Server Protocol — File operations

The `File` category (`op = 0x06xx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). These operations give a
client **positioned, stateless access to a file's content** — the byte-level
reads that back a demand-paged, page-cache-filled file mapping.

**Status:** Pre-stabilization. Introduced with Phase 2 slice 8 (the kernel page
cache). This is a **kernel↔server ABI** — the kernel hand-codes the request/reply
(`kernel/src/rsproto.rs`); `librsproto` (`userspace/librsproto/src/file.rs`)
carries the userspace mirror. Only `ReadRange` is defined; `stat`/`readdir` land
with their consumers.

`File` is deliberately distinct from the neighbouring categories:

| Category | Level | Role here |
|---|---|---|
| `Stream` (`0x02`) | byte, **cursor-based** | sequential read/write/seek; not used for page fills |
| `Block` (`0x03`) | extent / block, fs-specific | **Model A** (extent query) — deferred to Phase 3 |
| `File` (`0x06`) | byte, **positioned, stateless** | **Model B** page-cache fill (`ReadRange`) |

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

## Versioning

Adding `File` is a new category (minor version bump per the wire-format spec's
evolution rules). Older servers that do not advertise `File` in `Meta::QueryCaps`
are never sent `RESOLVE_FILE_LAZY`; the kernel falls back to the eager
`RESOLVE_FILE_AS_MEMOBJ` path (slice 7).
