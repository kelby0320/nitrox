# The clipboard

**Status: built 2026-09-02 (Milestone 12 Part E).** `clipboard-server` runs, `/dev/clipboard` is
bound into the root namespace and into both session columns' namespaces, and `desktop-shell` binds
it into every application namespace it constructs. `nxedit` copies, cuts, pastes and cycles;
`nxterm` selects with the pointer and copies and pastes; `clip` reads and writes the ring from a
pipeline. Text only — `CLIP_KIND_TEXT` is the only kind defined. Focus-gated reads and chunked
transfers are deferred with triggers written down.

The wire contract is [`librsproto::clipboard`](../../userspace/librsproto/src/clipboard.rs); the
ring is [`clipboard_server::Ring`](../../userspace/clipboard-server/src/lib.rs). This document is
the design behind them.

## Why it is a resource server

A clipboard is **shared mutable state between mutually untrusting programs**. "Anything running may
read what you last copied" is ambient authority, and it is the exact mechanism by which a password
manager's clipboard gets scraped on a real desktop.

This system already has the answer, and it needed no new machinery: **you can read the clipboard if
it is in your namespace.** A profile that binds no `/dev/clipboard` has no clipboard at all; one
that binds an endpoint attenuated to `RIGHT_SEND` can copy and not read. Sandboxing is by namespace
construction, not by permission denial — the same rule everything else here follows.

The alternative was the compositor holding a slot, which is what a windowed system reaches for
first. It was rejected because it makes the clipboard a **display** concept: a program with no
window could then neither copy nor paste, and §4 below is about exactly that program.

## Why it stores rather than brokers

Wayland's model is that the **copier keeps the data** and the compositor arranges a transfer when
somebody pastes. Nothing is ever held by a third party, which is a genuinely better privacy story.

It was rejected for one reason: **the clipboard dies with the application you copied from.** That is
the behaviour people install clipboard managers to escape. Here you copy from the editor, close it,
and paste.

The cost is that the server holds what you copied for as long as the machine runs. That cost is
bounded and stated: [`CLIP_RING`] entries of [`MAX_CLIP_BYTES`] each, about 63 KiB, in `.bss`.

## The ring, and the two halves of it

The server keeps the last 16 entries, most recent first, and a `Copy` pushes. What makes this a
**kill ring** rather than a stack of slots is the division:

- **The ring is shared.** Every holder of the endpoint sees the same entries in the same order.
- **The position in it is not.** "Paste the one before that" is a property of the editing somebody
  is doing right now, not of the machine. Two applications cycling at once would fight over one
  cursor, and a cursor one client advanced would move under another — so the server **answers by
  index and holds no per-client state**.

Three rules follow, and they are the design rather than an implementation detail:

1. **A paste always takes the newest entry and never consults a cursor.** That is the ordinary case
   and the one that matters: copy in one application, paste in another.
2. **Cycling is a continuation of a paste.** It is valid only immediately after one, it *replaces
   what was just inserted*, and any other action ends the sequence — typing, a copy, a tab switch.
   That is Emacs's `M-y` rule, and it is what makes a stale position **unreachable** rather than
   merely unlikely: the position exists only inside one uninterrupted gesture, and anything that
   could invalidate it has already ended it.
3. **Where it can still go stale, the server says so.** §4 makes the ring reachable from a pipeline,
   so something not being driven by the person can push mid-cycle. Every entry comes back with the
   ring's **serial**; a cycle carries the serial it last saw; if the ring has moved the server
   refuses. One `u64`, and it turns a silent wrong paste into a visible restart.

**A terminal does not cycle, and that is a decision.** A paste there is bytes already sent down the
pty to a program that has read them; taking them back would mean sending backspaces to something
that may not be a line editor at all. So `Ctrl+Shift+V` in `nxterm` always pastes the newest, and
cycling lives in the editor, where the buffer is the client's own.

## It is reachable as a path

`/dev/clipboard`, bound the way `/dev/tty` and `/dev/draw` are. There is no generic read/write verb
in this system — a resource server speaks its own ops, as the tty does — so shell access is a small
utility either side of the pipe rather than a new file interface:

```
clip                       # the newest entry, as a text stream
clip 2                     # the third-newest
clip --list                # index, kind, length
"hello" | clip --copy      # push
```

