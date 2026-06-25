# Namespaces and Resource Servers

Nitrox has **no global filesystem tree, no mount table, no VFS**. What it has
instead is the **per-process namespace**: a private map from paths to resources.
Looking up `/store/config` is meaningful only relative to *your* namespace; there
is no system-wide namespace that all processes share. A resource your namespace
does not bind simply does not exist for you — a lookup returns *not found*, not
*permission denied*. Sandboxing is by **construction** (what a supervisor chose to
bind into your namespace), not by permission checks layered over a shared tree.

This document is the design for the namespace substrate and the resource-server
model that rides on it. It is the contract `docs/rationale/why-supervisor-registration.md`,
`docs/architecture/ipc.md`, and the storage/fs slices refer to. Exact ABIs (the
`sys_ns_*` signatures, the rsproto wire format) live in `docs/spec/`; this is the
*why* and the *shape*.

> **Implementation phasing.** This doc designs the whole model, but it lands in
> three waves (the slice-3/slice-7 split was decided 2026-06-22 — see the decision
> log). **Slice 1 (the namespace substrate)** builds the `Namespace` object,
> resolution, the four `sys_ns_*` syscalls, and **direct-handle** bindings.
> **Slice 3 (in-kernel resource servers)** adds the **in-kernel** server framework —
> the `KernelServer` binding target, the synchronous `OpStatus` dispatch (no IPC),
> the registry, and the first servers (`/dev/entropy`, `/proc/self`) bound at boot.
> **Slice 7 (userspace resource servers, with the fs-server)** adds the
> **IPC-forwarded** path — the `UserspaceServer` binding target, cross-context handle
> install, `librsproto`, and the Ready handshake — built when the fs-server, the
> first userspace RS, consumes it. Each section below marks which wave it belongs to.
> The async lookup *contract* is fixed in slice 1 (see "Lookup is asynchronous")
> precisely so slices 3 and 7 need no ABI change.

## The Namespace object

A `Namespace` is a kernel object (`KObjectType::Namespace`), reached through a
handle like everything else, and **per-process**: each process holds a handle to
the namespace it resolves names against (`Process::namespace`). A namespace is an
ordered set of **bindings**, each:

```
Binding { path, target, rights }
```

- **`path`** — the absolute path prefix this binding owns (e.g. `/dev/log`).
- **`target`** — what the path resolves to (see "Binding targets").
- **`rights`** — the maximum rights a lookup through this binding may obtain. The
  supervisor that created the binding chose them; a lookup can only *attenuate*.

A namespace is **not a filesystem** — it is a *view*. It holds no inodes, no
blocks, no directory contents; it maps name prefixes to the resources (kernel
objects or resource-server endpoints) that *do* hold those things. Two processes
with different namespaces resolving the same path reach different resources, or
one reaches a resource and the other reaches nothing.

There is deliberately **no global namespace, no global mount table, no global
dentry/inode cache** in the kernel (see `docs/rationale/why-no-global-vfs` in
`deferred-decisions.md` non-goals). The only namespace state the kernel holds is
the set of per-process `Namespace` objects (plus, in slice 3, a flat resource-server
registry).

## Path grammar

Paths cross the syscall boundary as **bytes + length** (`UserPtr<u8>, len`), not
NUL-terminated C strings — consistent with the rest of the syscall ABI. A path is:

- **Absolute** — begins with `/`. (There is no per-process "current directory" in
  the kernel; a userspace cwd, if any, is resolved to an absolute path before the
  syscall.)
- **`/`-separated** into components. The empty path is invalid; `/` is the root.
- **Bounded** — at most `NS_PATH_MAX` bytes (**1024** in v1). Longer → rejected
  (`InvalidArgument`).
- **Normalized by the caller** — v1 rejects `.`, `..`, empty (`//`), and trailing
  `/` (except root) components rather than normalizing them in the kernel. Path
  traversal tricks therefore cannot exist: a path is a literal sequence of
  components. (Normalization/`..` handling, if ever wanted, is a userspace concern
  layered above, never a kernel one — it would reintroduce the ambient-authority
  hazards namespaces exist to avoid.)

Paths are **byte strings**, not required to be UTF-8 at the kernel level (the
kernel compares bytes); userspace conventions may restrict them further.

## Resolution

A lookup resolves a path against the namespace by **longest-prefix match**: the
binding whose `path` is the longest prefix of the looked-up path (on component
boundaries) wins. Resolution yields:

```
(binding, suffix)
```

