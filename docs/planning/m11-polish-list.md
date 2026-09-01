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

----

## Open

- [] **Color Scheme** - We have defaulted to a dark theme. I want to default to a light theme.  Note we don't have any explicit support for light or dark themes I'm just describing the general color scheme we are using.  We can experiment with some different palettes but I've uploaded a screenshot of one of my laptops running MATE.  I like the look of it and I think it's a good starting point.  I will continue to reference it.  The file is build-cache/Screenshot at 2026-09-01 10-39-59.png
- [] Slight gradient on title bars (and scrollbars?) See screenshot.
- [] Small spacing on left, right, and bottom of windows.  See screenshot.
- [] Real icons for minimize, maximize, and close buttons. (Icon support needed?)
- [] Desktop top and bottom bars should be light rather than dark.  Exact colors are a theme question we can interate on.
- [] build-cache/Screenshot at 2026-09-01 10-51-25.png show a similar desktop, but with a dropdown menu open.  This is a good reference for how a dropdown menu should look.  Note there are icons and keyboard shortcuts show on menu items.  These are nice to have, but we don't have to do them now.  I would like to highlight the selected item in the menu.  It is not only highlighted blue, but it has a thin dark blue border.  We should do something like this.  The menu itself also has a thin dark border around it.
- [] **Desktop Background** - We can choose a default color that fits the theme.  Probably some shade of blue.  It would be nice to be able to have background image support.  Image support can be a stretch goal, but all real OS desktops have background image support.
- [] **Stretch 1** - Drop shadow support around windows and menus.  Could be a good addition, but I don't know how difficult this will be.  Not critical.

---

## Done

Batches land here as they go, so the record of what changed and why is beside the request.

*(nothing yet)*

---

## Not this milestone — feel

Performance and responsiveness. Decision 6: its own list, its own milestone.

- **Moving a window is slow** (reported 2026-08-28, on TCG). Recompose and damage work.

---

## Not this milestone — M12, applications deepened

Capability rather than appearance. See
[`display-arm-plan.md`](display-arm-plan.md) → Milestone 12.

*(nothing new yet)*
