# Service Manager (`service-mgr`)

Design doc for `service-mgr`, the userspace process supervisor. Status:
**pre-implementation** — this is the design; the crate does not exist yet. It is
the first entry in the Phase 3 service backlog.

## What it is

`service-mgr` is the userspace daemon that starts, supervises, and restarts the
system's services. It is spawned by `init` once the critical-path boot is stable,
receives a delegated subset of `init`'s capabilities, and from then on owns the
service ecosystem: init reaps orphaned processes and handles shutdown, but does not
supervise services — that is `service-mgr`'s job.

It is the userspace counterpart to what an init system (systemd, launchd, SysV
init + inittab) does on a Unix, but with Nitrox's structure: services are addressed
and granted authority through **namespaces and handles**, not UIDs or ambient
filesystem permissions; a service is registered into the namespace **by the
supervisor holding `BIND_NAMESPACE`**, never by the service itself (see
[why-supervisor-registration](../rationale/why-supervisor-registration.md)).

## Responsibilities

From the Phase 3 implementation plan (§ "Service manager"):

1. **Parse service declarations** — TOML per [service-toml-schema](../spec/service-toml-schema.md).
2. **Build a dependency graph** from `after`/`before`/`wants` and start services in
   topological order (reject cycles at parse time).
3. **Supervise** running services: observe exits via the notification channel and
   apply a **restart policy** (`never` / `on-failure` / `always`).
4. **Back off** restarts (`none` / `linear` / `exponential`, bounded by
   `max_attempts` and `backoff_max`).
5. **Run the Resource Server Startup Protocol** for RS-style services (spawn with a
   control channel → await `Meta::Ready` → bind the service's endpoint into the
   namespace).
6. **Lifecycle control** via a per-service control IPC channel (shutdown, health
   check, config reload) — `service-mgr` keeps one end of each.

## Where it sits in the boot flow

```
kernel_main
  └─ run_first_userspace → init (PID 1)
       ├─ read /etc/init.toml, process critical-path mounts (fs-server-ext4 → bind /)
       ├─ read /system/current-generation, spawn + bind the system profile server
       ├─ delegate a capability subset + spawn service-mgr   ← THE HANDOFF
       ├─ release the initramfs (once boot is stable)
       └─ reap loop (orphans + shutdown notifications)
                                    │
service-mgr ────────────────────────┘
  ├─ read service declarations, build the dependency graph
  ├─ start services in dependency order (RS protocol for RS-style ones)
  ├─ supervise: reap + restart per policy
  └─ lifecycle control channels
```

**The handoff point already exists.** After the boot-normalization pass, init's
`supervise()` launches the interactive console directly, with the standing note
"when the service manager lands, the normal path spawns *it* here instead of
eshell." So the integration is: init's normal (non-selftest) path spawns
`service-mgr` rather than `eshell`; `eshell` remains the **emergency** path (a
critical-path boot failure still drops to the recovery shell) and the `selftest`
demo path. init continues its reap loop underneath (service-mgr is init's child; if
service-mgr itself dies, that is a system-level failure init logs).

## The init / service-mgr boundary

service-mgr takes on most of what a classical init does — and that is intended. The
boundary is:

- **init = the irreducible PID-1 roles** — things that are *structurally* PID-1-only
  or must exist before service-mgr does. Non-declarative, must-never-crash, backstop.
- **service-mgr = the policy-driven service ecosystem** — everything expressible as a
  declaration and supervised.

**Litmus test:** *could this be written as a `service.toml` and supervised?* → it is
service-mgr's. *Does it require being the kernel's first process / the reparent target
/ the thing that exists when nothing else does?* → it is init's.

init's irreducible list is short and bounded:

1. **Receive the initial kernel handle set** (root namespace, notification channel,
   full syscaps) — only PID 1 gets these from the kernel.
2. **Critical-path bootstrap to *reach* service-mgr** — mount the root fs, read the
   manifest. service-mgr's binary and declarations live behind a mounted root, so
   this must precede it.
3. **Reaper of last resort** — the kernel reparents *all* orphans to PID 1 (creator-
   based reparenting, `overview.md`). This reaping is **split**: service-mgr reaps its
   *own* service children to drive restart; a service's grandchildren, or anything
   orphaned when service-mgr itself dies, land on init. Two levels, not a conflict.
