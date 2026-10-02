# Polish from the laptop install

Part of the [Nitrox Implementation Plan index](implementation-plan.md). Scheduled after
[administration](administration.md), which is complete, and before [Phase 6](phase-6-usb.md).

**Status: complete — Parts A–D built 2026-10-01, E 2026-10-02.** Parts A and E were shaped with the maintainer
on 2026-10-01; each part has its detail pass before it is built. **Nothing below describes current
behaviour** — Part A's is in [`rsproto-views-ops.md`](../spec/rsproto-views-ops.md) and
[`rsproto-tty-ops.md`](../spec/rsproto-tty-ops.md).

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
  of it — 128 random bits from the kernel's entropy, valid for thirty seconds. Only a process
  holding a terminal on that backend can get one: the shell and its stages, as `sudo`'s
  per-terminal window admits every process on the terminal. A GUI application holds none.
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

### Part A in detail *(2026-10-01)*

**The maintainer's calls**, the questions the sketch left:
- **Five minutes, fixed.** A per-rule length in `views.toml` waits for someone to need it.
- **Per view.** A password typed for `admin` skips the prompt for `admin` requests only.
- **`with --forget` forgets the whole session's windows**, every terminal's. Forgetting more is the
  safe direction, and it needs no token.

**The spike.** What the design rests on, checked against the source:
- **A backend's id is a `u32` from a counter**, never reused in a run; the console is `0`
  (`tty_server::routing`). Every sibling of a terminal shares its backend.
- **Resolving `/dev/tty` gives a fresh terminal channel**, on the console backend, and the server
  holds at most fourteen terminals (`MAX_TTYS`). So the broker does not keep one: it opens one to
  redeem a token and closes it after.
- **Any process can draw random bytes** (`sys_entropy_create`), as `nxinstall` does for a GUID.
- **The broker already calls servers it trusts synchronously, with a deadline** — the storage
  service's `InUse`, `auth-service`'s check — and drops the channel when one does not answer. The
  redeem is the same shape.
- **A `Request` is a sequence of fields ending with the environment**, so a token can be an
  optional field after it, and a caller that sends none — `desktop-shell`'s Restart — is
  unchanged.
- **Sixteen gate sites wait for `with`'s prompt**, most of them in `test-interactive`'s run of
  `with admin` steps on one serial terminal, and all of them would stop being asked after the first
  password. A gate whose subject is the prompt runs **`with --forget` first**, so no gate's outcome
  depends on five minutes passing or not under TCG.

**The wire:**
- **`Tty::Token` (`0x0B0A`)**, on a terminal: no body. Reply: 16 bytes, a token for the terminal's
  backend, valid for thirty seconds and once.
- **`Tty::Redeem` (`0x0B0B`)**, on any terminal: the 16 bytes. Reply: the backend's id, 4 bytes,
  and the token is gone; or `NotFound` for one never minted, already redeemed, or expired.
- **`Request` gains an optional last field**: the byte `16`, then the token; absent when the caller
  has none. Any other length is refused, `0` included: a correct writer never writes one.
- **`Forget` (`0x0E10`)**, client: no body, empty reply. The session's windows are gone.

**The broker:**
- **On a request whose rule asks for a password**, a token is redeemed first. A window for this
  session, this backend and this view that has not expired starts the program without asking. The
  audit says `allowed, within the grace period`.
- **Otherwise it asks, as today**, remembering the backend the token named. When the password
  succeeds, a window opens for that session, backend and view.
- **A refused password closes every window of the session.** So does `Forget`, and so does the
  session ending.
- **A request whose token names nothing**, or that has none, asks. A redeem that does not answer
  within its deadline counts as naming nothing.
- **The windows live in a table in the library**, host-tested at their expiry and either side of
  it, per view, per backend and per session.

**`with`:**
- Before it asks the broker, it asks its own terminal for a token, and sends it if it got one.
- **`with --forget`** sends `Forget` and prints nothing, as `sudo -k` prints nothing.

**The pieces:**
- [x] **A.1 — tokens in `tty-server`.** A `tokens` module in its library, host-tested: one minted
  and redeemed, one redeemed twice, one never minted, one expired at thirty seconds and one just
  short of it, and a full table. `Token` and `Redeem` in the server.
- [x] **A.2 — the broker's window, and `with`.** The request's token field and `Forget` in
  `librsproto`, with tests that hand the parser bodies a correct writer would not produce; the
  window table; the redeem; the audit; `with`'s token and `--forget`.
