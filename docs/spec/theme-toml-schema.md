# Nitrox: Theme file schema (`theme.toml`)

The colours and text size a graphical session draws itself in. Read once by
[`desktop-shell`](../../userspace/desktop-shell) at session start and handed to every
application it launches; see [`display-arm-plan.md`](../planning/display-arm-plan.md) Milestone 11
Part C.

**Built as of M11 Part D; two schemes and the design's palette since the desktop refresh's Part A**
(2026-09-18, [`desktop-refresh.md`](../planning/desktop-refresh.md)). What is not here yet: any way
to change a theme without restarting what draws with it. That is deliberate — see "Not a live
protocol" below.

## Where it lives

`/home/theme.toml`, in the user's own subtree.

**Not `/etc`, and that is a namespace decision rather than a filing preference.** A session
namespace binds `/home`, `/bin`, `/dev/tty` and — for a graphical session — `/system/fonts`. It
has no `/etc`, and `session-mgr/CLAUDE.md` requires that adding a member be a design decision
each time. A theme is *a user's*, so the subtree the user already owns is where it belongs: no
new authority is needed to read it, and it is somewhere a person can actually edit and delete.

A system-wide default under `/etc`, merged beneath the user's, is the obvious next step and is
not built. **Trigger: a second user on one machine**, or a control panel that wants to offer
"reset to the system theme".

## The format

Flat `key = value` lines. Comments run from `#` to the end of the line, except inside the quotes
of a value — which matters, because every colour begins with `#`.

```toml
# The session's theme. Delete this file for the built-in one.
#
# `scheme` picks the built-in palette the rest of this file adjusts: "light" or
# "dark". Everything else is shown as that scheme has it, commented out; remove
# the `#` from a line to change it. Colours are "#RRGGBB"; font_px is a size
# in pixels per em.

