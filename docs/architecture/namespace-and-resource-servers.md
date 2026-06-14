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

> **Implementation phasing.** This doc designs the whole model, but it lands in two
> waves. **Slice 1 (the namespace substrate)** builds the `Namespace` object,
> resolution, the four `sys_ns_*` syscalls, and **direct-handle** bindings.
> **Slice 3 (resource servers)** adds the `ResourceServer` trait, `OpStatus`, the
> registry, and **IPC-forwarded** lookup. Each section below marks which wave it
> belongs to. The async lookup *contract* is fixed in slice 1 (see "Lookup is
> asynchronous") precisely so slice 3 needs no ABI change.

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

A binding's `target` is one of (the enum is fixed now; only `DirectHandle` is
*implemented* in slice 1):

| Target | Meaning | Lands |
|---|---|---|
| **`DirectHandle`** | A bound kernel-object `ObjectRef` (a `MemoryObject`, an `IpcChannel` endpoint, …). Lookup returns the object directly. The leaf case. | **slice 1** |
| **`ResourceServer`** | An IPC endpoint to a resource server. Lookup forwards the suffix over IPC; the server answers with a handle. | slice 3 |
| **`SubNamespace`** | Another `Namespace`, overlaid at the prefix — composition. Lookup recurses into it with the suffix. | slice 3+ |
| **`Rewrite`** | Rewrite the prefix and re-resolve — aliasing/redirection. | later |

Slice 1 binds **direct handles**: a supervisor binds, say, a `MemoryObject` at
`/store` or a channel endpoint at `/dev/log`, and a client lookup returns that
object (rights-attenuated). This is enough to exercise the entire create → bind →
lookup → use path before any resource server exists, and it is exactly how
in-kernel leaf resources (`/dev/null`, a framebuffer object) are exposed.

## Lookup is asynchronous

`sys_ns_lookup` returns a **`PendingOperation`** (`docs/architecture/`… the
async-I/O primitive), not a handle directly. The completion delivers either the
resolved resource handle or an error.

This is because the *general* lookup blocks: resolving through a `ResourceServer`
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
- **Slice 1 (direct handles):** the lookup resolves entirely in the caller's
  syscall context (no IPC), installs the rights-attenuated handle into the caller's
  table, and returns a **pre-signalled** PO whose `result` is that handle. The
  caller's `sys_wait` returns immediately — the full async result path, exercised
  without any resource-server machinery.
- **Slice 3 (resource servers):** the lookup forwards over IPC and returns a
  *pending* PO; when the server responds, the kernel installs the handle into the
  **original caller's** table (a cross-context install — a new mechanism that lands
  with this wave) and signals the PO with the result.

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

## Resource servers (slice 3)

*Designed here; implemented with the resource-server slice.*

A **resource server** is a process (or in-kernel module) that owns a subtree of a
namespace and answers lookups/operations for it. A binding of kind
`ResourceServer` points at the server's IPC endpoint; the kernel routes resolved
requests there.

- **The `ResourceServer` contract** (userspace, `librsproto`): a server handles
  `lookup`, `submit`, `cancel` over the rsproto wire format
  (`docs/spec/rsproto-wire-format.md`). It returns an `OpStatus`:
  - **`Completed`** — answer is ready now (in-kernel servers, cache hits).
  - **`Pending`** — answer comes asynchronously (the normal userspace case: the
    IPC round-trip is inherently async).
  - **`Rejected`** — the request is refused (with an error).
- **The `ResourceServerRegistry`** (kernel): the flat set of bound server endpoints
  the resolver routes to — effectively the `ResourceServer` bindings across
  namespaces, indexed for dispatch.
- **In-kernel resource servers** (`/proc`, `/dev`, `/initramfs`) implement the same
  contract but are dispatched by a direct function call, bypassing IPC, and can
  answer `Completed` synchronously.

### Registration: the Ready handshake

Resource servers **do not register themselves** (`why-supervisor-registration.md`).
A supervisor:

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
| Routing a resolved lookup to a server endpoint over IPC; the registry | **kernel** (slice 3) |
| Cross-context handle install on RS response | **kernel** (slice 3) |
| `ResourceServer` trait, `OpStatus`, the rsproto codec | **userspace** (`librsproto`, slice 3) |
| Deciding *what* to bind *where* (system construction) | **userspace** supervisors (init/service-mgr) |

The boundary is strict: the kernel never touches a server's internal state or
policy; a server never touches namespace bindings.

## The syscalls

Full signatures and error space: `docs/spec/syscall-abi.md`. In brief:

- **`sys_ns_create() -> handle`** — a fresh, empty `Namespace` with full namespace
  rights (`LOOKUP | BIND | UNBIND` + generic management).
- **`sys_ns_bind(ns, path, path_len, resource) -> 0`** — bind `resource` (a direct
  handle in slice 1; an endpoint in slice 3) at `path`. Needs `BIND` on `ns` (and,
  later, the `BIND_NAMESPACE` syscap).
- **`sys_ns_unbind(ns, path, path_len) -> 0`** — remove the binding at `path`. Needs
  `UNBIND`.
- **`sys_ns_lookup(ns, path, path_len, rights) -> PendingOperation`** — resolve
  `path`, requesting at most `rights`; the PO completes with the resolved handle
  (`IoResult.result`) or an error (`IoResult.status`). Needs `LOOKUP`.

Numbers `22`–`25` (reserved in the spec; pre-stabilization).

## Scope summary

| Item | Slice 1 (substrate) | Slice 3 (resource servers) |
|---|---|---|
| `Namespace` object + binding store | ✅ | |
| Longest-prefix resolution + attenuation | ✅ | |
| `DirectHandle` bindings | ✅ | |
| `sys_ns_create/bind/unbind/lookup` | ✅ | |
| Async lookup contract (`PO` + `IoResult.result`) | ✅ (pre-signalled) | (real forwarding) |
| `BIND` handle-right gate | ✅ | |
| `BIND_NAMESPACE` syscap gate | (deferred — syscap model) | |
| Lookup cache | ✅ | |
| Per-process `Namespace` + spawn inheritance | ✅ | |
| `ResourceServer` / `OpStatus` / registry | | ✅ |
| IPC-forwarded lookup + cross-context install | | ✅ |
| `SubNamespace` / `Rewrite` targets | | (designed; later) |

## Where to read more

- `docs/rationale/why-supervisor-registration.md` — why supervisors bind, not servers
- `docs/rationale/why-capabilities.md` — the authority model resolution attenuates within
- `docs/architecture/ipc.md` — the channels resource-server forwarding rides on
- `docs/spec/syscall-abi.md` — the `sys_ns_*` ABI and `IoResult`
- `docs/spec/rsproto-wire-format.md` — the resource-server wire format (slice 3)
