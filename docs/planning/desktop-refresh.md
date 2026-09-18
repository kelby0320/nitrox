# The desktop refresh — adopting the Nitrox shell design

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the current
status and the running order. **Between Phases 5 and 6**, and before the administration work:
those tools are UI surfaces, and building them against the current look to restyle them later is
building something that has to be replaced.

**Not a milestone of the display arm.** That subproject is complete at M15 and
[its plan](display-arm-plan.md) is the record. This stands on it — M11's theme, M13's translucent
surfaces and overview, M15's argument about what makes a control read as one — without being
numbered into it: the source of truth here is an artifact produced outside the codebase, and the
constraints include a machine that did not exist while M1–M15 were written.

**A north star produced outside the codebase.** The maintainer designed a polished shell with
Claude Design and handed it over as a runnable page plus screenshots, now in
[`docs/design/nitrox-shell/`](../design/nitrox-shell/). It renders at 1360×768 — this laptop's
size — and it is the answer to "what should this look like": when a question about appearance
comes up, open the page and see what it does.

**What this work is not.** The page contains a notification centre, a quick-settings panel
with volume and brightness, a System Settings application, desktop icons and a launcher. Four of
those are **out of scope by decision** (2026-09-17): notifications and quick settings need
infrastructure that does not exist — no audio, no backlight, no notification service — Settings
mostly existed so the maintainer could try layouts, and desktop icons duplicate what the Places
menu already gives. The launcher is deferred rather than rejected: `Run Application… super+space`
is in the design and would be welcome, but it is a surface of its own and this is about the
surfaces we already have.

## The one structural rule

**The design lands as data, and both sides of every gate read it.** `check-display` compares the
guest's screen against a `libdraw` render computed in `xtask`; `preview` draws the same frames on
the host so a judgement costs a glance. That only works because the expected answer is *computed*
— `tools/CLAUDE.md` puts it as "the place a gate's expected answer is computed, not stored".

A redesign is exactly the change that can quietly break that. Every visual assertion is about to
move, and the tempting repair is to adjust the reference until it matches whatever the code now
draws — which turns a gate into a tautology and it never fails again. **So no colour, radius or
metric here is written twice.** They go in the theme; the toolkit reads the theme;
the reference render reads the same theme. A gate then fails when the *code* disagrees with the
*design*, which is the only disagreement worth a gate.

The design makes this easy, because it contains its own theme file: the mock editor is displaying
`theme.toml`, and its keys are nearly ours already.

## Two palettes doubles the surface

Dark mode is in scope. It is not hard and it is *wide*: every `preview` frame, every
`check-display` reference and every one of `check-resolutions`' twenty boots exists per palette.
Part A decides how that is covered without doubling the CI bill — the likely answer is that the
host render covers both and the boot covers one, since a palette is data and a boot proves the
data reaches the screen.

## One deliberate divergence

**Focus will be more visible than the design makes it.** In the mock, a focused window differs
from an unfocused one by a thin outline in dark mode and almost nothing in light. The maintainer
called this out and is right: a desktop where you cannot tell which window has the keyboard is a
desktop that will be reported as a focus bug. We already carry `title_active` and `title_inactive`
— two keys that exist for this and currently barely differ — so the divergence costs nothing but
choosing two colours that do.

- [ ] **Part A — the design as data**: the theme gains what the design needs, in two palettes,
      and `libdraw` gains the three primitives they imply.
- [ ] **Part B — window chrome**: rounded corners, the new titlebar, focus that reads, and menus
      in the design's style.
- [ ] **Part C — the panels**: the Applications and Places menus, and a bottom bar with a
      show-desktop button and the new switcher.
- [ ] **Part D — the greeter**, which the design does not cover and which has to match anyway.
- [ ] **Part E — the overview**, whose layout changes.
- [ ] **Part F — a terminal with colour**, which is `nxsh` using a mechanism `libterm` already
      has.

## Part A — the design as data

The token set, read out of the page's own stylesheet:

```
--accent #2C7F92   --fg  #16201F   --face   #EDECE9   --panel #F2F1ED
--bg     #FFFFFF   --fgdim #5B6766 --sidebar #E5E3DE  --line  #C7C4BD
--ok     #3C7A5A   --warn #9A6B22  --deny   #A4453C   --term  #0C1213
--r 8px            --sel #2C7F9233 --soft #2C7F921A
--sh 0 10px 28px rgba(0,0,0,.22), 0 2px 6px rgba(0,0,0,.14)
```

- [ ] **`theme.toml` gains `radius_px`, `panel`, `ok`, `warn`, `deny` and `term`.** The schema
      doc and the parser move together; `docs/spec/theme-toml-schema.md` is the contract.
- [ ] **Colours may carry alpha.** `--sel` and `--soft` are the accent at 20% and 10%, and the
      parser today requires exactly six hex digits (`libdraw/src/theme.rs`). Eight-digit hex, for
      the slots that want it.
- [ ] **`Theme::dark()` beside `Theme::light()`**, and a key that names which a file starts from,
      so a theme file is an override on a palette rather than a list of thirty colours. `light()`
      is already a `const fn` and the compositor depends on that; `dark()` matches it.
- [ ] **`blend_rect` in `libdraw`.** A translucent wash over content. The arithmetic exists
      (`Rgb::blend`), the loop exists (`fill_rect`), and `blend_pixel` already pairs them for
      antialiasing — this is the rectangle case, which nothing has needed until now.
      `fill_rect_alpha` is **not** it: that stores an opacity for something further down to
      composite, which is the overview's trick, not a wash.
- [ ] **Rounded rectangles**, radius from the theme.
- [ ] **A two-layer shadow.** The design's is a wide soft one plus a tight dark one; ours is
      single. `cargo xtask tune` is how the parameters get chosen — it exists so this judgement
      costs a second rather than a boot.
- [ ] **Both palettes are covered**, and the decision about how is part of this part rather than
      discovered in CI.

**The risk worth stating: a blended wash depends on what is under it**, so every damage path must
paint the ground before the wash. `libdraw`'s gradient code already carries this warning in
another form — "two rectangles rather than one… computing the ramp from the clip instead would
make a partial repaint draw a *different* picture from a full one". A selection that is correct on
a full repaint and wrong on a partial one is the same bug, and it is invisible to a test that only
ever paints whole surfaces.

## Part B — window chrome

- [ ] **Rounded corners**, and see below — they are not a paint change.
- [ ] **The titlebar**, restyled: the title left, the three controls right, `panel` behind.
- [ ] **Focus reads at a glance** — `title_active` against `title_inactive`, diverging from the
      design as argued above.
- [ ] **Menus in the design's style**: dim small-caps section headers, a right-aligned hint
      column (the shortcut in Applications, the path in Places), separators, and `deny` for a
      destructive item. The design has no *window* menu open, so `File`/`Edit`/`View` take this
      same style — inferred, and recorded here as inferred.

**Rounded corners are a compositing change, not a painting one.** `compose_exposed` skips
painting the background under any surface that `covers` its rectangle, where `covers` is "opaque
format, and long enough to write every pixel it claims" (`libdraw/src/compose.rs:309`). A window
with rounded corners **does not cover its rectangle** — its corner pixels must show whatever is
behind, which is the desktop sometimes and another window otherwise. Three ways out, and the
choice belongs in review:

1. **Give windows an alpha format and transparent corners.** Correct, and the machinery exists —
   M13 Part B already put translucent surfaces in this path. But `covers` then fails for the
   *whole* window, so every window is blended rather than copied, everywhere. That is the
   double-write M13 Part A measured, paid on every pixel of every window.
2. **Let the compositor paint the corners.** Cheap, and wrong where windows overlap: the corner
   would be filled with the desktop when another window is behind it.
3. **Make `covers` a region rather than a predicate** — the rectangle minus four corner squares.
   The subtraction machinery is already there (`cut`, which the exposed-region walk uses), the
   optimisation survives for the bulk of every window, and the corners compose normally.

**Three is the one to cost first.** It is more change than the others and it is the only one that
is both correct and cheap, and this is a machine where a framebuffer mistake cost 45× within
living memory.

## Part C — the panels