scheme = "light"
# background = "#FFFFFF"
# foreground = "#16201F"
# foreground_dim = "#5B6766"
…
font_px = 12
# bevel = 0
…
wallpaper = "/home/wallpaper.png"
wallpaper_mode = "fill"
```

**That is the head of the file the image build ships**, elided where marked — a fragment of the
real thing rather than an illustration. **Every value the scheme already has is written commented
out**, and only the file's own choices are live: the scheme, and three values deliberately not the
default, each read back by a gate (`check-login` reads the `font_px` and the wallpaper). Until the
refresh the file wrote every line live, which was harmless while it restated the only palette there
was; with two, thirty live colour lines would pin a desktop to the light one whatever `scheme` said.
Copying a *stale* example and getting near-white widgets on a near-black ground is what this block
once did, because it still showed the dark theme M11 Part E replaced (PR #265 review, finding 6).

| Key | Type | What it colours | Design token |
|---|---|---|---|
| `scheme` | `"light"` or `"dark"` | Nothing itself: which palette every other key adjusts — see below | the page's `theme` |
| `background` | `"#RRGGBB"` | A window's ground (**not** the space between windows — see below), the current tab's face, and the ink on an accent pill | `--bg` |
| `foreground` | `"#RRGGBB"` | Text and other ink | `--fg` |
| `foreground_dim` | `"#RRGGBB"` | Ink read second: section headers, a path beside a name, a size | `--fgdim` |
| `face` | `"#RRGGBB"` | A button at rest | `--face` |
| `face_hover` | `"#RRGGBB"` | A button under the pointer, every application's menu bar (desktop refresh, Part B), and the ground of a tab strip and a status bar (Part H) | `--faceHi` |
| `face_pressed` | `"#RRGGBB"` | A button being held | `--faceLo` |
| `accent` | `"#RRGGBB"` | The focus ring and caret, what the selection is made from, **the edge of the focused window** and the ground of a primary action's pill (desktop refresh, Part H) | `--accent` |
| `track` | `"#RRGGBB"` | A list's ground | `--bg` |
| `groove` | `"#RRGGBB"` | A scrollbar's channel — **darker than `track`**, or the bar is invisible | `--faceLo` |
| `sidebar` | `"#RRGGBB"` | A panel beside content, such as the file browser's | `--sidebar` |
| `panel` | `"#RRGGBB"` | The desktop's own bars, top and bottom | `--panel` |
| `thumb` | `"#RRGGBB"` | A scrollbar's thumb | `--line` |
| `syntax_keyword` | `"#RRGGBB"` | A language's reserved words, and TOML's `true`/`false` | — |
| `syntax_string` | `"#RRGGBB"` | String literals, Markdown code spans and fenced blocks | — |
| `syntax_comment` | `"#RRGGBB"` | Comments, and Markdown block quotes | — |
| `syntax_number` | `"#RRGGBB"` | Numeric literals | — |
| `syntax_heading` | `"#RRGGBB"` | A Markdown heading, a TOML `[table]` header | — |
| `syntax_variable` | `"#RRGGBB"` | A shell variable — `$name` and `${name}` | — |
| `ok` | `"#RRGGBB"` | Something working: a running window's dot | `--ok` |
| `deny` | `"#RRGGBB"` | Something destructive: such a menu item, `Root` in Places | `--deny`, light only |
| `title_active` | `"#RRGGBB"` | A title bar whose window holds the keyboard | `--accent` over `--face` |
| `title_inactive` | `"#RRGGBB"` | A title bar whose window does not | `--face` |
| `cursor_body` | `"#RRGGBB"` | The pointer's fill | — |
| `cursor_outline` | `"#RRGGBB"` | The pointer's outline | — |
| `outline` | `"#RRGGBB"` | A resize outline, a snap preview, a drop target | — |
| `border` | `"#RRGGBB"` | The line around an **unfocused** window (a focused one's is `accent`), a menu, or anything with an edge | `--line` |
| `desktop` | `"#RRGGBB"` | The ground between windows | the `reef` wallpaper |
| `font_px` | number, `6`–`16` | The body text size in pixels per em, read to the nearest hundredth; `13` if absent | |
| `bevel` | number, `0`–`64` | How far a gradient's top lightens and its bottom darkens | flat: `0` |
| `font_ui` | `"/path"` | The face labels, buttons and list rows are drawn with — proportional | |
| `font_mono` | `"/path"` | The fixed-advance face: a character grid, and anything the toolkit draws through `mono` — the editor's buffer since Part J | |
| `wallpaper` | `"/path"` or `""` | A PNG to draw behind everything. Empty means none | |
| `wallpaper_mode` | `"fit"` or `"fill"` | How it is placed when it is not the screen's size | |

**The design token column** names where each built-in value comes from in
[the design](../design/nitrox-shell/) (`docs/design/nitrox-shell/nitrox-shell.html`): its light
palette for the light scheme, its `deep` palette for the dark one. A dash is a key the design has
no token for, and the value's doc comment in [`libdraw::theme`](../../userspace/libdraw/src/theme.rs)
says what was chosen instead and why.

## Two schemes

**`scheme` decides what every other line is an override on**, and it is read first wherever it is
in the file: a file that sets `accent` and then `scheme = "dark"` is the dark scheme with that
accent. A file with no `scheme` is the light one, which is also what a missing file is. A value
other than the two names is reported like any bad value and leaves the file on the light scheme.

**Two schemes reverse M11's decision 4** — "one theme … nothing ships a second" — on the
evidence of a design that specifies both and a maintainer who asked for both (decision log,
2026-09-17). What decision 4 was right about is the cost: every judgement is made twice. The
host-side renders are what pay for that, not the boots.

**What changed about the keys, and why.** `focus_ring` and `selection` are gone and `accent`
replaced them, which is the design's own model: its page stores one accent and *computes* the
selection (the accent at 20%) and a hover (10%, 18% in the dark scheme) from it, so a file that
named all three could set them out of step. A file written before the refresh that names either
old key has that line reported as unknown and otherwise reads as it did. The design's `--faceHi`
and `--faceLo` were already keys (`face_hover`, `face_pressed`); its `--lineSoft` is `border` at
half strength over the ground in both palettes, which is a derivation rather than a key. Its
`--warn` and `--panelFg` are defined in the page and used nowhere in it, and `--accentInk` colours
only surfaces the refresh does not build — a primary button, a notification badge.

**The terminal's colours are not here**, as they were not before: `--term` and `--termFg` sit
outside the design's two palettes too, and they belong to `libterm`'s sixteen, where retheming a
desktop does not retheme `ls`. Since the desktop refresh's Part F they are *literally* those two —
`Palette::default`'s `background` and `foreground` are the design's pair, and the sixteen were
retuned in the same key. One set, not one per scheme: a light desktop does not retune them, which
is the same conclusion the design reaches by keeping them outside both of its palettes.

**The six `syntax_*` keys, and `accent`, `ok` and `deny`, are the colours not derived from a
surface** (M14 Part G; the last three since the desktop refresh). Every other key here is a ground
or its ink, and a widget wanting a third was told to derive one; a keyword and a comment cannot be
derived from a window's ground, because what they encode is meaning rather than depth — and
neither can emphasis, "working" or "destructive". They are read by whatever highlights text — today
`nxedit` — and ignored by everything else, so a theme that omits them is a theme with the
shipped scheme, like any other omitted key.

**Two fonts because a grid is not a label** (M11 Part D). Everything the toolkit draws takes
`font_ui` unless it asks for the other; `nxterm`'s grid takes `font_mono`. Before Part D there
was a single path constant and every client loaded it, so every label in the system was
monospaced.

**`nxterm` was the one program that loaded both until the desktop refresh's Part J**, and this
said so. `nxedit` loads both now and attaches the fixed-advance face to the proportional one as a
companion, which is how `libui`'s `mono` reaches a second face: the editor's buffer, its byte
count and its position readout are set in `font_mono` while the chrome around them is not. So
this key is no longer "the face a character grid is drawn with" — it is the face anything drawn
through `mono` uses, a grid included.

**The wallpaper is a file a person supplies**, which is why the built-in theme names none and
why the guest decodes PNG at all rather than reading something the build converted (M12
decision 2). `desktop-shell` reads it — the shell holds `/home` and a theme, where the compositor
holds neither and should not gain a filesystem in order to draw — and puts it in a full-screen
bottom-most window it owns. A file that is absent, unreadable, or not a PNG this decoder handles
leaves the desktop its `desktop` colour and says on the console which of those it was.

**`wallpaper_mode` has two values**, and the key existed before the second so that it would be a
value rather than a new key (M12 decision 7):

- **`fit`** — the built-in theme's — scales a too-large picture down to fit inside the screen with
  its aspect ratio kept, and centres a smaller one.
- **`fill`** — the staged theme's, since Phase 5 Part E put a 16:10 picture on a 16:9 screen —
  scales the picture down to *cover* the screen and crops the overhang, centred.

**Neither scales up.** A `fill` whose picture is smaller than the screen in either axis draws it at
its own size, centred, which is what `fit` would have done with it; upscaling needs a decision about
interpolation and is `TODO(wallpaper-fill)`. Any other value is refused by name rather than quietly
fitted.

**What the decoder accepts**: bit depth 8, every colour type (greyscale, RGB, palette,
greyscale+alpha, RGBA), not interlaced, and at most 64 megapixels. An alpha channel is read past
and dropped — not because the system cannot composite with alpha, which it has been able to since
M13 Part B, but because a wallpaper is drawn on
nothing.

A path is **absolute, at most 128 bytes, and free of control characters, `"` and `\`**; a longer
or relative one leaves that role at its default. The last two are refused for the reason the
whole file is a TOML file: a path holding a quote would round-trip through *this* reader and read
as something else in any other one. The bound is not arbitrary: the theme travels to each
application on the setup record, which is one 4 KiB message carrying all of argv and the
environment, so a path a file could make arbitrarily long is a theme that could stop applications
from launching.