**This is the more interesting half of the design.** A clipboard only graphical applications could
reach would be the first resource in this system that a pipeline cannot. Making it a path also makes
the ring *inspectable*: listing what is in it is a command rather than a feature somebody has to
build a window for.

**Both session columns bind it**, which makes it the first endpoint that is not display-specific and
not console-specific. `/dev/draw` goes only to the graphical column because a serial session has no
compositor; the clipboard goes to both because a pipeline runs in either.

## What the server never logs

Counts, serials, lengths and kinds. **Never bytes.** A clipboard holds what a person copied, which
on any real machine eventually includes a password, and the serial console is a log file that a gate
reads and a maintainer pastes into a bug report. `nxedit` and `nxterm` follow the same rule for
their own lines — they report how many bytes moved, never which.

## Selection in the terminal

`libterm`'s grid had no selection at all; M5 deferred it in the same breath as the clipboard. The
question inside it was **what a selection is across a reflow**.

The answer is that a **logical line's text is invariant** across a rewrap — that is exactly what
`Line::wrapped` was added for in M9 Part D — so a position expressed as an offset into a logical
line survives, and re-dividing that offset by the new width is the whole mapping. `Reflow::map_position`
does it, and the cursor's own remap goes through the same function, so the two cannot disagree about
where a character went.

The alternative was to **clear the selection on resize**, which several real terminals do. It was not
taken because the mechanism already existed: the scrolled-back viewport's anchor had to survive a
rewrap for the same reason a milestone earlier.

Two consequences worth stating:

- **A selection is a pair of absolute `(line, column)` positions**, the same coordinate the viewport
  is anchored to. Screen-relative ones would slide up a row per line printed, so a selection made in
  the history would drift under the person who made it while a program was still writing.
- **A soft wrap does not become a newline** when the text is copied. A line that ran past the right
  margin is one line of text the terminal broke; pasting it with a break in the middle would insert
  something the program never wrote.

A selected cell is drawn **inverted**, exactly as the cursor is — reusing the mechanism rather than
adding a colour, because a highlight with its own colour is a third thing to keep in step with a
theme. The cursor wins where they overlap: inverting twice is not inverting, so a cursor inside a
selection would otherwise appear as a hole in it.

## Bindings

`Ctrl+C` / `Ctrl+X` / `Ctrl+V` in the editor — what fingers already know. **The terminal has to
differ**: `Ctrl+C` means interrupt there and always will, which is the one binding a terminal cannot
give away, so it is `Ctrl+Shift+C` / `Ctrl+Shift+V`. The kill ring's third binding is
`Ctrl+Shift+V` in the editor, cycling on repeat.

A chord that acts on the **buffer** stays a text field's while a field is open — so `Ctrl+C` during a
find goes to the find field. That is the rule the two applications settled in M12 Part D, applied
here without a second thought about it.

## What is deferred, and what fires it

- **Focus-gated reads** — `TODO(clipboard-focus)`. Trigger: an application inside a session that the
  person does not trust.
- **Chunked transfers** — `TODO(clipboard-chunking)`. Trigger: the first thing somebody cannot copy.
- **A file-path kind for the browser's cut and paste** — `TODO(file-clipboard)`, whose trigger *has*
  fired: the ring exists. What it still needs is a second `CLIP_KIND_*`, the menu items, and a
  decision about pasting a path into a text field.

All three are in [`deferred-decisions.md`](../rationale/deferred-decisions.md).

## Gates

- **`cargo xtask test-interactive`** step 19c: a pipeline pushes, `clip` reads it back, and index 1
  is still the entry before it — the property a single slot would fail while passing everything
  above it. From the **serial** column, which is the half a windowed test cannot see.
- **`cargo xtask check-login`** step 9d: the editor copies a selection, a terminal pastes it into a
  command, and the **serial** session lists the file that command made. One gesture crossing two
  applications and a server, asserted through the filesystem rather than through a log line —
  because `nxterm` in a release image does not report its grid, and because a paste that delivered
  the wrong bytes makes a differently-named file rather than a matching count.
- Host tests: the ring in `clipboard-server`, the codec in `librsproto::clipboard`, selection and
  reflow in `libterm::grid`, the paste primitive in `libui::widget`, and the chords and the cycling
  discipline in `nxedit` and `nxterm`.

[`CLIP_RING`]: ../../userspace/librsproto/src/clipboard.rs
[`MAX_CLIP_BYTES`]: ../../userspace/librsproto/src/clipboard.rs
