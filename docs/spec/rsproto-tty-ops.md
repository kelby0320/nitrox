# rsproto — Tty operations (`0x0Bxx`)

**Status: normative for what is built (2026-08-13).** `ReadLine`, `Read`, `Write`, `SetMode`,
`Close` and `Interrupt` are implemented in `userspace/tty-server/`; `AttachBackend`, `Output`
and `Input` landed with Milestone 5 Part C. Job control and terminal emulation are unbuilt —
see [`console-and-tty.md`](../architecture/console-and-tty.md) for the design and its staging.

Written 2026-08-13, when Part C gave this category a **second channel role** and the contract
stopped fitting in doc comments. Every other rsproto category already had a spec file; this one
did not, which is why the "one terminal per session" misreading survived as long as it did.

## The shape

A terminal is **requested, not bound**: resolving `/dev/tty` mints a fresh channel for the
caller, exactly as `/log/<principal>` does. What gets bound into a namespace is the server's
**forwarding endpoint**; what a program holds is a **terminal channel**. They are both
`IpcChannel`s and only the role differs — binding the wrong one produces a `/dev/tty` that
exists and answers `Namespace::Resolve` with `Unsupported`.

Since Part C there is a third role, the **backend channel**, held by a terminal emulator.

| Role | Speaks | Who holds it |
|---|---|---|
| forwarding endpoint | `Namespace::Resolve` | bound by init at `/dev/tty`; by session-mgr in each session |
| terminal channel | `ReadLine` / `Read` / `Write` / `SetMode` / `Close` / `AttachBackend` | the program using the terminal |
| backend channel | `Output` (server→emulator), `Input` (emulator→server) | a terminal emulator |

**A terminal is per resolver, not per session.** Each program that resolves `/dev/tty` gets its
own — `session-mgr` opens one for the login prompt, `nxsh` opens its own, and every stage `nxsh`
spawns can open another. Nothing in the protocol groups them by session.

## Operations

### `ReadLine` (`0x0B00`)

Request: empty. Reply: the line's bytes, **no terminator**.

Runs the line discipline: echo, erase, kill, and the `Ctrl-D`/`Ctrl-C` rules below. **One
outstanding read per terminal** — a second is refused with `WouldBlock`, because two would race
for the same input and there is no rule saying which should win.

`Ctrl-D` at an empty line replies **`PeerClosed`**, not an empty line. Those are different
answers: a reader that conflated them would either exit on a stray Enter or never exit at all.

### `Read` (`0x0B04`)

Request: empty. Reply: whatever is available, up to 64 bytes.

The raw counterpart. Completes as soon as any byte arrives; the discipline neither consumes,
echoes, nor interprets it. A client doing its own line editing — `nxsh`'s REPL — uses this.

**A raw read stops at an interrupt byte.** Bytes typed before `Ctrl-C` are ordinary input and are
delivered; the interrupt itself is never handed over as data.

Switching a terminal from `ReadLine` to `Read` **discards any partial line**, so a client's first
raw keystroke does not arrive with somebody else's prefix.

### `Write` (`0x0B01`)

Request: the bytes. Reply: empty.

Goes to the terminal's backend — the serial console, or an `Output` message to its emulator.

### `SetMode` (`0x0B02`)

Request: one flags byte. Reply: empty. Bit 0 (`TTY_MODE_ECHO`) enables echo of typed characters.

Echo is a **request**, not a parameter of a read: a password prompt turns it off, prompts, and
turns it back on. An absent body is read as echo on.

### `Close` (`0x0B03`)

Request: empty. Reply: empty, and then the terminal is gone.

**The revocation point.** Handles are refcounted and this kernel has no revocation, so a process
that outlived its session while holding a terminal cannot have it taken away. The server
declining to serve the channel is what makes teardown a guarantee rather than a convention.

A terminal also ends when its last holder exits — the server sees `PeerClosed` — so `Close` is
for the case that needs it: a supervisor ending a terminal it holds, as `session-mgr` does with
the one it used for the login prompt.

### `Interrupt` (`0x0B05`)

**Server → client. Unsolicited, `request_id` 0, no reply.** Empty body.

The server saw `Ctrl-C` and is telling whoever holds the terminal. **It is not a signal** — it is
data on a channel the client already holds, and what it means is entirely the client's decision.
A client that never looks for it is unaffected: it queues like any other message.

Two rules make it useful rather than decorative:

- **An outstanding read completes empty.** Otherwise a client at a prompt stays blocked on a read
  whose byte just became an event. A `Read` otherwise never completes with zero bytes, so an
  empty completion is unambiguous.
- **An interrupt with nobody reading is remembered**, and ends the *next* read before it starts —
  once. Without this, `Ctrl-C` between prompts would take effect only when the user pressed
  something else.

**Delivered to every terminal on the same backend, and no others.** Before Part C it went to
every terminal in the system, justified by "a session has one" — which was never true. It is not
a guess about which terminal is foreground (job control is unbuilt): a backend is a *physical*
grouping, and `Ctrl-C` typed in one window has no business interrupting a program in another.

### `AttachBackend` (`0x0B06`)

Request: empty body, **one moved handle** — a channel whose far end the sender keeps. Reply:
empty on success; `InvalidArgument` if no handle was moved, `NotFound` if the terminal is gone.

From then on the terminal's output arrives on that channel as `Output`, and its input is whatever
comes back as `Input`, instead of the serial console.

**This is a pty with the pieces named differently.** Unix puts the line discipline in the kernel
between a master and a slave; here the tty server *is* the discipline, so an emulator holds what
a master would be, and the terminal channel is what it hands to the program it hosts. The usual
sequence is: resolve `/dev/tty`, `AttachBackend`, then give the terminal channel away — the
emulator has no further use for it.

A terminal may be re-pointed; the previous backend is dropped if no terminal is left on it.
Re-pointing **keeps an outstanding read**: it is the same terminal and the same client, and
failing the read would make attaching a backend a visible hiccup for a program that never asked
about backends.

### `Output` (`0x0B07`)

**Server → emulator on a backend channel. Unsolicited, `request_id` 0, no reply.** Body is the
bytes to render, already through the line discipline — the same stream a serial console would
have received, so an emulator is a terminal for an ordinary byte stream rather than a special
one. Writes longer than one message are split across several.

### `Input` (`0x0B08`)

**Emulator → server on a backend channel. Unsolicited, `request_id` 0, no reply.** Body is what
a keyboard would have produced on a serial line. The server runs the discipline over it exactly
as over console input, which is what keeps one implementation of `Ctrl-C`, erase and echo.

## Lifetime

- A **terminal** ends when its holder exits (`PeerClosed`), or on `Close`.
- A **backend** is dropped when the last terminal on it is gone; if it was a channel, the server
  closes its end, which is how an emulator learns its terminal has ended.
- An **emulator** going away ends every terminal on its backend. A terminal whose window has
  closed cannot be interacted with, and leaving it alive would give its programs a `/dev/tty`
  that silently discards — the failure that looks like a hang.

## References

- [`console-and-tty.md`](../architecture/console-and-tty.md) — the design and why the server is
  in userspace
- [`rsproto-wire-format.md`](rsproto-wire-format.md) — framing, request ids, error replies
- [`rsproto-namespace-ops.md`](rsproto-namespace-ops.md) — how a resolve mints a channel
