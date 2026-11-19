# nxsh — the Nitrox shell

## Synopsis

```
nxsh
nxsh SCRIPT [ARG...]
nxsh -c SOURCE [ARG...]
```

## Description

`nxsh` is the shell: the program a login starts, and a terminal window starts in each tab. Its
language, with its pipelines of typed values, its variables, functions and operators, is
`shell(7)`, `shell-operators(7)` and `shell-language(7)`. This page is about running the program.

With no operand `nxsh` is interactive. It reads from the terminal it was handed, or from
`/dev/tty` if it was handed none, prints a banner, and runs each line you type, printing the
value it gives. The banner names the build and how to leave:

```
nxsh: interactive shell, Nitrox COMMIT (Ctrl-D or `exit` to leave)
```

With a SCRIPT, `nxsh` runs the file and exits. A relative SCRIPT is read from the working
directory. A script is read as UTF-8 text. A file given by a path, as `./tool`, is also run at a
shell's prompt, where the shell decides whether it is a program, a `#!` script or an `nxsh` script.

With `-c`, `nxsh` runs SOURCE, one line or several, as a script.

What follows the script or the source is the script's arguments. It reads them as `$args`, a list
of strings. At the prompt `$args` is empty.

## Options

- `-c SOURCE`: run SOURCE as a script, then exit. The arguments after it are `$args`.
- `-h`, `--help`: print the usage and exit.

## The prompt

The line being typed has a cursor, and a row of keys edits it:

| Key | Does |
|---|---|
| Left, Right | Move the cursor a character |
| Home, End | Move to the line's start or end |
| Backspace, Delete | Erase before, or under, the cursor |
| `Ctrl-U` | Erase everything before the cursor |
| `Ctrl-W` | Erase the word before the cursor |
| Up, Down | An older or a newer line from history |
| `Ctrl-R` | Search back through history |
| Tab | Complete a command or a path |
| Enter | Run the line |
| `Ctrl-C` | Discard the line |
| `Ctrl-D` | At an empty line, leave |

History is a ring in the running shell: a new login starts with none. `exit` leaves the shell,
with a status where it is given one.

## Exit status

0 when the script ran, or the shell was left at the prompt. `exit N` in a script sets the status
to N. 1 when the script ran and failed. 2 when the script could not be read, is not valid UTF-8,
or does not parse; the parse error names its line. 64 when `-c` has no script, or an interactive
shell has no terminal.

## Examples

```
nxsh                           # an interactive shell
nxsh backup.nx /home           # run a script, with $args holding "/home"
nxsh -c 'list /bin | count'    # run one line
```

## See also

`shell(7)`, `shell-operators(7)`, `shell-language(7)`, `man(1)`, `nxterm(1)`
