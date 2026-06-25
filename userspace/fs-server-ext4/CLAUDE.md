# userspace/fs-server-ext4/CLAUDE.md

Constraints for the ext4 filesystem server. Loaded when working under
`userspace/fs-server-ext4/`.

## What this is

The first **userspace resource server** (Phase 2 slice 7): a process that reads a
**read-only** ext4 filesystem over the block device and serves it through the
resource-server protocol, reached transparently via the namespace (the kernel
forwards `sys_ns_lookup` to it). It is **not** in the kernel — filesystems are
userspace processes (`CLAUDE.md` core rule).

## Structure

- **`src/ext4.rs` + `src/lib.rs` — the reader library.** Pure parsing behind the
  `BlockReader` trait (`read_at(offset, buf)`), so it is 100% host-tested against
  an `mke2fs` fixture. `no_std`, **no `alloc`**: `read_file` reads into a
  caller-provided buffer; parsing uses bounded stack scratch (≤ one 4 KiB block).
  Do not pull in `alloc` here — the reader must stay buffer-based.
- **`src/main.rs` — the server `[[bin]]`** (Part 4): the bare-target `_start`,
  a `BlockReader` over `sys_io_submit`, and the `librsproto` server loop
  (Hello/Ready + `Namespace::Resolve`). Reuses init's static-arena
  `#[global_allocator]` pattern (the server may use `alloc`; the *reader* may not).

## Read-only, minimal (Phase 2)

Implements: superblock (`0xEF53`), block-group descriptors, inodes, the **extent
tree** (`0xF30A`), a linear `ext4_dir_entry_2` directory walk, path resolution to
a regular file. **Reject / skip** (return `FsError::Unsupported`/`Corrupt`): the
journal, bigalloc, inline-data inodes, 64-bit block numbers, ≥ 8 KiB blocks, RW,
xattrs, ACLs, symlinks, checksums. A served file is capped at `ext4::MAX_FILE`
(64 KiB) — the slice-7 read model; slice 8's page cache lifts the cap with lazy
faulting. htree directories need no special handling (the linear walk is
backward-compatible).

## Capability discipline

The server receives only what it needs at spawn: a **read-only block-device
handle** and a **control channel** (for the Ready handshake). It never holds
`BIND_NAMESPACE` — the supervisor (init) binds its endpoint. See
`docs/rationale/why-supervisor-registration.md`.

## Forbidden

- `alloc` in the reader library (`ext4.rs`/`lib.rs`) — buffer-based only.
- Any write path (Phase 2 is read-only).
- Binding itself into a namespace, or holding `BIND_NAMESPACE`.
- Trusting on-disk structures without bounds-checking (a malformed image must
  yield `FsError`, never a panic or OOB read).
