# desktop — list, switch and name the graphical session's desktops

## Synopsis

```
desktop
desktop switch N
desktop name N LABEL...
```

## Description

`desktop` works the desktops of the graphical session, the ones the desktop switcher on the bottom bar shows. It
needs a graphical session: in a shell that has none, it says so and fails.

With no operand it lists the desktops and marks the current one.

`desktop switch N` makes the Nth desktop current. `desktop name N LABEL` names the Nth desktop.
Naming is what makes a desktop stay: an unnamed desktop goes away when its last window leaves it,
and a named one does not. The words after N are joined with single spaces into the label.

N is a position as the desktop switcher counts them, starting at 1. It is not an id: ids are
never reused, so after a few desktops have come and gone they stop matching what is on the
screen.

## Options

- `--help`: show the usage and exit.
- `--version`: show the version and exit.

`desktop` takes no other option.

## Output

The listing is `Table<{position: Int, id: Int, name: String, current: Bool}>`, one row per
desktop. `current` is a column, not a marker, so a pipeline can filter on it:

```
desktop | filter current
```

Where the command has no output to write to, the listing is printed as lines instead, `*` marking
the current desktop and `(unnamed)` standing for a desktop with no name. If there are more
desktops than the command can show, it says so.

`switch` and `name` write nothing to their output.

## Exit status

0 when the request was done. 1 when there is no `/dev/desktop` in this namespace, a desktop at
that position does not exist, or the graphical session did not answer. 2 for an option it does
not know, bad operands, a position that is not a number from 1, or a name that is too long.

## Examples

```
desktop                        # the desktops, the current one marked
desktop switch 2               # go to the second desktop
desktop name 2 Mail            # name it, so it stays when its last window leaves
desktop | filter current       # which one is current
```

## See also

`shell-operators(7)`, `nxsh(1)`
