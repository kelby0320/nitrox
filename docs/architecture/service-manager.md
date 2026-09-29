# Service Manager (`service-mgr`)

**Status:** Implemented (Phase 3) — `userspace/service-mgr`, spawned by `init`, supervising
the service set and performing supervisor-side namespace binding. Verified 2026-08-05; last
checked 2026-09-28, when it took over starting and binding the servers `init` used to — the
boundary below, as built at last — through a registry of its own (administration Part E.1a),
and began handing the sessions its own routes to them (Part E.1b), and reading its declarations
from the root (Part E.1c), and serving `/svc/services` — the list, and starting and stopping on
an admin session (Part E.2a); last checked 2026-09-29, when every server came to exit on
`CTRL_OP_SHUTDOWN` and `init` to answer a shutdown's `Finish` (Part E.4a), and it came to run the
shutdown itself (Part E.4b);
before that 2026-09-25, when a death found before its exit code learned to wait for it (below);
before that, 2026-08-21, when it learned to hold **more than one** service and a stale
"pre-implementation" line below was removed.

Design doc for `service-mgr`, the userspace process supervisor.

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
2. **Start services in file order**, waiting where `after` says to. A dependency *graph*
   from `after`/`before`/`wants` with a topological sort is the general design and is **not
   built**; `after` is implemented with the narrow meaning that is achievable — "this service
   has exited" — and ordinary ordering comes from the order declarations appear in. `before`
   and `wants` are unparsed. See [service-toml-schema](../spec/service-toml-schema.md).
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
       ├─ spawn service-mgr; hand it a full-rights root,     ← THE HANDOFF
       │  the fs and profile endpoints, down the terminal channel
       └─ reap loop (orphans + the terminal channel)
                                    │
service-mgr ────────────────────────┘
  ├─ read service declarations
  ├─ start the servers in order (RS protocol; bind in the registry and at each path)
  ├─ the login chain, then the other declarations in the file's order
  ├─ supervise: reap + restart per policy; answer resolves forwarded to it
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

### Several services, and how their exits are told apart

