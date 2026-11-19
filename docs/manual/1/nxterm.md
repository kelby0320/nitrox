# nxterm — the terminal window

## Synopsis

```
nxterm
```

## Description

`nxterm` is the terminal in a window. Each tab holds a terminal with `nxsh(1)` running on it, so
what you type goes to the shell and what the shell prints is drawn in the window.

It is started from the Applications menu on the top bar, which opens with its word or with
`Super+A`.

A window can hold several tabs, shown in a strip, each with a shell of its own. Closing a tab ends
its shell. Closing the last tab closes the window. New Window opens another window, with one fresh
tab, not a copy of this one's shells; past the number of tabs a terminal may hold it says
`nxterm: no more tabs` in the tab instead. Quit closes every window.

The window keeps what has scrolled off the top. The scrollbar and the mouse wheel move through it,
and typing follows the output back to the bottom.

To select text, press the left button and drag. A double click selects a word, and a triple click
a line. Typing clears the selection. A right click, or the Menu key or `Shift+F10`, opens a menu
of the Edit menu's rows that act on the text: Copy, Paste, Select All, Find and Clear Scrollback.
The menu from the keyboard opens at the cursor.

Copy and Paste use the clipboard the windows share, which `clip(1)` reaches from a shell. Paste
types the newest entry at the shell.

## Options

- `--help`: print the usage and exit, before any window opens.

## Menus

| Menu | Item | Key |
|---|---|---|
| File | New Tab | `Ctrl+Shift+T` |
| File | Close Tab | `Ctrl+Shift+W` |
| File | New Window | `Ctrl+Shift+N` |
| File | Close Window | |
| File | Quit | `Ctrl+Shift+Q` |
| Edit | Copy | `Ctrl+Shift+C` |
| Edit | Paste | `Ctrl+Shift+V` |
| Edit | Select All | `Ctrl+Shift+A` |
| Edit | Find | `Ctrl+Shift+F` |
| Edit | Clear Scrollback | `Ctrl+Shift+K` |
| Edit | Clear | |
| Edit | Reset | |

Copy is available when there is a selection. Clear Scrollback is available when there is
scrollback to clear. Clear clears the screen. Reset starts the tab's terminal afresh.

## Keys

Every chord above holds Shift as well as Ctrl, so the plain Ctrl keys stay the program's: `Ctrl+C`
is still the interrupt. Every other key goes to the shell.

Find opens a field. While it is open the keys are the field's: type what to look for, Enter finds
the next match, and Esc closes it.

## See also

`nxsh(1)`, `clip(1)`, `nxfiles(1)`, `nxedit(1)`, `shell(7)`
