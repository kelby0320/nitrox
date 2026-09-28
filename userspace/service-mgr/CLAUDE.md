# userspace/service-mgr/CLAUDE.md

`service-mgr` workspace constraints. Loaded when Claude Code reads files under
`userspace/service-mgr/`.

## What service-mgr is

The userspace **service manager**: init spawns it once critical-path boot is stable,
and it then starts, supervises, and restarts the system's services. It is the
userspace supervisor that holds `BIND_NAMESPACE` and registers each service's
endpoint into the namespace (services never self-register — see
`docs/rationale/why-supervisor-registration.md`).

**It starts the system's servers** (administration Part E.1a, 2026-09-28): `auth-service`,
`logging-service`, `tty-server`, `clipboard-server`, the view broker, `device-mgr`,
`storage-service`, `input-server` and the compositor are declarations with an `endpoint`, which
`init` started and bound until then. Each endpoint is bound in `service-mgr`'s **registry** — a
namespace of its own, at `/<name>` — and each path in the root is bound once, to the server's
**route**: an endpoint of `service-mgr`'s own, one per server. A resolve there is answered with a
`SUBNAMESPACE` continuation into the registry, so **a restart rebinds only the registry** and
reaches every binding of the path. The login supervisors are handed the same routes (Part E.1b),
so that includes every session's. Two servers' routes lead to an endpoint the server minted for
sessions (`registry::DERIVED`), re-derived each time it comes up. See the design doc's § *Servers,
and the registry*.

Design doc: **`docs/architecture/service-manager.md`** — read it before significant
work. The init/service-mgr boundary, the capability posture, the RS startup protocol,
and the slice plan all live there.

## Build environment

- **`#![no_std]` + `#![no_main]`.** The userspace target (`x86_64-unknown-nitrox`), static
  non-PIE ET_EXEC via `user.ld` + `.cargo/config.toml` (mirrors the other userspace
  bins). Spawned by `init` from `/bin/service-mgr`, the profile server's projection of the
  store.
- **Stable Rust only.**
- **Layering:** unlike `init`/`eshell`, service-mgr **is** allowed the stateful
  runtime — it runs after the ecosystem is coming up, not in the pre-allocator
  critical path. Trajectory: `libkern` + `libheap` + `libos` + `librsproto` (+ later
  `libstream`), eventual `std`. **Today it links `libkern`, `libheap`, `librsproto` and
  `libfs`**: the last two since administration Part E.1a, for the `SUBNAMESPACE` replies its
  endpoint answers and for reading its declarations.

## Discipline

- **Never block on anything that can wait on `service-mgr`.** Every server's path resolves through
  its routes, so a `service-mgr` that sits in a wait of its own while a server resolves a path
  deadlocks the boot. Its main wait is `Mgr::run`'s: its notifications, every route, and the
  control channel of each server still starting, with the nearest deadline as the timeout. A
  `Ready` awaited, a backoff, an `after` — each is a deadline there, and resolves are answered on
  every pass. The grace wait for an exit code and the lookup of a derived endpoint
  (`wait_serving`, `lookup_serving`) answer them too. The lookups it waits on outright
  (`ns_lookup`: `/bin`, a server's log endpoint) reach the root filesystem, the profile server and
  servers already serving, none of which waits on it.
- **The wait set is budgeted**: the notification channel, `registry::MAX_ROUTES` routes and
  `registry::STARTING_ROOM` starting servers make the kernel's 32. A running service's channel is
  not in it — a death queues `ChildExited`, which wakes the pass that finds it.
- **Answer every resolve.** A forwarded resolve has no deadline: one dropped is a caller hung for
  good. A failed reply is answered with an error, and the endpoint's ring is `SERVE_DEPTH` (64)
  deep, because a full one answers the caller `WouldBlock` at once.
- **No `panic!()` in normal operation.** service-mgr is the supervisor; its death is a critical
  system fault: init reports it and does not respawn it — a fresh service-mgr cannot re-adopt
  orphaned services or the registry — and the machine needs a restart. Every error path must degrade
  gracefully, not panic.
- **Capability least-authority, bar one handle.** service-mgr holds `BIND_NAMESPACE` (own use
  + re-delegation to session-mgr, the view broker and the storage service); **not**
  `PHYSICAL_MEMORY`. **Its root handle carries `init`'s own rights** — `BIND` and `UNBIND`, over
  `/`, `/bin` and `/store` too — because it binds the servers (the maintainer's call, 2026-09-28:
  service-mgr is in init's trust tier). Bind with it only at a declaration's `endpoint`, and only
  once. Grant each service only the handles its declaration lists, attenuated. A service's
  `syscaps` are masked to service-mgr's own set (`child = parent & args`); most services get
  `[]`.
- **Bounded everything** — a service table sized by the declaration count, bounded
  restart attempts + backoff, bounded waits.

## What service-mgr owns vs. what init owns

service-mgr owns the **policy/declaration-driven** ecosystem: parsing declarations,
dependency-ordered startup, supervision, restart policy + backoff, RS registration,
lifecycle control channels. init keeps the **irreducible PID-1 roles**: the initial
handle set, critical-path bootstrap, reaper of last resort, and the terminal
shutdown/reboot + emergency backstop — which service-mgr asks for over the **terminal
channel** (`TERMINAL_OP_EMERGENCY`) when a `critical` server does not come up at boot.
Litmus test: *expressible as a `service.toml` and supervised?* → service-mgr's. See the design doc's
"init / service-mgr boundary" section.

## Forbidden patterns

- `panic!()` / `unwrap()` outside provably-impossible cases (with a `// reason`).
- Granting a service more rights/caps than its declaration calls for.
- Letting a resource server self-register (service-mgr does the `sys_ns_bind`).
- Binding or handing out a server's own endpoint anywhere but the registry. The root and the
  sessions hold its route; bind the server itself there and its first restart strands every
  binding of the path.
- **One endpoint that reaches more than one server.** A route is handed to `desktop-shell`,
  which holds `BIND_NAMESPACE` and could bind it at any base: a route that routed on the suffix
  would reach `/auth-service/admin` from an application. One route, one place in the registry.
- A wait that stops answering resolves while it waits on something that could be waiting on
  service-mgr — a server starting, above all.
- Unbounded restart loops (respect `max_attempts` + backoff).
