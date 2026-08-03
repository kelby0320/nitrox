# Console and TTY

**Status: design, not built.** Written 2026-08-03 as a scoping pass, because "the console/tty
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

### One server, many ttys

Unlike the profile server, this needs no endpoint-per-consumer trick: a tty is *requested*, and
the channel handed back is per-session by construction. `session-mgr` asks for a tty when it
builds a session and binds the returned endpoint at `/dev/tty`.

## Staging

1. **The server, with the line discipline and a writable path.** `session-mgr` and `nxsh` move
   onto it; the three copies of line editing collapse into one. `eshell` stays on the raw device
   and `kprint`.
2. **Echo control as a request**, retiring the `echo: bool` parameter. Password entry becomes
   "the server is in no-echo mode", which a client cannot forget.
3. **History and reverse-search**, once there is one place that owns line state.
4. Later, independently: job control (needs a process-group concept), key events (needs the
   input slice), terminal emulation (needs the compositor).

## Open questions

- **Does the tty server outlive a session?** One server holding the single serial device, handing
  out per-session channels, is simplest. But when a session ends, its tty must be revoked or the
  next session inherits a live channel to the same device.
- **Where does the raw device go when the compositor exists?** A serial console and a terminal
  surface are two backends for one discipline. Deciding the backend seam now costs nothing;
  discovering it later costs a rewrite.
- **What arbitrates between eshell and the tty server?** Both want the one console. If eshell
  runs because the filesystem failed, the tty server is not running, so in practice they never
  overlap — but that is an argument, not a mechanism.
- **Is `/dev/tty` the right name** given there is exactly one, and a session's tty is not the
  system's console? `/dev/console` for the raw device and `/dev/tty` for the session's cooked
  view mirrors Unix closely enough to be unsurprising.
