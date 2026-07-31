# userspace/session-mgr/CLAUDE.md

Constraints for the session manager. Loaded when working under
`userspace/session-mgr/`.

## What this is

The Tier-5 supervisor that logs a user in and hands them a sandboxed shell: it
authenticates a credential (via auth-service), constructs a **per-user namespace**,
and spawns the user shell into it. It holds re-delegated `BIND_NAMESPACE` (from
service-mgr) and the building-block endpoints it composes sessions from — the fs-server
forwarding endpoint, the profile-server forwarding endpoint, and a channel to auth-service.
See `docs/architecture/session-and-auth.md`.

**Part D (current):** the plumbing — receive the handed-over endpoints, authenticate
the demo user over the auth channel, and construct the session namespace binding
`/home` as a subtree of the fs-server (proving `BIND_NAMESPACE` + subtree scoping +
shared-registration bind-mount). session-mgr fires the self-test boot verdict.
**Part E:** replace the hardcoded round-trip with an interactive `login:` prompt and
spawn the user shell into the constructed namespace.

## Discipline (init/supervisor family)

- **`#![no_std]` + `#![no_main]`, with `alloc`.** The no-`alloc` rule was lifted on
  2026-07-31: session-mgr hands each session its **environment**, and every step of that
  needs a heap — a TSM1 `Record` holds `Vec`s, `send_setup` builds a `Vec<String>` of
  `argv`, and encoding returns a `Vec<u8>`. The old rule's own escape clause was "unless a
  real need appears", and this is one: without it the *parent* cannot give the child its
  environment, which is the whole basis of Milestone 3.5. The alternative — a second,
  allocation-free encoding path in `libstream` — would have cost more than it saved.
  `#![no_std]`/`#![no_main]` stay: `std` is not ported, and there is no runtime to hand a
  `main`.
- **`libkern` + `librsproto` + `libstream` + `libheap`.** Still no `libos` unless a real
  need appears. It remains a supervisor whose death is a system fault, so the *spirit* of
  the rule — keep it minimal — still applies to everything else.
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
(2) the **profile-server** forwarding endpoint, (3) the auth channel. session-mgr `recv`s
all three before doing anything. The endpoints are handed over IPC (not the namespace)
because constructing namespaces means binding *endpoint handles* — and a `UserspaceServer`
binding resolves to a kernel registration record, never back to the endpoint, so a process
holding a LOOKUP-only root namespace can *use* `/bin` but can never obtain what it would
take to bind it elsewhere.

**The receives are positional.** A sender with an endpoint missing sends an *empty message*
rather than skipping the send; skipping would shift every later handoff up a slot and land
the auth channel where the profile endpoint belongs.

## What a session namespace contains

`/home` (the user's home, a subtree of the fs-server), `/bin` (the profile server,
whole-tree), `/session/user`, `/dev/console`. Both server bindings **share** init's
registration rather than minting a rival — the kernel's bind-mount semantics, one server
connection under many names.

That list is the sandbox. Nothing else is reachable: not `/system`, not `/store`, not
`/initramfs`. Adding a member is granting every session that authority, so it is a design
decision each time, not plumbing. In particular, **do not bind `/initramfs/sbin` to make
programs reachable** — that hands a session the boot image instead of a profile, and
"absence is the sandbox" stops meaning anything once every session sees every binary.

## Forbidden

- Storing or logging a password.
- Holding more than `BIND_NAMESPACE`; granting a user shell any syscaps.
- Investing in the throwaway demo/login path (the real shell + login are Phase 4 /
  Part E).