where `suffix` is the remainder of the looked-up path after the binding's prefix.
For a **direct-handle** binding the suffix must be empty (a direct handle is a
leaf — it has no sub-paths); a non-empty suffix is *not found*. For a
**resource-server** binding (slice 3) the suffix is what the kernel forwards to
the server ("you own `/store`; the client wants `/store` + `config`").

Examples (bindings `/dev` → devfs RS, `/dev/log` → logd RS, `/store` → a
`MemoryObject`):

| Looked-up path | Resolves to | Suffix |
|---|---|---|
| `/dev/log` | `/dev/log` (logd) — longest prefix | `` |
| `/dev/tty0` | `/dev` (devfs) | `tty0` |
| `/store` | `/store` (the MemoryObject, direct) | `` |
| `/store/x` | `/store` direct binding, non-empty suffix → **not found** | — |
| `/net/...` | no covering binding → **not found** | — |

**Rights attenuation.** A lookup requests at most some `rights`; the resolved
handle's rights are `requested ∩ binding.rights`. A binding can only *reduce* what
its subtree grants; a client can only *reduce* further. Authority never amplifies
through resolution — the core capability invariant (`docs/rationale/why-capabilities.md`).

The slice-1 store is a small per-namespace list scanned for the longest match
(bindings are few — a handful per namespace). A prefix trie is a later
optimization if a namespace ever grows large enough to matter; the resolution
*contract* (longest-prefix, suffix, attenuation) is independent of the structure.

## Binding targets

A binding's `target` is one of:

| Target | Meaning | Lands |
|---|---|---|
| **`DirectHandle`** | A bound kernel-object `ObjectRef` (a `MemoryObject`, an `IpcChannel` endpoint, …). Lookup returns the object directly. The leaf case. | **slice 1** |
| **`KernelServer`** | A **Kernel Server**: a dispatch id/function the kernel calls during lookup. The server *computes* a handle synchronously (no IPC). Backs `/proc/self`, `/dev/entropy`, … | **slice 3** |
| **`UserspaceServer`** | An IPC endpoint to a **Userspace Server**. Lookup forwards the suffix over IPC; the server answers with a handle. | slice 7 |
| **`SubNamespace`** | Another `Namespace`, overlaid at the prefix — composition. Lookup recurses into it with the suffix. | later |
| **`Rewrite`** | Rewrite the prefix and re-resolve — aliasing/redirection. | later |

Slice 1 implemented only `DirectHandle` (the `target` field was a bare `ObjectRef`).
Slice 3 introduces the `BindingTarget` **enum** with the `KernelServer` variant
(`DirectHandle` + `KernelServer`); `UserspaceServer` (IPC) lands in slice 7,
`SubNamespace`/`Rewrite` later.

A **direct handle** is a supervisor binding a concrete object — a `MemoryObject` at
`/store`, a channel endpoint, an `EntropyObject` — that lookup returns
(rights-attenuated). A **`KernelServer`** is the answer for resources that must be
*computed per lookup* (e.g. `/proc/self/process`, which depends on *who* is asking)
without spinning up a userspace process — see "In-kernel resource servers" below.

## Lookup is asynchronous

`sys_ns_lookup` returns a **`PendingOperation`** (`docs/architecture/`… the
async-I/O primitive), not a handle directly. The completion delivers either the
resolved resource handle or an error.

This is because the *general* lookup blocks: resolving through a `UserspaceServer`
binding is `syscall → kernel resolves → IPC round-trip to the server → server
returns a handle`. A blocking operation must return a `PendingOperation` and have
the caller block in `sys_wait` — never block inside the syscall (the async-first
rule, `docs/rationale/why-async-syscalls.md`). We commit to that shape **from
slice 1** so the syscall ABI never has to change when forwarding arrives.

**Delivering a handle through a completion.** A `PendingOperation` today completes
with only an `i32` status — it cannot carry a 64-bit handle. So this slice grows
the completion record: `IoResult` gains a **`result: u64`** word (16 → 24 bytes,
`result` at offset 16; existing offsets unchanged — exactly the growth
`docs/spec/` reserved for "richer waitables that report payloads"). A `sys_wait`
on a completed lookup PO returns `IoResult { handle: <the PO>, status: 0,
result: <the resolved handle> }`; on error, `status` is the negative `KError` and
`result` is 0. Edge-style waitables (timers, channels) report `result = 0`.

