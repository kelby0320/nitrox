# Nitrox: Desktop operations (`0x0Cxx`)

The graphical session's desktops, served by
[`desktop-shell`](../../userspace/desktop-shell) at `/dev/desktop`. Three ops: describe them,
switch to one, name one.

## Where it sits

**The desktop shell serves this, and it is the one process that both serves a resource and
constructs namespaces.** [`graphical-session.md`](../architecture/graphical-session.md) §3 is
where that is reconciled: the shell does not *register* itself — nothing binds this endpoint into
a namespace a supervisor owns — it binds it into the namespaces it **constructs** for the
applications it launches, which is the constructor role it already holds `BIND_NAMESPACE` for.

**A session channel, not a path per object.**
[`ui-composition-model.md`](../architecture/ui-composition-model.md) §2a sketches `new`,
`current`, `N/info` and `N/windows/` as separate paths. What exists is the bare resolve, answered
with a session the way `/dev/draw/new` and `/dev/tty` are — the operations that matter here are
*mutations*, and a namespace resolve is a lookup rather than a call. The per-object paths would
duplicate what one `List` returns, for no consumer.

Resolving `/dev/desktop` with a **non-empty suffix** is `NotFound`: there is no second level.

## Positions, not ids

Every op that names a desktop names a **position**, one-based, as the desktop indicator counts
them. Ids are stable and never reused, so after a few desktops have come and gone they stop
matching what anyone sees — and `Super+N` addresses positions for the same reason. `List` reports
both, so a caller that wants to hold on to a desktop across a renumbering can.

## `List` (`0x0C00`)

Request: empty. Reply body:

| Offset | Size | Type | Field |
|---|---|---|---|
| 0 | 4 | `u32` | `count` — entries that follow |
| 4 | 4 | `u32` | `current` — the current desktop's one-based position, or `0` |
| 8 | 4 | `u32` | `truncated` — non-zero if the server has more desktops than it described |
| 12 | … | | `count` entries |

Each entry: `id` (u32), `len` (u32), then `len` bytes of UTF-8. `len` is at most **32**
(`MAX_DESKTOP_NAME`); an empty name means the desktop has none — **which is also what says it
will not persist**.

**Truncation is reported rather than implied.** The shell's desktop list is unbounded — desktops
are created on demand — and at most **16** (`MAX_LISTED`) are described. A reply that could not
say "there are more" would be lying by omission, and a caller would show a complete-looking list.

**A short buffer is refused, not truncated.** A half-written list parses as a *shorter* list and
a caller cannot tell the difference.

## `Switch` (`0x0C01`)

Request, 4 bytes: `index` (u32), one-based. Empty reply.

`NotFound` if there is no desktop at that position. `InvalidArgument` if the body is short.
`Unsupported` if the shell holds no manager channel — it cannot tell the compositor to change
what is composited, so it declines rather than moving its own model out of step.

## `Name` (`0x0C02`)

Request: `index` (u32), one-based, then UTF-8 bytes — at most `MAX_DESKTOP_NAME`. Empty reply.

**Naming is what makes a desktop persist.** An unnamed desktop is removed when its last window
leaves; a named one is kept, and the list always ends with one empty unnamed desktop to create
into. So this op changes the desktop *lifecycle*, not a label —
[`ui-composition-model.md`](../architecture/ui-composition-model.md) §6's "name it if it turns
out to matter" is the rule itself rather than a separate mechanism.

`NotFound` for a position that does not exist; `InvalidArgument` for a short body, a name over
the cap, or bytes that are not UTF-8.

## What this does not do

**No `new`, and no delete.** Desktops are created and destroyed by the lifecycle rule rather than
by request: something lands on the trailing empty desktop and a new empty one is appended; a
desktop that is unnamed and empty goes. A caller that wants a fresh desktop moves a window to the
last one.

**No events.** A caller learns the state by asking. The shell's own bar and overview are driven
by its internal model rather than by this protocol, so there is no second consumer to notify and
nothing that would go stale unnoticed.

## See also

- [Wire format](rsproto-wire-format.md) — envelope, categories, reply rules
- [`ui-composition-model.md`](../architecture/ui-composition-model.md) — what a desktop *is*, and §2a's namespace sketch
- [`desktop-shell.md`](../architecture/desktop-shell.md) — the bar, the indicator and the overview that drive the same model
