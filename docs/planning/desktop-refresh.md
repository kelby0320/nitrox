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

- [x] **Part A — the design as data**: the theme gains what the design needs, in two palettes,
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

- [x] **`theme.toml` gains `accent`, `foreground_dim`, `panel`, `ok` and `deny`** (2026-09-18).
      **Not `warn`**: the page defines `--warn` and uses it nowhere. **Not `term`**: `--term` sits
      outside the design's two palettes and belongs to `libterm`'s sixteen, which is Part F's —
      this list contradicted Part F as first written. `fgdim` is `foreground_dim`, in the
      schema's own naming. `accent` **replaced** `focus_ring` and `selection` rather than joining
      them — see the next item. The schema doc and the parser moved together.
- [x] **The near-duplicates, decided from the page's code rather than its pictures.** Its
      `apply()` *computes* `--sel` (accent + `33`) and `--soft` (accent + `1A`, or `2E` in the dark
      palette) from `--accent`, so storing them would be storing a computation: `selection` became
      `Theme::selection()`, derived. `--faceHi`/`--faceLo` are `face_hover`/`face_pressed`, the
      mapping the design's own mock `theme.toml` writes. `--lineSoft` is `--line` at half strength
      over the ground in **both** palettes (0.49–0.56 per channel), so it is a derivation.
      `panel` and `sidebar` are two colours: the panel is lighter than `face` in light and the
      darkest surface in dark, which no fixed shade of either produces. `--panelFg` is used nowhere;
      `--accentInk` only on surfaces this refresh does not build.
- [x] **Colours may carry alpha — not built, because nothing needs it.** The only alpha colours
      were the two washes, and they are derived. Eight-digit hex would have been parsing for a
      value no file should hold.
- [x] **`Theme::dark()` beside `Theme::light()`**, both `const fn`, and **`scheme = "light" |
      "dark"`** naming which one a file starts from — read first wherever it sits, so a line above
      it is not thrown away. The staged `theme.toml` now shows every value but writes only its
      deliberate overrides live; thirty live colour lines would have pinned the desktop to light
      whatever `scheme` said. **Two departures from the design, each tested**: the dark scheme's
      `deny` is `#D46F63` (the design's is 2.9:1 on its own dark ground, and `deny` is drawn as
      text here), and `title_active` is the accent washed over the face (the divergence below). A
      test holds every colour drawn as text to 4.5:1 (7:1 for body text) in both schemes.
- [x] **`blend_rect` in `libdraw`**, and `fill_rounded_rect`.
- [x] **Rounded rectangles** — `fill_rounded_rect`, through one integer curve
      (`libdraw::corner`) shared with the masked blit, so the toolkit's corners and the
      compositor's cannot disagree by a crescent. **The radius is 8**, the design's default, as a
      physical size (see the metrics item); the constant lands with its first user in Part B.
- [x] **A masked blit** on the compositor's row path: a rounded surface is three bands, the
      middle one the `memcpy` it always was, each corner row a shorter span down the same path, and
      only the pixels on the curve blended. **"`covers` minus the corners" turned out to be
      required, not an optimisation**: cut the whole rectangle and the corners keep whatever the
      framebuffer last held. The rounded `compose`/`compose_exposed` equivalence test over a stale
      framebuffer fails without it.
- [x] **A two-layer shadow**, and the design's numbers taken as they are — `tune` compared them
      against the shipped layer and two scaled pairs, and nothing argued for moving off them. **It
      cost a boot first**: the pair measured 2.0× the single layer, and `test-qemu`'s idle check
      failed. The shadow loop visited every pixel of a window's overlap with the damage to reject
      the covered ones; skipping them as a span, and blending with one pixel lookup instead of two,
      took the shipped single layer from 1.39 to 0.71 ms and the pair to 1.51 ms on the host, with
      byte-identical output (a test holds the fast loop to the old one).
- [x] **Both schemes are covered: the host renders both and a boot proves one**, since a scheme is
      data. `preview` draws `ui-dark` beside `ui`, and says it is the one frame no gate compares
      against. **The compositor learns the scheme from the manager's `SetScheme`** (`0x0928`), which
      the shell sends on every session start, light included, so every graphical boot runs the path;
      `check-login` asserts the compositor's `scheme light` line against the staged theme. It
      decides one thing today — how dark the shadow is — because the cursor, the drag outline and the
      desktop's ground are the same in both schemes.
- [x] **Which metrics are absolute and which relative — decided, and the type needed no change.**
      **Absolute, because a CSS pixel is this machine's pixel**: CSS defines one as 1/96 inch and
      the laptop's panel is about 100 per inch (1366 across 15.6 in), so the design's pixel metrics
      are physical sizes and transfer as written — a 30 px panel, a 31 px title bar, 8 px corners,
      the design's shadow. **Relative: whatever was composed against the 1440×900 canvas** —
      default window sizes and positions, the overview's cards — which becomes a fraction of the
      work area, because 24% less screen means less fits, not smaller things. **The type is already
      the design's**: `font_px` is `ab_glyph`'s ascent-to-descent height, 1.164 em for DejaVu Sans,
      so the staged theme's 14 is a 12.0 px em — and measured the same way off the design's own 1:1
      screenshot, its `n` is 7 px tall and DejaVu's at 14 is 7 px (at the built-in 16, 8). The
      built-in stays 16, because `check-login`'s proof that the theme reaches a window needs the
      staged value to differ from it; a session with no theme file draws text one step larger than
      the design, and that divergence is named rather than accidental.
- [x] **`widget-toolkit.md` stops naming a `Theme::dark()` as the fallback** — and so does
      `Theme::from_config`'s own doc, which said the same thing and had not been found.

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
