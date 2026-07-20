# userspace/session-mgr/CLAUDE.md

Constraints for the session manager. Loaded when working under
`userspace/session-mgr/`.

## What this is

The Tier-5 supervisor that logs a user in and hands them a sandboxed shell: it
authenticates a credential (via auth-service), constructs a **per-user namespace**,
and spawns the user shell into it. It holds re-delegated `BIND_NAMESPACE` (from
service-mgr) and the building-block endpoints it composes sessions from — the
fs-server forwarding endpoint + a channel to auth-service. See
`docs/architecture/session-and-auth.md`.

**Part D (current):** the plumbing — receive the handed-over endpoints, authenticate
the demo user over the auth channel, and construct the session namespace binding
`/home` as a subtree of the fs-server (proving `BIND_NAMESPACE` + subtree scoping +
shared-registration bind-mount). session-mgr fires the self-test boot verdict.
**Part E:** replace the hardcoded round-trip with an interactive `login:` prompt and
spawn the user shell into the constructed namespace.

## Discipline (init/supervisor family)

- **`#![no_std]` + `#![no_main]`, no `alloc`.** Fixed `.bss` buffers, no
  `#[global_allocator]`. It is a supervisor (its death is a system fault), so keep it
  minimal and robust.
- **`libkern` + `librsproto`** (the Auth codec + rsproto envelope). No `libos`/`libheap`
  unless a real need appears.
- **No `panic!()` / `unwrap()`** in normal operation — degrade + log.
- **Capability least-authority.** session-mgr holds `BIND_NAMESPACE` (to construct
  session namespaces) and no more. It spawns the user shell with **empty syscaps** and
  a namespace naming only that session's resources — the sandbox is the namespace's
  *contents*, not a permission check.
- **Never trust or store a password.** It forwards the console-entered password to
  auth-service once (over the auth channel) and does not keep it; the DB + hashing are
  auth-service's, never session-mgr's.
- **The demo credential is a throwaway test fixture** (Part D), gated by matching the
  xtask-seeded `DEMO_USER`/`DEMO_PASSWORD`. Part E reads the credential from the
  console; do not grow the hardcoded path.

## Boot handoff

service-mgr spawns session-mgr with a control channel (`rdx`) + re-delegated
`BIND_NAMESPACE`, then transfers, in order: (1) the fs-server forwarding endpoint,
(2) the auth channel. session-mgr `recv`s both before doing anything. The endpoints
are handed over IPC (not the namespace) because constructing namespaces means binding
*endpoint handles*.

## Forbidden

- `alloc` / `#[global_allocator]`.
- Storing or logging a password.
- Holding more than `BIND_NAMESPACE`; granting a user shell any syscaps.
- Investing in the throwaway demo/login path (the real shell + login are Phase 4 /
  Part E).