**Naming a proportional face in `font_mono` is a theme breaking its own terminal**, and it is not
refused, because nothing that reads the file can tell: `libterm` takes a cell's width from one
glyph's advance, so a proportional face yields a plausible number and then draws every column at
the wrong x. It is stated here instead. The two shipped faces are `DejaVuSans.ttf` and
`DejaVuSansMono.ttf`, both under `/system/fonts`, which is the only directory a session binds for
them.

**Read to the nearest hundredth of a pixel**, which is finer than a rasteriser resolves and is
not about precision: whatever draws with a size reports it to the console, and `check-terminal`
recomputes a character cell from that number on the host. A size the line cannot print exactly is
a size the two sides can disagree about, and the gate would blame the font. Rounding here makes
"the size printed is the size used" true by construction.

**`font_px` shrinks and does not grow, and 16 is not an arbitrary ceiling.** Text measures
exactly its em size, and the tightest fixed box in the system is a list row: 20 pixels with 2
above and 2 below, leaving 16. Larger text is clipped by the painter and overlapped by its
neighbours, because **chrome metrics are not themeable** (M11's decision 2) — which is also why
the gates can click a title bar at a fixed offset. Raising this means metrics that follow type,
and that is the decision to revisit, not this number.

**`font_px` is the body size, and the toolkit's two other steps follow it** (desktop refresh,
Part G): `TextSize::Small`, ⅞ of it, for a menu's shortcut column and the editor's status line;
and `TextSize::Large`, 13⁄12 of it, for the top bar's two words. So the largest text on the screen
is 17⅓ pixels at the ceiling, and it is only ever in the 30-pixel top bar. A window's title is
bold rather than larger. **At the floor the steps stop rather than follow**: ⅞ of the smallest
legal 6 would be 5.25, under the size this range exists to forbid, so `TextSize::px` clamps at
`MIN_FONT_PX` and a 6-pixel theme sets its hints at 6 too. The shipped default is 13; the staged file above sets 12, as the
refresh chose, and the default differs so that a client reading the file is visible.

