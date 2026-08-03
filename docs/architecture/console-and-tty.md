# Console and TTY

**Status: stage 1a built (2026-08-03) — the server exists and `/dev/tty` is a capability.
Its clients have not moved yet.** Written 2026-08-03 as a scoping pass, because "the console/tty
server" is referenced as a gate in four documents without existing anywhere. This says what it
owns, what it does not, and what has to be decided before it can be built.

## What exists today

Verified against the source rather than recalled:

**Input is a capability.** `/dev/console` is a char-class `DeviceNode` served by an in-kernel
driver (`drivers::console`): COM1 RX IRQ → ring → a parked `sys_io_submit(Read)`. A process
that has the handle can read; one that does not, cannot. The driver delivers **raw bytes** and
says so — "echo and line editing live in userspace".

**Output is not.** There is no write path at all: `CharBackend` has only `submit_read`. Every
byte any userspace program prints goes through `SYS_DEBUG_KPRINT`, an ambient debug syscall
that takes no handle.

**So the console is half a capability**, and the half that is missing is the half that matters
for a system whose thesis is that authority travels through the namespace. A process with an
empty namespace can still write to the console. Nothing can redirect, pipe, capture, or log a
shell's output, because there is no object to redirect.

**Line editing is duplicated three times** — `eshell`, `session-mgr`'s login, and `nxsh`'s REPL
each implement backspace, echo, and line accumulation over the raw byte stream. They have
already diverged once: `session-mgr`'s `read_line` returned on CR/LF without echoing it while
`nxsh`'s loop echoed CRLF, which is why the password prompt rendered as `alicepassword:` for
weeks.

**Password entry is echo-suppression by convention.** `read_line(..., echo: false)` is a
parameter each caller must remember to pass. A client that forgets echoes the password.

**The single-reader assumption is undocumented in the consumers.** The driver notes it
("single-reader (eshell) in Phase 2"), but today `session-mgr` reads for login and `nxsh` reads
at the prompt. They alternate rather than overlap — session-mgr's login finishes before the
shell spawns, and it reads again only after the shell exits — so this is fragile rather than
broken. A second concurrent reader would race for bytes with no arbiter.

## What the TTY server owns

1. **The line discipline.** Canonical vs raw mode, echo on/off, backspace, word-erase, kill,
   and the line buffer. One implementation instead of three.
2. **The output path.** A writable object, so printing becomes a capability like everything
   else and `SYS_DEBUG_KPRINT` reverts to being what it is: a kernel debug facility for init
   and eshell, before a tty exists.
3. **Exclusive ownership of the raw device.** It holds `/dev/console`; sessions do not. That
   removes the multi-reader race by construction rather than by convention.
4. **Per-session isolation.** Two sessions must not see each other's input.

## What it does not own

Naming these keeps the scope honest — each is a separate design problem that this server would
otherwise absorb by accident.

- **Job control.** `fg`/`&` need process groups, which do not exist in this kernel (no `pgid`
  anywhere), and cannot be built on signals, which the design rejects outright. Interrupting a
  foreground pipeline has to be expressed with the notification queue and process handles. That
  is its own design, and it is *not* a prerequisite for the rest of the rich REPL.
- **Key events.** Shift-Enter continuation needs to distinguish a modifier, which a serial byte
  stream structurally cannot express. That waits on a real keyboard driver (the display + input
  slice), not on this. The rich REPL therefore splits: history, reverse-search and completion
  need cooked lines and can land here; anything needing modifiers cannot.
- **Terminal emulation.** ANSI parsing, scrollback, and a rendered grid belong to the
  compositor terminal. This server should be shaped so that terminal is simply another
  *backend* — serial today, a compositor surface later — but it should not contain one.

## Shape — the decisions

### It is a userspace resource server

Like the fs-server, the profile server and the logging service: spawned by a supervisor, bound
into namespaces by a supervisor, never self-registering. It holds `/dev/console` and is bound
at **`/dev/tty`** in each session namespace by `session-mgr`, exactly where `/dev/console` is
bound today.

The consequence is the point: **a session gets `/dev/tty` and not `/dev/console`.** It cannot
reach the raw device at all, because the name is not in its namespace.

### A client gets an IPC channel, not a device node

Two candidates:

- **Extend the char device** with `CharBackend::submit_write` and hand out a `DeviceNode`. Keeps
  `sys_io_submit` as the one I/O verb, but requires kernel work, and a byte device has nowhere
  to put "turn echo off" or "give me raw mode" — those become out-of-band syscalls or ioctl-like
  warts.
- **An IPC channel** (recommended). The tty server is userspace; the way to talk to a userspace
  server here is IPC. Mode changes are ordinary requests on the same channel rather than a
  second mechanism, and it needs no kernel change.

The cost of the channel is that reading a line is a round trip rather than a device read. For an
interactive terminal that is irrelevant.