`service-mgr` supervises every declaration in `/system/services.toml`, on the root (the
initramfs's `etc/services.toml` until administration Part E.1c), up to
`MAX_SERVICES` (24), and applies each one's own restart policy. It says which declarations it
dropped rather than truncating silently.

**Which child exited is decided by the child's control channel, not by the notification.**
`KIND_CHILD_EXITED` names a child by **pid**, and nothing maps a process handle to a pid —
so a supervisor with two children would learn *that* one exited and never *which*
(`TODO(child-exit-attribution)` in [deferred-decisions](../rationale/deferred-decisions.md)).
Each service instead gets its own control channel; when the child dies its end is destroyed,
and a non-blocking `sys_channel_recv` on the survivor answers `PeerClosed` rather than
`WouldBlock`. Each pass tries every running service's channel, and the death's `ChildExited`
is what wakes the pass (since Part E.1b; the channels themselves were waited on before). A
handle cannot be recycled under its holder the way a pid can, so this is exact.

The exit **code** is still taken from the notification queue in arrival order, since it
arrives beside a pid that cannot be matched. One exit per wake — every case the system
produces — pairs correctly; the residual is in the deferral entry. **The close can come
first**: `sys_process_exit` closes the child's handles before it queues `ChildExited`, so a
wake that finds more deaths than codes waits on the notification channel for the rest, up to
`CODE_GRACE_NS` (1 s). Only past that is an exit reported `code=unknown`.

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
4. **Terminal shutdown/reboot + emergency backstop** — **shutdown is split**:
   service-mgr does *graceful, dependency-ordered* service teardown (via the control
   channels); init does the *terminal* step (it is the last process) and is the
   recovery backstop when service-mgr can't come up, or asks for it.

(A fifth item, **releasing the initramfs** once boot is stable, stood here until administration
Part E.1a. It was never built, and cannot be: the root fs-server's restart image can only come
from the initramfs. See `userspace/init/CLAUDE.md` § *Initramfs interaction*.)

Everything else — even things init *could* spawn (init may spawn more than one
process) — should be a service. init spawns only the **irreducible minimum to reach
service-mgr** (the root fs-server; eventually the profile server *if* declarations
move to `/store`) plus the emergency eshell.

**As built since administration Part E.1a (2026-09-28)**: `init` spawns its mounts' fs-servers,
the profile server at `/bin` — which `service-mgr` is spawned from — `service-mgr`, and the
emergency shell when it must. The nine servers it used to start as well are declarations:
`auth-service`, `logging-service`, `tty-server`, `clipboard-server`, the view broker, `device-mgr`,
`storage-service`, `input-server` and the compositor (*Servers, and the registry*, below).

**The bootstrap ordering to respect:** long-term, declarations come from `/store`
projected by the profile server — but service-mgr needs its declarations *to start*.
So init must bring up "enough" (root fs, later the profile server) before service-mgr
reads anything: init owns the *minimum substrate*, service-mgr owns *everything
policy-driven on top*. Slice A sidesteps the chicken-and-egg by reading declarations
from the **initramfs**.

### Servers, and the registry

*(Administration Parts E.1a and E.1b, 2026-09-28.)*

**A declaration can describe a server.** `endpoint = "<path>"` names the path in the root the
server is reached at (`service-toml-schema.md`). For such a declaration, `service-mgr`:
- spawns it with the control channel `init` gave a server — `SEND`, `RECV`, `TRANSFER` and `WAIT`,
  and no log handoff, since a server resolves its own log;
- waits for its `Meta::Ready`, within 30 s;
- binds the endpoint in **its registry**, a namespace of its own, at `/<name>`;
- and, the first time only, binds the root path to the server's **route**: an endpoint of
  `service-mgr`'s own, made for that server and kept for the boot.

A resolve on that path reaches `service-mgr` first, on the route. It answers `SUBNAMESPACE` into
its registry at `/<name>`, the whole suffix continuing, and the resolve goes on into whichever
server is bound there now — as `/storage` works. **A restart rebinds in the registry and nowhere
else**, so every binding of the path reaches the new server, and a program whose connection closed
resolves the same path and reaches it. A route whose server is not up answers `NotFound`.

**The sessions bind the same routes** (Part E.1b). The login supervisors are handed a duplicate of
the route to each server a session binds — the terminal server, the clipboard, the view broker,
the compositor — never the server's own endpoint, so a session built before a restart reaches the
new server too; `desktop-shell` passes the same handles into every application. `boot-probe`
proves it with a copy of the root made before a restart, which holds the same binding and is never
rebound.

**One route reaches one server.** Part E.1a bound every root path to a single endpoint, with the
server's name as the base, and routed on the suffix. That could not be handed to a session:
`desktop-shell` holds what it is handed with `BIND_NAMESPACE`, and could have bound it with any
base — `/auth-service/admin`, `/device-mgr/input`. A route per server is attenuation by
construction, like the device manager's info-only endpoint.

**Two servers mint an endpoint for sessions**, narrower than their root one, and each gets a
route of its own (`service_mgr::registry::DERIVED`): the device manager's `info-endpoint`, which
answers its tables and never a class, and the storage service's `session-endpoint`, which answers
the filesystems and never an admin endpoint. Each time one comes up, `service-mgr` resolves it in
the registry — answering resolves while it waits — and binds what it gets at `/<name>.<suffix>`,
a place no server's name can take. The supervisors are handed the route to it, and resolved the
storage one themselves until Part E.1b. **No restart reaches either today**: both are `essential`,
and every server's policy is `never`; the re-derivation is written for when one has a policy.

**Declarations start in file order, each server's `Ready` awaited before the next**, which keeps
the orders `init` relied on: the broker after the log it audits to, the device manager before the
two that take its devices, and the input server before the compositor. **The login chain starts
after the last server and before the rest** (`service_mgr::bringup`): its supervisors need the
servers, and a test image's clients must start after the greeter. `service-mgr` keeps the
supervisors' process handles and control channels: a shutdown asks them on the first to end their
sessions, and learns of their exits from the second closing (§ *Shutdown*).

**`service-mgr` never blocks on anything that can wait on it.** Every resolve on a server's path
waits on it, so a blocking wait for a server that is itself resolving one — the broker opens its
log at startup — would be two processes waiting on each other. So a server's `Ready`, a restart's
backoff and an `after` are deadlines in its one wait, and resolves are answered on every pass, as
they are while it waits for an exit code or a derived endpoint. The lookups it does make outright
wait on the root filesystem, the profile server and servers already serving, none of which waits
on it. Each route's ring is 64 deep, since a full one answers a resolve `WouldBlock` at once.

**Its wait holds the notification channel, every route, and the control channel of each server
still starting** — whose `Ready` is the one thing a control channel brings. A death needs no slot:
it closes the channel and queues `ChildExited`, and each pass looks at every channel. That keeps
the wait inside the kernel's 32 handles with sixteen routes (`registry::MAX_ROUTES`); until Part
E.1b it held every running service's channel, which capped the services at 31 and left no room.

**`critical = true`, at bring-up only**, marks `auth-service` and `logging-service`, the two `init`
treated as critical-path. If one does not come up at boot, `service-mgr` starts nothing more and
asks `init` for the emergency shell over the **terminal channel** (below); the console is still
free, since both start before the terminal server. At runtime a critical server's death is its
restart policy's, because the terminal server holds the console by then. Every server's policy is
`never` today, as `init` never restarted them. **A declarations file that has lost its critical
servers is an emergency too** (`bringup::unfit`, PR #340 review): a critical declaration that is
skipped, a missing or empty file, or none critical — each would otherwise go straight to a login
chain with no `auth-service` behind it. Every skipped declaration is logged by name.

**The terminal channel** is the handoff channel `init` keeps open: three handoffs down it — a root
handle with `init`'s rights, and the root filesystem's and the profile server's endpoints — and
then `TERMINAL_OP_EMERGENCY` back up it. Its closing is how `init` learns `service-mgr` has died.
**`TERMINAL_OP_FINISH`** (Part E.4a), with a byte saying whether to reboot, is a shutdown's last
word to `init`, which unmounts its own filesystems and calls `sys_power`, sent as a shutdown's
last step (§ *Shutdown*, Part E.4b).

### `/svc/services`: the list, and starting and stopping

*(Administration Part E.2a, 2026-09-28.)* `service-mgr` serves `/svc/services` from an endpoint of
its own, bound there in the root ([`rsproto-services-ops.md`](../spec/rsproto-services-ops.md)):
- **`all.tsm`**, a table with a row per declaration — `name`, `state` and `restarts` — for anyone
  who can reach it;
- **`admin-endpoint`**, which mints an endpoint on which any resolve opens an admin session, where
  `Start`, `Stop` and `Restart` are asked. The view broker's `services` grant is what binds one
  into a view (Part E.2b).
- **A session endpoint**, made at startup and handed to both login supervisors, which bind it at
  `/dev/services` in every session and, through `desktop-shell`, every application (Part E.2b). It
  answers the table and nothing else, so no session starts or stops a service without the grant.

**Every request is answered once it has happened**, and never by a wait: a stop is `Held` until
the service's exit, a start until its `Meta::Ready` — deadlines in the one loop, like every other.
A stop is `CTRL_OP_SHUTDOWN` on the service's control channel, **a request**: one not honoured in 5
s is answered "asked, and still running", stays asked, and is still a stop if it comes later. There
is no forcible kill. An `essential` service's stop and restart are refused here, whoever asks.

**Which services exit when asked**: `heartbeat`, and every server a release image runs — the
terminal server, the clipboard, the input server and the compositor since Part E.2a, and
`auth-service`, `logging-service`, the view broker, `device-mgr` and the storage service since
Part E.4a. The storage service unmounts everything it mounted first, and the log sinks what is
queued. The test image's graphical clients and `restart-probe` do not. **Only the clipboard may be
stopped by request**:
the other three are `essential`, since their clients do not reconnect, and on a machine with no
serial port stopping one leaves nothing to type at (PR #341 review). Their exit is for a shutdown.
Each waits on its control channel beside its work, through `libkern::control`, and **takes it out of
its wait set if it closes**, since a closed channel stays signalled for good. A stopped server's
registry entry goes with it, so its path answers `NotFound` until it is started again, and every
binding reaches the new one then.

**The wait set pays for it**: `/svc/services`' endpoint, a session endpoint, two admin endpoints
and four admin sessions (`services::SLOTS`), which is what took `MAX_ROUTES` from 16 to 14 — and
since Part E.4b two power endpoints and two power sessions, which took the starting servers' room
(`registry::STARTING_ROOM`) from nine to five. Bring-up starts one server at a time. (One power
endpoint until Part E.4d, when every `admin` view's `power` grant was found to make the view broker
hold it for the boot.)

### Shutdown

*(Administration Part E.4b, 2026-09-29.)* `/svc/services/power-endpoint` mints a **power endpoint**,
on which any resolve opens a **power session** taking `Shutdown` alone
([`rsproto-services-ops.md`](../spec/rsproto-services-ops.md)); the view broker's `power` grant
binds one at `/dev/power`, and `with power shutdown [--reboot]` asks there — anyone at the machine,
with no password, in the seeded policy (Part E.4d). A `Shutdown` is answered as soon as it begins, and then
`service-mgr` takes the machine down in `service_mgr::shutdown`'s order, each wait a deadline in
the one loop:

1. **Nothing more starts** — not bring-up, not a policy's restart — every service is marked asked
   to stop, and every waiting admin request is refused.
2. **The sessions.** Both login supervisors are sent a terminate request, and have 10 s; their
   control channels closing is how their exits are seen. Ending a session is theirs (Part E.4c,
   [`graphical-session.md`](graphical-session.md) §4): each passes the request to its session's
   leader, gives it 5 s, closes the session at the view broker and exits — or exits at once from
   its prompt or greeter.
3. **The services, last declared first**, each `CTRL_OP_SHUTDOWN` and 3 s to exit, or what its
   declaration's `stop_timeout` gives it. The reverse of
   the start order lets each server outlive what was started after it, and may use it. One not
   honoured is logged "still running", and the shutdown goes on — a stop is a request, and there is
   no forcible kill. The storage service unmounts everything it mounted on the way out.
4. **`init` is told to finish** on the terminal channel: its own filesystems, then `sys_power`.

What honours a stop is § *Servers, and the registry* above: every server a release image runs. In a
test image `nxterm`, `ui-testclient`, `input-testclient` and `restart-probe` do not, and each is
passed over after its bound.

**A declaration may say how long its stop takes** (`stop_timeout`, PR #343 review): the storage
service declares 60 s, since its stop is the write-back and unmount of every filesystem it mounted,
and grows with what is dirty. The same bound answers a `service --stop`. Past it the shutdown goes
on, and a filesystem still being written would be left not clean.

## Capability posture

`init` holds the full initial `SysCaps` set. It delegates to `service-mgr` **only
the subset service-mgr legitimately needs**, using the kernel's spawn-time rule
`child_syscaps = parent_syscaps & args.syscaps` (the kernel rejects any attempt to
amplify — a child can never gain a capability its parent lacks).

| SysCap | service-mgr holds? | Why |
|---|---|---|
| `BIND_NAMESPACE` | **yes — own use** | It registers each service's endpoint into the system namespace (the RS protocol's bind step). This is the defining supervisor capability. It also *re-delegates* `BIND_NAMESPACE` to **both login supervisors** — `session-mgr` and, since M7 Part D, `desktop-session-mgr` — each of which needs it to construct a session namespace. |
| `LOAD_MODULE` | **yes — pass-through** | Not used by service-mgr directly; held so it can *delegate* it to the `device-manager` service (delegation can only attenuate — to grant a cap, you must hold it). |
| `SYSTEM_CLOCK` | **yes — pass-through** | Same: held to delegate to a `time-sync` service, not exercised by service-mgr itself. |
| `PHYSICAL_MEMORY` | **no** | Only `init` keeps this, for extreme recovery. A supervisor has no business with raw physical memory (called out explicitly in `userspace/init/CLAUDE.md`). |
| `REAL_TIME` / `AUDIT_CONTROL` | **no** | No service-mgr need; a service wanting `REAL_TIME` acquires it another way, `AUDIT_CONTROL` belongs to the audit service's own grant path. |

Each *service* receives `service_syscaps = servicemgr_syscaps & decl.syscaps`, enforced
**once**, by the kernel at spawn time (`child = parent & args`). That masking is silent: a
declaration asking for more than service-mgr holds spawns successfully with the extra
capabilities absent and nothing reported.

There is **no parse-time subset check**, and this paragraph claimed one until 2026-08-24.
service-mgr validates the *names* against the schema's table — an unrecognised one is reported
and withheld — but it cannot check the subset, because nothing reports a process its own
capability set (`/proc/self/status` carries pid and tid only). Closing that is
`TODO(spawn-syscap-attenuation)`. **Most services get `[]`** (zero ambient capability); authority comes from
the handles granted in `[service.<name>.handles]`, not from syscaps.

**Binding is two-gated.** Every `sys_ns_bind` service-mgr issues is checked against
*both* the ambient `BIND_NAMESPACE` syscap *and* `Rights::BIND` on the specific
namespace handle being bound into. A spawned process only ever gets a lookup-only root handle,
which is why `init` did every root binding until administration Part E.1a.

**`service-mgr` sits in `init`'s trust tier** since then — the maintainer's call, 2026-09-28. `init`
hands it a root handle with `init`'s own rights, `BIND` and `UNBIND` among them, so it can bind the
servers it starts. This section said service-mgr "should hold BIND-righted handles only to the
subtrees it actually manages"; it holds the whole root now, which reaches `/`, `/bin` and `/store`
as well. A kernel handle scoped to a subtree would restore the narrower posture, and nothing
builds one.

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
exiting is a **critical fault**.

**As built since administration Part E.1a**, which is when the code stopped respawning it: `init`
learns of the death from the terminal channel closing, and reports that the machine needs a
restart. It cannot drop to the emergency shell, because the terminal server — `service-mgr`'s
child, still running — holds the console. And every server path in every session goes through
`service-mgr`'s endpoint, so the death takes them all. Until Part E.4's `shutdown`, the restart is
the person's.

**A `service-mgr` that never starts does get the emergency shell**: a spawn that fails leaves no
server at all, `auth-service` and `logging-service` among them, and nothing holding the console.
`init` takes the emergency path, as it did for that pair when it started them.

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

- **Parse** → `valid` or `misconfigured` (skipped, logged). Implemented today: a declaration
  with no `executable` is skipped, a duplicate name is dropped, and unrecognised `syscaps`
  names are reported. Not implemented: the subset check and the acyclic check, neither of
  which service-mgr is in a position to make (see above, and `after`'s note below).
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
   the service's **forwarding endpoint** handle — or a **refusal** saying why there is
   none (`docs/spec/rsproto-wire-format.md` § Meta::Ready), which only
   `fs-server-ext4` sends today.
5. **Bind** that endpoint into the namespace at the declared path
   (`SYS_NS_BIND`, requires `BIND_NAMESPACE`) — the kernel adopts the `IpcChannel`
   as a userspace-server binding (slice-7 forwarding). Close the control channel and
   the local endpoint reference (the bind took its own).

The rsproto Ready envelope is `RS_MAGIC = "RSMG"`, op `Meta::Ready = 0x0004`, in the
`IpcMsg` payload (init hand-parses it to avoid `librsproto`, in the host-tested
`userspace/init/src/ready.rs`; service-mgr, which is *not* under init's no-librsproto
constraint, should use `librsproto` properly). **init says which way a handshake failed**
— no Ready within the timeout, the server exited first, something other than a Ready, or
a refusal and its reason — naming the server, and for a mount the mount point and device.

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
- **Ordering** is the declaration file's order, plus `after`. `after` names services that
  must have **exited** — for a one-shot, finishing is readiness, and there is no readiness
  protocol for anything else — and it can only refer *backwards*, since a service is matched
  against those already started. The wait is bounded and reported; a name that matches nothing
  yet started, a dependency that failed to spawn, and a timeout are all logged and non-fatal.
  Cycles are therefore not rejected: a forward reference simply does not wait. A real graph
  with a topological sort remains the general answer, unbuilt.
- **State table**: one entry per service (name, decl, state, control endpoint,
  child handle, restart bookkeeping). Bounded by the number of declarations.

## Reality vs. the schema: the buildability gap

The [service-toml schema](../spec/service-toml-schema.md) is the **full aspirational
contract**. Several of its assumptions do not exist yet, and the *first* slice must
be scoped to what is buildable:

*(This is slice 1's table, and its "today" is 2026-07-15's. Several rows have moved since: spawn
is by path through `/bin`, a logging service exists, and the declarations are
`/system/services.toml` on the root (administration Part E.1c). It is kept as the scoping record.)*

| Schema assumes | Reality at slice 1 | Implication for slice 1 |
|---|---|---|
| `executable = "/store/…"` path spawns | Spawn is a **kernel-embedded `ImageId` enum** (no ELF-from-namespace loader) | Slice-1 services are embedded images selected by `ImageId`; the `executable` field maps to a known image, not an arbitrary path. Full path-based spawn is a later slice (needs a userspace ELF loader). |
| Declarations in `/store/…-system-services/` projected to `/etc/services/` | No content store, no profile server | Declarations come from the **initramfs**, like `init.toml`. **One file** — `/initramfs/etc/services.toml` — not a directory: nothing can enumerate one (schema changed 2026-08-21). |
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
2. **Declaration source**: the **initramfs** — `/initramfs/etc/services.toml`, mirroring `init.toml`
   — which exercises the real parse path and sidesteps the profile-server bootstrap ordering.
   *(Moved to `/system/services.toml` on the root by administration Part E.1c, 2026-09-28: `init`
   mounts the root before it spawns `service-mgr`, so the ordering needed no sidestepping, and on
   the root the file can be edited.)* **One file holding every service**, revised 2026-08-21 from
   `/etc/services/*.toml`: a directory of declarations needs enumeration, and neither the initramfs
   (a CPIO archive the kernel looks up by name) nor `profile-server` (which projects packages'
   `bin/` only) can do it. See `docs/spec/service-toml-schema.md`.
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
