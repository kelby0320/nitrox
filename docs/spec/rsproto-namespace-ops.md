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
| `RESOLVE_FILE_AS_MEMOBJ` | `1 << 0` | Slice-7 mode: a regular file resolves to a read-only `MemoryObject` holding its content, materialised eagerly. |
| `RESOLVE_FILE_LAZY` | `1 << 1` | Slice-8 mode: a regular file resolves to a lazily page-cache-filled `File` (`OBJECT_KIND_FILE`); the reply carries the file **size**, not its bytes, and the kernel fills pages on demand via `File::ReadRange` ([rsproto-file-ops.md](rsproto-file-ops.md)). |

With `RESOLVE_FILE_LAZY` the server reports the file size and does not materialise
content up front; pages are pulled later by `File::ReadRange`. Without it the
server must materialise the content eagerly.

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
| `FILE` | `4` | **none** — `content_len` is the total file size; the kernel builds a page-cache object filled via `File::ReadRange`. Paired with `RESOLVE_FILE_LAZY`. | ✅ (slice 8) |

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
   caller's** table and signals the lookup PO: `IoResult { status: 0, result: <the
   installed handle> }`, or `status: <kerror>` on error. The client's `sys_wait`
   returns the handle.

   **Installed rights = `requested ∩ (the rights the server granted on the
   transferred handle)`.** Unlike a Kernel Server / direct-handle binding — whose
   bound object's rights are a sensible cap on what a lookup yields — a Userspace
   Server binding's bound object is the *IPC endpoint*, whose rights (`SEND`/`RECV`/
   …) are not a meaningful cap on a returned `MemoryObject`. The meaningful cap is
   what the (trusted) server granted on the object it transfers (read-only content
   ⇒ `MAP_READ`); the binding's role is gating *whether* the client may resolve
   through the mount, which the namespace handle's `LOOKUP` right already enforces.
   The server is responsible for attenuating the transferred handle to the rights
   the content should carry. *(Recorded in the decision log, 2026-06-25; refines the
   earlier `requested ∩ binding.rights` wording, which fits the in-kernel paths.)*

The client then `sys_memory_map`s the `MemoryObject` `MAP_READ` and reads — the
identical flow it uses against the in-kernel `/initramfs` server today.

## Where to read more

- [rsproto wire format](rsproto-wire-format.md)
- [Namespaces and resource servers](../architecture/namespace-and-resource-servers.md)
- [Why supervisor-mediated registration](../rationale/why-supervisor-registration.md)
