# Content-Addressed Store

The content-addressed store is where all installed software lives, at immutable
paths keyed by a hash of their contents. It is the NixOS/Guix store model, adapted
to Nitrox's capability + namespace architecture. This doc is the design; for *why*
this model at all, read [why-content-addressed-store](../rationale/why-content-addressed-store.md)
first. The user-facing projection layer that makes the store usable is
[profiles-and-namespace-projection](profiles-and-namespace-projection.md).

Status: **pre-implementation** (Phase 3 backlog item 2). The first slice ships a
**read-only** store, pre-built into the ext4 image; the package manager (the store's
writer), runtime generation switching, and GC are designed here but land later (they
need fs-server RW).

## What it is

Every installed software unit — a "package" — lives at a path that includes a hash:

```
/store/9f3a2c1b-heartbeat-0.1.0/
/store/8d3f2a1c-glibc-2.38/
/store/a7c3e8f2-nginx-1.24.0/
```

The hash makes the path unique to the contents. Two builds producing identical
output land at the same path; any difference lands at a different path. This yields
the store's defining properties **structurally**, not as bolted-on features:
multiple versions coexist (different paths, no conflict), updates are atomic (a new
version is a new path; nothing existing is modified), and rollback is instantaneous
(the old path was never deleted). See the rationale doc for the full argument.

## Store path convention

```
/store/<hash>-<name>-<version>/
    bin/<executable>          # projected to /bin by a profile
    lib/<library>             # projected to /lib by a profile (once dynamic linking exists)
    ...                       # package-defined contents (share/, etc.)
```

- **`<hash>`** — an opaque, unique identifier derived from the package's contents.
  **For now it is a content hash** (a truncated hash of the package's files, computed
  at build time). The full NixOS-style *build-input* hash (derived from the build
  recipe + transitive dependency hashes, giving reproducible builds) is a build-system
  / package-manager concern that arrives with those tools; the *layout* is what we
  commit to now. The system treats the hash as an opaque directory name — it does not
  parse or verify it (that verification is the package manager's job later).
- **`<name>`, `<version>`** — human-readable, for display and for the profile
  manifest to reference; not semantically load-bearing (the hash is the identity).
- A package's `bin/` and `lib/` are the projectable subdirs (see the profiles doc).

## Physical storage: ext4, and a dumb fs-server

The store lives on the **ext4 root filesystem**, served by the ordinary
read-only `fs-server-ext4`. `/store` is physically the same ext4 inodes as everything
else on that filesystem. **The fs-server has no special knowledge of `/store`** — it
reads blocks and serves the resource-server protocol; that is all. (Slice 1 pre-builds
the store into the ext4 image at build time — `tools/xtask` writes
`store/<hash>-<name>-<version>/…` into the staging tree before `mke2fs -d`, mirroring
how `system/current-generation` is seeded today. Writing store paths at *runtime*
needs fs-server RW + the package manager, both deferred.)

## Immutability is a namespace-rights property

The store is immutable, but not via a filesystem "immutable" flag. **Immutability is
enforced at the namespace level** — the same "policy lives in namespace bindings"
pattern as [supervisor-mediated registration](../rationale/why-supervisor-registration.md):

- **Every normal process's namespace binds `/store` with `LOOKUP | READ | MAP_READ`
  — but not `WRITE` or `MAP_WRITE`.** The kernel attenuates rights at lookup time
  (`resolved = requested ∩ binding.rights`; see
  [namespace-and-resource-servers](namespace-and-resource-servers.md)), so a handle
  obtained through that binding *cannot* carry write rights — regardless of what the
  underlying ext4 inode permissions say. A store file simply cannot be written through
  a normal process's `/store`.
- **The package manager — the one legitimate writer — has a *different* namespace
  route** to the same subtree, bound with `BIND | WRITE | MAP_WRITE`. Same ext4 inodes;
  a different binding; different rights; different effective behavior. From a normal
  process's view that route doesn't exist — it can't even articulate a path that would
  let it write.

**There is no store-specific `SysCap`.** The writer's authority is *just a different
namespace binding*, which a supervisor holding `BIND_NAMESPACE` constructs for it at
spawn. `SysCaps::PHYSICAL_MEMORY` (map arbitrary physical memory, bypassing the
namespace-mediated interface) is explicitly out of scope for the store guarantee — it
is held only by privileged boot-path code, never by normal processes.

This layering means the store mechanism is independent of filesystem format: it works
the same on any filesystem the fs-server supports.

## Reading store programs: `FileObject` + the ELF loader

A store path resolved through the fs-server is a **`FileObject`** — a demand-paged
file (bytes materialized on fault via the fs-server's `File::ReadRange` producer),
distinct from the eager `MemoryObject` the in-kernel initramfs server hands back. The
path-based-spawn ELF loader therefore accepts **both** image types: for a `FileObject`
it materializes the image by driving `FileObject::fault_in_page` per page (which
blocks on the producer until resident) into a contiguous buffer, then runs `load_elf`
— the direct analog of `MemoryObject::copy_to_kvec`. This is what lets a program be
spawned from the store (`executable = "/bin/heartbeat"` → the profile server → a store
`FileObject` → spawn). See [process-spawn-args](../spec/process-spawn-args.md).

## Generations and garbage collection

A **generation** is a snapshot of a profile — which store paths are "current" (see the
[profiles doc](profiles-and-namespace-projection.md) for switch/rollback). The store
side of generations is **reachability**: a store path is *reachable* if it is
referenced by any current generation of any profile (directly, or transitively via a
package's recorded dependencies). Garbage collection is then principled — mark the
paths reachable from all live generations, sweep the rest. There is no "is this still
needed?" ambiguity; the dependency graph is explicit.

GC and the dependency graph are **designed, not yet built** — they belong to the
package manager (the store's owner/writer), which needs fs-server RW to add/remove
paths. Slice 1 ships a single pre-built generation, so nothing is unreachable and
nothing is collected.

## The package manager (deferred)

The package manager daemon is the store's single source of truth and its one writer:
list / add / remove store paths, manage generation manifests, run GC. It is a normal
process distinguished only by the **write-granting `/store` binding** it is handed at
spawn (per above) — not by any special capability. Direct manual manipulation of
`/store` is a recovery operation, not a workflow. Deferred until fs-server RW lands
(Phase 3 backlog item 4).

## What slice 1 builds vs. defers

| | Slice 1 | Deferred (later slices) |
|---|---|---|
| Store on ext4 | ✅ pre-built into the image (read-only) | runtime writes (package manager + fs RW) |
| `/store` RO namespace binding | ✅ (`LOOKUP\|READ\|MAP_READ`) | the writer's `WRITE` route |
| Reading store programs | ✅ `FileObject`-spawn | — |
| Generations | ✅ one static generation | runtime switch / rollback (need persisted state) |
| Package manager, GC, dep graph | — | ✅ designed here; built after fs RW |

## References

- Rationale: [why-content-addressed-store](../rationale/why-content-addressed-store.md)
- The projection layer: [profiles-and-namespace-projection](profiles-and-namespace-projection.md)
- Namespace rights + lookup attenuation: [namespace-and-resource-servers](namespace-and-resource-servers.md)
- Capabilities: [syscaps](syscaps.md), [why-supervisor-registration](../rationale/why-supervisor-registration.md)
- Spawn / image loading: [process-spawn-args](../spec/process-spawn-args.md)
