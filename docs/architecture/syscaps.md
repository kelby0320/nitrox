# System capabilities (SysCaps)

**Status:** Implemented (Phase 3 slice 6, 2026-07-14). Living document. The type,
the `Process.syscaps` field, spawn inheritance, the init boot grant, and the two wired
gates (`BIND_NAMESPACE` on `sys_ns_bind`, `REAL_TIME` on the RT scheduling class) are
in; the other four caps are defined and inherited, their gates deferred to their
operations' slices.

**SysCaps** are the kernel's second axis of authority: **ambient, per-process
capabilities** for privileged *operations*, distinct from per-handle
[`Rights`](handle-system.md), which authorize operations on a *specific object*. A
process either holds `BIND_NAMESPACE` or it doesn't; there is no handle to point at.
They are the mechanism behind "authority is granted, never assumed" at the
whole-process level, and the last defining kernel feature still missing (authority is
faked with handle-`Rights` stand-ins today).

## Two axes of authority

| | **`Rights`** (exists) | **`SysCaps`** (this slice) |
|---|---|---|
| Scope | one handle → one object | the whole process |
| Question | "may I do X *to this object*?" | "may I do X *at all*?" |
| Held in | a handle-table entry | a field on `Process` |
| Example | `BIND` on *this* namespace handle | `BIND_NAMESPACE` — allowed to construct namespaces |
| Acquired | handed a handle | granted at spawn (⊆ parent) |

The two **compose**: `sys_ns_bind` requires *both* the `BIND` right on the target
namespace handle (you may bind into *this* namespace) *and* the `BIND_NAMESPACE`
syscap (you are a process permitted to do namespace construction at all). Neither
subsumes the other — `Rights` scopes authority to objects; SysCaps concentrate
privileged *classes of operation* in the few processes that need them.

## The capability set

Six capabilities (the v5.1-committed set; `docs/history/os-design-v5.1.md` §"System
Capability Bitmask"). Each is one bit of a `u64`:

| Bit | Capability | Gates | Wired |
|---|---|---|---|
| `1<<0` | `LOAD_MODULE` | loading/unloading Tier-2 LKMs | **defined only** (no loader yet) |
| `1<<1` | `BIND_NAMESPACE` | `sys_ns_bind`, `sys_ns_release_initramfs` | **this slice** |
| `1<<2` | `PHYSICAL_MEMORY` | mapping arbitrary physical memory | **defined only** (no phys-map syscall) |
| `1<<3` | `REAL_TIME` | requesting the `RealTime` scheduling class | **this slice** |
| `1<<4` | `SYSTEM_CLOCK` | setting the realtime-clock offset | **defined only** (clock is Monotonic-only) |
| `1<<5` | `AUDIT_CONTROL` | audit-subsystem management | **defined only** (no audit subsystem) |

**All six are *defined*** (the type + bitmask + the inheritance model need the full
set to be meaningful, and init holds all of them). **Only two are *wired* to a gate**
this slice — `BIND_NAMESPACE` and `REAL_TIME` — because those are the only ones with a
gate-able operation that exists today. The other four are held, inherited, and
attenuated like any bit, but nothing checks them yet; their gate is added by the slice
that builds the operation (module loading, phys-map, clock-offset, audit). This is the
same "wire what has a consumer" discipline used across Phase 3 — the model is complete,
the enforcement is only as broad as the operations that exist.

## The type

A hand-rolled `SysCaps(u64)`, mirroring `Rights`'s style (the kernel forbids the
`bitflags` crate). It is an ABI type — `SpawnArgs` carries it across the syscall
boundary — so it lives in `kernel/src/libkern/syscaps.rs` and is mirrored in
`userspace/libkern/src/syscaps.rs`, exactly like `Rights`:

```rust
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct SysCaps(u64);

impl SysCaps {
    pub const LOAD_MODULE:     SysCaps = SysCaps(1 << 0);
    pub const BIND_NAMESPACE:  SysCaps = SysCaps(1 << 1);
    pub const PHYSICAL_MEMORY: SysCaps = SysCaps(1 << 2);
    pub const REAL_TIME:       SysCaps = SysCaps(1 << 3);
    pub const SYSTEM_CLOCK:    SysCaps = SysCaps(1 << 4);
    pub const AUDIT_CONTROL:   SysCaps = SysCaps(1 << 5);

    pub const fn empty() -> SysCaps;
    pub const fn all() -> SysCaps;              // the full boot set
    pub const fn bits(self) -> u64;
    pub const fn from_bits_truncate(bits: u64) -> SysCaps;   // ignore unknown bits
    pub const fn contains(self, other: SysCaps) -> bool;
    pub const fn is_subset_of(self, other: SysCaps) -> bool; // (self & other) == self
    // BitOr / BitAnd, like Rights.
}
```

