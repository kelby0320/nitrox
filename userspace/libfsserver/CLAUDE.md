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
- **`disk.rs`** — the device over `sys_io_submit`: `Disk`, a 4 KiB block per submit, which ext4's
  server uses; and `SectorDisk` (Part E.3), sector-granular and up to 64 KiB per submit, which
  FAT's uses. A server hands `server::bootstrap` the one it wants.
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
- **The device layer's granularity is a measured question** (`TODO(fs-throughput)`, Phase 6 Part H).
  Change what `Disk` moves per submit with a number from the laptop, not a guess.

## Capability discipline

A server receives a read-write block-device handle and a control channel at spawn, and nothing
else; it hands the kernel a `READ | TRANSFER` duplicate of the device for the Model A data path. It
never holds `BIND_NAMESPACE`: its supervisor binds its endpoint
(`docs/rationale/why-supervisor-registration.md`).
