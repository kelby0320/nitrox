# userspace/service-mgr/CLAUDE.md

`service-mgr` workspace constraints. Loaded when Claude Code reads files under
`userspace/service-mgr/`.

## What service-mgr is

The userspace **service manager**: init spawns it once critical-path boot is stable,
and it then starts, supervises, and restarts the system's services. It is the
userspace supervisor that holds `BIND_NAMESPACE` and registers each service's
endpoint into the namespace (services never self-register — see
`docs/rationale/why-supervisor-registration.md`).

Design doc: **`docs/architecture/service-manager.md`** — read it before significant
work. The init/service-mgr boundary, the capability posture, the RS startup protocol,
and the slice plan all live there.

## Build environment

- **`#![no_std]` + `#![no_main]`.** Bare target (`x86_64-unknown-none`), static
  non-PIE ET_EXEC via `user.ld` + `.cargo/config.toml` (mirrors the other userspace
  bins). Kernel-embedded as `ImageId::ServiceMgr` (a Phase-3 stand-in until a
  path-based ELF loader lands).
- **Stable Rust only.**
- **Layering:** unlike `init`/`eshell`, service-mgr **is** allowed the stateful
  runtime — it runs after the ecosystem is coming up, not in the pre-allocator
  critical path. Trajectory: `libkern` + `libheap` + `libos` + `librsproto` (+ later
  `libstream`), eventual `std`. **Slice A is `libkern`-only** (the supervision spine);
  `libheap` (declaration parsing) arrives in Part C, `librsproto`/`libos` (the RS
  startup protocol) in slice B.

## Discipline

- **No `panic!()` in normal operation.** service-mgr is the supervisor; its death is
  a critical system fault (init reboots / drops to emergency — a fresh service-mgr
  cannot re-adopt orphaned services + stale bindings). Every error path must degrade
  gracefully, not panic.
- **Capability least-authority.** service-mgr holds `BIND_NAMESPACE` (own use +
  re-delegation to session-mgr) and `LOAD_MODULE`/`SYSTEM_CLOCK` as pass-through to
  re-delegate; **not** `PHYSICAL_MEMORY`. Grant each service only the handles its
  declaration lists, attenuated. A service's `syscaps` are masked to service-mgr's
  own set (`child = parent & args`); most services get `[]`.
- **Bounded everything** — a service table sized by the declaration count, bounded
  restart attempts + backoff, bounded waits.

## What service-mgr owns vs. what init owns

service-mgr owns the **policy/declaration-driven** ecosystem: parsing declarations,
dependency-ordered startup, supervision, restart policy + backoff, RS registration,
lifecycle control channels. init keeps the **irreducible PID-1 roles**: the initial
handle set, critical-path bootstrap, reaper of last resort, initramfs release, and
the terminal shutdown/reboot + emergency backstop. Litmus test: *expressible as a
`service.toml` and supervised?* → service-mgr's. See the design doc's
"init / service-mgr boundary" section.

## Forbidden patterns

- `panic!()` / `unwrap()` outside provably-impossible cases (with a `// reason`).
- Granting a service more rights/caps than its declaration calls for.
- Letting a resource server self-register (service-mgr does the `sys_ns_bind`).
- Unbounded restart loops (respect `max_attempts` + backoff).