**Where the resolved handle is installed.**
- **Slice 1 (direct handles)** and **slice 3 (in-kernel servers):** the lookup
  resolves entirely in the caller's syscall context (no IPC) — slice 1 returns the
  bound `ObjectRef`; slice 3 calls the `KernelServer` to *compute* the handle — then
  installs the rights-attenuated handle into the caller's table and returns a
  **pre-signalled** PO whose `result` is that handle. The caller's `sys_wait` returns
  immediately — the full async result path, synchronously, without IPC.
- **Slice 7 (userspace servers):** the lookup forwards over IPC and returns a
  *pending* PO; when the server responds, the kernel installs the handle into the
  **original caller's** table (a cross-context install — a new mechanism that lands
  with that wave) and signals the PO with the result.

Either way the userspace contract is identical: `lookup → PO → sys_wait →
IoResult.result`.

## The capability model

Three handle rights govern a `Namespace` (already defined in
`kernel/src/libkern/handle.rs`; principal mask `LOOKUP | BIND` in
`handle/type_rights.rs`):

- **`LOOKUP`** — resolve names. A client process holds a `LOOKUP`-only handle to
  its namespace.
- **`BIND`** — add a binding.
- **`UNBIND`** — remove a binding.

A handle's rights bound what `sys_ns_*` it may call: `sys_ns_lookup` needs
`LOOKUP`, `sys_ns_bind` needs `BIND`, `sys_ns_unbind` needs `UNBIND`. Because
authority is the handle, **you can only bind into a namespace you hold a
`BIND`-righted handle to** — and supervisors hand clients `LOOKUP`-only handles.
This is the capability-correct gate, and it is what slice 1 enforces.

**`BIND_NAMESPACE` (a system capability) is an *additional* gate**, above the
handle right, that lands with the process-capability model (`SysCaps`), which is
not yet designed. It concentrates *all* namespace mutation in a few coordination
roles (init, service-mgr, session-mgr) so namespace policy has a chokepoint that
can be audited — see `docs/rationale/why-supervisor-registration.md`. Until the
syscap model exists, the handle `BIND` right is the enforced gate; `sys_ns_bind`
will additionally require `BIND_NAMESPACE` once syscaps land. (Both gates apply in
the final design; slice 1 implements the handle-right one.)

**Supervisor-mediated binding.** Resource servers never bind themselves; a
supervisor holding the binding authority does it on their behalf after a Ready
handshake. That protocol is in "Resource servers" below and in the rationale doc.

## The lookup cache

Resolution is a hot path (every open, every fault-in on a file mapping). Each
`Namespace` keeps a small, bounded, pre-reserved **cache of recently-resolved
paths → (binding, attenuated rights)**. A cache entry is invalidated when a bind
or unbind touches a path that could change that entry's resolution (conservatively:
any unbind/bind on the namespace flushes affected entries; v1 may flush the whole
cache on any mutation — mutations are rare, lookups are frequent). The cache is an
optimization with no semantic effect; it is the last code part of slice 1 and can
be added without changing the resolver's contract.

## Resource servers

**Resource server** is the **umbrella** term: anything that owns a namespace subtree
and answers lookups for it by returning an `OpStatus`:

- **`Completed(handle)`** — the answer is ready now (a handle to the resolved
  resource).
- **`Rejected(error)`** — the request is refused.
- **`Pending`** — the answer arrives asynchronously (slice 7; see below).

That umbrella has exactly **two kinds** (children), which share the contract above
but differ in *where the server lives* and *how it is dispatched*:

```
            Resource Server            (umbrella: lookup → OpStatus)
            /                \
   Kernel Server        Userspace Server
   (in-kernel,           (separate process,
    direct dispatch,      over IPC,
    binding target        binding target
    `KernelServer`)       `UserspaceServer`)
       slice 3                 slice 7
```

- A **Kernel Server** (**slice 3**) is a kernel module reached by a **direct function
  call** during lookup. No IPC; answers `Completed`/`Rejected` synchronously. Bound
  by the kernel at boot. Backs `/proc/self`, `/dev/entropy`, the `/dev` stub.
- A **Userspace Server** (**slice 7**) is a separate process reached over **IPC**.
  The normal case for filesystems/devices; inherently async (`Pending`). Bound by a
  supervisor after the Ready handshake.

The two binding-target variants (`KernelServer`, `UserspaceServer`) name the two
children; "resource server" (lowercase) is the umbrella, never a binding kind.

