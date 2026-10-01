# Polish from the laptop install

Part of the [Nitrox Implementation Plan index](implementation-plan.md). Scheduled after
[administration](administration.md), which is complete, and before [Phase 6](phase-6-usb.md).

**Status: scoped 2026-10-01; nothing built.** Parts A and E were shaped with the maintainer on
2026-10-01; each part has its detail pass before it is built. **Nothing below describes current
behaviour.**

## Why it exists

Installing on the laptop, from 2026-09-30 to 2026-10-01, was the first time a person used the
administration tools on the machine they were built for. It worked, and it turned up five rough
edges, none of which belongs to USB. Listed in Phase 6's plan they had no checkbox, no gate and no
row in the phase table — the state that kept the kernel-module loader moving from plan to plan for
months — so they are a small plan of their own.

| Part | What | Shape |
|---|---|---|
| **A** | A grace period for `with`, keyed on the session and the terminal | decided; a token `tty-server` mints and the broker redeems |
| **B** | An empty table cell drawn blank, not `null` | mechanical |
| **C** | The disk's model and serial in `disk --list` | mechanical |
| **D** | The build's commit on the screen | mechanical |
| **E** | Which output is a diagnostic: `--help` on `stdout`, and a level on every `stderr` message | decided; touches every program |

**A comes first**: it is the improvement the maintainer most wants. The rest in any order.

## Decisions, 2026-10-01

The maintainer's calls:
- **The grace window is keyed on the session and the terminal**, as `sudo` keys it on the
  terminal. A window keyed on the person would carry a password typed at the desktop to a serial
  login; one keyed on the session alone would carry it to every window and application in the
  desktop session, since `desktop-shell` binds the broker into everything it launches. Keyed on
  both, only the terminal that typed the password skips the prompt — **provided the broker learns
  the terminal from `tty-server` itself**, never from the caller (*Part A*, below).
- **`stderr` keeps progress and gains levels.** `--help` is the answer to what was asked, so it goes
  to `stdout` and exits 0. Progress stays on `stderr`: `stdout` is the pipeline's value, which the
  shell gathers and shows when the pipeline ends, so progress there would arrive all at once at the
  end and be piped as data — which is why `dd`, `curl` and `git` keep theirs off `stdout` too. What
  changes is the colour: each message carries a level, and only errors are painted as errors.

## Part A — a grace period for `with`

**What exists.** Every request asks for a password whose rule says so. The broker receives the
caller's terminal with the request (`REQ_TERMINAL`) and passes it to the program it starts. A serial
login and a desktop login are separate broker sessions, each with its own `/s/<session>` base.
`tty-server` mints sibling terminals on one backend (`Tty::OpenSibling`) — every stage of a
pipeline gets one — so a window's terminals share a backend. `TODO(view-grace)` in
[`deferred-decisions.md`](../rationale/deferred-decisions.md) records the question.

