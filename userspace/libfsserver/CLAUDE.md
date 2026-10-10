# userspace/libfsserver/CLAUDE.md

Constraints for the filesystem-server protocol library. Loaded when working under
`userspace/libfsserver/`.

## What this is

**The half of a filesystem server that is the protocol** (Phase 6 Part E.1). Everything
`fs-server-ext4`'s binary did that was not ext4 moved here when `fs-server-fat` arrived, so the two
servers speak one protocol and a fix to it is a fix to both:

- **`block.rs`** — `BlockReader`, `BlockWriter`, `FsError`, `BlockRun`, and `ReadOnly`, through
  which a read-only mount is served. `fs-server-ext4` re-exports them, so the installer, the storage
  service and `boot-probe` still name them as `fs_server_ext4::…`.
- **`volume.rs`** — `Volume`, the filesystem as the protocol sees it: what each server implements
  over its library. Its methods are the calls the ext4 binary made into its library.
- **`serve.rs`** — the pure request→reply core for a forwarded resolve or range read, generic over
  `Volume`, and the one `FsError`→`KError` mapping. It touches no syscalls, so it is host-tested —
  **through each server's volume**, in that server's crate, against an image its own tools built.
  A grow's reply marks what the grow added `UNWRITTEN` (`serve_grown`, Phase 6 Part H.4), which
  `Volume::grow_file` makes true by answering the size it grew from, past which everything reads
  as zero.
- **`disk.rs`** — the device over `sys_io_submit`: `Disk`, a 4 KiB block per submit, which ext4's
  server uses — but zeroes up to 64 KiB a submit, from a zeroed memory object of its own
  (`BlockWriter::write_zeroes`, Phase 6 Part H.4); and `SectorDisk` (Part E.3), sector-granular and up to 64 KiB per submit, which
  FAT's uses. A server hands `server::bootstrap` the one it wants. And **`PartitionIo`** (Phase 6
  Part G.2), a window onto part of a device, for a program that makes a filesystem on a disk it
  holds rather than a server handed a partition: `nxinstall`'s until `disk --format` needed it too.
  **None of the three is `Sync`**: each moves every transfer through one scratch mapping.
- **`cache.rs`** — **`MetaCache`** (Phase 6 Part H.4), the metadata cache each server serves its
  filesystem through: a fixed store of 4 KiB blocks, filled only by `BlockReader::read_meta`, every
  write through to the device and into any copy it holds, `BlockWriter::forget` letting freed blocks
  go. A server keeps its `Store` in a `static` (`SERVER_BLOCKS`, 1 MiB, `.bss`).
- **`server.rs`** — the bootstrap and the loop: the setup message, `Ready` or a refusal, the
  forwarding endpoint, directory sessions and their wait slots, a rename resolved ahead of a
  session, `File::Forget` before a file is freed, `File::Touch` by id, and `Meta::Unmount`.

## Rules

- **A server that panics exits** (`server::panicked`, which both binaries' handlers call): a
  forwarded resolve has no deadline, so a server spinning in its handler held every client waiting
  on it for ever. An exit closes the endpoint, and the kernel fails each `PeerClosed`.
- **`no_std`, no `alloc`.** The loop's buffers are statics: one set per process, which is one
  server. A library holding them is no different from the binary holding them, since a server is
  one process serving one filesystem.
- **Nothing filesystem-specific.** A name, a kind, a reason a device cannot be served: each comes
  through `Volume`. A rule that holds for one filesystem only belongs in that server's library.
- **A file is freed only after the kernel has forgotten it** — `forget_then_release` — wherever
  the id comes from: an unlink's last name, a replaced rename target, or a truncate that ends an id
  (FAT's, to zero).
- **A rename is resolved before the directory-session path**, which infers "directory open" from
  the suffix naming a directory; renaming a directory names one too.
- **`read_meta` is for what only the server writes**: directories, inode tables, bitmaps,
  descriptors, superblocks, extent nodes, the FAT. **Never a file's data**, which the kernel writes
  to the device behind the server's back — a cached copy of it would be stale. A read left as
  `read_at` is only slower; one wrongly made `read_meta` returns old bytes. **And whatever a library
  frees it `forget`s**, so a block that may next be a file's leaves the cache. Each server's crate
  holds this in a test that writes data the kernel's way under a cache that evicts all the time, and
  reads it back.
- **The device layer's granularity is a measured question** (`TODO(fs-throughput)`, Phase 6 Part H).
  Change what `Disk` moves per submit with a number from the laptop, not a guess. Its zeroes moved
  to 64 KiB on one: ext4's grows wrote a block a command, 65,536 for a 256 MiB copy.

## Capability discipline

A server receives a read-write block-device handle and a control channel at spawn, and nothing
else; it hands the kernel a `READ | TRANSFER` duplicate of the device for the Model A data path. It
never holds `BIND_NAMESPACE`: its supervisor binds its endpoint
(`docs/rationale/why-supervisor-registration.md`).
