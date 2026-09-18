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
[`docs/design/nitrox-shell/`](../design/nitrox-shell/). It is the answer to "what should this
look like": when a question about appearance comes up, open the page and see what it does.

**It is composed at 1440×900, not at the laptop's 1360×768** — its root is a fixed
`width:1440px; height:900px` (review, finding 8; the first draft of this document said 1360×768
and the README's capture command cropped 80 px off the right and 132 px off the bottom, which is
why the bottom bar is missing from that picture). So about 24% more screen than the machine this
runs on, and **its proportions are a starting point rather than a specification**: a 30 px panel
and a 548×404 window were sized against a screen we do not have. Part A's first job is to decide
which of the design's metrics are absolute and which are relative, because transcribing them will
produce a desktop that fits worse than the one we have.

**What this work is not.** The page contains a notification centre, a quick-settings panel
with volume and brightness, a System Settings application, desktop icons and a launcher. Four of
those are **out of scope by decision** (2026-09-17): notifications and quick settings need
infrastructure that does not exist — no audio, no backlight, no notification service — Settings
mostly existed so the maintainer could try layouts, and desktop icons duplicate what the Places
menu already gives. The launcher is deferred rather than rejected: `Run Application… super+space`
is in the design and would be welcome, but it is a surface of its own and this is about the
surfaces we already have.

## The structural rule, and where it stops

**Colours land as data, and both sides of every gate read them.** `check-display` compares the
guest's screen against a `libdraw` render computed in `xtask`; `preview` draws the same frames on
the host so a judgement costs a glance. That only works because the expected answer is *computed*
— `tools/CLAUDE.md` puts it as "the place a gate's expected answer is computed, not stored".

A redesign is exactly the change that can quietly break that. Every visual assertion is about to
move, and the tempting repair is to adjust the reference until it matches whatever the code now
draws — which turns a gate into a tautology and it never fails again. **So no colour here is
written twice.** Colours go in the theme; the toolkit reads the theme; the reference render reads
the same theme. A gate then fails when the *code* disagrees with the *design*.

The design makes this easy, because it contains its own theme file: the mock editor is displaying
`theme.toml`, and its keys are nearly ours already.

### …and metrics deliberately *are* written twice — M11 decision 2

