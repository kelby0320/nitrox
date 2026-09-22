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
`ENTRY_W`, `TITLE_BAR_H` and the greeter's size with a comment on each saying why: *"a gate
that read the shell's layout to know where to aim could agree with a shell that had stopped
drawing where it says"*. **A copy is not an excuse for an unmeasured number, and it needs a test
of its own**: the greeter's `340×141` was `420×200`, and since Part D its height is what the card
measures in a host test *and* `xtask`'s copy is compared against the greeter's source by a second
one — the first without the second leaves exactly the drift the copy was supposed to survive.

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
- [x] **Part B — window chrome**: rounded corners, the new titlebar, focus that reads, and menus
      in the design's style.
- [x] **Part C — the panels**: the Applications and Places menus, and a bottom bar with a
      show-desktop button and the new switcher.
- [x] **Part D — the greeter**, which the design does not cover and which has to match anyway.
- [ ] **Part E — the overview**, whose layout changes.
- [ ] **Part F — a terminal with colour**, which is `nxsh` using a mechanism `libterm` already
      has.
- [x] **Part G — type, in DejaVu**: a smaller body size, a size scale, and weight only if it earns it.
- [x] **Part H — the parts of a window**: title and subtitle, the focus border, the tab strip, the
      status bar, fields and buttons — shared by all three applications.
- [x] **Part I — the file browser**: toolbar, columns, sidebar, status bar.
- [x] **Part J — the editor**: monospace text, a gutter, Save on the tab strip, a status bar.
- [x] **Part K — the terminal's window**: line spacing, the scrollbar, the working directory.

