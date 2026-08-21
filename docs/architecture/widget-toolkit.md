# Nitrox: The Widget Toolkit

**Status: built (2026-08-11, last checked 2026-08-20), and this document describes what
exists.** Milestone 4 built
all of it: the retained tree and the declarative `view` (`userspace/libui/src/element.rs`),
measure/arrange layout (`layout.rs`), the keyed diff that damage falls out of (`diff.rs`),
per-buffer damage accumulation (`damage.rs`), event routing with implicit capture
(`route.rs`), painting within a damage rectangle (`paint.rs`), and the widget set —
`button`, `scrollbar`, `menu_bar`, and the `custom` escape hatch (`widget.rs`). Text is real
TrueType through `libdraw::text`, rasterised from a font read off the root filesystem at
runtime. `reference.rs` is the fixed picture `cargo xtask check-display` renders on the host
and compares against the guest's screen pixel for pixel.

**What is specified here and not built**, each with its reason in place: the **application
runtime** (§2.2 — every piece of the loop exists and nothing owns the sequence; M5's terminal
assembles it by hand and is the first application to do so), **theming** (§11), and the
**text area** (§8, absent on purpose — the terminal's grid is a `custom` widget, so nothing yet
needs one). The menu's **popup half** left this list in M5 Part B and left the toolkit
entirely in M6 C3: a menu is a `popup` *window* now, parented to its application's window,
positioned by the client at the anchor `locate` gives and clipped by the **screen**. `nxterm`
still uses `locate`; it no longer uses `offset`, which has no consumer outside this crate's own
tests. A layer can only be clipped to its parent, which is not what a menu needs.

Everything else below describes running code. Three sections carry a **banner** marking them as
the design argument for something now built, rather than a description of a gap — §9.1 the
focus record, §9.2 key repeat, §9.3 the cursor — and two bullets in §2.1 and §11 carry a
parenthetical striking a claim that was true when written and is not now.

**Graduated from `design/` 2026-08-12**, as Milestone 5's first prerequisite. It sat in
`design/` for a day after the code landed, which root `CLAUDE.md` tells every session to read
as "not current behaviour" — so a fresh session was told the toolkit was hypothetical.

**The body was audited against source line by line on 2026-08-12**, in the PR's review, after a
first pass rewrote only this Status line and left the four hundred lines it vouches for
unchecked. That pass shipped a document asserting that key repeat and the cursor did not exist,
that `libdraw`'s font was a bitmap, that userspace took no external dependencies, and that a
runtime existed which does not — while claiming to be verified. Design settled with the
maintainer 2026-08-10; the reasoning below is unchanged from that pass except where a banner or
a parenthetical says otherwise.

**Companion documents.** [`display-substrate.md`](../design/display-substrate.md) states the
principles this rests on — client-side rendering, damage-rectangle commits, the compositor
owning window focus. [`ui-composition-model.md`](../design/ui-composition-model.md) owns what a window
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
- **It is a shape, not a port.** No ecosystem crate is involved — the toolkit is written here,
  borrowing Elm's structure rather than any implementation of it.

  (This bullet also asserted that "the kernel and userspace take no external dependencies",
  which was true when it was written on 2026-08-10 and stopped being true the next day.
  Userspace takes crates and the kernel does not; `userspace/CLAUDE.md` states the bar each one
  clears, and `libdraw` takes `ab_glyph` and `libui` takes `libm` under it. The claim was never
  the point being made here, and is struck rather than repaired.)

### 2.2 The application's loop

```rust
fn update(state: &mut App, msg: Msg);
fn view(state: &App) -> Element<Msg>;
```

Everything else is: wait for events, translate them to `Msg`, call `update`, call `view`,
diff, paint the damage, commit. An application should never touch a buffer, a damage
rectangle, or the event queue.

