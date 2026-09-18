# The Nitrox shell design

**Status: the north star for the desktop's appearance, adopted by display-arm Milestone 16
([display-arm-plan.md](../../planning/display-arm-plan.md)). Not a description of what is
built** — `design/` never is. When a question about appearance comes up, open the page and see
what it does; when the page and the code disagree, the page is the intent and the code is behind.

Produced with Claude Design by the maintainer, 2026-09-15, and handed over as a runnable page
rather than a picture — so it answers questions about *behaviour* too: what a menu does when it
opens, what the switcher shows when there is no previous desktop.

## What is here

| | |
|---|---|
| `nitrox-shell.html` | the design itself, self-contained — open it in a browser |
| `light/`, `dark/` | the six states that need a click to reach, in both palettes |

The screenshots exist because the page cannot be driven headlessly: `applications-menu`,
`places-menu`, `desktop-switcher`, `bottom-bar`, `nxterm-focused`, `launcher`. The bottom bar is
cut off in the others — the page is taller than the window it was captured in.

## Rendering it

The resting state, at the laptop's resolution:

```
google-chrome --headless --disable-gpu --virtual-time-budget=8000 \
  --window-size=1360,768 --screenshot=/tmp/shell.png \
  "file://$PWD/docs/design/nitrox-shell/nitrox-shell.html"
```

1360×768 because that is what every screen gate boots and as near as QEMU shows the laptop's
1366×768 — the same reasoning as `docs/conventions/qemu-integration-tests.md`.

The page's own stylesheet carries the token set (`--accent`, `--r`, `--sel`, …), and the mock
text editor is displaying a `theme.toml` whose keys are nearly ours. **That is the useful part**:
the design can be adopted as *data* rather than transcribed, which is what keeps `check-display`
from becoming a picture of whatever the code happens to draw. Milestone 16's opening section
argues this at length.

## What was taken and what was not

Decided 2026-09-17, recorded in the decision log. **Not adopted**: the notification centre and
quick settings (no service, no audio, no backlight), System Settings (it existed mostly to try
layouts), desktop icons (the Places menu covers it), and menu categories (a taxonomy invented for
three programs). **Deferred**: the launcher — wanted, and a surface of its own.

**One deliberate divergence**: the design barely distinguishes a focused window from an unfocused
one. We do, on the titlebar. A desktop where you cannot tell which window has the keyboard gets
reported as a focus bug.
