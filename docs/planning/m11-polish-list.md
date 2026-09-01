# Nitrox: The M11 polish list

**Status: open, and this is Part E's only input.** Written by the maintainer while driving the
system; [`display-arm-plan.md`](display-arm-plan.md) Milestone 11 Part E works from it in
batches. **M11 ends when the Open section is empty** — that is decision 5, and it is the
stopping condition a milestone about taste needs agreed before it starts.

## How to write in it

One line per thing that looks wrong. Terse is fine — "the cascade buries the first window",
"title bar text sits too high" — and a line nobody but you would understand is still worth more
than a line not written. Where it helps, name the shot or the preview it is visible in.

Nothing here needs to be a diagnosis. "This looks off" is a complete entry; working out *why*
is the batch's job, not the list's.

**Two things it is not for**, each with somewhere else to go:

- **Feel** — slowness, latency, a drag that lags the pointer. That is decision 6: it shares
  nothing with a colour but the window it is noticed on, and it has its own list below.
- **Missing capability** — "the editor should have tabs", "the browser needs a rename". That is
  Milestone 12, which exists so those have somewhere to go that is not this list.

## How to look at it

```
cargo xtask preview            # ~1s, host: the toolkit's own surfaces, no boot
cargo xtask shot               # ~3min, guest: the whole desktop, photographed
cargo xtask qemu --grab        # drive it yourself
```

`preview` writes `tools/build-cache/preview-{ui,term}.png` — what the toolkit draws, rendered
here rather than booted. `shot` writes `tools/build-cache/shot-{greeter,desktop,apps,windows}.png`
— the real screen, which is the only thing that can show the cursor, the window frames, the
ground between windows and how two windows sit next to each other.

**Use `--grab` whenever you drive it yourself**: the guest has a relative pointing device, so an
ungrabbed pointer and the guest's cursor drift apart permanently.

---

## Settled before the first batch

Four questions the list raised that had to be answered before any of it could be built
(2026-09-01). Recorded here rather than only in the decision log, because each one is the reason
a request is being met in a particular way.

- **One theme, and it is light.** `Theme::light()` replaces `Theme::dark()` rather than joining
  it — M11's decision 4, applied: one theme means one `check-display` reference and one judgement
  per polish item, where two would double both. Dark comes back any day as a constructor; the
  mechanism was built to hold a second and still does.
- **The terminal's grid stays dark.** `libterm` carries its own two defaults again instead of
  taking them from the theme. Part B tied them when there was one theme and it was dark, so the
  tie cost nothing; making the desktop light is exactly the event that shows the tie was between
  the grid and the *sixteen ANSI colours*, which are tuned for a dark ground. Bright white is
  `#ECF0F4` and would be invisible on white.
- **Gradients are one number.** A theme-wide bevel, lightening the top and darkening the bottom
  of a gradient fill. Measured against the reference: its title bar is ±10 around its midpoint and
  its menu selection ±14, so one number reproduces both — and a new palette needs one value rather
  than eight kept coherent by hand.
- **Image decoding is filed, not built here.** A gradient desktop ground and window buttons drawn
  as shapes get most of the reference look with no asset pipeline. Real images — a wallpaper, an
  icon set — share one dependency, and the design for it is written down rather than built: a
  full-screen background **window owned by `desktop-shell`**, which already holds `/home` and a
  theme, rather than the compositor, which holds neither.

---

## Open

- [] Real icons for minimize, maximize, and close buttons. (Icon support needed?)
- [] **Desktop Background** - We can choose a default color that fits the theme.  Probably some shade of blue.  It would be nice to be able to have background image support.  Image support can be a stretch goal, but all real OS desktops have background image support.
- [] **Stretch 1** - Drop shadow support around windows and menus.  Could be a good addition, but I don't know how difficult this will be.  Not critical.