G–K were added on 2026-09-18, after measuring the applications against the page — see [How far
the applications are from the design](#how-far-the-applications-are-from-the-design--measured-2026-09-18)
and the revised [running order](#running-order-revised-2026-09-18).

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

- [x] **Rounded corners** (2026-09-18): the compositor cuts every role that floats — normal,
      popup, dialog, the ones that already cast a shadow — to `WINDOW_RADIUS` (8); panels stay
      square, and so does any window covering the whole screen — the overview, whose cut corners
      showed the undimmed bars beneath it until the PR #313 review measured them. The window's own border is a `Node::Outline` along the same curve, painted last and
      blended at `corner::border_share` so the curve is not faded twice. **It is never a hit-test
      target** — painted last and full-size, the first version took every press in every framed
      window. `check-display` composes its expectation through `libdraw::compose` and leaves out
      only the bottom reference window's corner squares, where the self-test image's `nxterm`
      shows through; a square expectation fails it at the scene's corner, so it sees the rounding.
- [x] **The titlebar**, restyled to the design's source: 31 pixels with a rule along the bottom,
      the title 11 in and centred down it, the controls borderless in `foreground_dim`, 23 wide,
      9 apart, 5 from the border. **On `title_active`/`title_inactive`, not `panel`** — this line
      said `panel`, which was read off a screenshot; the page's title bar is `--face`. A window's
      content runs flush to its border (`WINDOW_FRAME` 3 → 0), which took away `nxterm`'s only
      margin — its text touched the window's edge — so its pane has the design's own, 9 by 11, in
      the terminal's ground. The menu bar takes `face_hover` and
      a `--lineSoft` rule, **inside the 24 pixels** three applications carry — the design's is 25
      with its rule, a pixel of divergence rather than a pixel of every aim moving.
- [x] **Focus reads at a glance** — the accent-tinted `title_active` from Part A, drawn.
- [x] **Menus in the design's style**, as far as window menus have the design's parts: on the
      window's ground, rows padded 6/12 with 5 above and below, edge-to-edge `--lineSoft` rules, the
      chord column and disabled rows in `foreground_dim` (closing a gap the toolkit doc recorded),
      and `Item::destructive` in `deny` — the file browser's `Delete`. **Section headers are
      Part C's**: no window menu has sections, and the Applications menu, which the design gives
      them, is not a window menu. Selection and hover everywhere are `Node::Wash` — the accent at
      20%, and at 10% (18% dark) — replacing a ring round a bevelled fill.
- [ ] **Not built: a hover state on the title bar's controls.** The design lightens a control
      under the pointer (`--faceLo`) and reddens close (`--deny`); `title_bar` takes no hover, and
      giving it one means threading the router's hovered key through three applications' title
      bars. A change of its own, and named here so it is not assumed done.

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

- [x] **The mask, on the row path** — the primitive itself, and the only way any of this happens.
- [x] **Skip the corner pixels and let the stack fill them.** Surfaces are painted in stack order,
      so a skipped pixel keeps whatever lower surface already painted there — the desktop where
      nothing is behind, the window below where something is. This is what the first draft's
      "option 2" should have said; it is wrong only if the compositor paints corners with the
      desktop colour, which is a mistake rather than a design.
- [x] **`covers` minus the corners**, as the optimisation that *accompanies* the mask rather than
      an alternative to it: a masked surface no longer covers its full rectangle, so the
      background beneath the corners must be filled. The subtraction machinery (`cut`) exists.

All three were built in Part A and switched on in Part B. **The third turned out to be required,
not an optimisation**: a rounded window still counted as covering its whole rectangle leaves its
corners holding whatever the framebuffer last held.

**The option not to take, and its real cost.** Giving windows an alpha format with transparent
corners would work, and the first draft priced it against M13 Part A's double-write measurement —
which is the wrong number. That measurement found removing half the pixel writes bought **6%**,
because *"the writes were never the cost"*. The cost is the per-pixel path: an alpha surface goes
through `blit_blended` instead of the `memcpy`, and the `memcpy` fast path is what made compose
about **5× faster** (4.54 → 0.90 ms under KVM, 2026-09-03). Alpha windows would pay that on every
pixel of every window, not just at the corners. So the option is worse than the first draft said,
for a different reason.

## Part C — the panels

- [x] **The Applications menu becomes a menu**, not a modal — sections, rows with icons, a
      separator. **No categories**: the design groups three programs under "Accessories" and a
      System section, and a taxonomy invented for three entries is one we would have to live
      with. A flat list until there are enough entries to need grouping (maintainer, 2026-09-17).
      Built in C.2 (2026-09-18): `libui`'s `MenuState` and popup, hanging from the word, on the
      window's ground with the design's rows. **No separator**, since with no sections and the
      two System rows omitted there is nothing to separate.
- [x] **…but typing still narrows it.** Today `Super+A` opens a modal and typing filters; on the
      laptop that is the fastest path to a program and the only one that does not need a pointer.
      Adopting the menu's *look* must not cost the menu's *behaviour* — this is the one place the
      design would be a regression if followed literally. A filter field above the rows
      (`menu::popup_headed`); the menu resizes to what matches, the top match is lit
      (`MenuState::select_first`) and Enter launches it. One change: Enter with nothing typed
      closes rather than launching the first entry, the rule every chord-opened menu keeps.
- [x] **A Places menu**: `Home`, `Documents`, `Downloads`, `Pictures`, `Root`, with a swatch, a
      label and a dim right-aligned path. **Trash is dropped**, because there is no trash. A place
      opens `nxfiles` at it — `argv[1]`, which the browser learned in C.1 — and `check-login`'s
      step 13 opens `Documents` and reads the listing back.
- [x] **The list has one source, which means moving it.** `DEFAULT_FOLDERS` is only `Documents`,
      `Downloads` and `Pictures`; `Home` first and `Root` last are added by `Browser::places()`, a
      method on the file browser that `desktop-shell` cannot call (review). Either that function
      moves somewhere both can reach, or the menu is a second copy of the list — and a second copy
      is what the first draft claimed it was not. Moved: `libfs::places` and `HOME_FOLDERS` (C.1).
- [x] **`Run Application…` and `End session` are omitted**, not disabled: the launcher is
      deferred and there is no logout. A menu item that does nothing is worse than an absent one.
- [ ] **Not built: icons on the Applications menu's rows.** The design's are three CSS boxes keyed
      by program, and an icon is an asset question — a format, where the files live beside a
      desktop entry, who draws them — rather than a drawing one. Three glyphs keyed by name would
      be the thing that has to be removed when that is answered. Named here so it is not assumed
      done; the Places menu's swatches are built, because a swatch *is* a colour.
- [x] **The bottom bar gains a show-desktop button** on the left: minimise everything, press
      again to restore. **The restore set belongs to the shell**, which already tracks
      `WinEntry.minimized` — minimising is a manager operation, and the compositor's "whole part
      is to check the caller owns the window and hand the manager the question" (M9 Part B). The
      first draft put this state in the compositor, which is the process the architecture keeps
      policy-free (review, finding 6). Built in C.3 (2026-09-18): the restore set is exactly what
      the press minimised, bounded by the windows the bar shows, and it lets go when anything on
      that desktop comes back another way — so the button's light is always true.
- [x] **The switcher moves to the bottom right**: `‹`, up to three squares, `›`, the current
      desktop's name. The rule is `min(3, total)` — **not** "two when there is no previous
      desktop", which is what the first draft read off a screenshot with two desktops open
      (review, finding 7). On desktop 1 of three or more it shows three cells with the back
      arrow disabled. `panel::switcher_cells`, host-tested at every position up to seven desktops.
      **One divergence**: an empty desktop's cell has a faint border where the design dashes it —
      the missing mark already says "empty", and a dashed rectangle would be a primitive for one
      border.
- [x] **`desktop-shell.md` §7 is updated in the same change.** It records a *compact indicator*
      as the decision and rejects "GNOME 2's full desktop switcher", on the grounds that a row of
      boxes churns as desktops come and go and "a name is also a better use of the space". The
      design answers the churn objection — three cells is bounded — and keeps the name beside
      them. That is a decision being revisited on new evidence, which is fine, and leaving §7
      saying the opposite is not. §7a records the old decision and why it no longer holds.
- [x] **The window list stays** and is restyled: 186-pixel buttons with the design's state dot,
      the focused one a raised face, and how many fit measured against the switcher beside them
      (`panel::task_capacity`), since the switcher carries the desktop's name.

## Part D — the greeter

**The design never drew one**, so this part reads the design's *language* off the surfaces it did
draw — the card, the field, the dim second line — rather than transcribing a picture. The greeter
is `desktop-session-mgr`'s own window, and `check-login` asserts its size and where it centres, so
those numbers move with the restyle and the gate moves with them. Named because a gate whose
expectation changes in the same commit as the code is the shape that needs a reason in the message.

- [x] **D.1 — a library beside the binary.** `desktop-session-mgr` is the last of the six to be
      one file: `init`, `service-mgr`, `nxterm`, `nxfiles`, `nxedit` and `desktop-shell` all keep
      their pure half in a lib so the host can test it. The greeter's state, its key handling and
      its view are pure and today are untested, and the size `check-login` writes down is a
      literal nothing checks. A `#![cfg_attr(not(test), no_std)]` lib fixes both.
      **Built.** `desktop-session-mgr` now has a lib beside its binary, holding `Greeter`, its
      keys, `view` and `render`, plus `greeter_theme()` — one function, so the theme the binary
      resolves a font from and the theme the view is painted with cannot differ. `cargo xtask
      test` runs its tests.
- [x] **D.2 — a field with an edge.** The design's field is a white ground inside a one-pixel
      `border` rounded to the control radius. Ours is a flat fill of `track`, which is `--bg` in
      the light scheme — so on the greeter's white card **an unfocused field is invisible**, and
      that is what the refresh's own smaller type made plain. In `libui::widget::text_field`, so
      every field follows: the chooser's, the browser's search and the shell's filter. This is
      part of what Part H lists as "field, icon-button and accent-pill styles"; H keeps the rest.
      **Built.** A resting field is the design's: the well inside a one-pixel `border`, rounded to
      `CONTROL_RADIUS` — public now, and the shell's bar dropped its own second copy of the 8.
      Focus keeps its two-pixel accent ring, so a state still reads as a state.
      `a_resting_field_has_an_edge_against_the_ground_it_sits_on` fails against the flat fill it
      replaced. **Every field got the rounding; only the greeter's is ever drawn at rest** — every
      other caller passes `active: true` (PR #318 review, optional 3). So the edge waits for the
      first field that is not the focused one, which Part I's search is.
- [x] **D.3 — the card.** A bold title, labels in `foreground_dim`, a refusal in `deny` rather
      than in body ink, the rhythm of spacing the design uses, and the one-pixel rounded edge
      every other surface in this system now has. **The copy does not change**: what a greeter
      should say is not a question this part is answering.
      **Built.** The card is `popup_frame` — the same edge and curve as a menu — with the heading
      bold a step up, labels in `foreground_dim`, and a refusal in `deny`. **The refusal's line is
      always there and empty until there is one**: pushed in when it happened, it moved both
      fields down the card as it appeared and back on the next keystroke.
- [x] **D.4 — the gate follows, and a host test goes first.** The card is sized to its content,
      so `GREETER_W`×`GREETER_H` moves and `check-login`'s copy of it moves too. D.1's lib is what
      makes that safe: the size is measured in a host test, so the two numbers cannot drift
      without something failing on the host first.
      **Built.** 420×200 became **340×141**, and the height is measured:
      `the_card_is_exactly_the_window_it_is_drawn_in` pins it, and asserts a refusal does not
      change it. `check-login` types and never clicks the greeter, so no aim moved with it.
      **The gate's copy needed a test of its own**, which this box first thought it did not: the
      greeter's test compares the card with the *greeter's* constant, so the two could still drift
      (PR #318 review, finding 1). `the_gates_greeter_size_is_the_greeters_own` compares `xtask`'s
      copy against the greeter's source, as `abi-sync-check` does for the ABI.

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

## How far the applications are from the design — measured 2026-09-18

**Parts A–F restyle the chrome around the applications and almost nothing inside them**, and after
Part C that is where the distance to the design is. The maintainer, comparing the two: *"we aren't
close enough to the nitrox shell.html reference"*. The parts below are that distance, written
down.

**A north star, not an overlay** (maintainer, 2026-09-18). A screendump and the page are not
expected to lie on top of each other, and no time goes into moving things a pixel or two to make
them. What should match is the **structure** — which parts a window has, in what order, how they
are grouped — the **colours**, and the **rhythm** of the spacing: that a toolbar breathes and a
status bar is tight. The numbers below are proportions to aim at, not targets to hit; our face is
not the page's (Part G), so text-bound measurements could not match exactly anyway.

**Measured, not read.** Until Part C this plan took its numbers from the page's markup, which
worked for the bars and gets error-prone inside a window. `docs/design/nitrox-shell/drive.mjs` now
runs the page in headless Chrome and writes every element of a window with the rectangle, colours,
font, padding, radius and border the browser computed; `cargo xtask shot` gained `terminal`,
`files` and `editor` pictures of our three windows in the same states the page draws them. Every
number below comes from one or the other.

**What the design has and we do not, per window** (the page's values, at its 1440×900):

| | The design | Ours |
|---|---|---|
| **Every window** | a bold 12 px title with a dim subtitle (`Files  /home`, `nxterm  ~ /home`); the focused window's 1 px border in the accent | a regular-weight title; the border always `border` |
| **Type** | IBM Plex Sans and Mono, 10–12.5 px, weights 400/500/600 | DejaVu Sans and Mono, one size (14 staged), one weight |
| **Tab strip** (terminal, editor) | 30 px on `--faceHi`; rounded-top tabs, the active one on the window's ground and open to it, `×` in 10 px dim, a `+`; a slot on the right | a grey strip with square tabs; no `+` |
| **Files: toolbar** | a 39 px row: a 24 px rounded `↑`, a breadcrumb path field (mono, `/` in `--line`), a 120 px Search field | `^` and the path as plain text |
| **Files: listing** | a header row (Name / Size / Kind / Modified, 10.5 px dim on `--faceHi`); 25 px rows with an 11×9 swatch (folders accent, files `--line`), size, kind and time in dim, mono where numeric | names only |
| **Files: sidebar** | 132 px on `--sidebar` against the window's edge, 5 px dots (`ok`; Root `deny`), selection the `--sel` wash; `disk0 · 18.4 GB free` at its foot | inset with a margin, no dots |
| **Files: status bar** | `6 items │ Documents/ selected`, 25 px, mono 10.5 dim on `--faceHi` | none |
| **Files: tabs** | none shown | a tab strip even with one tab |
| **Editor** | monospace text on 20 px lines; a 36 px line-number gutter; the byte count and an accent **Save** pill on the tab strip; a status bar (`opened — 848 bytes` · `toml · ln 6, col 14`) | proportional text; no gutter; a bevelled `save` and the status on a separate row at the top |
| **Terminal** | 12 px text on 19.5 px rows; no visible scrollbar | rows at the font's own line height; a scrollbar always drawn |
| **Menus** | `Go` (Files), `View` (editor), `Terminal` (terminal) — **labels that open nothing** | none of the three |

**Two things the page shows that we cannot honestly build yet.** The sidebar's free-space
readout needs a filesystem operation that does not exist — no `statfs`-like request reaches a
filesystem server today — and the Search field is a placeholder in the page, a `div` with a word
in it. Neither is faked: the readout waits for the operation, and Search is built as a real filter
or left out (Part I).

**And one thing we can build that we did not know we could.** A directory entry already carries
its size and modification time (`librsproto::file::OwnedEntry`), so the columns need no protocol
change — only a renderer.

## Part G — type, in DejaVu

**The single most visible difference in every window.** The page sets everything in IBM Plex Sans
and Mono at 10–12.5 px, in three weights; we draw everything in DejaVu at one size and one weight.
**The same words are 35% wider in ours** — `Documents` is 59 px in the page's Plex at 11.5 and 80
in our DejaVu at the staged 14, and five names measured all fall at 1.32–1.35 — and the design's
hierarchy (a semibold title, medium tab labels, dim 10.5 px metadata) is carried almost entirely by
size and weight, where we have one of each.

**DejaVu stays** (maintainer, 2026-09-18). Plex was considered — it is under the SIL Open Font
License and would ship as freely — and not taken, to keep the image and the build as they are.
**Most of the gap is size rather than face**, which is what makes that cheap:

| DejaVu Sans at | 14 (staged today) | 13 | 12 | 11.5 | 11 |
|---|---|---|---|---|---|
| width against the page's Plex 11.5 | 1.35× | 1.25× | 1.15× | 1.11× | 1.06× |

- [x] **A smaller body size**, chosen on a screendump rather than from the table: DejaVu's large
      x-height reads bigger than its pixel size, so 12 may read like the page's 11.5. The staged
      `font_px` and the built-in default move together.
      **Built (2026-09-18): 12 staged, 13 built in** — 14 and 16 before. Beside the page on a
      `shot`, 12 reads as the page's 11.5 does and 13 reads a size larger. The default stays one
      step apart so that `check-login` can tell the file reached a client. The terminal's grid is
      sized from the same number, so at 12 it is also the page's 12 px terminal. **The greeter
      draws at 13**: it runs before there is a user and so before a `theme.toml`, which is
      Part D's to look at.
- [x] **A size scale instead of one size.** The page uses 12.5 (top bar), 12 (titles, the
      terminal), 11.5 (menus, rows, fields), 10.5 (headers, status bars, byte counts) and 10
      (hints). The theme keeps one number — `font_px`, the body size — and the toolkit derives the
      others from it, so a theme still sets one value and the steps between them are ours. Status
      bars and column headers are where a smaller step shows most.
      **Built: three steps, `TextSize::{Small, Body, Large}`** — ⅞, 1 and 13⁄12 of `font_px`,
      which is 10.5, 12 and 13 at the staged size — set by an inherited `scaled` wrapper that,
      unlike `ink`, changes what the text inside measures (widget-toolkit §5). Used where the page
      steps today: `Large` for the top bar's two words, and `Small` for a menu's shortcut column
      and the editor's status line. Column headers and the other status bars are Parts H–J's,
      which build them.
- [x] **Weight, if a screendump asks for it.** The title is the one place the page leans on it.
      DejaVu Sans Bold is the same family under the same licence, so it would ship beside
      `DejaVuSans.ttf` with nothing new to decide; if the title reads well enough in the dim-and-
      size hierarchy alone, no second face.
      **It asked, and titles are bold at the body size.** At 2× beside the page, a regular title
      set a step larger did not anchor the window, and an overstruck bold smudged the counters.
      So `DejaVuSans-Bold.ttf` ships beside the regular face, attached as its bold by `load_ui`,
      and staged and rendered on the host from the same path. Set at `Large` as well, it
      overshot, because DejaVu's bold is already heavier than the page's semibold. Measured
      against the menu's `File`, the title is 1.62× its width at the body size and 1.75× a step
      up, where the page's is 1.47×.
- [x] **Every aim moves, and each is pinned first.** Part C's arrangement — the gates' literals
      pinned by host tests at the staged size and the built-in one — is what makes this change
      reviewable rather than a boot-by-boot hunt.
      **Done.** The shell's pinned sizes moved to 12 and 13 before any aim did. Then each literal
      moved into the overlap of the two sizes' targets, found by a host probe rather than by
      booting: the Places press, the Applications menu's filtered row, the first Places row, and
      the workspace switcher's arrow and first cell. The Applications press stayed where it was.
- [x] **Why it comes first**: every part after it measures against text. Doing type after them
      would measure everything twice, the greeter included.

## Part H — the parts of a window

Toolkit pieces the three applications share, so each is built once and restyled once.

- [x] **Title and subtitle**: the title — bold if Part G adds the face — then a dim subtitle:
      the directory for Files, the working directory for a terminal (which needs the shell to
      tell its terminal where it is), and for the editor the design's single
      `theme.toml — Text Editor`.
      **Built.** `title_bar` takes an optional subtitle, set in `foreground_dim` beside the bold
      title. The browser shows its directory and the editor `Text Editor` after the file's name,
      which is the design's `theme.toml — Text Editor` in two parts. The terminal's would be its
      shell's working directory and nothing can tell it that yet, so it passes `None` and Part K
      keeps the box.
- [x] **The focused window's border in the accent**, the design's own focus cue. We keep the
      tinted title bar as well (the deliberate divergence above); this is additive and cheap,
      since a client knows its own focus and draws its own `Outline`.
      **Built.** `window_frame` and `window_frame_with_grip` take `focused`, and the edge is
      `accent` when it is set and `border` when it is not — the design draws exactly this, its
      editor edged in the accent and the two windows behind it in the line colour. Additive, as
      planned: the tinted title bar stays.
- [x] **The tab strip**: 30 px on `face_hover`, a `border` rule beneath, padding 5/6/0, tabs 24 px
      with 8 px top corners, 9 px sides, 8 px from label to `×`, 1 px between tabs; the active tab
      on the window's ground with its border open at the bottom, inactive tabs dim and flat; a
      22×24 `+`; a right-hand slot for a window's own controls.
      **Built**, to those numbers, with the tabs rounded at the top by two layers because the
      toolkit has no per-corner radius. The `+` is wired to the new-tab message all three
      applications already had. The right-hand slot exists and is empty until Part J puts the
      editor's byte count and Save in it.
- [x] **The status bar**: 25 px on `face_hover`, a `--lineSoft` rule above, 10.5 px mono in
      `foreground_dim`, 11 px sides, a left and a right slot, separators in `border`.
      **Built** as `status_bar`, with `status_text` and `status_separator` beside it. **Not in the
      mono face**: a window is painted with one face, and the fixed-advance one belongs to a
      character grid — the hierarchy comes from the size step and the dim ink instead. The
      editor's strip is this widget now, still under the chrome; Part J moves it to the foot,
      which is what the `rule` argument is for.
- [x] **Field, icon button and pill.** A field is 24 px with 8 px corners, a `border` line and 8 px
      sides, on the window's ground or on `face_hover`; an icon button is a 24×24 of the same; a
      primary action is an accent pill — accent ground, white 500-weight label, 12 px sides.

      **Built.** The field's sides are the design's 8, `button` is rounded to `CONTROL_RADIUS` —
      which is what makes an icon button a 24×24 of the same, since both applications' up-arrows
      are buttons with a glyph on them — and `pill` is new: the accent as a ground with a light
      ink on it, which the editor's `Save` is now. **Its ink is chosen rather than fixed** — the
      further of the surface's two by weighted brightness, since the dark scheme's paper is
      near-black on that accent — and its focus ring is a band of that ink, where the first
      version drew the ground over the ground and put down no different pixel (PR #319 review).

## Part I — the file browser

- [x] **The toolbar row** (39 px with its rule; padding 7/9; gap 6): the `↑` icon button, the path
      field as breadcrumb segments — mono, dim segments, `/` in `border` — and the Search field.
      **Partly built.** The `↑` is a button with a glyph, rounded by Part H; the path is
      breadcrumb segments with the separators in `border` and the names in the body ink. **Not the
      design's 39-pixel row**, and not mono: the strip is still `PATH_H`, because its three modes
      — the path, the location field and a rename — share a height, and re-cutting that is a
      bigger change than this part needs.
- [x] **Search filters or is absent.** The page's is a placeholder. The honest version filters the
      current listing as you type, by the rule the Applications menu already uses — and a field
      that looked like search and did nothing would be the defect every earlier part has named.
      **Built as a filter.** `Ctrl+F` (View ▸ Search…) opens a field in the toolbar; typing
      narrows the listing by the Applications menu's rule, case-insensitive and anywhere in the
      name; `Esc` closes it and `Enter` opens what is left. **It is absent until opened**, which
      is the honest reading of "or is absent" — a field sitting there doing nothing was the
      alternative this box ruled out. Rows keep their index into the whole listing, so a filtered
      row opens the file it names.
- [x] **Columns**: a 25 px header row (Name flexible, Size 70 right-aligned, Kind 60, Modified 96;
      10.5 px medium dim on `face_hover`, a `--lineSoft` rule); rows of 25 px, padding 5/11, gap 8.
      Size human-readable (`848`, `11K`, `2.8M`), Kind `dir` or the extension, Modified `HH:MM`
      today and a date otherwise — `libtime`, which already formats the clock. Whether a header
      click sorts is decided here: the View menu already holds the orders.
      **Built**, in the toolkit rather than in the browser: a `ListRow` carries cells and
      `list_view` takes the widths. `Size` is `4096`/`2.8M` right-aligned, `Kind` is `dir` or the
      extension, `Modified` is `HH:MM` today and a date before it — from `libtime`, with the clock
      read by the binary per listing so the view stays a function of values. **A header click does
      not sort**; the View menu's four orders are the answer for now.
- [x] **The row swatch**: 11×9, 1 px corners — the accent for a folder, `border` for a file.
      **Built**: 11×9 with a one-pixel corner, the accent for a folder and `border` for a file.
      The shape is a `Swatch`, so the sidebar's dot is the same field at another size.
- [x] **The sidebar**: 132 px on `sidebar` against the window's edge rather than inset, rows of 25
      px with a 5 px dot (`ok`; Root `deny`) and the selection wash.
      **Built**: 132 px, against the window's edge (the margin was 6 and is now nought), with a
      5-pixel dot per place — `ok`, and `deny` for Root, which is the only warning before stepping
      out of a home directory. The selection wash was already there.
- [x] **The status bar**: `N items`, and the selection.
      **Built**: `N items`, and after a separator what is selected.
- [x] **The tab strip only when there are two tabs.** The design shows none; tabs are ours, and a
      strip holding one tab is chrome with nothing to switch between.
      **Built**, and it had to be *left out* rather than sized to nothing: a zero-height box still
      paints its children, so the tab and the `+` drew over the chrome below it. The screendump
      caught that; the test that now catches it counts ink in the band rather than asserting a
      height.
- [ ] **Not built: the free-space readout**, until a filesystem operation reports free space.
      Trigger: that operation, which the administration phase's disk tools will want anyway.

## Part J — the editor

- [x] **Monospace text** in `font_mono`, on 20 px lines at the body size. Code in a proportional
      face is the largest single difference in this window.
      **Built**, and it needed the toolkit to reach a second face: `Font` carries a mono companion
      beside its bold, `Node::Mono` threads `TextStyle.mono` through measure, arrange, diff and
      paint, and the face is chosen fixed-advance first with the weight within it. The chrome
      around the buffer stays proportional. **The caret had to move with it** — turning a pixel
      into a column means measuring in the face the text is *drawn* in, and `BUFFER_STYLE` names
      that pair so the two cannot drift.
- [x] **A line-number gutter**: 36 px on `face_hover` with a `--lineSoft` rule, numbers right-aligned
      8 px from it in dim mono, text starting 12 px after.
      **Built**: 36 px on `face_hover` with a rule down its right edge, numbers right-aligned
      clear of it in the dim step of the same fixed-advance face. **A sibling of the document, not
      a wrapper round it** — the router hands a widget pointer events in its own coordinates, so a
      gutter inside the area would have shifted every column by its width.
- [x] **The tab strip carries the file's controls**: the byte count in dim mono and an accent
      **Save** pill in its right-hand slot, replacing the separate `save` row.
      **Built**: the byte count in dim mono and the accent `Save` pill in the strip's right-hand
      slot, which Part H put there for this. The separate save row is gone.
- [x] **A status bar at the foot**: the message on the left, `language · ln N, col M` on the right.
      **Built**, using Part H's widget with its rule facing the content: the message on the left,
      `toml · line 1, col 1` on the right in mono so the numbers do not shuffle the line about as
      the caret moves.
- [x] **Syntax colours, decided rather than copied.** The page colours TOML's keys in the accent —
      a kind this scanner does not have (Part A recorded it) — strings in a red and numbers in
      `ok`. Adding a key kind is a scanner change; whether the six colours move to the design's
      is a palette decision.

      **Decided, and one half built.** The scanner gained the `Key` kind the design colours and
      this system did not have: the plain name at the head of a line, before the `=` a language
      declares in its table — `None` for every language whose `=` is an operator. **The six
      colours stay ours**: the design paints a TOML key in its accent, and ours is the focus ring
      and what a selection is made from, so a third meaning for it is how a person stops being
      able to read either. Whether the palette moves to the design's is still the open decision
      Part A recorded.

**Named by Part J's review and not fixed there**: `text_area` inserts the caret as a two-pixel
element *in* the row, so the line the caret is on is drawn two pixels right of every other line
(`libui::widget`, the caret is a `Row` split at the cursor). In a proportional buffer nobody could
tell; in a fixed-advance one "columns that line up" is the stated point of the part. Fixing it
means drawing the caret as an overlay rather than as a sibling of the text, which is a change to
a widget three applications use — so it is written down here rather than bundled into the part
that noticed it.

## Part K — the terminal's window

Beside Part F, which owns the terminal's colours.

- [x] **The tab strip**, from Part H. **Already built** — `nxterm` took `libui::widget::tab_strip`
      with the `+` in Part H along with its two siblings, and nothing here is the terminal's own.
      Kept as a box because it is part of what "the terminal's window" means, and a part nobody
      would otherwise confirm.
- [x] **Line spacing**: the design's 12 px text sits on 19.5 px rows, 1.6 times its size. Our row
      is the font's own line height (`libterm::render`'s `cell_h`), so a terminal reads cramped
      beside everything else. A leading on the cell, with the grid's reflow unchanged.

      **Built** as `render::LINE_HEIGHT`, applied to the text's *size* the way a typographer sets
      leading and then held to at least the face's own line height, so no face is given rows its
      glyphs do not fit in. The extra room is split above and below the glyphs rather than piling
      under them — a line hard against the top of a tall cell reads as a line with a gap after it.
      **Nothing pinned `cell_h` before this**: all 131 of `libterm`'s tests passed unchanged with
      the leading in, which is what the new assertions in `the_metrics_are_the_faces` fix.
- [x] **The scrollbar**: the page shows none; ours is always drawn. Hidden until scrolled, or
      restyled — decided by looking at both on a screendump.

      **Hidden**, and the screendump decided it: always drawn, the bar was a solid 12 px strip of
      `--line` down the right of the near-black pane, so the loudest thing in an empty terminal
      was a control that did nothing. **The column is kept and filled with the terminal's own
      ground** rather than removed — taking it out of `CHROME_W` would widen the grid, and the
      grid would reflow the first time output ran off the top, which is the tab strip's argument
      one widget over.
- [x] **The working directory in the title** — the subtitle from Part H, which needs the shell to
      tell its terminal where it is.

      **Built**, as `OSC 7`: `nxsh` writes `ESC ] 7 ; <path> BEL` beside every prompt (in
      `repl::prompt`, so the announcement and the prompt cannot drift), `libterm::parse` reads it
      and `nxterm` puts it in the title bar, per tab. **A path, not the `file://` URL the
      sequence conventionally carries** — Nitrox has no hosts and no URL type, and half-reading a
      URL is worse than not claiming to read one. Three crates whose host tests cannot see each
      other, so `cargo xtask check-terminal` asserts the round trip in a boot, including that it
      follows a `cd`. **The kernel's framebuffer console learned to swallow a string sequence in
      the same change** — it read `ESC ]` as a two-byte escape and would have drawn `7;/home`
      into the boot log of a machine with no serial port.

## Menus the design names and does not fill

`Go`, `View` and `Terminal` are words on the page's menu bars that open nothing. **A menu is added
only with things in it** — the rule Part C applied to `Run Application…`: an empty menu is worse
than none. Each application's part decides whether it has the items (`Go`: up, home, the places;
`View`: the gutter, wrapping; `Terminal`: new tab, clear) and adds the word only then.

## Running order, revised 2026-09-18

**Proposed** — the maintainer decides. **G first**, because type moves every metric the others
measure; then **D** (the greeter), then **H** before **I**, **J** and **K**, which are built from
it; then **F** beside **K**, and **E** (the overview) last, since it is the one surface a person
visits rather than works in.

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
  **Answered 2026-09-18 by measuring**: six parts restyled the chrome and left the applications'
  interiors as they were, so G–K were added and the running order revised.
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
