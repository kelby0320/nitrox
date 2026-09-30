# Nitrox: The Graphical Prompt — Design Notes

## Status

**Designed, not built** (administration Part F.4, 2026-09-29). Nothing described here exists yet.
It is written down now so that its guarantees are fixed before the day something needs it. The
plan had sketched it in a paragraph ([`administration.md`](../planning/administration.md) § *The
prompt, and a terminal to prompt on*). This document **graduates to `architecture/` when it is
built**.

## What it is for

The view broker asks for a password when a rule says `auth = "password"`. **This is how it asks on
the display**, in a way nothing in the session can imitate or read. It has two jobs:

- **The desktop's own need.** A desktop action whose rule wants a password has nobody to ask today.
  The first is its trigger (*Guarantee 4*).
- **The terminal prompt's limit.** `with` reads a password on its terminal, and any program holding
  a read on the same `tty-server` backend can receive it. The PR #329 review recorded the remedy as
  "a prompt nothing in the session can read". This is that prompt.

It is **not** a login greeter: the greeter runs before any application does, and needs none of this.
It is not a dialog service for applications, and it stores nothing.

## Where things stand today

- **The terminal prompt** (administration Part A):
  - `with` turns echo off and reads the password on its terminal with `coreutils::prompt`, then
    sends it to the broker as `Password`.
  - The broker paces each check: the session's delay after a failure, and three failures per
    request (`view_broker::pacing::Held`).
  - Input goes to the *oldest* terminal on a backend with a read pending. So a stage's child that
    outlived its pipeline, or an earlier stage of the same one, receives the line typed at the
    prompt. Pacing does not help, since this is not a guess.
- **The desktop refuses** (Part F.3). A Restart or a Shut down under a `power` rule that wants a
  password is refused before any window closes. The dialog says only a terminal can ask yet, and the
  console line names this document.
- **The compositor would let an application imitate a prompt today.** This is the finding this
  document most needs to record. The plan's premise — "the desktop dimmed behind it, which no
  client surface can do" — is **not true yet**. Here is why:
  - **A `popup` is never held for the manager.** It is placed where its creator asks, relative to
    its parent, negative offsets included, at any size (`WindowStack::create`).
  - **A new window goes on top of the stack**, and **the topmost window that takes focus has the
    keyboard** (`focus_candidate`).
  - **A probe on 2026-09-29 showed it.** An application's popup the size of the screen, offset back
    to its window's origin, landed at (0,0) above the top bar and took the keyboard.
  - **Nothing restricts the roles an application's connection may create**, so it may ask for a
    `panel` too. The manager decides where a panel goes, but nothing refuses one.
  - Guarantee 2 names the change. It is `TODO(app-covers-panels)` until then.

## What it guarantees

### 1. Only the broker can open it

- **The compositor serves the prompt at `/dev/draw` with the suffix `prompt`, and only on a resolve
  with no base** — that is, one that came through the root namespace's `/dev/draw` binding.
- **A session binds `/dev/draw` at a base.** `/dev/views`, `/dev/devices` and an application's
  `/dev/draw/new` are already bound that way. The shell's resolves then arrive as `<base>/…` and can
  never name `prompt`. An application has `/dev/draw/new` alone.
- **Who can open one, then:** no application, and no session process — the shell included. Every
  holder of the root namespace still can, and that is every system service. It is the trusted set
  every admin endpoint sits in today (`TODO(svc-auth-ungated)`). The fix recorded there, constructed
  namespaces for services, is what turns "only the broker" from a convention into a structure.
- **The broker does not draw it** (*Rejected* says why). It spawns **`view-prompt`** for each
  prompt, in a namespace it builds itself. That namespace holds:
  - the compositor's endpoint, bound at the base `/prompt`, so it reaches `prompt` and nothing else;
  - `/system/fonts`, read-only;
  - nothing more.
- **Why it matters:** whatever is typed into a prompt goes to whoever opened it. A process that
  could open a real one could collect the password the person typed for it. That is true of the
  shell, which is a manager but is not in the password's path today.

### 2. Above everything, the desktop dimmed behind it — the bars included

- **It is a layer the compositor keeps above the whole stack, not a window.**
  - The manager is not told of it, and cannot restack, close or capture it.
  - Only the process that opened it can close it, or the compositor, when that process exits.