- [x] The open applications menu is positioned wrong.  It should be a drop down from the applications button.  It also doesn't close when you click outside of it.  Also, you can't click any of the menu items. Menu item hovering doesn't work either.
      *(batch 4 — all four were one cause: the shell read pointer events for the overview, the
      applications button and the taskbar, and never for the modal's own window.)*
- [x] Scrollbars don't appear to work.  You can't click and drag them. *(batch 6 — `list_view`
      built one and gave it no pointer handler, so a list's bar showed a position and could not be
      dragged; `nxterm`'s was always wired. The launcher's list also kept no offset between
      frames.)*
- [x] The default window position should be more right.  The left side of the window should not be on the border of the screen. *(batch 4)*
- [x] The "nitrox" label next to "applications" on the top bar doesn't do anything.  It doesn't appear to be a drop down menu.  Recommend we remove it. *(batch 4 — removed)*
- [] **Stretch 2** - Can we have a date and time centered on the top bar similar to Gnome?
- [] **Stretch 3** - Transparency support.  Specifically for the desktop sidebar.  Instead of a solid white sidbar it would be nice if it was transparent and instead of line items (desktop 1, desktop 2, etc.) it had a small preview of the desktop similar to Gnome.  We could do scaled down windows in the same orientation as the actual desktop.  That would be very close to what Gnome looks like.  Or, if it's similar, we could just draw rectangles in instead of scaled down windows.  Whatever is easy.  The north star for this is the look of Gnome or Cosmic desktop.
- [x] nxedit doesn't launch from the menu. *(batch 7 — it launched and exited: it required
      `argv[1]` and the modal passes none. It opens untitled now and asks for a name on save.)*
- [x] Open windows on the bottom bar should have a border around them.  See the screenshots for an example. *(batch 8)*
- [x] The login prompt should be centered on the screen rather than in the upper left corner.
      *(batch 8 — it could not be: a `normal` window's requested origin was discarded on the wire,
      so a client that runs before any window manager could only appear in the corner.)*
---

## Done

Batches land here as they go, so the record of what changed and why is beside the request.

- **Batch 1 — the palette turns light** (2026-09-01). One theme, `Theme::light()`, replacing
  dark. Values measured from the reference. Two colours had to split off: `desktop` (the ground
  between windows, which stopped being the same kind of thing as a window's ground) and a
  saturated `outline` (the one colour composited over both). The terminal's grid keeps its own
  dark ground.
- **Batch 2a — gradients, drawn window controls, borders** (2026-09-01). One `bevel` number for
  every gradient; `_`/`[]`/`X` became shapes; a `border` colour, with a line around popups and a
  darker edge on a selected row.
- **Batch 8 — taskbar buttons, and a greeter that can be centred** (2026-09-01). Entries are
  bordered boxes now. Centring the login prompt turned out to need a protocol fix: a `normal`
  window's requested origin was written and read as zero, so a client running before any manager
  had no way to ask.
- **Batch 7 — nxedit opens untitled** (2026-09-01). It required `argv[1]` and the applications
  modal passes none, so it started and exited. It opens empty now, asks for a name in its own
  status strip when there is something to save, and writes into the session's `/home`.
- **Batch 6 — a scrollbar you can drag** (2026-09-01). `list_view`'s bar had no pointer handler
  at all; the arithmetic was there and shared with `nxterm` all along. The launcher's list also
  stopped being rebuilt from zero every frame, so `/bin`'s 26 entries are all reachable.
- **Batch 5 — the blue highlight, and dismissal for real** (2026-09-01). Both reported as still
  broken after batch 4, both correctly: hover took the quiet branch because that list keeps no
  selection, and `Focus(false)` only fires when something raises — so clicking bare desktop or a
  panel left the menu up. A new `Surface::Dismissed` op carries the half a client cannot see.
- **Batch 4 — the applications menu, and two small things** (2026-09-01). The menu hangs from
  its button, its rows can be clicked, it dismisses when focus leaves, and it highlights — all one
  cause. Plus the dead "nitrox" label removed and the cascade inset from the screen edge. Found
  three latent bugs on the way: two row builders that keyed the same row differently, a log line
  that hardcoded the value it reported, and a gate that assumed windows start at x=0.
- **Batch 3 — hover** (2026-09-01). The menu's highlighted item, and the discovery behind it:
  nothing in the system had ever reacted to the pointer being over it. Menu rows highlight like a
  selection; list rows get a quieter face. The applications modal still has none, because the
  shell has no router.
- **Batch 2b — the window frame** (2026-09-01). A one-pixel edge and three pixels of frame on the
  left, right and bottom, with the title bar flush at the top. Found a `check-login` bug on the
  way: its drag was landing on the window's last pixel column by accident.

---

## Not this milestone — feel

Performance and responsiveness. Decision 6: its own list, its own milestone.

- **Moving a window is slow** (reported 2026-08-28, on TCG). Recompose and damage work.

---

## Not this milestone — M12, applications deepened

Capability rather than appearance. See
[`display-arm-plan.md`](display-arm-plan.md) → Milestone 12.

*(nothing new yet)*
