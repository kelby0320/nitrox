# Nitrox: The Widget Toolkit — Design Notes (v1)

## Status

**The rename is done; the toolkit is not.** As of M4 Part A the Surface-protocol client —
window lifecycle, shared buffers, commit/release, and the input queue — is `libsurface`,
and the name `libui` is reserved for the toolkit described here. There are no widgets, no
layout, and no notion of focus inside a window. This document specifies the toolkit that
[`display-arm-plan.md`](../planning/display-arm-plan.md) Milestone 4 builds, and the three
things below it that Milestone 4 forces into existence. Settled with the maintainer
2026-08-10.

**Companion documents.** [`display-substrate.md`](display-substrate.md) states the
principles this rests on — client-side rendering, damage-rectangle commits, the compositor
owning window focus. [`ui-composition-model.md`](ui-composition-model.md) owns what a window
*is*; this document owns what is inside one.
[`input-subsystem.md`](input-subsystem.md) owns everything up to a `KeyEvent` arriving at a
window; routing it to a widget starts here.

---

## 1. What decides how big this is

The plan's rule is that **the terminal decides how much toolkit exists**, and that minimal
is a requirement rather than a compromise. That rule has to survive contact with a document
that could happily specify a hundred widgets, so it is worth stating what it actually
forbids: any widget, property, or layout feature that Milestone 5's terminal does not use is
not in Milestone 4. Not "deferred pending review" — absent, with a note here saying what
would bring it back.

This is not asceticism. A toolkit's API is the hardest thing in this tree to change later,
because every application encodes it. The way to keep that surface honest is to let a real
application pull each piece into existence.

**One consequence, found while writing this.** Plan Part C lists *a text area*. Milestone 5
Part B says the terminal's grid is **a custom-drawn widget of its own**, because "a
terminal's selection, wrapping and scrollback semantics are not a text editor's, and bending
a generic text area to serve both would distort the whole text stack". Both cannot be right:
if the grid is custom-drawn, nothing in Milestone 5 uses a text area, and by the rule above
it should not exist yet. §8 resolves this.

---

## 2. The model: a retained tree with a declarative face

Two axes get conflated in toolkit discussions, and separating them is most of the decision.

**Does a widget object exist between frames?** *Immediate mode* says no — `ui.button("OK")`
is called inside a draw loop every frame and no Button is stored anywhere. *Retained* says
yes: a persistent tree of nodes. **Nitrox is retained.** Immediate mode redraws everything
every frame, which throws away the damage-rectangle model the whole display arm is built on,
and it needs a continuous loop where this system has no vsync to pace against.

**How does the application talk to that tree?** This is the axis that matters, and both
answers are retained:

- **Mutation** (Windows Forms, GTK, Qt). The application holds widget handles and changes
  them: `label.set_text("Cancel")`. Widgets call back into the application. State lives in
  two places — the model and the widgets — and keeping them agreeing is the application's
  job.
- **Declarative** (Elm, and Iced after it). The application holds *state*. It writes
  `view(&state) -> Element`, a description of what the UI should look like right now. The
  runtime diffs that description against the tree it already has and updates only what
  differs. Events come back as `Msg` values folded into state by `update`.

**Nitrox takes the declarative shape.** Two reasons, both specific to this project rather
than to fashion:

**`view` is a pure function, so it host-tests.** Every subsystem in this tree has a pure
library half tested on the host in milliseconds and a thin syscall shell around it —
`libinput`'s interpreter, the compositor's `WindowStack`, `InputRouter` and `Outbox`. A
mutation toolkit's correctness lives in *ordering*: did every path that changed the model
also update the widget, and in the right order. That is exactly what host tests reach badly.
`view(state) -> Element` is a value in, a value out.

**Damage becomes derived rather than remembered.** Under mutation, every widget must
remember to invalidate itself when it changes; miss one and a stale rectangle appears only
under load. Under a diff, the changed nodes' bounds *are* the damage. This project has spent
two milestones finding places where the code did not do what its comment claimed; an
invalidation discipline is that failure mode by construction, and a derived one is not.

### 2.1 What this costs, stated plainly

- **Allocation per update.** Building an `Element` tree allocates on a hand-rolled heap.
  Mitigated by rebuilding on *events* rather than at frame rate — there is no animation loop
  — and by trees that are small. It is still real, and §6.3 bounds it.