- **The compositor dims everything beneath it**: the bars, an open menu, the overview. It blends
  already, for shadows and translucent surfaces, so the dim is one more pass.
- **What makes that unforgeable is a change the compositor needs first: no application surface may
  cover a panel.**
  - An application's popup is kept inside the work area, which is the screen less the panels'
    reservations. A menu near an edge moves inward, as a positioner does elsewhere.
  - An application's connection may not create a panel.
  - **The manager's own surfaces are exempt.** Its menus hang from the bars, and its overview covers
    the screen.
  - That needs the compositor to know which connection is the manager's, and a resolve carries no
    identity (`WindowInfo`'s note says so). So **the manager opens its surface connection through
    `manage`**, and the compositor marks that connection as the manager's.
- **Then the bars are pixels only the compositor and the manager ever draw.**
  - The prompt draws a strip across the top bar's place, saying the system is asking. No
    application can draw one.
  - That is the thing a person can be taught: **a password prompt that leaves the top bar undimmed
    is not the system's.**
- **The manager can imitate it**, since it may cover the bars. It is already inside the trusted set
  for the display: it captures, holds the chords and places every window. GNOME Shell holds the same
  position for its own authentication dialog. Guarantee 3 still holds against the shell: a
  look-alike it drew receives keys only as any of the shell's windows does.

### 3. It holds the keyboard while it is up

- **Every key goes to the prompt.** None reaches the focused window, and no global hotkey fires: the
  manager's chords are suspended. A pointer press outside the prompt reaches nobody.
- **So a password typed into it travels kernel → `input-server` → compositor → `view-prompt` →
  broker → `auth-service`.** The input path already sees every key on the machine, the greeter's
  password included. Neither the terminal backend nor any session process is on that path.
- **This is the remedy for the same-backend limit.**
  - **In a session on the display**, `with` asks the broker to prompt on the display rather than
    reading its terminal. A program with a read pending on that terminal's backend receives
    nothing.
  - `pkexec` in a terminal on a GNOME desktop behaves the same way: it asks the graphical agent.
  - **In a session not on the display** — the serial console — `with` prompts on its terminal as
    today. There the limit stays, as it is `sudo`'s.

### 4. Its trigger, unchanged

**The first desktop action the policy will not allow without a password**: unmounting a USB stick
from Files (Phase 6), or a Settings application.

- A `power` rule asking for a password would fire it early. Until then Part F.3's refusal says so.
- The terminal remedy rides along with the build. It is not a trigger of its own: Part A accepted
  the limit and recorded it.

## What it shows

- **What will run**: the view, the program and its arguments, as the broker will run them.
  - The arguments are the requester's text, so they are escaped as the audit escapes them.
  - They are cut short at the prompt's width, with the whole line in the audit.
- **Whose password**: the session's person. The broker checks the principal's own password, as it
  does now.
- **Not who asked.** A request carries no process identity, and a capability system authorizes a
  request for what it does. That is what the prompt names.
- A masked field, masked as the greeter's is (`libui`'s `MASK_CHAR`), and **Allow** and **Cancel**.
  After a wrong password it says "Wrong password (1 of 3)".
- The strip over the top bar (Guarantee 2).

## The exchange

1. **A request gets `NeedPassword`, as now**, and the answer says whether its session is on the
   display.
2. **On the display, the client sends `Prompt`** rather than reading a password, so it never holds
   one. This is a new op.
3. **The broker spawns `view-prompt`** with the request's description and a channel of its own, and
   holds the request.
   - **One prompt at a time.** A second request waits its turn, in the order it arrived.
4. **The person types, and Enter sends the password to the broker on the helper's channel.**
   - The helper scrubs its copy.
   - The broker checks it through `Held`, as it checks `Password`: the session's delay after a
     failure, three failures per request.
5. **Then one of three things:**
   - **Right**: the broker answers the client `Started`, and the helper closes.
   - **Wrong**: the helper shows the count and asks again. The third failure ends the request
     `Denied`.
   - **Cancel, Escape, the helper exiting, or the session ending**: `Denied`, "cancelled".
6. **The audit records it** as it records a terminal's, marked "on the display".

**Which session is on the display: the one `desktop-session-mgr` opened.** `OpenSession` gains a
flag that only the supervisor channel can set, the same channel that names the principal. A request
for `Prompt` from a session not on the display is refused. Otherwise a serial session could put a
prompt in front of whoever is at the screen.