### Output becomes a write on that channel

`nxsh` writes its prompt and results to its tty handle instead of `kprint`. This is the change
that makes shell output redirectable, and the one that lets a test capture it without scraping
serial.

`eshell` deliberately keeps `kprint`: it is the emergency path that runs when the filesystem —
and therefore the tty server — may not exist.

### One server, many ttys — and which channel gets bound

A tty is *requested*: resolving `/dev/tty` mints a fresh channel for the caller, exactly as
resolving `/log/<principal>` does.

**The distinction that cost an afternoon.** A channel plays one of two roles, and they are
indistinguishable by type:

| Role | Speaks | Who binds it |
|---|---|---|
| **forwarding endpoint** | `Namespace::Resolve`, sent by the kernel | init at `/dev/tty` in the root ns; session-mgr at `/dev/tty` in each session |
| **minted tty channel** | `Tty::ReadLine` / `Write` / `SetMode` / `Close` | nobody — it is *held*, not bound |

The kernel adopts **any** bound `IpcChannel` as a userspace server, so binding a minted tty
channel produces a namespace entry that answers `Namespace::Resolve` with `Unsupported` — a
`/dev/tty` that exists and cannot be opened. Both are `IpcChannel`s; only the role differs,
and only the binder knows which it holds.

So a session binds the **forwarding endpoint**, handed down init → service-mgr →
session-mgr alongside the fs and profile endpoints, sharing init's registration exactly as
`/home` shares the fs-server's. Every program in the session then resolves its own terminal.

### What ends a terminal

A program's terminal ends when the **program** exits: its handle closes, the server sees
`PeerClosed`, and frees the tty. Exit-context teardown makes that prompt.

`Tty::Close` remains for the case that needs it — a supervisor ending a terminal it holds,
as `session-mgr` does with the one it used for the login prompt. It is the revocation point
because a refcounted handle cannot be taken back; the server declining to serve is what
ends the terminal regardless of who still holds the other end.

## Staging

**A migration constraint discovered while building stage 1a, and it shapes the rest.** The
console driver is single-reader, and until every client has moved onto this server,
`session-mgr`'s login and `nxsh`'s REPL still read the device directly. A
permanently-outstanding read in the server *steals their input* — the interactive login test
timed out waiting for a password prompt whose keystrokes the server had swallowed.

So the server submits a console read **only when a terminal is actually waiting for a
line**. A terminal with no reader competes with nobody, which is what lets clients migrate
one at a time instead of all at once. That is not a temporary hack: reading on demand is the
right behaviour anyway.

1. **The server, with the line discipline and a writable path.** ✅ 2026-08-03 `session-mgr` and `nxsh` move
   onto it; the three copies of line editing collapse into one. `eshell` stays on the raw device
   and `kprint`.
2. **Echo control as a request**, retiring the `echo: bool` parameter. Password entry becomes
   "the server is in no-echo mode", which a client cannot forget. ✅ 2026-08-03 —
   `session-mgr`'s login moved onto the tty and its copy of the line editor is deleted.
   (An earlier draft of this line said "one terminal per session, closed when the session
   ends". Stage 1c changed that: the session binds the *forwarding endpoint*, so each
   program resolves its own terminal and a terminal ends when its holder exits. See
   "What ends a terminal" below.)
3. **History and reverse-search**, once there is one place that owns line state.
4. Later, independently: job control (needs a process-group concept), key events (needs the
   input slice), terminal emulation (needs the compositor).

## Stage 3 — line editing, history, and where they belong

Scoping pass, 2026-08-03. Stage 3 reads as "add history", and the interesting part is that
deciding *where* it lives decides the shape of everything after it.

### What Linux does

Worth being precise, because the split is the useful part:

- **The kernel's `N_TTY` line discipline** implements *canonical mode*: erase (backspace),
  kill (`Ctrl-U`), word-erase (`Ctrl-W`), reprint (`Ctrl-R`), and returning completed lines.
  That is the whole of it. It has no history, no arrow keys, no completion.
- **History, reverse-search and completion live in userspace libraries** — GNU readline,
  libedit — linked into the application. Not in the kernel, and not in a server.
- **An interactive program turns canonical mode off.** `bash` clears `ICANON` and `ECHO` via
  `termios` and does *all* editing itself: it wants the keystrokes, not the lines.
- **Arrow keys are escape sequences** (`Up` = `ESC [ A`), parsed by the application against
  terminfo. The kernel passes the bytes through untouched.

One detail sharpens it: `Ctrl-R` means *reprint the line* to `N_TTY` and *reverse-search
history* to readline. The same key means different things at the two layers because the
layers do not overlap — the kernel's editing is what you get when the application does none.

### What that maps to here

Our `Discipline` is `N_TTY`: canonical mode, in the server, returning lines. The question is
whether history joins it there or goes in the shell.