- [ ] **The Applications menu becomes a menu**, not a modal — sections, rows with icons, a
      separator. **No categories**: the design groups three programs under "Accessories" and a
      System section, and a taxonomy invented for three entries is one we would have to live
      with. A flat list until there are enough entries to need grouping (maintainer, 2026-09-17).
- [ ] **…but typing still narrows it.** Today `Super+A` opens a modal and typing filters; on the
      laptop that is the fastest path to a program and the only one that does not need a pointer.
      Adopting the menu's *look* must not cost the menu's *behaviour* — this is the one place the
      design would be a regression if followed literally.
- [ ] **A Places menu**: `Home`, `Documents`, `Downloads`, `Pictures`, `Root`, with a swatch, a
      label and a dim right-aligned path. The entries are `nxfiles::DEFAULT_FOLDERS` — the same
      list, not a second copy of it — and **Trash is dropped**, because there is no trash.
- [ ] **`Run Application…` and `End session` are omitted**, not disabled: the launcher is
      deferred and there is no logout. A menu item that does nothing is worse than an absent one.
- [ ] **The bottom bar gains a show-desktop button** on the left: minimise everything, press
      again to restore. The compositor has to remember what it minimised, which is new state.
- [ ] **The switcher moves to the bottom right**: `‹`, two or three squares, `›`, the current
      desktop's name. Two squares when there is no previous desktop and the back arrow is
      disabled — the design's own rule, and it matches how desktops already work here.
- [ ] **The window list stays** and is restyled.

## Part D — the greeter

- [ ] **The greeter matches**, which the design does not cover because it was never drawn. It is
      `desktop-session-mgr`'s own window, and `check-login` asserts its size and where it centres
      — so those numbers move with the restyle and the gate moves with them. Named because a gate
      whose expectation changes in the same commit as the code is the shape that needs a reason
      in the message.

## Part E — the overview

- [ ] **Cards per desktop**, each a live miniature, the current one outlined in `accent`, with
      `Desktop 1 · 3 windows` beneath. A layout change to a surface that exists (M13 Part C),
      over the translucent ground that already works.

## Part F — a terminal with colour

The design shows `nxsh`'s output coloured: the banner, the table's header row, the prompt.
**Linux terminals do not do this** — they implement SGR and the *program* emits it, which is why
`ls --color` and `grep --color` each carry their own flag and their own `isatty` check.

**We already have the whole mechanism, and a better place to put it.** `libterm` parses SGR and a
`Cell` carries `Colour::Default | Colour::Ansi(..)` — symbolically, because, as `cell.rs` argues,
"a stored colour would freeze the theme into the scrollback, and re-theming would recolour new
text only". And because output here is a *typed stream rendered by the shell*, colouring `nxsh`'s
renderer once colours every program's output: no per-program flag, no `isatty` dance, no
reinvention per tool. `nxsh` knows which cell is a header because it built the table.

- [ ] **`nxsh` emits SGR** for what it knows is structural, and only when it has a terminal.
- [ ] **The sixteen get values per palette**, which is where the design's terminal colours land.
- [ ] **No truecolour.** Sixteen symbolic colours is what keeps scrollback re-themable, and
      adding a stored 24-bit colour would undo the argument `libterm` already makes.

## Deferred, and named so they are choices

- **The launcher** (`Run Application…`, `super+space`) — wanted, its own surface.
- **System Settings** — the mock's version exists to try layouts; a real one wants the admin
  phase's tools behind it, or it is a window full of controls that change nothing.
- **Notifications and quick settings** — no service, no audio, no backlight.
- **Desktop icons** — the Places menu covers it.
- **`End session`** — needs a logout path, and probably a System menu to live in.
- **The dark-mode focus outline** — we are indicating focus on the titlebar instead.

## Open questions for review

- **Which of Part B's three ways to round a corner**, and what the region form costs.
- **How both palettes are gated** without doubling the QEMU bill.
- **Whether `panel` and `sidebar` are two colours or one.** The design uses `#F2F1ED` and
  `#E5E3DE`; we already have `sidebar`. A new key that is nearly an old key is how a theme grows
  thirty of them.
- **Where the sixteen terminal colours live** — in `theme.toml` as sixteen more keys, or derived
  from the palette. Sixteen keys is a lot of surface; derivation is a rule nobody can override.