The umbrella earns its keep through **substitutability**: because both children
answer the identical `lookup(suffix, rights) → OpStatus` contract, a path served by a
Kernel Server today can be re-bound to a Userspace Server at the same path tomorrow
with **zero client change** — the caller's `sys_ns_lookup` is unaffected by which kind
sits behind the binding. This is how a facility can graduate from an in-kernel
bring-up implementation to a userspace one (or be sandboxed by swapping in a
restricted server) without touching any consumer.

### Kernel Servers (slice 3)

The slice-3 framework is small because it reuses the slice-1 lookup machinery
end-to-end. A `KernelServer` binding holds a dispatch id into a small kernel
**registry** of server functions:

```
fn lookup(suffix: &[u8], requested: Rights) -> OpStatus   // Completed(handle) | Rejected(err)
```

`sys_ns_lookup` resolves the path to the binding as in slice 1; for a `KernelServer`
target it **calls the server function in the caller's syscall context**, gets a
handle (or error), installs the rights-attenuated handle into the caller's table,
and **pre-signals the lookup's `PendingOperation`** with the result — exactly the
path slice-1 direct handles already take. So an in-kernel lookup is synchronous: the
caller's `sys_wait` returns immediately with the handle in `IoResult.result`. No
IPC, no cross-context install, no new ABI. (`OpStatus::Pending` is **reserved** for
slice 7 — an in-kernel server never returns it.)

**The content model: a lookup yields a handle to a kernel object.** An in-kernel
server's job is to *produce the right handle* for `suffix`:

- `/dev/entropy` → an `EntropyObject` handle (the caller then `sys_entropy_read`s it).
- `/proc/self/process` → the caller's own `Process` handle; `/proc/self/status` → a
  freshly-synthesized read-only `MemoryObject` snapshot; etc.

**Registration is by the kernel at boot, not a handshake.** In-kernel servers are
always present, so the kernel binds them into **pid 1's root namespace** during
boot (a `KernelServer` binding per server) — no Ready handshake, no `BIND_NAMESPACE`
holder needed (that machinery is for *userspace* servers, slice 7). Children inherit
these bindings through the normal namespace inheritance (slice 1, Part D).

### `/proc/self` — self-reference, not ambient authority

`/proc/self/*` is an in-kernel server that resolves to the **caller's own**
resources, derived from the calling syscall context:

| Path | Resolves to | Status |
|---|---|---|
| `/proc/self/process` | the caller's own `Process` handle | **slice 3** |
| `/proc/self/thread` | the calling `Thread` handle | **slice 3** |
| `/proc/self/namespace` | the caller's root-namespace handle | **slice 3** |
| `/proc/self/status` (numeric pid/tid) | a small readable snapshot | deferred |

Each shipped leaf is its **own** `KernelServer` binding with **type-correct rights**
(`process`/`thread` → `SIGNAL | TERMINATE` + generic band; `namespace` → `LOOKUP` +
generic, no `BIND`) — not one `/proc/self` prefix binding, because the returned types
carry disjoint principal rights and a lookup installs `requested ∩ binding.rights`,
which must be valid for the resolved type. The binding is a dispatch id; the *answer*
is the looking-up thread's own object, so one binding is shared by all callers.

Numeric **pid/tid** retrieval (`/proc/self/status`) is **deferred** — pid/tid are
attributes of the `Process`/`Thread` objects a caller now holds, so the eventual
mechanism (a synthesized read-only `MemoryObject` snapshot vs. extending handle
introspection) is itself an open choice; see `docs/rationale/deferred-decisions.md`.

This is **not** ambient authority, on two independent axes:

1. **Reachability is by namespace construction.** `/proc/self` resolves only if a
   supervisor bound it into your namespace; a sandbox may omit it. It is *not* a
   kernel-forced universal — the kernel binds it into pid 1's root by default, and
   normal supervisors propagate it, but a locked-down namespace need not.
2. **It is strictly self-reference.** The server reads the *current* process/thread
   from the calling context — there is **no pid parameter to forge**, so it grants
   nothing about any other process (and returned handles are still owner-pid-checked
   on use, per `handle-encoding.md`).

**Cross-process introspection** (`/proc/<pid>`, enumeration) is a **separate,
narrowly-bound** capability — a distinct server with its own global process registry,
bound only into privileged (init/admin) namespaces, scoped (a "filtered" server sees
only your subtree; a "full" server sees all). It is **deferred** (it needs the
registry and is the ambient-authority-sensitive surface); slice 3 ships only
`/proc/self`. See `os-design-v5.1.md` §"Synthetic /proc/self" + the
namespace-composition examples (standard user → filtered `/proc`; admin → full
`/proc`; sandbox → none).

