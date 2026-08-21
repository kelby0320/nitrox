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

## The session loop

It is a real loop as of 2026-07-31: prompt, authenticate, build the namespace, run the
shell, **tear the session down, prompt again**. It was a single pass before, so typing
`exit` parked the supervisor forever and left a machine with no prompt and no way back
short of a reboot.

**One login path, in every build** (2026-08-21). `login()`, `tty_open` and the `tty_*`
helpers are unconditional; the file has no build-mode `cfg` at all.

Three things the loop has to get right, all of which only matter once it *is* a loop:

- **Close the session namespace after the shell is reaped.** That drops the last reference
  and with it every binding — `/home`, `/bin`, the `/session/user` snapshot. Leaking one
  per logout is invisible while there is exactly one login per boot.
- **The long-lived endpoints are not per-session.** The fs, profile and auth endpoints are
  received once at startup and must survive every session; only the namespace is per-login.
- **The loop is supposed to iterate, and no build stops it.** It used to end at the first
  iteration under `test-harness`, because the verdict fired there. `session-mgr` writes no
  verdict now — see below.

A denied login **re-prompts** rather than locking out: a serial console has no second way
in, so a lockout bricks the machine. The pause before re-prompting is what keeps repeated
failure from being a free brute-force oracle.

**Never spin to wait.** `idle` parks on the notification channel and the panic path sleeps
in long hops. A `pause` loop here does not merely waste a CPU — a run queue that is never
empty starves the idle thread, which is where deferred handle reclamation runs, so a
spinning supervisor stops *every* exited process on the system from being reclaimed. That
is the 2026-07-31 `logging-service` bug, found from a hung shell three subsystems away.

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
- **There is no hardcoded credential**, and re-adding one is the specific regression this
  crate is watched for. `DEMO_USER`/`DEMO_PASSWORD` existed for a `test-harness` auto-login
  and are gone (2026-08-21); the credential a session authenticates comes from the console.
  The demo credential still exists as an xtask-seeded **fixture** in `/system/users`, which is
  data — `cargo xtask test-interactive` types it at a real prompt.

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
- **Any `#[cfg(feature = "test-harness")]` or `#[cfg(feature = "selftest")]` in this crate.**
  It has zero, the crate no longer declares either feature, and `xtask` no longer passes one —
  so a test-only branch does not compile here, by construction rather than by discipline.
  That is deliberate: `session-mgr` is where this project's worst instance of test/ship
  divergence lived. Under `test-harness` it auto-logged-in, ran a fixed `-c` script, and
  compiled out the interactive `login()` and the entire `tty_*` layer — so the gate that
  adjudicated the whole boot proved that a string comparison worked. See
  `docs/planning/test-path-retrofit.md`, and if you need a deterministic login, add a step to
  `test-interactive` instead.