**`bevel` is one number for every gradient in the system** — a title bar, a scrollbar's thumb, a
selected row. The reference desktop's own gradients span ±10 and ±14 around their midpoints, so
one amount reproduces both; two colours per gradient would be eight more values for a palette to
keep coherent. **Both built-in schemes are `0`, flat, since the refresh** — the design has not one
gradient in it — and the key stays because a bevel is still a theme somebody may want.

**`cursor_body`, `cursor_outline`, `outline` and `desktop` are read but do not take effect.** They are drawn
by the *compositor*, which `init` starts rather than the session — so it never sees a setup
record and uses the built-in values. They are listed because they are part of one theme and a
file that omitted them would be describing a different thing than the type does. **Trigger for
making them live: a control panel that wants to restyle the cursor** — the mechanism is a manager
op on a channel the shell already holds.

**`scheme` does reach the compositor, and by exactly that mechanism** (desktop refresh, Part A).
The shell sends the manager's [`SetScheme`](rsproto-surface-ops.md#setscheme-0x0928) on every
session start, and the compositor draws each window's shadow for that scheme — the design's dark
shadow is more than twice as strong as its light one. It names one of the two compiled palettes
rather than carrying colours, so the four keys above stay what they were: read, and not live.

**And `background` is live for window interiors only**, for exactly the same reason. The ground
*between* windows is the compositor's `scene::BACKGROUND`, a compile-time constant taken from the
built-in theme — so a file that sets `background` recolours what windows draw on and leaves the
desktop behind them as it was. Two consequences worth knowing before changing it: a window whose
committed buffer is smaller than its frame shows a seam against the old ground, and `nxterm`'s
grid draws on `libterm`'s own default background, which is the built-in value too. The type's own
doc calls one ground "what a window's ground and the space between windows share"; today the file
reaches only the first.

## What a broken file does

**Nothing fatal, ever.** A theme is decoration, and a desktop that will not start because its
colours did not parse is a worse failure than any colour could be. Specifically:

- **A missing file** is the built-in theme. So is an empty one, one of only comments, and one
  this system cannot read as UTF-8.
- **A key this version does not know** is skipped. That is the forward-compatibility rule
  [`service-toml-schema.md`](service-toml-schema.md) states: a file written by a newer system
  must still start an older one. It is *reported* all the same, because a misspelled key and a
  future key look identical and silence is what makes a typo take an afternoon.
- **A value that cannot be read** leaves that one field at its default, and the rest of the file
  is still read.
- **`font_px` outside 6–16** is refused: zero divides in the layout, and anything above what the
  fixed chrome holds is clipped and overlapped. Both are a text file that makes the machine
  unusable, which is the one thing a theme must not be able to be.
- **A font path that does not load** falls back to the built-in face for that role, and the
  application says so on the console: `nxfiles: theme font /home/Fancy.ttf did not resolve (is it
  staged into the rootfs?); using /system/fonts/DejaVuSans.ttf`.

  **This one check is not where the others are**, and the reason is worth knowing: the shell
  parses the file, but a path resolves in the *application's* namespace, and two applications can
  answer differently. So the syntax is checked when the file is read and the existence is checked
  where the font is loaded — a desktop with no text is exactly the failure this whole section
  exists to prevent.

The shell names each bad line, with the line number an editor would show, up to a bound.

## Not TOML in general

What reads this takes flat `key = value` and nothing else — no tables, no arrays, no dotted keys.
What it *accepts* is valid TOML, so the file is a TOML file and an editor will highlight it; a
reader that grew tables would be reading a different schema than this one has.

This is the house pattern rather than an exception: `init`'s `toml_lite` handles table arrays and
one-level subtables, `service-mgr`'s `service_toml` tracks two-level sections, and each says how
it differs from the others. The reader for this one lives beside the type it produces, in
[`libdraw::theme`](../../userspace/libdraw/src/theme.rs).

## Not a live protocol

A change takes effect when an application starts. The shell reads the file once per session and
puts the result on the setup record each launch already carries, so nothing new goes on the wire
and no client polls anything.

That is M11's decision 1, and its reasoning is what nothing *else* needs: polish iterations
rebuild the image anyway, so a push to running windows would be protocol work bought entirely for
a settings application. **Trigger: a control panel that must show a change without a restart.**
The shape it would take is already known — a server-to-client event, exactly like
`Surface::Dropped`.

## See also

- [`widget-toolkit.md`](../architecture/widget-toolkit.md) §11 — what is themeable and what is not
- [`libdraw::theme`](../../userspace/libdraw/src/theme.rs) — the type, the reader and the writer
- [`display-arm-plan.md`](../planning/display-arm-plan.md) — Milestone 11's parts and decisions
