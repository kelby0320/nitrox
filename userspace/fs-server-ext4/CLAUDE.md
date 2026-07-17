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
- **`src/serve.rs` — the request→reply core.** `serve_resolve(reader, request,
  content, reply)` parses a forwarded `Namespace::Resolve`, reads the file via the
  `BlockReader`, and builds the reply (success names a `MemoryObject` of the
  content; error carries a `KError`). Generic over `BlockReader`, so it is
  **host-tested** against the same `mke2fs` fixture as the parser (see
  `test_support`). Touches no syscalls.
- **`src/main.rs` — the server `[[bin]]`.** The bare-target `_start` + the syscall
  plumbing only: a `BlockReader` **and `BlockWriter`** over `sys_io_submit` (sector-at-
  a-time into a scratch `MemoryObject`; writes are read-modify-write per sector), the
  bootstrap (recv the **read-write** device handle via the setup message; forwarding
  channel; `Meta::Ready`), and the serve loop — a Model A lazy resolve replies the file's
  `BlockRun` map + transfers a device handle, and a `RESOLVE_GROW` request grows the file
  (`maybe_grow` → `grow_file`) before mapping. **Alloc-free** — fixed `.bss` buffers, no
  `#[global_allocator]`.

## Scope

Implements: superblock (`0xEF53`), block-group descriptors, inodes, the **extent
tree** (`0xF30A`, walk + in-place extend), a linear `ext4_dir_entry_2` directory walk,
path resolution to a regular file, block-bitmap allocation + free-count updates, and
file growth. **Reject / skip** (return `FsError::Unsupported`/`Corrupt`): the journal,
bigalloc, inline-data inodes, 64-bit block numbers, ≥ 8 KiB blocks, xattrs, ACLs,
symlinks, checksums. htree directories need no special handling (the linear walk is
backward-compatible).

**Write path deferred** (see `docs/architecture/ext4-fs-server-rw.md`): extent-tree
splitting / index nodes (depth > 0), cross-group allocation, truncate / delete / rename,
inode allocation + directory-entry insertion (new-file creation), `metadata_csum`
checksums, and jbd2 journaling + replay (the fixtures are `^has_journal`). Overwrite is
data-only (no metadata change) and is the kernel's writeback; the server allocates on
growth but never touches file data (Model A).

## Capability discipline

The server receives only what it needs at spawn: a **read-write block-device
handle** (`READ | WRITE`, for metadata I/O; it hands a `DUPLICATE`d copy to the kernel
for the Model A data path) and a **control channel** (for the Ready handshake). It never
holds `BIND_NAMESPACE` — the supervisor (init) binds its endpoint. See
`docs/rationale/why-supervisor-registration.md`.

## Forbidden

- `alloc` in the parser library (`ext4.rs`/`lib.rs`) — buffer-based only.
- Touching **file data** — the kernel owns the data path (Model A); the server writes
  only metadata (bitmaps, extent tree, inode, superblock).
- Binding itself into a namespace, or holding `BIND_NAMESPACE`.
- Trusting on-disk structures without bounds-checking (a malformed image must
  yield `FsError`, never a panic or OOB read).