- **Identity is now a design problem.** A diff must decide whether the third child this time
  is the same widget as the third child last time. §4 settles it, and gets it wrong loudly
  rather than quietly.
- **More machinery than mutation**, for one application at first.
- **It is a shape, not a port.** No ecosystem crate is involved; the kernel and userspace
  take no external dependencies.

### 2.2 The application's loop

```rust
fn update(state: &mut App, msg: Msg);
fn view(state: &App) -> Element<Msg>;
```

The runtime owns everything else: wait for events, translate them to `Msg`, call `update`,
call `view`, diff, paint the damage, commit. An application never touches a buffer, a
damage rectangle, or the event queue.

---

## 3. `Element`, and what a widget is

`Element<Msg>` is a **description**, not a widget: a node kind, an optional key, layout
properties, and children. It is built fresh by `view` and dropped after the diff.

A **widget** is the retained counterpart: the node the runtime keeps between frames, holding
whatever state the description does not carry — a scrollbar's drag origin, a menu's open
item, a custom widget's opaque payload. The diff's job is to pair each `Element` with the
widget it corresponds to, update it, and paint what changed.

The split matters because it says where state may live. **Anything the application owns
belongs in `App` and arrives through `view`.** Widget-retained state is strictly
*interaction* state — mid-drag, mid-keypress, mid-animation — which the application has no
opinion about and would be tedious to thread through `Msg`.

---

## 4. Identity: how the diff knows which widget is which

The diff walks the old widget tree and the new `Element` tree together. Two nodes pair if
they are at the same position under the same parent **and have the same kind**. A kind
change destroys and rebuilds, which is correct: a button is not a scrollbar with different
properties.

Positional identity is wrong for one case, and it is a common one: a **list whose items are
reordered, inserted into, or removed from**. Positionally, deleting the first of five rows
makes every later row pair with its neighbour, so row 2's widget state — its scroll offset,
its half-finished drag — silently belongs to row 3.

So: `Element` carries an optional **key**. Within a parent, keyed children pair by key
rather than by position. Unkeyed children pair positionally, which is right for the fixed
structural nesting that makes up most of a UI.

**The failure is made loud rather than avoided**, and the rule has to be about *change over
time*, not only about one frame's internal consistency. Two things are errors the runtime
detects and reports:

- A parent whose children are keyed **inconsistently within a frame** — some keyed, some not.
- A parent whose **keyed-ness changes between frames** — keyed last time, unkeyed this time,
  or the reverse.

The second matters because each frame can be internally consistent while the pair is not, and
`view` being a pure function of state makes it easy to reach:
`if state.compact { column(rows) } else { column(keyed_rows) }`. Neither the mixed-keying
check nor a uniqueness assert fires, the diff silently falls back to positional pairing, and
the result is exactly the "wrong row remembered my selection" bug this section exists to
prevent. Debug builds additionally assert that keys within a parent are unique.

All of it is cheap, and the alternative is a class of bug nobody diagnoses from the symptom.

---

## 5. Layout

Two passes, the model Flutter uses and the one that survives contact with a scrollbar:

1. **Measure.** The parent passes *constraints* (a min/max width and height) down; each
   child returns the size it wants within them.
2. **Arrange.** The parent assigns each child a rectangle in its own coordinates.

Containers, and no more than these:

| Container | What it does | What in Milestone 5 needs it |
|---|---|---|
| `column` / `row` | Stack children on one axis; each child is fixed-size or takes a share of the remainder (`flex`) | The terminal's vertical split: menu bar, grid, status |
| `dock` | Pin children to an edge, the last child fills the rest | The scrollbar on the right of the grid |
| `stack` | Overlay children in paint order, positioned within the parent | Menu popups over the grid |

Plus `padding` and fixed `sized` wrappers. That is the whole layout vocabulary.

**Not a CSS box model.** Margins-collapsing, floats, and inline flow are an enormous surface
for a system whose first application is a rectangle with a bar on top. **Not absolute
positioning either**: a terminal is resized constantly and every widget would need to
recompute, which is a layout engine written in application code.

**Layout runs on the diff's result**, not on every event: a node is re-measured only when its
own properties changed, or an ancestor's constraints did. A pointer moving across a static
window costs no layout at all.

---

## 6. Painting, and how invalidation becomes a damage rectangle

### 6.1 The wire decides the shape