### Kernel Server shapes: singleton, self-reference, registry-backed

Kernel Servers come in three shapes, distinguished by *how a lookup's suffix maps
to an answer*:

- **Singleton** (`/dev/entropy`) — one resource; the suffix is empty; every lookup
  returns a handle to the same object.
- **Self-reference** (`/proc/self/*`) — the answer is derived from the *calling
  context*, not stored state; no pid/instance parameter exists to forge.
- **Registry-backed (instance) server** (`/dev/blk`, the storage slice) — one
  binding owns a **subtree**, and the suffix (`0`, `1`, …) indexes a **runtime
  registry** of objects (`DeviceNode`s). This is the first server whose answer set
  is *discovered at runtime*.

The registry-backed shape is what reconciles two facts that look in tension:

- **The set of Kernel Server *implementations* is static** — it is the
  `KernelServerId` enum + `dispatch`, i.e. *kernel code*. A new server *kind* can
  appear only by recompiling the kernel or (later) loading a Tier 2 module. This
  is correct: it enumerates the servers compiled into *this* build.
- **The set of *resources* and *binding points* is fully dynamic** — paths are a
  runtime per-process tree, and a server *computes* its answer per lookup, free to
  consult runtime state.

A registry **bridges** the two: you cannot mint a new enum variant per discovered
disk, so one static variant (`BlockDevice`) owns the `/dev/blk` subtree and
resolves the suffix against a dynamic table. (The alternative — baking an instance
key into each binding, one binding per disk — was rejected because block devices
grow sub-paths: partitions, `/dev/disk/by-partuuid/*`. A server owning a *subtree*
is the on-design match to the umbrella definition above.)

### Liveness: what is "live", and what enables it

Until the storage slice every Kernel Server is **unconditionally live** — bound
into pid 1's root at boot, always. As hardware drivers arrive, only *some* servers
matter on a given machine (an AHCI box needs no NVMe support). The enabling logic
splits across three layers, and crucially **drivers, not servers, are what's
conditional**:

1. **Compile-time — Cargo features.** Whether driver/server code is in the binary
   at all (Tier 1 features `ahci`, `nvme`, `gpt`; see `drivers-and-irps.md`).
2. **Runtime, drivers — device matching.** A **driver** (AHCI, NVMe — *not* a
   server; see `drivers-and-irps.md` § "Three concepts, kept distinct") goes live
   when a matching `DeviceNode` appears during enumeration. On an AHCI-only box the
   NVMe driver's match never fires and its code stays cold — **hardware presence
   *is* the enable**; no flag is needed.
3. **Runtime, servers — binding.** A server is "live" when a **supervisor** binds
   it (servers never self-enable — `why-supervisor-registration.md`). Today that
   supervisor is the kernel at boot.

For a registry-backed instance server, layer 3 needs **no per-server enable
switch**: bind `/dev/blk` *unconditionally* (uniform with `/dev/entropy`), and let
the **registry carry liveness**. Whichever drivers matched hardware (layer 2)
populate it; `/dev/blk/0` resolves iff a disk is registered, else `NotFound`; if no
block driver matched, the server is bound but inert — harmless. So in Phase 2 the
*only* conditional thing is the driver match. End-state, layers 2–3 graduate to a
userspace **device manager** + supervisors (driver-to-node policy, especially for
Tier 2 where a driver *process* receives a `Handle<DeviceNode>`); substitutability
lets `/dev/blk/0` move from a kernel-served node to a Tier 2 userspace endpoint with
zero client change.

### Userspace Servers (slice 7)

A Userspace Server is a process reached over IPC: a `UserspaceServer` binding points
at its endpoint, lookup **forwards** the suffix over IPC, the server answers with a
handle, and the kernel **installs it cross-context** into the original caller's table
(the `Pending` path — the lookup's `PendingOperation` completes when the server
replies). This is built with the **fs-server** (the first Userspace Server) and needs:
the `UserspaceServer` binding target, IPC-forwarded lookup, the cross-context install,
the `librsproto` codec (`docs/spec/rsproto-wire-format.md`), and the registration
handshake below. Kernel Servers (slice 3) need **none** of it.

**Registration: the Ready handshake.** Userspace servers **do not register
themselves** (`why-supervisor-registration.md`). A supervisor:

1. spawns the RS with what it needs (a block-device handle, a log channel, a
   minimal own-namespace, and a **control channel**) — but **not** `BIND_NAMESPACE`;