**The pieces of that loop are built and the loop itself is not.** `libui` has `Element` and
`view`'s return type (§3), `Tree::update` for the diff (§4), `Router` for events (§7),
`BufferDamage` for accumulation (§6.2) and `paint` (§6) — but there is no `libui` module that
owns the sequence. An application wires them together, and so far exactly one does, for a
fixed picture with no `update` and no `Msg` dispatch (`libui::reference`, which the display
gate renders).

Whether the loop becomes a `libui` runtime or stays the application's is **an open question
this document should not pretend to have settled**, and Milestone 5's terminal is the first
thing that will answer it: it is the first application with state, messages and a reason to
re-`view`. The signatures above are the shape either answer takes.

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

Plus `padding`, fixed `sized`, and `offset` wrappers. That is the whole layout vocabulary.

**Not a CSS box model.** Margins-collapsing, floats, and inline flow are an enormous surface
for a system whose first application is a rectangle with a bar on top.

**Absolute positioning exists for exactly one thing: overlays.** `offset(dx, dy, child)` shifts
a child within its parent at the child's own *measured* size. The general prohibition still
holds and the reason is unchanged — a terminal is resized constantly, so a layout built from
coordinates is a layout engine written in application code, recomputed by hand on every resize.
That argument does not reach a popup, whose entire definition is "here, under the item that
opened it": there is nothing to recompute, because the position is derived from another
element's laid-out rectangle (`locate`) rather than chosen. Added in M5 Part B, when the
terminal's menu became the first thing to need it.

Two properties keep it from becoming general-purpose positioning by the back door. It changes
its **own** rect, not merely its child's, so the containment invariant below holds. And that
rect is **clipped to its parent**, so an overlay bigger than its container is cut off rather
than escaping — which is the same rule as §5's "a menu clipped to its window is not a menu",
one level down. A menu that must escape its *window* is a `Role::Popup` window, not a node.

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

**The handler may be on an ancestor of what was hit, and dispatch walks up to find it.** A
composite widget is built from parts — `button` is a `Stack` carrying `on_press`, with a `fill`
and a `text` inside it — so the deepest widget under the cursor is one of the *parts*, which
handles nothing. `on_press` and `on_pointer` are therefore looked for on the hit node and then on
each ancestor in turn, and the first that has one receives the event **in that widget's
coordinates**. This is the same rule §7.2 states for keys, arriving late: until M6 C3 no button
in this toolkit was clickable, because every click landed on a label.

**Focus does not walk up.** It is a claim on the keyboard rather than a response to this event,
and an ancestor that is `focusable` without an `on_key` — which `button` is — would take the
keyboard from whatever had it and handle nothing. Clicking a menu bar killed typing until this
was made an exception.

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

- ~~**Widget focus is a path** to a node in the tree~~ — **a widget id**, as built. A path
  breaks under exactly the reordering that keys exist to survive: delete a row above the
  focused one and the path now names its neighbour. The id was not in this document because
  it did not exist when this was written; it arrived in Part A, and focus is the second thing
  after the diff to need it. Paths are still how a *frame* reaches a widget — `path_to_id`
  resolves one each time — but nothing stores one across frames.
- **Tab traverses** it in tree order among widgets that accept focus.
- A key goes to the focused widget; if it has no handler **or its handler declines**, the
  event bubbles to ancestors. That is how a menu accelerator works without every widget
  knowing about menus — and declining is what makes it possible at all: with a handler that
  cannot decline, a focused text field swallows every accelerator, and this bullet describes
  something unreachable. `on_key` therefore returns `Option<Msg>`.
- A key nothing claims returns nothing. There is no separate "reaches the application" step:
  the caller *is* the application and still holds the event it passed in.
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

**What the terminal added, which is §1's rule leaving a trace.** M4 shipped the widgets above
and stopped; M5's terminal then needed three things that were missing, and each is a small
public addition rather than a redesign:

