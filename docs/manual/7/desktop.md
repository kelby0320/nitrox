# desktop — the graphical desktop: bars, windows, desktops, menus and chords

## Description

The graphical desktop is what a login on the screen gives you: a bar across the top, a bar across
the bottom, and your windows between them. A program called `desktop-shell` draws the bars and the
menus, decides where new windows go, and ends the session when you ask. The compositor draws the
windows and carries the pointer and the keyboard to them. After you log in at the greeter, the
desktop is yours until the session ends from the power menu: Log out brings the greeter back.

This page is the desktop as a person uses it. The `desktop(1)` command is its companion at a
shell: it lists the desktops, switches between them and names them, the same things the switcher
on the bottom bar and the chords here do. Applications are started from the Applications menu;
`nxterm(1)` gives a window with a shell in it, `nxfiles(1)` browses files and `nxedit(1)` edits
text.

Windows are of several kinds. An application's ordinary window is listed on the bottom bar. A
dialog, such as a question an application asks, is not listed: it opens centred over the window
it belongs to. Minimizing, raising or moving that window to another desktop leaves its dialog
where it is. The bars and the menus are the desktop's own and are on every desktop.

## The bars

The bars are 30 pixels high and span the screen.

The **top bar** has, from the left:

- `Applications`, with a dot beside it: the Applications menu.
- `Places`: the Places menu.
- The date and time, `YYYY-MM-DD HH:MM`, centred on the screen. It is empty when the machine has
  no wall clock set.
- The power button, at the right-hand end: the power menu.

The **bottom bar** has, from the left:

- The **show-desktop** button. It puts away every window of this desktop that the bar lists, and
  lights. Pressed again, it brings back exactly the windows it put away, and the one that had the
  keyboard has it again. The light goes out, and the windows it would bring back are forgotten,
  as soon as a window of that desktop comes back another way, a window opens or is moved there,
  or the button is used on another desktop: it keeps one set at a time.
- The **window list**: one button per window of the current desktop, a dot and the title. The dot
  is dim for a window that is minimized, the accent colour for the one that has the keyboard, and
  a third colour for the rest. Pressing a button raises its window and brings it back if it was
  minimized; pressing the button of the window that has the keyboard minimizes it. A middle click
  asks the window to close; see Windows. Only as many buttons are shown as fit beside the
  switcher.
- The **desktop switcher**, at the right-hand end: an arrow `‹`, up to three cells, an arrow `›`,
  and the current desktop's name. See Desktops.

Neither bar takes the keyboard: typing always goes to a window.

## Windows

An application draws its own title bar. It holds the window's title and, on the right, buttons to
minimize, maximize and close the window; a dialog has fewer. There is no menu on a right click
of a title bar, and none on the desktop itself.

**Focus.** Keys go to the window that has focus, which is the topmost window that takes it.
Pressing a window with the pointer raises it and gives it focus. An application draws its own
border, and the system's applications draw the focused window's in the accent colour.

**New windows** open in a cascade, each 24 pixels below the last; when the next would run past the
bottom of the area between the bars, the cascade starts again at the top. A window opens on the
desktop that is current.

**Moving.** Press on a title bar and drag. A press on one of its buttons does not drag.

**Resizing.** Windows that can be resized have a grip in the bottom-right corner, inside the
border: press it and drag. An outline shows the new size while you drag, and the window takes the
size when you let go. A window cannot be made smaller than 64 pixels either way.

**Snapping.** When you let go of a window being moved with the pointer within 24 pixels of an edge
of the area between the bars, it takes the place the edge stands for, and an outline shows it
while you hold:

| Where | The window becomes |
|---|---|
| Left edge | The left half |
| Right edge | The right half |
| Bottom edge | The bottom half |
| A corner at the bottom | That quarter |
| A top corner | That quarter |
| Top edge | The whole area: maximized |

A top corner is checked before the top edge, so it gives the quarter. The top edge's band reaches
up over the top bar.

**Maximizing.** The maximize button, or a snap at the top edge, makes the window fill the area
between the bars. The button shows what the shell did, not what it asked. Pressing it again puts
the window back where it was before.

**Minimizing.** The minimize button, a press on the focused window's button on the bottom bar,
`Super+H`, and the show-desktop button all put a window away. Its button stays on the bottom
bar, with a dim dot; pressing it brings the window back.

**Closing.** A window's close button, and a middle click on its button on the bottom bar, ask it
to close, which an application with unsaved work answers with a question of its own.
A second middle click on the same button within five seconds closes the window without waiting
for the answer. After five seconds the next middle click asks again.

## Desktops

The windows are spread over desktops, one of which is on the screen at a time. Desktops are made
when they are wanted: the list always ends with one empty desktop to put a window on or switch
to, and when a window is put there it is not empty and another appears after it.

- **Switching.** Press a cell in the switcher, or its arrows to step to the previous or the next
  desktop; the arrow at either end of the list does nothing. The cells show up to three desktops
  around the current one, which is lit in the accent colour; a cell with a mark has windows in it.
  `Super+1` to `Super+4` go to the first four desktops directly, and `desktop switch N` does it
  from a shell. Pressing the name in the switcher opens the overview.
- **Moving a window.** `Super+Shift+1` to `Super+Shift+4` move the window that has focus to that
  desktop, and the overview can drag one there. The window moves and you stay, unless it was the
  last window of a desktop with no name: that desktop goes, as below.