**The decisive argument is completion, not history.** §11 wants *schema-aware* tab
completion — completing on a command's typed parameters. That needs the shell's knowledge of
commands, schemas and bindings, which the tty server does not have and should not acquire.
And completion is not a separate feature bolted beside history: they are the same editing
loop, dispatching on different keys against the same line buffer. Put history in the server
and completion in the shell, and there are two editors fighting over one line.

So the editing loop belongs where completion must be: **in the shell**. Which is exactly the
conclusion Linux reached, for the same reason.

### The obvious objection

*Does that not recreate the fourth copy of line editing we just deleted?*

No, and the distinction matters. The three copies we removed were three **implementations**.
What Linux has is one implementation deployed as a **library**. We already have that: the
discipline is a crate half with no syscalls in it, precisely so it can be linked rather than
reimplemented. `nxsh` linking it is not a fourth copy; it is the second *deployment* of the
first copy.

Being honest about the residual cost: two deployments of one implementation can still
diverge in *use* — canonical mode in the server for simple readers, raw mode in the shell
for interactive ones — and a bug fixed in one call path is not automatically exercised in the
other. The 16 host tests cover the discipline itself either way.

### What it needs

- **A raw read** returning *available bytes* rather than waiting for a line. The shell reads
  this way, gets keystrokes, and echoes through the write path it already has.

  Built 2026-08-03 as `Tty::Read`, and **without** the `TTY_MODE_RAW` flag this section
  first proposed: two read ops say the same thing with less state. A client asks for what it
  wants per read, and cannot leave a terminal in a mode some later reader did not expect —
  which is a hazard `termios` has and this does not need to inherit.
- **Escape-sequence recognition.** `ESC [ A` is three bytes; the discipline currently drops
  `ESC` as an undefined control byte. This is a small state machine, and it is the first step
  toward terminal *input* parsing — worth deciding deliberately where that stops rather than
  letting it grow.
- **A redraw model.** Reverse-search rewrites the whole visible line repeatedly. Today the
  discipline only ever appends a character or erases one, so it does not know what is
  currently displayed. Redrawing needs it to.

### The cheaper alternative, and why not

**History in the server**: it keeps a per-terminal ring, handles `Up`/`Down` itself, and
returns the recalled line. No raw mode, no client editing, no escape parsing in the shell —
much less work, and it would deliver history and reverse-search now.

It is rejected because it has to be **removed** when completion arrives: the editing loop
moves to the shell at that point, and everything built server-side is thrown away. It also
gives the server shell semantics it should not have, and leaves two shells on one terminal
sharing a history that is not really theirs.

Cheap-now-rewrite-later is the right trade when the later may never come. Here §11 names
completion explicitly, so it will.

### Scope

Stage 3 is **raw mode + the editing loop moving into the shell, with history**.
Reverse-search follows once the redraw model exists; completion is a separate piece needing
the schema work. Splitting them keeps the first landing small enough to verify.

Not in stage 3, unchanged: job control, key events needing modifiers (`Shift-Enter` cannot
be expressed as a serial escape sequence — that waits on a real keyboard driver), and
terminal emulation.

## Resolved (2026-08-03)

**One server; `session-mgr` closes the tty when the session ends.** It already closes the
session namespace at logout, so this is the same shape: drop the binding, drop its own handle,
and the endpoint's last reference goes with the shell's — the server sees `PeerClosed` and
releases that session's state. Exit-context teardown makes that prompt rather than eventual.

**With one refinement, because closing is not revoking.** Handles are refcounted and this
kernel has no revocation — only `duplicate` and `restrict` — which is inherent to capabilities
rather than a gap. So if any process outlives the session still holding a resolved `/dev/tty`
handle (a background program the shell spawned, inheriting it through the namespace), closing
session-mgr's copy does not take it away, and that orphan keeps a live channel to the console.

The capability-consistent answer is that **the server is the revocation point**: `session-mgr`
tells it the session is over, and it stops honouring that channel regardless of who still holds
the other end. A resource holder declining to serve, not the kernel confiscating. That makes
teardown a guarantee rather than a convention, and it costs one request.

**Backend seam: serial now, compositor later.** The device side is pluggable from the start —
the line discipline talks to a backend, and the backend is the serial console today and a
terminal surface later. Building the seam now costs nothing; retrofitting it costs a rewrite.

**`eshell` is separate, and has to be.** It is the path that runs when the filesystem failed,
so the tty server does not exist when it matters. It keeps the raw `/dev/console` and
`SYS_DEBUG_KPRINT`. The two never overlap because eshell's precondition is the server's
absence — and that is now a stated invariant rather than an accident of timing.

**`/dev/tty` is the name.** `/dev/console` stays the raw device, held by the server; `/dev/tty`
is the session's cooked view. Close enough to Unix to be unsurprising.
