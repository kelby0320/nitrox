# Why Content-Addressed Store

Nitrox stores all installed software in a content-addressed immutable store (`/store/<hash>-<name>-<version>/`), with a separate profile/namespace layer projecting selected store paths into user-facing locations. This is the NixOS/Guix model, adapted for the Nitrox capability and namespace architecture. This document explains the choice.

## What problem this solves

The traditional Unix system update problem is that updates happen in place. You have `/usr/bin/python` at version 3.11. You upgrade to 3.12. The same file is now version 3.12. Anything that depended on 3.11-specific behavior is now broken. Rolling back means installing the old version on top of the new one, with whatever ordering hazards that produces. Library versions can't coexist; if `/usr/lib/libssl.so.3` is OpenSSL 3.0, every program that links it gets 3.0 — there's no way to say "this program needs 3.0 and this one needs 3.1."

The bandaids are well-known:

**Versioned filenames** (`libssl.so.1.1`, `libssl.so.3`) let two versions coexist for libraries with stable ABIs. Doesn't help for programs.

**Package managers maintain "alternatives"** (Debian's `update-alternatives`, etc.) to flip between coexisting installations. Doesn't make the actual files coexist; just provides an indirection.

**Containers and chroots isolate processes** so they see different filesystems. Heavy-handed for what's fundamentally a versioning problem.

**Static linking** sidesteps shared library issues at the cost of disk space and update granularity.

**System snapshots** (Btrfs/ZFS-based) give rollback at the filesystem level, but are coarse — you roll back the whole system, not just a package.

None of these address the core issue, which is that the install path encodes "this is the only version" by its position in a global namespace. The path `/usr/bin/python` is taking a strong position about what `python` means; conflict is inevitable.

## The content-addressed approach

NixOS and Guix took a different approach. Every installed software unit lives at a path that includes a cryptographic hash of its contents and all its transitive dependencies. The hash is derived deterministically from the build inputs — the source code, compiler version, build flags, and recursively the hashes of every dependency.

```
/store/8d3f2a1c-glibc-2.38/
/store/f4e9b2d1-openssl-3.1.2/
/store/a7c3e8f2-nginx-1.24.0/
```

The hash is what makes the path unique. Two builds that produce identical outputs land at the same path; two builds with any difference (even a one-byte source change) land at different paths.

This produces several useful properties as a consequence of the structure rather than as features bolted on.

**No conflicts.** Multiple versions of any package coexist trivially. Two versions of glibc both live in the store, at different paths. A program built against version A holds references to version A's path; a program built against version B holds references to version B's path. They don't see each other.

**Atomic updates.** Installing version B doesn't modify version A's files. The new version is built into a new store path. The system's view of "the current version" is updated separately, in one atomic step. A power failure mid-update leaves the old version intact.

**Rollback is instantaneous.** "Use the previous generation" is a one-step operation that points the namespace projection at the previous version's store paths. The previous version's files were never deleted; they're still in the store.

**Reproducible builds.** Given the same source and build inputs, the build always produces the same hash. This is verifiable — different machines building the same package should land at the same store path. If they don't, something nondeterministic crept in (timestamps in build outputs, randomized file ordering, etc.). The hash is a strong forcing function for build hygiene.

**Garbage collection is principled.** A store path is reachable if it's referenced by any current generation of any profile. Unreachable paths can be safely deleted. There's no question of "is this still needed somewhere?" — the dependency graph is explicit.

**Binary caches are trivially safe.** A build server can produce store paths and publish them. Clients downloading the same store path are guaranteed identical content (the hash check is the verification). There's no trust relationship beyond "do you trust the build server's hash to match what they claim."

## How Nitrox integrates with the namespace layer

Nitrox doesn't put store paths directly in user-facing locations. The store is at `/store/<hash>-<name>/`, but no user is expected to invoke `/store/abc123-bash-5.2/bin/bash` directly. The integration with the namespace layer is what makes this usable.

A **profile** is a namespace mapping that translates user-friendly paths to store paths. The system profile, for instance, contains:

```
/bin/bash       → /store/abc123-bash-5.2/bin/bash
/bin/python     → /store/def456-python-3.11/bin/python
/lib/libc.so.6  → /store/jkl012-glibc-2.38/lib/libc.so.6
```

A profile server is a userspace resource server backed by a profile manifest (TOML, stored in the store, also content-addressed). When a process looks up `/bin/bash` in its namespace, the lookup is forwarded to the profile server, which returns a handle to the store path's bash binary.

Different users get different profiles. The system profile is shared. A user might have a personal profile layered over the system profile, providing additional packages or different versions. Administrators have a profile that includes admin-only tools. Different profiles can reference different sets of store paths, all coexisting on disk simultaneously.

A **generation** is a snapshot of a profile at a point in time. Switching generations is atomic: change which profile manifest the profile server is serving. Old generations remain referenceable. Rollback is "go back to the previous generation."

This integration matters because it means the store's properties (immutability, version coexistence, atomic updates) compose with the namespace layer's properties (per-process views, capability scoping). A user can be granted access to one profile and not another. A sandboxed app can be given a namespace projection that includes only the dependencies it needs from the store, with no `/bin` profile at all.

## Immutability is a namespace property, not a filesystem property

The store is immutable. But it's not on a filesystem with special "immutable" flags. It lives on an ordinary ext4 partition served by an ordinary fs-server-ext4. fs-server has no special knowledge of `/store`.

The immutability is enforced at the namespace level. Every normal process's namespace binds `/store` with a rights set that does not include `WRITE` or `MAP_WRITE`. The kernel enforces those rights on every operation through any handle obtained from that namespace. A process holding a handle to a store file cannot write to it because the handle doesn't have write rights — regardless of what the underlying ext4 inode permissions say.

The package manager — the one process that legitimately writes to the store — has a different namespace. It sees a binding (perhaps `/system/store-builder` or similar) that does grant write rights to the store subtree. It uses that route to add new store paths. From normal processes' perspective, this route doesn't exist; they can't even articulate the path that would let them write.

This is a property of how Nitrox is layered. The fs-server is dumb. Policy lives in namespace bindings. The store's immutability is a consequence of the system architecture, not a special filesystem feature. This means the store mechanism doesn't depend on filesystem-format support — it works the same on any filesystem the underlying fs-server supports. ext4 today, btrfs tomorrow, something else later. The store property survives.

## What this gives up

**Disk space.** Every distinct version of every dependency is on disk simultaneously. NixOS systems frequently have many gigabytes of "old" store paths kept for rollback. Garbage collection helps; aggressive collection negates the rollback benefit. There's a tradeoff between safety and space, and the store model errs toward safety. For a hobby OS where a user can configure GC aggressiveness based on their available disk, this is fine.

**Some operational unfamiliarity.** Users coming from traditional Unix expect to find files at predictable paths. `/usr/bin/python` is `/usr/bin/python`. In Nitrox, `/bin/python` (note: no `/usr/`) resolves through the profile server to whatever store path is currently selected. The resolution is invisible in normal use, but operations like "tell me the actual path of this binary" produce store paths that look strange. This is a learning curve issue, not a fundamental problem.

**Build complexity.** Producing reproducible builds is more disciplined than producing "any build that happens to work." Build inputs have to be enumerated; nondeterministic tooling (timestamps, random orderings, parallel-build artifacts) has to be eliminated. The Nix and Guix ecosystems have invested heavily in build tooling that gets this right; Nitrox will need similar tooling, though for a hobby project the rigor can be relaxed initially.

**Package management is more involved.** The package manager has to be the one source of truth for the store. Direct manual file manipulation in `/store` is at best a recovery operation, not a normal workflow. This is an enforcement issue but also a discipline change.

## Influences

**NixOS** is the original and the most fully-realized example of the content-addressed store model. The store at `/nix/store/<hash>-<name>/`, the profile abstraction, generations, atomic switches — Nitrox borrows the full pattern. The differences are in language (Nix the language vs. Nitrox's TOML profile manifests), in integration (Nitrox profiles are namespace-projection-based; Nix profiles use symlinks), and in scope (NixOS is a full distribution; Nitrox starts simpler and grows).

**Guix** is the GNU project's implementation of the same model, with Guile Scheme as the configuration language instead of the Nix language. Architecturally similar to NixOS; reinforces that the store model is a robust idea, not a Nix-specific quirk.

**Other content-addressed systems** (IPFS, Bazel's remote cache, Docker's layer hashing) demonstrate the concept's applicability beyond OS package management. The idea that "deterministic build → deterministic hash → safe sharing and verification" is broadly applicable.

**OSTree** is another approach to the same general problem (atomic system updates) but doesn't have the dependency-isolation property of content-addressed stores. Mentioned for completeness; not adopted.

## Where to read more

- [Content-addressed store architecture](../architecture/content-addressed-store.md) — implementation details
- [Profiles and namespace projection](../architecture/profiles-and-namespace-projection.md) — how profile servers work
- [NixOS manual on the Nix store model](https://nixos.org/manual/nix/stable/store/file-system-object.html) — the original reference
- [Why namespace-mediated immutability rather than filesystem-mediated](why-supervisor-registration.md) (related: same pattern of policy-in-namespace-bindings)