| Added | Why the terminal needed it |
|---|---|
| `layout::locate` | Finding where the menu item was laid out, which is the anchor the popup *window* is created at (§5). `offset` was added here for the same job and is no longer used for it — a menu is a window as of M6 C3, and a `stack` layer cannot leave its parent |
| `ScrollState::offset_at` | `thumb()`'s inverse. M4 could say where a thumb goes for an offset but not what *grabbing* it means, so a scrollbar was a picture of a scrollbar |
| `diff::Tree::find_by_key` | `locate`'s companion — "which widget is it", where `locate` answers "where was it laid out". A window has to name the widget that starts with the keyboard, and tree order would give it to the menu button |

None of the three is speculative and none would have been designed right in the abstract:
`offset_at`'s centring rule, in particular, is a decision about how a drag feels that only a
real drag poses.

The `custom` widget deserves emphasis rather than apology. A toolkit whose escape hatch is
an afterthought forces its flagship application to fight it; here the flagship *is* an
escape-hatch client, and that is the design working as intended — the toolkit supplies
chrome, layout, focus and input plumbing, and gets out of the way where an application
knows better.

---

## 9. Three things below the toolkit that Milestone 4 forces

They are described here because the toolkit cannot be designed without knowing their shape.

**All three are built** — the focus record in M4 Part B, key repeat and the cursor in Part C —
and each section below carries a banner saying so before its design argument. Where each was
*recorded* while it was still owed differed, and is worth keeping straight: key repeat was a
filed deferral in [`deferred-decisions.md`](../rationale/deferred-decisions.md); the focus
record and the cursor were gaps named in `rsproto-surface-ops.md`, which is now where the
contract for all three lives.

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

**Built in M4 Part C.** The compositor generates repeats: `Repeat::after_key` decides when one
arms, `Repeat::due` advances it, and `KEY_REPEAT` (`value == 2`) reaches a widget through
`libui::route`. `rsproto-surface-ops.md` is the current contract. The rest of this section is
the design as it was argued, kept because it records *why* the compositor generates them — not
as a description of what is missing.

The record format already reserved `value == 2`
([`input-subsystem.md`](input-subsystem.md) §3), so no wire change was needed.

**The compositor generates repeats**, not each client, because it knows which window has
focus and so can stop a repeat when focus moves with no client involvement. The alternative
is Wayland's: send clients a repeat *rate* and let each implement its own timer. That is
better when clients differ in what they want repeated, which is a distinction nothing here
makes yet, and it costs every client a timer and a state machine.

**What it cost, stated accurately — and this was the estimate, which held.** The outbox retry
gave the serve loop a bounded wait, but it is armed *only while something is parked*, and a
held key with an empty outbox is precisely the not-parked case. So this was never "the timer is
already there": it took a second deadline source `min`-ed with the retry one, held-key state,
and re-deriving the deadline when focus moves. That is what was built. Two things the estimate
did not foresee: focus validation is **self-enforcing** — `fire_repeat` checks the repeat's
window is still the focus candidate, rather than clearing at every focus-moving site — and
**modifiers must not arm a repeat**, which cost a bug found in review (holding Ctrl repeated it,
and pressing Shift mid-run displaced the key that was actually held).

Delay and rate are policy with no configuration surface yet: `REPEAT_DELAY_NS` and
`REPEAT_INTERVAL_NS` in the compositor, named rather than spelled inline so that a settings
service owning them later is a change of provenance.

### 9.3 A cursor on screen

**Built in M4 Part C.** `WindowStack::present_into` composites the stack and then draws the
pointer over every damage rectangle; `check-display` asserts it is on screen. The rest of this
section is the design as it was argued — not as a description of what is missing.

Before it, nothing drew the pointer: the compositor knew where it was and the screen did not
show it. Tolerable for a gate that injects clicks at known coordinates, not for a person
opening a menu.

The compositor draws a fixed arrow after the window stack, and repaints both the old and the
new cursor rectangles on every move — both, because the cursor is *drawn over* the composed
stack rather than composited into it, so the pixels it covered are still on screen after it
moves. That is also why `compose_into` is `pub(crate)` and `present_into` is the only way the
server updates a region: three code paths recomposed without redrawing the pointer, and a click
erased it until the mouse moved next. Per-client cursor shapes — a text I-beam over
the grid, a resize arrow on an edge — are a protocol addition and are **not** in this
milestone; a single arrow is what the terminal needs to be usable.

