# Resource Server Protocol — Namespace operations

The `Namespace` category (`op = 0x01xx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). These operations let a
resource server answer **namespace resolution** forwarded to it by the kernel: a
client's `sys_ns_lookup` of a path bound to a `UserspaceServer` is forwarded by
the kernel as a `Namespace::Resolve` request, and the server's reply carries the
resolved resource (Phase 2: a file's content as a `MemoryObject`).

**Status:** Pre-stabilization. Introduced with Phase 2 slice 7 (the first
userspace resource server, `fs-server-ext4`). This is a **kernel↔server ABI** —
the kernel hand-codes the request/reply; `librsproto` carries the userspace
mirror. Only `Resolve` is defined; `Enumerate` and others land with their
consumers.

## Resolve (`op = 0x0100`)

Resolve a path suffix to a resource. The kernel sends this on a client's behalf
when the client looks up a path under a `UserspaceServer` mount; the `suffix` is
the path past the binding prefix (leading `/` stripped — exactly the suffix
`Namespace::resolve` yields). The server resolves it and replies with the
resource handle in `IpcMsg.handles[0]`.

### Request body

```rust
#[repr(C, packed)]
pub struct ResolveRequest {
    pub requested_rights: u64,   // offset 0  — the lookup's requested Rights bits
    pub flags: u32,              // offset 8  — RESOLVE_* flags
    pub suffix_len: u16,         // offset 12 — length of the suffix that follows
    pub _reserved: u16,          // offset 14
    // followed by `suffix` (UTF-8, suffix_len bytes; no leading '/')
}
```

`handle_count = 0`. Body length = `16 + suffix_len`.

**Flags:**

| Flag | Value | Meaning |
|---|---|---|
| `RESOLVE_FILE_AS_MEMOBJ` | `1 << 0` | Phase-2 mode: a regular file resolves to a read-only `MemoryObject` holding its content. The only mode slice 7 defines. |

A future flag selects the lazy/page-cache-backed resolution (slice 8); without it
the server must materialise the content eagerly.

### Reply body (success)

`RsFlags::REPLY` set, `handle_count = 1` (the resource in `IpcMsg.handles[0]`):

```rust
#[repr(C, packed)]
pub struct ResolveReply {
    pub object_kind: u16,        // offset 0  — see below
    pub _reserved: u16,          // offset 2
    pub content_len: u32,        // offset 4  — exact resource length in bytes
                                 //   (a MemoryObject may zero-pad the tail)
}
```

Body length = 8.

**`object_kind`:**

| Kind | Value | `handles[0]` | Phase 2 |
|---|---|---|---|
| `MEMOBJ` | `1` | a read-only `MemoryObject` of the file content | ✅ |
| `DIRECTORY` | `2` | (a directory resource) | deferred |
| `SUBNAMESPACE` | `3` | (a nested namespace) | deferred |

`content_len` is the exact byte length, so the client can trim the
`MemoryObject`'s zero-padded tail precisely. Phase 2 caps the content at **64
KiB**; a larger file replies with the error `TooLarge` (the page cache, slice 8,
lifts the cap with lazy faulting).

### Reply body (error)

`RsFlags::REPLY | RsFlags::ERROR` set, `handle_count = 0`, body is the standard
[`ErrorBody`](rsproto-wire-format.md#error-replies): `kerror` is a `KError`
discriminant (`NotFound` for a missing path, `TooLarge` past the cap,
`Unsupported` for an unhandled kind / flag / a non-regular-file).

## How the kernel uses this (the forwarded lookup)

1. A client `sys_ns_lookup(path)` resolves to a `UserspaceServer` binding with
   `suffix`. The kernel builds a `Resolve` request (`requested_rights` = the
   lookup's rights, `RESOLVE_FILE_AS_MEMOBJ`, the `suffix`), assigns a
   `request_id`, and sends it to the server's kernel-held endpoint. The lookup's
   `PendingOperation` stays **pending**.
2. The server resolves the suffix, reads the file, and replies — transferring the
   `MemoryObject` in `handles[0]` (success) or an `ErrorBody` (failure).
3. The kernel installs the transferred `MemoryObject` into the **original
   caller's** table (rights = `requested ∩ binding.rights`) and signals the
   lookup PO: `IoResult { status: 0, result: <the installed handle> }`, or
   `status: <kerror>` on error. The client's `sys_wait` returns the handle.

The client then `sys_memory_map`s the `MemoryObject` `MAP_READ` and reads — the
identical flow it uses against the in-kernel `/initramfs` server today.

## Where to read more

- [rsproto wire format](rsproto-wire-format.md)
- [Namespaces and resource servers](../architecture/namespace-and-resource-servers.md)
- [Why supervisor-mediated registration](../rationale/why-supervisor-registration.md)