`Surface::Commit` carries **exactly one** damage rectangle
([`rsproto-surface-ops.md`](../spec/rsproto-surface-ops.md)). So the toolkit unions dirty
regions into one rectangle rather than tracking a list. That is a decision already made by
the protocol, and this document is recording it rather than re-opening it.

The union is coarse: two small changes at opposite corners damage the whole window. That is
acceptable while a window is one buffer being blitted; if it ever stops being acceptable,
the fix is a multi-rectangle `Commit`, which is a protocol change and belongs there rather
than here.

### 6.2 Damage is per buffer, not per frame

This is the subtlety that must be designed in rather than discovered, and the first thing to
say is that **"per buffer" and "per frame" are not alternatives** — they answer different
questions, and a first implementation typically notices only one of them:

| Question | Answer |
|---|---|
| What must the **client redraw** into the buffer it just acquired? | Damage since *that buffer* was last drawn — **per buffer** |
| What must the **compositor copy** to the screen? | Damage since the *last commit*, because the screen shows the last committed buffer — **per frame** |

A window has **two or more buffers**. The client draws into a free one while the compositor
reads another. Suppose damage in frame *n* is rectangle A and in frame *n+1* is rectangle B.
Buffer 0 is drawn in frame *n* and released during *n+2*; when it comes back it is missing
**both** A and B — its pixels are from frame *n−1*. Repainting only the current frame's delta
leaves the rest stale.

So each buffer carries **its own accumulated damage** since it was last drawn. Every commit
adds its damage rectangle to *every other* buffer's accumulation. On acquiring a buffer, the
region to repaint is that buffer's accumulation, and drawing it resets that accumulation to
empty.

**The `Commit` damage sent to the compositor is that same accumulation.** It is a *superset*
of what actually changed on screen, so the compositor copies slightly more than it must —
which is safe, where copying less is not. Splitting the two quantities to send a minimal
rectangle is a later optimisation and needs a reason; at one blit per frame there is none.

The bookkeeping is one rectangle per buffer: union on commit, reset on acquire. That is
small enough to be worth stating why the obvious cheaper design was not taken. **Redrawing
the whole buffer every time** removes per-buffer state entirely and is always correct — but
it permanently constrains every widget's paint to be cheap enough to run on every event, and
for the terminal that is re-rendering every glyph per keystroke. The constraint outlives the
ten lines it saves.

Getting this wrong produces stale rectangles that appear only when the compositor is slow
enough to hold a buffer for more than one frame — i.e. under load, and not in a test. It is
called out here because it is invisible in the simple case and the simple case is what a
first implementation exercises.

### 6.3 What triggers a repaint

Only three things:

- A `Msg` was folded into state, so `view` ran and the diff found differences.
- A widget's own interaction state changed (a button became pressed).
- The window was resized or first mapped — a full-window repaint.

There is no clock and no idle redraw. A window nobody is touching costs nothing, which is
also what makes the per-update allocation in §2.1 acceptable: it happens per *event*, not
sixty times a second.

---

## 7. Event routing

### 7.1 Pointer

Hit-test the arranged tree **topmost-first** — the reverse of paint order, so an overlay
menu takes the click that visually landed on it.

**A press captures.** Every pointer event up to the release of the last button goes to the
widget the press landed on, even after the cursor leaves it. This is the same rule the
compositor applies between windows, one layer down, and it exists for the same reason: a
drag that ends outside a scrollbar must still deliver the release, or the scrollbar believes
it is still being dragged forever. Coordinates are widget-local and **signed**, because
mid-capture they are routinely negative.

Enter and leave are synthesised at widget boundaries the same way the compositor synthesises
them at window boundaries, and suppressed during a capture for the same reason.

### 7.2 Keyboard, and the second focus

**There are two focus concepts and they must never share a field.** The compositor decides
which *window* has focus. The toolkit decides which *widget* within a window has it.
Conflating them is the classic source of typing arriving in the wrong field, and it is why
this milestone needs the compositor to start telling clients about the first one (§9.1).

- **Widget focus is a path** to a node in the tree, held by the runtime.
- **Tab traverses** it in tree order among widgets that accept focus.
- A key goes to the focused widget; if unhandled it **bubbles to ancestors**, then to the
  application. That is how a menu accelerator works without every widget knowing about
  menus.
- A caret blinks, and a focus ring is drawn, only when the widget has focus **and** the
  window does. Two conditions, from two sources, which is exactly why they are two fields.