2. the RS initializes, creates its serving endpoint (`sys_channel_create`, keeping
   the receive end), and sends **Ready** (with the endpoint handle) on the control
   channel (`docs/spec/rsproto-wire-format.md` § Ready);
3. the supervisor receives Ready and calls `sys_ns_bind(ns, path, endpoint, rights)`
   to bind the endpoint at the chosen path with chosen rights;
4. lookups resolving to `path` are now routed to the endpoint.

The control channel persists as the supervisor↔RS management channel (shutdown,
reload, health, swap-in-place).

## Kernel vs userspace split

| Concern | Where |
|---|---|
| `Namespace` object: hold bindings, resolve, attenuate rights, cache | **kernel** (slice 1) |
| `sys_ns_create/bind/unbind/lookup` | **kernel** (slice 1) |
| `KernelServer` binding target + in-kernel dispatch registry | **kernel** (slice 3) |
| In-kernel servers (`/proc/self`, `/dev/entropy`, `/dev` stub) + boot binding | **kernel** (slice 3) |
| Routing a resolved lookup to a server endpoint over IPC; the registry | **kernel** (slice 7) |
| Cross-context handle install on RS response | **kernel** (slice 7) |
| Userspace Server impl: `OpStatus` codec + the rsproto wire format | **userspace** (`librsproto`, slice 7) |
| Deciding *what* to bind *where* (system construction) | **userspace** supervisors (init/service-mgr) |

The boundary is strict: the kernel never touches a server's internal state or
policy; a server never touches namespace bindings.

## The syscalls

Full signatures and error space: `docs/spec/syscall-abi.md`. In brief:

- **`sys_ns_create() -> handle`** — a fresh, empty `Namespace` with full namespace
  rights (`LOOKUP | BIND | UNBIND` + generic management).
- **`sys_ns_bind(ns, path, path_len, resource) -> 0`** — bind `resource` (a direct
  handle in slice 1; a userspace-server endpoint in slice 7) at `path`. Needs `BIND`
  on `ns` (and, later, the `BIND_NAMESPACE` syscap). In-kernel `KernelServer`
  bindings are made by the kernel at boot, not through this syscall.
- **`sys_ns_unbind(ns, path, path_len) -> 0`** — remove the binding at `path`. Needs
  `UNBIND`.
- **`sys_ns_lookup(ns, path, path_len, rights) -> PendingOperation`** — resolve
  `path`, requesting at most `rights`; the PO completes with the resolved handle
  (`IoResult.result`) or an error (`IoResult.status`). Needs `LOOKUP`.

Numbers `22`–`25` (reserved in the spec; pre-stabilization).

## Scope summary

| Item | Slice 1 (substrate) | Slice 3 (in-kernel servers) | Slice 7 (userspace servers) |
|---|---|---|---|
| `Namespace` object + binding store | ✅ | | |
| Longest-prefix resolution + attenuation | ✅ | | |
| `DirectHandle` bindings | ✅ | | |
| `sys_ns_create/bind/unbind/lookup` | ✅ | | |
| Async lookup contract (`PO` + `IoResult.result`) | ✅ (pre-signalled) | (reused, synchronous) | (real forwarding) |
| `BIND` handle-right gate | ✅ | | |
| `BIND_NAMESPACE` syscap gate | (deferred — syscap model) | | |
| Lookup cache | ✅ | | |
| Per-process `Namespace` + spawn inheritance | ✅ | | |
| `BindingTarget` enum + `KernelServer` dispatch + registry | | ✅ | |
| In-kernel servers (`/dev/entropy`, `/proc/self`, `/dev` stub) + boot binding | | ✅ | |
| `OpStatus` (`Completed`/`Rejected`; `Pending` reserved) | | ✅ | (`Pending`) |
| `UserspaceServer` (IPC) target + forwarded lookup + cross-context install | | | ✅ |
| `librsproto` codec + Ready handshake | | | ✅ |
| Cross-process `/proc` (filtered/full) + process registry | | (deferred) | (deferred) |
| `SubNamespace` / `Rewrite` targets | | | (designed; later) |

## Where to read more

- `docs/rationale/why-supervisor-registration.md` — why supervisors bind, not servers
- `docs/rationale/why-capabilities.md` — the authority model resolution attenuates within
- `docs/architecture/ipc.md` — the channels resource-server forwarding rides on
- `docs/spec/syscall-abi.md` — the `sys_ns_*` ABI and `IoResult`
- `docs/spec/rsproto-wire-format.md` — the resource-server wire format (slice 3)
