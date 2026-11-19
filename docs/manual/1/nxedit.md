# nxedit — the text editor

## Synopsis

```
nxedit [PATH]
```

## Description

`nxedit` edits a text file in a window, a file to a tab. It is started from the Applications menu
on the top bar, which opens with its word or with `Super+A`; and by `nxfiles(1)`, which opens a
file in it. From a shell, `nxedit PATH` opens PATH. With no PATH it opens an untitled buffer.

A new tab is an untitled buffer. Save on one asks for a name on the status strip, where Enter
confirms and Esc cancels, and puts the file in the person's home folder. Save As opens a chooser
to save under another name, and Open opens one to pick a file into a tab. In a chooser the arrows
move through the list, Enter accepts, and Esc cancels; Backspace goes up a folder when opening, and
edits the name when saving.

A save writes a temporary file beside the target and renames it over, so a save that fails leaves
the old file as it was, and the buffer on the screen is kept. The tab strip shows the size of the
buffer in bytes, and marks a buffer that has unsaved changes.

Closing a tab or a window over unsaved changes asks first. The dialog's answers are to discard
them, and to keep editing; Esc keeps editing. Quit closes every window and asks as each window's
own close does, and cancelling one stops the quit.

The text is coloured by the file's name: a script ending `.nx` as the shell's language, and TOML,
Rust and Markdown files by their endings. A file with any other name is plain text.

A right click, or the Menu key or `Shift+F10`, opens a menu of the Edit menu's rows that act on
the text: Undo, Redo, Cut, Copy, Paste and Select All. From the keyboard it opens at the caret.

## Options

- `--help`: print the usage and exit, before any window opens.

## Menus

| Menu | Item | Key |
|---|---|---|
| File | New Tab | `Ctrl+T` |
| File | Close Tab | `Ctrl+W` |
| File | Open... | `Ctrl+O` |
| File | Save | `Ctrl+S` |
| File | Save As... | `Ctrl+Shift+S` |
| File | New Window | `Ctrl+Shift+N` |
| File | Close Window | |
| File | Quit | `Ctrl+Q` |
| Edit | Undo | `Ctrl+Z` |
| Edit | Redo | `Ctrl+Y` |
| Edit | Cut | `Ctrl+X` |
| Edit | Copy | `Ctrl+C` |
| Edit | Paste | `Ctrl+V` |
| Edit | Select All | `Ctrl+A` |
| Edit | Find | `Ctrl+F` |
| Edit | Replace... | `Ctrl+H` |
| Edit | Go to Line... | `Ctrl+G` |

Cut and Copy are available when there is a selection. While a field is open, on the status strip,
nothing in the menus is available; the keys are the field's.

## Keys

The Ctrl chords above work wherever the window has the keyboard, except that `Ctrl+T` and
`Ctrl+W` also work while a field is open. A Ctrl chord that is not above does nothing. Every
other key edits the text.

`Ctrl+Shift+V` straight after a paste replaces the pasted text with the next older entry of the
clipboard, and again for the next; any other key ends the run.

Find, Replace and Go to Line open a field. Esc closes it.

- Find: Enter finds the next match, and the field stays open. The strip says what was found.
- Replace: type what to find and Enter; then type what to put there and Enter. Each Enter
  replaces the selected match, finds the next, and says how many it has replaced.
- Go to Line: type a number and Enter. A number past the end goes to the last line. Something that
  is not a number is refused.

## See also

`nxfiles(1)`, `nxterm(1)`, `clip(1)`