**Why the caller cannot simply say** (PR #350 review). The first version of this plan had the
broker ask the terminal handle in the request which terminal it was. But the caller chooses that
handle: `with` fills it, any process can create a channel pair, and nothing lets the broker tell a
`tty-server` terminal from a channel the caller serves itself — `sys_handle_stat` reports an
`IpcChannel` either way. An application could answer as window 1, trying ids until one matched.
**The broker has to hear the terminal's identity from `tty-server`, over a channel it holds
itself.**

**The design:**
- **`Tty::Token`**, asked by `with` on its own terminal: `tty-server` mints a **one-time token**
  for that terminal's backend — the serial console, or one terminal window, shared by every sibling
  of it — 128 random bits from the kernel's entropy, valid for a few seconds. Only a process holding
  a terminal on that backend can get one: the shell and its stages, as `sudo`'s per-terminal window
  admits every process on the terminal. A GUI application holds none.
- **`with` puts the token in its request**, beside the handles it already sends.
- **The broker redeems it with `tty-server`** over its own channel, resolved from its own namespace
  (`tty-server` serves `/dev/tty` there): `tty-server` answers which backend the token was minted
  for, once, and forgets it. A token it did not mint, or one already redeemed, names nothing, and
  the request asks for a password as it does today. **The broker never calls out on a handle a
  caller sent**, so a channel that never answers cannot stall it; how the redeem waits without
  holding up other sessions is the detail pass's.
- **After a password succeeds, the broker remembers it** for that session, that terminal and that
  view, for five minutes. A later request matching all three, whose rule asks for a password, starts
  without asking, and the audit says it did.
- **It ends** when the time runs out, when a password is refused, when the session ends, and when
  the person runs **`with --forget`**, as `sudo -k` does.
- **A request with no token always asks**, which without a terminal means it fails: a GUI
  application holds no terminal, so it cannot use a window a terminal opened.
- **For the detail pass:** whether five minutes is fixed or set per rule in `views.toml`, and
  whether a window for `admin` covers another view. The proposal is per view.

**Gate:**
- `test-interactive`: a second `with admin` inside the window does not ask, one after
  `with --forget` does, and the audit says which.
- `check-login`: a second terminal window still asks.
- Host tests on the broker's window at its expiry and either side of it, and on `tty-server`'s
  tokens: one redeemed twice, one never minted, one expired.
- **A forgery control**: a test client that hands the broker its own channel as a terminal and a
  made-up token is asked for a password inside the window.

**Docs:** [`rsproto-tty-ops.md`](../spec/rsproto-tty-ops.md),
[`rsproto-views-ops.md`](../spec/rsproto-views-ops.md),
[`session-and-auth.md`](../architecture/session-and-auth.md), the deferral resolved.

## Part B — an empty table cell drawn blank

**What exists.** `nxsh` renders `Value::Null` as `null` everywhere (`userspace/nxsh/src/value.rs`),
table cells included, so `disk --list` shows a column of `null` for every disk without a filesystem.

**The change:** in a table cell, a null is drawn blank. A bare `null`, the value of an expression,
is still `null`, since there it is the answer.

**Gate:** `nxsh` host tests for a table with a null cell and for a bare `null`; `test-interactive`'s
storage step reads a blank cell where `disk --list` has a null.

**Docs:** [`shell-language.md`](../spec/shell-language.md), where values are rendered.

## Part C — the disk's model and serial in `disk --list`

**What exists.** The storage service's table names each device `blk-<n>` and says what it holds,
where it is mounted and whether it was left clean ([`storage.md`](../architecture/storage.md) §9).
The model and serial are in the device's record — `nxinstall` prints them — but not in the table.

**The change:** a `description` column, from the device's record: the model and serial for a SATA
disk, the module for a RAM disk, the partition's name for a partition. The column is additive, so
everything that filters the table by name or mount keeps working.

**Gate:** `test-interactive` reads the release disk's description in `disk --list`; the storage
service's host tests cover each kind.

**Docs:** [`storage.md`](../architecture/storage.md) §9.

## Part D — the build's commit on the screen

**What exists.** Nothing on a running system says which build it is. On 2026-10-01 a stick that had
never been rewritten looked, for an afternoon, like a bug in the installer.

**The change:** `xtask` passes the commit it builds from — `git rev-parse --short=12 HEAD`, with
`-dirty` when the tree has changes — to every build. **The kernel logs it at boot**, so it is on
the hardware report's first page, and **`nxsh`'s banner** says it, so it is in every terminal.

**Gate:** `test-qemu` asserts the kernel's line names the commit `xtask` built from;
`check-report` reads it off the report's first page; `test-interactive` reads it in the banner.

**Docs:** [`boot-flow.md`](../architecture/boot-flow.md), and the hardware report's description.

## Part E — which output is a diagnostic

**What exists.** Every message on `stderr` is drawn in the error colour: the shell paints each
diagnostic it drains (`nxsh`'s `drain_diagnostics`), and a message carries no level. So
`nxinstall`'s progress and every coreutil's `--help` read as errors.

**And `nxsh --help` reaches nobody on the laptop.** It writes through the shell's own output, which
with no terminal — as when it is run as a stage, in script mode — falls through to `kprint`
(PR #350 review). It is the stderr fix's class of bug, on a different path.

**The change:**
- **`--help` writes to `stdout`** and exits 0: the coreutils, `with` and `nxinstall`. **`nxsh`** is
  spawned with no `stdout` as a session's shell, so its usage goes where its output goes: `stdout`
  when it is a stage, its terminal otherwise, and never `kprint`.
- **A `stderr` message carries a level** — error, warning or notice. **A message with none is an
  error**, so a program not yet changed draws as it does today. How the level is encoded is the
  detail pass's, in [`pipeline-stdio.md`](../spec/pipeline-stdio.md).
- **The shell paints by level**: errors in the error colour, warnings in their own, notices plain.
- **Programs say which they mean**: `nxinstall`'s progress and "done" are notices, as are other
  programs' informational lines. Its notes on why a disk is missing — "in use", "holds the running
  system" — are **warnings**: each is a reason something the person may have wanted is not there.

**A gate that proves the `stderr` path needs a painted level** (PR #350 review). On the serial
console a message with no colour cannot be told from `kprint`, which is why `test-interactive`
step 20b(d) checks the colour at all. Its line is `nxinstall`'s listing note for `/dev/blk/0`, not a
refusal — so as a warning it stays painted, and the check asserts the warning's colour.

**Gate:** host tests on the encoding, and on a message with no level being an error;
`test-interactive` reads a notice unpainted, a warning and an error each in its colour, and 20b(d)'s
note as a warning; `--help` arriving as output, not as a diagnostic; `nxsh --help` reaching the
terminal it was run in.

**Docs:** [`pipeline-stdio.md`](../spec/pipeline-stdio.md),
[`console-and-tty.md`](../architecture/console-and-tty.md).
