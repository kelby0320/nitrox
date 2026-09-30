# userspace/session-mgr/CLAUDE.md

Constraints for the session manager. Loaded when working under
`userspace/session-mgr/`.

## What this is

The Tier-5 supervisor that logs a user in and hands them a sandboxed shell: it authenticates a
credential (via auth-service), constructs a **per-user namespace**, and spawns the user shell into
it. It holds re-delegated `BIND_NAMESPACE` (from service-mgr) and the building-block endpoints it
composes sessions from — the fs-server's and the profile server's forwarding endpoints, and
**`service-mgr`'s routes** to the tty server, the clipboard server, the view broker, the device
manager's info-only endpoint and the storage service's session endpoint — and resolves a channel to
auth-service itself. See `docs/architecture/session-and-auth.md`.

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
- **`libkern` + `librsproto` + `libstream` + `libheap` + `libsession`.** Still no `libos`
  unless a real need appears — and `libsession` was built to that rule for this reason, since
  a dependency's dependencies are yours. It remains a supervisor whose death is a system fault, so the *spirit* of
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
`BIND_NAMESPACE`, then transfers, in order:
1. the fs-server forwarding endpoint;
2. the **profile-server** forwarding endpoint;
3. the route to the **tty server**;
4. the route to the **clipboard server** (M12 Part E);
5. the route to the **view broker** (administration Part A.4);
6. the route to an **info-only endpoint of the device manager's** (administration Part B.4) — not
   the one reached at `/svc/devices`, which could subscribe to a device class;
7. the route to the **storage service's session endpoint** (administration Part C.6; resolved
   here from `/svc/storage/session-endpoint` until Part E.1b);
8. **`service-mgr`'s own session endpoint** for its services (administration Part E.2b), bound at
   `/dev/services`: the table of services, and nothing that starts or stops one.

**3–7 are `service-mgr`'s routes, not the servers' own endpoints** (administration Part E.1b): each
is an endpoint of `service-mgr`'s that continues every resolve into whichever instance of that one
server is running, so a session bound before a restart reaches the new server. `0` for a server
that did not come up; the session then binds nothing there.

session-mgr `recv`s all eight before doing anything. `desktop-session-mgr` gets the same with the
compositor's route fourth, nine in all, and the control channels are 10 deep; `service-mgr`'s
`create_control_channel` says why the depth is a bound on the count rather than a round number.
This list was three long until the PR #333 review, three parts after it had stopped being true,
and six long until the PR #340 review, one part after.

**There is no auth handoff as of M7 Part C.** This list said the third was the auth channel —
wrong twice over, since the third has been the tty endpoint for some time and the auth channel
is now *resolved* from `/svc/auth` rather than couriered at all. It is the list most likely to
be mis-applied, because the positional rule below is reasoned from it. The endpoints are handed over IPC (not the namespace)
because constructing namespaces means binding *endpoint handles* — and a `UserspaceServer`
binding resolves to a kernel registration record, never back to the endpoint, so a process
holding a LOOKUP-only root namespace can *use* `/bin` but can never obtain what it would
take to bind it elsewhere.

**The receives are positional.** A sender with an endpoint missing sends an *empty message*
rather than skipping the send; skipping would shift every later handoff up a slot and land
the tty endpoint where the profile endpoint belongs.

## What a session namespace contains

`libsession::build_namespace` builds it, for this column and the graphical one — and, since
administration Part F.1, `libsession::build` builds each application's in the graphical session,
from the same `NamespaceSpec`, with `/applications` and the console off and `/dev/draw/new` and
`/dev/desktop` on:
- `/home` — the user's home, a subtree of the fs-server;
- `/bin` — the profile server, whole-tree;
- `/applications` — the profile server's projection of each package's desktop entries;
- `/session/user` — who the session belongs to;
- `/dev/tty` — the tty server;
- `/dev/clipboard` — the clipboard server;
- `/dev/views` — the view broker, at the session's base `/s/<id>`, which is its identity there;
- `/dev/devices` — the device manager's tables, through an info-only endpoint at the base `/info`;
- `/storage` and `/dev/storage` — the storage service's session endpoint, at the bases `/fs` (every
  mounted filesystem) and `/info` (the table), through the route `service-mgr` hands over;
- `/dev/services` — `service-mgr`'s table of services (administration Part E.2b);
- `/dev/console` — **this column only** (`bind_console`); a graphical session has none;
- `/system/fonts` — **the graphical column only** (`bind_fonts`);
- **on an installer boot only**, the machine's block devices, each bound individually with its
  `info` snapshot (Phase 5 Part H.1).

The last is the conditional member that matters most, and it is a design decision like every
other: it is selected by the live image's own boot-menu entry, never by an installed system's
ordinary login, and it is what the view broker's `disks` grant gives instead. Both server bindings **share** init's
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
