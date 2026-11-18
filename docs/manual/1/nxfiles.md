# nxfiles — the file browser

## Synopsis

```
nxfiles [PATH]
```

## Description

`nxfiles` shows a folder as a list of its entries, and moves, copies, renames and deletes them.

It is started from the Applications menu on the top bar, which opens with its word or with
`Super+A`; and from a Places entry, which starts it at that place. From a shell, `nxfiles PATH`
opens the window at PATH. Without a PATH it starts at the person's home folder.

The window has a sidebar of places and of drives, a strip showing the path, and the list. A
double click on a folder goes into it, and on anything else asks the desktop shell to open it.
Each tab has its own folder, its own order, and whether it shows hidden files. A drive in the
sidebar can be opened, and a removable drive ejected, from its row or its menu.

Delete asks first, and no key answers it but Esc, which keeps the entry. Properties opens a
dialog about the selected entry, which Esc or Enter closes. New File, New Folder, Rename and
Duplicate ask for a name in the strip, where Enter confirms and Esc cancels.

A right click, or the Menu key or `Shift+F10`, opens a menu for the row under it, for the folder
shown, or for a drive. A row's menu holds Open, Cut, Copy, Duplicate, Rename, Delete and
Properties. The folder's holds New File, New Folder, Paste and Show Hidden Files.

Cut, Copy and Paste move a path through the clipboard the windows share.

## Menus

| Menu | Item | Key |
|---|---|---|
| File | New Tab | `Ctrl+T` |
| File | Close Tab | `Ctrl+W` |
| File | Go to Location... | `Ctrl+L` |
| File | New Window | `Ctrl+Shift+N` |
| File | Quit | `Ctrl+Q` |
| File | New File | |
| File | New Folder | |
| File | Rename | |
| File | Delete | |
| File | Properties | `Ctrl+I` |
| Edit | Cut | `Ctrl+X` |
| Edit | Copy | `Ctrl+C` |
| Edit | Paste | `Ctrl+V` |
| Edit | Duplicate... | |
| View | Name (A-Z) | |
| View | Name (Z-A) | |
| View | Oldest First | |
| View | Newest First | |
| View | Show Hidden Files | `Ctrl+H` |
| View | Search... | `Ctrl+F` |

The items that act on an entry are greyed without a selection, and Close Tab on the last tab. The
four orders are a group, the current one marked.

## Keys

| Key | Does |
|---|---|
| Up, Down | Move the selection |
| Enter | Open the selected entry |
| Backspace | Go up to the parent folder |
| Menu, `Shift+F10` | Open the menu for the selection |

The Ctrl chords in the Menus table work wherever the window has the keyboard; a Ctrl chord that is
not in it does nothing.

Search opens a field over the list. Typing narrows it to the entries that match, Up and Down move
through them, Enter opens the selection, or the first match, and Esc closes it. Go to Location
opens a field for a path: Enter goes there, and Esc cancels. While a field or a name is being
typed the keys are its own, and Backspace edits it.

## See also

`nxedit(1)`, `nxterm(1)`, `clip(1)`, `disk(1)`
