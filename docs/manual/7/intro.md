# intro — the manual, and where to start

This is Nitrox's manual. `man` lists its pages, and `man NAME` prints one; `man man` says how.

## Sections

| Section | Holds |
|---|---|
| 1 | Commands |
| 2 | System calls |
| 5 | File formats |
| 7 | Overviews: the system, the shell, the protocols |
| 8 | Services |

## What it holds

- `man(1)`, its reader.
- The system's specifications, as they are:
  - in section 2, what a program calls: `syscalls(2)`, `handles(2)`, `io(2)` and the rest;
  - in section 5, what a program reads and writes: `init.toml(5)`, `services.toml(5)`,
    `theme.toml(5)`, `views.toml(5)`, `typed-stream(5)` and the rest;
  - in section 7, how the parts speak: `shell-language(7)`, `pipeline-stdio(7)`, and the
    resource-server protocols, `rsproto(7)` and the pages named after it.

`man --keyword WORD` finds a page by a word in its name or its summary.
