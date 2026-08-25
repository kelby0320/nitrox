# desktop-session-mgr/CLAUDE.md

The **graphical login supervisor** — `session-mgr`'s twin. Read
[`session-mgr/CLAUDE.md`](../session-mgr/CLAUDE.md) as well: the rules there are this crate's
too, and this file exists because a per-crate `CLAUDE.md` is what Claude Code loads when
editing under *this* directory, so rules that live only next to the twin do not reach here.

## What it is

Authenticate → construct the namespace → spawn the leader → reap → tear down, exactly as the
serial column does, through the same [`libsession`](../libsession/src/lib.rs). It differs in
one place: **its greeter is a window**. That split is the whole reason `libsession` exists
(`docs/design/graphical-session.md` §4), and it is the line to keep — anything that is not
"how a user is asked for credentials" belongs in `libsession`, where both columns get it.

It is also the first compositor client to exist in a **release** image. Everything graphical
before M7 Part D was `selftest`-gated.

## Discipline (inherited from the supervisor family, and load-bearing here)

- **Zero build-mode `cfg`s, by construction.** `docs/planning/test-path-retrofit.md` took
  `session-mgr` from 31 to zero and `init` from 41 to one; this crate has never had any and
  must not acquire one. The greeter behaves in a test image the way it behaves in a release
  one, and `cargo xtask check-login` drives the real thing.
- **Never store, log, or echo a password.** The redraw diagnostic is a **count**. Reporting
  what was typed would put a credential on the console; reporting its *length* would leak it
  more slowly. `String::clear` on a field is not a scrub — it is a claim about the screen.
- **No hardcoded credential**, in source or in tests. The demo user is a build input.
- **Never spin.** Block on the event channel. A spinning supervisor keeps a run queue
  non-empty, so the idle thread never runs and deferred handle reclamation stops for the whole
  machine — the 2026-07-31 `logging-service` bug, found from a hung shell three subsystems
  away.
- **No `panic!()` / `unwrap()`** in normal operation. A greeter that cannot draw is not a
  degraded greeter: report and exit, and let the serial column remain the way in.

## Two things specific to this crate

- **Adding a member to a graphical session namespace is a design decision each time**, and
  more so than in the serial column. `/dev/console` is deliberately withheld
  (`bind_console: false`, governing decision 3) because the console is the recovery path.
  Note that `/dev/tty` *is* bound and reaches the same physical console — see
  `TODO(gui-dev-tty)`, whose trigger fired with this crate.
- **The greeter's window lands at the origin before every other client's**, because
  `service-mgr` brings the login chain up before declared services. `check-display` and
  `check-terminal` depend on their reference windows stacking above it. A change to this
  window's size, role, or presentation is a change to what the display gate compares —
  `display.yml` path-filters on this directory for that reason.

## Build

A bare-target bin crate: it needs `.cargo/config.toml` **and** `build.rs` + `user.ld`, not just
a `Cargo.toml`. Without them the ELF is `ET_DYN`, the kernel's loader rejects PIE, and the
symptom is `SYS_PROCESS_SPAWN` returning `InvalidArgument` long after a clean build.
