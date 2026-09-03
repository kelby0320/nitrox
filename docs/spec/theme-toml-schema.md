# Nitrox: Theme file schema (`theme.toml`)

The colours and text size a graphical session draws itself in. Read once by
[`desktop-shell`](../../userspace/desktop-shell) at session start and handed to every
application it launches; see [`display-arm-plan.md`](../planning/display-arm-plan.md) Milestone 11
Part C.

**Built as of M11 Part D.** What is not here yet: any way to change a theme without restarting
what draws with it. That is deliberate — see "Not a live protocol" below.

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
background = "#FFFFFF"
foreground = "#2F2F2F"
font_px = 16
bevel = 12
font_ui = "/system/fonts/DejaVuSans.ttf"
wallpaper = ""
wallpaper_mode = "fit"
```

**Those are the built-in values**, and the first line is what the image build writes at the top of
the shipped file — so this block is a fragment of the real thing rather than an illustration.
Copying it and changing one colour is the intended way to start; copying a *stale* one and getting
near-white widgets on a near-black ground is what this example used to do, because it still showed
the dark theme M11 Part E replaced (PR #265 review, finding 6). The shipped file also carries a
`font_px` that is deliberately not the default — see `check-login`, which reads it back.

| Key | Type | What it colours |
|---|---|---|
| `background` | `"#RRGGBB"` | A window's ground (**not** the space between windows — see below) |
| `foreground` | `"#RRGGBB"` | Text and other ink |
| `face` | `"#RRGGBB"` | A button at rest |
| `face_hover` | `"#RRGGBB"` | A button under the pointer |
| `face_pressed` | `"#RRGGBB"` | A button being held |
| `focus_ring` | `"#RRGGBB"` | The ring around the focused widget, and the text caret |
| `track` | `"#RRGGBB"` | A scrollbar's groove, and a list's ground |
| `thumb` | `"#RRGGBB"` | A scrollbar's thumb |
| `selection` | `"#RRGGBB"` | Behind selected text |
| `title_active` | `"#RRGGBB"` | A title bar whose window holds the keyboard |
| `title_inactive` | `"#RRGGBB"` | A title bar whose window does not |
| `cursor_body` | `"#RRGGBB"` | The pointer's fill |
| `cursor_outline` | `"#RRGGBB"` | The pointer's outline |
| `outline` | `"#RRGGBB"` | A resize outline, a snap preview, a drop target |
| `border` | `"#RRGGBB"` | The line around a window, a menu, or anything with an edge |
| `desktop` | `"#RRGGBB"` | The ground between windows |
| `font_px` | number, `6`–`16` | Text size in pixels per em, read to the nearest hundredth |
| `bevel` | number, `0`–`64` | How far a gradient's top lightens and its bottom darkens |
| `font_ui` | `"/path"` | The face labels, buttons and list rows are drawn with — proportional |
| `font_mono` | `"/path"` | The face a character grid is drawn with — fixed advance |
| `wallpaper` | `"/path"` or `""` | A PNG to draw behind everything. Empty means none |
| `wallpaper_mode` | `"fit"` | How it is placed when it is not the screen's size |

**Two fonts because a grid is not a label** (M11 Part D). Everything the toolkit draws takes
`font_ui`; `nxterm`'s grid takes `font_mono`, and `nxterm` is the one program that loads both —
its menus are widgets and its cells are not. Before Part D there was a single path constant and
every client loaded it, so every label in the system was monospaced.

**The wallpaper is a file a person supplies**, which is why the built-in theme names none and
why the guest decodes PNG at all rather than reading something the build converted (M12
decision 2). `desktop-shell` reads it — the shell holds `/home` and a theme, where the compositor
holds neither and should not gain a filesystem in order to draw — and puts it in a full-screen
bottom-most window it owns. A file that is absent, unreadable, or not a PNG this decoder handles
leaves the desktop its `desktop` colour and says on the console which of those it was.

**`wallpaper_mode` has exactly one legal value today**, and that is the point of it existing:
`fit` scales a too-large picture down to fit inside the screen with its aspect ratio kept, and
centres a smaller one. Filling the screen needs an upscaler and a decision about interpolation —
`TODO(wallpaper-fill)` — so the *dimension* is in the schema now and a second mode will be a
value rather than a new key. A file naming `fill` is refused by name rather than quietly fitted,
which is what makes that deferral observable from the outside.

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
above and 2 below, leaving 16. The window bars are 24 with 4+4 of button padding, which lands on
the same number. Larger text is clipped by the painter and overlapped by its neighbours, because
**chrome metrics are not themeable** (M11's decision 2) — which is also why the gates can click a
title bar at a fixed offset. Raising this means metrics that follow type, and that is the
decision to revisit, not this number.

**`bevel` is one number for every gradient in the system** — a title bar, a scrollbar's thumb, a
selected row. The reference desktop's own gradients span ±10 and ±14 around their midpoints, so
one amount reproduces both; two colours per gradient would be eight more values for a palette to
keep coherent. `0` is flat, and is a theme somebody may legitimately want.

**`cursor_body`, `cursor_outline`, `outline` and `desktop` are read but do not take effect.** They are drawn
by the *compositor*, which `init` starts rather than the session — so it never sees a setup
record and uses the built-in values. They are listed because they are part of one theme and a
file that omitted them would be describing a different thing than the type does. **Trigger for
making them live: a control panel that wants to restyle the cursor** — the mechanism is a manager
op on a channel the shell already holds.

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
