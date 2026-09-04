# Profiles and Namespace Projection

The [content-addressed store](content-addressed-store.md) holds software at
hash-keyed paths like `/store/9f3a-heartbeat-0.1.0/bin/heartbeat` — but no one invokes
those paths directly. A **profile** projects a chosen set of store packages into
user-friendly locations (`/bin`, `/lib`). This is what makes the store usable, and
it's where NixOS/Guix generations, atomic switches, and rollback live in Nitrox.

Nitrox's distinctive choice: projection is done by a **resource server** resolving
lookups, **not** by symlink farms (as Nix does). This is the capability-native form —
it composes with per-process namespaces and rights.

Status: **implemented, slice 1** (Phase 3), and **projecting a second directory since
2026-09-04** (M14 Part H): the same server also projects each package's `applications/` at
`/applications`, which is where [desktop entries](../spec/desktop-entry.md) live.

**Every bind of the endpoint is scoped, and the first component of the suffix names the
projection.** `/bin` bound with base `/bin` forwards `bin` and `bin/list`; `/applications` bound
with base `/applications` forwards `applications` and `applications/nxterm.toml`. The kernel joins
a base *without* its leading slash (`join_subtree` takes `base[1..]`), so the forwarded suffix
never carries one. There is no default and no bare form.

**`/bin` was unscoped until 2026-09-04, and that was an alias rather than a shortcut.** A bare
suffix meant `bin`, so `/bin/applications` forwarded as `applications` and landed on the
applications projection's *root* — putting all of it inside every namespace that binds `/bin`,
including the application namespaces documented as not having it (PR #279 review). Scoping both
removes the bare form that made two different things look alike.

The projected set is a fixed list in the server, not "whatever a package happens to contain": a
package is data, and what appears in a namespace is policy.

The **system profile** is projected at `/bin`
from a manifest over the read-only store by `userspace/profile-server`, which `init`
spawns and whose endpoint it hands to `service-mgr`. Per-user profile overlays, runtime
generation switching, and rollback are designed here but **not built**. Verified 2026-08-05.

## What a profile is

A profile is **a set of packages** (store paths) that together form a coherent
environment. The profile server projects each package's `bin/*` into `/bin` and
`lib/*` into `/lib` — the *union* of the packages' binaries and libraries. A lookup of
`/bin/foo` resolves to the `foo` provided by one of the profile's packages.

This is the Nix "a profile is an environment = union of packages" model, not an
explicit per-binary path map. It scales cleanly: adding a package to a generation is
adding one line to a manifest; the projected `/bin` updates as a consequence.

## The profile server

A profile server is a **userspace resource server** (like the fs-server):

- It **loads a profile manifest** (TOML — see [Manifest schema](#manifest-schema)).
- It holds a **read handle to the store** (a namespace handle that resolves `/store`,
  granted by the supervisor at spawn).
- It **responds to forwarded lookups** by resolving into the store and returning a
  handle (a "forwarding address" into the store, exactly like the fs-server's
  forwarding — see [namespace-and-resource-servers](namespace-and-resource-servers.md)).
- It is **bound into a namespace by a supervisor** (init for the system profile;
  session-mgr layers user profiles later). It **never self-registers** and does **not**
  hold `BIND_NAMESPACE` (it consumes namespace, it doesn't construct it).
- It performs **no access control**. Security is in namespace construction — which
  profile a process's namespace includes, and with what rights, is decided when the
  namespace is built, not by the profile server.

### The index

Resolve and readdir are served from **one** merged view, built lazily on first use and kept.

Caching it is sound *by construction*, not by invalidation: a store path is
content-addressed, so `/store/<hash>-coreutils-0.1.0/bin/` cannot change contents — different
contents would be a different hash and so a different path. What can change is which packages
a profile names, and the server reads its manifest exactly once at startup, so it cannot
observe that either. Making an installed package visible without a logout is
`TODO(profile-generation-refresh)`.

Building it lazily rather than at startup keeps a boot with no `/bin` consumer from paying
one directory read per package over IPC. Serving *both* paths from it also retires the
original probe, which cost one `sys_ns_lookup` per package per program spawn — invisible at
two packages, fifty round trips at fifty.

A package whose `bin/` cannot be read is skipped rather than fatal: a profile naming one
broken package should lose that package, not all of `/bin`.

### How a lookup resolves — resolve-by-probe

The profile server is bound at a projected root (`/bin`) and receives the **suffix**
of a forwarded lookup (`/bin/foo` → suffix `foo`). It resolves by **probing each
package in manifest order** for that name and returning the first hit:

```
lookup /bin/foo  (forwarded to the profile server bound at /bin)
  for pkg in manifest.packages:            # manifest order = priority
      try resolve  <pkg.path>/bin/foo  in the store   (LOOKUP|READ|MAP_READ)
      if it resolves → return that handle
  → NotFound
```

**Probing, not enumeration** — this deliberately needs only *lookup*, never directory
listing — so since 2026-07-31 the profile server **mints a directory session** for `/bin`
itself (an empty forwarded suffix) and answers `OP_FILE_READ_DIR` from a merged index it
builds by reading each package's `bin/` back through the fs-server's own `readdir`. The
union `/bin` therefore both resolves *and* lists, from one index, so a listing cannot drift
from what a spawn would find.
Name collisions resolve by **order**, on two axes: *within* a profile, manifest order
(first package listed wins); *across* layered profiles, layer order — a per-user
profile is probed before the system profile, so **the user's package overrides the
system's** (the intuitive override, not an error).

**Collision *detection* + explicit priorities are the package manager's job, deferred**
(it lands after fs-server RW). This mirrors Nix: detect at profile-*build* time, resolve
at *use* time. Runtime resolution stays order-based (fast, no `readdir`); when the
package manager composes/writes a generation it *has* enumeration + write access, so it
can warn or error on *unintended* within-profile collisions and honor an explicit
`priority` field on `[[package]]`. Recording this now keeps collision-surfacing out of
the lookup path (where we'd need `readdir`) — it belongs at build time.

`/lib` works identically (probe `<pkg>/lib/<name>`), but **slice 1 projects only
`/bin`** — Nitrox userspace is statically linked today, so there are no shared
libraries to project. `/lib` is designed here and wired when dynamic linking exists.

### Composition with the ELF loader

Because projection is lookup-based, it composes with path-based spawn for free: a
`service.toml` `executable = "/bin/heartbeat"` resolves through the process's namespace
→ forwarded to the profile server → probed to `/store/<hash>-heartbeat/bin/heartbeat`
→ a store `FileObject` handle → `sys_process_spawn`. The whole stack — supervisor,
profile server, store, ELF loader — composes without any of them knowing about the
others. This is the slice-1 demo: heartbeat becomes a store package, and service-mgr
spawns it via `/bin/heartbeat`.

## Manifest schema

A profile manifest is TOML, stored in the store (content-addressed like everything
else) — transitionally in the initramfs for slice 1, per the plan. It lists the
generation's packages:

```toml
# System profile manifest (one generation).
[profile]
name       = "system"
generation = 1

[[package]]
name    = "heartbeat"
version = "0.1.0"
path    = "/store/9f3a2c1b-heartbeat-0.1.0"
# The profile server projects <path>/bin/* → /bin/* (and <path>/lib/* → /lib/* later).
```

- **`[profile]`** — `name` (which profile), `generation` (an integer; monotonic, for
  display + rollback ordering).
- **`[[package]]`** — one per package: `name`/`version` (display; must match the store
  path's), `path` (the store path, the identity). Manifest order is projection priority.
- Unknown keys/tables are ignored (forward-compat), like the service schema.

(This starts inline in this architecture doc; if it grows it splits into a
`docs/spec/profile-manifest-schema.md`, as `service.toml` did.) <!-- check-docs: allow-missing -->

## Generations, switching, rollback

A **generation** is a profile manifest at a point in time — a specific set of store
paths. Because the store never deletes paths, all generations coexist on disk.

- **Switch** = the profile server serves a *different* manifest (or the supervisor
  rebinds a profile server serving the new one). Atomic: a process either sees the old
  projection or the new one, never a half-applied mix. A power failure mid-update
  leaves the old generation intact (the new manifest either committed or didn't).
- **Rollback** = "serve the previous generation's manifest." The previous
  generation's store paths were never removed, so it is a one-step, instantaneous
  operation — the headline NixOS property.
- **`/system/current-generation`** names the active generation's profile manifest.
  (Today it's a plain file init reads as a boot smoke test — text
  `"nitrox-rootfs generation 1"`; it becomes the real generation pointer here.)

Slice 1 ships **one static generation**: `current-generation` names the gen-1 system
profile manifest; the profile server serves it. Runtime *switching* + *rollback* need
the package manager to write new manifests + persisted generation history (fs-server
RW) — designed here, built after item 4.

## Profile composition (overlays) — designed, deferred

A process's namespace can layer profiles: a **user profile** over the **system
profile**, so a user gets extra/newer packages without affecting the system. This is
namespace *layering* (per-process views composing) — a `/bin` lookup tries the user
profile's packages first, then the system profile's. Different users get different
profiles; a sandboxed app can be given a namespace with *no* `/bin` profile at all,
only the specific store dependencies it needs.

Slice 1 has the **system profile only**, bound by init. Since 2026-07-31 session-mgr binds
that same system profile into each login session's namespace at `/bin` — the endpoint is
handed down init → service-mgr → session-mgr, and the kernel *shares* init's registration
rather than minting a rival. A session's `/bin` is therefore the system profile exactly,
with no overlay yet.

Overlays remain the eventual shape: session-mgr layers a *user* profile over the system one
when there is a reason for two users to see different programs. Nothing needs that yet, and
building the layering before the requirement would be guessing at it.

The store package a session actually runs from is `coreutils` (the ten coreutils plus
`nxsh`). Before it existed, `/bin` projected exactly `heartbeat`, so binding it into a
session would have granted access to nothing a person could type.

## Who binds what

| Namespace | Bound by | Slice |
|---|---|---|
| `/store` (`LOOKUP\|READ\|MAP_READ`) in the root ns | init | 1 |
| system profile server at `/bin` (`+/lib` later) in the root ns | init | 1 |
| the **system** profile server at `/bin` in a session ns | session-mgr | 2026-07-31 |
| per-user profile overlay + `/tmp` in a session ns | session-mgr | later |
| the writer's `/store` (`BIND\|WRITE\|MAP_WRITE`) | supervisor → package manager | later |

All via the standard Resource Server Startup Protocol (the supervisor spawns the RS
with a control channel, awaits `Meta::Ready`, binds the endpoint). Profile servers are
[non-holders of `BIND_NAMESPACE`](namespace-and-resource-servers.md).

## What slice 1 builds vs. defers

| | Slice 1 | Deferred |
|---|---|---|
| Profile server RS (resolve-by-probe over `/bin`) | ✅ | `/lib` (needs dynamic linking) |
| System profile manifest | ✅ (in initramfs, transitional) | manifest in the store |
| init binds `/store` RO + profile server at `/bin` | ✅ | — |
| Generations | ✅ one static | runtime switch / rollback |
| Per-user profile overlays | — | ✅ designed; session-mgr |

## References

- The store it projects: [content-addressed-store](content-addressed-store.md)
- Rationale: [why-content-addressed-store](../rationale/why-content-addressed-store.md)
- Forwarding resource servers + namespace resolution: [namespace-and-resource-servers](namespace-and-resource-servers.md)
- Supervisor-mediated registration: [why-supervisor-registration](../rationale/why-supervisor-registration.md)
- Spawn / image loading: [process-spawn-args](../spec/process-spawn-args.md)