- [x] **A.3 — the gates.**
  - `boot-probe`, in `test-qemu`, opens a session as a supervisor does, takes a real token from a
    terminal of its own, and answers the prompt with the build's fixture password. Then it checks
    that a second real token is started without asking. It checks that each of these is asked: a
    made-up token, a reused one, another view, a token from a terminal on a backend of its own —
    one it attached, as `nxterm` does for a window — and a request after `Forget` (**the forgery
    control**, and the per-terminal claim).
  - `test-interactive` gains the step for a person: `with admin` twice in a row asks once, and once
    more after `with --forget`. Every step whose subject is the prompt forgets first.
  - The other serial gates, and `check-install`'s helper, forget first.
  - `check-login` 9a2: the second request in the same desktop terminal is not asked.

## Part B — an empty table cell drawn blank

**What exists.** `nxsh` renders `Value::Null` as `null` everywhere (`userspace/nxsh/src/value.rs`),
table cells included, so `disk --list` shows a column of `null` for every disk without a filesystem.

**The change:** in a table cell, a null is drawn blank. A bare `null`, the value of an expression,
is still `null`, since there it is the answer.

**Gate:** `nxsh` host tests for a table with a null cell and for a bare `null`; `test-interactive`'s
storage step reads a blank cell where `disk --list` has a null.

**Docs:** [`shell-language.md`](../spec/shell-language.md), where values are rendered.

### Part B in detail *(2026-10-01)*

- [x] **Built.** `display` (`userspace/nxsh/src/ops.rs`) draws a cell whose value is null blank.
  - Only the cell itself: a null inside a list in a cell is still `[null]`, and a bare `null` is
    still `null`.
  - **A row ends at its last non-blank cell**, so blank cells at the end leave no run of padding
    for a narrow terminal to wrap. Blank cells before it are padded, so the columns stay aligned.
  - **Host test**: a table with blank cells in the middle and at the end, a bare `null`, and a
    nested one. **Control**: the old rendering fails it with `blk-0  null     null`.
  - **Gate**: `test-interactive` 20c(a). The row for `init`'s root has a null `clean`, since it is
    mounted writable, and must not draw `null`. **Control**: the old rendering fails it there, on
    the real row.

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

### Part C in detail *(2026-10-01)*

- [x] **Built.** `description`, after `kind`, is the device record's name: `QEMU HARDDISK (QM00001)`
  for the release disk, `module 3 (/boot/install-root.img)` for a RAM disk, `nitrox-root` for a
  partition. A record that names nothing has an empty cell, so it draws blank (Part B).
  - **Host test**: each kind, and an unnamed record. Two older tests indexed `clean` and `by` by
    position, which the new column would have moved; they find them by name now, as every reader
    of the table already did.
  - **Gate**: `test-interactive` 20c(a) reads `blk-0`'s row and expects the model and serial, which
    the typed command does not contain. **Control**: with the column emptied, the gate times out
    at that step.

## Part D — the build's commit on the screen

**What exists.** Nothing on a running system says which build it is. On 2026-10-01 a stick that had
never been rewritten looked, for an afternoon, like a bug in the installer.

**The change:** `xtask` passes the commit it builds from — `git rev-parse --short=12 HEAD`, with
`-dirty` when the tree has changes — to every build. **The kernel logs it at boot**, so it is on
the hardware report's first page, and **`nxsh`'s banner** says it, so it is in every terminal.

**Gate:** `test-qemu` asserts the kernel's line names the commit `xtask` built from;
`check-report` reads it off the report's first page; `test-interactive` reads it in the banner.

**Docs:** [`boot-flow.md`](../architecture/boot-flow.md), and the hardware report's description.

### Part D in detail *(2026-10-01)*

- [x] **Built.** `xtask` works the commit out once, at start (`build_commit`), and sets
  `NITROX_COMMIT` in its own environment, so every cargo it runs inherits it and no build path can
  forget it. The kernel and `nxsh` read it with `option_env!`, which rustc records as a dependency,
  so a new commit rebuilds those two crates and nothing else.
  - **The kernel** logs `nitrox: built from <commit>` before the handoff lines, on the report's
    first page. **`nxsh`'s banner** reads `nxsh: interactive shell, Nitrox <commit> (…)`.
  - **Gates**: `test-qemu` holds the kernel's line to the commit `xtask` built from, beside the
    hardware facts; `check-report` reads it on the report's **first** page; `test-interactive`
    reads it in the serial shell's banner. **Controls**: a kernel logging a made-up commit fails
    `test-qemu`, and a banner naming one fails `test-interactive`.

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