4. **Release the initramfs** once boot is stable.
5. **Terminal shutdown/reboot + emergency backstop** — **shutdown is split**:
   service-mgr does *graceful, dependency-ordered* service teardown (via the control
   channels); init does the *terminal* step (it is the last process) and is the
   recovery backstop when service-mgr can't come up or dies.

Everything else — even things init *could* spawn (init may spawn more than one
process) — should be a service. init spawns only the **irreducible minimum to reach
service-mgr** (the root fs-server; eventually the profile server *if* declarations
move to `/store`) plus the emergency eshell.

**The bootstrap ordering to respect:** long-term, declarations come from `/store`
projected by the profile server — but service-mgr needs its declarations *to start*.
So init must bring up "enough" (root fs, later the profile server) before service-mgr
reads anything: init owns the *minimum substrate*, service-mgr owns *everything
policy-driven on top*. Slice A sidesteps the chicken-and-egg by reading declarations
from the **initramfs**.

## Capability posture

`init` holds the full initial `SysCaps` set. It delegates to `service-mgr` **only
the subset service-mgr legitimately needs**, using the kernel's spawn-time rule
`child_syscaps = parent_syscaps & args.syscaps` (the kernel rejects any attempt to
amplify — a child can never gain a capability its parent lacks).

| SysCap | service-mgr holds? | Why |
|---|---|---|
| `BIND_NAMESPACE` | **yes — own use** | It registers each service's endpoint into the system namespace (the RS protocol's bind step). This is the defining supervisor capability. It also *re-delegates* `BIND_NAMESPACE` to `session-mgr` (an already-made decision — session-mgr gets its scoped bind capability from service-mgr). |
| `LOAD_MODULE` | **yes — pass-through** | Not used by service-mgr directly; held so it can *delegate* it to the `device-manager` service (delegation can only attenuate — to grant a cap, you must hold it). |
| `SYSTEM_CLOCK` | **yes — pass-through** | Same: held to delegate to a `time-sync` service, not exercised by service-mgr itself. |
| `PHYSICAL_MEMORY` | **no** | Only `init` keeps this, for extreme recovery. A supervisor has no business with raw physical memory (called out explicitly in `userspace/init/CLAUDE.md`). |
| `REAL_TIME` / `AUDIT_CONTROL` | **no** | No service-mgr need; a service wanting `REAL_TIME` acquires it another way, `AUDIT_CONTROL` belongs to the audit service's own grant path. |