- **Naming.** `Super+R` opens a prompt, "Name this desktop", above the right-hand end of the
  bottom bar. Type a name and press `Enter` to take it, or `Esc` to cancel. A name is at most 32
  bytes: the prompt cuts a longer one, and `desktop name N LABEL`, which names one from a shell,
  refuses it.
- **Which stay.** A desktop that has no name goes away when its last window leaves it. A desktop
  with a name stays, empty, until it is renamed to nothing and emptied. Naming is how you keep one.
  If the desktop that goes away is the one on the screen, you land on the one that takes its
  place.

A desktop without a name is shown by its position, `Desktop 2`. The number chords count positions
in the switcher from 1, not ids: ids are never reused, so after some desktops have come and gone
they stop matching what is on the screen.

A number chord for a desktop that does not exist does nothing, and so does a move chord with no
focused window. Only four desktops have chords; the others are reached from the switcher, the
overview or `desktop(1)`.

## The overview

Pressing the desktop's name in the switcher opens the overview: a card for each desktop over the
darkened desktop. A card is a small model of the screen, with the top bar as a strip across it
and a box where each of that desktop's windows is. The card of the current desktop has an
outline in the accent colour, and its boxes show what the windows looked like when the overview
opened; it does not follow a window that changes while it is open. Cards of other desktops show
their windows as boxes, each titled where it has room. A minimized window has no box.

- Press a card: go to that desktop.
- Press a window's box: go to its desktop, if it is on another, and raise it; the overview closes.
- Drag a window's box onto another card: the window moves to that desktop. The empty desktop at the
  end of the list is a card like the rest, which is how a window is put on a new desktop.
- Press the card you are already on, the background between the cards, or `Esc`: close the
  overview.

## Menus

A press elsewhere, or the menu losing the keyboard, closes it without choosing.

**The Applications menu** is opened by its word on the top bar or by `Super+A`. It lists the
graphical applications the system has, by name. Typing narrows the list to the rows whose name,
or whose program's name, holds what is typed, the case of English letters aside, and the menu is
as short as what matches. Each key typed lights the first match. `Enter` starts the lit row; when
the menu opens nothing is lit, and `Enter` closes it. `Up` and `Down` move the lit row, `Esc`
closes the menu, and a click starts the row. A filter that matches nothing says so in a row that
cannot be chosen.

**The Places menu** lists `Home`, `Documents`, `Downloads`, `Pictures` and `Root`, each with its
path beside it and the home written `~`. Choosing one opens a file browser, `nxfiles(1)`, at that
place.

**The power menu** holds **Log out**, **Restart** and **Shut down**, the last two drawn in the
colour the theme gives an action that cannot be undone. While one menu is open, `Left` and `Right`
move between the three menus, which is how the Places and power menus are reached from the keyboard.

Choosing one of them closes your windows first, so that nothing is lost. Each window of every
desktop is asked to close, and an application with unsaved work asks its own question, such as the
editor's. If windows are still open half a second later, a dialog under the power button names how
many are left and the first two by title, with two answers:

- **Cancel** keeps the session, and so does `Esc` while the dialog has the keyboard: an
  application's question that had it keeps it. Nothing more is asked, and what was asked stays
  asked.
- **End anyway** closes every window still open, whatever it was asking. It has no key.

It stays until it is answered or the last window has gone, and a press elsewhere, to answer an
application's question, does not dismiss it. When no window is left, Log out ends the session and
the greeter returns. Restart and Shut down then ask the system to run `shutdown(1)` with `--reboot`
or without, as `with power shutdown` does at a terminal. If the system's policy would refuse that, a
dialog says so, with a **Close** button, before any window is asked to close; so does a policy that
asks for your password, which only a terminal can ask for yet. Choosing another ending while the
windows are being closed, or while `shutdown` is being asked, changes nothing.

## Chords

A chord is a modifier held with a key. These work wherever the focus is, because the desktop asks
the compositor for them, and no application sees them. `Super` is the logo key.

| Keys | Does |
|---|---|
| `Super+1` | Go to desktop 1 |
| `Super+2` | Go to desktop 2 |
| `Super+3` | Go to desktop 3 |
| `Super+4` | Go to desktop 4 |
| `Super+Shift+1` | Move the focused window to desktop 1 |
| `Super+Shift+2` | Move the focused window to desktop 2 |
| `Super+Shift+3` | Move the focused window to desktop 3 |
| `Super+Shift+4` | Move the focused window to desktop 4 |
| `Super+R` | Name the current desktop |
| `Super+A` | Open the Applications menu; again, close whatever menu or prompt is open |
| `Super+H` | Minimize the focused window |

The modifiers must be exactly those shown: `Super+Shift+R` is not `Super+R`. `Super+R` does
nothing while a menu or the naming prompt is open. `Super+H` minimizes only a window that has a
button on the bottom bar, since a window with no button has no way back. Inside the naming prompt,
`Enter` takes the name and `Esc` cancels; inside a menu, the keys are those described under Menus.

## See also

`desktop(1)`, `nxfiles(1)`, `nxedit(1)`, `nxterm(1)`, `shutdown(1)`, `with(1)`, `intro(7)`,
`shell(7)`, `boot(7)`