**This rule is about colour and stops there** (corrected after review; the first draft said "no
colour, radius or metric", which reverses a recorded decision without naming it). M11 decision 2
is that **chrome metrics are not themeable**, and `xtask` copies `BAR_H`, `INDICATOR_W`,
`ENTRY_W`, `TITLE_BAR_H` and the greeter's `420×200` with a comment on each saying why: *"a gate
that read the shell's layout to know where to aim could agree with a shell that had stopped
drawing where it says"*.

That is the same anti-tautology argument, reaching the opposite conclusion — because a *colour* is
compared against a computed render, while a *metric* is where a gate aims a click. A gate that
took its metrics from the code under test could not tell a moved button from a correct one. So
metrics stay compiled, stay duplicated in the gates, and the duplication keeps its reasons.

### `radius_px` is a compiled constant, not a theme key

For a second reason, and it is the one that decides it: **the compositor never reads a theme
file.** It is started by `init`, "so it never sees the setup record a theme is handed on" (M11
decision 1, `compositor/src/lib.rs`). After Part B the corner is drawn by the compositor and the
title bar by the client's toolkit — *two processes that must agree to the pixel*. A `radius_px`
that reached only one of them would show, at every corner, a wedge of client fill between two
arcs of different radius; a smaller file value would have the mask cut into client content
instead.

One constant, compiled into both, is the only form that cannot disagree. Making the radius
adjustable is a later question whose mechanism is M11 decision 1's own: a manager op on a channel
the shell already holds.

## Two palettes, which reverses a recorded decision

**Dark mode is in scope, and M11 decision 4 said not to**: *"One theme … Dark-and-light doubles
both … nothing ships one."* That decision was made when the desktop had one user and no polished
design; it now has a design that specifies two palettes, and the maintainer has asked for both.
Naming the reversal rather than quietly outgrowing it is the point of this paragraph — the
decision log carries a matching entry.

What the decision was right about is the cost, which is width rather than difficulty: every
`preview` frame, every `check-display` reference and every one of `check-resolutions`' twenty
boots exists per palette. Part A decides how that is covered without doubling the CI bill; the
likely answer is that the host render covers both and the boot covers one, since a palette is
data and a boot proves the data reaches the screen.

**And the compositor has to learn which palette is in force**, which it currently cannot. It
paints the shadow, the desktop ground, the drag outline and the cursor from values compiled in
from `Theme::light()`, and it never reads the theme file (M11 decision 1, above). The design's two
palettes differ in exactly those values — its dark shadow is `rgba(0,0,0,.55)` against light's
`.22`. Left alone, a dark desktop would light every window with a light-theme shadow. Part A says
whether the palette is chosen at build time or at run time, and by what mechanism it reaches that
process.

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

- [ ] **`theme.toml` gains `panel`, `ok`, `warn`, `deny` and `term`**, plus `accent` and `fgdim`,
      which Parts B, C and E all use — the outline on the current desktop card, the dim section
      headers, the dim hint column. `radius_px` is **not** among them; see above. The schema doc
      and the parser move together; `docs/spec/theme-toml-schema.md` is the contract.
- [ ] **Decide the near-duplicates before adding them.** The page also defines `--faceHi`/
      `--faceLo` (about our `face_hover`/`face_pressed`), `--lineSoft`, `--panelFg` and
      `--accentInk`, and `--soft` is 18% in the dark palette against 10% in light. Each is a key
      that is nearly a key we have, and a theme grows thirty of them one reasonable addition at a
      time. The same question as `panel` versus `sidebar` below.
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
- [ ] **Rounded rectangles** for toolkit surfaces, at the compiled radius.
- [ ] **A masked blit** on the compositor's row path — the primitive Part B's corners need, and
      the only thing that actually rounds one.
- [ ] **A two-layer shadow.** The design's is a wide soft one plus a tight dark one; ours is
      single. `cargo xtask tune` is how the parameters get chosen — it exists so this judgement
      costs a second rather than a boot.
- [ ] **Both palettes are covered**, and the decision about how is part of this part rather than
      discovered in CI. So is **how the compositor learns which palette is in force**, since it
      cannot read a theme file and it draws the shadow, the ground, the outline and the cursor.
- [ ] **Which of the design's metrics are absolute and which are relative**, given it was composed
      for a screen 24% larger than the target. This is a decision, not a transcription.
- [ ] **`widget-toolkit.md` stops naming a `Theme::dark()` that does not exist.** A review found
      it (2026-09-17); once this part adds a real one, that sentence would read as true and still
      be wrong about which theme is the fallback, which stays `light()`.

**The risk worth stating: a blended wash depends on what is under it**, so every damage path must
paint the ground before the wash. `libdraw`'s gradient code already carries this warning in
another form — "two rectangles rather than one… computing the ramp from the clip instead would
make a partial repaint draw a *different* picture from a full one". A selection that is correct on
a full repaint and wrong on a partial one is the same bug, and it is invisible to a test that only
ever paints whole surfaces.

## Part B — window chrome

- [ ] **Rounded corners**, which need a new blit primitive — see below. They are neither a
      paint change nor, as the first draft had it, a `covers` change.
- [ ] **The titlebar**, restyled: the title left, the three controls right, `panel` behind.
- [ ] **Focus reads at a glance** — `title_active` against `title_inactive`, diverging from the
      design as argued above.
- [ ] **Menus in the design's style**: dim small-caps section headers, a right-aligned hint
      column (the shortcut in Applications, the path in Places), separators, and `deny` for a
      destructive item. The design has no *window* menu open, so `File`/`Edit`/`View` take this
      same style — inferred, and recorded here as inferred.

**Rounded corners are a blit change, and the first draft of this section had the mechanism
wrong.** It claimed the fix was to make `covers` a region rather than a predicate. `covers`
decides only which *background* `compose_exposed` fills, and `compose_exposed` is contracted —
and tested, by `compose_exposed_draws_the_same_picture_as_compose` — to draw exactly what
`compose` draws, and `compose` has no `covers` at all. **No change to `covers` can alter a single
output pixel.** The corner is written by `blit_rows`, which for an opaque surface in the screen's
own format is a row `memcpy` of the whole rectangle, corners included. Build the region form
exactly as first written and the background is filled under the corners and then copied straight
over. Nothing is rounded. (PR #311 review, finding 2.)

**What rounds a corner is a masked blit**: a row path that leaves the corner pixels unwritten, or
blends them along an antialiased edge. That is the new primitive to cost, and it belongs beside
`blend_rect` in Part A. The choice is then *how the corner pixels get their colour*:

- [ ] **The mask, on the row path** — the primitive itself, and the only way any of this happens.
- [ ] **Skip the corner pixels and let the stack fill them.** Surfaces are painted in stack order,
      so a skipped pixel keeps whatever lower surface already painted there — the desktop where
      nothing is behind, the window below where something is. This is what the first draft's
      "option 2" should have said; it is wrong only if the compositor paints corners with the
      desktop colour, which is a mistake rather than a design.
- [ ] **`covers` minus the corners**, as the optimisation that *accompanies* the mask rather than
      an alternative to it: a masked surface no longer covers its full rectangle, so the
      background beneath the corners must be filled. The subtraction machinery (`cut`) exists.

**The option not to take, and its real cost.** Giving windows an alpha format with transparent
corners would work, and the first draft priced it against M13 Part A's double-write measurement —
which is the wrong number. That measurement found removing half the pixel writes bought **6%**,
because *"the writes were never the cost"*. The cost is the per-pixel path: an alpha surface goes
through `blit_blended` instead of the `memcpy`, and the `memcpy` fast path is what made compose
about **5× faster** (4.54 → 0.90 ms under KVM, 2026-09-03). Alpha windows would pay that on every
pixel of every window, not just at the corners. So the option is worse than the first draft said,
for a different reason.

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
      label and a dim right-aligned path. **Trash is dropped**, because there is no trash.
- [ ] **The list has one source, which means moving it.** `DEFAULT_FOLDERS` is only `Documents`,
      `Downloads` and `Pictures`; `Home` first and `Root` last are added by `Browser::places()`, a
      method on the file browser that `desktop-shell` cannot call (review). Either that function
      moves somewhere both can reach, or the menu is a second copy of the list — and a second copy
      is what the first draft claimed it was not.
- [ ] **`Run Application…` and `End session` are omitted**, not disabled: the launcher is
      deferred and there is no logout. A menu item that does nothing is worse than an absent one.
- [ ] **The bottom bar gains a show-desktop button** on the left: minimise everything, press
      again to restore. **The restore set belongs to the shell**, which already tracks
      `WinEntry.minimized` — minimising is a manager operation, and the compositor's "whole part
      is to check the caller owns the window and hand the manager the question" (M9 Part B). The
      first draft put this state in the compositor, which is the process the architecture keeps
      policy-free (review, finding 6).
- [ ] **The switcher moves to the bottom right**: `‹`, up to three squares, `›`, the current
      desktop's name. The rule is `min(3, total)` — **not** "two when there is no previous
      desktop", which is what the first draft read off a screenshot with two desktops open
      (review, finding 7). On desktop 1 of three or more it shows three cells with the back
      arrow disabled.
- [ ] **`desktop-shell.md` §7 is updated in the same change.** It records a *compact indicator*
      as the decision and rejects "GNOME 2's full desktop switcher", on the grounds that a row of
      boxes churns as desktops come and go and "a name is also a better use of the space". The
      design answers the churn objection — three cells is bounded — and keeps the name beside
      them. That is a decision being revisited on new evidence, which is fine, and leaving §7
      saying the opposite is not.
- [ ] **The window list stays** and is restyled.

## Part D — the greeter

- [ ] **The greeter matches**, which the design does not cover because it was never drawn. It is
      `desktop-session-mgr`'s own window, and `check-login` asserts its size and where it centres
      — so those numbers move with the restyle and the gate moves with them. Named because a gate
      whose expectation changes in the same commit as the code is the shape that needs a reason
      in the message.

## Part E — the overview

- [ ] **Cards per desktop**, the current one outlined in `accent`, with `Desktop 1 · 3 windows`
      beneath. A layout change to a surface that exists (M13 Part C), over the translucent ground
      that already works.
- [ ] **The miniatures stay frozen**, which `desktop-shell.md` §6 already decided: "thumbnails are
      frozen … live thumbnails are an optimisation with a trigger". The first draft said "live",
      which would have pulled that trigger by accident (review, finding 7). The design's own cards
      are schematic boxes labelled with the application's name rather than pixels at all, so
      frozen captures are if anything more than it asks for.

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
- [ ] **One new set of sixteen, not one per palette.** The design keeps `--term` and `--termFg`
      *outside* its two themes, so its terminal is identical in light and dark — and
      `Palette::default`'s own doc says why that is right: "retheming a desktop must not retheme
      `ls` output", the sixteen are tuned for a dark ground, and "a dark terminal sits on a light
      desktop". So this is a change to `Palette::default()`'s values, once (review, finding 5).
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

- **Which way the corner mask gets its pixels** — skip-and-let-the-stack-fill is the cheap,
  correct one, but it has not been built and the exposed-region bookkeeping that goes with it is
  the part that could surprise.
- **How both palettes are gated** without doubling the QEMU bill.
- **Whether `panel` and `sidebar` are two colours or one**, and the same for the four
  near-duplicates in Part A. A new key that is nearly an old key is how a theme grows thirty of
  them.
- **Whether six parts is the right cut.** F is independent of A–E and could land separately.
- **What "adapting the proportions" means concretely** — the design is 1440×900 and the machine
  is 1360×768, and the answer is probably not a uniform scale.

## What the first draft got wrong

Recorded because the errors are more instructive than the corrections, and because a detail pass
that quietly fixes itself teaches nobody.

1. **The headline finding was wrong.** It said rounded corners were a `covers` change. `covers`
   decides only which background gets filled, and `compose_exposed` is *tested* to draw the same
   picture as `compose`, which has no `covers` — so no change to it can alter an output pixel. The
   corner is a `memcpy` in `blit_rows`. The real primitive is a masked blit, which the first draft
   never mentioned.
2. **"No colour, radius or metric is written twice" reversed M11 decision 2** — chrome metrics are
   deliberately duplicated in the gates, by the same anti-tautology argument pointed the other
   way, because a gate that takes its aim from the code under test cannot see a moved button.
3. **Dark mode reverses M11 decision 4** and the first draft did not say so.
4. **Three claims were read off screenshots rather than the source**: the switcher's rule
   (`min(3, total)`, not "two when there is no previous"), the overview's thumbnails (the design's
   are schematic, and §6 already decided frozen), and the page's size (1440×900, not 1360×768).
5. **Show-desktop state was put in the compositor**, the one process the architecture keeps
   policy-free.

The common thread in 3, 4 and 5: **a design is evidence about appearance and not about
architecture**, and three of those five came from treating a picture as a specification.