Each *service* then receives `service_syscaps = servicemgr_syscaps & decl.syscaps`
— enforced **twice**: at parse time by service-mgr (the schema requires a
declaration's `syscaps` to be a subset of what service-mgr holds) and again at spawn
time by the kernel (`child = parent & args`, which silently masks any cap the parent
lacks). **Most services get `[]`** (zero ambient capability); authority comes from
the handles granted in `[service.<name>.handles]`, not from syscaps.

**Binding is two-gated.** Every `sys_ns_bind` service-mgr issues is checked against
*both* the ambient `BIND_NAMESPACE` syscap *and* `Rights::BIND` on the specific
namespace handle being bound into. So service-mgr should hold BIND-righted handles
only to the subtrees it actually manages — matching the rationale doc's "delegated
`BIND_NAMESPACE` for the subtrees it manages." Holding the ambient cap is necessary
but not sufficient; the per-handle right scopes *where* it can bind.

**init retains `BIND_NAMESPACE` for life — and that's fine.** Syscaps are *immutable
after spawn* (`syscaps.md`): a process sheds authority only by spawning a
less-privileged child, never from itself — there is no self-attenuation syscall. So
init cannot drop `BIND_NAMESPACE` after the handoff; it keeps its full cap set for
its whole life. (init's "delegate and drop" discipline is real for **handles** — it
closes them — but does not apply to syscaps.) This is low-risk: init is tiny,
critical-path, and audited. The tighter posture (init actually shedding the cap
post-handoff) would need a monotonic self-attenuation syscall — capability-safe, but
a mutation path the design deliberately avoids; **deferred**, not adopted.

**Recovery: service-mgr death → reboot / emergency, not respawn.** A naive respawn is
unsound regardless of capabilities: service-mgr's death orphans all its services
(they reparent to init), kills their control channels, and leaves its namespace
bindings stale in the system namespace — a fresh service-mgr would have to *re-adopt*
that live state (a real checkpoint/re-attach feature, not a respawn). So service-mgr
exiting is a **critical fault**: init logs it and reboots (once a reboot mechanism
exists) or drops to the emergency eshell until then. init's retained `BIND_NAMESPACE`
is latent capability we do not lean on — available if a restart-aware service-mgr is
ever built.

## The service lifecycle

```
        parse
   ┌──────────────┐
   │              ▼
 declaration → [valid] ──start──▶ [starting] ──Ready/running──▶ [running]
   │              ▲                    │                            │
   └▶[misconfigured]                   ▼ (start fails)              ▼ (exits)
      (skip, report)              [failed-to-start]           ┌─ policy ─┐
                                                              ▼          ▼
                                                        [restarting]  [stopped]
                                                          (backoff)   (never / gave up)
                                                              │
                                                              └──▶ back to [starting]
```

- **Parse** → `valid` or `misconfigured` (skipped, logged; per the schema's
  parse-time validation: required fields, subset syscaps, acyclic graph, known
  restart policy, valid handle kinds).
- **Start** in dependency order → `starting`; an RS-style service reaches `running`
  when it sends `Meta::Ready`; a plain service is `running` once spawned.
- **Exit** → consult the restart policy; `on-failure` restarts only on abnormal
  exit (non-zero code / crash / killed), `always` on any exit, `never` not at all.
- **Restart** honours backoff and `max_attempts`; after giving up, the service is
  `failed` and logged, no further attempts unless explicitly requested.

## The Resource Server Startup Protocol (generalized)

`service-mgr` generalizes exactly what `init` already does for `fs-server-ext4`
today (`userspace/init/src/main.rs`), the canonical template:

1. **Create a control channel pair.** Keep one end; the other is moved to the
   service at spawn.
2. **Spawn** the service (`SYS_PROCESS_SPAWN` with `SpawnArgs`), moving the control
   endpoint in via `handles[]` + `move_mask`, granting the declared namespace/
   resource/log handles, and setting `syscaps = servicemgr_syscaps & decl.syscaps`.
3. **Send the setup message** on the control channel, transferring any handles the
   service needs to bootstrap (init transfers the block-device handle to
   `fs-server-ext4` this way).
4. **Await `Meta::Ready`** (bounded by a timeout — init uses 30 s), which carries
   the service's **forwarding endpoint** handle.
5. **Bind** that endpoint into the namespace at the declared path
   (`SYS_NS_BIND`, requires `BIND_NAMESPACE`) — the kernel adopts the `IpcChannel`
   as a userspace-server binding (slice-7 forwarding). Close the control channel and
   the local endpoint reference (the bind took its own).

The rsproto Ready envelope is `RS_MAGIC = "RSMG"`, op `Meta::Ready = 0x0004`, in the
`IpcMsg` payload (init hand-parses it to avoid `librsproto`; service-mgr, which is
*not* under init's no-librsproto constraint, should use `librsproto` properly).

Non-RS services (a plain daemon with no endpoint to bind) skip steps 3–5: they are
`running` once spawned, supervised only for exit/restart.

## Internal architecture

- **`no_std` + `alloc`**, `libos` + `libheap` + `libkern` + `librsproto`. Unlike
  `init` and `eshell`, service-mgr **is** allowed the stateful runtime
  (`librsproto`, later `libstream`): it runs after the ecosystem is coming up, not
  in the pre-allocator critical path. (Eventual `std` target, like all userspace.)
- **Async on the libos executor.** Each supervised service is a task: spawn → await
  Ready (RS) → await its `ChildExited` notification → apply policy. The notification
  channel drives reaping; per-service control channels drive lifecycle commands.
  Backoff waits are timer-driven (`sys_timer_*`), not busy sleeps.
- **The dependency graph** is built once from the parsed declarations; a topological
  order gates `starting` (a service waits until its `after`/`wants` set is
  `running`/`ready`). Cycles are a parse-time rejection.
- **State table**: one entry per service (name, decl, state, control endpoint,
  child handle, restart bookkeeping). Bounded by the number of declarations.

## Reality vs. the schema: the buildability gap

The [service-toml schema](../spec/service-toml-schema.md) is the **full aspirational
contract**. Several of its assumptions do not exist yet, and the *first* slice must
be scoped to what is buildable:

| Schema assumes | Reality today | Implication for slice 1 |
|---|---|---|
| `executable = "/store/…"` path spawns | Spawn is a **kernel-embedded `ImageId` enum** (no ELF-from-namespace loader) | Slice-1 services are embedded images selected by `ImageId`; the `executable` field maps to a known image, not an arbitrary path. Full path-based spawn is a later slice (needs a userspace ELF loader). |
| Declarations in `/store/…-system-services/` projected to `/etc/services/` | No content store, no profile server | Slice-1 declarations come from the **initramfs** (e.g. `/etc/services/*.toml`), like `init.toml`. |
| `log` handles → a logging service | No logging service | Slice-1 `stdout`/`stderr`/`log` route to `sys_kprint` / the kernel log; the logging service is a later backlog item. |
| Typed `environment` / `argv` envmap | Spawn passes a single `arg0` + moved handles | Defer typed envmap/argv delivery; slice-1 services take handles only. |
| `stdin`=`/dev/null`, stream stdio | No `/dev/null`, no stream stdio yet | Defer auto-stdio; slice-1 grants only explicitly-declared handles + the auto namespace/notification/control. |

None of these are blockers for a *useful* first service-mgr — they scope what its
first slice supervises.

## Proposed slicing

**Slice A — minimal supervisor (the milestone's spine).** A `service-mgr` crate
(`ImageId::ServiceMgr = 5`, embedded), spawned by init with `BIND_NAMESPACE`
(+ `LOAD_MODULE` to hold for delegation). It reads one or two service declarations
from the initramfs, parses them with a minimal TOML reader (init's `toml_lite`
lineage), starts an embedded-image demo service, supervises it (reap via
notifications, restart per `on-failure`/`always`/`never` with backoff), and exposes
a per-service control channel. **Proof:** boot reaches "service-mgr running, demo
service supervised"; kill the demo service and watch it restart per policy.

**Slice B — take over `fs-server-ext4`.** Move fs-server supervision from init to
service-mgr: init mounts the *root* to boot, but post-handoff service-mgr owns the
RS protocol for additional/declared fs-servers (generalizing init's handshake, via
`librsproto`). Proves the full RS startup path under service-mgr.

**Slice C — dependency graph + multiple services.** `after`/`before`/`wants`,
topological startup, several supervised services concurrently — the plan's milestone
("multiple services running, all supervised").

**Later** (own slices, per the backlog): path-based ELF spawn; the logging service +
`log` channel routing; profile server + `/store` declarations; typed envmap/argv;
device-manager (`LOAD_MODULE` delegation); auth/session.

## Resolved decisions (slice-A scope)

Settled in review (2026-07-15):

1. **Slice-A demo service**: a purpose-built trivial **heartbeat** service — a clean,
   controllable restart/backoff demonstration (not a reused image).
2. **Declaration source**: the **initramfs** (`/etc/services/*.toml`, mirroring
   `init.toml`) — it exercises the real parse path and sidesteps the profile-server
   bootstrap ordering.
3. **fs-server ownership**: **stays in init for slice A** (critical path — init must
   reach a mounted root to find service-mgr's declarations); service-mgr owns only
   *additional* services in A. Whether service-mgr re-adopts the root fs-server is a
   slice-B question.
4. **init retains `BIND_NAMESPACE`** (it cannot self-drop — see Capability posture);
   **service-mgr death → reboot / emergency, not respawn.** init's CLAUDE.md
   "delegate and drop" is corrected to apply to handles, not syscaps.

## References

- Schema: [service-toml-schema](../spec/service-toml-schema.md)
- Supervisor registration: [why-supervisor-registration](../rationale/why-supervisor-registration.md)
- Resource server protocol: [namespace-and-resource-servers](namespace-and-resource-servers.md)
- Capabilities: [syscaps](syscaps.md), [why-capabilities](../rationale/why-capabilities.md)
- Boot flow: [boot-flow](boot-flow.md)
- init's constraints: `userspace/init/CLAUDE.md`; the concrete RS handshake template
  in `userspace/init/src/main.rs`