### Part E in detail *(2026-10-02)*

**The spike:**
- **A diagnostic is one IPC message whose payload is the text**, nothing else
  (`coreutils::stage::send_diag`, `nxinstall`'s `say_to`). `nxsh`'s `drain_diagnostics` paints
  every one in `style::DIAG`, or prints it through `kprint` with no terminal.
- **About 250 `stderr` writes in the coreutils, nearly all errors.** So a message with no level
  must stay an error, and only the informational lines change.
- **Every coreutil answers `--help` with the same two lines**, `stage.diag(HELP); exit(EXIT_OK)`;
  `with` answers with `stage.die(HELP, EXIT_OK)`. `clip` already writes text to `stdout` as a
  `TEXT_FALLBACK` stream, which `display` prints as lines.
- **`nxsh` is handed a terminal in script mode and does not use it**: `run` sets the host's
  terminal to none, so `--help` goes to `kprint`.
- **The design has no warning colour.** ANSI bright yellow (`93`) is the one left, and the
  palette, not the shell, decides what it looks like.

**The design:**
- **A level is a leading byte**: `0x01` a notice, `0x02` a warning, and a message whose first byte
  is neither is an error, whole. No text starts with either control, and every unchanged sender
  stays an error. The framing lives in `libstream::diag`, shared by every sender and the shell,
  and host-tested.
- **The shell paints by level**: an error in `DIAG`, a warning in a new `WARN` (bright yellow), and
  a notice plain. Printed through `kprint`, the level byte is dropped.
- **`Stage::answer`** writes a program's usage to `stdout` as a `TEXT_FALLBACK` stream, and exits 0.
  With no `stdout` it sends the usage as a notice. Every coreutil and `with` use it, and
  `nxinstall` does the same with its own lines; **`nxsh`** writes its usage on the terminal it was
  handed, and through `kprint` only without one. **`--version` too**, found on the way: it was the
  same two lines as `--help`, and the same answer drawn as an error.
- **What is informational:** `nxinstall`'s progress, its "done" and "nothing was written." are
  notices, and its notes on why a disk is missing are warnings. `with`'s "gets no diagnostic
  channel" is a warning. The sweep of the coreutils' other lines decides theirs.

**The sweep, as built.** Every other line stays an error. Notices: a result said in words where
there is no `stdout` (`disk`'s "mounted at" and "unmounted", `service`'s, the text listings of
`disk`, `service`, `desktop`, `account` and `with --list`), `shutdown`'s "shutting down" and
"restarting", `account`'s "set a new password" and a broker's answer that it did what was asked
(`account`, `with --check`, `with --install`), `with --show FILE`'s "wrote the policy to", and a
cancelled question in `nxinstall`. Warnings: `date`'s clock set for this boot only, and `log`'s
records dropped from the ring. **`with --show` with no file writes the policy to `stdout`**
(`Stage::text_out`) rather than as one diagnostic, which a policy longer than a message's payload
could not be — it went to the kernel log.

**Gates:**
- `test-interactive` 20b(d) asserts the **warning** colour on `nxinstall`'s listing note. That
  stays the proof that the line came through `stderr` and not `kprint`.
- `test-interactive` reads `disk --help` **not** in the error colour — and, since a notice is
  drawn plain too, counts it: `disk --help | count` is 13 only if the usage is on `stdout`.
- `test-interactive` 20d(b) reads `with --show FILE`'s notice plain. The `stderr` line names the
  file where the console's says "a copy", so finding it proves the path.
- The reset-before-the-line-ends check covers a warning as well as an error.
- `check-terminal` reads `nxsh --help` in the grid, which only a terminal writes to: `kprint`
  never reaches it.
- Host tests on the framing, on a message with no level being an error, and on the shell's colour
  for each level.

- [x] **Built** (2026-10-02). **Controls, each failing at its own step:**
  - every level painted as an error, the old paint: 20b(d);
  - a notice painted as an error: 20d(b);
  - `--help` sent as an error: the `disk --help` colour check;
  - `--help` sent as a notice: the count, which reads nothing;
  - `nxsh --help` through `kprint`: `check-terminal`, with the usage on the serial port and not
    in the grid.