Losing window focus does **not** clear widget focus. Returning to a window must put the
caret back where it was; discarding it would make every window switch lose the user's place.

---

## 8. The widget set

Bounded by Milestone 5, as §1 requires:

| Widget | Why it exists |
|---|---|
| `text` | Labels in menus and the status line |
| `button` | Dialogs, and the menu bar's items |
| `menu` | The terminal's menu bar and its popups |
| `scrollbar` | The terminal's scrollback |
| `custom` | The escape hatch: a node with an opaque payload, a paint callback, and raw event delivery |

**The text area is not in this set**, resolving the contradiction §1 found. Milestone 5's
terminal grid is a `custom` widget by the plan's own decision, so nothing in Milestone 5
would use a text area, and building one now would be a guess at an editor's requirements
made a milestone before any editor exists. It returns when something needs it — plausibly
the file browser or a "find" box — and its absence is exactly the rule working.

**A knock-on worth recording**: `deferred-decisions.md` gives key repeat the trigger "the
first text field — M4's toolkit". With no text field in M4, that wording would say the
trigger has not fired. It has, for a different reason: **holding a key in the terminal**
must repeat, and the grid is a `custom` widget receiving raw keys. The trigger was right
about the milestone and wrong about the widget.

The `custom` widget deserves emphasis rather than apology. A toolkit whose escape hatch is
an afterthought forces its flagship application to fight it; here the flagship *is* an
escape-hatch client, and that is the design working as intended — the toolkit supplies
chrome, layout, focus and input plumbing, and gets out of the way where an application
knows better.

---

## 9. Three things below the toolkit that Milestone 4 forces

They are described here because the toolkit cannot be designed without knowing their shape.

**Where each is recorded**, because they are not all the same kind of thing and a reader
closing the loop should not have to guess. Key repeat (§9.2) is a filed deferral in
[`deferred-decisions.md`](../rationale/deferred-decisions.md). The focus record (§9.1, **built
in M4 Part B**) and the cursor (§9.3, still a gap) are **recorded in the spec** —
`rsproto-surface-ops.md` is where each is either specified or named as missing — not deferral
entries. All three are scheduled by
[`display-arm-plan.md`](../planning/display-arm-plan.md) Milestone 4.

### 9.1 The compositor must tell a client about focus

**Built in M4 Part B; `rsproto-surface-ops.md` now specifies it (`FocusEvent`, op `0x0907`),
and that spec is the current contract.** The rest of this section is the design as it was
argued, kept because §7.2 reads against it — not as a description of what is missing.

A toolkit needs it: §7.2's caret and focus ring depend on it, and a window that keeps blinking
a caret while another window has the keyboard is straightforwardly wrong.

A new Surface record — one op, a boolean and padding — sent to a window when it gains or
loses focus. The compositor already knows: `focus_candidate` changes, and it can compare
against what it last announced. Delivery goes through the outbox like everything else, so
it cannot be displaced by input.

The record is small; the care needed is in **when** it is sent. Focus changes on window
creation, destruction, and raise, and each of those already has a path in the compositor.
**Creation is the one that bit** — it is the only op that replies with a window id, so a
compositor that announced from the "applied with no reply" path skipped it silently (PR #184
review). The implementation announces after every request instead, and compares.

### 9.2 Key repeat

Held keys do not repeat. The record format already reserves `value == 2` for it
([`input-subsystem.md`](input-subsystem.md) §3), so no wire change is needed.

**The compositor generates repeats**, not each client, because it knows which window has
focus and so can stop a repeat when focus moves with no client involvement. The alternative
is Wayland's: send clients a repeat *rate* and let each implement its own timer. That is
better when clients differ in what they want repeated, which is a distinction nothing here
makes yet, and it costs every client a timer and a state machine.

**What this costs, stated accurately.** The outbox retry gave the serve loop a bounded wait,
but it is armed *only while something is parked* — and a held key with an empty outbox is
precisely the not-parked case. The loop also documents an invariant that repeat must break:
"an idle compositor still sleeps indefinitely". So this is not "the timer is already there".
It is a second deadline source `min`-ed with the retry one, per-focus-window held-key state,
and re-deriving the deadline when focus moves. Small, but machinery — and saying otherwise
would be the same class of error as the "eager mapping" cost this milestone already had to
correct.

Delay and rate are policy with no configuration surface yet: fixed initial delay, fixed
repeat interval, both named constants with a note that a settings service owns them
eventually.