`from_bits_truncate` (masking unknown bits) is deliberate: a `SpawnArgs.syscaps` word
from userspace may set reserved bits; the kernel ignores them rather than rejecting,
and the ⊆-parent check (below) already bounds what actually takes effect.

## Storage: a field on `Process`

`Process` (`kernel/src/object/process.rs`) reserves the slot in its docstring today;
this slice fills it:

```rust
pub struct Process {
    header: KObjectHeader,
    pid: u32,
    // …
    syscaps: SysCaps,   // NEW: this process's ambient authority
}
```

Constructors take the set: `try_new_user(pid, address_space, syscaps)`. A `syscaps()`
accessor reads it. Syscaps are **immutable after spawn** — a process's authority is
fixed at creation; it can only ever *drop* authority by spawning a less-privileged
child. There is no "gain a capability" syscall (privilege escalation is *handle
acquisition* from a broker, not a state change — v5.1 §"Privilege Escalation").

## Inheritance: granted at spawn, ⊆ parent

Authority flows down the process tree and only ever attenuates:

- **`SpawnArgs` grows a `syscaps: u64` field** (see ABI changes). The parent names the
  capability set the child should hold.
- **`sys_process_spawn` intersects it with its own**: `child.syscaps = parent.syscaps
  & args.syscaps`. A parent can never grant a capability it does not hold (the v5.1
  "SysCaps must be ⊆ parent's" rule). No amplification — mirrors `Rights` on handles.
- The child's `Process` is created with that set; every syscap check the child later
  makes consults it.

This is the whole enforcement model for *distribution*: supervisors hand out subsets.
`BIND_NAMESPACE` illustrates the intent — init holds it, delegates it to the service
manager and session manager (coordination processes that construct namespaces), and
**never** grants it to an ordinary resource server (which registers via the
supervisor, per the [RS Startup Protocol](namespace-and-resource-servers.md)).

## The boot grant: init holds the full set

All authority traces to a single kernel grant. When the kernel spawns init
(`run_first_userspace`, `kernel/src/main.rs`), it creates init's `Process` with
`SysCaps::all()`. Every other process's authority is a subset init (or its delegates)
chose to pass. The kernel itself is above the model — its boot-time
`bind_kernel_server` calls are internal, not syscalls, so they need no syscap.

## Enforcement: the check point

A syscap check sits in the handler, **immediately after the caller's `Process` is
resolved** (`crate::sched::current_process()`), in the same slot the `Rights` gates
occupy now. A small helper keeps it uniform:

```rust
// in the syscall table
fn require_syscap(cap: SysCaps) -> Result<(), KError> {
    let proc = crate::sched::current_process().ok_or(KError::KernelError)?;
    if Process::syscaps_of(&proc).contains(cap) { Ok(()) } else { Err(KError::NoAccess) }
}
```

A missing syscap returns **`KError::NoAccess`** — the same error a missing handle
right returns, so userspace sees one "insufficient authority" story.

### The two gates wired this slice

**`BIND_NAMESPACE` — on `sys_ns_bind`** (`table.rs:1025`) and
`sys_ns_release_initramfs`. The existing `Rights::BIND`-on-the-handle check stays; the
syscap is an *additional* gate checked first:

```rust
pub fn sys_ns_bind(ns_h, path, len, resource_h) -> SysResult {
    require_syscap(SysCaps::BIND_NAMESPACE)?;                    // NEW
    let ns_ok = lookup_typed(ns_h, pid, Rights::BIND, Namespace)?;  // unchanged
    // …
}
```