## What it does not guarantee

- **Anything, against a compromised trusted path**: the kernel, `input-server`, the compositor, the
  broker, `view-prompt` and `auth-service`. The manager belongs on that list for imitating a prompt,
  though not for reading one.
- **Protection from an application's look-alike inside its own window.** It cannot dim the bars or
  take a key from another window, so the person has to notice.
  - The stronger answer is a **secure attention key**: a chord the compositor never delivers to
    anyone, answered by showing the real prompt, or by saying there is none. Windows can require
    one before a credential prompt.
  - It is not in the first build. It is the next step if imitation proves to matter.
- **Protection from someone looking over a shoulder.** The mask shows the password's length, as the
  greeter's does.
- **Anything for the terminal prompt in a session not on the display.**

## Rejected

- **The broker draws it.** The plan calls the broker "the most trusted process in userspace after
  `init`" and says it "should be small". A toolkit and a TrueType rasterizer would make it the
  largest. A helper spawned for each prompt holds the interface and nothing else. The cost is that
  the password crosses one more channel.
- **The compositor draws it.** The compositor renders no text and no widgets — it links `libdraw`
  and not `libui`, and loads no font — and "the one process that must never wedge"
  ([`display-substrate.md`](../architecture/display-substrate.md) §2) should not grow a toolkit.
- **A dialog in the session, drawn by the shell** — the polkit-agent shape. Anything that can open a
  window can draw the same dialog, and the password would pass through a session process.
- **A second, "secure" desktop**, as Windows does. The compositor already owns every pixel and every
  key, so a layer above the stack gives the same guarantee without a second desktop.
- **A prompt endpoint under the session's `/dev/draw`.** The shell would then be in the password's
  path.
- **The requester collects the password from the prompt and relays it**, as `with` does on a
  terminal. The requester would hold the password.

## Building it, when triggered

In dependency order:

1. **The compositor keeps applications off the panels.**
   - The manager's connection is opened through `manage`.
   - An application's popup stays inside the work area.
   - An application's connection may not create a panel.
   - This closes `TODO(app-covers-panels)`, and it can land on its own.
2. **The compositor's prompt layer.**
   - Sessions bind `/dev/draw` at a base, and the compositor answers `prompt` only on a resolve with
     no base.
   - The layer sits above everything, with the dim.
   - The keyboard and the pointer are held, and the manager's chords are suspended.
   - The manager is never told of it, and cannot capture or close it.
3. **The broker.**
   - `OpenSession`'s display flag, and `NeedPassword` saying whether the session is on the display.
   - `Prompt`, and spawning `view-prompt`.
   - One prompt at a time, and the audit.
4. **`view-prompt`**: `libui`, the masked field and the strip.
5. **The callers.**
   - The triggering action.
   - `with`, in a session on the display.
   - Part F.3's refusal becomes a prompt.
6. **Gates.**
   - **Host tests:**
     - an application's popup stays off the panels, tested at the neighbour of every edge;
     - the prompt stacks above everything;
     - keys and chords reach only the prompt;
     - capture refuses the prompt;
     - a resolve of `prompt` through a based binding is refused.
   - **A boot on the release image.** `with admin` runs in a desktop terminal while a reader holds a
     read on the terminal's backend. The prompt appears, the password typed reaches the broker, and
     **the reader receives nothing**. Today's terminal prompt fails that assertion, which is what
     makes it worth making. The boot also checks:
     - a wrong password is counted;
     - Cancel works;
     - no password appears in the transcript;
     - a screendump shows the top bar dimmed under the prompt.
   - **A `--selftest` boot**, where the test client is an application that can try: its popup the
     size of the screen leaves the top bar visible, in a screendump.
7. **This document moves to `architecture/`.**

## Open questions

- **The strip's words**, and whether it names the view.
- **A prompt nobody answers** could end on its own, or hold the display until it is answered or the
  session ends. This leans towards holding: it holds the display and nothing else, and the serial
  console is untouched.
- **Whether `account`'s check of the current password** (Part D.4's `ChangePassword`) uses `Prompt`
  in a session on the display. It has the same limit, so probably yes.
- **The keymap.** The helper maps keycodes as every client does. A person with another keymap needs
  the session's, and there is no per-session keymap yet.