### 9.3 A cursor on screen

Nothing draws the pointer. The compositor knows where it is; the screen does not show it.
Tolerable for a gate that injects clicks at known coordinates, not for a person opening a
menu.

The compositor composites a fixed arrow after the window stack, and damages the union of the
old and new cursor rectangles on every move. Per-client cursor shapes — a text I-beam over
the grid, a resize arrow on an edge — are a protocol addition and are **not** in this
milestone; a single arrow is what the terminal needs to be usable.

Worth noting: this makes pointer motion damage the screen on every move, where today it
damages nothing. The compositor's existing damage path handles it, but it is the first thing
in the system that repaints because a person moved a mouse, and it is where the compositing
loop's cost becomes visible.

---

## 10. Crate boundary, and a rename

`libui` *was* window lifecycle, buffers, commit/release, and the input queue: a **client of
the Surface protocol**, not a UI toolkit. The name was aspirational and the code went
elsewhere. Renamed in M4 Part A.

So:

```
libui        ← the toolkit: widgets, layout, focus, the diff   (new)
  ↓
libsurface   ← window, buffers, events                          (today's libui, renamed)
  ↓
libdraw      ← pixels
```

The toolkit re-exports what an application needs, so an application links and imports one
crate.

**Why rename rather than invent a second name.** `libui` is what an application wants to
write `use libui::` for, and leaving it on the transport layer means every application
imports the aspirational name to get the thing it does not want. The rename is mechanical,
touches roughly a dozen imports, and is cheapest now — before a second client exists. It is
the same argument this project applies to filenames carrying version numbers: the cost of a
name that lies grows with every reference to it.

**Why not one crate.** The toolkit will be several times the size of the transport, and a
single crate has no enforced direction — a widget could reach into the channel. Two crates
make the dependency explicit and checked by cargo. It is the same separation the maintainer
chose between `libinput` and `libui` in Milestone 3, for the same reason: input is not UI,
and transport is not toolkit.

---

## 11. Deliberately not here

Each of these would be reasonable in a mature toolkit and none is needed by the terminal.

- **Animation and a frame clock.** §6.3 has no timer. Anything that moves on its own needs
  one, and a compositor-side vsync signal to pace it. Trigger: the desktop shell's window
  transitions.
- **Theming and styling.** Colours and metrics are constants. Trigger: a second application
  that must look different from the terminal, or a settings service.
- **Accessibility.** No accessible tree, no screen-reader surface. Worth stating plainly as
  a gap rather than leaving it unmentioned; retrofitting one is substantially harder than
  designing it in, and this is a deliberate deferral rather than an oversight.
- **Text input methods, bidirectional text, complex shaping.** `libinput`'s keymap is
  US-only and `libdraw`'s font is a bitmap. The whole text stack has one milestone's worth
  of capability; the toolkit does not get ahead of it.
- **Subscriptions / async effects.** Iced's model for timers and streams. This system has a
  notification queue that would serve, but nothing needs it until an application wants to
  react to something other than input.
- **Scrolling containers.** The terminal's scrollback is the `custom` grid's own business;
  a general scroll viewport is a different widget with clipping and virtualisation
  questions. Trigger: the file browser.
- **Multi-window applications.** One `App` drives one window. Trigger: dialogs that are
  real windows rather than `stack` overlays.

---

## 12. Build order

Mapping onto [`display-arm-plan.md`](../planning/display-arm-plan.md) Milestone 4:

- **Part A** — `Element`, the diff with keyed identity (§3, §4), layout (§5), and painting
  with per-buffer damage (§6). The whole of this is pure and host-testable; the crate
  rename (§10) lands here because everything after it imports the new names. So does the
  **user stack going 32 KiB → 8 MiB with a guard gap**, which is a filed deferral this
  milestone triggers: a toolkit is recursive by construction, and the stack is demand-paged
  so the size costs address space rather than memory
  ([`deferred-decisions.md`](../rationale/deferred-decisions.md)).
- **Part B** — event routing: pointer capture, widget focus and traversal, key bubbling
  (§7) — plus §9.1's focus record, without which the second focus concept has no source.
- **Part C** — the widget set (§8), plus §9.2 key repeat and §9.3 the cursor, which are what
  make the set usable by a person rather than only by the harness.

The gate throughout is the existing one extended: `cargo xtask check-input` already drives a
real client through the compositor, and a toolkit client is the next thing it should drive.