Effect: **namespace construction becomes a supervisor-only privilege.** A process
without `BIND_NAMESPACE` cannot bind *even into a namespace it created itself* — which
is the point (sandboxes receive pre-constructed namespaces; they don't build them).

**`REAL_TIME` — on requesting the `RealTime` scheduling class.** Slice 2 built the
three-class scheduler but left user threads always `TimeShared`; this slice finalizes
the user path. `sys_thread_create` reads the (now-populated) `ThreadArgs` scheduling
fields; if `class == RealTime` and the caller lacks `REAL_TIME`, it is rejected
(`NoAccess`). `nice` and CPU-affinity-at-creation are **ungated** (pinning/renicing
your own thread is not privileged). Trusted kernel threads keep setting the class
directly via `spawn_with_class` (they bypass the syscall).

### The other four: defined, gate deferred to their slice

`LOAD_MODULE`, `PHYSICAL_MEMORY`, `SYSTEM_CLOCK`, `AUDIT_CONTROL` have no operation to
gate yet (no module loader, no phys-map syscall, Monotonic-only clock, no audit
subsystem). They are defined and flow through inheritance; the slice that builds each
operation adds the one-line `require_syscap(...)` at its entry. This doc is the
registry of which bit that slice uses.

## ABI changes

Two syscall-argument structs grow. Both are the **userspace↔kernel syscall ABI**
(passed by pointer to a syscall), self-pinned by compile-time `size_of`/`offset_of`
asserts + their spec docs — *not* the Tier-2 module-boundary layouts in
`abi-version-hash.md`. Growing them is a deliberate pre-v1 syscall-ABI change tracked
by the spec docs + the asserts.

- **`SpawnArgs`** (`kernel/src/libkern/spawn.rs`, mirrored `userspace/libkern`) gains
  `syscaps: u64` appended after `namespace` — **96 → 104 bytes**. Every spawn call
  site sets it (0 = an unprivileged child). Update the mirror + the compile-asserts +
  `docs/spec/process-spawn-args.md`.
- **`ThreadArgs`** (`kernel/src/libkern/thread.rs`) uses its existing **`_reserved:
  [u8; 40]`** growth room (size stays **64**): a `class: u8`, `rt_priority: u8`,
  `nice: i8`, and `cpu_affinity` sub-layout, with the rest still reserved.
  `sys_thread_create` already rejects a nonzero reserved block — this replaces that
  with real field parsing (unknown/reserved bytes still rejected). Update the mirror +
  asserts + `docs/spec/thread-args.md`.

> **Hash-doc reconciliation (small, tracked separately):** the source comments on
> `SpawnArgs`/`ThreadArgs` call themselves "ABI-hash inputs like IpcMsg," but
> `abi-version-hash.md` does not list them (nor `IpcMsg`). They don't cross the
> module boundary, so they don't belong in the *module* hash; the fix is to correct
> those comments to say "syscall-ABI, self-pinned by asserts + spec," not to add them
> to the hash. Filed as a doc cleanup, not a blocker.

## Migration: turning the `BIND_NAMESPACE` gate on

Enabling the gate makes every current `ns_bind` caller need `BIND_NAMESPACE`:

- **init** holds the full set → unaffected (it binds fs-server endpoints).
- **The `parent` demo** does `ns_create` + `ns_bind` on a fresh namespace. Once the
  gate is on, that bind requires `BIND_NAMESPACE`, so init must grant it to `parent`
  in the demo's `SpawnArgs.syscaps` (or the demo is adjusted to illustrate the denial).
- **The `child` demo** already *expects* its bind to be denied (LOOKUP-only) — it stays
  denied, now for the additional reason that it holds no `BIND_NAMESPACE`.
- Kernel-server bindings at boot are internal (not syscalls) → unaffected.

The verification must confirm the healthy boot still reaches `eshell` with the demos
passing under the new gate (init/parent granted appropriately), *and* that a process
*without* `BIND_NAMESPACE` is denied `ns_bind`.

## Scope (slice 6)

- **In:** the `SysCaps` type (kernel + userspace mirror); the `Process.syscaps` field
  + inheritance (`child = parent & args.syscaps`) in `sys_process_spawn`; the boot
  grant (init = full set); the `BIND_NAMESPACE` gate on `ns_bind`/`ns_release_initramfs`;
  the `REAL_TIME` gate + the finalized `ThreadArgs` class/nice/affinity ABI; the
  `SpawnArgs.syscaps` field.
- **Defined, not wired:** `LOAD_MODULE`, `PHYSICAL_MEMORY`, `SYSTEM_CLOCK`,
  `AUDIT_CONTROL` (their gates land with their operations).
- **Out:** libos wrappers for the new spawn/thread ABI (that's the slice-7
  authority-facing libos surface); any userspace privilege broker / policy.

## Host-testability

The `SysCaps` type (bit ops, subset, `all`/`empty`, `from_bits_truncate`) is pure and
host-tested like `Rights`. The inheritance intersection (`parent & args`) and the
`require_syscap` decision are host-testable against a constructed `Process`. The
end-to-end gate (ns_bind denied without the cap, RT rejected without it) is verified
under QEMU.

## References

- `docs/history/os-design-v5.1.md` §"System Capability Bitmask", §"Capability
  Bootstrap", §"Policy vs. Mechanism" — the committed model.
- `docs/rationale/why-capabilities.md`; `docs/rationale/why-supervisor-registration.md`.
- `docs/architecture/handle-system.md` — the `Rights` axis SysCaps complements.
- `docs/architecture/scheduler.md` — the `RealTime` class the `REAL_TIME` cap gates.
- `docs/spec/process-spawn-args.md`, `docs/spec/thread-args.md` — the ABIs that grow.
- `docs/planning/implementation-plan.md` slice 6; the 2026-07-13 decision-log
  sequencing entries.