Worth noting: this makes pointer motion damage the screen on every move, where before it
damaged nothing. The compositor's existing damage path absorbed it, but it is the first thing
in the system that repaints because a person moved a mouse, and it is where the compositing
loop's cost becomes visible.

---

## 10. Crate boundary, and a rename

`libui` *was* window lifecycle, buffers, commit/release, and the input queue: a **client of
the Surface protocol**, not a UI toolkit. The name was aspirational and the code went
elsewhere. Renamed in M4 Part A.

So:

```
libui        ← the toolkit: widgets, layout, focus, the diff
  ↓
libsurface   ← window, buffers, events                (what `libui` used to be, renamed)
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
- **Text input methods, bidirectional text, complex shaping.** `libinput`'s keymap is US-only,
  with no dead keys and no compose sequences. The whole text stack has one milestone's worth of
  capability; the toolkit does not get ahead of it.

  (This said "and `libdraw`'s font is a bitmap", written the day before `libdraw` grew a real
  TrueType rasteriser. Glyphs are `ab_glyph` at any size, from a font read off the root
  filesystem at runtime; what the text stack still lacks is *shaping* — ligatures, bidi,
  combining marks — which is a different thing from a rasteriser and is the thing this bullet
  defers.)
- **Subscriptions / async effects.** Iced's model for timers and streams. This system has a
  notification queue that would serve, but nothing needs it until an application wants to
  react to something other than input.
- **Scrolling containers.** The terminal's scrollback is the `custom` grid's own business;
  a general scroll viewport is a different widget with clipping and virtualisation
  questions. Trigger: the file browser.
- **Multi-window applications.** One `App` drives one window. Trigger: dialogs that are
  real windows rather than `stack` overlays.

---

## 12. How it was built, and what building it changed

Milestone 4 of [`display-arm-plan.md`](../planning/display-arm-plan.md), in three parts, all
landed by 2026-08-11:

- **Part A** — `Element`, the keyed diff (§3, §4), layout (§5), and painting with per-buffer
  damage (§6): pure and host-tested throughout. The crate rename (§10) landed here because
  everything after it imports the new names, and so did the **user stack going 32 KiB →
  8 MiB with a guard gap**, a filed deferral this milestone triggered — a toolkit is
  recursive by construction, and the stack is demand-paged so the size costs address space
  rather than memory.
- **Part B** — event routing: pointer capture, widget focus and traversal, key bubbling (§7),
  plus §9.1's focus record, without which the second focus concept had no source.
- **Part C** — the widget set (§8), real TrueType glyphs, §9.2 key repeat and §9.3 the cursor,
  and the font on the root filesystem.

**Four places where the code diverged from this document, and the code won.** Recorded here
because a reader comparing the two should know which corrections are already applied:

- **`Element::on_key` returns `Option<Msg>`**, not `Msg`. §7.2's motivating example —
  unhandled keys reaching the menu — was unreachable with a non-optional return, because
  having a handler *meant* handling.
- **Widget focus is an id, not a path.** A path breaks under exactly the reordering that keys
  exist to survive. This document predates `Widget::id`, which Part A added.
- **The compositor's `ENTER`/`LEAVE` are inputs to the router, not events it forwards.** Its
  crossings are about windows and the toolkit's are about widgets; forwarding both handed a
  widget two enters.
- **Damage accumulates per buffer**, and §6.2 described that without saying which frame's
  damage a newly-released buffer is charged.

**The gate.** `cargo xtask check-input` injects a keystroke and a click and asserts both reach
a *widget* through `route.rs`; `cargo xtask check-display` renders `libui::reference` on the
host and compares it against the guest's screen pixel for pixel. That second gate found a
defect on its first run — `Node::Fill` measured to the whole available space, so the first
widget in a `Row` took it all and its siblings got none — which every widget test had missed
by laying one widget into a rectangle of its own.
